/// View ▾ menu on the editor toolbar. Hosts display-only toggles per
/// editor.md's "View options menu" section; flips persist via the
/// shared `SettingsManager` (vault scope).
///
/// Step 4a of the main.ts refactor. Reads view-toggle state via
/// `viewSettingsStore` (the canonical source of truth — the editor
/// host's `setX(on)` setters write through to it), and mutates state
/// via `EditorHost`'s `setX` surface so the editor's compartments
/// stay in lockstep with the menu's checkmarks. The View button's
/// click handler lives in `mountModeControls` and calls into the
/// `buildItems` factory exposed here.
///
/// Reserved entries (currently the heading-breadcrumb row) appear as
/// greyed-out rows with dependency tooltips per the spec: when the
/// backing feature lands, flip its row from disabled-stub to live
/// without restructuring the menu.
import type { CtxMenuItem } from "../widgets/contextMenu";
import type { EditorHost } from "./editor";
import type { SettingsManager } from "../settings/manager";
import { viewSettingsStore } from "./state";

export interface ViewMenuDeps {
  editor: EditorHost;
  settings: SettingsManager;
  /// Sidebar / related panel toggle button sync. Called when the menu
  /// triggers a paint that may have flipped a panel-collapse class on
  /// `appEl` (currently a no-op for the View menu items, but kept for
  /// parity with the host's existing call sites).
  syncToggleButtons: () => void;
}

export interface ViewMenuApi {
  /// Returns the current menu-item list for `openContextMenu`. The
  /// `mountModeControls` View button click handler calls this on every
  /// open so checkmarks reflect the latest `viewSettingsStore` snapshot.
  buildItems: () => CtxMenuItem[];
}

export function mountViewMenu(deps: ViewMenuDeps): ViewMenuApi {
  function buildItems(): CtxMenuItem[] {
    const v = viewSettingsStore.get();
    return [
      {
        label: "Live preview",
        checked: v.livePreviewEnabled,
        run: () => {
          const on = !v.livePreviewEnabled;
          deps.editor.setLivePreviewEnabled(on);
          void deps.settings.setVaultSetting("editor.live_preview", on);
        },
      },
      {
        // status: view-show-chunk-boundaries
        label: "Show chunk boundaries",
        checked: v.chunkBoundariesEnabled,
        run: () => {
          const on = !v.chunkBoundariesEnabled;
          deps.editor.setChunkBoundariesEnabled(on);
          void deps.settings.setVaultSetting("editor.show_chunk_boundaries", on);
        },
      },
      {
        // status: view-hide-frontmatter-toggle
        label: "Hide frontmatter",
        checked: v.hideFrontmatterEnabled,
        run: () => {
          const on = !v.hideFrontmatterEnabled;
          deps.editor.setHideFrontmatter(on);
          void deps.settings.setVaultSetting("editor.hide_frontmatter", on);
        },
      },
      {
        // status: view-render-txt-as-markdown-toggle
        label: "Render .txt as markdown",
        checked: v.renderTxtAsMarkdown,
        run: () => {
          const on = !v.renderTxtAsMarkdown;
          deps.editor.setRenderTxtAsMarkdown(on);
          void deps.settings.setVaultSetting("editor.render_txt_as_markdown", on);
        },
      },
      {
        // status: view-word-wrap-toggle
        label: "Word wrap",
        checked: v.wordWrapEnabled,
        run: () => {
          const on = !v.wordWrapEnabled;
          deps.editor.setWordWrapEnabled(on);
          void deps.settings.setVaultSetting("editor.word_wrap", on);
        },
      },
      {
        label: "Show whitespace",
        checked: v.whitespaceEnabled,
        run: () => {
          const on = !v.whitespaceEnabled;
          deps.editor.setWhitespaceEnabled(on);
          void deps.settings.setVaultSetting("editor.show_whitespace", on);
        },
      },
      {
        label: "Show line numbers",
        checked: v.lineNumbersVisible,
        run: () => {
          const on = !v.lineNumbersVisible;
          deps.editor.setLineNumbersVisible(on);
          void deps.settings.setVaultSetting("editor.show_line_numbers", on);
        },
      },
      {
        // status: view-heading-breadcrumb-toggle
        label: "Show heading breadcrumb",
        checked: false,
        disabled: true,
        tooltip: "Pairs with view-show-chunk-boundaries",
      },
    ];
  }

  return { buildItems };
}
