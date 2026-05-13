// Chat panel — frontend half of the basic agent loop. Subscribes to
// `hiker:chat-event` and renders the transcript; the backend tasks live
// in `ui/src-tauri/src/chat.rs` and the agent loop itself in
// `core/src/agent.rs`.
//
// status: chat-panel-pinned-bottom
// status: chat-panel-detached-scroll
// status: chat-panel-vertical-resize
// status: chat-panel-default-height
// status: agent-event-stream-shape
// status: agent-iteration-cap-prompt
// status: chat-session-persisted-history
// status: chat-session-new-button
// status: chat-session-resume-latest
// status: chat-active-note-context-injection
// status: chat-input-at-mentions
// status: chat-input-at-mentions-dedup
// status: chat-panel-note-link-render
// status: chat-panel-thinking-indicator
// status: chat-panel-tool-call-collapsible
// status: chat-panel-stop-button

import { listen } from "@tauri-apps/api/event";
import { Ipc, type SessionListItem } from "./ipc";
import { mountChatMarkdown, type ChatMarkdownView } from "./chatMarkdownView";
import { mountAtMentions, type AtMentionsApi, type ParsedAtToken } from "./atMentions";
import { resolveAtNote } from "./notes/resolver";
import type { BufferApi } from "./app/state";
import { Icons } from "./icons";

type AgentEvent =
  | { kind: "turn_started"; turn_id: string; user_message_summary: string }
  | { kind: "step_started"; turn_id: string; step_id: number }
  | { kind: "text_delta"; turn_id: string; step_id: number; text: string }
  | {
      kind: "tool_call_start";
      turn_id: string;
      step_id: number;
      call_id: string;
      tool_name: string;
    }
  | {
      kind: "tool_call_args_delta";
      turn_id: string;
      step_id: number;
      call_id: string;
      args_delta: string;
    }
  | {
      kind: "tool_call_complete";
      turn_id: string;
      step_id: number;
      call_id: string;
      args: string;
    }
  | {
      kind: "tool_result";
      turn_id: string;
      step_id: number;
      call_id: string;
      ok: boolean;
      summary: string;
      output?: string;
    }
  | {
      kind: "step_finished";
      turn_id: string;
      step_id: number;
      finish_reason: FinishReason;
    }
  | {
      kind: "iteration_cap_hit";
      turn_id: string;
      completed_iterations: number;
    }
  | { kind: "turn_finished"; turn_id: string; finish_reason: FinishReason }
  | { kind: "error"; turn_id: string; step_id: number | null; message: string };

type FinishReason =
  | "end_turn"
  | "tool_use"
  | "cap_hit"
  | "user_halted"
  | "cancelled"
  | "errored";

interface ResumedTurn {
  user: string;
  agent: string;
}

interface ActiveSessionDto {
  sessionId: string;
  relPath: string;
  turns: ResumedTurn[];
}

interface ChatPanel {
  setEnabled(enabled: boolean): void;
  setHeight(fraction: number): void;
  /// Apply a persisted chat input height (pixels). 0 means auto-grow.
  setInputHeight(px: number): void;
  reset(): void;
  /// Start a fresh session — backed by `chat_session_new`. Clears the
  /// transcript and pins the new id as active.
  /// status: chat-session-new-button
  newSession(): Promise<void>;
  /// Re-seed the panel from the resume-latest payload returned by
  /// `chat_session_active` at vault open. Idempotent — calling with
  /// `null` clears the panel without affecting the backend registry.
  /// status: chat-session-resume-latest
  hydrate(active: ActiveSessionDto | null): void;
  /// Returns the active session id, or null when no session is active.
  /// status: chat-panel-expand-to-editor
  getActiveSessionId?(): string | null;
}

export interface ChatPanelOptions {
  appEl: HTMLElement;
  regionEl: HTMLElement;
  handleEl: HTMLElement;
  collapseBtnEl: HTMLButtonElement;
  /// Dropdown trigger on the left side of the handle. Click opens a
  /// popover listing past sessions + a "New session" entry.
  sessionMenuBtnEl: HTMLButtonElement;
  /// `<span>` inside the menu button that mirrors the active session's
  /// short label ("New session" / first-message preview). Updated on
  /// hydrate / new-session / open-session.
  sessionMenuLabelEl: HTMLElement;
  panelEl: HTMLElement;
  transcriptEl: HTMLElement;
  formEl: HTMLFormElement;
  inputEl: HTMLTextAreaElement;
  sendBtnEl: HTMLButtonElement;
  /// Called when the user finishes a drag, so the host can persist the
  /// new fraction via `set_setting("vault.chat_height", ...)`.
  onResizePersist: (fraction: number) => void;
  /// Called when the user finishes dragging the input-area resize handle,
  /// so the host can persist the new height via
  /// `set_setting("vault.chat_input_height", ...)`. Passes 0 when the
  /// user drags to the auto-grow floor (unsets the override).
  onInputHeightPersist: (heightPx: number) => void;
  /// Called by the chat panel when an `hiker://note/<rel>` (or bare
  /// vault-relative) link in an agent message is clicked. The host runs
  /// the existing `openFile` machinery (which handles
  /// `file-switch-guard-dirty`).
  /// status: chat-panel-note-link-render
  onOpenNoteLink: (rel: string) => void;
  /// Open a staged proposal for review — same seam the activity widget
  /// uses (`vaultHome` / `tree`). Used by the tool-call card's header
  /// click when the result carries `status: "staged"`, so we don't call
  /// `openFile` against a path that has no on-disk content yet
  /// (`bug-chat-tool-card-no-link-for-staged-writes`). Host routes
  /// through `openProposalReview`, which dispatches to patch-review or
  /// write-note review via `note-open-routes-to-pending-review`.
  onOpenStagingProposal: (proposal: { id: string; target_path: string }) => void;
  /// Cross-module read surface for the active editable buffer +
  /// selection. Replaces the prior `getActiveNote` / `getActiveSelection`
  /// closures over main.ts internals (`bug-chat-couples-to-main-buffer-globals`).
  /// Chat polls `bufferApi.getActive()` at submit time; subscriptions
  /// aren't wired since chat has no reactive surface today.
  /// status: chat-active-note-context-injection
  /// status: chat-input-at-selection
  bufferApi: BufferApi;
  /// Surface a transient toast (e.g. for failed `@<rel-path>` resolution
  /// at submit time). The host wires this to its existing toast helper.
  /// status: chat-input-at-mentions
  toast: (message: string) => void;
}

/// Wire shape for the backend's composed context block list (per
/// `chat-input-at-mentions`). Frontend-built and passed to `chat_send`.
interface ChatContextBlock {
  kind: "activeNote" | "selection" | "note";
  relPath: string;
  content: string;
  lineRange?: string | null;
}

interface RenderState {
  // The currently-rendering assistant message bubble (text deltas stream
  // into here). Reset on `step_started`.
  assistantBubble: { body: HTMLElement; mdView: ChatMarkdownView } | null;
  // Currently-rendering tool-call cards keyed by `call_id` so
  // ToolCallArgsDelta can stream into the right place.
  toolCards: Map<string, ToolCardEls>;
}

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
  /// `bug-chat-tool-card-no-link-for-staged-writes`.
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

export function mountChatPanel(opts: ChatPanelOptions): ChatPanel {
  const {
    appEl,
    regionEl,
    handleEl,
    collapseBtnEl,
    sessionMenuBtnEl,
    sessionMenuLabelEl,
    panelEl,
    transcriptEl,
    formEl,
    inputEl,
    sendBtnEl,
    onResizePersist,
    onInputHeightPersist,
    onOpenNoteLink,
    onOpenStagingProposal,
    bufferApi,
    toast,
  } = opts;

  // status: chat-input-at-mentions
  // status: chat-input-at-autocomplete
  const atMentions: AtMentionsApi = mountAtMentions({
    inputEl,
    anchorEl: panelEl,
    hasEditorSelection: () => (bufferApi.getActive()?.selection ?? null) !== null,
  });

  let activeSessionId: string | null = null;
  let activeTurnId: string | null = null;
  let pausedAtCap = false;
  // status: chat-panel-thinking-indicator
  // The pulsing "..." dots node currently in the transcript, if any.
  // Inserted after a user send (or after a tool result) and removed on
  // the first content event of the upcoming step.
  let thinkingEl: HTMLElement | null = null;
  const renderState: RenderState = {
    assistantBubble: null,
    toolCards: new Map(),
  };

  // status: staging-accept-reject-from-chat-card
  // `bug-chat-tool-card-stale-after-cross-surface-accept` /
  // `bug-chat-tool-card-no-link-for-staged-writes`.
  //
  // Local mirror of pending staging proposal ids, refreshed from
  // `Ipc.stagingList()` on every `hiker:staging-changed` event. Used to
  // (a) re-render the Accept/Reject buttons on tool cards so a
  // cross-surface accept/reject (e.g. from the activity-detail page)
  // clears them in the chat session too, and (b) decide whether the
  // header-click on a touched-note tool card routes to the staging
  // preview or directly to the on-disk note.
  let pendingStagingIds: Set<string> = new Set();
  async function refreshPendingStagingIds(): Promise<void> {
    try {
      const list = await Ipc.stagingList();
      pendingStagingIds = new Set(list.map((p) => p.id));
    } catch {
      pendingStagingIds = new Set();
    }
    for (const c of renderState.toolCards.values()) {
      renderActionButtons(c);
    }
  }
  void refreshPendingStagingIds();
  void listen("hiker:staging-changed", () => {
    void refreshPendingStagingIds();
  });

  // User-set input height in px. 0 = auto-grow mode (no manual override).
  let userInputHeight = 0;

  // ---------- send / continue / stop ----------

  function newTurnId(): string {
    return `t-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }

  // status: chat-panel-stop-button
  // While a turn is in flight the send affordance flips to a Stop
  // button. Click invokes `chat_stop` (same Tauri command the cap-hit
  // row's Stop already calls); the button reverts to Send on the next
  // `TurnFinished`. Cap-hit rows aren't "in flight" — `setBusy(false)`
  // fires when the cap row appears, so the button reverts to Send and
  // the row's own Stop handles that path.
  let busyState = false;
  function setBusy(busy: boolean): void {
    busyState = busy;
    if (busy) {
      sendBtnEl.disabled = false;
      sendBtnEl.innerHTML = Icons.stop();
      sendBtnEl.title = "Stop";
      sendBtnEl.setAttribute("aria-label", "Stop");
    } else {
      sendBtnEl.disabled = inputEl.value.trim().length === 0;
      sendBtnEl.innerHTML = Icons.send();
      sendBtnEl.title = "Send";
      sendBtnEl.setAttribute("aria-label", "Send");
    }
    inputEl.disabled = busy;
  }

  function stopActiveTurn(): void {
    if (!activeTurnId) return;
    const turnId = activeTurnId;
    void Ipc.chatStop({ sessionId: activeSessionId, turnId });
  }

  async function send(): Promise<void> {
    const message = inputEl.value;
    if (!message.trim()) return;

    // status: chat-input-at-mentions
    // Resolve `@`-tokens *before* clearing the input, so a resolution
    // error leaves the user's text intact for them to fix.
    const tokens = atMentions.parseTokens();
    let contextBlocks: ChatContextBlock[];
    try {
      contextBlocks = await composeContextBlocks(tokens);
    } catch (e) {
      toast(describeErr(e));
      return;
    }

    inputEl.value = "";
    autoSizeInput();

    if (pausedAtCap && activeTurnId) {
      pausedAtCap = false;
      removeCapRows();
    } else {
      activeTurnId = newTurnId();
    }
    appendUserMessage(message);
    showThinking();
    setBusy(true);
    try {
      const returnedSessionId = await Ipc.chatSend({
        sessionId: activeSessionId,
        turnId: activeTurnId,
        message,
        contextBlocks,
      });
      activeSessionId = returnedSessionId;
    } catch (e) {
      appendError(`Failed to send: ${describeErr(e)}`);
      setBusy(false);
    }
  }

  // status: chat-input-at-mentions
  // status: chat-input-at-mentions-dedup
  // Compose the list of turn-scoped context blocks from (a) the
  // auto-injected active note and (b) the explicit `@`-mentions in the
  // user message, in source order. De-dup by rel-path so the same note
  // isn't sent twice; `@selection` is never de-duped against
  // `@<rel-path>` (the slice is the point).
  async function composeContextBlocks(
    tokens: ParsedAtToken[],
  ): Promise<ChatContextBlock[]> {
    const blocks: ChatContextBlock[] = [];
    const seenRelPaths = new Set<string>();

    // One snapshot per send — the buffer / selection can't change mid-compose.
    const active = bufferApi.getActive();
    if (active) {
      blocks.push({
        kind: "activeNote",
        relPath: active.relPath,
        content: active.bufferText,
      });
      seenRelPaths.add(active.relPath);
    }

    for (const tok of tokens) {
      if (tok.kind === "selection") {
        const sel = active?.selection ?? null;
        if (!active || !sel) {
          throw new Error("@selection has no selected text");
        }
        // Selection blocks are never de-duped — the slice is the point.
        blocks.push({
          kind: "selection",
          relPath: active.relPath,
          content: sel.text,
          lineRange: sel.lineRange,
        });
      } else {
        // Resolve `@<rel-path-no-ext>` against the vault. Backend
        // probes .md / .markdown / .txt and returns the actual rel-path
        // (with extension) + body. Throws "note not found: <rel>" when
        // the path no longer resolves.
        const resolved = await resolveAtNote(tok.relPathNoExt);
        if (seenRelPaths.has(resolved.relPath)) continue;
        seenRelPaths.add(resolved.relPath);
        blocks.push({
          kind: "note",
          relPath: resolved.relPath,
          content: resolved.content,
        });
      }
    }
    return blocks;
  }

  formEl.addEventListener("submit", (ev) => {
    ev.preventDefault();
    if (busyState) {
      stopActiveTurn();
      return;
    }
    void send();
  });

  inputEl.addEventListener("input", () => {
    if (busyState) return;
    sendBtnEl.disabled = inputEl.value.trim().length === 0;
    autoSizeInput();
  });

  inputEl.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
      if (busyState) {
        stopActiveTurn();
        return;
      }
      void send();
    }
  });

  // ---------- session-picker dropdown ----------

  let menuPopover: HTMLElement | null = null;

  sessionMenuBtnEl.addEventListener("click", (ev) => {
    ev.stopPropagation();
    void toggleSessionMenu();
  });

  function setSessionLabel(text: string): void {
    sessionMenuLabelEl.textContent = text;
    sessionMenuBtnEl.title = text;
  }

  async function toggleSessionMenu(): Promise<void> {
    if (menuPopover) {
      closeSessionMenu();
      return;
    }
    let items: SessionListItem[] = [];
    try {
      items = await Ipc.chatSessionList();
    } catch (e) {
      appendError(`Failed to list sessions: ${describeErr(e)}`);
      return;
    }
    openSessionMenu(items);
  }

  function openSessionMenu(items: SessionListItem[]): void {
    closeSessionMenu();
    const pop = document.createElement("div");
    pop.className = "chat-session-menu-popover";
    pop.setAttribute("role", "menu");

    const newRow = document.createElement("button");
    newRow.type = "button";
    newRow.className = "chat-session-menu-row chat-session-menu-row-new";
    newRow.setAttribute("role", "menuitem");
    newRow.innerHTML = `<span class="chat-session-menu-row-icon">+</span><span class="chat-session-menu-row-label">New session</span>`;
    newRow.addEventListener("click", (ev) => {
      ev.stopPropagation();
      closeSessionMenu();
      void doNewSession();
    });
    pop.appendChild(newRow);

    if (items.length > 0) {
      const sep = document.createElement("div");
      sep.className = "chat-session-menu-sep";
      pop.appendChild(sep);
    }

    for (const item of items) {
      // Two stacked elements per row: the open-session button (full row)
      // + a small trash icon overlaid on the right. We can't nest a
      // <button> inside the row's <button>, so the row is a <div role=
      // "menuitem"> with an inner click target and a sibling icon.
      const row = document.createElement("div");
      row.className = "chat-session-menu-row";
      if (item.isActive) row.classList.add("active");
      row.setAttribute("role", "menuitem");
      const date = formatShortDate(item.mtimeUnix);
      const preview = item.firstUserPreview || "(empty session)";

      const dateEl = document.createElement("span");
      dateEl.className = "chat-session-menu-row-date";
      dateEl.textContent = date;
      const labelEl = document.createElement("span");
      labelEl.className = "chat-session-menu-row-label";
      labelEl.textContent = preview;
      const countEl = document.createElement("span");
      countEl.className = "chat-session-menu-row-count";
      countEl.textContent = String(item.turnCount);
      // status: chat-session-trash
      const trashBtn = document.createElement("button");
      trashBtn.type = "button";
      trashBtn.className = "chat-session-menu-row-trash";
      trashBtn.title = "Move session to trash";
      trashBtn.setAttribute("aria-label", "Move session to trash");
      trashBtn.innerHTML = Icons.trash();
      trashBtn.addEventListener("click", (ev) => {
        ev.stopPropagation();
        void doDeleteSession(item.sessionId, item.isActive);
      });

      row.append(dateEl, labelEl, countEl, trashBtn);
      row.addEventListener("click", (ev) => {
        if (ev.target instanceof Element && ev.target.closest(".chat-session-menu-row-trash")) {
          return;
        }
        ev.stopPropagation();
        closeSessionMenu();
        if (!item.isActive) {
          void doOpenSession(item.sessionId);
        }
      });
      pop.appendChild(row);
    }

    // Position above the trigger; the popover floats absolutely against
    // the discovery panel so it doesn't get clipped by the resize handle's
    // border or the chat region's overflow.
    pop.style.position = "absolute";
    panelEl.appendChild(pop);
    const btnRect = sessionMenuBtnEl.getBoundingClientRect();
    const panelRect = panelEl.getBoundingClientRect();
    pop.style.left = `${btnRect.left - panelRect.left}px`;
    pop.style.bottom = `${panelRect.bottom - btnRect.top + 4}px`;
    pop.style.maxHeight = `${Math.max(120, btnRect.top - panelRect.top - 12)}px`;

    menuPopover = pop;
    sessionMenuBtnEl.setAttribute("aria-expanded", "true");

    // One-shot outside-click closer; rebuilt every open.
    setTimeout(() => {
      document.addEventListener("click", outsideClickClose, { once: true });
    }, 0);
  }

  function outsideClickClose(ev: MouseEvent): void {
    if (
      menuPopover &&
      ev.target instanceof Node &&
      !menuPopover.contains(ev.target) &&
      !sessionMenuBtnEl.contains(ev.target)
    ) {
      closeSessionMenu();
    } else if (menuPopover) {
      // Click landed inside; re-arm the listener for the next outside click.
      document.addEventListener("click", outsideClickClose, { once: true });
    }
  }

  function closeSessionMenu(): void {
    if (menuPopover) {
      menuPopover.remove();
      menuPopover = null;
    }
    sessionMenuBtnEl.setAttribute("aria-expanded", "false");
  }

  async function doNewSession(): Promise<void> {
    try {
      const sid = await Ipc.chatSessionNew();
      activeSessionId = sid;
      activeTurnId = null;
      pausedAtCap = false;
      renderState.assistantBubble = null;
      renderState.toolCards.clear();
      transcriptEl.replaceChildren();
      inputEl.value = "";
      autoSizeInput();
      setBusy(false);
      setSessionLabel("New session");
    } catch (e) {
      appendError(`Failed to start new session: ${describeErr(e)}`);
    }
  }

  // status: chat-session-trash
  async function doDeleteSession(id: string, wasActive: boolean): Promise<void> {
    try {
      await Ipc.chatSessionDelete({ sessionId: id });
    } catch (e) {
      appendError(`Failed to delete session: ${describeErr(e)}`);
      return;
    }
    if (wasActive) {
      // Active session went to trash; clear the panel so the user
      // doesn't keep typing into a turn that will land in a stale id.
      // Next send will lazily create a fresh session.
      activeSessionId = null;
      activeTurnId = null;
      pausedAtCap = false;
      thinkingEl = null;
      renderState.assistantBubble = null;
      renderState.toolCards.clear();
      transcriptEl.replaceChildren();
      setSessionLabel("New session");
    }
    // Re-open the menu against the fresh listing so the user can pick
    // a different session or restore from the regular trash bin.
    closeSessionMenu();
    void toggleSessionMenu();
  }

  async function doOpenSession(id: string): Promise<void> {
    try {
      const active = await Ipc.chatSessionOpen({ sessionId: id });
      if (!active) return;
      hydrateInternal(active);
    } catch (e) {
      appendError(`Failed to open session: ${describeErr(e)}`);
    }
  }

  function autoSizeInput(): void {
    if (userInputHeight > 0) return;
    // Clear inline height when empty — let CSS min-height carry the row.
    if (!inputEl.value) {
      inputEl.style.height = "";
      return;
    }
    inputEl.style.height = "auto";
    inputEl.style.height = `${Math.min(inputEl.scrollHeight, 120)}px`;
  }

  // ---------- transcript rendering ----------

  function ensureAssistantBubble(): { body: HTMLElement; mdView: ChatMarkdownView } {
    if (renderState.assistantBubble) return renderState.assistantBubble;
    const wrap = document.createElement("div");
    wrap.className = "chat-msg chat-msg-assistant";
    const role = document.createElement("span");
    role.className = "chat-msg-role";
    role.textContent = "Agent";
    const body = document.createElement("div");
    body.className = "chat-msg-body";
    wrap.appendChild(role);
    wrap.appendChild(body);
    transcriptEl.appendChild(wrap);
    const mdView = mountChatMarkdown({
      host: body,
      onOpenNoteLink,
    });
    const entry = { body, mdView };
    renderState.assistantBubble = entry;
    return entry;
  }

  function appendUserMessage(text: string): void {
    const wrap = document.createElement("div");
    wrap.className = "chat-msg chat-msg-user";
    wrap.textContent = text;
    transcriptEl.appendChild(wrap);
    scrollToBottom();
    renderState.assistantBubble = null;
  }

  function appendTextDelta(text: string): void {
    const bubble = ensureAssistantBubble();
    bubble.mdView.append(text);
    scrollToBottom();
  }

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
    transcriptEl.appendChild(card);

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

    renderState.toolCards.set(callId, els);
    scrollToBottom();
    renderState.assistantBubble = null;
  }

  // Tools whose successful result identifies a single touched note.
  // `chat-tool-call-opens-touched-note` resolution-rule tool list;
  // `edit_note` is included per
  // `bug-chat-tool-card-no-link-for-staged-writes`.
  const TOUCHED_NOTE_TOOLS = new Set<string>([
    "get_note",
    "write_note",
    "edit_note",
    "set_frontmatter",
    "apply_tag",
    "remove_tag",
  ]);

  function handleHeadClick(c: ToolCardEls): void {
    if (!c.touched) {
      toggleCard(c);
      return;
    }
    const stagedId = c.touched.stagingIds.find((id) => pendingStagingIds.has(id));
    if (stagedId) {
      // Staged: route through the staging-preview seam so the host
      // lands the user in the appropriate review surface
      // (`note-open-routes-to-pending-review`). Single seam for both
      // `write_note` (singular `staging_id`) and `edit_note` (N
      // `staging_ids` sharing a `batch_id`) — `openProposalReview` walks
      // through `openFile`, which auto-routes by action.
      onOpenStagingProposal({ id: stagedId, target_path: c.touched.targetPath });
      return;
    }
    // Not staged (or staging proposal already resolved): the file
    // exists on disk; open it directly.
    onOpenNoteLink(c.touched.targetPath);
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

  function prettyJson(s: string): string {
    if (!s.trim()) return "(empty)";
    try {
      return JSON.stringify(JSON.parse(s), null, 2);
    } catch {
      return s;
    }
  }

  function appendToolCallArgsDelta(callId: string, delta: string): void {
    const c = renderState.toolCards.get(callId);
    if (!c) return;
    c.argsBuf += delta;
    // Don't re-render the summary on every delta; stream-time summaries
    // would churn. We update once on `tool_call_complete`.
  }

  function appendToolCallComplete(callId: string, args: string): void {
    const c = renderState.toolCards.get(callId);
    if (!c) return;
    c.finalArgs = args;
    c.argsSummaryEl.textContent = `(${shortenArgs(args)})`;
    if (c.expanded) renderExpanded(c);
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

  function appendToolResult(callId: string, ok: boolean, summary: string, output?: string): void {
    const c = renderState.toolCards.get(callId);
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
    scrollToBottom();
  }

  /// Parse the tool result's JSON output and extract touched-note
  /// routing info for note-touching tools. Resolution rule per
  /// `chat-tool-call-opens-touched-note`: prefer the result's
  /// `rel_path` / `path` field; fall back to the call's args. Carries
  /// `staging_id` (write_note / set_frontmatter / apply_tag) and/or
  /// `staging_ids` (edit_note) when the result is staged.
  function resolveTouchedNote(c: ToolCardEls, output?: string): TouchedNoteRouting | null {
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

  /// (Re-)render the Accept/Reject button cluster on the card head from
  /// the current pending-staging set. Called from `appendToolResult` and
  /// from the `hiker:staging-changed` listener so a cross-surface
  /// accept/reject in another surface clears the buttons live
  /// (`bug-chat-tool-card-stale-after-cross-surface-accept`).
  function renderActionButtons(c: ToolCardEls): void {
    const prevAction = c.headEl.querySelector(".chat-tool-call-action");
    if (prevAction) prevAction.remove();
    if (!c.touched) return;
    const liveStagedIds = c.touched.stagingIds.filter((id) =>
      pendingStagingIds.has(id),
    );
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
        if (target) onOpenNoteLink(target);
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

  function appendCapRow(turnId: string, completed: number): void {
    const row = document.createElement("div");
    row.className = "chat-cap-row";
    const text = document.createElement("span");
    text.textContent = `Agent has made ${completed} tool calls — `;
    const cont = document.createElement("button");
    cont.textContent = "Continue";
    cont.addEventListener("click", () => {
      row.remove();
      pausedAtCap = false;
      void Ipc.chatContinue({
        sessionId: activeSessionId,
        turnId,
      });
      setBusy(true);
    });
    const stop = document.createElement("button");
    stop.textContent = "Stop";
    stop.addEventListener("click", () => {
      row.remove();
      pausedAtCap = false;
      void Ipc.chatStop({
        sessionId: activeSessionId,
        turnId,
      });
    });
    row.append(text, cont, stop);
    transcriptEl.appendChild(row);
    pausedAtCap = true;
    scrollToBottom();
  }

  function removeCapRows(): void {
    transcriptEl.querySelectorAll(".chat-cap-row").forEach((el) => el.remove());
  }

  function appendError(message: string): void {
    const row = document.createElement("div");
    row.className = "chat-msg-error";
    row.textContent = message;
    transcriptEl.appendChild(row);
    scrollToBottom();
  }

  function appendSystemRow(message: string): void {
    const row = document.createElement("div");
    row.className = "chat-msg-system";
    row.textContent = message;
    transcriptEl.appendChild(row);
    scrollToBottom();
  }

  function scrollToBottom(): void {
    const dist =
      transcriptEl.scrollHeight - transcriptEl.scrollTop - transcriptEl.clientHeight;
    if (dist < 60) {
      transcriptEl.scrollTop = transcriptEl.scrollHeight;
    }
  }

  // status: chat-panel-thinking-indicator
  // Pulsing-dots placeholder shown while the agent is "quiet" — between
  // a user send and the first streaming event, and between a tool
  // result and the next streaming event of the resumed step. Plain
  // text nodes; the pulse is pure CSS.
  function showThinking(): void {
    if (thinkingEl) return;
    const el = document.createElement("div");
    el.className = "chat-msg chat-msg-thinking";
    el.setAttribute("aria-label", "Agent is thinking");
    for (let i = 0; i < 3; i++) {
      const dot = document.createElement("span");
      dot.className = "chat-thinking-dot";
      dot.textContent = "•";
      el.appendChild(dot);
    }
    transcriptEl.appendChild(el);
    thinkingEl = el;
    scrollToBottom();
  }

  function hideThinking(): void {
    if (!thinkingEl) return;
    thinkingEl.remove();
    thinkingEl = null;
  }

  // ---------- event stream ----------

  function handleEvent(ev: AgentEvent): void {
    if (activeTurnId && ev.turn_id !== activeTurnId) return;
    switch (ev.kind) {
      case "turn_started":
        break;
      case "step_started":
        renderState.assistantBubble = null;
        break;
      case "text_delta":
        // First content of this step — drop the indicator.
        hideThinking();
        appendTextDelta(ev.text);
        break;
      case "tool_call_start":
        // Tool card is also content for the spec's purposes.
        hideThinking();
        appendToolCallStart(ev.call_id, ev.tool_name);
        break;
      case "tool_call_args_delta":
        appendToolCallArgsDelta(ev.call_id, ev.args_delta);
        break;
      case "tool_call_complete":
        appendToolCallComplete(ev.call_id, ev.args);
        break;
      case "tool_result":
        appendToolResult(ev.call_id, ev.ok, ev.summary, ev.output);
        // Tool returned → model goes quiet again until the next step's
        // first TextDelta / ToolCallStart. Show the indicator so the
        // pause is visible rather than implied.
        showThinking();
        break;
      case "step_finished":
        break;
      case "iteration_cap_hit":
        hideThinking();
        appendCapRow(ev.turn_id, ev.completed_iterations);
        setBusy(false);
        break;
      case "turn_finished":
        hideThinking();
        if (ev.finish_reason === "user_halted") {
          appendSystemRow("Stopped.");
        } else if (ev.finish_reason === "cancelled") {
          appendSystemRow("Cancelled.");
        }
        if (ev.finish_reason !== "cap_hit") {
          activeTurnId = null;
        }
        setBusy(false);
        break;
      case "error":
        hideThinking();
        appendError(ev.message);
        setBusy(false);
        break;
    }
  }

  void listen<AgentEvent>("hiker:chat-event", (event) => {
    handleEvent(event.payload);
  });

  // ---------- resize ----------

  let dragStartY = 0;
  let dragStartFraction = 0;

  handleEl.addEventListener("pointerdown", (ev) => {
    if (
      ev.target instanceof Element &&
      (ev.target.closest("#chat-collapse-btn") ||
        ev.target.closest("#chat-session-menu-btn") ||
        ev.target.closest(".chat-session-menu-popover"))
    ) {
      return;
    }
    if (appEl.classList.contains("chat-collapsed")) return;
    ev.preventDefault();
    handleEl.classList.add("dragging");
    handleEl.setPointerCapture(ev.pointerId);
    dragStartY = ev.clientY;
    dragStartFraction = currentFraction();
  });

  handleEl.addEventListener("pointermove", (ev) => {
    if (!handleEl.classList.contains("dragging")) return;
    const dy = ev.clientY - dragStartY;
    const panelH = panelEl.clientHeight || 1;
    const next = clamp(dragStartFraction - dy / panelH, 0.1, 0.9);
    setFraction(next);
  });

  handleEl.addEventListener("pointerup", (ev) => {
    if (!handleEl.classList.contains("dragging")) return;
    handleEl.classList.remove("dragging");
    handleEl.releasePointerCapture(ev.pointerId);
    onResizePersist(currentFraction());
  });

  // ---------- input resize ----------

  // Create a resize handle just above the chat input form. Mirrors the
  // shape of `#chat-resize-handle` one level up: a thin horizontal grab
  // bar that the user drags to grow/shrink the input area.
  const inputResizeEl = document.createElement("div");
  inputResizeEl.className = "chat-input-resize-handle";
  inputResizeEl.setAttribute("role", "separator");
  inputResizeEl.setAttribute("aria-orientation", "horizontal");
  inputResizeEl.setAttribute("aria-label", "Resize chat input");
  formEl.before(inputResizeEl);

  let inputDragStartY = 0;
  let inputDragStartHeight = 0;

  inputResizeEl.addEventListener("pointerdown", (ev) => {
    if (appEl.classList.contains("chat-collapsed")) return;
    ev.preventDefault();
    inputResizeEl.classList.add("dragging");
    inputResizeEl.setPointerCapture(ev.pointerId);
    inputDragStartY = ev.clientY;
    inputDragStartHeight = inputEl.getBoundingClientRect().height;
  });

  inputResizeEl.addEventListener("pointermove", (ev) => {
    if (!inputResizeEl.classList.contains("dragging")) return;
    const dy = ev.clientY - inputDragStartY;
    // Dragging up (negative dy) grows the input; dragging down shrinks it.
    const next = clamp(inputDragStartHeight - dy, MIN_INPUT_HEIGHT, maxInputHeight());
    setInputHeightPx(next);
  });

  inputResizeEl.addEventListener("pointerup", (ev) => {
    if (!inputResizeEl.classList.contains("dragging")) return;
    inputResizeEl.classList.remove("dragging");
    inputResizeEl.releasePointerCapture(ev.pointerId);
    const h = userInputHeight;
    // Persist 0 when user drags to the auto-grow floor, so the next
    // vault open starts in auto-grow mode.
    const flush = h <= MIN_INPUT_HEIGHT + 4 ? 0 : Math.round(h);
    onInputHeightPersist(flush);
  });

  const MIN_INPUT_HEIGHT = 28;

  function maxInputHeight(): number {
    // Chat region height minus room for the handle + form padding +
    // at least a couple transcript lines (~60px).
    const regionH = regionEl.clientHeight || 0;
    return Math.max(MIN_INPUT_HEIGHT, regionH - 68);
  }

  function setInputHeightPx(px: number): void {
    userInputHeight = px;
    inputEl.style.height = `${px}px`;
  }

  // ---------- collapse ----------

  collapseBtnEl.addEventListener("click", (ev) => {
    ev.stopPropagation();
    const collapsed = !appEl.classList.contains("chat-collapsed");
    appEl.classList.toggle("chat-collapsed", collapsed);
    collapseBtnEl.classList.toggle("active", !collapsed);
    collapseBtnEl.title = collapsed ? "Show chat" : "Hide chat";
    collapseBtnEl.setAttribute(
      "aria-label",
      collapsed ? "Show chat" : "Hide chat",
    );
  });

  function currentFraction(): number {
    const flexBasis = regionEl.style.flexBasis;
    if (flexBasis.endsWith("%")) {
      return parseFloat(flexBasis) / 100;
    }
    const panel = panelEl.clientHeight;
    const region = regionEl.clientHeight;
    return panel > 0 ? region / panel : 0.3;
  }

  function setFraction(f: number): void {
    const pct = (clamp(f, 0.1, 0.9) * 100).toFixed(2);
    regionEl.style.flex = `0 0 ${pct}%`;
  }

  // ---------- public API ----------

  return {
    setEnabled(enabled) {
      appEl.classList.toggle("chat-disabled", !enabled);
      if (!enabled) {
        activeTurnId = null;
        pausedAtCap = false;
        setBusy(false);
      }
    },
    setHeight(fraction) {
      setFraction(fraction);
    },
    setInputHeight(px) {
      if (px > 0) {
        setInputHeightPx(px);
      } else {
        userInputHeight = 0;
        inputEl.style.height = "";
      }
    },
    reset() {
      activeSessionId = null;
      activeTurnId = null;
      pausedAtCap = false;
      thinkingEl = null;
      renderState.assistantBubble = null;
      renderState.toolCards.clear();
      transcriptEl.replaceChildren();
      inputEl.value = "";
      autoSizeInput();
      setBusy(false);
    },
    newSession: doNewSession,
    hydrate: hydrateInternal,
    // status: chat-panel-expand-to-editor
    /// Returns the active session id, or null when no session is active.
    getActiveSessionId: () => activeSessionId,
  };

  function hydrateInternal(active: ActiveSessionDto | null): void {
    activeTurnId = null;
    pausedAtCap = false;
    thinkingEl = null;
    renderState.assistantBubble = null;
    renderState.toolCards.clear();
    transcriptEl.replaceChildren();
    if (!active) {
      activeSessionId = null;
      setSessionLabel("New session");
      return;
    }
    activeSessionId = active.sessionId;
    for (const t of active.turns) {
      appendUserMessage(t.user);
      const bubble = ensureAssistantBubble();
      bubble.mdView.setText(t.agent);
      renderState.assistantBubble = null;
    }
    const firstUser = active.turns[0]?.user ?? "";
    setSessionLabel(firstUser ? shortLabel(firstUser, 28) : "Session");
  }

  // Defer the initial auto-size pass until after the first layout so
  // `scrollHeight` reads the laid-out element rather than a zero-width
  // / display-pending state.
  requestAnimationFrame(() => autoSizeInput());
}

function shortLabel(s: string, max: number): string {
  const one = s.split(/\r?\n/, 1)[0] ?? "";
  if (one.length <= max) return one;
  return one.slice(0, max - 1) + "…";
}

function formatShortDate(unix: number): string {
  if (!unix) return "";
  const d = new Date(unix * 1000);
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}

function clamp(x: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, x));
}

function describeErr(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "message" in e) {
    return String((e as { message: unknown }).message);
  }
  return JSON.stringify(e);
}

export type { AgentEvent, FinishReason, ActiveSessionDto };
