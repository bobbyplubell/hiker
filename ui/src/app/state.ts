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
import { createStore, type Store, type Unsubscribe } from "../store/createStore";

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
    };

export interface Buffer {
  path: string;
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
/// active tab — they don't get their own tab entry.
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
}

export const viewSettingsStore: Store<ViewSettings> = createStore<ViewSettings>({
  livePreviewEnabled: true,
  chunkBoundariesEnabled: false,
  hideFrontmatterEnabled: false,
  whitespaceEnabled: false,
  lineNumbersVisible: true,
  wordWrapEnabled: true,
  renderTxtAsMarkdown: true,
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

/// Cross-module read surface for the active buffer. Future consumers
/// (chat panel per `bug-chat-couples-to-main-buffer-globals`, future
/// panels) take this interface instead of being passed bespoke
/// `getActiveNote` / `getActiveSelection` deps closures over main.ts
/// internals. The host wires `getActive` to whatever produces a
/// snapshot of the buffer + selection (CM6 view lives in main.ts).
export interface ActiveBufferSnapshot {
  relPath: string;
  bufferText: string;
  selection: { text: string; lineRange: string } | null;
}

export interface BufferApi {
  /// Snapshot the active editable buffer and its current selection.
  /// Returns `null` for read-only previews (trash / snapshot) and when
  /// no buffer is open.
  getActive(): ActiveBufferSnapshot | null;
  /// Subscribe to active-buffer changes (open / close / swap). Fires
  /// after every `bufferStore.set`.
  onChanged(cb: (snapshot: ActiveBufferSnapshot | null) => void): Unsubscribe;
}
