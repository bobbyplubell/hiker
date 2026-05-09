// Chat-input `@`-mention parsing + autocomplete popover.
//
// Owns:
//   - parseAtTokens(text): pure parser, exported for testing + submit-time
//     re-parse.
//   - mountAtMentions({inputEl, ...}): wires the input element to the
//     popover, handles keyboard nav, backspace-as-unit, suppression in
//     fenced code blocks / after `\@`.
//
// status: chat-input-at-mentions
// status: chat-input-at-selection
// status: chat-input-at-note
// status: chat-input-at-autocomplete

import { invoke } from "@tauri-apps/api/core";

export type ParsedAtToken =
  | { kind: "selection"; start: number; end: number; raw: string }
  | {
      kind: "note";
      relPathNoExt: string;
      start: number;
      end: number;
      raw: string;
    };

interface AtSuggestion {
  relPath: string;
  basename: string;
  parentDir: string;
  lastAccessedAt: number | null;
}

const TOKEN_BODY_RE = /^[A-Za-z0-9/._\-]+/;

/// Parse all `@`-prefixed tokens in `text`. Honors fenced code blocks
/// (triple-backtick on a line by itself) — content inside is skipped.
/// Honors the `\@` escape — leaves the `@` literal. `@` must appear at
/// start-of-string or after whitespace; otherwise the parser ignores it
/// (matches an email address `foo@bar`, etc.).
export function parseAtTokens(text: string): ParsedAtToken[] {
  const out: ParsedAtToken[] = [];
  const fenced = computeFencedRanges(text);
  let i = 0;
  while (i < text.length) {
    if (text[i] !== "@") {
      i += 1;
      continue;
    }
    // Skip if inside a fenced code block.
    if (isInsideAny(i, fenced)) {
      i += 1;
      continue;
    }
    // Must be at start-of-string or preceded by whitespace.
    const prev = i > 0 ? text[i - 1] : "";
    if (prev !== "" && !/\s/.test(prev)) {
      i += 1;
      continue;
    }
    // Escape: `\@` suppresses parsing.
    if (i >= 1 && text[i - 1] === "\\") {
      i += 1;
      continue;
    }
    const body = TOKEN_BODY_RE.exec(text.slice(i + 1));
    if (!body) {
      i += 1;
      continue;
    }
    const start = i;
    const end = i + 1 + body[0].length;
    const raw = text.slice(start, end);
    if (body[0] === "selection") {
      out.push({ kind: "selection", start, end, raw });
    } else {
      out.push({ kind: "note", relPathNoExt: body[0], start, end, raw });
    }
    i = end;
  }
  return out;
}

/// Identify fenced ranges as `[fenceStart, fenceEndOfClosingLine)`. A
/// fence opens at any line starting with ```; the next such line closes
/// it. Unclosed fences extend to end-of-string. Tokens at any byte
/// position inside the range are suppressed — including the fence lines
/// themselves, which is fine since the spec's example "type ```code
/// @selection```" puts the `@` between fences.
function computeFencedRanges(text: string): Array<[number, number]> {
  const ranges: Array<[number, number]> = [];
  let cursor = 0;
  let openStart: number | null = null;
  while (cursor < text.length) {
    const lineEnd = text.indexOf("\n", cursor);
    const eol = lineEnd === -1 ? text.length : lineEnd;
    const line = text.slice(cursor, eol);
    // Inline triple-backtick on the same line (e.g. `` ```code @selection``` ``)
    // — treat the entire line as fenced content for parsing purposes when
    // there's a matched pair within it.
    if (openStart === null) {
      // Look for any pair of triple-backticks on this line.
      const first = line.indexOf("```");
      if (first !== -1) {
        const second = line.indexOf("```", first + 3);
        if (second !== -1) {
          // Fully fenced span on this single line.
          ranges.push([cursor + first, cursor + second + 3]);
        } else {
          // Open-only triple-backtick — block fence opens.
          openStart = cursor + first;
        }
      }
    } else {
      const close = line.indexOf("```");
      if (close !== -1) {
        ranges.push([openStart, cursor + close + 3]);
        openStart = null;
      }
    }
    if (lineEnd === -1) break;
    cursor = lineEnd + 1;
  }
  if (openStart !== null) {
    ranges.push([openStart, text.length]);
  }
  return ranges;
}

function isInsideAny(pos: number, ranges: Array<[number, number]>): boolean {
  for (const [s, e] of ranges) {
    if (pos >= s && pos < e) return true;
  }
  return false;
}

/// Active typing context — used by the popover trigger. Returns the
/// `@<partial>` range immediately preceding the caret, or null if the
/// caret isn't at the tail of an in-progress mention.
function activeMentionContext(
  text: string,
  caret: number,
): { start: number; end: number; prefix: string } | null {
  // Walk back from caret while the chars match the token-body charset.
  let i = caret;
  while (i > 0 && /[A-Za-z0-9/._\-]/.test(text[i - 1])) {
    i -= 1;
  }
  if (i === 0 || text[i - 1] !== "@") return null;
  const at = i - 1;
  // `@` must be at start-of-string or after whitespace, and not after `\`.
  const prev = at > 0 ? text[at - 1] : "";
  if (prev !== "" && !/\s/.test(prev)) return null;
  if (at >= 1 && text[at - 1] === "\\") return null;
  // Suppress inside fenced code blocks.
  if (isInsideAny(at, computeFencedRanges(text))) return null;
  return { start: at, end: caret, prefix: text.slice(at + 1, caret) };
}

export interface AtMentionsApi {
  destroy(): void;
  /// Re-parse the current input value. Used by chat.ts at submit time.
  parseTokens(): ParsedAtToken[];
  /// True when the popover is currently visible — chat.ts's Enter-to-send
  /// handler checks this and yields to the popover's own Enter handling.
  isPopoverOpen(): boolean;
}

export interface AtMentionsOptions {
  inputEl: HTMLTextAreaElement;
  /// The chat region (or any positioned ancestor) the popover anchors
  /// inside. Same element chat.ts uses for its session-menu popover.
  anchorEl: HTMLElement;
  /// True when the active editor has a non-empty selection. Drives the
  /// `@selection` row's enabled state.
  hasEditorSelection: () => boolean;
}

interface PopoverState {
  el: HTMLElement;
  rows: HTMLElement[];
  items: PopoverItem[];
  selected: number;
  ctx: { start: number; end: number };
}

type PopoverItem =
  | { kind: "selection"; enabled: boolean }
  | { kind: "note"; suggestion: AtSuggestion };

export function mountAtMentions(opts: AtMentionsOptions): AtMentionsApi {
  const { inputEl, anchorEl, hasEditorSelection } = opts;
  let popover: PopoverState | null = null;
  let suppressNextOpen = false;

  function isOpen(): boolean {
    return popover !== null;
  }

  function close(): void {
    if (popover) {
      popover.el.remove();
      popover = null;
    }
  }

  async function openOrUpdateForCaret(): Promise<void> {
    if (suppressNextOpen) {
      suppressNextOpen = false;
      return;
    }
    const caret = inputEl.selectionStart ?? 0;
    const text = inputEl.value;
    const ctx = activeMentionContext(text, caret);
    if (!ctx) {
      close();
      return;
    }
    let suggestions: AtSuggestion[] = [];
    try {
      suggestions = await invoke<AtSuggestion[]>("chat_at_autocomplete", {
        prefix: ctx.prefix,
        limit: 10,
      });
    } catch {
      // Soft fail: still show the selection row even if the autocomplete
      // backend errored (e.g. no vault open).
      suggestions = [];
    }
    // After an `await` the user may have moved the caret elsewhere. Re-check.
    const caret2 = inputEl.selectionStart ?? 0;
    const ctx2 = activeMentionContext(inputEl.value, caret2);
    if (!ctx2 || ctx2.start !== ctx.start) {
      close();
      return;
    }
    const items: PopoverItem[] = [];
    const lower = ctx2.prefix.toLowerCase();
    if (lower === "" || "selection".startsWith(lower)) {
      items.push({ kind: "selection", enabled: hasEditorSelection() });
    }
    for (const s of suggestions) {
      items.push({ kind: "note", suggestion: s });
    }
    if (items.length === 0) {
      close();
      return;
    }
    renderPopover(ctx2, items);
  }

  function renderPopover(
    ctx: { start: number; end: number },
    items: PopoverItem[],
  ): void {
    close();
    const el = document.createElement("div");
    el.className = "chat-at-popover";
    el.setAttribute("role", "listbox");
    const rows: HTMLElement[] = [];
    items.forEach((item, idx) => {
      const row = document.createElement("div");
      row.className = "chat-at-popover-row";
      row.setAttribute("role", "option");
      if (item.kind === "selection") {
        row.classList.add("chat-at-popover-row-selection");
        if (!item.enabled) row.classList.add("disabled");
        const label = document.createElement("span");
        label.className = "chat-at-popover-row-label";
        label.textContent = "selection";
        const hint = document.createElement("span");
        hint.className = "chat-at-popover-row-hint";
        hint.textContent = item.enabled
          ? "the active editor's highlighted text"
          : "Select text in the editor first.";
        row.append(label, hint);
        if (!item.enabled) row.title = "Select text in the editor first.";
      } else {
        const label = document.createElement("span");
        label.className = "chat-at-popover-row-label";
        label.textContent = item.suggestion.basename;
        const hint = document.createElement("span");
        hint.className = "chat-at-popover-row-hint";
        hint.textContent = item.suggestion.parentDir
          ? `${item.suggestion.parentDir}/`
          : "(vault root)";
        row.append(label, hint);
      }
      row.addEventListener("mousedown", (ev) => {
        ev.preventDefault();
        if (item.kind === "selection" && !item.enabled) return;
        accept(idx);
      });
      row.addEventListener("mousemove", () => {
        setSelected(idx);
      });
      el.appendChild(row);
      rows.push(row);
    });
    anchorEl.appendChild(el);
    // Position above the input's caret line (anchored against the
    // anchorEl). Simple: dock to the input's left edge, just above it.
    const inputRect = inputEl.getBoundingClientRect();
    const anchorRect = anchorEl.getBoundingClientRect();
    el.style.position = "absolute";
    el.style.left = `${inputRect.left - anchorRect.left}px`;
    el.style.bottom = `${anchorRect.bottom - inputRect.top + 4}px`;
    el.style.maxHeight = `${Math.max(120, inputRect.top - anchorRect.top - 8)}px`;
    el.style.minWidth = `${Math.min(360, inputRect.width)}px`;
    popover = { el, rows, items, selected: 0, ctx };
    setSelected(initialSelectedIdx(items));
  }

  function initialSelectedIdx(items: PopoverItem[]): number {
    // Prefer the first enabled row. If selection is disabled, jump past it.
    for (let i = 0; i < items.length; i++) {
      const it = items[i];
      if (it.kind === "selection" && !it.enabled) continue;
      return i;
    }
    return 0;
  }

  function setSelected(i: number): void {
    if (!popover) return;
    popover.selected = i;
    popover.rows.forEach((r, idx) => {
      r.classList.toggle("selected", idx === i);
    });
    const sel = popover.rows[i];
    if (sel) sel.scrollIntoView({ block: "nearest" });
  }

  function moveSelected(delta: number): void {
    if (!popover) return;
    const n = popover.items.length;
    let i = popover.selected;
    for (let step = 0; step < n; step++) {
      i = (i + delta + n) % n;
      const it = popover.items[i];
      if (it.kind === "selection" && !it.enabled) continue;
      break;
    }
    setSelected(i);
  }

  function accept(idx?: number): void {
    if (!popover) return;
    const i = idx ?? popover.selected;
    const item = popover.items[i];
    if (item.kind === "selection" && !item.enabled) return;
    const tokenText =
      item.kind === "selection" ? "@selection" : `@${item.suggestion.relPath}`;
    const before = inputEl.value.slice(0, popover.ctx.start);
    const after = inputEl.value.slice(popover.ctx.end);
    inputEl.value = `${before}${tokenText}${after}`;
    const newCaret = (before + tokenText).length;
    inputEl.selectionStart = newCaret;
    inputEl.selectionEnd = newCaret;
    close();
    suppressNextOpen = true; // the resulting `input` event shouldn't reopen.
    inputEl.dispatchEvent(new Event("input", { bubbles: true }));
  }

  // ---------- event wiring ----------

  function onKeyDown(ev: KeyboardEvent): void {
    if (popover) {
      if (ev.key === "ArrowDown") {
        ev.preventDefault();
        ev.stopImmediatePropagation();
        moveSelected(1);
        return;
      }
      if (ev.key === "ArrowUp") {
        ev.preventDefault();
        ev.stopImmediatePropagation();
        moveSelected(-1);
        return;
      }
      if (ev.key === "Enter" || ev.key === "Tab") {
        ev.preventDefault();
        ev.stopImmediatePropagation();
        accept();
        return;
      }
      if (ev.key === "Escape") {
        ev.preventDefault();
        ev.stopImmediatePropagation();
        close();
        return;
      }
    }
    // Backspace-as-unit (only when popover closed; if open, the user is
    // mid-typing the prefix and Backspace just edits the prefix).
    if (
      !popover &&
      ev.key === "Backspace" &&
      !ev.shiftKey &&
      !ev.ctrlKey &&
      !ev.metaKey &&
      !ev.altKey
    ) {
      const caret = inputEl.selectionStart ?? 0;
      const end = inputEl.selectionEnd ?? caret;
      if (caret !== end) return; // selection delete handled normally.
      const tokens = parseAtTokens(inputEl.value);
      for (const t of tokens) {
        // Caret right at token end → delete the whole token.
        if (t.end === caret && t.end - t.start > 1) {
          ev.preventDefault();
          const before = inputEl.value.slice(0, t.start);
          const after = inputEl.value.slice(t.end);
          inputEl.value = before + after;
          inputEl.selectionStart = t.start;
          inputEl.selectionEnd = t.start;
          inputEl.dispatchEvent(new Event("input", { bubbles: true }));
          return;
        }
      }
    }
  }

  function onInput(): void {
    void openOrUpdateForCaret();
  }

  function onBlur(): void {
    // Defer so a click-on-row mousedown lands before we tear down.
    setTimeout(() => {
      if (popover && document.activeElement !== inputEl) {
        // Only close if the popover itself isn't focused (it isn't —
        // rows are mousedown'd, never focused).
        close();
      }
    }, 100);
  }

  function onSelectionChange(): void {
    void openOrUpdateForCaret();
  }

  // Capture-phase keydown so we get first crack at Enter/Arrow/Tab/Escape
  // before chat.ts's normal listener turns Enter into a send.
  inputEl.addEventListener("keydown", onKeyDown, { capture: true });
  inputEl.addEventListener("input", onInput);
  inputEl.addEventListener("blur", onBlur);
  document.addEventListener("selectionchange", onSelectionChange);

  return {
    destroy(): void {
      close();
      inputEl.removeEventListener("keydown", onKeyDown, {
        capture: true,
      } as EventListenerOptions);
      inputEl.removeEventListener("input", onInput);
      inputEl.removeEventListener("blur", onBlur);
      document.removeEventListener("selectionchange", onSelectionChange);
    },
    parseTokens(): ParsedAtToken[] {
      return parseAtTokens(inputEl.value);
    },
    isPopoverOpen(): boolean {
      return isOpen();
    },
  };
}
