/// One-shot capture of every `document.getElementById` `bootstrap()`
/// reads at startup. Bundled into one typed object so the refs are
/// grep-able as a unit and bootstrap doesn't open with ~80 untyped
/// lookups. Grouped by domain (editor / status bar / sidebar / chat /
/// vault home / etc.) rather than a flat 80-key bag.
///
/// Step 5a of the main.ts refactor.
///
/// Element types — most ids resolve to a generic `HTMLElement`; the
/// handful with a more specific type (`HTMLButtonElement` for buttons
/// the host calls `.disabled = true` on; input/textarea/form for the
/// chat box and search input) are typed precisely so call sites don't
/// need to re-cast. Optional refs (window-control buttons that may not
/// exist on every platform shell) use `| null`.
export interface DomRefs {
  editor: {
    appEl: HTMLElement;
    editorEl: HTMLElement;
    editorPaneEl: HTMLElement;
    saveBtn: HTMLButtonElement;
    diffBtn: HTMLButtonElement;
    modeControlsEl: HTMLElement;
    viewMenuBtn: HTMLButtonElement;
    mutationsMenuBtn: HTMLButtonElement;
    tabStripEl: HTMLElement;
  };
  statusBar: {
    statusPathEl: HTMLElement;
    statusCursorEl: HTMLElement;
    statusWordsEl: HTMLElement;
    statusIndexEl: HTMLElement;
  };
  vaultBar: {
    pickBtn: HTMLButtonElement;
    vaultPathEl: HTMLElement;
    homeBtn: HTMLButtonElement;
    settingsBtn: HTMLButtonElement;
    navBackBtn: HTMLButtonElement;
    navForwardBtn: HTMLButtonElement;
    queueBtnEl: HTMLButtonElement | null;
    queueIndicatorEl: HTMLElement | null;
  };
  topStrip: {
    topStripEl: HTMLElement | null;
    winMinBtn: HTMLElement | null;
    winMaxBtn: HTMLElement | null;
    winCloseBtn: HTMLElement | null;
  };
  tree: {
    treeEl: HTMLElement;
    newNoteBtn: HTMLButtonElement;
    sidebarActionsBtn: HTMLButtonElement;
  };
  trash: {
    binEl: HTMLElement;
    headerEl: HTMLElement;
    listEl: HTMLElement;
    chevronEl: HTMLElement;
    labelEl: HTMLElement;
  };
  discovery: {
    panelEl: HTMLElement;
    relatedListEl: HTMLElement;
    searchInputEl: HTMLInputElement;
    searchClearBtn: HTMLButtonElement;
    toggleModeSemanticBtn: HTMLButtonElement;
    toggleModeLexicalBtn: HTMLButtonElement;
    searchSectionEl: HTMLElement;
    searchListEl: HTMLElement;
    searchCountEl: HTMLElement;
    searchSpinnerEl: HTMLElement;
    relatedSectionEl: HTMLElement;
    relatedCountEl: HTMLElement;
    toggleSidebarBtn: HTMLButtonElement;
    toggleRelatedBtn: HTMLButtonElement;
  };
  chat: {
    regionEl: HTMLElement;
    handleEl: HTMLElement;
    collapseBtnEl: HTMLButtonElement;
    transcriptEl: HTMLElement;
    formEl: HTMLFormElement;
    inputEl: HTMLTextAreaElement;
    sendBtnEl: HTMLButtonElement;
    sessionMenuBtnEl: HTMLButtonElement;
    sessionMenuLabelEl: HTMLElement;
    // status: chat-panel-expand-to-editor
    expandBtnEl: HTMLButtonElement;
  };
  settingsPane: {
    paneEl: HTMLElement;
  };
  vaultHome: {
    rootEl: HTMLElement;
    titleEl: HTMLElement;
    statsBodyEl: HTMLElement;
    modifiedListEl: HTMLElement;
    accessedListEl: HTMLElement;
    newNoteBtn: HTMLButtonElement;
    overviewEl: HTMLElement;
    detailEl: HTMLElement;
    detailTitleEl: HTMLElement;
    detailCountEl: HTMLElement;
    detailListEl: HTMLElement;
    detailFiltersEl: HTMLElement;
    activitySectionEl: HTMLElement;
    activityHeaderEl: HTMLElement;
    activityListEl: HTMLElement;
    queueDetailEl: HTMLElement;
    tasksSection: HTMLElement | null;
    tasksHeader: HTMLElement | null;
    tasksSummary: HTMLElement | null;
  };
  // status: note-properties-tab
  propertiesPane: {
    paneEl: HTMLElement;
  };
}

function byId<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) {
    throw new Error(`captureDomRefs: element #${id} not found`);
  }
  return el as T;
}

function maybeId<T extends HTMLElement>(id: string): T | null {
  return (document.getElementById(id) as T | null) ?? null;
}

/// Capture the host's well-known DOM ids in one pass. Throws if a
/// required element is missing — that's a build / template bug, not a
/// runtime case worth guarding against per call site.
export function captureDomRefs(): DomRefs {
  return {
    editor: {
      appEl: byId("app"),
      editorEl: byId("editor"),
      editorPaneEl: byId("editor-pane"),
      saveBtn: byId<HTMLButtonElement>("save-btn"),
      diffBtn: byId<HTMLButtonElement>("diff-btn"),
      modeControlsEl: byId("mode-controls"),
      viewMenuBtn: byId<HTMLButtonElement>("view-menu-btn"),
      mutationsMenuBtn: byId<HTMLButtonElement>("mutations-menu-btn"),
      tabStripEl: byId("tab-strip"),
    },
    statusBar: {
      statusPathEl: byId("status-path"),
      statusCursorEl: byId("status-cursor"),
      statusWordsEl: byId("status-words"),
      statusIndexEl: byId("status-index"),
    },
    vaultBar: {
      pickBtn: byId<HTMLButtonElement>("pick-vault"),
      vaultPathEl: byId("vault-path"),
      homeBtn: byId<HTMLButtonElement>("home-btn"),
      settingsBtn: byId<HTMLButtonElement>("settings-btn"),
      navBackBtn: byId<HTMLButtonElement>("nav-back-btn"),
      navForwardBtn: byId<HTMLButtonElement>("nav-forward-btn"),
      queueBtnEl: maybeId<HTMLButtonElement>("queue-btn"),
      queueIndicatorEl: maybeId("queue-btn-indicator"),
    },
    topStrip: {
      topStripEl: maybeId("top-strip"),
      winMinBtn: maybeId("win-min"),
      winMaxBtn: maybeId("win-max"),
      winCloseBtn: maybeId("win-close"),
    },
    tree: {
      treeEl: byId("tree"),
      newNoteBtn: byId<HTMLButtonElement>("new-note-btn"),
      sidebarActionsBtn: byId<HTMLButtonElement>("sidebar-actions-btn"),
    },
    trash: {
      binEl: byId("trash-bin"),
      headerEl: byId("trash-header"),
      listEl: byId("trash-list"),
      chevronEl: byId("trash-chevron"),
      labelEl: byId("trash-label"),
    },
    discovery: {
      panelEl: byId("discovery"),
      relatedListEl: byId("related-list"),
      searchInputEl: byId<HTMLInputElement>("search-input"),
      searchClearBtn: byId<HTMLButtonElement>("search-clear-btn"),
      toggleModeSemanticBtn: byId<HTMLButtonElement>("toggle-mode-semantic"),
      toggleModeLexicalBtn: byId<HTMLButtonElement>("toggle-mode-lexical"),
      searchSectionEl: byId("search-section"),
      searchListEl: byId("search-list"),
      searchCountEl: byId("search-count"),
      searchSpinnerEl: byId("search-spinner"),
      relatedSectionEl: byId("related-section"),
      relatedCountEl: byId("related-count"),
      toggleSidebarBtn: byId<HTMLButtonElement>("toggle-sidebar"),
      toggleRelatedBtn: byId<HTMLButtonElement>("toggle-related"),
    },
    chat: {
      regionEl: byId("chat-region"),
      handleEl: byId("chat-resize-handle"),
      collapseBtnEl: byId<HTMLButtonElement>("chat-collapse-btn"),
      transcriptEl: byId("chat-transcript"),
      formEl: byId<HTMLFormElement>("chat-form"),
      inputEl: byId<HTMLTextAreaElement>("chat-input"),
      sendBtnEl: byId<HTMLButtonElement>("chat-send-btn"),
      sessionMenuBtnEl: byId<HTMLButtonElement>("chat-session-menu-btn"),
      sessionMenuLabelEl: byId("chat-session-menu-label"),
      // status: chat-panel-expand-to-editor
      expandBtnEl: byId<HTMLButtonElement>("chat-expand-btn"),
    },
    settingsPane: {
      paneEl: byId("settings-pane"),
    },
    vaultHome: {
      rootEl: byId("vault-home"),
      titleEl: byId("vault-home-title"),
      statsBodyEl: byId("vault-home-stats-body"),
      modifiedListEl: byId("vault-home-modified-list"),
      accessedListEl: byId("vault-home-accessed-list"),
      newNoteBtn: byId<HTMLButtonElement>("vault-home-new-note"),
      overviewEl: byId("vault-home-overview"),
      detailEl: byId("vault-home-detail"),
      detailTitleEl: byId("vault-home-detail-title"),
      detailCountEl: byId("vault-home-detail-count"),
      detailListEl: byId("vault-home-detail-list"),
      detailFiltersEl: byId("vault-home-detail-filters"),
      activitySectionEl: byId("vault-home-activity"),
      activityHeaderEl: byId("vault-home-activity-header"),
      activityListEl: byId("vault-home-activity-list"),
      queueDetailEl: byId("vault-home-queue-detail"),
      tasksSection: maybeId("vault-home-tasks"),
      tasksHeader: maybeId("vault-home-tasks-header"),
      tasksSummary: maybeId("vault-home-tasks-summary"),
    },
    // status: note-properties-tab
    propertiesPane: {
      paneEl: byId("properties-pane"),
    },
  };
}
