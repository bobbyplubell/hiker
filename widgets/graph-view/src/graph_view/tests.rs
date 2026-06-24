//! Engine-level tests for the graph-view module: the locked Poincaré disk
//! frame, Möbius navigation, and the view-snapshot persistence round-trip.

/// Zoom-driven auto-bundling: the engine's per-frame cull + edge-rollup state.
/// status: code-graph-bundling
mod bundle_tests {
    use eframe::egui;
    use hiker_graph::LayoutKind;
    use hiker_projection::{Mobius, ProjectionConfig};

    use crate::graph_view::source::{NodeDescriptor, NodeShape};
    use crate::graph_view::{styling::Style, Lens, State};

    /// A descriptor at an explicit `world_pos` with a rep-rank (`label_scale`); everything else inert.
    fn desc_at(index: usize, world_pos: egui::Vec2, label_scale: f32) -> NodeDescriptor {
        NodeDescriptor {
            index,
            world_pos,
            radius: 4.0,
            shape: NodeShape::Circle,
            fill: egui::Color32::WHITE,
            resting_stroke: egui::Stroke::NONE,
            hover_stroke: egui::Stroke::NONE,
            badge: None,
            bug_badge: None,
            label: None,
            label_min_zoom: 0.0,
            label_scale,
            click_path: None,
            tooltip: None,
        }
    }

    /// A descriptor at `(index, 0)` with a unit rep-rank — the simple line used by the reveal tests.
    fn desc(index: usize, world_pos: egui::Vec2) -> NodeDescriptor {
        desc_at(index, world_pos, 1.0)
    }

    /// SPATIAL clustering on the FA2 positions: two on-screen-close nodes share a world cell and the
    /// lower-`label_scale` one culls into the higher-rank rep; zooming in (cell shrinks below their
    /// separation) splits them into their own cells; a far-apart node is never bundled with them; and
    /// because the grid is WORLD-FIXED, shifting every position by a sub-cell constant doesn't change
    /// membership. status: code-graph-bundling
    #[test]
    fn spatial_cluster_splits_on_zoom_and_is_pan_stable() {
        let state = State::new(Style::flat(), LayoutKind::ForceDirected);
        // Two nodes 12 world units apart (node 1 has the higher label_scale → it's the rep), plus a
        // far node at x=4000 that never shares their cell.
        let near_a = desc_at(0, egui::vec2(0.0, 0.0), 1.0);
        let near_b = desc_at(1, egui::vec2(12.0, 0.0), 1.8); // higher rank → rep
        let far = desc_at(2, egui::vec2(4000.0, 0.0), 1.0);
        let nodes = [near_a, near_b, far];
        let positions: Vec<egui::Vec2> = nodes.iter().map(|d| d.world_pos).collect();
        let lens = Lens::centred(ProjectionConfig::default(), Mobius::identity(), &positions);

        // LOW screen scale (1.0): cell = pow2 ceil(MERGE_PX / 1.0 = 48) = 64 world units, so the two
        // near nodes (12 apart) share one cell and collapse into the higher-rank rep (node 1). The far
        // node (4000) is in its own cell. The 0.0 sentinel would disable bundling — we pass > 0.
        let b = state.compute_bundles(&nodes, &lens, 1.0);
        assert!(b.is_visible(1), "the higher-label_scale node is the visible rep");
        assert!(!b.is_visible(0), "the lower-rank near node culls into the rep");
        assert_eq!(b.rep(0), 1, "rep = max label_scale member of the shared cell");
        assert_eq!(b.rolled_count(1), 1, "one member rolled into the rep");
        assert!(b.is_visible(2), "the far node is never bundled with the near pair");
        assert_eq!(b.rolled_count(2), 0);

        // HIGH screen scale (8.0): cell = pow2 ceil(48/8 = 6) = 8 world units < the 12-unit separation,
        // so the two near nodes fall into DIFFERENT cells → BOTH visible, nothing rolled up.
        let b = state.compute_bundles(&nodes, &lens, 8.0);
        assert!(b.is_visible(0) && b.is_visible(1), "zoom split the cluster — both shown");
        assert!((0..3).all(|i| b.rolled_count(i) == 0), "no bundles once the cell shrinks past them");

        // PAN stability: shift every node by a sub-cell constant (≪ 64) at the low scale — a
        // world-fixed grid keeps the same membership (node 1 still the rep, node 0 still culled).
        let shifted: Vec<NodeDescriptor> = nodes
            .iter()
            .map(|d| desc_at(d.index, d.world_pos + egui::vec2(3.0, -2.0), d.label_scale))
            .collect();
        let b = state.compute_bundles(&shifted, &lens, 1.0);
        assert!(b.is_visible(1) && !b.is_visible(0), "a sub-cell pan never changes membership");
        assert_eq!(b.rep(0), 1);

        // The 0.0 sentinel (read-only / Poincaré panes) disables bundling → identity, every node shown.
        let b = state.compute_bundles(&nodes, &lens, 0.0);
        assert!((0..3).all(|i| b.is_visible(i)), "non-positive screen scale → no spatial bundling");
        assert_eq!(b.fingerprint(), 0, "identity → zero cache perturbation");
    }

    /// Un-bundling reveal: a culled→visible transition restarts `reveal_t` at `0.0`, advancing by
    /// `dt` eases it toward `1.0` and clamps there, and a node that stays visible just keeps
    /// advancing (no restart). status: code-graph-bundling
    #[test]
    fn reveal_resets_and_advances_on_unbundle() {
        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        // Rep at origin (high label_scale) + a near member 12 units away (low rank). At a low screen
        // scale (cell 64) they share a cell → the member culls into the rep; at a high scale (cell 8 <
        // 12) they split → the member reveals.
        let nodes = [desc_at(0, egui::vec2(0.0, 0.0), 1.8), desc_at(1, egui::vec2(12.0, 0.0), 1.0)];
        state.positions = nodes.iter().map(|d| d.world_pos).collect();
        state.reset_reveal_anim(); // sizes reveal_t to 2, all settled (1.0)
        let lens = Lens::centred(ProjectionConfig::default(), Mobius::identity(), &state.positions);

        // Frame 1 — low scale (1.0): the member is culled into the rep. No reveal yet; history seeded.
        let b = state.compute_bundles(&nodes, &lens, 1.0);
        assert!(!b.is_visible(1) && b.rep(1) == 0);
        let animating = state.advance_reveal(&b, 0.1);
        assert!(!animating, "nothing visible-but-unsettled yet");
        assert_eq!(state.reveal_t[1], 1.0, "still-culled node untouched");

        // Frame 2 — high scale (8.0): the cell splits, the member emerges. The culled→visible edge
        // resets it to 0, captures the rep (node 0) as its fly-out origin, then advances by dt/REVEAL_DUR.
        let b = state.compute_bundles(&nodes, &lens, 8.0);
        assert!(b.is_visible(1));
        let animating = state.advance_reveal(&b, 0.1);
        assert!(animating, "a just-revealed member is mid-flight");
        let after_one = state.reveal_t[1];
        assert!(after_one > 0.0 && after_one < 1.0, "advanced off 0 but not settled: {after_one}");
        assert!((after_one - 0.1 / super::super::REVEAL_DUR).abs() < 1e-5, "stepped by dt/REVEAL_DUR");

        // Frame 3 — still visible (no restart): keeps advancing from where it was.
        let b = state.compute_bundles(&nodes, &lens, 8.0);
        state.advance_reveal(&b, 0.1);
        assert!(state.reveal_t[1] > after_one, "kept advancing without a reset");

        // A big dt clamps at 1.0 (settled), and the animation reports done.
        let b = state.compute_bundles(&nodes, &lens, 8.0);
        let animating = state.advance_reveal(&b, 10.0);
        assert_eq!(state.reveal_t[1], 1.0, "clamped at fully settled");
        assert!(!animating, "settled → not animating");
    }

    /// `effective_positions` lerps a mid-flight node from its SPATIAL cluster rep's `world_pos` at `t
    /// = 0` to its own at `t = 1` (ease endpoints exact), reading the captured `reveal_origin`; a
    /// settled node sits at its own position. status: code-graph-bundling
    #[test]
    fn effective_positions_lerp_from_rep() {
        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        // rep at x=0, member at x=10.
        let nodes = [desc_at(0, egui::vec2(0.0, 0.0), 1.8), desc_at(1, egui::vec2(10.0, 0.0), 1.0)];
        state.positions = nodes.iter().map(|d| d.world_pos).collect();
        state.reset_reveal_anim();
        // The member's recorded fly-out origin is node 0 (the cluster rep it emerged from).
        state.reveal_origin = vec![0, 0];

        // t = 0 → exactly the rep's position (fly-out origin).
        state.reveal_t = vec![1.0, 0.0];
        let eff = state.effective_positions(&nodes);
        assert_eq!(eff[1], egui::vec2(0.0, 0.0), "at t=0 the member starts at the rep centre");

        // t = 1 → exactly its own position.
        state.reveal_t = vec![1.0, 1.0];
        let eff = state.effective_positions(&nodes);
        assert_eq!(eff[1], egui::vec2(10.0, 0.0), "at t=1 the member sits at its own spot");

        // Mid-flight (t = 0.5) → strictly between, and past the linear midpoint (ease-out front-loads
        // the motion: it's already MORE than halfway out at t=0.5).
        state.reveal_t = vec![1.0, 0.5];
        let eff = state.effective_positions(&nodes);
        assert!(eff[1].x > 5.0 && eff[1].x < 10.0, "mid-flight past the midpoint: {}", eff[1].x);
        // A node whose origin is itself never moves.
        assert_eq!(eff[0], egui::vec2(0.0, 0.0));
    }

    /// A relayout (`recompute_layout`) clears the un-bundling animation: every node is reset to
    /// settled (`reveal_t == 1.0`) and sized to the new node count, so a fresh layout never animates
    /// the whole graph. status: code-graph-bundling
    #[test]
    fn relayout_clears_reveal_anim() {
        use super::super::source::{LayoutConfig, NodeDescriptor, NodeShape, Source};
        use hiker_graph::{LayoutKind as LK, LayoutTree};

        struct TriSource;
        impl Source for TriSource {
            fn node_count(&self) -> usize {
                3
            }
            fn nodes(&self, positions: &[egui::Vec2], _s: &Style) -> Vec<NodeDescriptor> {
                positions
                    .iter()
                    .enumerate()
                    .map(|(index, &world_pos)| NodeDescriptor {
                        index,
                        world_pos,
                        radius: 4.0,
                        shape: NodeShape::Circle,
                        fill: egui::Color32::WHITE,
                        resting_stroke: egui::Stroke::NONE,
                        hover_stroke: egui::Stroke::NONE,
                        badge: None,
                        bug_badge: None,
                        label: None,
                        label_min_zoom: 0.0,
                        label_scale: 1.0,
                        click_path: None,
                        tooltip: None,
                    })
                    .collect()
            }
            fn edges(&self) -> Vec<(u32, u32)> {
                vec![(0, 1), (1, 2)]
            }
            fn layout_tree(&self, _k: LK) -> LayoutTree {
                LayoutTree::from_parents(&vec![None; 3])
            }
            fn preview_for(&self, _i: usize) -> Option<(String, String)> {
                None
            }
        }

        let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
        // Dirty the animation state as if a fly-out were in progress.
        state.positions = vec![egui::vec2(0.0, 0.0)];
        state.reveal_t = vec![0.3];
        state.prev_bundle_visible = vec![true];
        state.prev_bundle_rep = vec![0];
        state.reveal_origin = vec![0];

        state.recompute_layout(&TriSource, LayoutConfig { area: 1_000.0, seed_box: 80.0 });
        assert_eq!(state.reveal_t.len(), 3, "resized to the new node count");
        assert!(state.reveal_t.iter().all(|&t| t == 1.0), "every node reset to settled");
        assert!(state.prev_bundle_visible.is_empty(), "transition history cleared on relayout");
        assert!(state.prev_bundle_rep.is_empty() && state.reveal_origin.is_empty(), "rep/origin cleared");
    }

    /// Nodes far apart on screen (one per cell) → every node visible + a zero fingerprint, even at a
    /// positive screen scale, so a sparse graph never spuriously bundles. status: code-graph-bundling
    #[test]
    fn no_bundle_when_each_node_owns_its_cell() {
        let state = State::new(Style::flat(), LayoutKind::ForceDirected);
        // 2000 world units apart at screen scale 1.0 (cell 64) → different cells.
        let nodes = [desc(0, egui::vec2(0.0, 0.0)), desc(1, egui::vec2(2000.0, 0.0))];
        let positions: Vec<egui::Vec2> = nodes.iter().map(|d| d.world_pos).collect();
        let lens = Lens::centred(ProjectionConfig::default(), Mobius::identity(), &positions);
        let b = state.compute_bundles(&nodes, &lens, 1.0);
        assert!(b.is_visible(0) && b.is_visible(1));
        assert_eq!(b.rep(0), 0);
        assert_eq!(b.rolled_count(0), 0, "no shared cell → nothing rolled up");
        assert_eq!(b.fingerprint(), 0, "all visible → zero cache perturbation");
    }
}

mod poincare_disk_tests {
    use crate::graph_view::{poincare_disk, Lens};
    use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};

    /// The Poincaré projection config the locked-disk tests render at. A
    /// strength that visibly spreads the clusters across the disk.
    fn poincare_cfg() -> ProjectionConfig {
        ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.4,
            ..Default::default()
        }
    }

    /// A small clustered layout: a central blob at the origin plus a ring of
    /// off-centre clusters, mirroring the synthetic snapshot graph's shape so
    /// the periphery has real spread for the lens to compress toward the rim.
    fn clustered_positions() -> Vec<egui::Vec2> {
        let mut pos = Vec::new();
        // Central cluster.
        for i in 0..6 {
            let a = i as f32;
            pos.push(egui::vec2(a * 7.0 - 17.0, a * -5.0 + 12.0));
        }
        // Ring clusters.
        let radius = 320.0;
        for c in 0..5 {
            let ang = c as f32 / 5.0 * std::f32::consts::TAU;
            let (cx, cy) = (ang.cos() * radius, ang.sin() * radius);
            for i in 0..6 {
                let a = i as f32;
                pos.push(egui::vec2(cx + a * 9.0 - 22.0, cy + a * -6.0 + 15.0));
            }
        }
        pos
    }

    /// The locked-disk frame is a pure function of the pane rect + zoom: centre =
    /// pane centre (ZOOM-INVARIANT), radius = `DISK_FILL` of the shorter
    /// half-dimension × zoom — and it does NOT (cannot) depend on any
    /// `View`/pan/zoom. This is the core regression guard for "the disk drifts":
    /// the signature itself forbids the bug. The centre must be the pane centre
    /// for ANY zoom; only the radius scales.
    #[test]
    fn poincare_disk_is_pane_locked() {
        let rects = [
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0)),
            egui::Rect::from_min_size(egui::pos2(40.0, 90.0), egui::vec2(800.0, 400.0)),
            egui::Rect::from_min_size(egui::pos2(-120.0, 30.0), egui::vec2(300.0, 900.0)),
        ];
        for r in rects {
            for zoom in [0.5_f32, 1.0, 3.0] {
                let (center, radius) = poincare_disk(r, zoom);
                assert_eq!(
                    center,
                    r.center(),
                    "disk centre must be the pane centre at zoom {zoom}"
                );
                let expected = 0.5 * r.size().min_elem() * hiker_projection_view::DISK_FILL * zoom;
                assert!(
                    (radius - expected).abs() < 1e-4,
                    "disk radius {radius} != {expected} for {r:?} at zoom {zoom}"
                );
            }
        }
    }

    /// Scroll-zoom scales the RADIUS while leaving the CENTRE fixed: two zoom
    /// values yield the same centre, and the radius ratio equals the zoom ratio
    /// (the disk grows/shrinks centred, never drifting).
    #[test]
    fn poincare_zoom_scales_radius_keeps_center() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(640.0, 480.0));
        let (c_a, r_a) = poincare_disk(rect, 1.0);
        let (c_b, r_b) = poincare_disk(rect, 2.5);
        assert_eq!(c_a, c_b, "centre must be zoom-invariant");
        assert_eq!(c_a, rect.center(), "centre must be the pane centre");
        // radius ratio == zoom ratio (2.5 / 1.0).
        assert!(
            (r_b / r_a - 2.5).abs() < 1e-4,
            "radius ratio {} != zoom ratio 2.5",
            r_b / r_a
        );
    }

    /// The disk→screen map sends the unit-disk origin to the disk centre and a
    /// rim point (|z| = 1) to the disk centre + radius — the disk fills exactly
    /// the locked frame.
    #[test]
    fn poincare_maps_origin_to_center_and_rim_to_radius() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(640.0, 480.0));
        let (center, radius) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| center + egui::vec2(z.re, z.im) * radius;
        let o = disk_to_screen(Complex::ORIGIN);
        assert!((o - center).length() < 1e-4, "origin {o:?} != centre {center:?}");
        let rim = disk_to_screen(Complex::new(1.0, 0.0));
        let expected = center + egui::vec2(radius, 0.0);
        assert!(
            (rim - expected).length() < 1e-4,
            "rim {rim:?} != centre+radius {expected:?}"
        );
    }

    /// Every node, projected through the locked Poincaré map at fit (zoom 1.0),
    /// lands inside the fixed disk (within 1px of the boundary) — the whole graph
    /// is pressed into the disk, never panned off-screen. (At higher zoom the disk
    /// may exceed the pane and nodes legitimately fall outside it, so this is a
    /// zoom-1.0 / relative-to-disk-radius invariant.)
    #[test]
    fn poincare_keeps_all_nodes_inside_disk() {
        let pos = clustered_positions();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0));
        let (center, radius) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| center + egui::vec2(z.re, z.im) * radius;
        let lens = Lens::centred(poincare_cfg(), Mobius::identity(), &pos);
        for &w in &pos {
            let p = disk_to_screen(lens.disk(w));
            let dist = (p - center).length();
            assert!(
                dist <= radius + 1.0,
                "node at {dist}px from centre exceeds disk radius {radius}"
            );
        }
    }

    /// Möbius navigation moves the projected content but leaves the disk frame
    /// invariant: the disk centre/radius are unchanged and at least one node's
    /// screen position changes. The disk is a fixed viewport; nav re-aims the
    /// graph within it.
    #[test]
    fn mobius_nav_moves_content_but_not_disk_geometry() {
        let pos = clustered_positions();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 600.0));
        let cfg = poincare_cfg();
        // The disk frame depends only on the pane (+ a fixed zoom) — recomputing
        // it after nav must yield the identical centre/radius.
        let (c0, r0) = poincare_disk(rect, 1.0);
        let disk_to_screen = |z: Complex| c0 + egui::vec2(z.re, z.im) * r0;

        let plain = Lens::centred(cfg, Mobius::identity(), &pos);
        // Recentre on a peripheral node, as a drag/fly-to would.
        let target = plain.disk(pos[pos.len() - 1]);
        let nav = Mobius::from_point_pair(target, Complex::ORIGIN);
        let navd = Lens::centred(cfg, nav, &pos);

        let (c1, r1) = poincare_disk(rect, 1.0);
        assert_eq!(c0, c1, "nav must not move the disk centre");
        assert!((r0 - r1).abs() < 1e-6, "nav must not rescale the disk");

        let mut moved = false;
        for &w in &pos {
            let before = disk_to_screen(plain.disk(w));
            let after = disk_to_screen(navd.disk(w));
            if (before - after).length() > 1.0 {
                moved = true;
            }
            // Content stays inside the (unchanged) disk under navigation too.
            assert!((after - c1).length() <= r1 + 1.0, "navigated node left the disk");
        }
        assert!(moved, "navigation did not move any content");
    }
}

mod nav_tests {
    use crate::graph_view::Lens;
    use hiker_projection::{Complex, Mobius, ProjectionConfig, ProjectionKind};

    fn positions() -> Vec<egui::Vec2> {
        vec![
            egui::vec2(0.0, 0.0),
            egui::vec2(100.0, 0.0),
            egui::vec2(-100.0, 50.0),
            egui::vec2(40.0, -90.0),
        ]
    }

    /// A non-identity nav must leave Off and Fisheye disk points untouched —
    /// navigation is Poincaré-only, so those modes stay byte-identical.
    #[test]
    fn nav_ignored_off_and_fisheye() {
        let pos = positions();
        let nav = Mobius::from_point_pair(Complex::new(0.3, -0.2), Complex::ORIGIN);
        for kind in [ProjectionKind::Affine, ProjectionKind::Fisheye] {
            let cfg = ProjectionConfig { kind, strength: 1.2, ..Default::default() };
            let plain = Lens::centred(cfg, Mobius::identity(), &pos);
            let navd = Lens::centred(cfg, nav, &pos);
            for &w in &pos {
                let a = plain.disk(w);
                let b = navd.disk(w);
                assert!(
                    (a.re - b.re).abs() < 1e-6 && (a.im - b.im).abs() < 1e-6,
                    "{kind:?} disk changed under nav: {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Under Poincaré the recentre `from_point_pair(z_node, ORIGIN)` must drive
    /// the chosen node's disk point exactly to the origin.
    #[test]
    fn poincare_flyto_centres_target() {
        let pos = positions();
        let cfg = ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.2,
            ..Default::default()
        };
        let base = Lens::centred(cfg, Mobius::identity(), &pos);
        let target = base.disk(pos[2]); // a peripheral node, pre-nav
        let nav = Mobius::from_point_pair(target, Complex::ORIGIN);
        let navd = Lens::centred(cfg, nav, &pos);
        let centred = navd.disk(pos[2]);
        assert!(
            centred.abs() < 1e-4,
            "fly-to did not centre target: {centred:?}"
        );
    }
}

mod view_snapshot_tests {
    use crate::graph_view::source::Snapshot;
    use crate::graph_view::styling::Style;
    use crate::graph_view::{FocusMode, State};
    use hiker_graph::LayoutKind;
    use hiker_projection::ProjectionKind;

    /// `view_snapshot` → `restore_view` round-trips the persistable view bits
    /// (pan/zoom, projection, focus mode, toggles, LOD) and seeds the warm-layout
    /// positions keyed by `node_key` back into `prev_positions`.
    #[test]
    fn snapshot_restore_round_trips() {
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        s.view.pan = egui::vec2(-12.0, 34.0);
        s.view.zoom = 0.75;
        s.projection.kind = ProjectionKind::Poincare;
        s.projection.strength = 1.6;
        s.projection.size_falloff = 0.4;
        s.focus_mode = FocusMode::Cursor;
        s.toggles.show_labels = false;
        s.toggles.show_edges = false;
        s.toggles.show_preview = true;
        s.lod_full_mag = 0.6;
        s.lod_marker_mag = 0.2;
        s.prev_positions.insert("a".into(), egui::vec2(1.0, 2.0));
        s.prev_positions.insert("b".into(), egui::vec2(-3.0, 4.0));

        let snap = s.view_snapshot();
        assert_eq!(snap.projection_kind, "poincare");
        assert_eq!(snap.focus_mode, "cursor");
        assert_eq!(snap.positions["a"], (1.0, 2.0));

        let mut t = State::new(Style::flat(), LayoutKind::ForceDirected);
        t.restore_view(&snap);
        assert_eq!(t.view.pan, egui::vec2(-12.0, 34.0));
        assert!((t.view.zoom - 0.75).abs() < 1e-6);
        assert!(matches!(t.projection.kind, ProjectionKind::Poincare));
        assert!((t.projection.strength - 1.6).abs() < 1e-6);
        assert!(matches!(t.focus_mode, FocusMode::Cursor));
        assert!(!t.toggles.show_labels && !t.toggles.show_edges && t.toggles.show_preview);
        assert!((t.lod_full_mag - 0.6).abs() < 1e-6);
        assert_eq!(t.prev_positions["a"], egui::vec2(1.0, 2.0));
        assert_eq!(t.prev_positions["b"], egui::vec2(-3.0, 4.0));
    }

    /// A zero zoom in the snapshot (e.g. an uninitialised default) is ignored so
    /// restore never zeroes the view's zoom.
    #[test]
    fn restore_ignores_zero_zoom() {
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        let original = s.view.zoom;
        let snap = Snapshot { zoom: 0.0, ..Default::default() };
        s.restore_view(&snap);
        assert_eq!(s.view.zoom, original);
    }
}

mod pulse_tests {
    use hiker_graph::LayoutKind;

    use super::super::{styling::Style, State};

    /// `pulse_nodes` injects full fluid energy at the given indices (the host
    /// entry into the hover fluid for spec lighting), sizing the field to the
    /// layout, ignoring out-of-range indices, and no-op'ing while the fluid
    /// highlight is toggled off (stale energy would pop in when re-enabled).
    #[test]
    fn pulse_nodes_injects_energy_where_fluid_is_on() {
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        s.positions = vec![egui::vec2(0.0, 0.0), egui::vec2(1.0, 0.0), egui::vec2(0.0, 1.0)];
        s.pulse_nodes(&[1, 99]);
        assert_eq!(s.fluid_energy, vec![0.0, 1.0, 0.0], "in-range injected, 99 ignored");

        // Fluid off: the field never advances, so injection must be a no-op.
        let mut off = State::new(Style::flat(), LayoutKind::ForceDirected);
        off.positions = vec![egui::vec2(0.0, 0.0)];
        off.highlight.fluid = false;
        off.pulse_nodes(&[0]);
        assert!(off.fluid_energy.is_empty(), "fluid off → nothing injected");
    }
}

/// `pump_layout` lets a host settle a force layout WITHOUT rendering through
/// `ui()` — the fix for the spec graph sitting at its scatter seed when it was
/// only ever driven as a read-only minimap. status: spec-minimap-swap
mod pump_layout_tests {
    use std::time::{Duration, Instant};

    use hiker_graph::{LayoutKind, LayoutTree};

    use super::super::source::{LayoutConfig, NodeDescriptor, Source};
    use super::super::{styling::Style, LayoutWorker, State};

    /// A trivial line graph A—B—C—D source (degree-weighted dots, stable keys),
    /// enough to give the force worker something to relax.
    struct LineSource {
        n: usize,
    }

    impl Source for LineSource {
        fn node_count(&self) -> usize {
            self.n
        }

        fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
            positions
                .iter()
                .enumerate()
                .map(|(index, &world_pos)| NodeDescriptor {
                    index,
                    world_pos,
                    radius: 4.0,
                    shape: super::super::source::NodeShape::Circle,
                    fill: egui::Color32::WHITE,
                    resting_stroke: egui::Stroke::NONE,
                    hover_stroke: egui::Stroke::NONE,
                    badge: None,
                    bug_badge: None,
                    label: None,
                    label_min_zoom: 0.0,
                    label_scale: 1.0,
                    click_path: None,
                    tooltip: None,
                })
                .collect()
        }

        fn edges(&self) -> Vec<(u32, u32)> {
            (0..self.n.saturating_sub(1)).map(|i| (i as u32, (i + 1) as u32)).collect()
        }

        fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
            LayoutTree::from_parents(&vec![None; self.n])
        }

        fn node_key(&self, index: usize) -> Option<String> {
            Some(format!("n{index}"))
        }

        fn preview_for(&self, _index: usize) -> Option<(String, String)> {
            None
        }
    }

    /// A settling worker's positions become readable via `pump_layout` alone —
    /// no `ui()` call, no egui context to allocate a pane. The layout starts at a
    /// scatter seed and `pump_layout` snapshots the worker's relaxed positions, so
    /// the nodes move off the seed (the bug was: with only a minimap, nothing ever
    /// pumped, so the spec graph stayed scattered).
    #[test]
    fn pump_layout_settles_without_ui() {
        let ctx = egui::Context::default();
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        let source = LineSource { n: 4 };
        s.recompute_layout(&source, LayoutConfig { area: 1_000.0, seed_box: 80.0 });
        let seed = s.positions.clone();
        assert_eq!(seed.len(), 4, "scatter seed has one position per node");
        // Pump until the worker converges (bounded by a wall-clock deadline so a
        // slow/non-converging build can't hang the test), exactly as a
        // minimap-only host would each frame.
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.worker.as_ref().is_some_and(LayoutWorker::is_running)
            && Instant::now() < deadline
        {
            s.pump_layout(&ctx);
            std::thread::sleep(Duration::from_millis(5));
        }
        // The relaxed line graph spreads its endpoints apart — the positions are
        // no longer the seed, proving the worker advanced through `pump_layout`.
        assert!(
            s.positions.iter().zip(&seed).any(|(a, b)| (*a - *b).length() > 1.0),
            "pump_layout advanced the force layout off its scatter seed",
        );
    }
}

/// Affine glide-to-selection: `glide_to` aims `view.pan` at `-world`, `advance_glide`
/// eases it there and lands exactly on target at `t >= 1`, and a tiny move snaps.
/// status: code-graph
mod glide_tests {
    use eframe::egui;
    use hiker_graph::LayoutKind;

    use super::super::{styling::Style, State};

    /// Mirror of the engine's affine glide duration (`nav::GLIDE_DUR`), inlined here so the test
    /// doesn't depend on a private const's visibility.
    const GLIDE_DUR: f32 = 0.4;

    #[test]
    fn glide_to_targets_negated_world_and_advance_lands_exactly() {
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        s.view.pan = egui::vec2(0.0, 0.0);
        s.view.zoom = 1.0;
        let world = egui::vec2(100.0, -50.0);
        s.glide_to(world);

        // Target pan = -world (centring law `pan = -w`), animation just started.
        let g = s.glide.expect("a non-tiny move starts a glide");
        assert_eq!(g.target_pan, -world, "glide aims pan at the negated world point");
        assert_eq!(g.start_pan, egui::vec2(0.0, 0.0), "starts from the current pan");
        assert_eq!(g.t, 0.0);

        // One sub-duration step moves pan off the start toward the target, but not
        // all the way (ease-out has 0 < e < 1 for 0 < t < 1).
        let still = s.advance_glide(GLIDE_DUR * 0.5);
        assert!(still, "mid-flight at half the duration");
        let mid = s.view.pan;
        assert!(mid != egui::vec2(0.0, 0.0), "pan moved off the start");
        assert!(mid != -world, "pan not yet at target");
        // Moving toward the target: closer to -world than the start was.
        assert!(
            (mid - (-world)).length() < world.length(),
            "pan glided toward the target",
        );

        // A step past the end lands EXACTLY on target and clears the glide.
        let still = s.advance_glide(GLIDE_DUR);
        assert!(!still, "glide finished");
        assert_eq!(s.view.pan, -world, "ease endpoint is exact at t >= 1");
        assert!(s.glide.is_none(), "finished glide is cleared");
    }

    #[test]
    fn glide_to_snaps_on_a_tiny_move() {
        let mut s = State::new(Style::flat(), LayoutKind::ForceDirected);
        // Pan already centres a world point a hair away from the new target.
        s.view.pan = egui::vec2(0.0, 0.0);
        // world ~ (0.1, 0.0) → target pan (-0.1, 0) is < GLIDE_MIN_MOVE from start.
        s.glide_to(egui::vec2(0.1, 0.0));
        assert!(s.glide.is_none(), "a tiny move never starts an animation");
        assert_eq!(s.view.pan, egui::vec2(-0.1, 0.0), "tiny move snaps straight to target");
    }
}
