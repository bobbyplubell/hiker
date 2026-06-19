//! Note metadata index: the write side that re-derives `note_meta` from
//! frontmatter on ingest, and the structured `query_notes` read side that
//! backs tag / lifecycle / author / field queries.
//!
//! All SQL stays here, behind plain DTOs (`store-module-discipline`). The
//! query builder assembles a single parameterized statement — every
//! user-supplied string is a bound `?`, never interpolated.
//
// status: store-note-metadata-index

use std::collections::{BTreeMap, HashMap};

use rusqlite::params;
use rusqlite::types::ToSql;

use super::dto::{
    title_from_path, MetaEntry, MetaFilter, NoteMetaRow, NoteOrder, NoteProblem, NoteQuery,
    NoteQueryRow, OrderDir,
};
use super::error::Error;
use super::Store;

const fn dir_sql(d: OrderDir) -> &'static str {
    match d {
        OrderDir::Asc => "ASC",
        OrderDir::Desc => "DESC",
    }
}

impl Store {
    /// Replace the `note_meta` rows for `note_id` with `entries` in one
    /// transaction. Called by the indexer right after `upsert_note`,
    /// mirroring the `trail_waypoints` re-derivation. An empty slice just
    /// clears any stale rows (a note that lost its frontmatter).
    ///
    /// status: store-note-metadata-index
    pub fn replace_note_metadata(
        &mut self,
        note_id: &str,
        entries: &[MetaEntry],
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM note_meta WHERE note_id = ?1", params![note_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO note_meta (note_id, key, value, num) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in entries {
                stmt.execute(params![note_id, e.key, e.value, e.num])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete all metadata rows for a note id. Used by the delete / skip
    /// paths — `note_meta` carries no FK cascade, same posture as
    /// `chunk_vecs`.
    ///
    /// status: store-note-metadata-index
    pub fn delete_note_metadata(&self, note_id: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM note_meta WHERE note_id = ?1", params![note_id])?;
        Ok(())
    }

    /// Project every indexed, non-skipped note plus its relationship /
    /// provenance frontmatter (`hiker.parent` / `hiker.author` /
    /// `hiker.provenance` / `hiker.kind`) in one query. Backs the Vault-view
    /// lens (`vault-view.md`), which groups and nests on these fields per
    /// render and so cannot afford a per-note disk read.
    ///
    /// The four keys are read from the existing `note_meta` index
    /// (`store-note-metadata-index`) via correlated scalar subqueries — a
    /// note missing a key yields `None` rather than dropping out of the
    /// result. A list-valued key (none of these four are, in practice)
    /// collapses to its first value.
    ///
    /// status: vault-view-source-groups
    pub fn notes_with_meta(&self) -> Result<Vec<NoteMetaRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT n.path, n.path,
                    (SELECT m.value FROM note_meta m
                       WHERE m.note_id = n.path AND m.key = 'hiker.parent' LIMIT 1),
                    (SELECT m.value FROM note_meta m
                       WHERE m.note_id = n.path AND m.key = 'hiker.author' LIMIT 1),
                    (SELECT m.value FROM note_meta m
                       WHERE m.note_id = n.path AND m.key = 'hiker.provenance' LIMIT 1),
                    (SELECT m.value FROM note_meta m
                       WHERE m.note_id = n.path AND m.key = 'hiker.kind' LIMIT 1)
             FROM notes n
             WHERE n.skipped = 0",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(NoteMetaRow {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    parent: row.get(2)?,
                    author: row.get(3)?,
                    provenance: row.get(4)?,
                    kind: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Structured retrieval over the metadata index. Each filter becomes an
    /// EXISTS subquery against `note_meta`; `folder` a path GLOB; `order` /
    /// `limit` shape the result; skipped notes are excluded. When `select`
    /// is non-empty, the named keys are fetched in a second pass and packed
    /// into each row's `fields`.
    ///
    /// status: store-note-query
    pub fn query_notes(&self, q: &NoteQuery) -> Result<Vec<NoteQueryRow>, Error> {
        let mut sql = String::from("SELECT n.path, n.path, n.mtime FROM notes n WHERE n.skipped = 0");
        // Bound values, in the exact order their `?` appears in `sql`.
        let mut binds: Vec<Box<dyn ToSql>> = Vec::new();

        for f in &q.filters {
            match f {
                MetaFilter::Equals { key, values } => {
                    // Value-list OR inside one EXISTS (`query-filter-grammar`).
                    // An empty list matches nothing; SQLite rejects `IN ()`,
                    // so compile it to a constant-false term instead.
                    if values.is_empty() {
                        sql.push_str(" AND 0");
                        continue;
                    }
                    let placeholders = vec!["?"; values.len()].join(",");
                    sql.push_str(&format!(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.path AND m.key = ? AND m.value IN ({placeholders}))",
                    ));
                    binds.push(Box::new(key.clone()));
                    for v in values {
                        binds.push(Box::new(v.clone()));
                    }
                }
                MetaFilter::Exists { key } => {
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.path AND m.key = ?)",
                    );
                    binds.push(Box::new(key.clone()));
                }
                MetaFilter::NumRange { key, min, max } => {
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.path AND m.key = ? AND m.num IS NOT NULL",
                    );
                    binds.push(Box::new(key.clone()));
                    if let Some(lo) = min {
                        sql.push_str(" AND m.num >= ?");
                        binds.push(Box::new(*lo));
                    }
                    if let Some(hi) = max {
                        sql.push_str(" AND m.num <= ?");
                        binds.push(Box::new(*hi));
                    }
                    sql.push(')');
                }
                MetaFilter::Board { board_path, columns } => {
                    // Board membership joins through the derived
                    // `board_cards` table (`query-filter-grammar`); freeform
                    // cards never appear there, so only note cards match.
                    // `columns` is the (possibly category-expanded,
                    // `kind-column-state-map`) name set: `None` = whole
                    // board; an empty set matches nothing (SQLite rejects
                    // `IN ()`, so compile it to a constant-false term).
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM board_cards b \
                         WHERE b.card_note_path = n.path AND b.board_path = ?",
                    );
                    binds.push(Box::new(board_path.clone()));
                    match columns {
                        Some(cols) if cols.is_empty() => sql.push_str(" AND 0"),
                        Some(cols) => {
                            let placeholders = vec!["?"; cols.len()].join(",");
                            sql.push_str(&format!(" AND b.column_name IN ({placeholders})"));
                            for c in cols {
                                binds.push(Box::new(c.clone()));
                            }
                        }
                        None => {}
                    }
                    sql.push(')');
                }
                MetaFilter::MatchNone => {
                    // Constant-false term (no bind): a predicate-less query
                    // matches nothing rather than the whole vault.
                    sql.push_str(" AND 0");
                }
            }
        }

        if let Some(folder) = &q.folder {
            let prefix = folder.trim_end_matches('/');
            if !prefix.is_empty() {
                sql.push_str(" AND n.path GLOB ?");
                binds.push(Box::new(format!("{prefix}/*")));
            }
        }
        if let Some(glob) = &q.path_glob {
            sql.push_str(" AND n.path GLOB ?");
            binds.push(Box::new(glob.clone()));
        }
        // status: rule-condition-reuses-queries
        if let Some(path) = &q.path_eq {
            sql.push_str(" AND n.path = ?");
            binds.push(Box::new(path.clone()));
        }

        // Meta-keyed ordering uses a correlated scalar subquery so a note
        // missing the key sorts as NULL rather than being dropped.
        match &q.order {
            Some(NoteOrder::Mtime { dir }) => {
                sql.push_str(" ORDER BY n.mtime ");
                sql.push_str(dir_sql(*dir));
            }
            Some(NoteOrder::Path { dir }) => {
                sql.push_str(" ORDER BY n.path ");
                sql.push_str(dir_sql(*dir));
            }
            Some(NoteOrder::MetaNum { key, dir }) => {
                sql.push_str(
                    " ORDER BY (SELECT m.num FROM note_meta m \
                     WHERE m.note_id = n.path AND m.key = ? LIMIT 1) ",
                );
                sql.push_str(dir_sql(*dir));
                binds.push(Box::new(key.clone()));
            }
            Some(NoteOrder::MetaText { key, dir }) => {
                sql.push_str(
                    " ORDER BY (SELECT m.value FROM note_meta m \
                     WHERE m.note_id = n.path AND m.key = ? LIMIT 1) ",
                );
                sql.push_str(dir_sql(*dir));
                binds.push(Box::new(key.clone()));
            }
            None => {}
        }

        if let Some(lim) = q.limit {
            sql.push_str(" LIMIT ?");
            binds.push(Box::new(i64::from(lim)));
        }

        let bind_refs: Vec<&dyn ToSql> = binds.iter().map(std::convert::AsRef::as_ref).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(rusqlite::params_from_iter(bind_refs), |row| {
                let path: String = row.get(1)?;
                let title = title_from_path(&path);
                Ok(NoteQueryRow {
                    note_id: row.get(0)?,
                    path,
                    title,
                    mtime: row.get(2)?,
                    fields: BTreeMap::new(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        if q.select.is_empty() || rows.is_empty() {
            return Ok(rows);
        }
        self.fill_selected_fields(&mut rows, &q.select)?;
        Ok(rows)
    }

    /// Second pass for `query_notes`: pull the `select` keys for the matched
    /// notes in one query and pack them into each row's `fields`. A key with
    /// multiple values (a list / tag set) is joined with `, ` for display.
    fn fill_selected_fields(
        &self,
        rows: &mut [NoteQueryRow],
        select: &[String],
    ) -> Result<(), Error> {
        let ids: Vec<&str> = rows.iter().map(|r| r.note_id.as_str()).collect();
        let id_ph = vec!["?"; ids.len()].join(",");
        let key_ph = vec!["?"; select.len()].join(",");
        let sql = format!(
            "SELECT note_id, key, value FROM note_meta \
             WHERE note_id IN ({id_ph}) AND key IN ({key_ph})"
        );
        let mut binds: Vec<&dyn ToSql> = Vec::with_capacity(ids.len() + select.len());
        for id in &ids {
            binds.push(id);
        }
        for k in select {
            binds.push(k);
        }
        let mut stmt = self.conn.prepare(&sql)?;
        // note_id -> key -> [values]
        let mut acc: HashMap<String, BTreeMap<String, Vec<String>>> = HashMap::new();
        let mut q = stmt.query(rusqlite::params_from_iter(binds))?;
        while let Some(row) = q.next()? {
            let nid: String = row.get(0)?;
            let key: String = row.get(1)?;
            let val: String = row.get(2)?;
            acc.entry(nid).or_default().entry(key).or_default().push(val);
        }
        for r in rows.iter_mut() {
            if let Some(keys) = acc.get(&r.note_id) {
                for (k, vals) in keys {
                    r.fields.insert(k.clone(), vals.join(", "));
                }
            }
        }
        Ok(())
    }

    /// Every indexed `note_meta` row for one note, in key order — the
    /// before-rows read the rule pass diffs across `replace_note_metadata`
    /// to detect `frontmatter-changed` (`docs/rules.md`).
    ///
    /// status: rule-triggers
    pub fn note_metadata(&self, note_id: &str) -> Result<Vec<MetaEntry>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value, num FROM note_meta WHERE note_id = ?1 ORDER BY key, value",
        )?;
        let rows = stmt
            .query_map(params![note_id], |row| {
                Ok(MetaEntry { key: row.get(0)?, value: row.get(1)?, num: row.get(2)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Distinct note paths whose indexed `(key, num)` mirror falls in
    /// `(gt, lte]` — the `date-passed` sweep's "crossings since the last
    /// watermark" read over the epoch mirror (`docs/rules.md`). Exclusive
    /// at the watermark so a crossing fires exactly once.
    ///
    /// status: rule-triggers
    pub fn note_paths_with_meta_num_between(
        &self,
        key: &str,
        gt: f64,
        lte: f64,
    ) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT m.note_id FROM note_meta m
             JOIN notes n ON n.path = m.note_id AND n.skipped = 0
             WHERE m.key = ?1 AND m.num IS NOT NULL AND m.num > ?2 AND m.num <= ?3
             ORDER BY m.note_id",
        )?;
        let rows = stmt
            .query_map(params![key, gt, lte], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One value from the store's `meta(key, value)` sidecar table — the
    /// same row family `chunk_vecs_dim` lives in. Backs the rules layer's
    /// date-sweep watermark.
    ///
    /// status: rule-triggers
    pub fn meta_kv_get(&self, key: &str) -> Result<Option<String>, Error> {
        use rusqlite::OptionalExtension;
        let v = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Upsert one `meta(key, value)` sidecar row.
    ///
    /// status: rule-triggers
    pub fn meta_kv_set(&self, key: &str, value: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// One indexed frontmatter value for `(note_path, key)`, or `None` when
    /// the note carries no such key. A list-valued key collapses to its
    /// first row in `value` order — a deterministic `ORDER BY` so repeated
    /// calls return the same row rather than whichever the engine happens to
    /// surface. Backs the query layer's category expansion (reading a
    /// board-doc's `hiker.kind`, `kind-column-state-map`) and the lenient
    /// validation's ref-target kind check (`kind-lenient-validation`).
    pub fn meta_value(&self, note_path: &str, key: &str) -> Result<Option<String>, Error> {
        use rusqlite::OptionalExtension;
        let v = self
            .conn
            .query_row(
                "SELECT value FROM note_meta WHERE note_id = ?1 AND key = ?2 \
                 ORDER BY value LIMIT 1",
                params![note_path, key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Replace the `note_problems` rows for `note_path` in one transaction.
    /// Called by the indexer right after `replace_note_metadata` — the
    /// lenient-validation report is a derived view, re-derived on ingest
    /// like `note_meta`. An empty slice clears any stale rows (a note that
    /// became clean, or stopped carrying a registered kind).
    ///
    /// status: kind-lenient-validation
    pub fn replace_note_problems(
        &mut self,
        note_path: &str,
        problems: &[NoteProblem],
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM note_problems WHERE note_path = ?1", params![note_path])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO note_problems (note_path, field, message) VALUES (?1, ?2, ?3)",
            )?;
            for p in problems {
                stmt.execute(params![note_path, p.field, p.message])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The lenient-validation problems recorded for one note (empty =
    /// clean or never validated). Backs the per-note badge / report.
    pub fn note_problems(&self, note_path: &str) -> Result<Vec<NoteProblem>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT field, message FROM note_problems WHERE note_path = ?1 ORDER BY field, message",
        )?;
        let rows = stmt
            .query_map(params![note_path], |row| {
                Ok(NoteProblem { field: row.get(0)?, message: row.get(1)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every note carrying validation problems, with its problem count —
    /// the vault-wide problems report / badge data, one indexed query.
    pub fn notes_with_problems(&self) -> Result<Vec<(String, u32)>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT note_path, COUNT(*) FROM note_problems GROUP BY note_path ORDER BY note_path",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?.max(0) as u32))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// True when any indexed row for `key` carries a numeric mirror. One
    /// probe on the `(key, num)` index; the query layer uses it to order a
    /// field by its numeric mirror when present and by its text value
    /// otherwise (`docs/queries.md` §"Filter grammar").
    pub fn meta_key_has_num(&self, key: &str) -> Result<bool, Error> {
        let found: i64 = self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM note_meta WHERE key = ?1 AND num IS NOT NULL)",
            params![key],
            |row| row.get(0),
        )?;
        Ok(found != 0)
    }
}
