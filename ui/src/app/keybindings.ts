/// Window-level keybinding handlers — the "outside CM6 focus" half of
/// keybinds whose other half lives in the CM6 keymap registry. Per
/// editor.md's "Bindings only fire when the editor has DOM focus", we
/// install a window-level listener for each binding that needs to fire
/// when focus is on the tree, sidebar, status bar, etc.
///
/// Covers:
/// - settings.open chord (`Mod-,`) — global half of the registry binding.
/// - search.focusInput (`Ctrl-Space`) — every-platform global half.
/// - tab keybinds (`Mod-W` close, `Ctrl-Tab` cycle, `Mod-1..9` jump).
/// - navigation keybinds (`Mod-[` / `Mod-]`, `Alt-Left` / `Alt-Right`).
import type { NavApi } from "../navigation";

export interface WindowKeybindingsDeps {
  toggleSettings: () => unknown;
  focusSearchInput: () => void;
  closeActiveTab: () => void;
  cycleTab: (delta: 1 | -1) => void;
  jumpToTab: (n: number) => void;
  /// Forward-declared at host module scope — `nav` may be null during
  /// boot. We read it lazily inside the handler so the TDZ-safe shape
  /// in main.ts (let nav: NavApi | null) keeps working.
  getNav: () => NavApi | null;
  getActivePath: () => string | null;
}

export function installWindowKeybindings(deps: WindowKeybindingsDeps): void {
  // status: settings-pane-keybind
  // `settings.open` chord: `Mod-,` (Cmd-, on macOS, Ctrl-, elsewhere).
  // Same dual-half shape as `search-keybind-ctrl-space` — registered in
  // CM6 so it wins inside the editor, plus this window-level handler for
  // everywhere else.
  window.addEventListener(
    "keydown",
    (e) => {
      // Match Mod-,: meta on macOS, ctrl elsewhere. Skip when the editor
      // has focus (the registry-side binding handles that case) so we
      // don't double-toggle.
      if (e.key !== "," || e.altKey || e.shiftKey) return;
      const isMac = navigator.platform.toLowerCase().includes("mac");
      const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
      if (!mod) return;
      if ((e.target as HTMLElement | null)?.closest(".cm-editor")) return;
      e.preventDefault();
      void deps.toggleSettings() as unknown;
    },
    { capture: true },
  );

  // status: search-keybind-ctrl-space (global half)
  // Document-level Ctrl-Space handler — matches the spec's "every
  // platform" rule by checking ctrlKey, *not* metaKey, so Cmd-Space on
  // macOS stays Spotlight. Capture phase + preventDefault stops the
  // browser's default (and CM6's startCompletion via the registry
  // binding above when the editor has focus) before downstream handlers
  // see it.
  window.addEventListener(
    "keydown",
    (e) => {
      if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && e.code === "Space") {
        e.preventDefault();
        deps.focusSearchInput();
      }
    },
    { capture: true },
  );

  // status: editor-tab-keybinds
  // Window-level listener for the tab keybinds so they fire even when
  // focus is outside CM6 (file tree, status bar, sidebar). The CM6
  // keymap registrations above cover the editor-focus case; this
  // handler covers the rest. Skip when the user is typing into an input
  // (so Cmd-W in a textarea doesn't hijack normal close-line behavior —
  // but in Tauri there's no browser tab to close anyway, so we always
  // handle it; we only skip the tab-cycle / number keys for inputs
  // because those have meaningful in-input behavior).
  window.addEventListener(
    "keydown",
    (e) => {
      const nav = deps.getNav();
      // status: navigation-keybind
      // Alt-Left / Alt-Right (Linux/Windows browser convention) — fire
      // regardless of modifier-state of Mod, before the Mod gate below
      // since these don't require Cmd/Ctrl.
      if (e.altKey && !e.metaKey && !e.ctrlKey && !e.shiftKey) {
        if (e.key === "ArrowLeft") {
          e.preventDefault();
          void nav?.back();
          return;
        }
        if (e.key === "ArrowRight") {
          e.preventDefault();
          void nav?.forward();
          return;
        }
      }
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      const target = e.target as HTMLElement | null;
      const inInput =
        target?.tagName === "INPUT"
        || target?.tagName === "TEXTAREA"
        || target?.isContentEditable;
      // status: navigation-keybind
      // Cmd/Ctrl-[ / Cmd/Ctrl-] — back/forward when focus is outside CM6
      // (editor focus is covered by the registry-side bindings above).
      if (!e.shiftKey && !e.altKey) {
        if (e.key === "[") {
          e.preventDefault();
          void nav?.back();
          return;
        }
        if (e.key === "]") {
          e.preventDefault();
          void nav?.forward();
          return;
        }
      }
      // Cmd/Ctrl-W → close active tab. Always fires (Tauri has no browser
      // tab to close).
      if (e.key === "w" && !e.shiftKey && !e.altKey) {
        e.preventDefault();
        if (deps.getActivePath()) deps.closeActiveTab();
        return;
      }
      // Cmd/Ctrl-Tab cycle. Only when not typing — in an input the user
      // expects normal Tab behavior.
      if (e.key === "Tab" && !inInput) {
        e.preventDefault();
        deps.cycleTab(e.shiftKey ? -1 : +1);
        return;
      }
      // Cmd/Ctrl-1..9 → jump to tab. Skip in inputs.
      if (!inInput && !e.shiftKey && !e.altKey) {
        const n = parseInt(e.key, 10);
        if (n >= 1 && n <= 9) {
          e.preventDefault();
          deps.jumpToTab(n);
        }
      }
    },
    { capture: true },
  );
}
