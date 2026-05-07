//! Vault-wide hybrid search: lexical (FTS5) + semantic (sqlite-vec) with
//! reciprocal rank fusion. See `docs/search.md`.
//!
//! status: search-engine-trait
//!
//! Both backends are trait-bounded so a future tantivy swap-in
//! (`search-tantivy-swap`) is a single new file. Both concrete impls are
//! thin wrappers over rusqlite running against the same `index.db` —
//! lexical queries the new `chunks_fts` virtual table; semantic reuses
//! the existing `chunk_vecs` table populated by the indexer.
//!
//! The top-level entry is `query()`. It composes the two engines based on
//! `SearchModes`, groups chunk-level hits by note (best chunk wins per
//! note), and fuses the two ranked lists via RRF (k=60). Callers (Tauri
//! command, future MCP) hand it the query string + modes and get a
//! `SearchResponse` with `lexical_hits`, `semantic_hits`, and `fused`
//! all populated; the frontend renders whichever bucket matches the mode.

use std::collections::HashMap;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::store::{knn_chunks_on, Store, StoreError, EMBED_DIM};

/// Per-backend top-k pulled internally before fusion. Spec: 25 per backend
/// gives RRF a tail of below-the-fold candidates from each side without
/// exploding payload size. `search-result-budget`.
pub const PER_BACKEND_TOP_K: usize = 25;

/// Top-k notes returned in the fused list (and rendered in the panel).
/// `search-result-budget`.
pub const FUSED_TOP_K: usize = 20;

/// RRF k constant. Standard choice; works without per-backend score
/// normalization (BM25 and cosine aren't on the same scale). `search-rrf-fusion`.
const RRF_K: f32 = 60.0;

/// Snippet window for FTS5's `snippet()` aux function (in tokens).
const SNIPPET_WINDOW: usize = 32;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("embed dim mismatch: got {got}, expected {expected}")]
    EmbedDim { got: usize, expected: usize },
}

/// Which backends should run for this query. Both true = hybrid via RRF;
/// one true = native ranking from that backend, no fusion. Both false is
/// rejected at the UI layer (`search-modes-both-off-disabled`); if a
/// caller ever passes both-false the response is simply empty.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SearchModes {
    pub semantic: bool,
    pub lexical: bool,
}

/// One note-level hit. The `chunk_id` / `heading_path` / `snippet` come
/// from the highest-ranked chunk that matched within the note —
/// `search-result-grouped-by-note`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteHit {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub score: f32,
    pub chunk_id: String,
    pub chunk_index: u32,
    pub heading_path: Option<String>,
    /// May contain `<mark>...</mark>` for lexical hits (FTS5 `snippet()`);
    /// plain text for semantic-only and fused-via-semantic hits. The
    /// frontend renders `<mark>` as a styled span — never via raw HTML.
    pub snippet: String,
}

/// Wire shape returned by the `search_vault` Tauri command. We hand back
/// all three buckets (rather than just the one the panel will render)
/// per `search-tauri-cmd` — keeps the command flat and enables future
/// "show what each backend found separately" affordances without a new
/// command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    /// Echoed back so the frontend can drop stale results
    /// (`search-typeahead-debounce`).
    pub epoch: u64,
    pub lexical_hits: Vec<NoteHit>,
    pub semantic_hits: Vec<NoteHit>,
    pub fused: Vec<NoteHit>,
}

/// Lexical search. `Fts5LexicalEngine` is the v2 concrete impl; tantivy
/// would be a sibling module behind the same trait.
///
/// Writes happen via the FTS5 sync triggers on `chunks` (declared in
/// `Store::ensure_schema`), so this trait is read-only at v2; future
/// engines without trigger-based sync would need their own writer hooks.
/// Constructed per-call against a borrowed read connection (cheap
/// open + sqlite-vec auto-extension is process-once); no `Send + Sync`
/// bound — engine values don't outlive a single query.
pub trait LexicalEngine {
    fn query(&self, q: &str, top_k: usize) -> Result<Vec<NoteHit>, SearchError>;
    fn version(&self) -> &str;
}

/// Semantic search over a precomputed embedding. The query string's
/// embedding is produced by the caller (via the shared Embedder + a
/// `spawn_blocking` hop per `search-query-embed-spawn-blocking`); this
/// trait operates on the resulting vector so the implementation stays
/// embedder-agnostic.
pub trait SemanticEngine {
    fn query(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<NoteHit>, SearchError>;
    fn version(&self) -> &str;
}

/// Concrete FTS5-backed lexical engine. status: search-fts5-lexical
pub struct Fts5LexicalEngine<'a> {
    pub conn: &'a Connection,
}

impl<'a> LexicalEngine for Fts5LexicalEngine<'a> {
    fn query(&self, q: &str, top_k: usize) -> Result<Vec<NoteHit>, SearchError> {
        if q.trim().is_empty() {
            return Ok(Vec::new());
        }
        // status: search-fts5-bm25-snippet
        // SQLite's bm25() returns a NEGATIVE-going score (more-negative =
        // better match), so ORDER BY bm25 ASC produces best-first.
        // snippet() column index 0 = the `text` column on chunks_fts.
        let sql = format!(
            "SELECT c.id AS chunk_id, c.note_id, n.path, c.chunk_index, c.heading_path,
                    snippet(chunks_fts, 0, '<mark>', '</mark>', '…', {win}) AS snip,
                    bm25(chunks_fts) AS score
             FROM chunks_fts
             JOIN chunks c ON c.rowid = chunks_fts.rowid
             JOIN notes  n ON n.id    = c.note_id
             WHERE chunks_fts MATCH ?1
               AND n.skipped = 0
             ORDER BY score
             LIMIT ?2",
            win = SNIPPET_WINDOW,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        // Pull a wider candidate set so the per-note dedupe below still
        // returns top_k distinct notes even when one note dominates the
        // raw chunk-level results.
        let candidate_k = top_k.saturating_mul(4).max(top_k + 16) as i64;
        let rows = stmt.query_map(params![q, candidate_k], |row| {
            let raw_score: f64 = row.get("score")?;
            // Flip sign so higher = better, matching the semantic side.
            // Magnitude isn't comparable across backends — RRF doesn't
            // care about absolute values, only ranks.
            let score = (-raw_score) as f32;
            Ok(RawChunkHit {
                chunk_id: row.get("chunk_id")?,
                note_id: row.get("note_id")?,
                path: row.get("path")?,
                chunk_index: row.get::<_, i64>("chunk_index")? as u32,
                heading_path: row.get("heading_path")?,
                snippet: row.get("snip")?,
                score,
            })
        })?;
        let chunk_hits: Vec<RawChunkHit> = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(group_by_note(chunk_hits, top_k))
    }

    fn version(&self) -> &str {
        // FTS5's behavior is keyed on the configured tokenizer (declared in
        // ensure_schema). Bumping this would require a rebuild of the
        // virtual table, which today is part of the schema-version bump.
        "fts5-unicode61-rd2"
    }
}

/// Concrete semantic engine reusing the existing `chunk_vecs` table.
/// status: search-semantic-existing-vecs
pub struct VecSemanticEngine<'a> {
    pub conn: &'a Connection,
}

impl<'a> SemanticEngine for VecSemanticEngine<'a> {
    fn query(
        &self,
        embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<NoteHit>, SearchError> {
        if embedding.len() != EMBED_DIM {
            return Err(SearchError::EmbedDim {
                got: embedding.len(),
                expected: EMBED_DIM,
            });
        }
        // Pull a wider candidate set so per-note dedupe still produces top_k
        // distinct notes. knn_chunks_on already over-pulls (×4) internally,
        // but here we want note-level top_k, not chunk-level.
        let candidate_k = top_k.saturating_mul(4).max(top_k + 16);
        let chunk_hits = knn_chunks_on(self.conn, embedding, candidate_k, None)?;
        // Re-shape into RawChunkHit; chunk_index isn't on ChunkHit so we
        // pull it from the chunk_id (`<note_id>:<idx>`), matching the
        // format `Store::upsert_note` writes.
        let raws: Vec<RawChunkHit> = chunk_hits
            .into_iter()
            .map(|h| {
                let chunk_index = h
                    .chunk_id
                    .rsplit(':')
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                RawChunkHit {
                    chunk_id: h.chunk_id,
                    note_id: h.note_id,
                    path: h.note_path,
                    chunk_index,
                    heading_path: h.heading_path,
                    snippet: snippet_from_text(&h.text),
                    score: h.score,
                }
            })
            .collect();
        Ok(group_by_note(raws, top_k))
    }

    fn version(&self) -> &str {
        // Semantic ranking is purely a function of stored embeddings; the
        // model identity that produced them is on each `notes` row. No
        // engine-side version to track.
        "vec0"
    }
}

struct RawChunkHit {
    chunk_id: String,
    note_id: String,
    path: String,
    chunk_index: u32,
    heading_path: Option<String>,
    snippet: String,
    score: f32,
}

/// Reduce chunk-level hits to one row per note, keeping the highest-scoring
/// chunk as the note's representative. Preserves the input order for ties
/// (which is the engine's native ranking — BM25 ascending or cosine
/// descending after sign-flip), so a stable rank index falls out for free.
fn group_by_note(chunks: Vec<RawChunkHit>, top_k: usize) -> Vec<NoteHit> {
    let mut best: HashMap<String, RawChunkHit> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for ch in chunks {
        match best.get(&ch.note_id) {
            Some(cur) if cur.score >= ch.score => {}
            _ => {
                if !best.contains_key(&ch.note_id) {
                    order.push(ch.note_id.clone());
                }
                best.insert(ch.note_id.clone(), ch);
            }
        }
    }
    let mut hits: Vec<NoteHit> = order
        .into_iter()
        .filter_map(|id| best.remove(&id))
        .map(|c| NoteHit {
            note_id: c.note_id,
            title: title_from_path(&c.path),
            path: c.path,
            score: c.score,
            chunk_id: c.chunk_id,
            chunk_index: c.chunk_index,
            heading_path: c.heading_path,
            snippet: c.snippet,
        })
        .collect();
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top_k);
    hits
}

/// Reciprocal rank fusion (k=60) over two ranked, per-note lists.
/// Group-by-note happens before fusion (each input is already deduped).
/// The chunk shown in the fused row is the best chunk from whichever
/// backend ranked the note highest. status: search-rrf-fusion
fn rrf_fuse(
    lexical: &[NoteHit],
    semantic: &[NoteHit],
    top_k: usize,
) -> Vec<NoteHit> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut best: HashMap<String, (NoteHit, f32)> = HashMap::new();

    let mut record = |hits: &[NoteHit]| {
        for (rank, hit) in hits.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + (rank as f32 + 1.0));
            *scores.entry(hit.note_id.clone()).or_insert(0.0) += contribution;
            // Track which side gave the better-ranked appearance — that's
            // whose snippet/heading we surface.
            best.entry(hit.note_id.clone())
                .and_modify(|(cur, cur_contribution)| {
                    if contribution > *cur_contribution {
                        *cur = hit.clone();
                        *cur_contribution = contribution;
                    }
                })
                .or_insert_with(|| (hit.clone(), contribution));
        }
    };
    record(lexical);
    record(semantic);

    let mut fused: Vec<(String, f32)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
        .into_iter()
        .take(top_k)
        .filter_map(|(note_id, fused_score)| {
            best.remove(&note_id).map(|(mut hit, _)| {
                hit.score = fused_score;
                hit
            })
        })
        .collect()
}

/// Top-level entry. `lexical_query_text` and `query_embedding` are the
/// caller's responsibility; embedding is done in a `spawn_blocking` hop
/// outside the store's connection pool per
/// `search-query-embed-spawn-blocking`. Either may be `None` when the
/// corresponding mode is off; both off returns an empty response.
pub fn query(
    store: &Store,
    epoch: u64,
    modes: SearchModes,
    lexical_query_text: Option<&str>,
    query_embedding: Option<&[f32]>,
) -> Result<SearchResponse, SearchError> {
    let conn = store.open_reader()?;

    let lexical_hits = if modes.lexical {
        let q = lexical_query_text.unwrap_or("");
        Fts5LexicalEngine { conn: &conn }.query(q, PER_BACKEND_TOP_K)?
    } else {
        Vec::new()
    };

    let semantic_hits = if modes.semantic {
        match query_embedding {
            Some(emb) => VecSemanticEngine { conn: &conn }.query(emb, PER_BACKEND_TOP_K)?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let fused = if modes.lexical && modes.semantic {
        rrf_fuse(&lexical_hits, &semantic_hits, FUSED_TOP_K)
    } else if modes.lexical {
        lexical_hits.iter().take(FUSED_TOP_K).cloned().collect()
    } else if modes.semantic {
        semantic_hits.iter().take(FUSED_TOP_K).cloned().collect()
    } else {
        Vec::new()
    };

    Ok(SearchResponse {
        epoch,
        lexical_hits,
        semantic_hits,
        fused,
    })
}

fn title_from_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let stem = last.strip_suffix(".md").unwrap_or(last);
    if stem.is_empty() {
        "Untitled".into()
    } else {
        stem.to_string()
    }
}

fn snippet_from_text(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= 200 {
        collapsed
    } else {
        let cutoff = collapsed
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
        format!("{}…", &collapsed[..cutoff])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::Chunk;
    use crate::store::{new_id, NoteUpsert};
    use tempfile::tempdir;

    fn unit_vec(seed: f32) -> Vec<f32> {
        (0..EMBED_DIM).map(|i| seed + i as f32 * 0.001).collect()
    }

    fn mk_chunk(idx: u32, text: &str) -> Chunk {
        Chunk {
            index: idx,
            byte_start: 0,
            byte_end: text.len(),
            text: text.to_string(),
            heading_path: None,
        }
    }

    fn fresh_store() -> (tempfile::TempDir, Store) {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn fts5_finds_lexical_match_and_renders_mark() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "alpha.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![
                    (mk_chunk(0, "the quick brown fox"), unit_vec(0.0)),
                    (mk_chunk(1, "lazy dog jumps over"), unit_vec(1.0)),
                ],
            })
            .unwrap();

        let conn = store.open_reader().unwrap();
        let engine = Fts5LexicalEngine { conn: &conn };
        let hits = engine.query("fox", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].note_id, id);
        assert!(hits[0].snippet.contains("<mark>fox</mark>"));
        assert_eq!(hits[0].chunk_index, 0);
    }

    #[test]
    fn fts5_groups_by_note_keeping_best_chunk() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "a.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![
                    (mk_chunk(0, "rust rust rust rust"), unit_vec(0.0)),
                    (mk_chunk(1, "rust"), unit_vec(1.0)),
                ],
            })
            .unwrap();

        let conn = store.open_reader().unwrap();
        let engine = Fts5LexicalEngine { conn: &conn };
        let hits = engine.query("rust", 10).unwrap();
        assert_eq!(hits.len(), 1, "two chunks of the same note must collapse");
    }

    #[test]
    fn fts5_excludes_skipped_notes() {
        let (_dir, mut store) = fresh_store();
        store.upsert_skipped("big.md", "file too large", 1, 1).unwrap();

        // Indexed counterpart so the test confirms the skipped one is the
        // only thing being filtered, not "everything is empty."
        let id_ok = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id_ok,
                path: "ok.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "hello world"), unit_vec(0.0))],
            })
            .unwrap();

        let conn = store.open_reader().unwrap();
        let engine = Fts5LexicalEngine { conn: &conn };
        let hits = engine.query("hello", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "ok.md");
    }

    #[test]
    fn semantic_engine_returns_note_hits() {
        let (_dir, mut store) = fresh_store();
        let id_a = new_id();
        let id_b = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id_a,
                path: "a.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "a-body"), unit_vec(0.0))],
            })
            .unwrap();
        store
            .upsert_note(NoteUpsert {
                id: &id_b,
                path: "b.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "b-body"), unit_vec(50.0))],
            })
            .unwrap();

        let conn = store.open_reader().unwrap();
        let engine = VecSemanticEngine { conn: &conn };
        let hits = engine.query(&unit_vec(0.0), 10).unwrap();
        assert!(hits.len() >= 2);
        assert_eq!(hits[0].note_id, id_a);
    }

    #[test]
    fn rrf_fusion_promotes_notes_ranked_well_by_both() {
        let lex = vec![
            mk_hit("n1", 10.0),
            mk_hit("n2", 9.0),
            mk_hit("n3", 8.0),
        ];
        let sem = vec![
            mk_hit("n3", 0.9),
            mk_hit("n1", 0.8),
            mk_hit("n4", 0.7),
        ];
        let fused = rrf_fuse(&lex, &sem, 10);
        // n1 (1+2) and n3 (3+1) both appear high in both lists; n2 only in
        // lex, n4 only in sem. Ordering details depend on RRF; assert n1 and
        // n3 outrank n2 and n4.
        let pos = |id: &str| fused.iter().position(|h| h.note_id == id).unwrap();
        assert!(pos("n1") < pos("n2"));
        assert!(pos("n3") < pos("n4"));
    }

    #[test]
    fn query_both_modes_off_returns_empty() {
        let (_dir, store) = fresh_store();
        let resp = query(
            &store,
            7,
            SearchModes { lexical: false, semantic: false },
            Some("anything"),
            Some(&unit_vec(0.0)),
        )
        .unwrap();
        assert_eq!(resp.epoch, 7);
        assert!(resp.lexical_hits.is_empty());
        assert!(resp.semantic_hits.is_empty());
        assert!(resp.fused.is_empty());
    }

    #[test]
    fn query_lexical_only_skips_embedding() {
        let (_dir, mut store) = fresh_store();
        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "n.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![(mk_chunk(0, "needle haystack"), unit_vec(0.0))],
            })
            .unwrap();
        let resp = query(
            &store,
            42,
            SearchModes { lexical: true, semantic: false },
            Some("needle"),
            None, // no embedding required when semantic is off
        )
        .unwrap();
        assert_eq!(resp.epoch, 42);
        assert_eq!(resp.lexical_hits.len(), 1);
        assert!(resp.semantic_hits.is_empty());
        assert_eq!(resp.fused.len(), 1);
    }

    fn mk_hit(id: &str, score: f32) -> NoteHit {
        NoteHit {
            note_id: id.into(),
            path: format!("{id}.md"),
            title: id.into(),
            score,
            chunk_id: format!("{id}:0"),
            chunk_index: 0,
            heading_path: None,
            snippet: "".into(),
        }
    }
}
