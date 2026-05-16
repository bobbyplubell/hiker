// status: editor-tab-keybinds, navigation-keybind
// status: search-keybind-ctrl-space, chat-session-new-button
//
// Register CM6 keybinds at boot. These are the editor-scoped half of the
// dual-half bindings; the window-level half lives in `./keybindings`.

import { register, validate } from "../editor/keybinds";
import { services } from "./services";

export function registerEditorKeybinds(): void {
  register({
    id: "editor.save",
    keys: "Mod-s",
    label: "Save current buffer",
    run: () => {
      void services.save();
      return true;
    },
  });
  // status: search-keybind-ctrl-space
  // Inside the editor, this binding wins over CM6's default `Ctrl-Space →
  // startCompletion`. Outside the editor (tree, status bar, anywhere with
  // focus), the document-level keydown handler installed in
  // `installSearchFocusKeybind()` covers the global case. The keybind
  // registry doesn't currently support a `scope` field — see editor.md
  // "Bindings only fire when the editor has DOM focus" — so the global
  // half lives outside the registry until that scope refactor lands.
  register({
    id: "search.focusInput",
    keys: "Ctrl-Space",
    label: "Focus search input",
    run: () => {
      services.focusSearchInput();
      return true;
    },
  });
  // status: chat-session-new-button
  // Reserved keybind for the "New chat session" affordance. The shortcut
  // itself is bound here so power users can fire it without touching the
  // button; the button still ships the same call.
  register({
    id: "chat.new-session",
    keys: "Mod-Shift-n",
    label: "Start a new chat session",
    run: () => {
      void services.chatNewSession();
      return true;
    },
  });
  // status: editor-tab-keybinds
  // Tab close / cycle / jump. Registered in the CM6 keymap so the editor
  // case works; a window-level keydown listener (further down) covers the
  // case where focus is outside CM6 (tree, sidebar, status bar). Two
  // sinks for one set of bindings is a wart of `keybind-registry`'s
  // editor-only scope; the spec acknowledges it under "When a future
  // binding needs to fire outside the editor".
  register({
    id: "tab.close",
    keys: "Mod-w",
    label: "Close active tab",
    run: () => {
      services.closeActiveTab();
      return true;
    },
  });
  register({
    id: "tab.next",
    keys: "Ctrl-Tab",
    label: "Next tab",
    run: () => {
      services.cycleTab(+1);
      return true;
    },
  });
  register({
    id: "tab.previous",
    keys: "Ctrl-Shift-Tab",
    label: "Previous tab",
    run: () => {
      services.cycleTab(-1);
      return true;
    },
  });
  for (let i = 1; i <= 9; i++) {
    const idx = i;
    register({
      id: `tab.jump-${idx}`,
      keys: `Mod-${idx}`,
      label: `Jump to tab ${idx === 9 ? "(last)" : idx}`,
      run: () => {
        services.jumpToTab(idx);
        return true;
      },
    });
  }
  // status: navigation-keybind
  // Browser-conventional Cmd/Ctrl-[ for back, Cmd/Ctrl-] for forward.
  // Registered in CM6 so they fire when the editor has focus; a window-
  // level keydown handler further down covers tree / sidebar / status-bar
  // focus and adds the Linux/Windows-conventional Alt-Left / Alt-Right.
  register({
    id: "navigation.back",
    keys: "Mod-[",
    label: "Navigate back",
    run: () => {
      void services.navBack();
      return true;
    },
  });
  register({
    id: "navigation.forward",
    keys: "Mod-]",
    label: "Navigate forward",
    run: () => {
      void services.navForward();
      return true;
    },
  });

  // status: settings-pane-keybind
  // `settings.open` chord: `Mod-,` (Cmd-, on macOS, Ctrl-, elsewhere). Same
  // dual-half shape as `search-keybind-ctrl-space` — registered in CM6 so it
  // wins inside the editor, plus a window-level handler for everywhere else.
  register({
    id: "settings.open",
    keys: "Mod-,",
    label: "Open settings",
    run: () => {
      services.openSettingsPage();
      return true;
    },
  });

  validate();
}
