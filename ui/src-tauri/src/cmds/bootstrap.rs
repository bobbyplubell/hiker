//! Vault-open bootstrap. Stands up every long-lived subsystem the UI
//! needs for a single open vault (`VaultSession`): indexer, watcher,
//! staging, trees, activity, autosave, MCP server, direct LLM worker,
//! scheduled triage rerun, config-file watcher, etc.
//!
//! Extracted verbatim from `lib.rs` — no behavior changes. The function
//! is intentionally still one big block; future refactor passes can
//! decompose it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use tauri::{Emitter, Manager};

use hiker_core::activity::Activity;
use hiker_core::autosave::Autosave;
use hiker_core::changes::{ChangeRow, Changes};
use hiker_core::config::{Config, SettingsScope};
use hiker_core::indexer::route_watcher_events;
use hiker_core::staging::{Staging, StagingFilter};
use hiker_core::store::Store;
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, HikerError, Vault};

use crate::chat;
use crate::cmds::cluster::parse_rerun_interval;
use crate::cmds::mcp::{start_config_watcher, start_mcp};
use crate::{log_cmd_result, AppState, DirectWorkerHandlers, VaultSession};

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
pub(crate) fn spawn_staging_recheck(
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
pub(crate) fn recheck_path(
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
pub(crate) async fn open_vault_at(
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

    // status: task-queue-core-module
    // status: embedder-model-load-as-task
    // Construct the task queue up front so the indexer can submit an
    // `EmbedderModelLoad` row around its startup `FastembedEmbedder::load_id`
    // call. The event-forwarder / maintenance-tick / direct-worker spawn
    // still happens further down where the LLM client + audit log are
    // ready — we just need the `Queue` handle here.
    let tasks = Arc::new(hiker_core::tasks::Queue::new(config.tasks.clone()));

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately. The model id is read
    // from `[indexing].model` and threaded into the loader; switching models
    // via the settings UI is hot-reloaded (see `embedder-hot-reload-on-model-change`),
    // and both the startup and hot-reload loads now surface as queue rows
    // (`embedder-model-load-as-task`).
    let model_id_for_loader = config.indexing.model.clone();
    let model_id_for_task = config.indexing.model.clone();
    let indexer = hiker_core::indexer::start_indexer_with_tasks(
        vault.clone(),
        store,
        move || {
            FastembedEmbedder::load_id(&model_id_for_loader).map(|e| {
                std::sync::Arc::new(e) as std::sync::Arc<dyn hiker_core::embed::Embedder>
            })
        },
        Some(hiker_core::indexer::EmbedderLoadTaskPlumbing {
            queue: tasks.clone(),
            initial_model_id: model_id_for_task,
        }),
    );

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
    // Stand up the direct-LLM worker + queue event-forwarder. The
    // `tasks` Queue itself is constructed earlier (so the indexer can
    // submit `EmbedderModelLoad` rows around its startup load — see
    // `embedder-model-load-as-task`). Always construct the queue so
    // the MCP server can advertise `task_*` (gated separately on
    // `[mcp] enabled`); the direct worker only spawns when both
    // `[llm] enabled` and `[tasks] direct_worker.enabled` are true
    // (per `task-queue-respects-llm-disable`).
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
