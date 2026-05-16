// Inline editors used by `renderTreeNode` for click-to-edit on cluster
// names and summaries. The single-line version is a plain `<input>`;
// the multiline variant swaps in a small CM6 editor so a wrapped
// multi-paragraph summary stays readable while the user edits.

import { EditorState } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { history, defaultKeymap, historyKeymap } from "@codemirror/commands";
import type { TreeRowDeps } from "./state";

export function beginInlineEdit(
  el: HTMLElement,
  initial: string,
  deps: TreeRowDeps,
  commit: (v: string) => Promise<void> | void,
): void {
  const input = document.createElement("input");
  input.type = "text";
  input.value = initial;
  input.className = "ce-inline-edit";
  const parent = el.parentElement;
  if (!parent) return;
  parent.replaceChild(input, el);
  input.focus();
  input.select();
  const finish = (save: boolean) => {
    if (save) void commit(input.value);
    else deps.repaint();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(true));
}

/// Multiline variant of `beginInlineEdit` — swaps the target element for
/// a small CM6 editor with line-wrapping + history. Used for the cluster
/// summary edit, where the single-line `<input>` swap collapses a
/// multi-paragraph summary down to a single row at click time. Save on
/// Cmd/Ctrl-Enter or blur; cancel on Escape.
export function beginInlineEditMultiline(
  el: HTMLElement,
  initial: string,
  deps: TreeRowDeps,
  commit: (v: string) => Promise<void> | void,
): void {
  const host = document.createElement("div");
  host.className = "ce-inline-edit-multiline";
  const parent = el.parentElement;
  if (!parent) return;
  // Preserve the outgoing summary's depth-based left margin so the
  // CM6 host's left edge lines up with the read-state box (and with
  // the row's name-text column above it).
  const depth = Number(el.dataset.depth ?? "0");
  host.dataset.depth = String(depth);
  host.style.marginLeft = `${depth * 14 + 16}px`;
  parent.replaceChild(host, el);

  // Re-paint cancel path needs to fire only once. We also guard against
  // the post-blur dispatch from CM6 destroying the view while the
  // commit's async dispatch is still running.
  let done = false;
  let view: EditorView | null = null;
  const finish = (save: boolean) => {
    if (done) return;
    done = true;
    const text = view ? view.state.doc.toString() : initial;
    if (view) {
      view.destroy();
      view = null;
    }
    // Unchanged text (including the blur-with-no-edit case) skips the
    // commit's refresh path, so repaint here to restore the read-state
    // summary element. Otherwise the empty CM6 host stays in the DOM
    // and collapses to a sliver.
    if (save && text !== initial) void commit(text);
    else deps.repaint();
  };

  view = new EditorView({
    parent: host,
    state: EditorState.create({
      doc: initial,
      extensions: [
        history(),
        EditorView.lineWrapping,
        keymap.of([
          {
            key: "Mod-Enter",
            run: () => {
              finish(true);
              return true;
            },
          },
          {
            key: "Escape",
            run: () => {
              finish(false);
              return true;
            },
          },
          ...defaultKeymap,
          ...historyKeymap,
        ]),
        EditorView.domEventHandlers({
          blur: () => {
            // Defer so a click inside the editor that loses focus
            // momentarily (e.g. the user dragging to select text and
            // releasing outside) doesn't terminate the edit. CM6 fires
            // blur synchronously on focus loss; a microtask is enough
            // to let any incoming click re-focus.
            setTimeout(() => finish(true), 0);
          },
        }),
      ],
    }),
  });
  view.focus();
  // Place caret at end so the user can keep typing.
  view.dispatch({ selection: { anchor: view.state.doc.length } });
}
