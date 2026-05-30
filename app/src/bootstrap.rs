//! Vault-open recipe. Constructs `AppState` from a vault path.
//!
//! Wires the long-lived subsystems hiker needs to be useful (store,
//! op log, trees, activity, watcher, indexer, autosave). The direct LLM
//! worker + MCP server layer on top.
//!
//! Spawned background tasks (watcher relay, indexer progress forwarder,
//! direct LLM worker) all clone the `VaultSession.cancel` token so the
//! vault-swap path can shut them down before the new session lands —
//! see `state::VaultSession` and `main::update`'s vault-switch branch.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use hiker_core::activity::Activity;
use hiker_core::audit::AgentLog;
use hiker_core::autosave::Autosave;
use hiker_core::config::Config;
use hiker_core::embed::FastembedEmbedder;
use hiker_core::indexer::{route_watcher_events, start, Handle};
use hiker_core::oplog::OpLog;
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
            // status: inbox-rules
            P::InboxApplied {
                rule_index,
                original_path,
                final_path,
                moved_to,
                tagged,
            } => {
                let mut bits = Vec::new();
                if let Some(dest) = moved_to {
                    bits.push(format!("moved to {dest}"));
                }
                if let Some(tag) = tagged {
                    bits.push(format!("tagged #{tag}"));
                }
                format!(
                    "inbox rule {rule_index} on {original_path}: {} (now at {final_path})",
                    bits.join(", "),
                )
            }
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
pub(crate) struct Spawner {
    pub(crate) cancel: CancellationToken,
}

/// The already-spawned relay channel ends the UI drains each frame:
/// filesystem events, indexer progress lines, and note-mutation
/// outcomes (both the receiver and the sender clone handed to the UI).
struct RelayChannels {
    fs_rx: tokio::sync::mpsc::UnboundedReceiver<hiker_core::watcher::FileEvent>,
    ev_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut_rx: tokio::sync::mpsc::UnboundedReceiver<MutationEvent>,
    mut_tx: tokio::sync::mpsc::UnboundedSender<MutationEvent>,
    sync_rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    /// Receiver for on-demand fork-diff fetch results (`sync-fork-diff`):
    /// `(path, Ok(their_text) | Err(message))`, drained each frame into the
    /// Sync page's fork-diff cache. Held even when sync is disabled (the channel
    /// just stays empty), like `sync_rx`.
    fork_diff_rx: tokio::sync::mpsc::UnboundedReceiver<crate::sync_service::ForkDiffResult>,
}

/// Inputs for `Spawner::snapshot_poller`: the read-side handles to poll
/// plus the watch senders the UI reads.
struct SnapshotChannels {
    tasks: Arc<TaskQueue>,
    read_store: Arc<Mutex<Store>>,
    task_snap_tx: watch::Sender<Vec<TaskRecord>>,
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

    /// Set up the watcher fan-out for a vault session: the app-state relay
    /// (returns the `fs_rx` the UI drains) plus the op-log external-edit
    /// reconciliation relay. Both subscribe to the same broadcast and are
    /// torn down on `cancel`. Bundled so `open_vault` stays a single call
    /// site and the relay-loop complexity counts toward `Spawner`.
    fn watcher_relays(
        &self,
        watcher: &Arc<Watcher>,
        oplog: Arc<hiker_core::oplog::OpLog>,
        vault: Arc<hiker_core::vault::Vault>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<hiker_core::watcher::FileEvent> {
        let fs_rx = self.watcher_relay(watcher.subscribe());
        self.oplog_external_sync_relay(watcher.subscribe(), oplog, vault);
        fs_rx
    }

    /// External-edit-sync relay (`op-log-external-edit-sync`). Subscribes to
    /// the watcher and, for every `.md` Created/Modified event hiker didn't
    /// initiate (the watcher already drops self-writes via
    /// `watcher-suppress-self-writes` before broadcasting), reconciles the
    /// new disk bytes into `accepted` through `core::ops::op_writes`. The
    /// substrate compares disk against `materialize(accepted)`: equal →
    /// ignored as a self-write echo (the safety net); different → applied as
    /// an `author=external` text delta. Mirrors `watcher_relay`'s shape; one
    /// spawned task per vault session, torn down on `cancel`.
    fn oplog_external_sync_relay(
        &self,
        mut sub: tokio::sync::broadcast::Receiver<hiker_core::watcher::FileEvent>,
        oplog: Arc<hiker_core::oplog::OpLog>,
        vault: Arc<hiker_core::vault::Vault>,
    ) {
        use hiker_core::watcher::FileEvent;
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    recv = sub.recv() => match recv {
                        Ok(FileEvent::Created { path } | FileEvent::Modified { path }) => {
                            if !hiker_core::indexer::is_indexable_path(&path) {
                                continue;
                            }
                            match hiker_core::ops::op_writes::external_edit(&oplog, &vault, &path) {
                                Ok(true) => tracing::debug!(%path, "external-edit-sync: reconciled disk change"),
                                Ok(false) => {}
                                Err(e) => tracing::warn!(%path, error = %e, "external-edit-sync failed"),
                            }
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        // Lagged: events were dropped. A missed external edit
                        // is caught by the editor's pre-write drift check; no
                        // forced reconcile needed here.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    },
                }
            }
        });
    }

    /// Build the live sync engine and spawn its responder loop, when
    /// `[sync].enabled`. Mirrors `oplog_external_sync_relay`'s spawn shape:
    /// one tokio task per vault session, torn down on `cancel`. The task
    /// `listen`s on the configured/default addr, records the bound address,
    /// then drives the swarm event loop as a responder (answering enrolled
    /// peers) in short windows so the shared node lock is released between
    /// turns and a UI-spawned `force_sync`/`discover` can interleave.
    ///
    /// Returns the constructed service (stored on `Services`), or `None` when
    /// sync is disabled or the key store can't be opened — both non-fatal.
    pub(crate) fn spawn_sync_service(
        &self,
        vault_root: &std::path::Path,
        oplog: Arc<hiker_core::oplog::OpLog>,
        section: &hiker_core::config::sections::SyncSection,
        sync_tx: tokio::sync::mpsc::UnboundedSender<String>,
        fork_diff_tx: crate::sync_service::ForkDiffSender,
    ) -> Option<Arc<crate::sync_service::SyncService>> {
        if !section.enabled {
            return None;
        }
        let service = match crate::sync_service::SyncService::new(
            vault_root,
            oplog,
            section,
            sync_tx,
            fork_diff_tx,
        ) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::warn!(error = %e, "sync: failed to build service (non-fatal)");
                return None;
            }
        };

        let node = service.node();
        let events_tx = service.events_tx();
        let svc = service.clone();
        let cancel = self.cancel.clone();
        // Per-service kill switch (the live `[sync].enabled = false` path),
        // in addition to the session-wide cancel (vault switch / close).
        let svc_cancel = service.cancel_token();
        tokio::spawn(async move {
            // Start listening; resolve the OS-assigned port.
            {
                let mut node = node.lock().await;
                match node
                    .listen(crate::sync_service::DEFAULT_LISTEN_ADDR)
                    .await
                {
                    Ok(addr) => {
                        let _ = events_tx.send(format!("sync: listening on {addr}"));
                        svc.set_listen_addr(addr);
                    }
                    Err(e) => {
                        let _ = events_tx.send(format!("sync: listen failed — {e}"));
                        return;
                    }
                }
            }

            // Auto-sync driver, folded into the responder loop so it never
            // fights the responder for the node lock — both interleave by
            // yielding the lock between turns. Three triggers fire a round:
            //   * startup: one round shortly after `listen` succeeds, so a
            //     device that just came online catches up immediately.
            //   * periodic: the `AUTO_SYNC_INTERVAL` tick.
            //   * on-discovery: a new enrolled peer surfaced via mDNS in the
            //     responder window (`take_newly_discovered`).
            // `[sync].enabled` already gated building the whole task; the
            // cancel tokens below stop it (kill switch / vault swap). Rounds
            // run when server-mode is usable, or LAN discovery is on; with
            // discovery off and no server, the driver still ticks but every
            // round is a benign no-op (no known peers → `Ok(None)`, silent).
            // ~15s: cheap (manifest/state-vector exchange, deltas only) and
            // quiet on empty rounds; a config knob is a future nicety.
            const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(15);
            // Startup delay: let the listener settle + give mDNS a beat to
            // surface a peer before the first round.
            const STARTUP_DELAY: Duration = Duration::from_secs(2);
            let mut interval = tokio::time::interval(AUTO_SYNC_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The interval's first tick fires immediately; consume it so the
            // *periodic* arm doesn't double up with the explicit startup round.
            interval.tick().await;
            let startup = tokio::time::sleep(STARTUP_DELAY);
            tokio::pin!(startup);
            let mut did_startup = false;

            // Responder loop: drive the swarm in short windows, releasing the
            // node lock between turns so UI-spawned dialer work and the
            // auto-sync rounds below can take it.
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = svc_cancel.cancelled() => break,
                    _ = &mut startup, if !did_startup => {
                        did_startup = true;
                        svc.auto_sync_round().await;
                    }
                    _ = interval.tick() => {
                        svc.auto_sync_round().await;
                    }
                    _ = async {
                        let newly_discovered = {
                            let mut node = node.lock().await;
                            // Short window so the node lock is held only briefly
                            // between turns — a UI-spawned action that needs the
                            // lock (enroll / import / resolve) waits at most this
                            // long, not half a second.
                            if let Err(e) = node.run(Duration::from_millis(120)).await {
                                tracing::warn!(error = %e, "sync: responder run window failed");
                            }
                            // Mirror the node-derived state the page renders
                            // (enrolled LAN peers + blocked docs) into `Shared`
                            // while we hold the lock (cheap sync reads), so the
                            // egui render path never locks the node itself.
                            svc.fold_discovered(node.discovered_peers());
                            svc.fold_blocked(node.blocked_docs());
                            // Mirror the unenrolled-seen LAN peers for the page,
                            // and emit a one-time log line per newly-seen one so
                            // the user sees a hiker instance is reachable but
                            // needs enrolling. [sync-mdns-discovery]
                            svc.fold_seen_unenrolled(node.seen_unenrolled());
                            for peer in node.take_newly_seen_unenrolled() {
                                let _ = events_tx.send(format!(
                                    "sync: discovered un-enrolled peer {peer} on LAN — \
                                     enroll its fingerprint to sync"
                                ));
                            }
                            node.take_newly_discovered()
                        };
                        // Yield so a waiting dialer can grab the lock between
                        // the responder window and any on-discovery round.
                        tokio::task::yield_now().await;
                        if newly_discovered {
                            // A new enrolled peer just appeared — sync with it
                            // promptly rather than waiting for the next tick.
                            svc.auto_sync_round().await;
                        }
                    } => {}
                }
            }
        });

        Some(service)
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

    /// Snapshot pollster: tasks every 200ms; the read-store-locked
    /// skipped-paths query every ~3s. First tick fires immediately so the
    /// very first UI frame sees a populated cache. Exits on `cancel`. (The
    /// pending-op badge feed reads the op log directly each frame in
    /// `main::refresh_pending_proposals` — it isn't polled here.)
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
        read_store: &Arc<Mutex<Store>>,
        relays: RelayChannels,
    ) -> VaultEvents {
        let (task_snap_tx, task_snap_rx) = watch::channel::<Vec<TaskRecord>>(Vec::new());
        let (skipped_snap_tx, skipped_snap_rx) = watch::channel::<HashSet<String>>(HashSet::new());

        self.snapshot_poller(SnapshotChannels {
            tasks: tasks.clone(),
            read_store: read_store.clone(),
            task_snap_tx,
            skipped_snap_tx,
        });

        VaultEvents {
            fs_events: Mutex::new(relays.fs_rx),
            indexer_events_rx: Mutex::new(relays.ev_rx),
            mutation_events: Mutex::new(relays.mut_rx),
            mutation_events_tx: relays.mut_tx,
            indexer_events: VecDeque::new(),
            sync_events_rx: Mutex::new(relays.sync_rx),
            sync_events: VecDeque::new(),
            fork_diff_rx: Mutex::new(relays.fork_diff_rx),
            task_snapshot_rx: task_snap_rx,
            skipped_paths_rx: skipped_snap_rx,
        }
    }
}

/// Open the vault's op log and seed it from the on-disk notes on first
/// open. The op log is the CRDT write substrate every producer rides on
/// (`op-log-ops-producer-helpers`); the seed (`op-log-doc-id-bootstrap`)
/// mints a doc per existing note and is idempotent — already-mapped notes
/// are skipped — so subsequent opens are a cheap walk. The compaction
/// threshold comes from `[op-log] compact_threshold`. A bootstrap seed
/// failure is non-fatal (logged) so a single unreadable note can't block
/// the whole vault opening.
fn open_and_seed_oplog(
    root: &std::path::Path,
    vault: &Vault,
    config: &std::sync::RwLock<Config>,
) -> Result<Arc<OpLog>> {
    let compact_threshold = config
        .read()
        .map(|c| c.op_log.compact_threshold)
        .unwrap_or(4.0);
    let oplog = Arc::new(
        OpLog::open_with_threshold(root, compact_threshold).with_context(|| "open op log")?,
    );
    match hiker_core::ops::op_writes::bootstrap(vault, &oplog) {
        Ok(n) if n > 0 => tracing::info!(seeded = n, "oplog: seeded documents on first open"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "oplog: bootstrap seed failed (non-fatal)"),
    }
    run_oplog_retention_gc_on_open(&oplog, config);
    Ok(oplog)
}

/// On-open retention GC (`op-log-retention`): drop accepted/rejected
/// side-table rows past their `[op-log]` retention horizons. Compaction of
/// the `.yrs` snapshots already ran inside `OpLog::open_with_threshold`, so
/// this only covers the side-table sweep. Failures are logged, not fatal —
/// a vault still opens with stale metadata rows.
fn run_oplog_retention_gc_on_open(oplog: &Arc<OpLog>, config: &std::sync::RwLock<Config>) {
    let (meta_days, rejected_days) = config
        .read()
        .map(|c| {
            (
                c.op_log.metadata_retention_days,
                c.op_log.rejected_retention_days,
            )
        })
        .unwrap_or((365, 14));
    match hiker_core::ops::op_writes::run_retention_gc(oplog, meta_days, rejected_days) {
        Ok((a, r)) if a + r > 0 => {
            tracing::info!(accepted_dropped = a, rejected_dropped = r, "oplog: retention GC on open")
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "oplog: retention GC failed (non-fatal)"),
    }
}

/// Load the persisted session state for a freshly-opened vault: crash-recovery
/// entries, the saved tab snapshot, and the trails list. Any missing or
/// malformed file degrades to an empty value so a fresh vault still opens
/// cleanly.
fn load_persisted_session(
    autosave: &Autosave,
    root: &std::path::Path,
) -> (
    Vec<hiker_core::autosave::RecoveredEntry>,
    Option<hiker_core::autosave::TabState>,
    Vec<crate::state::Trail>,
) {
    let recovery_entries = autosave.recover().unwrap_or_default();
    let tab_state = autosave.load_tab_state().ok().flatten();
    // Read `<root>/.hiker/trails.json`; any failure (missing file / bad JSON)
    // becomes an empty list.
    let trails = {
        let path = root.join(".hiker/trails.json");
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<crate::state::Trail>>(&bytes).ok())
            .unwrap_or_default()
    };
    (recovery_entries, tab_state, trails)
}

/// Build the plugin host for a vault and load its enabled, hash-pinned
/// plugins (per-plugin failures logged, not fatal). The host API reads through
/// the shared index store so `notes.query` hits the same structured index as
/// the rest of the app.
fn build_plugin_host(
    read_store: &Arc<Mutex<Store>>,
    vault_root: &std::path::Path,
) -> hiker_core::plugins::PluginHost {
    let mut host = hiker_core::plugins::PluginHost::with_wasmi(Arc::new(
        hiker_core::plugins::dispatch::StoreHostApi {
            store: read_store.clone(),
        },
    ));
    for (id, err) in host.load_enabled(vault_root) {
        tracing::warn!(plugin = %id, error = %err, "plugin failed to load");
    }
    host
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

    // Long-lived subsystems. The read store is shared by the indexer and
    // any read-side commands; everything lives under `.hiker/`.
    let read_store = Arc::new(Mutex::new(
        Store::open(&root).with_context(|| "open read store")?,
    ));

    let oplog = open_and_seed_oplog(&root, &vault, &config)?;

    let trees = Arc::new(
        Db::new(oplog.clone(), vault.clone()).with_context(|| "open trees store")?,
    );
    // Activity feed projects over the op log: accepted ops → change rows,
    // pending ops → pending proposal rows. The op log is the sole changelog
    // substrate.
    let activity = Arc::new(Activity::new(oplog.clone()));
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
    // Let the indexer's move / delete jobs record renames + tombstones in the
    // op log so `doc-index.db` follows file moves and deletes reach history.
    indexer.attach_oplog(oplog.clone());
    // status: inbox-rules
    // Compile the [inbox] rule list once at vault open and hand it to the
    // indexer; the Created-event hook applies rules before the upsert.
    // Strict-load already validated the rules in Config::load above, so
    // compile here is expected to succeed; log + skip on the off chance it
    // doesn't (e.g. drift between validate / compile).
    let inbox_rules_src = config
        .read()
        .map(|c| c.inbox.rules.clone())
        .unwrap_or_default();
    match hiker_core::inbox::Rules::compile(&inbox_rules_src) {
        Ok(rules) => indexer.attach_inbox_rules(Arc::new(rules)),
        Err(e) => tracing::error!(error = %e, "inbox: rule compile failed; rules disabled"),
    }

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

    // Watcher fan-out: app-state relay + op-log external-edit reconciliation.
    let fs_rx = spawner.watcher_relays(&watcher, oplog.clone(), vault.clone());

    // Indexer progress forwarder.
    let indexer_arc = Arc::new(indexer);
    let ev_rx = spawner.indexer_progress_relay(indexer_arc.subscribe_progress());

    // Channel for note-mutation outcomes.
    let (mut_tx, mut_rx) =
        tokio::sync::mpsc::unbounded_channel::<MutationEvent>();

    // Channel for sync progress lines. Created up-front (even when sync is
    // disabled) so `VaultEvents` always holds both ends; the tx is also handed
    // to the sync service so its async tasks can push.
    let (sync_tx, sync_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Channel for on-demand fork-diff fetch results (`sync-fork-diff`). Created
    // up-front like `sync_tx`; the tx is handed to the sync service so its
    // fetch task can deliver `(path, Ok(text) | Err(msg))` back to the UI.
    let (fork_diff_tx, fork_diff_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::sync_service::ForkDiffResult>();

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
    // status: mcp-tool-get-active-note, mcp-tool-get-open-notes,
    // mcp-tool-get-selection — shared UI-context snapshot the MCP read
    // tools consume. Refreshed each frame by `refresh_ui_context_snapshot`.
    let mcp_ui_context = hiker_mcp::ui_context::shared_empty();
    let mcp = McpStarter.start(mcp_cfg.enabled, hiker_mcp::McpDeps {
        vault: (*vault).clone(),
        vault_root: root.clone(),
        read_store: read_store.clone(),
        jobs: indexer_arc.job_sender(),
        watcher: watcher.clone(),
        embedder_provider: indexer_arc.embedder_provider(),
        config: mcp_cfg.clone(),
        tools: mcp_tools_cfg.clone(),
        audit: audit.clone(),
        tasks: tasks.clone(),
        tasks_config: tasks_cfg,
        boards_config: config.read().map(|c| c.boards.clone()).unwrap_or_default(),
        llm_enabled: config.read().map(|c| c.llm.enabled).unwrap_or(false),
        oplog: Some(oplog.clone()),
        ui_context: mcp_ui_context.clone(),
    }).await;

    // Crash recovery, persisted tab snapshot, and trails for the new Session.
    let (recovery_entries, tab_state, trails) = load_persisted_session(&autosave, &root);

    // Live sync engine: built + responder-spawned only when `[sync].enabled`.
    // When disabled, `None` — no keys, no swarm, no listener. The `sync_tx`
    // end is also stashed in `VaultEvents` so the UI drains the progress ring.
    let sync_section = config.read().map(|c| c.sync.clone()).unwrap_or_default();
    let sync = spawner.spawn_sync_service(
        &root,
        oplog.clone(),
        &sync_section,
        sync_tx.clone(),
        fork_diff_tx.clone(),
    );

    // Wire the UI `watch` channels + snapshot pollster and fold them in
    // with the relay channels (a `Spawner` method, so its wiring counts
    // toward `Spawner`, not `open_vault`).
    let events = spawner.build_vault_events(
        &tasks,
        &read_store,
        RelayChannels { fs_rx, ev_rx, mut_rx, mut_tx, sync_rx, fork_diff_rx },
    );
    // `sync_tx` / `fork_diff_tx` were handed to the sync service (when enabled)
    // above; drop our copies so each channel closes cleanly if no service holds
    // a sender.
    drop(sync_tx);
    drop(fork_diff_tx);

    // Plugin host: load the enabled, hash-pinned WASM plugins for this vault.
    let plugins = build_plugin_host(&read_store, &root);

    // Assemble the compartments.
    let services = Services {
        read_store,
        oplog,
        trees,
        activity,
        autosave,
        watcher,
        indexer: indexer_arc,
        audit,
        tasks,
        mcp,
        mcp_tools_cfg,
        mcp_ui_context,
        sync,
    };
    let vault_session = VaultSession {
        vault: vault.clone(),
        vault_root: root.clone(),
        config: config.clone(),
        services,
        events,
        cancel,
        plugins,
    };

    // Recovered autosave buffers restore silently (no modal) once `AppState`
    // exists — see the `auto_restore_recovered` call below, before the
    // tab-state restore. `autosave-recovery-auto-restore`.
    let session = Session {
        modal: None,
        ..Session::default()
    };

    // Seed the search surface from persisted vault settings (mode
    // toggles, lexical/semantic tuning, section collapse); everything
    // else on `PanelStates`/the feature states defaults. Read the config
    // guard once here — `with_config` is a no-op fallback if it's
    // poisoned. [feature-search-migration]
    let search_state = match config.read().ok().as_deref() {
        Some(c) => crate::search::state::State::default().with_config(c),
        None => crate::search::state::State::default(),
    };

    let mut state = AppState {
        vault_session,
        session,
        file_tree_state: crate::state::FileTreeState::default(),
        ui_cache: UiCache::default(),
        panels: PanelStates::default(),
        clusters_state: crate::clusters::state::State::default(),
        trails_state: crate::trails::state::State {
            trails,
            ..crate::trails::state::State::default()
        },
        backlinks_state: crate::backlinks::State::default(),
        related_state: crate::related::State::default(),
        search_state,
        vault_state: crate::vault_view::State::default(),
        trash_state: crate::trash::State,
        chat_state: crate::chat::state::State::default(),
        // Per-vault feature registry: built-ins (Clusters in v1) plus
        // (Phase 3) plugin-derived features. `feature-registry`.
        features: crate::feature::Registry::build(crate::feature::builtin_features()),
        ui: UiState::default(),
        toasts: Vec::new(),
        vault_switch: VaultSwitchState::Idle,
        workbench: {
            // Inlined `new_workbench`: fresh workbench with the default
            // activity (`Files`) open in the splittable primary side
            // region, the Chat (secondary) side bar visible, and the
            // global bottom status strip on.
            let mut wb = egui_workbench::workspace::Workbench::default();
            wb.open_primary_panel("files".to_string());
            wb.secondary_side_bar.visible = true;
            wb.status_bar.visible = true;
            wb
        },
    };

    // Silently auto-restore recovered autosave buffers as dirty sticky tabs
    // (no modal), then restore the persisted tab state. Recovered buffers open
    // first so `tab_state.active_path` still wins when resolvable, per
    // `autosave-tab-state-silent-restore`.
    if !recovery_entries.is_empty() {
        crate::widgets::modal::auto_restore_recovered(&mut state, recovery_entries);
    }
    if let Some(ts) = tab_state {
        state.restore_tab_state(ts);
    }

    // Restore the persisted primary side-panel accordion (open sections,
    // collapse, weights, focus, visibility). No-op on a fresh vault —
    // the default single `Files` section set above stands.
    crate::side_panel_persist::restore(&mut state, &root);

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

    state.ui.toolbars = load_toolbar_layout(&state.vault_session.vault_root);

    Ok(state)
}

/// Load the data-driven toolbar layout from `.hiker/toolbars.json`,
/// falling back to the default when the file is missing or malformed.
fn load_toolbar_layout(vault_root: &Path) -> crate::state::Toolbars {
    let path = vault_root.join(".hiker/toolbars.json");
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<crate::state::Toolbars>(&bytes).unwrap_or_else(
            |err| {
                tracing::warn!(error = %err, "toolbars: parse failed; using default");
                crate::state::Toolbars::default()
            },
        ),
        Err(_) => crate::state::Toolbars::default(),
    }
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
            // status: board-view
            // Per-doc board tab: the persist key is `board:<doc-path>`.
            p if p.starts_with("board:") => {
                Some(TabKind::Board { path: p["board:".len()..].to_string() })
            }
            ":patch_review" => Some(TabKind::PatchReview),
            // status: board-index-page
            ":boards_index" => Some(TabKind::BoardsIndex),
            ":plugins" => Some(TabKind::Plugins),
            ":indexer" => Some(TabKind::IndexerDetail),
            ":sync" => Some(TabKind::Sync),
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
