# Activity registry

A registry of activity descriptors that app-shell consumers (sidebar mode switcher, activity bar, hamburger menu, command palette, keybind registry) iterate to discover what to render. The registry is the **generic activity family in `egui_workbench`** (the shell); each consumer app binds it with its own umbrella `Ctx` and registers activities against it.

## The headline decisions

- **One registry, in the shell.** The generic `Activity<C>` / `View<C>` / `ActivityRegistry<C>` family in `egui_workbench` is *the* registry; `hiker-app` binds it as `Activity<dyn AppCtx>`. There is no second concrete copy. [feature-workbench-home]
status:: done
note:: `egui_workbench::activity` is *the* registry home: the generic `Activity<C>`/`View<C>`/`ActivityRegistry<C>` family + `Ctx` marker base + `ActionError` are the live registry, bound by hiker-app as `Activity<dyn AppCtx>`. The 648-LOC concrete twin in `app/src/activity/` is gone. Binding `C = dyn AppCtx` (a `'static` trait object) lets the registry live on `AppState`; the per-surface borrow comes from `AppCtx::surface_ctx() -> Option<SurfaceCtx<'_>>`, not from a borrowed type parameter — which is what makes the generic family compile without `unsafe`
- **The umbrella `Ctx` is a requirement trait, not a struct.** Each app implements `AppCtx` (a supertrait of the `Ctx` marker base); `render` takes `&mut dyn AppCtx` and opens its borrow with `ctx.surface_ctx(state_key)`, which returns a `SurfaceCtx` of disjoint `AppState` references. Shared activities instead bind their own *minimal* requirement trait (`FileTreeCtx`, …) that every app's `AppCtx` supertraits. [feature-app-ctx]
status:: done
note:: `AppCtx: Ctx` (`app/src/activity/mod.rs`) implemented on `AppState` with a single object-safe `surface_ctx(state_key) -> Option<SurfaceCtx<'_>>` accessor (`SurfaceCtx<'a>` is the disjoint-`AppState`-field borrow). App-owned activities bind `Activity<dyn AppCtx>` and open render with `let ctx = &mut ctx.surface_ctx(self.state_key())`; shared activities bind their own minimal requirement trait (e.g. `FileTreeCtx`) that `AppCtx` supertraits. `defer` lives on `SurfaceCtx`; the `Effect` queue is `AppState::pending_effects`, drained at each surface-invocation site (debug-assert guards against cross-frame leaks)
- **Two-tier feature placement.** A feature's engine is a crate **iff it's reusable without hiker**; otherwise the feature is one directory `app/src/features/<x>/`. Every feature has exactly one app-side home. [feature-feature-home]
status:: planned
note:: Two-tier placement rule: a feature's engine is a crate iff reusable without hiker (editor, hiker-canvas, zxr, hiker-render, hiker-llm, the shell); otherwise the feature is one directory `app/src/features/<x>/` holding its `Activity<dyn AppCtx>` impl + views + state + requirement-trait impl. Collapses the `app/src/*` + `panels/*` + `*_activity/` scatter
- **Generic activities ship in the shell as feature-gated batteries.** Filetree, find/replace, hex view live in `egui_workbench` so every consumer gets them once. They depend on **egui + capability traits only** (a `Vfs`), never the OS, so the shell stays wasm-compilable. [feature-shell-battery] [feature-shell-battery-wasm]
status:: planned
note:: Generic PKM-free activities ship in `egui_workbench` itself, feature-gated, so every consumer gets them once: filetree ([[spec:feature-filetree-shared]]), find/replace, hex view ([[spec:feature-hexview-shared]]). A consumer turns off the ones it doesn't want

[feature-shell-battery-wasm]
status:: planned
note:: A shell battery depends on egui + capability traits only — zero native crates in its body. Filesystem is a `Vfs` capability trait the host fills (`FileTreeCtx::vfs`); native apps supply a `std::fs` impl, wasm a in-memory/IndexedDB one. Keeps `egui_workbench` wasm-compilable; `std::fs`/`walkdir`/`rfd` live in each app's `Ctx` impl
- **A second app validates the split.** `hiker-crawler` is built on the shell; a seam earns its keep only when the crawler pulls through it cleanly. [feature-crawler-shell-consumer]
status:: planned
note:: `hiker-crawler` lives in its own repository and is the validating second consumer: a second composition root over the `egui_workbench` submodule + shared crates (`hiker-llm`, `hiker-theme`) + the shell batteries it reuses (editor/buffer, filetree, find/replace, command palette, chat). A shared seam earns its keep iff the crawler pulls through it cleanly; PKM-only features (clusters/trails/boards/graph/embeddings) stay app-only and are never touched for it

## The generic activity family

`egui_workbench::activity` ships the registry, parameterized over `C: Ctx + ?Sized + 'static`:

```rust
pub trait Ctx {}   // marker base — constrains `Activity<C>`'s C to context types

pub trait Activity<C: Ctx + ?Sized>: Send + Sync {
    fn id(&self) -> &'static str;            // "clusters" — kebab-case, stable
    fn label(&self) -> &'static str;
    fn icon(&self) -> egui::Image<'static>;
    fn keybind_chord(&self) -> Option<&'static str> { None }

    fn views(&self) -> Vec<&dyn View<C>> { Vec::new() }   // ordered render surfaces
    fn view_id(&self, view: &dyn View<C>) -> String { /* "<activity>/<view>" or bare id */ }
    fn on_activity_bar(&self) -> bool { /* non-empty views + LeftBar default */ }
    fn default_location(&self) -> egui_workbench::side_bar::Location { Location::LeftBar }

    fn hamburger(&self) -> Option<&dyn HamburgerEntry<C>>     { None }
    fn activity_bar(&self) -> Option<&dyn ActivityBarItem<C>> { None }
    fn command_palette(&self, ctx: &mut C) -> Vec<PaletteCommand<C>> { vec![] }
    fn dispatch_action(&self, ctx: &mut C, action: &str, args: serde_json::Value)
        -> Result<serde_json::Value, ActionError> { /* default: UnknownAction */ }
}

pub trait View<C: Ctx + ?Sized>: Send + Sync {
    fn id(&self) -> &'static str;                      // kebab-case, unique within its activity
    fn state_key(&self) -> &'static str { self.id() }  // key for this view's state slice
    fn render(&self, ui: &mut egui::Ui, ctx: &mut C);
}
```

- A **single-view** activity returns one `View` whose `id()` equals the activity id; the wire `view_id` collapses to the bare activity id. **Multi-view** containers slash: `"<activity>/<view>"`. `Activity::view_id` centralizes the convention — call sites never hand-build the id. [feature-trait]
status:: done
note:: The generic `Activity<C>` / `View<C>` (+ sub-traits) in `egui_workbench::activity` is the live family; hiker-app binds `Activity<dyn AppCtx>` for all 12 builtins. The 648-LOC concrete `Activity`/`View` twin in `app/src/activity/mod.rs` is deleted
- `default_location()` returns `side_bar::Location` (`LeftBar`/`RightBar`) — which side bar the activity docks into. Left for most; right for chat. `on_activity_bar()` is `true` for any left-bar activity with at least one view. [feature-surface-sidebar]
status:: partial
touches:: [[code:hiker/clusters]]
note:: `app/src/activity/mod.rs` (`SidebarSurface`) + `app/src/clusters/mod.rs:69`. Trait defined; the only impl (`ClustersSidebar`) is a stub that downcasts then prints `(routed via panels_registry in v1)` — the live body still renders through `panels_registry`, not the trait
- The other surface sub-traits — `HamburgerEntry<C>`, `ActivityBarItem<C>`, `PaletteCommand<C>` — describe a top-strip hamburger item, an activity-bar item override (the auto-rendered item uses `Activity::icon` + `label`), and a command-palette entry. [feature-surface-hamburger] [feature-surface-activity-bar] [feature-surface-command-palette]
status:: partial
touches:: [[code:hiker/toolbar]]
note:: `app/src/activity/mod.rs` (`HamburgerEntry`, `#[allow(dead_code)]`) + consumer `app/src/toolbar.rs:574`. Trait defined + iterated; no activity implements it yet, so the registry contributes no entries

[feature-surface-activity-bar]
status:: partial
touches:: [[code:hiker/workbench_host]]
note:: `app/src/activity/mod.rs` (`ActivityBarItem`, `#[allow(dead_code)]`) + `app/src/workbench_host.rs:303`. Override trait defined; activity bar auto-renders from `Activity::icon`/`label`; the override iteration collects into `_override_items` and *discards* it

[feature-surface-command-palette]
status:: planned
note:: `app/src/activity/mod.rs` (`PaletteCommand`, `#[allow(dead_code)]`). Data shape defined; `Activity::command_palette` default returns empty; palette consumer unfinished ([[spec:command-palette]] in `editor.md`)

The trait carries the whole v1 surface set — sidebar / panel / hamburger / activity-bar / command-palette — additively: each surface is opt-out via a `None`-returning default, so an activity declares only the surfaces it uses. The set is convention, not enforced. [feature-surface-additive]
status:: done
note:: `app/src/activity/mod.rs` — trait carries the v1 surface set (sidebar / panel / hamburger / activity-bar / command-palette), each opt-out via `None`. Convention only

The trait is object-safe over heterogeneous activities because `C` is erased to a trait object (`dyn AppCtx`) and no trait method is generic — per-activity state is reached through `C`'s own accessors (hiker's `SurfaceCtx`, below), not a generic on the trait.

## `AppCtx`, `SurfaceCtx`, and requirement traits

`C` is the app's **umbrella requirement trait** — a `'static` trait object the shell never names concretely. It is object-safe and minimal: one accessor that hands back the per-surface borrow.

```rust
// hiker-app
pub trait AppCtx: Ctx + FileTreeCtx /* + other capability traits */ {
    fn surface_ctx(&mut self, state_key: &str) -> SurfaceCtx<'_>;   // the per-surface borrow
}

pub struct SurfaceCtx<'a> {              // disjoint &mut borrows of AppState fields
    pub services: &'a mut Services,
    pub state: &'a mut dyn Any,          // this surface's slice; downcast to the activity's struct
    pub active_path: Option<&'a str>,
    pub effects: &'a mut Vec<Effect>,    // Effect = Box<dyn FnOnce(&mut AppState) + Send>
    // …plus the other shared reads (toasts, config, vault)
}
impl SurfaceCtx<'_> {
    pub fn defer(&mut self, f: impl FnOnce(&mut AppState) + Send + 'static) { /* push to effects */ }
}
```

- An **app-owned** activity (clusters, trails, board) impls `Activity<dyn AppCtx>`. Its `render` opens with one line — `let ctx = &mut ctx.surface_ctx(self.state_key());` — then works against `SurfaceCtx`, with simultaneous disjoint access to `services`/`state`/`active_path`/`defer`. `AppCtx::surface_ctx`'s body resolves the disjoint `AppState`-field borrows + the `state_key` → slice match; that resolution is irreducibly app-specific and stays in `hiker-app`.
- A **shared** activity binds its own minimal requirement trait — `impl Activity<C> for C: FileTreeCtx + ?Sized` — depending only on what it needs, and reaches data through that trait's methods (`ctx.vfs()`), never `SurfaceCtx`. Each app's umbrella `Ctx` supertraits that requirement trait, so the same `Arc<FileTree>` slots into any app's `ActivityRegistry<dyn AppCtx>`. A shared activity never names either app's god-object; its effects (open a note, reveal a row) are methods on *its* requirement trait. [feature-app-ctx]

`C = dyn AppCtx` is a `'static` trait object — which is exactly what lets `ActivityRegistry<dyn AppCtx>` be stored on `AppState` for the session. The per-surface borrow is **not** the type parameter; it is produced on demand by `surface_ctx()` and lives only for the body of one `render`. `AppCtx` exposes no `impl Trait` method (the `defer` sink lives on the concrete `SurfaceCtx`), so it stays object-safe.

## Per-surface borrow

`SurfaceCtx` exposes only the shared seam plus this surface's own state slice. `SurfaceCtx.state` is that slice — each view downcasts it via `ctx.state.downcast_mut::<ClustersState>()`. The shell never knows what an activity stores, and the `dyn Any` is type-checked at the activity-impl boundary, not via serde. [feature-ctx]
status:: done
note:: `Ctx` is a marker base in `egui_workbench` (no methods); the app's per-surface slice rides `SurfaceCtx<'a>` (`app/src/activity/mod.rs`) carrying `services`/`toasts`/`config`/`vault`/`state: &mut dyn Any`/`active_path`/`effects`. The old `with_ctx` is now `AppCtx::surface_ctx(state_key) -> Option<SurfaceCtx<'_>>`

Discipline is by convention: a surface *could* reach another activity's data through `services`, but the narrow seam makes wrong cross-activity pokes visible. A surface needing more than `SurfaceCtx` (or its requirement trait) exposes is a signal to extend deliberately or trim the activity. [feature-state-ownership]
status:: partial
note:: `app/src/state.rs` (`clusters_state`/`trails_state` top-level + `PanelStates`), `clusters/state.rs`, `trails/state.rs`. Clusters/trails/vault state lifted onto `AppState`; everything else stays in `PanelStates` (by design during migration). Target home is `app/src/features/<x>/` per [[spec:feature-feature-home]]; surfaces reach state through `&mut dyn AppCtx`, not `&mut AppState`

Cross-cutting effects that need broad `&mut AppState` (open a note, reveal a path) can't run inside the narrow `SurfaceCtx` borrow. A surface queues them via `SurfaceCtx::defer`; they accumulate on an `AppState` effect field that `surface_ctx()` borrows, and the consumer drains them right after the surface returns, each with full `&mut AppState`. The queue is a shared `AppState` field, so every surface-invocation site drains it synchronously — a debug assertion guards against a surface leaking effects into a later frame.

## Registry

`ActivityRegistry<C>` is an ordered `Vec<Arc<dyn Activity<C>>>`, built once at vault open from the app's builtin list and stashed on `AppState`.

- **Order is meaningful** — the sidebar mode switcher + activity bar render in registry order; `files` is first (the primary mode). [feature-registry]
status:: done
touches:: [[code:hiker/bootstrap]]
note:: `ActivityRegistry<dyn AppCtx>` (the shell's `ActivityRegistry<C>`: `build`/`iter`/`by_id`/`invoke`) is built at `app/src/bootstrap.rs` and stashed on `AppState::activities`; the app's concrete twin is gone
- **Lookup is `O(N)`** over a small N (~5–15) — no hashmap; callers walk the list.
- **Lifetime is per-session** — vault swap rebuilds it; activities hold descriptors, not per-vault state.

The shell surfaces consume the registry generically rather than hardcoding each activity:

- **Sidebar mode switcher** resolves each migrated mode's icon/label from the registry; the body still renders via `panels_registry` while the `SidebarSurface` impls are stubs ([[spec:feature-surface-sidebar]]). [feature-consumer-sidebar]
  status:: partial
  touches:: [[code:hiker/workbench_host]]
  note:: `app/src/workbench_host.rs:316` + `app/src/activity/mod.rs:103`. Sidebar mode switcher resolves icon/label from the registry for migrated ids; the body still renders via `panels_registry` (the `SidebarSurface` impls are stubs, per [[spec:feature-surface-sidebar]])
- **Activity bar** builds its visible list by iterating the registry and filtering `Activity::on_activity_bar()` — no longer the hardcoded `HikerMode::all()`. [feature-consumer-activity-bar]
  status:: partial
  touches:: [[code:hiker/workbench_host]]
  note:: `app/src/workbench_host.rs` (`activity_items`). Visible list is built by iterating the registry and filtering `Activity::on_activity_bar()` — no longer the hardcoded `HikerMode::all()`; each item's id/icon/label come straight from the registered `Activity`. The `ActivityBarItem` override is defined (`#[allow(dead_code)]`) but no activity implements it and `activity_items` does not yet consult it, so per-item overrides / badges are unfilled
- **Toolbar hamburger** iterates the registry's `hamburger()` entries; it becomes functional once an activity implements `HamburgerEntry` (none do yet, per [[spec:feature-surface-hamburger]]). [feature-consumer-hamburger]
  status:: partial
  touches:: [[code:hiker/toolbar]]
  note:: `app/src/toolbar.rs:574` iterates `registry … f.hamburger()`; functional once an activity implements `HamburgerEntry` (none do yet, per [[spec:feature-surface-hamburger]])
- **Command palette** (Phase 2) pulls feature commands at palette-open time via `Activity::command_palette` ([[spec:command-palette]]). [feature-consumer-command-palette]
  status:: planned

## Two-tier feature placement

Every feature has at most two layers; the rule decides which exist:

- **Tier 1 — a crate iff reusable without hiker.** `editor`, `hiker-canvas`, `zxr`, `hiker-render`, `hiker-llm`, `hiker-theme`, the shell — these are crates because another app (crawler, lite, future) can reuse them. Extract *more* into crates only when there's genuinely app-agnostic logic **and** a second consumer pulling it; never speculatively. [feature-feature-home]
- **Tier 2 — one app-side home.** Every feature is exactly one directory `app/src/features/<x>/` holding its `Activity<dyn AppCtx>` impl + `View`s + state + requirement-trait impl. No feature is split across top-level `app/src/*` + `panels/*` + `*_activity/`.

| feature | tier 1 (reusable crate) | tier 2 (`app/src/features/<x>/`) |
| --- | --- | --- |
| canvas | `hiker-canvas/{core,view-core,view}` | `canvas/` |
| zim | `zxr` | `zim/` |
| editor/buffer | `editor/*` | `buffer/` |
| graph | `hiker-render/graph` + `widgets/graph-widgets` | `graph/` |
| chat | `hiker-llm` | `chat/` |
| backlinks · trails · clusters · search · properties | — (indexer queries only) | one dir each |

## Shell batteries and wasm

Generic, PKM-free activities ship in `egui_workbench` itself, feature-gated, so the shell is batteries-included and every consumer gets them once: **filetree** [feature-filetree-shared], **find/replace**, **hex view** [feature-hexview-shared]. A consumer that doesn't want one turns the feature off.
status:: planned
note:: The filetree is a **shell battery** in `egui_workbench` (`FileTreeCtx` = `vfs` + `decorate_row` hook + `move_path`), capability-trait/VFS-pure per [[spec:feature-shell-battery-wasm]]. Model on hiker-lite's pure `panels/filetree.rs` (144 LOC). hiker-app's Vault feature composes it ([[spec:feature-vault-as-feature]])

[feature-hexview-shared]
status:: planned
touches:: [[code:hiker/panels/hex_view]]
note:: Hex view is a **shell battery** in `egui_workbench` (stateless `show(ui, &HexBuffer)` + a tiny `HexViewCtx`); promote from `hiker-lite/src/panels/hex_view.rs`. Simplest battery — no coupling. No hex view in hiker-app today

A shell battery may depend on **egui + capability traits only** — zero native-only crates in its own body. Filesystem needs are a capability trait the host fills: [feature-shell-battery]

```rust
// in the shell — wasm-clean
pub trait Vfs { fn list(&self, dir: &str) -> Vec<Entry>; fn read(&self, p: &str) -> Vec<u8>; /* … */ }
pub trait FileTreeCtx: Ctx { fn vfs(&self) -> &dyn Vfs; }
```

Native apps supply a `std::fs` `Vfs`; wasm apps supply an in-memory/IndexedDB/fetched one. The activity body never names `std::fs`/`walkdir`/`rfd`, so `egui_workbench` stays wasm-compilable; the native-only deps live in each app's `Ctx` impl. An activity that can't be written this way isn't shell-worthy — it stays in `app/` (or a native-only crate). [feature-shell-battery-wasm]

`hiker-app`'s Vault feature *composes* the shell filetree: it supplies a `std::fs` `Vfs` and layers hiker decorations (index markers, trail/board rows + menus) and the referrer-rewriting move through `FileTreeCtx`, rather than reimplementing the tree. [feature-vault-as-feature]
status:: planned
note:: The Vault feature (`app/src/features/vault/`) *composes* the shell filetree battery: supplies a `std::fs` `Vfs` and layers hiker decorations via `FileTreeCtx::decorate_row` (index markers, trail/board rows + menus) + the referrer-rewriting `move_path`. Lands as an `Activity<dyn AppCtx>` from day one — no separate migration. Coupled to [[spec:feature-filetree-shared]]

## Side panels: `SidePanelStack`

The primary and secondary side bars are each a **`SidePanelStack<Mode>`** (`egui_workbench::side_panel_stack`): a VSCode-style accordion hosting an ordered list of collapsible, reorderable, resizable activity sections, instantiated as `SidePanelStack<String>` keyed on view id. [feature-multi-region-sidebar]
status:: partial
touches:: [[code:hiker/side_panel_persist]], [[code:hiker/side_panel_stack]]
note:: `egui-workbench/src/side_panel_stack.rs` — VSCode-style accordion. `SidePanelStack<Mode>` holds an ordered `sections: Vec<Mode>` + per-section `collapsed`/height `weights` + `focused`. Activity-bar click *switches* (focus if already open keeping the arrangement, else open in full — replace the whole stack with that one section; never adds to a split); clicking the focused icon hides the bar (`workspace.rs` click handler). Dragging an activity icon out of the strip and releasing it over the window *adds* that mode as a section (VSCode "drag a view into the sidebar"): `activity_bar.rs` reports `ActivityBarResponse::dropped_out` when a drag releases outside the strip rect, publishes a `drag_active_id()` flag mid-drag, and `side_panel_stack::paint_drop_target` lights up the bar while a drag hovers it. `Render` paints each section: a uniform header (twistie + title = drag handle for reorder + host action buttons; no per-section focus highlight, no `+`/`…` buttons) and, when expanded, the host `side_bar_ui` body. Right-clicking a header opens the panel menu: host `side_bar_actions_menu` + "Add panel" submenu (unopened modes) + "Close panel". Inter-section resize handles transfer height `weights` (clamped to `MIN_BODY`, guarded against the short-window `clamp` inversion). `Workbench::open_primary_panel` (switch) / `add_primary_panel` (add section) / `set_primary_panels` (restore) + `SidePanelStack::restore` / `open_modes` / `collapsed_modes` / `section_weight` / `focused` accessors. No egui_tiles, no tab strip — a lone section looks like the old single side bar. **Persistence:** `app/src/side_panel_persist.rs` writes the arrangement (open sections + collapsed + weights + focused + visibility) to `<vault>/.hiker/side-panel.json` (own `version`, currently 1) — captured + value-compared each autosave tick (no dirty flag since the accordion mutates inside egui_workbench), restored in `bootstrap::open_vault` after tab-state restore. Tests: `egui-workbench/tests/resize.rs` + `side_panel_stack::tests` (switch / split-restore). Drag-reorder + resize need a manual smoke check

- **Activity-bar click switches** — focuses the section if already open, else opens that activity in full (replaces the whole stack; never adds to a split).
- **Adding a section** — the header right-click "Add panel" submenu, or dragging an activity icon into the bar, appends a section.
- **Collapse** is per-section via a twistie; headers are uniform (no per-section focus highlight, no `+`/`…` buttons — the right-click menu is the single entry point).
- **Persistence is per-vault** — section set + collapse + per-mode weights + focus + visibility persist to `.hiker/side-panel.json` (`app/src/side_panel_persist.rs`), decoupled from the editor dock layout (`.hiker/layout.json`).
- **No new trait surface** — an activity already declares `views()`; the stack renders one section per open view.
- **One accordion, no inner collapsible** — a feature panel body renders its content directly under the accordion section header; the Related/Backlinks/Search panels no longer wrap their results in a second `collapsible_header` (that module is removed), and the `search.sections.*_expanded` settings are now inert. [feature-panel-single-accordion]
  status:: done
  note:: evidence: `app/src/panels/{related,backlinks,search}.rs`

`SidePanelStack` lives in `egui_workbench` (pure UI-shell mechanics) so a second consumer gets the same accordion from one implementation.

## Inter-activity actions

When activity A needs a verb on activity B (the file tree opens a board; search reveals a note; a panel invalidates the tree cache), the call must NOT take a raw module path like `crate::panels::board::open(...)` — direct cross-module calls recreate the sibling coupling the registry dissolves.

The registry exposes a typed-by-name action seam: `ActivityRegistry::invoke(ctx, activity_id, action, json_args) -> Result<Value, ActionError>` routes to the target's `dispatch_action` (default `UnknownAction`). [feature-inter-feature-actions]
status:: partial
note:: `app/src/activity/mod.rs` — `ActivityRegistry::invoke` + `Activity::dispatch_action` + `ActionError`, unit-tested (`invoke_routes_to_named_activity` / `invoke_unknown_*`). **Gap:** zero production callers — direct cross-module calls remain (e.g. `app/src/sidebar/files.rs:341` calls `crate::panels::board::open`)

- **Shape mirrors MCP tools** — name + JSON args + JSON result is the surface MCP tools speak (`mcp.md`); an activity's in-process actions and its MCP tools can share an implementation.
- **Typed wrappers where worth it** — an activity MAY publish typed helpers next to its impl that wrap an `invoke(...)` call; other activities import these for the type-checked path while JSON dispatch stays underneath.

It removes direct `crate::panels::*::*` calls and direct `app.session.sidebar.*` reads. It does not remove hot shared **state**, which stays on `AppState` and is read through the borrow. The action seam is for verbs, not hot data.

## Second consumer: the crawler

`hiker-crawler` (the JS/CEF companion) lives in its **own repository** and is the **validating second consumer** of the shell: a second composition root over the `egui_workbench` submodule plus the shared crates (`hiker-llm`, `hiker-theme`) and the shell batteries it reuses (editor/buffer, filetree, find/replace, command palette, chat). [feature-crawler-shell-consumer]

- A shared seam earns its keep iff the crawler pulls through it cleanly — the litmus for promoting a feature to a shared requirement trait or a tier-1 crate.
- PKM-specific features the crawler never wants (clusters, trails, boards, graph, embeddings) stay app-only and are never touched for the crawler's sake.
- `hiker-lite` is a minimal consumer (an editor demo) and a convenient "no PKM" smoke build, not the driver of the design. [feature-hiker-lite-adoption]
status:: planned
note:: hiker-lite is a minimal "no PKM" consumer/editor demo on the shell (file search / find-replace / hex view as shell batteries) — a convenient smoke build, not the driver of the design (that's [[spec:feature-crawler-shell-consumer]])

## Migration and sequencing

Gated behind the substrate refactor landing first (`scratch/substrate_decision.md`) — it delivers the generic `Document` and keeps the two refactors off the same files. The shell-side work (genericizing the registry, collapsing dead crates) is independent of the substrate churn and can land first on its own; the app-side migration waits for a green substrate checkpoint.

1. **Shell registry carries the richer model.** The generic `Activity<C>`/`View<C>`/`ActivityRegistry<C>` in `egui_workbench` gains views/locations/`dispatch_action`/multi-view — additive, tests green standalone.
2. **`AppCtx` + `SurfaceCtx` + one converted activity** (e.g. `search`) as the proof: rename the app's per-surface borrow struct to `SurfaceCtx`, turn its builder into `AppCtx::surface_ctx()`, and move the effect sink onto `AppState`.
3. **Migrate the rest** — each activity's render gains the one `ctx.surface_ctx(self.state_key())` line (the body is otherwise unchanged); **delete the app-side concrete registry** and repoint the shell consumers (mode switcher, activity bar, hamburger, action dispatch) at the generic family. [feature-state-ownership]
4. **Shell batteries** — filetree/find-replace/hex view land in `egui_workbench` as capability-trait-pure, feature-gated activities.
5. **Crawler adopts the shell**, validating each shared seam.

Two Phase-2 features already moved their state onto `AppState` and registered an `Activity`:

- **Clusters** — state on `AppState::clusters_state`; the `Clusters` activity is registered and its activity-bar icon/label resolve from the registry. [feature-cluster-migration]
  status:: partial
  note:: `app/src/clusters/{mod,state,sidebar,panel}.rs` + `builtin_activities`. State on `AppState::clusters_state`; `Clusters` Feature registered; activity-bar icon/label resolve from the registry. **Gap:** `SidebarSurface` body is a stub — live rendering still routes through `panels_registry`; cluster-review tab still via `TabKind::ClusterReview`
- **Trails** — a `Trails` activity with a real sidebar `View`, transient UI state on `AppState::trails_state`, and the trails read/mutated live through core ops (no parallel `.hiker/trails.json` model). [feature-trails-migration]
  status:: done
  note:: `app/src/trails/{mod,state,sidebar,bridge}.rs`. `Trails` Activity (`impl Activity` with a real sidebar `View` rendering `sidebar::TrailsCtx`) + `trails_state` (transient UI state only) on `AppState`. The parallel `.hiker/trails.json` model is gone (drafts removed 2026-06-05); the sidebar reads trails live via `core::trails::list`/`get_trail` and mutates via core ops through `bridge.rs` (`create_trail`/`append_waypoint`/`remove_waypoint`/`set_append_cursor`/`delete_trail`). Sidebar-only in v1 (no `panel.rs`)

The remaining per-feature `app/src/features/<x>/` regrouping is a separate, deferred Phase-3 pass: each relocates a panel into its tier-2 home implementing `Activity<dyn AppCtx>` without changing the registry seam, so it rides after the registry-unify rather than within it.

- **Boards** — full `app/src/boards/{state,panel,actions,mod}.rs` replacing the Phase-2 shim; `panels/board.rs` (1160 LOC) + `panels/boards_index.rs` (129 LOC) move; `boards: HashMap<TabId, Pane>` moves off `PanelStates`. [feature-boards-migration]
  status:: planned
- **Search** — `app/src/search/{state,panel,mod}.rs`; `panels/search.rs` (1005 LOC) moves; `search: State` moves off `PanelStates`. [feature-search-migration]
  status:: planned
- **Related** — `app/src/related/{state,panel,mod}.rs`; `panels/related.rs` (203 LOC) moves; `related: State` moves off `PanelStates`. [feature-related-migration]
  status:: planned
- **Backlinks** — `app/src/backlinks/{state,panel,mod}.rs`; `panels/backlinks.rs` (157 LOC) moves; `backlinks: State` moves off `PanelStates`. [feature-backlinks-migration]
  status:: planned
- **Home** — `app/src/home/{state,panel,mod}.rs`; `panels/home.rs` moves; the vault home `State` moves off `PanelStates`; activity-bar item registered. [feature-home-migration]
  status:: planned
- **Vault graph** — `app/src/vault_graph/{state,panel,mod}.rs`; `panels/graph.rs` moves; the `graph: Option<State>` moves off `PanelStates`. Named `vault_graph` to avoid collision with `clusters/panel/cluster_graph.rs`. [feature-vault-graph-migration]
  status:: planned
- **Chat** — `app/src/chat/` becomes a feature with both a sidebar surface (the docked chat region today on the right) and a panel surface (the agent tab), so chat can be placed anywhere via the registry, not just the right-docked region; the legacy `chat_dock: ChatDockState` field on `PanelStates` deletes as part of this slug. [feature-chat-migration]
  status:: planned

## Constraints

- **No god-struct expansion.** New activities land their state directly on `AppState`; `PanelStates` only shrinks. Migrated activities remove their `PanelStates` entry.
- **No special-case dispatch in shells.** An activity's `Activity` impl is the only hardcoded reference to it; new consumers iterate the registry generically.
- **State stays Rust-owned.** No serde-typed dynamic dispatch on the state slot; the `dyn Any` is type-checked at the activity-impl boundary.
- **Shell stays PKM-free and wasm-compilable.** No hiker types and no native-only crates leak into `egui_workbench`; batteries respect the capability-trait rule.

## Deferred

- **Per-activity persistence config.** An `Activity::persist_state` method joins the trait when ≥2 activities want it; persistence is ad-hoc per activity today.
- **Removal of `PanelStates`.** When every panel is an activity, it goes away.
- **Macro for surface registration.** Could trim boilerplate; revisit at ≥3 shipped activities.
- **`decoration-provider` / shared `item_menu` extraction.** Only worth it behind a real consumer (the crawler pulling canvas/files); not built speculatively.

## Out of scope

- **Compile-time droppable PKM addon crates / a plugin marketplace.** Single-user native app, no ecosystem; feature-dropping has no consumer. The shell + tier-1 crates are reusable, but hiker's PKM features are not split into per-feature droppable crates.
- **Generic dependency injection.** The requirement-trait seam is deliberately narrow; no service-locator.
- **Multiple instances of an activity.** Each `Activity` is a registry singleton; multi-instance is a tab-payload concern.

## Forward refs

- `docs/editor.md` — sidebar mode switcher ([[spec:sidebar-mode-switcher]]), activity bar (`activity-bar-*`), command palette ([[spec:command-palette]]), keybind registry ([[spec:keybind-registry]]). All are registry consumers.
- `docs/cluster-editor.md` — the cluster editor surface in `app/src/features/clusters/`.
