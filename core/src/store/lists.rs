//! Derived `list_refs` index queries (`docs/pm.md`): membership edges of
//! list-like notes (epics, plans, any registered list-like kind). Mirrors
//! `store::boards`: re-derived from each list-doc's `hiker.refs`
//! frontmatter on ingest (clear-by-list + re-insert), cleared on list-doc
//! delete, re-keyed on rename, fail-loud on schema bump like every other
//! derived table. The table is pure structure — what a membership *means*
//! stays on the kind.
//
// status: pm-epic-derived-table

use rusqlite::params;

use super::dto::ListRefRow;
use super::error::Error;
use super::Store;

impl Store {
    /// Clear every row for `list_path`, then insert one row per member in
    /// order. The board-doc ingest path uses the same clear-then-reinsert
    /// shape so the derived table stays consistent with the list-doc's
    /// `hiker.refs` array (frontmatter is the source of truth).
    ///
    /// status: pm-epic-derived-table
    pub fn replace_list_refs(
        &mut self,
        list_path: &str,
        members: &[String],
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM list_refs WHERE list_path = ?1", params![list_path])?;
        for (position, member) in members.iter().enumerate() {
            tx.execute(
                "INSERT INTO list_refs (list_path, member_path, position)
                 VALUES (?1, ?2, ?3)",
                params![list_path, member, position as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every row tied to `list_path`. Used on list-doc delete.
    ///
    /// status: pm-epic-derived-table
    pub fn delete_list_refs_by_list(&mut self, list_path: &str) -> Result<usize, Error> {
        let n = self.conn.execute(
            "DELETE FROM list_refs WHERE list_path = ?1",
            params![list_path],
        )?;
        Ok(n)
    }

    /// All member rows of a given list, ordered by `position` — the member
    /// set `epic_progress` rolls up and a plan's presentation order.
    ///
    /// status: pm-epic-derived-table
    pub fn members_of(&self, list_path: &str) -> Result<Vec<ListRefRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT list_path, member_path, position
             FROM list_refs
             WHERE list_path = ?1
             ORDER BY position",
        )?;
        let rows = stmt
            .query_map(params![list_path], |row| {
                Ok(ListRefRow {
                    list_path: row.get(0)?,
                    member_path: row.get(1)?,
                    position: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every membership row across all lists, ordered by `(list_path,
    /// position)`. Backs the vault graph's list-membership edge set
    /// (`vault-graph-typed-edges`, Phase D): the build unions every
    /// list-doc's doc→member edges (epics, plans, any registered list-like
    /// kind) in one pass and so wants the whole table at once rather than a
    /// per-list query per rebuild (mirrors `all_board_cards`).
    ///
    /// status: vault-graph-typed-edges
    pub fn all_list_refs(&self) -> Result<Vec<ListRefRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT list_path, member_path, position
             FROM list_refs
             ORDER BY list_path, position",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ListRefRow {
                    list_path: row.get(0)?,
                    member_path: row.get(1)?,
                    position: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Lists that reference a given note as a member, matched on the
    /// note's vault-relative path (path-as-identity) — the "epics
    /// containing this story" reverse lookup, and the membership read plan
    /// resolution goes through.
    ///
    /// status: pm-epic-derived-table
    pub fn lists_containing_note(&self, note_path: &str) -> Result<Vec<ListRefRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT list_path, member_path, position
             FROM list_refs
             WHERE member_path = ?1
             ORDER BY list_path, position",
        )?;
        let rows = stmt
            .query_map(params![note_path], |row| {
                Ok(ListRefRow {
                    list_path: row.get(0)?,
                    member_path: row.get(1)?,
                    position: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rewrite the `member_path` of every row pointing at `old_path` to
    /// `new_path` — the auto-update-on-move path when a *member note*
    /// moves, so the derived table tracks the new path before the next
    /// list-doc ingest re-derives it.
    ///
    /// status: pm-epic-derived-table
    pub fn rename_list_ref_member_paths(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "UPDATE list_refs SET member_path = ?1 WHERE member_path = ?2",
            params![new_path, old_path],
        )?;
        Ok(n)
    }

    /// Rewrite the `list_path` of every row for the list-doc that moved
    /// from `old_path` to `new_path` (the list-doc itself moved).
    ///
    /// status: pm-epic-derived-table
    pub fn rename_list_refs_for_list(
        &mut self,
        old_path: &str,
        new_path: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "UPDATE list_refs SET list_path = ?1 WHERE list_path = ?2",
            params![new_path, old_path],
        )?;
        Ok(n)
    }
}
