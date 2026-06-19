//! Engine-level tests for the graph-view module: the locked Poincaré disk
//! frame, Möbius navigation, and the view-snapshot persistence round-trip.

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
