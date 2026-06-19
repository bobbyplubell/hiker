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
use editor_core::decoration::{ChildItem, ChildKind, ChildRect, ChildTexture};

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
/// Taller view for the mixed-block demo (5 rows, each holding a diagram /
/// image scaled into its column). status: widget-table-render
#[cfg(feature = "screenshot")]
const VIEW_H_MIXED: f32 = 720.0;

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

// --- Phase B: a math cell rasterized through the real hiker-math + resvg path,
//     hosted in a composite BlockWidget and drawn by the real editor-egui
//     painter. Mirrors the app's tables::composite_children. ------------------

/// A demo Phase-B table: a text column, an inline-`$…$` column, and a
/// display-`$$…$$` column — each math cell a rasterized texture.
#[cfg(feature = "screenshot")]
struct MathTableBlock {
    /// Per row: each cell is either text runs or a rasterized math texture.
    rows: Vec<Vec<MathCellKind>>,
}

#[cfg(feature = "screenshot")]
enum MathCellKind {
    Text(Vec<StyledRun>),
    Math(MathTex),
}

/// A rasterized math formula: straight RGBA8 + physical size + a stable id.
#[cfg(feature = "screenshot")]
struct MathTex {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    id: u64,
}

#[cfg(feature = "screenshot")]
impl MathTex {
    /// Intrinsic logical (point) width / height (dpr = 1 in the snapshot).
    const fn logical_w(&self) -> f32 {
        self.width as f32
    }
    const fn logical_h(&self) -> f32 {
        self.height as f32
    }
}

/// A populated SVG font database (system fonts + bundled mermaid sans), so resvg
/// renders the `<text>` labels mermaid / wavedrom emit. Mirrors `render.rs`'s
/// `svg_fontdb` — without it every diagram label rasterizes blank.
#[cfg(feature = "screenshot")]
fn svg_fontdb() -> resvg::usvg::fontdb::Database {
    let mut db = resvg::usvg::fontdb::Database::new();
    db.load_system_fonts();
    db.load_font_data(hiker_mermaid::font::FONT_BYTES.to_vec());
    db.set_sans_serif_family(hiker_mermaid::font::FONT_FAMILY);
    db.set_serif_family("Liberation Serif");
    db.set_monospace_family("Liberation Mono");
    db
}

/// Rasterize an SVG document to straight (un-premultiplied) RGBA8, the same way
/// `render.rs::rasterize_svg` does, with a populated fontdb so diagram labels
/// render. status: widget-table-render
#[cfg(feature = "screenshot")]
fn raster_svg(svg: &str, id: u64) -> MathTex {
    let opt = resvg::usvg::Options { fontdb: std::sync::Arc::new(svg_fontdb()), ..Default::default() };
    let rtree = resvg::usvg::Tree::from_data(svg.as_bytes(), &opt).expect("usvg parse");
    let size = rtree.size();
    let (w, h) = (size.width().ceil() as u32, size.height().ceil() as u32);
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w.max(1), h.max(1)).expect("pixmap");
    resvg::render(&rtree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    // Un-premultiply (tiny-skia stores premultiplied; editor blits straight RGBA8).
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3];
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u32::from(*c) * 255 + u32::from(a) / 2) / u32::from(a)).min(255) as u8;
            }
        }
    }
    MathTex { rgba, width: w.max(1), height: h.max(1), id }
}

/// Rasterize a LaTeX `src` to straight RGBA8 via the SAME hiker-math → resvg
/// path `render.rs` uses. `display` picks block vs. inline style.
/// status: widget-table-render
#[cfg(feature = "screenshot")]
fn raster_math(src: &str, display: bool, id: u64) -> MathTex {
    use hiker_math::{MathOptions, MathStyle, render_latex};
    let opts = MathOptions {
        font_size_px: FONT_SIZE,
        color: [30, 30, 30, 255],
        style: if display { MathStyle::Display } else { MathStyle::Inline },
    };
    let r = render_latex(src, &opts).expect("math renders");
    raster_svg(&r.svg, id)
}

/// Rasterize a mermaid `src` via the SAME hiker-mermaid → resvg path the app uses.
/// status: widget-table-render
#[cfg(feature = "screenshot")]
fn raster_mermaid(src: &str, id: u64) -> MathTex {
    use hiker_mermaid::{MermaidOptions, render};
    let opts = MermaidOptions { font_size_px: FONT_SIZE, ..Default::default() };
    let r = render(src, &opts).expect("mermaid renders");
    raster_svg(&r.svg, id)
}

/// Rasterize a WaveJSON `src` via the SAME hiker-wavedrom → resvg path the app
/// uses. status: widget-table-render
#[cfg(feature = "screenshot")]
fn raster_wavedrom(src: &str, id: u64) -> MathTex {
    use hiker_wavedrom::{WaveDromOptions, render};
    let opts = WaveDromOptions { font_size_px: FONT_SIZE, ..Default::default() };
    let r = render(src, &opts).expect("wavedrom renders");
    raster_svg(&r.svg, id)
}

/// Decode a small synthetic PNG (a colored gradient) the way the app's
/// `render_image` would decode an `![alt](path)` cell's file — proving an image
/// cell hosts a real raster. status: widget-table-render
#[cfg(feature = "screenshot")]
fn raster_image(id: u64) -> MathTex {
    let (w, h) = (96u32, 60u32);
    let mut buf = image::RgbaImage::new(w, h);
    for (x, y, px) in buf.enumerate_pixels_mut() {
        let r = (x * 255 / w) as u8;
        let g = (y * 255 / h) as u8;
        *px = image::Rgba([r, g, 160, 255]);
    }
    MathTex { rgba: buf.into_raw(), width: w, height: h, id }
}

#[cfg(feature = "screenshot")]
fn math_demo_table() -> MathTableBlock {
    MathTableBlock {
        rows: vec![
            vec![
                MathCellKind::Text(vec![bold("Name")]),
                MathCellKind::Text(vec![bold("Inline")]),
                MathCellKind::Text(vec![bold("Display")]),
            ],
            vec![
                MathCellKind::Text(vec![plain("Pythagoras")]),
                MathCellKind::Math(raster_math("a^2 + b^2 = c^2", false, 0x101)),
                MathCellKind::Math(raster_math("\\frac{a+b}{c+d}", true, 0x102)),
            ],
            vec![
                MathCellKind::Text(vec![plain("Sum")]),
                MathCellKind::Math(raster_math("x_i^2", false, 0x103)),
                MathCellKind::Math(raster_math("\\sum_{i=0}^{n} x_i", true, 0x104)),
            ],
        ],
    }
}

/// A demo Phase-B table mixing all the block kinds the cell path now hosts: a
/// text label column plus a mermaid flowchart, a wavedrom waveform, a display
/// formula, and a decoded image — each a rasterized texture child, proving the
/// kind-agnostic cell-block path. status: widget-table-render
#[cfg(feature = "screenshot")]
fn mixed_demo_table() -> MathTableBlock {
    MathTableBlock {
        rows: vec![
            vec![
                MathCellKind::Text(vec![bold("Kind")]),
                MathCellKind::Text(vec![bold("In a cell")]),
                MathCellKind::Text(vec![bold("Notes")]),
            ],
            vec![
                MathCellKind::Text(vec![plain("Mermaid")]),
                MathCellKind::Math(raster_mermaid("graph TD; A-->B; B-->C", 0x201)),
                MathCellKind::Text(vec![plain("a flowchart rendered inside the cell")]),
            ],
            vec![
                MathCellKind::Text(vec![plain("WaveDrom")]),
                MathCellKind::Math(raster_wavedrom(
                    "{ signal: [{ name: 'clk', wave: 'p...' }, { name: 'd', wave: '01.0' }] }",
                    0x202,
                )),
                MathCellKind::Text(vec![plain("a timing waveform")]),
            ],
            vec![
                MathCellKind::Text(vec![plain("Math")]),
                MathCellKind::Math(raster_math("\\frac{a+b}{c+d}", true, 0x203)),
                MathCellKind::Text(vec![plain("a display formula")]),
            ],
            vec![
                MathCellKind::Text(vec![plain("Image")]),
                MathCellKind::Math(raster_image(0x204)),
                MathCellKind::Text(vec![plain("a decoded raster image")]),
            ],
        ],
    }
}

#[cfg(feature = "screenshot")]
const BLOCK_W_CAP: f32 = 22.0;

#[cfg(feature = "screenshot")]
impl MathTableBlock {
    fn col_count(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(1).max(1)
    }

    /// Column widths reconciling text natural width against block intrinsic
    /// width (capped) — mirrors the app's `column_widths` for the fits-case.
    fn column_widths(&self, font_px: f32) -> Vec<f32> {
        let cols = self.col_count();
        let char_w = font_px * CHAR_W_RATIO;
        let pad = CELL_PAD_X * 2.0;
        let mut natural = vec![0.0_f32; cols];
        for cells in &self.rows {
            for (i, cell) in cells.iter().enumerate().take(cols) {
                let nat = match cell {
                    MathCellKind::Text(runs) => cell_natural_width(runs, char_w, false),
                    MathCellKind::Math(m) => m.logical_w().min(BLOCK_W_CAP * font_px) + pad,
                };
                if nat > natural[i] {
                    natural[i] = nat;
                }
            }
        }
        let nat_sum: f32 = natural.iter().sum();
        if nat_sum <= 0.0 || nat_sum >= CONTENT_W {
            return natural;
        }
        let slack = CONTENT_W - nat_sum;
        (0..cols).map(|i| natural[i] + slack * (natural[i] / nat_sum)).collect()
    }

    /// Row height = max(text line height, each block cell's scaled height).
    fn row_height(&self, cells: &[MathCellKind], widths: &[f32]) -> f32 {
        let text_h = FONT_SIZE * LINE_H_RATIO + CELL_PAD_Y * 2.0;
        let block_h = cells
            .iter()
            .enumerate()
            .filter_map(|(i, c)| match c {
                MathCellKind::Math(m) => {
                    let cw = (widths.get(i).copied().unwrap_or(0.0) - CELL_PAD_X * 2.0).max(1.0);
                    let h = if m.logical_w() > cw { m.logical_h() * (cw / m.logical_w()) } else { m.logical_h() };
                    Some(h + CELL_PAD_Y * 2.0)
                }
                MathCellKind::Text(_) => None,
            })
            .fold(0.0_f32, f32::max);
        text_h.max(block_h)
    }
}

#[cfg(feature = "screenshot")]
impl BlockWidget for MathTableBlock {
    fn measure(&self, _font_size: f32, _width: f32) -> f32 {
        let widths = self.column_widths(FONT_SIZE);
        self.rows.iter().map(|r| self.row_height(r, &widths)).sum()
    }

    fn widget_id(&self) -> u64 {
        0x4A7
    }

    fn composite(&self, _font_size: f32, _width: f32) -> Option<Vec<ChildItem>> {
        let widths = self.column_widths(FONT_SIZE);
        let total_w: f32 = widths.iter().sum();
        let mut children: Vec<ChildItem> = Vec::new();
        // Grid chrome as one native child.
        let mut chrome: Vec<BlockPaint> = Vec::new();
        let total_h: f32 = self.rows.iter().map(|r| self.row_height(r, &widths)).sum();
        chrome.push(BlockPaint::Rect { x: 0.0, y: 0.0, w: total_w, h: self.row_height(&self.rows[0], &widths), color: HEADER_BG });
        let mut y = 0.0_f32;
        for cells in &self.rows {
            y += self.row_height(cells, &widths);
            chrome.push(BlockPaint::Line { from: (0.0, y), to: (total_w, y), width: RULE_W, color: RULE });
        }
        chrome.push(BlockPaint::Line { from: (0.0, 0.0), to: (total_w, 0.0), width: RULE_W, color: RULE });
        let mut x = 0.0_f32;
        for w in widths.iter().chain(std::iter::once(&0.0_f32)) {
            chrome.push(BlockPaint::Line { from: (x, 0.0), to: (x, total_h), width: RULE_W, color: RULE });
            x += w;
        }
        children.push(ChildItem { rect: ChildRect { x: 0.0, y: 0.0, w: total_w, h: total_h }, kind: ChildKind::Native(chrome), clip: None });
        // Per-cell children.
        let mut y = 0.0_f32;
        for cells in &self.rows {
            let row_h = self.row_height(cells, &widths);
            let mut x = 0.0_f32;
            for (i, cell) in cells.iter().enumerate() {
                let cw = widths.get(i).copied().unwrap_or(0.0);
                let content_x = x + CELL_PAD_X;
                let content_y = y + CELL_PAD_Y;
                let content_w = (cw - CELL_PAD_X * 2.0).max(1.0);
                let content_h = (row_h - CELL_PAD_Y * 2.0).max(1.0);
                match cell {
                    MathCellKind::Text(runs) => children.push(ChildItem {
                        rect: ChildRect { x: content_x, y: content_y, w: content_w, h: row_h },
                        kind: ChildKind::Native(vec![BlockPaint::RichText {
                            x: 0.0,
                            y: 0.0,
                            runs: runs.clone(),
                            max_width: content_w,
                            align: TextAlign::Left,
                        }]),
                        clip: None,
                    }),
                    MathCellKind::Math(m) => children.push(ChildItem {
                        rect: ChildRect { x: content_x, y: content_y, w: content_w, h: content_h },
                        kind: ChildKind::Texture(ChildTexture {
                            rgba: m.rgba.clone(),
                            width: m.width,
                            height: m.height,
                            id: m.id,
                        }),
                        clip: None,
                    }),
                }
                x += cw;
            }
            y += row_h;
        }
        Some(children)
    }
}

// --- Overflow modes: a WIDE text table rendered Fit vs. Scrollable -----------
//
// Mirrors the app's `TableWidget` overflow path: Fit stretches/shrinks the
// columns to `CONTENT_W`; Scrollable lays them out at NATURAL width, then offsets
// every composite child left by the clamped scroll and clips them to the
// `CONTENT_W` inset (`ChildItem.clip`). status: widget-table-overflow-scroll

/// A wide multi-column table for the overflow demo — natural width well past the
/// content box, so Fit visibly shrinks while Scrollable clips.
#[cfg(feature = "screenshot")]
struct OverflowTableBlock {
    rows: Vec<Vec<Vec<StyledRun>>>,
    scrollable: bool,
    h_offset: f32,
}

#[cfg(feature = "screenshot")]
fn wide_overflow_rows() -> Vec<Vec<Vec<StyledRun>>> {
    // Eight columns of substantial prose so the NATURAL width (~2000pt) comfortably
    // exceeds the CONTENT_W (1000pt) inset — Fit must shrink it, Scrollable clips.
    let head: Vec<&str> = vec![
        "Alpha column", "Bravo column", "Charlie column", "Delta column", "Echo column",
        "Foxtrot column", "Golf column", "Hotel column",
    ];
    let body: Vec<&str> = vec![
        "one two three", "four five six", "seven eight nine", "ten eleven", "twelve thirteen",
        "fourteen fifteen", "sixteen seventeen", "eighteen nineteen",
    ];
    vec![
        head.iter().map(|s| vec![bold(s)]).collect(),
        body.iter().map(|s| vec![plain(s)]).collect(),
    ]
}

#[cfg(feature = "screenshot")]
impl OverflowTableBlock {
    fn col_count(&self) -> usize {
        self.rows.iter().map(Vec::len).max().unwrap_or(1).max(1)
    }

    /// Per-column natural widths (no stretch/shrink) — the Scrollable layout, and
    /// the starting point Fit redistributes from.
    fn natural(&self, font_px: f32) -> Vec<f32> {
        let cols = self.col_count();
        let char_w = font_px * CHAR_W_RATIO;
        let mut natural = vec![0.0_f32; cols];
        for cells in &self.rows {
            for (i, runs) in cells.iter().enumerate().take(cols) {
                let nat = cell_natural_width(runs, char_w, false);
                if nat > natural[i] {
                    natural[i] = nat;
                }
            }
        }
        natural
    }

    /// Mode-dependent column widths: Scrollable returns natural verbatim; Fit
    /// stretches to fill `CONTENT_W` (or shrinks if it overflowed — here it
    /// always overflows, so Fit clamps to `CONTENT_W` proportionally).
    fn widths(&self, font_px: f32) -> Vec<f32> {
        let natural = self.natural(font_px);
        if self.scrollable {
            return natural;
        }
        let nat_sum: f32 = natural.iter().sum();
        if nat_sum <= 0.0 {
            return natural;
        }
        let cols = self.col_count();
        // Overflow case: scale every column proportionally down to fit CONTENT_W
        // (the snapshot's wide table always overflows). A faithful-enough mirror
        // of the app's floor-protected shrink for the visual.
        let scale = (CONTENT_W / nat_sum).min(1.0);
        (0..cols).map(|i| natural[i] * scale).collect()
    }

    fn row_height(&self) -> f32 {
        FONT_SIZE * LINE_H_RATIO + CELL_PAD_Y * 2.0
    }
}

#[cfg(feature = "screenshot")]
impl BlockWidget for OverflowTableBlock {
    fn measure(&self, _font_size: f32, _width: f32) -> f32 {
        self.row_height() * self.rows.len() as f32
    }

    fn widget_id(&self) -> u64 {
        0x0FFE
    }

    fn composite(&self, font_size: f32, _width: f32) -> Option<Vec<ChildItem>> {
        let widths = self.widths(font_size);
        let total_w: f32 = widths.iter().sum();
        let total_h = self.row_height() * self.rows.len() as f32;
        // Scrollable: shift children left by clamped offset, clip to the inset.
        let (x_shift, clip) = if self.scrollable {
            let max_off = (total_w - CONTENT_W).max(0.0);
            let off = self.h_offset.clamp(0.0, max_off);
            (-off, Some(ChildRect { x: 0.0, y: 0.0, w: CONTENT_W, h: total_h }))
        } else {
            (0.0, None)
        };
        let mut children: Vec<ChildItem> = Vec::new();
        // Chrome.
        let mut chrome: Vec<BlockPaint> = Vec::new();
        chrome.push(BlockPaint::Rect { x: 0.0, y: 0.0, w: total_w, h: self.row_height(), color: HEADER_BG });
        let mut y = 0.0_f32;
        for _ in &self.rows {
            y += self.row_height();
            chrome.push(BlockPaint::Line { from: (0.0, y), to: (total_w, y), width: RULE_W, color: RULE });
        }
        chrome.push(BlockPaint::Line { from: (0.0, 0.0), to: (total_w, 0.0), width: RULE_W, color: RULE });
        let mut cx = 0.0_f32;
        for w in widths.iter().chain(std::iter::once(&0.0_f32)) {
            chrome.push(BlockPaint::Line { from: (cx, 0.0), to: (cx, total_h), width: RULE_W, color: RULE });
            cx += w;
        }
        children.push(ChildItem {
            rect: ChildRect { x: x_shift, y: 0.0, w: total_w, h: total_h },
            kind: ChildKind::Native(chrome),
            clip,
        });
        // Per-cell text.
        let mut y = 0.0_f32;
        for cells in &self.rows {
            let row_h = self.row_height();
            let mut x = 0.0_f32;
            for (i, runs) in cells.iter().enumerate() {
                let cw = widths.get(i).copied().unwrap_or(0.0);
                let content_w = (cw - CELL_PAD_X * 2.0).max(1.0);
                children.push(ChildItem {
                    rect: ChildRect { x: x + CELL_PAD_X + x_shift, y: y + CELL_PAD_Y, w: content_w, h: row_h },
                    kind: ChildKind::Native(vec![BlockPaint::RichText {
                        x: 0.0,
                        y: 0.0,
                        runs: runs.clone(),
                        max_width: content_w,
                        align: TextAlign::Left,
                    }]),
                    clip,
                });
                x += cw;
            }
            y += row_h;
        }
        Some(children)
    }
}

/// Which snapshot to render: the Phase-A inline-markdown table, or the Phase-B
/// table with math cells (`TABLE_SNAPSHOT_MATH=1`).
/// The overflow demo variant requested via `TABLE_SNAPSHOT_OVERFLOW`, if any:
/// `fit` (wide table shrunk to the box), `scroll0` (natural width, clipped to the
/// box, offset 0), or `scroll` (scrolled so later columns show, earlier ones
/// clipped off the left). status: widget-table-overflow-scroll
#[cfg(feature = "screenshot")]
fn overflow_variant() -> Option<&'static str> {
    match std::env::var("TABLE_SNAPSHOT_OVERFLOW").ok().as_deref() {
        Some("fit") => Some("fit"),
        Some("scroll0") => Some("scroll0"),
        Some("scroll") => Some("scroll"),
        _ => None,
    }
}

#[cfg(feature = "screenshot")]
fn snapshot_widget() -> Arc<dyn BlockWidget> {
    if let Some(variant) = overflow_variant() {
        let rows = wide_overflow_rows();
        // A scroll offset of ~half the overflow reveals the middle columns.
        return match variant {
            "fit" => Arc::new(OverflowTableBlock { rows, scrollable: false, h_offset: 0.0 }),
            "scroll0" => Arc::new(OverflowTableBlock { rows, scrollable: true, h_offset: 0.0 }),
            _ => Arc::new(OverflowTableBlock { rows, scrollable: true, h_offset: 420.0 }),
        };
    }
    if std::env::var("TABLE_SNAPSHOT_MIXED").is_ok_and(|v| v == "1") {
        Arc::new(mixed_demo_table())
    } else if editing_mode() || std::env::var("TABLE_SNAPSHOT_MATH").is_ok_and(|v| v == "1") {
        // The editing snapshot reuses the math demo so an edited DIAGRAM cell is
        // on screen. status: widget-table-cell-edit-inplace
        Arc::new(math_demo_table())
    } else {
        Arc::new(TableBlock { table: demo_table(), legacy: legacy_mode() })
    }
}

#[cfg(feature = "screenshot")]
fn decorations() -> Set {
    RangeSet::from_iter([(
        0..1,
        Decoration::BlockWidget { side: BlockSide::Below, widget: snapshot_widget() },
    )])
}

/// Whether to render the in-place cell-edit state (`TABLE_SNAPSHOT_EDITING=1`):
/// the math demo table fully rendered, with the accent frame around the whole
/// table, a brighter outline on one active cell, and a popover floating over it.
/// status: widget-table-cell-edit-inplace
#[cfg(feature = "screenshot")]
fn editing_mode() -> bool {
    std::env::var("TABLE_SNAPSHOT_EDITING").is_ok_and(|v| v == "1")
}

#[cfg(feature = "screenshot")]
fn main() -> Result<()> {
    let mixed = std::env::var("TABLE_SNAPSHOT_MIXED").is_ok_and(|v| v == "1");
    let math = std::env::var("TABLE_SNAPSHOT_MATH").is_ok_and(|v| v == "1");
    let name = if editing_mode() {
        "target/table-snapshot-editing.png"
    } else if let Some(variant) = overflow_variant() {
        match variant {
            "fit" => "target/table-snapshot-overflow-fit.png",
            "scroll0" => "target/table-snapshot-overflow-scroll0.png",
            _ => "target/table-snapshot-overflow-scroll.png",
        }
    } else if mixed {
        "target/table-snapshot-mixed.png"
    } else if math {
        "target/table-snapshot-math.png"
    } else if legacy_mode() {
        "target/table-snapshot-before.png"
    } else {
        "target/table-snapshot-after.png"
    };
    let out = std::path::PathBuf::from(name);
    let (w, h) = render_png(&out).map_err(anyhow::Error::msg)?;
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

/// Draw the in-place cell-edit cues over the rendered math demo table
/// (`widget-table-cell-edit-inplace`): a soft accent frame around the whole
/// table, a brighter accent outline on the active cell (the display-math cell at
/// body row 1 / column 2), and a popover floating just over it showing the cell's
/// source seeded into a one-line fence. `origin` is the editor body top-left; the
/// table block paints below the single host line. Mirrors the app's host-drawn
/// cues so the PNG reflects what the user sees. The geometry comes from the same
/// `MathTableBlock::column_widths` / `row_height` solve the widget paints with.
#[cfg(feature = "screenshot")]
fn draw_editing_cues(ui: &egui::Ui, origin: egui::Pos2) {
    let table = math_demo_table();
    let widths = table.column_widths(FONT_SIZE);
    let total_w: f32 = widths.iter().sum();
    let host_line = FONT_SIZE * LINE_H_RATIO; // line 0 above the block
    let table_top = origin.y + host_line;
    // Row tops (header = row 0).
    let mut row_tops = Vec::new();
    let mut y = table_top;
    for cells in &table.rows {
        row_tops.push(y);
        y += table.row_height(cells, &widths);
    }
    let table_rect = egui::Rect::from_min_max(
        egui::pos2(origin.x, table_top),
        egui::pos2(origin.x + total_w, y),
    );
    // Active cell = body row 1, column 2 (the display-math cell).
    let (ar, ac) = (1usize, 2usize);
    let cell_x = origin.x + widths[..ac].iter().sum::<f32>();
    let cell_y = row_tops[ar];
    let cell_rect = egui::Rect::from_min_size(
        egui::pos2(cell_x, cell_y),
        egui::vec2(widths[ac], table.row_height(&table.rows[ar], &widths)),
    );
    let accent = egui::Color32::from_rgb(120, 110, 230);
    let p = ui.painter();
    // Soft whole-table frame.
    p.rect_stroke(
        table_rect.expand(3.0),
        4.0,
        egui::Stroke::new(1.5, accent.gamma_multiply(0.6)),
        egui::StrokeKind::Outside,
    );
    // Brighter active-cell outline.
    p.rect_stroke(
        cell_rect.expand(1.0),
        2.0,
        egui::Stroke::new(2.0, accent),
        egui::StrokeKind::Outside,
    );
    // Popover anchored to the cell's top-left, wider than the narrow cell.
    let pop = egui::Rect::from_min_size(cell_rect.left_top(), egui::vec2(320.0, 56.0));
    p.rect_filled(pop, 6.0, egui::Color32::from_rgb(250, 250, 252));
    p.rect_stroke(pop, 6.0, egui::Stroke::new(1.0, accent), egui::StrokeKind::Outside);
    p.text(
        pop.min + egui::vec2(8.0, 8.0),
        egui::Align2::LEFT_TOP,
        "$$ \\frac{a+b}{c+d} $$",
        egui::FontId::monospace(14.0),
        egui::Color32::from_rgb(30, 30, 30),
    );
}

#[cfg(feature = "screenshot")]
fn render_png(out: &std::path::Path) -> Result<(u32, u32), String> {
    // The mixed-block demo is taller (5 rows of scaled diagrams / images).
    let view_h = if std::env::var("TABLE_SNAPSHOT_MIXED").is_ok_and(|v| v == "1") {
        VIEW_H_MIXED
    } else {
        VIEW_H
    };
    let renderer = egui_kittest::wgpu::WgpuTestRenderer::new();
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::Vec2::new(VIEW_W, view_h))
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
                height: view_h,
                ..Default::default()
            };
            view.wrap_map.set_enabled(false);
            view.sync_to(&editor);
            view.decorations.clear();
            view.decorations.push_with_heights(decorations());

            let mut paint_cache = PaintCache::default();
            let rect = egui::Rect::from_min_size(ui.min_rect().min, egui::vec2(VIEW_W, view_h));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            EditorWidget::new(&mut editor, &mut view)
                .with_paint_cache(&mut paint_cache)
                .show(&mut child);
            // In-place cell-edit cues (`widget-table-cell-edit-inplace`): drawn on
            // top of the fully-rendered table — the soft whole-table frame, the
            // brighter active-cell outline, and a popover floating over it.
            if editing_mode() {
                draw_editing_cues(ui, rect.min);
            }
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
