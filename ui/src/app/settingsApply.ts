/// Pure mapping from a freshly loaded `Settings` snapshot to the list of
/// UI mutations that mirror it across every surface that reflects a
/// setting (View menu, tree sort, panel collapse states, chat panel
/// height/enabled, search modes/sections). Hosted by `main.ts`, called
/// on vault open and again whenever the settings pane writes through
/// `set_setting` / `reload_config`.
///
/// Module is intentionally a thin orchestration shell: it owns no state,
/// no DOM lookups, no IPC. The host wires the per-surface mutators in via
/// `deps` so the same set of "this is how a setting becomes a UI change"
/// statements live in one place.
import { viewSettingsStore } from "./state";
import { sortOrderFromSettings } from "../tree";

// Mirror of `core::config::Config` for the frontend. Returned by
// `get_settings` on vault open; consumed to seed View menu / tree state /
// panel state defaults. Field shapes match the Rust serde output.
export interface Settings {
  schema_version: number;
  editor: {
    render_txt_as_markdown: boolean;
    live_preview: boolean;
    word_wrap: boolean;
    show_line_numbers: boolean;
    show_whitespace: boolean;
    show_chunk_boundaries: boolean;
    hide_frontmatter: boolean;
    tab_size: number;
  };
  indexing: {
    model: string;
    batch_size: number;
    ignored_paths: string[];
  };
  vault: {
    recent: string[];
    default: string | null;
    sidebar_open: boolean;
    related_open: boolean;
    trash_expanded: boolean;
    chat_height: number;
    sidebar_width: number;
    discovery_width: number;
    show_sessions_in_tree: boolean;
    tree: { sort_by: "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc" };
  };
  search: {
    modes: { semantic: boolean; lexical: boolean };
    sections: { results_expanded: boolean; related_expanded: boolean };
  };
  llm: {
    enabled: boolean;
    provider: { backend: string; model: string; api_key_env: string; base_url: string };
    limits: { max_tokens: number; timeout_secs: number };
    agent: { iteration_cap: number; tool_timeout_secs: number };
    audit: { log_full_prompt: boolean };
  };
  mcp: unknown;
}

export interface SettingsApplyDeps {
  setLivePreviewEnabled: (on: boolean) => void;
  setWordWrapEnabled: (on: boolean) => void;
  setLineNumbersVisible: (on: boolean) => void;
  setWhitespaceEnabled: (on: boolean) => void;
  setChunkBoundariesEnabled: (on: boolean) => void;
  setHideFrontmatterEnabled: (on: boolean) => void;
  setTreeSortFromSettings: (sortBy: Settings["vault"]["tree"]["sort_by"]) => void;
  appEl: HTMLElement;
  trashBinEl: HTMLElement;
  trashChevronEl: HTMLElement;
  setChatEnabled: (on: boolean) => void;
  setChatHeight: (h: number) => void;
  setSidebarWidth: (px: number) => void;
  setDiscoveryWidth: (px: number) => void;
  setSearchMode: (mode: "semantic" | "lexical", on: boolean) => void;
  setSearchSection: (
    section: "results" | "related",
    expanded: boolean,
  ) => void;
  syncToggleButtons: () => void;
}

export function applySettingsToUi(s: Settings, deps: SettingsApplyDeps): void {
  viewSettingsStore.update((v) => ({
    ...v,
    renderTxtAsMarkdown: s.editor.render_txt_as_markdown,
  }));
  deps.setLivePreviewEnabled(s.editor.live_preview);
  deps.setWordWrapEnabled(s.editor.word_wrap);
  deps.setLineNumbersVisible(s.editor.show_line_numbers);
  deps.setWhitespaceEnabled(s.editor.show_whitespace);
  deps.setChunkBoundariesEnabled(s.editor.show_chunk_boundaries);
  deps.setHideFrontmatterEnabled(s.editor.hide_frontmatter);
  deps.setTreeSortFromSettings(s.vault.tree.sort_by);
  deps.appEl.classList.toggle("sidebar-collapsed", !s.vault.sidebar_open);
  deps.appEl.classList.toggle("related-collapsed", !s.vault.related_open);
  deps.trashBinEl.classList.toggle("collapsed", !s.vault.trash_expanded);
  deps.trashChevronEl.textContent = s.vault.trash_expanded ? "▾" : "▸";
  // status: chat-panel-default-height, llm-disable-mode (UI half)
  deps.setChatEnabled(s.llm.enabled);
  if (typeof s.vault.chat_height === "number") {
    deps.setChatHeight(s.vault.chat_height);
  }
  // status: side-panel-resize
  if (typeof s.vault.sidebar_width === "number") {
    deps.setSidebarWidth(s.vault.sidebar_width);
  }
  if (typeof s.vault.discovery_width === "number") {
    deps.setDiscoveryWidth(s.vault.discovery_width);
  }
  // status: search-mode-state-persisted, search-section-collapsible
  deps.setSearchMode("semantic", s.search.modes.semantic);
  deps.setSearchMode("lexical", s.search.modes.lexical);
  deps.setSearchSection("results", s.search.sections.results_expanded);
  deps.setSearchSection("related", s.search.sections.related_expanded);
  deps.syncToggleButtons();
}

export { sortOrderFromSettings };
