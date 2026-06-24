//! Visual test harness for the code-graph view (`graph-visual-harness`).
//!
//! Renders the REAL [`EntityGraphSource`] through the engine headless (egui_kittest's wgpu
//! backend) into PNGs, with camera control (zoom/pan) and injected pointer input — so the LOD /
//! label / layout / hover behaviour can be SEEN and verified instead of guessed at. Each scenario
//! writes `target/graph-harness/<name>.png`, which the developer (or an agent) reads back.
//!
//! It's `#[ignore]`d (needs a SCIP index + a wgpu device + writes files), run on demand:
//!   HIKER_HARNESS_SCIP=~/code-intel-vault/pyproj.scip \
//!     cargo test -p hiker-app --lib graph_harness -- --ignored --nocapture
//! Defaults to `~/code-intel-vault/pyproj.scip` (small/fast); point at `hiker.scip` for the real
//! self-host graph. Skips cleanly (exit-ok) if the SCIP or a wgpu device is unavailable.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use eframe::egui;
use hiker_code::governance::Governance;
use hiker_code::ScipAdapter;
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_graph::LayoutKind;
use hiker_graph_view::graph_view::styling::{Style, LABEL_PILL};
use hiker_graph_view::graph_view::State;
use spec_engine::SourceId;

use crate::panels::code_graph::CODE_CFG;
use crate::panels::entity_graph::{self, filter_for, EntityGraph, EntityGraphSource, Lens};

/// Square render size (px) — matches the app's graph pane scale.
const SIZE: f32 = 1100.0;
/// Frames to pump the force worker before the first scenario, to settle the layout.
const SETTLE_STEPS: usize = 240;

/// One thing to render: a name (→ PNG filename), a zoom multiple over the fitted overview, an
/// optional pan (world units from the centroid), and optional pointer input to inject.
struct Scenario {
    name: &'static str,
    /// Zoom relative to the fitted overview (1.0 = overview, >1 = zoomed in).
    zoom: f32,
    /// Pointer events injected before the capture (screen px, origin = pane top-left).
    input: Input,
    /// When `true`, instead of re-centering on the GLOBAL layout centroid this
    /// scenario centers on the single container node with the MOST members (the
    /// `bundle-open` test): zoom in on ONE bundle and confirm ITS members appear
    /// clustered in-viewport when it unbundles. status: code-graph-containment-layout
    bundle_open: bool,
    /// When `true`, capture the un-bundling fly-out MID-animation: like `bundle_open` (centre on the
    /// most-populous container, zoom past its members' reveal threshold), but render only a frame or
    /// two AFTER the visibility flips — so the members are caught CLUSTERED TIGHT around the parent
    /// (mid-flight), distinctly tighter than the settled `bundle-open.png`. status: code-graph-bundling
    reveal_mid: bool,
}

/// Pointer input for a scenario.
enum Input {
    None,
    /// Hover at a screen position (shows the hover decoration / preview).
    Hover(egui::Pos2),
    /// Primary-click at a screen position (selects).
    Click(egui::Pos2),
}

fn scip_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HIKER_HARNESS_SCIP") {
        let p = PathBuf::from(shellexpand_home(&p));
        return p.exists().then_some(p);
    }
    let home = std::env::var("HOME").ok()?;
    let default = PathBuf::from(home).join("code-intel-vault/pyproj.scip");
    default.exists().then_some(default)
}

fn shellexpand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// What [`load`] produced: the FILTERED overview display (primary lens), the loaded governance
/// (drift colors / badges), the per-id label-importance map, and the size-by-LOC flag.
struct Loaded {
    display: EntityGraph,
    gov: Option<Governance>,
    importance: std::collections::HashMap<String, f32>,
    size_by_loc: bool,
}

/// Build the unified entity graph from a SCIP index, then the primary-lens overview display — the
/// FULL path (code + specs + governance) when the vault layout is present, falling back to
/// code-only (`from_code`) otherwise. Faithful to what the app's overview renders.
///
/// Default paths from the scip (overridable by `HIKER_HARNESS_VAULT` / `HIKER_HARNESS_REPO`):
/// vault root = scip's dir (its `.hiker/index.db` is the store), repo root = `<vault>/<scip-stem>`
/// (the mirror with `docs/` + `links.json`), docs = `<repo>/docs`.
fn load(scip: &PathBuf) -> Result<Loaded, String> {
    let scip_dir = scip.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    let vault_root = std::env::var("HIKER_HARNESS_VAULT")
        .map(|v| PathBuf::from(shellexpand_home(&v)))
        .unwrap_or_else(|_| scip_dir.clone());
    let stem = scip.file_stem().and_then(|s| s.to_str()).unwrap_or("repo");
    let repo_root = std::env::var("HIKER_HARNESS_REPO")
        .map(|v| PathBuf::from(shellexpand_home(&v)))
        .unwrap_or_else(|_| vault_root.join(stem));
    let adapter_root = if repo_root.exists() { repo_root.clone() } else { scip_dir.clone() };

    let src = SourceId("harness".into());
    let adapter = ScipAdapter::load(scip, &adapter_root, src.clone())
        .map_err(|e| format!("load scip: {e}"))?;
    let code = adapter.code_graph();

    // Full build (specs + governance) when the vault + repo mirror are present; else code-only.
    let (graph, gov) = match (repo_root.exists(), Store::open(&vault_root), Vault::open(&vault_root))
    {
        (true, Ok(store), Ok(vault)) => {
            let gov =
                Governance::load(&repo_root, &repo_root.join("docs"), &src, &adapter).ok();
            let g = EntityGraph::build(&code, gov.as_ref(), &store, &vault);
            eprintln!("graph_harness: FULL build (specs+gov) from {}", repo_root.display());
            (g, gov)
        }
        _ => {
            eprintln!("graph_harness: code-only build (no vault/repo mirror at {})", repo_root.display());
            (EntityGraph::from_code(&code), None)
        }
    };

    let importance = entity_graph::label_importance(&graph);
    let lens = Lens::primary_default(&graph);
    let display = filter_for(&graph, &lens, None, None, None, &[]);
    Ok(Loaded { display, gov, importance, size_by_loc: lens.size_by_loc })
}

#[test]
#[ignore = "manual visual harness: needs a SCIP index + wgpu device, writes PNGs"]
fn graph_harness() {
    let Some(scip) = scip_path() else {
        eprintln!("graph_harness: no SCIP index (set HIKER_HARNESS_SCIP); skipping");
        return;
    };
    let loaded = match load(&scip) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("graph_harness: {e}; skipping");
            return;
        }
    };
    eprintln!(
        "graph_harness: display {} nodes, {} edges from {}",
        loaded.display.nodes.len(),
        loaded.display.edges.len(),
        scip.display()
    );

    // The display node with the MOST direct members (children in the rewritten display `parent`
    // tree) — the prominent container the `bundle-open` scenario zooms into. We center on THIS
    // node and zoom past its members' reveal threshold; with containment springs its members must
    // appear clustered around the viewport center. status: code-graph-containment-layout
    let (bundle_target, bundle_target_members) = most_populous_container(&loaded.display);
    eprintln!(
        "graph_harness: bundle-open target = display node {} ({:?}) with {} members",
        bundle_target,
        loaded.display.nodes.get(bundle_target).map(|n| n.name.as_str()),
        bundle_target_members,
    );

    let scenarios = [
        Scenario { name: "overview", zoom: 1.0, input: Input::None, bundle_open: false, reveal_mid: false },
        Scenario { name: "zoom-2x", zoom: 2.0, input: Input::None, bundle_open: false, reveal_mid: false },
        Scenario { name: "zoom-4x", zoom: 4.0, input: Input::None, bundle_open: false, reveal_mid: false },
        Scenario { name: "hover-center", zoom: 2.0, input: Input::Hover(egui::pos2(SIZE / 2.0, SIZE / 2.0)), bundle_open: false, reveal_mid: false },
        Scenario { name: "click-center", zoom: 2.0, input: Input::Click(egui::pos2(SIZE / 2.0, SIZE / 2.0)), bundle_open: false, reveal_mid: false },
        // Zoom WELL past the members' reveal threshold (several × the fitted overview) centered on
        // the most-populous container, so its bundle dissolves and the members must show in-place.
        Scenario { name: "bundle-open", zoom: 8.0, input: Input::None, bundle_open: true, reveal_mid: false },
        // Same camera as bundle-open, but rendered MID fly-out — members should be pulled in tight
        // around the parent vs. the settled bundle-open spread. status: code-graph-bundling
        Scenario { name: "bundle-reveal-mid", zoom: 8.0, input: Input::None, bundle_open: true, reveal_mid: true },
    ];

    let out_dir = PathBuf::from("target/graph-harness");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("graph_harness: mkdir {}: {e}; skipping", out_dir.display());
        return;
    }

    let Loaded { display, gov, importance, size_by_loc } = loaded;
    let display = Rc::new(display);
    let gov = Rc::new(gov);
    let importance = Rc::new(importance);
    let state = Rc::new(RefCell::new(make_state()));

    // Lay out once (force), then settle, on a harness we keep across scenarios.
    {
        let src = EntityGraphSource::new(&display, size_by_loc, None, gov.as_ref().as_ref())
            .with_importance(&importance);
        state.borrow_mut().recompute_layout(&src, CODE_CFG);
    }

    let renderer = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        egui_kittest::wgpu::WgpuTestRenderer::new,
    )) {
        Ok(r) => r,
        Err(_) => {
            eprintln!("graph_harness: no wgpu device; skipping");
            return;
        }
    };

    let (d, g, imp, st) = (display.clone(), gov.clone(), importance.clone(), state.clone());
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::splat(SIZE))
        // A small per-step dt (~33ms) so the un-bundling reveal tween (REVEAL_DUR ≈ 0.35s) advances in
        // small increments per `step()` — the `bundle-reveal-mid` scenario then catches the fly-out
        // mid-flight (reveal_t ~0.1–0.3) by stepping just once or twice after the bundle dissolves.
        // The default 0.25s would over-advance it to nearly settled in one frame. status: code-graph-bundling
        .with_step_dt(1.0 / 30.0)
        .renderer(renderer)
        .build_ui(move |ui| {
            // Install the app theme so renders match what the user sees (light bg, the LABEL_PILL
            // reads subtly over it — on egui's default dark bg the contrast is misleading).
            hiker_theme::Theme.install(ui.ctx());
            let src = EntityGraphSource::new(&d, size_by_loc, None, g.as_ref().as_ref())
                .with_importance(&imp);
            // Bundling is OFF by default (the full dense graph); the per-scenario loop turns it ON only
            // for the bundle-open / reveal-mid scenarios that exercise it. status: code-graph-bundling
            st.borrow_mut().ui(ui, &src, |_, _, _, _, _| {});
        });

    for _ in 0..SETTLE_STEPS {
        harness.step();
    }

    for sc in &scenarios {
        // Re-fit to the overview, then apply the scenario zoom (about the pane centre).
        {
            let mut s = state.borrow_mut();
            s.needs_fit = true;
        }
        harness.step(); // consumes needs_fit → fitted overview
        {
            let mut s = state.borrow_mut();
            // Bundling is the opt-in toggle: ON only for the scenarios that exercise it, OFF (full
            // dense graph — the default) for overview/zoom/hover/click. status: code-graph-bundling
            s.bundling = sc.bundle_open || sc.reveal_mid;
            s.view.zoom *= sc.zoom;
            if sc.bundle_open {
                // Zoom INTO one bundle: center on the most-populous container's laid-out position
                // so, if containment co-location works, its members fill the viewport center.
                if let Some(&pos) = s.positions.get(bundle_target) {
                    s.center_on(pos);
                }
            } else if sc.zoom > 1.0 && !s.positions.is_empty() {
                // Re-center on the layout centroid (mass), so zooming in frames the dense cluster
                // rather than drifting it off a corner about the pane center.
                let sum = s.positions.iter().fold(egui::Vec2::ZERO, |a, &p| a + p);
                let centroid = sum / s.positions.len() as f32;
                s.center_on(centroid);
            }
        }
        // Settle one frame at the scenario zoom so labels are placed, then resolve a pane-centre
        // hover/click to the centre of the drawn LABEL nearest the pane centre — hovering a label
        // always resolves to its node through the label hit-test, so the pointer reliably lands on a
        // VISIBLE (labelled) node. A raw centre-of-pane hover often misses: the densest node near the
        // centroid is usually culled into a bundle, and the node hit-test ignores culled nodes.
        harness.step();
        // Mid-animation capture: the step just above is the FIRST interactive paint at the zoomed-in
        // camera, so it's the frame the bundle dissolved — its members flipped culled→visible, reset
        // their reveal tween to ~0, and took one small step. We render NOW (one or two frames in) so
        // the members are caught clustered tight around the parent, mid-fly-out. We deliberately do
        // NOT run the settle drain below, which would let them spread to their settled spots.
        // status: code-graph-bundling
        if sc.reveal_mid {
            harness.step(); // one more small advance so the members are clearly off the parent but tight
            let prog = state.borrow().reveal_progress_for_demo();
            eprintln!(
                "graph_harness: {} mid-flight reveal progress min/max = {:.3}/{:.3} ({} in-flight)",
                sc.name, prog.0, prog.1, prog.2,
            );
            let out = out_dir.join(format!("{}.png", sc.name));
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| harness.render())) {
                Ok(Ok(image)) => match image.save(&out) {
                    Ok(()) => eprintln!("graph_harness: wrote {} ({}x{})", out.display(), image.width(), image.height()),
                    Err(e) => eprintln!("graph_harness: save {}: {e}", out.display()),
                },
                Ok(Err(e)) => eprintln!("graph_harness: render {}: {e}", sc.name),
                Err(_) => eprintln!("graph_harness: render panicked for {}", sc.name),
            }
            continue;
        }
        // Every other scenario renders the SETTLED graph: drain the un-bundling tween (REVEAL_DUR /
        // step_dt ≈ 11 frames) so a freshly-dissolved bundle (e.g. bundle-open) shows its members at
        // their FINAL spread, the fair comparison baseline for bundle-reveal-mid. status: code-graph-bundling
        for _ in 0..14 {
            harness.step();
        }
        let snap_to_node =
            |p: egui::Pos2| -> egui::Pos2 { state.borrow().nearest_label_center(p).unwrap_or(p) };
        // Inject pointer input (hover/click) if any, then step so it takes effect.
        match sc.input {
            Input::None => {}
            Input::Hover(p) => {
                let p = snap_to_node(p);
                harness.input_mut().events.push(egui::Event::PointerMoved(p));
            }
            Input::Click(p) => {
                let p = snap_to_node(p);
                harness.input_mut().events.push(egui::Event::PointerMoved(p));
                harness.input_mut().events.push(egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                });
                harness.input_mut().events.push(egui::Event::PointerButton {
                    pos: p,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                });
                // The engine never sets `selected_node` itself (the app's lens does, from the doc).
                // Mirror that here: the clicked node becomes the selection, which is what fires the
                // affine glide-to-selection. Read the nearest label's node from THIS frame's hit-test.
                let selected = state.borrow().nearest_label_node(p);
                state.borrow_mut().selected_node = selected;
            }
        }
        harness.step();
        harness.step(); // a second frame so label-hit (1-frame lag) + hover decoration settle
        // Drain the affine glide-to-selection (GLIDE_DUR 0.4s / step_dt ≈ 12 frames) so a clicked
        // node ends CENTERED in the captured frame. Inert for non-click scenarios (no selection
        // change → no glide). status: code-graph
        for _ in 0..14 {
            harness.step();
        }

        let out = out_dir.join(format!("{}.png", sc.name));
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| harness.render())) {
            Ok(Ok(image)) => match image.save(&out) {
                Ok(()) => eprintln!("graph_harness: wrote {} ({}x{})", out.display(), image.width(), image.height()),
                Err(e) => eprintln!("graph_harness: save {}: {e}", out.display()),
            },
            Ok(Err(e)) => eprintln!("graph_harness: render {}: {e}", sc.name),
            Err(_) => eprintln!("graph_harness: render panicked for {}", sc.name),
        }
    }
}

/// The display node index with the MOST direct children in `display`'s (rewritten-to-display)
/// `parent` tree, and that child count — a prominent container (a `code:module` / `code:package`)
/// to zoom into for the `bundle-open` test. Returns `(0, 0)` when the graph carries no containment.
/// status: code-graph-containment-layout
fn most_populous_container(display: &EntityGraph) -> (usize, usize) {
    let mut child_count = vec![0usize; display.nodes.len()];
    for node in &display.nodes {
        if let Some(p) = node.parent {
            if p < child_count.len() {
                child_count[p] += 1;
            }
        }
    }
    child_count
        .into_iter()
        .enumerate()
        .max_by_key(|&(_, c)| c)
        .map_or((0, 0), |(i, c)| (i, c))
}

/// A code-graph engine configured like the app's primary view (label pill, force layout).
fn make_state() -> State {
    let mut state = State::new(Style::flat(), LayoutKind::ForceDirected);
    state.style.label_bg = Some(LABEL_PILL);
    state
}
