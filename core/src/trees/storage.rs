//! Storage layer for `core::trees`. **Every operational rusqlite import
//! lives in this file** (per `trees-module-discipline`). The schema, the
//! connection setup, the SQL helpers, the basic CRUD methods, and the
//! low-level row-shape mutations all live here. Higher-level ops in
//! `super::ops` and `super::history` compose these helpers and use the
//! re-exported `params!` macro / `OptionalExtension` trait via this
//! module — they do not `use rusqlite::*` directly.

use std::path::Path;
use std::sync::Mutex;

pub(super) use rusqlite::{params, Connection, OptionalExtension};

use super::types::{
    EditableNode, NodeInsert, NodeKind, NodePolicy, TreeInsert, TreeRow, Trees, TreesError,
    SCHEMA_VERSION,
};

// ── Connection + schema ──────────────────────────────────────────────

impl Trees {
    /// Open or create the trees db at `<vault_root>/.hiker/trees.db`.
    /// Fails loud on schema-version mismatch (pre-1.0 policy: delete the
    /// file and retry).
    pub fn open(vault_root: &Path) -> Result<Self, TreesError> {
        let hiker_dir = vault_root.join(".hiker");
        std::fs::create_dir_all(&hiker_dir)?;
        let db_path = hiker_dir.join("trees.db");
        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        ensure_schema(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    // ── Tree-level operations ────────────────────────────────────────

    /// Insert a new tree row. Returns the tree id (generated when the
    /// caller passes `None`).
    pub fn insert_tree(&self, t: TreeInsert) -> Result<super::types::TreeId, TreesError> {
        let id = t.id.unwrap_or_else(crate::store::new_id);
        let now = now_ms();
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "INSERT INTO cluster_trees
               (id, name, source, state, scope, method, created_at_ms, vault_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                t.name,
                t.source,
                t.state,
                t.scope_json,
                t.method_json,
                now,
                t.vault_snapshot,
            ],
        )?;
        Ok(id)
    }

    /// Look up one tree row by id. Returns `None` if it doesn't exist.
    pub fn get_tree(&self, tree_id: &str) -> Result<Option<TreeRow>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let row = conn
            .query_row(
                "SELECT id, name, source, state, scope, method, created_at_ms, vault_snapshot
                 FROM cluster_trees WHERE id = ?1",
                params![tree_id],
                map_tree_row,
            )
            .optional()?;
        Ok(row)
    }

    /// List every tree row, newest first.
    pub fn list_trees(&self) -> Result<Vec<TreeRow>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, source, state, scope, method, created_at_ms, vault_snapshot
             FROM cluster_trees ORDER BY created_at_ms DESC",
        )?;
        let rows = stmt
            .query_map([], map_tree_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Update a tree's state. Returns `TreeNotFound` if no row matched.
    pub fn set_tree_state(&self, tree_id: &str, state: &str) -> Result<(), TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let n = conn.execute(
            "UPDATE cluster_trees SET state = ?1 WHERE id = ?2",
            params![state, tree_id],
        )?;
        if n == 0 {
            return Err(TreesError::TreeNotFound(tree_id.to_string()));
        }
        Ok(())
    }

    /// Delete a tree row. Cascades to `cluster_nodes` and
    /// `cluster_tree_history` via FK ON DELETE CASCADE.
    pub fn delete_tree(&self, tree_id: &str) -> Result<(), TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "DELETE FROM cluster_trees WHERE id = ?1",
            params![tree_id],
        )?;
        Ok(())
    }

    // ── Node-level operations ────────────────────────────────────────

    /// Bulk-insert nodes for a tree under one transaction. Used by the
    /// build pipeline when it lands a fresh tree's initial state.
    pub fn insert_nodes(&self, tree_id: &str, nodes: &[NodeInsert]) -> Result<(), TreesError> {
        let mut conn = self.conn.lock().expect("trees mutex poisoned");
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO cluster_nodes
                   (tree_id, node_id, parent_id, kind, note_id, name, summary,
                    user_edited_name, user_edited_summary, policy, centroid,
                    confidence, summary_membership_churn)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            for n in nodes {
                let policy_json = match &n.policy {
                    Some(p) => Some(serde_json::to_string(p)?),
                    None => None,
                };
                let centroid_bytes = n.centroid.as_ref().map(pack_centroid);
                stmt.execute(params![
                    tree_id,
                    n.node_id,
                    n.parent_id,
                    n.kind.as_str(),
                    n.note_id,
                    n.name,
                    n.summary,
                    n.user_edited_name as i32,
                    n.user_edited_summary as i32,
                    policy_json,
                    centroid_bytes,
                    n.confidence as f64,
                    n.summary_membership_churn as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Fetch one node, hydrated into an `EditableNode`.
    pub fn get_node(
        &self,
        tree_id: &str,
        node_id: &str,
    ) -> Result<Option<EditableNode>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let row = conn
            .query_row(
                NODE_SELECT_SQL,
                params![tree_id, node_id],
                map_editable_node,
            )
            .optional()?;
        row.transpose()
    }

    /// Every node in the tree, in arbitrary order. Caller groups by
    /// `parent` to walk the tree.
    pub fn list_nodes(&self, tree_id: &str) -> Result<Vec<EditableNode>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT node_id, parent_id, kind, note_id, name, summary,
                    user_edited_name, user_edited_summary, policy, centroid,
                    confidence, summary_membership_churn
             FROM cluster_nodes WHERE tree_id = ?1",
        )?;
        let rows: Vec<EditableNode> = stmt
            .query_map(params![tree_id], map_editable_node)?
            .collect::<Result<Vec<Result<EditableNode, TreesError>>, _>>()?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Children of a given parent (or the root when `parent_id` is
    /// `None`). Cheap thanks to the `(tree_id, parent_id)` index.
    pub fn children_of(
        &self,
        tree_id: &str,
        parent_id: Option<&str>,
    ) -> Result<Vec<EditableNode>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let (sql, rows) = match parent_id {
            Some(pid) => (
                "SELECT node_id, parent_id, kind, note_id, name, summary,
                        user_edited_name, user_edited_summary, policy, centroid,
                        confidence, summary_membership_churn
                 FROM cluster_nodes WHERE tree_id = ?1 AND parent_id = ?2",
                {
                    let mut stmt = conn.prepare(
                        "SELECT node_id, parent_id, kind, note_id, name, summary,
                                user_edited_name, user_edited_summary, policy, centroid,
                                confidence, summary_membership_churn
                         FROM cluster_nodes WHERE tree_id = ?1 AND parent_id = ?2",
                    )?;
                    stmt.query_map(params![tree_id, pid], map_editable_node)?
                        .collect::<Result<Vec<_>, _>>()?
                },
            ),
            None => (
                "(root)",
                {
                    let mut stmt = conn.prepare(
                        "SELECT node_id, parent_id, kind, note_id, name, summary,
                                user_edited_name, user_edited_summary, policy, centroid,
                                confidence, summary_membership_churn
                         FROM cluster_nodes WHERE tree_id = ?1 AND parent_id IS NULL",
                    )?;
                    stmt.query_map(params![tree_id], map_editable_node)?
                        .collect::<Result<Vec<_>, _>>()?
                },
            ),
        };
        let _ = sql;
        rows.into_iter().collect::<Result<Vec<_>, _>>()
    }

    /// Append a single new node row. Used by split + future operations
    /// that grow the tree mid-edit. Doesn't write a history row itself —
    /// callers wrap this inside a higher-level op that does.
    pub fn insert_single_node(&self, tree_id: &str, n: NodeInsert) -> Result<(), TreesError> {
        let policy_json = match &n.policy {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        let centroid_bytes = n.centroid.as_ref().map(pack_centroid);
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "INSERT INTO cluster_nodes
               (tree_id, node_id, parent_id, kind, note_id, name, summary,
                user_edited_name, user_edited_summary, policy, centroid,
                confidence, summary_membership_churn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                tree_id,
                n.node_id,
                n.parent_id,
                n.kind.as_str(),
                n.note_id,
                n.name,
                n.summary,
                n.user_edited_name as i32,
                n.user_edited_summary as i32,
                policy_json,
                centroid_bytes,
                n.confidence as f64,
                n.summary_membership_churn as i64,
            ],
        )?;
        Ok(())
    }

    /// Delete a single node by id. Reserved for the cluster editor's
    /// own clean-up paths (e.g. removing an empty cluster after a
    /// reshape).
    ///
    /// status: cluster-summary-staleness-counter
    /// No churn bump here: this is a structural drop of an already-empty
    /// cluster shell (the caller's policy in `update_for_folder_rename`
    /// only invokes us once `still_has_children == 0`), so no leaf
    /// insert-or-remove crosses an ancestor boundary that wasn't already
    /// counted by the upstream move. Leaf removals proper happen through
    /// `move_node` / `reparent_many` / `promote_outlier`, all of which
    /// bump their ancestor chains directly.
    pub fn delete_node(&self, tree_id: &str, node_id: &str) -> Result<(), TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "DELETE FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
            params![tree_id, node_id],
        )?;
        Ok(())
    }

    /// Collect a node and all of its ancestors up to the root as a set.
    /// Used to compute the LCA stop-set for move-style ops that should
    /// not bump churn on common ancestors (their subtree's leaf set is
    /// unchanged by an internal move).
    pub fn ancestors_inclusive(
        &self,
        tree_id: &str,
        node_id: &str,
    ) -> Result<std::collections::HashSet<String>, TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        let mut out = std::collections::HashSet::new();
        let mut cursor: Option<String> = Some(node_id.to_string());
        while let Some(id) = cursor {
            let parent: Option<Option<String>> = conn
                .query_row(
                    "SELECT parent_id FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            let Some(parent) = parent else {
                break;
            };
            out.insert(id);
            cursor = parent;
        }
        Ok(out)
    }

    /// Like `bump_churn_chain` but stops (without bumping) when the walk
    /// reaches any node in `stop_at`. Used by move-style ops to skip
    /// churn bumps on the LCA and its ancestors — those nodes' subtree
    /// leaf sets are unchanged by a within-tree move.
    pub fn bump_churn_chain_until(
        &self,
        tree_id: &str,
        from_node: &str,
        stop_at: &std::collections::HashSet<String>,
        delta: u32,
    ) -> Result<(), TreesError> {
        if delta == 0 {
            return Ok(());
        }
        let mut conn = self.conn.lock().expect("trees mutex poisoned");
        let tx = conn.transaction()?;
        let mut cursor: Option<String> = Some(from_node.to_string());
        while let Some(id) = cursor {
            if stop_at.contains(&id) {
                break;
            }
            let parent: Option<Option<String>> = tx
                .query_row(
                    "SELECT parent_id FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            let Some(parent) = parent else {
                break;
            };
            tx.execute(
                "UPDATE cluster_nodes
                 SET summary_membership_churn = summary_membership_churn + ?1
                 WHERE tree_id = ?2 AND node_id = ?3",
                params![delta as i64, tree_id, id],
            )?;
            cursor = parent;
        }
        tx.commit()?;
        Ok(())
    }

    /// Increment the membership-churn counter on a node and every
    /// ancestor up to the root. Per
    /// `cluster-build-from-folders-summary-staleness`.
    pub fn bump_churn_chain(
        &self,
        tree_id: &str,
        from_node: &str,
        delta: u32,
    ) -> Result<(), TreesError> {
        if delta == 0 {
            return Ok(());
        }
        let mut conn = self.conn.lock().expect("trees mutex poisoned");
        let tx = conn.transaction()?;
        let mut cursor: Option<String> = Some(from_node.to_string());
        while let Some(id) = cursor {
            let parent: Option<Option<String>> = tx
                .query_row(
                    "SELECT parent_id FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2",
                    params![tree_id, id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?;
            let Some(parent) = parent else {
                break;
            };
            tx.execute(
                "UPDATE cluster_nodes
                 SET summary_membership_churn = summary_membership_churn + ?1
                 WHERE tree_id = ?2 AND node_id = ?3",
                params![delta as i64, tree_id, id],
            )?;
            cursor = parent;
        }
        tx.commit()?;
        Ok(())
    }

    /// Reset the churn counter on one node — called when the user runs
    /// "Regenerate" on the node.
    pub fn reset_churn(&self, tree_id: &str, node_id: &str) -> Result<(), TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "UPDATE cluster_nodes
             SET summary_membership_churn = 0
             WHERE tree_id = ?1 AND node_id = ?2",
            params![tree_id, node_id],
        )?;
        Ok(())
    }

    /// Set the churn counter on one node to a specific value. Used by ops
    /// that need to neutralize spurious `bump_churn_chain` walks from
    /// nested reshape primitives (e.g. recluster-subtree restoring the
    /// selected-node-and-ancestors chain after `reparent_many`).
    pub fn set_churn(
        &self,
        tree_id: &str,
        node_id: &str,
        value: u32,
    ) -> Result<(), TreesError> {
        let conn = self.conn.lock().expect("trees mutex poisoned");
        conn.execute(
            "UPDATE cluster_nodes
             SET summary_membership_churn = ?1
             WHERE tree_id = ?2 AND node_id = ?3",
            params![value as i64, tree_id, node_id],
        )?;
        Ok(())
    }
}

// ── SQL helpers ──────────────────────────────────────────────────────

pub(super) const NODE_SELECT_SQL: &str = "
    SELECT node_id, parent_id, kind, note_id, name, summary,
           user_edited_name, user_edited_summary, policy, centroid,
           confidence, summary_membership_churn
    FROM cluster_nodes WHERE tree_id = ?1 AND node_id = ?2";

pub(super) fn map_tree_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TreeRow> {
    Ok(TreeRow {
        id: row.get(0)?,
        name: row.get(1)?,
        source: row.get(2)?,
        state: row.get(3)?,
        scope_json: row.get(4)?,
        method_json: row.get(5)?,
        created_at_ms: row.get(6)?,
        vault_snapshot: row.get(7)?,
    })
}

pub(super) fn map_editable_node(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<EditableNode, TreesError>> {
    let node_id: String = row.get(0)?;
    let parent_id: Option<String> = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let note_id: Option<String> = row.get(3)?;
    let name: String = row.get(4)?;
    let summary: String = row.get(5)?;
    let user_edited_name: i32 = row.get(6)?;
    let user_edited_summary: i32 = row.get(7)?;
    let policy_json: Option<String> = row.get(8)?;
    let centroid_blob: Option<Vec<u8>> = row.get(9)?;
    let confidence: f64 = row.get(10)?;
    let churn: i64 = row.get(11)?;

    Ok((|| -> Result<EditableNode, TreesError> {
        let kind = NodeKind::parse(&kind_str).ok_or_else(|| {
            TreesError::Sqlite(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("unknown node kind: {kind_str}"),
                )),
            ))
        })?;
        let policy: Option<NodePolicy> = match policy_json {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        };
        let centroid = centroid_blob.as_deref().map(unpack_centroid);
        Ok(EditableNode {
            id: node_id,
            parent: parent_id,
            kind,
            note_ref: note_id,
            name,
            summary,
            user_edited_name: user_edited_name != 0,
            user_edited_summary: user_edited_summary != 0,
            policy,
            centroid,
            confidence: confidence as f32,
            summary_membership_churn: churn.max(0) as u32,
        })
    })())
}

pub(super) fn pack_centroid(v: &Vec<f32>) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub(super) fn unpack_centroid(bytes: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let arr = [chunk[0], chunk[1], chunk[2], chunk[3]];
        out.push(f32::from_le_bytes(arr));
    }
    out
}

pub(super) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn configure(conn: &Connection) -> Result<(), TreesError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn ensure_schema(conn: &mut Connection) -> Result<(), TreesError> {
    let user_version: i32 =
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if user_version != 0 && user_version != SCHEMA_VERSION {
        return Err(TreesError::VersionMismatch {
            found: user_version,
            expected: SCHEMA_VERSION,
        });
    }
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS cluster_trees (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            source          TEXT NOT NULL,
            state           TEXT NOT NULL,
            scope           TEXT NOT NULL,
            method          TEXT NOT NULL,
            created_at_ms   INTEGER NOT NULL,
            vault_snapshot  TEXT
        );

        CREATE TABLE IF NOT EXISTS cluster_nodes (
            tree_id                  TEXT NOT NULL REFERENCES cluster_trees(id) ON DELETE CASCADE,
            node_id                  TEXT NOT NULL,
            parent_id                TEXT,
            kind                     TEXT NOT NULL,
            note_id                  TEXT,
            name                     TEXT NOT NULL,
            summary                  TEXT NOT NULL,
            user_edited_name         INTEGER NOT NULL DEFAULT 0,
            user_edited_summary      INTEGER NOT NULL DEFAULT 0,
            policy                   TEXT,
            centroid                 BLOB,
            confidence               REAL,
            summary_membership_churn INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (tree_id, node_id)
        );
        CREATE INDEX IF NOT EXISTS cluster_nodes_parent
            ON cluster_nodes(tree_id, parent_id);
        CREATE INDEX IF NOT EXISTS cluster_nodes_note
            ON cluster_nodes(tree_id, note_id);

        CREATE TABLE IF NOT EXISTS cluster_tree_history (
            tree_id   TEXT NOT NULL REFERENCES cluster_trees(id) ON DELETE CASCADE,
            seq       INTEGER NOT NULL,
            ts_ms     INTEGER NOT NULL,
            op        TEXT NOT NULL,
            args      TEXT NOT NULL,
            undo_args TEXT NOT NULL,
            PRIMARY KEY (tree_id, seq)
        );
        CREATE INDEX IF NOT EXISTS cluster_tree_history_seq_desc
            ON cluster_tree_history(tree_id, seq DESC);
        "#,
    )?;
    if user_version == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(
            schema_version = SCHEMA_VERSION,
            "trees: created trees db schema",
        );
    }
    Ok(())
}
