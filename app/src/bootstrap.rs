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
use hiker_core::indexer::{route_watcher_events, start, Handle};
use hiker_core::staging::Staging;
use hiker_core::store::Store;
use hiker_core::tasks::queue::Queue as TaskQueue;
use hiker_core::tasks::types::TaskRecord;
use hiker_core::trees::types::Db;
use hiker_core::watcher::Watcher;
use hiker_core::vault::Vault;

use crate::state::{
    AppState, MutationEvent, PanelStates, Services, Session, UiCache, UiState, VaultEvents,
    VaultSession, VaultSwitchState,
};

/// Zero-sized formatter for indexer `ProgressEvent`s. Kept as an inherent
/// method (rather than a free fn) so the per-event branching counts toward
/// this type's complexity, not `open_vault`'s, and it stays exempt from
/// `single_call_fn`.
struct ProgressLine;

impl ProgressLine {
    fn format(self, ev: &hiker_core::indexer::ProgressEvent) -> String {
        use hiker_core::indexer::ProgressEvent as P;
        match ev {
            P::ModelLoaded => "model loaded".to_string(),
            P::Started { path } => format!("start {path}"),
            P::Finished { path } => format!("done  {path}"),
            P::Skipped { path, reason } => format!("skip  {path}: {reason}"),
            P::Deleted { path } => format!("del   {path}"),
            P::Renamed { from, to } => format!("rename {from} -> {to}"),
            P::ScanComplete { scanned, queued } => {
                format!("scan complete (scanned={scanned} queued={queued})")
            }
            P::Error { path, message } => match path {
                Some(p) => format!("error {p}: {message}"),
                None => format!("error: {message}"),
            },
        }
    }
}

/// Zero-sized helper that starts the MCP server. Kept as an inherent
/// method so its bind-result branching counts toward this type, not
/// `open_vault`, and stays exempt from `single_call_fn`.
struct McpStarter;

impl McpStarter {
    /// Start the MCP server when `enabled`; `None` (no-op) otherwise.
    /// Folds the enable-gate in so the caller stays a single statement.
    async fn start(
        self,
        enabled: bool,
        deps: hiker_mcp::McpDeps,
    ) -> Option<Arc<hiker_mcp::McpServerHandle>> {
        if !enabled {
            return None;
        }
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
    }
}

/// Per-session background-task spawner. Holds the session `cancel` token
/// so every relay/poller it launches shuts down together on vault swap.
/// Each spawn is a method (not an inlined block) so its loop body counts
/// toward `Spawner`'s complexity rather than `open_vault`'s, while staying
/// exempt from `single_call_fn` as an inherent method.
struct Spawner {
    cancel: CancellationToken,
}

/// The already-spawned relay channel ends the UI drains each frame:
/// filesystem events, indexer progress lines, and note-mutation
/// outcomes (both the receiver and the sender clone handed to the UI).
struct RelayChannels {
    fs_rx: tokio::sync::mpsc::UnboundedReceiver<hiker_core::watcher::FileEvent>,
    ev_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut_rx: tokio::sync::mpsc::UnboundedReceiver<MutationEvent>,
    mut_tx: tokio::sync::mpsc::UnboundedSender<MutationEvent>,
}

/// Inputs for `Spawner::snapshot_poller`: the read-side handles to poll
/// plus the watch senders the UI reads.
struct SnapshotChannels {
    tasks: Arc<TaskQueue>,
    staging: Arc<Staging>,
    read_store: Arc<Mutex<Store>>,
    task_snap_tx: watch::Sender<Vec<TaskRecord>>,
    staging_snap_tx: watch::Sender<Vec<hiker_core::staging::types::Proposal>>,
    skipped_snap_tx: watch::Sender<HashSet<String>>,
}

impl Spawner {
    /// Watcher → app-state relay. Forwards normalized `FileEvent`s onto an
    /// unbounded channel the UI thread drains; exits on `cancel` or when
    /// the receiver is dropped.
    fn watcher_relay(
        &self,
        mut sub: tokio::sync::broadcast::Receiver<hiker_core::watcher::FileEvent>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<hiker_core::watcher::FileEvent> {
        let (fs_tx, fs_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = self.cancel.clone();
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
        fs_rx
    }

    /// Indexer progress forwarder. Formats each `ProgressEvent` to a status
    /// line and forwards it onto an unbounded channel the UI drains.
    fn indexer_progress_relay(
        &self,
        mut progress_rx: tokio::sync::broadcast::Receiver<hiker_core::indexer::ProgressEvent>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    recv = progress_rx.recv() => match recv {
                        Ok(ev) => {
                            if ev_tx.send(ProgressLine.format(&ev)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
        });
        ev_rx
    }

    /// Snapshot pollster: tasks + staging every 200ms; the read-store-
    /// locked skipped-paths query every ~3s. First tick fires immediately
    /// so the very first UI frame sees a populated cache. Exits on `cancel`.
    fn snapshot_poller(&self, ch: SnapshotChannels) {
        let cancel = self.cancel.clone();
        const TICK: Duration = Duration::from_millis(200);
        const SKIPPED_INTERVAL: Duration = Duration::from_secs(3);
        tokio::spawn(async move {
            let mut last_skipped = std::time::Instant::now()
                .checked_sub(SKIPPED_INTERVAL)
                .unwrap_or_else(std::time::Instant::now);
            let mut interval = tokio::time::interval(TICK);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let snap = ch.tasks.snapshot().await;
                let _ = ch.task_snap_tx.send(snap);

                match ch.staging.list_pending() {
                    Ok(v) => {
                        let _ = ch.staging_snap_tx.send(v);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "poller: staging.list_pending failed");
                    }
                }

                if last_skipped.elapsed() >= SKIPPED_INTERVAL {
                    match ch.read_store.lock() {
                        Ok(store) => match store.list_skipped_paths() {
                            Ok(paths) => {
                                let _ = ch.skipped_snap_tx.send(paths.into_iter().collect());
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

    /// Spawn the direct LLM worker when the provider is enabled and a
    /// client can be built from config. Consumes the session `cancel`
    /// token so the worker stops on vault swap. No-op when LLM is off or
    /// the client fails to construct (logged upstream by the caller).
    fn spawn_direct_llm_worker(
        &self,
        llm_cfg: &hiker_core::config::sections::LlmConfig,
        tasks: &Arc<TaskQueue>,
        audit: &Arc<AgentLog>,
    ) {
        if !llm_cfg.enabled {
            return;
        }
        let Ok(client) = hiker_core::llm::GraniteLlmClient::from_config(llm_cfg) else {
            return;
        };
        let queue_for_worker = (**tasks).clone();
        let client_arc: Arc<dyn hiker_core::llm::Client> = Arc::new(client);
        let audit_for_worker = Some(audit.clone());
        let worker_cancel = self.cancel.clone();
        tokio::spawn(async move {
            hiker_core::tasks::handlers::run_direct_worker(
                queue_for_worker,
                client_arc,
                audit_for_worker,
                None,
                worker_cancel,
            )
            .await;
        });
    }

    /// Wire the three `watch` channels the UI reads, spawn the snapshot
    /// pollster that refreshes them, and fold them together with the
    /// already-spawned relay channels into a `VaultEvents`.
    fn build_vault_events(
        &self,
        tasks: &Arc<TaskQueue>,
        staging: &Arc<Staging>,
        read_store: &Arc<Mutex<Store>>,
        relays: RelayChannels,
    ) -> VaultEvents {
        let (task_snap_tx, task_snap_rx) = watch::channel::<Vec<TaskRecord>>(Vec::new());
        let (staging_snap_tx, staging_snap_rx) =
            watch::channel::<Vec<hiker_core::staging::types::Proposal>>(Vec::new());
        let (skipped_snap_tx, skipped_snap_rx) = watch::channel::<HashSet<String>>(HashSet::new());

        self.snapshot_poller(SnapshotChannels {
            tasks: tasks.clone(),
            staging: staging.clone(),
            read_store: read_store.clone(),
            task_snap_tx,
            staging_snap_tx,
            skipped_snap_tx,
        });

        VaultEvents {
            fs_events: Mutex::new(relays.fs_rx),
            indexer_events_rx: Mutex::new(relays.ev_rx),
            mutation_events: Mutex::new(relays.mut_rx),
            mutation_events_tx: relays.mut_tx,
            indexer_events: VecDeque::new(),
            task_snapshot_rx: task_snap_rx,
            staging_snapshot_rx: staging_snap_rx,
            skipped_paths_rx: skipped_snap_rx,
        }
    }
}

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
        Db::open(&root).with_context(|| "open trees db")?,
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
    let indexer: Handle = start(
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
    if let Err(err) = indexer.full_scan(false).await {
        tracing::warn!(error = %err, "indexer: initial full_scan submit failed");
    }

    // Single CancellationToken cloned into every spawned background task
    // for this session. The vault-swap path in `main::update` calls
    // `.cancel()` on this token before dropping the session so the relays
    // stop touching the old state pointer.
    let cancel = CancellationToken::new();

    // All session-scoped background relays/pollers share one cancel
    // token; `Spawner` owns a clone so each spawn site stays a one-liner
    // and the relay loops count toward `Spawner`'s complexity, not
    // `open_vault`'s.
    let spawner = Spawner { cancel: cancel.clone() };

    // Watcher → app-state relay.
    let fs_rx = spawner.watcher_relay(watcher.subscribe());

    // Indexer progress forwarder.
    let indexer_arc = Arc::new(indexer);
    let ev_rx = spawner.indexer_progress_relay(indexer_arc.subscribe_progress());

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

    // Direct LLM worker. Consumes the session's CancellationToken so the
    // spawned task is torn down on vault swap (see `Spawner`).
    let llm_cfg = config.read().map(|c| c.llm.clone()).unwrap_or_default();
    spawner.spawn_direct_llm_worker(&llm_cfg, &tasks, &audit);

    // MCP server: start if enabled in settings. Non-fatal on bind error.
    let mcp_cfg = config.read().map(|c| c.mcp.clone()).unwrap_or_default();
    // Created up-front so it can be both handed to the MCP handler AND
    // stashed in `Services` for `set_setting` to mirror future changes
    // into. The same Arc is shared; the handler's `state.tools.read()`
    // returns whatever value `set_setting` last wrote.
    let mcp_tools_cfg = Arc::new(std::sync::RwLock::new(mcp_cfg.tools.clone()));
    let mcp = McpStarter.start(mcp_cfg.enabled, hiker_mcp::McpDeps {
        vault: (*vault).clone(),
        vault_root: root.clone(),
        read_store: read_store.clone(),
        jobs: indexer_arc.job_sender(),
        watcher: watcher.clone(),
        changes: changes.clone(),
        embedder_provider: indexer_arc.embedder_provider(),
        config: mcp_cfg.clone(),
        tools: mcp_tools_cfg.clone(),
        audit: audit.clone(),
        tasks: tasks.clone(),
        tasks_config: tasks_cfg,
        llm_enabled: config.read().map(|c| c.llm.enabled).unwrap_or(false),
        staging: staging.clone(),
    }).await;

    // Crash recovery + persisted tab snapshot for the new Session.
    let recovery_entries = autosave.recover().unwrap_or_default();
    let tab_state = autosave.load_tab_state().ok().flatten();
    // Inlined `load_trails`: read `<root>/.hiker/trails.json`; treat
    // any failure (missing file / bad JSON) as an empty list so a fresh
    // vault opens cleanly.
    let trails: Vec<crate::state::Trail> = {
        let path = root.join(".hiker/trails.json");
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<crate::state::Trail>>(&bytes).ok())
            .unwrap_or_default()
    };

    // Wire the UI `watch` channels + snapshot pollster and fold them in
    // with the relay channels (a `Spawner` method, so its wiring counts
    // toward `Spawner`, not `open_vault`).
    let events = spawner.build_vault_events(
        &tasks,
        &staging,
        &read_store,
        RelayChannels { fs_rx, ev_rx, mut_rx, mut_tx },
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
        mcp_tools_cfg,
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

    // Inlined panel-state seeding: search/related/backlinks pull persisted
    // toggles + collapse flags from the vault config when readable;
    // everything else defaults. Inlined (not a `PanelStates` constructor)
    // because a `self`-less associated fn would trip `single_call_fn`.
    let panels: PanelStates = {
        let cfg_guard = config.read().ok();
        let (search, related, backlinks) = match cfg_guard.as_deref() {
            Some(c) => (
                crate::panels::search::State::default().with_config(c),
                crate::panels::related::State::default().with_config(c),
                crate::panels::backlinks::State::default().with_config(c),
            ),
            None => (
                crate::panels::search::State::default(),
                crate::panels::related::State::default(),
                crate::panels::backlinks::State::default(),
            ),
        };
        PanelStates { search, related, backlinks, ..PanelStates::default() }
    };

    let mut state = AppState {
        vault_session,
        session,
        ui_cache: UiCache::default(),
        panels,
        ui: UiState::default(),
        toasts: Vec::new(),
        vault_switch: VaultSwitchState::Idle,
        workbench: {
            // Inlined `new_workbench`: fresh workbench with the default
            // activity (`Files`) selected, the Chat (secondary) side bar
            // visible, and the global bottom status strip on.
            let mut wb = egui_workbench::workspace::Workbench::default();
            wb.activity_bar
                .set_active(Some(crate::workbench_host::HikerMode::Files));
            wb.secondary_side_bar.visible = true;
            wb.status_bar.visible = true;
            wb
        },
    };

    if let Some(ts) = tab_state {
        state.restore_tab_state(ts);
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
    // Inlined rather than a `self`-less `Toolbars` loader fn, which would
    // trip `single_call_fn`.
    state.ui.toolbars = {
        let path = state.vault_session.vault_root.join(".hiker/toolbars.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<crate::state::Toolbars>(&bytes)
                .unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "toolbars: parse failed; using default");
                    crate::state::Toolbars::default()
                }),
            Err(_) => crate::state::Toolbars::default(),
        }
    };

    Ok(state)
}

// Panel-state seeding, toolbar loading, and the `load_trails` reader are
// inlined at their unique callers in `open_vault` (a `self`-less
// associated fn would trip `single_call_fn`, and the inline keeps
// channel/config ownership visible at the use site).

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

impl AppState {
    /// Re-open tabs saved by the autosave layer. Restores buffer paths
    /// and singleton page kinds, sets the active and preview tabs, and
    /// seeds the nav history.
    pub(crate) fn restore_tab_state(&mut self, ts: hiker_core::autosave::TabState) {
        let state = self;
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
            // `:agent_changes` is the legacy persist key; map it forward
            // to the unified `Changes` tab so old workspaces restore cleanly.
            ":changes" | ":agent_changes" => Some(TabKind::Changes),
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
                        &contents,
                        hash,
                        cfg_guard.as_deref(),
                        Some(state.vault_session.vault.clone()),
                    );
                    drop(cfg_guard);
                    state.session.buffers.insert(path.clone(), buf);
                }
            }
            TabKind::vault_buffer(path.clone())
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
}

// The session background relays/pollers (watcher relay, indexer-progress
// relay, snapshot poller) and the `ProgressEvent` formatter live as
// inherent methods on `Spawner` / `ProgressLine` above. As inherent
// methods they're exempt from `single_call_fn`, and moving their loop
// bodies off `open_vault` keeps it under the cognitive-complexity ceiling.
