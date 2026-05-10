/// Status-bar index label + per-path tree-row marker rendering.
/// Pairs with `./indexStatusBus` (the data side, owning the listeners
/// for `hiker:index-status` and `hiker:reindex-progress`); this module
/// is the rendering half — it owns the `IndexStatus` snapshot and the
/// `outstandingCount` running tally that the status-bar label paints
/// off, and it owns the per-path DOM marker mutation that progress
/// events cascade into via the bus's `updateIndexStateForPath` hook.
///
/// Step 4b of the main.ts refactor.
///
/// status: status-bar-index-label, status-bar-active-file-index-state,
/// per-file-index-state markers (`tree-row-unsupported-marker` /
/// `tree-row-skipped-marker` / `tree-row-queued-marker`)
import type { Buffer } from "./state";
import type { IndexState } from "../tree";
import type { IndexStatus } from "./indexStatusBus";
import { Classes, IX_STATE_CLASSES } from "../style/classes";
import { Logger } from "../logger";

export interface IndexStatusViewDeps {
  /// Status-bar label element (`#status-index`).
  statusIndexEl: HTMLElement;
  /// Active buffer accessor — drives the per-file mirror branch + the
  /// "skip if read-only preview" check. Returning `null` blanks out
  /// per-file state and the label falls back to the aggregate path.
  getBuffer: () => Buffer | null;
  isReadOnlyBuffer: (b: Buffer | null) => boolean;
  /// True when a vault is open. Without one, no indexer exists so the
  /// label blanks out rather than reporting stale state from a previous
  /// vault. Implemented host-side; `vaultIsOpen` lifecycle is step 5.
  isVaultOpen: () => boolean;
  /// Per-path index-state cache lives in the tree controller; this
  /// module reads / writes / clears via these accessors. The tree
  /// module is the canonical owner — we touch it via these hooks
  /// rather than holding our own cache (would fork the source of
  /// truth and force every reader to know about both).
  getIndexState: (path: string) => IndexState | undefined;
  setIndexState: (path: string, state: IndexState) => void;
  fetchIndexState: (path: string) => Promise<IndexState>;
  /// CSS.escape wrapper. Host-side helper; passed in so the tree-row
  /// marker DOM query stays a no-op in test harnesses without
  /// `window.CSS`.
  cssEscape: (s: string) => string;
}

export interface IndexStatusViewApi {
  /// Repaint the status-bar label off the current snapshot. Call
  /// after every status / outstanding / active-buffer transition that
  /// might shift what the label shows.
  render: () => void;
  /// Update the cached `IndexStatus` snapshot. Bus listeners call this
  /// on every `hiker:index-status` event + every progress event that
  /// flips `last_error`.
  setStatus: (next: IndexStatus) => void;
  /// Update the running `outstandingCount`. Bus listeners call this on
  /// `scan_complete` (additive) and on every terminal progress event
  /// (subtractive).
  setOutstanding: (count: number) => void;
  /// Cache + render a per-path index state. Cascades into the tree's
  /// row marker classes + the status label when the path matches the
  /// active buffer.
  updateIndexStateForPath: (path: string, state: IndexState) => void;
}

export function mountIndexStatusView(
  deps: IndexStatusViewDeps,
): IndexStatusViewApi {
  let indexStatus: IndexStatus = {
    model_ready: false,
    queued: 0,
    total_notes: 0,
    last_error: null,
  };
  let outstandingCount = 0;

  function render(): void {
    // No vault → no indexer; blank the label rather than reporting state
    // from a previous vault (or a half-initialized "Model loading…" before
    // any vault has even been picked).
    if (!deps.isVaultOpen()) {
      deps.statusIndexEl.textContent = "";
      deps.statusIndexEl.title = "";
      return;
    }
    if (indexStatus.last_error) {
      deps.statusIndexEl.textContent = "Index error";
      deps.statusIndexEl.title = indexStatus.last_error;
      return;
    }
    deps.statusIndexEl.title = "";
    if (!indexStatus.model_ready) {
      deps.statusIndexEl.textContent = "Model loading…";
      return;
    }
    // status: status-bar-active-file-index-state
    // Mirror the active buffer's per-file state when it's non-Indexed; fall
    // back to the aggregate label otherwise (or while previewing trash /
    // a snapshot — neither has live index state worth mirroring).
    const buffer = deps.getBuffer();
    if (buffer && !deps.isReadOnlyBuffer(buffer)) {
      const cached = deps.getIndexState(buffer.path);
      if (!cached) {
        const path = buffer.path;
        void deps
          .fetchIndexState(path)
          .catch((err: unknown) =>
            Logger.error("ui::app", "index_state_for failed", { path, err }),
          )
          .finally(() => {
            const cur = deps.getBuffer();
            if (cur && cur.path === path) render();
          });
      }
      const state = deps.getIndexState(buffer.path);
      if (state) {
        switch (state.kind) {
          case "unsupported":
            deps.statusIndexEl.textContent = "Not indexed (unsupported filetype)";
            return;
          case "skipped":
            deps.statusIndexEl.textContent = `Skipped — ${state.reason}`;
            return;
          case "queued":
            deps.statusIndexEl.textContent = "Queued for indexing";
            return;
          case "indexed":
            break;
        }
      }
    }
    if (outstandingCount > 0) {
      deps.statusIndexEl.textContent = `Indexing ${outstandingCount} pending`;
      return;
    }
    deps.statusIndexEl.textContent = `Indexed (${indexStatus.total_notes} notes)`;
  }

  function setStatus(next: IndexStatus): void {
    indexStatus = next;
    render();
  }

  function setOutstanding(count: number): void {
    outstandingCount = count;
    render();
  }

  function updateIndexStateForPath(path: string, state: IndexState): void {
    deps.setIndexState(path, state);
    // Force a re-render of the row(s) by toggling marker classes via DOM.
    // The tree module's lazy fetch path also writes the cache; this branch
    // covers progress events that resolve a state without a render trigger.
    document
      .querySelectorAll(`#tree li[data-path="${deps.cssEscape(path)}"]`)
      .forEach((el) => {
        const li = el as HTMLElement;
        li.classList.remove(...IX_STATE_CLASSES);
        li.removeAttribute("data-ix-reason");
        let marker = li.querySelector<HTMLSpanElement>(":scope > .ix-marker");
        if (state.kind !== "indexed") {
          if (!marker) {
            marker = document.createElement("span");
            marker.className = "ix-marker";
            li.append(marker);
          }
        } else if (marker) {
          marker.remove();
        }
        switch (state.kind) {
          case "unsupported":
            li.classList.add(Classes.IX_UNSUPPORTED);
            li.removeAttribute("title");
            break;
          case "skipped":
            li.classList.add(Classes.IX_SKIPPED);
            li.dataset.ixReason = state.reason;
            li.title = `Skipped — ${state.reason}`;
            break;
          case "queued":
            li.classList.add(Classes.IX_QUEUED);
            li.removeAttribute("title");
            break;
          case "indexed":
            li.classList.add(Classes.IX_INDEXED);
            li.removeAttribute("title");
            break;
        }
      });
    const buffer = deps.getBuffer();
    if (buffer && !deps.isReadOnlyBuffer(buffer) && buffer.path === path) {
      render();
    }
  }

  return { render, setStatus, setOutstanding, updateIndexStateForPath };
}
