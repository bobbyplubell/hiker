# egui_editor — Implementation Spec

Companion to `SPEC.md`. This document is the **developer-facing** architecture: crate
layout, types, data structures, build order. SPEC.md says *what*; this says *how*.

---

## 0. Locked decisions (from design discussion)

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Rust edition | 2024, stable | No nightly features. |
| Async in core | **No tokio in editor-core.** Pure sync (State, Tx) → State. | Matches Helix's `helix-core`, Zed's `crates/text`, CM6. Async (LSP, fs, background parse) lives in the host or in worker threads producing transactions. |
| Persistent collections | **Hand-rolled SumTree** from day one. | Same structure backs rope, RangeSet, and Selection storage. Avoids retrofitting later. |
| Rope | SumTree-of-UTF8-chunks, behind `trait Rope`. **No ropey dep.** | Custom rope decided based on user preference; SumTree is needed anyway for RangeSet so reusing it is cheap. |
| IME | Decoupled from egui via an `InputEvent` enum in `editor-view`. Host translates platform events. | User requirement: don't tie IME to egui. |
| Bidi | Deferred entirely. APIs are LTR-only; revisit if ever needed. | User does not need it. |
| Accessibility | Deferred. | v2. |
| Diff view | In v1. Side-by-side + unified+intraline. | User priority feature. |
| Live preview | Full markdown live preview (inline mode) in v1, including tables. | User priority feature. |
| Workspace | Multi-crate: core / view / egui / md / ts. | Renderer-agnostic state layer; testable headlessly. |

---

## 1. Workspace layout

```
egui_editor/
  Cargo.toml                  # workspace
  SPEC.md                     # user requirements
  IMPLEMENTATION.md           # this file
  references/                 # cloned reference editors (gitignored)
  crates/
    editor-core/              # zero rendering deps
    editor-view/              # viewport, layout cache, commands, input — backend-agnostic
    editor-egui/              # egui::Widget impl
    editor-md/                # markdown language + live-preview decorations
    editor-ts/                # tree-sitter language adapter
  examples/
    minimal/                  # egui app with just the widget
    markdown/                 # markdown live preview demo
    diff/                     # diff view demo
```

Dependency rules (enforced by cargo, not just convention):

- `editor-core` depends on nothing graphical.
- `editor-view` depends on `editor-core` only.
- `editor-egui` depends on `editor-view`, `editor-core`, `egui`, `epaint`.
- Language crates depend on `editor-core` only (they register fields and decoration providers).

---

## 2. SumTree — the foundational data structure

A persistent B-tree parameterized by a `Summary` trait. Every node caches the sum of its
subtree's items. Used three places: rope chunks, decoration ranges, line-height cache.

```rust
pub trait Item {
    type Summary: Summary;
    fn summary(&self) -> Self::Summary;
}

pub trait Summary: Clone + Default {
    fn add(&mut self, other: &Self);
}

pub trait Dimension<S: Summary>: Clone + Default + Ord {
    fn add_summary(&mut self, summary: &S);
}

pub struct SumTree<T: Item>(Arc<Node<T>>);

enum Node<T: Item> {
    Leaf  { items: SmallVec<[T; LEAF_CAP]>, summary: T::Summary },
    Inner { children: SmallVec<[Arc<Node<T>>; BRANCH]>, summary: T::Summary, height: u8 },
}
```

- Persistent: every mutation returns a new `SumTree` sharing untouched subtrees via `Arc`.
- `Cursor<'a, T, D>` walks the tree in `D`-space (a `Dimension` over `T::Summary`).
  Maintains a parent stack to avoid root-relative re-traversal.
- Branching factor: 6 children per inner node; leaf capacity 8 items. Tune later.
- `concat`, `split_at(pos: D)`, `slice(range: Range<D>)`, `edit(splice: Edit<T>)` are the
  core ops; everything else is built on `Cursor` + these.

Lifted directly from Zed's `crates/sum_tree`. We are not reinventing; we are implementing
the same idea against our own use cases.

---

## 3. Rope

```rust
pub struct Chunk(pub SmolStr);          // UTF-8, capacity ~1 KiB

#[derive(Default, Clone)]
pub struct ChunkSummary {
    pub bytes: u32,
    pub chars: u32,
    pub lines: u32,           // count of '\n'
    pub utf16: u32,            // for LSP / future collab
    pub last_line_bytes: u32,  // for column math
    // graphemes intentionally omitted — recomputed on demand per chunk
}

pub struct Rope(SumTree<Chunk>);
```

Public API:

```rust
impl Rope {
    pub fn new() -> Self;
    pub fn from_str(s: &str) -> Self;
    pub fn len_bytes(&self) -> usize;
    pub fn len_chars(&self) -> usize;
    pub fn len_lines(&self) -> usize;

    pub fn slice(&self, bytes: Range<usize>) -> RopeSlice<'_>;
    pub fn chunks(&self) -> Chunks<'_>;
    pub fn bytes(&self) -> Bytes<'_>;
    pub fn chars(&self) -> Chars<'_>;

    pub fn byte_to_char(&self, byte: usize) -> usize;
    pub fn char_to_byte(&self, ch: usize) -> usize;
    pub fn byte_to_line(&self, byte: usize) -> usize;
    pub fn line_to_byte(&self, line: usize) -> usize;
    pub fn byte_to_utf16(&self, byte: usize) -> usize;
    pub fn utf16_to_byte(&self, utf16: usize) -> usize;

    pub fn line(&self, idx: usize) -> RopeSlice<'_>;
    pub fn line_len(&self, idx: usize) -> usize;          // bytes
    pub fn grapheme_before(&self, byte: usize) -> usize;
    pub fn grapheme_after(&self, byte: usize) -> usize;
}
```

All positions are **byte offsets** by default. Char / line / utf16 conversions are O(log n)
via `Dimension`-typed cursors.

Grapheme walking uses `unicode-segmentation` over a small chunk-spanning window; no
grapheme cache in v1 (revisit if hot).

`trait Rope` *is not* added in v1 — we have one impl, custom rope from the start. If a
second impl is ever wanted (e.g. for benchmarking), the existing concrete API becomes the
trait at that point.

---

## 4. ChangeSet

```rust
#[derive(Clone)]
pub enum Op {
    Retain(u32),
    Delete(u32),
    Insert(SmolStr),
}

#[derive(Clone)]
pub struct ChangeSet {
    pub(crate) ops: Vec<Op>,
    pub(crate) len_before: u32,
    pub(crate) len_after: u32,
}
```

Operations:

```rust
impl ChangeSet {
    pub fn empty(doc_len: usize) -> Self;
    pub fn of(doc_len: usize, edits: impl IntoIterator<Item = (Range<usize>, &str)>) -> Self;

    pub fn apply(&self, rope: &Rope) -> Rope;
    pub fn invert(&self, before: &Rope) -> ChangeSet;          // pre-compute for undo
    pub fn compose(&self, after: &ChangeSet) -> ChangeSet;     // self then after
    pub fn map_pos(&self, pos: usize, bias: Bias) -> usize;
    pub fn map_range(&self, range: Range<usize>, mode: MapMode) -> Option<Range<usize>>;

    pub fn is_empty(&self) -> bool;
    pub fn len_before(&self) -> usize;
    pub fn len_after(&self) -> usize;
}

pub enum Bias { Before, After }
pub enum MapMode { TrackBefore, TrackAfter, Drop }  // what to do when range deleted
```

Implementation notes:
- `apply` walks the rope and the op list together in O(n) of changed bytes (untouched
  spans are passed through via slice-and-concat on the sumtree).
- `compose` is the CM6 algorithm: walk both op lists with parallel cursors, merging
  retains/deletes/inserts.
- `invert` requires `before` to capture deleted text into `Insert` ops.

---

## 5. Anchor

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub byte: u32,
    pub bias: Bias,
}

impl Anchor {
    pub fn at(byte: usize, bias: Bias) -> Self;
    pub fn map(self, changes: &ChangeSet) -> Self;
}
```

Just a biased offset for v1. The struct is reserved so it can grow into a CRDT timestamp
later without breaking callers (everyone goes through `Anchor`, never raw `usize`).

---

## 6. Selection

```rust
#[derive(Clone, Copy)]
pub struct SelRange {
    pub anchor: Anchor,
    pub head: Anchor,
    pub goal_col: Option<f32>,
}

#[derive(Clone)]
pub struct Selection {
    ranges: SmallVec<[SelRange; 1]>,
    main: u32,
}

impl Selection {
    pub fn single(pos: usize) -> Self;
    pub fn ranges(&self) -> &[SelRange];
    pub fn main(&self) -> &SelRange;
    pub fn map(&self, changes: &ChangeSet) -> Self;
    pub fn normalize(&mut self);   // sort, merge overlaps
}
```

Normalization runs automatically after every transaction.

---

## 7. RangeSet<T>

Persistent set of `(Range<Anchor>, T)` pairs backed by SumTree. The summary tracks:

```rust
struct RangeSummary {
    count: u32,
    max_end: u32,        // for stabbing queries
}
```

Operations:
```rust
impl<T: Clone> RangeSet<T> {
    pub fn empty() -> Self;
    pub fn insert(&self, range: Range<Anchor>, value: T) -> Self;
    pub fn iter(&self, range: Range<usize>) -> impl Iterator<Item = (&Range<Anchor>, &T)>;
    pub fn map(&self, changes: &ChangeSet) -> Self;     // re-position all ranges
}
```

Used by:
- Decoration sets (one set per provider, layered by precedence).
- Selection match highlights.
- Diff hunks.

---

## 8. State, fields, facets

```rust
pub struct EditorState {
    doc:       Rope,
    selection: Selection,
    fields:    Arc<FieldStore>,     // type-erased map<FieldId, Arc<dyn Any>>
    facets:    Arc<FacetStore>,
    config:    Arc<Configuration>,
}

pub trait StateField: 'static + Send + Sync {
    type Value: 'static + Send + Sync + Clone;
    fn create(state: &EditorState) -> Self::Value;
    fn update(prev: &Self::Value, tx: &Transaction, new_state: &EditorState) -> Self::Value;
}

pub trait FacetKey: 'static + Send + Sync {
    type Input: Clone + 'static + Send + Sync;
    type Output: 'static + Send + Sync + Clone;
    fn combine(inputs: &[Self::Input]) -> Self::Output;
}
```

Access:
```rust
let history: &HistoryField::Value = state.field::<HistoryField>();
let decos:   &Vec<DecorationSet>  = state.facet::<DecorationsFacet>();
```

`Configuration` is computed once when the state is built from an `ExtensionSet`. It
resolves: which fields exist (in dependency order), which providers feed which facets,
and the keymap stack. Reconfiguration is supported via a `Reconfigure` state effect that
swaps the `Configuration`.

---

## 9. Transaction & dispatch

```rust
pub struct Transaction {
    pub changes:     ChangeSet,
    pub selection:   Option<Selection>,
    pub effects:     Vec<StateEffect>,
    pub annotations: Annotations,
}

pub struct TransactionSpec { /* builder fields */ }
impl Transaction { pub fn build(state: &EditorState) -> TransactionSpec; /* … */ }

pub enum StateEffect {
    Reconfigure(Arc<Configuration>),
    ScrollTo(Anchor),
    SetField(FieldId, Arc<dyn Any + Send + Sync>),
    UserDefined(Arc<dyn Any + Send + Sync>),
}

impl EditorState {
    pub fn update(&self, spec: TransactionSpec) -> (Self, Vec<Effect>);
}
```

Pipeline (inside `update`):
1. Build initial `Transaction` from spec.
2. Run `change_filter` facet providers (can veto changes).
3. Run `transaction_filter` facet providers (can rewrite the tx).
4. Apply `changes` → new doc.
5. Map `selection` → new selection; if tx supplied one, use it instead.
6. Run each `StateField::update`.
7. Recompute dirty facets (lazy: only those whose deps changed).
8. Return `(new_state, effects_for_view)`.

`(Vec<Effect>)` is the view-relevant subset (scroll-to, focus changes, IME-cleared, etc.).

---

## 10. History (StateField)

```rust
pub struct HistoryField;

pub struct HistoryValue {
    revisions: Vector<Revision>,
    head: usize,
}

struct Revision {
    parent: Option<u32>,
    last_child: Option<u32>,
    tx: ChangeSet,
    inverse: ChangeSet,
    selection_before: Selection,
    selection_after: Selection,
    timestamp: SystemTime,
    edit_type: EditType,
}

pub enum EditType { Input, Delete, Paste, Indent, Reformat, Other }
```

Coalescing:
- `Input + Input` within 500 ms and adjacent positions → merge into the head revision.
- Any other pair → new revision.

Commands `undo`, `redo`, `undo_selection`, `earlier(duration)`, `later(duration)` operate
on `HistoryValue` by issuing transactions whose `changes` are the precomputed inverses.

---

## 11. Decorations

```rust
pub enum Decoration {
    Mark    { style: MarkStyle, inclusive_start: bool, inclusive_end: bool },
    Line    { style: LineStyle },
    Inline  { side: Side, widget: Arc<dyn InlineWidget> },
    Replace { widget: Option<Arc<dyn InlineWidget>>, block: bool },
    Block   { side: BlockSide, widget: Arc<dyn BlockWidget>, height: f32 },
}

pub trait InlineWidget: Send + Sync {
    fn measure(&self, ctx: &MeasureCtx) -> Size;
    fn paint(&self, ctx: &mut PaintCtx);
    fn eq_widget(&self, other: &dyn InlineWidget) -> bool;
}

pub trait BlockWidget: Send + Sync { /* same shape */ }

pub type DecorationSet = RangeSet<Decoration>;
```

Providers are facet producers:

```rust
pub struct DecorationsFacet;
impl FacetKey for DecorationsFacet {
    type Input = Arc<dyn Fn(&EditorState) -> DecorationSet + Send + Sync>;
    type Output = Vec<DecorationSet>;
    fn combine(inputs: &[Self::Input]) -> Self::Output {
        // Producers are computed lazily by the view, in registration order.
        inputs.iter().map(|f| f(/* needs state */)).collect()
    }
}
```

(Real impl: producers are typed with their state deps; the facet stores producers, and the
view resolves them against the current state when computing visible decorations. This
matters for incremental recompute — only providers whose deps changed re-run.)

---

## 12. View layer (`editor-view`)

### 12.1 Input events (backend-neutral)

```rust
pub enum InputEvent {
    Key { key: Key, mods: Mods, repeat: bool },
    Text(SmolStr),
    Ime(ImeEvent),
    Mouse(MouseEvent),
    Scroll { delta: Vec2 },
    Focus(bool),
    ClipboardPaste(String),
    CopyRequest,
    CutRequest,
}

pub enum ImeEvent {
    Enabled,
    Disabled,
    Preedit { text: SmolStr, cursor: Option<Range<usize>> },
    Commit(SmolStr),
}
```

Host backends translate native events into `InputEvent`. `editor-egui` ships
`pub fn translate(event: &egui::Event) -> Option<InputEvent>`.

### 12.2 IME handling

IME is a state concept, not a rendering concept:
- On `Preedit`, the view stores `pending_ime: Option<(Anchor, SmolStr)>` and emits a
  transient decoration via a dedicated IME field — phantom text at the cursor with an
  underline. No actual edit happens yet.
- On `Commit`, the view dispatches a normal `Transaction` that inserts the committed text,
  clears the IME state.
- `Enabled/Disabled` toggle whether the editor reports IME areas back to the host (host
  positions the OS IME candidate window).

### 12.3 Viewport & height map

```rust
pub struct ViewState {
    pub scroll: f32,            // y offset, pixels
    pub size: Vec2,             // widget size
    pub height_map: HeightMap,
    pub layout_cache: LineLayoutCache,
    pub viewport: Range<usize>, // visible line indices
}

struct HeightMap { /* SumTree<LineHeight> */ }

struct LineHeight { measured: bool, height: f32 }
```

On every state change with a non-empty ChangeSet:
- Map height-map indices through the change (insert/delete lines).
- Invalidate measured flag on lines whose content changed.

### 12.4 Layout cache

Keyed by `(line_idx, content_hash, deco_hash, wrap_width, font_scale)`. LRU, capped at
~viewport_height × 4 entries. Values are an opaque `Box<dyn LineLayout>` so each backend
can cache its own shaped form (an `Arc<Galley>` for the egui backend).

### 12.5 Command engine

```rust
pub trait Command: 'static + Send + Sync {
    fn exec(&self, ctx: &mut CommandCtx) -> bool;
}

pub struct CommandCtx<'a> {
    pub state:    &'a EditorState,
    pub view:     &'a ViewState,
    pub dispatch: &'a mut dyn FnMut(TransactionSpec),
    pub effects:  &'a mut Vec<ViewEffect>,
}
```

Keymap is a facet (layered by precedence). Chord state lives in `ViewState`.

### 12.6 Painter abstraction

```rust
pub trait Surface {
    fn measure(&mut self, run: &TextRun) -> ShapedLine;
    fn fill_rect(&mut self, rect: Rect, color: Color);
    fn paint_glyphs(&mut self, line: &ShapedLine, origin: Pos, color: Color);
    fn paint_underline(&mut self, line: &ShapedLine, origin: Pos, color: Color, style: UnderlineStyle);
    fn set_clip(&mut self, rect: Rect);
    fn cursor_rect(&mut self, rect: Rect, color: Color);
    /* … */
}
```

`editor-egui` implements `Surface` over `egui::Painter`. This is the *only* trait
`editor-view` has on the renderer; everything else is pure logic.

---

## 13. egui adapter (`editor-egui`)

```rust
pub struct EditorWidget<'a> {
    state: &'a EditorState,
    view: &'a mut ViewState,
    dispatch: Box<dyn FnMut(Transaction) + 'a>,
}

impl<'a> egui::Widget for EditorWidget<'a> {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response { /* … */ }
}
```

Inside `ui()`:
1. Allocate the widget rect; check focus.
2. Translate egui events → `InputEvent`s; feed into view's command engine.
3. Update viewport from scroll offset.
4. For each visible line: get layout from cache (or build via egui's font system into a
   `Galley`), wrap in `EguiLineLayout`.
5. Drive paint through the `Surface` impl over `ui.painter()`.

### 13.1 Galley constraints we accept
- `Galley` is immutable; selection rects are painted as separate filled rectangles below
  the glyphs (not baked into the mesh like egui's TextEdit does).
- Per-row height: we manage row positions ourselves (we're not letting egui's layout drive
  the editor), so heading rows just get more vertical space allocated; the glyphs sit at a
  larger font in the same row box.
- Inline widgets: we measure the widget, allocate the gap in the line's `LayoutJob` via a
  fixed-width whitespace section, then paint the widget on top after the glyphs.

---

## 14. Markdown extension (`editor-md`)

Components:
- `MarkdownLanguage` (implements `Language` from editor-core).
- `MarkdownTreeField` — `StateField<Tree>` driven by tree-sitter-markdown (via editor-ts).
- `MarkdownDecorationProvider` — reads the tree + cursor position, emits a `DecorationSet`
  with:
  - `Line { height_scale }` for heading lines,
  - `Mark { bold/italic/strike/code }` for inline spans,
  - `Replace` for syntax markers (`#`, `**`, `*`, `_`, backticks) when cursor not on line,
  - `Inline` widgets for list bullets and task checkboxes,
  - `Block` widgets for images and (later) tables.
- `LiveTableWidget` — block widget for GFM tables.

The "reveal source on cursor line" rule is implemented by reading the selection's main
range row from state and skipping `Replace` decorations on that row.

---

## 15. Diff (`editor-core` + helpers in `editor-view`)

```rust
pub struct Diff {
    pub left: Arc<Rope>,
    pub right: Arc<Rope>,
    pub hunks: Vec<Hunk>,
}

pub struct Hunk {
    pub left: Range<usize>,     // lines
    pub right: Range<usize>,
    pub kind: HunkKind,
    pub intraline: Vec<(Range<usize>, Range<usize>)>,  // word-level
}

pub enum HunkKind { Added, Removed, Modified, Context }
```

Engine: `similar` crate for line diff (Myers) + word diff for intraline. Hunks become a
`DecorationSet` via a diff decoration provider.

View modes:
- Unified: one `EditorView` over `right`, with `Block` decorations rendering removed
  context and intraline `Mark` decorations for word-level emphasis.
- Side-by-side: two `EditorView`s, gutters aligned via shared `HeightMap` (line-pair
  alignment is a new view-level concept — see §17 open question).

---

## 16. Build order

### Phase 1 — core foundation (no rendering yet, all unit-tested)
1. `editor-core` skeleton crate, workspace setup.
2. SumTree + Cursor + Dimension. Property tests.
3. Rope on SumTree. Position-conversion tests vs. naive impl.
4. Anchor + ChangeSet + apply/invert/compose/map_pos. Round-trip property tests.
5. Selection + map.
6. RangeSet<T> + map.
7. EditorState + StateField + Facet + Configuration.
8. Transaction pipeline (`update`), basic effects.
9. HistoryField with coalescing.

### Phase 2 — minimal egui widget (proves the loop)
10. `editor-view` skeleton: `InputEvent`, `ViewState`, viewport, height map (uniform
    heights for now), Surface trait, command engine, default keymap.
11. `editor-egui` skeleton: `EditorWidget`, event translation, basic painter, scroll.
12. Demo: `examples/minimal` — open a string, edit, save, undo. No syntax, no decorations.

### Phase 3 — decorations & multi-cursor
13. Decoration types + painter integration (marks: bold/italic/color/bg).
14. Per-line height scale + Line decorations.
15. Selection rendering with multi-cursor.
16. Multi-cursor commands (add at click, add next occurrence, column select).
17. Selection-occurrence highlight provider (viewport-scoped).
18. Inline + Block widget paint.

### Phase 4 — languages
19. `editor-ts`: tree-sitter wrapper, incremental reparse hooked into a `SyntaxField`.
20. Syntax highlighting decoration provider.
21. `editor-md`: markdown language + live-preview decoration provider (everything but tables).
22. Demo: `examples/markdown`.

### Phase 5 — tables, diff, search
23. `LiveTableWidget` and table cell editing.
24. Diff engine + unified mode.
25. Side-by-side mode + aligned scrolling.
26. Demo: `examples/diff`.
27. Search/replace API + UI hooks.

### Phase 6 — polish
28. IME end-to-end through egui adapter.
29. Performance pass: profile typing on 10 MB files, fix the worst offenders.
30. Default themes (light + dark).
31. Documentation pass.

---

## 16.5 Additional capabilities (post-foundation)

Each of these maps a SPEC entry to a concrete implementation surface.
Ordered roughly from smallest to largest in code impact.

**Status legend**: ✅ implemented · ⚠️ partial · 📋 spec only.

| # | Feature | Status | Where |
|---|---------|--------|-------|
| 16.5.1 | Diagnostics API | ✅ | `editor-core::diagnostic`, `editor-view::diagnostics` |
| 16.5.2 | Tooltip | ✅ | `editor-view::tooltip`, `editor-egui::tooltip` |
| 16.5.3 | Autocomplete | ✅ | `editor-view::completion`, `editor-egui::completion`, `editor-md::completion` |
| 16.5.4 | Auto-pair brackets | ✅ (incl. skip-over-close) | `editor-view::autopair` |
| 16.5.5 | Atomic ranges | ✅ | `editor-view::motion::skip_atomic_ranges` |
| 16.5.6 | Widget event semantics | ⚠️ traits + placeholder paint | `editor-core::decoration`, `editor-egui::widget` |
| 16.5.7 | Drag-and-drop | ✅ | `editor-view::command::handle_mouse_with_mods` |
| 16.5.8 | Serialization | ⚠️ behind `serde` feature; ChangeSet + trait-object decorations TODO | `editor-core/*` |
| 16.5.9 | Post-transaction listeners | ✅ | `editor-core::state::TransactionListener` |
| 16.5.10 | Compartments | ✅ | `editor-core::compartment` |
| 16.5.11 | Soft line wrapping | ✅ | `editor-view::wrap`, `editor-egui::widget` |
| 16.5.12 | Markdown extensions | ✅ (math/mermaid are visual placeholders) | `editor-md/*` |

### 16.5.1 Diagnostics API (SPEC §9.7)

```rust
// editor-core/src/diagnostic.rs
pub struct Diagnostic {
    pub range: std::ops::Range<usize>,    // byte range in the doc
    pub severity: Severity,                // Error / Warning / Info / Hint
    pub message: SmolStr,
    pub source: SmolStr,                   // "rustc", "clippy", "tree-sitter", …
    pub code: Option<SmolStr>,             // e.g. "E0308"
}
pub enum Severity { Error, Warning, Info, Hint }
```

- `DiagnosticsField` is a `StateField<Vec<Diagnostic>>`. Anchors handle mapping
  through edits so diagnostics survive typing.
- A built-in decoration provider turns each diagnostic into:
  - a `Mark` with wavy-underline + severity color over `range`,
  - a `LineStyle::gutter_marker = Some(GutterMarker::Diagnostic(severity))`,
  - a hover-tooltip (§16.5.2) with the message.
- The host writes diagnostics in via a `SetDiagnostics(Vec<Diagnostic>)` state
  effect; the field replaces its contents.

### 16.5.2 Tooltip system (SPEC §9.5)

```rust
// editor-view/src/tooltip.rs
pub struct Tooltip {
    pub anchor: TooltipAnchor,             // BufferPos { byte } | Coords { x, y }
    pub placement: Placement,              // Above / Below / Smart
    pub content: Arc<dyn Fn(&mut TooltipPainter) + Send + Sync>,
}
```

- A `tooltip` facet collects `Vec<Tooltip>` per frame. The widget picks the
  active tooltip from the facet (max one), positions it, and paints it into a
  sub-painter that draws *over* the main editor area.
- In egui, the tooltip area is allocated via `Area::new(id).order(Order::Foreground)`
  so it floats above all editor content and respects screen edges.
- Dismiss-on-blur: when the cursor moves off the anchor (for hover-tooltips) or
  the user presses Escape (for popup-tooltips).
- Used by: diagnostics-on-hover, footnote-definition-on-hover, autocomplete popup.

### 16.5.3 Autocomplete (SPEC §9.6)

```rust
// editor-view/src/completion.rs
pub trait CompletionSource: Send + Sync {
    /// Trigger characters that auto-open the popup (e.g. ['[', '.', '@']).
    fn triggers(&self) -> &[char];
    /// Given the state and the position the user is typing at, return matches.
    fn matches(&self, state: &EditorState, pos: usize) -> Vec<CompletionItem>;
}

pub struct CompletionItem {
    pub label: SmolStr,                    // visible in the popup
    pub detail: Option<SmolStr>,           // right-aligned hint
    pub insert: SmolStr,                   // text to insert
    pub replace_range: Option<Range<usize>>, // defaults to "from trigger to cursor"
    pub kind: CompletionKind,              // for the icon
}
```

- A `completion_sources` facet collects all sources. The view layer maintains
  a `CompletionState { active, items, selected, anchor }` that opens on
  trigger char or Ctrl-Space and closes on commit / Esc / cursor-away.
- Popup is implemented as a Tooltip (§16.5.2) anchored at the cursor.
- v1 ships the framework + a wikilink source in `editor-md`. LSP completion is
  the host's job: it registers a `CompletionSource` that asks the LSP server.

### 16.5.4 Auto-close brackets / auto-pair (SPEC §9.8)

- Implemented as a `transaction_filter` facet. The filter inspects every Insert
  op; if the inserted text is a configured opener (`(`, `[`, `{`, `"`, `` ` ``,
  optionally `<`), it extends the Insert with the matching closer and adjusts
  the resulting selection so the cursor lands between the pair.
- Skip-over-close: if the user types the closing char and the next char in the
  buffer is exactly that char and was auto-inserted in the *immediately previous*
  transaction (tracked via an annotation), drop the insert and just move the
  cursor right by 1.
- Per-language config: each `Language` carries an `AutoPairConfig` listing which
  pairs to enforce.

### 16.5.5 Atomic ranges (SPEC §3.9)

```rust
pub struct MarkStyle {
    /* … */
    pub atomic: bool,
}
pub struct LineStyle { /* … */ pub atomic: bool, }
// `Decoration::Replace` is implicitly atomic.
```

- Cursor motion (`motion::move_char`, `move_word`, etc.) consults the
  decoration set after computing the next position: if the new position lands
  inside an atomic range, snap to the range boundary in the direction of
  motion.
- `step_into_atomic(range, dir) -> usize`: skip-to-boundary helper used by
  every motion command.

### 16.5.6 Widget event semantics (SPEC §3.10)

```rust
pub trait InlineWidget: Send + Sync {
    fn measure(&self, ctx: &MeasureCtx) -> Size;
    fn paint(&self, ctx: &mut PaintCtx);
    fn eq_widget(&self, other: &dyn InlineWidget) -> bool;
    /// If false (default), clicks fall through to cursor placement.
    fn handles_click(&self) -> bool { false }
    /// Called when `handles_click` is true.
    fn on_click(&self, _ctx: &mut WidgetEventCtx) {}
    /// Pixel-precise hit test; default = bounding-rect contains.
    fn hit_test(&self, local: Pos2, size: Size) -> bool { default_rect_hit(local, size) }
}
```

- The widget's mouse handler runs widget hit-tests *before* the normal
  cursor-positioning path. Widget click zones get registered in `ClickZone`
  with a generic `ClickAction::WidgetClick(WidgetId, …)` action.
- Same pattern for `BlockWidget`.

### 16.5.7 Drag-and-drop of selected text (SPEC §7.3)

- Added to `editor-view::command`: on `MouseEvent::Down` inside an existing
  selection range, set `view.drag_state = MaybeDraggingSelection { start, threshold }`.
- On `MouseEvent::Drag` beyond threshold, transition to `DraggingSelection`;
  set the OS-level drag cursor.
- On `MouseEvent::Up` at a position outside the selection: produce a transaction
  that deletes the selection text and inserts it at the drop position (or
  insert-only with Alt held).
- Drop *outside the widget* surfaces as `Action::DragOut { text, mime }` so the
  host can route it to other panes / files.

### 16.5.8 Serialization (SPEC §9.9)

- All public types in `editor-core` derive `serde::Serialize + Deserialize`
  behind a `serde` feature flag.
- `EditorState` serializes:
  - `doc` (as plain UTF-8 string),
  - `selection` (anchors → byte offsets + bias),
  - per-field opt-in: each `StateField` declares
    `const SERIALIZE: bool = false` (default) or true. History defaults false
    (too large); fold sets and language config default true.
- A `SavedState` struct wraps the above so future format-version migration
  has a header to look at.

### 16.5.9 updateListener / post-transaction observers

- Add a `transaction_listener` facet:

```rust
pub type TransactionListener = Arc<
    dyn Fn(&EditorState /*before*/, &EditorState /*after*/, &Transaction)
    + Send + Sync
>;
```

- Called by `EditorState::apply` *after* the new state is built. Listeners
  cannot modify the state (the &refs are immutable); they can stash side
  effects in their own state field via a follow-up dispatch.
- Used by: tooltip-reposition-on-selection-change, fold-state-cleanup,
  diagnostics-decay-on-edit, autosave debouncer.

### 16.5.10 Compartment-style reconfiguration

- A `Compartment` is a typed handle to a swappable subtree of the extension
  graph. Replaces the current "swap the whole `Configuration`" model.

```rust
pub struct Compartment<E: ExtensionGroup> { id: CompartmentId, _phantom: … }
state.reconfigure(theme_compartment, new_theme_extensions);
```

- Implementation: `Configuration` becomes
  `Vec<(CompartmentId, Vec<Extension>)>` instead of a flat list. Reconfigure
  effects target a specific compartment, only that subtree's facet outputs
  recompute.
- Hosts typically have ~3–5 compartments: theme, keymap, language, lints,
  debug-tools.

### 16.5.11 Soft line wrapping (SPEC §3.8)

The biggest change of this list. Architecture:

```rust
// editor-view/src/wrap.rs
pub struct WrapMap {
    /// Per-buffer-line: visual line count + cached x-positions of breaks.
    lines: Vec<WrappedLine>,
}

pub struct WrappedLine {
    pub visual_count: u16,
    pub breaks: SmallVec<[u32; 4]>,   // byte offsets within the buffer line
    pub width: f32,                    // width at which this wrap was computed
}
```

- `WrapMap` invalidates entries whose `width` doesn't match the current
  widget width. It rewraps only visible lines + a margin (the offscreen
  visible-count is still needed for scroll math but is estimated cheaply
  from char count).
- Painter changes:
  - Iterate by visual line within each buffer line.
  - Selection rects clip to per-VLine x-range.
  - Cursor up/down by VLine, not buffer line.
- `HeightMap` per-line height becomes `visual_count * base_line_height`.
- Soft-wrap is a facet (`Soft | Hard`); host enables per editor.

### 16.5.12 Markdown ecosystem extensions (SPEC §4.0)

Each shipped in `editor-md/src/`:

- `wikilink.rs`: regex `\[\[([^\]|]+)(?:\|([^\]]+))?\]\]`. Emits a
  `Replace` decoration whose display is the label, plus an `Inline` widget
  with `handles_click = true` that fires a `WikilinkClicked(target)` event
  through the click sink. `atomic = true` on the Replace so cursor steps over
  the chip in one move.
- `transclusion.rs`: `\!\[\[…\]\]` → `Block(Above|Below)` widget. Content
  rendering is a host callback; the widget supplies the title + placeholder.
- `callout.rs`: matches `> [!type]` prefix in blockquotes. Emits Line bg
  variants by type (note=blue, warning=yellow, tip=green, …) with a left
  border bar (a colored `Mark` over the leading `> ` chars).
- `footnote.rs`: inline `[^id]` → superscript chip; `[^id]: …` def lines
  get dimmed Line bg. Hover popup (Tooltip §16.5.2) shows the definition.
- `frontmatter.rs`: YAML `---\n…\n---` at byte 0 produces a `FoldRegion`
  added to the fold state, collapsed by default. Lives as its own provider
  so non-markdown languages can register their own frontmatter detector.
- Math (`$…$`, `$$…$$`) and Mermaid (` ```mermaid `) — out of v1; reserve
  block-widget shapes for them so adding the renderer later is wiring, not
  architecture.

---

## 16.6 CM6-parity additions

| # | Feature | Status | Where |
|---|---------|--------|-------|
| 16.6.1 | Bracket matching | ✅ | `editor-view::brackets` |
| 16.6.2 | Active-line highlight | ✅ | `editor-view::highlights::active_line_decorations` |
| 16.6.3 | Placeholder text | ✅ | `ViewState::placeholder` + widget paint |
| 16.6.4 | Search engine | ✅ | `editor-view::search` |
| 16.6.4 | Search panel UI | ✅ | `editor-egui::panel::paint_search_panel` |
| 16.6.5 | Indent-on-input (markdown) | ✅ | `editor-md::MarkdownIndent` via `IndentProvider` trait |
| 16.6.6 | Rectangular selection | ✅ | `DragState::RectangleSelecting` |
| 16.6.7 | Special-char rendering | ✅ | `editor-view::special_chars` |
| 16.6.8 | Trailing-whitespace highlight | ✅ | `editor-view::highlights::trailing_whitespace_decorations` |
| 16.6.9 | Scroll past end | ✅ | `ViewState::scroll_past_end` |
| 16.6.10 | Transaction hook filters | ✅ | `editor-core::state` ChangeFilter / TransactionFilter / TransactionExtender |
| 16.6.11 | Drop cursor indicator | ✅ | painter check on `DragState::DraggingSelection` |
| 16.6.12 | Themes as data | ✅ | `editor-core::theme`; all providers accept `Option<&Theme>` |
| 16.6.13 | Panels framework | ✅ | `editor-view::panel`, `editor-egui::panel::paint_panels` |
| 16.6.14 | Snippet expansion + tab stops + mirrors | ✅ | `editor-view::snippet` |
| 16.6.15 | Facet / StateField / ViewPlugin | ⚠️ scaffold only; built-ins not migrated | `editor-core::facet` |
| 16.6.16 | Tree-sitter integration | ⚠️ crate + framework; per-language grammar deps gated, not bundled | `crates/editor-ts/` |
| 16.6.17 | Undo tree | ✅ | `editor-core::history` with parent/last_child + `earlier`/`later` |

### 16.6 CM6-parity additions (post-polish round)

Following a gap audit against CM6 (SPEC §9.10–§9.22, §13). Each item below
maps directly to a SPEC section; sub-§§ sketch the implementation surface.

### 16.6.1 Bracket matching (SPEC §9.10)

`editor-view::brackets`:
```rust
pub struct BracketPair { pub open: char, pub close: char }
pub const DEFAULT_BRACKETS: &[BracketPair] = &[
    BracketPair { open: '(', close: ')' },
    BracketPair { open: '[', close: ']' },
    BracketPair { open: '{', close: '}' },
];
pub fn bracket_match_decorations(
    state: &EditorState,
    pairs: &[BracketPair],
    max_scan: usize,
) -> DecorationSet;
```
Scans up to `max_scan` chars in either direction from each cursor; emits a
`Mark` with `bg: Some(MATCH_COLOR)` on both brackets when a balanced match
is found, or `bg: Some(WARN_COLOR)` on the unmatched bracket otherwise.
String/comment awareness comes from tree-sitter (§13.5); v1 ignores them.

### 16.6.2 Active-line highlight (SPEC §9.11)

`editor-view::highlights::active_line_decorations(state: &EditorState) -> DecorationSet`
returns a `Line { bg: Some(ACTIVE_LINE_BG) }` for each line containing a
selection head. Hosts opt in via `view.decorations.push(active_line_decorations(&state))`.

### 16.6.3 Placeholder text (SPEC §9.12)

`ViewState::placeholder: Option<SmolStr>`. Painter checks `state.doc.is_empty()`
on the first paint of the frame and renders the placeholder dimmed at the
text origin if both are set. Cursor still paints at byte 0; selection /
input still works.

### 16.6.4 Search panel (SPEC §9.13)

New `editor-view::search` module:
```rust
pub struct SearchState {
    pub active: bool,
    pub query: String,
    pub replacement: String,
    pub flags: SearchFlags,    // case, whole-word, regex, in-selection
    pub matches: Vec<Range<usize>>,
    pub current_idx: Option<usize>,
}
pub fn run_search(state: &EditorState, query: &str, flags: SearchFlags) -> Vec<Range<usize>>;
pub fn next_match(state: &EditorState, search: &SearchState) -> Option<usize>;
pub fn replace_current(state: &EditorState, search: &SearchState) -> Option<Transaction>;
pub fn replace_all(state: &EditorState, search: &SearchState) -> Option<Transaction>;
```
Add `search: SearchState` to `ViewState`. egui adapter draws a panel at the
bottom of the editor (§16.6.13 panels) when `search.active`. Cmd-F opens,
Esc closes. The match-highlight decorations come from a new
`search_decorations(state, search)` that emits `Mark { bg }` per match plus
a stronger style on `current_idx`.

### 16.6.5 Indent-on-input (SPEC §9.14)

Implemented as a `transaction_filter` (§16.6.10). Hook the `Enter` key in
command::handle_key; before producing the insert transaction, ask the
active language for indent. For markdown:
```rust
pub fn markdown_indent_on_enter(state: &EditorState, pos: usize) -> Option<String>;
```
Returns a string like `"\n  - "` if the cursor is inside a list item; `"\n"`
otherwise. Empty-list-item Enter returns `"\n"` *and* a delete transaction
for the leading marker (so the user "escapes" the list).

For code (placeholder for languages that follow): a `default_indent` rule
that copies the previous line's leading whitespace.

Tab handling: in `command::handle_key`, when the cursor is at line start +
whitespace-only, insert one indent unit (4 spaces or 1 tab per config).
Shift-Tab removes one indent.

### 16.6.6 Rectangular selection (SPEC §9.15)

`editor-view::command::handle_mouse_with_mods`: when Alt is held during a
mouse drag, transition `DragState` into a new variant `RectangleSelecting`
and on each drag-update compute the column-aligned cursors:
```rust
fn rectangle_cursors(state: &EditorState, view: &ViewState,
                     start_xy: (f32,f32), end_xy: (f32,f32)) -> Vec<SelRange>;
```
Maps `start.y..end.y` → buffer line range; for each line, builds a SelRange
from `(start.x → byte)` to `(end.x → byte)`. Selection is `from_ranges`.

### 16.6.7 Special-char rendering (SPEC §9.16)

A decoration provider over the visible byte range: for each tab / NBSP /
zero-width char it emits a `Replace { display: Some("→") }` or similar.
Toggle bits live in `ViewState::special_chars: SpecialCharsFlags`.

### 16.6.8 Trailing-whitespace highlight (SPEC §9.17)

Decoration provider: for each line, find trailing-space run, emit `Mark { bg
}` over it. Provider runs each frame; cheap because it only scans visible
lines.

### 16.6.9 Scroll past end (SPEC §9.18)

`ViewState::scroll_past_end: f32` (0.0–1.0 = portion of the viewport that
can be empty at the bottom). Implementation: in the scroll bounds clamp,
add `scroll_past_end * view.height` to `max_scroll`.

### 16.6.10 Transaction filters (SPEC §13.4)

```rust
pub type ChangeFilter = Arc<dyn Fn(&EditorState, &ChangeSet) -> Option<ChangeSet> + Send + Sync>;
pub type TransactionFilter = Arc<dyn Fn(&EditorState, Transaction) -> Transaction + Send + Sync>;
pub type TransactionExtender = Arc<dyn Fn(&EditorState, &Transaction) -> Vec<StateEffect> + Send + Sync>;
```
Add three `Vec`s to `EditorState`. `apply()` runs them in order:
1. `change_filter`s (any `None` cancels the change)
2. `transaction_filter`s (each rewrites the whole transaction)
3. `transaction_extender`s (collect extra effects, append to tx)

Auto-indent / read-only-range / paste-cleanup / max-line-length all live as
filters registered by the host or language.

### 16.6.11 Drop cursor indicator (SPEC §9.19)

When `DragState::DraggingSelection { drop_caret }` is active, the painter
draws a thin vertical line at the `drop_caret` position (computed via
existing `paint_cursors` style with a distinct color). No state surgery
needed — the marker already exists.

### 16.6.12 Themes as data (SPEC §9.20)

`editor-core::theme`:
```rust
pub struct Theme {
    pub palette: ThemePalette,   // bg, fg, accent, error, …
    pub tokens: HashMap<SmolStr, Color>,   // syntax tag → color
    pub diff: DiffColors,
    pub diagnostics: DiagnosticColors,
    /* … */
}
pub struct Compartment<Theme>;   // hosts swap themes via reconfigure
```
Built-in: `theme::light_default()`, `theme::dark_default()`. All decoration
providers (markdown, diff, diagnostics) consult `state.compartments.get(theme)`
for their colors instead of hardcoding RGB.

### 16.6.13 Panels (SPEC §9.21)

```rust
pub trait Panel: Send + Sync {
    fn placement(&self) -> PanelPlacement;   // Top | Bottom
    fn height(&self, font_size: f32) -> f32;
    fn paint(&self, ctx: &mut PanelPaintCtx);
}
pub struct PanelStack { panels: Vec<Arc<dyn Panel>> }
```
ViewState gets `panels: PanelStack`. Widget computes `text_rect = rect with
panels stripped from top/bottom` before laying out the editor. egui adapter
calls `panel.paint(ctx)` for each registered panel.

### 16.6.14 Snippet expansion (SPEC §9.22)

`editor-view::snippet`:
```rust
pub struct Snippet { template: String, /* parsed tab-stop spans */ }
impl Snippet {
    pub fn parse(s: &str) -> Result<Self, ParseError>;
    pub fn expand(&self, state: &EditorState, pos: usize) -> Transaction;
}
pub struct SnippetState { pub stops: Vec<Vec<Range<Anchor>>>, pub current: usize }
```
`SnippetState` lives on `EditorState` (initially as a field, later via
`StateField`). Tab/Shift-Tab while a snippet is active cycles to next/prev
stop. Mirrored stops (same number across spans) sync edits.

### 16.6.15 Facet / StateField / ViewPlugin proper (SPEC §13.1–§13.3)

The big refactor. Replaces the current ad-hoc fields-on-EditorState pattern.

```rust
pub trait Facet: 'static + Send + Sync {
    type Input: Clone + 'static;
    type Output: 'static + Clone;
    fn combine(inputs: &[Self::Input]) -> Self::Output;
}
pub struct FacetId(u64);
pub struct FacetStore { /* type-erased map */ }

pub trait StateField: 'static + Send + Sync {
    type T: 'static + Clone + Send + Sync;
    fn create(state: &EditorState) -> Self::T;
    fn update(prev: &Self::T, tx: &Transaction, new_state: &EditorState) -> Self::T;
}
pub struct FieldStore { /* type-erased map keyed by TypeId */ }

pub trait ViewPlugin: Send + Sync {
    fn create(view: &ViewState) -> Self where Self: Sized;
    fn update(&mut self, view: &ViewState, updates: &[Transaction]);
}
```

Migration path: built-in fields (history, fold state, IME, completion,
tooltips, wrap) all become `StateField` / `ViewPlugin` instances. The
public surface changes (`state.history` → `state.field::<HistoryField>()`),
which is breaking. Bundle this with a major version bump.

### 16.6.16 Tree-sitter integration (SPEC §13.5)

New `crates/editor-ts`:
```rust
pub struct TsLanguage {
    pub language: tree_sitter::Language,
    pub highlights_query: String,
    pub injections_query: Option<String>,
    pub locals_query: Option<String>,
    pub indent_query: Option<String>,
    pub folds_query: Option<String>,
}

pub struct TsState {
    pub tree: tree_sitter::Tree,
    /// Highlight ranges as `Vec<(Range<usize>, SmolStr /* tag */)>` for this frame.
    pub highlights: Vec<(Range<usize>, SmolStr)>,
}

pub fn parse(language: &TsLanguage, doc: &str, prev: Option<&TsState>) -> TsState;
pub fn ts_decorations(state: &EditorState, ts: &TsState, theme: &Theme) -> DecorationSet;
```
Incremental parse driven by `ChangeSet → tree_sitter::InputEdit`. Bundles
parsers behind features: `lang-rust`, `lang-python`, `lang-javascript`,
`lang-typescript`, `lang-bash`, `lang-go`, `lang-json`, `lang-yaml`,
`lang-toml`, `lang-html`, `lang-css`. Markdown stays on pulldown-cmark for
the live-preview; tree-sitter handles fenced-code-block injections.

### 16.6.17 Undo tree (upgrade existing History)

Replace `Vec<Revision>` + linear head with:
```rust
pub struct History {
    revisions: Vec<Revision>,
    /// Tree edges: parent pointer per revision.
    /// `head` is the current revision; undo follows parent, redo follows
    /// last_child.
    head: u32,
}
pub struct Revision {
    parent: Option<u32>,
    last_child: Option<u32>,
    /* existing fields */
}
```
Adds `earlier(duration)` / `later(duration)` navigation. New edits after an
undo create a sibling instead of dropping the redo branch.

### 16.6.18 Minimap (SPEC §9.23)

`editor-egui::minimap`. The strip is a host-facing widget over `&EditorState
+ &mut ViewState` (same shape as the main `Widget`). The original renderer
issued one `rect_filled` per visible line **every frame**; that's the scroll
jank SPEC §8 forbids. The fix and the new glyph style are the same move:
rasterize the strip into an offscreen image once, upload it as a single egui
texture, and paint one quad — rebuilding only when inputs that affect pixels
change, never on scroll.

**Texture-backed renderer.** A host-owned `MinimapImage` cache replaces the
per-frame draw loop:
```rust
pub struct MinimapImage {
    tex: Option<egui::TextureHandle>,
    key: u64,            // rebuild fingerprint (below)
    size: [usize; 2],    // strip pixel dims the tex was built at
}
```
Rebuild fingerprint mixes `doc.content_id()`, `decorations.signature`, a
theme/`Options` hash, the strip's pixel `W×H`, and the selected `style`. On a
frame where the key is unchanged the widget just paints the cached texture +
the live overlay (thumb, selection/search marks, interaction); on a change it
re-rasterizes a fresh `egui::ColorImage`, uploads via
`ui.ctx().load_texture(...)` (or `tex.set(...)`), and repaints. This keeps
the egui-side type off `ViewState` exactly like `PaintCache` / the existing
`minimap::Cache` do — the cache lives on the host `Buffer`, threaded in via
`.with_image_cache(...)`.

**Projection (shared, unchanged).** Keep the current `height_map`-driven
mapping: `scale = strip_h / total_content`, per-line `y = y_at_text(line) *
scale`, `h = text_height(line) * scale`. This preserves lockstep with wrap /
heading scale / hidden lines. The rasterizer walks lines and writes each
line's pixel rows `[y, y+h)` into the buffer. When `total_content` exceeds the
strip, rows collapse/blend (VSCode `MinimapSamplingState`-style downsampling
falls out of the projection naturally — multiple lines map to one pixel row).
Texture height is bounded to the strip, so it never approaches GPU max-texture
limits.

**Bars style** (`MinimapStyle::Bars`): port the existing
`measure_lines` + `classify_lines` (`LineMetrics`, `LineKind`) — already memoized
by `minimap::Cache` — but write the bar/indent rects, section rules, and mark
strips **into the pixel buffer** instead of issuing `painter.rect_filled`.
Same look, now O(1) per frame.

**Glyphs style** (`MinimapStyle::Glyphs`, default): a VSCode-style glyph
atlas built by reading back egui's *own* rasterization — no new dependency
(`ab_glyph` is already transitive):
```rust
struct GlyphAtlas {
    cw: usize, ch: usize,          // shared line-box cell (≈ advance × font line-height, ×2 SS)
    cov: Vec<f32>,                 // coverage per cell pixel, indexed by ASCII 0x20..=0x7E
    advance: f32, font_size: f32,  // cache key + per-column step
}
```
Build: lay out printable ASCII once at the editor font size; snapshot the
font atlas (`ui.fonts(|f| f.image())` → `ColorImage`, coverage in alpha) and,
for each glyph, **rasterize it into a shared line-box cell at its true
baseline** — for each cell pixel, map to a point in `[0,advance]×[0,font_h]`,
test whether it lands in the glyph's bitmap rect (`glyph.pos + uv_rect.offset`,
size `uv_rect.size` — epaint's own placement formula, `pos.y` = baseline) and
sample the atlas there. This preserves x-height/cap-height/descenders and a
common baseline, so the text reads correctly; **do NOT** stretch each glyph's
tight bbox to fill the cell (every letter ends up the same height → mush).
Cache keyed on `font_size`; rebuilds only when it changes. Non-ASCII → block
fallback.
Rasterize a line: take the per-span fg colors from the same `LineLayout`
the editor paints (`widget::layout` segments carry the resolved
`Color32`), and for each char blit `sprite[ch] * fg` into the buffer at the
char's x-cell (`alpha * fg over bg`). Indentation, density, and syntax color
are all preserved → a true miniature.

**Live preview + soft-wrap.** When lines are ≥ ~2px tall (i.e. not a huge
doc collapsing to density), glyph rendering goes through the editor's
*display* model rather than raw `doc.line_str`: `widget::layout::display_rows`
reuses `LineLayoutBuilder` per visual row (split by `view.wrap_map`'s
`vlines`) and flattens the decorated segments to `(display_text, fg,
is_widget)` runs — so hidden markdown markers, heading styling, and list
bullet/checkbox widgets render exactly as the editor shows them, and a line
that soft-wraps into N rows occupies N minimap rows. Below ~2px it falls back
to the cheap per-line decimated path (glyphs are sub-pixel; only density
reads). Heading rows are taller *and* wider: markdown emits both
`Line{height_scale}` and `Mark{font_scale}`, so each display row recovers its
font scale (`row_h / (line_h·scale)`) and widens the glyph advance by it —
otherwise headings stretch tall-and-thin. To keep the cell from over-magnifying
short docs, the glyph atlas
cell adapts to the font (`cw≈advance`, `ch≈advance·1.7`) so the blit is ~1:1
when the doc fits and a clean downscale when it doesn't, and plain ink uses
`color_plain` unmultiplied to full opacity with a coverage contrast curve so
small text doesn't wash out.

**Uniform scale.** Bars keep the fit-to-height projection (fill the strip).
Glyphs use `content_scale = min(strip_h/total_content, usable_w/wrap_width)`
so wrapped rows fit the strip width without vertical stretching; a short doc's
glyph strip occupies the top at true aspect. The chosen scale is computed once
in `show` and shared by the texture and the live overlays (thumb / marks /
press-to-scroll) so they stay aligned. The texture rebuild fingerprint folds
in `total_content` + `wrap_map.width()`/`enabled()` so an editor-width reflow
rebuilds the strip.

**Overlays stay live (cheap, off-texture):** the viewport thumb, the
selection / search marks, and all interaction (click/drag-to-scroll via
`is_pointer_button_down_on`, wheel routed through `command::handle(Scroll)`)
remain per-frame painter calls / event handling exactly as today — they
depend on scroll position, so baking them into the texture would force a
rebuild on every scroll frame.

**Config & host wiring.** Add `style: MinimapStyle` to
`minimap::Options`; host maps it from a new `MinimapConfig.style` field
(`core/src/config/sections.rs`, default `glyphs`) in `to_minimap_options`
(`app/src/panels/buffer/mod.rs`). The bar palette knobs stay; glyph mode
ignores bar-specific ones.

**Tests** (`editor-egui`): atlas downsample produces expected alpha for a
known glyph box; the rebuild fingerprint moves iff one of its inputs moves
(and *not* on a pure scroll); projection mapping matches the pre-rework
`y_at_text`/`text_height` math; existing bar classification tests carry over.
Pixel fidelity needs an in-app eyeball.

---

## 17. Open questions to resolve during implementation

These don't block starting; they need answers before the relevant phase.

- **Table editing UX**: when cursor enters a rendered table, do we drop to source view or
  do we stay in widget-edit mode with cell-level cursor navigation? Probably revisit
  during Phase 5 after the simple cases work.
- **Side-by-side row alignment**: do we align via a shared `HeightMap` that knows about
  paired lines, or by inserting invisible block decorations on each side? Probably the
  former; design during Phase 5.
- **Layout cache memory budget**: tunable knob; default at 4× viewport height.
- **Tree-sitter incremental edit boundaries**: tree-sitter's `InputEdit` takes a byte
  range; we need to convert ChangeSet → InputEdits. Multi-edit transactions need careful
  ordering (apply right-to-left).
- **Folding / expansion model** (see SPEC §3.6): a top-level `Folding`
  StateField owning a set of `(range, collapsed)` entries. Surfaces as
  `Replace` decorations for collapsed ranges plus an `Inline` chevron widget
  at the boundary regardless of state. Diff-context "▼ N unchanged lines"
  expansion uses the same primitive, seeded from the Context hunks. Cursor
  motion treats a collapsed fold as one step. Not yet implemented.
- **Whether `editor-view` defines its own font metric trait or just borrows egui's**:
  current plan is its own (`FontMetric`) so the headless path stays clean, and
  `editor-egui` implements it over `epaint::Fonts`.
- **History compression**: when revisions hit some count (say 10k), do we collapse the
  oldest into a base snapshot + new root? Not v1, but design the API so it can be added.

---

## 18. Test strategy

- `editor-core` is fully unit-tested with property tests (proptest) for SumTree, Rope,
  ChangeSet round-trips, Selection mapping, RangeSet mapping, History inverse correctness.
- `editor-view` tested via a `MockSurface` that records draw calls; assertions on what
  gets drawn for given inputs.
- `editor-egui` tested via egui's `Harness` (the headless test harness) — feed events,
  snapshot the produced shapes.
- Markdown / diff have golden-file tests: input doc + cursor → expected decoration set.

Performance regression tests: a benchmark suite typing into a 10 MB file, asserting
frame budget.
