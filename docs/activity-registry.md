# Activity registry

A small registry of activity descriptors that app-shell consumers (sidebar mode switcher, activity bar, hamburger menu, command palette, keybind registry) iterate to discover what to render. Replaces hard-coded enum dispatch in the sidebar / activity-bar mode switchers and lifts per-activity state ownership out of the `PanelStates` god-struct.

The shipped model: an `Activity` exposes an ordered list of render surfaces (`View`s); a registry of `Arc<dyn Activity>` is built at vault open; consumers walk the registry instead of matching on a hardcoded mode enum. Each activity owns its UI state slice on `AppState`, reached through a narrow `Ctx` borrow.


## `Activity` trait

Lives in `app/src/activity/mod.rs`. Each activity module implements it; surface accessors default so an activity only implements what it wants.

```rust
pub trait Activity: Send + Sync {
    fn id(&self) -> &'static str;            // "clusters" — kebab-case, stable
    fn label(&self) -> &'static str;         // "Cluster trees" — human-facing
    fn icon(&self) -> egui::Image<'static>;
    fn keybind_chord(&self) -> Option<&'static str> { None }

    /// Ordered render surfaces. Single-view activities return one
    /// element (rendered headerless); multi-view containers return
    /// several. Empty (default) opts out of the activity bar.
    fn views(&self) -> Vec<&dyn View> { Vec::new() }
    fn view_id(&self, view: &dyn View) -> String { /* "<activity>/<view>" or bare id */ }

    fn on_activity_bar(&self) -> bool { /* non-empty views + LeftBar default */ }
    fn default_location(&self) -> egui_workbench::side_bar::Location { Location::LeftBar }

    fn hamburger(&self) -> Option<&dyn HamburgerEntry>     { None }
    fn activity_bar(&self) -> Option<&dyn ActivityBarItem> { None }
    fn command_palette(&self, ctx: &Ctx<'_>) -> Vec<PaletteCommand> { vec![] }

    fn dispatch_action(&self, ctx: &mut Ctx<'_>, action: &str, args: serde_json::Value)
        -> Result<serde_json::Value, ActionError> { /* default: UnknownAction */ }
}
```

The load-bearing surface is **`View`** — the render-surface trait the mode switcher invokes:

```rust
pub trait View: Send + Sync {
    fn id(&self) -> &'static str;                 // kebab-case, unique within its activity
    fn state_key(&self) -> &'static str { self.id() }  // key for this view's AppState slice
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>);
}
```

- A single-view activity returns one `View` whose `id()` equals the activity id; the wire `view_id` collapses to the bare activity id (byte-identical to a pre-registry mode). Multi-view containers slash: `"<activity>/<view>"`. `Activity::view_id` centralizes this — call sites never hand-build the id.
- `state_key` names this view's per-activity state slice (defaults to the view id; a view backed by a differently-named slice overrides it). `with_ctx` threads that slice in.
- `default_location()` returns `egui_workbench::side_bar::Location` (`LeftBar` / `RightBar`) — which side bar the activity's views dock into. Left (the primary accordion driven by the activity bar) for most; right (the secondary accordion) for chat. `on_activity_bar()` is `true` for any left-bar activity with at least one view.

The other surface sub-traits — `HamburgerEntry`, `ActivityBarItem`, `PaletteCommand` — are defined so the trait vocabulary is stable, but their consumers aren't wired yet, so they carry `#[allow(dead_code)]`. They describe a top-strip hamburger item, an activity-bar item override (the auto-rendered item uses `Activity::icon` + `label`), and a command-palette entry respectively.


## `Ctx`

A per-surface borrow, built fresh by the consumer right before invoking a surface and dropped immediately after, so the borrow window is a single call:

```rust
pub struct Ctx<'a> {
    pub services: &'a mut Services,
    pub nav:      &'a mut NavState,
    pub toasts:   &'a mut Vec<Toast>,
    pub config:   &'a Arc<RwLock<Config>>,
    pub state:    &'a mut dyn std::any::Any,   // the active view's own slice
    // + a read handle to the open vault
}
```

- **`state: &mut dyn Any`** is the key decision: each view downcasts to its own state struct via `ctx.state.downcast_mut::<ClustersState>().unwrap()`. The shell never has to know what each activity stores, and the trait stays object-safe (a registry over heterogeneous activities can't carry a per-impl state type as a generic without exploding).
- **Discipline is by convention.** A surface could in principle reach into another activity's data through `ctx.services`. Code review is the gate; the narrow seam makes wrong cross-activity pokes visible. If an activity genuinely needs more than `Ctx` exposes, that's a signal to either extend `Ctx` deliberately or trim the activity.


## Registry

The registry is built once at vault open from `builtin_activities()` (`app/src/activity/mod.rs`) and stashed on `AppState`.

- **Order is meaningful.** The sidebar mode switcher + activity bar render buttons in registry order. The shipped order: `files`, `clusters`, `trails`, `vault`, `canvases`, `context`, `search`, `trash`, `chat`. `Files` is first (the primary mode).
- **Lookup is `O(N)` over a small N.** No hashmap — callers walk the ordered list (~5–15 entries).
- **Lifetime is per-session.** Vault swap rebuilds the registry. Activities hold no per-vault state; they hold descriptors pointing at state on `AppState`.

The legacy `panels_registry` fallback is **fully retired** — zero live references (only a retirement comment at `workbench_host.rs:307`), removed once `Files` (the last hardcoded mode) migrated.


## State ownership

Each activity owns a directory, e.g.:

```
app/src/clusters/
    mod.rs       # exports the Activity impl + its View(s)
    state.rs     # ClustersState
    sidebar.rs   # the View render body
```

`ClustersState` lives on `AppState` as a top-level field, not inside `PanelStates`. The mode switcher reads the active activity's `View` from the registry and invokes `render` — no `match` on a hardcoded mode enum. This eliminates the `panels <-> sidebar` cross-sibling coupling the old `PanelStates` god-struct caused (per `scripts/check-splits.py` rule 8). `PanelStates` shrinks as activities migrate out; eventually it goes away (deferred, not v1).


## Side panels: `SidePanelStack`

The primary and secondary side bars are each a **`SidePanelStack<Mode>`** (`egui_workbench::side_panel_stack`): a VSCode-style accordion hosting an ordered list of collapsible, reorderable, resizable activity sections. hiker instantiates it as `SidePanelStack<String>`, keyed on view id.

- **Activity-bar click switches.** It focuses the section if already open, else opens that activity in full — replacing the whole stack; it never adds to a split.
- **Adding a section.** The header right-click "Add panel" submenu, or dragging an activity icon into the bar, appends a section.
- **Collapse** is per-section via a twistie; section headers are uniform (no per-section focus highlight, no discrete `+`/`…` buttons — the right-click menu is the single entry point).
- **Persistence is per-vault.** Section set + collapse + per-mode weights + focus + visibility persist to `.hiker/side-panel.json` via `app/src/side_panel_persist.rs`, decoupled from the editor dock layout (`.hiker/layout.json`).
- **No new trait surface.** An activity already declares its `views()`; the stack host just renders one section per open view.

`SidePanelStack` lives in `egui_workbench` (purely UI-shell mechanics) so a second consumer app would get the same accordion with one implementation. (Per-vault layout weights live in a per-mode `weights` map, not a `{ feature_id, size_fraction }` region struct.)


## Modularity: the generic activity family

`egui_workbench::activity` **ships a fully-generic activity-registry foundation**: `Activity<C>`, `SidebarSurface<C>`, `ActivityRegistry<C>`, and a universal `Ctx` base trait — all parameterized over `C: Ctx + ?Sized`, so an activity can be written once and run in any consumer app whose umbrella Ctx satisfies the bound.

- **`Ctx` base trait** — the minimum universal contract, `fn state(&mut self) -> &mut dyn Any`. An app's per-surface Ctx impls this; shared activities reference it.
- **`Activity<C>`** — singleton in the registry, generic over the Ctx bound. An app-owned activity binds the app's concrete umbrella Ctx; a shared activity binds its own minimal *requirement trait* (e.g. `FileTreeCtx`) that each app's umbrella Ctx supertraits, so the same `Arc<Foo>` slots into any app's registry.
- **`ActivityRegistry<C>`** — ordered `Vec<Arc<dyn Activity<C>>>`, built once at startup; `iter()` / `invoke(...)` consumers.
- **`ActionError`** — error type for the inter-activity action seam.

**The hiker app does not currently use this generic family.** It runs its own concrete `View`-trait registry in `app/src/activity/` (the trait shown above is non-generic, bound to hiker's `Ctx<'a>` struct directly). The app's only `egui_workbench` activity import today is `activity_bar::Item`; there are no `impl Activity<C>` outside egui-workbench's own tests.

So the generic path is **built and reserved, not impossible** — the egui-workbench foundation exists (with passing tests) for a future generic host (e.g. a hiker-lite that reuses shared activity render functions). When that lands, app-owned activities keep binding a concrete umbrella Ctx and shared activities bind a per-activity requirement trait; until then, hiker's concrete registry is the simpler shape and carries no per-app duplication cost.

**Tradeoff.** A single generic registry shared across two apps would be cleaner in the abstract, but it asks every consumer to thread the `C` bound and a requirement-trait split through its Ctx. hiker takes the concrete registry now and keeps the generic foundation on the shelf; the cost of that choice is one future refactor (binding the generic trait) if and when a second host appears — paid then, not speculatively now.


## Inter-activity actions

When activity A needs a verb on activity B (the file tree opens a board; search reveals a note in the tree; a panel invalidates the tree cache), the call must NOT take a raw module path like `crate::panels::board::open(...)` — direct cross-module calls re-create exactly the sibling coupling the registry dissolves.

The registry exposes a typed-by-name action seam: `ActivityRegistry::invoke(ctx, activity_id, action, json_args) -> Result<Value, ActionError>` routes to the target activity's `dispatch_action`, which defaults to `ActionError::UnknownAction`.

- **Shape mirrors MCP tools.** Name + JSON args + JSON result is the same surface MCP tools speak (`mcp.md`); an activity's in-process actions and its MCP tools can share an implementation.
- **Typed wrappers where it's worth it.** An activity MAY publish typed helpers next to its impl (e.g. `crate::trails::actions::...`) that wrap a `registry.invoke(...)` call; other activities import these for the type-checked path while the JSON dispatch stays underneath.

What it removes: direct `crate::panels::*::*` calls from sidebar files and direct `app.session.sidebar.*` reads from panel files. What it doesn't remove: hot shared **state**, which stays on `AppState` and is read through `Ctx`. The action seam is for verbs, not hot data.


## Migration plan

Phased; each phase is a coherent landing the user can stop at.

**Phase 1 — Foundation + first activity.** Define `Activity` + `View` + `Ctx` in `app/src/activity/`; build the registry and stash it on `AppState`; extract `clusters` to its own module and implement `Activity` for it; wire the sidebar mode switcher to read from the registry (other modes stay hardcoded until they migrate).

**Phase 2 — Second activity + consumers + action seam.** Migrate `trails` on the same pattern; wire the hamburger menu, activity bar, and command palette to the registry; land `dispatch_action` + `ActivityRegistry::invoke` and replace direct cross-module calls with registry invocations.

**Phase 3 — Generic foundation in `egui_workbench`.** Land the generic `Activity<C>` / `SidebarSurface<C>` / `ActivityRegistry<C>` / `Ctx` family + `ActionError` + `side_panel_stack::SidePanelStack` in `egui_workbench`. hiker keeps its own concrete `Activity` trait + `Ctx<'a>` in `app/src/activity/`; the generic family stays reserved for a future generic host (see "Modularity" above).

**Phase 4 — Remaining activity migrations.** Migrate the rest (`files`/`vault`, `canvases`, `context`, `search`, `trash`, `chat`) to their own activity modules on the concrete trait. Shared-activity candidates (a pure filetree, etc.) would split into a render function plus a thin wrapper Activity if/when the generic host arrives.

**Phase 5 — Second host adopts.** A future generic host (e.g. hiker-lite) defines its own umbrella `Ctx` impl, wraps shared render functions in local Activity impls binding the generic trait, and lights up `SidePanelStack`. This validates the split actually serves two consumers; the trait surface gets revised then if friction shows up.


## Constraints

- **No god-struct expansion.** `PanelStates` stays for non-migrated activities in v1; new activities land directly on `AppState`. Migrated activities remove their `PanelStates` entry.
- **No special-case dispatch in shells.** An activity's `Activity` impl is the only hardcoded reference to it from any consumer; new consumers iterate the registry generically.
- **State stays Rust-owned.** No serde-typed dynamic dispatch on `ctx.state`; the `dyn Any` is type-checked at the activity-impl boundary.
- **Backwards-compatible config.** Old `[vault] sidebar_mode = "..."` lines still load: the field survives only as `sidebar_mode_legacy_ignored` (`core/src/config/sections.rs`, `skip_serializing`) and is ignored at runtime. `vault.sidebar_mode` is gone as a live field.


## Deferred

- **Per-activity persistence config.** A `Activity::persist_state` method joins the trait when ≥2 activities want it; persistence is ad-hoc per activity today.
- **Removal of `PanelStates`.** When every panel is an activity, `PanelStates` goes away.
- **Macro for surface registration.** Could trim boilerplate; revisit when ≥3 activities have shipped.
- **Multi-vault activity registry.** Per-vault rebuild already covers vault swap; a single-process multi-vault model is its own concern (deferred per `design.md`).


## Out of scope

- **Generic dependency injection.** `Ctx` is the seam, deliberately narrow. No service-locator pattern.
- **Multiple instances of an activity.** Each `Activity` is a registry singleton. Multi-instance (two cluster trees side-by-side) is a tab-payload concern, not an activity concern.
- **Cross-activity messaging.** Activities don't talk directly; cross-cutting actions (opening a note) route through `Ctx::services` and the shared open-note path.


## Forward refs

- `docs/editor.md` — sidebar mode switcher (`sidebar-mode-switcher`), activity bar (`activity-bar-*`), command palette (`command-palette`), keybind registry (`keybind-registry`). All are registry consumers.
- `docs/cluster-editor.md` — the cluster editor surface in `app/src/clusters/`. Migration changed file location + ownership, not spec content.
