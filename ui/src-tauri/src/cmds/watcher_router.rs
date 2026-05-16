//! Single-dispatcher fan-out for `core::watcher::FileEvent`.
//!
//! The watcher itself stays a clean primitive (a `broadcast::Sender`). At
//! vault open we used to call `watcher.subscribe()` once per consumer and
//! spawn an independent task per consumer, each with its own filter/match
//! on event kind. That worked but every new "react when a file changes"
//! feature added another `tokio::spawn` + match block to `bootstrap.rs`.
//!
//! `WatcherRouter` consolidates the fan-out. It takes one
//! `broadcast::Receiver` and a set of registered handlers. A single task
//! reads the broadcast and, for each event, dispatches to each handler
//! whose `wants` filter matches via a per-handler bounded mpsc. Each
//! handler runs in its own task draining its mpsc — slow handlers don't
//! block fast ones, exactly mirroring the property the previous
//! per-`subscribe()` design had, just unified.
//!
//! Handlers are registered as closures. The previous code's per-feature
//! `spawn_X` helpers were already closures over Arc state; keeping the
//! same shape on the router-side avoids inventing a trait + impl ladder
//! for a small fixed set of subscribers.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use hiker_core::watcher::FileEvent;

/// Per-handler queue depth. The watcher broadcast is sized 1024; an
/// individual handler is unlikely to need a deeper buffer than that, and
/// keeping this small bounds memory if a handler stalls. On overflow we
/// drop the event (with a warn) — the broadcast's own lag-recovery story
/// is what each previous independent subscriber relied on anyway.
const HANDLER_QUEUE_DEPTH: usize = 256;

type WantsFn = dyn Fn(&FileEvent) -> bool + Send + Sync + 'static;

/// One registered fan-out subscriber. Owns the sending half of its
/// bounded mpsc; the handler task owns the receiving half and was
/// spawned at `add()` time.
struct Subscriber {
    name: &'static str,
    wants: Arc<WantsFn>,
    tx: mpsc::Sender<FileEvent>,
}

/// Fan-out router. Build with `WatcherRouter::new()`, register handlers
/// with `add()`, then call `start(rx)` to spawn the dispatch task. The
/// returned join handle is intentionally dropped at the call site — the
/// task ends when the broadcast is closed (vault swap drops the watcher).
pub(crate) struct WatcherRouter {
    subscribers: Vec<Subscriber>,
}

impl WatcherRouter {
    pub(crate) fn new() -> Self {
        Self { subscribers: Vec::new() }
    }

    /// Register a handler. `wants` is the per-event filter (return true
    /// to receive the event). `handler` is the async fn that processes
    /// each event; it runs in its own task draining a bounded mpsc, so
    /// it owns its own backpressure.
    pub(crate) fn add<W, H, F>(&mut self, name: &'static str, wants: W, mut handler: H)
    where
        W: Fn(&FileEvent) -> bool + Send + Sync + 'static,
        H: FnMut(FileEvent) -> F + Send + 'static,
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<FileEvent>(HANDLER_QUEUE_DEPTH);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                handler(ev).await;
            }
        });
        self.subscribers.push(Subscriber {
            name,
            wants: Arc::new(wants),
            tx,
        });
    }

    /// Start the dispatch task. Reads from `rx` and fans each event out
    /// to every registered handler whose `wants` matches. Returns
    /// immediately; the task ends when the broadcast channel closes.
    pub(crate) fn start(self, mut rx: broadcast::Receiver<FileEvent>) {
        let subscribers = self.subscribers;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        for sub in &subscribers {
                            if !(sub.wants)(&ev) {
                                continue;
                            }
                            // try_send so a stalled handler can't block
                            // the dispatch loop (or other handlers). On
                            // full / closed we log and drop.
                            if let Err(e) = sub.tx.try_send(ev.clone()) {
                                tracing::warn!(
                                    handler = sub.name,
                                    error = %e,
                                    "watcher-router: dropping event for backed-up handler",
                                );
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Match the previous per-subscriber behavior:
                        // independent subscribers each just `continue`d
                        // on lag. The indexer path (still owned by
                        // `route_watcher_events`) has its own
                        // FullScan-on-lag recovery.
                        tracing::warn!(
                            dropped = n,
                            "watcher-router: broadcast lag — events dropped",
                        );
                        continue;
                    }
                }
            }
        });
    }
}
