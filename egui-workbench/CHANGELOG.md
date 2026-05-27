# Changelog

## 0.1.0 — Initial release

- Activity bar with reorderable, badged icon items, per-item Hide, and a
  right-click visibility checklist (checkbox per item, reachable from the
  empty strip) that toggles each item's visibility. Hidden set + item
  order persist through `WorkbenchLayout`.
- Primary + secondary side bars, resizable, swappable left/right.
- Editor area with tabbed groups, splits, drag-to-reorder, focused
  group indicator.
- Pinned / Preview / Regular tab states with proper close-others and
  close-to-right semantics (pinned tabs survive bulk closes).
- Dirty-dot indicator that swaps to a close X on hover.
- Tab context menu (Close / Close Others / Close to Right / Close All /
  Pin / Unpin / Keep Open) plus host-extensible items.
- Theme-accented drop indicators with translucent zone overlay during
  in-flight drags.
- Per-group "All tabs" dropdown for overflowing tab strips.
- Bottom panel area with maximize / close controls and auto-hide when
  empty.
- Status bar (host-rendered cells).
- Keyboard navigation hooks: `focus_group`, `focus_next_group`,
  `focus_prev_group`, `next_tab_in_group`, `prev_tab_in_group`,
  `close_active`.
- Layout persistence to a versioned JSON document (`WorkbenchLayout`,
  `migrate`).
- Smoke, persistence, and snapshot tests via `egui_kittest`.
- Demo binary at `examples/workbench-demo/`.

### Known limitations / deferred to v0.2

- Cross-tree drag (editor area ↔ panel area).
- Floating / detachable windows.
- Activity bar position other than left / right.
- Panel area position other than bottom.
- Animated slide for panel area show / hide (instant flip for v0.1).
- Programmatic `split_active_group(dir)` (drag-to-edge already works).
