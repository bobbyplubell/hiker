// Tool-call card rendering for the chat panel. Extracted from
// `ui/src/chat.ts` so that file stays under the TS file-length cap;
// behavior is unchanged.
//
// status: chat-panel-tool-call-collapsible
// status: staging-accept-reject-from-chat-card

import { Ipc } from "../ipc";

interface ToolCardEls {
  root: HTMLElement;
  headEl: HTMLElement;
  glyphEl: HTMLElement;
  nameEl: HTMLElement;
  argsSummaryEl: HTMLElement;
  resultSummaryEl: HTMLElement;
  expandedEl: HTMLElement;
  toolName: string;
  argsBuf: string;
  finalArgs: string | null;
  result: { ok: boolean; summary: string } | null;
  expanded: boolean;
  /// Touched-note routing payload extracted from the tool result for
  /// note-touching tools (`write_note` / `edit_note` / `set_frontmatter`
  /// / `apply_tag` / `remove_tag`). When set, the head-click opens the
  /// note (or its staged proposal); chevron-click still toggles.
  /// `bug-chat-tool-call-no-link-for-staged-writes`.
  touched: TouchedNoteRouting | null;
}

/// Resolved routing info for a note-touching tool call. `stagingIds`
/// carries any pending proposal ids the result returned (singular
/// `staging_id` for `write_note` / `set_frontmatter` / `apply_tag`,
/// plural `staging_ids` for `edit_note`). When `stagingIds` is non-empty
/// AND any of those ids is still in the live staging set, the head-click
/// routes through `onOpenStagingProposal`; otherwise it falls back to
/// opening the note directly.
interface TouchedNoteRouting {
  targetPath: string;
  stagingIds: string[];
}

export interface ToolCardContext {
  transcriptEl: HTMLElement;
  scrollToBottom: () => void;
  onOpenNoteLink: (rel: string) => void;
  onOpenStagingProposal: (proposal: { id: string; target_path: string }) => void;
  /// Called whenever appending a tool card clears the in-flight
  /// assistant bubble (so the next text delta opens a new bubble).
  onClearAssistantBubble: () => void;
  /// Current set of pending staging proposal ids; read fresh on each
  /// render (caller updates the underlying set in place).
  getPendingStagingIds: () => Set<string>;
}

export interface ToolCardController {
  /// Re-render Accept/Reject action buttons across every card. Called
  /// from the `hiker:staging-changed` listener.
  rerenderAllActionButtons(): void;
  appendToolCallStart(callId: string, toolName: string): void;
  appendToolCallArgsDelta(callId: string, delta: string): void;
  appendToolCallComplete(callId: string, args: string): void;
  appendToolResult(
    callId: string,
    ok: boolean,
    summary: string,
    output?: string,
  ): void;
  clear(): void;
}

// Tools whose successful result identifies a single touched note.
// `chat-tool-call-opens-touched-note` resolution-rule tool list;
// `edit_note` is included per
// `bug-chat-tool-call-no-link-for-staged-writes`.
const TOUCHED_NOTE_TOOLS = new Set<string>([
  "get_note",
  "write_note",
  "edit_note",
  "set_frontmatter",
  "apply_tag",
  "remove_tag",
]);

export function mountToolCards(ctx: ToolCardContext): ToolCardController {
  const toolCards = new Map<string, ToolCardEls>();

  function appendToolCallStart(callId: string, toolName: string): void {
    const card = document.createElement("div");
    card.className = "chat-tool-call collapsed";

    const head = document.createElement("div");
    head.className = "chat-tool-call-head";
    head.setAttribute("role", "button");
    head.tabIndex = 0;

    const chevron = document.createElement("span");
    chevron.className = "chat-tool-call-chevron";
    chevron.textContent = "▸";
    const glyph = document.createElement("span");
    glyph.className = "chat-tool-call-glyph";
    glyph.textContent = "⏳";
    const nameEl = document.createElement("span");
    nameEl.className = "chat-tool-call-name";
    nameEl.textContent = toolName;
    const argsSummary = document.createElement("span");
    argsSummary.className = "chat-tool-call-args-summary";
    argsSummary.textContent = "()";
    const resultSummary = document.createElement("span");
    resultSummary.className = "chat-tool-call-result-summary";

    head.append(chevron, glyph, nameEl, argsSummary, resultSummary);

    const expanded = document.createElement("div");
    expanded.className = "chat-tool-call-expanded";
    expanded.hidden = true;

    card.append(head, expanded);
    ctx.transcriptEl.appendChild(card);

    const els: ToolCardEls = {
      root: card,
      headEl: head,
      glyphEl: glyph,
      nameEl,
      argsSummaryEl: argsSummary,
      resultSummaryEl: resultSummary,
      expandedEl: expanded,
      toolName,
      argsBuf: "",
      finalArgs: null,
      result: null,
      expanded: false,
      touched: null,
    };
    // Chevron click expands; clicks elsewhere on the head route through
    // `handleHeadClick`, which opens the touched note (or its staged
    // proposal) for note-touching tool calls and falls back to toggling
    // for everything else. The Accept/Reject button cluster, when
    // present, lives inside the head and stops propagation itself.
    // Pairs with `chat-tool-call-opens-touched-note` (still planned;
    // this lands the routing seam plus the staged-write prong from
    // `bug-chat-tool-card-no-link-for-staged-writes`).
    chevron.addEventListener("click", (ev) => {
      ev.stopPropagation();
      toggleCard(els);
    });
    head.addEventListener("click", () => handleHeadClick(els));
    head.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        handleHeadClick(els);
      }
    });

    toolCards.set(callId, els);
    ctx.scrollToBottom();
    ctx.onClearAssistantBubble();
  }

  function handleHeadClick(c: ToolCardEls): void {
    if (!c.touched) {
      toggleCard(c);
      return;
    }
    const pending = ctx.getPendingStagingIds();
    const stagedId = c.touched.stagingIds.find((id) => pending.has(id));
    if (stagedId) {
      // Staged: route through the staging-preview seam so the host
      // lands the user in the appropriate review surface
      // (`note-open-routes-to-pending-review`). Single seam for both
      // `write_note` (singular `staging_id`) and `edit_note` (N
      // `staging_ids` sharing a `batch_id`) — `openProposalReview` walks
      // through `openFile`, which auto-routes by action.
      ctx.onOpenStagingProposal({ id: stagedId, target_path: c.touched.targetPath });
      return;
    }
    // Not staged (or staging proposal already resolved): the file
    // exists on disk; open it directly.
    ctx.onOpenNoteLink(c.touched.targetPath);
  }

  function toggleCard(c: ToolCardEls): void {
    c.expanded = !c.expanded;
    c.root.classList.toggle("collapsed", !c.expanded);
    c.expandedEl.hidden = !c.expanded;
    if (c.expanded) {
      renderExpanded(c);
    }
  }

  function renderExpanded(c: ToolCardEls): void {
    c.expandedEl.replaceChildren();
    const argsPre = document.createElement("pre");
    argsPre.className = "chat-tool-call-json";
    argsPre.textContent = `args:\n${prettyJson(c.finalArgs ?? c.argsBuf)}`;
    const copy = document.createElement("button");
    copy.type = "button";
    copy.className = "chat-tool-call-copy";
    copy.textContent = "Copy";
    copy.addEventListener("click", (ev) => {
      ev.stopPropagation();
      void navigator.clipboard.writeText(argsPre.textContent ?? "");
    });
    c.expandedEl.append(argsPre, copy);
    if (c.result) {
      const resPre = document.createElement("pre");
      resPre.className = "chat-tool-call-json";
      resPre.textContent = `result (${c.result.ok ? "ok" : "fail"}):\n${c.result.summary}`;
      c.expandedEl.append(resPre);
    }
  }

  function appendToolCallArgsDelta(callId: string, delta: string): void {
    const c = toolCards.get(callId);
    if (!c) return;
    c.argsBuf += delta;
    // Don't re-render the summary on every delta; stream-time summaries
    // would churn. We update once on `tool_call_complete`.
  }

  function appendToolCallComplete(callId: string, args: string): void {
    const c = toolCards.get(callId);
    if (!c) return;
    c.finalArgs = args;
    c.argsSummaryEl.textContent = `(${shortenArgs(args)})`;
    if (c.expanded) renderExpanded(c);
  }

  function appendToolResult(
    callId: string,
    ok: boolean,
    summary: string,
    output?: string,
  ): void {
    const c = toolCards.get(callId);
    if (!c) return;
    c.result = { ok, summary };
    c.glyphEl.textContent = ok ? "✓" : "✗";
    c.resultSummaryEl.textContent = ` — ${summary}`;
    c.resultSummaryEl.classList.toggle("ok", ok);
    c.resultSummaryEl.classList.toggle("fail", !ok);
    // Resolve touched-note routing for note-touching tool calls so the
    // head-click opens the right surface
    // (`chat-tool-call-opens-touched-note`).
    c.touched = resolveTouchedNote(c, output);
    c.headEl.classList.toggle("has-touched", c.touched !== null);
    renderActionButtons(c);
    if (c.expanded) renderExpanded(c);
    ctx.scrollToBottom();
  }

  /// (Re-)render the Accept/Reject button cluster on the card head from
  /// the current pending-staging set. Called from `appendToolResult` and
  /// from the `hiker:staging-changed` listener so a cross-surface
  /// accept/reject in another surface clears the buttons live
  /// (`bug-chat-tool-card-stale-after-cross-surface-accept`).
  function renderActionButtons(c: ToolCardEls): void {
    const prevAction = c.headEl.querySelector(".chat-tool-call-action");
    if (prevAction) prevAction.remove();
    if (!c.touched) return;
    const pending = ctx.getPendingStagingIds();
    const liveStagedIds = c.touched.stagingIds.filter((id) => pending.has(id));
    if (liveStagedIds.length === 0) return;

    // status: staging-accept-reject-from-chat-card
    const actionEl = document.createElement("span");
    actionEl.className = "chat-tool-call-action";
    // Stop click propagation so the head's open-note routing doesn't
    // fire when the user clicks Accept / Reject (the buttons live
    // inside the head element).
    actionEl.addEventListener("click", (ev) => ev.stopPropagation());

    // `bug-write-note-review-accept-stale-proposal-id`: the captured
    // `staging_id` can go stale when a write_note proposal is replayed
    // / reissued through staging churn. For whole-file proposals
    // (single captured id), refresh by `(target_path, action)` and
    // pick the newest. For batch (`edit_note`) shapes the captured
    // ids are still the source of truth — they're authoritative until
    // accepted/rejected (staged once per `propose_batch`).
    const resolveLiveProposalIds = async (): Promise<string[]> => {
      if (!c.touched) return liveStagedIds;
      if (liveStagedIds.length !== 1) return liveStagedIds;
      const captured = liveStagedIds[0];
      const targetPath = c.touched.targetPath;
      try {
        const live = await Ipc.stagingList({ path: targetPath });
        const matches = live.filter(
          (p) => p.action === c.toolName && p.target_path === targetPath,
        );
        if (matches.length === 0) return [captured];
        if (matches.some((p) => p.id === captured)) return [captured];
        matches.sort((a, b) => b.created_at_ms - a.created_at_ms);
        return [matches[0].id];
      } catch {
        return [captured];
      }
    };

    const acceptBtn = document.createElement("button");
    acceptBtn.className = "chat-tool-call-action-accept";
    acceptBtn.textContent = "Accept";
    acceptBtn.addEventListener("click", async () => {
      try {
        const ids = await resolveLiveProposalIds();
        let target = c.touched?.targetPath ?? null;
        for (const id of ids) {
          const outcome = await Ipc.stagingAccept({ proposalId: id });
          target = outcome.target_path;
        }
        if (target) ctx.onOpenNoteLink(target);
      } catch {
        /* host listener will surface failure via toast on the IPC layer */
      }
      // Optimistic UX update; the `hiker:staging-changed` listener
      // re-renders us authoritatively right after.
      c.resultSummaryEl.textContent = " — ✓ Applied";
      c.resultSummaryEl.classList.add("ok");
      c.resultSummaryEl.classList.remove("fail");
    });

    const rejectBtn = document.createElement("button");
    rejectBtn.className = "chat-tool-call-action-reject";
    rejectBtn.textContent = "Reject";
    rejectBtn.addEventListener("click", async () => {
      try {
        const ids = await resolveLiveProposalIds();
        for (const id of ids) {
          await Ipc.stagingReject({ proposalId: id });
        }
      } catch {
        /* host listener will surface failure via toast on the IPC layer */
      }
      c.resultSummaryEl.textContent = " — ✗ Rejected";
      c.glyphEl.textContent = "✗";
      c.resultSummaryEl.classList.add("fail");
      c.resultSummaryEl.classList.remove("ok");
    });

    actionEl.append(acceptBtn, rejectBtn);
    c.headEl.appendChild(actionEl);
  }

  /// Parse the tool result's JSON output and extract touched-note
  /// routing info for note-touching tools. Resolution rule per
  /// `chat-tool-call-opens-touched-note`: prefer the result's
  /// `rel_path` / `path` field; fall back to the call's args. Carries
  /// `staging_id` (write_note / set_frontmatter / apply_tag) and/or
  /// `staging_ids` (edit_note) when the result is staged.
  function resolveTouchedNote(
    c: ToolCardEls,
    output?: string,
  ): TouchedNoteRouting | null {
    if (!c.result?.ok) return null;
    if (!TOUCHED_NOTE_TOOLS.has(c.toolName)) return null;

    let parsed: Record<string, unknown> | null = null;
    if (output) {
      try {
        const v = JSON.parse(output);
        if (v && typeof v === "object" && !Array.isArray(v)) {
          parsed = v as Record<string, unknown>;
        }
      } catch {
        /* fall through to args fallback */
      }
    }

    const targetPath =
      pickRelPath(parsed) ?? pickRelPathFromArgs(c.finalArgs ?? c.argsBuf);
    if (!targetPath) return null;

    const stagingIds: string[] = [];
    if (parsed) {
      const sid = parsed.staging_id;
      if (typeof sid === "string" && sid) stagingIds.push(sid);
      const sids = parsed.staging_ids;
      if (Array.isArray(sids)) {
        for (const v of sids) if (typeof v === "string" && v) stagingIds.push(v);
      }
    }
    return { targetPath, stagingIds };
  }

  function rerenderAllActionButtons(): void {
    for (const c of toolCards.values()) {
      renderActionButtons(c);
    }
  }

  function clear(): void {
    toolCards.clear();
  }

  return {
    rerenderAllActionButtons,
    appendToolCallStart,
    appendToolCallArgsDelta,
    appendToolCallComplete,
    appendToolResult,
    clear,
  };
}

function prettyJson(s: string): string {
  if (!s.trim()) return "(empty)";
  try {
    return JSON.stringify(JSON.parse(s), null, 2);
  } catch {
    return s;
  }
}

function shortenArgs(args: string): string {
  let parsed: Record<string, unknown> | null = null;
  try {
    const v = JSON.parse(args);
    if (v && typeof v === "object" && !Array.isArray(v)) {
      parsed = v as Record<string, unknown>;
    }
  } catch {
    /* fall through */
  }
  if (!parsed) {
    const trimmed = args.replace(/\s+/g, " ").trim();
    return trimmed.length > 80 ? trimmed.slice(0, 79) + "…" : trimmed;
  }
  const pairs: string[] = [];
  let used = 0;
  for (const [k, v] of Object.entries(parsed)) {
    const repr = JSON.stringify(v) ?? String(v);
    const tail = repr.length > 30 ? repr.slice(0, 29) + "…" : repr;
    const piece = `${k}: ${tail}`;
    pairs.push(piece);
    used += piece.length + 2;
    if (pairs.length >= 2 || used > 60) break;
  }
  let out = pairs.join(", ");
  if (Object.keys(parsed).length > pairs.length) out += ", …";
  return out.length > 80 ? out.slice(0, 79) + "…" : out;
}

function pickRelPath(obj: Record<string, unknown> | null): string | null {
  if (!obj) return null;
  for (const k of ["rel_path", "path"]) {
    const v = obj[k];
    if (typeof v === "string" && v) return v;
  }
  return null;
}

function pickRelPathFromArgs(argsJson: string): string | null {
  if (!argsJson.trim()) return null;
  try {
    const v = JSON.parse(argsJson);
    if (v && typeof v === "object" && !Array.isArray(v)) {
      return pickRelPath(v as Record<string, unknown>);
    }
  } catch {
    /* ignore */
  }
  return null;
}
