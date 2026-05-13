// status: editor-tab-strip
// status: editor-tab-active-state
// status: editor-tab-dirty-marker
// status: editor-tab-overflow
// status: editor-tab-disambiguation
// status: editor-tab-context-menu
// status: editor-preview-tab
// status: editor-preview-tab-promotion
//
// Browser-style multi-buffer tab strip rendered into the top strip's
// trailing region. One tab per file-mode buffer; click switches; × (or
// middle-click) closes; right-click opens the context menu. Disambig-
// uation: when two open buffers share a basename, both render with a
// folder hint (`note.md (research/)` vs `note.md (inbox/)`).
//
// This module is render-only. Buffer state lives in main.ts; the host
// passes a snapshot of `{ path, isDirty, basename, folder }` per tab on
// every render and wires actions back via callbacks.

import { openContextMenu, type CtxMenuItem } from "../widgets/contextMenu";

export interface TabSnapshot {
  path: string;
  basename: string;
  /// Parent folder relative to vault root (or "" for vault root).
  folder: string;
  dirty: boolean;
  /// status: editor-preview-tab
  /// True for the single preview tab; rendered with italic title.
  preview: boolean;
  // status: tab-kinds
  /// Tab kind discriminator — renders a `tab--kind-<kind>` CSS class
  /// on the tab element and gates buffer-only affordances (folder hints,
  /// reveal-in-tree) on `kind === "buffer"`.
  kind: string;
}

export interface TabStripDeps {
  hostEl: HTMLElement;
  /// Returns the current set of tabs in display order.
  getTabs: () => TabSnapshot[];
  /// Returns the active path, or null when no file tab is active (e.g.
  /// while a snapshot/trash preview is up, or before any file is open).
  getActivePath: () => string | null;
  onActivate: (path: string) => void;
  onClose: (path: string) => void;
  onCloseOthers: (path: string) => void;
  onCloseToRight: (path: string) => void;
  onRevealInTree: (path: string) => void;
  /// status: editor-preview-tab-promotion
  /// Promote a preview tab to sticky (driven by double-click on the tab
  /// or the "Keep open" right-click verb). Idempotent — host no-ops on
  /// already-sticky tabs.
  onPromote: (path: string) => void;
}

export interface TabStripApi {
  /// Re-render the strip from `getTabs()` / `getActivePath()`. Idempotent.
  render(): void;
}

export function mountTabStrip(deps: TabStripDeps): TabStripApi {
  // Last rendered signature — short-circuits redundant DOM rebuilds.
  // Why: render() wipes & rebuilds every tab element via replaceChildren().
  // updateStatus() fires it on every CM6 ViewUpdate (statusUpdater plugin),
  // including the focusChanged update produced when the editor blurs on a
  // tab-strip mousedown. Without this guard the DOM is swapped between
  // mousedown and mouseup on the same node, the browser suppresses the
  // resulting `click`, and the user has to click again.
  let lastSig = "";
  function render(): void {
    const tabs = deps.getTabs();
    const active = deps.getActivePath();
    const sig = JSON.stringify({
      a: active,
      t: tabs.map((t) => [t.path, t.basename, t.folder, t.dirty, t.preview, t.kind]),
    });
    if (sig === lastSig && deps.hostEl.childElementCount === tabs.length) return;
    lastSig = sig;
    // Compute basenames that collide so we can render folder hints
    // (editor-tab-disambiguation).
    const basenameCounts = new Map<string, number>();
    for (const t of tabs) {
      basenameCounts.set(t.basename, (basenameCounts.get(t.basename) ?? 0) + 1);
    }
    deps.hostEl.replaceChildren();
    for (const t of tabs) {
      const el = document.createElement("button");
      el.type = "button";
      el.className =
        "tab" +
        (t.path === active ? " tab--active" : "") +
        (t.preview ? " tab--preview" : "") +
        ` tab--kind-${t.kind}`;
      el.dataset.path = t.path;
      el.title = t.path;
      el.setAttribute("role", "tab");
      el.setAttribute("aria-selected", t.path === active ? "true" : "false");

      const label = document.createElement("span");
      label.className = "tab-label";
      label.textContent = t.basename;
      el.appendChild(label);

      if (t.kind === "buffer" && (basenameCounts.get(t.basename) ?? 0) > 1) {
        const hint = document.createElement("span");
        hint.className = "tab-folder-hint";
        hint.textContent = `(${t.folder ? t.folder + "/" : "/"})`;
        el.appendChild(hint);
      }

      // status: editor-tab-dirty-marker — dot on dirty tabs, swapped
      // for an × on hover (CSS handles the swap; both elements ride
      // along, one is hidden via :hover rules).
      if (t.dirty) {
        const dot = document.createElement("span");
        dot.className = "tab-dirty-dot";
        dot.title = "Unsaved changes";
        el.appendChild(dot);
        const close = document.createElement("button");
        close.type = "button";
        close.className = "tab-close tab-close-shadow";
        close.title = "Close tab";
        close.setAttribute("aria-label", "Close tab");
        close.textContent = "×";
        close.addEventListener("click", (e) => {
          e.preventDefault();
          e.stopPropagation();
          deps.onClose(t.path);
        });
        el.appendChild(close);
      } else {
        const close = document.createElement("button");
        close.type = "button";
        close.className = "tab-close";
        close.title = "Close tab";
        close.setAttribute("aria-label", "Close tab");
        close.textContent = "×";
        close.addEventListener("click", (e) => {
          e.preventDefault();
          e.stopPropagation();
          deps.onClose(t.path);
        });
        el.appendChild(close);
      }

      el.addEventListener("click", (e) => {
        e.preventDefault();
        deps.onActivate(t.path);
      });
      // status: editor-preview-tab-promotion
      // Double-click promotes a preview tab to sticky.
      el.addEventListener("dblclick", (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (t.preview) deps.onPromote(t.path);
      });
      // Middle-click closes (browser convention, editor-tab-keybinds).
      el.addEventListener("auxclick", (e) => {
        if (e.button === 1) {
          e.preventDefault();
          deps.onClose(t.path);
        }
      });
      el.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        e.stopPropagation();
        const items: CtxMenuItem[] = [];
        // status: editor-preview-tab-promotion — "Keep open" on preview tabs.
        if (t.preview) {
          items.push({ label: "Keep open", run: () => deps.onPromote(t.path) });
        }
        items.push(
          { label: "Close", run: () => deps.onClose(t.path) },
          {
            label: "Close others",
            disabled: tabs.length <= 1,
            run: () => deps.onCloseOthers(t.path),
          },
          {
            label: "Close all to the right",
            disabled: tabs[tabs.length - 1]?.path === t.path,
            run: () => deps.onCloseToRight(t.path),
          },
          { label: "Reveal in tree", run: () => deps.onRevealInTree(t.path) },
        );
        openContextMenu(e.clientX, e.clientY, items);
      });

      deps.hostEl.appendChild(el);
    }
    // Auto-scroll the active tab into view (editor-tab-overflow).
    if (active) {
      const activeEl = deps.hostEl.querySelector<HTMLElement>(
        `.tab--active`,
      );
      activeEl?.scrollIntoView({ behavior: "smooth", inline: "nearest", block: "nearest" });
    }
  }

  return { render };
}
