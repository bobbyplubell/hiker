//! Vault-open recipe. Constructs `AppState` from a vault path.
//!
//! Wires the long-lived subsystems hiker needs to be useful (store,
//! changes, staging, trees, activity, watcher, indexer, autosave). The
//! direct LLM worker + MCP server layer on top.
//!
//! Spawned background tasks (watcher relay, indexer progress forwarder,
//! direct LLM worker) all clone the `VaultSession.cancel` token so the
//! vault-swap path can shut them down before the new session lands —
//! see `state::VaultSession` and `main::update`'s vault-switch branch.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use hiker_core::activity::Activity;
use hiker_core::audit::AgentLog;
use hiker_core::autosave::Autosave;
use hiker_core::changes::Changes;
use hiker_core::config::Config;
use hiker_core::embed::FastembedEmbedder;
use hiker_core::indexer::{route_watcher_events, start_indexer, IndexerHandle};
use hiker_core::staging::Staging;
use hiker_core::store::Store;
use hiker_core::tasks::{Queue as TaskQueue, TaskRecord};
use hiker_core::trees::Trees;
use hiker_core::watcher::Watcher;
use hiker_core::Vault;

use crate::state::{
    AppState, MutationEvent, PanelStates, Services, Session, UiCache, UiState, VaultEvents,
    VaultSession, VaultSwitchState,
};

pub async fn open_vault(root: PathBuf) -> Result<AppState> {
    let root = std::fs::canonicalize(&root)
        .with_context(|| format!("canonicalize vault root: {}", root.display()))?;

    let vault = Vault::open(&root)
        .with_context(|| format!("open vault at {}", root.display()))?;
    let vault = Arc::new(vault);

    let config = Config::load(&root).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "config load failed, using defaults");
        Config::default()
    });
    let config = Arc::new(std::sync::RwLock::new(config));

    // Long-lived subsystems. Order matters: store comes before changes/
    // staging since they all live under .hiker/, and the read store is
    // shared by indexer and any read-side commands.
    let read_store = Arc::new(Mutex::new(
        Store::open(&root).with_context(|| "open read store")?,
    ));

    let changes = Arc::new(
        Changes::open(&root).with_context(|| "open changes log")?,
    );
    let staging = Arc::new(
        Staging::open(&root).with_context(|| "open staging")?,
    );
    let trees = Arc::new(
        Trees::open(&root).with_context(|| "open trees db")?,
    );
    let activity = Arc::new(Activity::new(changes.clone(), staging.clone()));
    let autosave = Arc::new(
        Autosave::open(&root).with_context(|| "open autosave")?,
    );

    // Watcher — broadcast channel that other subsystems subscribe to.
    let watcher = Arc::new(
        Watcher::start(&root).with_context(|| "start watcher")?,
    );

    // Indexer.
    let writer_store = Store::open(&root).with_context(|| "open writer store")?;
    let model_id = config
        .read()
        .map(|c| c.indexing.model.clone())
        .unwrap_or_default();
    let indexer: IndexerHandle = start_indexer(
        (*vault).clone(),
        writer_store,
        move || {
            FastembedEmbedder::load_id(&model_id)
                .map(|e| Arc::new(e) as Arc<dyn hiker_core::embed::Embedder>)
        },
    );
    indexer.attach_watcher(watcher.clone());
    indexer.attach_changes(changes.clone());

    // Wire watcher → indexer router so file events drive re-indexing.
    let _router = route_watcher_events(watcher.subscribe(), indexer.job_sender());

    // Kick off the initial full scan. Errors are logged but non-fatal.
    if let Err(err) = indexer.full_scan().await {
        tracing::warn!(error = %err, "indexer: initial full_scan submit failed");
    }

    // Single CancellationToken cloned into every spawned background task
    // for this session. The vault-swap path in `main::update` calls
    // `.cancel()` on this token before dropping the session so the relays
    // stop touching the old state pointer.
    let cancel = CancellationToken::new();

    // Watcher → app-state relay.
    let (fs_tx, fs_rx) = tokio::sync::mpsc::unbounded_channel();
    spawn_watcher_relay(watcher.subscribe(), fs_tx, cancel.clone());

    // Indexer progress forwarder.
    let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let indexer_arc = Arc::new(indexer);
    spawn_indexer_progress_relay(
        indexer_arc.subscribe_progress(),
        ev_tx,
        cancel.clone(),
    );

    // Channel for note-mutation outcomes.
    let (mut_tx, mut_rx) =
        tokio::sync::mpsc::unbounded_channel::<MutationEvent>();

    // Audit log + task queue: shared dependencies for MCP + agent loop.
    let audit = Arc::new(AgentLog::new(
        root.join(".hiker/audit"),
        config
            .read()
            .map(|c| c.llm.audit.log_full_prompt)
            .unwrap_or(false),
    ));
    let tasks_cfg = config.read().map(|c| c.tasks.clone()).unwrap_or_default();
    let tasks = Arc::new(TaskQueue::new(tasks_cfg.clone()));

    // Direct LLM worker. Now consumes the session's CancellationToken
    // (previously it constructed a throwaway token and the spawned task
    // leaked across vault swaps).
    let llm_cfg = config.read().map(|c| c.llm.clone()).unwrap_or_default();
    if llm_cfg.enabled
        && let Ok(client) = hiker_core::llm::GraniteLlmClient::from_config(&llm_cfg)
    {
        let queue_for_worker = (*tasks).clone();
        let client_arc: Arc<dyn hiker_core::llm::LlmClient> = Arc::new(client);
        let audit_for_worker = Some(audit.clone());
        let worker_cancel = cancel.clone();
        tokio::spawn(async move {
            hiker_core::tasks::run_direct_worker(
                queue_for_worker,
                client_arc,
                audit_for_worker,
                None,
                worker_cancel,
            )
            .await;
        });
    }

    // MCP server: start if enabled in settings. Non-fatal on bind error.
    let mcp_cfg = config.read().map(|c| c.mcp.clone()).unwrap_or_default();
    let mcp: Option<Arc<hiker_mcp::McpServerHandle>> = if mcp_cfg.enabled {
        let mcp_tools_cfg = Arc::new(std::sync::RwLock::new(mcp_cfg.tools.clone()));
        let llm_enabled = config.read().map(|c| c.llm.enabled).unwrap_or(false);
        let deps = hiker_mcp::McpDeps {
            vault: (*vault).clone(),
            vault_root: root.clone(),
            read_store: read_store.clone(),
            jobs: indexer_arc.job_sender(),
            watcher: watcher.clone(),
            changes: changes.clone(),
            embedder_provider: indexer_arc.embedder_provider(),
            config: mcp_cfg.clone(),
            tools: mcp_tools_cfg,
            audit: audit.clone(),
            tasks: tasks.clone(),
            tasks_config: tasks_cfg,
            llm_enabled,
            staging: staging.clone(),
        };
        match hiker_mcp::start(deps).await {
            Ok(handle) => {
                tracing::info!(addr = %handle.addr(), "mcp server started");
                Some(Arc::new(handle))
            }
            Err(err) => {
                tracing::warn!(error = %err, "mcp server failed to start");
                None
            }
        }
    } else {
        None
    };

    // Crash recovery + persisted tab snapshot for the new Session.
    let recovery_entries = autosave.recover().unwrap_or_default();
    let tab_state = autosave.load_tab_state().ok().flatten();
    let trails = load_trails(&root);

    let events = build_events_and_spawn_poller(
        fs_rx,
        ev_rx,
        mut_rx,
        mut_tx,
        tasks.clone(),
        staging.clone(),
        read_store.clone(),
        cancel.clone(),
    );

    // Assemble the compartments.
    let services = Services {
        read_store,
        changes,
        staging,
        trees,
        activity,
        autosave,
        watcher,
        indexer: indexer_arc,
        audit,
        tasks,
        mcp,
    };
    let vault_session = VaultSession {
        vault: vault.clone(),
        vault_root: root.clone(),
        config: config.clone(),
        services,
        events,
        cancel,
    };

    let modal = if !recovery_entries.is_empty() {
        Some(crate::state::Modal::Recovery {
            entries: recovery_entries,
        })
    } else {
        None
    };
    let bundle = crate::layout::load_for_vault(&root);
    let session = Session {
        trails,
        modal,
        dock: bundle.tree,
        center_tile: bundle.center_tile,
        left_tile: bundle.left_tile,
        right_tile: bundle.right_tile,
        ..Session::default()
    };

    let panels = build_panel_states(&config);

    let mut state = AppState {
        vault_session,
        session,
        ui_cache: UiCache::default(),
        panels,
        ui: UiState::default(),
        toasts: Vec::new(),
        vault_switch: VaultSwitchState::Idle,
        workbench: crate::workbench_host::new_workbench(),
    };

    if let Some(ts) = tab_state {
        restore_tab_state(&mut state, ts);
    }

    // Ensure the dock's center never starts blank. If nothing was
    // restored from autosave (fresh vault, or first launch on this
    // machine), open the Home tab so the reconciler has something to
    // populate the center leaf with. Without this, the default dock's
    // center is `Node::Empty`, which egui_dock allocates rect for but
    // never paints, leaving a visible void between the side zones.
    if state.session.tabs.is_empty() {
        let id = state.next_tab_id();
        state.session.tabs.push(crate::tab::Tab {
            id,
            kind: crate::tab::TabKind::Home,
            sticky: true,
        });
        state.session.active_tab = Some(id);
    }

    // Load data-driven toolbar layout (falls back to default if the file
    // is missing or malformed). Done after the rest of the state is
    // assembled so the loader gets a stable `vault_root` to read from.
    state.ui.toolbars =
        crate::actions::load_toolbars(&state.vault_session.vault_root);

    Ok(state)
}

/// Load the persisted trails list from `<root>/.hiker/trails.json`.
/// Supports the legacy flat-`Vec<String>` format by migrating into a
/// single "Recent" trail.
/// Seed the per-panel UI state for a fresh `AppState`. The search,
/// related, and backlinks sub-panels each pull a few fields from the
/// vault config (persisted toggles + collapse flags); everything else
/// falls back to `PanelStates::default()`.
fn build_panel_states(
    config: &std::sync::Arc<std::sync::RwLock<Config>>,
) -> PanelStates {
    let cfg_guard = config.read().ok();
    let (search, related, backlinks) = match cfg_guard.as_deref() {
        Some(c) => (
            crate::panels::search::SearchState::from_config(c),
            crate::panels::related::RelatedState::from_config(c),
            crate::panels::backlinks::BacklinksState::from_config(c),
        ),
        None => (
            crate::panels::search::SearchState::default(),
            crate::panels::related::RelatedState::default(),
            crate::panels::backlinks::BacklinksState::default(),
        ),
    };
    PanelStates {
        search,
        related,
        backlinks,
        ..PanelStates::default()
    }
}

fn load_trails(root: &std::path::Path) -> Vec<crate::state::Trail> {
    let path = root.join(".hiker/trails.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    if let Ok(trails) = serde_json::from_slice::<Vec<crate::state::Trail>>(&bytes) {
        return trails;
    }
    if let Ok(legacy) = serde_json::from_slice::<Vec<String>>(&bytes) {
        let waypoints: Vec<crate::state::Waypoint> = legacy
            .into_iter()
            .map(|p| crate::state::Waypoint {
                path: p,
                at_ms: 0,
                children: Vec::new(),
                annotation: String::new(),
            })
            .collect();
        return vec![crate::state::Trail {
            id: "recent-migrated".to_string(),
            name: crate::state::RECENT_TRAIL.to_string(),
            waypoints,
            created_at_ms: 0,
            last_activated_at_ms: 0,
            append_under: None,
        }];
    }
    Vec::new()
}

/// Persist the trails list to `<root>/.hiker/trails.json`.
pub fn save_trails(
    root: &std::path::Path,
    trails: &[crate::state::Trail],
) -> std::io::Result<()> {
    let dir = root.join(".hiker");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
    }
    let path = dir.join("trails.json");
    let bytes = serde_json::to_vec(trails).unwrap_or_else(|_| b"[]".to_vec());
    std::fs::write(path, bytes)
}

fn restore_tab_state(state: &mut AppState, ts: hiker_core::autosave::TabState) {
    use crate::tab::{Tab, TabKind};

    let mut first_id: Option<crate::tab::TabId> = None;
    let mut active_id: Option<crate::tab::TabId> = None;
    let mut preview_id: Option<crate::tab::TabId> = None;
    for path in ts.open_paths {
        let singleton_kind: Option<TabKind> = match path.as_str() {
            ":home" => Some(TabKind::Home),
            ":queue" => Some(TabKind::Queue),
            ":settings" => Some(TabKind::Settings),
            ":graph" => Some(TabKind::Graph),
            ":patch_review" => Some(TabKind::PatchReview),
            ":plugins" => Some(TabKind::Plugins),
            ":indexer" => Some(TabKind::IndexerDetail),
            ":agent_changes" => Some(TabKind::AgentChanges),
            _ => None,
        };
        let kind = if let Some(k) = singleton_kind {
            k
        } else {
            if !state.session.buffers.contains_key(&path) {
                if let Ok((contents, hash)) =
                    state.vault_session.vault.read_file_with_hash(&path)
                {
                    let cfg_guard = state.vault_session.config.read().ok();
                    let buf = crate::buffer::Buffer::with_config_and_vault(
                        path.clone(),
                        contents,
                        hash,
                        cfg_guard.as_deref(),
                        Some(state.vault_session.vault.clone()),
                    );
                    drop(cfg_guard);
                    state.session.buffers.insert(path.clone(), buf);
                }
            }
            TabKind::Buffer { path: path.clone() }
        };
        let id = state.next_tab_id();
        state.session.tabs.push(Tab {
            id,
            kind,
            sticky: true,
        });
        if first_id.is_none() {
            first_id = Some(id);
        }
        if ts.active_path.as_deref() == Some(path.as_str()) {
            active_id = Some(id);
        }
        if ts.preview_path.as_deref() == Some(path.as_str()) {
            preview_id = Some(id);
            if let Some(t) = state.session.tabs.iter_mut().find(|t| t.id == id) {
                t.sticky = false;
            }
        }
    }
    state.session.active_tab = active_id.or(first_id);
    state.session.preview_tab = preview_id;

    if let Some(active) = ts.active_path.as_deref() {
        crate::state::nav_push(state, active);
    }
}

fn spawn_watcher_relay(
    mut sub: tokio::sync::broadcast::Receiver<hiker_core::watcher::FileEvent>,
    fs_tx: tokio::sync::mpsc::UnboundedSender<hiker_core::watcher::FileEvent>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = sub.recv() => match recv {
                    Ok(ev) => {
                        if fs_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
    });
}

fn spawn_indexer_progress_relay(
    mut progress_rx: tokio::sync::broadcast::Receiver<hiker_core::indexer::ProgressEvent>,
    ev_tx: tokio::sync::mpsc::UnboundedSender<String>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = progress_rx.recv() => match recv {
                    Ok(ev) => {
                        let line = format_progress_event(&ev);
                        if ev_tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
            }
        }
    });
}

fn format_progress_event(ev: &hiker_core::indexer::ProgressEvent) -> String {
    match ev {
        hiker_core::indexer::ProgressEvent::ModelLoaded => "model loaded".to_string(),
        hiker_core::indexer::ProgressEvent::Started { path } => format!("start {path}"),
        hiker_core::indexer::ProgressEvent::Finished { path } => format!("done  {path}"),
        hiker_core::indexer::ProgressEvent::Skipped { path, reason } => {
            format!("skip  {path}: {reason}")
        }
        hiker_core::indexer::ProgressEvent::Deleted { path } => format!("del   {path}"),
        hiker_core::indexer::ProgressEvent::Renamed { from, to } => {
            format!("rename {from} -> {to}")
        }
        hiker_core::indexer::ProgressEvent::ScanComplete { scanned, queued } => {
            format!("scan complete (scanned={scanned} queued={queued})")
        }
        hiker_core::indexer::ProgressEvent::Error { path, message } => match path {
            Some(p) => format!("error {p}: {message}"),
            None => format!("error: {message}"),
        },
    }
}

/// Construct `VaultEvents` and spawn the background snapshot pollster.
/// Splits a chunk of plumbing out of `open_vault` so the latter stays
/// under the function-length budget.
#[allow(clippy::too_many_arguments)]
fn build_events_and_spawn_poller(
    fs_rx: tokio::sync::mpsc::UnboundedReceiver<hiker_core::watcher::FileEvent>,
    ev_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut_rx: tokio::sync::mpsc::UnboundedReceiver<MutationEvent>,
    mut_tx: tokio::sync::mpsc::UnboundedSender<MutationEvent>,
    tasks: Arc<TaskQueue>,
    staging: Arc<Staging>,
    read_store: Arc<Mutex<hiker_core::store::Store>>,
    cancel: CancellationToken,
) -> VaultEvents {
    let (task_snap_tx, task_snap_rx) = watch::channel::<Vec<TaskRecord>>(Vec::new());
    let (staging_snap_tx, staging_snap_rx) =
        watch::channel::<Vec<hiker_core::staging::Proposal>>(Vec::new());
    let (skipped_snap_tx, skipped_snap_rx) =
        watch::channel::<std::collections::HashSet<String>>(std::collections::HashSet::new());
    spawn_snapshot_poller(
        tasks,
        staging,
        read_store,
        task_snap_tx,
        staging_snap_tx,
        skipped_snap_tx,
        cancel,
    );
    VaultEvents {
        fs_events: Mutex::new(fs_rx),
        indexer_events_rx: Mutex::new(ev_rx),
        mutation_events: Mutex::new(mut_rx),
        mutation_events_tx: mut_tx,
        indexer_events: VecDeque::new(),
        task_snapshot_rx: task_snap_rx,
        staging_snapshot_rx: staging_snap_rx,
        skipped_paths_rx: skipped_snap_rx,
    }
}

/// Background pollster that owns the per-vault snapshot cadence.
///
/// Replaces the per-frame `tasks.snapshot().await` / `staging.list_pending()`
/// / `store.list_skipped_paths()` calls that used to fire from the egui
/// render loop. The pollster wakes every 200ms; tasks + staging are
/// re-snapshotted every tick (cheap), and the read-store-locked skipped
/// paths query is re-snapshotted every ~3s.
///
/// First tick fires immediately so the very first UI frame after
/// `open_vault` returns sees a populated cache instead of empty defaults.
/// On `cancel.cancelled()` the task exits and the watch senders drop;
/// receivers stay valid (they latch the last value) until the
/// `VaultSession` itself drops.
fn spawn_snapshot_poller(
    tasks: Arc<TaskQueue>,
    staging: Arc<Staging>,
    read_store: Arc<Mutex<hiker_core::store::Store>>,
    task_tx: watch::Sender<Vec<TaskRecord>>,
    staging_tx: watch::Sender<Vec<hiker_core::staging::Proposal>>,
    skipped_tx: watch::Sender<HashSet<String>>,
    cancel: CancellationToken,
) {
    const TICK: Duration = Duration::from_millis(200);
    const SKIPPED_INTERVAL: Duration = Duration::from_secs(3);

    tokio::spawn(async move {
        let mut last_skipped = std::time::Instant::now()
            .checked_sub(SKIPPED_INTERVAL)
            .unwrap_or_else(std::time::Instant::now);
        let mut interval = tokio::time::interval(TICK);
        // `MissedTickBehavior::Delay` keeps us from spamming bursts after
        // a hiccup; we don't care about exactly-200ms cadence.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // Snapshot first, then await — so the first iteration publishes
            // an immediate value (the interval's first `.tick()` resolves
            // instantly anyway, but doing the work up-front documents the
            // invariant that the UI sees real data on frame one).
            let snap = tasks.snapshot().await;
            // Watch channels short-circuit equality checks via `send_replace`;
            // we use `send` which always notifies and just overwrites — fine
            // for "always want latest" semantics and cheaper than a deep
            // equality test on a Vec of records.
            let _ = task_tx.send(snap);

            match staging.list_pending() {
                Ok(v) => {
                    let _ = staging_tx.send(v);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "poller: staging.list_pending failed");
                }
            }

            if last_skipped.elapsed() >= SKIPPED_INTERVAL {
                match read_store.lock() {
                    Ok(store) => match store.list_skipped_paths() {
                        Ok(paths) => {
                            let _ = skipped_tx.send(paths.into_iter().collect());
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "poller: list_skipped_paths failed");
                        }
                    },
                    Err(_) => {
                        tracing::warn!("poller: skipped paths refresh: store mutex poisoned");
                    }
                }
                last_skipped = std::time::Instant::now();
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
        }
    });
}
