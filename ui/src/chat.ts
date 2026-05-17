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

import { onHikerEventAs } from "./events";
import { Ipc, type SessionListItem } from "./ipc";
import { mountChatMarkdown, type ChatMarkdownView } from "./chatMarkdownView";
import { mountAtMentions, type AtMentionsApi, type ParsedAtToken } from "./atMentions";
import { resolveAtNote } from "./notes/resolver";
import { getActiveBufferSnapshot } from "./app/state";
import { controllers } from "./app/controllers";
import { Icons } from "./icons";
import { mountToolCards, type ToolCardController } from "./chat/toolCard";
import { clamp, describeErr, formatShortDate, shortLabel } from "./chat/utils";
import { el } from "./widgets/dom";

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
    toast,
  } = opts;

  // status: chat-input-at-mentions
  // status: chat-input-at-autocomplete
  const atMentions: AtMentionsApi = mountAtMentions({
    inputEl,
    anchorEl: panelEl,
    hasEditorSelection: () => (getActiveBufferSnapshot()?.selection ?? null) !== null,
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
  };

  // Tool-call card rendering lives in `chat/toolCard.ts`; pass the
  // closures it needs so it can route touched-note clicks and re-render
  // when staging changes.
  const toolCards: ToolCardController = mountToolCards({
    transcriptEl,
    scrollToBottom: () => scrollToBottom(),
    onOpenNoteLink: (rel) => onOpenNoteLink(rel),
    onOpenStagingProposal: (p) => onOpenStagingProposal(p),
    onClearAssistantBubble: () => {
      renderState.assistantBubble = null;
    },
    getPendingStagingIds: () => pendingStagingIds,
  });

  // status: staging-accept-reject-from-chat-card
  // `bug-chat-tool-card-stale-after-cross-surface-accept` /
  // `bug-chat-tool-card-no-link-for-staged-writes`.
  //
  // Local mirror of pending staging proposal ids, driven by the shared
  // `stagingFeedCache` broadcast (single subscription + debounced fetch
  // shared across surfaces). Used to (a) re-render the Accept/Reject
  // buttons on tool cards so a cross-surface accept/reject (e.g. from
  // the activity-detail page) clears them in the chat session too, and
  // (b) decide whether the header-click on a touched-note tool card
  // routes to the staging preview or directly to the on-disk note.
  let pendingStagingIds: Set<string> = new Set();
  const stagingFeed = controllers.stagingFeedCache.get();
  stagingFeed.subscribe((proposals) => {
    pendingStagingIds = new Set(proposals.map((p) => p.id));
    toolCards.rerenderAllActionButtons();
  });
  // First-paint seed — kick a refresh so a chat session opened before any
  // staging event fires still gets a populated pending set.
  void stagingFeed.refresh();

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
    const active = getActiveBufferSnapshot();
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
    const pop = el("div", {
      class: "chat-session-menu-popover",
      attrs: { role: "menu" },
    }, [
      el("button", {
        class: "chat-session-menu-row chat-session-menu-row-new",
        html: `<span class="chat-session-menu-row-icon">+</span><span class="chat-session-menu-row-label">New session</span>`,
        attrs: { type: "button", role: "menuitem" },
        onClick: (ev) => {
          ev.stopPropagation();
          closeSessionMenu();
          void doNewSession();
        },
      }),
    ]);

    if (items.length > 0) {
      pop.appendChild(el("div", { class: "chat-session-menu-sep" }));
    }

    for (const item of items) {
      // Two stacked elements per row: the open-session button (full row)
      // + a small trash icon overlaid on the right. We can't nest a
      // <button> inside the row's <button>, so the row is a <div role=
      // "menuitem"> with an inner click target and a sibling icon.
      const date = formatShortDate(item.mtimeUnix);
      const preview = item.firstUserPreview || "(empty session)";
      pop.appendChild(el("div", {
        class: item.isActive ? "chat-session-menu-row active" : "chat-session-menu-row",
        attrs: { role: "menuitem" },
        onClick: (ev) => {
          if (ev.target instanceof Element && ev.target.closest(".chat-session-menu-row-trash")) {
            return;
          }
          ev.stopPropagation();
          closeSessionMenu();
          if (!item.isActive) {
            void doOpenSession(item.sessionId);
          }
        },
      }, [
        el("span", { class: "chat-session-menu-row-date", text: date }),
        el("span", { class: "chat-session-menu-row-label", text: preview }),
        el("span", { class: "chat-session-menu-row-count", text: String(item.turnCount) }),
        // status: chat-session-trash
        el("button", {
          class: "chat-session-menu-row-trash",
          title: "Move session to trash",
          html: Icons.trash(),
          attrs: { type: "button", "aria-label": "Move session to trash" },
          onClick: (ev) => {
            ev.stopPropagation();
            void doDeleteSession(item.sessionId, item.isActive);
          },
        }),
      ]));
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
      toolCards.clear();
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
      toolCards.clear();
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
    const body = el("div", { class: "chat-msg-body" });
    transcriptEl.appendChild(el("div", { class: "chat-msg chat-msg-assistant" }, [
      el("span", { class: "chat-msg-role", text: "Agent" }),
      body,
    ]));
    const mdView = mountChatMarkdown({
      host: body,
      onOpenNoteLink,
    });
    const entry = { body, mdView };
    renderState.assistantBubble = entry;
    return entry;
  }

  function appendUserMessage(text: string): void {
    transcriptEl.appendChild(el("div", { class: "chat-msg chat-msg-user", text }));
    scrollToBottom();
    renderState.assistantBubble = null;
  }

  function appendTextDelta(text: string): void {
    const bubble = ensureAssistantBubble();
    bubble.mdView.append(text);
    scrollToBottom();
  }

  function appendCapRow(turnId: string, completed: number): void {
    const row = el("div", { class: "chat-cap-row" });
    row.append(
      el("span", { text: `Agent has made ${completed} tool calls — ` }),
      el("button", {
        text: "Continue",
        onClick: () => {
          row.remove();
          pausedAtCap = false;
          void Ipc.chatContinue({
            sessionId: activeSessionId,
            turnId,
          });
          setBusy(true);
        },
      }),
      el("button", {
        text: "Stop",
        onClick: () => {
          row.remove();
          pausedAtCap = false;
          void Ipc.chatStop({
            sessionId: activeSessionId,
            turnId,
          });
        },
      }),
    );
    transcriptEl.appendChild(row);
    pausedAtCap = true;
    scrollToBottom();
  }

  function removeCapRows(): void {
    transcriptEl.querySelectorAll(".chat-cap-row").forEach((el) => el.remove());
  }

  function appendError(message: string): void {
    transcriptEl.appendChild(el("div", { class: "chat-msg-error", text: message }));
    scrollToBottom();
  }

  function appendSystemRow(message: string): void {
    transcriptEl.appendChild(el("div", { class: "chat-msg-system", text: message }));
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
    const dots: HTMLElement[] = [];
    for (let i = 0; i < 3; i++) {
      dots.push(el("span", { class: "chat-thinking-dot", text: "•" }));
    }
    const node = el("div", {
      class: "chat-msg chat-msg-thinking",
      attrs: { "aria-label": "Agent is thinking" },
    }, dots);
    transcriptEl.appendChild(node);
    thinkingEl = node;
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
        toolCards.appendToolCallStart(ev.call_id, ev.tool_name);
        break;
      case "tool_call_args_delta":
        toolCards.appendToolCallArgsDelta(ev.call_id, ev.args_delta);
        break;
      case "tool_call_complete":
        toolCards.appendToolCallComplete(ev.call_id, ev.args);
        break;
      case "tool_result":
        toolCards.appendToolResult(ev.call_id, ev.ok, ev.summary, ev.output);
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

  void onHikerEventAs<AgentEvent>("hiker:chat-event", (payload) => {
    handleEvent(payload);
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
  const inputResizeEl = el("div", {
    class: "chat-input-resize-handle",
    attrs: {
      role: "separator",
      "aria-orientation": "horizontal",
      "aria-label": "Resize chat input",
    },
  });
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
      toolCards.clear();
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
    toolCards.clear();
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

export type { AgentEvent, FinishReason, ActiveSessionDto };
