// Lightweight bottom toast with optional action button (used for the
// undo-after-delete affordance and other one-shot notifications).

let toastTimer: number | null = null;

export interface ToastAction {
  label: string;
  run: () => void | Promise<void>;
}

export function showToast(
  message: string,
  action?: ToastAction,
  ttlMs = 5000,
): void {
  let toast = document.getElementById("toast") as HTMLDivElement | null;
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "toast";
    document.body.appendChild(toast);
  }
  toast.innerHTML = "";
  const msgEl = document.createElement("span");
  msgEl.className = "toast-message";
  msgEl.textContent = message;
  toast.appendChild(msgEl);
  if (action) {
    const btn = document.createElement("button");
    btn.className = "toast-action";
    btn.textContent = action.label;
    btn.addEventListener("click", async () => {
      toast?.classList.remove("visible");
      if (toastTimer !== null) window.clearTimeout(toastTimer);
      await action.run();
    });
    toast.appendChild(btn);
  }
  toast.classList.add("visible");
  if (toastTimer !== null) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toast?.classList.remove("visible");
  }, ttlMs);
}
