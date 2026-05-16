// status: task-queue-home-widget
// status: task-queue-home-widget-respects-llm-disable
// status: vault-bar-queue-button
// status: staging-review-top-bar-badge
//
// Home-page Task queue tile + vault-bar queue button + staging change
// reactions. Reads singletons directly.

import { Ipc } from "../ipc";
import { onHikerEvent } from "../events";
import { getBuffer } from "./state";
import { dom } from "./dom";
import { controllers } from "./controllers";
import { services } from "./services";

export interface TaskQueueTileApi {
  refresh(): Promise<void>;
  stopStagingPolling(): void;
}

export function setupTaskQueueTile(): TaskQueueTileApi {
  const { tasksSection, tasksHeader, tasksSummary } = dom().vaultHome;
  const { queueBtnEl, queueIndicatorEl } = dom().vaultBar;

  let activeCount = 0;
  let succeededCount = 0;
  let failedCount = 0;
  let unreadFailure = false;
  let llmEnabled = true;
  let pendingReviewCount = 0;

  const SUMMARY_DOT = " · ";
  function paintSummary(): void {
    if (!tasksSection || !tasksSummary) return;
    if (!llmEnabled) {
      tasksSection.hidden = true;
      return;
    }
    tasksSection.hidden = false;
    if (activeCount + succeededCount + failedCount === 0) {
      tasksSummary.textContent = "No tasks queued";
      return;
    }
    tasksSummary.textContent = [
      `${activeCount} active`,
      `${succeededCount} succeeded`,
      `${failedCount} failed`,
    ].join(SUMMARY_DOT);
  }

  function paintIndicator(): void {
    if (!queueBtnEl || !queueIndicatorEl) return;
    if (!llmEnabled) {
      queueBtnEl.hidden = true;
      queueIndicatorEl.hidden = true;
      return;
    }
    queueBtnEl.hidden = false;

    const total = activeCount + pendingReviewCount;

    if (total > 0) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.textContent = String(total);
      if (activeCount > 0) {
        queueIndicatorEl.classList.add("queue-indicator-active");
      } else {
        queueIndicatorEl.classList.remove("queue-indicator-active");
      }
      return;
    }

    queueIndicatorEl.classList.remove("queue-indicator-active");
    if (unreadFailure) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.textContent = "";
      queueIndicatorEl.classList.add("queue-indicator-failed");
      return;
    }
    queueIndicatorEl.hidden = true;
    queueIndicatorEl.classList.remove("queue-indicator-failed");
  }

  function repaint(): void {
    paintSummary();
    paintIndicator();
  }

  // status: staging-review-top-bar-badge
  async function refreshStaging(): Promise<void> {
    try {
      pendingReviewCount = await Ipc.stagingCount();
    } catch {
      pendingReviewCount = 0;
    }
    repaint();
  }

  let stagingInterval: ReturnType<typeof setInterval> | undefined;

  function startStagingPolling(): void {
    stopStagingPolling();
    stagingInterval = setInterval(() => { void refreshStaging(); }, 30_000);
  }

  function stopStagingPolling(): void {
    if (stagingInterval) { clearInterval(stagingInterval); stagingInterval = undefined; }
  }

  // Listen for staging changes so the badge updates immediately after
  // an accept/reject/accept_all, without waiting for the next poll tick.
  // Refresh the cached `pendingProposals` BEFORE re-rendering the tree —
  // otherwise the render reads stale proposals and leaves the just-accepted
  // file's row marked `staging-new` / `staging-dirty` (see
  // `bug-accept-new-note-from-activity-page-leaves-stale-dirty-marker`).
  void onHikerEvent("hiker:staging-changed", () => {
    void refreshStaging();
    void (async () => {
      const tree = controllers.tree.get();
      await tree.api.refreshStagingProposals();
      await tree.api.refresh();
    })();
    // status: patch-review-mode
    // Re-sync local pending-proposals cache for the agent-diff toggle
    // grey state and the active patch-review hunk decorations.
    void (async () => {
      await services.refreshPendingProposalsCache();
      const buf = getBuffer();
      if (buf && (buf.mode.kind === "patch-review" || buf.mode.kind === "file")) {
        const proposals = services.pendingEditProposalsForPath(buf.path);
        controllers.editorPane.get().patchReview.setProposals(
          buf.mode.kind === "patch-review" ? (proposals as never) : [],
        );
      }
      services.refreshAgentDiffBtn();
      // status: write-note-pending-banner
      // Staging change may have added or removed a pending write-shape
      // proposal for the active path. Invalidate the existence cache
      // (cheap) so the label re-probes if the target's on-disk presence
      // changed since last paint, then repaint.
      services.clearWriteNoteTargetExistsCache();
      services.refreshWriteNotePendingBanner();
    })();
    // bug-home-recent-activity-missing-pending-agent-review:
    // home recent-activity widget consumes the unified feed and must
    // repaint when staging proposals appear/disappear.
    controllers.vaultHome.get().api.notifyStagingChanged();
  });

  function openQueueDetail(): void {
    if (!llmEnabled) return;
    // status: tab-kinds — open the queue as a app-page tab instead of
    // swapping the vaultHome sub-mode.
    void services.openAppPageTab("queue", {});
    unreadFailure = false;
    paintIndicator();
  }

  if (tasksHeader) {
    tasksHeader.style.cursor = "pointer";
    tasksHeader.addEventListener("click", openQueueDetail);
  }
  if (queueBtnEl) {
    queueBtnEl.addEventListener("click", openQueueDetail);
  }

  void onHikerEvent("hiker:queue-event", (payload) => {
    const k = payload.event;
    if (k === "task_queued") {
      activeCount += 1;
    } else if (k === "task_completed") {
      activeCount = Math.max(0, activeCount - 1);
      succeededCount += 1;
    } else if (k === "task_failed") {
      activeCount = Math.max(0, activeCount - 1);
      failedCount += 1;
      // Only flag the indicator red if the user isn't currently looking
      // at the queue — otherwise the dot would light up under their
      // cursor for no reason.
      if (!controllers.queueDetail.get().isVisible()) unreadFailure = true;
    } else if (k === "task_cancelled") {
      activeCount = Math.max(0, activeCount - 1);
    }
    repaint();
  });

  async function refresh(): Promise<void> {
    try {
      const cfg = await Ipc.getSettings<{ llm: { enabled: boolean } }>();
      llmEnabled = cfg.llm.enabled;
    } catch {
      // No vault open yet — keep the tile + button hidden until
      // refresh() runs again.
      llmEnabled = false;
    }
    try {
      const rows = await Ipc.tasksSnapshot<{ state: string }>();
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      for (const r of rows) {
        if (r.state === "queued" || r.state === "leased") activeCount += 1;
        else if (r.state === "completed") succeededCount += 1;
        else if (r.state === "failed") failedCount += 1;
      }
      // Fresh vault → no unread state to inherit.
      unreadFailure = false;
    } catch {
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      unreadFailure = false;
    }
    repaint();
    void refreshStaging();
    startStagingPolling();
  }
  repaint();
  void refresh();
  return { refresh, stopStagingPolling };
}
