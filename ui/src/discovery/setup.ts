// Host wiring for the discovery panel. Reads singletons directly.

import { EditorView } from "@codemirror/view";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { emit as emitBusEvent } from "../events/bus";
import { mountDiscovery, type DiscoveryController } from "./index";
import { getBuffer } from "../app/state";
import { dom } from "../app/dom";
import { controllers } from "../app/controllers";
import { services } from "../app/services";

export function setupDiscovery(): DiscoveryController {
  const appEl = dom().editor.appEl;
  const d = dom().discovery;

  return mountDiscovery({
    toast: services.panelToast,
    formatErr: services.formatError,
    settings: controllers.settings.get() as never,
    openNote: (rel, opts) => services.openFile(rel, opts ?? {}).then(() => undefined),
    focusEditor: () => controllers.editorPane.get().host.focus(),
    appEl,
    inputEl: d.searchInputEl,
    clearBtn: d.searchClearBtn,
    toggleSemanticBtn: d.toggleModeSemanticBtn,
    toggleLexicalBtn: d.toggleModeLexicalBtn,
    searchSectionEl: d.searchSectionEl,
    searchListEl: d.searchListEl,
    searchCountEl: d.searchCountEl,
    searchSpinnerEl: d.searchSpinnerEl,
    relatedSectionEl: d.relatedSectionEl,
    relatedListEl: d.relatedListEl,
    relatedCountEl: d.relatedCountEl,
    onScrollToChunk: async (rel, chunkIndex) => {
      if (getBuffer()?.path !== rel) return;
      try {
        const editor = controllers.editorPane.get().host;
        const bounds = await Ipc.chunksFor({ rel });
        const target = bounds.find((b) => b.chunk_index === chunkIndex);
        if (!target) return;
        const safe = Math.min(target.char_start, editor.getDocLength());
        editor.dispatch({
          selection: { anchor: safe },
          effects: EditorView.scrollIntoView(safe, { y: "start" }),
        });
        editor.focus();
      } catch (err) {
        Logger.error("ui::app", "scroll-to-chunk failed", { err });
      }
    },
    expandPanelIfCollapsed: () => {
      const wasCollapsed = appEl.classList.contains("related-collapsed");
      if (wasCollapsed) {
        appEl.classList.remove("related-collapsed");
        void services.persistSetting("vault", "vault.related_open", true);
        controllers.sidebarMode.get().syncToggleButtons();
        emitBusEvent("sidebar-toggled", { open: true });
      }
      return wasCollapsed;
    },
  });
}
