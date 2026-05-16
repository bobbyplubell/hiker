// status: patch-review-mode
// status: patch-review-agent-diff-toggle
// status: patch-review-readonly-while-active
// status: patch-review-per-hunk-accept
// status: patch-review-dirty-buffer-transactional-accept
// status: write-note-pending-banner
//
// Host wiring for patch-review + write-note pending banner. Reads
// `dom()` / `controllers` / `services` singletons directly instead of
// taking a `deps` bag.

import { Ipc } from "../ipc";
import type { Proposal } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";
import { applyEditPure } from "../patchReview";
import { getBuffer } from "../app/state";
import { dom } from "../app/dom";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export interface PatchReviewWiringApi {
  enterPatchReviewMode(rel: string): Promise<void>;
  exitPatchReviewMode(): void;
  refreshAgentDiffBtn(): void;
  refreshWriteNotePendingBanner(): void;
  pendingEditProposalsForPath(path: string): Proposal[];
  pendingWriteProposalsForPath(path: string): Proposal[];
  refreshPendingProposalsCache(): Promise<void>;
  acceptPatchReviewHunk(proposal: Proposal): Promise<void>;
  rejectPatchReviewHunk(proposal: Proposal): Promise<void>;
  openProposalReview(proposal: { id: string; target_path: string }): Promise<void>;
  getPendingProposalsCache(): Proposal[];
  clearWriteNoteTargetExistsCache(): void;
}

export function setupPatchReviewWiring(): PatchReviewWiringApi {
  const agentDiffBtn = dom().editor.agentDiffBtn;
  const writeNotePendingBannerEl = dom().editor.writeNotePendingBannerEl;
  const writeNotePendingBannerLabelEl = dom().editor.writeNotePendingBannerLabelEl;
  const writeNotePendingBannerBtn = dom().editor.writeNotePendingBannerBtn;

  // status: patch-review-mode
  // status: patch-review-agent-diff-toggle
  // status: patch-review-readonly-while-active
  // Enter / exit patch-review mode against the active buffer's file. The
  // buffer mode flips to `"patch-review"`, CM6 goes read-only, and the
  // active proposal snapshot is pushed into the hunk renderer.
  async function enterPatchReviewMode(rel: string): Promise<void> {
    const buf = getBuffer();
    if (!buf || buf.kind !== "buffer") return;
    if (buf.path !== rel) return;
    await refreshPendingProposalsCache();
    const proposals = pendingEditProposalsForPath(rel);
    if (proposals.length === 0) return;
    const editorPane = controllers.editorPane.get();
    const editor = editorPane.host;
    // Force the user-diff toggle off before entering patch-review per
    // `patch-review-toggles-mutually-exclusive`.
    if (editorPane.dirtyBufferDiff.isActive()) {
      editorPane.dirtyBufferDiff.forceOff();
    }
    buf.mode = { kind: "patch-review", targetPath: rel };
    editorPane.patchReview.setProposals(proposals);
    editor.setReadOnly(true);
    refreshAgentDiffBtn();
    services.updateStatus();
  }

  function exitPatchReviewMode(): void {
    const buf = getBuffer();
    if (!buf || buf.mode.kind !== "patch-review") return;
    const editorPane = controllers.editorPane.get();
    const editor = editorPane.host;
    buf.mode = { kind: "file" };
    editorPane.patchReview.setProposals([]);
    editor.setReadOnly(false);
    refreshAgentDiffBtn();
    services.updateStatus();
  }

  // Repaint the agent-diff toolbar button's enable / pressed state. Greys
  // when the active buffer has no pending `edit_note` proposals, pressed
  // when patch-review mode is active.
  function refreshAgentDiffBtn(): void {
    const buf = getBuffer();
    const inReview = buf?.mode.kind === "patch-review";
    let hasPending = false;
    if (buf && buf.kind === "buffer") {
      hasPending = pendingEditProposalsForPath(buf.path).length > 0;
    }
    agentDiffBtn.disabled = !inReview && !hasPending;
    agentDiffBtn.classList.toggle("active", inReview);
    // `bug-patch-review-gutter-not-restored-on-exit`: the gutter's
    // visibility is a derived state of the toggle, not an imperative
    // side-effect of enter/exit. Reapplying on every status pulse keeps
    // the class in sync even when CM6 internal re-renders or focus
    // events would otherwise lose an imperatively-added class.
    const editor = controllers.editorPane.get().host;
    editor.dom.classList.toggle("hiker-patch-review-active", inReview);
    if (inReview) {
      agentDiffBtn.title = "Exit agent-edit review";
    } else if (!hasPending) {
      agentDiffBtn.title = "No pending agent edits";
    } else {
      agentDiffBtn.title = "Review agent edits";
    }
  }

  agentDiffBtn.addEventListener("click", () => {
    if (agentDiffBtn.disabled) return;
    const buf = getBuffer();
    if (!buf) return;
    if (buf.mode.kind === "patch-review") {
      exitPatchReviewMode();
    } else if (buf.mode.kind === "file") {
      void enterPatchReviewMode(buf.path);
    }
  });

  // status: write-note-pending-banner
  // Cache of path → "does this exist on disk" probes so the banner's label
  // can distinguish "Pending rewrite for this note" from "Pending new-note
  // proposal" without re-issuing readFile every paint. Populated lazily on
  // first sighting of a path; invalidated on staging-changed (cheap to
  // re-probe; rare event).
  const writeNoteTargetExistsCache = new Map<string, boolean>();
  let writeNoteBannerLatestProposalId: string | null = null;

  function refreshWriteNotePendingBanner(): void {
    const buf = getBuffer();
    // Visible iff the active buffer is in plain editing mode AND has at
    // least one pending write-shaped proposal targeting its path.
    if (!buf || buf.kind !== "buffer" || buf.mode.kind !== "file") {
      writeNotePendingBannerEl.hidden = true;
      writeNoteBannerLatestProposalId = null;
      return;
    }
    const writeProposals = pendingWriteProposalsForPath(buf.path);
    if (writeProposals.length === 0) {
      writeNotePendingBannerEl.hidden = true;
      writeNoteBannerLatestProposalId = null;
      return;
    }
    // Newest proposal wins (matches `note-open-routes-to-pending-review`).
    const sorted = writeProposals
      .slice()
      .sort((a, b) => b.created_at_ms - a.created_at_ms);
    const latest = sorted[0];
    writeNoteBannerLatestProposalId = latest.id;
    const path = buf.path;
    const cached = writeNoteTargetExistsCache.get(path);
    // Origin suffix mirrors `write-note-review-mode-label` so the user
    // sees the same provenance framing in both surfaces. Unknown surfaces
    // render no suffix rather than leaking an internal token.
    let origin = "";
    if (latest.surface === "chat") origin = " · chat";
    else if (latest.surface === "trails") origin = " · trail";
    else if (latest.surface === "batch-mutation") origin = " · batch";
    const paint = (exists: boolean): void => {
      const base = exists
        ? "Pending rewrite for this note"
        : "Pending new-note proposal";
      writeNotePendingBannerLabelEl.textContent = base + origin;
    };
    if (cached !== undefined) {
      paint(cached);
    } else {
      // Default to the existing-file framing while the probe resolves —
      // it's the common case. Update once readFile returns.
      paint(true);
      void Ipc.readFile({ rel: path })
        .then(() => writeNoteTargetExistsCache.set(path, true))
        .catch(() => writeNoteTargetExistsCache.set(path, false))
        .finally(() => {
          // Guard: buffer may have switched away during the probe.
          const cur = getBuffer();
          if (
            cur
            && cur.kind === "buffer"
            && cur.mode.kind === "file"
            && cur.path === path
          ) {
            const known = writeNoteTargetExistsCache.get(path);
            if (known !== undefined) paint(known);
          }
        });
    }
    writeNotePendingBannerEl.hidden = false;
  }

  writeNotePendingBannerBtn.addEventListener("click", () => {
    const id = writeNoteBannerLatestProposalId;
    if (!id) return;
    const proposal = pendingProposalsCache.find((p) => p.id === id);
    if (!proposal) return;
    void services.openWriteNoteReview(proposal);
  });

  // status: patch-review-mode
  // Local cache of pending staging proposals — pulled at vault open + on
  // every `hiker:staging-changed` event. The cache backs the patch-review
  // hunk decoration set, the agent-diff toggle's grey-when-empty state,
  // and the openFile auto-routing rule.
  let pendingProposalsCache: Proposal[] = [];
  function pendingEditProposalsForPath(path: string): Proposal[] {
    return pendingProposalsCache.filter(
      (p) => p.target_path === path && p.action === "edit_note" && p.edit,
    );
  }
  function pendingWriteProposalsForPath(path: string): Proposal[] {
    return pendingProposalsCache.filter(
      (p) => p.target_path === path && p.action !== "edit_note",
    );
  }
  async function refreshPendingProposalsCache(): Promise<void> {
    try {
      pendingProposalsCache = await Ipc.stagingList();
    } catch {
      pendingProposalsCache = [];
    }
  }

  // status: patch-review-per-hunk-accept
  // status: patch-review-dirty-buffer-transactional-accept
  // Per-hunk accept handler. Routes:
  // 1. If the active buffer is the same path AND dirty, pre-check that the
  //    edit can apply to the in-memory text — refuse with a toast when the
  //    user's edits clobber the anchor.
  // 2. Call `staging.accept(id)`. Rust re-anchors against current disk and
  //    writes via `write_file_checked`. Anchor / drift failures surface as
  //    errors; we surface them as a toast.
  // 3. On success, dispatch the new buffer text (computed by applying the
  //    same edit to the in-memory buffer) + refresh loadedText / token via
  //    a fresh `open_for_edit` so subsequent commits ride a valid token.
  async function acceptPatchReviewHunk(proposal: Proposal): Promise<void> {
    const edit = proposal.edit;
    if (!edit) return;
    const buf = getBuffer();
    const editorPane = controllers.editorPane.get();
    const editor = editorPane.host;
    const isActiveTarget =
      buf !== null
      && buf.path === proposal.target_path
      && (buf.mode.kind === "file" || buf.mode.kind === "patch-review");
    let bufferAppliedText: string | null = null;
    if (isActiveTarget && buf && buf.mode.kind === "file") {
      const currentBuffer = editor.getActiveText();
      bufferAppliedText = applyEditPure(currentBuffer, edit);
      if (bufferAppliedText === null) {
        showToast(
          "Your edits conflict with this proposal — save or revert first to accept.",
        );
        return;
      }
    }
    try {
      await Ipc.stagingAccept({ proposalId: proposal.id });
    } catch (err) {
      showToast("Accept failed: " + services.formatError(err));
      return;
    }
    // Re-sync local cache before re-rendering decorations.
    await refreshPendingProposalsCache();
    if (isActiveTarget && buf) {
      try {
        const fresh = await Ipc.openForEdit({ rel: proposal.target_path });
        // For patch-review buffers (read-only by mode), just reset
        // loadedText + token. For file buffers with user edits, dispatch
        // `bufferAppliedText` (the edit applied to the user's text) so the
        // user's edits + agent's edit both land.
        const dispatchText =
          buf.mode.kind === "file" && bufferAppliedText !== null
            ? bufferAppliedText
            : fresh.contents;
        editor.dispatch({
          changes: { from: 0, to: editor.getDocLength(), insert: dispatchText },
        });
        buf.loadedText = fresh.contents;
        buf.token = fresh.token;
      } catch (err) {
        Logger.error("ui::app", "patch-review accept reload failed", { err });
      }
    }
    // Re-paint decorations against the new doc/proposals snapshot.
    editorPane.patchReview.setProposals(
      pendingEditProposalsForPath(proposal.target_path),
    );
    services.updateStatus();
    if (buf?.mode.kind === "patch-review") {
      // If the user accepted the last applyable hunk, automatically exit
      // patch-review mode back to plain editing.
      const remaining = pendingEditProposalsForPath(proposal.target_path);
      if (remaining.length === 0) exitPatchReviewMode();
    }
  }

  async function rejectPatchReviewHunk(proposal: Proposal): Promise<void> {
    try {
      await Ipc.stagingReject({ proposalId: proposal.id });
    } catch (err) {
      showToast("Reject failed: " + services.formatError(err));
      return;
    }
    await refreshPendingProposalsCache();
    controllers.editorPane.get().patchReview.setProposals(
      pendingEditProposalsForPath(proposal.target_path),
    );
    services.updateStatus();
    if (getBuffer()?.mode.kind === "patch-review") {
      const remaining = pendingEditProposalsForPath(proposal.target_path);
      if (remaining.length === 0) exitPatchReviewMode();
    }
  }

  // Whole-file staging proposals (`write_note` / `set_frontmatter` /
  // `apply_tag`) open in the spec-conformant `write-note-review` mode
  // (`openWriteNoteReview`). The older `staging` buffer mode was
  // removed; this dispatcher routes the three legacy entry points
  // (status-bar version dropdown, tree click, vault-home activity click)
  // to the right surface based on the proposal's action.
  async function openProposalReview(proposal: { id: string; target_path: string }): Promise<void> {
    const full = pendingProposalsCache.find((p) => p.id === proposal.id);
    if (!full) {
      // Proposal vanished (accepted/rejected concurrently). Surface the
      // live file instead.
      void services.openFile(proposal.target_path, { preview: true });
      return;
    }
    // Route through the `openFile` wrapper so the
    // `note-open-routes-to-pending-review` auto-routing rule consults
    // `pendingEditProposalsForPath` / `pendingWriteProposalsForPath` and
    // lands the user in patch-review (for `edit_note`) or write-note
    // review (for whole-file proposals). Keeps every staging-row entry
    // point on one path so the contract holds regardless of which
    // surface triggered the open. Fixes
    // `bug-activity-pending-row-skips-patch-review-mode`.
    await services.openFile(proposal.target_path, { preview: true });
  }

  return {
    enterPatchReviewMode,
    exitPatchReviewMode,
    refreshAgentDiffBtn,
    refreshWriteNotePendingBanner,
    pendingEditProposalsForPath,
    pendingWriteProposalsForPath,
    refreshPendingProposalsCache,
    acceptPatchReviewHunk,
    rejectPatchReviewHunk,
    openProposalReview,
    getPendingProposalsCache: () => pendingProposalsCache,
    clearWriteNoteTargetExistsCache: () => writeNoteTargetExistsCache.clear(),
  };
}
