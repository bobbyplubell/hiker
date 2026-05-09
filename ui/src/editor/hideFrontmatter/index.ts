// status: view-hide-frontmatter-toggle
//
// Visually collapse a leading YAML frontmatter block (`---\n…\n---\n`) into
// a single placeholder line. The file is never mutated — this is a
// rendering-only fold, recomputed off `state.doc` so edits inside or around
// the block update the placeholder line count immediately.
//
// Range detection lives in `../frontmatter.ts` and is shared with the
// livePreview pass-through styling so the two CM6 surfaces can't drift.

import type { Extension } from "@codemirror/state";
import { Decoration, EditorView, WidgetType } from "@codemirror/view";
import { findFrontmatter } from "../frontmatter";

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
