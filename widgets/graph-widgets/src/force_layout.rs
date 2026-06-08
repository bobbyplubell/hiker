//! egui-facing adapter over `hiker_graph`'s ForceAtlas2 background worker.
//!
//! The layout math (Barnes–Hut quadtree, FR forces, freeze-on-converge) and
//! the worker thread itself live in the egui-agnostic `hiker-graph` crate,
//! which carries its own `Vec2`. This module wraps [`hiker_graph::LayoutWorker`]
//! so callers keep handing node positions in and out as `egui::Vec2`; we
//! convert at the boundary. The vector-free [`hiker_graph::LayoutParams`] is
//! used unchanged — callers import it from `hiker_graph` directly.

use eframe::egui::Vec2;
use hiker_graph::LayoutParams;

#[inline]
const fn to_hiker(v: Vec2) -> hiker_graph::Vec2 {
    hiker_graph::Vec2::new(v.x, v.y)
}

#[inline]
const fn from_hiker(v: hiker_graph::Vec2) -> Vec2 {
    Vec2::new(v.x, v.y)
}

/// Background ForceAtlas2 layout worker — an `egui::Vec2` façade over
/// [`hiker_graph::LayoutWorker`].
pub struct LayoutWorker {
    inner: hiker_graph::LayoutWorker,
}

impl LayoutWorker {
    /// Spawn the worker over `initial` node positions and an `edges` list.
    pub fn spawn(initial: Vec<Vec2>, edges: Vec<(u32, u32)>, params: LayoutParams) -> Self {
        let initial = initial.into_iter().map(to_hiker).collect();
        Self {
            inner: hiker_graph::LayoutWorker::spawn(initial, edges, params),
        }
    }

    /// Spawn the worker like [`spawn`](Self::spawn), but with **anchor
    /// springs**: `anchors[i] == Some(p)` tethers node `i` toward `p` (in the
    /// same world space as `initial`); `None` lets it settle freely. The
    /// `egui::Vec2` inputs are converted at the boundary and delegated to
    /// [`hiker_graph::LayoutWorker::spawn_anchored`]. Anchors are honoured only
    /// when `params.anchor_stiffness > 0.0`.
    pub fn spawn_anchored(
        initial: Vec<Vec2>,
        edges: Vec<(u32, u32)>,
        params: LayoutParams,
        anchors: Vec<Option<Vec2>>,
    ) -> Self {
        let initial = initial.into_iter().map(to_hiker).collect();
        let anchors = anchors.into_iter().map(|a| a.map(to_hiker)).collect();
        Self {
            inner: hiker_graph::LayoutWorker::spawn_anchored(initial, edges, params, anchors),
        }
    }

    /// Copy the worker's current positions into `out` (cleared, then refilled
    /// in node-index order).
    pub fn snapshot_into(&self, out: &mut Vec<Vec2>) {
        let mut tmp = Vec::with_capacity(out.len());
        self.inner.snapshot_into(&mut tmp);
        out.clear();
        out.extend(tmp.into_iter().map(from_hiker));
    }

    /// True while the worker thread is still iterating (not yet converged).
    pub fn is_running(&self) -> bool {
        self.inner.is_running()
    }

    /// Iterations completed so far (for status display).
    pub fn iters_done(&self) -> u32 {
        self.inner.iters_done()
    }
}
