// status: sidebar-mode-switcher
// status: side-panel-resize
//
// Sidebar mode (Files / Cluster trees / Trails) + side-panel resize wiring.
// Reads `dom()` + `services` singletons directly.

import { emit as emitBusEvent } from "../events/bus";
import { dom } from "../app/dom";
import { services } from "../app/services";

export type SidebarMode = "files" | "clusters" | "trails";

export interface SidebarModeApi {
  syncToggleButtons(): void;
  setSidebarMode(mode: SidebarMode, persist: boolean): void;
  getSidebarMode(): SidebarMode;
  setSidebarWidthVar(px: number): void;
  setDiscoveryWidthVar(px: number): void;
}

export function setupSidebarMode(): SidebarModeApi {
  const appEl = dom().editor.appEl;
  const toggleSidebarBtn = dom().discovery.toggleSidebarBtn;
  const toggleRelatedBtn = dom().discovery.toggleRelatedBtn;

  function syncToggleButtons(): void {
    toggleSidebarBtn.classList.toggle("active", !appEl.classList.contains("sidebar-collapsed"));
    toggleRelatedBtn.classList.toggle("active", !appEl.classList.contains("related-collapsed"));
  }

  toggleSidebarBtn.addEventListener("click", () => {
    appEl.classList.toggle("sidebar-collapsed");
    syncToggleButtons();
    // Emit on the bus so other modules can react without the toggle
    // handler having to know who cares. The discovery / tree / chat
    // panels don't take direct calls from here anymore — they subscribe
    // if they care about sidebar visibility transitions.
    const open = !appEl.classList.contains("sidebar-collapsed");
    emitBusEvent("sidebar-toggled", { open });
    if (!services.vaultIsOpen()) return;
    void services.persistSetting("vault", "vault.sidebar_open", open);
  });
  toggleRelatedBtn.addEventListener("click", () => {
    appEl.classList.toggle("related-collapsed");
    syncToggleButtons();
    const open = !appEl.classList.contains("related-collapsed");
    // The related panel rides the same bus event as the sidebar — both
    // are "a side column collapsed/uncollapsed" transitions. If a
    // subscriber needs to distinguish, the payload can grow a `which`
    // field; for now no consumer needs the distinction.
    emitBusEvent("sidebar-toggled", { open });
    if (!services.vaultIsOpen()) return;
    void services.persistSetting("vault", "vault.related_open", open);
  });

  // Default: tree open, related collapsed (per editor.md). Overridden once
  // `get_settings` lands in `openVault` for vaults that have explicit values.
  appEl.classList.add("related-collapsed");
  syncToggleButtons();

  // status: sidebar-mode-switcher
  // Files / Cluster trees / Trails switcher at the top of the sidebar.
  // Files-mode body is the existing tree + toolbar; Clusters-mode swaps in a
  // placeholder until the cluster editor surface (`cluster-editor-sidebar-mode`)
  // lands; Trails is greyed in v1 until trails do. The trash bin
  // (`#trash-bin`) is shared across modes per spec. Mode persists per-vault
  // under `vault.sidebar_mode`.
  const sidebarEl = document.getElementById("sidebar");
  const sidebarModeFilesBtn = document.getElementById("sidebar-mode-files");
  const sidebarModeClustersBtn = document.getElementById("sidebar-mode-clusters");
  const sidebarModeTrailsBtn = document.getElementById("sidebar-mode-trails");
  let sidebarMode: SidebarMode = "files";
  function paintSidebarMode(): void {
    if (!sidebarEl) return;
    sidebarEl.classList.toggle("mode-files", sidebarMode === "files");
    sidebarEl.classList.toggle("mode-clusters", sidebarMode === "clusters");
    sidebarEl.classList.toggle("mode-trails", sidebarMode === "trails");
    for (const [btn, mode] of [
      [sidebarModeFilesBtn, "files"],
      [sidebarModeClustersBtn, "clusters"],
      [sidebarModeTrailsBtn, "trails"],
    ] as const) {
      if (!btn) continue;
      const active = sidebarMode === mode;
      btn.classList.toggle("active", active);
      btn.setAttribute("aria-selected", active ? "true" : "false");
    }
  }
  function setSidebarMode(mode: SidebarMode, persist: boolean): void {
    if (mode === sidebarMode) return;
    sidebarMode = mode;
    paintSidebarMode();
    if (persist && services.vaultIsOpen()) {
      void services.persistSetting("vault", "vault.sidebar_mode", mode);
    }
  }
  paintSidebarMode();
  sidebarModeFilesBtn?.addEventListener("click", () =>
    setSidebarMode("files", true),
  );
  sidebarModeClustersBtn?.addEventListener("click", () =>
    setSidebarMode("clusters", true),
  );
  sidebarModeTrailsBtn?.addEventListener("click", () => {
    setSidebarMode("trails", true);
  });

  // status: side-panel-resize
  // Drag handles on the inner edge of the sidebar / discovery columns.
  // Per the spec: 4px handles, `col-resize` cursor, min/max clamped, persisted
  // per-vault on pointerup. The CSS grid column-template reads
  // `--sidebar-width` / `--discovery-width` from `#app`'s inline style; the
  // drag updates those vars live so CM6 reflows for free, and the toggle
  // (`sidebar-collapsed` / `related-collapsed`) still hides the column
  // wholesale via `grid-template-columns: 0 …` overrides — collapse is
  // not "drag width to 0."
  const SIDEBAR_MIN_PX = 160;
  const DISCOVERY_MIN_PX = 220;
  function maxSidePanelPx(): number {
    return Math.max(SIDEBAR_MIN_PX, Math.floor(window.innerWidth * 0.5));
  }
  function setSidebarWidthVar(px: number): void {
    const clamped = Math.round(
      Math.min(Math.max(px, SIDEBAR_MIN_PX), maxSidePanelPx()),
    );
    appEl.style.setProperty("--sidebar-width", `${clamped}px`);
  }
  function setDiscoveryWidthVar(px: number): void {
    const clamped = Math.round(
      Math.min(Math.max(px, DISCOVERY_MIN_PX), maxSidePanelPx()),
    );
    appEl.style.setProperty("--discovery-width", `${clamped}px`);
  }
  function readWidthVar(name: "--sidebar-width" | "--discovery-width"): number {
    const raw = getComputedStyle(appEl).getPropertyValue(name).trim();
    const n = parseFloat(raw);
    return Number.isFinite(n) ? n : name === "--sidebar-width" ? 280 : 320;
  }

  function wireSidePanelResize(
    handle: HTMLElement,
    edge: "sidebar" | "discovery",
  ): void {
    let dragStartX = 0;
    let dragStartW = 0;
    handle.addEventListener("pointerdown", (ev) => {
      if (ev.button !== 0) return;
      const collapsedCls =
        edge === "sidebar" ? "sidebar-collapsed" : "related-collapsed";
      if (appEl.classList.contains(collapsedCls)) return;
      ev.preventDefault();
      handle.classList.add("dragging");
      handle.setPointerCapture(ev.pointerId);
      dragStartX = ev.clientX;
      dragStartW = readWidthVar(
        edge === "sidebar" ? "--sidebar-width" : "--discovery-width",
      );
    });
    handle.addEventListener("pointermove", (ev) => {
      if (!handle.classList.contains("dragging")) return;
      const dx = ev.clientX - dragStartX;
      // Sidebar grows when dragging right; discovery grows when dragging left.
      const next = edge === "sidebar" ? dragStartW + dx : dragStartW - dx;
      if (edge === "sidebar") setSidebarWidthVar(next);
      else setDiscoveryWidthVar(next);
    });
    function endDrag(ev: PointerEvent): void {
      if (!handle.classList.contains("dragging")) return;
      handle.classList.remove("dragging");
      try {
        handle.releasePointerCapture(ev.pointerId);
      } catch {}
      if (!services.vaultIsOpen()) return;
      const px = readWidthVar(
        edge === "sidebar" ? "--sidebar-width" : "--discovery-width",
      );
      const key =
        edge === "sidebar" ? "vault.sidebar_width" : "vault.discovery_width";
      void services.persistSetting("vault", key, Math.round(px));
    }
    handle.addEventListener("pointerup", endDrag);
    handle.addEventListener("pointercancel", endDrag);
  }

  const sidebarResizeHandleEl = document.getElementById("sidebar-resize-handle");
  const discoveryResizeHandleEl = document.getElementById(
    "discovery-resize-handle",
  );
  if (sidebarResizeHandleEl) wireSidePanelResize(sidebarResizeHandleEl, "sidebar");
  if (discoveryResizeHandleEl)
    wireSidePanelResize(discoveryResizeHandleEl, "discovery");

  return {
    syncToggleButtons,
    setSidebarMode,
    getSidebarMode: () => sidebarMode,
    setSidebarWidthVar,
    setDiscoveryWidthVar,
  };
}
