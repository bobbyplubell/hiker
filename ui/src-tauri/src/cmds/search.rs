//! Search-shaped Tauri commands.
//!
//! `search_vault` — hybrid lexical + semantic search runner, plus
//! `related_notes` (semantic-only nearest-neighbor lookup off the
//! per-note embedding).
//!
//! status: search-tauri-cmd, search-query-embed-spawn-blocking,
//! embedder-first-run-nonblocking

use hiker_core::search::{self, LexicalOpts, SearchModes, SearchResponse, SemanticOpts};
use hiker_core::store::RelatedHit;
use tauri::State;

use crate::{log_cmd_result, AppState};

/// Hybrid search across the vault. Runs the lexical + semantic backends
/// in parallel (per the requested modes) and returns all three buckets
/// (lexical, semantic, fused). The frontend renders whichever matches
/// its toggle state. Empty query, both modes off, or model-not-yet-ready
/// all return empty buckets without erroring — see
/// `embedder-first-run-nonblocking`.
///
/// status: search-tauri-cmd
#[tauri::command]
pub(crate) async fn search_vault(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,
    epoch: u64,
    lexical_opts: Option<LexicalOpts>,
    semantic_opts: Option<SemanticOpts>,
) -> Result<SearchResponse, String> {
    log_cmd_result(
        "search_vault",
        search_vault_inner(
            state,
            query,
            modes,
            epoch,
            lexical_opts.unwrap_or_default(),
            semantic_opts.unwrap_or_default(),
        )
        .await,
    )
}

async fn search_vault_inner(
    state: State<'_, AppState>,
    query: String,
    modes: SearchModes,
    epoch: u64,
    lexical_opts: LexicalOpts,
    semantic_opts: SemanticOpts,
) -> Result<SearchResponse, String> {
    // Empty buckets short-circuit: empty query, both modes off, or no
    // session. Each early-return preserves the echoed `epoch` so the
    // frontend's stale-result check still works.
    if query.trim().is_empty() || (!modes.lexical && !modes.semantic) {
        return Ok(SearchResponse {
            epoch,
            lexical_hits: Vec::new(),
            semantic_hits: Vec::new(),
            fused: Vec::new(),
            hits: Vec::new(),
        });
    }
    // Embed the query string (only when semantic is on) on the blocking
    // pool, off the loaded indexer embedder. Per
    // `search-query-embed-spawn-blocking`. Skip entirely when the model
    // isn't ready — search returns empty rather than blocking.
    let embedding: Option<Vec<f32>> = if modes.semantic {
        let embedder = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            session.indexer.embedder()
        };
        match embedder {
            Some(e) => {
                let q = query.clone();
                let res = tokio::task::spawn_blocking(move || e.embed_batch(&[q]))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                res.into_iter().next()
            }
            None => None,
        }
    } else {
        None
    };

    // If semantic was requested but embedding isn't available, run lexical
    // only (still return the requested-modes shape so the panel knows to
    // fall back). Mirrors the spec's "search returns empty with indicator
    // until first batch completes" but keeps any usable hit visible.
    let effective_modes = SearchModes {
        lexical: modes.lexical,
        semantic: modes.semantic && embedding.is_some(),
    };

    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    let read_store = session
        .read_store
        .lock()
        .map_err(|_| "read_store mutex poisoned".to_string())?;
    search::query(
        &read_store,
        epoch,
        effective_modes,
        Some(&query),
        embedding.as_deref(),
        lexical_opts,
        semantic_opts,
    )
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub(crate) fn related_notes(
    state: State<AppState>,
    rel: String,
    top_k: Option<usize>,
) -> Result<Vec<RelatedHit>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let id = match read_store.id_for_path(&rel).map_err(|e| e.to_string())? {
            Some(id) => id,
            None => return Ok(Vec::new()),
        };
        read_store
            .related_notes(&id, top_k.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("related_notes", result)
}
