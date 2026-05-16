// Modal confirm primitives. `confirm3` is the three-button save/discard/cancel
// shape used by the file-switch dirty guard and the save-on-conflict path;
// `confirmDanger` is the two-button cancel/destructive shape used by delete
// modals and trash-empty.

import { el } from "./dom";

export function confirmDanger(
  message: string,
  dangerLabel: string,
): Promise<boolean> {
  return new Promise((resolve) => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (v: boolean) => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); finish(false); }
      else if (e.key === "Enter") { e.preventDefault(); finish(false); }
    };
    const cancelBtn = el("button", {
      class: "modal-btn",
      text: "Cancel",
      onClick: () => finish(false),
    });
    const dangerBtn = el("button", {
      class: "modal-btn modal-btn-danger",
      text: dangerLabel,
      onClick: () => finish(true),
    });
    const overlay = el("div", {
      class: "modal-overlay",
      on: { mousedown: (e) => { if (e.target === overlay) finish(false); } },
    }, [
      el("div", {
        class: "modal-dialog",
        attrs: { role: "dialog", "aria-modal": "true" },
      }, [
        el("p", { class: "modal-message", text: message }),
        el("div", { class: "modal-buttons" }, [dangerBtn, cancelBtn]),
      ]),
    ]);
    document.addEventListener("keydown", onKey, true);
    document.body.append(overlay);
    cancelBtn.focus();
  });
}

// Two-button confirm with the accent treatment (not the danger red). Used
// by consequential but non-destructive flows — the embedder-model change
// modal in particular (`settings-embedder-model-change-warning`), where the
// action triggers hours of CPU work but isn't destroying data the way trash
// emptying would. Cancel default-focused per spec; Enter activates Cancel
// so a stray keypress doesn't commit. `message` may include newlines for
// the bullet list shape the spec wants.
export function confirmAccent(
  message: string,
  confirmLabel: string,
): Promise<boolean> {
  return new Promise((resolve) => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (v: boolean) => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      // Enter activates Cancel (matches Cancel default-focus posture in
      // spec — a stray keypress should never commit a re-embed).
      if (e.key === "Escape" || e.key === "Enter") {
        e.preventDefault();
        finish(false);
      }
    };
    const cancelBtn = el("button", {
      class: "modal-btn",
      text: "Cancel",
      onClick: () => finish(false),
    });
    const confirmBtn = el("button", {
      class: "modal-btn modal-btn-primary",
      text: confirmLabel,
      onClick: () => finish(true),
    });
    const overlay = el("div", {
      class: "modal-overlay",
      on: { mousedown: (e) => { if (e.target === overlay) finish(false); } },
    }, [
      el("div", {
        class: "modal-dialog",
        attrs: { role: "dialog", "aria-modal": "true" },
      }, [
        // Preserve embedded newlines so the bullet body renders as the spec's
        // multi-line layout rather than collapsing to a single paragraph.
        el("p", {
          class: "modal-message",
          text: message,
          style: { whiteSpace: "pre-line" },
        }),
        el("div", { class: "modal-buttons" }, [confirmBtn, cancelBtn]),
      ]),
    ]);
    document.addEventListener("keydown", onKey, true);
    document.body.append(overlay);
    // Cancel default-focused per spec.
    cancelBtn.focus();
  });
}

export function confirm3(
  message: string,
  a: string,
  b: string,
  cancel: string,
): Promise<"a" | "b" | "cancel"> {
  return new Promise((resolve) => {
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (choice: "a" | "b" | "cancel") => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(choice);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); finish("cancel"); }
      else if (e.key === "Enter") { e.preventDefault(); finish("a"); }
      else if (e.key === "1") { e.preventDefault(); finish("a"); }
      else if (e.key === "2") { e.preventDefault(); finish("b"); }
    };
    const aBtn = el("button", {
      class: "modal-btn modal-btn-primary",
      text: a,
      onClick: () => finish("a"),
    });
    const bBtn = el("button", {
      class: "modal-btn",
      text: b,
      onClick: () => finish("b"),
    });
    const cBtn = el("button", {
      class: "modal-btn",
      text: cancel,
      onClick: () => finish("cancel"),
    });
    const overlay = el("div", {
      class: "modal-overlay",
      on: { mousedown: (e) => { if (e.target === overlay) finish("cancel"); } },
    }, [
      el("div", {
        class: "modal-dialog",
        attrs: { role: "dialog", "aria-modal": "true" },
      }, [
        el("p", { class: "modal-message", text: message }),
        el("div", { class: "modal-buttons" }, [cBtn, bBtn, aBtn]),
      ]),
    ]);
    document.addEventListener("keydown", onKey, true);
    document.body.append(overlay);
    aBtn.focus();
  });
}
