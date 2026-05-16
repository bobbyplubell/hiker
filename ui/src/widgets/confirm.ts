// Modal confirm primitives. `confirm3` is the three-button save/discard/cancel
// shape used by the file-switch dirty guard and the save-on-conflict path;
// `confirmDanger` is the two-button cancel/destructive shape used by delete
// modals and trash-empty.

export function confirmDanger(
  message: string,
  dangerLabel: string,
): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    const msg = document.createElement("p");
    msg.className = "modal-message";
    msg.textContent = message;
    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const dangerBtn = document.createElement("button");
    dangerBtn.className = "modal-btn modal-btn-danger";
    dangerBtn.textContent = dangerLabel;
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
    cancelBtn.addEventListener("click", () => finish(false));
    dangerBtn.addEventListener("click", () => finish(true));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish(false);
    });
    document.addEventListener("keydown", onKey, true);
    btnRow.append(dangerBtn, cancelBtn);
    dialog.append(msg, btnRow);
    overlay.append(dialog);
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
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    const msg = document.createElement("p");
    msg.className = "modal-message";
    // Preserve embedded newlines so the bullet body renders as the spec's
    // multi-line layout rather than collapsing to a single paragraph.
    msg.style.whiteSpace = "pre-line";
    msg.textContent = message;
    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const confirmBtn = document.createElement("button");
    confirmBtn.className = "modal-btn modal-btn-primary";
    confirmBtn.textContent = confirmLabel;
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
    cancelBtn.addEventListener("click", () => finish(false));
    confirmBtn.addEventListener("click", () => finish(true));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish(false);
    });
    document.addEventListener("keydown", onKey, true);
    btnRow.append(confirmBtn, cancelBtn);
    dialog.append(msg, btnRow);
    overlay.append(dialog);
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
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";

    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const msg = document.createElement("p");
    msg.className = "modal-message";
    msg.textContent = message;

    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";

    const aBtn = document.createElement("button");
    aBtn.className = "modal-btn modal-btn-primary";
    aBtn.textContent = a;
    const bBtn = document.createElement("button");
    bBtn.className = "modal-btn";
    bBtn.textContent = b;
    const cBtn = document.createElement("button");
    cBtn.className = "modal-btn";
    cBtn.textContent = cancel;

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

    aBtn.addEventListener("click", () => finish("a"));
    bBtn.addEventListener("click", () => finish("b"));
    cBtn.addEventListener("click", () => finish("cancel"));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish("cancel");
    });
    document.addEventListener("keydown", onKey, true);

    btnRow.append(cBtn, bBtn, aBtn);
    dialog.append(msg, btnRow);
    overlay.append(dialog);
    document.body.append(overlay);
    aBtn.focus();
  });
}
