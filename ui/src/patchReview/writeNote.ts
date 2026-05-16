// status: write-note-review-surface
// status: write-note-review-mode-label
// status: write-note-review-blocks-on-dirty
// status: patch-review-mode-controls
//
// Host wiring for write-note review + the patch-review mode-controls
// registration. Reads singletons (`controllers` / `services`) directly
// instead of taking a `deps` bag.

import { Ipc } from "../ipc";
import type { Proposal } from "../ipc";
import { showToast } from "../widgets/toast";
import { iconButton } from "../modeControls";
import { Icons } from "../icons";
import { hideFrontmatter } from "../editor/hideFrontmatter";
import { getBuffer, setBufferState, getOpenBuffers } from "../app/state";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export interface WriteNoteReviewApi {
  openWriteNoteReview(proposal: Proposal): Promise<void>;
  exitWriteNoteReview(): void;
  toggleWriteNoteReviewDiff(): Promise<void>;
}

export function setupWriteNoteReview(): WriteNoteReviewApi {
  // status: write-note-review-surface
  // Open a write-note-review session for a proposal: replaces the active
  // buffer with a read-only view of the proposed content, with a diff
  // toggle against current disk and Accept/Reject in the mode-controls
  // slot. Accept blocks if any open buffer for the same target is dirty
  // (per `write-note-review-blocks-on-dirty`).
  async function openWriteNoteReview(proposal: Proposal): Promise<void> {
    // Per `patch-review.md` (Pane integration): "Write-note review entry
    // never blocks either. The user can be dirty *while reviewing*; accept
    // is what blocks (per `write-note-review-blocks-on-dirty`)."
    let contents: string;
    try {
      contents = await Ipc.stagingContent({ proposalId: proposal.id });
    } catch (err) {
      alert(`Failed to load proposal: ${services.formatError(err)}`);
      return;
    }
    // Determine whether the target file exists on disk.
    let isCreate = true;
    try {
      await Ipc.readFile({ rel: proposal.target_path });
      isCreate = false;
    } catch {
      isCreate = true;
    }
    const editor = controllers.editorPane.get().host;
    const openBuffers = getOpenBuffers();
    // Persist the outgoing file-mode buffer's CM6 state before we clobber
    // the live editor. Otherwise any unsaved edits sitting in the editor
    // (not yet saved into `openBuffers[path].savedState`) vanish for the
    // duration of the review and never come back on exit/reject.
    const buffer = getBuffer();
    if (buffer && buffer.mode.kind === "file") {
      const out = openBuffers.get(buffer.path);
      if (out) out.savedState = editor.getState();
    }
    setBufferState({ buffer: null, activePath: null, previewTabPath: null });
    editor.dispatch({
      changes: { from: 0, to: editor.getDocLength(), insert: contents },
      effects: [
        editor.language.reconfigure(editor.languageExtensionForPath(proposal.target_path)),
        editor.livePreviewCompartment.reconfigure(
          editor.livePreviewExtensionForPath(proposal.target_path),
        ),
      ],
    });
    const vaultHome = controllers.vaultHome.get();
    if (vaultHome.isVisible()) vaultHome.setVisible(false);
    setBufferState({
      buffer: {
        path: proposal.target_path,
        loadedText: editor.getActiveText(),
        token: null,
        kind: "buffer",
        mode: {
          kind: "write-note-review",
          proposal_id: proposal.id,
          targetPath: proposal.target_path,
          diffActive: false,
          isCreate,
        },
        pendingChangesMetadata: null,
        preview: false,
      },
      activePath: proposal.target_path,
      previewTabPath: null,
    });
    editor.setReadOnly(true);
    services.updateStatus();
    services.checkpointNav();
  }

  function exitWriteNoteReview(): void {
    const buffer = getBuffer();
    if (buffer?.mode.kind !== "write-note-review") return;
    const targetPath = buffer.mode.targetPath;
    const editor = controllers.editorPane.get().host;
    const openBuffers = getOpenBuffers();
    editor.resetDiffDecorations();
    editor.setReadOnly(false);
    // If the user was looking at the target file (possibly with unsaved
    // edits) before entering review, restore that tab — `activateTab`
    // re-applies the saved CM6 state we stashed on entry.
    const priorEntry = openBuffers.get(targetPath);
    if (priorEntry && priorEntry.buffer.mode.kind === "file") {
      controllers.tabs.get().activateTab(targetPath);
      return;
    }
    setBufferState({ buffer: null, activePath: null, previewTabPath: null });
    editor.dispatch({
      changes: { from: 0, to: editor.getDocLength(), insert: "" },
    });
    services.updateStatus();
    services.checkpointNav();
  }

  async function toggleWriteNoteReviewDiff(): Promise<void> {
    const buf = getBuffer();
    if (!buf || buf.mode.kind !== "write-note-review") return;
    const mode = buf.mode;
    const editorPane = controllers.editorPane.get();
    const editor = editorPane.host;
    if (mode.diffActive) {
      editor.clearDiff(buf.loadedText);
      editor.dispatch({
        effects: [
          editor.livePreviewCompartment.reconfigure(
            editor.livePreviewExtensionForPath(mode.targetPath),
          ),
          editor.hideFrontmatterCompartment.reconfigure(
            services.getHideFrontmatterEnabled() ? hideFrontmatter() : [],
          ),
        ],
      });
      mode.diffActive = false;
      editorPane.modeControls.render();
      return;
    }
    let currentContent = "";
    try {
      currentContent = await Ipc.readFile({ rel: mode.targetPath });
    } catch {
      currentContent = ""; // new-note case
    }
    if (buf.mode.kind !== "write-note-review") return;
    editor.dispatch({
      effects: [
        editor.livePreviewCompartment.reconfigure([]),
        editor.hideFrontmatterCompartment.reconfigure([]),
      ],
    });
    await editor.renderDiff({
      before: { label: `${mode.targetPath} · current`, content: currentContent },
      after: { label: `${mode.targetPath} · proposed`, content: buf.loadedText },
    });
    mode.diffActive = true;
    editorPane.modeControls.render();
  }

  // status: write-note-review-surface
  // status: write-note-review-mode-label
  // status: write-note-review-blocks-on-dirty
  controllers.editorPane.get().modeControls.register("write-note-review", (host) => {
    const buffer = getBuffer();
    if (!buffer || buffer.mode.kind !== "write-note-review") return;
    const mode = buffer.mode;
    const proposal = controllers.patchReview.get().getPendingProposalsCache().find((p) => p.id === mode.proposal_id);
    const labelEl = document.createElement("span");
    labelEl.style.cssText = "margin-right:8px;font-size:12px;color:var(--fg-muted);";
    const base = mode.isCreate ? "Review new note" : "Review rewrite";
    // status: write-note-review-mode-label — surface origin suffix
    let origin = "";
    const surface = proposal?.surface ?? "";
    if (surface === "chat") origin = " · chat";
    else if (surface === "trails") origin = " · trail";
    else if (surface === "batch-mutation") origin = " · batch";
    const conflicted = proposal?.state === "conflicted";
    const conflictSuffix = conflicted
      ? ` · conflicted (${proposal?.conflict_reason ?? "unknown"})`
      : "";
    labelEl.textContent = base + origin + conflictSuffix;
    host.appendChild(labelEl);
    host.appendChild(
      iconButton({
        title: mode.diffActive ? "Hide diff" : "Show diff vs current",
        pressed: mode.diffActive,
        svg: Icons.diff(),
        onClick: () => toggleWriteNoteReviewDiff(),
      }),
    );
    const acceptBtn = iconButton({
      title: conflicted
        ? `Cannot accept: ${proposal?.conflict_reason ?? "conflicted"}`
        : "Accept",
      svg: Icons.check(),
      onClick: async () => {
        if (acceptBtn.disabled) return;
        await acceptHandler();
      },
    });
    acceptBtn.classList.add("mode-controls-accept");
    if (conflicted) acceptBtn.disabled = true;
    async function acceptHandler(): Promise<void> {
      // write-note-review-blocks-on-dirty: refuse if any open buffer for
      // the same target path is dirty — active or background.
      const targetPath = mode.targetPath;
      const editor = controllers.editorPane.get().host;
      const openBuffers = getOpenBuffers();
      for (const [, entry] of openBuffers) {
        if (
          entry.buffer.path !== targetPath
          || entry.buffer.mode.kind !== "file"
        ) {
          continue;
        }
        const isActive = entry.buffer === buffer;
        const dirty = isActive
          ? editor.isDirty()
          : entry.savedState
            ? entry.buffer.loadedText !== entry.savedState.doc.toString()
            : false;
        if (dirty) {
          alert("Your buffer has unsaved changes. Save or revert before accepting this rewrite.");
          return;
        }
      }
      try {
        await Ipc.stagingAccept({ proposalId: mode.proposal_id });
        // No "Accepted" toast: per `patch-review.md`, the buffer reload
        // is the user-visible confirmation; a transient toast would be
        // redundant chrome. (`bug-write-note-review-redundant-exit-x`)
        await services.refreshPendingProposalsCache();
        exitWriteNoteReview();
        void services.openFile(targetPath, { preview: true });
      } catch (err) {
        alert("Accept failed: " + services.formatError(err));
      }
    }
    host.appendChild(acceptBtn);
    const rejectBtn = iconButton({
      title: "Reject",
      svg: Icons.cross(),
      onClick: async () => {
        if (!confirm("Reject this proposed change?")) return;
        try {
          await Ipc.stagingReject({ proposalId: mode.proposal_id });
          showToast("Rejected");
          await services.refreshPendingProposalsCache();
          exitWriteNoteReview();
        } catch (err) {
          alert("Reject failed: " + services.formatError(err));
        }
      },
    });
    rejectBtn.classList.add("mode-controls-reject");
    host.appendChild(rejectBtn);
    // No separate Exit verb in the slot: the agent-diff toolbar toggle
    // already serves as the exit affordance (per `patch-review.md:52`).
    // A second exit `X` next to the Reject `X` was redundant chrome.
    // (`bug-write-note-review-redundant-exit-x`)
  });

  // status: patch-review-mode-controls
  controllers.editorPane.get().modeControls.register("patch-review", (host) => {
    const buffer = getBuffer();
    if (buffer?.mode.kind !== "patch-review") return;
    const target = buffer.mode.targetPath;
    const patchReview = controllers.patchReview.get();
    const proposals = patchReview.pendingEditProposalsForPath(target);
    const applyable = proposals.filter((p) => p.state !== "conflicted");
    const conflicted = proposals.length - applyable.length;
    const labelEl = document.createElement("span");
    labelEl.style.cssText = "margin-right:8px;font-size:12px;color:var(--fg-muted);";
    const conflictSuffix = conflicted > 0 ? ` (${conflicted} conflicted)` : "";
    labelEl.textContent = `Review agent edits · ${applyable.length} hunks${conflictSuffix}`;
    host.appendChild(labelEl);
    const acceptAll = iconButton({
      title: `Accept all ${applyable.length} applyable hunks`,
      svg: Icons.check(),
      onClick: async () => {
        if (applyable.length === 0) return;
        if (applyable.length > 5 && !confirm(`Accept ${applyable.length} hunks?`)) return;
        for (const p of applyable) {
          await patchReview.acceptPatchReviewHunk(p);
        }
      },
    });
    acceptAll.disabled = applyable.length === 0;
    acceptAll.classList.add("mode-controls-accept");
    host.appendChild(acceptAll);
    const rejectAll = iconButton({
      title: `Reject all ${proposals.length} hunks`,
      svg: Icons.cross(),
      onClick: async () => {
        if (proposals.length === 0) return;
        if (!confirm(`Reject ${proposals.length} hunks? Agent work will be discarded.`)) return;
        for (const p of proposals) {
          await patchReview.rejectPatchReviewHunk(p);
        }
      },
    });
    rejectAll.disabled = proposals.length === 0;
    rejectAll.classList.add("mode-controls-reject");
    host.appendChild(rejectAll);
  });

  return {
    openWriteNoteReview,
    exitWriteNoteReview,
    toggleWriteNoteReviewDiff,
  };
}
