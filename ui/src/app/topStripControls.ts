// Custom window controls (decorations: false in tauri.conf.json — the
// top strip is the title bar, so we provide our own min/max/close +
// drag-to-move). Tauri 2's `data-tauri-drag-region` attribute only
// matches the exact event target, which makes clicks on inner
// containers (vault-path span, leading-cluster wrapper, empty tab-strip
// space) fall through and not initiate a drag. A mousedown listener on
// the whole strip that excludes interactive descendants gives us the
// behavior the OS title bar used to: drag to move, double-click to
// maximize, click on a button to do its action.
//
// status: autosave-close-no-modal
// Always preventDefault and drive the close ourselves via `win.destroy()`.
// Returning without preventDefault to "let Tauri default-close" is
// unreliable (X button becomes a no-op), and `win.close()` would re-enter
// this handler — `destroy()` skips the close-requested round-trip.
//
// No dirty-buffer modal: every dirty buffer is flushed through the
// autosave pipeline and the open-tab snapshot is pushed, so next launch
// auto-restores the workspace as it was. Recovered tabs surface as dirty
// (autosaved bytes ≠ on-disk loadedText) and the user can save or revert
// then via the existing dirty-buffer affordances.

import { Logger } from "../logger";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { setBufferState, getOpenBuffers } from "./state";
import { dom } from "./dom";
import { controllers } from "./controllers";

export function installTopStripControls(): void {
  const win = getCurrentWindow();
  const { topStripEl, winMinBtn, winMaxBtn, winCloseBtn } = dom().topStrip;

  function isInteractiveTarget(t: EventTarget | null): boolean {
    if (!(t instanceof Element)) return false;
    return !!t.closest(
      "button, input, textarea, a, [role='tab'], [role='button']",
    );
  }
  topStripEl?.addEventListener("mousedown", (e) => {
    if (e.button !== 0) return;
    if (isInteractiveTarget(e.target)) return;
    e.preventDefault();
    void win.startDragging();
  });
  topStripEl?.addEventListener("dblclick", (e) => {
    if (isInteractiveTarget(e.target)) return;
    void win.toggleMaximize();
  });
  winMinBtn?.addEventListener("click", () => {
    void win.minimize();
  });
  winMaxBtn?.addEventListener("click", () => {
    void win.toggleMaximize();
  });
  winCloseBtn?.addEventListener("click", () => {
    // Routes through the same `onCloseRequested` handler below so the
    // autosave flush + tab-state snapshot run before destroy.
    void win.close();
  });
  void win.onCloseRequested(async (event) => {
    event.preventDefault();
    const autosave = controllers.autosave.get();
    try {
      await autosave.flushAllAndWait();
    } catch (err) {
      Logger.error("ui::app", "autosave flush (close) failed", { err });
    }
    try {
      await autosave.pushTabStateNow();
    } catch (err) {
      Logger.error("ui::app", "autosave_save_tab_state (close) failed", { err });
    }
    autosave.stop();
    getOpenBuffers().clear();
    setBufferState({ buffer: null, activePath: null, previewTabPath: null });
    await win.destroy();
  });
}
