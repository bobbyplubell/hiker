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
//! note), and fuses the two ranked lists via RRF (k=60). Callers (the
//! search command, MCP) hand it the query string + modes and get a
//! `SearchResponse` with `lexical_hits`, `semantic_hits`, and `fused`
//! all populated; the frontend renders whichever bucket matches the mode.

use std::collections::HashMap;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::RecencyBias;
use crate::store::{knn_chunks_on, Store, StoreError};
#[cfg(test)]
use crate::store::DEFAULT_EMBED_DIM;

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
    // Kept as a typed shape callers may construct in the future; the live
    // dim check now lives in `knn_chunks_on` and surfaces via `Store`.
    #[allow(dead_code)]
    #[error("embed dim mismatch: got {got}, expected {expected}")]
    EmbedDim { got: usize, expected: usize },
}

/// Per-side option flags surfaced via the right-click options menu on the
/// Lexical (`Aa`) toggle. See `search.md` §"Lexical options menu". All
/// fields default to `false` to preserve current FTS5 behavior.
///
/// status: search-lexical-options
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LexicalOpts {
    pub case_sensitive: bool,
    pub diacritic_sensitive: bool,
    pub prefix_match: bool,
    pub phrase_mode: bool,
    /// Override for the lexical-side `top_k`. `0` defers to
    /// `PER_BACKEND_TOP_K`. Mirrors the existing `SemanticOpts.top_k`.
    pub top_k: u32,
}

/// Per-side option flags for the semantic engine, surfaced via the
/// right-click options menu on the brain toggle.
///
/// status: search-semantic-options
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticOpts {
    /// Cosine-similarity floor; hits below this are dropped before fusion.
    /// status: search-semantic-min-similarity
    pub min_similarity: f32,
    /// Override of `PER_BACKEND_TOP_K` for the semantic side only.
    /// status: search-semantic-top-k-override
    pub top_k: u32,
    /// RRF blend of `notes.mtime` rank into the semantic score.
    /// status: search-semantic-recency-bias
    pub recency_bias: RecencyBias,
}

impl Default for SemanticOpts {
    fn default() -> Self {
        Self {
            min_similarity: 0.0,
            top_k: PER_BACKEND_TOP_K as u32,
            recency_bias: RecencyBias::Off,
        }
    }
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

/// Wire shape returned by the `search_vault` command. We hand back
/// all three buckets (rather than just the one the panel will render)
/// per `search-cmd` — keeps the command flat and enables future
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
    /// Which bucket consumers should render. Both modes on → `fused`
    /// (RRF, capped at FUSED_TOP_K). One mode on → that engine's full
    /// PER_BACKEND_TOP_K-ranked bucket. Both off → empty. Picking lives
    /// in core so the UI, MCP, and CLI never diverge on the rule.
    pub hits: Vec<NoteHit>,
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
    fn query(
        &self,
        q: &str,
        top_k: usize,
        opts: LexicalOpts,
    ) -> Result<Vec<NoteHit>, SearchError>;
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
    fn query(
        &self,
        q: &str,
        top_k: usize,
        opts: LexicalOpts,
    ) -> Result<Vec<NoteHit>, SearchError> {
        if q.trim().is_empty() {
            return Ok(Vec::new());
        }
        // status: search-lexical-phrase-mode, search-lexical-prefix-match
        //
        // Phrase mode wraps the whole query in double quotes for FTS5
        // exact-phrase semantics; prefix match rewrites each token to
        // `token*`. Phrase mode wins when both are set (FTS5 ignores `*`
        // inside a quoted phrase per the spec hint in `search.md`).
        let match_string = build_match_string(q, opts);
        // status: search-fts5-bm25-snippet
        // SQLite's bm25() returns a NEGATIVE-going score (more-negative =
        // better match), so ORDER BY bm25 ASC produces best-first.
        // snippet() column index 0 = the `text` column on chunks_fts.
        // We additionally pull `c.text` so the post-filter pass for
        // `case_sensitive` / `diacritic_sensitive` can run against the raw
        // chunk body rather than the highlighted snippet.
        let sql = format!(
            "SELECT c.id AS chunk_id, c.note_id, n.path, c.chunk_index, c.heading_path,
                    snippet(chunks_fts, 0, '<mark>', '</mark>', '…', {win}) AS snip,
                    c.text AS chunk_text,
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
        let rows = stmt.query_map(params![match_string, candidate_k], |row| {
            let raw_score: f64 = row.get("score")?;
            // Flip sign so higher = better, matching the semantic side.
            // Magnitude isn't comparable across backends — RRF doesn't
            // care about absolute values, only ranks.
            let score = (-raw_score) as f32;
            let chunk_text: String = row.get("chunk_text")?;
            Ok((
                RawChunkHit {
                    chunk_id: row.get("chunk_id")?,
                    note_id: row.get("note_id")?,
                    path: row.get("path")?,
                    chunk_index: row.get::<_, i64>("chunk_index")? as u32,
                    heading_path: row.get("heading_path")?,
                    snippet: row.get("snip")?,
                    score,
                },
                chunk_text,
            ))
        })?;
        let mut chunk_hits: Vec<RawChunkHit> = Vec::new();
        for row in rows {
            let (hit, chunk_text) = row?;
            // status: search-lexical-case-sensitive,
            // search-lexical-diacritic-sensitive
            //
            // Post-filter the FTS5 candidate set against the raw chunk
            // text. FTS5's tokenizer is case-folded + diacritic-stripped at
            // index time, so a per-query toggle of either knob is honored
            // by checking whether the (optionally case-folded) chunk body
            // still contains the user's query verbatim. Diacritic-sensitive
            // falls out for free: a literal substring of a query without
            // diacritics will not appear inside a chunk that carries them
            // (and vice versa) without a normalization pass — so the
            // narrower setting is enforced by exactly the same byte-level
            // contains check as case_sensitive.
            if (opts.case_sensitive || opts.diacritic_sensitive)
                && !chunk_contains(q, &chunk_text, opts)
            {
                continue;
            }
            chunk_hits.push(hit);
        }
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
        // Dim check is owned by `knn_chunks_on`, which reads the live
        // on-disk `chunk_vecs` dim and rejects mismatches there — that's
        // the source of truth after `embedder-dim-from-model`, not a
        // const baked into search.
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

/// Build the FTS5 `MATCH` string from the user's raw query plus the
/// lexical option flags. Phrase mode wraps the whole query in double
/// quotes (FTS5 exact-phrase). Prefix match rewrites each whitespace
/// token to `token*`. Phrase wins over prefix when both are set —
/// FTS5 silently ignores `*` inside a quoted phrase.
fn build_match_string(q: &str, opts: LexicalOpts) -> String {
    let trimmed = q.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if opts.phrase_mode {
        // Escape any embedded double quotes by doubling them per FTS5's
        // string literal rules.
        let escaped = trimmed.replace('"', "\"\"");
        return format!("\"{escaped}\"");
    }
    if opts.prefix_match {
        return trimmed
            .split_whitespace()
            .map(|tok| format!("{tok}*"))
            .collect::<Vec<_>>()
            .join(" ");
    }
    trimmed.to_string()
}

/// Substring check used by the lexical post-filter pass. When
/// `case_sensitive` is off, both sides are lowercased before comparison.
/// Diacritic-sensitivity is handled by the natural byte-level mismatch
/// between accented and unaccented forms — see the call site for why no
/// normalization pass runs here.
fn chunk_contains(query: &str, chunk_text: &str, opts: LexicalOpts) -> bool {
    if opts.case_sensitive {
        chunk_text.contains(query)
    } else {
        chunk_text.to_lowercase().contains(&query.to_lowercase())
    }
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
    lexical_opts: LexicalOpts,
    semantic_opts: SemanticOpts,
) -> Result<SearchResponse, SearchError> {
    let conn = store.open_reader()?;

    let lexical_hits = if modes.lexical {
        let q = lexical_query_text.unwrap_or("");
        let cap = if lexical_opts.top_k == 0 {
            PER_BACKEND_TOP_K
        } else {
            (lexical_opts.top_k as usize).clamp(5, 100)
        };
        Fts5LexicalEngine { conn: &conn }.query(q, cap, lexical_opts)?
    } else {
        Vec::new()
    };

    // Clamp top_k override to [5, 100] per spec. A misconfigured value
    // upstream defaults to PER_BACKEND_TOP_K rather than panicking.
    let semantic_top_k = (semantic_opts.top_k.clamp(5, 100)) as usize;
    let semantic_hits = if modes.semantic {
        match query_embedding {
            Some(emb) => {
                let mut hits = VecSemanticEngine { conn: &conn }.query(emb, semantic_top_k)?;
                // status: search-semantic-min-similarity
                if semantic_opts.min_similarity > 0.0 {
                    hits.retain(|h| h.score >= semantic_opts.min_similarity);
                }
                // status: search-semantic-recency-bias
                if semantic_opts.recency_bias != RecencyBias::Off && !hits.is_empty() {
                    apply_recency_bias(&conn, &mut hits, semantic_opts.recency_bias)?;
                }
                hits
            }
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

    let hits = pick_bucket(&lexical_hits, &semantic_hits, &fused, modes);

    Ok(SearchResponse {
        epoch,
        lexical_hits,
        semantic_hits,
        fused,
        hits,
    })
}

/// Pick the canonical bucket to render given the request's mode flags.
/// Both modes on → `fused` (RRF over the per-backend results). One mode on
/// → that engine's full per-backend ranking (no FUSED_TOP_K truncation, so
/// callers see the same count they'd see if they ran a single-engine
/// query). Both off → empty.
pub fn pick_bucket(
    lexical_hits: &[NoteHit],
    semantic_hits: &[NoteHit],
    fused: &[NoteHit],
    modes: SearchModes,
) -> Vec<NoteHit> {
    if modes.lexical && modes.semantic {
        fused.to_vec()
    } else if modes.lexical {
        lexical_hits.to_vec()
    } else if modes.semantic {
        semantic_hits.to_vec()
    } else {
        Vec::new()
    }
}

/// Blend an `notes.mtime`-derived rank into each semantic hit's score
/// using the same RRF k=60 shape as the cross-mode fusion:
///
/// ```text
/// score' = 1/(k + sim_rank) + w · 1/(k + recency_rank)
/// ```
///
/// where `w` is `0.5` (Mild) or `1.0` (Strong) and `recency_rank` is the
/// note's position when the candidate set is sorted by `notes.mtime
/// DESC`. The semantic rank used is the input ordering: `hits` arrives
/// already sorted best-first by cosine similarity. Mtimes for every hit
/// are pulled in one bulk `IN (...)` query.
///
/// status: search-semantic-recency-bias
fn apply_recency_bias(
    conn: &Connection,
    hits: &mut [NoteHit],
    bias: RecencyBias,
) -> Result<(), SearchError> {
    let weight = bias.weight();
    if weight == 0.0 {
        return Ok(());
    }
    // Pull each candidate's mtime in one query. `IN (?, ?, ?, ...)`.
    let mut placeholders = String::new();
    for i in 0..hits.len() {
        if i > 0 {
            placeholders.push(',');
        }
        placeholders.push('?');
    }
    let sql = format!("SELECT id, mtime FROM notes WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::ToSql> = hits
        .iter()
        .map(|h| &h.note_id as &dyn rusqlite::ToSql)
        .collect();
    let mtime_rows = stmt.query_map(rusqlite::params_from_iter(params_iter), |row| {
        let id: String = row.get(0)?;
        let mtime: i64 = row.get(1)?;
        Ok((id, mtime))
    })?;
    let mut mtimes: HashMap<String, i64> = HashMap::new();
    for r in mtime_rows {
        let (id, m) = r?;
        mtimes.insert(id, m);
    }
    // Recency rank: sort note ids by mtime DESC. Notes missing from the
    // map (shouldn't happen, but guard) tail with mtime=0.
    let mut by_recency: Vec<(&str, i64)> = hits
        .iter()
        .map(|h| (h.note_id.as_str(), *mtimes.get(&h.note_id).unwrap_or(&0)))
        .collect();
    by_recency.sort_by_key(|x| std::cmp::Reverse(x.1));
    let mut recency_rank: HashMap<String, usize> = HashMap::new();
    for (i, (id, _)) in by_recency.iter().enumerate() {
        recency_rank.insert((*id).to_string(), i + 1);
    }
    // Apply the RRF blend in place. `sim_rank` is the input order (1-based).
    let total = hits.len();
    for (i, hit) in hits.iter_mut().enumerate() {
        let sim_rank = (i + 1) as f32;
        let rec_rank = *recency_rank.get(&hit.note_id).unwrap_or(&total) as f32;
        let blended = 1.0 / (RRF_K + sim_rank) + weight * (1.0 / (RRF_K + rec_rank));
        hit.score = blended;
    }
    // Re-sort by the blended score so the panel sees the recency-aware order.
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Ok(())
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
        (0..DEFAULT_EMBED_DIM).map(|i| seed + i as f32 * 0.001).collect()
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
        let hits = engine.query("fox", 10, LexicalOpts::default()).unwrap();
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
        let hits = engine.query("rust", 10, LexicalOpts::default()).unwrap();
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
        let hits = engine.query("hello", 10, LexicalOpts::default()).unwrap();
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
            LexicalOpts::default(),
            SemanticOpts::default(),
        )
        .unwrap();
        assert_eq!(resp.epoch, 7);
        assert!(resp.lexical_hits.is_empty());
        assert!(resp.semantic_hits.is_empty());
        assert!(resp.fused.is_empty());
        assert!(resp.hits.is_empty());
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
            LexicalOpts::default(),
            SemanticOpts::default(),
        )
        .unwrap();
        assert_eq!(resp.epoch, 42);
        assert_eq!(resp.lexical_hits.len(), 1);
        assert!(resp.semantic_hits.is_empty());
        assert_eq!(resp.fused.len(), 1);
        assert_eq!(resp.hits.len(), 1);
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
