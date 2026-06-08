//! Headless snapshot of an inline-markdown pipe table (Phase A:
//! `widget-table-render`).
//!
//! Builds a `BlockWidget` whose `paint_list` emits the same primitives the app's
//! real `TableWidget` does — header background, grid rules, and one
//! `BlockPaint::RichText` per cell — then renders it through the genuine
//! `editor-egui` block painter (`paint_native`'s `RichText` arm: a wrapping
//! multi-format `LayoutJob`) to a PNG. The cells carry bold / italic / inline
//! code / strikethrough / link runs so the snapshot proves the styling renders
//! (not the literal `**…**` source).
//!
//! The column-sizing math here is a faithful mirror of the app's
//! `TableWidget::column_widths` (style-aware natural/floor widths, stretch-to-fill
//! when content fits) so the PNG reflects what the real widget paints. Set
//! `TABLE_SNAPSHOT_LEGACY=1` to render with the pre-fix sizing (flat char ratio,
//! no stretch) for a before/after comparison.
//!
//! Self-contained (editor crates only) for the feature-unification reason called
//! out in the Cargo manifest. Run:  cargo run -p table-snapshot
//! Output: target/table-snapshot.png

#[cfg(feature = "screenshot")]
use anyhow::Result;
use editor_core::decoration::{BlockPaint, BlockWidget, Color, StyledRun, TextAlign};

#[cfg(feature = "screenshot")]
use std::sync::Arc;
#[cfg(feature = "screenshot")]
use editor_core::decoration::{BlockSide, Decoration, Set};
#[cfg(feature = "screenshot")]
use editor_core::rangeset::RangeSet;
#[cfg(feature = "screenshot")]
use editor_core::state::Editor;
#[cfg(feature = "screenshot")]
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
#[cfg(feature = "screenshot")]
use editor_view::viewport::ViewState;

const FONT_SIZE: f32 = 16.0;
const LINE_H_RATIO: f32 = 1.35;
const CELL_PAD_X: f32 = 8.0;
const CELL_PAD_Y: f32 = 4.0;
const RULE_W: f32 = 1.0;
/// Content box width the table fills — a realistic editor page width.
const CONTENT_W: f32 = 1000.0;
#[cfg(feature = "screenshot")]
const VIEW_W: f32 = 1040.0;
#[cfg(feature = "screenshot")]
const VIEW_H: f32 = 360.0;

// --- Sizing factors mirrored from the app's `tables.rs` ---------------------
const CHAR_W_RATIO: f32 = 0.52;
const BOLD_W_FACTOR: f32 = 1.12;
const CODE_W_FACTOR: f32 = 1.25;
const WIDTH_SAFETY: f32 = 1.06;

const TEXT: Color = Color::rgb(30, 30, 30);
const CODE_BG: Color = Color::rgba(120, 120, 120, 40);
const HEADER_BG: Color = Color::rgba(170, 120, 220, 30);
const RULE: Color = Color::rgba(120, 120, 120, 140);

/// A demo table: each cell is a run list (markers already stripped, exactly what
/// the app's inline parser produces). A first column of **bold** short widget
/// names (including the regression case "WaveDrom"), plus two wider text columns.
struct DemoTable {
    rows: Vec<Vec<Vec<StyledRun>>>,
    aligns: Vec<TextAlign>,
}

fn plain(s: &str) -> StyledRun {
    StyledRun::plain(s, TEXT)
}

fn bold(s: &str) -> StyledRun {
    StyledRun { bold: true, ..StyledRun::plain(s, TEXT) }
}

fn code(s: &str) -> StyledRun {
    StyledRun { code: true, bg: Some(CODE_BG), ..StyledRun::plain(s, TEXT) }
}

fn demo_table() -> DemoTable {
    DemoTable {
        rows: vec![
            vec![vec![bold("Widget")], vec![plain("You write")], vec![plain("Renders as")]],
            vec![
                vec![bold("WaveDrom")],
                vec![code("```wavedrom")],
                vec![plain("a timing / signal waveform diagram")],
            ],
            vec![
                vec![bold("Mermaid")],
                vec![code("```mermaid")],
                vec![plain("flowcharts, sequence and state diagrams")],
            ],
            vec![
                vec![bold("Math")],
                vec![code("$$ … $$")],
                vec![plain("a centered display equation")],
            ],
        ],
        aligns: vec![TextAlign::Left, TextAlign::Left, TextAlign::Left],
    }
}

struct TableBlock {
    table: DemoTable,
    legacy: bool,
}

/// Width (logical pt) the cell's styled runs need on one unwrapped line, honoring
/// bold (faux-bold) and `code` (monospace) running wider than a plain glyph. With
/// `legacy`, falls back to the flat char-count estimate the pre-fix code used.
fn cell_natural_width(runs: &[StyledRun], char_w: f32, legacy: bool) -> f32 {
    let pad = CELL_PAD_X * 2.0;
    if legacy {
        let n: usize = runs.iter().map(|r| r.text.chars().count()).sum();
        return n as f32 * char_w + pad;
    }
    let w: f32 = runs.iter().map(|r| run_width(r, char_w)).sum();
    w * WIDTH_SAFETY + pad
}

fn run_width(run: &StyledRun, char_w: f32) -> f32 {
    let n = run.text.chars().count() as f32;
    let factor = if run.code {
        CODE_W_FACTOR
    } else if run.bold {
        BOLD_W_FACTOR
    } else {
        1.0
    };
    n * char_w * factor
}

impl TableBlock {
    fn col_count(&self) -> usize {
        self.table.rows.iter().map(Vec::len).max().unwrap_or(1).max(1)
    }

    fn column_widths(&self, font_px: f32) -> Vec<f32> {
        let cols = self.col_count();
        let char_w = font_px * CHAR_W_RATIO;
        let mut natural = vec![0.0_f32; cols];
        for cells in &self.table.rows {
            for (i, runs) in cells.iter().enumerate().take(cols) {
                let nat = cell_natural_width(runs, char_w, self.legacy);
                if nat > natural[i] {
                    natural[i] = nat;
                }
            }
        }
        let nat_sum: f32 = natural.iter().sum();
        if self.legacy || nat_sum >= CONTENT_W || nat_sum <= 0.0 {
            return natural;
        }
        let slack = CONTENT_W - nat_sum;
        (0..cols).map(|i| natural[i] + slack * (natural[i] / nat_sum)).collect()
    }

    fn row_height(&self) -> f32 {
        FONT_SIZE * LINE_H_RATIO + CELL_PAD_Y * 2.0
    }
}

impl BlockWidget for TableBlock {
    fn measure(&self, _font_size: f32, _width: f32) -> f32 {
        self.row_height() * self.table.rows.len() as f32
    }

    fn widget_id(&self) -> u64 {
        0x7AB1E
    }

    fn paint_list(&self, font_size: f32, _width: f32) -> Option<Vec<BlockPaint>> {
        let widths = self.column_widths(font_size);
        let total_w: f32 = widths.iter().sum();
        let row_h = self.row_height();
        let mut list = Vec::new();

        // Header background strip.
        list.push(BlockPaint::Rect { x: 0.0, y: 0.0, w: total_w, h: row_h, color: HEADER_BG });

        let mut y = 0.0_f32;
        for cells in &self.table.rows {
            let mut x = 0.0_f32;
            for (i, runs) in cells.iter().enumerate() {
                let col_w = widths.get(i).copied().unwrap_or(0.0);
                let align = self.table.aligns.get(i).copied().unwrap_or(TextAlign::Left);
                list.push(BlockPaint::RichText {
                    x: x + CELL_PAD_X,
                    y: y + CELL_PAD_Y,
                    runs: runs.clone(),
                    max_width: (col_w - CELL_PAD_X * 2.0).max(font_size),
                    align,
                });
                x += col_w;
            }
            y += row_h;
            list.push(BlockPaint::Line {
                from: (0.0, y),
                to: (total_w, y),
                width: RULE_W,
                color: RULE,
            });
        }
        // Vertical separators + top rule.
        list.push(BlockPaint::Line { from: (0.0, 0.0), to: (total_w, 0.0), width: RULE_W, color: RULE });
        let mut x = 0.0_f32;
        for w in widths.iter().chain(std::iter::once(&0.0_f32)) {
            list.push(BlockPaint::Line { from: (x, 0.0), to: (x, y), width: RULE_W, color: RULE });
            x += w;
        }
        Some(list)
    }
}

fn legacy_mode() -> bool {
    std::env::var("TABLE_SNAPSHOT_LEGACY").is_ok_and(|v| v == "1")
}

#[cfg(feature = "screenshot")]
fn decorations(legacy: bool) -> Set {
    let widget: Arc<dyn BlockWidget> = Arc::new(TableBlock { table: demo_table(), legacy });
    RangeSet::from_iter([(
        0..1,
        Decoration::BlockWidget { side: BlockSide::Below, widget },
    )])
}

#[cfg(feature = "screenshot")]
fn main() -> Result<()> {
    let legacy = legacy_mode();
    let name = if legacy { "target/table-snapshot-before.png" } else { "target/table-snapshot-after.png" };
    let out = std::path::PathBuf::from(name);
    let (w, h) = render_png(&out, legacy).map_err(anyhow::Error::msg)?;
    println!("wrote {} ({w}x{h})", out.display());
    Ok(())
}

#[cfg(not(feature = "screenshot"))]
fn main() {
    // Sanity-build the table widget (exercises the editor-core RichText/StyledRun
    // surface) without the egui_kittest dep; the PNG needs --features screenshot.
    let widget = TableBlock { table: demo_table(), legacy: legacy_mode() };
    let list = widget.paint_list(FONT_SIZE, CONTENT_W).expect("paint list");
    let rich = list.iter().filter(|p| matches!(p, BlockPaint::RichText { .. })).count();
    println!(
        "table built: {} paint primitives ({rich} rich-text cells). \
         Build with --features screenshot to render the PNG.",
        list.len(),
    );
}

#[cfg(feature = "screenshot")]
fn render_png(out: &std::path::Path, legacy: bool) -> Result<(u32, u32), String> {
    let renderer = egui_kittest::wgpu::WgpuTestRenderer::new();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(VIEW_W, VIEW_H))
        .renderer(renderer)
        .build_ui(move |ui| {
            ui.ctx().set_visuals(egui::Visuals::light());
            // A short doc so the below-line block has a host line; the table
            // renders in the gap under line 0.
            let mut editor = Editor::new("Inline-markdown table:\n");
            let mut view = ViewState {
                font_size: FONT_SIZE,
                line_height: FONT_SIZE * LINE_H_RATIO,
                width: VIEW_W,
                height: VIEW_H,
                ..Default::default()
            };
            view.wrap_map.set_enabled(false);
            view.sync_to(&editor);
            view.decorations.clear();
            view.decorations.push_with_heights(decorations(legacy));

            let mut paint_cache = PaintCache::default();
            let rect = egui::Rect::from_min_size(ui.min_rect().min, egui::vec2(VIEW_W, VIEW_H));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            EditorWidget::new(&mut editor, &mut view)
                .with_paint_cache(&mut paint_cache)
                .show(&mut child);
        });
    harness.run();
    let rendered = harness.render().map_err(|e| format!("wgpu render: {e}"))?;
    let (w, h) = (rendered.width(), rendered.height());
    // Re-wrap the raw RGBA8 buffer in this tool's `image` version so the png
    // encoder feature is guaranteed present (kittest's transitive `image` may be
    // built without it).
    let buf = image::RgbaImage::from_raw(w, h, rendered.into_raw())
        .ok_or_else(|| "raw buffer size mismatch".to_string())?;
    buf.save(out).map_err(|e| format!("save png: {e}"))?;
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretches_to_content_width_when_fits() {
        let t = TableBlock { table: demo_table(), legacy: false };
        let sum: f32 = t.column_widths(FONT_SIZE).iter().sum();
        assert!((sum - CONTENT_W).abs() < 0.5, "fits-case fills content width: {sum}");
    }

    #[test]
    fn legacy_does_not_stretch() {
        let t = TableBlock { table: demo_table(), legacy: true };
        let sum: f32 = t.column_widths(FONT_SIZE).iter().sum();
        assert!(sum < CONTENT_W, "legacy renders at natural width: {sum}");
    }
}
