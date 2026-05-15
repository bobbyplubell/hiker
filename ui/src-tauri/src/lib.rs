mod chat;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use notify::{Event as NotifyEvent, EventKind as NotifyEventKind, Watcher as NotifyWatcher, RecursiveMode};

use hiker_core::autosave::{Autosave, RecoveredEntry, TabState};
use hiker_core::activity::{Activity, ActivityFilter, ActivityItem, ActivitySource};
use hiker_core::changes::{ChangeOp, ChangeRow, Changes};
use hiker_core::config::{Config, SettingsScope, TreeSortBy};
use hiker_core::indexer::{
    route_watcher_events, start_indexer, IndexJob, IndexStatus, IndexerHandle, ProgressEvent,
};
use hiker_core::search::{self, LexicalOpts, SearchModes, SearchResponse, SemanticOpts};
use hiker_core::staging::{Staging, AcceptOutcome, Proposal, StagingFilter};
use hiker_core::store::{ChunkBounds, RecentNote, RelatedHit, Store, VaultStats};
use hiker_core::trash::{Trash, TrashEntry, TrashListItem};
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, DirEntryDto, HikerError, Vault};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State};

/// All long-lived state for an open vault. Constructed in `open_vault_at`,
/// dropped on swap.
pub(crate) struct VaultSession {
    vault: Vault,
    pub(crate) root: PathBuf,
    indexer: IndexerHandle,
    /// Held to keep the watcher alive; dropping this closes the broadcast.
    /// Also referenced by `create_note` / `move_note` to register self-write
    /// suppression around fs mutations. Wrapped in `Arc` so the mutating
    /// commands (which call `core::ops::*`) can clone a cheap handle out
    /// from under the session lock and pass it across the indexer-reply
    /// `.await` without holding the sync mutex across it.
    watcher: Arc<Watcher>,
    /// status: changes-write-path
    /// Append-only changelog. Shared writer (single mutex inside `Changes`)
    /// across every mutating command site so all writes flow into one file.
    /// Subscribed by a tokio task that re-emits each append as
    /// `hiker:changes-appended` for the home-page activity widget.
    changes: Arc<Changes>,
    /// Staging area for proposed writes (see docs/settings.md "## Staging
    /// review"). Created at vault open, passed to the MCP server so write
    /// tools can route proposals through it when `[mcp.tools].review_required`
    /// is true.
    ///
    /// status: agent-write-review-mode
    pub(crate) staging: Arc<Staging>,
    /// status: trees-db
    /// Owner of `vault/.hiker/trees.db` — cluster trees, nodes,
    /// history. Backs every `cluster_*` Tauri command for the cluster
    /// editor surface.
    pub(crate) trees: Arc<hiker_core::trees::Trees>,
    /// status: activity-feed-module
    /// Pure projection over `changes` + `staging`. Constructed at vault
    /// open; no on-disk state of its own. Backs `activity_list` /
    /// `activity_list_for_path` / `activity_count`.
    pub(crate) activity: Arc<Activity>,
    /// status: autosave-backend-module
    /// Owns all `<vault>/.hiker/autosave/` writes and recovery. Same
    /// module-discipline shape as `core::store` / `core::changes` —
    /// every Tauri `autosave_*` command wraps a 5–15 line call into
    /// this handle.
    autosave: Arc<Autosave>,
    /// status: settings-load-once-at-startup
    /// Frozen merged user+vault settings. `set_setting` writes through to
    /// disk via `Config::set` and swaps the in-memory copy in this RwLock.
    pub(crate) config: RwLock<Config>,
    /// Long-lived read-side store handle. The indexer task owns the writer
    /// connection (one per vault); this is a *second* connection against
    /// the same on-disk db, used by every read-side Tauri command
    /// (`index_state_for`, `chunks_for`, `related_notes`) so they don't
    /// pay sqlite/PRAGMA/sqlite-vec setup cost on every call.
    ///
    /// Safe to coexist with the writer: WAL mode is per-file, the
    /// sqlite-vec auto-extension is registered process-once via `Once`,
    /// and `ensure_schema` is idempotent.
    ///
    /// Wrapped in `Mutex` because `rusqlite::Connection` is `Send` but not
    /// `Sync`. Read calls are sub-millisecond so serializing them through
    /// the mutex is fine; if read concurrency ever matters, swap this for
    /// an `r2d2` pool — `core::store` confines all SQL so the change is
    /// local.
    ///
    /// Convention only: nothing in the type prevents a writer call. Read
    /// commands stick to the `&self` methods on `Store`.
    read_store: Arc<Mutex<Store>>,
    /// status: mcp-server-crate
    /// In-process MCP server task — bound on vault open, dropped on swap.
    /// `None` when the vault is opened with `[mcp] enabled = false` or when
    /// the bind failed (logged but non-fatal — vault open still succeeds).
    /// Held purely for its `Drop` side effect (cancel the task + remove the
    /// discovery file); never read directly.
    pub(crate) mcp: Option<hiker_mcp::McpServerHandle>,
    /// status: agent-chat-command-surface
    /// Per-turn live state for the basic agent loop (`core::agent`). See
    /// `chat.rs`. Outlives any single `chat_send`/`chat_continue` call so
    /// cap-hit pauses can be resumed mid-session.
    pub(crate) chat: Arc<chat::ChatRegistry>,
    /// status: llm-prompts-file-store
    /// Loaded once at vault open and shared across every chat turn so we
    /// don't re-read disk on every `chat_send`. The user-edited prompt
    /// file is the authoritative surface; relaunching hiker picks up
    /// changes (matches the rest of the settings-load-once-at-startup
    /// discipline).
    pub(crate) prompts: Arc<hiker_core::prompts::Prompts>,
    /// status: llm-audit-log
    /// Shared JSONL audit-log writer. Every LLM-driven surface
    /// (`core::agent`, `core::llm`, MCP tool calls) records through
    /// this single writer so all rows land in one daily file. See
    /// `core::audit`.
    pub(crate) audit: Arc<hiker_core::audit::AgentLog>,
    /// status: task-queue-core-module
    /// Shared work queue for non-interactive LLM jobs. Plumbed into the
    /// MCP server (so external rmcp clients + the basic chat agent reach
    /// the same `task_*` surface) and drained by the in-process direct
    /// worker.
    pub(crate) tasks: Arc<hiker_core::tasks::Queue>,
    /// CancellationToken used to wind down the direct worker + queue
    /// maintenance task on vault swap. Dropped with the session.
    pub(crate) tasks_cancel: tokio_util::sync::CancellationToken,
    /// CancellationToken that stops the config-file watcher task on vault
    /// swap. Dropped with the session; the watcher task selects on this.
    pub(crate) config_watcher_cancel: tokio_util::sync::CancellationToken,
    /// status: mcp-tool-toggles
    /// Shared `[mcp.tools]` config — also held by the MCP handler so
    /// per-tool toggles apply live. Mutated by `set_setting` /
    /// `reload_config`.
    pub(crate) mcp_tools: Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>>,
    /// status: staging-config-section
    /// Shared `[staging]` config. Read live by the staging recheck task
    /// (so `auto_reject_on_conflict` applies without a restart) and by
    /// the vault-open GC pass via the in-memory copy. Mutated alongside
    /// `mcp_tools` by `set_setting` / `reload_config` / the config-file
    /// watcher.
    pub(crate) staging_config: Arc<std::sync::RwLock<hiker_core::config::StagingConfig>>,
}

impl Drop for VaultSession {
    fn drop(&mut self) {
        // Stop the direct worker + queue maintenance/event-pump tasks.
        // Safe to call multiple times — `CancellationToken::cancel` is
        // idempotent.
        self.tasks_cancel.cancel();
        // Stop the config-file watcher task.
        self.config_watcher_cancel.cancel();
    }
}

pub(crate) struct AppState {
    pub(crate) session: Mutex<Option<VaultSession>>,
    /// Suppression timestamp for the config-file watcher so
    /// `set_setting` writes don't round-trip back through the file
    /// watcher and re-fire `hiker:config-reloaded`.
    pub(crate) config_last_write: Mutex<Option<Instant>>,
}

// status: task-queue-raptor-triage-match
// Non-LLM dispatch arms for the direct worker. Today this is just the
// triage classifier; future feature-shaped tasks (auto-tag-on-save,
// summary-on-save, …) will add additional `try_handle` arms here.
//
// Author-class wiring: `triage-author-class` is honest-partial for now —
// the task variant carries no author hint, so we pass `User` here. The
// agent-author signal lands with the auto-accept worker / cancellable
// queue (see status row notes).
pub(crate) struct DirectWorkerHandlers {
    pub(crate) trees: Arc<hiker_core::trees::Trees>,
    pub(crate) vault: hiker_core::Vault,
    pub(crate) staging: Arc<hiker_core::staging::Staging>,
    pub(crate) read_store: Arc<Mutex<hiker_core::store::Store>>,
    pub(crate) config: Arc<std::sync::RwLock<hiker_core::config::Config>>,
    /// Loaded prompt store. Cluster build tasks render the
    /// `cluster_summarize` prompt via this handle without re-reading
    /// disk on every cluster.
    pub(crate) prompts: Arc<hiker_core::prompts::Prompts>,
}

impl DirectWorkerHandlers {
    /// Build the LLM-backed cluster summarizer the cluster-build task
    /// arms feed into `build_and_persist` / `rebuild_and_persist` /
    /// `build_tree`. Mirrors `build_cluster_summarizer` above but reads
    /// from the handler's own config + prompts so it doesn't need the
    /// session.
    fn cluster_summarizer(&self) -> Result<hiker_core::cluster::LlmSummarizer, String> {
        let cfg = self.config.read().map_err(|_| "config lock poisoned".to_string())?;
        if !cfg.llm.enabled {
            return Err("llm is disabled in settings; enable it to build clusters with summaries".into());
        }
        let client = hiker_core::llm::GraniteLlmClient::from_config(&cfg.llm)
            .map_err(|e| format!("llm client build: {e}"))?;
        let prompt_body = self
            .prompts
            .body("cluster_summarize")
            .ok_or_else(|| "cluster_summarize prompt missing".to_string())?
            .to_string();
        let client_arc: std::sync::Arc<dyn hiker_core::llm::LlmClient> = std::sync::Arc::new(client);
        Ok(hiker_core::cluster::LlmSummarizer::new(client_arc, prompt_body))
    }

    /// Resolve a `BuildScope` to a Vec<NoteInput> by walking the read
    /// store. Lazy-populates missing note embeddings. Shared between
    /// `ClusterBuildTree` and `ClusterRebuildTree` task arms.
    fn notes_for_scope(
        &self,
        scope: &hiker_core::cluster::BuildScope,
    ) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
        let mut store = self.read_store.lock().map_err(|e| e.to_string())?;
        let candidate_paths: Vec<String> = match scope {
            hiker_core::cluster::BuildScope::Vault { .. } => {
                store.all_note_paths().map_err(|e| e.to_string())?
            }
            hiker_core::cluster::BuildScope::Folder { rel, .. } => {
                let prefix = if rel.ends_with('/') || rel.is_empty() {
                    rel.clone()
                } else {
                    format!("{rel}/")
                };
                store
                    .all_note_paths()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter(|p| prefix.is_empty() || p.starts_with(&prefix))
                    .collect()
            }
            hiker_core::cluster::BuildScope::Notes { ids, .. } => {
                let mut out = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Ok(Some(p)) = store.path_for_id(id) {
                        out.push(p);
                    }
                }
                out
            }
        };
        // status: cluster-build-scope-source-types — apply the scope's
        // source-types filter after path resolution. Empty filter = match
        // everything (legacy behavior).
        let mut notes: Vec<hiker_core::cluster::NoteInput> = Vec::new();
        for path in candidate_paths {
            if !scope.matches_path(&path) {
                continue;
            }
            let emb = match store.note_embedding_for_path(&path) {
                Ok(Some(e)) => e,
                Ok(None) => match store.compute_and_store_note_embedding(&path) {
                    Ok(Some(e)) => e,
                    _ => continue,
                },
                Err(_) => continue,
            };
            let note_id = match store.id_for_path(&path) {
                Ok(Some(i)) => i,
                _ => continue,
            };
            let title = title_from_rel_path(&path);
            let folder = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
            notes.push(hiker_core::cluster::NoteInput {
                id: note_id,
                title,
                summary: String::new(),
                folder,
                embedding: emb,
            });
        }
        Ok(notes)
    }
}

impl hiker_core::tasks::NonLlmHandlers for DirectWorkerHandlers {
    fn try_handle(
        &self,
        task: &hiker_core::tasks::Task,
    ) -> Result<Option<serde_json::Value>, String> {
        match &task.kind {
            hiker_core::tasks::TaskKind::RaptorTriageMatch { tree_id, source_path } => {
                let store_guard = self.read_store.lock().map_err(|e| e.to_string())?;
                let note_id = store_guard
                    .id_for_path(source_path)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("note not indexed: {source_path}"))?;
                let embedding = store_guard
                    .note_embedding_for_path(source_path)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no embedding for {source_path}"))?;
                let cfg_triage = self
                    .config
                    .read()
                    .map_err(|_| "config lock poisoned".to_string())?
                    .suggestions
                    .triage
                    .clone();
                let opts = hiker_core::suggest::TriageOpts {
                    review_required: cfg_triage.review_required,
                    scope: cfg_triage.scope.clone(),
                    beam_width: 2,
                };
                let input = hiker_core::suggest::TriageInput {
                    tree_id,
                    note_id: &note_id,
                    source_path,
                    embedding: &embedding,
                    // status: triage-author-class
                    // The task payload carries no author hint in Sprint
                    // D — agent-author routing arrives with the
                    // auto-accept worker (see `triage-author-class`
                    // status row, currently `partial`).
                    author_class: hiker_core::suggest::NoteAuthorClass::User,
                    opts: &opts,
                };
                let outcome = hiker_core::suggest::triage_match(
                    &self.trees,
                    &self.vault,
                    &store_guard,
                    &self.staging,
                    input,
                )
                .map_err(|e| e.to_string())?;
                Ok(Some(serde_json::to_value(&outcome).map_err(|e| e.to_string())?))
            }
            hiker_core::tasks::TaskKind::RaptorSummarize {
                tree_id,
                cluster_node_id,
                level,
            } => {
                let node = self
                    .trees
                    .get_node(tree_id, cluster_node_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| {
                        format!("cluster node not found: {tree_id}/{cluster_node_id}")
                    })?;
                if !matches!(node.kind, hiker_core::trees::NodeKind::Cluster) {
                    return Err(format!(
                        "raptor_summarize target is not a cluster: {cluster_node_id}"
                    ));
                }
                if node.user_edited_name && node.user_edited_summary {
                    // Both fields user-edited — nothing to write back.
                    return Ok(Some(serde_json::json!({
                        "node_id": cluster_node_id,
                        "skipped": "user_edited",
                    })));
                }
                let children = self
                    .trees
                    .children_of(tree_id, Some(cluster_node_id))
                    .map_err(|e| e.to_string())?;
                if children.is_empty() {
                    return Err(format!(
                        "cluster has no children to summarize: {cluster_node_id}"
                    ));
                }
                // Resolve member info for the prompt. Cluster children
                // contribute their name+summary; leaf children contribute
                // the note's basename (looked up from the read store) and
                // an empty summary — matches the producer shape used by
                // the inline `build_and_persist` summarization pass.
                struct OwnedMember {
                    title: String,
                    summary: String,
                }
                let mut owned: Vec<OwnedMember> = Vec::with_capacity(children.len());
                {
                    let store_guard = self.read_store.lock().map_err(|e| e.to_string())?;
                    for c in &children {
                        match c.kind {
                            hiker_core::trees::NodeKind::Cluster => {
                                owned.push(OwnedMember {
                                    title: c.name.clone(),
                                    summary: c.summary.clone(),
                                });
                            }
                            hiker_core::trees::NodeKind::Leaf => {
                                let title = match c.note_ref.as_deref() {
                                    Some(id) => match store_guard.path_for_id(id) {
                                        Ok(Some(p)) => title_from_rel_path(&p),
                                        _ => c.note_ref.clone().unwrap_or_default(),
                                    },
                                    None => c.name.clone(),
                                };
                                owned.push(OwnedMember {
                                    title,
                                    summary: String::new(),
                                });
                            }
                            hiker_core::trees::NodeKind::OutlierBucket => {
                                // Outliers contribute as a structural
                                // entry only; no representative title.
                                owned.push(OwnedMember {
                                    title: "(outliers)".to_string(),
                                    summary: String::new(),
                                });
                            }
                        }
                    }
                }
                let members: Vec<hiker_core::cluster::MemberInfo<'_>> = owned
                    .iter()
                    .map(|m| hiker_core::cluster::MemberInfo {
                        title: m.title.as_str(),
                        summary: m.summary.as_str(),
                    })
                    .collect();
                let summarizer = self.cluster_summarizer()?;
                use hiker_core::cluster::Summarizer as _;
                let out = summarizer
                    .summarize(hiker_core::cluster::SummarizeInput {
                        level: *level as usize,
                        members,
                    })
                    .map_err(|e| format!("summarize: {e}"))?;
                let (wrote_name, wrote_summary) = self
                    .trees
                    .auto_set_name_summary(
                        tree_id,
                        cluster_node_id,
                        &out.name,
                        &out.summary,
                    )
                    .map_err(|e| e.to_string())?;
                Ok(Some(serde_json::json!({
                    "node_id": cluster_node_id,
                    "name": out.name,
                    "summary": out.summary,
                    "confidence": out.confidence,
                    "wrote_name": wrote_name,
                    "wrote_summary": wrote_summary,
                })))
            }
            hiker_core::tasks::TaskKind::ClusterBuildTree {
                name,
                source,
                scope_json,
                method_json,
            } => {
                let scope: hiker_core::cluster::BuildScope = serde_json::from_str(scope_json)
                    .map_err(|e| format!("scope_json: {e}"))?;
                let method: hiker_core::cluster::BuildMethod = serde_json::from_str(method_json)
                    .map_err(|e| format!("method_json: {e}"))?;
                let notes = self.notes_for_scope(&scope)?;
                if notes.is_empty() {
                    return Err("no notes with embeddings found in scope".into());
                }
                let summarizer = self.cluster_summarizer()?;
                let tree_id = hiker_core::cluster::build_and_persist(
                    &self.trees,
                    name,
                    source,
                    scope,
                    method,
                    &notes,
                    &summarizer,
                )
                .map_err(|e| e.to_string())?;
                Ok(Some(serde_json::json!({ "tree_id": tree_id })))
            }
            hiker_core::tasks::TaskKind::ClusterRebuildTree { tree_id, new_name } => {
                let old_row = self
                    .trees
                    .get_tree(tree_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("tree not found: {tree_id}"))?;
                let scope: hiker_core::cluster::BuildScope =
                    serde_json::from_str(&old_row.scope_json)
                        .map_err(|e| format!("scope_json: {e}"))?;
                let notes = self.notes_for_scope(&scope)?;
                if notes.is_empty() {
                    return Err("no notes with embeddings found in scope".into());
                }
                let summarizer = self.cluster_summarizer()?;
                let resolved_name = new_name
                    .clone()
                    .unwrap_or_else(|| format!("{} (rebuild)", old_row.name));
                let new_id = hiker_core::cluster::rebuild_and_persist(
                    &self.trees,
                    tree_id,
                    &resolved_name,
                    &notes,
                    &summarizer,
                    0.5,
                )
                .map_err(|e| e.to_string())?;
                Ok(Some(serde_json::json!({ "tree_id": new_id })))
            }
            hiker_core::tasks::TaskKind::ClusterReclusterSubtree {
                tree_id,
                node_id,
                cluster_params_json,
                carry_policies_down,
            } => recluster_subtree_in_worker(
                self,
                tree_id,
                node_id,
                cluster_params_json,
                *carry_policies_down,
            )
            .map(Some),
            _ => Ok(None),
        }
    }
}

fn with_vault<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Vault) -> Result<R, String>,
) -> Result<R, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(&session.vault)
}

/// Log an `Err(_)` returned to the frontend, then pass the Result through
/// unchanged. Wrap a command's final expression in this so every failure
/// shows up in the unified log without scattering `tracing::error!` calls
/// across each `.map_err` chain. Per `obs-error-context`: the error chain
/// rides the `error` field, the message stays grep-stable.
fn log_cmd_result<T, E: std::fmt::Display>(
    command: &'static str,
    r: Result<T, E>,
) -> Result<T, E> {
    if let Err(e) = &r {
        tracing::error!(error = %e, command, "tauri command failed");
    }
    r
}

#[tauri::command]
fn list_dir(
    state: State<AppState>,
    rel: String,
    sort: Option<TreeSortBy>,
) -> Result<Vec<DirEntryDto>, String> {
    let result = (|| -> Result<Vec<DirEntryDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let order = match sort {
            Some(o) => o,
            None => session
                .config
                .read()
                .map_err(|_| "config lock poisoned".to_string())?
                .vault
                .tree
                .sort_by,
        };
        session.vault.list_dir(&rel, order).map_err(|e| e.to_string())
    })();
    log_cmd_result("list_dir", result)
}

#[tauri::command]
fn read_file(state: State<AppState>, rel: String) -> Result<String, String> {
    log_cmd_result(
        "read_file",
        with_vault(&state, |v| v.read_file(&rel).map_err(|e| e.to_string())),
    )
}

#[derive(Serialize)]
struct FileWithHash {
    contents: String,
    hash: String,
}

#[tauri::command]
fn read_file_with_hash(state: State<AppState>, rel: String) -> Result<FileWithHash, String> {
    log_cmd_result(
        "read_file_with_hash",
        with_vault(&state, |v| {
            v.read_file_with_hash(&rel)
                .map(|(contents, hash)| FileWithHash { contents, hash })
                .map_err(|e| e.to_string())
        }),
    )
}

/// status: note-mutation-stash-changes-tag
/// Build the `metadata` JSON for a save's changes-row. Frontend may pass
/// `extra_metadata` to stamp one-shot context (e.g.
/// `{ "mutation": "<kind>" }` for the save that accepts an in-buffer
/// mutation). Object inputs are taken as-is; non-object / `None` falls
/// back to the empty object — same default as before this hook landed.
fn merge_extra_metadata(extra: Option<serde_json::Value>) -> serde_json::Value {
    match extra {
        Some(serde_json::Value::Object(_)) => extra.unwrap(),
        _ => serde_json::json!({}),
    }
}

#[tauri::command]
fn write_file(
    state: State<AppState>,
    rel: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let abs = session
            .vault
            .abs_path(&rel)
            .map_err(|e| e.to_string())?;
        let existed = abs.exists();
        // Baseline-on-first-save: if the file already existed but the
        // changelog has no row for it, snapshot the pre-write state so
        // rollback of this save has somewhere to go. Read failures fall
        // through silently — better to log a hash-less save than refuse
        // the write.
        if existed {
            if let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel) {
                if let Err(e) = session.changes.ensure_baseline(
                    &rel,
                    "user",
                    pre_text.as_bytes(),
                    &pre_hash,
                ) {
                    tracing::warn!(error = %e, "changes: ensure_baseline failed");
                }
            }
        }
        session
            .vault
            .write_file(&rel, &contents)
            .map_err(|e| e.to_string())?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        let hash = hiker_core::hash_str(&contents);
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file) failed");
        }
        Ok(())
    })();
    log_cmd_result("write_file", result)
}

/// Open `rel` for editing — read its bytes and mint an opaque
/// `BufferToken`. The UI seeds CM6 with `contents` and round-trips the
/// token verbatim through `commit_buffer`; it never holds the hash.
#[tauri::command]
fn open_for_edit(
    state: State<AppState>,
    rel: String,
) -> Result<hiker_core::ops::OpenForEditOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::open_for_edit(&session.vault, &rel)
    })();
    log_cmd_result("open_for_edit", result)
}

/// Commit a buffer's new text using the drift-check encoded in `token`.
/// Returns `Written { new_hash, token }` on success or `DriftDetected
/// { current_disk_text, current_hash }` on conflict — the UI shows its
/// modal and dispatches to `resolve_drift`.
#[tauri::command]
fn commit_buffer(
    state: State<AppState>,
    token: hiker_core::ops::BufferToken,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::CommitOutcome, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::commit_buffer(
            &session.vault,
            Some(&session.changes),
            &token,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("commit_buffer", result)
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the UI; this is the typed action surface.
#[tauri::command]
fn resolve_drift(
    state: State<AppState>,
    rel: String,
    choice: hiker_core::ops::DriftChoice,
    new_text: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<hiker_core::ops::DriftResolution, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        hiker_core::ops::resolve_drift(
            &session.vault,
            Some(&session.changes),
            &rel,
            choice,
            &new_text,
            extra_metadata.unwrap_or(serde_json::json!({})),
        )
    })();
    log_cmd_result("resolve_drift", result)
}

#[tauri::command]
fn write_file_checked(
    state: State<AppState>,
    rel: String,
    expected_hash: String,
    contents: String,
    extra_metadata: Option<serde_json::Value>,
) -> Result<String, hiker_core::HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| hiker_core::HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| hiker_core::HikerError::Io("no vault open".into()))?;
        // Detect created-vs-modified before the write. The drift check
        // upstream means `expected_hash` is empty for first-write (file
        // missing); after the write we tag the row accordingly.
        let abs = session.vault.abs_path(&rel)?;
        let existed = abs.exists();
        // Baseline-on-first-save: snapshot the pre-write content before
        // overwriting so rollback of this save restores the prior state.
        // No-op when the changelog already has a row for this path.
        if existed {
            if let Ok((pre_text, pre_hash)) = session.vault.read_file_with_hash(&rel) {
                if let Err(e) = session.changes.ensure_baseline(
                    &rel,
                    "user",
                    pre_text.as_bytes(),
                    &pre_hash,
                ) {
                    tracing::warn!(error = %e, "changes: ensure_baseline failed");
                }
            }
        }
        let new_hash = session
            .vault
            .write_file_checked(&rel, &expected_hash, &contents)?;
        // status: changes-write-path
        let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
        if let Err(e) = session.changes.append(hiker_core::changes::ChangeAppend {
            path: &rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(contents.as_bytes()),
            rename_from: None,
            metadata: merge_extra_metadata(extra_metadata),
        }) {
            tracing::warn!(error = %e, "changes: append (write_file_checked) failed");
        }
        Ok(new_hash)
    })();
    log_cmd_result("write_file_checked", result)
}

/// Snapshot of the active vault's merged settings. Frontend uses this on
/// vault open to seed View menu / tree-state defaults.
///
/// status: settings-load-once-at-startup
#[tauri::command]
fn get_settings(state: State<AppState>) -> Result<Config, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let cfg = session
            .config
            .read()
            .map_err(|_| "config lock poisoned".to_string())?;
        Ok(cfg.clone())
    })();
    log_cmd_result("get_settings", result)
}

/// Persist a single setting. The eligible-key set is closed (see
/// `core::config::ELIGIBLE_*`); anything not in it is rejected.
///
/// Concurrency: `Config::set` does file IO + reload outside the session
/// lock, then we re-acquire the write lock to swap the in-memory copy.
/// Two concurrent flips can therefore race so the older reload wins. In
/// practice users flip one toggle at a time, and the next set_setting
/// reload will reconverge — not worth a global write mutex for now.
///
/// status: settings-write-back
#[tauri::command]
async fn set_setting(
    state: State<'_, AppState>,
    scope: SettingsScope,
    key: String,
    value: serde_json::Value,
) -> Result<Config, HikerError> {
    let result = set_setting_inner(state, scope, key, value).await;
    log_cmd_result("set_setting", result)
}

async fn set_setting_inner(
    state: State<'_, AppState>,
    scope: SettingsScope,
    key: String,
    value: serde_json::Value,
) -> Result<Config, HikerError> {
    // Snapshot the previous mcp config (for the bind-restart decision)
    // and the vault root before doing any disk I/O.
    let (root, prev_mcp) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let cfg = session
            .config
            .read()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        (session.root.clone(), cfg.mcp.clone())
    };
    let updated = Config::set(scope, &key, value, &root)?;
    // Suppress the config-file watcher for a short window after this write
    // so the resulting fs event doesn't round-trip back through
    // `Config::load` -> `hiker:config-reloaded`.
    {
        if let Ok(mut guard) = state.config_last_write.lock() {
            *guard = Some(Instant::now());
        }
    }
    // Apply the live in-memory updates (config swap, queue cfg,
    // mcp tool gates).
    {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated.clone();
        drop(w);
        session.tasks.set_cfg(updated.tasks.clone());
        match session.mcp_tools.write() {
            Ok(mut tools) => *tools = updated.mcp.tools.clone(),
            Err(_) => {} // poisoned — best-effort
        };
        match session.staging_config.write() {
            Ok(mut s) => *s = updated.staging.clone(),
            Err(_) => {} // poisoned — best-effort
        };
    }
    // status: mcp-bind-host-configurable
    // Bind-affecting change → tear the MCP server down and start it
    // back up so port/host/discovery-file flips apply without a vault
    // re-open. `mcp.tools.*` is already live via the shared RwLock so
    // it's excluded; everything else in `[mcp]` (enabled, host, port,
    // discovery_file, max_top_k, audit.log_full_input) takes effect
    // through a server restart.
    let bind_changed = prev_mcp.enabled != updated.mcp.enabled
        || prev_mcp.host != updated.mcp.host
        || prev_mcp.port != updated.mcp.port
        || prev_mcp.discovery_file != updated.mcp.discovery_file
        || prev_mcp.max_top_k != updated.mcp.max_top_k
        || prev_mcp.audit.log_full_input != updated.mcp.audit.log_full_input;
    if bind_changed {
        restart_mcp_server(&state, &updated).await;
    }
    Ok(updated)
}

/// Tear down the existing in-process MCP server (if any) and bring up a
/// fresh one against the latest config. Failures during the restart log
/// at warn but don't propagate — a stale-but-running server is worse
/// UX than the user thinking their toggle didn't apply, but a setting
/// flip shouldn't kill the whole vault session.
///
/// status: mcp-bind-host-configurable
async fn restart_mcp_server(state: &State<'_, AppState>, updated: &Config) {
    // Pull the deps we need under the sync lock, then drop it before
    // any `.await` so we don't hold a std::sync::Mutex across the
    // bind. Take the old handle out of the session first so its `Drop`
    // (which cancels the axum task and removes the discovery file)
    // fires before we attempt to bind the new one.
    let restart_inputs = {
        let mut guard = match state.session.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(session) = guard.as_mut() else { return };
        let old = session.mcp.take();
        let inputs = (
            session.vault.clone(),
            session.root.clone(),
            session.indexer.job_sender(),
            session.indexer.embedder_provider(),
            session.read_store.clone(),
            session.watcher.clone(),
            session.changes.clone(),
            session.audit.clone(),
            session.tasks.clone(),
            session.mcp_tools.clone(),
            session.staging.clone(),
        );
        drop(old);
        inputs
    };
    if !updated.mcp.enabled {
        return;
    }
    let (vault, root, jobs, embedder_provider, read_store, watcher, changes, audit, tasks, mcp_tools, staging) =
        restart_inputs;
    let deps = hiker_mcp::McpDeps {
        vault,
        vault_root: root,
        read_store,
        jobs,
        watcher,
        changes,
        embedder_provider,
        config: updated.mcp.clone(),
        tools: mcp_tools,
        audit,
        tasks,
        tasks_config: updated.tasks.clone(),
        llm_enabled: updated.llm.enabled,
        staging,
    };
    match hiker_mcp::start(deps).await {
        Ok(handle) => {
            if let Ok(mut guard) = state.session.lock() {
                if let Some(session) = guard.as_mut() {
                    session.mcp = Some(handle);
                }
            }
        }
        Err(hiker_mcp::StartError::Disabled) => {}
        Err(e) => {
            tracing::warn!(error = %e, "mcp: restart failed");
        }
    }
}

/// Read a single TOML scope's contents (user or vault) without merging or
/// triggering auto-create. Backs the settings pane's per-section scope
/// toggle: each section card shows the values that the *currently-displayed
/// file alone* would contribute. Missing file → `Config::default()`.
///
/// status: settings-pane-scope-toggle
#[tauri::command]
fn get_settings_scoped(
    state: State<AppState>,
    scope: SettingsScope,
) -> Result<Config, HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Config("no vault open".into()))?;
            session.root.clone()
        };
        Config::read_file_only(scope, &root)
    })();
    log_cmd_result("get_settings_scoped", result)
}

/// Force `Config::load` to re-run and swap the in-memory copy. Backs the
/// settings pane's manual-refresh affordance for the "user hand-edited the
/// TOML while the pane was open" case.
///
/// status: settings-pane-manual-refresh
#[tauri::command]
fn reload_config(state: State<AppState>) -> Result<Config, HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Config("no vault open".into()))?;
            session.root.clone()
        };
        let updated = Config::load(&root)?;
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated.clone();
        drop(w);
        session.tasks.set_cfg(updated.tasks.clone());
        if let Ok(mut tools) = session.mcp_tools.write() {
            *tools = updated.mcp.tools.clone();
        }
        if let Ok(mut s) = session.staging_config.write() {
            *s = updated.staging.clone();
        }
        Ok(updated)
    })();
    log_cmd_result("reload_config", result)
}

/// Resolve the absolute path of one config TOML and reveal it in the OS file
/// manager. Used by the settings pane's "Open user/vault config.toml"
/// affordances and the read-only-row popovers.
///
/// status: settings-pane-open-toml-link
#[tauri::command]
fn reveal_config_file(
    state: State<AppState>,
    scope: SettingsScope,
) -> Result<(), HikerError> {
    let result = (|| {
        let root = {
            let guard = state
                .session
                .lock()
                .map_err(|_| HikerError::Io("session lock poisoned".into()))?;
            let session = guard
                .as_ref()
                .ok_or_else(|| HikerError::Io("no vault open".into()))?;
            session.root.clone()
        };
        let paths = hiker_core::config::ConfigPaths::resolve(&root);
        let abs = match scope {
            SettingsScope::User => paths
                .user
                .ok_or_else(|| HikerError::Config("no platform config dir available".into()))?,
            SettingsScope::Vault => paths.vault,
        };
        // Settings TOMLs auto-create on first `Config::load`, so a vault
        // that was opened normally already has both files. If the user TOML
        // dir was unresolvable that branch errored above; this fallback
        // covers the rare "file deleted between open and reveal" case.
        let target = if abs.exists() {
            abs
        } else if let Some(parent) = abs.parent() {
            parent.to_path_buf()
        } else {
            abs
        };
        reveal_path(&target).map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("reveal_config_file", result)
}

/// Read-only lookup of the user-scope `vault.default` field. Used by the
/// frontend bootstrap to decide whether to auto-open a configured default
/// vault before falling through to the JS-side folder picker. Returns
/// `Ok(None)` when no default is set or the user TOML doesn't exist yet;
/// real I/O / parse failures bubble up as `Err`.
///
/// status: settings-default-vault-autoopen
#[tauri::command]
fn get_default_vault() -> Result<Option<String>, String> {
    log_cmd_result(
        "get_default_vault",
        hiker_core::config::Config::user_default_vault().map_err(|e| e.to_string()),
    )
}

/// Open the vault at `path`. Single shared entry point for the frontend's
/// "Open vault" flow, the bootstrap auto-open path, and (eventually) CLI
/// / MCP entry points. The folder picker is *not* a backend concern —
/// the frontend uses `@tauri-apps/plugin-dialog` from JS when it needs
/// one. A path that no longer resolves returns `HikerError::NotFound` so
/// the bootstrap path can react with a toast + fall-through to picker
/// rather than auto-clearing the setting.
///
/// status: staging-drift-eager-recheck
/// Spawn a tokio task that consumes watcher + changes broadcasts and
/// re-checks every pending staging proposal whose `target_path` matches.
/// `Staging::recheck` persists transitions and broadcasts
/// `hiker:staging-changed` via the existing staging forwarder, so this
/// helper is fire-and-forget — it owns no event channel of its own.
fn spawn_staging_recheck(
    staging: Arc<Staging>,
    vault: Vault,
    staging_config: Arc<std::sync::RwLock<hiker_core::config::StagingConfig>>,
    mut file_rx: tokio::sync::broadcast::Receiver<FileEvent>,
    mut changes_rx: tokio::sync::broadcast::Receiver<ChangeRow>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = file_rx.recv() => match ev {
                    Ok(FileEvent::Created { path })
                    | Ok(FileEvent::Modified { path })
                    | Ok(FileEvent::Deleted { path }) => {
                        recheck_path(&staging, &vault, &staging_config, &path);
                    }
                    Ok(FileEvent::Renamed { from, to }) => {
                        recheck_path(&staging, &vault, &staging_config, &from);
                        recheck_path(&staging, &vault, &staging_config, &to);
                    }
                    Ok(FileEvent::Overflow) => {
                        // After overflow, our knowledge of the filesystem is
                        // stale. Recheck every pending proposal against
                        // current disk so conflicted state catches up.
                        if let Ok(all) = staging.list(&StagingFilter::default()) {
                            let mut seen = std::collections::HashSet::new();
                            for p in &all {
                                if seen.insert(p.target_path.clone()) {
                                    recheck_path(&staging, &vault, &staging_config, &p.target_path);
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
                ev = changes_rx.recv() => match ev {
                    Ok(row) => {
                        recheck_path(&staging, &vault, &staging_config, &row.path);
                        if let Some(ref from) = row.rename_from {
                            recheck_path(&staging, &vault, &staging_config, from);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                },
            }
        }
    });
}

/// status: staging-auto-reject-on-conflict
fn recheck_path(
    staging: &Staging,
    vault: &Vault,
    staging_config: &std::sync::RwLock<hiker_core::config::StagingConfig>,
    rel_path: &str,
) {
    let proposals = match staging.list(&StagingFilter {
        path: Some(rel_path.to_string()),
        ..Default::default()
    }) {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let disk = vault.read_file(rel_path).ok();
    for p in &proposals {
        match staging.recheck(&p.id, disk.as_deref()) {
            Ok(outcome) => {
                use hiker_core::staging::ProposalState;
                let transitioned_to_conflict = outcome.prior_state == ProposalState::Applyable
                    && outcome.new_state == ProposalState::Conflicted;
                if !transitioned_to_conflict {
                    continue;
                }
                let auto_reject = staging_config
                    .read()
                    .map(|c| c.auto_reject_on_conflict)
                    .unwrap_or(false);
                if !auto_reject {
                    continue;
                }
                let reason = outcome
                    .new_reason
                    .map(|r| r.as_str())
                    .unwrap_or("unknown");
                tracing::info!(
                    proposal_id = %p.id,
                    reason = %reason,
                    "staging: auto-rejecting proposal on conflict transition",
                );
                if let Err(e) = staging.reject(&p.id) {
                    tracing::warn!(
                        proposal_id = %p.id,
                        error = %e,
                        "staging: auto-reject failed",
                    );
                }
            }
            Err(e) => {
                tracing::warn!(proposal_id = %p.id, error = %e, "staging: recheck failed");
            }
        }
    }
}

/// status: settings-default-vault-autoopen
#[tauri::command]
async fn open_vault_at(
    app: tauri::AppHandle,
    path: String,
) -> Result<String, HikerError> {
    log_cmd_result("open_vault_at", open_vault_at_inner(app, PathBuf::from(path)).await)
}

async fn open_vault_at_inner(
    app: tauri::AppHandle,
    path_buf: PathBuf,
) -> Result<String, HikerError> {
    if !path_buf.is_dir() {
        tracing::warn!(
            path = %path_buf.display(),
            "open_vault_at: path does not resolve to a directory",
        );
        return Err(HikerError::NotFound(path_buf.display().to_string()));
    }
    let vault = Vault::open(&path_buf).map_err(|e| HikerError::Io(e.to_string()))?;
    let root = vault.root().to_path_buf();
    let display = root.to_string_lossy().into_owned();

    // Stand up the tracing pipeline (per-vault log files). Idempotent across
    // vault swaps in the same UI session — the first call wins.
    if let Err(e) = hiker_core::observability::init_tracing(&root) {
        // Subscriber init only fails on disk errors or a competing global
        // subscriber; surface it on stderr and keep the vault open. Falling
        // back to no logging is strictly better than refusing to open.
        eprintln!("[hiker] init_tracing failed: {e}");
    }
    tracing::info!(
        vault_root = %root.display(),
        "ui: vault opened",
    );

    // status: settings-load-once-at-startup
    // Read user + vault TOML, merge, validate. Auto-creates either file
    // with the current defaults if missing (settings-auto-create-defaults).
    // Strict-load: any unknown key, type mismatch, or schema-version
    // mismatch aborts here with a clear error.
    let mut config = Config::load(&root)?;

    // Push this vault onto the user-scope `vault.recent` list. Best-effort:
    // if the platform config dir isn't resolvable (sandboxed env), the
    // write fails silently rather than aborting vault open. The returned
    // Config is the freshly-reloaded merged view — adopt it so the in-memory
    // copy in the session matches what's on disk.
    let recent = hiker_core::config::push_recent_vault(&config.vault.recent, &root);
    if recent != config.vault.recent {
        let value = serde_json::Value::Array(
            recent.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
        );
        match Config::set(SettingsScope::User, "vault.recent", value, &root) {
            Ok(updated) => config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.recent"),
        }
    }

    // Persist this vault as the user's default so `bootstrapDefaultVault`
    // auto-opens it on next launch. `vault.recent` alone isn't enough —
    // bootstrap reads `vault.default` per `settings-default-vault-autoopen`.
    let root_str = root.to_string_lossy().to_string();
    if config.vault.default.as_deref() != Some(root_str.as_str()) {
        match Config::set(
            SettingsScope::User,
            "vault.default",
            serde_json::Value::String(root_str),
            &root,
        ) {
            Ok(updated) => config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.default"),
        }
    }

    // status: changes-store-file
    // Open the changelog db (separate from index.db so the index can be
    // regenerated freely while the changelog stays durable). Best-effort:
    // a failed open shouldn't block vault open, but every subsequent
    // append call will silently no-op until next vault swap.
    let changes = Arc::new(
        Changes::open(&root).map_err(|e| HikerError::Io(format!("changes db: {e}")))?,
    );

    // Open the staging area for proposed writes. Created at vault open,
    // lives for the duration of the session. The MCP server references
    // this instance to route write tools through propose() when
    // `[mcp.tools].review_required` is true.
    //
    // status: agent-write-review-mode
    let staging = Arc::new(
        Staging::open(&root).map_err(|e| HikerError::Io(format!("staging: {e}")))?,
    );

    // status: trees-db
    // Cluster-tree storage for the cluster editor (Sprint B). Opened
    // alongside staging so every command site can reach `session.trees`
    // without a second mutex hop.
    let trees = Arc::new(
        hiker_core::trees::Trees::open(&root)
            .map_err(|e| HikerError::Io(format!("trees: {e}")))?,
    );

    // status: activity-feed-module
    let activity = Arc::new(Activity::new(changes.clone(), staging.clone()));

    // status: autosave-backend-module, autosave-store-layout
    // Open the per-vault autosave store. Failure is fatal at vault open
    // (a future tick would just keep failing silently otherwise).
    let autosave = Arc::new(
        Autosave::open(&root).map_err(|e| HikerError::Io(format!("autosave: {e}")))?,
    );

    // One-shot retention pass at vault open. Bounds storage without a
    // periodic task; spec calls for "low-priority job from the indexer
    // task, opportunistically when no other work is queued" — vault open
    // is the cheapest such moment.
    if let Err(e) = changes.gc(50) {
        tracing::warn!(error = %e, "changes: gc on open failed");
    }

    // status: staging-config-section
    // Staging GC on vault open; retention threshold from `[staging]`
    // config (default 14 days). Lifts the previously-hardcoded value.
    if let Err(e) = staging.gc(config.staging.retention_days) {
        tracing::warn!(error = %e, "staging: gc on open failed");
    }

    // Forward each append to the frontend as `hiker:changes-appended`.
    // Lagging is fine — the home page widget re-fetches `recent` on each
    // notification so a missed event just means one less repaint.
    let app_for_changes = app.clone();
    let mut changes_rx = changes.subscribe();
    tokio::spawn(async move {
        loop {
            match changes_rx.recv().await {
                Ok(row) => {
                    let _ = app_for_changes.emit("hiker:changes-appended", &row);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Forward staging mutations to the frontend as `hiker:staging-changed`.
    // This catches proposals from the MCP surface (which lacks an AppHandle)
    // as well as the Tauri accept/reject commands.
    let app_for_staging = app.clone();
    let mut staging_rx = staging.subscribe();
    tokio::spawn(async move {
        loop {
            match staging_rx.recv().await {
                Ok(()) => {
                    let _ = app_for_staging.emit("hiker:staging-changed", ());
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Open the store (creates .hiker/index.db on first run). This is the
    // writer connection that the indexer task takes ownership of below.
    let store = Store::open(&root).map_err(|e| HikerError::Io(e.to_string()))?;

    // Open a *second* connection against the same db for every read-side
    // Tauri command. WAL mode (set on the writer above) is per-file, so
    // both connections see committed writes without locking; the sqlite-vec
    // extension auto-registers process-once. See `VaultSession.read_store`.
    let read_store =
        Arc::new(Mutex::new(Store::open(&root).map_err(|e| HikerError::Io(e.to_string()))?));

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately.
    let indexer = start_indexer(vault.clone(), store, || {
        FastembedEmbedder::load()
            .map(|e| std::sync::Arc::new(e) as std::sync::Arc<dyn hiker_core::embed::Embedder>)
    });

    // Start the filesystem watcher and bridge its events into the indexer.
    let watcher = Arc::new(Watcher::start(&root).map_err(|e| HikerError::Io(e.to_string()))?);
    let watcher_rx = watcher.subscribe();
    let job_sender = indexer.job_sender();
    tokio::spawn(route_watcher_events(watcher_rx, job_sender));

    // status: trail-auto-update-on-note-move
    // Late-bind watcher + changes to the indexer so the trails
    // auto-update path can suppress watcher events around its rewrites
    // and append `core::changes` rows for each touched file.
    indexer.attach_watcher(watcher.clone());
    indexer.attach_changes(changes.clone());

    // Forward watcher events to the frontend so the editor's drift logic
    // sees them too. Separate subscription so a slow consumer on one side
    // doesn't lag the other.
    let app_for_files = app.clone();
    let mut file_rx = watcher.subscribe();
    tokio::spawn(async move {
        loop {
            match file_rx.recv().await {
                Ok(FileEvent::Overflow) => {
                    let _ = app_for_files.emit("hiker:watcher-overflow", ());
                }
                Ok(ev) => {
                    let _ = app_for_files.emit("hiker:file-changed", &ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // status: staging-config-section
    // Shared `[staging]` config — read live by the staging recheck task so
    // `auto_reject_on_conflict` applies without a restart.
    let staging_config: Arc<std::sync::RwLock<hiker_core::config::StagingConfig>> =
        Arc::new(std::sync::RwLock::new(config.staging.clone()));

    // status: staging-drift-eager-recheck
    // Every watcher FileEvent and every appended `core::changes` row gets
    // routed through `Staging::recheck` for proposals whose `target_path`
    // matches. State transitions persist + broadcast `hiker:staging-changed`
    // via the staging forwarder above; this task is purely the trigger.
    spawn_staging_recheck(
        staging.clone(),
        vault.clone(),
        staging_config.clone(),
        watcher.subscribe(),
        changes.subscribe(),
    );

    // status: cluster-editor-triage-on-save
    // status: cluster-editor-triage-via-staging
    // status: cluster-build-from-folders-live-update
    //
    // Watch for note modifications and renames; on Modified/Created, run
    // the triage classifier against every saved-as-triage tree (per
    // `docs/cluster-editor.md` §"Triage execution" — on-save trigger);
    // on Renamed, update FromFolders live-update for any saved tree
    // tracking the filesystem. The spawn lives here so it picks up
    // `session.trees` / `session.staging` / `session.read_store` once
    // they're all bound; the in-flight classifier work is synchronous
    // and short (microseconds; no LLM) so we don't bother with
    // background concurrency.
    {
        let trees_for_triage = trees.clone();
        let staging_for_triage = staging.clone();
        let vault_for_triage = vault.clone();
        let read_store_for_triage = read_store.clone();
        let config_arc = std::sync::Arc::new(std::sync::RwLock::new(config.clone()));
        let _ = config_arc; // currently unused; the spawn re-reads cfg below.
        let mut trigger_rx = watcher.subscribe();
        let cfg_triage = config.suggestions.triage.clone();
        tokio::spawn(async move {
            loop {
                let ev = match trigger_rx.recv().await {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                };
                let (modified_path, rename_target): (Option<String>, Option<(String, String)>) =
                    match ev {
                        hiker_core::watcher::FileEvent::Modified { path }
                        | hiker_core::watcher::FileEvent::Created { path } => (Some(path), None),
                        hiker_core::watcher::FileEvent::Renamed { from, to } => {
                            (Some(to.clone()), Some((from, to)))
                        }
                        _ => (None, None),
                    };
                // FromFolders live-update on rename.
                if let Some((rel_from, rel_to)) = rename_target.clone() {
                    let trees_rows = match trees_for_triage.list_trees() {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let store_guard = match read_store_for_triage.lock() {
                        Ok(g) => g,
                        Err(_) => continue,
                    };
                    let note_id = store_guard
                        .id_for_path(&rel_to)
                        .ok()
                        .flatten();
                    drop(store_guard);
                    let new_folder = rel_to
                        .rsplit_once('/')
                        .map(|(a, _)| a.to_string())
                        .unwrap_or_default();
                    if let Some(nid) = note_id {
                        for t in &trees_rows {
                            if t.state != "saved-as-triage" {
                                continue;
                            }
                            let is_folders = serde_json::from_str::<serde_json::Value>(
                                &t.method_json,
                            )
                            .ok()
                            .and_then(|v| {
                                v.get("kind")
                                    .and_then(|k| k.as_str())
                                    .map(|s| s == "from-folders")
                            })
                            .unwrap_or(false);
                            if !is_folders {
                                continue;
                            }
                            let _ = trees_for_triage.update_for_folder_rename(
                                &t.id,
                                &nid,
                                &new_folder,
                            );
                        }
                    }
                    let _ = rel_from;
                }
                // Triage classifier on modify/create.
                let Some(rel) = modified_path else {
                    continue;
                };
                // Cheap scope pre-filter — skip files outside the
                // configured triage scope before touching the store.
                let scope_trim = cfg_triage.scope.trim();
                if !scope_trim.is_empty() && !rel.starts_with(scope_trim) {
                    continue;
                }
                // Run against every saved-as-triage tree. Synchronous —
                // beam descent is microseconds.
                let store_guard = match read_store_for_triage.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let Some(note_id) = store_guard
                    .id_for_path(&rel)
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                let Some(embedding) = store_guard
                    .note_embedding_for_path(&rel)
                    .ok()
                    .flatten()
                else {
                    continue;
                };
                drop(store_guard);
                let opts = hiker_core::suggest::TriageOpts {
                    review_required: cfg_triage.review_required,
                    scope: cfg_triage.scope.clone(),
                    beam_width: 2,
                };
                let store_guard = match read_store_for_triage.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let _ = hiker_core::suggest::triage_all_saved_trees(
                    &trees_for_triage,
                    &vault_for_triage,
                    &store_guard,
                    &staging_for_triage,
                    &note_id,
                    &rel,
                    &embedding,
                    hiker_core::suggest::NoteAuthorClass::User,
                    &opts,
                );
                drop(store_guard);
            }
        });
    }

    // Forward indexer progress events to the frontend.
    let app_for_progress = app.clone();
    let mut progress_rx = indexer.subscribe_progress();
    tokio::spawn(async move {
        loop {
            match progress_rx.recv().await {
                Ok(ev) => {
                    let _ = app_for_progress.emit("hiker:reindex-progress", &ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    // Forward status snapshots to the frontend so it can drop its 2s poll.
    // Emit the seeded value first (queued/total_notes/last_error are populated
    // before the indexer task even runs), then on every change.
    let app_for_status = app.clone();
    let mut status_rx = indexer.subscribe_status();
    tokio::spawn(async move {
        let _ = app_for_status.emit("hiker:index-status", &*status_rx.borrow_and_update());
        while status_rx.changed().await.is_ok() {
            let _ = app_for_status.emit("hiker:index-status", &*status_rx.borrow_and_update());
        }
    });

    // Kick the initial scan. Returns immediately; jobs flow as the model
    // load completes.
    let _ = indexer.full_scan().await;

    // status: llm-audit-log
    // One shared JSONL agent-log writer for every LLM-driven surface in
    // this session (core::agent turns, core::llm direct, mcp-tool-call).
    // `[llm.audit] log_full_prompt` mirrors the obs-no-content default;
    // callers that carry user content (the MCP wrapper) consult the
    // toggle before stuffing bodies into `details`. Constructed before
    // `start_mcp` so the MCP server can share the same writer.
    let audit = Arc::new(hiker_core::audit::AgentLog::new(
        root.join(".hiker").join("agent-log"),
        config.llm.audit.log_full_prompt,
    ));

    // status: task-queue-core-module
    // Stand up the unified work queue and the direct-LLM worker. Always
    // construct the queue so the MCP server can advertise `task_*`
    // (gated separately on `[mcp] enabled`); the direct worker only
    // spawns when both `[llm] enabled` and `[tasks] direct_worker.enabled`
    // are true (per `task-queue-respects-llm-disable`).
    let tasks = Arc::new(hiker_core::tasks::Queue::new(config.tasks.clone()));
    let tasks_cancel = tokio_util::sync::CancellationToken::new();
    // Loaded up here so the direct-worker handlers + the session both
    // share one Arc. The session uses it via `chat_send`; the worker
    // uses it to render the `cluster_summarize` prompt.
    let prompts_for_workers: Arc<hiker_core::prompts::Prompts> = Arc::new(
        hiker_core::prompts::Prompts::load(&root)
            .map_err(|e| HikerError::Io(format!("prompts: {e}")))?,
    );
    {
        // Forward queue events to the frontend.
        let app_for_queue = app.clone();
        let mut rx = tasks.subscribe();
        let cancel = tasks_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Ok(e) => { let _ = app_for_queue.emit("hiker:queue-event", &e); }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            }
        });
        // Maintenance tick: requeue expired leases + GC terminal rows.
        let q_for_tick = tasks.clone();
        let cancel_for_tick = tasks_cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_for_tick.cancelled() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        q_for_tick.tick_maintenance().await;
                    }
                }
            }
        });
        // Direct-LLM worker. Spawned whenever `[llm] enabled = true` —
        // the per-iteration `direct_worker.enabled` check inside
        // `run_direct_worker` honors live toggles from the settings UI
        // without a vault restart. (Spawning still requires `[llm]
        // enabled` because we need a valid LlmClient; flipping LLM on/off
        // remains restart-bound for now.)
        //
        // status: task-queue-raptor-triage-match
        // Build the non-LLM handler bundle so `RaptorTriageMatch` tasks
        // run the real classifier (`core::suggest::triage_match`) rather
        // than the LLM. Wired here (not in `core::tasks`) so the handler
        // can close over the session-scoped trees/vault/staging/store
        // handles without polluting the queue's API.
        let triage_handler: Arc<dyn hiker_core::tasks::NonLlmHandlers> =
            Arc::new(DirectWorkerHandlers {
                trees: trees.clone(),
                vault: vault.clone(),
                staging: staging.clone(),
                read_store: read_store.clone(),
                config: Arc::new(std::sync::RwLock::new(config.clone())),
                prompts: prompts_for_workers.clone(),
            });
        if config.llm.enabled {
            match hiker_core::llm::GraniteLlmClient::from_config(&config.llm) {
                Ok(client) => {
                    let llm_client: Arc<dyn hiker_core::llm::LlmClient> = Arc::new(client);
                    let q = tasks.clone();
                    let audit_for_worker = audit.clone();
                    let cancel = tasks_cancel.clone();
                    let handlers_for_worker = triage_handler.clone();
                    let parallelism = config.tasks.direct_worker.parallelism.max(1);
                    for _ in 0..parallelism {
                        let q = (*q).clone();
                        let client = llm_client.clone();
                        let audit = Some(audit_for_worker.clone());
                        let cancel = cancel.clone();
                        let handlers = Some(handlers_for_worker.clone());
                        tokio::spawn(async move {
                            hiker_core::tasks::run_direct_worker(q, client, audit, handlers, cancel).await;
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "tasks: direct worker not started (llm client build failed)",
                    );
                }
            }
        }
    }

    // status: cluster-editor-triage-scheduled-rerun
    //
    // Periodic re-run of the triage classifier over every note inside
    // the configured scope. The cron-shape parser is a follow-up — for
    // Sprint F we accept simple duration strings (`30m`, `1h`, `6h`,
    // `24h`, `7d`); cron expressions get logged and ignored. Empty
    // disables. Each tick enqueues one `RaptorTriageMatch` task at
    // `Low` priority per (saved-as-triage tree × note in scope).
    {
        let trees_for_sched = trees.clone();
        let read_store_for_sched = read_store.clone();
        let tasks_for_sched = tasks.clone();
        let cfg_sched_str = config.suggestions.triage.scheduled_rerun.clone();
        let cfg_scope = config.suggestions.triage.scope.clone();
        let interval = parse_rerun_interval(&cfg_sched_str);
        if let Some(every) = interval {
            tokio::spawn(async move {
                // Initial delay so we don't fire on startup.
                tokio::time::sleep(every).await;
                let mut ticker = tokio::time::interval(every);
                ticker.set_missed_tick_behavior(
                    tokio::time::MissedTickBehavior::Delay,
                );
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let saved: Vec<String> = match trees_for_sched.list_trees() {
                        Ok(rows) => rows
                            .into_iter()
                            .filter(|t| t.state == "saved-as-triage")
                            .map(|t| t.id)
                            .collect(),
                        Err(_) => continue,
                    };
                    if saved.is_empty() {
                        continue;
                    }
                    let all_paths = {
                        let store_guard = match read_store_for_sched.lock() {
                            Ok(g) => g,
                            Err(_) => continue,
                        };
                        match store_guard.all_note_paths() {
                            Ok(p) => p,
                            Err(_) => continue,
                        }
                    };
                    let scope_trim = cfg_scope.trim();
                    let scoped: Vec<String> = all_paths
                        .into_iter()
                        .filter(|p| scope_trim.is_empty() || p.starts_with(scope_trim))
                        .collect();
                    for rel in &scoped {
                        for tree_id in &saved {
                            let task = hiker_core::tasks::Task {
                                id: String::new(),
                                kind: hiker_core::tasks::TaskKind::RaptorTriageMatch {
                                    tree_id: tree_id.clone(),
                                    source_path: rel.clone(),
                                },
                                priority: hiker_core::tasks::Priority::Low,
                                shape: hiker_core::tasks::TaskShape::Direct,
                                payload: hiker_core::tasks::TaskPayload::default(),
                                output_schema: None,
                                submitted_at: std::time::SystemTime::now(),
                                metadata: serde_json::json!({
                                    "tree_id": tree_id,
                                    "source_path": rel,
                                    "trigger": "scheduled_rerun",
                                }),
                            };
                            let _ = tasks_for_sched.submit(task).await;
                        }
                    }
                }
            });
        } else if !cfg_sched_str.trim().is_empty() {
            eprintln!(
                "[hiker] suggestions.triage.scheduled_rerun: unsupported value {:?} — accepted forms are duration strings like '30m', '1h', '6h', '24h', '7d'. Cron expressions are not yet supported.",
                cfg_sched_str
            );
        }
    }

    // status: mcp-tool-toggles
    // Shared per-tool gate config. Held by the MCP handler so dispatches
    // read it live; updated by `set_setting` so flips in the settings UI
    // apply without a vault restart.
    let mcp_tools: Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>> =
        Arc::new(std::sync::RwLock::new(config.mcp.tools.clone()));

    // status: mcp-server-crate
    // Start the in-process MCP server. Failure to bind logs and continues —
    // the user's vault is more important than MCP availability.
    let mcp = match start_mcp(&vault, &root, &indexer, &watcher, &changes, &read_store, &config, &audit, &tasks, &mcp_tools, &staging).await {
        Ok(handle) => Some(handle),
        Err(hiker_mcp::StartError::Disabled) => None,
        Err(e) => {
            tracing::warn!(error = %e, "mcp: start failed");
            None
        }
    };

    // status: llm-prompts-file-store
    // Reuse the prompt store loaded earlier for the direct-worker
    // handlers. Cached on the session so chat_send doesn't re-read disk
    // per turn.
    let prompts = prompts_for_workers.clone();

    // status: llm-prompts-staleness-on-upgrade
    // Surface bundled-default drift once per session. Writes both a
    // tracing warn and an audit-log row per stale feature so the future
    // Prompts-tab can read either surface.
    match hiker_core::prompts::Prompts::staleness(&root) {
        Ok(stale) => {
            for feature in &stale {
                tracing::warn!(
                    feature = %feature,
                    "prompts: bundled default has drifted from the user's stamped hash; review and merge if desired",
                );
                audit.record(&hiker_core::audit::AuditEntry {
                    surface: "core::agent",
                    feature,
                    status: "stale_prompt",
                    error: None,
                    turn_id: None,
                    step_id: None,
                    details: serde_json::json!({
                        "message": "bundled default drifted; user override not clobbered",
                    }),
                });
            }
        }
        Err(e) => tracing::warn!(error = %e, "prompts: staleness check failed"),
    }

    // status: llm-providers-config
    // API-key preflight: surface a missing key at vault open rather
    // than waiting for the user's first chat send to fail. Logs *and*
    // emits `hiker:llm-warning` so the frontend can render a user-visible
    // toast. Per the spec's two-source rule, the literal `api_key`
    // (user-scope TOML) takes precedence; if it's set we don't need an
    // env var. Skipped when LLM is disabled or the provider doesn't
    // need a key (Ollama et al — empty `api_key_env` AND empty literal).
    if config.llm.enabled {
        let literal = config.llm.provider.api_key.as_str();
        let env_name = config.llm.provider.api_key_env.as_str();
        let literal_set = !literal.is_empty();
        let env_named_and_unset =
            !env_name.is_empty() && std::env::var(env_name).is_err();
        if !literal_set && env_named_and_unset {
            tracing::warn!(
                env = %env_name,
                backend = %config.llm.provider.backend,
                "llm: no api key — literal unset and env var missing; chat will fail until set",
            );
            let _ = app.emit(
                "hiker:llm-warning",
                serde_json::json!({
                    "kind": "missing_api_key",
                    "env": env_name,
                    "message": format!(
                        "{env_name} unset and no literal api_key — chat will fail until you set one in Settings or your shell",
                    ),
                }),
            );
        }
    }

    let chat_registry = Arc::new(chat::ChatRegistry::default());
    // status: chat-session-resume-latest
    // Adopt the most-recent on-disk session as the active one (if any
    // exist). The registry's `active` slot drives `chat_session_active`,
    // which the frontend calls on vault open to seed the panel.
    if let Err(e) = chat::resume_latest_at_open(&chat_registry, &root, &config) {
        tracing::warn!(error = %e, "sessions: resume_latest_at_open failed");
    }

    // Start the config-file watcher so external edits to either TOML are
    // picked up live and the UI re-applies settings without a restart.
    let config_watcher_cancel = tokio_util::sync::CancellationToken::new();
    {
        let app_for_cw = app.clone();
        let root_for_cw = root.clone();
        let cancel = config_watcher_cancel.clone();
        tokio::spawn(async move {
            start_config_watcher(app_for_cw, root_for_cw, cancel).await;
        });
    }

    let session = VaultSession {
        vault,
        root,
        indexer,
        watcher,
        changes,
        staging,
        trees,
        activity,
        autosave,
        config: RwLock::new(config),
        read_store,
        mcp,
        chat: chat_registry,
        prompts,
        audit,
        tasks,
        tasks_cancel,
        config_watcher_cancel,
        mcp_tools,
        staging_config,
    };

    let state = app.state::<AppState>();
    *state
        .session
        .lock()
        .map_err(|_| HikerError::Io("session lock poisoned".into()))? = Some(session);
    Ok(display)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IndexScope {
    All,
    Path { rel: String },
}

#[tauri::command]
async fn index(state: State<'_, AppState>, scope: IndexScope) -> Result<(), String> {
    let result = (|| -> Result<(IndexJob, hiker_core::indexer::IndexJobTx), String> {
        let job_sender = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            session.indexer.job_sender()
        };
        let job = match scope {
            // Explicit user-driven reindex: bypass the hash short-circuit so a
            // click on the menu actually re-embeds even when content is unchanged.
            IndexScope::All => IndexJob::FullScan { force: true },
            IndexScope::Path { rel } => IndexJob::Upsert { rel_path: rel, force: true },
        };
        Ok((job, job_sender))
    })();
    let send_result = match result {
        Ok((job, sender)) => sender.send(job).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    log_cmd_result("index", send_result)
}

/// Per-file index state for the tree-row markers and the active-file
/// status-bar mirror. See docs/index.md `tauri-cmd-file-index-state`.
///
/// status: tauri-cmd-file-index-state
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum IndexState {
    Indexed,
    Unsupported,
    Skipped { reason: String },
    Queued,
}

#[tauri::command]
fn index_state_for(state: State<AppState>, rel: String) -> Result<IndexState, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        if !hiker_core::indexer::is_indexable_path(&rel) {
            return Ok(IndexState::Unsupported);
        }
        if session.indexer.is_pending(&rel) {
            return Ok(IndexState::Queued);
        }
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        match read_store.get_note_by_path(&rel).map_err(|e| e.to_string())? {
            Some(row) if row.skipped => Ok(IndexState::Skipped {
                reason: row.skip_reason.unwrap_or_else(|| "skipped".into()),
            }),
            Some(_) => Ok(IndexState::Indexed),
            // No row yet for a supported file — either it's about to be indexed
            // or the watcher hasn't surfaced its create event. Either way, the
            // user's mental model is "queued."
            None => Ok(IndexState::Queued),
        }
    })();
    log_cmd_result("index_state_for", result)
}

/// Recursive count of indexable files under a folder. Backs the
/// delete-confirm modal so the UI doesn't have to walk the tree itself
/// via N round-trip `list_dir` calls. Empty vec / 0 for a file path.
/// Filters via `core::indexer::is_indexable_path` so the count matches
/// the indexer's allowlist (md / markdown / txt at v1) — same rule that
/// drives `tauri-cmd-file-index-state`.
#[tauri::command]
fn count_notes_in(state: State<AppState>, rel: String) -> Result<u32, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let files = session.vault.walk_indexable_files(&rel).map_err(|e| e.to_string())?;
        Ok(u32::try_from(files.len()).unwrap_or(u32::MAX))
    })();
    log_cmd_result("count_notes_in", result)
}

/// status: diff-core-module
/// Thin wrapper over `core::diff::compute`. Pure text-in / diff-out — no
/// session lock, no I/O, no async. The UI passes both strings (current
/// buffer text, snapshot blob via `change_content`, derived file via
/// `read_file`, etc.) and renders the returned `DiffResult`.
#[tauri::command]
fn compute_diff(
    before: String,
    after: String,
    intraline: Option<bool>,
) -> hiker_core::diff::DiffResult {
    hiker_core::diff::compute_with_intraline(&before, &after, intraline.unwrap_or(false))
}

#[tauri::command]
fn index_status(state: State<AppState>) -> Result<IndexStatus, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        Ok(session.indexer.status())
    })();
    log_cmd_result("index_status", result)
}

/// Create a new empty note in `folder` (vault-relative; `""` = vault root)
/// with an auto-suffixed `new-note-N.md` name. Returns the rel path of the
/// file actually created so the UI can open and inline-rename it.
///
/// status: create-note-button
#[tauri::command]
async fn create_note(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    log_cmd_result("create_note", create_note_inner(state, folder).await)
}

async fn create_note_inner(
    state: State<'_, AppState>,
    folder: String,
) -> Result<String, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::create_with_suffix(&watcher, &jobs, &vault, Some(&changes), &folder, "new-note").await
}

/// Atomic note rename. Backs both tree drag-and-drop and inline rename of
/// freshly-created notes. Errors leave both sides untouched per the spec.
///
/// status: drag-and-drop-move
#[tauri::command]
async fn move_note(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_note", move_note_inner(state, from, to).await)
}

async fn move_note_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_note(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Reveal a vault note in the OS file manager (Finder on macOS, Explorer on
/// Windows, default file manager on Linux). Backs the status-bar basename
/// click target.
///
/// status: status-bar-path-reveal
#[tauri::command]
fn reveal_in_file_manager(state: State<AppState>, rel: String) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let abs = session.vault.abs_path(&rel)?;
        reveal_path(&abs).map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("reveal_in_file_manager", result)
}

/// Spawn the platform's reveal-in-file-manager command. Runs the spawn
/// without waiting — the file manager UI is the user's concern, not ours.
#[cfg(target_os = "macos")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("open").arg("-R").arg(abs).spawn()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", abs.display()))
        .spawn()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn reveal_path(abs: &std::path::Path) -> std::io::Result<()> {
    // Linux has no portable "select this file" verb. Open the parent
    // directory in the user's file manager — close enough that the user
    // can pick the file out by sight, no dependency on a specific DE.
    let target = abs.parent().unwrap_or(abs);
    std::process::Command::new("xdg-open").arg(target).spawn()?;
    Ok(())
}

/// Folder rename: fs rename of the whole directory + bulk store path remap
/// for every contained `.md` file. Backs tree drag-and-drop of folder rows.
/// Empty subfolders move with the rename for free.
#[tauri::command]
async fn move_folder(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    log_cmd_result("move_folder", move_folder_inner(state, from, to).await)
}

async fn move_folder_inner(
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::ops::move_folder(&watcher, &jobs, &vault, Some(&changes), &from, &to).await
}

/// Soft-delete a note or folder. Backs the tree context-menu Delete entry
/// (`tree-context-delete`). Mirrors `move_note` shape: suppress watcher,
/// route through the indexer task so all writes go through its owned store
/// connection, await the reply, re-suppress for the post-op TTL window.
///
/// status: delete-note-core-cmd
#[tauri::command]
async fn delete_note(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result("delete_note", delete_note_inner(app, state, rel).await)
}

async fn delete_note_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::delete(&watcher, &jobs, &vault, Some(&changes), &rel).await;
    // Trash bin auto-refresh hook: forgetting this emit breaks
    // `tree-trash-flat-by-deleted` silently, so it stays in the Tauri layer
    // (core::ops doesn't depend on tauri).
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    result
}

/// Restore a previously soft-deleted entry from the vault trash. Backs the
/// undo affordance on the post-delete toast (`tree-context-delete`) and the
/// CLI `hiker trash restore` command.
///
/// status: vault-trash-restore
#[tauri::command]
async fn restore_trash_entry(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    log_cmd_result(
        "restore_trash_entry",
        restore_trash_entry_inner(app, state, id).await,
    )
}

async fn restore_trash_entry_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, trash, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            Trash::open(session.vault.root()),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    let result = hiker_core::ops::restore(&watcher, &jobs, &vault, Some(&changes), &trash, &id).await;
    // Trash bin auto-refresh hook — kept in the Tauri layer, see
    // `delete_note_inner` for the same rationale.
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    result
}

/// Disk-true listing of the vault trash. Backs the trash bin pinned at the
/// top of the file tree.
///
/// status: tree-trash-disk-listing
#[tauri::command]
fn list_trash(state: State<AppState>) -> Result<Vec<TrashListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.list_from_disk()
    })();
    log_cmd_result("list_trash", result)
}

/// Permanently empty the vault trash. Irrecoverable.
///
/// status: vault-trash-empty
#[tauri::command]
fn empty_trash(app: tauri::AppHandle, state: State<AppState>) -> Result<(), HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.empty()
    })();
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    log_cmd_result("empty_trash", result)
}

/// Permanently delete a single trash entry by its on-disk basename. Works on
/// orphaned entries too.
///
/// status: tree-trash-restore-action
#[tauri::command]
fn permanent_delete_trash_entry(
    app: tauri::AppHandle,
    state: State<AppState>,
    trashed_name: String,
) -> Result<(), HikerError> {
    tracing::info!(
        command = "permanent_delete_trash_entry",
        trashed_name = %trashed_name,
        "tauri cmd",
    );
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let trash = Trash::open(session.vault.root());
        trash.permanent_delete(&trashed_name)
    })();
    if result.is_ok() {
        let _ = app.emit("hiker:trash-changed", ());
    }
    log_cmd_result("permanent_delete_trash_entry", result)
}

/// Ordered chunk bounds for the active note. Empty vec when the note has
/// no row in the store (unsupported / queued / never indexed) or has zero
/// chunks. Spec: never errors on absence.
///
/// status: tauri-cmd-chunks-for-path
#[tauri::command]
fn chunks_for(state: State<AppState>, rel: String) -> Result<Vec<ChunkBounds>, String> {
    let result = (|| -> Result<Vec<ChunkBounds>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let mut bounds = {
            let read_store = session
                .read_store
                .lock()
                .map_err(|_| "read_store mutex poisoned".to_string())?;
            read_store.chunk_bounds_for(&rel).map_err(|e| e.to_string())?
        };
        // Read the file once and enrich each row's UTF-8 byte offsets with
        // matching UTF-16 char offsets. JS strings (and CM6) index by UTF-16
        // code units, so this saves the frontend from re-doing the encode
        // step every time it wants to map a chunk into the editor.
        if !bounds.is_empty() {
            if let Ok(text) = session.vault.read_file(&rel) {
                hiker_core::store::enrich_char_offsets(&text, &mut bounds);
            }
        }
        Ok(bounds)
    })();
    log_cmd_result("chunks_for", result)
}

/// Hybrid search across the vault. Runs the lexical + semantic backends
/// in parallel (per the requested modes) and returns all three buckets
/// (lexical, semantic, fused). The frontend renders whichever matches
/// its toggle state. Empty query, both modes off, or model-not-yet-ready
/// all return empty buckets without erroring — see
/// `embedder-first-run-nonblocking`.
///
/// status: search-tauri-cmd
#[tauri::command]
async fn search_vault(
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
fn related_notes(
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

/// Vault home stats payload: cheap counts off the index store, plus the live
/// queued count from the indexer handle. Surfaced by the home page; refreshed
/// on every `hiker:reindex-progress` tick.
///
/// status: vault-home-stats-widget
#[derive(Serialize)]
struct VaultHomeStats {
    total_notes: u32,
    total_chunks: u32,
    indexed: u32,
    skipped: u32,
    queued: u32,
}

#[tauri::command]
fn vault_home_stats(state: State<AppState>) -> Result<VaultHomeStats, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let stats: VaultStats = read_store.vault_stats().map_err(|e| e.to_string())?;
        let queued = session.indexer.status().queued;
        Ok(VaultHomeStats {
            total_notes: stats.total_notes,
            total_chunks: stats.total_chunks,
            indexed: stats.indexed,
            skipped: stats.skipped,
            queued,
        })
    })();
    log_cmd_result("vault_home_stats", result)
}

/// Top-N notes by filesystem mtime DESC. Backs the vault-home recently-modified
/// widget.
///
/// status: vault-home-recent-modified
#[tauri::command]
fn recent_notes_modified(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_mtime(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_modified", result)
}

/// Top-N notes by `last_accessed_at` DESC. Backs the vault-home
/// recently-accessed widget.
///
/// status: vault-home-recent-accessed
#[tauri::command]
fn recent_notes_accessed(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<RecentNote>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .recent_notes_by_access(limit.unwrap_or(10))
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("recent_notes_accessed", result)
}

/// Stamp `notes.last_accessed_at` via the indexer's owned writer. Called
/// from the frontend whenever a note becomes the active buffer. No-op when
/// the note isn't yet in the index — the next ingest creates the row, and
/// subsequent opens record normally.
///
/// status: note-access-tracking
#[tauri::command]
async fn note_accessed(state: State<'_, AppState>, rel: String) -> Result<(), String> {
    let jobs = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.indexer.job_sender()
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let send_result = jobs
        .send(IndexJob::TouchAccess { rel_path: rel, ts })
        .await
        .map_err(|e| e.to_string());
    log_cmd_result("note_accessed", send_result)
}

// status: note-properties-tab-content
/// DTO returned by `note_properties`. Mirrors `core::store::NoteProperties`
/// plus the changes count from `core::changes`. The struct uses the same
/// `#[serde(rename_all = "camelCase")]` as the core type.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotePropertiesDto {
    pub path: String,
    pub note_id: Option<String>,
    pub path_ids_id: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
    pub content_hash: Option<String>,
    pub extension: Option<String>,
    pub indexed_at: Option<i64>,
    pub embedder_version: Option<String>,
    pub skipped: Option<bool>,
    pub skip_reason: Option<String>,
    pub chunk_count: Option<i64>,
    pub last_accessed_at: Option<i64>,
    pub change_count: Option<i64>,
}

#[tauri::command]
fn note_properties(
    state: State<AppState>,
    rel: String,
) -> Result<NotePropertiesDto, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        let mut props = read_store
            .note_properties(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("note not indexed: {rel}"))?;
        let change_count = session
            .changes
            .count_for_path(&rel)
            .map_err(|e| e.to_string())?;
        props.change_count = Some(change_count);
        Ok(NotePropertiesDto {
            path: props.path,
            note_id: props.note_id,
            path_ids_id: props.path_ids_id,
            mtime: props.mtime,
            size: props.size,
            content_hash: props.content_hash,
            extension: props.extension,
            indexed_at: props.indexed_at,
            embedder_version: props.embedder_version,
            skipped: props.skipped,
            skip_reason: props.skip_reason,
            chunk_count: props.chunk_count,
            last_accessed_at: props.last_accessed_at,
            change_count: props.change_count,
        })
    })();
    log_cmd_result("note_properties", result)
}

/// Resolve a chat `@<rel-path-without-extension>` token to a concrete
/// vault path + file body. Probes `.md`, `.markdown`, `.txt` in order.
/// Errors with "note not found: <rel>" if no extension resolves — the
/// frontend toasts this and aborts the send (per `chat-input-at-note`).
///
/// status: chat-input-at-note
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AtNoteResolved {
    pub rel_path: String,
    pub content: String,
}

#[tauri::command]
fn chat_resolve_at_note(
    state: State<AppState>,
    rel_no_ext: String,
) -> Result<AtNoteResolved, String> {
    let result = (|| -> Result<AtNoteResolved, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let vault = session.vault.clone();
        drop(guard);
        for ext in hiker_core::indexer::INDEXABLE_EXTENSIONS {
            let candidate = format!("{}.{}", rel_no_ext, ext);
            if let Ok(abs) = vault.abs_path(&candidate) {
                if abs.is_file() {
                    if let Ok(content) = vault.read_file(&candidate) {
                        return Ok(AtNoteResolved {
                            rel_path: candidate,
                            content,
                        });
                    }
                }
            }
        }
        Err(format!("note not found: {rel_no_ext}"))
    })();
    log_cmd_result("chat_resolve_at_note", result)
}

/// Notes-table autocomplete for the chat `@`-mention popover. Empty
/// `prefix` returns the most-recently-accessed notes; non-empty filters by
/// case-insensitive basename substring with prefix-matches ranked first.
/// `limit` defaults to 10 to match the spec.
///
/// status: chat-input-at-autocomplete-tauri-cmd
#[tauri::command]
fn chat_at_autocomplete(
    state: State<AppState>,
    prefix: String,
    limit: Option<u32>,
) -> Result<Vec<hiker_core::store::AtSuggestion>, String> {
    let result = (|| {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let read_store = session
            .read_store
            .lock()
            .map_err(|_| "read_store mutex poisoned".to_string())?;
        read_store
            .at_autocomplete(&prefix, limit.unwrap_or(10) as usize)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("chat_at_autocomplete", result)
}

// ---------- changelog query / rollback commands ----------

/// Most recent changelog rows across the whole vault. Backs the home-page
/// recent-activity widget preview and detail view.
///
/// status: vault-home-recent-activity-widget
#[tauri::command]
fn recent_changes(
    state: State<AppState>,
    limit: Option<usize>,
) -> Result<Vec<ChangeRow>, HikerError> {
    let result = (|| -> Result<Vec<ChangeRow>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .recent(limit.unwrap_or(50))
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("recent_changes", result)
}

/// Total changelog row count. Backs the widget's "any rows yet?" gate so a
/// post-upgrade fresh vault doesn't render a confusing zero-count tile.
#[tauri::command]
fn changes_count(state: State<AppState>) -> Result<i64, HikerError> {
    let result = (|| -> Result<i64, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        session
            .changes
            .count()
            .map_err(|e| HikerError::Io(e.to_string()))
    })();
    log_cmd_result("changes_count", result)
}

/// Pull the post-op content blob for a change. Returns an empty string for
/// `op='deleted'` rows. Decoded as UTF-8 with a fallback to lossy so the
/// detail-view diff renderer always has something to show.
#[tauri::command]
fn change_content(
    state: State<AppState>,
    change_id: i64,
) -> Result<Option<String>, HikerError> {
    let result = (|| -> Result<Option<String>, HikerError> {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let blob = session
            .changes
            .content_at(change_id)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        Ok(blob.map(|b| String::from_utf8_lossy(&b).into_owned()))
    })();
    log_cmd_result("change_content", result)
}

/// Roll the file at `change.path` back to the most recent prior content
/// before `change_id`. Implementation per `changes.md` "Rollback":
///
/// 1. Resolve `(prior_id, prior_content)` via `previous_content_for_path`.
/// 2. Write that content via the standard `write_file_checked` path. The
///    write itself appends a *new* `'modified'` row tagged with
///    `metadata.rolled_back_from` so the activity feed shows the linkage.
///
/// Errors:
/// - `not_found` — no prior content within retention; rollback impossible.
/// - `drift` — the on-disk file changed since the change row was appended.
///   Caller can prompt the user to overwrite.
///
/// status: changes-rollback-helper
/// status: vault-home-recent-activity-detail
#[tauri::command]
async fn rollback_change(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result("rollback_change", rollback_change_inner(state, change_id).await)
}

#[derive(Serialize)]
struct RollbackOutcome {
    /// The id of the change row whose content was just rolled back to.
    /// Used by the UI's un-rollback affordance ("recently rolled back —
    /// restore?") so it knows which path/state was just left behind.
    prior_change_id: i64,
    /// The path that was rolled back. Convenience for UI refresh; identical
    /// to the original change row's path field.
    path: String,
    /// New on-disk hash after the rollback write. The Tauri write also
    /// appended a new changelog row; the UI re-reads `recent_changes` to
    /// pick that up.
    new_hash: String,
}

async fn rollback_change_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    // Resolve everything off the session up front so we don't hold the
    // session lock across the await/IO of the write.
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let row = session
            .changes
            .recent(0)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        let _ = row; // shut clippy up; we instead query via history below.
        // Resolve the change's path via a direct lookup — `recent` would
        // miss rows past the default window. The history call filters by
        // path post-hoc, so we use a single-row query.
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let (prior_id, prior_bytes) = changes_arc
        .previous_content_for_path(&target_path, change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "no earlier version of {target_path} is recorded — this is the oldest change in the log for this file"
            ))
        })?;

    let prior_content = String::from_utf8(prior_bytes)
        .map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    // Compute current on-disk hash for the drift-aware write. Empty hash
    // when the file is missing — matches the contract of write_file_checked.
    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash = vault.write_file_checked(&target_path, &current_hash, &prior_content)?;

    // Append the rollback row directly (rather than relying on the `write_file`
    // command) so we can stamp `metadata.rolled_back_from = <change_id>` per
    // spec — and so the on-disk file write + changelog append happen here as
    // one logical step instead of being routed through the Tauri write_file
    // command which doesn't carry the metadata.
    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(prior_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"rolled_back_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (rollback) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: prior_id,
        path: target_path,
        new_hash,
    })
}

/// Restore the file's content to match the given snapshot row. Writes the
/// row's `content` blob back to its `path` and appends a new `'modified'`
/// row stamped `metadata.restored_from = change_id`.
///
/// Different from `rollback_change` (which uses
/// `previous_content_for_path` to walk *before* the change): this command
/// matches the snapshot mental model — each row IS a saved version, and
/// "Restore" writes that version. The two share the changelog primitives
/// but live side-by-side: agent rollback per `mcp.md` calls
/// `rollback_change`; the home-page activity widget calls
/// `restore_snapshot`.
///
/// Errors:
/// - `not_found` — change row doesn't exist or has no content (e.g. a
///   `'deleted'` row, which carries NULL content by design).
/// - `drift` — the on-disk file changed since `expected_hash` was taken.
///   Surfaced as the same drift error `write_file_checked` produces; the
///   UI prompts the user.
///
/// status: vault-home-recent-activity-detail
#[tauri::command]
async fn restore_snapshot(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    log_cmd_result(
        "restore_snapshot",
        restore_snapshot_inner(state, change_id).await,
    )
}

async fn restore_snapshot_inner(
    state: State<'_, AppState>,
    change_id: i64,
) -> Result<RollbackOutcome, HikerError> {
    let (vault, changes_arc, target_path) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let target = lookup_change_path(&session.changes, change_id)?;
        (
            session.vault.clone(),
            session.changes.clone(),
            target,
        )
    };

    let blob = changes_arc
        .content_at(change_id)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "change {change_id} has no recorded content (deleted-row snapshots can't be restored directly — restore an earlier created/modified row instead)"
            ))
        })?;

    let snapshot_content =
        String::from_utf8(blob).map_err(|e| HikerError::NotUtf8(e.to_string()))?;

    let abs = vault.abs_path(&target_path)?;
    let current_hash = match std::fs::read(&abs) {
        Ok(bytes) => {
            let s = String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            hiker_core::hash_str(&s)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(HikerError::Io(e.to_string())),
    };

    let new_hash =
        vault.write_file_checked(&target_path, &current_hash, &snapshot_content)?;

    if let Err(e) = changes_arc.append(hiker_core::changes::ChangeAppend {
        path: &target_path,
        op: ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(snapshot_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"restored_from": change_id}),
    }) {
        tracing::warn!(error = %e, "changes: append (restore_snapshot) failed");
    }

    Ok(RollbackOutcome {
        prior_change_id: change_id,
        path: target_path,
        new_hash,
    })
}

/// Look up the path of a single change by id. Walks `recent` widely enough
/// to find it; rollback targets are usually recent so this is fine in
/// practice. Falls back to `NotFound` if the row is past the search window
/// (in which case retention has likely already dropped its content too).
fn lookup_change_path(changes: &Changes, change_id: i64) -> Result<String, HikerError> {
    // 5000 rows is well past the default 50-per-pair retention; if we don't
    // find it here, it's effectively gone.
    let rows = changes
        .recent(5000)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    rows.into_iter()
        .find(|r| r.id == change_id)
        .map(|r| r.path)
        .ok_or_else(|| HikerError::NotFound(format!("change {change_id}")))
}

/// status: task-queue-home-widget
/// status: task-queue-event-stream
/// Snapshot the current task-queue rows. Frontend seeds its local mirror
/// with this once at mount and applies `hiker:queue-event` deltas after.
#[tauri::command]
async fn tasks_snapshot(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::tasks::TaskRecord>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("tasks_snapshot", Ok::<_, String>(queue.snapshot().await))
}

/// status: task-queue-row-details
/// Lazy inspection: prompt + final result + final error + metadata for
/// a single task id. Returns `None` if the id has already been GC'd
/// past `terminal_retention_secs` (the user can scroll the queue tile
/// fast enough to miss the row).
#[tauri::command]
async fn task_details(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<hiker_core::tasks::TaskDetails>, String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    log_cmd_result("task_details", Ok::<_, String>(queue.details(&id).await))
}

/// status: task-queue-row-cancel-action
/// Cancel a task by id. Behavior depends on lease state — see
/// `core::tasks::Queue::cancel`.
#[tauri::command]
async fn tasks_cancel(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let queue = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    queue.cancel(&id).await;
    log_cmd_result("tasks_cancel", Ok::<(), String>(()))
}

// ---------- note-mutation producer surface ----------
//
// status: note-mutations-menu
// status: note-mutations-menu-task-shape
// status: note-mutation-reformat-as-markdown
// status: note-mutation-replace-original
// status: note-mutation-discard-derived
//
// The mutations menu submits a `Direct` `High`-priority task carrying the
// buffer's *live* text (per `chat-active-note-context-injection`'s same
// rule) plus the source extension. The direct-LLM worker drains it and
// produces text; on success the awaiter spawned here emits
// `hiker:note-mutation-applied` carrying the result content + the
// source-hash captured at submit time so the frontend can replace the
// open buffer (or hold + toast if the buffer was closed).

/// Frontend payload for a successful mutation result. The frontend
/// dispatches a single CM6 transaction replacing the active buffer's
/// content (when the buffer is still open and its content hash matches
/// `source_hash_at_submit`) or holds the result for a click-to-apply
/// toast (when the buffer has been closed).
#[derive(Debug, Clone, Serialize)]
struct NoteMutationAppliedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation_kind: &'a str,
    content: &'a str,
    source_hash_at_submit: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct NoteMutationFailedEvent<'a> {
    task_id: &'a str,
    source_path: &'a str,
    mutation: &'a str,
    error: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteMutationSubmitOutcome {
    pub task_id: String,
}

/// status: note-mutations-menu-task-shape
/// Submit a note-mutation task. `mutation` selects the prompt feature key
/// and is recorded in the changes-row metadata when the user accepts
/// (`note-mutation-replace-original`). Returns the task id immediately;
/// callers watch `hiker:queue-event` (and the new
/// `hiker:note-mutation-completed` / `-failed` events) for terminal state.
#[tauri::command]
async fn submit_note_mutation(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    let outcome = submit_note_mutation_inner(state, app, rel, mutation, source_extension, content)
        .await;
    log_cmd_result("submit_note_mutation", outcome)
}

async fn submit_note_mutation_inner(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    rel: String,
    mutation: String,
    source_extension: String,
    content: String,
) -> Result<NoteMutationSubmitOutcome, String> {
    if mutation != "reformat-as-markdown" {
        return Err(format!("unknown mutation: {mutation}"));
    }

    // Grab the per-vault handles we need before awaiting anywhere — clone
    // out from under the sync mutex. The source hash captured here is the
    // pre-mutation on-disk hash; the frontend uses it at apply-time to
    // decide whether the buffer's content still matches what the LLM saw.
    let (queue, prompts, source_hash) = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let source_hash = session
            .vault
            .read_file_with_hash(&rel)
            .map(|(_, h)| h)
            .map_err(|e| e.to_string())?;
        (
            session.tasks.clone(),
            session.prompts.clone(),
            source_hash,
        )
    };

    let title = std::path::Path::new(&rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&rel)
        .to_string();

    let prompt = prompts
        .render(
            "note_mutation_reformat_as_markdown",
            [
                ("title", title.as_str()),
                ("content", content.as_str()),
                ("source_extension", source_extension.as_str()),
            ],
        )
        .map_err(|e| e.to_string())?;

    let task = hiker_core::tasks::Task {
        id: String::new(),
        kind: hiker_core::tasks::TaskKind::NoteMutation {
            mutation: mutation.clone(),
            source_path: rel.clone(),
        },
        priority: hiker_core::tasks::Priority::High,
        shape: hiker_core::tasks::TaskShape::Direct,
        payload: hiker_core::tasks::TaskPayload {
            prompt,
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata: serde_json::json!({
            "source_hash_at_submit": source_hash,
        }),
    };

    let handle = queue.submit(task).await;
    let task_id = handle.id.clone();

    // Spawn the awaiter. On Completed → emit the result content as a
    // frontend event so the UI can replace the open buffer in a single
    // CM6 transaction (or hold + toast if the buffer is closed). On
    // Failed → toast via event. On Cancelled → silent (the user
    // already knows; queue events drive the widget).
    let app_for_await = app.clone();
    let rel_for_await = rel.clone();
    let mutation_for_await = mutation.clone();
    let source_hash_for_await = source_hash.clone();
    let task_id_for_await = task_id.clone();
    tokio::spawn(async move {
        let task_id = task_id_for_await;
        let outcome = handle.await_outcome().await;
        match outcome {
            hiker_core::tasks::TaskOutcome::Completed { value, .. } => {
                let body_owned: String;
                let result_body: &str = match &value {
                    serde_json::Value::String(s) => s.as_str(),
                    other => {
                        body_owned = serde_json::to_string_pretty(other)
                            .unwrap_or_else(|_| other.to_string());
                        body_owned.as_str()
                    }
                };
                // Empty / whitespace-only completions almost certainly
                // mean the provider returned a malformed or refused
                // response — replacing the buffer with empty bytes is a
                // worse failure than surfacing the problem.
                if result_body.trim().is_empty() {
                    let _ = app_for_await.emit(
                        "hiker:note-mutation-failed",
                        &NoteMutationFailedEvent {
                            task_id: &task_id,
                            source_path: &rel_for_await,
                            mutation: &mutation_for_await,
                            error: "empty response from LLM provider",
                        },
                    );
                    return;
                }
                let _ = app_for_await.emit(
                    "hiker:note-mutation-applied",
                    &NoteMutationAppliedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation_kind: &mutation_for_await,
                        content: result_body,
                        source_hash_at_submit: &source_hash_for_await,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Failed { error, .. } => {
                let _ = app_for_await.emit(
                    "hiker:note-mutation-failed",
                    &NoteMutationFailedEvent {
                        task_id: &task_id,
                        source_path: &rel_for_await,
                        mutation: &mutation_for_await,
                        error: &error,
                    },
                );
            }
            hiker_core::tasks::TaskOutcome::Cancelled { .. } => {
                // No preview, no toast — the queue widget already showed
                // the cancellation.
            }
        }
    });

    Ok(NoteMutationSubmitOutcome { task_id })
}

// ---------- frontend-bridge logger ----------

/// status: obs-frontend-bridge
/// Wire-side level enum for the `log_from_frontend` command. Tagged via
/// serde's snake_case so the JS payload `{ level: "error", ... }` round-trips
/// without an extra string match — Tauri's serde-driven arg deserialization
/// rejects garbage at the seam rather than at a `match` inside the body.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// status: obs-frontend-bridge
/// Pipe a structured event from the webview into the unified `tracing`
/// stream so `vault/.hiker/logs/hiker.log` is the single log file for both
/// halves of the app.
///
/// The `target` is constrained by convention to the `ui::` prefix — anything
/// else is rewritten to `ui::bad_target` and a `bad_target` field is recorded
/// rather than rejecting; the bridge should never become the reason a UI
/// error is lost. Each `fields` entry is flattened as a stringified key/value
/// pair on the event (matching the `error = %e` shape used in core).
///
/// Discipline: callers (the `Logger` wrapper in `ui/src/logger.ts`) MUST NOT
/// pass note body text, embeddings, or auth tokens through `fields`. Same
/// `obs-no-content` / `obs-no-secrets` rule that applies to every other
/// event in the system.
#[tauri::command]
fn log_from_frontend(
    level: LogLevel,
    target: String,
    message: String,
    fields: serde_json::Value,
) {
    // Flatten `fields` (expected to be an object) into a single compact JSON
    // string so the event carries one structured `fields` value rather than a
    // dynamic field set — `tracing::event!` field names are `'static` and
    // can't be built from a runtime map. Compact JSON keeps grep behavior
    // sane: fields land as `fields={"command":"open_vault_at",...}` in the
    // log line.
    let fields_str = match &fields {
        serde_json::Value::Object(_) => fields.to_string(),
        serde_json::Value::Null => "{}".to_string(),
        // Non-object payload is a caller bug; log as-is so it's grep-able
        // rather than dropping it.
        other => other.to_string(),
    };

    let target_str = if target.starts_with("ui::") {
        target.as_str()
    } else {
        // Stay in the `ui::` namespace so log filtering by target keeps
        // working even when a caller passes the wrong shape.
        "ui::bad_target"
    };
    let bad_target: Option<&str> = if target.starts_with("ui::") {
        None
    } else {
        Some(target.as_str())
    };

    match level {
        LogLevel::Trace => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::TRACE,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Debug => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::DEBUG,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Info => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::INFO,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Warn => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::WARN,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
        LogLevel::Error => {
            tracing::event!(
                target: "hiker::ui_bridge",
                tracing::Level::ERROR,
                ui_target = target_str,
                fields = %fields_str,
                bad_target = bad_target,
                "{}",
                message
            );
        }
    }
}

/// Wire the MCP server up against the vault session's handles. The server
/// task lives until the returned handle is dropped (which happens when the
/// `VaultSession` containing it is dropped — i.e. on vault swap or app
/// shutdown).
async fn start_mcp(
    vault: &Vault,
    root: &PathBuf,
    indexer: &IndexerHandle,
    watcher: &Arc<Watcher>,
    changes: &Arc<Changes>,
    read_store: &Arc<Mutex<Store>>,
    config: &Config,
    audit: &Arc<hiker_core::audit::AgentLog>,
    tasks: &Arc<hiker_core::tasks::Queue>,
    mcp_tools: &Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>>,
    staging: &Arc<Staging>,
) -> Result<hiker_mcp::McpServerHandle, hiker_mcp::StartError> {
    let deps = hiker_mcp::McpDeps {
        vault: vault.clone(),
        vault_root: root.clone(),
        read_store: read_store.clone(),
        jobs: indexer.job_sender(),
        watcher: watcher.clone(),
        changes: changes.clone(),
        embedder_provider: indexer.embedder_provider(),
        config: config.mcp.clone(),
        tools: mcp_tools.clone(),
        audit: audit.clone(),
        tasks: tasks.clone(),
        tasks_config: config.tasks.clone(),
        llm_enabled: config.llm.enabled,
        staging: staging.clone(),
    };
    hiker_mcp::start(deps).await
}

// status: autosave-backend-module
// Tauri command surface for the autosave layer. Each command parses args
// → calls `Autosave::*` → returns DTO; one-to-one with the Rust API per
// the spec.

fn with_autosave<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Autosave) -> Result<R, String>,
) -> Result<R, String> {
    let guard = state.session.lock().map_err(|e| e.to_string())?;
    let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
    f(&session.autosave)
}

#[tauri::command]
fn autosave_write(
    state: State<AppState>,
    path: String,
    contents: String,
) -> Result<(), String> {
    // Hash on the backend — frontend doesn't carry a blake3 dep, and
    // hashing a markdown buffer at 5s tick cadence is sub-millisecond
    // anyway. Same hash function (blake3) the rest of core uses, so
    // recover()'s on-disk-hash comparison stays apples-to-apples.
    let bytes = contents.as_bytes();
    let buffer_hash = hiker_core::hash_str(&contents);
    log_cmd_result(
        "autosave_write",
        with_autosave(&state, |a| {
            a.write(&path, bytes, &buffer_hash)
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_clear(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_clear",
        with_autosave(&state, |a| a.clear(&path).map_err(|e| e.to_string())),
    )
}

#[tauri::command]
fn autosave_save_tab_state(
    state: State<AppState>,
    state_payload: TabState,
) -> Result<(), String> {
    log_cmd_result(
        "autosave_save_tab_state",
        with_autosave(&state, |a| {
            a.save_tab_state(state_payload).map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_load_tab_state(state: State<AppState>) -> Result<Option<TabState>, String> {
    log_cmd_result(
        "autosave_load_tab_state",
        with_autosave(&state, |a| a.load_tab_state().map_err(|e| e.to_string())),
    )
}

/// Wire DTO for `autosave_recover` — the autosaved bytes ride as a UTF-8
/// string since hiker is a markdown editor and the frontend's CM6 can
/// only restore text-typed content. Non-UTF-8 sidecars (which shouldn't
/// happen for markdown buffers) become lossy strings; the recovery flow
/// still surfaces them so the user isn't silently denied their work.
#[derive(Serialize)]
struct RecoveredEntryDto {
    path: String,
    autosave_id: String,
    autosaved_content: String,
    autosaved_hash: String,
    on_disk_hash: Option<String>,
    saved_at_ms: i64,
}

impl From<RecoveredEntry> for RecoveredEntryDto {
    fn from(e: RecoveredEntry) -> Self {
        Self {
            path: e.path,
            autosave_id: e.autosave_id,
            autosaved_content: String::from_utf8_lossy(&e.autosaved_content).into_owned(),
            autosaved_hash: e.autosaved_hash,
            on_disk_hash: e.on_disk_hash,
            saved_at_ms: e.saved_at_ms,
        }
    }
}

#[tauri::command]
fn autosave_recover(state: State<AppState>) -> Result<Vec<RecoveredEntryDto>, String> {
    log_cmd_result(
        "autosave_recover",
        with_autosave(&state, |a| {
            a.recover()
                .map(|v| v.into_iter().map(RecoveredEntryDto::from).collect())
                .map_err(|e| e.to_string())
        }),
    )
}

#[tauri::command]
fn autosave_discard(state: State<AppState>, path: String) -> Result<(), String> {
    log_cmd_result(
        "autosave_discard",
        with_autosave(&state, |a| a.discard(&path).map_err(|e| e.to_string())),
    )
}

// ---------------------------------------------------------------------------
// Trails commands (slice U1)
// ---------------------------------------------------------------------------
// Tauri seams over `hiker_core::trails::*`. Each is the standard
// `parse args -> snapshot session deps -> call core -> return DTO` shape
// (see the rule in `hiker-dev` skill: commands are wrappers, not
// orchestrators). The `core::trails` ops own watcher suppression and
// changes append; this layer just plumbs the session deps in.
//
// status: active-trail-state

#[derive(Serialize)]
struct TrailCreatedDto {
    trail_doc_rel: String,
    trail_id: String,
}

#[derive(Serialize)]
struct WaypointAppendedDto {
    waypoint_rel: String,
    waypoint_id: String,
    trail_id: String,
}

#[derive(Serialize)]
struct WaypointRemovedDto {
    removed_count: u32,
}

#[tauri::command]
async fn trail_create(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let result = trail_create_inner(state, name).await;
    log_cmd_result("trail_create", result)
}

async fn trail_create_inner(
    state: State<'_, AppState>,
    name: String,
) -> Result<TrailCreatedDto, HikerError> {
    let (watcher, vault, jobs, changes, trails_cfg) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let cfg = session
            .config
            .read()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            cfg.trails.clone(),
        )
    };
    let outcome = hiker_core::trails::create_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trails_cfg,
        &name,
    )
    .await?;
    Ok(TrailCreatedDto {
        trail_doc_rel: outcome.trail_doc_rel,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
async fn trail_append_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
    let result = trail_append_waypoint_inner(
        state,
        trail_doc_rel,
        source_rel,
        parent_waypoint_id,
        annotation,
    )
    .await;
    log_cmd_result("trail_append_waypoint", result)
}

async fn trail_append_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    source_rel: String,
    parent_waypoint_id: Option<String>,
    annotation: Option<String>,
) -> Result<WaypointAppendedDto, HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    // Open a fresh Store reader for the call. `Store::open` against an
    // existing db is cheap (sub-ms warm) and is the documented per-command
    // read pattern (see `VaultSession.read_store` doc-comment). We can't
    // hand out the shared `Arc<Mutex<Store>>` here because the call holds
    // the reference across `.await` points and `MutexGuard` isn't `Send`.
    let mut store = Store::open(vault.root()).map_err(|e| HikerError::Io(e.to_string()))?;
    let outcome = hiker_core::trails::append_waypoint(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &mut store,
        &trail_doc_rel,
        &source_rel,
        parent_waypoint_id.as_deref(),
        annotation.as_deref(),
    )
    .await?;
    Ok(WaypointAppendedDto {
        waypoint_rel: outcome.waypoint_rel,
        waypoint_id: outcome.waypoint_id,
        trail_id: outcome.trail_id,
    })
}

#[tauri::command]
async fn trail_remove_waypoint(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let result = trail_remove_waypoint_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_remove_waypoint", result)
}

async fn trail_remove_waypoint_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<WaypointRemovedDto, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            Trash::open(session.vault.root()),
        )
    };
    let outcome = hiker_core::trails::remove_waypoint(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
        &waypoint_id,
    )
    .await?;
    Ok(WaypointRemovedDto {
        removed_count: outcome.removed_count,
    })
}

#[tauri::command]
fn trail_descendant_count(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: String,
) -> Result<u32, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        hiker_core::trails::descendant_count(
            &session.vault,
            &trail_doc_rel,
            &waypoint_id,
        )
    })();
    log_cmd_result("trail_descendant_count", result)
}

#[tauri::command]
async fn trail_delete(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<(), HikerError> {
    let result = trail_delete_inner(app, state, trail_doc_rel).await;
    log_cmd_result("trail_delete", result.map(|_| ()))
}

async fn trail_delete_inner(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<TrashEntry, HikerError> {
    let (watcher, vault, jobs, changes, trash) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            Trash::open(session.vault.root()),
        )
    };
    let entry = hiker_core::trails::delete_trail(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trash,
        &trail_doc_rel,
    )
    .await?;
    // Trash bin auto-refresh hook — same shape as `delete_note_inner`.
    let _ = app.emit("hiker:trash-changed", ());
    Ok(entry)
}

#[tauri::command]
fn trails_list(
    state: State<'_, AppState>,
) -> Result<Vec<hiker_core::trails::TrailListItem>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::list_trails(&session.vault, &store)
    })();
    log_cmd_result("trails_list", result)
}

#[tauri::command]
fn trail_get(
    state: State<'_, AppState>,
    trail_doc_rel: String,
) -> Result<hiker_core::trails::TrailDetail, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::get_trail(&session.vault, &store, &trail_doc_rel)
    })();
    log_cmd_result("trail_get", result)
}

/// Reverse-lookup: which trails contain `source_rel` as a waypoint at
/// any depth. Each hit pairs the derived-table `trail_id` with its
/// trail-doc rel-path so the UI can decide membership for a specific
/// trail (e.g. "is this note already a waypoint of the *active* trail?")
/// without a second round-trip per trail.
///
/// status: trail-add-to-active-from-editor-verb
#[tauri::command]
fn trails_containing_note(
    state: State<'_, AppState>,
    source_rel: String,
) -> Result<Vec<hiker_core::trails::TrailsContainingNoteHit>, HikerError> {
    let result = (|| {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        let store = session
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        hiker_core::trails::trails_containing_note_with_paths(
            &session.vault,
            &store,
            &source_rel,
        )
    })();
    log_cmd_result("trails_containing_note", result)
}

/// Set (or clear, with `None`) the active trail. Persists
/// `vault.active_trail` via the standard settings write-back path and
/// stamps `hiker.last_activated_at` on the trail-doc when activating.
///
/// status: active-trail-state
#[tauri::command]
async fn trail_set_active(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_active_inner(state, trail_doc_rel).await;
    log_cmd_result("trail_set_active", result)
}

async fn trail_set_active_inner(
    state: State<'_, AppState>,
    trail_doc_rel: Option<String>,
) -> Result<(), HikerError> {
    // Snapshot deps for the (optional) timestamp stamp before we touch
    // the settings file.
    let (watcher, vault, jobs, changes, root) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
            session.root.clone(),
        )
    };

    // Stamp the trail-doc's `last_activated_at` first (only when
    // activating a non-None value). If stamping fails we still proceed
    // to persist the setting — the timestamp is dropdown-ordering chrome,
    // not load-bearing for activation correctness.
    if let Some(rel) = trail_doc_rel.as_deref() {
        if let Err(e) = hiker_core::trails::stamp_last_activated_at(
            &watcher,
            &jobs,
            &vault,
            Some(&changes),
            rel,
        )
        .await
        {
            tracing::warn!(error = %e, path = %rel,
                "trail_set_active: stamp_last_activated_at failed; proceeding");
        }
    }

    let value = match trail_doc_rel {
        Some(s) => serde_json::Value::String(s),
        None => serde_json::Value::Null,
    };
    let updated =
        Config::set(SettingsScope::Vault, "vault.active_trail", value, &root)?;
    {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Config("session lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Config("no vault open".into()))?;
        let mut w = session
            .config
            .write()
            .map_err(|_| HikerError::Config("config lock poisoned".into()))?;
        *w = updated;
    }
    Ok(())
}

/// Set (or clear with `None`) the trail-doc's append cursor. Used by
/// the "Append from here" waypoint verb (`trail-append-from-here-verb`)
/// and the Trails-mode header's "Reset to main line" action
/// (`trail-reset-cursor-verb`); both surfaces land in slice C2.
///
/// status: trail-append-cursor
#[tauri::command]
async fn trail_set_append_cursor(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
) -> Result<(), HikerError> {
    let result = trail_set_append_cursor_inner(state, trail_doc_rel, waypoint_id).await;
    log_cmd_result("trail_set_append_cursor", result)
}

async fn trail_set_append_cursor_inner(
    state: State<'_, AppState>,
    trail_doc_rel: String,
    waypoint_id: Option<String>,
) -> Result<(), HikerError> {
    let (watcher, vault, jobs, changes) = {
        let guard = state
            .session
            .lock()
            .map_err(|_| HikerError::Io("lock poisoned".into()))?;
        let session = guard
            .as_ref()
            .ok_or_else(|| HikerError::Io("no vault open".into()))?;
        (
            session.watcher.clone(),
            session.vault.clone(),
            session.indexer.job_sender(),
            session.changes.clone(),
        )
    };
    hiker_core::trails::set_append_cursor(
        &watcher,
        &jobs,
        &vault,
        Some(&changes),
        &trail_doc_rel,
        waypoint_id.as_deref(),
    )
    .await
}

/// Background task: watches both config TOML files for external edits,
/// reloads the merged Config, swaps the in-memory copy, and emits
/// `hiker:config-reloaded` so the frontend re-applies settings.
///
/// Debounced at ~500 ms. Suppressed for 2 s after a `set_setting` write
/// (same `SUPPRESS_TTL` shape as the vault watcher) so UI-driven flips
/// don't round-trip back through the file watcher.
async fn start_config_watcher(
    app: tauri::AppHandle,
    vault_root: PathBuf,
    cancel: tokio_util::sync::CancellationToken,
) {
    use std::collections::HashSet;
    let paths = hiker_core::config::ConfigPaths::resolve(&vault_root);

    // Watch parent directories (notify works more reliably on dirs than
    // non-existent files, and some backends require dirs). Collect unique
    // parents; filter events by exact file path below.
    let mut parent_dirs: HashSet<PathBuf> = HashSet::new();
    parent_dirs.insert(
        paths
            .vault
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| paths.vault.clone()),
    );
    if let Some(ref user) = paths.user {
        parent_dirs.insert(
            user.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| user.clone()),
        );
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Result<NotifyEvent>>(16);

    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.blocking_send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "config watcher: failed to start");
            return;
        }
    };

    for dir in &parent_dirs {
        if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
            tracing::warn!(error = %e, dir = %dir.display(), "config watcher: failed to watch");
        }
    }

    const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
    const SUPPRESS_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    let mut last_reload = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break;
            }
            ev = rx.recv() => {
                match ev {
                    Some(Ok(event)) => {
                        // Only care about Modify / Create events on our config files.
                        let relevant = matches!(
                            event.kind,
                            NotifyEventKind::Modify(_) | NotifyEventKind::Create(_)
                        );
                        if !relevant {
                            continue;
                        }
                        let hits_config = event.paths.iter().any(|p| {
                            p == &paths.vault
                                || paths.user.as_ref().map_or(false, |u| p == u)
                        });
                        if !hits_config {
                            continue;
                        }

                        // Debounce: skip rapid bursts from a single save.
                        if last_reload.elapsed() < DEBOUNCE {
                            continue;
                        }

                        // Suppress: skip if `set_setting` wrote recently.
                        {
                            let suppressed = app.state::<AppState>()
                                .config_last_write
                                .lock()
                                .map_or(false, |g| g.map_or(false, |t| t.elapsed() < SUPPRESS_WINDOW));
                            if suppressed {
                                continue;
                            }
                        }

                        last_reload = Instant::now();

                        match hiker_core::config::Config::load(&vault_root) {
                            Ok(config) => {
                                // Swap in-memory copy + live mirrors.
                                let state = app.state::<AppState>();
                                if let Ok(guard) = state.session.lock() {
                                    if let Some(session) = guard.as_ref() {
                                        if let Ok(mut w) = session.config.write() {
                                            *w = config.clone();
                                        }
                                        session.tasks.set_cfg(config.tasks.clone());
                                        if let Ok(mut tools) = session.mcp_tools.write() {
                                            *tools = config.mcp.tools.clone();
                                        }
                                        if let Ok(mut s) = session.staging_config.write() {
                                            *s = config.staging.clone();
                                        }
                                    }
                                }
                                let _ = app.emit("hiker:config-reloaded", &config);
                                tracing::debug!("config watcher: reloaded, emitted hiker:config-reloaded");
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "config watcher: Config::load failed");
                            }
                        }
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "config watcher: notify error");
                    }
                    None => break,
                }
            }
        }
    }
}

// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject
// Tauri command surface for core::staging. Each command is the standard
// shape: parse args → snapshot session deps → call core → translate errors
// → return DTO.

#[derive(Debug, Default, Deserialize)]
struct StagingFilterArg {
    path: Option<String>,
    trail_id: Option<String>,
    surface: Option<String>,
    session_id: Option<String>,
}

impl From<StagingFilterArg> for StagingFilter {
    fn from(a: StagingFilterArg) -> Self {
        StagingFilter {
            path: a.path,
            trail_id: a.trail_id,
            surface: a.surface,
            session_id: a.session_id,
            state: None,
        }
    }
}

#[tauri::command]
fn staging_list(
    state: State<'_, AppState>,
    filter: Option<StagingFilterArg>,
) -> Result<Vec<Proposal>, String> {
    let result = (|| -> Result<Vec<Proposal>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: StagingFilter = filter.unwrap_or_default().into();
        session.staging.list(&f).map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_list", result)
}

#[tauri::command]
fn staging_count(state: State<'_, AppState>) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .count(&StagingFilter::default())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_count", result)
}

#[tauri::command]
fn staging_accept(
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<AcceptOutcome, String> {
    let result = (|| -> Result<AcceptOutcome, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let outcome = session
            .staging
            .accept(&proposal_id, &session.vault, Some(&session.changes))
            .map_err(|e| e.to_string())?;
        Ok(outcome)
    })();
    log_cmd_result("staging_accept", result)
}

#[tauri::command]
fn staging_reject(
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .reject(&proposal_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_reject", result)
}

#[tauri::command]
fn staging_accept_all(
    state: State<'_, AppState>,
) -> Result<Vec<AcceptOutcome>, String> {
    let result = (|| -> Result<Vec<AcceptOutcome>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .accept_all(
                &StagingFilter::default(),
                &session.vault,
                Some(&session.changes),
            )
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_accept_all", result)
}

/// Read the proposed `.md` content for a staging proposal so the frontend
/// can open it as a read-only preview buffer with the snapshot-preview diff
/// toggle pattern.
///
/// status: staging-review-activity-detail-filter
#[tauri::command]
fn staging_content(
    state: State<'_, AppState>,
    proposal_id: String,
) -> Result<String, String> {
    let result = (|| -> Result<String, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .staging
            .content(&proposal_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("staging_content", result)
}

// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject

// ---------- unified activity feed (changes + staging) ----------

/// Argument shape for `activity_list*` commands. Mirrors
/// `hiker_core::activity::ActivityFilter` but kept independent so the
/// snake_case JSON wire stays stable if the core struct gains fields.
#[derive(Debug, Deserialize)]
struct ActivityFilterArg {
    #[serde(default)]
    source: ActivitySource,
    #[serde(default = "default_activity_limit")]
    limit: usize,
    #[serde(default)]
    author_pattern: Option<String>,
    #[serde(default)]
    since_ms: Option<i64>,
}

fn default_activity_limit() -> usize {
    200
}

impl From<ActivityFilterArg> for ActivityFilter {
    fn from(a: ActivityFilterArg) -> Self {
        ActivityFilter {
            source: a.source,
            limit: a.limit,
            author_pattern: a.author_pattern,
            since_ms: a.since_ms,
        }
    }
}

// status: activity-feed-merged-query
#[tauri::command]
fn activity_list(
    state: State<'_, AppState>,
    filter: Option<ActivityFilterArg>,
) -> Result<Vec<ActivityItem>, String> {
    let result = (|| -> Result<Vec<ActivityItem>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session.activity.list(f).map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_list", result)
}

// status: activity-feed-merged-query
// status: status-bar-version-dropdown-uses-unified-feed
#[tauri::command]
fn activity_list_for_path(
    state: State<'_, AppState>,
    path: String,
    filter: Option<ActivityFilterArg>,
) -> Result<Vec<ActivityItem>, String> {
    let result = (|| -> Result<Vec<ActivityItem>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session
            .activity
            .list_for_path(&path, f)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_list_for_path", result)
}

// status: activity-feed-merged-query
#[tauri::command]
fn activity_count(
    state: State<'_, AppState>,
    filter: Option<ActivityFilterArg>,
) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let f: ActivityFilter = filter.map(Into::into).unwrap_or_default();
        session.activity.count(f).map_err(|e| e.to_string())
    })();
    log_cmd_result("activity_count", result)
}

// ---------- cluster editor (Sprint B) ----------
//
// Backing IPC for `docs/cluster-editor.md`. Every command thinks at the
// `Trees`-shape level: trees + nodes + history; the cluster build pass
// resolves a `BuildScope` against the read store before delegating to
// `core::cluster::build_and_persist`. UI surface lives in
// `ui/src/clusterEditor/`.
//
// status: cluster-editor-sidebar-mode

#[derive(Debug, Serialize)]
struct ClusterTreeRowDto {
    id: String,
    name: String,
    source: String,
    state: String,
    scope_json: String,
    method_json: String,
    created_at_ms: i64,
    vault_snapshot: Option<String>,
}

impl From<hiker_core::trees::TreeRow> for ClusterTreeRowDto {
    fn from(r: hiker_core::trees::TreeRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            source: r.source,
            state: r.state,
            scope_json: r.scope_json,
            method_json: r.method_json,
            created_at_ms: r.created_at_ms,
            vault_snapshot: r.vault_snapshot,
        }
    }
}

#[derive(Debug, Serialize)]
struct ClusterNodeDto {
    id: String,
    parent: Option<String>,
    kind: String,
    note_ref: Option<String>,
    note_path: Option<String>,
    note_title: Option<String>,
    name: String,
    summary: String,
    user_edited_name: bool,
    user_edited_summary: bool,
    policy_json: Option<String>,
    confidence: f32,
    summary_membership_churn: u32,
}

fn enrich_node(
    n: hiker_core::trees::EditableNode,
    store: &Store,
) -> ClusterNodeDto {
    let (note_path, note_title) = match &n.note_ref {
        Some(id) => match store.path_for_id(id) {
            Ok(Some(p)) => {
                let title = title_from_rel_path(&p);
                (Some(p), Some(title))
            }
            _ => (None, None),
        },
        None => (None, None),
    };
    let kind = match n.kind {
        hiker_core::trees::NodeKind::Cluster => "cluster",
        hiker_core::trees::NodeKind::Leaf => "leaf",
        hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
    };
    let policy_json = n
        .policy
        .as_ref()
        .and_then(|p| serde_json::to_string(p).ok());
    ClusterNodeDto {
        id: n.id,
        parent: n.parent,
        kind: kind.to_string(),
        note_ref: n.note_ref,
        note_path,
        note_title,
        name: n.name,
        summary: n.summary,
        user_edited_name: n.user_edited_name,
        user_edited_summary: n.user_edited_summary,
        policy_json,
        confidence: n.confidence,
        summary_membership_churn: n.summary_membership_churn,
    }
}

fn title_from_rel_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    last.strip_suffix(".md").unwrap_or(last).to_string()
}

// status: cluster-editor-triage-scheduled-rerun
//
// Best-effort parser for `[suggestions.triage].scheduled_rerun`. Sprint F
// supports simple duration suffixes (`s`/`m`/`h`/`d`); cron expressions
// (e.g. `"0 3 * * *"`) return `None` and are logged at startup so the
// user knows the value was unsupported. The cron parser proper is a
// follow-up — adding a dep just for Sprint F's lowest-priority slug is
// not justified.
fn parse_rerun_interval(s: &str) -> Option<std::time::Duration> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (num_part, unit) = match trimmed.chars().last() {
        Some(c) if "smhdSMHD".contains(c) => (&trimmed[..trimmed.len() - 1], c),
        _ => return None,
    };
    let n: u64 = num_part.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let secs = match unit.to_ascii_lowercase() {
        's' => n,
        'm' => n.checked_mul(60)?,
        'h' => n.checked_mul(3600)?,
        'd' => n.checked_mul(86400)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}

// status: cluster-summarize-llm
//
// Build the LLM-backed cluster summarizer the build pipeline hands to
// `core::cluster::build_tree`. Reads the `cluster_summarize` prompt
// body (user/vault-scoped per `core::prompts`) and constructs a fresh
// `GraniteLlmClient` from the live `[llm]` config. Errors when LLM is
// disabled — there is no fallback path.
// Common path for the three cluster tauri commands: pull the queue
// handle off the session, submit a Task carrying the requested
// `TaskKind`, and return its id immediately. The direct worker
// dispatches into `DirectWorkerHandlers::try_handle` on its own
// thread; the IPC reply is sub-millisecond.
async fn submit_cluster_task(
    state: &State<'_, AppState>,
    kind: hiker_core::tasks::TaskKind,
) -> Result<String, String> {
    let tasks = {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.tasks.clone()
    };
    let metadata = serde_json::json!({
        "variant": kind.variant_name(),
        "summary": kind.metadata_oneliner(),
    });
    let task = hiker_core::tasks::Task {
        id: String::new(),
        kind,
        priority: hiker_core::tasks::Priority::Normal,
        shape: hiker_core::tasks::TaskShape::Direct,
        payload: hiker_core::tasks::TaskPayload::default(),
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata,
    };
    let handle = tasks.submit(task).await;
    Ok(handle.id.clone())
}

// status: cluster-editor-sidebar-mode, cluster-editor-multiple-trees-open
#[tauri::command]
fn cluster_trees_list(state: State<'_, AppState>) -> Result<Vec<ClusterTreeRowDto>, String> {
    let result = (|| -> Result<Vec<ClusterTreeRowDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let rows = session.trees.list_trees().map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(Into::into).collect())
    })();
    log_cmd_result("cluster_trees_list", result)
}

// status: cluster-editor-sidebar-mode
#[tauri::command]
fn cluster_tree_get(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<Vec<ClusterNodeDto>, String> {
    let result = (|| -> Result<Vec<ClusterNodeDto>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let nodes = session
            .trees
            .list_nodes(&tree_id)
            .map_err(|e| e.to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        Ok(nodes.into_iter().map(|n| enrich_node(n, &store)).collect())
    })();
    log_cmd_result("cluster_tree_get", result)
}

// status: cluster-editor-new-tree-action, cluster-editor-tree-creation-modal,
//         cluster-editor-build-scope-picker, cluster-editor-build-params-advanced-disclosure
#[derive(Debug, Deserialize)]
struct ClusterTreeCreateArgs {
    name: String,
    /// "one-shot" | "saved-triage" (lifecycle hint).
    #[serde(default = "default_source_oneshot")]
    source: String,
    /// JSON of `core::cluster::BuildScope`. Resolved against the read
    /// store on the backend.
    scope_json: String,
    /// JSON of `core::cluster::BuildMethod`. Carries params inside.
    method_json: String,
}

fn default_source_oneshot() -> String {
    "one-shot".into()
}

#[tauri::command]
async fn cluster_tree_create(
    state: State<'_, AppState>,
    args: ClusterTreeCreateArgs,
) -> Result<String, String> {
    // Submit a ClusterBuildTree task to the queue. The direct worker's
    // non-LLM dispatch arm (`DirectWorkerHandlers::try_handle`) does the
    // actual build; the IPC reply lands as soon as the row is enqueued
    // so the UI stays responsive and the queue page surfaces progress.
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterBuildTree {
            name: args.name,
            source: args.source,
            scope_json: args.scope_json,
            method_json: args.method_json,
        },
    )
    .await;
    log_cmd_result("cluster_tree_create", result)
}

// status: cluster-build-rebuild
//
// Re-run the original build pipeline for `tree_id` against the current
// vault state. Produces a new draft tree row; the old tree is left
// intact so the user can compare / discard. User-edited names + summaries
// + policies on the old tree's clusters are preserved onto new clusters
// whose member-set Jaccard exceeds the merge threshold (0.5 default).
#[tauri::command]
async fn cluster_tree_rebuild(
    state: State<'_, AppState>,
    tree_id: String,
    new_name: Option<String>,
) -> Result<String, String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterRebuildTree { tree_id, new_name },
    )
    .await;
    log_cmd_result("cluster_tree_rebuild", result)
}

// status: cluster-editor-discard-draft
#[tauri::command]
fn cluster_tree_discard(state: State<'_, AppState>, tree_id: String) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session.trees.delete_tree(&tree_id).map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_tree_discard", result)
}

// status: cluster-editor-edit-name-summary
#[tauri::command]
fn cluster_node_rename(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    name: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .rename(&tree_id, &node_id, &name)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_rename", result)
}

#[tauri::command]
fn cluster_node_set_summary(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    summary: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_summary(&tree_id, &node_id, &summary)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_set_summary", result)
}

// status: cluster-editor-move-note-between-clusters, cluster-editor-promote-outlier
#[tauri::command]
fn cluster_node_move(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    new_parent: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .move_node(&tree_id, &node_id, new_parent.as_deref())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_move", result)
}

// status: cluster-editor-merge-siblings
#[tauri::command]
fn cluster_op_merge_siblings(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
) -> Result<String, String> {
    let result = (|| -> Result<String, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .merge_siblings(&tree_id, &node_ids)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_merge_siblings", result)
}

// status: cluster-editor-merge-children-up
#[tauri::command]
fn cluster_op_merge_children_up(
    state: State<'_, AppState>,
    tree_id: String,
    parent_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .merge_children_up(&tree_id, &parent_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_merge_children_up", result)
}

// status: cluster-editor-drop-cluster
#[tauri::command]
fn cluster_op_drop_cluster(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    outlier_bucket_id: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .drop_cluster(&tree_id, &node_id, &outlier_bucket_id)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_drop_cluster", result)
}

// status: cluster-editor-promote-outlier
#[tauri::command]
fn cluster_op_promote_outlier(
    state: State<'_, AppState>,
    tree_id: String,
    leaf_id: String,
    new_parent: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .promote_outlier(&tree_id, &leaf_id, new_parent.as_deref())
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_op_promote_outlier", result)
}

// status: cluster-editor-split-cluster
//
// Run HDBSCAN against just this cluster's leaf members with a tighter
// `min_cluster_size`; insert one new sub-cluster per HDBSCAN label, and
// re-parent each leaf onto its new sub-cluster. Leaves the parent's name
// untouched; new sub-clusters get TfIdf names so the rows aren't blank.
#[tauri::command]
fn cluster_op_split(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        // Walk to find all *leaf* descendants whose ancestor chain
        // includes node_id. We split over leaves, not over intermediate
        // cluster centroids, so the parent's nested structure (if any)
        // gets flattened by the split. Simpler + matches the spec's
        // "re-run HDBSCAN against just this cluster's members" line.
        let all = session.trees.list_nodes(&tree_id).map_err(|e| e.to_string())?;
        let mut children_by_parent: std::collections::HashMap<String, Vec<hiker_core::trees::EditableNode>> =
            std::collections::HashMap::new();
        for n in all.iter().cloned() {
            if let Some(p) = n.parent.clone() {
                children_by_parent.entry(p).or_default().push(n);
            }
        }
        let mut leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
        let mut stack = vec![node_id.clone()];
        while let Some(id) = stack.pop() {
            if let Some(kids) = children_by_parent.get(&id) {
                for k in kids {
                    if matches!(k.kind, hiker_core::trees::NodeKind::Leaf) {
                        leaves.push(k.clone());
                    } else {
                        stack.push(k.id.clone());
                    }
                }
            }
        }
        if leaves.len() < 4 {
            return Err("not enough members to split (need >= 4)".into());
        }
        // Pull each leaf's note embedding to feed HDBSCAN.
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let mut indexed: Vec<(hiker_core::trees::EditableNode, Vec<f32>, String)> = Vec::new();
        for l in &leaves {
            let Some(note_id) = l.note_ref.clone() else { continue };
            let Ok(Some(path)) = store.path_for_id(&note_id) else { continue };
            let Ok(Some(emb)) = store.note_embedding_for_path(&path) else { continue };
            indexed.push((l.clone(), emb, path));
        }
        drop(store);
        if indexed.len() < 4 {
            return Err("not enough embedded notes to split".into());
        }
        let embeddings: Vec<Vec<f32>> = indexed.iter().map(|(_, e, _)| e.clone()).collect();
        let min_size = (indexed.len() / 4).max(2);
        let assignments = hiker_core::cluster::partition(&embeddings, min_size, None)
            .map_err(|e| format!("cluster split: {e}"))?;
        let mut groups: std::collections::BTreeMap<i32, Vec<usize>> =
            std::collections::BTreeMap::new();
        for a in &assignments {
            if a.cluster_label == hiker_core::cluster::OUTLIER_LABEL {
                continue;
            }
            groups.entry(a.cluster_label).or_default().push(a.point_index);
        }
        if groups.len() < 2 {
            return Err("split produced fewer than 2 clusters".into());
        }
        let mut new_cluster_ids: Vec<String> = Vec::new();
        let mut new_clusters: Vec<serde_json::Value> = Vec::new();
        let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
        for (label, idxs) in groups {
            let new_id = format!("split-{}-{}", node_id, label);
            // Synthesize a template name from member paths.
            let titles: Vec<String> = idxs.iter().map(|&i| title_from_rel_path(&indexed[i].2)).collect();
            let name = titles.join(" / ");
            session
                .trees
                .insert_single_node(
                    &tree_id,
                    hiker_core::trees::NodeInsert {
                        node_id: new_id.clone(),
                        parent_id: Some(node_id.clone()),
                        kind: hiker_core::trees::NodeKind::Cluster,
                        note_id: None,
                        name: name.clone(),
                        summary: String::new(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 0.5,
                        summary_membership_churn: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
            for &i in &idxs {
                leaf_moves.push((indexed[i].0.id.clone(), Some(new_id.clone())));
            }
            new_clusters.push(serde_json::json!({
                "node_id": new_id,
                "parent_id": node_id,
                "kind": "cluster",
                "name": name,
                "summary": "",
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": null,
                "confidence": 0.5,
                "summary_membership_churn": 0,
            }));
            new_cluster_ids.push(new_id);
        }
        session
            .trees
            .reparent_many(&tree_id, &leaf_moves)
            .map_err(|e| e.to_string())?;
        // Freshly-inserted sub-clusters have summaries that describe
        // exactly the leaves they just received — so the churn that
        // `reparent_many` bumped on them is misleading. Zero it out;
        // future leaf moves into / out of these clusters will accumulate
        // real churn from a true baseline.
        for id in &new_cluster_ids {
            let _ = session.trees.reset_churn(&tree_id, id);
        }
        session
            .trees
            .record_split(&tree_id, &node_id, &new_clusters, &leaf_moves)
            .map_err(|e| e.to_string())?;
        Ok(new_cluster_ids)
    })();
    log_cmd_result("cluster_op_split", result)
}

// status: cluster-editor-recluster-subtree
// status: cluster-editor-recluster-subtree-policy-loss
// status: cluster-editor-recluster-subtree-placement-decoupled
//
// Re-run the full recursive cluster build pipeline against just the
// selected node's leaf descendants, then replace the subtree in place.
// The selected node's own row is preserved (id, name, summary,
// user-edit flags, policy); every descendant cluster row is deleted,
// freshly-built cluster nodes are inserted under the selected node,
// and the surviving leaves re-parent onto their new positions.
//
// Differs from `cluster_op_split`: split is one-level (one HDBSCAN pass
// produces a single new layer of children); recluster runs the full
// recursive build_tree pipeline so every level beneath the selected
// node is rebuilt. Always emits a `Cluster`-shaped subtree regardless
// of the surrounding tree's method (matches Split's behavior per
// `cluster-build-from-folders-uniform-output`).
//
// The reshape is structural only — it does not touch the filesystem.
// Already-placed notes stay where they are on disk; future triage
// classifications use the new structure (per
// `cluster-editor-recluster-subtree-placement-decoupled`).
#[derive(Debug, Deserialize)]
struct ClusterOpReclusterArgs {
    tree_id: String,
    node_id: String,
    /// JSON of `core::cluster::ClusterParams`. UI builds this from the
    /// advanced disclosure (with `min_cluster_size` halved by default).
    cluster_params_json: String,
    /// When true, copy the selected node's resolved policy onto every
    /// new direct child as an explicit policy. Default off per spec.
    #[serde(default)]
    carry_policies_down: bool,
}

#[tauri::command]
async fn cluster_op_recluster_subtree(
    state: State<'_, AppState>,
    args: ClusterOpReclusterArgs,
) -> Result<String, String> {
    let result = submit_cluster_task(
        &state,
        hiker_core::tasks::TaskKind::ClusterReclusterSubtree {
            tree_id: args.tree_id,
            node_id: args.node_id,
            cluster_params_json: args.cluster_params_json,
            carry_policies_down: args.carry_policies_down,
        },
    )
    .await;
    log_cmd_result("cluster_op_recluster_subtree", result)
}

// Body of the recluster operation, lifted from the original sync tauri
// command. Runs from inside the direct-worker's non-LLM dispatch
// (`DirectWorkerHandlers::try_handle`) so the LLM-heavy rebuild + the
// tree-mutation pass both happen on the worker thread rather than on
// the IPC channel. Operates on `DirectWorkerHandlers`' refs rather
// than reaching back into the session.
fn recluster_subtree_in_worker(
    handlers: &DirectWorkerHandlers,
    tree_id: &str,
    node_id: &str,
    cluster_params_json: &str,
    carry_policies_down: bool,
) -> Result<serde_json::Value, String> {
    let params: hiker_core::cluster::ClusterParams = serde_json::from_str(cluster_params_json)
        .map_err(|e| format!("cluster_params_json: {e}"))?;

    // Walk the subtree under `node_id` to collect every descendant
    // cluster (for snapshot + deletion) and every leaf (to feed the
    // rebuild and to know prior parents for undo).
    let all = handlers
        .trees
        .list_nodes(tree_id)
        .map_err(|e| e.to_string())?;
    let mut children_by_parent: std::collections::HashMap<
        String,
        Vec<hiker_core::trees::EditableNode>,
    > = std::collections::HashMap::new();
    for n in all.iter().cloned() {
        if let Some(p) = n.parent.clone() {
            children_by_parent.entry(p).or_default().push(n);
        }
    }
    let mut by_id: std::collections::HashMap<String, hiker_core::trees::EditableNode> =
        std::collections::HashMap::new();
    for n in all.iter().cloned() {
        by_id.insert(n.id.clone(), n);
    }
    let root_node = by_id
        .get(node_id)
        .cloned()
        .ok_or_else(|| format!("node not found: {node_id}"))?;
    if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
        return Err("recluster only works on cluster nodes".into());
    }

    let mut descendant_clusters: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut stack = vec![node_id.to_string()];
    while let Some(id) = stack.pop() {
        if let Some(kids) = children_by_parent.get(&id) {
            for k in kids {
                match k.kind {
                    hiker_core::trees::NodeKind::Leaf => {
                        descendant_leaves.push(k.clone());
                    }
                    _ => {
                        descendant_clusters.push(k.clone());
                        stack.push(k.id.clone());
                    }
                }
            }
        }
    }
    if descendant_leaves.len() < 4 {
        return Err("not enough leaves under this cluster to recluster (need >= 4)".into());
    }

    let resolved_policy: Option<hiker_core::trees::NodePolicy> = {
        let mut cursor: Option<String> = Some(node_id.to_string());
        let mut found = None;
        while let Some(id) = cursor {
            if let Some(n) = by_id.get(&id) {
                if let Some(p) = &n.policy {
                    found = Some(p.clone());
                    break;
                }
                cursor = n.parent.clone();
            } else {
                break;
            }
        }
        found
    };

    // Pull each leaf's note embedding to feed build_tree.
    let mut store = handlers.read_store.lock().map_err(|e| e.to_string())?;
    let mut note_inputs: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for l in &descendant_leaves {
        let Some(note_id_) = l.note_ref.clone() else {
            continue;
        };
        let Ok(Some(path)) = store.path_for_id(&note_id_) else {
            continue;
        };
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        note_inputs.push(hiker_core::cluster::NoteInput {
            id: note_id_,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    drop(store);
    if note_inputs.len() < 4 {
        return Err("not enough embedded notes to recluster (need >= 4)".into());
    }

    let prior_subtree: Vec<serde_json::Value> = descendant_clusters
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "parent_id": c.parent,
                "kind": match c.kind {
                    hiker_core::trees::NodeKind::Cluster => "cluster",
                    hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
                    hiker_core::trees::NodeKind::Leaf => "leaf",
                },
                "note_id": c.note_ref,
                "name": c.name,
                "summary": c.summary,
                "user_edited_name": c.user_edited_name,
                "user_edited_summary": c.user_edited_summary,
                "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                "confidence": c.confidence,
                "summary_membership_churn": c.summary_membership_churn,
            })
        })
        .collect();
    let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
        .iter()
        .map(|l| (l.id.clone(), l.parent.clone()))
        .collect();

    // Run the recursive build pass. Always Cluster-method (per spec).
    let summarizer = handlers.cluster_summarizer()?;
    let scope = hiker_core::cluster::BuildScope::Notes {
        ids: note_inputs.iter().map(|n| n.id.clone()).collect(),
        source_types: Vec::new(),
    };
    let build_method = hiker_core::cluster::BuildMethod::Cluster { params: params.clone() };
    let result =
        hiker_core::cluster::build_tree(scope, build_method, &note_inputs, &summarizer)
            .map_err(|e| format!("recluster build: {e}"))?;

    let ns = format!("recluster-{node_id}");
    let rename_id = |id: &str| -> String { format!("{}-{}", ns, id) };

    let levels = &result.tree.levels;
    let mut new_nodes_snapshot: Vec<serde_json::Value> = Vec::new();
    let mut new_cluster_ids: Vec<String> = Vec::new();

    let mut parent_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for level in levels.iter().skip(1) {
        for node in level {
            for child in &node.members {
                parent_of.insert(child.clone(), node.id.clone());
            }
        }
    }
    let top_level_idx = levels.len() - 1;
    let top = &levels[top_level_idx];
    let mut absorbed_top_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if top.len() == 1 {
        absorbed_top_ids.insert(top[0].id.clone());
    }

    for (level_idx, level) in levels.iter().enumerate().rev() {
        for node in level {
            if absorbed_top_ids.contains(&node.id) {
                continue;
            }
            let new_id = rename_id(&node.id);
            let parent_id = match parent_of.get(&node.id) {
                Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                _ => node_id.to_string(),
            };
            let policy = if carry_policies_down && parent_id == node_id {
                resolved_policy.clone()
            } else {
                None
            };
            let insert = hiker_core::trees::NodeInsert {
                node_id: new_id.clone(),
                parent_id: Some(parent_id.clone()),
                kind: hiker_core::trees::NodeKind::Cluster,
                note_id: None,
                name: node.name.clone(),
                summary: node.summary.clone(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: policy.clone(),
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            };
            new_nodes_snapshot.push(serde_json::json!({
                "id": new_id,
                "parent_id": parent_id,
                "kind": "cluster",
                "note_id": null,
                "name": insert.name,
                "summary": insert.summary,
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                "confidence": insert.confidence,
                "summary_membership_churn": 0,
                "level": level_idx,
            }));
            new_cluster_ids.push(new_id);
        }
    }

    let mut leaf_target: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(leaf_level) = levels.first() {
        for cluster in leaf_level {
            let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                node_id.to_string()
            } else {
                rename_id(&cluster.id)
            };
            for note_id_ in &cluster.members {
                leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
            }
        }
    }
    for note_id_ in &result.tree.outliers {
        leaf_target
            .entry(note_id_.clone())
            .or_insert_with(|| node_id.to_string());
    }

    let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
    for l in &descendant_leaves {
        let target = l
            .note_ref
            .as_ref()
            .and_then(|nid| leaf_target.get(nid).cloned())
            .unwrap_or_else(|| node_id.to_string());
        leaf_moves.push((l.id.clone(), Some(target)));
    }

    let preserved_chain: Vec<(String, u32)> = {
        let mut chain: Vec<(String, u32)> = Vec::new();
        let mut cursor: Option<String> = Some(node_id.to_string());
        while let Some(id) = cursor {
            if let Some(n) = by_id.get(&id) {
                chain.push((n.id.clone(), n.summary_membership_churn));
                cursor = n.parent.clone();
            } else {
                break;
            }
        }
        chain
    };

    for c in &descendant_clusters {
        handlers
            .trees
            .delete_node(tree_id, &c.id)
            .map_err(|e| e.to_string())?;
    }
    for snap in &new_nodes_snapshot {
        let id = snap.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let parent_id = snap
            .get("parent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let policy: Option<hiker_core::trees::NodePolicy> = snap.get("policy").and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
            _ => None,
        });
        let centroid = None;
        handlers
            .trees
            .insert_single_node(
                tree_id,
                hiker_core::trees::NodeInsert {
                    node_id: id,
                    parent_id,
                    kind: hiker_core::trees::NodeKind::Cluster,
                    note_id: None,
                    name: snap
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    summary: snap
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy,
                    centroid,
                    confidence: snap
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as f32,
                    summary_membership_churn: 0,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    handlers
        .trees
        .reparent_many(tree_id, &leaf_moves)
        .map_err(|e| e.to_string())?;

    for (id, prior) in &preserved_chain {
        let _ = handlers.trees.set_churn(tree_id, id, *prior);
    }
    for id in &new_cluster_ids {
        let _ = handlers.trees.reset_churn(tree_id, id);
    }

    handlers
        .trees
        .record_recluster_subtree(
            tree_id,
            node_id,
            &prior_subtree,
            &prior_leaf_parents,
            &new_nodes_snapshot,
            &leaf_moves,
            if carry_policies_down {
                resolved_policy.as_ref()
            } else {
                None
            },
        )
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({ "new_cluster_ids": new_cluster_ids }))
}

// ── Clustering review tab Tauri commands ─────────────────────────────
//
// status: cluster-review-tab
// status: cluster-review-tab-run-clustering
// status: cluster-review-tab-structural-pass-no-llm
// status: cluster-review-tab-confirm-and-name
//
// These three commands replace the legacy `cluster_tree_create` /
// `cluster_op_recluster_subtree` end-to-end paths for the UI. The
// legacy commands stay for non-UI callers (CLI, tests) until those have
// alternate plumbing. The UI now drives a two-phase flow:
//
//   1. `cluster_run_structural` runs HDBSCAN-only (no LLM) and returns
//      a serialized `BuiltClusterTree` plus per-note titles. Nothing is
//      persisted.
//   2. `cluster_persist_built_tree` (new-tree / rebuild) or
//      `cluster_op_recluster_subtree_from_built` (recluster) takes the
//      structural DTO + user-renamed names, persists rows, and submits
//      `RaptorSummarize` tasks for the un-renamed clusters.

#[derive(Debug, Deserialize)]
struct ClusterRunStructuralArgs {
    /// JSON of `core::cluster::BuildScope` — for the new-tree case;
    /// ignored when `recluster_target` is set.
    #[serde(default)]
    scope_json: Option<String>,
    /// JSON of `core::cluster::BuildMethod`. Carries the user-chosen
    /// `ClusterParams` / `FolderDeriveParams` (the structural pass
    /// forces `summarize = None` regardless).
    method_json: String,
    /// When set, scope is computed from the named subtree's leaves
    /// rather than `scope_json`. Used by the recluster-subtree flow.
    #[serde(default)]
    recluster_target: Option<ReclusterTarget>,
}

#[derive(Debug, Deserialize)]
struct ReclusterTarget {
    tree_id: String,
    node_id: String,
}

#[derive(Debug, Serialize)]
struct StructuralBuildDto {
    /// Echoed back so the persist command doesn't need to re-resolve
    /// scope — the resolved `BuildScope::Notes { ids }` is used directly.
    scope_json: String,
    method_json: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    /// Map of note_id → display title so the UI can render the
    /// preview rows without N more round-trips.
    note_titles: std::collections::HashMap<String, String>,
}

/// Resolve a `BuildScope` to a `Vec<NoteInput>` by walking the read
/// store. Lazy-populates missing note embeddings. Standalone twin of
/// `DirectWorkerHandlers::notes_for_scope` for the IPC-side commands.
fn notes_for_scope_via_session(
    session: &VaultSession,
    scope: &hiker_core::cluster::BuildScope,
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let mut store = session.read_store.lock().map_err(|e| e.to_string())?;
    let candidate_paths: Vec<String> = match scope {
        hiker_core::cluster::BuildScope::Vault { .. } => {
            store.all_note_paths().map_err(|e| e.to_string())?
        }
        hiker_core::cluster::BuildScope::Folder { rel, .. } => {
            let prefix = if rel.ends_with('/') || rel.is_empty() {
                rel.clone()
            } else {
                format!("{rel}/")
            };
            store
                .all_note_paths()
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|p| prefix.is_empty() || p.starts_with(&prefix))
                .collect()
        }
        hiker_core::cluster::BuildScope::Notes { ids, .. } => {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Ok(Some(p)) = store.path_for_id(id) {
                    out.push(p);
                }
            }
            out
        }
    };
    // status: cluster-build-scope-source-types
    let mut notes: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for path in candidate_paths {
        if !scope.matches_path(&path) {
            continue;
        }
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let note_id = match store.id_for_path(&path) {
            Ok(Some(i)) => i,
            _ => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
        notes.push(hiker_core::cluster::NoteInput {
            id: note_id,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    Ok(notes)
}

/// For the recluster-subtree case: walk the descendants of
/// `(tree_id, node_id)` and pull their leaves' embeddings the same way
/// `recluster_subtree_in_worker` does. Returns the resolved note inputs
/// (which the structural build will operate on).
fn notes_for_recluster_target(
    session: &VaultSession,
    target: &ReclusterTarget,
) -> Result<Vec<hiker_core::cluster::NoteInput>, String> {
    let all = session
        .trees
        .list_nodes(&target.tree_id)
        .map_err(|e| e.to_string())?;
    let mut children_by_parent: std::collections::HashMap<
        String,
        Vec<hiker_core::trees::EditableNode>,
    > = std::collections::HashMap::new();
    for n in all.iter().cloned() {
        if let Some(p) = n.parent.clone() {
            children_by_parent.entry(p).or_default().push(n);
        }
    }
    let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
    let mut stack = vec![target.node_id.clone()];
    while let Some(id) = stack.pop() {
        if let Some(kids) = children_by_parent.get(&id) {
            for k in kids {
                match k.kind {
                    hiker_core::trees::NodeKind::Leaf => descendant_leaves.push(k.clone()),
                    _ => stack.push(k.id.clone()),
                }
            }
        }
    }
    let mut store = session.read_store.lock().map_err(|e| e.to_string())?;
    let mut note_inputs: Vec<hiker_core::cluster::NoteInput> = Vec::new();
    for l in &descendant_leaves {
        let Some(nid) = l.note_ref.clone() else { continue };
        let Ok(Some(path)) = store.path_for_id(&nid) else { continue };
        let emb = match store.note_embedding_for_path(&path) {
            Ok(Some(e)) => e,
            Ok(None) => match store.compute_and_store_note_embedding(&path) {
                Ok(Some(e)) => e,
                _ => continue,
            },
            Err(_) => continue,
        };
        let title = title_from_rel_path(&path);
        let folder = path.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default();
        note_inputs.push(hiker_core::cluster::NoteInput {
            id: nid,
            title,
            summary: String::new(),
            folder,
            embedding: emb,
        });
    }
    Ok(note_inputs)
}

// status: cluster-review-tab-run-clustering
#[tauri::command]
fn cluster_run_structural(
    state: State<'_, AppState>,
    args: ClusterRunStructuralArgs,
) -> Result<StructuralBuildDto, String> {
    let result = (|| -> Result<StructuralBuildDto, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let method: hiker_core::cluster::BuildMethod = serde_json::from_str(&args.method_json)
            .map_err(|e| format!("method_json: {e}"))?;
        let (scope, notes) = match args.recluster_target {
            Some(target) => {
                let notes = notes_for_recluster_target(session, &target)?;
                if notes.len() < 4 {
                    return Err("not enough embedded notes under this cluster to recluster (need >= 4)".into());
                }
                let scope = hiker_core::cluster::BuildScope::Notes {
                    ids: notes.iter().map(|n| n.id.clone()).collect(),
                    source_types: Vec::new(),
                };
                (scope, notes)
            }
            None => {
                let scope_json = args
                    .scope_json
                    .as_deref()
                    .ok_or_else(|| "scope_json required when recluster_target is absent".to_string())?;
                let scope: hiker_core::cluster::BuildScope = serde_json::from_str(scope_json)
                    .map_err(|e| format!("scope_json: {e}"))?;
                let notes = notes_for_scope_via_session(session, &scope)?;
                if notes.is_empty() {
                    return Err("no notes with embeddings found in scope".into());
                }
                (scope, notes)
            }
        };
        // Capture titles before `notes` is consumed by the build pass.
        let note_titles: std::collections::HashMap<String, String> = notes
            .iter()
            .map(|n| (n.id.clone(), n.title.clone()))
            .collect();
        let build_result = hiker_core::cluster::build_tree_structural(
            scope.clone(),
            method.clone(),
            &notes,
        )
        .map_err(|e| format!("structural build: {e}"))?;
        let scope_json = serde_json::to_string(&build_result.scope)
            .map_err(|e| format!("scope serialize: {e}"))?;
        let method_json = serde_json::to_string(&build_result.method)
            .map_err(|e| format!("method serialize: {e}"))?;
        Ok(StructuralBuildDto {
            scope_json,
            method_json,
            tree: build_result.tree,
            note_titles,
        })
    })();
    log_cmd_result("cluster_run_structural", result)
}

#[derive(Debug, Deserialize)]
struct ClusterPersistArgs {
    name: String,
    /// "one-shot" | "saved-triage" — lifecycle hint, mirrors
    /// `cluster_tree_create`.
    #[serde(default = "default_source_oneshot")]
    source: String,
    scope_json: String,
    method_json: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    /// Map of build-pass cluster id → user-supplied name. User-renamed
    /// nodes land with `user_edited_name = 1` and skip the LLM naming
    /// pass.
    #[serde(default)]
    user_renamed: std::collections::HashMap<String, String>,
    /// When false, the persist call skips the `RaptorSummarize` task
    /// submission step — the tree lands with placeholder names
    /// (`"Cluster N"`) intact, and the user can run "Regenerate names"
    /// later from the cluster pane to fill in LLM-generated names.
    /// Default true preserves the original "Confirm and name" behavior
    /// for callers that don't set the flag.
    #[serde(default = "default_true")]
    submit_naming: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ClusterPersistResult {
    tree_id: String,
    /// Submitted task ids (one `RaptorSummarize` per un-renamed cluster).
    task_ids: Vec<String>,
}

// status: cluster-review-tab-confirm-and-name
#[tauri::command]
async fn cluster_persist_built_tree(
    state: State<'_, AppState>,
    args: ClusterPersistArgs,
) -> Result<ClusterPersistResult, String> {
    let result = async {
        let (trees, queue) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.trees.clone(), session.tasks.clone())
        };
        // Insert tree + nodes.
        let tree_id = trees
            .insert_tree(hiker_core::trees::TreeInsert {
                id: None,
                name: args.name,
                source: args.source,
                state: "draft".to_string(),
                scope_json: args.scope_json,
                method_json: args.method_json,
                vault_snapshot: None,
            })
            .map_err(|e| format!("insert_tree: {e}"))?;
        let mut inserts = hiker_core::cluster::result_to_node_inserts_pub(&args.tree);
        // Apply user-renamed names + the `user_edited_name = 1` flag.
        for ins in &mut inserts {
            if let Some(new_name) = args.user_renamed.get(&ins.node_id) {
                ins.name = new_name.clone();
                ins.user_edited_name = true;
            }
        }
        trees
            .insert_nodes(&tree_id, &inserts)
            .map_err(|e| format!("insert_nodes: {e}"))?;
        // Submit one RaptorSummarize task per cluster node whose name
        // is not user-edited. Mirrors `cluster_regenerate_names`.
        // status: cluster-review-tab-confirm-skip-naming
        // When the caller asks to skip the naming pass (the "Confirm
        // (no naming)" button), bypass the queue submission entirely
        // and return an empty `task_ids` list. The tree persists with
        // placeholder names; the user can run "Regenerate names" from
        // the cluster pane to fill them in later.
        let mut task_ids: Vec<String> = Vec::new();
        if !args.submit_naming {
            return Ok::<_, String>(ClusterPersistResult { tree_id, task_ids });
        }
        let nodes = trees.list_nodes(&tree_id).map_err(|e| e.to_string())?;
        for n in nodes {
            if !matches!(n.kind, hiker_core::trees::NodeKind::Cluster) {
                continue;
            }
            if n.user_edited_name {
                continue;
            }
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                    tree_id: tree_id.clone(),
                    cluster_node_id: n.id.clone(),
                    level: 0,
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": tree_id,
                    "cluster_node_id": n.id,
                }),
            };
            let handle = queue.submit(task).await;
            task_ids.push(handle.id.clone());
        }
        Ok::<_, String>(ClusterPersistResult { tree_id, task_ids })
    }
    .await;
    log_cmd_result("cluster_persist_built_tree", result)
}

#[derive(Debug, Deserialize)]
struct ClusterReclusterFromBuiltArgs {
    tree_id: String,
    node_id: String,
    tree: hiker_core::cluster::BuiltClusterTree,
    #[serde(default)]
    carry_policies_down: bool,
    /// Map of build-pass cluster id → user-supplied name (rename-before-Confirm).
    #[serde(default)]
    user_renamed: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ClusterReclusterFromBuiltResult {
    new_cluster_ids: Vec<String>,
    task_ids: Vec<String>,
}

// status: cluster-review-tab-confirm-and-name (recluster branch)
//
// Replace the selected subtree with the pre-built structural tree. The
// clustering already ran; this command only persists. Mirrors
// `recluster_subtree_in_worker`'s replace-subtree shape, minus the LLM
// summarizer call. Submits one `RaptorSummarize` task per non-user-renamed
// new cluster.
#[tauri::command]
async fn cluster_op_recluster_subtree_from_built(
    state: State<'_, AppState>,
    args: ClusterReclusterFromBuiltArgs,
) -> Result<ClusterReclusterFromBuiltResult, String> {
    let result = async {
        let (trees, queue) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.trees.clone(), session.tasks.clone())
        };

        // Walk the existing subtree under `node_id` to snapshot the
        // prior state for undo + collect leaves (so we know which leaves
        // need re-parenting onto the new structure).
        let all = trees.list_nodes(&args.tree_id).map_err(|e| e.to_string())?;
        let mut children_by_parent: std::collections::HashMap<
            String,
            Vec<hiker_core::trees::EditableNode>,
        > = std::collections::HashMap::new();
        for n in all.iter().cloned() {
            if let Some(p) = n.parent.clone() {
                children_by_parent.entry(p).or_default().push(n);
            }
        }
        let mut by_id: std::collections::HashMap<String, hiker_core::trees::EditableNode> =
            std::collections::HashMap::new();
        for n in all.iter().cloned() {
            by_id.insert(n.id.clone(), n);
        }
        let root_node = by_id
            .get(&args.node_id)
            .cloned()
            .ok_or_else(|| format!("node not found: {}", args.node_id))?;
        if !matches!(root_node.kind, hiker_core::trees::NodeKind::Cluster) {
            return Err("recluster only works on cluster nodes".into());
        }

        let mut descendant_clusters: Vec<hiker_core::trees::EditableNode> = Vec::new();
        let mut descendant_leaves: Vec<hiker_core::trees::EditableNode> = Vec::new();
        let mut stack = vec![args.node_id.clone()];
        while let Some(id) = stack.pop() {
            if let Some(kids) = children_by_parent.get(&id) {
                for k in kids {
                    match k.kind {
                        hiker_core::trees::NodeKind::Leaf => descendant_leaves.push(k.clone()),
                        _ => {
                            descendant_clusters.push(k.clone());
                            stack.push(k.id.clone());
                        }
                    }
                }
            }
        }

        let resolved_policy: Option<hiker_core::trees::NodePolicy> = {
            let mut cursor: Option<String> = Some(args.node_id.clone());
            let mut found = None;
            while let Some(id) = cursor {
                if let Some(n) = by_id.get(&id) {
                    if let Some(p) = &n.policy {
                        found = Some(p.clone());
                        break;
                    }
                    cursor = n.parent.clone();
                } else {
                    break;
                }
            }
            found
        };

        let prior_subtree: Vec<serde_json::Value> = descendant_clusters
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "parent_id": c.parent,
                    "kind": match c.kind {
                        hiker_core::trees::NodeKind::Cluster => "cluster",
                        hiker_core::trees::NodeKind::OutlierBucket => "outlier-bucket",
                        hiker_core::trees::NodeKind::Leaf => "leaf",
                    },
                    "note_id": c.note_ref,
                    "name": c.name,
                    "summary": c.summary,
                    "user_edited_name": c.user_edited_name,
                    "user_edited_summary": c.user_edited_summary,
                    "policy": c.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                    "confidence": c.confidence,
                    "summary_membership_churn": c.summary_membership_churn,
                })
            })
            .collect();
        let prior_leaf_parents: Vec<(String, Option<String>)> = descendant_leaves
            .iter()
            .map(|l| (l.id.clone(), l.parent.clone()))
            .collect();

        // Plan the new node inserts, mirroring the namespaced-id pattern
        // from `recluster_subtree_in_worker` so collisions with existing
        // ids in `trees.db` are impossible.
        let ns = format!("recluster-{}", args.node_id);
        let rename_id = |id: &str| -> String { format!("{}-{}", ns, id) };

        let levels = &args.tree.levels;
        let mut new_nodes_snapshot: Vec<serde_json::Value> = Vec::new();
        let mut new_cluster_ids: Vec<String> = Vec::new();

        let mut parent_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for level in levels.iter().skip(1) {
            for node in level {
                for child in &node.members {
                    parent_of.insert(child.clone(), node.id.clone());
                }
            }
        }
        let top_level_idx = if levels.is_empty() { 0 } else { levels.len() - 1 };
        let top = if levels.is_empty() { &[][..] } else { &levels[top_level_idx][..] };
        let mut absorbed_top_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if top.len() == 1 {
            absorbed_top_ids.insert(top[0].id.clone());
        }

        for (level_idx, level) in levels.iter().enumerate().rev() {
            for node in level {
                if absorbed_top_ids.contains(&node.id) {
                    continue;
                }
                let new_id = rename_id(&node.id);
                let parent_id = match parent_of.get(&node.id) {
                    Some(p) if !absorbed_top_ids.contains(p) => rename_id(p),
                    _ => args.node_id.clone(),
                };
                let policy = if args.carry_policies_down && parent_id == args.node_id {
                    resolved_policy.clone()
                } else {
                    None
                };
                let user_renamed_name = args.user_renamed.get(&node.id).cloned();
                let final_name = user_renamed_name.clone().unwrap_or_else(|| node.name.clone());
                new_nodes_snapshot.push(serde_json::json!({
                    "id": new_id,
                    "parent_id": parent_id,
                    "kind": "cluster",
                    "note_id": null,
                    "name": final_name,
                    "summary": node.summary,
                    "user_edited_name": user_renamed_name.is_some(),
                    "user_edited_summary": false,
                    "policy": policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
                    "confidence": node.confidence,
                    "summary_membership_churn": 0,
                    "level": level_idx,
                }));
                new_cluster_ids.push(new_id);
            }
        }

        let mut leaf_target: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Some(leaf_level) = levels.first() {
            for cluster in leaf_level {
                let parent_for_leaf = if absorbed_top_ids.contains(&cluster.id) {
                    args.node_id.clone()
                } else {
                    rename_id(&cluster.id)
                };
                for note_id_ in &cluster.members {
                    leaf_target.insert(note_id_.clone(), parent_for_leaf.clone());
                }
            }
        }
        for note_id_ in &args.tree.outliers {
            leaf_target
                .entry(note_id_.clone())
                .or_insert_with(|| args.node_id.clone());
        }

        let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
        for l in &descendant_leaves {
            let target = l
                .note_ref
                .as_ref()
                .and_then(|nid| leaf_target.get(nid).cloned())
                .unwrap_or_else(|| args.node_id.clone());
            leaf_moves.push((l.id.clone(), Some(target)));
        }

        let preserved_chain: Vec<(String, u32)> = {
            let mut chain: Vec<(String, u32)> = Vec::new();
            let mut cursor: Option<String> = Some(args.node_id.clone());
            while let Some(id) = cursor {
                if let Some(n) = by_id.get(&id) {
                    chain.push((n.id.clone(), n.summary_membership_churn));
                    cursor = n.parent.clone();
                } else {
                    break;
                }
            }
            chain
        };

        // Mutate trees.db in the same shape as `recluster_subtree_in_worker`.
        for c in &descendant_clusters {
            trees.delete_node(&args.tree_id, &c.id).map_err(|e| e.to_string())?;
        }
        for snap in &new_nodes_snapshot {
            let id = snap.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let parent_id = snap
                .get("parent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let policy: Option<hiker_core::trees::NodePolicy> =
                snap.get("policy").and_then(|v| match v {
                    serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
                    _ => None,
                });
            let user_edited_name = snap
                .get("user_edited_name")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            trees
                .insert_single_node(
                    &args.tree_id,
                    hiker_core::trees::NodeInsert {
                        node_id: id,
                        parent_id,
                        kind: hiker_core::trees::NodeKind::Cluster,
                        note_id: None,
                        name: snap
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        summary: snap
                            .get("summary")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        user_edited_name,
                        user_edited_summary: false,
                        policy,
                        centroid: None,
                        confidence: snap
                            .get("confidence")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0) as f32,
                        summary_membership_churn: 0,
                    },
                )
                .map_err(|e| e.to_string())?;
        }
        trees
            .reparent_many(&args.tree_id, &leaf_moves)
            .map_err(|e| e.to_string())?;

        for (id, prior) in &preserved_chain {
            let _ = trees.set_churn(&args.tree_id, id, *prior);
        }
        for id in &new_cluster_ids {
            let _ = trees.reset_churn(&args.tree_id, id);
        }

        trees
            .record_recluster_subtree(
                &args.tree_id,
                &args.node_id,
                &prior_subtree,
                &prior_leaf_parents,
                &new_nodes_snapshot,
                &leaf_moves,
                if args.carry_policies_down {
                    resolved_policy.as_ref()
                } else {
                    None
                },
            )
            .map_err(|e| e.to_string())?;

        // Submit RaptorSummarize tasks for the new clusters that aren't
        // user-renamed.
        let mut task_ids: Vec<String> = Vec::new();
        let user_renamed_new_ids: std::collections::HashSet<String> = args
            .user_renamed
            .keys()
            .map(|k| format!("{}-{}", ns, k))
            .collect();
        for id in &new_cluster_ids {
            if user_renamed_new_ids.contains(id) {
                continue;
            }
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                    tree_id: args.tree_id.clone(),
                    cluster_node_id: id.clone(),
                    level: 0,
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": args.tree_id,
                    "cluster_node_id": id,
                }),
            };
            let handle = queue.submit(task).await;
            task_ids.push(handle.id.clone());
        }

        Ok::<_, String>(ClusterReclusterFromBuiltResult {
            new_cluster_ids,
            task_ids,
        })
    }
    .await;
    log_cmd_result("cluster_op_recluster_subtree_from_built", result)
}

// status: cluster-editor-set-policy
#[tauri::command]
fn cluster_node_set_policy(
    state: State<'_, AppState>,
    tree_id: String,
    node_id: String,
    policy_json: Option<String>,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let policy: Option<hiker_core::trees::NodePolicy> = match policy_json {
            Some(s) if !s.is_empty() => Some(serde_json::from_str(&s).map_err(|e| e.to_string())?),
            _ => None,
        };
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_policy(&tree_id, &node_id, policy)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_node_set_policy", result)
}

// status: cluster-editor-apply-action
// status: suggestions-apply-cmd
//
// Walk the tree, resolve each leaf's effective policy via the walk-up
// rule, and emit one `staging.db` row per `Tag` / `Move` leaf. Returns
// the produced staging row ids + per-bucket counts (skipped /
// unpolicied / frozen) for the batch-review pane header.
#[tauri::command]
fn cluster_apply(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<hiker_core::suggest::ApplyOutcome, String> {
    let result = (|| -> Result<hiker_core::suggest::ApplyOutcome, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        hiker_core::suggest::apply_tree(
            &session.trees,
            &tree_id,
            &session.vault,
            &store,
            &session.staging,
            Some(&history),
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_apply", result)
}

// status: cluster-editor-multi-select-stage-move
#[tauri::command]
fn cluster_stage_moves(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    target_folder: String,
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        hiker_core::suggest::stage_moves(
            &session.trees,
            hiker_core::suggest::StageMoveArgs {
                tree_id: &tree_id,
                node_ids: &node_ids,
                target_folder: &target_folder,
            },
            &store,
            &session.staging,
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_stage_moves", result)
}

// status: cluster-editor-multi-select-stage-tag
#[tauri::command]
fn cluster_stage_tags(
    state: State<'_, AppState>,
    tree_id: String,
    node_ids: Vec<String>,
    tag_slug: String,
) -> Result<Vec<String>, String> {
    let result = (|| -> Result<Vec<String>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        hiker_core::suggest::stage_tags(
            &session.trees,
            hiker_core::suggest::StageTagArgs {
                tree_id: &tree_id,
                node_ids: &node_ids,
                tag_slug: &tag_slug,
            },
            &session.vault,
            &store,
            &session.staging,
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_stage_tags", result)
}

// status: cluster-editor-sapling-evergreen-lifecycle, cluster-editor-apply-action
// Free-string setter for the tree's lifecycle state. Sprint C uses
// `"draft"` / `"applied"`; Sprint D adds `"saved-as-triage"`.
#[tauri::command]
fn cluster_tree_set_state(
    state: State<'_, AppState>,
    tree_id: String,
    new_state: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        session
            .trees
            .set_tree_state(&tree_id, &new_state)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_tree_set_state", result)
}

// status: cluster-editor-triage-on-save
// status: cluster-editor-triage-via-staging
// status: triage-classifier-engine
// status: triage-staging-proposals
//
// Synchronous triage trigger. Resolves the note's embedding from the
// store, walks every `saved-as-triage` tree, and emits one staging row
// per matched policy. Returns the per-tree outcomes so the caller can
// log + toast appropriately. The async path (RaptorTriageMatch task)
// is wrapper'd by `cluster_triage_enqueue` below; both share this
// classifier.
#[tauri::command]
fn cluster_triage_run(
    state: State<'_, AppState>,
    rel: String,
    author_class: Option<String>,
) -> Result<Vec<hiker_core::suggest::TriageOutcome>, String> {
    let result = (|| -> Result<Vec<hiker_core::suggest::TriageOutcome>, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let note_id = store
            .id_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("note not indexed: {rel}"))?;
        let embedding = store
            .note_embedding_for_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no embedding for {rel}"))?;
        let cfg_triage = session.config.read().expect("config lock poisoned")
            .suggestions
            .triage
            .clone();
        let opts = hiker_core::suggest::TriageOpts {
            review_required: cfg_triage.review_required,
            scope: cfg_triage.scope.clone(),
            beam_width: 2,
        };
        let ac = match author_class.as_deref() {
            Some("agent") => hiker_core::suggest::NoteAuthorClass::Agent,
            _ => hiker_core::suggest::NoteAuthorClass::User,
        };
        hiker_core::suggest::triage_all_saved_trees(
            &session.trees,
            &session.vault,
            &store,
            &session.staging,
            &note_id,
            &rel,
            &embedding,
            ac,
            &opts,
        )
        .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_triage_run", result)
}

// status: cluster-editor-triage-via-task-queue
//
// Async triage trigger: enqueues one `RaptorTriageMatch` task per
// saved-as-triage tree. The worker pool drains the queue and emits
// staging rows via the same classifier as `cluster_triage_run`. Returns
// the queued task ids so the caller can correlate them with queue
// events.
#[tauri::command]
async fn cluster_triage_enqueue(
    state: State<'_, AppState>,
    rel: String,
) -> Result<Vec<String>, String> {
    let result = async {
        let (queue, trees) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.tasks.clone(), session.trees.clone())
        };
        let tree_rows = trees.list_trees().map_err(|e| e.to_string())?;
        let saved: Vec<String> = tree_rows
            .into_iter()
            .filter(|t| t.state == "saved-as-triage")
            .map(|t| t.id)
            .collect();
        let mut task_ids: Vec<String> = Vec::with_capacity(saved.len());
        for tree_id in saved {
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorTriageMatch {
                    tree_id: tree_id.clone(),
                    source_path: rel.clone(),
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": tree_id,
                    "source_path": rel,
                }),
            };
            let handle = queue.submit(task).await;
            task_ids.push(handle.id.clone());
        }
        Ok::<_, String>(task_ids)
    }
    .await;
    log_cmd_result("cluster_triage_enqueue", result)
}

// status: cluster-editor-regenerate-via-task-queue
// status: cluster-editor-llm-actions-via-task-queue
//
// Enqueue one `RaptorSummarize` task per non-user-edited cluster node
// in the tree. Caller-facing affordance is "Regenerate names" on the
// expanded pane's toolbar — the worker writes the new name/summary back
// through `trees.rename` / `trees.set_summary` and resets the node's
// `summary_membership_churn`.
#[tauri::command]
async fn cluster_regenerate_names(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<Vec<String>, String> {
    let result = async {
        let (queue, trees) = {
            let guard = state.session.lock().map_err(|e| e.to_string())?;
            let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
            (session.tasks.clone(), session.trees.clone())
        };
        let nodes = trees.list_nodes(&tree_id).map_err(|e| e.to_string())?;
        let mut ids: Vec<String> = Vec::new();
        for n in nodes {
            if !matches!(n.kind, hiker_core::trees::NodeKind::Cluster) {
                continue;
            }
            // Skip user-edited names — explicit "Regenerate this node"
            // from the row menu is the only way to clobber a user edit
            // (per cluster-editor-user-edit-provenance).
            if n.user_edited_name && n.user_edited_summary {
                continue;
            }
            let task = hiker_core::tasks::Task {
                id: String::new(),
                kind: hiker_core::tasks::TaskKind::RaptorSummarize {
                    tree_id: tree_id.clone(),
                    cluster_node_id: n.id.clone(),
                    level: 0,
                },
                priority: hiker_core::tasks::Priority::Normal,
                shape: hiker_core::tasks::TaskShape::Direct,
                payload: hiker_core::tasks::TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": tree_id,
                    "cluster_node_id": n.id,
                }),
            };
            let handle = queue.submit(task).await;
            ids.push(handle.id.clone());
        }
        Ok::<_, String>(ids)
    }
    .await;
    log_cmd_result("cluster_regenerate_names", result)
}

// status: cluster-build-from-folders-live-update
//
// On a vault rename, walk every saved-as-triage `from-folders` tree and
// re-parent the affected leaf. Wired below to the watcher's rename
// event stream alongside the indexer subscription.
#[tauri::command]
fn cluster_folder_rename_update(
    state: State<'_, AppState>,
    rel_from: String,
    rel_to: String,
) -> Result<u32, String> {
    let result = (|| -> Result<u32, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let store = session.read_store.lock().map_err(|e| e.to_string())?;
        let note_id = match store
            .id_for_path(&rel_to)
            .map_err(|e| e.to_string())?
        {
            Some(id) => id,
            None => return Ok(0),
        };
        drop(store);
        let new_folder = rel_to
            .rsplit_once('/')
            .map(|(a, _)| a.to_string())
            .unwrap_or_default();
        let trees_rows = session.trees.list_trees().map_err(|e| e.to_string())?;
        let mut n = 0u32;
        for t in trees_rows {
            if t.state != "saved-as-triage" {
                continue;
            }
            // Cheap filter: only `from-folders` method trees track the
            // filesystem. Detect via the method JSON's `kind`.
            let is_folders = serde_json::from_str::<serde_json::Value>(&t.method_json)
                .ok()
                .and_then(|v| {
                    v.get("kind")
                        .and_then(|k| k.as_str())
                        .map(|s| s == "from-folders")
                })
                .unwrap_or(false);
            if !is_folders {
                continue;
            }
            let updated = session
                .trees
                .update_for_folder_rename(&t.id, &note_id, &new_folder)
                .map_err(|e| e.to_string())?;
            if updated {
                n += 1;
            }
        }
        let _ = rel_from; // currently unused; kept on the signature so the caller
                          // doesn't have to fish the prior path out of the watcher
                          // event twice.
        Ok(n)
    })();
    log_cmd_result("cluster_folder_rename_update", result)
}

// status: suggestions-rejection-history
// Records a rejected cluster-editor row in
// `.hiker/suggestion-history.json`. Called by the batch-review pane
// alongside `staging_reject` for any row whose metadata carries a
// `tree_member_fingerprint`.
#[tauri::command]
fn cluster_record_rejection(
    state: State<'_, AppState>,
    fingerprint: String,
    note_path: String,
    action: String,
) -> Result<(), String> {
    let result = (|| -> Result<(), String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let history = hiker_core::suggest::RejectionHistory::open(&session.root)
            .map_err(|e| e.to_string())?;
        history
            .record_rejection(&fingerprint, &note_path, &action)
            .map_err(|e| e.to_string())
    })();
    log_cmd_result("cluster_record_rejection", result)
}

// status: cluster-editor-undo-redo
//
// Undo pops the most-recent history row and inverts its effect using
// the embedded `undo_args` JSON. Redo is a simple "redo stack" kept
// inside this command-level state — when the user undoes, the popped
// entry sits on the redo stack until the next forward edit clears it.
// Because the redo stack must persist across IPC calls but not across
// vault swaps, we store it on a fresh `AppState`-bound Mutex.
//
// We keep undo/redo per-tree-id; vault swap drops the AppState mutex
// contents (no `Drop` impl needed — Tauri rebuilds on `manage`).

// (The redo stack itself lives inline below — `lazy_static!` is overkill
// for a single Mutex.)

use std::sync::OnceLock;
static CLUSTER_REDO_STACKS: OnceLock<Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>>> = OnceLock::new();

fn redo_stacks() -> &'static Mutex<std::collections::HashMap<String, Vec<hiker_core::trees::HistoryEntry>>> {
    CLUSTER_REDO_STACKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn invert_history(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    entry: &hiker_core::trees::HistoryEntry,
) -> Result<(), String> {
    let undo: serde_json::Value =
        serde_json::from_str(&entry.undo_args_json).map_err(|e| e.to_string())?;
    match entry.op.as_str() {
        "rename" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let name = undo.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // Direct DB poke — we need to preserve the prior
            // user_edited_name flag too; rename() always stamps it true.
            let user_edited = undo
                .get("user_edited_name")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            trees
                .rename(tree_id, node_id, name)
                .map_err(|e| e.to_string())?;
            // Hop one more time to flip the flag back if needed.
            if !user_edited {
                // Pop the entry we just appended so undo stays
                // idempotent — we don't want the inverse to leak
                // forward history.
                let _ = trees.pop_last_history(tree_id);
            } else {
                let _ = trees.pop_last_history(tree_id);
            }
            Ok(())
        }
        "edit-summary" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let summary = undo.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            trees
                .set_summary(tree_id, node_id, summary)
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        "set-policy" => {
            let node_id = undo.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
            let policy = undo.get("policy");
            let policy_val: Option<hiker_core::trees::NodePolicy> = match policy {
                Some(serde_json::Value::Null) | None => None,
                Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| e.to_string())?),
            };
            trees
                .set_policy(tree_id, node_id, policy_val)
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        "move" | "promote-outlier" => {
            // Stored `node_id` (or `leaf_id`) + prior `parent_id`.
            let node_id = undo
                .get("node_id")
                .or_else(|| undo.get("leaf_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let parent = match undo.get("parent_id") {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            trees
                .move_node(tree_id, node_id, parent.as_deref())
                .map_err(|e| e.to_string())?;
            let _ = trees.pop_last_history(tree_id);
            Ok(())
        }
        // Reshape ops below do bulk DB work; we apply the recorded
        // inverse directly without re-routing through the high-level
        // methods (which would mutate history).
        "merge-siblings" => undo_merge_siblings(trees, tree_id, &undo),
        "merge-children-up" => undo_merge_children_up(trees, tree_id, &undo),
        "drop-cluster" => undo_drop_cluster(trees, tree_id, &undo),
        "split-cluster" => undo_split(trees, tree_id, &undo),
        "recluster-subtree" => undo_recluster_subtree(trees, tree_id, &undo),
        other => Err(format!("cannot undo op {other}")),
    }
}

fn parse_kind(s: &str) -> hiker_core::trees::NodeKind {
    match s {
        "leaf" => hiker_core::trees::NodeKind::Leaf,
        "outlier-bucket" => hiker_core::trees::NodeKind::OutlierBucket,
        _ => hiker_core::trees::NodeKind::Cluster,
    }
}

fn restore_node_row(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    id: &str,
    row: &serde_json::Value,
) -> Result<(), String> {
    let parent = row
        .get("parent_id")
        .and_then(|v| match v {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
    let kind = parse_kind(row.get("kind").and_then(|v| v.as_str()).unwrap_or("cluster"));
    let note_id = row
        .get("note_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let summary = row.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let policy: Option<hiker_core::trees::NodePolicy> = row
        .get("policy")
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => serde_json::from_str(s).ok(),
            _ => None,
        });
    trees
        .insert_single_node(
            tree_id,
            hiker_core::trees::NodeInsert {
                node_id: id.to_string(),
                parent_id: parent,
                kind,
                note_id,
                name,
                summary,
                user_edited_name: row
                    .get("user_edited_name")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                user_edited_summary: row
                    .get("user_edited_summary")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                policy,
                centroid: None,
                confidence: row
                    .get("confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0) as f32,
                summary_membership_churn: row
                    .get("summary_membership_churn")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
                    .max(0) as u32,
            },
        )
        .map_err(|e| e.to_string())
}

fn undo_merge_siblings(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let child_moves = undo
        .get("child_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for abs in absorbed {
        let id = abs.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let row = abs.get("row").cloned().unwrap_or(serde_json::Value::Null);
        restore_node_row(trees, tree_id, id, &row)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in child_moves {
        let cid = mv.get("child_id").and_then(|v| v.as_str()).unwrap_or("");
        let from = mv.get("from").and_then(|v| v.as_str()).unwrap_or("");
        moves.push((cid.to_string(), Some(from.to_string())));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_merge_children_up(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let grand = undo
        .get("grandchild_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for abs in absorbed {
        let id = abs.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let row = abs.get("row").cloned().unwrap_or(serde_json::Value::Null);
        restore_node_row(trees, tree_id, id, &row)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in grand {
        let cid = mv.get("child_id").and_then(|v| v.as_str()).unwrap_or("");
        let from = mv.get("from").and_then(|v| v.as_str()).unwrap_or("");
        moves.push((cid.to_string(), Some(from.to_string())));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_drop_cluster(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    let absorbed = undo
        .get("absorbed_clusters")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let leaf_moves = undo
        .get("leaf_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Re-insert each cluster row with its prior parent. We re-insert in
    // the recorded order — children before parents would fail FK
    // expectations, but cluster_nodes doesn't have an FK on parent_id,
    // so order is loose.
    for c in absorbed {
        let id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        restore_node_row(trees, tree_id, &id, &c)?;
    }
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in leaf_moves {
        let leaf = mv.get("leaf_id").and_then(|v| v.as_str()).unwrap_or("");
        let pp = match mv.get("prior_parent") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        moves.push((leaf.to_string(), pp));
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())
}

fn undo_split(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    // Re-parent the leaves back to their original parent (which equals
    // `parent_id` here — split moved them onto new sub-clusters under
    // `parent_id`).
    let parent_id = undo.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
    let leaf_moves = undo
        .get("leaf_moves")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in leaf_moves {
        // `leaf_moves` is `[(leaf_id, new_parent)]`; the inverse parks
        // the leaf back under the parent it was split out of.
        if let Some(arr) = mv.as_array() {
            if let (Some(leaf), Some(_)) = (arr.first(), arr.get(1)) {
                if let Some(s) = leaf.as_str() {
                    moves.push((s.to_string(), Some(parent_id.to_string())));
                }
            }
        }
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())?;
    // Delete the synthesized sub-clusters.
    let new_clusters = undo
        .get("new_cluster_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for nc in new_clusters {
        if let Some(s) = nc.as_str() {
            trees
                .delete_node(tree_id, s)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// status: cluster-editor-recluster-subtree
// status: cluster-editor-recluster-subtree-policy-loss
//
// Inverse of `cluster_op_recluster_subtree`. Delete every newly-inserted
// cluster row, re-insert the snapshotted prior-subtree clusters in their
// original positions (restoring policies + names + user-edit flags), and
// re-parent every leaf back to its prior parent. The selected node's own
// row was preserved through the forward op, so it needs no restoration.
fn undo_recluster_subtree(
    trees: &hiker_core::trees::Trees,
    tree_id: &str,
    undo: &serde_json::Value,
) -> Result<(), String> {
    // Delete the new cluster rows the forward op inserted. We delete
    // before re-inserting the prior subtree so a transient overlap on
    // (tree_id, node_id) primary keys can't happen — even though the
    // namespaced ids guarantee no collision in practice.
    let new_ids = undo
        .get("new_node_ids")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for id in &new_ids {
        if let Some(s) = id.as_str() {
            trees
                .delete_node(tree_id, s)
                .map_err(|e| e.to_string())?;
        }
    }
    // Re-insert each prior cluster row.
    let prior = undo
        .get("prior_subtree")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for row in &prior {
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        restore_node_row(trees, tree_id, &id, row)?;
    }
    // Re-parent every leaf back to its prior parent.
    let prior_leaves = undo
        .get("prior_leaf_parents")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut moves: Vec<(String, Option<String>)> = Vec::new();
    for mv in &prior_leaves {
        if let Some(arr) = mv.as_array() {
            if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                let leaf_s = leaf.as_str().unwrap_or("").to_string();
                let parent_s = match parent {
                    serde_json::Value::String(s) => Some(s.clone()),
                    _ => None,
                };
                moves.push((leaf_s, parent_s));
            }
        }
    }
    trees
        .reparent_many(tree_id, &moves)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn cluster_tree_undo(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<bool, String> {
    let result = (|| -> Result<bool, String> {
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let trees = session.trees.clone();
        drop(guard);
        let Some(entry) = trees.pop_last_history(&tree_id).map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        invert_history(&trees, &tree_id, &entry)?;
        let mut stacks = redo_stacks().lock().map_err(|e| e.to_string())?;
        stacks.entry(tree_id).or_default().push(entry);
        Ok(true)
    })();
    log_cmd_result("cluster_tree_undo", result)
}

#[tauri::command]
fn cluster_tree_redo(
    state: State<'_, AppState>,
    tree_id: String,
) -> Result<bool, String> {
    let result = (|| -> Result<bool, String> {
        // Pop from the redo stack and re-apply the forward args.
        let popped = {
            let mut stacks = redo_stacks().lock().map_err(|e| e.to_string())?;
            stacks.entry(tree_id.clone()).or_default().pop()
        };
        let Some(entry) = popped else { return Ok(false) };
        let guard = state.session.lock().map_err(|e| e.to_string())?;
        let session = guard.as_ref().ok_or_else(|| "no vault open".to_string())?;
        let trees = session.trees.clone();
        drop(guard);
        let args: serde_json::Value =
            serde_json::from_str(&entry.args_json).map_err(|e| e.to_string())?;
        // Re-apply by the same op-keyed dispatch as forward edits.
        // Simpler ops route through the existing methods (which write a
        // fresh history row); reshape ops re-build using the recorded
        // args.
        match entry.op.as_str() {
            "rename" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                trees.rename(&tree_id, node_id, name).map_err(|e| e.to_string())?;
            }
            "edit-summary" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                trees.set_summary(&tree_id, node_id, summary).map_err(|e| e.to_string())?;
            }
            "set-policy" => {
                let node_id = args.get("node_id").and_then(|v| v.as_str()).unwrap_or("");
                let policy = args.get("policy");
                let policy_val: Option<hiker_core::trees::NodePolicy> = match policy {
                    Some(serde_json::Value::Null) | None => None,
                    Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| e.to_string())?),
                };
                trees.set_policy(&tree_id, node_id, policy_val).map_err(|e| e.to_string())?;
            }
            "move" | "promote-outlier" => {
                let node_id = args
                    .get("node_id")
                    .or_else(|| args.get("leaf_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let parent = match args.get("parent_id") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    _ => None,
                };
                trees.move_node(&tree_id, node_id, parent.as_deref()).map_err(|e| e.to_string())?;
            }
            "merge-siblings" => {
                // Re-run the forward op against the recorded
                // [survivor, ...absorbed] ids. Undo restored the
                // absorbed nodes so the IDs are valid again.
                let survivor = args
                    .get("survivor")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "merge-siblings redo: missing survivor".to_string())?;
                let absorbed = args
                    .get("absorbed")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut node_ids: Vec<String> = vec![survivor.to_string()];
                for a in absorbed {
                    if let Some(s) = a.as_str() {
                        node_ids.push(s.to_string());
                    }
                }
                trees
                    .merge_siblings(&tree_id, &node_ids)
                    .map_err(|e| e.to_string())?;
            }
            "merge-children-up" => {
                let parent_id = args
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "merge-children-up redo: missing parent_id".to_string())?;
                trees
                    .merge_children_up(&tree_id, parent_id)
                    .map_err(|e| e.to_string())?;
            }
            "drop-cluster" => {
                let node_id = args
                    .get("node_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "drop-cluster redo: missing node_id".to_string())?;
                let bucket = args
                    .get("outlier_bucket_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "drop-cluster redo: missing outlier_bucket_id".to_string())?;
                trees
                    .drop_cluster(&tree_id, node_id, bucket)
                    .map_err(|e| e.to_string())?;
            }
            "split-cluster" => {
                // HDBSCAN is non-deterministic, so we don't re-cluster
                // on redo — we replay the snapshotted result. The
                // forward op recorded each new cluster's full row
                // shape + the leaf moves, so we just re-insert and
                // re-parent. Then `record_split` lays down a fresh
                // history row so a subsequent undo round-trips.
                let parent_id = args
                    .get("parent_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "split-cluster redo: missing parent_id".to_string())?;
                let new_clusters = args
                    .get("new_clusters")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if new_clusters.is_empty() {
                    return Err(
                        "split-cluster redo: legacy history row lacks new_clusters snapshot"
                            .into(),
                    );
                }
                for c in &new_clusters {
                    let id = c
                        .get("node_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    restore_node_row(&trees, &tree_id, &id, c)?;
                }
                let leaf_moves_json = args
                    .get("leaf_moves")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &leaf_moves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            leaf_moves.push((leaf_s, parent_s));
                        }
                    }
                }
                trees
                    .reparent_many(&tree_id, &leaf_moves)
                    .map_err(|e| e.to_string())?;
                trees
                    .record_split(&tree_id, parent_id, &new_clusters, &leaf_moves)
                    .map_err(|e| e.to_string())?;
            }
            "recluster-subtree" => {
                // HDBSCAN is non-deterministic and the build pipeline
                // is recursive on top — re-running won't reproduce the
                // same subtree. So redo replays from the snapshot: the
                // forward op recorded every new cluster row and the
                // (leaf_id, new_parent) moves; we re-insert and
                // re-parent, then lay down a fresh history row so a
                // subsequent undo round-trips. The descendants the
                // forward op deleted were *restored* by undo, so we
                // delete them again here before re-inserting.
                let root_id = args
                    .get("root_id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "recluster-subtree redo: missing root_id".to_string())?;
                let undo_args: serde_json::Value = serde_json::from_str(&entry.undo_args_json)
                    .map_err(|e| e.to_string())?;
                // Walk the tree to find every descendant cluster of
                // root_id and delete it (undo restored them).
                let all = trees
                    .list_nodes(&tree_id)
                    .map_err(|e| e.to_string())?;
                let mut children_by_parent: std::collections::HashMap<
                    String,
                    Vec<hiker_core::trees::EditableNode>,
                > = std::collections::HashMap::new();
                for n in all.iter().cloned() {
                    if let Some(p) = n.parent.clone() {
                        children_by_parent.entry(p).or_default().push(n);
                    }
                }
                let mut to_delete: Vec<String> = Vec::new();
                let mut stack = vec![root_id.to_string()];
                while let Some(id) = stack.pop() {
                    if let Some(kids) = children_by_parent.get(&id) {
                        for k in kids {
                            if !matches!(k.kind, hiker_core::trees::NodeKind::Leaf) {
                                to_delete.push(k.id.clone());
                                stack.push(k.id.clone());
                            }
                        }
                    }
                }
                for id in &to_delete {
                    trees.delete_node(&tree_id, id).map_err(|e| e.to_string())?;
                }
                // Re-insert the new cluster rows from the snapshot
                // (order in args is already top-down — parents first).
                let new_nodes = args
                    .get("new_nodes")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if new_nodes.is_empty() {
                    return Err(
                        "recluster-subtree redo: legacy history row lacks new_nodes snapshot"
                            .into(),
                    );
                }
                for snap in &new_nodes {
                    let id = snap
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    restore_node_row(&trees, &tree_id, &id, snap)?;
                }
                // Re-parent every leaf onto its recorded new home.
                let leaf_moves_json = args
                    .get("leaf_moves")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut leaf_moves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &leaf_moves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            leaf_moves.push((leaf_s, parent_s));
                        }
                    }
                }
                trees
                    .reparent_many(&tree_id, &leaf_moves)
                    .map_err(|e| e.to_string())?;
                // Re-record the history row so undo round-trips.
                let prior_subtree = undo_args
                    .get("prior_subtree")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let prior_leaves_json = undo_args
                    .get("prior_leaf_parents")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut prior_leaves: Vec<(String, Option<String>)> = Vec::new();
                for mv in &prior_leaves_json {
                    if let Some(arr) = mv.as_array() {
                        if let (Some(leaf), Some(parent)) = (arr.first(), arr.get(1)) {
                            let leaf_s = leaf.as_str().unwrap_or("").to_string();
                            let parent_s = match parent {
                                serde_json::Value::String(s) => Some(s.clone()),
                                _ => None,
                            };
                            prior_leaves.push((leaf_s, parent_s));
                        }
                    }
                }
                let carried_policy: Option<hiker_core::trees::NodePolicy> = args
                    .get("carried_policy")
                    .and_then(|v| match v {
                        serde_json::Value::Null => None,
                        other => serde_json::from_value(other.clone()).ok(),
                    });
                trees
                    .record_recluster_subtree(
                        &tree_id,
                        root_id,
                        &prior_subtree,
                        &prior_leaves,
                        &new_nodes,
                        &leaf_moves,
                        carried_policy.as_ref(),
                    )
                    .map_err(|e| e.to_string())?;
            }
            other => {
                return Err(format!("redo unsupported for op {other}"));
            }
        }
        Ok(true)
    })();
    log_cmd_result("cluster_tree_redo", result)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState {
            session: Mutex::new(None),
            config_last_write: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            list_dir,
            read_file,
            read_file_with_hash,
            write_file,
            write_file_checked,
            open_for_edit,
            commit_buffer,
            resolve_drift,
            open_vault_at,
            get_default_vault,
            index,
            index_status,
            index_state_for,
            count_notes_in,
            compute_diff,
            related_notes,
            search_vault,
            chunks_for,
            create_note,
            move_note,
            move_folder,
            reveal_in_file_manager,
            delete_note,
            restore_trash_entry,
            list_trash,
            empty_trash,
            permanent_delete_trash_entry,
            get_settings,
            get_settings_scoped,
            reload_config,
            reveal_config_file,
            set_setting,
            vault_home_stats,
            recent_notes_modified,
            recent_notes_accessed,
            note_accessed,
            note_properties,
            recent_changes,
            changes_count,
            change_content,
            rollback_change,
            restore_snapshot,
            chat::chat_send,
            chat::chat_continue,
            chat::chat_stop,
            chat::chat_cancel,
            chat::chat_session_new,
            chat::chat_session_active,
            chat::chat_session_list,
            chat::chat_session_open,
            chat::chat_session_delete,
            chat_at_autocomplete,
            chat_resolve_at_note,
            tasks_snapshot,
            tasks_cancel,
            task_details,
            submit_note_mutation,
            autosave_write,
            autosave_clear,
            autosave_save_tab_state,
            autosave_load_tab_state,
            autosave_recover,
            autosave_discard,
            log_from_frontend,
            trail_create,
            trail_append_waypoint,
            trail_remove_waypoint,
            trail_descendant_count,
            trail_delete,
            trails_list,
            trail_get,
            trails_containing_note,
            trail_set_active,
            trail_set_append_cursor,
            staging_list,
            staging_count,
            staging_accept,
            staging_reject,
            staging_accept_all,
            staging_content,
            activity_list,
            activity_list_for_path,
            activity_count,
            cluster_trees_list,
            cluster_tree_get,
            cluster_tree_create,
            cluster_tree_rebuild,
            cluster_tree_discard,
            cluster_node_rename,
            cluster_node_set_summary,
            cluster_node_move,
            cluster_node_set_policy,
            cluster_op_merge_siblings,
            cluster_op_merge_children_up,
            cluster_op_drop_cluster,
            cluster_op_promote_outlier,
            cluster_op_split,
            cluster_op_recluster_subtree,
            cluster_run_structural,
            cluster_persist_built_tree,
            cluster_op_recluster_subtree_from_built,
            cluster_tree_undo,
            cluster_tree_redo,
            cluster_apply,
            cluster_stage_moves,
            cluster_stage_tags,
            cluster_tree_set_state,
            cluster_record_rejection,
            cluster_triage_run,
            cluster_triage_enqueue,
            cluster_regenerate_names,
            cluster_folder_rename_update,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    // Bypass tokio runtime drop, which blocks waiting on in-flight
    // `spawn_blocking` embed tasks (fastembed runs can take seconds) and on
    // notify-debouncer's worker thread join. The OS reclaims everything.
    std::process::exit(0);
}

// FileEvent and ProgressEvent need to be Serialize for tauri::emit. Both
// are defined in core; this const block compile-asserts the contract.
const _: fn() = || {
    fn assert_serialize<T: Serialize>() {}
    assert_serialize::<FileEvent>();
    assert_serialize::<ProgressEvent>();
};
