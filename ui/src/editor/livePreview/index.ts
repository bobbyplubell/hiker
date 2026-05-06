// status: live-preview-tier1-scope
//
// Tier-1 markdown live preview. Walks @codemirror/lang-markdown's syntax tree
// and emits decorations that style inline elements while fading their literal
// markup characters when the cursor is on a different line.
//
// See docs/live-preview.md for the full spec; the slug above maps to the
// scope-level decision (Tier 1 only — no widgets, no media, no math).

import { syntaxTree } from "@codemirror/language";
import {
  Decoration,
  EditorView,
  ViewPlugin,
  type DecorationSet,
  type ViewUpdate,
} from "@codemirror/view";
import type { Extension } from "@codemirror/state";
import type { Text } from "@codemirror/state";

interface ActiveSet {
  // status: live-preview-cursor-line-reveal
  // status: live-preview-selection-reveal-all
  // Lines covered by any selection range (collapsed cursor counts as one line);
  // selection ranges separately tracked so we can apply the "any marker
  // intersecting a non-empty selection reveals" rule.
  lines: Set<number>;
  ranges: { from: number; to: number }[];
}

function computeActive(view: EditorView): ActiveSet {
  const lines = new Set<number>();
  const ranges: { from: number; to: number }[] = [];
  const doc = view.state.doc;
  for (const r of view.state.selection.ranges) {
    const fromLine = doc.lineAt(r.from).number;
    const toLine = doc.lineAt(r.to).number;
    for (let n = fromLine; n <= toLine; n++) lines.add(n);
    if (!r.empty) ranges.push({ from: r.from, to: r.to });
  }
  return { lines, ranges };
}

function isRangeActive(from: number, to: number, doc: Text, active: ActiveSet): boolean {
  const fromLine = doc.lineAt(from).number;
  const toLine = doc.lineAt(to).number;
  for (let n = fromLine; n <= toLine; n++) if (active.lines.has(n)) return true;
  for (const r of active.ranges) {
    if (r.from < to && r.to > from) return true;
  }
  return false;
}

interface DecoEntry {
  from: number;
  to: number;
  deco: Decoration;
}

const fadeMark = Decoration.mark({ class: "cm-lp-fade" });

function buildDecorations(view: EditorView): DecorationSet {
  const out: DecoEntry[] = [];
  const tree = syntaxTree(view.state);
  const active = computeActive(view);
  const doc = view.state.doc;

  // Frontmatter. status: live-preview-frontmatter-passthrough
  // lang-markdown's default parser doesn't emit a FrontMatter node, so we
  // detect the leading `---`/`---` (or `...`) block ourselves and apply a
  // line decoration. Spec: styled-but-plain block, no marker fading, no kv
  // parsing. Cheaper than wiring a custom MarkdownConfig.
  if (doc.length > 0 && doc.line(1).text === "---") {
    let closeLine = -1;
    const limit = Math.min(doc.lines, 200);
    for (let n = 2; n <= limit; n++) {
      const t = doc.line(n).text;
      if (t === "---" || t === "...") {
        closeLine = n;
        break;
      }
    }
    if (closeLine > 0) {
      for (let n = 1; n <= closeLine; n++) {
        const ln = doc.line(n);
        out.push({
          from: ln.from,
          to: ln.from,
          deco: Decoration.line({ class: "cm-lp-frontmatter" }),
        });
      }
    }
  }

  for (const { from: vFrom, to: vTo } of view.visibleRanges) {
    tree.iterate({
      from: vFrom,
      to: vTo,
      enter: (node) => {
        const name = node.name;

        // Headings (ATX). status: live-preview-heading-style-fade-marker
        const headingMatch = /^ATXHeading([1-6])$/.exec(name);
        if (headingMatch) {
          const level = headingMatch[1];
          const line = doc.lineAt(node.from);
          out.push({
            from: line.from,
            to: line.from,
            deco: Decoration.line({ class: `cm-lp-h cm-lp-h${level}` }),
          });
          if (!isRangeActive(node.from, node.to, doc, active)) {
            const headerMark = node.node.getChild("HeaderMark");
            if (headerMark) {
              let hideTo = headerMark.to;
              if (doc.sliceString(hideTo, hideTo + 1) === " ") hideTo += 1;
              out.push({ from: headerMark.from, to: hideTo, deco: fadeMark });
            }
          }
          return;
        }

        // Inline emphasis / code / strikethrough.
        // status: live-preview-marker-fade-inline
        // Bold+italic (`***x***`) composes safely: lang-markdown nests
        // Emphasis inside StrongEmphasis with disjoint EmphasisMark ranges
        // (outer `**` and inner `*` cover different chars), so the two fade
        // spans never overlap and opacity 0.35 does not compound. Outer
        // `cm-lp-strong` and inner `cm-lp-em` styling marks overlap on the
        // text and layer bold + italic as the spec requires.
        if (
          name === "StrongEmphasis" ||
          name === "Emphasis" ||
          name === "Strikethrough" ||
          name === "InlineCode"
        ) {
          let cls = "cm-lp-strong";
          if (name === "Emphasis") cls = "cm-lp-em";
          else if (name === "Strikethrough") cls = "cm-lp-strike";
          else if (name === "InlineCode") cls = "cm-lp-inline-code";
          out.push({ from: node.from, to: node.to, deco: Decoration.mark({ class: cls }) });
          if (!isRangeActive(node.from, node.to, doc, active)) {
            const c = node.node.cursor();
            if (c.firstChild()) {
              do {
                const cn = c.name;
                if (cn === "EmphasisMark" || cn === "StrikethroughMark" || cn === "CodeMark") {
                  out.push({ from: c.from, to: c.to, deco: fadeMark });
                }
              } while (c.nextSibling());
            }
          }
          return;
        }

        // Links. status: live-preview-link-url-fade
        if (name === "Link") {
          const marks: { from: number; to: number }[] = [];
          let urlFrom = -1;
          let urlTo = -1;
          const c = node.node.cursor();
          if (c.firstChild()) {
            do {
              if (c.name === "LinkMark") marks.push({ from: c.from, to: c.to });
              else if (c.name === "URL") {
                urlFrom = c.from;
                urlTo = c.to;
              }
            } while (c.nextSibling());
          }
          if (marks.length >= 2) {
            out.push({
              from: marks[0].to,
              to: marks[1].from,
              deco: Decoration.mark({ class: "cm-lp-link" }),
            });
          }
          if (!isRangeActive(node.from, node.to, doc, active)) {
            for (const m of marks) {
              out.push({ from: m.from, to: m.to, deco: fadeMark });
            }
            if (marks.length >= 4) {
              // Fade everything between `]` and `)` inclusive — covers the URL
              // node *and* the literal `(` / `)` parens, since URL doesn't span
              // the parens itself.
              out.push({ from: marks[2].from, to: marks[3].to, deco: fadeMark });
            } else if (urlFrom >= 0) {
              out.push({ from: urlFrom, to: urlTo, deco: fadeMark });
            }
          }
          return;
        }

        // Code fences. status: live-preview-code-fence-block-reveal
        if (name === "FencedCode") {
          const fromLine = doc.lineAt(node.from).number;
          const toLine = doc.lineAt(node.to).number;
          for (let n = fromLine; n <= toLine; n++) {
            const ln = doc.line(n);
            out.push({
              from: ln.from,
              to: ln.from,
              deco: Decoration.line({ class: "cm-lp-fence-line" }),
            });
          }
          // Per-block reveal: cursor anywhere inside the block keeps the
          // fences visible. isRangeActive handles both line-membership and
          // selection-overlap against the whole block range.
          if (!isRangeActive(node.from, node.to, doc, active)) {
            const c = node.node.cursor();
            if (c.firstChild()) {
              do {
                if (c.name === "CodeMark" || c.name === "CodeInfo") {
                  out.push({ from: c.from, to: c.to, deco: fadeMark });
                }
              } while (c.nextSibling());
            }
          }
          return;
        }
        // Blockquote / list markers intentionally untouched.
        // status: live-preview-block-markers-keep
      },
    });
  }

  return Decoration.set(
    out.map((e) => e.deco.range(e.from, e.to)),
    true,
  );
}

const livePreviewPlugin = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    constructor(view: EditorView) {
      this.decorations = buildDecorations(view);
    }
    update(update: ViewUpdate) {
      if (update.docChanged || update.viewportChanged || update.selectionSet) {
        this.decorations = buildDecorations(update.view);
      }
    }
  },
  { decorations: (v) => v.decorations },
);

const livePreviewTheme = EditorView.baseTheme({
  ".cm-lp-fade": { opacity: "0.35" },
  ".cm-lp-strong": { fontWeight: "bold" },
  ".cm-lp-em": { fontStyle: "italic" },
  ".cm-lp-strike": { textDecoration: "line-through" },
  ".cm-lp-inline-code": {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: "0.9em",
    backgroundColor: "rgba(127, 127, 127, 0.15)",
    borderRadius: "3px",
    padding: "0 2px",
  },
  ".cm-lp-link": {
    color: "#3b82f6",
    textDecoration: "underline",
  },
  "&dark .cm-lp-link": { color: "#60a5fa" },
  ".cm-lp-h": { fontWeight: "bold", lineHeight: "1.25" },
  ".cm-lp-h1": { fontSize: "1.6em" },
  ".cm-lp-h2": { fontSize: "1.4em" },
  ".cm-lp-h3": { fontSize: "1.2em" },
  ".cm-lp-h4": { fontSize: "1.1em" },
  ".cm-lp-h5": { fontSize: "1.05em" },
  ".cm-lp-h6": { fontSize: "1em", opacity: "0.8" },
  ".cm-lp-fence-line": {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: "0.9em",
    backgroundColor: "rgba(127, 127, 127, 0.08)",
  },
  ".cm-lp-frontmatter": {
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, monospace",
    fontSize: "0.9em",
    opacity: "0.65",
  },
});

export function livePreview(): Extension {
  // status: live-preview-built-on-lang-markdown
  return [livePreviewPlugin, livePreviewTheme];
}
