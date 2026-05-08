// status: view-hide-frontmatter-toggle
//
// Visually collapse a leading YAML frontmatter block (`---\n…\n---\n`) into
// a single placeholder line. The file is never mutated — this is a
// rendering-only fold, recomputed off `state.doc` so edits inside or around
// the block update the placeholder line count immediately.
//
// Detection mirrors `core::frontmatter::split`: the block must start at
// byte 0 with `---\n` (line 1 is exactly `---`) and have a closing `---`
// line before any body content. An unterminated frontmatter block is a
// no-op so users editing one mid-flight can still see what they're typing.

import type { Extension } from "@codemirror/state";
import { Decoration, EditorView, WidgetType } from "@codemirror/view";

class FrontmatterPlaceholder extends WidgetType {
  constructor(public readonly lineCount: number) {
    super();
  }
  override eq(other: WidgetType): boolean {
    return other instanceof FrontmatterPlaceholder && other.lineCount === this.lineCount;
  }
  override toDOM(): HTMLElement {
    const el = document.createElement("div");
    el.className = "cm-frontmatter-folded";
    const n = this.lineCount;
    el.textContent = `▸ frontmatter (${n} line${n === 1 ? "" : "s"})`;
    return el;
  }
  override ignoreEvent(): boolean {
    return false;
  }
}

interface FrontmatterRange {
  from: number;
  to: number;
  lineCount: number;
}

function findFrontmatter(doc: import("@codemirror/state").Text): FrontmatterRange | null {
  if (doc.lines < 2) return null;
  if (doc.line(1).text !== "---") return null;
  // Walk forward looking for a closing `---` line. Stop searching at a
  // reasonable cap so a runaway file (a `---` at byte 0 with no closer for
  // 10k lines) doesn't pin a frame walking the whole doc — matches the
  // chunker's posture of bailing on unterminated frontmatter.
  const cap = Math.min(doc.lines, 1000);
  for (let n = 2; n <= cap; n++) {
    if (doc.line(n).text === "---") {
      const close = doc.line(n);
      // Replace through the newline after the closing `---` so the body
      // starts at the next line cleanly. `close.to` is the position before
      // the newline; +1 includes it (clamped to doc end).
      const to = Math.min(doc.length, close.to + 1);
      return { from: 0, to, lineCount: n };
    }
  }
  return null;
}

const frontmatterDecorations = EditorView.decorations.compute(
  ["doc"],
  (state) => {
    const fm = findFrontmatter(state.doc);
    if (!fm) return Decoration.none;
    return Decoration.set([
      Decoration.replace({
        widget: new FrontmatterPlaceholder(fm.lineCount),
        block: true,
      }).range(fm.from, fm.to),
    ]);
  },
);

const frontmatterTheme = EditorView.baseTheme({
  ".cm-frontmatter-folded": {
    color: "#888",
    fontStyle: "italic",
    fontSize: "0.85em",
    padding: "2px 8px",
    borderLeft: "2px solid #ccc",
    backgroundColor: "rgba(0, 0, 0, 0.03)",
    cursor: "default",
    userSelect: "none",
  },
});

export function hideFrontmatter(): Extension {
  return [frontmatterDecorations, frontmatterTheme];
}
