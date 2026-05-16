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

use tauri::Manager;

use hiker_core::activity::Activity;
use hiker_core::autosave::Autosave;
use hiker_core::changes::Changes;
use hiker_core::config::{Config, SettingsScope};
use hiker_core::indexer::route_watcher_events;
use hiker_core::staging::{Staging, StagingFilter};
use hiker_core::store::Store;
use hiker_core::watcher::{FileEvent, Watcher};
use hiker_core::{embed::FastembedEmbedder, HikerError, Vault};

use crate::chat;
use crate::cmds::cluster::parse_rerun_interval;
use crate::cmds::mcp::{start_config_watcher, start_mcp};
use crate::cmds::watcher_router::WatcherRouter;
use crate::{log_cmd_result, AppState, DirectWorkerHandlers, VaultSession};

/// Borrow-bundle for the spawn helpers below. Holds &-refs to the
/// long-lived per-vault state constructed in `open_vault_at_inner`, so
/// helpers can `.clone()` only the Arcs they actually capture into a
/// spawned task instead of receiving them as long parameter lists. The
/// owned Arcs still flow into the final `VaultSession`; this struct is
/// purely an in-function plumbing convenience.
struct SessionHandle<'a> {
    app: &'a tauri::AppHandle,
    vault: &'a Vault,
    changes: &'a Arc<Changes>,
    staging: &'a Arc<Staging>,
    trees: &'a Arc<hiker_core::trees::Trees>,
    read_store: &'a Arc<Mutex<Store>>,
    watcher: &'a Arc<Watcher>,
    tasks: &'a Arc<hiker_core::tasks::Queue>,
    tasks_cancel: &'a tokio_util::sync::CancellationToken,
    audit: &'a Arc<hiker_core::audit::AgentLog>,
    prompts: &'a Arc<hiker_core::prompts::Prompts>,
    config: &'a Config,
    staging_config: &'a Arc<std::sync::RwLock<hiker_core::config::StagingConfig>>,
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
/// Apply a single FileEvent to the staging-recheck path. Called by a
/// `WatcherRouter` handler; the changes-broadcast side has its own
/// small task (`spawn_staging_changes_recheck`) since `WatcherRouter`
/// fans out FileEvents only.
fn staging_recheck_on_file_event(
    staging: &Staging,
    vault: &Vault,
    staging_config: &std::sync::RwLock<hiker_core::config::StagingConfig>,
    ev: &FileEvent,
) {
    match ev {
        FileEvent::Created { path }
        | FileEvent::Modified { path }
        | FileEvent::Deleted { path } => {
            recheck_path(staging, vault, staging_config, path);
        }
        FileEvent::Renamed { from, to } => {
            recheck_path(staging, vault, staging_config, from);
            recheck_path(staging, vault, staging_config, to);
        }
        FileEvent::Overflow => {
            // After overflow, our knowledge of the filesystem is
            // stale. Recheck every pending proposal against
            // current disk so conflicted state catches up.
            if let Ok(all) = staging.list(&StagingFilter::default()) {
                let mut seen = std::collections::HashSet::new();
                for p in &all {
                    if seen.insert(p.target_path.clone()) {
                        recheck_path(staging, vault, staging_config, &p.target_path);
                    }
                }
            }
        }
    }
}

/// status: staging-drift-eager-recheck
/// Spawn the changes-broadcast side of the staging-recheck path. The
/// watcher-broadcast side rides the `WatcherRouter` registered in
/// `open_vault_at_inner`; this small task owns the `core::changes`
/// subscription so an appended row also re-checks matching proposals.
fn spawn_staging_changes_recheck(session: &SessionHandle<'_>) {
    let staging = session.staging.clone();
    let vault = session.vault.clone();
    let staging_config = session.staging_config.clone();
    let mut changes_rx = session.changes.subscribe();
    tokio::spawn(async move {
        loop {
            match changes_rx.recv().await {
                Ok(row) => {
                    recheck_path(&staging, &vault, &staging_config, &row.path);
                    if let Some(ref from) = row.rename_from {
                        recheck_path(&staging, &vault, &staging_config, from);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
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

/// Vault-open is a fixed sequence of phases:
/// 1. `validate_path_and_open_vault` — directory check + `Vault::open`.
/// 2. `init_tracing_and_load_config` — per-vault tracing pipeline +
///    strict-load + recent/default persist.
/// 3. `open_storage_handles` — changes, staging, trees, activity,
///    autosave, store (writer + reader), GC passes, frontend forwarders
///    for changes/staging.
/// 4. `start_indexing` — task queue, indexer task, watcher,
///    `route_watcher_events`, late-attach watcher + changes.
/// 5. `prepare_session_handle` — staging_config Arc, audit log, tasks
///    cancel token, prompts; build the in-function `SessionHandle`.
/// 6. `attach_router_and_background` — staging-changes-recheck task,
///    `WatcherRouter`, indexer progress + status forwarders, full_scan
///    kick, direct-LLM workers, scheduled-triage rerun.
/// 7. `start_mcp_and_late_attach` — MCP server, staleness warnings,
///    api-key preflight, chat registry resume, config-file watcher.
/// 8. Assemble + install the `VaultSession`.
async fn open_vault_at_inner(
    app: tauri::AppHandle,
    path_buf: PathBuf,
) -> Result<String, HikerError> {
    let (vault, root, display) = validate_path_and_open_vault(path_buf)?;
    let mut config = init_tracing_and_load_config(&root)?;
    let storage = open_storage_handles(&app, &root, &config)?;
    let (indexer, watcher) = start_indexing(&vault, &root, &storage, &config)?;
    let prep = prepare_session_handle(&root, &config, &storage)?;

    // Borrow-bundle for the spawn helpers; each one `.clone()`s only
    // the Arcs it needs into the spawned task.
    let handle = SessionHandle {
        app: &app,
        vault: &vault,
        changes: &storage.changes,
        staging: &storage.staging,
        trees: &storage.trees,
        read_store: &storage.read_store,
        watcher: &watcher,
        tasks: &storage.tasks,
        tasks_cancel: &prep.tasks_cancel,
        audit: &prep.audit,
        prompts: &prep.prompts,
        config: &config,
        staging_config: &prep.staging_config,
    };
    attach_router_and_background(&app, &handle, &indexer).await;
    drop(handle);

    let late = start_mcp_and_late_attach(&app, &vault, &root, &indexer, &watcher, &mut config, &storage, &prep).await;

    let session = VaultSession {
        vault,
        root,
        indexer,
        watcher,
        changes: storage.changes,
        staging: storage.staging,
        trees: storage.trees,
        activity: storage.activity,
        autosave: storage.autosave,
        config: RwLock::new(config),
        read_store: storage.read_store,
        mcp: late.mcp,
        chat: late.chat_registry,
        prompts: prep.prompts,
        audit: prep.audit,
        tasks: storage.tasks,
        tasks_cancel: prep.tasks_cancel,
        config_watcher_cancel: late.config_watcher_cancel,
        mcp_tools: late.mcp_tools,
        staging_config: prep.staging_config,
    };

    let state = app.state::<AppState>();
    *state
        .session
        .lock()
        .map_err(|_| HikerError::Io("session lock poisoned".into()))? = Some(session);
    Ok(display)
}

/// Phase 1 — preflight + open the bare `Vault`.
fn validate_path_and_open_vault(
    path_buf: PathBuf,
) -> Result<(Vault, PathBuf, String), HikerError> {
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
    Ok((vault, root, display))
}

/// Phase 2 — bring up tracing, then strict-load the merged user + vault
/// config; persist recent / default vault back to user-scope TOML.
fn init_tracing_and_load_config(root: &std::path::Path) -> Result<Config, HikerError> {
    // Stand up the tracing pipeline (per-vault log files). Idempotent across
    // vault swaps in the same UI session — the first call wins.
    if let Err(e) = hiker_core::observability::init_tracing(root) {
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
    let mut config = Config::load(root)?;
    persist_vault_recent_and_default(&mut config, root);
    Ok(config)
}

/// Long-lived storage handles produced by phase 3.
struct StorageHandles {
    changes: Arc<Changes>,
    staging: Arc<Staging>,
    trees: Arc<hiker_core::trees::Trees>,
    activity: Arc<Activity>,
    autosave: Arc<Autosave>,
    read_store: Arc<Mutex<Store>>,
    /// Writer connection — taken by the indexer task in phase 4.
    /// `Mutex<Option<_>>` so phase 4 can `.take()` it through a shared
    /// reference; the slot is `None` for the rest of the session.
    store_writer: Mutex<Option<Store>>,
    /// Task queue, constructed up front so the indexer can submit
    /// `EmbedderModelLoad` rows around its startup load.
    tasks: Arc<hiker_core::tasks::Queue>,
}

/// Phase 3 — open every per-vault store + the read/write store
/// connections + the task queue, run opening GC passes, and start the
/// changes/staging frontend forwarders.
fn open_storage_handles(
    app: &tauri::AppHandle,
    root: &std::path::Path,
    config: &Config,
) -> Result<StorageHandles, HikerError> {
    // status: changes-store-file
    // Open the changelog db (separate from index.db so the index can be
    // regenerated freely while the changelog stays durable). Best-effort:
    // a failed open shouldn't block vault open, but every subsequent
    // append call will silently no-op until next vault swap.
    let changes = Arc::new(
        Changes::open(root).map_err(|e| HikerError::Io(format!("changes db: {e}")))?,
    );

    // Open the staging area for proposed writes. Created at vault open,
    // lives for the duration of the session. The MCP server references
    // this instance to route write tools through propose() when
    // `[mcp.tools].review_required` is true.
    //
    // status: agent-write-review-mode
    let staging = Arc::new(
        Staging::open(root).map_err(|e| HikerError::Io(format!("staging: {e}")))?,
    );

    // status: trees-db
    // Cluster-tree storage for the cluster editor (Sprint B). Opened
    // alongside staging so every command site can reach `session.trees`
    // without a second mutex hop.
    let trees = Arc::new(
        hiker_core::trees::Trees::open(root)
            .map_err(|e| HikerError::Io(format!("trees: {e}")))?,
    );

    // status: activity-feed-module
    let activity = Arc::new(Activity::new(changes.clone(), staging.clone()));

    // status: autosave-backend-module, autosave-store-layout
    // Open the per-vault autosave store. Failure is fatal at vault open
    // (a future tick would just keep failing silently otherwise).
    let autosave = Arc::new(
        Autosave::open(root).map_err(|e| HikerError::Io(format!("autosave: {e}")))?,
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
    {
        let app_for_fwd = app.clone();
        forward_broadcast(changes.subscribe(), move |row| {
            crate::events::emit_changes_appended(&app_for_fwd, row);
        });
    }

    // Forward staging mutations to the frontend as `hiker:staging-changed`.
    // This catches proposals from the MCP surface (which lacks an AppHandle)
    // as well as the Tauri accept/reject commands.
    {
        let app_for_fwd = app.clone();
        forward_broadcast_unit(staging.subscribe(), move || {
            crate::events::emit_staging_changed(&app_for_fwd);
        });
    }

    // Open the store (creates .hiker/index.db on first run). This is the
    // writer connection that the indexer task takes ownership of in
    // phase 4.
    let store = Store::open(root).map_err(|e| HikerError::Io(e.to_string()))?;

    // Open a *second* connection against the same db for every read-side
    // Tauri command. WAL mode (set on the writer above) is per-file, so
    // both connections see committed writes without locking; the sqlite-vec
    // extension auto-registers process-once. See `VaultSession.read_store`.
    let read_store =
        Arc::new(Mutex::new(Store::open(root).map_err(|e| HikerError::Io(e.to_string()))?));

    // status: task-queue-core-module
    // status: embedder-model-load-as-task
    // Construct the task queue up front so the indexer can submit an
    // `EmbedderModelLoad` row around its startup `FastembedEmbedder::load_id`
    // call. The event-forwarder / maintenance-tick / direct-worker spawn
    // still happens in phase 6 where the LLM client + audit log are
    // ready — we just need the `Queue` handle here.
    let tasks = Arc::new(hiker_core::tasks::Queue::new(config.tasks.clone()));

    Ok(StorageHandles {
        changes,
        staging,
        trees,
        activity,
        autosave,
        read_store,
        store_writer: Mutex::new(Some(store)),
        tasks,
    })
}

/// Phase 4 — start the indexer + watcher and bridge the watcher's
/// FileEvents into the indexer; late-bind watcher + changes onto the
/// indexer for the trails auto-update path.
fn start_indexing(
    vault: &Vault,
    root: &std::path::Path,
    storage: &StorageHandles,
    config: &Config,
) -> Result<(hiker_core::indexer::IndexerHandle, Arc<Watcher>), HikerError> {
    // The writer Store has to be consumed by the indexer task. We
    // moved it into `StorageHandles.store` to keep phase 3 returning a
    // single struct; take it out here.
    let store = storage
        .store_writer
        .lock()
        .map_err(|_| HikerError::Io("store_writer lock poisoned".into()))?
        .take()
        .expect("start_indexing called twice for the same vault open");

    // Spawn the indexer task. The embedder loader runs inside the task on a
    // blocking thread — this call returns immediately. The model id is read
    // from `[indexing].model` and threaded into the loader; switching models
    // via the settings UI is hot-reloaded (see `embedder-hot-reload-on-model-change`),
    // and both the startup and hot-reload loads now surface as queue rows
    // (`embedder-model-load-as-task`).
    let model_id_for_loader = config.indexing.model.clone();
    let model_id_for_task = config.indexing.model.clone();
    let tasks_for_loader = storage.tasks.clone();
    let indexer = hiker_core::indexer::start_indexer_with_tasks(
        vault.clone(),
        store,
        move || {
            FastembedEmbedder::load_id(&model_id_for_loader).map(|e| {
                std::sync::Arc::new(e) as std::sync::Arc<dyn hiker_core::embed::Embedder>
            })
        },
        Some(hiker_core::indexer::EmbedderLoadTaskPlumbing {
            queue: tasks_for_loader,
            initial_model_id: model_id_for_task,
        }),
    );

    // Start the filesystem watcher and bridge its events into the indexer.
    // `route_watcher_events` keeps its own dedicated `subscribe()` (a
    // pure path-to-IndexJob translator with FullScan-on-lag recovery);
    // every *other* watcher consumer flows through the single
    // `WatcherRouter` registered in phase 6.
    let watcher = Arc::new(Watcher::start(root).map_err(|e| HikerError::Io(e.to_string()))?);
    let watcher_rx = watcher.subscribe();
    let job_sender = indexer.job_sender();
    tokio::spawn(route_watcher_events(watcher_rx, job_sender));

    // status: trail-auto-update-on-note-move
    // Late-bind watcher + changes to the indexer so the trails
    // auto-update path can suppress watcher events around its rewrites
    // and append `core::changes` rows for each touched file.
    indexer.attach_watcher(watcher.clone());
    indexer.attach_changes(storage.changes.clone());

    Ok((indexer, watcher))
}

/// Phase 5 outputs — small, shared Arcs needed by phase 6 / 7 spawns
/// and finally folded into the `VaultSession`.
struct SessionPrep {
    staging_config: Arc<RwLock<hiker_core::config::StagingConfig>>,
    audit: Arc<hiker_core::audit::AgentLog>,
    tasks_cancel: tokio_util::sync::CancellationToken,
    prompts: Arc<hiker_core::prompts::Prompts>,
}

fn prepare_session_handle(
    root: &std::path::Path,
    config: &Config,
    _storage: &StorageHandles,
) -> Result<SessionPrep, HikerError> {
    // status: staging-config-section
    // Shared `[staging]` config — read live by the staging recheck task so
    // `auto_reject_on_conflict` applies without a restart.
    let staging_config: Arc<RwLock<hiker_core::config::StagingConfig>> =
        Arc::new(RwLock::new(config.staging.clone()));

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
    // Cancellation token for the direct-LLM worker + queue forwarders;
    // bound on the session so vault swap halts them cleanly.
    let tasks_cancel = tokio_util::sync::CancellationToken::new();
    // Loaded up here so the direct-worker handlers + the session both
    // share one Arc. The session uses it via `chat_send`; the worker
    // uses it to render the `cluster_summarize` prompt.
    let prompts: Arc<hiker_core::prompts::Prompts> = Arc::new(
        hiker_core::prompts::Prompts::load(root)
            .map_err(|e| HikerError::Io(format!("prompts: {e}")))?,
    );

    Ok(SessionPrep {
        staging_config,
        audit,
        tasks_cancel,
        prompts,
    })
}

/// Phase 6 — install the watcher router, indexer-event forwarders,
/// kick the initial scan, and spawn the direct-LLM workers + scheduled
/// triage rerun.
async fn attach_router_and_background(
    app: &tauri::AppHandle,
    handle: &SessionHandle<'_>,
    indexer: &hiker_core::indexer::IndexerHandle,
) {
    // status: staging-drift-eager-recheck
    // The `core::changes` side of the staging recheck flow runs as its
    // own small task; the watcher side rides the `WatcherRouter`
    // registered below (alongside triage-on-save and the frontend
    // forwarder). Both ultimately call `Staging::recheck`, which
    // persists state transitions and broadcasts `hiker:staging-changed`
    // via the staging forwarder set up in phase 3.
    spawn_staging_changes_recheck(handle);

    // status: cluster-editor-triage-on-save
    // status: cluster-editor-triage-via-staging
    // status: cluster-build-from-folders-live-update
    // status: staging-drift-eager-recheck
    //
    // Consolidated fan-out for every watcher consumer that used to call
    // `watcher.subscribe()` on its own. Each handler is a closure over
    // the per-vault Arc state it needs. Handlers run in their own task
    // draining a bounded mpsc, so a slow handler can't block fast ones
    // — preserving the property the previous independent-`subscribe()`
    // design had. See `cmds::watcher_router`.
    install_watcher_router(handle);

    // Forward indexer progress events to the frontend.
    {
        let app_for_fwd = app.clone();
        forward_broadcast(indexer.subscribe_progress(), move |ev| {
            crate::events::emit_reindex_progress(&app_for_fwd, ev);
        });
    }

    // Forward status snapshots to the frontend so it can drop its 2s poll.
    // Emit the seeded value first (queued/total_notes/last_error are populated
    // before the indexer task even runs), then on every change.
    spawn_index_status_forwarder(app.clone(), indexer.subscribe_status());

    // Kick the initial scan. Returns immediately; jobs flow as the model
    // load completes.
    let _ = indexer.full_scan().await;

    // status: task-queue-core-module
    // Stand up the direct-LLM worker + queue event-forwarder. The
    // `tasks` Queue itself is constructed in phase 3 (so the indexer
    // can submit `EmbedderModelLoad` rows around its startup load —
    // see `embedder-model-load-as-task`). Always construct the queue
    // so the MCP server can advertise `task_*` (gated separately on
    // `[mcp] enabled`); the direct worker only spawns when both
    // `[llm] enabled` and `[tasks] direct_worker.enabled` are true
    // (per `task-queue-respects-llm-disable`).
    spawn_task_queue_workers(handle);

    // status: cluster-editor-triage-scheduled-rerun
    //
    // Periodic re-run of the triage classifier over every note inside
    // the configured scope. The cron-shape parser is a follow-up — for
    // Sprint F we accept simple duration strings (`30m`, `1h`, `6h`,
    // `24h`, `7d`); cron expressions get logged and ignored. Empty
    // disables. Each tick enqueues one `RaptorTriageMatch` task at
    // `Low` priority per (saved-as-triage tree × note in scope).
    spawn_triage_scheduled_rerun(handle);
}

/// Phase 7 outputs — folded into the `VaultSession` by the caller.
struct LateAttach {
    mcp: Option<hiker_mcp::McpServerHandle>,
    mcp_tools: Arc<RwLock<hiker_core::config::McpToolsConfig>>,
    chat_registry: Arc<chat::ChatRegistry>,
    config_watcher_cancel: tokio_util::sync::CancellationToken,
}

#[allow(clippy::too_many_arguments)]
async fn start_mcp_and_late_attach(
    app: &tauri::AppHandle,
    vault: &Vault,
    root: &PathBuf,
    indexer: &hiker_core::indexer::IndexerHandle,
    watcher: &Arc<Watcher>,
    config: &mut Config,
    storage: &StorageHandles,
    prep: &SessionPrep,
) -> LateAttach {
    // status: mcp-tool-toggles
    // Shared per-tool gate config. Held by the MCP handler so dispatches
    // read it live; updated by `set_setting` so flips in the settings UI
    // apply without a vault restart.
    let mcp_tools: Arc<RwLock<hiker_core::config::McpToolsConfig>> =
        Arc::new(RwLock::new(config.mcp.tools.clone()));

    // status: mcp-server-crate
    // Start the in-process MCP server. Failure to bind logs and continues —
    // the user's vault is more important than MCP availability.
    let mcp = match start_mcp(
        vault,
        root,
        indexer,
        watcher,
        &storage.changes,
        &storage.read_store,
        config,
        &prep.audit,
        &storage.tasks,
        &mcp_tools,
        &storage.staging,
    )
    .await
    {
        Ok(handle) => Some(handle),
        Err(hiker_mcp::StartError::Disabled) => None,
        Err(e) => {
            tracing::warn!(error = %e, "mcp: start failed");
            None
        }
    };

    emit_prompts_staleness_warnings(root, &prep.audit);
    preflight_api_key_warning(app, config);

    let chat_registry = Arc::new(chat::ChatRegistry::default());
    // status: chat-session-resume-latest
    // Adopt the most-recent on-disk session as the active one (if any
    // exist). The registry's `active` slot drives `chat_session_active`,
    // which the frontend calls on vault open to seed the panel.
    if let Err(e) = chat::resume_latest_at_open(&chat_registry, root, config) {
        tracing::warn!(error = %e, "sessions: resume_latest_at_open failed");
    }

    // Start the config-file watcher so external edits to either TOML are
    // picked up live and the UI re-applies settings without a restart.
    let config_watcher_cancel = tokio_util::sync::CancellationToken::new();
    {
        let app_for_cw = app.clone();
        let root_for_cw = root.to_path_buf();
        let cancel = config_watcher_cancel.clone();
        tokio::spawn(async move {
            start_config_watcher(app_for_cw, root_for_cw, cancel).await;
        });
    }

    LateAttach {
        mcp,
        mcp_tools,
        chat_registry,
        config_watcher_cancel,
    }
}

/// status: staging-drift-eager-recheck
/// status: cluster-editor-triage-on-save
/// status: cluster-editor-triage-via-staging
/// status: cluster-build-from-folders-live-update
///
/// Build the `WatcherRouter` and register every consumer that used to
/// call `watcher.subscribe()` on its own. One subscription against the
/// watcher's broadcast feeds three handlers:
/// - Frontend forwarder (`hiker:file-changed` / `hiker:watcher-overflow`).
/// - Staging-recheck on FileEvent (the changes-broadcast side is its own
///   small task — see `spawn_staging_changes_recheck`).
/// - Triage on save / FromFolders live-update on rename.
fn install_watcher_router(session: &SessionHandle<'_>) {
    let mut router = WatcherRouter::new();

    // Frontend forwarder. Sees every event (including Overflow, which
    // emits a separate channel name to the frontend).
    let app_for_fwd = session.app.clone();
    router.add(
        "frontend-forwarder",
        |_ev| true,
        move |ev| {
            let app = app_for_fwd.clone();
            async move {
                match ev {
                    FileEvent::Overflow => {
                        crate::events::emit_watcher_overflow(&app);
                    }
                    _ => {
                        crate::events::emit_file_changed(&app, &ev);
                    }
                }
            }
        },
    );

    // Staging recheck on FileEvent. Synchronous body; the `async move`
    // is just here to satisfy the handler signature.
    let staging_for_recheck = session.staging.clone();
    let vault_for_recheck = session.vault.clone();
    let staging_cfg_for_recheck = session.staging_config.clone();
    router.add(
        "staging-recheck",
        |_ev| true,
        move |ev| {
            let staging = staging_for_recheck.clone();
            let vault = vault_for_recheck.clone();
            let staging_config = staging_cfg_for_recheck.clone();
            async move {
                staging_recheck_on_file_event(&staging, &vault, &staging_config, &ev);
            }
        },
    );

    // Triage on save + FromFolders live-update on rename.
    let trees_for_triage = session.trees.clone();
    let staging_for_triage = session.staging.clone();
    let vault_for_triage = session.vault.clone();
    let read_store_for_triage = session.read_store.clone();
    let cfg_triage = session.config.suggestions.triage.clone();
    router.add(
        "triage-on-save",
        |ev| {
            matches!(
                ev,
                FileEvent::Modified { .. }
                    | FileEvent::Created { .. }
                    | FileEvent::Renamed { .. }
            )
        },
        move |ev| {
            let trees = trees_for_triage.clone();
            let staging = staging_for_triage.clone();
            let vault = vault_for_triage.clone();
            let read_store = read_store_for_triage.clone();
            let cfg_triage = cfg_triage.clone();
            async move {
                let (modified_path, rename_target): (Option<String>, Option<(String, String)>) =
                    match ev {
                        FileEvent::Modified { path } | FileEvent::Created { path } => {
                            (Some(path), None)
                        }
                        FileEvent::Renamed { from, to } => {
                            (Some(to.clone()), Some((from, to)))
                        }
                        _ => (None, None),
                    };
                // FromFolders live-update on rename.
                if let Some((rel_from, rel_to)) = rename_target.clone() {
                    handle_from_folders_rename(&trees, &read_store, &rel_to);
                    let _ = rel_from;
                }
                // Triage classifier on modify/create.
                let Some(rel) = modified_path else {
                    return;
                };
                run_triage_on_path(&trees, &staging, &vault, &read_store, &cfg_triage, &rel);
            }
        },
    );

    router.start(session.watcher.subscribe());
}

fn handle_from_folders_rename(
    trees: &Arc<hiker_core::trees::Trees>,
    read_store: &Arc<Mutex<Store>>,
    rel_to: &str,
) {
    let trees_rows = match trees.list_trees() {
        Ok(r) => r,
        Err(_) => return,
    };
    let note_id = {
        let store_guard = match read_store.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        store_guard.id_for_path(rel_to).ok().flatten()
    };
    let new_folder = rel_to
        .rsplit_once('/')
        .map(|(a, _)| a.to_string())
        .unwrap_or_default();
    let Some(nid) = note_id else { return };
    for t in &trees_rows {
        if t.state != "saved-as-triage" {
            continue;
        }
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
        let _ = trees.update_for_folder_rename(&t.id, &nid, &new_folder);
    }
}

fn run_triage_on_path(
    trees: &Arc<hiker_core::trees::Trees>,
    staging: &Arc<Staging>,
    vault: &Vault,
    read_store: &Arc<Mutex<Store>>,
    cfg_triage: &hiker_core::config::TriageConfig,
    rel: &str,
) {
    // Cheap scope pre-filter — skip files outside the
    // configured triage scope before touching the store.
    let scope_trim = cfg_triage.scope.trim();
    if !scope_trim.is_empty() && !rel.starts_with(scope_trim) {
        return;
    }
    let (note_id, embedding) = {
        let store_guard = match read_store.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(note_id) = store_guard.id_for_path(rel).ok().flatten() else {
            return;
        };
        let Some(embedding) = store_guard.note_embedding_for_path(rel).ok().flatten() else {
            return;
        };
        (note_id, embedding)
    };
    let opts = hiker_core::suggest::TriageOpts {
        review_required: cfg_triage.review_required,
        scope: cfg_triage.scope.clone(),
        beam_width: 2,
    };
    let store_guard = match read_store.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let _ = hiker_core::suggest::triage_all_saved_trees(
        hiker_core::suggest::TriageBatch {
            trees,
            vault,
            store: &store_guard,
            staging,
            note_id: &note_id,
            source_path: rel,
            embedding: &embedding,
            author_class: hiker_core::suggest::NoteAuthorClass::User,
            opts: &opts,
        },
    );
}

/// status: cluster-editor-triage-scheduled-rerun
///
/// Periodic re-run of the triage classifier over every note inside the
/// configured scope.
fn spawn_triage_scheduled_rerun(session: &SessionHandle<'_>) {
    let trees = session.trees.clone();
    let read_store = session.read_store.clone();
    let tasks = session.tasks.clone();
    let cfg_sched_str = session.config.suggestions.triage.scheduled_rerun.clone();
    let cfg_scope = session.config.suggestions.triage.scope.clone();
    let Some(every) = parse_rerun_interval(&cfg_sched_str) else {
        if !cfg_sched_str.trim().is_empty() {
            eprintln!(
                "[hiker] suggestions.triage.scheduled_rerun: unsupported value {:?} — accepted forms are duration strings like '30m', '1h', '6h', '24h', '7d'. Cron expressions are not yet supported.",
                cfg_sched_str
            );
        }
        return;
    };
    tokio::spawn(async move {
        // Initial delay so we don't fire on startup.
        tokio::time::sleep(every).await;
        let mut ticker = tokio::time::interval(every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            let saved: Vec<String> = match trees.list_trees() {
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
                let store_guard = match read_store.lock() {
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
                    let _ = tasks.submit(task).await;
                }
            }
        }
    });
}

/// status: task-queue-core-module
/// Stand up the queue event-forwarder, maintenance tick, and direct-LLM
/// worker bundle.
fn spawn_task_queue_workers(session: &SessionHandle<'_>) {
    let tasks = session.tasks.clone();
    let tasks_cancel = session.tasks_cancel.clone();
    let trees = session.trees.clone();
    let vault = session.vault.clone();
    let staging = session.staging.clone();
    let read_store = session.read_store.clone();
    let prompts_for_workers = session.prompts.clone();
    let audit = session.audit.clone();
    let config = session.config;
    // Forward queue events to the frontend.
    let app_for_queue = session.app.clone();
    let mut rx = tasks.subscribe();
    let cancel = tasks_cancel.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                ev = rx.recv() => match ev {
                    Ok(e) => { crate::events::emit_queue_event(&app_for_queue, &e); }
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
    // Direct-LLM worker. Spawned whenever `[llm] enabled = true` — the
    // per-iteration `direct_worker.enabled` check inside
    // `run_direct_worker` honors live toggles from the settings UI
    // without a vault restart.
    //
    // status: task-queue-raptor-triage-match
    let triage_handler: Arc<dyn hiker_core::tasks::NonLlmHandlers> =
        Arc::new(DirectWorkerHandlers {
            trees,
            vault,
            staging,
            read_store,
            config: Arc::new(std::sync::RwLock::new(config.clone())),
            prompts: prompts_for_workers,
        });
    if !config.llm.enabled {
        return;
    }
    let client = match hiker_core::llm::GraniteLlmClient::from_config(&config.llm) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "tasks: direct worker not started (llm client build failed)",
            );
            return;
        }
    };
    let llm_client: Arc<dyn hiker_core::llm::LlmClient> = Arc::new(client);
    let parallelism = config.tasks.direct_worker.parallelism.max(1);
    for _ in 0..parallelism {
        let q = (*tasks).clone();
        let client = llm_client.clone();
        let audit = Some(audit.clone());
        let cancel = tasks_cancel.clone();
        let handlers = Some(triage_handler.clone());
        tokio::spawn(async move {
            hiker_core::tasks::run_direct_worker(q, client, audit, handlers, cancel).await;
        });
    }
}

/// Persist `vault.recent` and `vault.default` to user-scope TOML. Best-effort:
/// any failure is logged and ignored — the in-memory `config` is updated to
/// match disk on success.
fn persist_vault_recent_and_default(config: &mut Config, root: &std::path::Path) {
    let recent = hiker_core::config::push_recent_vault(&config.vault.recent, root);
    if recent != config.vault.recent {
        let value = serde_json::Value::Array(
            recent.iter().map(|s| serde_json::Value::String(s.clone())).collect(),
        );
        match Config::set(SettingsScope::User, "vault.recent", value, root) {
            Ok(updated) => *config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.recent"),
        }
    }
    let root_str = root.to_string_lossy().to_string();
    if config.vault.default.as_deref() != Some(root_str.as_str()) {
        match Config::set(
            SettingsScope::User,
            "vault.default",
            serde_json::Value::String(root_str),
            root,
        ) {
            Ok(updated) => *config = updated,
            Err(e) => tracing::warn!(error = %e, "failed to update vault.default"),
        }
    }
}

/// Generic forwarder from a broadcast receiver to a typed-emit closure.
/// The closure (rather than an event-name string) keeps the wire name
/// out of this helper — every backend → frontend channel now flows
/// through `crate::events::emit_*`.
fn forward_broadcast<T, F>(mut rx: tokio::sync::broadcast::Receiver<T>, emit: F)
where
    T: Clone + Send + 'static,
    F: Fn(&T) + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(payload) => {
                    emit(&payload);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
}

fn forward_broadcast_unit<F>(mut rx: tokio::sync::broadcast::Receiver<()>, emit: F)
where
    F: Fn() + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(()) => {
                    emit();
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
}

fn spawn_index_status_forwarder(
    app: tauri::AppHandle,
    mut status_rx: tokio::sync::watch::Receiver<hiker_core::indexer::IndexStatus>,
) {
    tokio::spawn(async move {
        crate::events::emit_index_status(&app, &status_rx.borrow_and_update());
        while status_rx.changed().await.is_ok() {
            crate::events::emit_index_status(&app, &status_rx.borrow_and_update());
        }
    });
}

/// status: llm-prompts-staleness-on-upgrade
/// Surface bundled-default drift once per session. Writes both a tracing warn
/// and an audit-log row per stale feature.
fn emit_prompts_staleness_warnings(
    root: &std::path::Path,
    audit: &hiker_core::audit::AgentLog,
) {
    match hiker_core::prompts::Prompts::staleness(root) {
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
}

/// status: llm-providers-config
/// API-key preflight: surface a missing key at vault open. Logs *and* emits
/// `hiker:llm-warning` so the frontend can render a user-visible toast.
fn preflight_api_key_warning(app: &tauri::AppHandle, config: &Config) {
    if !config.llm.enabled {
        return;
    }
    let literal = config.llm.provider.api_key.as_str();
    let env_name = config.llm.provider.api_key_env.as_str();
    let literal_set = !literal.is_empty();
    let env_named_and_unset = !env_name.is_empty() && std::env::var(env_name).is_err();
    if literal_set || !env_named_and_unset {
        return;
    }
    tracing::warn!(
        env = %env_name,
        backend = %config.llm.provider.backend,
        "llm: no api key — literal unset and env var missing; chat will fail until set",
    );
    crate::events::emit_llm_warning(
        app,
        &crate::events::LlmWarning {
            kind: "missing_api_key",
            env: env_name,
            message: format!(
                "{env_name} unset and no literal api_key — chat will fail until you set one in Settings or your shell",
            ),
        },
    );
}
