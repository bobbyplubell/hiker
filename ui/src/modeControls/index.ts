// status: editor-toolbar-mode-controls
// status: mode-controls-diff-toggle
// status: editor-view-options-menu
//
// Editor toolbar's center cluster: per-mode controls (snapshot / trash /
// future modes) registered by their owning module, rendered by this host
// based on the active buffer's `mode.kind`. Also hosts the View ▾ menu
// which lives on the same toolbar — wired through the same `iconButton`
// primitive that mode-control rows use.
//
// Per the spec: "one consumer per mode registers its renderer, host swaps
// based on buffer-mode union." The registry pattern keeps each mode's
// controls colocated with its lifecycle module rather than scattered
// across `main.ts`.

import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";

export interface IconBtnSpec {
  title: string;
  svg: string;
  onClick: () => void;
  pressed?: boolean;
}

export function iconButton(spec: IconBtnSpec): HTMLButtonElement {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "toolbar-btn";
  if (spec.pressed) btn.classList.add("active");
  btn.title = spec.title;
  btn.setAttribute("aria-label", spec.title);
  btn.innerHTML = spec.svg;
  btn.addEventListener("click", spec.onClick);
  return btn;
}

// status: snapshot-preview-diff-toggle (icon)
export const ICON_DIFF =
  '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><line x1="3" y1="8" x2="13" y2="8"/><polyline points="5,5 2,8 5,11"/><polyline points="11,5 14,8 11,11"/></svg>';
export const ICON_RESTORE =
  '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><path d="M3 8a5 5 0 1 0 1.5-3.5"/><polyline points="2,2 2,5 5,5"/></svg>';
export const ICON_CLOSE =
  '<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/></svg>';

/// A renderer paints whatever it wants into the supplied host element.
/// Called every time `render()` fires — should be idempotent.
export type ModeRenderer = (host: HTMLElement) => void;

export interface ModeControlsDeps {
  hostEl: HTMLElement;
  viewMenuBtn: HTMLButtonElement;
  buildViewMenuItems: () => CtxMenuItem[];
  /// Returns the active buffer's mode kind. The host swaps on this in
  /// `render()`. `null` means no buffer (or a kind without a registered
  /// renderer).
  getActiveMode: () => string | null;
}

export interface ModeControlsApi {
  /// Register a renderer for a given buffer-mode kind. Subsequent calls
  /// with the same `kind` replace the prior renderer.
  register(kind: string, renderer: ModeRenderer): void;
  /// Re-render the toolbar slot from the active mode. Call after any
  /// state change that affects the controls (mode entry/exit, diff
  /// toggle, etc.).
  render(): void;
}

export function mountModeControls(deps: ModeControlsDeps): ModeControlsApi {
  const renderers = new Map<string, ModeRenderer>();

  function register(kind: string, renderer: ModeRenderer): void {
    renderers.set(kind, renderer);
  }

  function render(): void {
    deps.hostEl.replaceChildren();
    const kind = deps.getActiveMode();
    if (kind === null) return;
    const renderer = renderers.get(kind);
    if (renderer) renderer(deps.hostEl);
  }

  deps.viewMenuBtn.addEventListener("click", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const rect = deps.viewMenuBtn.getBoundingClientRect();
    openContextMenu(rect.left, rect.bottom + 2, deps.buildViewMenuItems());
  });

  return { register, render };
}
