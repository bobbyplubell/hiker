mod chat;
mod cmd_error;
mod cmds;
mod events;

pub(crate) use cmd_error::{CmdError, CmdResult};

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use hiker_core::autosave::Autosave;
use hiker_core::activity::Activity;
use hiker_core::changes::Changes;
use hiker_core::config::Config;
use hiker_core::indexer::{IndexerHandle, ProgressEvent};
use hiker_core::staging::Staging;
use hiker_core::store::Store;
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::Vault;
use serde::Serialize;
use tauri::State;

use cmds::cluster::{recluster_subtree_in_worker, title_from_rel_path};
// Bring every `cluster_*` command into the `lib.rs` scope so the
// `invoke_handler!` registration can reference them by bare name (same
// shape as the unprefixed entries below). The non-command items in
// `cmds::cluster` are `pub(crate)` and named conventionally so wildcard
// import is safe.
use cmds::cluster::*;
// Same trick for the vault file-IO + note/trash commands.
use cmds::vault::*;
// Same trick for the settings commands.
use cmds::settings::*;
// Vault-open bootstrap (the `open_vault_at` command and all the long-lived
// subsystems it spins up — indexer, watcher, MCP, staging recheck, etc.).
use cmds::bootstrap::*;
// Wildcard imports for the remaining command modules so the
// `invoke_handler!` macro can reference each command by bare name.
use cmds::indexer::*;
use cmds::search::*;
use cmds::vault_home::*;
use cmds::mutations::*;
use cmds::mcp::*;

/// All long-lived state for an open vault. Constructed in `open_vault_at`,
/// dropped on swap.
pub(crate) struct VaultSession {
    pub(crate) vault: Vault,
    pub(crate) root: PathBuf,
    pub(crate) indexer: IndexerHandle,
    /// Held to keep the watcher alive; dropping this closes the broadcast.
    /// Also referenced by `create_note` / `move_note` to register self-write
    /// suppression around fs mutations. Wrapped in `Arc` so the mutating
    /// commands (which call `core::ops::*`) can clone a cheap handle out
    /// from under the session lock and pass it across the indexer-reply
    /// `.await` without holding the sync mutex across it.
    pub(crate) watcher: Arc<Watcher>,
    /// status: changes-write-path
    /// Append-only changelog. Shared writer (single mutex inside `Changes`)
    /// across every mutating command site so all writes flow into one file.
    /// Subscribed by a tokio task that re-emits each append as
    /// `hiker:changes-appended` for the home-page activity widget.
    pub(crate) changes: Arc<Changes>,
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
    pub(crate) autosave: Arc<Autosave>,
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
    pub(crate) read_store: Arc<Mutex<Store>>,
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
                self.handle_raptor_triage_match(tree_id, source_path).map(Some)
            }
            hiker_core::tasks::TaskKind::RaptorSummarize {
                tree_id,
                cluster_node_id,
                level,
            } => self
                .handle_raptor_summarize(tree_id, cluster_node_id, *level)
                .map(Some),
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

impl DirectWorkerHandlers {
    fn handle_raptor_triage_match(
        &self,
        tree_id: &str,
        source_path: &str,
    ) -> Result<serde_json::Value, String> {
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
        serde_json::to_value(&outcome).map_err(|e| e.to_string())
    }

    fn handle_raptor_summarize(
        &self,
        tree_id: &str,
        cluster_node_id: &str,
        level: u8,
    ) -> Result<serde_json::Value, String> {
        let node = self
            .trees
            .get_node(tree_id, cluster_node_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("cluster node not found: {tree_id}/{cluster_node_id}"))?;
        if !matches!(node.kind, hiker_core::trees::NodeKind::Cluster) {
            return Err(format!(
                "raptor_summarize target is not a cluster: {cluster_node_id}"
            ));
        }
        if node.user_edited_name && node.user_edited_summary {
            // Both fields user-edited — nothing to write back.
            return Ok(serde_json::json!({
                "node_id": cluster_node_id,
                "skipped": "user_edited",
            }));
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
                level: level as usize,
                members,
            })
            .map_err(|e| format!("summarize: {e}"))?;
        let (wrote_name, wrote_summary) = self
            .trees
            .auto_set_name_summary(tree_id, cluster_node_id, &out.name, &out.summary)
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({
            "node_id": cluster_node_id,
            "name": out.name,
            "summary": out.summary,
            "confidence": out.confidence,
            "wrote_name": wrote_name,
            "wrote_summary": wrote_summary,
        }))
    }
}

pub(crate) fn with_vault<R>(
    state: &State<AppState>,
    f: impl FnOnce(&Vault) -> CmdResult<R>,
) -> CmdResult<R> {
    let guard = state.session.lock()?;
    let session = guard.as_ref().ok_or_else(CmdError::no_vault_open)?;
    f(&session.vault)
}

/// Run a closure with a borrow of the open `VaultSession`. Returns
/// `CmdError::no_vault_open` if no vault is open. The lock is held only
/// while the closure runs — callers that need to drop it before an
/// `.await` should clone what they need out via `with_session_async`,
/// not this helper.
pub(crate) fn with_session<R>(
    state: &State<AppState>,
    f: impl FnOnce(&VaultSession) -> CmdResult<R>,
) -> CmdResult<R> {
    let guard = state.session.lock()?;
    let session = guard.as_ref().ok_or_else(CmdError::no_vault_open)?;
    f(session)
}

/// Async variant of `with_session`: snapshot the cloneable handles you
/// need from the session inside the synchronous `picker`, drop the lock,
/// then run `body` with those handles. The closure shape enforces the
/// "drop guard before await" rule statically (the picker can't `.await`).
pub(crate) async fn with_session_async<T, R, F, Fut>(
    state: &State<'_, AppState>,
    picker: impl FnOnce(&VaultSession) -> CmdResult<T>,
    body: F,
) -> CmdResult<R>
where
    F: FnOnce(T) -> Fut,
    Fut: std::future::Future<Output = CmdResult<R>>,
{
    let picked = {
        let guard = state.session.lock()?;
        let session = guard.as_ref().ok_or_else(CmdError::no_vault_open)?;
        picker(session)?
    };
    body(picked).await
}

/// Log an `Err(_)` returned to the frontend, then pass the Result through
/// unchanged. Wrap a command's final expression in this so every failure
/// shows up in the unified log without scattering `tracing::error!` calls
/// across each `.map_err` chain. Per `obs-error-context`: the error chain
/// rides the `error` field, the message stays grep-stable.
pub(crate) fn log_cmd_result<T, E: std::fmt::Display>(
    command: &'static str,
    r: Result<T, E>,
) -> Result<T, E> {
    if let Err(e) = &r {
        tracing::error!(error = %e, command, "tauri command failed");
    }
    r
}

// Vault-open bootstrap moved to `crate::cmds::bootstrap` (the
// `open_vault_at` Tauri command + its inner that stands up the indexer,
// watcher, MCP, staging recheck, scheduled triage rerun, etc.).
//
// Indexer-shaped commands (`index`, `index_state_for`, `index_status`,
// `count_notes_in`, `compute_diff`) moved to `crate::cmds::indexer`.
// Search-shaped commands (`search_vault`, `related_notes`) moved to
// `crate::cmds::search`.
// Vault-home + note-metadata commands (`vault_home_stats`,
// `recent_notes_modified` / `_accessed`, `note_accessed`,
// `note_properties`, `chat_resolve_at_note`, `chat_at_autocomplete`)
// moved to `crate::cmds::vault_home`.
// Note-mutation producer surface (`submit_note_mutation`) moved to
// `crate::cmds::mutations`.
// `start_mcp`, `start_config_watcher`, and `log_from_frontend` moved to
// `crate::cmds::mcp`.

// Autosave commands moved to `crate::cmds::autosave`.
// Trails commands moved to `crate::cmds::trails`.
// Staging commands moved to `crate::cmds::staging`.
// Activity feed commands moved to `crate::cmds::activity`.
// Cluster editor commands moved to `crate::cmds::cluster`.
// Changelog query / rollback commands moved to `crate::cmds::changes`.
// Task queue commands moved to `crate::cmds::queue`.

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
            cmds::changes::recent_changes,
            cmds::changes::changes_count,
            cmds::changes::change_content,
            cmds::changes::rollback_change,
            cmds::changes::restore_snapshot,
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
            cmds::queue::tasks_snapshot,
            cmds::queue::tasks_cancel,
            cmds::queue::task_details,
            submit_note_mutation,
            cmds::autosave::autosave_write,
            cmds::autosave::autosave_clear,
            cmds::autosave::autosave_save_tab_state,
            cmds::autosave::autosave_load_tab_state,
            cmds::autosave::autosave_recover,
            cmds::autosave::autosave_discard,
            log_from_frontend,
            cmds::trails::trail_create,
            cmds::trails::trail_append_waypoint,
            cmds::trails::trail_remove_waypoint,
            cmds::trails::trail_descendant_count,
            cmds::trails::trail_delete,
            cmds::trails::trails_list,
            cmds::trails::trail_get,
            cmds::trails::trails_containing_note,
            cmds::trails::trail_set_active,
            cmds::trails::trail_set_append_cursor,
            cmds::staging::staging_list,
            cmds::staging::staging_count,
            cmds::staging::staging_accept,
            cmds::staging::staging_reject,
            cmds::staging::staging_accept_all,
            cmds::staging::staging_content,
            cmds::activity::activity_list,
            cmds::activity::activity_list_for_path,
            cmds::activity::activity_count,
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
            cluster_summarize,
            cluster_op_rollup,
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
