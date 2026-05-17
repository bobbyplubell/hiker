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
// status: search-mode-options-menu
// status: search-lexical-options
// status: search-semantic-options
//
// Discovery panel: search input + mode toggles + lexical/semantic results +
// related-notes panel + collapsible sections + roving-tabindex keyboard nav.
// Chat lives in the same `<aside id="discovery">` host but is wired
// separately from `./chat`. This module owns search modes, debounced query,
// search/related epoch counters, and section-collapse state. The host wires
// DOM ids and the editor-coupled `onOpenNote` / `onScrollToChunk` callbacks.

import type { ChunkBounds } from "../editor/chunkBoundaries";
import { Ipc, type RelatedHit, type SearchNoteHit } from "../ipc";
import { Logger } from "../logger";
import {
  createPanelController,
  type PanelController,
  type PanelDeps,
} from "../panels/controller";
import { Classes, Selectors } from "../style/classes";
import { openMenuAtAnchor, type CtxMenuItem } from "../widgets/contextMenu";

export interface LexicalSearchOpts {
  case_sensitive: boolean;
  diacritic_sensitive: boolean;
  prefix_match: boolean;
  phrase_mode: boolean;
}

export interface SemanticSearchOpts {
  min_similarity: number;
  top_k: number;
  recency_bias: "off" | "mild" | "strong";
}

const DEFAULT_LEXICAL_OPTS: LexicalSearchOpts = {
  case_sensitive: false,
  diacritic_sensitive: false,
  prefix_match: false,
  phrase_mode: false,
};

const DEFAULT_SEMANTIC_OPTS: SemanticSearchOpts = {
  min_similarity: 0.0,
  top_k: 25,
  recency_bias: "off",
};

export interface DiscoveryDeps extends PanelDeps {
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
  /// Called after `openNote` completes when the search hit specifies a
  /// chunk_index. Host is responsible for fetching `chunks_for` and
  /// scrolling the editor — this module only signals "open at chunk N."
  onScrollToChunk: (rel: string, chunkIndex: number) => Promise<void>;
  /// Sidebar/related toggle state — focusing the search input expands the
  /// panel via the host's existing toggle mechanism so the existing
  /// persistence and toggle-button sync runs through one path.
  expandPanelIfCollapsed: () => boolean;
}

export interface DiscoveryApi {
  refreshRelated(rel: string | null): Promise<void>;
  scheduleRelatedRefresh(rel: string | null, delayMs: number): void;
  setMode(mode: "semantic" | "lexical", on: boolean, persist: boolean): void;
  setLexicalOpts(opts: LexicalSearchOpts): void;
  setSemanticOpts(opts: SemanticSearchOpts): void;
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

export type DiscoveryController = PanelController<DiscoveryApi>;

// Discovery's own DOM lives inside `<aside id="discovery">` whose
// visibility is sidebar-managed by the host (the `appEl.classList`
// `related-collapsed` flag). The controller exposes
// `isVisible: () => true; setVisible: noop` per the bug row's guidance —
// the migration here is purely about moving the factory's API under
// `controller.api` and bundling cross-panel uniforms (`PanelDeps`).
export function mountDiscovery(deps: DiscoveryDeps): DiscoveryController {
  let searchModeSemantic = true;
  let searchModeLexical = true;
  let lexicalOpts: LexicalSearchOpts = { ...DEFAULT_LEXICAL_OPTS };
  let semanticOpts: SemanticSearchOpts = { ...DEFAULT_SEMANTIC_OPTS };
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
      void deps.settings.setVaultSetting(`search.modes.${mode}`, on);
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
    return Array.from(list.querySelectorAll<HTMLElement>(Selectors.RELATED_ITEM));
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
    if (!target.classList.contains(Classes.RELATED_ITEM)) return;
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
      const resp = await Ipc.searchNotes({
        query,
        modes: { semantic: searchModeSemantic, lexical: searchModeLexical },
        epoch,
        lexicalOpts,
        semanticOpts,
      });
      if (resp.epoch !== searchEpoch) return;
      deps.searchSpinnerEl.hidden = true;
      renderSearchResults(resp.hits);
    } catch (err) {
      if (epoch !== searchEpoch) return;
      Logger.error("ui::discovery", "search_vault failed", { err });
      deps.searchSpinnerEl.hidden = true;
      deps.searchListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
      deps.searchCountEl.textContent = "";
    }
  }

  // Pure builder for one search-result row. No event listeners attached
  // here — click handling rides container-level delegation on
  // `searchListEl` (see the `click` listener wiring below). The row
  // carries a `data-rel` attribute so the delegated handler can resolve
  // back to the hit's path; chunk index is carried on `data-chunk-index`
  // for the scroll-to-chunk follow-up. Keyboard nav stays unchanged
  // because it dispatches a synthetic `click()` on the focused row,
  // which the container listener picks up identically.
  function domForSearchResult(hit: SearchNoteHit): HTMLElement {
    const item = document.createElement("div");
    item.className = `${Classes.RELATED_ITEM} search-item`;
    item.tabIndex = -1;
    item.setAttribute("role", "option");
    item.dataset.rel = hit.path;
    item.dataset.chunkIndex = String(hit.chunk_index);

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

    return item;
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
      deps.searchListEl.appendChild(domForSearchResult(hit));
    }
    setRovingTabIndex(deps.searchListEl, 0);
  }

  function onSearchListClick(e: MouseEvent): void {
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const row = target.closest<HTMLElement>("[data-rel]");
    if (!row || !deps.searchListEl.contains(row)) return;
    const rel = row.dataset.rel;
    if (!rel) return;
    const chunkIndexRaw = row.dataset.chunkIndex;
    const chunkIndex = chunkIndexRaw !== undefined ? Number(chunkIndexRaw) : NaN;
    const sticky = e.metaKey || e.ctrlKey;
    void openSearchHitByPath(rel, Number.isFinite(chunkIndex) ? chunkIndex : null, {
      preview: !sticky,
    });
  }

  async function openSearchHitByPath(
    rel: string,
    chunkIndex: number | null,
    opts?: { preview?: boolean },
  ): Promise<void> {
    await deps.openNote(rel, opts);
    if (chunkIndex !== null) await deps.onScrollToChunk(rel, chunkIndex);
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
        void deps.settings.setVaultSetting("search.sections.results_expanded", expanded);
      }
    } else {
      relatedSectionExpanded = expanded;
      applySectionCollapsed(deps.relatedSectionEl, deps.relatedListEl, expanded);
      if (persist) {
        void deps.settings.setVaultSetting("search.sections.related_expanded", expanded);
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
      const hits = await Ipc.relatedNotes({ rel, topK: 10 });
      if (seq !== relatedRequestSeq) return;
      renderRelated(hits);
    } catch (err) {
      if (seq !== relatedRequestSeq) return;
      Logger.error("ui::discovery", "related_notes failed", { err });
      deps.relatedListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
    }
  }

  // Pure builder for one related-notes row. No event listeners attached
  // here — click handling rides container-level delegation on
  // `relatedListEl`. The row carries `data-rel` so the delegated handler
  // can resolve back to the hit's path. Keyboard nav (Enter dispatches a
  // synthetic click on the focused row) is picked up identically by the
  // delegated listener.
  function domForRelatedRow(hit: RelatedHit): HTMLElement {
    const item = document.createElement("div");
    item.className = Classes.RELATED_ITEM;
    item.tabIndex = -1;
    item.setAttribute("role", "option");
    item.dataset.rel = hit.path;

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

    return item;
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
      deps.relatedListEl.appendChild(domForRelatedRow(hit));
    }
    setRovingTabIndex(deps.relatedListEl, 0);
  }

  function onRelatedListClick(e: MouseEvent): void {
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const row = target.closest<HTMLElement>("[data-rel]");
    if (!row || !deps.relatedListEl.contains(row)) return;
    const rel = row.dataset.rel;
    if (!rel) return;
    const sticky = e.metaKey || e.ctrlKey;
    void deps.openNote(rel, { preview: !sticky });
  }

  function scheduleRelatedRefresh(rel: string | null, delayMs: number): void {
    if (relatedDebounce !== null) window.clearTimeout(relatedDebounce);
    relatedDebounce = window.setTimeout(() => {
      relatedDebounce = null;
      void refreshRelated(rel);
    }, delayMs);
  }

  function setLexicalOpts(opts: LexicalSearchOpts): void {
    lexicalOpts = { ...opts };
  }
  function setSemanticOpts(opts: SemanticSearchOpts): void {
    semanticOpts = { ...opts };
  }

  function persistLexical<K extends keyof LexicalSearchOpts>(
    key: K,
    value: LexicalSearchOpts[K],
  ): void {
    lexicalOpts = { ...lexicalOpts, [key]: value };
    void deps.settings.setVaultSetting(`search.lexical.${key}`, value);
    maybeRerunSearchAfterModeChange();
  }
  function persistSemantic<K extends keyof SemanticSearchOpts>(
    key: K,
    value: SemanticSearchOpts[K],
  ): void {
    semanticOpts = { ...semanticOpts, [key]: value };
    void deps.settings.setVaultSetting(`search.semantic.${key}`, value);
    maybeRerunSearchAfterModeChange();
  }

  // status: search-mode-options-menu
  //
  // Right-click on either toggle pops a `openContextMenu` popover
  // anchored under the button with mode-specific options. Left-click
  // still flips on/off — opening the menu does *not* toggle enabled
  // state per the spec ("no behavioral overload of the primary
  // affordance").
  function openLexicalOptionsMenu(anchor: HTMLElement): void {
    const items: CtxMenuItem[] = [
      {
        label: "Case sensitive",
        checked: lexicalOpts.case_sensitive,
        run: () => persistLexical("case_sensitive", !lexicalOpts.case_sensitive),
      },
      {
        label: "Match diacritics",
        checked: lexicalOpts.diacritic_sensitive,
        run: () =>
          persistLexical(
            "diacritic_sensitive",
            !lexicalOpts.diacritic_sensitive,
          ),
      },
      {
        label: "Prefix match",
        checked: lexicalOpts.prefix_match,
        tooltip: lexicalOpts.phrase_mode
          ? "Phrase mode is on — FTS5 ignores prefix * inside a quoted phrase."
          : undefined,
        run: () => persistLexical("prefix_match", !lexicalOpts.prefix_match),
      },
      {
        label: "Phrase mode",
        checked: lexicalOpts.phrase_mode,
        run: () => persistLexical("phrase_mode", !lexicalOpts.phrase_mode),
      },
    ];
    openMenuAtAnchor(anchor, items);
  }

  function openSemanticOptionsMenu(anchor: HTMLElement): void {
    const items: CtxMenuItem[] = [
      {
        kind: "slider",
        label: "Minimum similarity",
        min: 0,
        max: 0.95,
        step: 0.05,
        value: semanticOpts.min_similarity,
        format: (v) => v.toFixed(2),
        onChange: (v) => persistSemantic("min_similarity", v),
      },
      {
        kind: "number",
        label: "Top-k override",
        min: 5,
        max: 100,
        step: 1,
        value: semanticOpts.top_k,
        onCommit: (v) => persistSemantic("top_k", v),
      },
      {
        kind: "radio",
        label: "Recency bias",
        value: semanticOpts.recency_bias,
        options: [
          { label: "Off", value: "off" },
          { label: "Mild", value: "mild" },
          { label: "Strong", value: "strong" },
        ],
        onChange: (v) =>
          persistSemantic(
            "recency_bias",
            v as SemanticSearchOpts["recency_bias"],
          ),
      },
    ];
    openMenuAtAnchor(anchor, items);
  }

  // ---------- DOM listeners ----------
  deps.toggleSemanticBtn.addEventListener("click", () => {
    setMode("semantic", !searchModeSemantic, true);
  });
  deps.toggleLexicalBtn.addEventListener("click", () => {
    setMode("lexical", !searchModeLexical, true);
  });
  // status: search-mode-options-menu
  deps.toggleSemanticBtn.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openSemanticOptionsMenu(deps.toggleSemanticBtn);
  });
  deps.toggleLexicalBtn.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openLexicalOptionsMenu(deps.toggleLexicalBtn);
  });
  // Container-level click delegation; rows are pure DOM with a
  // `data-rel` hook the handler resolves via `closest`.
  deps.searchListEl.addEventListener("click", onSearchListClick);
  deps.relatedListEl.addEventListener("click", onRelatedListClick);
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

  const api: DiscoveryApi = {
    refreshRelated,
    scheduleRelatedRefresh,
    setMode,
    setLexicalOpts,
    setSemanticOpts,
    setSectionExpanded,
    syncToggleButtons,
    clear,
    focusInput,
  };
  return createPanelController<DiscoveryApi>(api, {
    initialVisible: true,
    applyOnMount: false,
    onSetVisible: () => {
      // Sidebar-managed; no panel-level visibility toggle.
    },
  });
}

// Re-export `ChunkBounds` consumers may need from here for type imports if
// they don't already pull from `editor/chunkBoundaries`.
export type { ChunkBounds };
