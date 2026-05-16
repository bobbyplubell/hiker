/// Hoisted UI state for `main.ts`'s globals. Replaces the `let`-bindings
/// that previously lived as module closures (`buffer`, `activePath`,
/// `previewTabPath`, `inFlightMutationPaths`, `livePreviewEnabled`,
/// `chunkBoundariesEnabled`, …) per `bug-ui-state-in-mutable-closures`.
///
/// Stores are *single source of truth* — `main.ts` keeps narrow
/// accessor wrappers (`getBuffer()` etc.) for ergonomic local reads,
/// but every write rides through `setBuffer` / `update*` so cross-module
/// subscribers (next: `bug-chat-couples-to-main-buffer-globals`) see
/// the change without being passed bespoke deps closures.
///
/// This module is intentionally state-only: no DOM, no Tauri, no CM6.
/// Side-effects (status-bar repaint, tab-strip render, persist-setting
/// IPC) stay at the call sites in `main.ts`.
import type { EditorState } from "@codemirror/state";
import type { ChangeRow } from "../snapshotPreview";
import type { BufferToken } from "../ipc";
import { controllers } from "./controllers";
import { createStore, type Store, type Unsubscribe } from "../store/createStore";

// status: tab-kinds
export type TabKind =
  | "buffer"
  | "home"
  | "home-detail"
  | "queue"
  | "settings"
  | "agent"
  | "graph"
  | "properties"
  // status: cluster-editor-pane-mode
  // Tab hosting the expanded cluster editor. Carries two sub-modes keyed
  // off `BufferMode.kind`: `cluster-tree` (graphical tree view) and
  // `cluster-batch-review` (the post-Apply rows surface).
  | "cluster-pane"
  // status: cluster-review-tab-kind
  // Clustering review tab — hosts the configure → run structural pass →
  // review → confirm flow for a new tree, a subtree recluster, or an
  // Evergreen rebuild. Non-buffer, sticky; on Confirm the tab flips in
  // place to `cluster-pane` for the newly-persisted tree.
  | "cluster-review";

/// Discriminated union of buffer modes. `file` is the normal editable
/// buffer; the other two are read-only previews. Bundling per-mode state
/// onto the variant makes invalid combinations (e.g. a trash buffer with
/// a snapshot row) unrepresentable, and lets save / dirty / status code
/// narrow once via `mode.kind`.
export type BufferMode =
  | { kind: "file" }
  | { kind: "trash"; displayPath: string }
  | {
      kind: "snapshot";
      row: ChangeRow;
      changeId: number;
      /// status: snapshot-preview-diff-toggle
      diffActive: boolean;
    }
  // status: patch-review-mode
  // status: patch-review-as-mode-not-pane
  // Patch-review mode renders pending `edit_note` proposal hunks inline
  // over the live on-disk file. CM6 is read-only while active; hunks are
  // accepted or rejected per-hunk via gutter buttons, and the whole mode
  // shares Accept-all / Reject-all / Exit verbs in the mode-controls slot.
  | {
      kind: "patch-review";
      targetPath: string;
    }
  // status: write-note-review-surface
  // Whole-file proposal review (write_note / set_frontmatter / apply_tag).
  // Read-only buffer of the proposed content; accept blocks if the user
  // has a dirty buffer for the same path.
  | {
      kind: "write-note-review";
      proposal_id: string;
      targetPath: string;
      diffActive: boolean;
      /// True when the target path doesn't exist on disk (a create-shaped
      /// `write_note`). Drives the "Review new note" vs "Review rewrite"
      /// label in `#mode-controls`.
      isCreate: boolean;
    }
  // status: cluster-editor-pane-mode
  // Expanded cluster-tree editor view in the editor pane. Hosts the
  // graphical reshape surface + toolbar (Apply / Save-as-triage /
  // Discard). Flips to `cluster-batch-review` mode on Apply.
  | {
      kind: "cluster-tree";
      treeId: string;
    }
  // status: cluster-editor-batch-review-pane-mode
  // Post-Apply review surface keyed by tree id. Rows are loaded by
  // filtering `staging.db` for `surface = "cluster-editor"` and
  // `metadata.tree_id = treeId`. Back-to-tree pops back to
  // `cluster-tree` mode without closing pending rows.
  | {
      kind: "cluster-batch-review";
      treeId: string;
    };

export interface Buffer {
  path: string;
  /// tab-kinds — distinguishes buffer tabs from app-page / agent / graph /
  /// properties tabs so every cross-cutting concern (dirty marker, close
  /// guard, autosave, mode-controls, reveal-in-tree) gates on kind.
  kind: TabKind;
  /// Optional human-readable label for the tab strip. Non-buffer tabs
  /// (cluster pane, agent, etc.) set this so the strip shows the tree
  /// name / session title instead of the internal `__hiker:*` key.
  displayLabel?: string;
  loadedText: string;
  /// Opaque buffer-identity token issued by `core::ops::open_for_edit`
  /// and rotated by every successful `commit_buffer` /
  /// `resolve_drift`. The UI holds it but never inspects it — drift
  /// detection and the hash-as-cursor concept stay inside core.
  ///
  /// `null` for read-only previews (trash / snapshot) where commits
  /// are not a concept.
  token: BufferToken | null;
  mode: BufferMode;
  /// status: note-mutation-stash-changes-tag
  pendingChangesMetadata: Record<string, unknown> | null;
  /// status: editor-preview-tab
  preview: boolean;
}

/// Active buffer + its identity / preview-slot bookkeeping. Single source
/// of truth for "which file is in front of the user." `buffer` and
/// `activePath` are kept in sync by every code path that swaps the
/// editor's content; `previewTabPath` tracks the at-most-one preview
/// slot.
export interface BufferState {
  buffer: Buffer | null;
  activePath: string | null;
  previewTabPath: string | null;
}

export const bufferStore: Store<BufferState> = createStore<BufferState>({
  buffer: null,
  activePath: null,
  previewTabPath: null,
});

/// Open file-mode buffers, keyed by vault-relative path. The active one
/// is mirrored into `bufferStore.buffer` so existing single-buffer call
/// sites keep reading through the accessor. Per-buffer `EditorState` is
/// captured on tab switch so undo history / selection / scroll persist.
///
/// Snapshot / trash buffers are *transient previews* on top of the
/// active tab — they don't get their own tab entry. Non-file entries
/// (home, queue, settings, agent, graph, properties) are stored here
/// alongside file-mode buffers; the kind determines rendering.
export interface OpenBufferEntry {
  buffer: Buffer;
  /// CM6 state captured at last tab-deactivate. `null` until the user
  /// switches away from this tab for the first time.
  savedState: EditorState | null;
  /// Order of last activation; drives "switch to most recent" on close.
  lastActivatedAt: number;
}

/// Tab registry. The `Map` itself is the store value (mutated in place
/// by `set`/`delete` calls in main.ts); `tabStore.set(map)` on every
/// mutation re-fires the listeners with the same Map reference.
/// `activationCounter` is a monotonic tick for last-activated ordering.
/// Non-file entries (home, queue, settings, etc.) are stored here
/// alongside file-mode buffers — the kind on the buffer entry
/// distinguishes rendering.
export interface TabState {
  openBuffers: Map<string, OpenBufferEntry>;
  activationCounter: number;
}

export const tabStore: Store<TabState> = createStore<TabState>({
  openBuffers: new Map(),
  activationCounter: 0,
});

/// View-menu / View-toggle settings. Mirrors the per-vault Settings the
/// user persists; the store is the in-memory canonical copy that
/// `applySettingsToUi` seeds and the View menu's items mutate.
export interface ViewSettings {
  livePreviewEnabled: boolean;
  chunkBoundariesEnabled: boolean;
  hideFrontmatterEnabled: boolean;
  whitespaceEnabled: boolean;
  lineNumbersVisible: boolean;
  wordWrapEnabled: boolean;
  renderTxtAsMarkdown: boolean;
  /// status: view-intraline-diff-toggle
  intralineDiffEnabled: boolean;
}

export const viewSettingsStore: Store<ViewSettings> = createStore<ViewSettings>({
  livePreviewEnabled: true,
  chunkBoundariesEnabled: false,
  hideFrontmatterEnabled: false,
  whitespaceEnabled: false,
  lineNumbersVisible: true,
  wordWrapEnabled: true,
  renderTxtAsMarkdown: true,
  intralineDiffEnabled: false,
});

/// Active trail (vault-relative path of the trail-doc), mirrored from
/// `vault.active_trail` in the merged settings snapshot. `null` when no
/// trail is active. Slice U1 lands the read side only — Trails-mode
/// dropdown writes (slice U2) will go through `Ipc.trailSetActive` and
/// re-seed this store via the post-write `applySettingsToUi` pass.
///
/// status: active-trail-state
export interface ActiveTrailState {
  rel: string | null;
}

export const activeTrailStore: Store<ActiveTrailState> =
  createStore<ActiveTrailState>({ rel: null });

/// Read the active trail's vault-relative trail-doc path. Returns `null`
/// when no trail is active. Pure read — writes go through
/// `Ipc.trailSetActive` (which also stamps `hiker.last_activated_at` on
/// the trail-doc).
export function getActiveTrailRel(): string | null {
  return activeTrailStore.get().rel;
}

/// Source paths with an active or leased `NoteMutation` task. Populated
/// by `mountMutationsMenu`'s `onInFlightChanged` hook driven off
/// `hiker:queue-event`. The active buffer is set RO while its path is
/// in this set; cleared from terminal events.
///
/// status: note-mutation-buffer-ro-while-in-flight
export interface InFlightMutationsState {
  paths: Set<string>;
}

export const inFlightMutationsStore: Store<InFlightMutationsState> =
  createStore<InFlightMutationsState>({
    paths: new Set<string>(),
  });

/// Cross-module read surface for the active buffer. Consumers
/// (chat panel per `bug-chat-couples-to-main-buffer-globals`, future
/// panels) call `getActiveBufferSnapshot()` / `subscribeActiveBufferSnapshot()`
/// directly instead of being passed bespoke `getActiveNote` /
/// `getActiveSelection` deps closures over main.ts internals.
export interface ActiveBufferSnapshot {
  relPath: string;
  bufferText: string;
  selection: { text: string; lineRange: string } | null;
}

/// Snapshot the active editable buffer + its current selection. Returns
/// `null` for read-only previews (trash / snapshot / non-buffer-kind
/// tabs) and when no buffer is open or the editor pane hasn't mounted
/// yet. Selection-text is read from the live CM6 view, since text /
/// selection live there — but callers don't need to know that.
export function getActiveBufferSnapshot(): ActiveBufferSnapshot | null {
  const buffer = bufferStore.get().buffer;
  if (!buffer) return null;
  if (buffer.kind !== "buffer") return null;
  if (buffer.mode.kind !== "file") return null;
  const ed = controllers.editorPane.tryGet()?.host;
  if (!ed) return null;
  const state = ed.getState();
  const text = state.doc.toString();
  const sel = state.selection.main;
  let selection: { text: string; lineRange: string } | null = null;
  if (sel.from !== sel.to) {
    const selText = state.sliceDoc(sel.from, sel.to);
    if (selText.trim()) {
      const startLine = state.doc.lineAt(sel.from).number;
      const endLine = state.doc.lineAt(sel.to).number;
      selection = {
        text: selText,
        lineRange:
          startLine === endLine ? `L${startLine}` : `L${startLine}-L${endLine}`,
      };
    }
  }
  return { relPath: buffer.path, bufferText: text, selection };
}

/// Subscribe to active-buffer changes (open / close / swap). Fires
/// after every `bufferStore.set` with a fresh snapshot.
export function subscribeActiveBufferSnapshot(
  cb: (snapshot: ActiveBufferSnapshot | null) => void,
): Unsubscribe {
  return bufferStore.subscribe(() => cb(getActiveBufferSnapshot()));
}

// ---------- Module-level state helpers ----------
//
// Thin accessor wrappers around the stores above. Replace `Deps` bag
// fields like `getBuffer()` / `setBufferState()` that previously had
// to be threaded into every extracted module.

export function getBuffer(): Buffer | null {
  return bufferStore.get().buffer;
}

export function getActivePath(): string | null {
  return bufferStore.get().activePath;
}

export function getPreviewTabPath(): string | null {
  return bufferStore.get().previewTabPath;
}

export function setBufferState(patch: Partial<BufferState>): void {
  bufferStore.update((s) => ({ ...s, ...patch }));
}

export function bumpActivationCounter(): number {
  const next = tabStore.get().activationCounter + 1;
  tabStore.update((s) => ({ ...s, activationCounter: next }));
  return next;
}

export function getOpenBuffers(): Map<string, OpenBufferEntry> {
  return tabStore.get().openBuffers;
}

export function getInFlightMutationPaths(): Set<string> {
  return inFlightMutationsStore.get().paths;
}

export function addInFlightMutationPath(path: string): void {
  const paths = inFlightMutationsStore.get().paths;
  paths.add(path);
  inFlightMutationsStore.set({ paths });
}

export function removeInFlightMutationPath(path: string): void {
  const paths = inFlightMutationsStore.get().paths;
  paths.delete(path);
  inFlightMutationsStore.set({ paths });
}
