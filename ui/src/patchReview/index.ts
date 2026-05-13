// status: patch-review-mode
// status: patch-review-per-hunk-accept
// status: patch-review-readonly-while-active
// status: patch-review-conflicted-hunk-display
// status: patch-review-unanchored-hunk-pin
// status: patch-review-unanchored-hunk-expand
// status: patch-review-cm6-transactional
// status: patch-review-dirty-buffer-transactional-accept
//
// Per-hunk inline review for `edit_note` proposals. The CM6 view stays on
// the live on-disk file (with the user's dirty edits, if any); pending
// proposals targeting the active path render as widget decorations + per-
// hunk gutter buttons.
//
// Mode lifecycle:
//
// - `mount(view, deps)` constructs the StateField + gutter + theme and
//   wires the host's `getProposals()` poll, but only paints when a buffer
//   is in `patch-review` mode for the active file.
// - `enter(targetPath)` flips CM6 to read-only, asks the host to ensure
//   the active buffer's mode is `patch-review`, and pushes the current
//   proposal snapshot.
// - `exit()` flips CM6 back to editable and clears the decorations.
// - `setProposals(proposals)` is fired on every `hiker:staging-changed`
//   notification to rebuild the decoration set.
//
// Accept on a hunk goes through `deps.acceptHunk(proposalId, edit)` which
// owns the transactional disk + buffer apply (per
// `patch-review-dirty-buffer-transactional-accept`).

import { StateEffect, StateField, type EditorState, type Extension } from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  gutter,
  GutterMarker,
  WidgetType,
} from "@codemirror/view";
import type { EditPayload, Proposal } from "../ipc";

export interface PatchReviewDeps {
  /// CM6 dispatch — used to push proposal-snapshot effects.
  dispatch: EditorView["dispatch"];
  /// Per-hunk accept (transactional disk + buffer apply).
  acceptHunk: (proposal: Proposal) => Promise<void>;
  /// Per-hunk reject (drops the proposal).
  rejectHunk: (proposal: Proposal) => Promise<void>;
}

// CM6 state effect to push a fresh proposal snapshot into the field.
const setProposalsEffect = StateEffect.define<Proposal[]>();
// status: patch-review-unanchored-hunk-expand
// Marker effect dispatched when expansion state changes; the decoration
// state field listens for it and rebuilds so the pin widget re-renders
// with the updated expanded set.
const expansionChangedEffect = StateEffect.define<null>();

/// One non-overlapping span match of a proposal's `old_str` against the
/// current document, plus the proposal it came from.
interface HunkMatch {
  proposal: Proposal;
  from: number;
  to: number;
}

class InsertedWidget extends WidgetType {
  constructor(
    private readonly newStr: string,
    private readonly conflicted: boolean,
    private readonly reason: string | null,
  ) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return (
      other instanceof InsertedWidget &&
      other.newStr === this.newStr &&
      other.conflicted === this.conflicted &&
      other.reason === this.reason
    );
  }
  override toDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className =
      "cm-patch-review-inserted-inline" + (this.conflicted ? " conflicted" : "");
    if (this.conflicted && this.reason) {
      const warn = document.createElement("span");
      warn.className = "conflict-glyph";
      warn.textContent = "⚠";
      warn.title = `Conflict: ${this.reason}`;
      span.appendChild(warn);
    }
    span.appendChild(document.createTextNode(this.newStr));
    return span;
  }
  override ignoreEvent(): boolean {
    // Click-through to the live document under the inserted widget per
    // spec ("They are non-editable; click-through goes through to the
    // live document underneath them.").
    return true;
  }
}

/// status: patch-review-unanchored-hunk-pin
/// End-of-doc pinned block listing proposals whose `old_str` doesn't
/// match the current buffer text. Without this surface the hunks would
/// silently disappear from the inline view whenever the user's dirty
/// edits clobber the anchor; with it, they're reachable for Reject.
class UnanchoredPinWidget extends WidgetType {
  constructor(
    private readonly proposals: Proposal[],
    private readonly expanded: Set<string>,
    private readonly onReject: (p: Proposal) => void,
    private readonly onToggleExpand: (id: string) => void,
  ) {
    super();
  }
  override eq(other: WidgetType): boolean {
    if (!(other instanceof UnanchoredPinWidget)) return false;
    if (other.proposals.length !== this.proposals.length) return false;
    for (let i = 0; i < this.proposals.length; i++) {
      const a = this.proposals[i];
      const b = other.proposals[i];
      if (a.id !== b.id || a.state !== b.state) return false;
      if ((a.edit?.new_str ?? "") !== (b.edit?.new_str ?? "")) return false;
      if ((a.edit?.old_str ?? "") !== (b.edit?.old_str ?? "")) return false;
      if (this.expanded.has(a.id) !== other.expanded.has(b.id)) return false;
    }
    return true;
  }
  override toDOM(): HTMLElement {
    const block = document.createElement("div");
    block.className = "cm-patch-review-unanchored-pin";
    const header = document.createElement("div");
    header.className = "pin-header";
    header.textContent =
      `Unanchored agent edits (${this.proposals.length}) — your buffer no longer contains the text these edits target`;
    block.appendChild(header);
    for (const p of this.proposals) {
      const isExpanded = this.expanded.has(p.id);
      const row = document.createElement("div");
      row.className = "pin-row" + (isExpanded ? " expanded" : "");

      const headerRow = document.createElement("div");
      headerRow.className = "pin-row-header";
      headerRow.setAttribute("role", "button");
      headerRow.setAttribute("tabindex", "0");
      headerRow.setAttribute("aria-expanded", isExpanded ? "true" : "false");
      headerRow.title = isExpanded ? "Collapse" : "Click to view anchor and replacement";

      const chevron = document.createElement("span");
      chevron.className = "pin-chevron" + (isExpanded ? " open" : "");
      chevron.setAttribute("aria-hidden", "true");
      chevron.innerHTML =
        `<svg viewBox="0 0 16 16" width="10" height="10" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"><polyline points="5,3 10,8 5,13"/></svg>`;
      headerRow.appendChild(chevron);

      const glyph = document.createElement("span");
      glyph.className = "pin-glyph";
      glyph.textContent = "?";
      const reason = p.state === "conflicted"
        ? (p.conflict_reason ?? "conflicted")
        : "anchor_not_in_buffer";
      glyph.title = `Anchor not found in current buffer (${reason})`;
      headerRow.appendChild(glyph);

      const preview = document.createElement("span");
      preview.className = "pin-preview";
      const full = p.edit?.new_str ?? "";
      const firstLine = full.split("\n", 1)[0] ?? "";
      preview.textContent = firstLine.length > 80
        ? firstLine.slice(0, 80) + "…"
        : (firstLine || "(empty)");
      preview.title = full;
      headerRow.appendChild(preview);

      const rejectBtn = document.createElement("button");
      rejectBtn.type = "button";
      rejectBtn.className = "pin-reject";
      rejectBtn.title = "Reject this proposal";
      rejectBtn.setAttribute("aria-label", "Reject proposal");
      rejectBtn.innerHTML =
        `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/></svg>`;
      const proposal = p;
      rejectBtn.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        this.onReject(proposal);
      });
      headerRow.appendChild(rejectBtn);

      const toggle = (e: Event) => {
        e.preventDefault();
        e.stopPropagation();
        this.onToggleExpand(proposal.id);
      };
      headerRow.addEventListener("click", toggle);
      headerRow.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") toggle(e);
      });

      row.appendChild(headerRow);

      if (isExpanded) {
        const details = document.createElement("div");
        details.className = "pin-details";

        const oldStr = p.edit?.old_str ?? "";
        const newStr = p.edit?.new_str ?? "";

        const anchorLabel = document.createElement("div");
        anchorLabel.className = "pin-detail-label";
        anchorLabel.textContent = "Anchor";
        details.appendChild(anchorLabel);
        const anchorBlock = document.createElement("pre");
        anchorBlock.className = "pin-detail-block";
        if (oldStr.length === 0) {
          anchorBlock.classList.add("empty");
          anchorBlock.textContent = "(empty)";
        } else {
          anchorBlock.textContent = oldStr;
        }
        details.appendChild(anchorBlock);

        const replLabel = document.createElement("div");
        replLabel.className = "pin-detail-label";
        replLabel.textContent = "Replacement";
        details.appendChild(replLabel);
        const replBlock = document.createElement("pre");
        replBlock.className = "pin-detail-block";
        if (newStr.length === 0) {
          replBlock.classList.add("empty");
          replBlock.textContent = "(empty)";
        } else {
          replBlock.textContent = newStr;
        }
        details.appendChild(replBlock);

        row.appendChild(details);
      }

      block.appendChild(row);
    }
    return block;
  }
  override ignoreEvent(): boolean {
    // Need click events for the Reject buttons and expansion toggles.
    return false;
  }
}

class HunkGutterMarker extends GutterMarker {
  constructor(
    private readonly proposal: Proposal,
    private readonly onAccept: (p: Proposal) => void,
    private readonly onReject: (p: Proposal) => void,
  ) {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return (
      other instanceof HunkGutterMarker &&
      other.proposal.id === this.proposal.id &&
      other.proposal.state === this.proposal.state
    );
  }
  override toDOM(): HTMLElement {
    const wrap = document.createElement("span");
    wrap.className = "cm-patch-review-gutter-buttons";
    const accept = document.createElement("button");
    accept.type = "button";
    accept.className = "accept";
    accept.innerHTML =
      `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><polyline points="3,8 7,12 13,4"/></svg>`;
    const conflicted = this.proposal.state === "conflicted";
    if (conflicted) {
      accept.disabled = true;
      const reason = this.proposal.conflict_reason ?? "conflicted";
      accept.title = `Cannot accept: ${reason}`;
    } else {
      accept.title = "Accept this hunk";
    }
    accept.setAttribute("aria-label", "Accept hunk");
    accept.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (!accept.disabled) this.onAccept(this.proposal);
    });
    const reject = document.createElement("button");
    reject.type = "button";
    reject.className = "reject";
    reject.innerHTML =
      `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/></svg>`;
    reject.title = "Reject this hunk";
    reject.setAttribute("aria-label", "Reject hunk");
    reject.addEventListener("click", (e) => {
      e.preventDefault();
      e.stopPropagation();
      this.onReject(this.proposal);
    });
    wrap.appendChild(accept);
    wrap.appendChild(reject);
    return wrap;
  }
}

/// All non-overlapping byte ranges of `needle` in `haystack` (UTF-16 code
/// units, matching CM6's doc indexing). Mirrors `core::staging::find_all_matches`.
function findAllMatches(haystack: string, needle: string): Array<[number, number]> {
  if (!needle) return [];
  const out: Array<[number, number]> = [];
  let i = 0;
  while (true) {
    const idx = haystack.indexOf(needle, i);
    if (idx < 0) break;
    out.push([idx, idx + needle.length]);
    i = idx + needle.length;
  }
  return out;
}

/// status: patch-review-dirty-buffer-transactional-accept
/// Pure helper exported for host transactional-accept logic: try to apply
/// the proposal's edit to `before`. Returns the new text on success, null
/// on anchor failure (missing or non-unique without replace_all).
export function applyEditPure(before: string, edit: EditPayload): string | null {
  const matches = findAllMatches(before, edit.old_str);
  if (matches.length === 0) return null;
  if (matches.length > 1 && !edit.replace_all) return null;
  let out = "";
  let cursor = 0;
  for (const [start, end] of matches) {
    out += before.slice(cursor, start);
    out += edit.new_str;
    cursor = end;
  }
  out += before.slice(cursor);
  return out;
}

/// status: patch-review-unanchored-hunk-pin
/// Partition proposals into those that have an inline anchor in the
/// current doc (rendered as mark + widget hunks) and those whose
/// `old_str` doesn't match (rendered in the end-of-doc pin block).
function computeHunks(
  docText: string,
  proposals: Proposal[],
): { anchored: HunkMatch[]; unanchored: Proposal[] } {
  const anchored: HunkMatch[] = [];
  const unanchored: Proposal[] = [];
  for (const p of proposals) {
    if (!p.edit) continue;
    const matches = findAllMatches(docText, p.edit.old_str);
    if (matches.length === 0) {
      unanchored.push(p);
      continue;
    }
    // For replace_all, we still anchor the *visual* hunk at the first
    // match for now; the accept handler handles the full apply.
    anchored.push({ proposal: p, from: matches[0][0], to: matches[0][1] });
  }
  // Sort by document position so paints are stable.
  anchored.sort((a, b) => a.from - b.from);
  return { anchored, unanchored };
}

export interface PatchReviewApi {
  /// Replace the in-state proposal list and trigger a redraw.
  setProposals(proposals: Proposal[]): void;
  /// Convenience: pure span-anchored apply (mirrors core::staging::apply_edit).
  applyEdit(before: string, edit: EditPayload): string | null;
  /// CM6 extension to install at editor construction time. While no
  /// proposals are pushed, the extension is inert (no decorations,
  /// no gutter rows).
  extension(): Extension;
}

export function mountPatchReview(deps: PatchReviewDeps): PatchReviewApi {
  // status: patch-review-unanchored-hunk-expand
  // Per-mount expansion state for unanchored pin rows. Closure-scoped so
  // it persists across decoration rebuilds (proposal-snapshot updates,
  // doc edits) within the same review session. Entries for proposals no
  // longer present in the snapshot are pruned each rebuild.
  const expandedIds = new Set<string>();

  const proposalsField = StateField.define<Proposal[]>({
    create: () => [],
    update(value, tr) {
      for (const ef of tr.effects) {
        if (ef.is(setProposalsEffect)) return ef.value;
      }
      return value;
    },
  });

  // status: patch-review-unanchored-hunk-pin
  // Decoration source listens on both proposalsField changes and doc
  // changes so that a buffer revert lifts pinned (unanchored) hunks
  // back into the inline view automatically.
  function buildDecorations(state: EditorState, proposals: Proposal[]): DecorationSet {
    if (proposals.length === 0) return Decoration.none;
    const docText = state.doc.toString();
    const { anchored, unanchored } = computeHunks(docText, proposals);
    if (anchored.length === 0 && unanchored.length === 0) return Decoration.none;
    const ranges: Array<{ from: number; to: number; deco: Decoration }> = [];
    for (const h of anchored) {
      const conflicted = h.proposal.state === "conflicted";
      const reason = h.proposal.conflict_reason ?? null;
      ranges.push({
        from: h.from,
        to: h.to,
        deco: Decoration.mark({
          class: conflicted
            ? "cm-patch-review-removed-conflicted"
            : "cm-patch-review-removed",
        }),
      });
      ranges.push({
        from: h.to,
        to: h.to,
        deco: Decoration.widget({
          side: 1,
          block: false,
          widget: new InsertedWidget(h.proposal.edit!.new_str, conflicted, reason),
        }),
      });
    }
    if (unanchored.length > 0) {
      // Prune expansion entries for proposals no longer present.
      const liveIds = new Set(unanchored.map((p) => p.id));
      for (const id of [...expandedIds]) {
        if (!liveIds.has(id)) expandedIds.delete(id);
      }
      const end = state.doc.length;
      ranges.push({
        from: end,
        to: end,
        deco: Decoration.widget({
          side: 1,
          block: true,
          widget: new UnanchoredPinWidget(
            unanchored,
            new Set(expandedIds),
            (p) => {
              void deps.rejectHunk(p);
            },
            (id) => {
              if (expandedIds.has(id)) expandedIds.delete(id);
              else expandedIds.add(id);
              deps.dispatch({ effects: expansionChangedEffect.of(null) });
            },
          ),
        }),
      });
    }
    return Decoration.set(
      ranges
        .sort((a, b) => a.from - b.from || a.to - b.to)
        .map((r) => r.deco.range(r.from, r.to)),
      true,
    );
  }

  const decorationsField = StateField.define<DecorationSet>({
    create(state) {
      return buildDecorations(state, state.field(proposalsField));
    },
    update(value, tr) {
      let nextProposals: Proposal[] | null = null;
      let expansionChanged = false;
      for (const ef of tr.effects) {
        if (ef.is(setProposalsEffect)) nextProposals = ef.value;
        if (ef.is(expansionChangedEffect)) expansionChanged = true;
      }
      if (nextProposals !== null || tr.docChanged || expansionChanged) {
        return buildDecorations(
          tr.state,
          nextProposals ?? tr.state.field(proposalsField),
        );
      }
      return value;
    },
    provide: (f) => EditorView.decorations.from(f),
  });

  const hunkGutter = gutter({
    class: "cm-patch-review-gutter",
    lineMarker(view, line) {
      const proposals = view.state.field(proposalsField, false);
      if (!proposals || proposals.length === 0) return null;
      const docText = view.state.doc.toString();
      const { anchored } = computeHunks(docText, proposals);
      const ln = view.state.doc.lineAt(line.from).number;
      for (const h of anchored) {
        const hunkLine = view.state.doc.lineAt(h.from).number;
        if (hunkLine === ln) {
          return new HunkGutterMarker(
            h.proposal,
            (p) => {
              void deps.acceptHunk(p);
            },
            (p) => {
              void deps.rejectHunk(p);
            },
          );
        }
      }
      return null;
    },
  });

  // Install the extension via appendConfig so the host doesn't have to
  // know about a new compartment. Idempotent: the StateField / decoration
  // compute / gutter are inert when the proposals list is empty.
  deps.dispatch({
    effects: StateEffect.appendConfig.of([
      proposalsField,
      decorationsField,
      hunkGutter,
    ]),
  });

  return {
    setProposals(proposals: Proposal[]): void {
      deps.dispatch({
        effects: setProposalsEffect.of(proposals),
      });
    },
    applyEdit(before: string, edit: EditPayload): string | null {
      return applyEditPure(before, edit);
    },
    extension(): Extension {
      return [proposalsField, decorationsField, hunkGutter];
    },
  };
}
