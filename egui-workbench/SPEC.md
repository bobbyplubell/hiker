# egui_workbench — User-facing Specification

## Purpose

A reusable layout crate that gives any egui application the configurable
multi-pane workbench surface used by most modern IDE / editor / design
tools (JetBrains, Figma, Ableton, Theia, Trilium, Obsidian). The user
gets a familiar, dockable, persistable workspace; the host application
provides the content.

## Conceptual model

A *workbench* is a single full-window layout composed of these regions:

```
┌───────────────────────────────────────────────────────┐
│ Title bar (optional — host owns native title chrome)  │
├──┬───────────────┬──────────────────────┬─────────────┤
│  │               │                      │             │
│  │   Primary     │   Editor area        │  Secondary  │
│  │   side bar    │   (multiple groups,  │  side bar   │
│ A│   (one view   │    each with a tab   │  (optional, │
│ c│   container)  │    strip + splits)   │   any view  │
│ t│               │                      │  container) │
│ i│               │                      │             │
│ v│               │                      │             │
│ i│               ├──────────────────────┤             │
│ t│               │  Panel area          │             │
│ y│               │  (terminal-shaped,   │             │
│  │               │   optional, tabbed)  │             │
│  │               │                      │             │
├──┴───────────────┴──────────────────────┴─────────────┤
│ Status bar                                            │
└───────────────────────────────────────────────────────┘
```

Every region except the activity bar is hideable and resizable. The
arrangement is persistable per workspace.

## Requirements (numbered for traceability)

### 1. Activity bar

1.1. **Vertical icon strip**, fixed width, by default on the far left edge of the window.

1.2. Each item in the strip represents an **activity** (an addressable view container the user can summon to the side bar).

1.3. Clicking an item **toggles** its side bar visibility — first click shows, second hides.

1.4. The currently-active activity is rendered with an **accent indicator** (e.g., a colored bar on the leading edge of the item).

1.5. Items are **draggable** to reorder within the strip.

1.6. Items can be **right-clicked** to access per-item commands: Hide, Move to Other Side Bar, etc.

1.7. Each item may carry a **badge** — a small number or dot — driven by host-supplied state (e.g., "5 problems", "1 unread").

1.8. The activity bar itself may be hidden, moved to the right side, or moved to the top/bottom (top/bottom is a v1.1 stretch goal; v1.0 supports left and right).

1.9. **Right-clicking the strip** — an item or its empty area — opens a context menu listing **every available item as a checkbox**, checked when the item is currently shown. Toggling a checkbox shows or hides that item. This is the affordance that restores an item hidden via 1.6, so it stays reachable from the empty strip even when all items are hidden (per 14.3). Per-item right-click shows the checklist below the item-scoped `Hide` command and above the host's own entries.

1.10. The set of hidden items and the user's item order are **persisted** (see §8) so visibility and ordering survive a restart.

### 2. Primary side bar

2.1. The primary side bar **renders the content of the currently-active activity** as a stacked view (single-column).

2.2. The bar has its own **header** with a title (the active activity's name) and a small actions menu (hamburger or ellipsis).

2.3. The bar is **horizontally resizable** with a draggable splitter on its inner edge.

2.4. The bar can be **collapsed** entirely with a keyboard shortcut (`Ctrl+B` / `Cmd+B` by default) — the content is preserved, only visibility changes.

2.5. The bar's width is **persisted** across sessions.

2.6. The bar's position can be **swapped** to the right edge — content unchanged, just rendered on the other side.

### 3. Secondary side bar

3.1. The secondary side bar is **optional** and **off by default**. It appears when toggled.

3.2. It hosts the **same view container abstraction** as the primary side bar — any activity can be moved between them.

3.3. Its position is always **opposite** the primary (right when primary is left, left when primary is right).

3.4. All other behaviors mirror the primary side bar (resizable, collapsible, persisted width).

### 4. Editor area

4.1. The editor area is the **central region** of the workbench. It cannot be hidden (something must always fill the center).

4.2. The editor area contains one or more **editor groups**, each rendering a **tab strip + body**.

4.3. Editor groups can be **split horizontally or vertically** (drag a tab to an edge of an existing group, or use a command).

4.4. Each group has an **active tab** — clicking a tab activates it.

4.5. Splitter handles between groups are **draggable** to resize. Sizes are persisted.

4.6. The user can **drag a tab between groups**: dropping in the tab strip reorders; dropping on the body splits or moves.

4.7. There is always a **focused group** — actions like "Close active tab" target this group. The focused group has a **subtle visual indicator** (border accent or focus glow).

4.8. Keyboard shortcuts navigate between groups: `Ctrl+1` / `Ctrl+2` etc. focus group N (host-bindable).

4.9. Closing the **last tab in a group** collapses the group; the sibling group expands to fill.

4.10. Closing the **last group** leaves an empty editor area showing a host-customizable placeholder ("No editor open").

### 5. Tabs

5.1. Each tab shows: an **optional icon**, a **title** (host-provided), and a **close button** (X) that appears on hover.

5.2. Tabs in a **dirty/modified** state show a small **dot** instead of the X (or alongside it).

5.3. Tabs come in three states:
   - **Regular** — the default. Distinct, opaque.
   - **Preview** — italic title. The next "preview-open" replaces this tab. Becomes Regular on edit or double-click.
   - **Pinned** — sorted leftmost in the strip, smaller, with a pin glyph. Cannot be closed by "Close others"; only an explicit close removes it.

5.4. **Middle-click** on a tab closes it (subject to dirty-close guard if implemented by the host).

5.5. **Right-click** on a tab opens a context menu with: Close, Close Others, Close to the Right, Close All, Pin/Unpin, plus host-extensible items.

5.6. **Hovering** a tab shows a tooltip with the full title or file path (host-controllable).

5.7. Tab strips that overflow show **scroll affordances** (left/right chevrons) and may show a **dropdown** of all open tabs in the group.

5.8. Dragging a tab outside the workbench window detaches it to a **floating window** (v1.1 stretch goal; v1.0 doesn't require it).

### 6. Panel area

6.1. The panel area is **bottom-docked by default**, **optional**, **toggleable** with `Ctrl+J` / `Cmd+J` (host-bindable).

6.2. The panel area is a **single editor-group-like surface** with its own tab strip — but distinct from editor groups (terminal, output, problems live here, not source files).

6.3. The panel area has **maximize** and **close** controls in its top-right corner.

6.4. The panel area's height is **resizable** and **persisted**.

6.5. The panel area may be **moved** to top/right/bottom (v1.1 stretch; v1.0 supports bottom only).

### 7. Status bar

7.1. The status bar is a **thin horizontal strip** along the bottom of the window.

7.2. It contains **cells** — small interactive items aligned left or right.

7.3. Left-aligned cells typically show context (mode, encoding, line:col); right-aligned cells show status (sync, notifications, indexing).

7.4. Each cell may be **clickable**, **hoverable** (tooltip), or **passive** (display only).

7.5. The status bar's contents are entirely host-supplied; the crate provides the chrome.

7.6. The status bar may be **hidden** (rare; mostly for zen mode).

### 8. Layout persistence

8.1. The full workbench state — activity selection, activity-item visibility and order, sidebar positions, panel visibility, editor group tree, sizes, pinned/preview tab states — is **serializable** to/from a single JSON document.

8.2. The schema is **versioned**. Unknown versions log a warning and fall back to defaults rather than panic.

8.3. The host decides where to store the document (per-workspace file, user config, etc.). The crate provides serialize/deserialize, not storage.

8.4. **Layout profiles**: the user may save the current arrangement under a name, restore it later, or apply a named profile to any workspace. (Crate exposes the profile primitives; host decides UI.)

8.5. A **factory reset** command rebuilds the default layout.

### 9. Commands and keybindings

9.1. Every workbench-level action (toggle side bar, focus group N, close tab, split editor, etc.) is exposed as a **command** the host can wire into its own command palette / keybinding system.

9.2. The crate **does not own** a command palette UI — too domain-specific. It provides the action list.

9.3. Default keybindings are documented but **inactive** until the host binds them.

### 10. Theming

10.1. All colors, spacings, and animation durations are sourced from the **active `egui::Style`** by default — the workbench feels native to whatever theme the host configured.

10.2. The host may provide a `WorkbenchTheme` override for the few values that don't map to `egui::Style` (e.g., activity bar accent color, focused-group border).

10.3. The crate ships an `ide_defaults` theme module that approximates the IDE-classic dark/light look for hosts that want a more opinionated default than the ambient egui style.

### 11. Accessibility & input

11.1. All interactive elements expose **AccessKit roles** (via egui's existing accesskit integration) — tab labels, close buttons, splitters, activity items.

11.2. Keyboard navigation reaches every interactive item.

11.3. Splitters announce their resize range to AT.

### 12. Performance

12.1. Idle frames (no user input, no animations) **must not** issue per-frame computation beyond what egui itself does. No per-frame allocations, no per-frame layout recomputation when the tree hasn't changed.

12.2. The workbench must remain **interactive at 60fps** on a moderate-spec laptop with 50+ open tabs distributed across 4+ editor groups.

12.3. Drag-and-drop hit testing should not allocate per pointer position.

### 13. Drag-and-drop UX

13.1. While the user drags a tab, **drop indicators** preview where the drop will land:
   - **Stack** — over the tab strip of a target group (insert at position).
   - **Split N/S/E/W** — over the body of a target group (split that group on the indicated edge).
   - **Empty area** — into the editor area or panel area body to create a new group.

13.2. Drop targets are **highlighted** while the drag is active.

13.3. **Esc** cancels an in-flight drag.

### 14. Empty states

14.1. When the editor area has zero tabs across zero groups, the center shows a **host-customizable placeholder** (logo, "Open a file…" hint, etc.). Default placeholder is minimal text.

14.2. When the panel area is empty, it auto-hides.

14.3. When all side bar activities are hidden, the activity bar still renders but with no selection indicator.

---

## Out of scope for v1.0

- Floating/detachable windows (v1.1 stretch).
- Activity bar position other than left/right (v1.1).
- Panel area position other than bottom (v1.1).
- A built-in command palette UI (forever host concern).
- A built-in keybinding system (forever host concern).
- Built-in notifications / toasts (forever host concern).
- A theme picker UI (host concern).
- Built-in welcome page (host concern).

## Compatibility

- Targets egui **0.32**+; designed to track egui's release cadence.
- Built on **egui_tiles 0.13**+; consumer apps don't need to depend on egui_tiles directly but may interop via the exposed `Tree` for advanced uses.
- Rust 2024 edition, MSRV 1.85.

## Versioning

- Pre-1.0: minor bumps (0.1 → 0.2) may break API. Documented in CHANGELOG.
- Post-1.0: semver. Layout schema bumps independently.

## Success criteria for v0.1.0 release

- Hiker integrates with all visible UX matching or exceeding current setup.
- API stays generic: nothing in the crate references hiker types, hiker domain concepts, or hiker-specific layout choices. Other egui apps could adopt it (possibly with future extension points) without forking.
- `cargo test` green, `cargo clippy` clean with the workspace lint set, `cargo doc` warning-free.
- Demo binary `cargo run -p workbench-demo` launches and lets the user reproduce every requirement above.
