//! Background-thread force-directed graph layout (ForceAtlas2 model).
//!
//! Runs on a worker thread, publishing position snapshots through an
//! `Arc<RwLock<Vec<Vec2>>>`. Repulsion uses a Barnes–Hut quadtree
//! (O(n log n) per iteration).
//!
//! The force model and convergence mechanism follow ForceAtlas2 — see
//! Jacomy et al. 2014 ("ForceAtlas2, a Continuous Graph Layout Algorithm
//! for Handy Network Visualization") and the canonical implementation
//! in `graphology-layout-forceatlas2`. The earlier port used classic
//! Fruchterman–Reingold with a linear temperature schedule, which
//! oscillated indefinitely on real graphs: once near equilibrium, small
//! flip-flopping forces are indistinguishable from genuine drift, so
//! the global cooling schedule can't stabilize them.
//!
//! FA2's fix is **per-node adaptive damping** computed each step from
//! two scalars derived from the current and previous force vectors:
//!
//! * **swinging**  = |F_prev − F_curr| (large when direction flips)
//! * **traction**  = |F_prev + F_curr| (large when direction is consistent)
//!
//! and `nodespeed = convergence · log(1 + traction) / (1 + √swinging)`.
//! Oscillating nodes (F_prev ≈ −F_curr) get `traction ≈ 0` and
//! therefore `nodespeed ≈ 0` — they literally stop. Nodes drifting
//! steadily get fast nodespeed. This is the only known practical
//! convergence mechanism for force-directed layouts at this scale.
//!
//! Other FA2 ingredients ported here:
//! * Mass-weighted repulsion: `mass(v) = deg(v) + 1`, so hubs repel
//!   harder than leaves and the hub-and-spoke "exploded flower"
//!   pattern collapses into a tight cluster.
//! * Gravity toward origin (configurable strong/weak), so disconnected
//!   components don't drift to infinity.
//! * Outbound-attraction-distribution: edge attraction divided by the
//!   source's mass, so hubs don't get yanked around by their many
//!   leaves.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use eframe::egui::Vec2;

/// Tunable parameters for the ForceAtlas2 layout. The defaults match
/// what graphology-layout-forceatlas2 uses for "Obsidian-style" graph
/// visuals (modest gravity, linear attraction, degree-distributed
/// outbound attraction so hubs don't get dragged around).
pub struct LayoutParams {
    /// Hard clamp on position magnitude per axis. Layouts that escape
    /// this box get pulled back — useful as a safety belt while a
    /// poorly seeded graph is settling.
    pub bound: f32,
    /// Maximum iterations before the worker gives up. FA2 typically
    /// settles in 300–800 steps for graphs in our size range.
    pub max_iters: u32,
    /// Barnes–Hut opening criterion. Larger = faster, less accurate.
    pub theta: f32,
    /// Stop once **global** swinging stays below this for
    /// `convergence_streak` consecutive iterations. Global swinging is
    /// the sum of per-node swinging values; when it's small, the
    /// system has stopped oscillating.
    pub convergence_eps: f32,
    pub convergence_streak: u32,

    // ── FA2 knobs ──────────────────────────────────────────────────
    /// Global multiplier on repulsion and gravity. Default 1.0. Bigger
    /// = more spread-out layout.
    pub scaling_ratio: f32,
    /// Pull toward origin. 0 = none. Default 1.0 produces a compact
    /// layout; raise to keep big graphs from spreading too wide.
    pub gravity: f32,
    /// Strong gravity grows with distance (≈ linear pull). Weak gravity
    /// is constant magnitude regardless of distance. Strong is better
    /// for highly disconnected vaults; weak is gentler.
    pub strong_gravity: bool,
    /// Damping divisor on the applied step. 1.0 = no damping; 5–10 =
    /// safer but slower convergence. Higher slow_down trades speed for
    /// stability when the seed is very far from equilibrium.
    pub slow_down: f32,
    /// LinLog attraction: force ∝ log(1+dist) instead of constant per
    /// edge. Produces sharper cluster separation but takes longer to
    /// converge. Off by default; flip on for "discrete clusters" look.
    pub lin_log: bool,
    /// Divide each edge's attraction by the source's mass. Stops
    /// high-degree hubs from being jerked around by their many leaves.
    pub outbound_attraction_distribution: bool,
    /// Use `deg+1` as the per-node mass in repulsion. Fixes the
    /// hub-and-spoke "exploded flower" pattern by making hubs repel
    /// each other much harder than their satellite leaves repel each
    /// other.
    pub degree_repulsion: bool,
}

impl Default for LayoutParams {
    fn default() -> Self {
        Self {
            bound: 5000.0,
            max_iters: 800,
            theta: 0.9,
            convergence_eps: 0.5,
            convergence_streak: 20,

            scaling_ratio: 100.0,
            gravity: 1.0,
            strong_gravity: false,
            slow_down: 5.0,
            lin_log: false,
            outbound_attraction_distribution: true,
            degree_repulsion: true,
        }
    }
}

pub struct LayoutWorker {
    positions: Arc<RwLock<Vec<Vec2>>>,
    iter_count: Arc<AtomicU32>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl LayoutWorker {
    pub fn spawn(initial: Vec<Vec2>, edges: Vec<(u32, u32)>, params: LayoutParams) -> Self {
        let positions = Arc::new(RwLock::new(initial.clone()));
        let iter_count = Arc::new(AtomicU32::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let pos_arc = positions.clone();
        let iter_arc = iter_count.clone();
        let done_arc = done.clone();
        let stop_arc = stop.clone();

        let handle = thread::Builder::new()
            .name("graph-layout".into())
            .spawn(move || {
                run_layout(initial, edges, params, pos_arc, iter_arc, done_arc, stop_arc);
            })
            .expect("failed to spawn graph-layout thread");

        Self {
            positions,
            iter_count,
            done,
            stop,
            handle: Some(handle),
        }
    }

    /// Copy the worker's current position snapshot into `out`. Resizes
    /// `out` to match. Cheap — the worker writes the same `Vec` shape
    /// every iteration so the RwLock contention is short.
    pub fn snapshot_into(&self, out: &mut Vec<Vec2>) {
        if let Ok(p) = self.positions.read() {
            out.clear();
            out.extend_from_slice(&p);
        }
    }

    pub fn is_running(&self) -> bool {
        !self.done.load(Ordering::Relaxed)
    }

    pub fn iters_done(&self) -> u32 {
        self.iter_count.load(Ordering::Relaxed)
    }
}

impl Drop for LayoutWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn run_layout(
    mut pos: Vec<Vec2>,
    edges: Vec<(u32, u32)>,
    params: LayoutParams,
    pos_arc: Arc<RwLock<Vec<Vec2>>>,
    iter_arc: Arc<AtomicU32>,
    done_arc: Arc<AtomicBool>,
    stop_arc: Arc<AtomicBool>,
) {
    let n = pos.len();
    if n == 0 {
        done_arc.store(true, Ordering::Relaxed);
        return;
    }
    let nf = n as f32;

    // mass = deg + 1 (FA2 convention). Used in repulsion and in
    // outbound-attraction-distribution.
    let mut mass = vec![1.0f32; n];
    if params.degree_repulsion || params.outbound_attraction_distribution {
        for &(a, b) in &edges {
            let (a, b) = (a as usize, b as usize);
            if a < n {
                mass[a] += 1.0;
            }
            if b < n && b != a {
                mass[b] += 1.0;
            }
        }
    }
    // Repulsion weights — only mass-weighted when degree_repulsion is on;
    // attraction-distribution can still use mass below independently.
    let rep_mass: Vec<f32> = if params.degree_repulsion {
        mass.clone()
    } else {
        vec![1.0; n]
    };

    let mut disp = vec![Vec2::ZERO; n];
    let mut prev_disp = vec![Vec2::ZERO; n];
    let mut convergence = vec![1.0f32; n];

    let c_rep = params.scaling_ratio;
    let slow_down = params.slow_down.max(0.1);
    // FA2 splits gravity through `coefficient`: `g = gravity / scaling_ratio`
    // then `factor = scaling_ratio * mass * g / dist` — the ratios cancel,
    // leaving an effective `mass * gravity / dist`. We computed it as
    // `scaling_ratio² · mass · gravity / dist` for one painful day; that
    // crushed everything toward the origin and produced visible chaos at
    // small zooms.
    let g_eff = params.gravity;

    let mut converged_streak = 0u32;

    for iter in 0..params.max_iters {
        if stop_arc.load(Ordering::Relaxed) {
            break;
        }

        // Save previous disp; reset current.
        for i in 0..n {
            prev_disp[i] = disp[i];
            disp[i] = Vec2::ZERO;
        }

        // 1) Repulsion via Barnes–Hut.
        let tree = QuadTree::build(&pos, &rep_mass);
        for i in 0..n {
            // Pass `c_rep * rep_mass[i]` so the tree's internal
            // `force * com_mass` produces `c_rep * mass_i * mass_com / dist²`
            // per component (FA2 linear repulsion).
            let coeff = c_rep * rep_mass[i];
            disp[i] += tree.repulsion_on(pos[i], coeff, params.theta);
        }

        // 2) Gravity (toward origin).
        if params.gravity > 0.0 {
            for i in 0..n {
                let p = pos[i];
                let dist = p.length().max(1e-3);
                let factor = if params.strong_gravity {
                    mass[i] * g_eff
                } else {
                    mass[i] * g_eff / dist
                };
                disp[i] -= p * factor;
            }
        }

        // 3) Attraction along edges.
        for &(a, b) in &edges {
            let a = a as usize;
            let b = b as usize;
            if a >= n || b >= n || a == b {
                continue;
            }
            let d = pos[a] - pos[b];
            let attr_factor = if params.lin_log {
                let dist = d.length().max(1e-3);
                let f = -(1.0 + dist).ln() / dist;
                if params.outbound_attraction_distribution {
                    f / mass[a]
                } else {
                    f
                }
            } else {
                // Linear attraction: constant force per edge component
                // (per FA2, "distance is set to 1" — dx += xDist*factor
                // gives a force component proportional to xDist).
                if params.outbound_attraction_distribution {
                    -1.0 / mass[a]
                } else {
                    -1.0
                }
            };
            disp[a] += d * attr_factor;
            disp[b] -= d * attr_factor;
        }

        // 4) Apply forces with FA2 adaptive node speed. This is the
        // load-bearing piece for convergence — see module docs.
        let mut global_swinging = 0.0f32;
        let mut total_step = 0.0f32;
        for i in 0..n {
            let d = disp[i];
            let od = prev_disp[i];

            let swing_vec = od - d;
            let swinging = rep_mass[i] * swing_vec.length();
            let trac_vec = od + d;
            let traction = trac_vec.length() * 0.5;

            let denom = 1.0 + swinging.sqrt();
            let nodespeed = convergence[i] * (1.0 + traction).ln() / denom;

            // Update per-node convergence (capped at 1.0).
            let f2 = d.length_sq();
            convergence[i] = ((nodespeed * f2 / denom).sqrt()).min(1.0);

            let step = d * (nodespeed / slow_down);
            let np = pos[i] + step;
            pos[i] = Vec2::new(
                np.x.clamp(-params.bound, params.bound),
                np.y.clamp(-params.bound, params.bound),
            );
            global_swinging += swinging;
            total_step += step.length();
        }

        if let Ok(mut w) = pos_arc.write() {
            w.clone_from(&pos);
        }
        iter_arc.store(iter + 1, Ordering::Relaxed);

        // Stop once the system has nearly stopped oscillating. Mean
        // step magnitude is a more useful per-frame metric than
        // swinging for our slow_down range; use both as belts.
        let mean_step = total_step / nf;
        let mean_swing = global_swinging / nf;
        if mean_step < params.convergence_eps && mean_swing < params.convergence_eps {
            converged_streak += 1;
            if converged_streak >= params.convergence_streak {
                break;
            }
        } else {
            converged_streak = 0;
        }

        // Yield so we don't peg a core while the UI is busy.
        thread::sleep(Duration::from_micros(100));
    }

    done_arc.store(true, Ordering::Relaxed);
}

// ── Barnes–Hut quadtree ────────────────────────────────────────────────

const NO_CHILD: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Quad {
    center: Vec2,
    half: f32,
    com: Vec2,
    mass: f32,
    children: [u32; 4],
    /// Position of the single point in this cell, if any. Cleared once
    /// the cell subdivides.
    leaf_point: Option<Vec2>,
}

struct QuadTree {
    nodes: Vec<Quad>,
}

impl QuadTree {
    fn build(points: &[Vec2], weights: &[f32]) -> Self {
        if points.is_empty() {
            return Self { nodes: Vec::new() };
        }
        let mut min = points[0];
        let mut max = points[0];
        for &p in points.iter().skip(1) {
            min.x = min.x.min(p.x);
            min.y = min.y.min(p.y);
            max.x = max.x.max(p.x);
            max.y = max.y.max(p.y);
        }
        let center = (min + max) * 0.5;
        let span = (max.x - min.x).max(max.y - min.y).max(1.0);
        let half = span * 0.5 + 1.0;

        let mut tree = Self {
            nodes: Vec::with_capacity(points.len() * 2),
        };
        tree.nodes.push(Quad {
            center,
            half,
            com: Vec2::ZERO,
            mass: 0.0,
            children: [NO_CHILD; 4],
            leaf_point: None,
        });
        for (i, &p) in points.iter().enumerate() {
            let w = weights.get(i).copied().unwrap_or(1.0);
            tree.insert(0, p, w, 0);
        }
        tree
    }

    fn quadrant(center: Vec2, p: Vec2) -> usize {
        match (p.x >= center.x, p.y >= center.y) {
            (false, false) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (true, true) => 3,
        }
    }

    fn child_center(parent_center: Vec2, parent_half: f32, q: usize) -> Vec2 {
        let h = parent_half * 0.5;
        match q {
            0 => Vec2::new(parent_center.x - h, parent_center.y - h),
            1 => Vec2::new(parent_center.x + h, parent_center.y - h),
            2 => Vec2::new(parent_center.x - h, parent_center.y + h),
            _ => Vec2::new(parent_center.x + h, parent_center.y + h),
        }
    }

    fn insert(&mut self, idx: usize, p: Vec2, weight: f32, depth: u32) {
        if depth > 40 {
            let n = &mut self.nodes[idx];
            let new_mass = n.mass + weight;
            n.com = (n.com * n.mass + p * weight) / new_mass;
            n.mass = new_mass;
            return;
        }

        let (mass, existing_point, center) = {
            let n = &self.nodes[idx];
            (n.mass, n.leaf_point, n.center)
        };

        if mass == 0.0 {
            let n = &mut self.nodes[idx];
            n.com = p;
            n.mass = weight;
            n.leaf_point = Some(p);
            return;
        }

        if let Some(existing) = existing_point {
            // The existing leaf's weight equals the current `mass`
            // (only one point in this cell so far).
            let existing_weight = mass;
            self.nodes[idx].leaf_point = None;
            let q_ex = Self::quadrant(center, existing);
            let cidx = self.ensure_child(idx, q_ex);
            self.insert(cidx, existing, existing_weight, depth + 1);
        }

        {
            let n = &mut self.nodes[idx];
            let new_mass = n.mass + weight;
            n.com = (n.com * n.mass + p * weight) / new_mass;
            n.mass = new_mass;
        }

        let q = Self::quadrant(center, p);
        let cidx = self.ensure_child(idx, q);
        self.insert(cidx, p, weight, depth + 1);
    }

    fn ensure_child(&mut self, parent_idx: usize, q: usize) -> usize {
        if self.nodes[parent_idx].children[q] != NO_CHILD {
            return self.nodes[parent_idx].children[q] as usize;
        }
        let (center, half) = {
            let n = &self.nodes[parent_idx];
            (Self::child_center(n.center, n.half, q), n.half * 0.5)
        };
        let new_idx = self.nodes.len();
        self.nodes.push(Quad {
            center,
            half,
            com: Vec2::ZERO,
            mass: 0.0,
            children: [NO_CHILD; 4],
            leaf_point: None,
        });
        self.nodes[parent_idx].children[q] = new_idx as u32;
        new_idx
    }

    fn repulsion_on(&self, p: Vec2, k2: f32, theta: f32) -> Vec2 {
        if self.nodes.is_empty() {
            return Vec2::ZERO;
        }
        let mut acc = Vec2::ZERO;
        let theta2 = theta * theta;
        self.repulsion_rec(0, p, k2, theta2, &mut acc);
        acc
    }

    fn repulsion_rec(&self, idx: usize, p: Vec2, k2: f32, theta2: f32, acc: &mut Vec2) {
        let n = &self.nodes[idx];
        if n.mass == 0.0 {
            return;
        }
        let d = p - n.com;
        let dist2 = d.length_sq();
        if n.leaf_point.is_some() {
            // Single body in this cell. Skip when the body is `p` itself
            // (distance ~ 0); otherwise apply pairwise repulsion.
            if dist2 < 1e-6 {
                return;
            }
            let dist = dist2.sqrt();
            let force = k2 / dist;
            *acc += d / dist * force * n.mass;
            return;
        }
        let s = n.half * 2.0;
        if s * s < theta2 * dist2 {
            let dist = dist2.sqrt().max(0.01);
            let force = k2 / dist;
            *acc += d / dist * force * n.mass;
        } else {
            for &c in &n.children {
                if c != NO_CHILD {
                    self.repulsion_rec(c as usize, p, k2, theta2, acc);
                }
            }
        }
    }
}
