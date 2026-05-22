//! Streaming entry point + cancellation/event plumbing for the
//! structural clustering pass.
//!
//! `StreamCtx` is the shared per-build context threaded through the
//! recursive split. Blocking entry points use `StreamCtx::noop()`
//! (events dropped, cancellation atomic never flipped); the async
//! entry `build_tree_structural_streaming` constructs one with a real
//! mpsc sender and the caller's cancel atomic.
//!
//! status: cluster-build-async-pass
//! status: cluster-build-progress-stream

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{NoopSummarizer, build_cluster_tree, build_from_folders};
use crate::cluster::{
    BuildError, BuildEvent, BuildMethod, BuiltClusterNode, BuiltClusterTree, Id,
    NoteInput, Phase, SummarizeMode,
};

/// Periodic cancellation-check cadence inside the partition loop. Per
/// `cluster-build-async-pass`: "checked at every level boundary and on a
/// periodic per-node interval inside the partition loop." 64 is small
/// enough that even a few-thousand-node vault reacts to Cancel within
/// tens of milliseconds, and large enough to keep the atomic load out
/// of the hot inner loop.
pub(super) const PARTITION_CHECK_INTERVAL: u32 = 64;

/// Streaming context threaded through the recursive build.
pub(in crate::cluster) struct StreamCtx {
    pub(super) tx: Option<tokio::sync::mpsc::Sender<BuildEvent>>,
    pub(super) cancel: Arc<AtomicBool>,
    pub(super) items_processed: u32,
    pub(super) clusters_found: u32,
    pub(super) outliers: u32,
    pub(super) partition_loop_counter: u32,
    pub(super) max_partition_level_emitted: i32,
}

impl StreamCtx {
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub(super) fn check_cancel(&self) -> Result<(), BuildError> {
        if self.is_cancelled() {
            Err(BuildError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(super) fn check_cancel_periodic(&mut self) -> Result<(), BuildError> {
        self.partition_loop_counter = self.partition_loop_counter.wrapping_add(1);
        if self.partition_loop_counter >= PARTITION_CHECK_INTERVAL {
            self.partition_loop_counter = 0;
            self.check_cancel()?;
        }
        Ok(())
    }

    pub(super) fn emit(&self, event: BuildEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.blocking_send(event);
        }
    }

    pub(super) fn emit_phase(&self, phase: Phase) {
        self.emit(BuildEvent::Phase { phase });
    }

    pub(super) fn emit_counters(&self) {
        self.emit(BuildEvent::Counters {
            items_processed: self.items_processed,
            clusters_found: self.clusters_found,
            outliers: self.outliers,
        });
    }

    pub(super) fn emit_partition_phase_if_new(&mut self, depth: u32) {
        if (depth as i32) > self.max_partition_level_emitted {
            self.max_partition_level_emitted = depth as i32;
            self.emit_phase(Phase::PartitioningLevel(depth));
        }
    }

    pub(super) fn emit_cluster(&mut self, node: BuiltClusterNode, parent: Option<Id>) {
        self.clusters_found = self.clusters_found.saturating_add(1);
        self.emit(BuildEvent::ClusterDiscovered { node, parent });
    }
}

/// Async entry — spawns the structural pass on `spawn_blocking` and
/// returns the join handle + an mpsc receiver the producer drains.
pub fn build_tree_structural_streaming(
    method: BuildMethod,
    notes: Vec<NoteInput>,
    cancel: Arc<AtomicBool>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<BuildEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<BuildEvent>(64);
    let handle = tokio::task::spawn_blocking(move || {
        let forced_method = match method {
            BuildMethod::Cluster { mut params } => {
                params.summarize = SummarizeMode::None;
                BuildMethod::Cluster { params }
            }
            BuildMethod::FromFolders { mut params } => {
                params.summarize = SummarizeMode::None;
                BuildMethod::FromFolders { params }
            }
        };

        let mut sctx = StreamCtx {
            tx: Some(tx.clone()),
            cancel: cancel.clone(),
            items_processed: 0,
            clusters_found: 0,
            outliers: 0,
            partition_loop_counter: 0,
            max_partition_level_emitted: -1,
        };
        sctx.emit_phase(Phase::LoadingEmbeddings);
        sctx.emit_counters();

        if notes.is_empty() {
            sctx.emit(BuildEvent::Failed {
                error: BuildError::EmptyScope.to_string(),
            });
            return;
        }

        if sctx.is_cancelled() {
            sctx.emit(BuildEvent::Cancelled);
            return;
        }

        let result: Result<BuiltClusterTree, BuildError> = match &forced_method {
            BuildMethod::Cluster { params } => {
                build_cluster_tree(&notes, params, &NoopSummarizer, &mut sctx)
            }
            BuildMethod::FromFolders { params } => {
                if sctx.check_cancel().is_err() {
                    sctx.emit(BuildEvent::Cancelled);
                    return;
                }
                sctx.emit_partition_phase_if_new(0);
                let r = build_from_folders(&notes, params, &NoopSummarizer);
                sctx.emit_phase(Phase::Finalizing);
                r
            }
        };

        match result {
            Ok(mut tree) => {
                if let Some(leaf_level) = tree.levels.get_mut(0) {
                    let mut order: Vec<usize> = (0..leaf_level.len()).collect();
                    order.sort_by_key(|&i| std::cmp::Reverse(leaf_level[i].members.len()));
                    let mut next_n: usize = 1;
                    for &i in &order {
                        if leaf_level[i].name.is_empty() {
                            leaf_level[i].name = format!("Cluster {}", next_n);
                            next_n += 1;
                        }
                    }
                }
                let _ = forced_method;
                sctx.emit(BuildEvent::Done { tree });
            }
            Err(BuildError::Cancelled) => {
                sctx.emit(BuildEvent::Cancelled);
            }
            Err(e) => {
                sctx.emit(BuildEvent::Failed {
                    error: e.to_string(),
                });
            }
        }
    });
    (handle, rx)
}
