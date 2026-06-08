//! Headless PNG snapshots of the canvas projection lens (`proj-canvas-mode`),
//! mirroring `hiker-render/htmlview/examples/snapshot.rs`: render a hardcoded
//! scatter of text cards at three `ProjectionKind`s through `CanvasView::show_static`
//! and save one PNG per mode plus a 3-up comparison.
//!
//! - `canvas-proj-off.png` — the Affine (Off) lens: a plain grid of axis-aligned
//!   cards, byte-identical to the non-projected canvas.
//! - `canvas-proj-fisheye.png` — Fisheye: cards near the focus larger, peripheral
//!   cards smaller (some collapsed to LOD dots), all STILL axis-aligned.
//! - `canvas-proj-poincare.png` — Poincaré: cards compressed toward the disk,
//!   center cards bigger, rim cards small / dots, axis-aligned.
//! - `canvas-proj-compare.png` — the three side by side.
//!
//! If wgpu cannot initialize (no GPU/software backend in a headless env) the
//! example prints a clear SKIP and exits 0 rather than failing the build.

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use canvas_view::content::NoContentRenderer;
use canvas_view::widget::CanvasView;
use hiker_canvas::color::Color;
use hiker_canvas::geometry::Point;
use hiker_projection::{ProjectionConfig, ProjectionKind};
use egui::Vec2 as EVec2;
use hiker_canvas::model::{Canvas, Edge, Node, NodeKind};

const VIEW_W: f32 = 700.0;
const VIEW_H: f32 = 500.0;

/// A 4×4 scatter of text cards spread over a wide world box, so under a lens the
/// central cards magnify and the corner cards fall toward the rim, wired into a
/// grid mesh (each card linked to its right + down neighbour) so the projected
/// edges are visible: straight béziers under Off, bulging under Fisheye, and
/// geodesic-bowed under Poincaré.
fn sample_canvas() -> Canvas {
    let mut canvas = Canvas::default();
    let id_at = |row: i32, col: i32| format!("n{}", row * 4 + col + 1);
    for row in 0..4 {
        for col in 0..4 {
            let x = i64::from(-900 + col * 600);
            let y = i64::from(-700 + row * 470);
            canvas.nodes.push(Node {
                id: id_at(row, col),
                x,
                y,
                width: 260,
                height: 170,
                color: None,
                kind: NodeKind::Text { text: format!("Card {}\nrow {row} col {col}", row * 4 + col + 1) },
                extra: BTreeMap::new(),
            });
        }
    }
    // Grid mesh: connect each card to its right and down neighbour.
    let mut eid = 0;
    let mut link = |canvas: &mut Canvas, from: String, to: String| {
        eid += 1;
        canvas.edges.push(Edge {
            id: format!("e{eid}"),
            from_node: from,
            to_node: to,
            from_side: None,
            to_side: None,
            from_end: None,
            to_end: None,
            color: None,
            label: None,
            extra: BTreeMap::new(),
        });
    };
    for row in 0..4 {
        for col in 0..4 {
            if col < 3 {
                link(&mut canvas, id_at(row, col), id_at(row, col + 1));
            }
            if row < 3 {
                link(&mut canvas, id_at(row, col), id_at(row + 1, col));
            }
        }
    }
    canvas
}

/// The projection config for a mode (Off = default Affine; the lenses use a
/// moderate strength so the warp reads clearly in a still frame).
fn cfg_for(kind: ProjectionKind) -> ProjectionConfig {
    match kind {
        ProjectionKind::Affine => ProjectionConfig::default(),
        _ => ProjectionConfig { kind, strength: 1.2, size_falloff: 1.0, geodesic_segments: 16 },
    }
}

/// Render the sample canvas at an explicit projection `cfg`, saving the PNG to
/// `out_path`. Mirrors [`render_mode`] but lets the high-strength Poincaré frame
/// pick its own `strength`/`geodesic_segments`. [proj-card-fill]
fn render_cfg(cfg: ProjectionConfig, out_path: &Path) -> Result<image::RgbaImage, String> {
    let canvas = sample_canvas();
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(EVec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(18));
            let mut view = CanvasView::new();
            view.set_grid(false);
            view.fit(rect, &canvas);
            *view.projection_mut() = cfg;
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });

    harness.run();
    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok(image)
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the sample canvas at `kind` into a wgpu-backed harness, saving the PNG
/// to `out_path`. Returns the saved image (for the composite) + its dimensions,
/// or a human-readable error string on failure.
fn render_mode(kind: ProjectionKind, out_path: &Path) -> Result<image::RgbaImage, String> {
    let canvas = sample_canvas();
    let cfg = cfg_for(kind);

    // Building the wgpu renderer initializes a device; on a machine with no usable
    // (even software) backend `WgpuTestRenderer::new` panics. Trap that first.
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(EVec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            // Dark background so the cards read; fill the whole pane first.
            let rect = ui.max_rect();
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_gray(18));
            let mut view = CanvasView::new();
            view.set_grid(false);
            // Fit the affine camera to content first, then engage the lens — the
            // lens warps within the framed view.
            view.fit(rect, &canvas);
            *view.projection_mut() = cfg;
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });

    harness.run();

    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok(image)
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// The world-space center of a node (its top-left + half its size).
fn node_center(node: &Node) -> Point {
    Point::new(
        node.x as f64 + node.width as f64 / 2.0,
        node.y as f64 + node.height as f64 / 2.0,
    )
}

/// Render ONE Poincaré fly-to frame at eased glide fraction `e`: the peripheral
/// card `target_id` glides from the disk rim (`e = 0`, resting) to the disk
/// centre (`e = 1`, fully recentred), the rest of the board recentring
/// hyperbolically around it. Returns the saved-but-in-memory image.
fn render_nav_frame(target_id: &str, e: f32) -> Result<image::RgbaImage, String> {
    let canvas = sample_canvas();
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let target_id = target_id.to_owned();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(EVec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(18));
            let mut view = CanvasView::new();
            view.set_grid(false);
            // Fit the affine camera to content, engage Poincaré, then set the
            // navigation recentre for this frame's glide fraction.
            view.fit(rect, &canvas);
            *view.projection_mut() = cfg_for(ProjectionKind::Poincare);
            // Resolve the peripheral card's pre-nav disk point as the fly-to
            // target (needs the lens framing refreshed first).
            view.update_lens_for_demo(&canvas);
            let target_node = canvas.nodes.iter().find(|n| n.id == target_id).expect("target card");
            let target_disk = view.disk_point_for_demo(node_center(target_node));
            view.set_nav_flyto_for_demo(target_disk, e);
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });

    harness.run();
    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => Ok(image),
        Ok(Err(err)) => Err(format!("wgpu render failed: {err}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the 4-frame Poincaré fly-to / recentre filmstrip and save it as one
/// wide PNG: the peripheral corner card `n1` glides from the rim to the disk
/// centre across `e ∈ {0, .33, .66, 1}`, the board recentring around it.
fn save_nav_strip(out_path: &Path) -> Result<(), String> {
    let mut frames = Vec::new();
    for e in [0.0_f32, 0.33, 0.66, 1.0] {
        frames.push(render_nav_frame("n1", e)?);
    }
    save_compare(&frames, out_path)
}

/// Lay the three mode renders side by side into one wide PNG.
fn save_compare(images: &[image::RgbaImage], out_path: &Path) -> Result<(), String> {
    let gap = 8u32;
    let h = images.iter().map(image::RgbaImage::height).max().unwrap_or(0);
    let total_w: u32 = images.iter().map(image::RgbaImage::width).sum::<u32>() + gap * (images.len() as u32 + 1);
    let mut canvas = image::RgbaImage::from_pixel(total_w, h + gap * 2, image::Rgba([24, 24, 28, 255]));
    let mut x = gap;
    for img in images {
        image::imageops::overlay(&mut canvas, img, i64::from(x), i64::from(gap));
        x += img.width() + gap;
    }
    canvas.save(out_path).map_err(|e| format!("save compare png: {e}"))
}

/// A deliberately messy canvas for the auto-arrange demo: seven cards scattered
/// at random-looking positions wired into a small DAG (a root fanning to two
/// branches that reconverge, plus a side chain), with one group frame loosely
/// thrown around two of the cards. After tidy, the DAG should fall into clean
/// top-to-bottom ranks and the group frame should wrap its (re-placed) members.
fn messy_canvas() -> Canvas {
    let mut canvas = Canvas::default();
    let cards = [
        ("root", -400_i64, 120_i64),
        ("a", 650, -260),
        ("b", -720, 540),
        ("c", 480, 700),
        ("d", -120, -360),
        ("e", 900, 260),
        ("sink", -560, -120),
    ];
    for (id, x, y) in cards {
        canvas.nodes.push(Node {
            id: id.to_owned(),
            x,
            y,
            width: 200,
            height: 120,
            color: None,
            kind: NodeKind::Text { text: format!("{id}\nscattered") },
            extra: BTreeMap::new(),
        });
    }
    // One loose group frame around where `a` and `e` happen to scatter.
    canvas.nodes.push(Node {
        id: "grp".to_owned(),
        x: 430,
        y: -320,
        width: 720,
        height: 700,
        color: None,
        kind: NodeKind::Group {
            label: Some("cluster".to_owned()),
            background: None,
            background_style: None,
        },
        extra: BTreeMap::new(),
    });

    let mut eid = 0;
    let mut link = |canvas: &mut Canvas, from: &str, to: &str| {
        eid += 1;
        canvas.edges.push(Edge {
            id: format!("e{eid}"),
            from_node: from.to_owned(),
            to_node: to.to_owned(),
            from_side: None,
            to_side: None,
            from_end: None,
            to_end: None,
            color: None,
            label: None,
            extra: BTreeMap::new(),
        });
    };
    link(&mut canvas, "root", "a");
    link(&mut canvas, "root", "b");
    link(&mut canvas, "a", "c");
    link(&mut canvas, "b", "c");
    link(&mut canvas, "root", "d");
    link(&mut canvas, "d", "e");
    link(&mut canvas, "e", "sink");
    canvas
}

/// Render `canvas` in the plain Off (Affine) lens with the grid on, fitted to
/// content, saving the PNG to `out_path`. Used for the before/after tidy frames.
fn render_plain(canvas: &Canvas, out_path: &Path) -> Result<image::RgbaImage, String> {
    let canvas = canvas.clone();
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(EVec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(18));
            let mut view = CanvasView::new();
            view.set_grid(false);
            view.fit(rect, &canvas);
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });

    harness.run();
    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok(image)
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

/// Render the auto-arrange before/after pair and their 2-up comparison. Returns
/// the saved-but-in-memory before/after images for the composite, or a
/// human-readable error.
fn save_arrange_demo(target: &Path) -> Result<(), String> {
    use hiker_canvas::tidy::{auto_arrange, ArrangeOpts};

    let before = messy_canvas();
    let before_img = render_plain(&before, &target.join("canvas-arrange-before.png"))?;

    // Apply the pure tidy ops to a clone — exactly what the menu verb commits.
    let mut after = before.clone();
    for op in auto_arrange(&before, ArrangeOpts::default()) {
        op.apply(&mut after);
    }
    let after_img = render_plain(&after, &target.join("canvas-arrange-after.png"))?;

    save_compare(&[before_img, after_img], &target.join("canvas-arrange-compare.png"))
}

/// Number of text cards in the large "text-soup at scale" canvas. A few hundred
/// reproduces the hairball the bare-dot tier (`proj-lod-ladder`) is meant to fix.
const LARGE_CARD_COUNT: usize = 280;

/// A deterministic LARGE canvas — ~280 small text cards laid out as a clustered
/// tree (a root fanning into branch hubs, each hub fanning into leaves) wired
/// with edges, spread over a wide world box. At fit / zoom-out almost every card
/// falls below `BARE_DOT_PX`, so without the deepest LOD tier this renders as a
/// soup of overlapping frames + titles; with it, a clean colored constellation.
/// Cluster cards carry preset colours so the dots read as colored points.
fn large_canvas() -> Canvas {
    let mut canvas = Canvas::default();
    // A pseudo-random but fully deterministic position/cluster generator (a small
    // LCG) so the snapshot is byte-stable across runs without an rng dependency.
    let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) as f64 / f64::from(u32::MAX)
    };
    // 8 cluster hubs arranged on a wide ring; each hub seeds a fan of leaves.
    let hubs = 8usize;
    let mut prev_in_cluster: Vec<Option<String>> = vec![None; hubs];
    for i in 0..LARGE_CARD_COUNT {
        let cluster = i % hubs;
        let angle = std::f64::consts::TAU * cluster as f64 / hubs as f64;
        let hub_x = angle.cos() * 6000.0;
        let hub_y = angle.sin() * 6000.0;
        // Scatter leaves in a blob around the hub centre.
        let jitter_x = (next() - 0.5) * 4200.0;
        let jitter_y = (next() - 0.5) * 4200.0;
        let id = format!("c{cluster}_{i}");
        canvas.nodes.push(Node {
            id: id.clone(),
            x: (hub_x + jitter_x) as i64,
            y: (hub_y + jitter_y) as i64,
            width: 260,
            height: 170,
            // Preset slots 1..=6 so the dot constellation reads as coloured points.
            color: Some(Color::Preset((cluster % 6 + 1) as u8)),
            kind: NodeKind::Text { text: format!("Note {i}\ncluster {cluster}") },
            extra: BTreeMap::new(),
        });
        // Chain each leaf to the previous leaf in its cluster (a tree-ish mesh).
        if let Some(prev) = prev_in_cluster[cluster].replace(id.clone()) {
            canvas.edges.push(Edge {
                id: format!("e{i}"),
                from_node: prev,
                to_node: id,
                from_side: None,
                to_side: None,
                from_end: None,
                to_end: None,
                color: None,
                label: None,
                extra: BTreeMap::new(),
            });
        }
    }
    canvas
}

/// Render the large canvas at projection `cfg`, fitted to content (the camera
/// frames the whole big canvas, so almost every card lands below `BARE_DOT_PX`),
/// saving the PNG to `out_path`. The Poincaré frame produces the dot constellation.
fn render_large_fit(cfg: ProjectionConfig, out_path: &Path) -> Result<image::RgbaImage, String> {
    let canvas = large_canvas();
    let renderer = std::panic::catch_unwind(AssertUnwindSafe(|| {
        egui_kittest::wgpu::WgpuTestRenderer::new()
    }))
    .map_err(|_| "wgpu backend failed to initialize (no GPU/software device)".to_string())?;

    let mut harness = egui_kittest::Harness::builder()
        .with_size(EVec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            let rect = ui.max_rect();
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_gray(18));
            let mut view = CanvasView::new();
            view.set_grid(false);
            view.fit(rect, &canvas);
            *view.projection_mut() = cfg;
            view.show_static(ui, &canvas, &mut NoContentRenderer);
        });

    harness.run();
    let render = std::panic::catch_unwind(AssertUnwindSafe(|| harness.render()));
    match render {
        Ok(Ok(image)) => {
            image.save(out_path).map_err(|e| format!("save png: {e}"))?;
            Ok(image)
        }
        Ok(Err(e)) => Err(format!("wgpu render failed: {e}")),
        Err(_) => Err("wgpu render panicked".into()),
    }
}

fn main() {
    if std::env::var_os("LIBGL_ALWAYS_SOFTWARE").is_none() {
        // SAFETY: single-threaded at startup, before any rendering thread spawns.
        unsafe { std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1") };
    }

    let target = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/target"));
    let _ = std::fs::create_dir_all(&target);

    let jobs = [
        ("off", ProjectionKind::Affine, target.join("canvas-proj-off.png")),
        ("fisheye", ProjectionKind::Fisheye, target.join("canvas-proj-fisheye.png")),
        ("poincare", ProjectionKind::Poincare, target.join("canvas-proj-poincare.png")),
    ];

    let mut images = Vec::new();
    let mut first_err: Option<String> = None;
    for (label, kind, path) in &jobs {
        match render_mode(*kind, path) {
            Ok(img) => {
                let (w, h) = (img.width(), img.height());
                println!(
                    "OK  {label} -> {} ({w}x{h})",
                    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()).display()
                );
                images.push(img);
            }
            Err(e) => {
                println!("SKIP {label}: {e}");
                first_err.get_or_insert(e);
            }
        }
    }

    if images.len() == jobs.len() {
        let compare = target.join("canvas-proj-compare.png");
        match save_compare(&images, &compare) {
            Ok(()) => println!(
                "OK  compare -> {}",
                std::fs::canonicalize(&compare).unwrap_or(compare).display()
            ),
            Err(e) => println!("SKIP compare: {e}"),
        }
        // High-strength Poincaré: cards near the rim, sharply-bowed geodesics.
        // Validates the neighbor-gap fill sizing (cards still fill the disk, no
        // tiny floaters) and the adaptive geodesic segments (edges stay smooth,
        // no facets) at a strength well above the default. [proj-card-fill]
        let strong = target.join("canvas-proj-poincare-strong.png");
        let strong_cfg = ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 2.2,
            size_falloff: 1.0,
            geodesic_segments: 16,
        };
        match render_cfg(strong_cfg, &strong) {
            Ok(img) => println!(
                "OK  poincare-strong -> {} ({}x{})",
                std::fs::canonicalize(&strong).unwrap_or(strong.clone()).display(),
                img.width(),
                img.height()
            ),
            Err(e) => println!("SKIP poincare-strong: {e}"),
        }
        // The Poincaré navigation filmstrip — only attempted once we know wgpu
        // works (all three single-mode renders succeeded).
        let nav_strip = target.join("canvas-proj-nav-strip.png");
        match save_nav_strip(&nav_strip) {
            Ok(()) => println!(
                "OK  nav-strip -> {}",
                std::fs::canonicalize(&nav_strip).unwrap_or(nav_strip).display()
            ),
            Err(e) => println!("SKIP nav-strip: {e}"),
        }
        // The dagre auto-arrange ("Tidy") before/after demo.
        match save_arrange_demo(&target) {
            Ok(()) => {
                let compare = target.join("canvas-arrange-compare.png");
                println!(
                    "OK  arrange-compare -> {}",
                    std::fs::canonicalize(&compare).unwrap_or(compare).display()
                );
            }
            Err(e) => println!("SKIP arrange: {e}"),
        }
        // LARGE-canvas "text soup at scale" check (`proj-lod-ladder`): ~280 cards.
        // Poincaré: a clean colored DOT constellation with a few readable cards
        // near the centre — NOT overlapping frames + titles. Affine zoomed all the
        // way out (fit frames the whole big canvas, exercising MIN_SCALE): dots /
        // placeholders, the whole canvas fitting, not a frame+text blob.
        let poincare_large = target.join("canvas-proj-poincare-large.png");
        let large_poincare_cfg = ProjectionConfig {
            kind: ProjectionKind::Poincare,
            strength: 1.2,
            size_falloff: 1.0,
            geodesic_segments: 16,
        };
        match render_large_fit(large_poincare_cfg, &poincare_large) {
            Ok(img) => println!(
                "OK  poincare-large -> {} ({}x{})",
                std::fs::canonicalize(&poincare_large).unwrap_or(poincare_large.clone()).display(),
                img.width(),
                img.height()
            ),
            Err(e) => println!("SKIP poincare-large: {e}"),
        }
        let affine_large = target.join("canvas-affine-zoomout-large.png");
        match render_large_fit(ProjectionConfig::default(), &affine_large) {
            Ok(img) => println!(
                "OK  affine-zoomout-large -> {} ({}x{})",
                std::fs::canonicalize(&affine_large).unwrap_or(affine_large.clone()).display(),
                img.width(),
                img.height()
            ),
            Err(e) => println!("SKIP affine-zoomout-large: {e}"),
        }
    } else {
        println!();
        println!(
            "Headless snapshot could not render all modes: {}",
            first_err.unwrap_or_default()
        );
        println!("This environment appears to lack a usable GPU/software (Vulkan/GL) backend.");
        // Do NOT fail the build.
    }
}
