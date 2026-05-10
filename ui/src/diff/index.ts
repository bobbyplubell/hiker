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

import { Ipc, type DiffResult } from "../ipc";
import { Compartment, type Extension } from "@codemirror/state";
import { EditorView, Decoration, type DecorationSet } from "@codemirror/view";

// status: diff-viewer-input-shape
export interface DiffInput {
  before: { label: string; content: string; meta?: Record<string, unknown> };
  after: { label: string; content: string; meta?: Record<string, unknown> };
}

// `DiffResult` / `DiffHunk` / `DiffLine` mirror `core::diff::DiffResult`;
// they live in `../ipc` so every caller of `compute_diff` sees the same
// shape.

const diffTheme = EditorView.baseTheme({
  ".cm-diff-add": { backgroundColor: "var(--diff-add-fill)" },
  ".cm-diff-del": { backgroundColor: "var(--diff-del-fill)" },
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
interface RenderedDiff {
  text: string;
  kinds: ("ctx" | "add" | "del" | "sep")[];
}

function renderDiffLines(result: DiffResult): RenderedDiff {
  const lines: string[] = [];
  const kinds: RenderedDiff["kinds"] = [];
  result.hunks.forEach((h, i) => {
    if (i > 0) {
      lines.push("⋯");
      kinds.push("sep");
    }
    for (const l of h.lines) {
      lines.push(l.line);
      kinds.push(l.op === "insert" ? "add" : l.op === "delete" ? "del" : "ctx");
    }
  });
  if (lines.length === 0) {
    lines.push("(no differences)");
    kinds.push("ctx");
  }
  return { text: lines.join("\n"), kinds };
}

function decorationsFor(view: EditorView, kinds: RenderedDiff["kinds"]): DecorationSet {
  const builder: { from: number; deco: Decoration }[] = [];
  const doc = view.state.doc;
  const total = Math.min(kinds.length, doc.lines);
  for (let i = 0; i < total; i++) {
    const kind = kinds[i];
    if (kind === "ctx") continue;
    const cls =
      kind === "add" ? "cm-diff-add" : kind === "del" ? "cm-diff-del" : "cm-diff-sep";
    const line = doc.line(i + 1);
    builder.push({ from: line.from, deco: Decoration.line({ class: cls }) });
  }
  return Decoration.set(
    builder.map((b) => b.deco.range(b.from)),
    true,
  );
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
export async function renderDiff(view: EditorView, input: DiffInput): Promise<void> {
  let result: DiffResult;
  try {
    result = await Ipc.computeDiff({
      before: input.before.content,
      after: input.after.content,
    });
  } catch (err) {
    console.error("compute_diff failed:", err);
    alert(`diff failed: ${formatErr(err)}`);
    return;
  }
  const { text, kinds } = renderDiffLines(result);
  // Replace doc first, *then* reconfigure decorations against the new doc.
  // Computing decorations off the pre-dispatch doc would size them against
  // the previous buffer and they'd silently render as nothing.
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: text },
  });
  view.dispatch({
    effects: diffDecoCompartment.reconfigure(
      EditorView.decorations.of(decorationsFor(view, kinds)),
    ),
  });
}

/// Restore the host `view` from diff to plain content: replaces the doc
/// with `plainText` and clears the diff decoration compartment. Pair with
/// `renderDiff` for an in-place toggle.
export function clearDiff(view: EditorView, plainText: string): void {
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
  view.dispatch({
    effects: diffDecoCompartment.reconfigure(
      EditorView.decorations.of(Decoration.none),
    ),
  });
}
