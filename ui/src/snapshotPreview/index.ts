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

import { Ipc } from "../ipc";
import { hideFrontmatter } from "../editor/hideFrontmatter";
import { confirm3 } from "../widgets/confirm";
import type { EditorHost } from "../app/editor";

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

export type SnapshotBufferMode = {
  kind: "snapshot";
  row: ChangeRow;
  changeId: number;
  diffActive: boolean;
};

interface BufferLike {
  path: string;
  loadedText: string;
  /// Snapshot previews are read-only; the token slot is always `null`
  /// (no commits ride this code path — restore goes through a separate
  /// Tauri command).
  token: unknown | null;
  mode: { kind: string } & Record<string, unknown>;
  kind?: string;
}

export interface SnapshotPreviewDeps {
  editor: EditorHost;
  /// Returns the current open buffer (typed loose to avoid coupling this
  /// module to the host's full Buffer interface — only `mode` discriminator
  /// and a few common fields are read).
  getBuffer: () => BufferLike | null;
  setBuffer: (b: BufferLike | null) => void;
  getHideFrontmatterEnabled: () => boolean;
  renderModeControls: () => void;
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
    if (buffer && deps.editor.isDirty()) {
      const choice = await confirm3(
        `${buffer.path} has unsaved changes.`,
        "Save & switch",
        "Discard & switch",
        "Cancel",
      );
      if (choice === "cancel") return;
      if (choice === "a") {
        const ok = await deps.editor.save();
        if (!ok) return;
      }
    }
    let contents: string | null = null;
    try {
      contents = await Ipc.changeContent({ changeId: row.id });
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
    deps.editor.dispatch({
      changes: { from: 0, to: deps.editor.getDocLength(), insert: contents },
      effects: [
        deps.editor.language.reconfigure(deps.editor.languageExtensionForPath(row.path)),
        deps.editor.livePreviewCompartment.reconfigure(
          deps.editor.livePreviewExtensionForPath(row.path),
        ),
      ],
    });
    if (deps.isVaultHomeVisible()) deps.setVaultHomeVisible(false);
    deps.setBuffer({
      path: row.path,
      loadedText: deps.editor.getActiveText(),
      token: null,
      kind: "buffer",
      mode: { kind: "snapshot", row, changeId: row.id, diffActive: false },
    });
    deps.editor.setReadOnly(true);
    deps.editor.updateStatus();
    deps.editor.refreshChunkBoundaries();
  }

  function close(): void {
    const buffer = deps.getBuffer();
    if (buffer?.mode.kind !== "snapshot") return;
    // Drop any diff decorations the toggle may have applied so the next
    // buffer doesn't inherit them.
    deps.editor.resetDiffDecorations();
    deps.setBuffer(null);
    deps.editor.dispatch({
      changes: { from: 0, to: deps.editor.getDocLength(), insert: "" },
    });
    deps.editor.setReadOnly(false);
    deps.onClose();
    deps.editor.updateStatus();
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
        deps.editor.clearDiff(buffer.loadedText);
        deps.editor.dispatch({
          effects: [
            deps.editor.livePreviewCompartment.reconfigure(
              deps.editor.livePreviewExtensionForPath(row.path),
            ),
            deps.editor.hideFrontmatterCompartment.reconfigure(
              deps.getHideFrontmatterEnabled() ? hideFrontmatter() : [],
            ),
          ],
        });
        mode.diffActive = false;
        deps.renderModeControls();
        deps.editor.refreshChunkBoundaries();
        return;
      }
      let currentContent: string;
      try {
        // Pure read for the diff target — no token needed since this
        // is a one-shot read-only comparison, not a commit setup.
        currentContent = await Ipc.readFile({ rel: row.path });
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
      deps.editor.dispatch({
        effects: [
          deps.editor.livePreviewCompartment.reconfigure([]),
          deps.editor.hideFrontmatterCompartment.reconfigure([]),
        ],
      });
      await deps.editor.renderDiff({
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
      deps.editor.refreshChunkBoundaries();
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
