//! Spec-anchor index: the write side that re-derives `spec_anchors` from a note's bare
//! `[slug]` tokens on ingest, and the lookup that resolves a `[[spec:slug]]` link
//! (`wikilink-spec-links`) to the notes defining that anchor — one indexed query instead
//! of a vault walk.
//!
//! All SQL stays here (`store-module-discipline`). Lifecycle mirrors `note_meta`:
//! re-derived on every ingest (`indexer::jobs::process_upsert`), cleared on skip /
//! delete, re-keyed on rename (`notes.rs`).
//
// status: spec-anchor-index

use rusqlite::params;

use super::error::Error;
use super::Store;

impl Store {
    /// Replace the `spec_anchors` rows for `note_path` with `slugs` in one transaction —
    /// the per-note re-derive on ingest. Duplicate slugs in the input collapse via the
    /// `(slug, note_path)` primary key.
    pub fn replace_spec_anchors(
        &mut self,
        note_path: &str,
        slugs: &[String],
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM spec_anchors WHERE note_path = ?1", params![note_path])?;
        for slug in slugs {
            tx.execute(
                "INSERT OR IGNORE INTO spec_anchors (slug, note_path) VALUES (?1, ?2)",
                params![slug, note_path],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The whole anchor table as `(slug, note_path)` rows, ordered by
    /// `(slug, note_path)`. Backs the vault graph's spec-reference edge set
    /// (`vault-graph-spec-edges`): the build resolves every `[[spec:slug]]`
    /// body link against the full slug → defining-notes map in one pass, and
    /// types anchor-defining notes, so it wants the whole table at once
    /// rather than a per-slug query per link (mirrors `all_board_cards`).
    ///
    /// status: vault-graph-spec-edges
    pub fn all_spec_anchors(&self) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT slug, note_path FROM spec_anchors ORDER BY slug, note_path")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every note path defining the `[slug]` anchor, sorted — the read side of
    /// `[[spec:slug]]` resolution. Usually one path; multiple when a slug is anchored in
    /// more than one note (the caller picks, e.g. preferring the referrer's folder).
    pub fn spec_anchor_paths(&self, slug: &str) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT note_path FROM spec_anchors WHERE slug = ?1 ORDER BY note_path")?;
        let rows = stmt.query_map(params![slug], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}
