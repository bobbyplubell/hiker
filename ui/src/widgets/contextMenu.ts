// Generic context menu / popover primitive used by the tree, the trash bin
// row menus, the tree-actions menu, the View menu, the search-mode option
// menus, and a couple of others. One menu open at a time; clicking outside
// or pressing Escape dismisses.
//
// status: search-mode-options-menu (slider/number/radio rows)
//
// Most rows are simple buttons (`kind` undefined or "button"). The
// search-mode option menus added three richer kinds — slider, number,
// radio — so a config knob can ride the same popover as a checkable
// toggle without spawning its own one-off popover plumbing.

export interface CtxMenuButton {
  kind?: "button";
  label: string;
  run?: () => void | Promise<void>;
  disabled?: boolean;
  danger?: boolean;
  checked?: boolean;
  tooltip?: string;
}

export interface CtxMenuSlider {
  kind: "slider";
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  /// Format the live value for the inline read-out next to the label.
  format?: (v: number) => string;
  /// Fired on every slider input event with the new value. Caller is
  /// responsible for any debouncing / persistence.
  onChange: (v: number) => void;
  tooltip?: string;
}

export interface CtxMenuNumber {
  kind: "number";
  label: string;
  min: number;
  max: number;
  step?: number;
  value: number;
  /// Fired on commit (blur or Enter) with the parsed, clamped value.
  onCommit: (v: number) => void;
  tooltip?: string;
}

export interface CtxMenuRadio {
  kind: "radio";
  label: string;
  value: string;
  options: Array<{ label: string; value: string }>;
  onChange: (v: string) => void;
  tooltip?: string;
}

export type CtxMenuItem =
  | CtxMenuButton
  | CtxMenuSlider
  | CtxMenuNumber
  | CtxMenuRadio;

let openMenuEl: HTMLElement | null = null;
let openMenuTrigger: HTMLElement | null = null;
// Document-level listeners installed for the currently-open menu (and the
// pending setTimeout id used to defer their attachment past the opening
// click). Tracked at module scope so `closeContextMenu` can deterministically
// cancel the pending timer and detach already-installed listeners. Without
// this, a fast close+reopen (e.g. the radio onChange that swaps menu items
// in place) leaked one pair per cycle: the original cleanup hook was a
// MutationObserver that disconnected as soon as the menu was removed, but
// the listeners were still queued for attachment via setTimeout and ran
// AFTER the observer had detached. The stale listeners then closed any
// later-opened menu on mousedown, breaking click delivery to its items —
// the "stuck in graph mode" bug in the cluster editor view-options menu.
let openAttachTimer: number | null = null;
let openDocMouseDown: ((ev: MouseEvent) => void) | null = null;
let openDocKey: ((ev: KeyboardEvent) => void) | null = null;

function detachDocListeners(): void {
  if (openAttachTimer != null) {
    window.clearTimeout(openAttachTimer);
    openAttachTimer = null;
  }
  if (openDocMouseDown) {
    document.removeEventListener("mousedown", openDocMouseDown, true);
    openDocMouseDown = null;
  }
  if (openDocKey) {
    document.removeEventListener("keydown", openDocKey, true);
    openDocKey = null;
  }
}

export function closeContextMenu(): void {
  detachDocListeners();
  if (openMenuEl) {
    openMenuEl.remove();
    openMenuEl = null;
  }
  openMenuTrigger = null;
}

/// True iff a context menu is currently open. When `trigger` is
/// supplied, narrows the check to "open and anchored at this trigger" —
/// useful when a caller wants to refresh the menu's contents only if
/// the menu it owns is still on screen.
export function isContextMenuOpen(trigger?: HTMLElement): boolean {
  if (!openMenuEl) return false;
  if (trigger && openMenuTrigger !== trigger) return false;
  return true;
}

/// Open a menu anchored below an element. Same `triggerEl` toggle
/// semantics as `openContextMenu`. Consolidates the
/// `getBoundingClientRect()` + `openContextMenu(...)` boilerplate that
/// every button-spawned menu in the codebase (mode switcher, sidebar
/// view menus, mutations, cluster-pane view menu, …) used to repeat.
///
/// `align: "left"` (default) aligns the menu's left edge with the
/// anchor's left edge; `align: "right"` aligns the menu's left edge
/// with the anchor's right edge (useful for right-side toolbar buttons
/// where the menu should drop down-and-to-the-left).
export interface AnchorMenuOpts {
  align?: "left" | "right";
  /// Pixel offset added below the anchor's bottom edge. Defaults to 0.
  offsetY?: number;
}

export function openMenuAtAnchor(
  anchor: HTMLElement,
  items: CtxMenuItem[],
  opts?: AnchorMenuOpts,
): void {
  const rect = anchor.getBoundingClientRect();
  const x = opts?.align === "right" ? rect.right : rect.left;
  const y = rect.bottom + (opts?.offsetY ?? 0);
  openContextMenu(x, y, items, anchor);
}

/// Open a menu at the location of a mouse event. Thin wrapper around
/// `openContextMenu` that takes the (clientX, clientY) pair so the
/// right-click handlers don't repeat the destructuring everywhere.
export function openMenuAtEvent(
  ev: { clientX: number; clientY: number },
  items: CtxMenuItem[],
): void {
  openContextMenu(ev.clientX, ev.clientY, items);
}

export function openContextMenu(
  x: number,
  y: number,
  items: CtxMenuItem[],
  triggerEl?: HTMLElement,
): void {
  // Clicking the same trigger while its menu is open closes instead of
  // re-opening (toggle behavior). Without this guard the outside-click
  // handler fires first (mousedown capture), removes the menu, then
  // the button's own click handler immediately re-opens it.
  if (triggerEl && openMenuEl && openMenuTrigger === triggerEl) {
    closeContextMenu();
    return;
  }
  closeContextMenu();
  openMenuTrigger = triggerEl ?? null;
  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  menu.setAttribute("role", "menu");
  for (const item of items) {
    const kind = (item as { kind?: string }).kind ?? "button";
    if (kind === "button") {
      menu.appendChild(buildButton(item as CtxMenuButton));
    } else if (kind === "slider") {
      menu.appendChild(buildSlider(item as CtxMenuSlider));
    } else if (kind === "number") {
      menu.appendChild(buildNumber(item as CtxMenuNumber));
    } else if (kind === "radio") {
      menu.appendChild(buildRadio(item as CtxMenuRadio));
    }
  }
  document.body.appendChild(menu);
  const rect = menu.getBoundingClientRect();
  const left = Math.min(x, window.innerWidth - rect.width - 4);
  const top = Math.min(y, window.innerHeight - rect.height - 4);
  menu.style.left = `${Math.max(4, left)}px`;
  menu.style.top = `${Math.max(4, top)}px`;
  openMenuEl = menu;

  const onDocDown = (ev: MouseEvent) => {
    if (menu.contains(ev.target as Node)) return;
    // If the mousedown lands on the trigger that opened this menu, leave
    // dismissal to the trigger's own click handler — otherwise the
    // mousedown-capture close fires first and the click handler then
    // re-opens (the toggle guard in openContextMenu can't help because
    // openMenuEl is already null). Skipping here lets the click handler's
    // openContextMenu call hit the same-trigger short-circuit and close.
    if (openMenuTrigger && (openMenuTrigger === ev.target || openMenuTrigger.contains(ev.target as Node))) return;
    closeContextMenu();
  };
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeContextMenu();
    }
  };
  // Defer attachment past the opening click (so the same mousedown that
  // triggered the open doesn't immediately fire the outside-click close).
  // Both the timer id and the resolved listeners are tracked at module
  // scope so `closeContextMenu` can cancel/detach deterministically — see
  // the comment on `openAttachTimer` for why a MutationObserver-based
  // cleanup isn't sufficient.
  openAttachTimer = window.setTimeout(() => {
    openAttachTimer = null;
    openDocMouseDown = onDocDown;
    openDocKey = onKey;
    document.addEventListener("mousedown", onDocDown, true);
    document.addEventListener("keydown", onKey, true);
  });
}

function buildButton(item: CtxMenuButton): HTMLElement {
  const btn = document.createElement("button");
  let cls = "ctx-menu-item";
  if (item.danger) cls += " danger";
  if (item.checked !== undefined) cls += " checkable";
  if (item.checked) cls += " checked";
  btn.className = cls;
  btn.textContent = item.label;
  btn.disabled = item.disabled === true;
  if (item.tooltip) btn.title = item.tooltip;
  btn.addEventListener("click", async () => {
    closeContextMenu();
    if (item.run) await item.run();
  });
  return btn;
}

function buildSlider(item: CtxMenuSlider): HTMLElement {
  const row = document.createElement("div");
  row.className = "ctx-menu-row ctx-menu-slider";
  if (item.tooltip) row.title = item.tooltip;

  const labelLine = document.createElement("div");
  labelLine.className = "ctx-menu-row-label";
  const labelText = document.createElement("span");
  labelText.textContent = item.label;
  const valueText = document.createElement("span");
  valueText.className = "ctx-menu-row-value";
  const formatFn = item.format ?? ((v: number) => v.toString());
  valueText.textContent = formatFn(item.value);
  labelLine.appendChild(labelText);
  labelLine.appendChild(valueText);
  row.appendChild(labelLine);

  const slider = document.createElement("input");
  slider.type = "range";
  slider.min = String(item.min);
  slider.max = String(item.max);
  slider.step = String(item.step);
  slider.value = String(item.value);
  slider.addEventListener("input", () => {
    const v = Number(slider.value);
    valueText.textContent = formatFn(v);
    item.onChange(v);
  });
  row.appendChild(slider);
  return row;
}

function buildNumber(item: CtxMenuNumber): HTMLElement {
  const row = document.createElement("div");
  row.className = "ctx-menu-row ctx-menu-number";
  if (item.tooltip) row.title = item.tooltip;

  const labelEl = document.createElement("label");
  labelEl.className = "ctx-menu-row-label";
  labelEl.textContent = item.label;

  const input = document.createElement("input");
  input.type = "number";
  input.min = String(item.min);
  input.max = String(item.max);
  if (item.step !== undefined) input.step = String(item.step);
  input.value = String(item.value);
  const commit = () => {
    let n = Number(input.value);
    if (!Number.isFinite(n)) n = item.value;
    n = Math.max(item.min, Math.min(item.max, n));
    input.value = String(n);
    item.onCommit(n);
  };
  input.addEventListener("blur", commit);
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") {
      ev.preventDefault();
      commit();
    }
  });
  labelEl.appendChild(input);
  row.appendChild(labelEl);
  return row;
}

function buildRadio(item: CtxMenuRadio): HTMLElement {
  const row = document.createElement("div");
  row.className = "ctx-menu-row ctx-menu-radio";
  if (item.tooltip) row.title = item.tooltip;

  const labelLine = document.createElement("div");
  labelLine.className = "ctx-menu-row-label";
  labelLine.textContent = item.label;
  row.appendChild(labelLine);

  const group = document.createElement("div");
  group.className = "ctx-menu-radio-group";
  for (const opt of item.options) {
    const btn = document.createElement("button");
    btn.className =
      "ctx-menu-radio-btn" + (opt.value === item.value ? " active" : "");
    btn.textContent = opt.label;
    btn.addEventListener("click", () => {
      item.onChange(opt.value);
      // Repaint active state in place so the user sees the flip without
      // dismissing the menu — same shape the checkable buttons use.
      group
        .querySelectorAll<HTMLButtonElement>(".ctx-menu-radio-btn")
        .forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
    });
    group.appendChild(btn);
  }
  row.appendChild(group);
  return row;
}
