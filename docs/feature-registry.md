# Feature registry

A small registry of feature descriptors that App-shell consumers (sidebar mode switcher, activity bar, hamburger menu, command palette, keybind registry) iterate to discover what to render. Replaces hard-coded enum dispatch in the sidebar / activity-bar mode switchers and lifts per-feature state ownership out of the `PanelStates` god-struct.

**Implementation home:** the Feature trait + Registry + per-app Ctx struct stay in each consumer app's `<app>/src/feature/` (~200 LOC per app, intentionally duplicated). What's *generic* — the multi-region sidebar layout machinery + a universal `Ctx` base trait — lives in `egui_workbench::feature`. Shareable feature *implementations* (file tree, find/replace, hex view, etc.) live in the `hiker-features/` workspace crate as free render functions + per-feature Ctx requirement traits; each app wraps them in its own `Feature` impl. See "Crate placement" below for the rationale.

The headline decisions:

- **`Feature` is a trait.** Each feature module implements it, exposing optional surface descriptors. Returning `None` from a surface method opts out. New surfaces join the trait when ≥2 features want them. [feature-trait] [feature-surface-additive]
- **One registry built at vault open.** `FeatureRegistry` holds an ordered `Vec<Arc<dyn Feature>>` constructed in `bootstrap::open_vault` from the workspace's built-in features plus the loaded plugin features. Surfaces iterate the registry; nothing hardcodes a feature list. [feature-registry]
- **`FeatureCtx<'a>` is the access seam.** Surfaces receive a narrow ctx exposing services, nav, toasts, config, and the feature's own opaque state slice — never raw `&mut AppState`. Discipline: a feature surface touches only its own state + shared services. [feature-ctx]
- **State lives with its feature.** Each feature module owns its UI state (`app/src/clusters/state.rs`, etc.). The registry holds descriptors that point at where state lives, not state itself. Eliminates the `panels <-> sidebar` cross-sibling coupling the current `PanelStates` god-struct causes (per `scripts/check-splits.py` rule 8). [feature-state-ownership]
- **Plugins implement the same trait.** A loaded plugin's manifest declares its surfaces; the host wraps each in an adapter `Feature` impl that joins the registry alongside built-ins. Plugin surfaces appear in the same places without special-case dispatch. [feature-plugin-parity]


## `Feature` trait

```rust
pub trait Feature: Send + Sync {
    fn id(&self) -> &'static str;             // "clusters" — kebab-case, stable
    fn label(&self) -> &'static str;          // "Cluster trees" — human-facing
    fn icon(&self) -> egui::ImageSource<'static>;
    fn keybind_chord(&self) -> Option<&'static str> { None }

    // Surface descriptors — return None to opt out of that surface.
    fn sidebar(&self) -> Option<&dyn SidebarSurface>    { None }
    fn panel(&self) -> Option<&dyn PanelSurface>        { None }
    fn hamburger(&self) -> Option<&dyn HamburgerEntry>  { None }
    fn activity_bar(&self) -> Option<&dyn ActivityItem> { None }
    fn command_palette(&self, ctx: &FeatureCtx<'_>) -> Vec<PaletteCommand> { vec![] }
}
```

[feature-trait]

Sub-traits, each minimal:

- **`SidebarSurface`** [feature-surface-sidebar] — `render(&self, ui, ctx)` for the sidebar mode body. Returns nothing; mutates `ctx.state` for any per-feature UI state changes.
- **`PanelSurface`** [feature-surface-panel] — `render(&self, ui, ctx, payload)` for an opened center-pane tab. `payload` is the tab's serialized state from `TabKind::Feature { id, payload }`.
- **`HamburgerEntry`** [feature-surface-hamburger] — `label`, `keybind_id`, `invoke(&self, ctx)` for a top-strip hamburger menu item.
- **`ActivityItem`** [feature-surface-activity-bar] — `icon`, `tooltip`, `invoke(&self, ctx)` for the leftmost button column. The same icon as `Feature::icon` is the common case but `ActivityItem` can override.
- **`PaletteCommand`** [feature-surface-command-palette] — already shipped per `command-palette` in `editor.md`. The `command_palette` method on `Feature` returns the list at the moment the palette opens, so dynamic entries (per-context commands gated by `FeatureCtx` state) are supported.


## `FeatureCtx`

```rust
pub struct FeatureCtx<'a> {
    pub services:   &'a mut crate::state::Services,
    pub nav:        &'a mut crate::state::NavState,
    pub toasts:     &'a mut Vec<crate::state::Toast>,
    pub config:     &'a std::sync::Arc<std::sync::RwLock<hiker_core::config::Config>>,
    pub state:      &'a mut dyn std::any::Any,
}
```

[feature-ctx]

- **`state: &mut dyn Any`** is the load-bearing decision: each feature downcasts to its own state struct via `ctx.state.downcast_mut::<ClustersState>().unwrap()`. The registry shell never has to know what each feature stores.
- **Why `Any` and not a generic.** A registry over heterogeneous `Feature` impls can't carry a per-impl state type as a generic without exploding. `Any` keeps the trait object-safe.
- **Discipline is by convention, not enforcement.** A feature could in principle grab `ctx.services.read_store` and reach into another feature's data. Code review is the gate; the seam shape makes "wrong" cross-feature pokes visible.

The narrow shape is deliberate. If a feature genuinely needs more than what `FeatureCtx` exposes, that's a signal to either (a) the surface needs a richer ctx (extend `FeatureCtx`, with intent), or (b) the feature is doing too much.


## Registry

`FeatureRegistry::build(vault_session, plugin_host) -> Self` runs once in `bootstrap::open_vault` after services land, before the first frame. Returns `Arc<FeatureRegistry>` stashed on `AppState` (sibling to `vault_session`).

- **Order is meaningful.** Sidebar mode switcher + activity bar render feature buttons in registry order. Built-ins ship in a fixed order (defined in `app/src/feature/builtins.rs`); plugins append.
- **Lookup is `O(N)` over a small N.** No hashmap — features are an ordered list, callers walk it. Roughly 5–15 entries total expected; not worth indexing.
- **Lifetime is per-session.** Vault swap rebuilds the registry from the new `vault_session.plugins`. Features hold no per-vault state themselves; they hold descriptors that *point at* state in `AppState`.


## State ownership

Each feature owns a directory:

```
app/src/clusters/
    mod.rs          # exports the Feature impl
    state.rs        # ClustersState (was ClusterUiState)
    sidebar.rs      # impl SidebarSurface for Clusters
    panel.rs        # impl PanelSurface for Clusters
```

[feature-state-ownership]

`ClustersState` moves out of `PanelStates` and onto `AppState` as a top-level field (`app.clusters_state: ClustersState`). The sidebar's cluster mode body lives in `clusters/sidebar.rs`; the cluster-review panel body lives in `clusters/panel.rs`. The sidebar mode switcher in `app/src/sidebar/mod.rs` reads the feature's `SidebarSurface` from the registry and invokes it — no `match` on a hardcoded mode enum.

The same pattern applies to every migrated feature. `PanelStates` shrinks as features migrate out; eventually it goes away (deferred, not v1).


## Plugin parity

A loaded plugin already declares a `runtime`, capabilities, and (per `plugin-vdom-egui-renderer`) renders a VDOM panel. Once `Feature` exists, the plugin host wraps each enabled plugin in a `PluginFeature` adapter that implements `Feature`. The plugin's manifest declares which surfaces it provides:

```json
{
  "ui": {
    "sidebar": { "icon": "...", "label": "..." },
    "panel":   { "kind": "main" },
    "command_palette": [{"id": "...", "label": "..."}]
  }
}
```

[feature-plugin-parity]

The adapter dispatches surface methods to the plugin's VDOM tree (existing `plugin-vdom-egui-renderer`). Plugin features appear in the same registry, sorted after built-ins.

This is the architectural payoff: third-party surfaces use the same trait the built-ins do. No "plugin sidebar dispatch" code path separate from the built-in one.


## Crate placement

We surveyed references/ (zed, helix, lapce, rerun) for a generic feature-registry pattern that works in Rust + egui. Zed's pattern (GPUI's universal `App` reference accessible from anywhere) doesn't apply — egui has no equivalent framework-global context. Helix's pattern (a `Context<'a>` struct with disjoint `&mut` borrows + a `Component` trait that takes it) is what Phase 1+2 already adopted, and is what stays.

The result: a **helix-shaped split** between what's generic (lives in `egui_workbench`) and what's per-consumer (lives in each app):

### What lives in `egui_workbench::feature`

- **`Ctx` base trait** — a minimal universal contract (`fn state(&mut self) -> &mut dyn Any`). Apps' Ctx structs supertrait this; shareable feature render functions reference it. Doesn't unlock cross-app *Feature trait* sharing, but keeps a shared vocabulary.
- **`side_panel_stack`** — `SidePanelStack<Mode>`: the VSCode-style accordion that hosts the primary side bar as an ordered list of collapsible, reorderable, resizable feature sections. Activity-bar clicks *switch* (focus if open, else open that activity in full — replacing the whole stack; never adds to a split); a `+` header menu or dragging an activity icon in adds sections. Pure egui code keyed only on the host's `Mode` id — both apps get the same multi-panel side-bar machinery. (Earlier plan called this a generic `multi_region` `render_vertical` helper; it shipped as the self-contained `SidePanelStack` instead.)
- **`ActionError`** — error type for inter-feature actions. Generic enough to live here.

### What lives in each consumer app's `<app>/src/feature/`

Per-app — duplicated by design (~200 LOC each):

- `Feature` trait, sub-traits (`SidebarSurface`, `PanelSurface`, `HamburgerEntry`, `ActivityItem`)
- `Ctx<'a>` struct with disjoint `&mut` borrows of the app's state fields
- `Registry` (concrete, not generic)
- `with_ctx` helper that builds a Ctx from disjoint AppState borrows

The trait surface is the same shape across apps (hiker-app's `Feature` and hiker-lite's `Feature` look nearly identical), but each is concrete to its app's `Ctx<'a>` struct. **Rust's trait-object lifetime rules** (`dyn Trait` defaulting to `+ 'static`) prevent a single generic `Feature<C>` trait from accepting non-'static Ctx-impls without unsafe pointer dances; the per-app duplication is the honest cost of avoiding those.

### What lives in `hiker-features/`

The new workspace crate for **shareable feature implementations**. Each shareable feature ships as:

- A **Ctx requirement trait** (e.g. `FileTreeCtx`) declaring the methods the feature needs. Kept minimal so every host can satisfy it. Each app impls this on its `Ctx<'a>` struct.
- A **render function** generic over a type satisfying the requirement trait, e.g.:
  ```rust
  // hiker-features/src/filetree/mod.rs
  pub trait FileTreeCtx {
      fn vfs(&self) -> std::sync::Arc<dyn Vfs>;
      fn open_file(&mut self, rel: &str);
      fn move_path(&mut self, src: &str, dst: &str);
      /// Per-row decoration hook. Default: no decoration. A host that
      /// wants markers / extra context-menu entries on a row returns
      /// them here without the filetree knowing what they mean.
      fn decorate_row(&self, _rel: &str) -> RowDecoration { RowDecoration::default() }
  }

  pub fn render<C: FileTreeCtx + ?Sized>(ui: &mut egui::Ui, ctx: &mut C) {
      let vfs = ctx.vfs();
      // lists dirs, paints expand/collapse, drives drag → ctx.move_path,
      // double-click → ctx.open_file, and paints ctx.decorate_row(rel)
      // markers / appends its menu entries — blind to their meaning.
  }
  ```
- (Optional) typed action helpers for inter-feature calls (`pub fn open_file_action<C: FileTreeCtx + ?Sized>(...) { ... }`).

Each consumer app provides a wrapper Feature impl that holds no state and delegates rendering to the shared free function. The bare case (hiker-lite — no host decorations):

```rust
// hiker-lite: src/features/filetree.rs
use hiker_features::filetree;

impl filetree::FileTreeCtx for crate::feature::Ctx<'_> {
    fn vfs(&self) -> std::sync::Arc<dyn Vfs> { self.vfs.clone() }
    fn open_file(&mut self, rel: &str) { /* open in a hiker-lite buffer */ }
    fn move_path(&mut self, src: &str, dst: &str) { /* plain fs rename */ }
    // decorate_row uses the default → no markers, no extra menu entries.
}

pub struct FileTree;
impl crate::feature::Feature for FileTree {
    fn id(&self) -> &'static str { "filetree" }
    fn sidebar(&self) -> Option<&dyn crate::feature::SidebarSurface> { Some(&FileTreeSidebar) }
    // ...
}

struct FileTreeSidebar;
impl crate::feature::SidebarSurface for FileTreeSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut crate::feature::Ctx<'_>) {
        filetree::render(ui, ctx);  // <-- shared render
    }
}
```

hiker-app calls the same `filetree::render`, but the wrapper is the **Vault feature** (id `vault`), whose `FileTreeCtx` impl supplies the referrer-rewriting `move_path` and a non-default `decorate_row` — see "Pure filetree, vault-aware decorations" below.

### Pure filetree, vault-aware decorations

The shared filetree is a **pure tree**: it lists directories through the `Vfs`, paints expand/collapse, drives drag-to-move, and emits `open(rel)` / `move_path(src, dst)` intents. It knows nothing about trails, boards, the indexer, or wikilinks. The same render is usable identically in both apps; if it ever *can't* be, that's the signal a vault concern has leaked into the tree and belongs in the decoration seam instead.

All hiker-specific behavior lives in the **Vault feature** (`app/src/vault/`, `feature-vault-as-feature`), which *composes* `filetree::render` and layers decorations on top via `FileTreeCtx::decorate_row`:

- index-state markers (busy / skipped glyphs),
- trail rows + waypoint expansion + "Add to trail",
- board-doc detection + "Add to board" + open-in-board-view,
- the referrer-rewriting move policy (wikilink / board-card / trail-waypoint rewrites) supplied as the Vault host's `move_path` impl.

`RowDecoration` carries only opaque paint data + extra context-menu entries (label + action id the host dispatches); the filetree renders them without interpreting them. hiker-lite returns the default (no decoration) and supplies a plain fs-rename `move_path`.

Consequence: **`feature-filetree-shared` and `feature-vault-as-feature` are one coupled unit of work.** The pure tree can't be extracted without simultaneously rehoming the vault decorations into the Vault feature — hiker-app's current `sidebar/files.rs` is ~85% vault concerns, not tree concerns, so it splits (tree → shared crate, decorations → Vault host impl) rather than collapsing into a thin wrapper.

### Cost analysis

**Per shareable feature added to `hiker-features/`:**
- Ctx requirement trait: ~5 LOC
- Render function: the actual feature size
- Per app: ~15-30 LOC for the wrapping Feature impl + the Ctx requirement trait impl

**Per app-only feature:**
- Same as Phase 1+2: a `Feature` impl in `<app>/src/<feature>/` referencing the app's `Ctx<'a>` directly. No extra cost.

### What we lose vs. a fully-generic registry

- Plugin features can't be polymorphic across apps the way they could in a `Feature<C>` design. A plugin built against hiker-app's Ctx interface only works in hiker-app. In practice this is fine — plugin manifests already declare app-specific capabilities, and the plugin host loads them per-app.
- A "single source of truth" Feature trait would be cleaner in the abstract. The per-app duplication is real overhead at refactor time (changing the trait surface means changing it in 2 places). Mitigated by the trait being small (~5-7 methods) and stable.

### What we gain

- **Render functions in `hiker-features/` are genuinely shared.** A FileTree improvement lands once, both apps benefit.
- **Multi-region sidebar layout** is shared via `egui_workbench::side_panel_stack` — both apps get the same accordion (collapsible, reorderable, resizable sections) in their side panels with one implementation.
- **No unsafe code, no HRTB gymnastics.** Standard Rust idioms throughout.
- **The pattern matches helix** — a real-world Rust editor that exercises a similar shape successfully.


## Multi-region side panels

The current sidebar shows ONE active feature surface at a time (Files OR Trails OR Clusters, picked by the mode switcher). With the registry in place, each side panel becomes a *container* that can hold one or more stacked regions, each rendering a different feature's `SidebarSurface`. The user can show trails + file-tree at the same time (top + bottom of the left panel), or any other combination, with a vertical splitter between regions. Same model applies to the right (discovery) panel. [feature-multi-region-sidebar]

The registry doesn't change shape — features still expose `Option<&dyn SidebarSurface>`. What changes is the consumer:

```rust
// New layout state, persisted per-vault.
pub struct SidebarLayout {
    /// Top-to-bottom regions in this panel; size_fraction sums to 1.0.
    pub regions: Vec<SidebarRegion>,
}
pub struct SidebarRegion {
    pub feature_id: String,
    pub size_fraction: f32,
}
```

- **Default is one region.** `regions = [(active_feature_id, 1.0)]` matches today's behavior.
- **Splitter UX.** Drag handle between regions; click-and-drag adjusts the two flanking fractions. Min-height per region (~80px) prevents accidental collapse.
- **Adding a region.** Right-click on a sidebar mode button → "Open in second pane" → appends a region rendering that feature.
- **Removing a region.** Close-x on each region header. Last region can't be closed; the panel toggle hides the whole panel.
- **Per-vault persistence.** `vault.sidebar_layout` + `vault.discovery_layout` extend `settings-write-back`; default to single-region.
- **No new trait surface.** A feature already declares `sidebar()` returning `Option<&dyn SidebarSurface>`. The layout host just calls it once per region the user has open.
- **Compose with the existing collapse.** The whole-panel toggle (`sidebar-toggle-icon`) hides every region in the panel; reopening restores the user's region layout.

Lives in egui_workbench (per "Crate placement") — purely UI-shell mechanics.

**Shipped as** `egui_workbench::side_panel_stack::SidePanelStack<Mode>` (a VSCode-style accordion). Differences from the sketch above: regions are keyed on the host `Mode` id rather than a `SidebarRegion { feature_id, size_fraction }` struct (fractions live in a per-mode `weights` map); activity-bar click *switches* — focusing the section if already open, else opening that activity in full (replacing the whole stack) — rather than always opening a new region. Sections are added via the header right-click "Add panel" submenu or by dragging an activity icon into the bar, collapse via a twistie, and each section header is uniform (no per-section focus highlight, no discrete `+`/`…` buttons — the right-click menu is the single entry point). Per-vault persistence (section set + collapse + weights + focus + visibility) is wired in hiker via `app/src/side_panel_persist.rs` → `.hiker/side-panel.json`.


## Inter-feature actions

When feature A needs to invoke a verb on feature B (the file tree opens a board; search reveals a note in the sidebar tree; the board panel invalidates the file-tree cache), the call should NOT take a raw module path like `crate::panels::board::open(...)`. Direct cross-module calls re-create exactly the sibling-coupling the registry exists to dissolve.

The registry exposes a typed-by-name action seam: [feature-inter-feature-actions]

```rust
impl FeatureRegistry {
    pub fn invoke(
        &self,
        ctx: &mut FeatureCtx<'_>,
        feature_id: &str,
        action: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError>;
}

pub trait Feature: ... {
    fn dispatch_action(
        &self,
        ctx: &mut FeatureCtx<'_>,
        action: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        Err(ActionError::UnknownAction)
    }
}
```

- **Shape mirrors MCP tools.** Name + JSON args + JSON result is the same surface MCP tools already speak (`mcp.md`). A feature's in-process actions and its MCP-exposed tools can share an implementation.
- **Plugin parity.** A plugin feature implements `dispatch_action` against its sandboxed runtime — the action seam is the same one for built-ins and plugins.
- **Typed wrappers where it's worth it.** A feature MAY publish typed helpers next to its `Feature` impl (e.g. `crate::clusters::actions::open_review_tab(ctx, &args)`) that wrap `registry.invoke("clusters", "open-review-tab", json!({...}))`. Other features import these helpers for the type-checked path; the seam underneath is still the JSON dispatch.

What it removes: direct `crate::panels::*::*` calls from sidebar files, direct `app.session.sidebar.*` reads from panel files. Both become `registry.invoke(...)` (or the typed wrapper).

What it doesn't remove: shared **state** that's read frequently. Those still live on `AppState` and are accessed through `ctx.app`. The action seam is for verbs, not for hot data.


## Migration plan

Phased. Each phase is a coherent landing the user can stop at.

**Phase 1 — Foundation + first feature.**

- Define `Feature` trait + `FeatureCtx` + sub-traits in `app/src/feature/`.
- Implement `FeatureRegistry::build`; stash on `AppState`.
- Extract `clusters` to `app/src/clusters/` (move `ClusterUiState` → `ClustersState`, sidebar/panel rendering into sibling files in the new module).
- Implement `Feature` for `Clusters`.
- Wire the sidebar mode switcher to read from the registry (Files/Trails stay hardcoded internally until their migration lands). [feature-consumer-sidebar]
- `check.sh`'s `panels <-> sidebar` coupling violation drops below threshold for clusters' share.

Slugs: `feature-trait`, `feature-ctx`, `feature-registry`, `feature-state-ownership`, `feature-cluster-migration`, `feature-surface-sidebar`, `feature-surface-panel`, `feature-consumer-sidebar`.

**Phase 2 — Second feature + consumers + actions seam.**

- Migrate `trails` to `app/src/trails/` on the same pattern.
- Wire the hamburger menu + activity bar + command palette to read from the registry.
- Land `Feature::dispatch_action` + `FeatureRegistry::invoke` (per "Inter-feature actions" above). Replace direct cross-module calls (`sidebar/files.rs` → `panels::board::*`, `panels/search.rs` → `app.session.sidebar.*`) with registry invocations.

Slugs: `feature-trails-migration`, `feature-consumer-hamburger`, `feature-consumer-activity-bar`, `feature-consumer-command-palette`, `feature-inter-feature-actions`.

**Phase 3 — Extract layout machinery to `egui_workbench` + scaffold `hiker-features/`.**

After surveying references/ (zed / helix / lapce / rerun) the conclusion was that a fully-generic `Feature<C>` trait in `egui_workbench` runs into Rust's trait-object-lifetime rules (`dyn HikerCtx + 'static` can't accept a `CtxImpl<'_>` with non-'static borrows). The realistic split that ships:

- **`egui_workbench::feature`** gets:
  - `Ctx` base trait (`fn state(&mut self) -> &mut dyn Any`)
  - `ActionError` enum
  - `side_panel_stack` module — `SidePanelStack<Mode>`, the accordion side-bar host (collapsible/reorderable/resizable sections). (Shipped in place of the originally-sketched generic `multi_region` `render_vertical` helper.)
- **`hiker-features/`** workspace crate is scaffolded (empty modules) — Phase 4 / 5 will populate it as features move into the shared crate.
- **hiker-app's `Feature` trait + `Ctx<'a>` struct + `Registry` stay in `app/src/feature/`** (helix-pattern). The trait moves out only if a future refactor finds an idiom that handles the trait-object lifetime cleanly.

Slugs: `feature-workbench-home`, `feature-hiker-features-crate`. Multi-region UX (drag handle, "Open in second pane") is Phase 4 polish; v1 of `render_vertical` just renders each region in sequence at its allotted height.

**Phase 4 — Remaining feature migrations + plugin parity.** (Was the old "Phase 3".)

All migrations target hiker-app's `Feature` trait (concrete, not generic). Shared-feature candidates (FileTree, etc.) split into a free render function in `hiker-features/` + a thin Feature wrapper in hiker-app per the "Crate placement" pattern.

- Replace the Phase 2 Boards shim (if any) with a full `app/src/boards/` migration.
- Migrate `search`, `related`, `backlinks`, `home`, `vault_graph`, `chat` to their own feature modules.
- Fold `panels/cluster_graph.rs` into `app/src/clusters/panel/cluster_graph.rs` (Phase 1 finish-up).
- Extract the pure filetree to `hiker-features/src/filetree/` **together with** landing the Vault feature that composes it (see "Pure filetree, vault-aware decorations" above) — one coupled landing, not two. The tree becomes the first shared feature; `sidebar/files.rs` splits into the shared render + the Vault host's decoration impls.
- Land the plugin adapter — concrete to hiker-app's Feature trait (plugin Features bound to one app, per the cost-analysis note above).

Slugs: `feature-boards-migration`, `feature-search-migration`, `feature-related-migration`, `feature-backlinks-migration`, `feature-home-migration`, `feature-vault-graph-migration`, `feature-chat-migration`, `feature-filetree-shared`, `feature-plugin-parity`, `feature-vault-as-feature`.

**Phase 5 — hiker-lite adopts.**

- hiker-lite defines its own `Feature` trait + `Ctx<'a>` struct in `hiker-lite/src/feature/` (mirror of hiker-app's; ~200 LOC).
- File search (`hiker-lite/src/panels/file_search.rs`), find/replace, hex view become Features in `hiker-lite/src/features/`.
- hiker-lite impls `filetree::FileTreeCtx` (etc.) for its `Ctx<'a>` and wraps the shared `hiker_features::filetree::render` in a hiker-lite-local Feature impl — first cross-app code reuse. It returns the default `RowDecoration` and a plain fs-rename `move_path`, exercising the "pure tree, no host decorations" path.
- The hex view promotes into `hiker-features/src/hex_view/` (a tiny `HexViewCtx` + the existing stateless `show(ui, &HexBuffer)` render), wrapped by a thin Feature impl in **both** hiker-lite and hiker-app — the simplest shared feature, no coupling to untangle. [feature-hexview-shared]
- hiker-lite's activity bar + sidebar mode switching wire to its registry, mirroring hiker-app's shape.
- Multi-region sidebar from `egui_workbench::side_panel_stack` lights up.
- Validates the helix-pattern split actually serves two consumer apps cleanly. The trait surface gets revised here (in both apps) if hiker-lite reveals friction. A `FileTreeCtx` method hiker-lite can't satisfy is the coupling alarm — fix it in the shared crate, don't stub it.

Slugs: `feature-hiker-lite-adoption`, `feature-hexview-shared`.


## Constraints

- **No god-struct expansion.** `PanelStates` stays for non-migrated features in v1; new features land directly on `AppState` as top-level fields. Migrated features remove their entry from `PanelStates`.
- **No special-case dispatch in shells.** A feature's `Feature` impl is the only hardcoded reference to it from any UI consumer. New consumers iterate the registry generically.
- **State stays Rust-owned.** No serde-typed dynamic dispatch on `ctx.state`. The `dyn Any` is type-checked at the feature-impl boundary.
- **Backwards-compatible during migration.** Built-in features that haven't migrated still render through their existing call sites. The registry's `iter()` returns only migrated features; the sidebar mode switcher renders both registry features + the legacy hardcoded entries until every mode has migrated.


## Deferred

- **Per-feature persistence config.** A feature might want to persist its sidebar expansion / last-active sub-mode. Today persistence is ad-hoc per feature; a `Feature::persist_state` method joins the trait when ≥2 features want it.
- **Removal of `PanelStates`.** When every panel is a feature, `PanelStates` goes away. Lands at the end of Phase 3.
- **Macro for surface registration.** Could trim boilerplate; revisit when ≥3 features have shipped.
- **Multi-vault feature registry.** Per-vault registry rebuild already covers vault swap; a single-process multi-vault model is its own concern (deferred per `design.md`).


## Out of scope

- **Generic dependency injection.** `FeatureCtx` is the seam, deliberately narrow. No service-locator pattern.
- **Multiple instances of a feature.** Each `Feature` is a singleton in the registry. Multi-instance (e.g. two cluster trees side-by-side) is a tab-payload concern (`PanelSurface::render(payload)`), not a feature concern.
- **Cross-feature messaging.** Features don't talk to each other directly. Cross-cutting actions (e.g. opening a note) route through `FeatureCtx::services` and the shared open-note path.


## Forward refs

- `docs/editor.md` — sidebar mode switcher (`sidebar-mode-switcher`), activity bar (`activity-bar-*`), command palette (`command-palette`), keybind registry (`keybind-registry`). All become registry consumers.
- `docs/cluster-editor.md` — the cluster editor surface that lives in `app/src/clusters/panel.rs` post-migration. Migration doesn't change the spec content; only the file location + ownership.
- `docs/plugins.md` — `plugin-vdom-egui-renderer` is the existing seam plugins go through. The plugin adapter in Phase 4 wraps it. Plugin Features are bound to one consumer app (hiker-app) — cross-app plugin portability is out of scope per the cost-analysis above.
- `references/helix/helix-term/src/compositor.rs` — `Component` trait + `Context<'a>` struct. The closest external precedent for hiker's per-app Feature + Ctx shape.
