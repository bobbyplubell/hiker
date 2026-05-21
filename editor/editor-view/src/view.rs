//! ViewState: per-widget UI state (scroll, font, height map, IME, drag).

use std::sync::Arc;
use web_time::Instant;

use editor_core::{DecorationSet, EditorState, Transaction};

use crate::completion::{CompletionSource, CompletionState};
use crate::ime::ImeState;
use crate::tooltip::Tooltip;
use crate::panel::PanelStack;
use crate::search::SearchState;
use crate::snippet::SnippetState;
use crate::wrap::WrapMap;

/// Pixel rectangle in widget-local coordinates, populated by the painter on
/// every frame so the input layer can hit-test clickable decorations
/// (Expander blocks, etc.) before falling back to text positioning.
#[derive(Clone, Copy, Debug)]
pub struct ClickRect {
    pub x_min: f32,
    pub y_min: f32,
    pub x_max: f32,
    pub y_max: f32,
}

impl ClickRect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x_min && x < self.x_max && y >= self.y_min && y < self.y_max
    }
}

#[derive(Clone, Debug)]
pub struct ClickZone {
    pub rect: ClickRect,
    pub action: ClickAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClickAction {
    ToggleFold(u64),
    /// An inline or block widget with `handles_click() == true` was clicked.
    /// Carries the widget's `widget_id()` so the host can dispatch.
    WidgetClick(u64),
}

/// Mouse drag state machine. Encodes the four phases of a possible drag:
/// (1) nothing in progress, (2) mouse pressed outside a selection so a drag
/// extends/creates a selection, (3) mouse pressed inside an existing
/// selection — may turn into a text drag once the pointer moves past a
/// small threshold, and (4) an in-progress text drag with a drop caret.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum DragState {
    #[default]
    Idle,
    /// Mouse pressed outside a selection; subsequent drag extends a
    /// selection from `anchor` to the current pointer position.
    MaybeSelecting { anchor: usize },
    /// Mouse pressed inside a non-empty selection; a real text drag begins
    /// once the pointer moves more than `threshold` pixels from `start`.
    MaybeDraggingSelection { start: (f32, f32), threshold: f32 },
    /// Active text drag. `drop_caret` is the byte offset where the
    /// dragged text would be inserted if the mouse were released now.
    DraggingSelection { drop_caret: usize },
    /// Alt+drag column/rectangular selection in progress. `start_xy` is
    /// the widget-local pixel position where the drag began; the current
    /// pointer position defines the opposing corner.
    RectangleSelecting { start_xy: (f32, f32) },
}

/// A language-supplied hook that can intercept the Enter key to produce a
/// custom transaction (e.g. continuing a markdown list item). Returning
/// `None` lets the default newline-insert path run.
pub trait IndentProvider: Send + Sync {
    fn on_enter(&self, state: &EditorState) -> Option<Transaction>;
}

#[derive(Clone, Debug, Default)]
pub struct DecorationLayers {
    /// Per-frame decoration sets, layered in declaration order (later layers
    /// stack on top of earlier ones for marks; later layers override for
    /// line/replace).
    pub layers: Vec<DecorationSet>,
    /// Indices into `layers` of sets that may carry height-affecting entries
    /// (`Line.hide`, `Line.height_scale`, `Block`, `BlockWidget`). The
    /// heightmap driver only scans these layers; the painter still walks
    /// every layer. Pushing via [`Self::push`] marks a layer paint-only; use
    /// [`Self::push_with_heights`] when the layer may emit height entries.
    pub height_indices: Vec<usize>,
    /// Order-sensitive fingerprint over the content_ids of every pushed set.
    /// Equal signatures across frames mean the same exact sets were pushed in
    /// the same order — a strong "decorations unchanged" signal that the
    /// widget uses to skip the geometry pipeline.
    pub signature: u64,
    /// Same as `signature`, restricted to layers in `height_indices`. Lets
    /// the widget detect a no-op for the heightmap driver specifically
    /// (height-affecting layers unchanged even if paint-only ones did).
    pub height_signature: u64,
}

impl DecorationLayers {
    pub fn clear(&mut self) {
        self.layers.clear();
        self.height_indices.clear();
        self.signature = 0;
        self.height_signature = 0;
    }
    /// Push a paint-only decoration layer (no height-affecting entries).
    pub fn push(&mut self, set: DecorationSet) {
        self.signature = mix_u64(self.signature, set.content_id() as u64);
        self.layers.push(set);
    }
    /// Push a layer that may contain height-affecting entries. The heightmap
    /// driver will scan this layer.
    pub fn push_with_heights(&mut self, set: DecorationSet) {
        self.height_indices.push(self.layers.len());
        let id = set.content_id() as u64;
        self.signature = mix_u64(self.signature, id);
        self.height_signature = mix_u64(self.height_signature, id);
        self.layers.push(set);
    }
    /// Iterate only the layers flagged as containing height-affecting
    /// decorations.
    pub fn height_layers(&self) -> impl Iterator<Item = &DecorationSet> {
        self.height_indices
            .iter()
            .filter_map(move |i| self.layers.get(*i))
    }
}

/// splitmix64-style mixer. Order-dependent; used to build a fingerprint by
/// accumulating values one at a time.
fn mix_u64(seed: u64, x: u64) -> u64 {
    let mut z = seed.wrapping_add(x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub struct ViewState {
    pub scroll_y: f32,
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub gutter_width: f32,
    pub height_map: HeightMap,
    pub ime: ImeState,
    pub decorations: DecorationLayers,
    /// Current mouse-drag state machine. See [`DragState`].
    pub drag: DragState,
    /// Last interaction; for cursor blinking.
    pub last_interaction: Instant,
    /// When true, command dispatch ignores text-modifying input. Used by the
    /// diff view (which displays a synthesized rope that cannot be edited
    /// in-place).
    pub read_only: bool,
    /// When true, hide the gutter (line numbers + fold column).
    pub hide_gutter: bool,
    /// Clickable regions populated by the painter each frame. Mouse handlers
    /// hit-test against this before normal text positioning.
    pub click_zones: Vec<ClickZone>,
    /// Floating tooltips to draw over the editor this frame. The host sets
    /// this each frame (e.g. from a hover handler); the widget reads it and
    /// paints in an `egui::Area` overlay. See [`Tooltip`].
    pub tooltips: Vec<Tooltip>,
    /// Autocomplete popup state. Defaults to inactive.
    pub completion: CompletionState,
    /// Registered completion sources, queried on trigger characters and on
    /// explicit completion requests.
    pub completion_sources: Vec<Arc<dyn CompletionSource>>,
    /// Per-buffer-line wrap cache. When `wrap_map.enabled()` is true, the
    /// painter renders each buffer line as N stacked visual lines and motion
    /// commands navigate visually.
    pub wrap_map: WrapMap,
    /// Byte position immediately AFTER the close char of the most recently
    /// auto-inserted pair. Used by `autopair::autopair_skip` so typing the
    /// matching close char advances the cursor instead of inserting a second
    /// close. Cleared by any non-skip input (motion, delete, regular insert).
    pub autopair_skip_at: Option<usize>,
    /// Optional language-supplied Enter-key interceptor. When set, the
    /// command dispatcher consults it before inserting a literal newline.
    pub indent_provider: Option<Arc<dyn IndentProvider>>,
    /// When `doc.is_empty()`, the painter renders this string dimmed at the
    /// text origin instead of nothing. See SPEC §9.12.
    pub placeholder: Option<smol_str::SmolStr>,
    /// Fraction of the viewport (0.0–1.0) of extra empty space allowed below
    /// the last line for scrolling. 0.0 = clamp at the last line. See SPEC §9.18.
    pub scroll_past_end: f32,
    /// Multiplier applied to scroll-wheel deltas before they reach the
    /// scroll command. Host sets this from a user config setting; `1.0`
    /// keeps the egui default speed, `>1.0` scrolls proportionally faster.
    pub scroll_speed: f32,
    /// Find / find-and-replace panel state. Defaults to closed. See SPEC §9.13.
    pub search: SearchState,
    /// Stack of panels (top / bottom strips) docked around the text area.
    /// See SPEC §9.21.
    pub panels: PanelStack,
    /// Active snippet expansion state (Tab/Shift-Tab cycle through stops).
    /// Defaults to inactive. See SPEC §9.22.
    pub snippet: SnippetState,
    /// Last-frame fingerprints used by the widget's measure phase to detect
    /// which (if any) geometry inputs changed. When all four match the
    /// current frame's values, the measure pass is skipped entirely.
    pub measure_cache: MeasureCache,
}

/// Fingerprints of the inputs that drove the most recent measure pass
/// (heightmap build + wrap recomputation). The widget compares each input
/// to its cached value to decide what work to redo.
///
/// `u64::MAX` is reserved as a "never measured" sentinel that always misses.
#[derive(Clone, Copy, Debug)]
pub struct MeasureCache {
    /// `state.doc.content_id()` from the last measured frame.
    pub doc_id: u64,
    /// `view.decorations.height_signature` from the last measured frame.
    pub height_decos: u64,
    /// Hash of (width, gutter_width, font_size, line_height, wrap_enabled,
    /// wrap width, char width) — anything that, if changed, invalidates the
    /// height map and the wrap cache.
    pub metrics: u64,
    /// First/last visible line at the last measure (so we know when the
    /// viewport band has shifted enough to need re-prewrap).
    pub viewport: (usize, usize),
}

impl Default for MeasureCache {
    fn default() -> Self {
        Self {
            doc_id: u64::MAX,
            height_decos: u64::MAX,
            metrics: u64::MAX,
            viewport: (usize::MAX, usize::MAX),
        }
    }
}

impl std::fmt::Debug for ViewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewState")
            .field("scroll_y", &self.scroll_y)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("font_size", &self.font_size)
            .field("line_height", &self.line_height)
            .field("gutter_width", &self.gutter_width)
            .field("read_only", &self.read_only)
            .field("hide_gutter", &self.hide_gutter)
            .field("click_zones", &self.click_zones)
            .field("completion_sources", &self.completion_sources.len())
            .finish_non_exhaustive()
    }
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            scroll_y: 0.0,
            width: 0.0,
            height: 0.0,
            font_size: 14.0,
            line_height: 18.0,
            gutter_width: 56.0,
            height_map: HeightMap::default(),
            ime: ImeState::default(),
            decorations: DecorationLayers::default(),
            drag: DragState::Idle,
            last_interaction: Instant::now(),
            read_only: false,
            hide_gutter: false,
            click_zones: Vec::new(),
            tooltips: Vec::new(),
            completion: CompletionState::default(),
            completion_sources: Vec::new(),
            wrap_map: WrapMap::default(),
            autopair_skip_at: None,
            indent_provider: None,
            placeholder: None,
            scroll_past_end: 0.0,
            scroll_speed: 1.0,
            search: SearchState::default(),
            panels: PanelStack::default(),
            snippet: SnippetState::default(),
            measure_cache: MeasureCache::default(),
        }
    }
}

impl ViewState {
    pub fn touch(&mut self) {
        self.last_interaction = Instant::now();
    }

    pub fn sync_to(&mut self, state: &EditorState) {
        self.height_map.sync_to_lines(state.doc.len_lines(), self.line_height);
    }

    /// First visible line (inclusive) and last visible line (exclusive).
    pub fn visible_lines(&self) -> std::ops::Range<usize> {
        if self.height_map.is_empty() {
            return 0..0;
        }
        let top = self.height_map.line_at_y(self.scroll_y).saturating_sub(1);
        let bottom = self
            .height_map
            .line_at_y(self.scroll_y + self.height)
            .saturating_add(2)
            .min(self.height_map.len());
        top..bottom
    }

    pub fn line_top_y(&self, line: usize) -> f32 {
        self.height_map.y_at_line(line) - self.scroll_y
    }

    pub fn text_top_y(&self, line: usize) -> f32 {
        self.height_map.y_at_text(line) - self.scroll_y
    }
}

/// Per-line geometry. Retained as a public type for callers; the sparse
/// [`HeightMap`] doesn't store one per line.
#[derive(Clone, Debug)]
pub struct LineGeometry {
    pub height: f32,
}

/// Per-line override record stored only for lines whose geometry differs
/// from the default uniform row.
#[derive(Clone, Copy, Debug)]
struct LineOverride {
    /// `Some(h)` overrides the line's text height (h == 0 hides the line);
    /// `None` means "use the map's default_height".
    text_height: Option<f32>,
    block_above: f32,
    block_below: f32,
}

impl LineOverride {
    fn is_noop(&self) -> bool {
        self.text_height.is_none() && self.block_above == 0.0 && self.block_below == 0.0
    }
    fn full_height(&self, default_height: f32) -> f32 {
        self.text_height.unwrap_or(default_height) + self.block_above + self.block_below
    }
}

/// Sparse per-line height map. Conceptually represents N lines of
/// `default_height` each, with a small set of `overrides` for lines whose
/// height differs (decorations applied by the painter — `Line.hide`,
/// `Line.height_scale`, `Block`, `BlockWidget`, soft-wrap multipliers).
///
/// Memory and per-frame cost scale with the number of overrides — not the
/// number of lines. For a 50k-line markdown doc with ~50 headings and a
/// handful of folds, the override map holds ~100 entries; `total_height`,
/// `y_at_line`, and `line_at_y` are O(log K) over an internal prefix index
/// (where K is the override count).
///
/// Operations that read are cheap and O(1) / O(log K). Operations that mutate
/// (`set_line_height`, `add_block_*`, `clear_blocks`) mark the prefix index
/// dirty; the next read call rebuilds it lazily in O(K).
#[derive(Clone, Debug, Default)]
pub struct HeightMap {
    line_count: usize,
    default_height: f32,
    overrides: std::collections::BTreeMap<usize, LineOverride>,
    /// Cached total height. Stored explicitly so [`Self::total_height`] is
    /// O(1) regardless of override count.
    total: f32,
    /// Prefix snapshot of `overrides`. Each entry is
    /// `(line_idx, y_at_row_top_of_line)`. Sorted by `line_idx`.
    /// Lazily rebuilt by [`Self::ensure_prefix`] when reads happen after
    /// mutations.
    prefix_index: Vec<PrefixEntry>,
    prefix_dirty: bool,
}

#[derive(Clone, Copy, Debug)]
struct PrefixEntry {
    line: usize,
    y_at_row_top: f32,
    full_height: f32,
}

impl HeightMap {
    pub fn len(&self) -> usize {
        self.line_count
    }

    pub fn is_empty(&self) -> bool {
        self.line_count == 0
    }

    /// Resize to `line_count` lines of `default_height`. Clears all
    /// overrides if either changes.
    pub fn sync_to_lines(&mut self, line_count: usize, default_height: f32) {
        let height_changed = (self.default_height - default_height).abs() > f32::EPSILON;
        if self.line_count != line_count || height_changed {
            self.line_count = line_count;
            self.default_height = default_height;
            // A default-height change reinterprets every existing override's
            // total contribution; cheapest is to drop them and let the next
            // measure pass re-emit (which it will — sync is followed by
            // apply_line_height_decorations every measure).
            self.overrides.clear();
            self.total = (line_count as f32) * default_height;
            self.prefix_index.clear();
            self.prefix_dirty = false;
        }
    }

    fn entry_mut(&mut self, line: usize) -> &mut LineOverride {
        self.overrides.entry(line).or_insert(LineOverride {
            text_height: None,
            block_above: 0.0,
            block_below: 0.0,
        })
    }

    pub fn set_line_height(&mut self, line: usize, height: f32) {
        if line >= self.line_count {
            return;
        }
        let default = self.default_height;
        let is_default = (height - default).abs() < f32::EPSILON;
        // Fast path: setting an unoverridden line to its default is a
        // no-op. Avoid the `entry().or_insert(...)` allocation that
        // otherwise fires N times per `apply_line_height_decorations`
        // pass (which runs every scroll frame because viewport change
        // invalidates the measure cache). Without this guard, scrolling
        // a 10k-line file does 10k BTreeMap inserts per frame.
        if is_default && !self.overrides.contains_key(&line) {
            return;
        }
        let prev_full = self
            .overrides
            .get(&line)
            .map(|o| o.full_height(default))
            .unwrap_or(default);
        let entry = self.entry_mut(line);
        entry.text_height = if is_default { None } else { Some(height) };
        let next_full = entry.full_height(default);
        let is_noop = entry.is_noop();
        if is_noop {
            self.overrides.remove(&line);
        }
        self.total += next_full - prev_full;
        self.prefix_dirty = true;
    }

    pub fn add_block_above(&mut self, line: usize, height: f32) {
        if line >= self.line_count || height == 0.0 {
            return;
        }
        let default = self.default_height;
        let prev_full = self
            .overrides
            .get(&line)
            .map(|o| o.full_height(default))
            .unwrap_or(default);
        let entry = self.entry_mut(line);
        entry.block_above += height;
        let next_full = entry.full_height(default);
        self.total += next_full - prev_full;
        self.prefix_dirty = true;
    }

    pub fn add_block_below(&mut self, line: usize, height: f32) {
        if line >= self.line_count || height == 0.0 {
            return;
        }
        let default = self.default_height;
        let prev_full = self
            .overrides
            .get(&line)
            .map(|o| o.full_height(default))
            .unwrap_or(default);
        let entry = self.entry_mut(line);
        entry.block_below += height;
        let next_full = entry.full_height(default);
        self.total += next_full - prev_full;
        self.prefix_dirty = true;
    }

    /// Reset every override back to default text height (block_above /
    /// block_below preserved). O(K) over the current override count
    /// instead of O(N) over total lines. Used by the painter driver at
    /// the start of each `apply_line_height_decorations` pass so it can
    /// re-apply only the heights it actually needs without first walking
    /// every line.
    pub fn reset_text_heights(&mut self) {
        if self.overrides.is_empty() {
            return;
        }
        let default = self.default_height;
        let mut delta: f32 = 0.0;
        let mut drops: Vec<usize> = Vec::new();
        for (line, o) in self.overrides.iter_mut() {
            if o.text_height.is_none() {
                continue;
            }
            let prev = o.full_height(default);
            o.text_height = None;
            let next = o.full_height(default);
            delta += next - prev;
            if o.is_noop() {
                drops.push(*line);
            }
        }
        for l in drops {
            self.overrides.remove(&l);
        }
        self.total += delta;
        self.prefix_dirty = true;
    }

    pub fn clear_blocks(&mut self) {
        if self.overrides.is_empty() {
            return;
        }
        let default = self.default_height;
        let mut delta: f32 = 0.0;
        let mut drops: Vec<usize> = Vec::new();
        for (line, o) in self.overrides.iter_mut() {
            let prev = o.full_height(default);
            o.block_above = 0.0;
            o.block_below = 0.0;
            let next = o.full_height(default);
            delta += next - prev;
            if o.is_noop() {
                drops.push(*line);
            }
        }
        for l in drops {
            self.overrides.remove(&l);
        }
        self.total += delta;
        self.prefix_dirty = true;
    }

    pub fn block_above(&self, line: usize) -> f32 {
        self.overrides.get(&line).map(|o| o.block_above).unwrap_or(0.0)
    }

    pub fn block_below(&self, line: usize) -> f32 {
        self.overrides.get(&line).map(|o| o.block_below).unwrap_or(0.0)
    }

    pub fn text_height(&self, line: usize) -> f32 {
        if line >= self.line_count {
            return 0.0;
        }
        self.overrides
            .get(&line)
            .and_then(|o| o.text_height)
            .unwrap_or(self.default_height)
    }

    /// Recompute the cached prefix index. Idempotent — safe to call multiple
    /// times. Reads call this lazily; mutations only set the dirty flag.
    pub fn recompute(&mut self) {
        self.ensure_prefix();
    }

    fn ensure_prefix(&mut self) {
        if !self.prefix_dirty && !self.prefix_index.is_empty() {
            return;
        }
        if !self.prefix_dirty && self.overrides.is_empty() {
            return;
        }
        self.prefix_index.clear();
        self.prefix_index.reserve(self.overrides.len());
        let mut prev_line: usize = 0;
        let mut y: f32 = 0.0;
        for (&line, o) in &self.overrides {
            // Gap of `line - prev_line` default-height rows lies between the
            // last override (or 0) and this one.
            y += (line - prev_line) as f32 * self.default_height;
            let full = o.full_height(self.default_height);
            self.prefix_index.push(PrefixEntry {
                line,
                y_at_row_top: y,
                full_height: full,
            });
            y += full;
            prev_line = line + 1;
        }
        self.prefix_dirty = false;
    }

    /// Top y of line `line`'s visual row (i.e. above-block top).
    ///
    /// Reads expect the prefix index to be fresh. Callers that mutate the
    /// map must invoke [`Self::recompute`] before reading — the painter
    /// driver (`apply_line_height_decorations`) already does this at the end
    /// of every measure pass. If you mutate and forget to recompute, results
    /// reflect the last-known prefix state (same behaviour as the previous
    /// flat-prefix implementation).
    pub fn y_at_row_top(&self, line: usize) -> f32 {
        if line == 0 || self.line_count == 0 {
            return 0.0;
        }
        let idx = match self
            .prefix_index
            .binary_search_by_key(&line, |e| e.line)
        {
            Ok(i) => return self.prefix_index[i].y_at_row_top,
            Err(i) => i,
        };
        // No override AT `line`. Find the last override strictly before it.
        if idx == 0 {
            // No prior overrides — `line` lines of default_height.
            return (line as f32) * self.default_height;
        }
        let prev = &self.prefix_index[idx - 1];
        // y up to and including the prev override row, plus default-height
        // rows from `prev.line + 1` to `line - 1` (inclusive).
        let lines_after_prev = line - (prev.line + 1);
        prev.y_at_row_top + prev.full_height + (lines_after_prev as f32) * self.default_height
    }

    /// Top y of line `line`'s text (after its above-block).
    pub fn y_at_text(&self, line: usize) -> f32 {
        self.y_at_row_top(line) + self.block_above(line)
    }

    /// Backwards-compat alias used by code that wants the top of the row.
    pub fn y_at_line(&self, line: usize) -> f32 {
        self.y_at_row_top(line)
    }

    pub fn total_height(&self) -> f32 {
        self.total
    }

    pub fn line_at_y(&self, y: f32) -> usize {
        if y <= 0.0 || self.line_count == 0 {
            return 0;
        }
        // See y_at_row_top: callers are expected to have invoked
        // `recompute()` after any mutations.
        // Walk overrides in prefix order, charging gaps in default_height
        // until we land on or past `y`.
        if self.prefix_index.is_empty() {
            // No overrides — uniform default-height rows.
            return ((y / self.default_height) as usize)
                .min(self.line_count.saturating_sub(1));
        }
        // Find the first prefix entry whose row_top exceeds `y`. The override
        // immediately before it (or the gap up to it) contains `y`.
        let mut lo = 0usize;
        let mut hi = self.prefix_index.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.prefix_index[mid].y_at_row_top + self.prefix_index[mid].full_height <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // `lo` is the index of the entry whose [y_at_row_top, +full_height]
        // window contains `y`, OR points past the last entry if `y` lies in
        // the trailing default-height gap.
        if lo < self.prefix_index.len() {
            let entry = self.prefix_index[lo];
            if y < entry.y_at_row_top {
                // `y` is in the gap BEFORE this entry. Lines start at
                // `prev_line + 1` (or 0 if first entry) at row_top = previous
                // entry's row_top + full_height (or 0).
                let (gap_start_line, gap_start_y) = if lo == 0 {
                    (0usize, 0.0f32)
                } else {
                    let prev = self.prefix_index[lo - 1];
                    (prev.line + 1, prev.y_at_row_top + prev.full_height)
                };
                let into_gap = y - gap_start_y;
                let n = (into_gap / self.default_height) as usize;
                return (gap_start_line + n).min(self.line_count.saturating_sub(1));
            }
            return entry.line;
        }
        // `y` past the last override row — falls in trailing gap.
        let last = *self.prefix_index.last().unwrap();
        let gap_start_line = last.line + 1;
        let gap_start_y = last.y_at_row_top + last.full_height;
        let into_gap = (y - gap_start_y).max(0.0);
        let n = (into_gap / self.default_height) as usize;
        (gap_start_line + n).min(self.line_count.saturating_sub(1))
    }
}
