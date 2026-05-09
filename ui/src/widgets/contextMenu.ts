// Generic context menu / popover primitive used by the tree, the trash bin
// row menus, the tree-actions toolbar menu, and the View menu. One menu
// open at a time; clicking outside or pressing Escape dismisses.

export interface CtxMenuItem {
  label: string;
  run?: () => void | Promise<void>;
  disabled?: boolean;
  danger?: boolean;
  checked?: boolean;
  tooltip?: string;
}

let openMenuEl: HTMLElement | null = null;

export function closeContextMenu(): void {
  if (openMenuEl) {
    openMenuEl.remove();
    openMenuEl = null;
  }
}

export function openContextMenu(
  x: number,
  y: number,
  items: CtxMenuItem[],
): void {
  closeContextMenu();
  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  menu.setAttribute("role", "menu");
  for (const item of items) {
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
    menu.appendChild(btn);
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
