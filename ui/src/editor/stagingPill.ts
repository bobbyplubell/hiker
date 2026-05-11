// status: staging-accept-reject-from-editor
//
// Editor toolbar pill: "Proposed change — [Accept] [Reject]". Per
// `docs/settings.md`'s "Surface 4: Editor toolbar", when the open
// buffer has a pending staging proposal, a small pill appears in the
// editor toolbar between the `#mode-controls` slot and the right-side
// button cluster (same placement and visual weight as the existing
// "Add to trail" pill).
//
// Visibility — shown iff:
//   - the active buffer is a file-mode buffer (not trash / snapshot /
//     read-only previews)
//   - a staging proposal exists for the active buffer's path (queried
//     via `Ipc.stagingList({ path })`)
//
// Hidden otherwise.
//
// Accept → fetches proposed content, calls `Ipc.stagingAccept`, then
// compares accepted content with current editor text; if different,
// calls `onAcceptAfter(acceptedContent, targetPath)` so the host can
// replace the editor content.
//
// Reject → calls `Ipc.stagingReject`, refreshes pill state.

import { Ipc, type Proposal } from "../ipc";
import { Logger } from "../logger";
import { bufferStore, type Buffer } from "../app/state";
import { showToast } from "../widgets/toast";

export interface StagingPillApi {
  /// Force a re-render after the active-path / proposal set changes.
  refresh(): void;
}

export function mountStagingPill(deps: {
  pillEl: HTMLDivElement;
  /// Called after a successful accept when the accepted content
  /// differs from what's currently in the editor, so the host can
  /// replace the editor content with the version that was just
  /// written to disk.
  onAcceptAfter?: (acceptedContent: string, targetPath: string) => void;
  /// Called after a successful accept or reject so the host can
  /// close the preview and return to the activity detail page.
  onAfterAction?: () => void;
}): StagingPillApi {
  const { pillEl, onAcceptAfter, onAfterAction } = deps;

  const acceptBtn = pillEl.querySelector<HTMLButtonElement>(".pill-accept");
  const rejectBtn = pillEl.querySelector<HTMLButtonElement>(".pill-reject");

  if (!acceptBtn || !rejectBtn) {
    throw new Error("mountStagingPill: pill element missing .pill-accept or .pill-reject");
  }

  let currentProposal: Proposal | null = null;

  async function fetchProposalForPath(path: string): Promise<Proposal | null> {
    try {
      const proposals = await Ipc.stagingList({ path });
      return proposals.length > 0 ? proposals[0] : null;
    } catch (err) {
      Logger.error("ui::editor", "staging list failed", {
        error: String(err),
        path,
      });
      return null;
    }
  }

  function hidden() {
    pillEl.style.display = "none";
    currentProposal = null;
  }

  async function refresh(): Promise<void> {
    const buf: Buffer | null = bufferStore.get().buffer;
    if (!buf || buf.mode.kind !== "staging") {
      hidden();
      return;
    }
    const proposal = await fetchProposalForPath(buf.path);
    currentProposal = proposal;
    if (!proposal) {
      hidden();
      return;
    }
    pillEl.style.display = "";
  }

  acceptBtn.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (!currentProposal) return;

    const proposal = currentProposal;
    acceptBtn.disabled = true;
    rejectBtn.disabled = true;

    try {
      // Fetch the proposed content before accepting — after accept
      // the .md sidecar is deleted.
      let proposedContent = "";
      try {
        proposedContent = await Ipc.stagingContent({
          proposalId: proposal.id,
        });
      } catch {
        // Content-less proposals (e.g. waypoint-adds) have no .md file.
        // That's fine — accept still succeeds; we just skip the editor
        // content comparison.
      }

      await Ipc.stagingAccept({ proposalId: proposal.id });

      // If we have proposed content and it differs from what's currently
      // in the editor, replace the editor content (it's been saved to
      // disk so the editor should reflect the new state).
      if (proposedContent && onAcceptAfter) {
        onAcceptAfter(proposedContent, proposal.target_path);
      }

      showToast("Change accepted");
      onAfterAction?.();
      await refresh();
    } catch (err) {
      Logger.error("ui::editor", "staging accept failed", {
        error: String(err),
        proposal_id: proposal.id,
      });
      showToast("Failed to accept change");
    } finally {
      acceptBtn.disabled = false;
      rejectBtn.disabled = false;
    }
  });

  rejectBtn.addEventListener("click", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    if (!currentProposal) return;

    const proposal = currentProposal;
    acceptBtn.disabled = true;
    rejectBtn.disabled = true;

    try {
      await Ipc.stagingReject({ proposalId: proposal.id });
      showToast("Change rejected");
      onAfterAction?.();
      await refresh();
    } catch (err) {
      Logger.error("ui::editor", "staging reject failed", {
        error: String(err),
        proposal_id: proposal.id,
      });
      showToast("Failed to reject change");
    } finally {
      acceptBtn.disabled = false;
      rejectBtn.disabled = false;
    }
  });

  // Re-query on every active-path change.
  bufferStore.subscribe(() => {
    void refresh();
  });

  // First paint.
  void refresh();

  return { refresh };
}
