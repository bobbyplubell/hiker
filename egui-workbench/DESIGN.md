# egui_workbench — Design Document

This document is the developer-facing complement to `SPEC.md`. It
specifies the crate's internal architecture, public API shape, and
implementation strategy. Read `SPEC.md` first for the user-facing
requirements this document satisfies.

## Architectural foundations

- Substrate: `egui_tiles` v0.13 supplies the dockable tree (Tabs +
  Linear containers, drag-and-drop hit testing, resizable splits, serde
  persistence). egui_workbench is a **thin layer above** egui_tiles
  that adds the missing concepts (activity bar, status bar, distinct
  editor vs panel areas, pinned/preview tabs, layout persistence).
- Coordinate model: the **app window** is partitioned by egui panels:
  - Top: optional title bar (host-supplied, crate provides hook).
  - Left/Right strip: activity bar (`SidePanel`, fixed narrow width).
  - Inside activity bar: primary/secondary side bars (`SidePanel`).
  - Center: the workbench `CentralPanel` containing two stacked
    `egui_tiles::Tree`s (editor area on top, panel area on bottom),
    separated by a host-owned vertical splitter.
  - Bottom: status bar (`TopBottomPanel`, fixed thin height).
- Two trees, not one. Editor area and panel area are **separate
  `Tree<DocumentTab>` instances**. They never mix at the
  egui_tiles level — drags between them are intercepted by the
  `WorkbenchBehavior` and translated into "move tab from editor tree to
  panel tree" operations. This avoids egui_tiles' built-in drop logic
  from accidentally dropping editor tabs into the panel hierarchy.

## Type hierarchy

```
Workbench<Tab, Mode>
├── activity_bar: ActivityBar<Mode>
├── side_bars: HashMap<Mode, SideBarContent>
├── editor_area: EditorArea<Tab>
│   └── tree: Tree<DocumentTab<Tab>>
├── panel_area: PanelArea<Tab>
│   └── tree: Tree<DocumentTab<Tab>>
├── status_bar: StatusBar
└── layout_state: WorkbenchLayout

WorkbenchBehavior<Tab>  (trait, host-implemented)
├── pane_ui(...)            // render a single tab's body
├── on_tab_close(...)       // confirm/intercept close
├── icon_for_tab(...)       // optional tab-strip icon
├── status_cells(...)       // status bar contents
├── side_bar_content(...)   // render a side bar view container
├── activity_items()        // list of activities for the activity bar
└── theme()                 // optional WorkbenchTheme override
```

## Public API surface

### Top-level entry point

```rust
pub struct Workbench<Tab: DocumentTab, Mode: ActivityMode> {
    // private — see workspace.rs
}

impl<Tab, Mode> Workbench<Tab, Mode> {
    pub fn new() -> Self;
    pub fn from_layout(layout: WorkbenchLayout) -> Self;
    pub fn ui(&mut self, ctx: &egui::Context, behavior: &mut impl WorkbenchBehavior<Tab, Mode>);
    pub fn layout(&self) -> WorkbenchLayout;          // borrow for serialization
    pub fn into_layout(self) -> WorkbenchLayout;
    pub fn apply_layout(&mut self, layout: WorkbenchLayout);

    // Tab manipulation (operations the host invokes from its own actions)
    pub fn open_tab(&mut self, tab: Tab, opts: OpenTabOptions) -> TabHandle;
    pub fn close_tab(&mut self, handle: TabHandle) -> bool;
    pub fn focus_tab(&mut self, handle: TabHandle);
    pub fn pin_tab(&mut self, handle: TabHandle, pinned: bool);
    pub fn promote_preview(&mut self, handle: TabHandle);
    pub fn iter_tabs(&self) -> impl Iterator<Item = (TabHandle, &Tab)>;

    // Group manipulation
    pub fn split_active_group(&mut self, dir: SplitDir);
    pub fn close_active_group(&mut self);
    pub fn focus_group(&mut self, idx: usize);
    pub fn focused_group(&self) -> Option<GroupHandle>;

    // Side bar & panel area visibility
    pub fn toggle_primary_side_bar(&mut self);
    pub fn toggle_secondary_side_bar(&mut self);
    pub fn toggle_panel_area(&mut self);
    pub fn set_side_bar_side(&mut self, side: SideBarSide);
}
```

### `DocumentTab` trait

```rust
pub trait DocumentTab: Clone + 'static {
    fn title(&self) -> egui::WidgetText;
    fn icon(&self) -> Option<egui::Image<'static>> { None }
    fn is_dirty(&self) -> bool { false }
    fn tooltip(&self) -> Option<String> { None }
    fn closable(&self) -> bool { true }
}
```

`Serialize + DeserializeOwned` is required only on `Tab` types that
will be persisted via `WorkbenchLayout`. We **do not** force this on
the base trait — apps that don't persist layouts (e.g. a temporary
debugger view) shouldn't pay the bound.

For persistence, we expose:

```rust
pub trait PersistableTab: DocumentTab + Serialize + DeserializeOwned {}
impl<T: DocumentTab + Serialize + DeserializeOwned> PersistableTab for T {}
```

`WorkbenchLayout::serialize` is gated on `Tab: PersistableTab`.

### `TabState`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabState {
    Regular,
    Preview,    // italic, replaced by next preview-open
    Pinned,     // leftmost, smaller, survives "close all"
}
```

Stored alongside each tab in our internal `TabEntry<Tab>` (see below).
Not part of `DocumentTab` because state depends on user actions, not
on the tab's intrinsic identity.

### Internal `TabEntry`

```rust
pub(crate) struct TabEntry<Tab> {
    pub tab: Tab,
    pub state: TabState,
    pub handle: TabHandle,    // stable across reorder/split
}
```

`TabHandle` is a `u64` wrapper, monotonic per `Workbench`. Stable for
the lifetime of the tab; reused only after a tab is closed.

### `WorkbenchBehavior`

```rust
pub trait WorkbenchBehavior<Tab: DocumentTab, Mode: ActivityMode> {
    // === Tab rendering ===

    /// Render the body of a tab in a given pane area.
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tab: &mut Tab,
        ctx: TabUiContext<'_>,
    );

    /// Optional: override per-tab styling. Default returns None
    /// (inherit ambient theme).
    fn tab_style(&self, _tab: &Tab) -> Option<TabStyle> { None }

    // === Tab lifecycle hooks ===

    /// Called when the user clicks the close button. Return `false` to
    /// veto the close (e.g., the host wants to show a save-prompt
    /// modal). The crate will leave the tab open; the host can later
    /// call `Workbench::close_tab` to actually close.
    fn on_tab_close(&mut self, _tab: &Tab) -> bool { true }

    /// Called when a tab's preview state transitions to Regular.
    fn on_preview_promoted(&mut self, _tab: &Tab) {}

    /// Custom context-menu items for a tab. Crate adds Close/Close
    /// Others/Pin/Unpin around this.
    fn tab_context_menu(&mut self, _ui: &mut egui::Ui, _tab: &Tab) {}

    // === Side bar content ===

    /// Render the content of the side bar for a given activity. The
    /// host owns the widgets; the crate provides the chrome (header,
    /// resizer, collapse button).
    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &Mode);

    fn side_bar_title(&self, mode: &Mode) -> egui::WidgetText;

    // === Activity bar ===

    /// The list of activities to render in the activity bar, in order.
    fn activity_items(&self) -> Vec<ActivityItem<Mode>>;

    /// Optional: handle right-click on an activity item.
    fn activity_context_menu(&mut self, _ui: &mut egui::Ui, _mode: &Mode) {}

    // === Status bar ===

    /// Render status bar cells. Use `ui.with_layout` to align left/right.
    fn status_bar_ui(&mut self, ui: &mut egui::Ui);

    // === Theming ===

    /// Per-workbench theme overrides. Default returns the ambient
    /// egui::Style-derived theme.
    fn theme(&self, _style: &egui::Style) -> WorkbenchTheme {
        WorkbenchTheme::from_egui_style(_style)
    }
}
```

### `ActivityItem` / `ActivityMode`

```rust
pub trait ActivityMode: Clone + Eq + std::hash::Hash + 'static {}
impl<T: Clone + Eq + std::hash::Hash + 'static> ActivityMode for T {}

pub struct ActivityItem<Mode> {
    pub mode: Mode,
    pub icon: egui::Image<'static>,
    pub label: String,                  // for hover tooltip + accessibility
    pub badge: Option<ActivityBadge>,
}

pub enum ActivityBadge {
    Dot,                // small unobtrusive indicator
    Count(usize),       // numeric badge
    Text(String),       // arbitrary short text
}
```

### `OpenTabOptions`

```rust
pub struct OpenTabOptions {
    pub state: TabState,                 // default Regular
    pub focus: bool,                     // default true
    pub group: GroupTarget,              // where to open
}

pub enum GroupTarget {
    Focused,              // default — open in the currently-focused group
    NewSplit(SplitDir),   // create a new group by splitting the focused one
    Specific(GroupHandle),
}
```

### `WorkbenchLayout` and persistence

```rust
#[derive(Serialize, Deserialize)]
pub struct WorkbenchLayout {
    pub version: u32,                   // currently 1
    pub primary_side: SideBarSide,
    pub side_bar_visible: bool,
    pub side_bar_width: f32,
    pub secondary_side_bar_visible: bool,
    pub secondary_side_bar_width: f32,
    pub active_activity: Option<String>, // serialized Mode (via Mode: Serialize)
    pub panel_area_visible: bool,
    pub panel_area_height: f32,
    pub editor_tree: SerializedTree<EntryRef>,
    pub panel_tree: SerializedTree<EntryRef>,
    pub tab_entries: Vec<TabEntryDto>,   // tabs referenced by trees
}

#[derive(Serialize, Deserialize)]
pub struct TabEntryDto {
    pub handle: u64,
    pub state: TabState,
    pub payload: serde_json::Value,      // the Tab itself, free-form
}

pub enum SideBarSide { Left, Right }
```

`SerializedTree` wraps `egui_tiles::Tree`'s serde shape but our entries
reference `tab_entries` by handle, so the tree structure and the tab
data are decoupled. This keeps the serialized format flat and
debuggable.

### Schema versioning

```rust
pub enum WorkbenchLayoutVersion { V1 = 1 }

fn migrate(value: serde_json::Value) -> Option<WorkbenchLayout> {
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    match version {
        1 => serde_json::from_value(value).ok(),
        other => {
            tracing::warn!(version = other, "workbench: unknown layout schema; ignoring");
            None
        }
    }
}
```

Future schema bumps add a new `V2` variant and a migration arm.

## Module layout

```
egui-workbench/
├── Cargo.toml
├── SPEC.md
├── DESIGN.md
├── README.md
├── src/
│   ├── lib.rs                  — public re-exports, crate docs
│   ├── activity_bar.rs         — ActivityBar widget, ActivityItem, drag-to-add
│   ├── side_bar.rs             — secondary SideBar host + resize logic
│   ├── side_panel_stack.rs     — primary side bar: accordion of collapsible sections
│   ├── editor_area.rs          — EditorArea: editor groups, tab strip
│   ├── panel_area.rs           — PanelArea: bottom panel host
│   ├── status_bar.rs           — StatusBar widget
│   ├── tab.rs                  — DocumentTab, TabState, TabHandle
│   ├── behavior.rs             — WorkbenchBehavior trait, defaults
│   ├── workspace.rs            — Workbench (top-level), layout state
│   ├── persistence.rs          — WorkbenchLayout, serde, migrate
│   ├── theme.rs                — WorkbenchTheme, ide_defaults
│   ├── drag_drop.rs            — drop indicator paint helpers
│   ├── handle.rs               — TabHandle, GroupHandle types
│   └── internal/
│       ├── tree_adapter.rs     — bridge to egui_tiles::Tree
│       └── enforcement.rs      — invariant checks (post-frame)
├── examples/
│   └── workbench-demo/
│       └── src/main.rs         — demo binary showing requirements
└── tests/
    └── smoke.rs                — kittest harness, frame stability
```

## Key implementation notes

### Two-tree architecture for editor vs panel area

Editor area and panel area are *visually* distinct (top vs bottom)
**and** *behaviorally* distinct (editor tabs can't drop into panel
area and vice versa). The cleanest way to enforce this is **two
independent `Tree<TabHandle>` instances**.

Both trees share a single `Vec<TabEntry<Tab>>` keyed by `TabHandle`.
The trees store only handles; payload lookup is one hashmap lookup.
This means moving a tab between trees is just moving the handle.

### Pinned tabs

egui_tiles' `Tabs` container has no concept of pinning. We enforce
"pinned tabs sort first" by **post-process every frame**: after
`dock.ui()`, walk each `Container::Tabs` and reorder its `children`
vec so pinned-state handles come first. This is O(n) per frame across
all tabs (typically < 30); cheap enough to be invisible.

"Close others" semantics: collected via our own enumeration of
`children`, filtering out pinned entries before issuing closes.

### Preview tabs

The "next preview-open replaces the current preview" semantics live
entirely in our `Workbench::open_tab` logic — when `OpenTabOptions::
state == Preview` and there's already a Preview tab in the target
group, close the existing one first.

Italic rendering is handled in our custom `tab_ui` override on the
`Behavior` impl.

### Drop indicators

egui_tiles 0.13 has `Behavior::paint_drag_preview`. We override it
with a richer implementation:
- For drops on a tab strip: draw a vertical insert bar between tabs.
- For drops on a group body: divide the body into 5 zones (center +
  4 edges) and highlight the active zone with a translucent overlay.

Animation: ease-in over 100ms when a new zone activates.

### Performance discipline

- Hot path (per-frame) does **no allocations**: tab strip iteration
  reuses preallocated `Vec<TabHandle>` scratch buffers.
- The `tab_entries` map is `HashMap<TabHandle, TabEntry<Tab>>`. Lookup
  is O(1).
- Drag-and-drop hit testing reuses egui_tiles'; no extra work.
- Side bar swap on activity click is a single `HashMap::get`.
- Status bar cells render top-to-bottom; no caching needed.

### Two-tree drag-and-drop

When the user drags a tab from the editor tree into the panel area's
visible region, egui_tiles only sees the editor tree. The drag
preview stops at the editor tree's bounds. To support cross-tree
drag, we paint our own drop indicators over the panel area when an
editor tree drag is in progress (and vice versa), then on pointer-up
do a `move_tile_to_container` call across trees manually.

For v0.1 we may **defer cross-tree drag** and require explicit
"move to panel area" / "move to editor area" commands. This greatly
simplifies the model; cross-tree drag is a v0.2 feature.

### Activity bar reorder + visibility

The host supplies the full activity list each frame via
`Host::activity_items`. The bar resolves that against two pieces of
user state held on `ActivityBar`:

- `order: Vec<Mode>` — the user's preferred ordering. A drag commits a
  reorder by rewriting this list; rendering sorts the host list to match
  and appends any unlisted modes in host order.
- `hidden: Vec<Mode>` — modes filtered out of the strip. The per-item
  `Hide` command pushes to this list.

Both are read/written through public accessors (`order` / `set_order`,
`hidden` / `set_hidden`, `is_hidden`, `show_all`) and round-trip through
`WorkbenchLayout` (§8), so visibility and ordering survive a restart.

Right-clicking the strip — an item or its empty area — opens a context
menu carrying a **checkbox per host item** (checked = shown); toggling a
box adds/removes the mode from `hidden`. Because the menu enumerates the
*unfiltered* host list, it is the path that restores a hidden item, and
it is reachable from the empty strip even when every item is hidden.

## Demo binary

The `examples/workbench-demo/` binary demonstrates each SPEC
requirement. It uses a fake "files / search / scm / debug" activity
set and a fake content type that includes both markdown documents
and a faux "terminal" panel. Should compile and run with:

```
cargo run -p workbench-demo
```

## Testing strategy

### Smoke tests (kittest)

- Build a `Workbench` with default layout, run 3 frames in a kittest
  Harness, assert no panic. **Mirrors the smoke test in hiker.**
- Build a workbench, simulate a tab close via `workbench.close_tab`,
  run a frame, assert tab is gone.
- Build a workbench, simulate splitting, run frames, assert focus
  shifts to new group.

### Unit tests

- `TabHandle` allocation: monotonic.
- `WorkbenchLayout` round-trip: serialize → deserialize → equal.
- Pinned tab sorting: regardless of insert order, pinned-first.
- Preview replacement: opening a second preview tab closes the first.
- Schema migration: a v0 JSON returns `None`; a v1 JSON parses.

### Visual snapshots (egui_kittest snapshot feature)

- Default layout at 1400x900.
- Default layout with both side bars open.
- Default layout with panel area open.

Snapshots stored in `tests/snapshots/`; CI compares against committed PNGs.

## Out-of-scope notes

These were considered and explicitly deferred:

- **Floating windows / detached tabs**: requires egui multi-viewport.
  v0.2.
- **Cross-tree drag**: explicit commands suffice for v0.1.
- **Builtin command palette**: host concern (the host has its own).
- **Builtin keybinding system**: host concern.
- **Theme picker UI**: host concern.

## API stability notes

Pre-1.0 (0.x): minor bumps may break the API. `WorkbenchBehavior`
trait is the highest-churn surface. Default impls for every method
soften this — adding a new method is backwards compatible.

Layout schema (`WorkbenchLayout`) bumps independently. v1 stays
forever; v2 adds new fields with defaults. Migration code never deletes.

## Out-of-tree integration story (for hiker)

Once egui_workbench is functional, hiker migrates:

1. Replace `app/src/layout.rs` + `app/src/tabs.rs` + `app/src/panels_registry.rs` with calls into `egui_workbench`.
2. Hiker's `DockTab` enum becomes hiker's `Tab` type — `impl DocumentTab for DockTab`.
3. Hiker's activity items: Files / Clusters / Trails / Search / Related / Backlinks / Chat — each becomes an `ActivityItem<HikerMode>`.
4. Hiker's `actions.rs` panel toggles call `workbench.toggle_primary_side_bar()`, etc.
5. Layout persistence: hiker calls `workbench.layout()` for the dock state and stores it in `.hiker/layout.json` (replacing our current schema).

Estimated migration: ~3 days once egui_workbench v0.1.0 is solid.

## Implementation phases

### Phase A — Skeleton (2 days)
- Empty module files with trait/struct stubs.
- `lib.rs` re-exports compile.
- Demo binary compiles but renders nothing.
- `cargo check` clean.

### Phase B — Activity bar + side bar host (3 days)
- ActivityBar widget renders, clicks toggle side bar visibility.
- SideBar renders the active activity via `WorkbenchBehavior::side_bar_ui`.
- Resize works.

### Phase C — Editor area MVP (5 days)
- Wraps egui_tiles::Tree with our DocumentTab.
- Tab strip, body rendering via `pane_ui`.
- Tab close (with `on_tab_close` hook).
- Group splits via drag-to-edge.
- Active group tracking + focus indicator.

### Phase D — Tab states (3 days)
- Preview tab promotion.
- Pinned tab sorting + "close others" semantics.
- Modified-dot indicator.

### Phase E — Panel area + status bar (3 days)
- Bottom panel area (second tree).
- Maximize / close.
- Status bar widget with cells.

### Phase F — Persistence + theming (3 days)
- WorkbenchLayout serde round-trip.
- Schema versioning + migrate.
- Theme overrides.

### Phase G — Polish (4 days)
- Refined drop indicators.
- Hover tooltips.
- Tab scroll affordances.
- Keyboard nav hooks (Ctrl+1/2/3 etc as commands).

### Phase H — Tests + docs + release (3 days)
- Smoke tests via kittest.
- API docs pass.
- README with screenshots.
- v0.1.0 published.

Total: ~26 days of focused engineering. The original estimate of
3.5-4.5 weeks (~25 days) holds.

## References

When implementing, consult in this priority:

1. `references/rerun/crates/viewer/re_viewport/src/viewport_ui.rs` — the
   gold-standard `egui_tiles::Behavior` impl in production. Models for
   pane_ui, tab_title_for_pane, tab_ui, simplification_options, on_edit.
2. `references/zed/crates/workspace/src/pane.rs` — Rust idioms for tab
   management, close-button hover, pinned/preview, context menus.
3. `references/zed/crates/workspace/src/dock.rs` — dock abstraction
   (left/right/bottom).
4. `references/zed/crates/workspace/src/status_bar.rs` — cell-based
   status bar.
5. `references/zed/crates/workspace/src/pane_group.rs` — split-pane
   geometry.
6. `references/theia/packages/core/src/browser/shell/
   application-shell.ts` — API design (services vs widgets vs handlers).
