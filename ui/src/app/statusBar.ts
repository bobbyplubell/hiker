/// Status bar paint — window title, `#status-path` (basename + version
/// dropdown), `#status-cursor` (line:col), `#status-words` (word count),
/// Save button enable, dirty-tree-dot fan-out, and the diff button's
/// enable / pressed state.
///
/// The `#status-path` element doubles as the trigger for the per-buffer
/// version dropdown (`status-bar-version-dropdown`). Closed-state label is
/// the basename plus a mode qualifier when a non-current version is in
/// view; clicking opens a popover listing the live editor + every
/// snapshot row + every staging proposal for the active file (populated
/// from the merged `core::activity` feed). Right-click opens
/// "Reveal in file manager" (`status-bar-path-reveal`), which moved off
/// the left-click target when the dropdown landed.
///
/// status: status-bar-path-basename-tooltip, status-bar-path-reveal,
/// status-bar-layout, dirty-tree-dot, editor-tab-dirty-marker,
/// editor-diff-vs-disk-toggle (diff button enable), window-title,
/// status-bar-version-dropdown, status-bar-version-dropdown-selection,
/// status-bar-version-dropdown-uses-unified-feed,
/// status-bar-version-dropdown-live-refresh
import { type UnlistenFn } from "@tauri-apps/api/event";
import { onHikerEvent } from "../events";

import { Ipc, type ActivityItem } from "../ipc";
import type { ChangeRow } from "../snapshotPreview";
import { Logger } from "../logger";
import { bufferStore, inFlightMutationsStore, tabStore, type Buffer } from "./state";
import type { EditorHost } from "./editor";

export interface StatusBarDeps {
  /// DOM refs for the status bar region. Captured via `domRefs.ts`.
  statusPathEl: HTMLElement;
  statusCursorEl: HTMLElement;
  statusWordsEl: HTMLElement;
  /// Save button — paint clears `disabled` when the active buffer is
  /// editable + dirty.
  saveBtn: HTMLButtonElement;
  /// Diff (vs on-disk) button — paint flips `disabled` / `.active` /
  /// tooltip based on the dirty-buffer diff toggle's state.
  diffBtn: HTMLButtonElement;
  /// Tree element — used for the dirty-tree-dot fan-out (only the
  /// active buffer's row gets the `.dirty` class).
  treeEl: HTMLElement;
  editor: EditorHost;
  isReadOnlyBuffer: (b: Buffer | null) => boolean;
  /// Dirty-buffer diff toggle's "is the editor currently showing the
  /// diff" predicate. The status-bar paint reads this for the diff
  /// button's `pressed` class.
  isDirtyBufferDiffActive: () => boolean;
  /// CSS.escape passthrough for the dirty-tree-dot per-row marker.
  cssEscape: (s: string) => string;
  /// Reveal-in-file-manager handler — wired by host to `Ipc.revealInFileManager`.
  onRevealInFileManager: (rel: string) => Promise<void>;
  /// status: status-bar-version-dropdown-selection
  /// Re-open the live editable file at `path`, exiting any snapshot /
  /// staging preview mode. Host wires this to the normal `openFile` path.
  onSelectCurrent: (path: string) => void | Promise<void>;
  /// Open a snapshot read-only in the editor. Same code path as the
  /// activity-detail page's snapshot click.
  onSelectSnapshot: (row: ChangeRow) => void | Promise<void>;
  /// Open a staging proposal in write-note review mode. Same code path
  /// as the activity-detail page's proposal click.
  onSelectStaging: (proposal: { id: string; target_path: string }) => void | Promise<void>;
}

export interface StatusBarApi {
  /// Repaint everything the status-bar owns. Idempotent. Called from
  /// the editor host's `onAfterStatus` pulse and from the bufferStore
  /// subscriber on active-buffer swap.
  repaint: () => void;
}

interface DropdownEntry {
  /// Discriminator + payload for the click handler.
  kind: "current" | "snapshot" | "staging";
  /// Label material for the row.
  primary: string;
  secondary: string;
  /// Marker shown when the row matches what's currently in view.
  selected: boolean;
  /// Callback bound at row build time.
  onClick: () => void;
}

function relativeTime(unixSecs: number): string {
  const now = Math.floor(Date.now() / 1000);
  const d = Math.max(0, now - unixSecs);
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  if (d < 86400 * 7) return `${Math.floor(d / 86400)}d ago`;
  return new Date(unixSecs * 1000).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

export function mountStatusBar(deps: StatusBarDeps): StatusBarApi {
  const {
    statusPathEl,
    statusCursorEl,
    statusWordsEl,
    saveBtn,
    diffBtn,
    treeEl,
    editor,
    isReadOnlyBuffer,
    isDirtyBufferDiffActive,
    cssEscape,
    onRevealInFileManager,
    onSelectCurrent,
    onSelectSnapshot,
    onSelectStaging,
  } = deps;

  let dropdownPopover: HTMLDivElement | null = null;
  let dropdownAbortController: AbortController | null = null;

  function modeQualifierFor(buffer: Buffer | null): string | null {
    if (!buffer) return null;
    switch (buffer.mode.kind) {
      case "trash":
        return "Trash preview";
      case "snapshot": {
        const ts = buffer.mode.row.timestamp_ms;
        const when = relativeTime(Math.floor(ts / 1000));
        const author = buffer.mode.row.author;
        return `Snapshot ${when} · ${author}`;
      }
      case "write-note-review": {
        // Buffer mode carries id + path but not surface; the dropdown's
        // row label carries the richer `Staging · <surface> ·
        // <relative-time>` form.
        return buffer.mode.isCreate ? "Review new note" : "Review rewrite";
      }
      case "file": {
        if (inFlightMutationsStore.get().paths.has(buffer.path)) {
          return "Reformatting…";
        }
        return null;
      }
      default:
        return null;
    }
  }

  function paintTitleAndPath(buffer: Buffer | null, dirty: boolean): void {
    const isTrash = buffer?.mode.kind === "trash";
    const isSnap = buffer?.mode.kind === "snapshot";
    const isStaging = buffer?.mode.kind === "write-note-review";
    const path =
      buffer?.mode.kind === "trash"
        ? buffer.mode.displayPath
        : (buffer?.path ?? "");
    const titleSuffix = isTrash
      ? " (trash)"
      : isSnap
        ? " (snapshot)"
        : isStaging
          ? " (staging)"
          : "";
    document.title =
      (dirty ? "• " : "") + (path ? `Hiker — ${path}${titleSuffix}` : "Hiker");
    // status: status-bar-version-dropdown — closed-state label is the
    // basename plus an optional mode qualifier appended when a non-current
    // version is selected.
    const basename = path ? (path.split("/").pop() ?? path) : "";
    const qualifier = modeQualifierFor(buffer);
    const label = qualifier ? `${basename} — ${qualifier}` : basename;
    statusPathEl.replaceChildren(document.createTextNode(label));
    if (buffer?.mode.kind === "snapshot") {
      const idEl = document.createElement("span");
      idEl.className = "status-snapshot-id";
      idEl.textContent = `#${buffer.mode.changeId}`;
      idEl.title = `Snapshot id ${buffer.mode.changeId}`;
      statusPathEl.appendChild(idEl);
    }
    statusPathEl.title = isTrash ? buffer!.path : path;
    // The version dropdown is buffer-only — non-buffer tab kinds get an
    // empty status bar via the tab-kinds visibility rule.
    const canDropdown = !!buffer && !isTrash;
    statusPathEl.classList.toggle("clickable", canDropdown);
    statusPathEl.style.cursor = canDropdown ? "pointer" : "";
    statusPathEl.setAttribute("aria-haspopup", canDropdown ? "listbox" : "false");
    if (!canDropdown) closeDropdown();
  }

  function paintCursorAndWords(): void {
    const state = editor.getState();
    const sel = state.selection.main;
    const line = state.doc.lineAt(sel.head);
    const col = sel.head - line.from + 1;
    statusCursorEl.textContent = `${line.number}:${col}`;
    const text = editor.getActiveText();
    const words = text.trim() === "" ? 0 : text.trim().split(/\s+/).length;
    statusWordsEl.textContent = `${words} word${words === 1 ? "" : "s"}`;
  }

  function paintDirtyTreeDot(buffer: Buffer | null, dirty: boolean): void {
    // Build the set of paths whose tree row should carry the dirty dot.
    // Includes every open file-mode buffer with unsaved edits — not just
    // the active one — so background dirty tabs stay marked after a
    // tab switch. Pending agent-review proposals carry their own
    // `staging-dirty` class applied at tree render time, so they show
    // a dot regardless of whether they have an open tab.
    const dirtyPaths = new Set<string>();
    const tabs = tabStore.get().openBuffers;
    for (const [path, entry] of tabs) {
      if (entry.buffer.kind !== "buffer" || entry.buffer.mode.kind !== "file") continue;
      const isActive = buffer?.mode.kind === "file" && buffer.path === path;
      const entryDirty = isActive
        ? dirty
        : entry.savedState !== null
          && entry.savedState.doc.toString() !== entry.buffer.loadedText;
      if (entryDirty) dirtyPaths.add(path);
    }
    treeEl.querySelectorAll("li.dirty").forEach((el) => {
      const p = el.getAttribute("data-path");
      if (p === null || !dirtyPaths.has(p)) el.classList.remove("dirty");
    });
    for (const path of dirtyPaths) {
      const li = treeEl.querySelector(`li[data-path="${cssEscape(path)}"]`);
      li?.classList.add("dirty");
    }
  }

  function paintDiffButton(): void {
    const available = editor.diffButtonAvailable();
    const active = isDirtyBufferDiffActive();
    diffBtn.disabled = !available && !active;
    diffBtn.classList.toggle("active", active);
    diffBtn.title = active
      ? "Hide diff"
      : available
        ? "Diff vs on-disk"
        : "Nothing to diff";
  }

  function repaint(): void {
    const buffer = bufferStore.get().buffer;
    const dirty = editor.isDirty();
    paintTitleAndPath(buffer, dirty);
    paintCursorAndWords();
    saveBtn.disabled = !buffer || !dirty || isReadOnlyBuffer(buffer);
    paintDiffButton();
    paintDirtyTreeDot(buffer, dirty);
  }

  // ── Version dropdown ─────────────────────────────────────────────

  function activeBufferPath(): string | null {
    const buf = bufferStore.get().buffer;
    if (!buf) return null;
    // Snapshot / staging buffers carry the *real* file path on `buf.path`
    // (snapshot) or `buf.mode.targetPath` (staging). Trash previews
    // expose an internal `.hiker/trash/` path and are excluded.
    if (buf.mode.kind === "trash") return null;
    if (buf.mode.kind === "write-note-review") return buf.mode.targetPath;
    return buf.path;
  }

  function closeDropdown(): void {
    if (dropdownAbortController) {
      dropdownAbortController.abort();
      dropdownAbortController = null;
    }
    if (dropdownPopover) {
      dropdownPopover.remove();
      dropdownPopover = null;
      statusPathEl.setAttribute("aria-expanded", "false");
    }
  }

  function selectedKey(buffer: Buffer | null): string {
    if (!buffer) return "current";
    switch (buffer.mode.kind) {
      case "snapshot":
        return `snapshot:${buffer.mode.changeId}`;
      case "write-note-review":
        return `staging:${buffer.mode.proposal_id}`;
      default:
        return "current";
    }
  }

  async function openDropdown(): Promise<void> {
    const buffer = bufferStore.get().buffer;
    if (!buffer || buffer.mode.kind === "trash") return;
    if (dropdownPopover) {
      closeDropdown();
      return;
    }
    const path = activeBufferPath();
    if (!path) return;

    // Render a placeholder immediately so the popover anchors and the
    // user gets feedback even if the IPC takes a moment.
    const pop = buildPopoverShell();
    appendDropdownLoading(pop);
    positionPopover(pop);
    dropdownPopover = pop;
    statusPathEl.setAttribute("aria-expanded", "true");
    wireDropdownOutsideClose();

    let items: ActivityItem[];
    try {
      items = await Ipc.activityListForPath(path, {
        source: "merged",
        limit: 100,
      });
    } catch (err) {
      Logger.error("ui::status-bar", "activity_list_for_path failed", { err });
      items = [];
    }
    if (!dropdownPopover) return; // closed mid-flight
    renderDropdownContents(dropdownPopover, items, buffer, path);
  }

  function buildPopoverShell(): HTMLDivElement {
    const pop = document.createElement("div");
    pop.className = "status-version-dropdown";
    pop.setAttribute("role", "listbox");
    return pop;
  }

  function appendDropdownLoading(pop: HTMLElement): void {
    const row = document.createElement("div");
    row.className = "status-version-row status-version-row-loading";
    row.textContent = "Loading versions…";
    pop.appendChild(row);
  }

  function renderDropdownContents(
    pop: HTMLDivElement,
    items: ActivityItem[],
    buffer: Buffer | null,
    path: string,
  ): void {
    pop.replaceChildren();
    const selected = selectedKey(buffer);
    const entries: DropdownEntry[] = [];

    entries.push({
      kind: "current",
      primary: "Current",
      secondary: "live editor",
      selected: selected === "current",
      onClick: () => {
        closeDropdown();
        void onSelectCurrent(path);
      },
    });

    const snapshots = items.filter((i) => i.payload.kind === "change");
    const stagings = items.filter((i) => i.payload.kind === "staging");

    if (snapshots.length > 0) {
      pushSection(pop, "Snapshots");
      for (const it of snapshots) {
        if (it.payload.kind !== "change") continue;
        const { kind: _k, ...row } = it.payload;
        const changeRow = row as ChangeRow;
        const when = relativeTime(Math.floor(changeRow.timestamp_ms / 1000));
        const primary = `Snapshot · ${when}`;
        const secondary = `${changeRow.author} · ${changeRow.op}`;
        const isSelected = selected === `snapshot:${changeRow.id}`;
        entries.push({
          kind: "snapshot",
          primary,
          secondary,
          selected: isSelected,
          onClick: () => {
            closeDropdown();
            void onSelectSnapshot(changeRow);
          },
        });
      }
    }

    if (stagings.length > 0) {
      pushSection(pop, "Staging");
      for (const it of stagings) {
        if (it.payload.kind !== "staging") continue;
        const s = it.payload;
        const when = relativeTime(Math.floor(s.created_at_ms / 1000));
        const primary = `Staging · ${s.surface}`;
        const secondary = when;
        const isSelected = selected === `staging:${s.id}`;
        entries.push({
          kind: "staging",
          primary,
          secondary,
          selected: isSelected,
          onClick: () => {
            closeDropdown();
            void onSelectStaging({ id: s.id, target_path: s.target_path });
          },
        });
      }
    }

    // Always emit the "Current" entry first; sections are pre-pushed
    // before their entries so they appear in render order.
    const headChild = pop.firstChild;
    const currentRow = buildEntryRow(entries[0]);
    pop.insertBefore(currentRow, headChild);

    for (const e of entries.slice(1)) {
      pop.appendChild(buildEntryRow(e));
    }
  }

  function pushSection(pop: HTMLElement, label: string): void {
    const sep = document.createElement("div");
    sep.className = "status-version-section";
    sep.textContent = label;
    pop.appendChild(sep);
  }

  function buildEntryRow(e: DropdownEntry): HTMLElement {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "status-version-row";
    if (e.selected) row.classList.add("selected");
    row.setAttribute("role", "option");
    row.setAttribute("aria-selected", e.selected ? "true" : "false");
    const primary = document.createElement("span");
    primary.className = "status-version-primary";
    primary.textContent = e.primary;
    const secondary = document.createElement("span");
    secondary.className = "status-version-secondary";
    secondary.textContent = e.secondary;
    row.append(primary, secondary);
    row.addEventListener("click", (ev) => {
      ev.stopPropagation();
      e.onClick();
    });
    return row;
  }

  function positionPopover(pop: HTMLDivElement): void {
    pop.style.position = "fixed";
    document.body.appendChild(pop);
    const anchorRect = statusPathEl.getBoundingClientRect();
    const margin = 4;
    // Anchor by `bottom` so the popover grows upward as contents load —
    // anchoring by `top` (computed from the placeholder height) leaves
    // the top fixed and lets the popover spill past the window bottom
    // once the real items arrive.
    const bottom = Math.max(margin, window.innerHeight - anchorRect.top + margin);
    const popWidth = pop.getBoundingClientRect().width;
    const left = Math.min(
      Math.max(margin, anchorRect.left),
      Math.max(margin, window.innerWidth - popWidth - margin),
    );
    pop.style.bottom = `${bottom}px`;
    pop.style.left = `${left}px`;
    pop.style.maxHeight = `${Math.max(120, anchorRect.top - margin * 2)}px`;
  }

  function wireDropdownOutsideClose(): void {
    const ac = new AbortController();
    dropdownAbortController = ac;
    setTimeout(() => {
      document.addEventListener(
        "click",
        (ev) => {
          if (!dropdownPopover) return;
          if (ev.target instanceof Node) {
            if (
              dropdownPopover.contains(ev.target) ||
              statusPathEl.contains(ev.target)
            ) {
              return;
            }
          }
          closeDropdown();
        },
        { signal: ac.signal },
      );
      document.addEventListener(
        "keydown",
        (ev) => {
          if (ev.key === "Escape") closeDropdown();
        },
        { signal: ac.signal },
      );
    }, 0);
  }

  // ── Wire interactions ────────────────────────────────────────────

  // Active-buffer swaps repaint automatically. CM6 doc / selection
  // pulses come through `editor.onAfterStatus` (wired host-side).
  bufferStore.subscribe(() => {
    repaint();
    closeDropdown();
  });

  // status: status-bar-version-dropdown — left-click opens the dropdown.
  statusPathEl.addEventListener("click", () => {
    const buf = bufferStore.get().buffer;
    if (!buf || buf.mode.kind === "trash") return;
    void openDropdown();
  });

  // status: status-bar-path-reveal — right-click on the closed-state
  // label opens "Reveal in file manager" (moved off the left-click
  // target by the version-dropdown rollout).
  statusPathEl.addEventListener("contextmenu", async (ev) => {
    const buf = bufferStore.get().buffer;
    if (!buf || buf.mode.kind === "trash") return;
    if (buf.mode.kind === "snapshot" || buf.mode.kind === "write-note-review") {
      // Preview buffers don't have a meaningful "reveal" target — the
      // user's intent here is to inspect the live file's location.
      // Still allow it; the path on disk is buf.path / mode.targetPath.
    }
    ev.preventDefault();
    try {
      await onRevealInFileManager(buf.path);
    } catch (err) {
      Logger.error("ui::status-bar", "reveal_in_file_manager failed", { err });
    }
  });

  // Mutation in-flight state can change without a CM6 pulse, so
  // subscribe independently so the "Reformatting…" label appears and
  // disappears promptly.
  inFlightMutationsStore.subscribe(() => repaint());

  // status: status-bar-version-dropdown-live-refresh
  // Re-fetch the dropdown when changes or staging mutate for the active
  // buffer's path. Debounced to coalesce bursts.
  let refreshTimer: ReturnType<typeof setTimeout> | null = null;
  function scheduleDropdownRefresh(forPath: string): void {
    if (refreshTimer) clearTimeout(refreshTimer);
    refreshTimer = setTimeout(async () => {
      refreshTimer = null;
      const cur = activeBufferPath();
      if (!cur || cur !== forPath) return;
      if (!dropdownPopover) return; // dropdown closed — nothing to repaint
      try {
        const items = await Ipc.activityListForPath(cur, {
          source: "merged",
          limit: 100,
        });
        if (dropdownPopover) {
          renderDropdownContents(
            dropdownPopover,
            items,
            bufferStore.get().buffer,
            cur,
          );
        }
      } catch (err) {
        Logger.error(
          "ui::status-bar",
          "activity refresh failed",
          { err },
        );
      }
    }, 150);
  }

  const unlisteners: UnlistenFn[] = [];
  void onHikerEvent("hiker:changes-appended", (payload) => {
    const cur = activeBufferPath();
    if (!cur) return;
    if (payload.path === cur) scheduleDropdownRefresh(cur);
  }).then((u) => unlisteners.push(u));
  void onHikerEvent("hiker:staging-changed", () => {
    const cur = activeBufferPath();
    if (cur) scheduleDropdownRefresh(cur);
  }).then((u) => unlisteners.push(u));

  return { repaint };
}
