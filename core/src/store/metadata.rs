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
    title_from_path, MetaEntry, MetaFilter, NoteOrder, NoteQuery, NoteQueryRow, OrderDir,
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

    /// Structured retrieval over the metadata index. Each filter becomes an
    /// EXISTS subquery against `note_meta`; `folder` a path GLOB; `order` /
    /// `limit` shape the result; skipped notes are excluded. When `select`
    /// is non-empty, the named keys are fetched in a second pass and packed
    /// into each row's `fields`.
    ///
    /// status: store-note-query
    pub fn query_notes(&self, q: &NoteQuery) -> Result<Vec<NoteQueryRow>, Error> {
        let mut sql = String::from("SELECT n.id, n.path, n.mtime FROM notes n WHERE n.skipped = 0");
        // Bound values, in the exact order their `?` appears in `sql`.
        let mut binds: Vec<Box<dyn ToSql>> = Vec::new();

        for f in &q.filters {
            match f {
                MetaFilter::Equals { key, value } => {
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.id AND m.key = ? AND m.value = ?)",
                    );
                    binds.push(Box::new(key.clone()));
                    binds.push(Box::new(value.clone()));
                }
                MetaFilter::Exists { key } => {
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.id AND m.key = ?)",
                    );
                    binds.push(Box::new(key.clone()));
                }
                MetaFilter::NumRange { key, min, max } => {
                    sql.push_str(
                        " AND EXISTS (SELECT 1 FROM note_meta m \
                         WHERE m.note_id = n.id AND m.key = ? AND m.num IS NOT NULL",
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
            }
        }

        if let Some(folder) = &q.folder {
            let prefix = folder.trim_end_matches('/');
            if !prefix.is_empty() {
                sql.push_str(" AND n.path GLOB ?");
                binds.push(Box::new(format!("{prefix}/*")));
            }
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
                     WHERE m.note_id = n.id AND m.key = ? LIMIT 1) ",
                );
                sql.push_str(dir_sql(*dir));
                binds.push(Box::new(key.clone()));
            }
            Some(NoteOrder::MetaText { key, dir }) => {
                sql.push_str(
                    " ORDER BY (SELECT m.value FROM note_meta m \
                     WHERE m.note_id = n.id AND m.key = ? LIMIT 1) ",
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
}
