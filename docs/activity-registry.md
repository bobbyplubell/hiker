# Activity registry

A registry of activity descriptors that app-shell consumers (sidebar mode switcher, activity bar, hamburger menu, command palette, keybind registry) iterate to discover what to render. The registry is the **generic activity family in `egui_workbench`** (the shell); each consumer app binds it with its own umbrella `Ctx` and registers activities against it.

## The headline decisions

- **One registry, in the shell.** The generic `Activity<C>` / `View<C>` / `ActivityRegistry<C>` family in `egui_workbench` is *the* registry; `hiker-app` binds it as `Activity<dyn AppCtx>`. There is no second concrete copy. [feature-workbench-home]
- **The umbrella `Ctx` is a requirement trait, not a struct.** Each app implements `AppCtx` (a supertrait of the `Ctx` marker base); `render` takes `&mut dyn AppCtx` and opens its borrow with `ctx.surface_ctx(state_key)`, which returns a `SurfaceCtx` of disjoint `AppState` references. Shared activities instead bind their own *minimal* requirement trait (`FileTreeCtx`, …) that every app's `AppCtx` supertraits. [feature-app-ctx]
- **Two-tier feature placement.** A feature's engine is a crate **iff it's reusable without hiker**; otherwise the feature is one directory `app/src/features/<x>/`. Every feature has exactly one app-side home. [feature-feature-home]
- **Generic activities ship in the shell as feature-gated batteries.** Filetree, find/replace, hex view live in `egui_workbench` so every consumer gets them once. They depend on **egui + capability traits only** (a `Vfs`), never the OS, so the shell stays wasm-compilable. [feature-shell-battery] [feature-shell-battery-wasm]
- **A second app validates the split.** `hiker-crawler` is built on the shell; a seam earns its keep only when the crawler pulls through it cleanly. [feature-crawler-shell-consumer]

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
- `default_location()` returns `side_bar::Location` (`LeftBar`/`RightBar`) — which side bar the activity docks into. Left for most; right for chat. `on_activity_bar()` is `true` for any left-bar activity with at least one view. [feature-surface-sidebar]
- The other surface sub-traits — `HamburgerEntry<C>`, `ActivityBarItem<C>`, `PaletteCommand<C>` — describe a top-strip hamburger item, an activity-bar item override (the auto-rendered item uses `Activity::icon` + `label`), and a command-palette entry. [feature-surface-hamburger] [feature-surface-activity-bar] [feature-surface-command-palette]

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

Discipline is by convention: a surface *could* reach another activity's data through `services`, but the narrow seam makes wrong cross-activity pokes visible. A surface needing more than `SurfaceCtx` (or its requirement trait) exposes is a signal to extend deliberately or trim the activity. [feature-state-ownership]

Cross-cutting effects that need broad `&mut AppState` (open a note, reveal a path) can't run inside the narrow `SurfaceCtx` borrow. A surface queues them via `SurfaceCtx::defer`; they accumulate on an `AppState` effect field that `surface_ctx()` borrows, and the consumer drains them right after the surface returns, each with full `&mut AppState`. The queue is a shared `AppState` field, so every surface-invocation site drains it synchronously — a debug assertion guards against a surface leaking effects into a later frame.

## Registry

`ActivityRegistry<C>` is an ordered `Vec<Arc<dyn Activity<C>>>`, built once at vault open from the app's builtin list and stashed on `AppState`.

- **Order is meaningful** — the sidebar mode switcher + activity bar render in registry order; `files` is first (the primary mode). [feature-registry]
- **Lookup is `O(N)`** over a small N (~5–15) — no hashmap; callers walk the list.
- **Lifetime is per-session** — vault swap rebuilds it; activities hold descriptors, not per-vault state.

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

A shell battery may depend on **egui + capability traits only** — zero native-only crates in its own body. Filesystem needs are a capability trait the host fills: [feature-shell-battery]

```rust
// in the shell — wasm-clean
pub trait Vfs { fn list(&self, dir: &str) -> Vec<Entry>; fn read(&self, p: &str) -> Vec<u8>; /* … */ }
pub trait FileTreeCtx: Ctx { fn vfs(&self) -> &dyn Vfs; }
```

Native apps supply a `std::fs` `Vfs`; wasm apps supply an in-memory/IndexedDB/fetched one. The activity body never names `std::fs`/`walkdir`/`rfd`, so `egui_workbench` stays wasm-compilable; the native-only deps live in each app's `Ctx` impl. An activity that can't be written this way isn't shell-worthy — it stays in `app/` (or a native-only crate). [feature-shell-battery-wasm]

`hiker-app`'s Vault feature *composes* the shell filetree: it supplies a `std::fs` `Vfs` and layers hiker decorations (index markers, trail/board rows + menus) and the referrer-rewriting move through `FileTreeCtx`, rather than reimplementing the tree. [feature-vault-as-feature]

## Side panels: `SidePanelStack`

The primary and secondary side bars are each a **`SidePanelStack<Mode>`** (`egui_workbench::side_panel_stack`): a VSCode-style accordion hosting an ordered list of collapsible, reorderable, resizable activity sections, instantiated as `SidePanelStack<String>` keyed on view id. [feature-multi-region-sidebar]

- **Activity-bar click switches** — focuses the section if already open, else opens that activity in full (replaces the whole stack; never adds to a split).
- **Adding a section** — the header right-click "Add panel" submenu, or dragging an activity icon into the bar, appends a section.
- **Collapse** is per-section via a twistie; headers are uniform (no per-section focus highlight, no `+`/`…` buttons — the right-click menu is the single entry point).
- **Persistence is per-vault** — section set + collapse + per-mode weights + focus + visibility persist to `.hiker/side-panel.json` (`app/src/side_panel_persist.rs`), decoupled from the editor dock layout (`.hiker/layout.json`).
- **No new trait surface** — an activity already declares `views()`; the stack renders one section per open view.

`SidePanelStack` lives in `egui_workbench` (pure UI-shell mechanics) so a second consumer gets the same accordion from one implementation.

## Inter-activity actions

When activity A needs a verb on activity B (the file tree opens a board; search reveals a note; a panel invalidates the tree cache), the call must NOT take a raw module path like `crate::panels::board::open(...)` — direct cross-module calls recreate the sibling coupling the registry dissolves.

The registry exposes a typed-by-name action seam: `ActivityRegistry::invoke(ctx, activity_id, action, json_args) -> Result<Value, ActionError>` routes to the target's `dispatch_action` (default `UnknownAction`). [feature-inter-feature-actions]

- **Shape mirrors MCP tools** — name + JSON args + JSON result is the surface MCP tools speak (`mcp.md`); an activity's in-process actions and its MCP tools can share an implementation.
- **Typed wrappers where worth it** — an activity MAY publish typed helpers next to its impl that wrap an `invoke(...)` call; other activities import these for the type-checked path while JSON dispatch stays underneath.

It removes direct `crate::panels::*::*` calls and direct `app.session.sidebar.*` reads. It does not remove hot shared **state**, which stays on `AppState` and is read through the borrow. The action seam is for verbs, not hot data.

## Second consumer: the crawler

`hiker-crawler` (the JS/CEF companion) lives in its **own repository** and is the **validating second consumer** of the shell: a second composition root over the `egui_workbench` submodule plus the shared crates (`hiker-llm`, `hiker-theme`) and the shell batteries it reuses (editor/buffer, filetree, find/replace, command palette, chat). [feature-crawler-shell-consumer]

- A shared seam earns its keep iff the crawler pulls through it cleanly — the litmus for promoting a feature to a shared requirement trait or a tier-1 crate.
- PKM-specific features the crawler never wants (clusters, trails, boards, graph, embeddings) stay app-only and are never touched for the crawler's sake.
- `hiker-lite` is a minimal consumer (an editor demo) and a convenient "no PKM" smoke build, not the driver of the design. [feature-hiker-lite-adoption]

## Migration and sequencing

Gated behind the substrate refactor landing first (`scratch/substrate_decision.md`) — it delivers the generic `Document` and keeps the two refactors off the same files. The shell-side work (genericizing the registry, collapsing dead crates) is independent of the substrate churn and can land first on its own; the app-side migration waits for a green substrate checkpoint.

1. **Shell registry carries the richer model.** The generic `Activity<C>`/`View<C>`/`ActivityRegistry<C>` in `egui_workbench` gains views/locations/`dispatch_action`/multi-view — additive, tests green standalone.
2. **`AppCtx` + `SurfaceCtx` + one converted activity** (e.g. `search`) as the proof: rename the app's per-surface borrow struct to `SurfaceCtx`, turn its builder into `AppCtx::surface_ctx()`, and move the effect sink onto `AppState`.
3. **Migrate the rest** — each activity's render gains the one `ctx.surface_ctx(self.state_key())` line (the body is otherwise unchanged); **delete the app-side concrete registry** and repoint the shell consumers (mode switcher, activity bar, hamburger, action dispatch) at the generic family. [feature-state-ownership]
4. **Shell batteries** — filetree/find-replace/hex view land in `egui_workbench` as capability-trait-pure, feature-gated activities.
5. **Crawler adopts the shell**, validating each shared seam.

The per-feature `app/src/features/<x>/` regrouping (`feature-boards-migration`, `feature-search-migration`, `feature-related-migration`, `feature-backlinks-migration`, `feature-home-migration`, `feature-vault-graph-migration`, `feature-chat-migration`) is a separate, deferred pass: each relocates a panel into its tier-2 home implementing `Activity<dyn AppCtx>` without changing the registry seam, so it rides after the registry-unify rather than within it.

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

- `docs/editor.md` — sidebar mode switcher (`sidebar-mode-switcher`), activity bar (`activity-bar-*`), command palette (`command-palette`), keybind registry (`keybind-registry`). All are registry consumers.
- `docs/cluster-editor.md` — the cluster editor surface in `app/src/features/clusters/`.
