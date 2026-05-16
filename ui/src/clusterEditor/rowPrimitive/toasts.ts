// status: cluster-editor-summarize-verb
//
// Toast wording for the subset-scope Summarize verbs. Mirrors what the
// stale-action button on the pane toolbar reports — the user reads the
// outcome in one shape regardless of which surface triggered the call.

import type { SummarizeSweepOutcome } from "./api";

export function summarizeOutcomeToast(
  outcome: SummarizeSweepOutcome,
  requested: number,
): string {
  const enq = outcome.enqueued.length;
  const fresh = outcome.skipped_fresh.length;
  const userEdited = outcome.skipped_user_edited.length;
  if (enq === 0) {
    if (requested > 0 && fresh === requested) {
      return `All ${requested} already fresh — nothing to do`;
    }
    if (enq === 0 && fresh === 0 && userEdited > 0) {
      return `${userEdited} skipped (user-edited) — nothing to do`;
    }
    return "Nothing to summarize";
  }
  const noun = enq === 1 ? "cluster summary" : "cluster summaries";
  if (fresh > 0) {
    return `Enqueued ${enq} ${noun} — ${fresh} already fresh, skipped`;
  }
  return `Enqueued ${enq} ${noun} — watch the queue for progress`;
}
