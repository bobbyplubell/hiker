/// Agent-driven buffer/tree updates. Owns two listeners:
///
///   - `hiker:note-mutation-applied` — the queue's terminal "applied"
///     event for a NoteMutation task. Routes through
///     `applyMutationToBuffer`, which updates the active buffer's
///     editor doc (or background tab's saved CM6 state) in place,
///     stamps `pendingChangesMetadata`, and leaves the tab dirty so
///     the next save tags the row with `metadata.mutation`.
///
///   - `hiker:changes-appended` — every changes broadcast (user save,
///     rollback, agent write). Agent writes (per `mcp.md`) suppress
///     the watcher around their fs writes for the same correctness
///     reasons move/delete do, so `hiker:file-changed` never fires
///     for them. Ride this broadcast instead: any row whose author
///     is `agent` applies the same tree-refresh + active-buffer
///     reload shape the watcher handler would have. Non-agent rows
///     keep flowing through the watcher path so we don't double-
///     refresh.
///
/// status: mcp-ui-refresh-on-agent-write,
/// note-mutation-applies-as-buffer-edit
///
/// Step 4c of the main.ts refactor.
import { listen } from "@tauri-apps/api/event";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import type { ChangeRow } from "../snapshotPreview";
import type { Buffer, OpenBufferEntry } from "./state";
import type { EditorHost } from "./editor";

interface NoteMutationAppliedEvent {
  task_id: string;
  source_path: string;
  mutation_kind: string;
  content: string;
  source_hash_at_submit: string;
}

export interface AgentChangesDeps {
  editor: EditorHost;
  /// Tab registry — agent mutations write through to the active doc
  /// or to the background tab's saved CM6 state.
  openBuffers: Map<string, OpenBufferEntry>;
  /// Active buffer accessors. The closures fire on event delivery,
  /// not at mount time; reading via getters keeps this module
  /// independent of the host's `let buffer` mirror lifecycle.
  getBuffer: () => Buffer | null;
  getActivePath: () => string | null;
  getPreviewTabPath: () => string | null;
  isReadOnlyBuffer: (b: Buffer | null) => boolean;
  isDirty: () => boolean;
  /// Buffer-state setter — atomic update of any subset of buffer-state
  /// fields. Same contract as `tabs.ts`'s `setBufferState`.
  setBufferState: (
    patch: Partial<{
      buffer: Buffer | null;
      activePath: string | null;
      previewTabPath: string | null;
    }>,
  ) => void;
  /// Toggle CM6 read-only after a mutation apply / agent reload. The
  /// terminal queue event also clears RO via `onInFlightChanged`;
  /// clearing here from the apply path is idempotent + defensive.
  setReadOnly: (ro: boolean) => void;
  updateStatus: () => void;
  scheduleChunkBoundariesRefresh: (delayMs: number) => void;
  /// Re-render the tab strip (background-tab edits + closed-tab
  /// removals both flip dirty / open-set state visible there).
  renderTabStrip: () => void;
  /// Trigger a debounced tree refresh — ride the watcher path so
  /// callers don't have to know the tree's refresh cadence.
  scheduleTreeRefreshFromWatcher: () => void;
  /// Active tree sort order; mtime-sorts move rows on every
  /// `modified` event, others don't.
  getTreeSortOrder: () => string;
  /// Vault-home recents/activity refresh — every changes-appended row
  /// updates the home-screen widgets (independent of agent vs user).
  notifyChangesAppended: () => void;
}

export function mountAgentChanges(deps: AgentChangesDeps): void {
  // status: note-mutation-buffer-ro-while-in-flight
  // Apply a `NoteMutation` task's terminal output into the open buffer.
  // - If the path is the active buffer, dispatch the swap into CM6 so
  //   undo history captures it as one step.
  // - If it's a background tab, swap on the saved CM6 state so a later
  //   activation lands on the post-mutation content with undo history
  //   still intact.
  // - If the user closed the tab mid-flight (only via the explicit close
  //   path, since it's pinned + RO), the result is dropped silently
  //   because `openBuffers.get(path)` is empty. Stamps
  //   `pendingChangesMetadata` so the next save tags the `'modified'`
  //   row with `metadata.mutation`. Tab is left dirty (`loadedText`
  //   stays at pre-mutation), surfacing the dirty marker in the strip
  //   + tree.
  function applyMutationToBuffer(
    path: string,
    content: string,
    mutationKind: string,
    _expectedSourceHash: string,
  ): void {
    const entry = deps.openBuffers.get(path);
    if (!entry) return;
    entry.buffer.pendingChangesMetadata = { mutation: mutationKind };
    const buffer = deps.getBuffer();
    const activePath = deps.getActivePath();
    const isActive = activePath === path && buffer?.path === path;
    if (isActive) {
      deps.editor.dispatch({
        changes: { from: 0, to: deps.editor.getDocLength(), insert: content },
      });
      // Terminal queue event also clears RO via `onInFlightChanged`;
      // clearing here is idempotent + defensive.
      deps.setReadOnly(false);
      deps.updateStatus();
    } else if (entry.savedState) {
      // Background tab. Update the saved CM6 state in place via a
      // transaction off the existing state — preserves history so Ctrl-Z
      // on activation reverts the whole replacement as one undo step
      // (same shape as the active path).
      const tr = entry.savedState.update({
        changes: {
          from: 0,
          to: entry.savedState.doc.length,
          insert: content,
        },
      });
      entry.savedState = tr.state;
      // Re-render so the tab strip's dirty dot reflects the change.
      deps.renderTabStrip();
    }
  }

  void listen<NoteMutationAppliedEvent>("hiker:note-mutation-applied", (ev) => {
    const p = ev.payload;
    applyMutationToBuffer(
      p.source_path,
      p.content,
      p.mutation_kind,
      p.source_hash_at_submit,
    );
  });

  async function handleAgentChange(row: ChangeRow): Promise<void> {
    if (row.op === "created" || row.op === "deleted" || row.op === "renamed") {
      deps.scheduleTreeRefreshFromWatcher();
    } else if (
      row.op === "modified"
      && (deps.getTreeSortOrder() === "mtime-newest"
        || deps.getTreeSortOrder() === "mtime-oldest")
    ) {
      deps.scheduleTreeRefreshFromWatcher();
    }

    const buffer = deps.getBuffer();
    if (!buffer || deps.isReadOnlyBuffer(buffer)) return;

    if (row.op === "modified" && row.path === buffer.path) {
      if (deps.isDirty()) {
        showToast(`${row.path} was rewritten by an agent; save to keep yours.`);
        return;
      }
      try {
        // Buffer is clean — silent reload via `open_for_edit` reseeds the
        // doc + rotates the token; no UI-side hash compare needed.
        const fresh = await Ipc.openForEdit({ rel: row.path });
        deps.editor.dispatch({
          changes: {
            from: 0,
            to: deps.editor.getDocLength(),
            insert: fresh.contents,
          },
        });
        const cur = deps.getBuffer();
        if (cur && cur.path === row.path) {
          cur.loadedText = deps.editor.getActiveText();
          cur.token = fresh.token;
          deps.updateStatus();
          deps.scheduleChunkBoundariesRefresh(500);
        }
      } catch (err) {
        Logger.error("ui::app", "agent-change silent reload failed", { err });
      }
      return;
    }

    if (row.op === "deleted" && row.path === buffer.path) {
      if (deps.isDirty()) {
        showToast(`${row.path} was removed by an agent; save to recreate.`);
      } else {
        // status: editor-tab-strip — drop the tab for the removed path.
        deps.openBuffers.delete(row.path);
        const previewTabPath = deps.getPreviewTabPath();
        deps.setBufferState({
          buffer: null,
          activePath: null,
          ...(previewTabPath === row.path ? { previewTabPath: null } : {}),
        });
        deps.editor.dispatch({
          changes: { from: 0, to: deps.editor.getDocLength(), insert: "" },
        });
        deps.updateStatus();
        deps.renderTabStrip();
        showToast(`${row.path} was removed by an agent`);
      }
      return;
    }

    if (row.op === "renamed" && row.rename_from === buffer.path) {
      buffer.path = row.path;
      deps.updateStatus();
    }
  }

  void listen<ChangeRow>("hiker:changes-appended", (event) => {
    deps.notifyChangesAppended();
    const row = event.payload;
    if (row.author_class !== "agent") return;
    void handleAgentChange(row);
  });
}
