//! The read-only query surface that PKM features consume — the "indexer
//! service" boundary (the platform service other features resolve, akin to an
//! LSP for language features). Implemented by [`Store`]; feature logic depends
//! on this trait rather than the concrete store, so each feature can later
//! become a self-contained extension that knows only the indexer's API and not
//! the SQLite store behind it.
//!
//! The trait is intentionally object-safe (`&dyn IndexerQueryApi`) so a feature
//! helper can take the query surface without a generic parameter. Method names
//! and signatures mirror the inherent `Store` methods one-to-one; the impl
//! delegates, so the inherent methods stay the single source of truth and
//! method resolution on a concrete `Store` is unchanged (inherent wins).
//!
//! See `scratch/potential_oplog_refactor.md` (workbench follow-on, gap #2).

use super::dto::{
    BoardContainingHit, NoteProperties, NoteQuery, NoteQueryRow, NoteRow, RelatedHit,
    TrailContainingHit,
};
use super::error::Error;
use super::Store;

/// Read-only queries against the index. The whole of what a feature needs to
/// ask the indexer; mutations (reindex) and the write path stay on `Store`.
pub trait IndexerQueryApi {
    /// The indexed row for a path, or `None` if never indexed.
    fn get_note_by_path(&self, rel_path: &str) -> Result<Option<NoteRow>, Error>;

    /// Everything the index knows about one note (hashes, mtime, skip state).
    fn note_properties(&self, rel_path: &str) -> Result<Option<NoteProperties>, Error>;

    /// Top-`k` vector-similarity neighbours of a note id.
    fn related_notes(&self, source_note_id: &str, top_k: usize) -> Result<Vec<RelatedHit>, Error>;

    /// Metadata query over the notes table (used by cluster presets).
    fn query_notes(&self, q: &NoteQuery) -> Result<Vec<NoteQueryRow>, Error>;

    /// Trails whose waypoints include this note.
    fn trails_containing_note(&self, note_path: &str) -> Result<Vec<TrailContainingHit>, Error>;

    /// Boards whose columns include this note.
    fn boards_containing_note(&self, note_path: &str) -> Result<Vec<BoardContainingHit>, Error>;
}

impl IndexerQueryApi for Store {
    fn get_note_by_path(&self, rel_path: &str) -> Result<Option<NoteRow>, Error> {
        Store::get_note_by_path(self, rel_path)
    }

    fn note_properties(&self, rel_path: &str) -> Result<Option<NoteProperties>, Error> {
        Store::note_properties(self, rel_path)
    }

    fn related_notes(&self, source_note_id: &str, top_k: usize) -> Result<Vec<RelatedHit>, Error> {
        Store::related_notes(self, source_note_id, top_k)
    }

    fn query_notes(&self, q: &NoteQuery) -> Result<Vec<NoteQueryRow>, Error> {
        Store::query_notes(self, q)
    }

    fn trails_containing_note(&self, note_path: &str) -> Result<Vec<TrailContainingHit>, Error> {
        Store::trails_containing_note(self, note_path)
    }

    fn boards_containing_note(&self, note_path: &str) -> Result<Vec<BoardContainingHit>, Error> {
        Store::boards_containing_note(self, note_path)
    }
}
