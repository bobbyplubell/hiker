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

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { mountChatMarkdown, type ChatMarkdownView } from "./chatMarkdownView";
import { mountAtMentions, type AtMentionsApi, type ParsedAtToken } from "./atMentions";

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

interface SessionListItem {
  sessionId: string;
  relPath: string;
  mtimeUnix: number;
  firstUserPreview: string;
  turnCount: number;
  isActive: boolean;
}

interface ChatPanel {
  setEnabled(enabled: boolean): void;
  setHeight(fraction: number): void;
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
  /// Called by the chat panel when an `hiker://note/<rel>` (or bare
  /// vault-relative) link in an agent message is clicked. The host runs
  /// the existing `openFile` machinery (which handles
  /// `file-switch-guard-dirty`).
  /// status: chat-panel-note-link-render
  onOpenNoteLink: (rel: string) => void;
  /// Returns the currently-open note's vault-relative path + buffer
  /// text, or `null` if there's no eligible buffer (preview modes
  /// excluded). Called once per `chat_send` so the active note rides
  /// as turn-scoped context.
  /// status: chat-active-note-context-injection
  getActiveNote: () => { relPath: string; bufferText: string } | null;
  /// Returns the active editor's current selection (text + source
  /// rel-path + 1-based inclusive line range), or `null` if no editor
  /// is open or the selection is empty. Called at submit time when an
  /// `@selection` token is in the input.
  /// status: chat-input-at-selection
  getActiveSelection: () =>
    | { relPath: string; text: string; lineRange: string }
    | null;
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
    onOpenNoteLink,
    getActiveNote,
    getActiveSelection,
    toast,
  } = opts;

  // status: chat-input-at-mentions
  // status: chat-input-at-autocomplete
  const atMentions: AtMentionsApi = mountAtMentions({
    inputEl,
    anchorEl: panelEl,
    hasEditorSelection: () => getActiveSelection() !== null,
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

  // ---------- send / continue / stop ----------

  function newTurnId(): string {
    return `t-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  }

  function setBusy(busy: boolean): void {
    sendBtnEl.disabled = busy || inputEl.value.trim().length === 0;
    inputEl.disabled = busy;
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
      const returnedSessionId = await invoke<string>("chat_send", {
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

    const note = getActiveNote();
    if (note) {
      blocks.push({
        kind: "activeNote",
        relPath: note.relPath,
        content: note.bufferText,
      });
      seenRelPaths.add(note.relPath);
    }

    for (const tok of tokens) {
      if (tok.kind === "selection") {
        const sel = getActiveSelection();
        if (!sel) {
          throw new Error("@selection has no selected text");
        }
        // Selection blocks are never de-duped — the slice is the point.
        blocks.push({
          kind: "selection",
          relPath: sel.relPath,
          content: sel.text,
          lineRange: sel.lineRange,
        });
      } else {
        // Resolve `@<rel-path-no-ext>` against the vault. Backend
        // probes .md / .markdown / .txt and returns the actual rel-path
        // (with extension) + body. Throws "note not found: <rel>" when
        // the path no longer resolves.
        const resolved = await invoke<{ relPath: string; content: string }>(
          "chat_resolve_at_note",
          { relNoExt: tok.relPathNoExt },
        );
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
    void send();
  });

  inputEl.addEventListener("input", () => {
    sendBtnEl.disabled = inputEl.value.trim().length === 0 || inputEl.disabled;
    autoSizeInput();
  });

  inputEl.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) {
      ev.preventDefault();
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
      items = await invoke<SessionListItem[]>("chat_session_list");
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
      trashBtn.innerHTML = `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><path d="M3 4.5h10"/><path d="M6.5 4.5V3a1 1 0 0 1 1-1h1a1 1 0 0 1 1 1v1.5"/><path d="M4.5 4.5l.6 8.2a1 1 0 0 0 1 .9h3.8a1 1 0 0 0 1-.9l.6-8.2"/></svg>`;
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
      const sid = await invoke<string>("chat_session_new");
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
      await invoke("chat_session_delete", { sessionId: id });
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
      const active = await invoke<ActiveSessionDto | null>("chat_session_open", {
        sessionId: id,
      });
      if (!active) return;
      hydrateInternal(active);
    } catch (e) {
      appendError(`Failed to open session: ${describeErr(e)}`);
    }
  }

  function autoSizeInput(): void {
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
    };
    head.addEventListener("click", () => toggleCard(els));
    head.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter" || ev.key === " ") {
        ev.preventDefault();
        toggleCard(els);
      }
    });

    renderState.toolCards.set(callId, els);
    scrollToBottom();
    renderState.assistantBubble = null;
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

  function appendToolResult(callId: string, ok: boolean, summary: string): void {
    const c = renderState.toolCards.get(callId);
    if (!c) return;
    c.result = { ok, summary };
    c.glyphEl.textContent = ok ? "✓" : "✗";
    c.resultSummaryEl.textContent = ` — ${summary}`;
    c.resultSummaryEl.classList.toggle("ok", ok);
    c.resultSummaryEl.classList.toggle("fail", !ok);
    if (c.expanded) renderExpanded(c);
    scrollToBottom();
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
      void invoke("chat_continue", {
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
      void invoke("chat_stop", {
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
        appendToolResult(ev.call_id, ev.ok, ev.summary);
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
