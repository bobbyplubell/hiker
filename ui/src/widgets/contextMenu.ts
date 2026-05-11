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

export function closeContextMenu(): void {
  if (openMenuEl) {
    openMenuEl.remove();
    openMenuEl = null;
  }
  openMenuTrigger = null;
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
    if (!menu.contains(ev.target as Node)) closeContextMenu();
  };
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeContextMenu();
    }
  };
  setTimeout(() => {
    document.addEventListener("mousedown", onDocDown, true);
    document.addEventListener("keydown", onKey, true);
  });
  const cleanup = new MutationObserver(() => {
    if (!document.body.contains(menu)) {
      document.removeEventListener("mousedown", onDocDown, true);
      document.removeEventListener("keydown", onKey, true);
      cleanup.disconnect();
    }
  });
  cleanup.observe(document.body, { childList: true });
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
