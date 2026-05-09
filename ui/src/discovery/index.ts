// status: search-discovery-panel
// status: search-mode-toggles
// status: search-modes-both-off-disabled
// status: search-empty-collapses-results
// status: search-typeahead-debounce
// status: search-keybind-ctrl-space
// status: search-keyboard-nav
// status: search-result-row, search-result-grouped-by-note (UI side)
// status: search-section-counts
// status: search-result-click-opens-chunk
// status: search-section-collapsible
//
// Discovery panel: search input + mode toggles + lexical/semantic results +
// related-notes panel + collapsible sections + roving-tabindex keyboard nav.
// Chat lives in the same `<aside id="discovery">` host but is wired
// separately from `./chat`. This module owns search modes, debounced query,
// search/related epoch counters, and section-collapse state. The host wires
// DOM ids and the editor-coupled `onOpenNote` / `onScrollToChunk` callbacks.

import { invoke } from "@tauri-apps/api/core";
import type { ChunkBounds } from "../editor/chunkBoundaries";

interface RelatedHit {
  note_id: string;
  path: string;
  title: string;
  score: number;
  best_heading_path: string | null;
  snippet: string;
}

interface SearchNoteHit {
  note_id: string;
  path: string;
  title: string;
  score: number;
  chunk_id: string;
  chunk_index: number;
  heading_path: string | null;
  snippet: string;
}

interface SearchResponse {
  epoch: number;
  lexical_hits: SearchNoteHit[];
  semantic_hits: SearchNoteHit[];
  fused: SearchNoteHit[];
  hits: SearchNoteHit[];
}

export interface DiscoveryDeps {
  appEl: HTMLElement;
  inputEl: HTMLInputElement;
  clearBtn: HTMLButtonElement;
  toggleSemanticBtn: HTMLButtonElement;
  toggleLexicalBtn: HTMLButtonElement;
  searchSectionEl: HTMLElement;
  searchListEl: HTMLElement;
  searchCountEl: HTMLElement;
  searchSpinnerEl: HTMLElement;
  relatedSectionEl: HTMLElement;
  relatedListEl: HTMLElement;
  relatedCountEl: HTMLElement;
  /// Called when the user clicks a result. Host opens the file (and may
  /// reject via a dirty-buffer guard).
  onOpenNote: (rel: string, opts?: { preview?: boolean }) => Promise<void>;
  /// Called after `onOpenNote` completes when the search hit specifies a
  /// chunk_index. Host is responsible for fetching `chunks_for` and
  /// scrolling the editor — this module only signals "open at chunk N."
  onScrollToChunk: (rel: string, chunkIndex: number) => Promise<void>;
  /// Persist a setting key/value via `set_setting`.
  persistSetting: (
    scope: "user" | "vault",
    key: string,
    value: unknown,
  ) => Promise<void>;
  /// Sidebar/related toggle state — focusing the search input expands the
  /// panel via the host's existing toggle mechanism so the existing
  /// persistence and toggle-button sync runs through one path.
  expandPanelIfCollapsed: () => boolean;
}

export interface DiscoveryApi {
  refreshRelated(rel: string | null): Promise<void>;
  scheduleRelatedRefresh(rel: string | null, delayMs: number): void;
  setMode(mode: "semantic" | "lexical", on: boolean, persist: boolean): void;
  setSectionExpanded(
    section: "results" | "related",
    expanded: boolean,
    persist: boolean,
  ): void;
  syncToggleButtons(): void;
  /// Drop the search input value + any in-flight result. Called on vault
  /// open so prior-vault matches don't linger.
  clear(): void;
  focusInput(): void;
}

const SEARCH_DEBOUNCE_MS = 250;

export function mountDiscovery(deps: DiscoveryDeps): DiscoveryApi {
  let searchModeSemantic = true;
  let searchModeLexical = true;
  let searchEpoch = 0;
  let searchDebounceTimer: number | null = null;
  let searchSectionExpanded = true;
  let relatedSectionExpanded = true;
  let relatedRequestSeq = 0;
  let relatedDebounce: number | null = null;

  function applySearchInputDisabledState(): void {
    const bothOff = !searchModeSemantic && !searchModeLexical;
    deps.inputEl.disabled = bothOff;
    deps.inputEl.placeholder = bothOff
      ? "Enable Semantic or Lexical to search"
      : "Search vault…";
  }

  function syncToggleButtons(): void {
    deps.toggleSemanticBtn.classList.toggle("active", searchModeSemantic);
    deps.toggleLexicalBtn.classList.toggle("active", searchModeLexical);
    applySearchInputDisabledState();
  }

  function setMode(mode: "semantic" | "lexical", on: boolean, persist: boolean): void {
    if (mode === "semantic") searchModeSemantic = on;
    else searchModeLexical = on;
    syncToggleButtons();
    if (persist) {
      void deps.persistSetting("vault", `search.modes.${mode}`, on);
      maybeRerunSearchAfterModeChange();
    }
  }

  function applySearchSectionVisibility(): void {
    const hasQuery = deps.inputEl.value.trim().length > 0;
    deps.searchSectionEl.hidden = !hasQuery;
  }

  function applyClearButtonVisibility(): void {
    deps.clearBtn.hidden = deps.inputEl.value.length === 0;
  }

  function focusInput(): void {
    const wasCollapsed = deps.expandPanelIfCollapsed();
    const doFocus = () => {
      deps.inputEl.focus();
      deps.inputEl.select();
    };
    if (wasCollapsed) requestAnimationFrame(doFocus);
    else doFocus();
  }

  // status: search-keyboard-nav
  function discoveryRows(list: HTMLElement): HTMLElement[] {
    return Array.from(list.querySelectorAll<HTMLElement>(".related-item"));
  }

  function setRovingTabIndex(list: HTMLElement, idx: number): void {
    const rows = discoveryRows(list);
    rows.forEach((r, i) => {
      r.tabIndex = i === idx ? 0 : -1;
    });
  }

  function focusRow(list: HTMLElement, idx: number): boolean {
    const rows = discoveryRows(list);
    if (rows.length === 0 || idx < 0 || idx >= rows.length) return false;
    setRovingTabIndex(list, idx);
    rows[idx].focus();
    return true;
  }

  function activeRowIndex(list: HTMLElement): number {
    return discoveryRows(list).findIndex((r) => r === document.activeElement);
  }

  function onResultListKeydown(e: KeyboardEvent): void {
    const target = e.target as HTMLElement;
    if (!target.classList.contains("related-item")) return;
    const list = target.closest("#search-list, #related-list") as HTMLElement | null;
    if (!list) return;
    const idx = activeRowIndex(list);
    if (idx < 0) return;
    switch (e.key) {
      case "ArrowDown": {
        e.preventDefault();
        const rows = discoveryRows(list);
        if (idx + 1 < rows.length) {
          focusRow(list, idx + 1);
        } else if (list === deps.searchListEl) {
          if (!focusRow(deps.relatedListEl, 0)) {
            // No related rows; stay put.
          }
        }
        break;
      }
      case "ArrowUp": {
        e.preventDefault();
        if (idx > 0) {
          focusRow(list, idx - 1);
        } else if (list === deps.relatedListEl) {
          const searchRows = discoveryRows(deps.searchListEl);
          if (searchRows.length > 0) {
            focusRow(deps.searchListEl, searchRows.length - 1);
          }
        }
        break;
      }
      case "Enter": {
        e.preventDefault();
        target.click();
        break;
      }
      case "Escape": {
        e.preventDefault();
        deps.inputEl.focus();
        break;
      }
    }
  }

  function onSearchInput(): void {
    applyClearButtonVisibility();
    applySearchSectionVisibility();
    if (searchDebounceTimer !== null) {
      window.clearTimeout(searchDebounceTimer);
      searchDebounceTimer = null;
    }
    const trimmed = deps.inputEl.value.trim();
    if (trimmed.length === 0) {
      searchEpoch += 1;
      deps.searchSpinnerEl.hidden = true;
      deps.searchListEl.innerHTML = "";
      deps.searchCountEl.textContent = "";
      return;
    }
    if (!searchModeSemantic && !searchModeLexical) return;
    deps.searchSpinnerEl.hidden = false;
    searchDebounceTimer = window.setTimeout(() => {
      searchDebounceTimer = null;
      const epoch = ++searchEpoch;
      void runSearch(trimmed, epoch);
    }, SEARCH_DEBOUNCE_MS);
  }

  async function runSearch(query: string, epoch: number): Promise<void> {
    try {
      const resp = await invoke<SearchResponse>("search_vault", {
        query,
        modes: { semantic: searchModeSemantic, lexical: searchModeLexical },
        epoch,
      });
      if (resp.epoch !== searchEpoch) return;
      deps.searchSpinnerEl.hidden = true;
      renderSearchResults(resp.hits);
    } catch (err) {
      if (epoch !== searchEpoch) return;
      console.error("search_vault failed:", err);
      deps.searchSpinnerEl.hidden = true;
      deps.searchListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
      deps.searchCountEl.textContent = "";
    }
  }

  function renderSearchResults(hits: SearchNoteHit[]): void {
    deps.searchListEl.innerHTML = "";
    deps.searchCountEl.textContent = hits.length > 0 ? `(${hits.length})` : "";
    if (hits.length === 0) {
      const empty = document.createElement("div");
      empty.className = "related-empty";
      empty.textContent = "No matches.";
      deps.searchListEl.appendChild(empty);
      return;
    }
    for (const hit of hits) {
      const item = document.createElement("div");
      item.className = "related-item search-item";
      item.tabIndex = -1;
      item.setAttribute("role", "option");
      // status: editor-preview-tab-from-open-callsites
      // status: editor-preview-tab-mod-click-sticky
      item.addEventListener("click", (e) => {
        const sticky = e.metaKey || e.ctrlKey;
        void openSearchHit(hit, { preview: !sticky });
      });

      const title = document.createElement("div");
      title.className = "related-item-title";
      title.textContent = hit.title;
      item.appendChild(title);

      const meta = document.createElement("div");
      meta.className = "related-item-meta";
      const heading = hit.heading_path ? `${hit.heading_path} · ` : "";
      meta.textContent = `${heading}score ${hit.score.toFixed(3)}`;
      item.appendChild(meta);

      const snippet = document.createElement("div");
      snippet.className = "related-item-snippet";
      appendSnippetWithMarks(snippet, hit.snippet);
      item.appendChild(snippet);

      deps.searchListEl.appendChild(item);
    }
    setRovingTabIndex(deps.searchListEl, 0);
  }

  function appendSnippetWithMarks(host: HTMLElement, snippet: string): void {
    let i = 0;
    while (i < snippet.length) {
      const open = snippet.indexOf("<mark>", i);
      if (open < 0) {
        host.appendChild(document.createTextNode(snippet.slice(i)));
        return;
      }
      if (open > i) {
        host.appendChild(document.createTextNode(snippet.slice(i, open)));
      }
      const inner = open + "<mark>".length;
      const close = snippet.indexOf("</mark>", inner);
      if (close < 0) {
        host.appendChild(document.createTextNode(snippet.slice(open)));
        return;
      }
      const span = document.createElement("span");
      span.className = "search-mark";
      span.textContent = snippet.slice(inner, close);
      host.appendChild(span);
      i = close + "</mark>".length;
    }
  }

  async function openSearchHit(
    hit: SearchNoteHit,
    opts?: { preview?: boolean },
  ): Promise<void> {
    await deps.onOpenNote(hit.path, opts);
    await deps.onScrollToChunk(hit.path, hit.chunk_index);
  }

  function clear(): void {
    deps.inputEl.value = "";
    searchEpoch += 1;
    deps.searchSpinnerEl.hidden = true;
    deps.searchListEl.innerHTML = "";
    deps.searchCountEl.textContent = "";
    applyClearButtonVisibility();
    applySearchSectionVisibility();
  }

  function maybeRerunSearchAfterModeChange(): void {
    if (deps.inputEl.disabled) return;
    if (deps.inputEl.value.trim().length === 0) return;
    const epoch = ++searchEpoch;
    deps.searchSpinnerEl.hidden = false;
    void runSearch(deps.inputEl.value.trim(), epoch);
  }

  // status: search-section-collapsible
  // status: bug-discovery-sections-overlap-after-toggle-cycles (fixed)
  //
  // The previous helper toggled both `[hidden]` on the body AND a
  // `.collapsed` class on the section. CSS using flex layout on the
  // section's body could leave residual sizing across cycles when both
  // mechanisms competed for "what makes the body not contribute height."
  // Drive collapse purely from the boolean `[hidden]` attribute on the
  // body — the browser pulls hidden elements out of layout reliably — and
  // keep the `.collapsed` class only as a visual hook for the chevron.
  function applySectionCollapsed(
    section: HTMLElement,
    body: HTMLElement,
    expanded: boolean,
  ): void {
    section.classList.toggle("collapsed", !expanded);
    body.hidden = !expanded;
    // Belt-and-suspenders: reset any inline height that prior CSS animations
    // may have left behind. The bug repro toggled rapidly enough that
    // residual `style.height` values lingered on the body element.
    body.style.removeProperty("height");
    body.style.removeProperty("min-height");
    body.style.removeProperty("max-height");
  }

  function setSectionExpanded(
    section: "results" | "related",
    expanded: boolean,
    persist: boolean,
  ): void {
    if (section === "results") {
      searchSectionExpanded = expanded;
      applySectionCollapsed(deps.searchSectionEl, deps.searchListEl, expanded);
      if (persist) {
        void deps.persistSetting("vault", "search.sections.results_expanded", expanded);
      }
    } else {
      relatedSectionExpanded = expanded;
      applySectionCollapsed(deps.relatedSectionEl, deps.relatedListEl, expanded);
      if (persist) {
        void deps.persistSetting("vault", "search.sections.related_expanded", expanded);
      }
    }
  }

  // ---------- related-notes panel ----------
  async function refreshRelated(rel: string | null): Promise<void> {
    const seq = ++relatedRequestSeq;
    if (!rel) {
      deps.relatedListEl.innerHTML = "";
      deps.relatedCountEl.textContent = "";
      return;
    }
    try {
      const hits = await invoke<RelatedHit[]>("related_notes", { rel, topK: 10 });
      if (seq !== relatedRequestSeq) return;
      renderRelated(hits);
    } catch (err) {
      if (seq !== relatedRequestSeq) return;
      console.error("related_notes failed:", err);
      deps.relatedListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
    }
  }

  function renderRelated(hits: RelatedHit[]): void {
    deps.relatedListEl.innerHTML = "";
    deps.relatedCountEl.textContent = hits.length > 0 ? `(${hits.length})` : "";
    if (hits.length === 0) {
      const empty = document.createElement("div");
      empty.className = "related-empty";
      empty.textContent = "No related notes yet.";
      deps.relatedListEl.appendChild(empty);
      return;
    }
    for (const hit of hits) {
      const item = document.createElement("div");
      item.className = "related-item";
      item.tabIndex = -1;
      item.setAttribute("role", "option");
      // status: editor-preview-tab-from-open-callsites
      // status: editor-preview-tab-mod-click-sticky
      item.addEventListener("click", (e) => {
        const sticky = e.metaKey || e.ctrlKey;
        void deps.onOpenNote(hit.path, { preview: !sticky });
      });

      const title = document.createElement("div");
      title.className = "related-item-title";
      title.textContent = hit.title;
      item.appendChild(title);

      const meta = document.createElement("div");
      meta.className = "related-item-meta";
      const heading = hit.best_heading_path ? `${hit.best_heading_path} · ` : "";
      meta.textContent = `${heading}score ${hit.score.toFixed(3)}`;
      item.appendChild(meta);

      const snippet = document.createElement("div");
      snippet.className = "related-item-snippet";
      snippet.textContent = hit.snippet;
      item.appendChild(snippet);

      deps.relatedListEl.appendChild(item);
    }
    setRovingTabIndex(deps.relatedListEl, 0);
  }

  function scheduleRelatedRefresh(rel: string | null, delayMs: number): void {
    if (relatedDebounce !== null) window.clearTimeout(relatedDebounce);
    relatedDebounce = window.setTimeout(() => {
      relatedDebounce = null;
      void refreshRelated(rel);
    }, delayMs);
  }

  // ---------- DOM listeners ----------
  deps.toggleSemanticBtn.addEventListener("click", () => {
    setMode("semantic", !searchModeSemantic, true);
  });
  deps.toggleLexicalBtn.addEventListener("click", () => {
    setMode("lexical", !searchModeLexical, true);
  });
  deps.searchListEl.addEventListener("keydown", onResultListKeydown);
  deps.relatedListEl.addEventListener("keydown", onResultListKeydown);
  deps.inputEl.addEventListener("input", onSearchInput);
  deps.inputEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.preventDefault();
      if (deps.inputEl.value.length > 0) {
        deps.inputEl.value = "";
        onSearchInput();
      } else {
        deps.inputEl.blur();
      }
    } else if (e.key === "ArrowDown") {
      const searchRows = discoveryRows(deps.searchListEl);
      if (searchRows.length > 0) {
        e.preventDefault();
        focusRow(deps.searchListEl, 0);
        return;
      }
      const relatedRows = discoveryRows(deps.relatedListEl);
      if (relatedRows.length > 0) {
        e.preventDefault();
        focusRow(deps.relatedListEl, 0);
      }
    }
  });
  deps.clearBtn.addEventListener("click", () => {
    deps.inputEl.value = "";
    onSearchInput();
    deps.inputEl.focus();
  });
  deps.searchSectionEl
    .querySelector(".discovery-section-header")!
    .addEventListener("click", () => {
      setSectionExpanded("results", !searchSectionExpanded, true);
    });
  deps.relatedSectionEl
    .querySelector(".discovery-section-header")!
    .addEventListener("click", () => {
      setSectionExpanded("related", !relatedSectionExpanded, true);
    });

  return {
    refreshRelated,
    scheduleRelatedRefresh,
    setMode,
    setSectionExpanded,
    syncToggleButtons,
    clear,
    focusInput,
  };
}

// Re-export `ChunkBounds` consumers may need from here for type imports if
// they don't already pull from `editor/chunkBoundaries`.
export type { ChunkBounds };
