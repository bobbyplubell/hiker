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

// status: multi-buffer-window-close-guard
// Multi-buffer window-close confirmation. Lists every dirty tab with
// per-tab Save / Discard radio choice plus Save All / Discard All /
// Cancel actions. Returns one of:
//   { kind: "cancel" } — user wants to keep working
//   { kind: "save-all" } — save every dirty tab in turn
//   { kind: "discard-all" } — drop all dirty state
//   { kind: "per-tab", choices: { [path]: "save" | "discard" } } — mixed
export type WindowCloseChoice =
  | { kind: "cancel" }
  | { kind: "save-all" }
  | { kind: "discard-all" }
  | { kind: "per-tab"; choices: Record<string, "save" | "discard"> };

export function confirmWindowClose(
  dirtyPaths: string[],
): Promise<WindowCloseChoice> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";

    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const msg = document.createElement("p");
    msg.className = "modal-message";
    msg.textContent =
      `${dirtyPaths.length} ${dirtyPaths.length === 1 ? "tab has" : "tabs have"} unsaved changes:`;

    const list = document.createElement("ul");
    list.className = "modal-dirty-list";
    const choices: Record<string, "save" | "discard"> = {};
    for (const p of dirtyPaths) {
      choices[p] = "save";
      const li = document.createElement("li");
      const label = document.createElement("span");
      label.className = "modal-dirty-path";
      label.textContent = p;
      li.appendChild(label);
      const saveLbl = document.createElement("label");
      const saveR = document.createElement("input");
      saveR.type = "radio";
      saveR.name = `wc-${p}`;
      saveR.checked = true;
      saveR.addEventListener("change", () => {
        if (saveR.checked) choices[p] = "save";
      });
      saveLbl.append(saveR, document.createTextNode(" Save"));
      const discardLbl = document.createElement("label");
      const discardR = document.createElement("input");
      discardR.type = "radio";
      discardR.name = `wc-${p}`;
      discardR.addEventListener("change", () => {
        if (discardR.checked) choices[p] = "discard";
      });
      discardLbl.append(discardR, document.createTextNode(" Discard"));
      li.append(saveLbl, discardLbl);
      list.appendChild(li);
    }

    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const discardAllBtn = document.createElement("button");
    discardAllBtn.className = "modal-btn modal-btn-danger";
    discardAllBtn.textContent = "Discard all";
    const saveAllBtn = document.createElement("button");
    saveAllBtn.className = "modal-btn modal-btn-primary";
    saveAllBtn.textContent = "Save all";
    const perTabBtn = document.createElement("button");
    perTabBtn.className = "modal-btn";
    perTabBtn.textContent = "Apply per-tab choices";

    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (v: WindowCloseChoice) => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        finish({ kind: "cancel" });
      }
    };
    cancelBtn.addEventListener("click", () => finish({ kind: "cancel" }));
    saveAllBtn.addEventListener("click", () => finish({ kind: "save-all" }));
    discardAllBtn.addEventListener("click", () =>
      finish({ kind: "discard-all" }),
    );
    perTabBtn.addEventListener("click", () =>
      finish({ kind: "per-tab", choices: { ...choices } }),
    );
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish({ kind: "cancel" });
    });
    document.addEventListener("keydown", onKey, true);

    btnRow.append(cancelBtn, perTabBtn, discardAllBtn, saveAllBtn);
    dialog.append(msg, list, btnRow);
    overlay.append(dialog);
    document.body.append(overlay);
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
