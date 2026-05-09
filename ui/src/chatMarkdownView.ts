// status: chat-panel-markdown-render
//
// Renders an agent message as markdown by mounting a read-only CodeMirror
// view with the same `livePreview` extension the editor uses. Reusing CM6
// keeps one rendering pipeline for markdown across the app — no extra
// dependency, no parallel sanitizer to maintain, and the live-preview
// fades work the same way users already see in the editor.

import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { syntaxTree } from "@codemirror/language";
import { markdown } from "@codemirror/lang-markdown";
import { livePreview } from "./editor/livePreview";

export interface ChatMarkdownView {
  /// Append streamed text to the end of the doc.
  append(text: string): void;
  /// Replace the entire doc (used on hydrate).
  setText(text: string): void;
  destroy(): void;
}

export interface ChatMarkdownOpts {
  host: HTMLElement;
  initialText?: string;
  /// Called when the user clicks an in-vault link inside this bubble.
  onOpenNoteLink: (rel: string) => void;
}

export function mountChatMarkdown(opts: ChatMarkdownOpts): ChatMarkdownView {
  const { host, initialText = "", onOpenNoteLink } = opts;

  const view = new EditorView({
    parent: host,
    state: EditorState.create({
      doc: initialText,
      extensions: [
        EditorView.editable.of(false),
        EditorState.readOnly.of(true),
        EditorView.lineWrapping,
        markdown(),
        livePreview(),
        EditorView.domEventHandlers({
          click: (ev, v) => handleClick(ev, v, onOpenNoteLink),
        }),
      ],
    }),
  });
  host.classList.add("chat-md-host");

  return {
    append(text) {
      const len = view.state.doc.length;
      view.dispatch({ changes: { from: len, insert: text } });
    },
    setText(text) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: text },
      });
    },
    destroy() {
      view.destroy();
    },
  };
}

function handleClick(
  ev: MouseEvent,
  view: EditorView,
  onOpenNoteLink: (rel: string) => void,
): void {
  const pos = view.posAtCoords({ x: ev.clientX, y: ev.clientY });
  if (pos == null) return;
  const tree = syntaxTree(view.state);
  let node: ReturnType<typeof tree.resolveInner> | null = tree.resolveInner(pos, 0);
  // Walk up to find an enclosing Link / URL / Autolink node.
  let target: string | null = null;
  let label: string | null = null;
  while (node) {
    const name = node.name;
    if (name === "URL" || name === "Autolink") {
      target = view.state.sliceDoc(node.from, node.to);
      // Autolinks include the surrounding `<>`; strip them.
      if (name === "Autolink") target = target.replace(/^<|>$/g, "");
      break;
    }
    if (name === "Link") {
      // Pull the URL child out of the link.
      const c = node.cursor();
      let urlFrom = -1;
      let urlTo = -1;
      let labelFrom = -1;
      let labelTo = -1;
      let sawFirstMark = false;
      if (c.firstChild()) {
        do {
          if (c.name === "URL") {
            urlFrom = c.from;
            urlTo = c.to;
          } else if (c.name === "LinkMark") {
            // First two LinkMarks bracket the label: `[`, `]`.
            if (!sawFirstMark) {
              labelFrom = c.to;
              sawFirstMark = true;
            } else if (labelTo < 0) {
              labelTo = c.from;
            }
          }
        } while (c.nextSibling());
      }
      if (urlFrom >= 0) {
        target = view.state.sliceDoc(urlFrom, urlTo);
        if (labelFrom >= 0 && labelTo >= 0) {
          label = view.state.sliceDoc(labelFrom, labelTo);
        }
        break;
      }
    }
    if (!node.parent) break;
    node = node.parent;
  }
  if (!target) return;

  if (/^(https?|file):/i.test(target)) {
    // External link — open in browser.
    ev.preventDefault();
    window.open(target, "_blank", "noreferrer,noopener");
    return;
  }
  const rel = resolveVaultRelative(target);
  if (rel === null) return;
  ev.preventDefault();
  onOpenNoteLink(rel);
  // `label` reserved for future hover/tooltip use; consumed to satisfy
  // the unused-locals linter without changing behavior.
  void label;
}

function resolveVaultRelative(target: string): string | null {
  if (target.startsWith("hiker://note/")) {
    return decodeURI(target.slice("hiker://note/".length));
  }
  if (/^[a-z][a-z0-9+.-]*:/i.test(target)) return null;
  if (target.startsWith("/")) return null;
  if (target.startsWith("#")) return null;
  return target;
}
