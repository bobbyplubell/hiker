// status: snapshot-preview-mode
//
// Snapshot preview lifecycle: open a `ChangeRow` as a read-only preview in
// the editor, toggle the diff vs current, restore the snapshot back to disk,
// and exit. The buffer-mode union (`{kind: "snapshot", row, diffActive}`)
// owns the per-buffer state so this module only manages the cross-buffer
// in-flight guard for the diff toggle.
//
// Pairs with the vault-home detail view (`vault-home-recent-activity-detail`):
// vault-home invokes `open(row)`, and on `restore()` the host re-routes to
// vault-home's `doRestoreSnapshot` via the `onRestore` dep so activity rows
// and the recently-rolled-back highlight refresh correctly.

import { invoke } from "@tauri-apps/api/core";
import type { EditorView } from "@codemirror/view";
import type { Compartment, Extension } from "@codemirror/state";
import { renderDiff, clearDiff, resetDiffDecorations } from "../diff";
import { hideFrontmatter } from "../editor/hideFrontmatter";
import { confirm3 } from "../widgets/confirm";

export type ChangeOp = "created" | "modified" | "deleted" | "renamed";

/// Mirrors `core::changes::AuthorClass`. Coarse author taxonomy
/// (`design.md`'s authorship trichotomy) — the wire format of `author`
/// is `class[:identifier]`; the UI consumes the typed `author_class`
/// rather than parsing the string. `other` is forward-compat for
/// future classes.
export type AuthorClass = "user" | "agent" | "sync" | "import" | "other";

export interface ChangeRow {
  id: number;
  timestamp_ms: number;
  path: string;
  op: ChangeOp;
  author: string;
  author_class: AuthorClass;
  content_hash: string | null;
  rename_from: string | null;
  metadata: Record<string, unknown>;
  is_current: boolean;
}

interface FileWithHash {
  contents: string;
  hash: string;
}

export type SnapshotBufferMode = {
  kind: "snapshot";
  row: ChangeRow;
  changeId: number;
  diffActive: boolean;
};

interface BufferLike {
  path: string;
  loadedText: string;
  loadedHash: string;
  mode: { kind: string } & Record<string, unknown>;
}

export interface SnapshotPreviewDeps {
  view: EditorView;
  /// Returns the current open buffer (typed loose to avoid coupling this
  /// module to the host's full Buffer interface — only `mode` discriminator
  /// and a few common fields are read).
  getBuffer: () => BufferLike | null;
  setBuffer: (b: BufferLike | null) => void;
  language: Compartment;
  livePreviewCompartment: Compartment;
  hideFrontmatterCompartment: Compartment;
  languageExtensionForPath: (rel: string) => Extension;
  livePreviewExtensionForPath: (rel: string) => Extension;
  getHideFrontmatterEnabled: () => boolean;
  setReadOnly: (ro: boolean, mode?: "trash" | "snapshot" | null) => void;
  updateStatus: () => void;
  refreshChunkBoundaries: () => void;
  renderModeControls: () => void;
  /// Save-on-switch flow: dirty-buffer guard before opening the preview.
  isDirty: () => boolean;
  save: () => Promise<boolean>;
  /// Host reaction when the user closes the preview — typically returns
  /// to the recent-activity detail view.
  onClose: () => void;
  /// Host reaction when the preview's `↻ Restore` is clicked — typically
  /// re-routes to vault-home's `doRestoreSnapshot(row)` so activity rows /
  /// recently-rolled-back highlight stay coherent.
  onRestore: (row: ChangeRow) => Promise<void>;
  /// Open-from-home navigation: when a snapshot opens we leave home view.
  isVaultHomeVisible: () => boolean;
  setVaultHomeVisible: (on: boolean) => void;
  formatError: (err: unknown) => string;
}

export interface SnapshotPreviewApi {
  open(row: ChangeRow): Promise<void>;
  close(): void;
  toggleDiff(): Promise<void>;
  restore(): Promise<void>;
}

export function mountSnapshotPreview(deps: SnapshotPreviewDeps): SnapshotPreviewApi {
  // status: snapshot-preview-diff-toggle
  // In-flight guard prevents a double-click during the `compute_diff` IPC
  // from interleaving two render passes.
  let diffToggleInFlight = false;

  async function open(row: ChangeRow): Promise<void> {
    const buffer = deps.getBuffer();
    if (buffer && deps.isDirty()) {
      const choice = await confirm3(
        `${buffer.path} has unsaved changes.`,
        "Save & switch",
        "Discard & switch",
        "Cancel",
      );
      if (choice === "cancel") return;
      if (choice === "a") {
        const ok = await deps.save();
        if (!ok) return;
      }
    }
    let contents: string | null = null;
    try {
      contents = await invoke<string | null>("change_content", {
        changeId: row.id,
      });
    } catch (err) {
      alert(`snapshot preview failed: ${deps.formatError(err)}`);
      return;
    }
    if (contents === null) {
      alert(
        "This change has no recorded content (delete events carry no body — preview the prior version to see what was deleted).",
      );
      return;
    }
    deps.setBuffer(null);
    deps.view.dispatch({
      changes: { from: 0, to: deps.view.state.doc.length, insert: contents },
      effects: [
        deps.language.reconfigure(deps.languageExtensionForPath(row.path)),
        deps.livePreviewCompartment.reconfigure(
          deps.livePreviewExtensionForPath(row.path),
        ),
      ],
    });
    if (deps.isVaultHomeVisible()) deps.setVaultHomeVisible(false);
    deps.setBuffer({
      path: row.path,
      loadedText: deps.view.state.doc.toString(),
      loadedHash: row.content_hash ?? "",
      mode: { kind: "snapshot", row, changeId: row.id, diffActive: false },
    });
    deps.setReadOnly(true, "snapshot");
    deps.updateStatus();
    deps.refreshChunkBoundaries();
  }

  function close(): void {
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "snapshot") return;
    // Drop any diff decorations the toggle may have applied so the next
    // buffer doesn't inherit them.
    resetDiffDecorations(deps.view);
    deps.setBuffer(null);
    deps.view.dispatch({
      changes: { from: 0, to: deps.view.state.doc.length, insert: "" },
    });
    deps.setReadOnly(false, null);
    deps.onClose();
    deps.updateStatus();
  }

  async function toggleDiff(): Promise<void> {
    if (diffToggleInFlight) return;
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "snapshot") return;
    const mode = buffer.mode as unknown as SnapshotBufferMode;
    const row = mode.row;
    if (row.op === "deleted") return;
    diffToggleInFlight = true;
    try {
      if (mode.diffActive) {
        clearDiff(deps.view, buffer.loadedText);
        deps.view.dispatch({
          effects: [
            deps.livePreviewCompartment.reconfigure(
              deps.livePreviewExtensionForPath(row.path),
            ),
            deps.hideFrontmatterCompartment.reconfigure(
              deps.getHideFrontmatterEnabled() ? hideFrontmatter() : [],
            ),
          ],
        });
        mode.diffActive = false;
        deps.renderModeControls();
        deps.refreshChunkBoundaries();
        return;
      }
      let currentContent: string;
      try {
        const cur = await invoke<FileWithHash>("read_file_with_hash", {
          rel: row.path,
        });
        currentContent = cur.contents;
      } catch (err) {
        alert(`could not load current ${row.path}: ${deps.formatError(err)}`);
        return;
      }
      const after = deps.getBuffer();
      if (
        after?.mode.kind !== "snapshot" ||
        (after.mode as unknown as SnapshotBufferMode).row.id !== row.id
      ) {
        return;
      }
      const when = new Date(row.timestamp_ms).toLocaleString();
      deps.view.dispatch({
        effects: [
          deps.livePreviewCompartment.reconfigure([]),
          deps.hideFrontmatterCompartment.reconfigure([]),
        ],
      });
      await renderDiff(deps.view, {
        before: {
          label: `${row.path} · snapshot ${when}`,
          content: buffer.loadedText,
          meta: { changeId: row.id },
        },
        after: {
          label: `${row.path} · current`,
          content: currentContent,
        },
      });
      (after.mode as unknown as SnapshotBufferMode).diffActive = true;
      deps.renderModeControls();
      deps.refreshChunkBoundaries();
    } finally {
      diffToggleInFlight = false;
    }
  }

  async function restore(): Promise<void> {
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "snapshot") return;
    const row = (buffer.mode as unknown as SnapshotBufferMode).row;
    await deps.onRestore(row);
    close();
  }

  return { open, close, toggleDiff, restore };
}
