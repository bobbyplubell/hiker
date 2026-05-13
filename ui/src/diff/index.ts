// status: diff-renderer
//
// Read-only rendering primitive that paints a line-level diff into a CM6
// `EditorView` the consumer already owns. Computation lives in `core::diff`
// (Rust, similar crate); this module is presentation only.
//
// The diff is *not* its own pane state. Each consumer (snapshot preview
// today; mutation accept/decline and agent-write staging when those land)
// hosts the diff inline: the same CM6 view toggles between the consumer's
// primary content and the diff rendering. See docs/diff.md.

import { Ipc, type DiffResult, type DiffLine } from "../ipc";
import { Compartment, type Extension, RangeSetBuilder } from "@codemirror/state";
import { EditorView, Decoration, type DecorationSet } from "@codemirror/view";

// status: diff-viewer-input-shape
export interface DiffInput {
  before: { label: string; content: string; meta?: Record<string, unknown> };
  after: { label: string; content: string; meta?: Record<string, unknown> };
}

export interface RenderDiffOpts {
  /// When true, request char-level intraline spans from `compute_diff` and
  /// paint `.cm-diff-add-intra` / `.cm-diff-del-intra` mark decorations on
  /// top of the line backgrounds for paired delete/insert lines. Default
  /// false → existing line-level rendering, identical to pre-intraline.
  // status: view-intraline-diff-toggle
  intraline?: boolean;
}

// `DiffResult` / `DiffHunk` / `DiffLine` mirror `core::diff::DiffResult`;
// they live in `../ipc` so every caller of `compute_diff` sees the same
// shape.

const diffTheme = EditorView.baseTheme({
  ".cm-diff-add": { backgroundColor: "var(--diff-add-fill)" },
  ".cm-diff-del": { backgroundColor: "var(--diff-del-fill)" },
  ".cm-diff-add-intra": { backgroundColor: "var(--diff-add-intra-fill)" },
  ".cm-diff-del-intra": {
    backgroundColor: "var(--diff-del-intra-fill)",
    textDecoration: "line-through",
  },
  ".cm-diff-sep": {
    backgroundColor: "var(--bg-panel-elevated-strong)",
    borderTop: "1px solid var(--diff-sep-border)",
    borderBottom: "1px solid var(--diff-sep-border)",
    color: "var(--text-dim)",
    fontStyle: "italic",
  },
});

// Compartment held on the consumer's view so `renderDiff` / `clearDiff` can
// reconfigure decorations without touching any other extension.
const diffDecoCompartment = new Compartment();

// Per-view memo of the last `DiffInput` so `rerenderActiveDiff` can recompute
// with a fresh intraline flag when the View toggle flips. Dropped on
// `clearDiff` / `resetDiffDecorations` so a stale toggle press after the
// consumer exited the diff is a no-op.
const lastInputs: WeakMap<EditorView, DiffInput> = new WeakMap();

/// Extensions the host CM6 view must include so `renderDiff` / `clearDiff`
/// have a reachable compartment + theme. Spread this into the consumer's
/// `EditorState.create({ extensions: [...] })` once at view construction.
export function diffExtensions(): Extension[] {
  return [
    diffDecoCompartment.of(EditorView.decorations.of(Decoration.none)),
    diffTheme,
  ];
}

// status: diff-viewer-line-unified
// Render the hunk list as a single doc with `⋯` separator lines between
// non-adjacent hunks; collect line decorations keyed by 1-based line index
// so the dispatch below can apply them in one pass.
//
// Intraline rendering collapses a paired Delete-then-Insert into a single
// composite doc line: equal spans render once, delete spans show the
// removed text struck through in red, insert spans show the added text in
// green. This is the patch-review inline shape — a true intraline view —
// rather than stacking the two source lines and tinting each in full.
interface InlineSegment {
  /// UTF-16 indices into the composite line's text.
  from: number;
  to: number;
  cls: "cm-diff-del-intra" | "cm-diff-add-intra";
}
type Row =
  | { kind: "ctx" | "sep" }
  | { kind: "add" | "del"; line: DiffLine } // line-level (no intraline spans)
  | { kind: "intra"; segments: InlineSegment[] };

interface RenderedDiff {
  text: string;
  rows: Row[];
}

function sliceUtf8Range(text: string, byteStart: number, byteEnd: number): string {
  const fromUtf16 = utf8ByteToUtf16Index(text, byteStart);
  const toUtf16 = utf8ByteToUtf16Index(text, byteEnd);
  return text.slice(fromUtf16, toUtf16);
}

function buildCompositeRow(del: DiffLine, ins: DiffLine): { text: string; row: Row } {
  // Both paired lines carry the same span list (per `compute_with_intraline`),
  // so walk either side's spans and interleave the appropriate source text.
  const spans = del.intraline_spans ?? ins.intraline_spans ?? [];
  let composite = "";
  const segments: InlineSegment[] = [];
  for (const s of spans) {
    let text: string;
    let cls: InlineSegment["cls"] | null = null;
    if (s.op === "equal") {
      text = sliceUtf8Range(ins.line, s.byte_start_after, s.byte_end_after);
    } else if (s.op === "delete") {
      text = sliceUtf8Range(del.line, s.byte_start_before, s.byte_end_before);
      cls = "cm-diff-del-intra";
    } else {
      text = sliceUtf8Range(ins.line, s.byte_start_after, s.byte_end_after);
      cls = "cm-diff-add-intra";
    }
    if (text.length === 0) continue;
    const from = composite.length;
    composite += text;
    if (cls) segments.push({ from, to: composite.length, cls });
  }
  return { text: composite, row: { kind: "intra", segments } };
}

function renderDiffLines(result: DiffResult): RenderedDiff {
  const text: string[] = [];
  const rows: Row[] = [];
  result.hunks.forEach((h, hi) => {
    if (hi > 0) {
      text.push("⋯");
      rows.push({ kind: "sep" });
    }
    for (let i = 0; i < h.lines.length; i++) {
      const l = h.lines[i];
      const next = h.lines[i + 1];
      // Paired Delete-then-Insert with intraline spans → one composite row.
      if (
        l.op === "delete" &&
        next?.op === "insert" &&
        l.intraline_spans &&
        next.intraline_spans
      ) {
        const built = buildCompositeRow(l, next);
        text.push(built.text);
        rows.push(built.row);
        i++; // skip the matching Insert
        continue;
      }
      text.push(l.line);
      if (l.op === "insert") rows.push({ kind: "add", line: l });
      else if (l.op === "delete") rows.push({ kind: "del", line: l });
      else rows.push({ kind: "ctx" });
    }
  });
  if (text.length === 0) {
    text.push("(no differences)");
    rows.push({ kind: "ctx" });
  }
  return { text: text.join("\n"), rows };
}

function utf8ByteToUtf16Index(s: string, byteOffset: number): number {
  if (byteOffset <= 0) return 0;
  let bytes = 0;
  for (let i = 0; i < s.length; i++) {
    if (bytes >= byteOffset) return i;
    const code = s.charCodeAt(i);
    if (code < 0x80) bytes += 1;
    else if (code < 0x800) bytes += 2;
    else if (code >= 0xd800 && code <= 0xdbff) {
      bytes += 4;
      i++; // surrogate pair = one code point, two UTF-16 units
    } else bytes += 3;
  }
  return s.length;
}

function decorationsFor(view: EditorView, rows: Row[]): DecorationSet {
  const doc = view.state.doc;
  const total = Math.min(rows.length, doc.lines);
  const builder = new RangeSetBuilder<Decoration>();
  for (let i = 0; i < total; i++) {
    const row = rows[i];
    const line = doc.line(i + 1);
    if (row.kind === "sep") {
      builder.add(line.from, line.from, Decoration.line({ class: "cm-diff-sep" }));
    } else if (row.kind === "add") {
      builder.add(line.from, line.from, Decoration.line({ class: "cm-diff-add" }));
    } else if (row.kind === "del") {
      builder.add(line.from, line.from, Decoration.line({ class: "cm-diff-del" }));
    } else if (row.kind === "intra") {
      // status: diff-intraline-render-marks — composite line: no full-line
      // background, only the differing spans get coloured. Delete spans
      // show the removed text struck through; insert spans the new text in
      // green; equal spans render as plain text.
      for (const seg of row.segments) {
        if (seg.to <= seg.from) continue;
        builder.add(
          line.from + seg.from,
          line.from + seg.to,
          Decoration.mark({ class: seg.cls }),
        );
      }
    }
    // `ctx` rows take no decoration.
  }
  return builder.finish();
}

function formatErr(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object") {
    const e = err as { message?: unknown };
    if (typeof e.message === "string") return e.message;
    try { return JSON.stringify(err); } catch { /* fall through */ }
  }
  return String(err);
}

/// Compute the diff for `input` (round-trips through `compute_diff` so the
/// `similar` Myers algorithm runs in Rust per `diff-core-module`) and paint
/// it into the host `view` — replaces the doc with the unified diff text
/// and reconfigures the decoration compartment with the per-line classes.
///
/// The host view must include `diffExtensions()` in its extension list, and
/// the host should set its read-only state appropriately *before* calling.
export async function renderDiff(
  view: EditorView,
  input: DiffInput,
  opts: RenderDiffOpts = {},
): Promise<void> {
  let result: DiffResult;
  try {
    result = await Ipc.computeDiff({
      before: input.before.content,
      after: input.after.content,
      intraline: opts.intraline === true,
    });
  } catch (err) {
    console.error("compute_diff failed:", err);
    alert(`diff failed: ${formatErr(err)}`);
    return;
  }
  // Remember the input so a View-menu toggle flip can re-render with the
  // updated intraline flag without the consumer re-supplying the buffers.
  lastInputs.set(view, input);
  const { text, rows } = renderDiffLines(result);
  // Replace doc first, *then* reconfigure decorations against the new doc.
  // Computing decorations off the pre-dispatch doc would size them against
  // the previous buffer and they'd silently render as nothing.
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: text },
  });
  view.dispatch({
    effects: diffDecoCompartment.reconfigure(
      EditorView.decorations.of(decorationsFor(view, rows)),
    ),
  });
}

/// Restore the host `view` from diff to plain content: replaces the doc
/// with `plainText` and clears the diff decoration compartment. Pair with
/// `renderDiff` for an in-place toggle.
export function clearDiff(view: EditorView, plainText: string): void {
  lastInputs.delete(view);
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: plainText },
  });
  view.dispatch({
    effects: diffDecoCompartment.reconfigure(
      EditorView.decorations.of(Decoration.none),
    ),
  });
}

/// Drop any diff decorations without touching the doc. Useful when the
/// host is about to swap to a different content surface entirely (e.g.
/// exiting the snapshot preview) and just wants the diff classes gone.
export function resetDiffDecorations(view: EditorView): void {
  lastInputs.delete(view);
  view.dispatch({
    effects: diffDecoCompartment.reconfigure(
      EditorView.decorations.of(Decoration.none),
    ),
  });
}

/// Re-run `renderDiff` against the last input painted into `view`. No-op
/// when no diff is currently rendered (consumer never called `renderDiff`,
/// or has since called `clearDiff` / `resetDiffDecorations`). Used by the
/// View-menu intraline toggle to recompute the live diff without the
/// consumer re-supplying its `before` / `after` buffers.
// status: view-intraline-diff-toggle
export async function rerenderActiveDiff(
  view: EditorView,
  opts: RenderDiffOpts = {},
): Promise<void> {
  const input = lastInputs.get(view);
  if (!input) return;
  await renderDiff(view, input, opts);
}
