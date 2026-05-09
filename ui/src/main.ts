import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { EditorState, Compartment, type Extension } from "@codemirror/state";
import { EditorView, ViewPlugin, type ViewUpdate, highlightWhitespace } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { markdown, markdownLanguage } from "@codemirror/lang-markdown";
import { register, validate, toCMKeymap } from "./editor/keybinds";
import { livePreview } from "./editor/livePreview";
import {
  chunkBoundaries,
  chunkBoundariesHintState,
  chunkBoundsToState,
  clearChunkBoundariesState,
  setChunkBoundaries,
  type ChunkBounds,
} from "./editor/chunkBoundaries";
import { hideFrontmatter } from "./editor/hideFrontmatter";
import { diffExtensions, resetDiffDecorations } from "./diff";
import { mountChatPanel, ActiveSessionDto } from "./chat";
import { mountSettingsPane, type SettingsPaneApi } from "./settings";
import {
  mountSnapshotPreview,
  type SnapshotPreviewApi,
  type ChangeRow,
} from "./snapshotPreview";
import { mountTrash, type TrashApi } from "./trash";
import {
  mountTree,
  type TreeApi,
  type IndexState,
  sortOrderFromSettings,
} from "./tree";
import { openContextMenu, type CtxMenuItem } from "./widgets/contextMenu";
import { showToast } from "./widgets/toast";
import { confirm3, confirmWindowClose } from "./widgets/confirm";
import { mountVaultHome, type VaultHomeApi } from "./vaultHome";
import { mountQueueDetail, type QueueDetailApi } from "./queueDetail";
import { mountMutationsMenu, type MutationsMenuApi } from "./mutations";
import {
  mountDirtyBufferDiff,
  type DirtyBufferDiffApi,
} from "./dirtyBufferDiff";
import { mountTabStrip, type TabStripApi } from "./tabStrip";
import { mountDiscovery, type DiscoveryApi } from "./discovery";
import {
  mountNavigation,
  installNavigationSwipe,
  type NavApi,
  type NavState,
} from "./navigation";
import {
  mountModeControls,
  type ModeControlsApi,
  iconButton,
  ICON_DIFF,
  ICON_RESTORE,
  ICON_CLOSE,
} from "./modeControls";

// `DirEntry` re-exported from `./tree`.
interface FileWithHash {
  contents: string;
  hash: string;
}
// `TrashEntry` / `TrashListItem` now live in `./trash`.
// `RelatedHit` / `SearchNoteHit` / `SearchResponse` now live in `./discovery`.
interface IndexStatus {
  model_ready: boolean;
  queued: number;
  total_notes: number;
  last_error: string | null;
}
// `IndexState` re-exported from `./tree`.

// status: settings-load-once-at-startup
// Mirror of core::config::Config for the frontend. Returned by
// `get_settings` on vault open; consumed to seed View menu / tree state /
// panel state defaults. Field shapes match the Rust serde output.
interface Settings {
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

type SettingsScope = "user" | "vault";

// status: settings-write-back
// Persist a single setting via the Tauri write-back command. Failures are
// logged but never propagated to the user — a flip that worked locally
// should not show an error toast just because the disk write failed; the
// in-memory change still took effect for the session.
async function persistSetting(
  scope: SettingsScope,
  key: string,
  value: unknown,
): Promise<void> {
  try {
    await invoke("set_setting", { scope, key, value });
  } catch (err) {
    console.error(`set_setting ${scope}.${key} failed:`, err);
  }
}

// Apply a freshly loaded `Settings` snapshot to every UI surface that
// reflects a setting. Called on vault open and again whenever the settings
// pane writes through `set_setting` / `reload_config` so the View menu,
// tree sort, panel collapse states, etc. stay in sync with the on-disk
// canonical values.
function applySettingsToUi(s: Settings): void {
  renderTxtAsMarkdown = s.editor.render_txt_as_markdown;
  setLivePreviewEnabled(s.editor.live_preview);
  setWordWrapEnabled(s.editor.word_wrap);
  setLineNumbersVisible(s.editor.show_line_numbers);
  setWhitespaceEnabled(s.editor.show_whitespace);
  setChunkBoundariesEnabled(s.editor.show_chunk_boundaries);
  setHideFrontmatterEnabled(s.editor.hide_frontmatter);
  void tree.setSortOrder(sortOrderFromSettings(s.vault.tree.sort_by), false);
  appEl.classList.toggle("sidebar-collapsed", !s.vault.sidebar_open);
  appEl.classList.toggle("related-collapsed", !s.vault.related_open);
  trashBinEl.classList.toggle("collapsed", !s.vault.trash_expanded);
  trashChevronEl.textContent = s.vault.trash_expanded ? "▾" : "▸";
  // status: chat-panel-default-height, llm-disable-mode (UI half)
  chatPanel.setEnabled(s.llm.enabled);
  if (typeof s.vault.chat_height === "number") {
    chatPanel.setHeight(s.vault.chat_height);
  }
  // status: search-mode-state-persisted, search-section-collapsible
  discovery.setMode("semantic", s.search.modes.semantic, false);
  discovery.setMode("lexical", s.search.modes.lexical, false);
  discovery.setSectionExpanded("results", s.search.sections.results_expanded, false);
  discovery.setSectionExpanded("related", s.search.sections.related_expanded, false);
  syncToggleButtons();
}

type ProgressEvent =
  | { kind: "model_loaded" }
  | { kind: "started"; path: string }
  | { kind: "finished"; path: string }
  | { kind: "skipped"; path: string; reason: string }
  | { kind: "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string }
  | { kind: "scan_complete"; scanned: number; queued: number }
  | { kind: "error"; path: string | null; message: string };

const appEl = document.getElementById("app")!;
const treeEl = document.getElementById("tree")!;
const editorEl = document.getElementById("editor")!;
const pickBtn = document.getElementById("pick-vault") as HTMLButtonElement;
const vaultPathEl = document.getElementById("vault-path")!;
const saveBtn = document.getElementById("save-btn") as HTMLButtonElement;
const diffBtn = document.getElementById("diff-btn") as HTMLButtonElement;
const statusPathEl = document.getElementById("status-path")!;
const statusCursorEl = document.getElementById("status-cursor")!;
const statusWordsEl = document.getElementById("status-words")!;
const statusIndexEl = document.getElementById("status-index")!;
const relatedListEl = document.getElementById("related-list")!;
// status: search-discovery-panel
const searchInputEl = document.getElementById("search-input") as HTMLInputElement;
const searchClearBtn = document.getElementById("search-clear-btn") as HTMLButtonElement;
const toggleModeSemanticBtn = document.getElementById("toggle-mode-semantic") as HTMLButtonElement;
const toggleModeLexicalBtn = document.getElementById("toggle-mode-lexical") as HTMLButtonElement;
const searchSectionEl = document.getElementById("search-section")!;
const searchListEl = document.getElementById("search-list")!;
const searchCountEl = document.getElementById("search-count")!;
const searchSpinnerEl = document.getElementById("search-spinner")!;
const relatedSectionEl = document.getElementById("related-section")!;
const relatedCountEl = document.getElementById("related-count")!;
const toggleSidebarBtn = document.getElementById("toggle-sidebar") as HTMLButtonElement;
const toggleRelatedBtn = document.getElementById("toggle-related") as HTMLButtonElement;
const newNoteBtn = document.getElementById("new-note-btn") as HTMLButtonElement;
const treeActionsBtn = document.getElementById("tree-actions-btn") as HTMLButtonElement;
const trashBinEl = document.getElementById("trash-bin")!;
const trashHeaderEl = document.getElementById("trash-header")!;
const trashListEl = document.getElementById("trash-list")!;
const trashChevronEl = document.getElementById("trash-chevron")!;
const trashLabelEl = document.getElementById("trash-label")!;
const modeControlsEl = document.getElementById("mode-controls")!;
const homeBtn = document.getElementById("home-btn") as HTMLButtonElement;
const settingsBtn = document.getElementById("settings-btn") as HTMLButtonElement;
const settingsPaneEl = document.getElementById("settings-pane")!;
const editorPaneEl = document.getElementById("editor-pane")!;
const vaultHomeEl = document.getElementById("vault-home")!;
// status: chat-panel-pinned-bottom
const discoveryPanelEl = document.getElementById("discovery")!;
const chatRegionEl = document.getElementById("chat-region")!;
const chatHandleEl = document.getElementById("chat-resize-handle")!;
const chatCollapseBtnEl = document.getElementById("chat-collapse-btn") as HTMLButtonElement;
const chatTranscriptEl = document.getElementById("chat-transcript")!;
const chatFormEl = document.getElementById("chat-form") as HTMLFormElement;
const chatInputEl = document.getElementById("chat-input") as HTMLTextAreaElement;
const chatSendBtnEl = document.getElementById("chat-send-btn") as HTMLButtonElement;
const chatSessionMenuBtnEl = document.getElementById("chat-session-menu-btn") as HTMLButtonElement;
const chatSessionMenuLabelEl = document.getElementById("chat-session-menu-label")!;

const chatPanel = mountChatPanel({
  appEl,
  regionEl: chatRegionEl,
  handleEl: chatHandleEl,
  collapseBtnEl: chatCollapseBtnEl,
  sessionMenuBtnEl: chatSessionMenuBtnEl,
  sessionMenuLabelEl: chatSessionMenuLabelEl,
  panelEl: discoveryPanelEl,
  transcriptEl: chatTranscriptEl,
  formEl: chatFormEl,
  inputEl: chatInputEl,
  sendBtnEl: chatSendBtnEl,
  onResizePersist: (fraction) => {
    if (!vaultIsOpen) return;
    void persistSetting("vault", "vault.chat_height", fraction);
  },
  // status: chat-panel-note-link-render
  // status: editor-preview-tab-from-open-callsites
  onOpenNoteLink: (rel) => {
    void openFile(rel, { preview: true });
  },
  // status: chat-active-note-context-injection
  // Pull the live editor text from the open buffer; preview-mode buffers
  // (trash / snapshot / mutation) deliberately don't inject — they're
  // derived views, not the user's working note.
  getActiveNote: () => {
    if (!buffer || isReadOnlyBuffer(buffer)) return null;
    return { relPath: buffer.path, bufferText: view.state.doc.toString() };
  },
  // status: chat-input-at-selection
  // Pull the active editor's current selection. Empty selection → null;
  // preview-mode buffers also return null since they're derived views.
  getActiveSelection: () => {
    if (!buffer || isReadOnlyBuffer(buffer)) return null;
    const { from, to } = view.state.selection.main;
    if (from === to) return null;
    const text = view.state.sliceDoc(from, to);
    if (!text.trim()) return null;
    const startLine = view.state.doc.lineAt(from).number;
    const endLine = view.state.doc.lineAt(to).number;
    const lineRange =
      startLine === endLine ? `L${startLine}` : `L${startLine}-L${endLine}`;
    return { relPath: buffer.path, text, lineRange };
  },
  toast: (message) => showToast(message, undefined, 6000),
});

/// Discriminated union of buffer modes. `file` is the normal editable
/// buffer; the other two are read-only previews. Bundling per-mode state
/// onto the variant makes invalid combinations (e.g. a trash buffer with
/// a snapshot row) unrepresentable, and lets save / dirty / status code
/// narrow once via `mode.kind`.
type BufferMode =
  | { kind: "file" }
  | { kind: "trash"; displayPath: string }
  | {
      kind: "snapshot";
      row: ChangeRow;
      changeId: number;
      /// status: snapshot-preview-diff-toggle
      /// True when the snapshot's CM6 view currently renders the diff vs
      /// current rather than the snapshot blob.
      diffActive: boolean;
    };

interface Buffer {
  path: string;
  loadedText: string;
  loadedHash: string;
  mode: BufferMode;
  /// status: note-mutation-stash-changes-tag
  /// One-shot stash consumed by the next save: when a mutation lands on
  /// the buffer (`note-mutation-applies-as-buffer-edit`) we set
  /// `{ mutation: "<kind>" }` so the resulting `'modified'` `core::changes`
  /// row carries `metadata.mutation`. Cleared post-save; subsequent saves
  /// don't carry it.
  pendingChangesMetadata: Record<string, unknown> | null;
  /// status: editor-preview-tab
  /// True while this buffer occupies the single preview slot. Promoted
  /// to false (= sticky) on first edit, double-click of the tab, drag,
  /// or "Keep open" right-click verb. Preview tabs are *never dirty* by
  /// construction — the moment a doc-changing transaction lands the
  /// promotion fires before the dirty check, so the existing dirty-
  /// buffer machinery ignores this field entirely.
  preview: boolean;
}

let buffer: Buffer | null = null;

// status: editor-tab-strip, multi-buffer-in-memory-only
// Open file-mode buffers, keyed by vault-relative path. The active one
// is mirrored into `buffer` above so existing single-buffer call sites
// (save / mutations / view menu / status bar) keep reading `buffer`
// directly. Per-buffer EditorState (`savedState`) is captured on tab
// switch so undo history / selection / scroll persist.
//
// Snapshot / trash buffers are *transient previews* on top of the
// active tab — they don't get their own tab entry. When a preview
// opens, we stash the current file buffer's state into the registry
// (so closing the preview restores it), then point `buffer` at the
// transient preview shape.
interface OpenBufferEntry {
  buffer: Buffer;
  /// CM6 state captured at last tab-deactivate. `null` until the user
  /// switches away from this tab for the first time.
  savedState: EditorState | null;
  /// Order of last activation; drives "switch to most recent" on close.
  lastActivatedAt: number;
}
const openBuffers = new Map<string, OpenBufferEntry>();
let activePath: string | null = null;
let activationCounter = 0;

// status: editor-tab-strip
// status: editor-preview-tab
// At most one preview tab exists at a time. Holds the path of the
// currently-previewed buffer or `null` when no preview slot is in use.
// Cleared on promotion, on close of the preview tab, or on vault swap.
let previewTabPath: string | null = null;

// status: note-mutation-buffer-ro-while-in-flight
// Source paths with an active or leased `NoteMutation` task. Populated
// by `mountMutationsMenu`'s `onInFlightChanged` hook driven off
// `hiker:queue-event`. The active buffer is set RO while its path is
// in this set; cleared from terminal events.
const inFlightMutationPaths = new Set<string>();

// Forward-declared so the early top-level `updateStatus()` paint can safely
// call `mutationsMenu?.refreshButtonState()` before the actual mount runs
// further down. Without the forward declaration, `mutationsMenu` is in TDZ
// at the first paint and *any* reference (including `?.`) throws — the
// throw aborts module init mid-file, which used to manifest as
// "Cannot access 'taskQueueTile' before initialization" downstream because
// the rest of the module never ran.
let mutationsMenu: MutationsMenuApi | null = null;
// Same forward-decl shape as `mutationsMenu` above: the initial top-level
// `updateStatus()` paint runs before either mount, so a `const` reference
// would TDZ-throw and abort module init. `let` + null seed lets the early
// callers no-op safely; the actual mounts assign these further down.
let modeControls: ModeControlsApi | null = null;
let dirtyBufferDiff: DirtyBufferDiffApi | null = null;
let tabStrip: TabStripApi | null = null;
// status: navigation-history-stack
// Forward-declared so transition sites (`activateTabInner`, `openFile`,
// `closeTab`, etc.) can call `nav?.checkpoint()` before the actual mount
// runs further down. Same TDZ-avoidance shape as `mutationsMenu` above.
let nav: NavApi | null = null;
function checkpointNav(): void {
  nav?.checkpoint();
}

/// True for any read-only preview buffer (trash / snapshot). Most code paths
/// share the "no save, no dirty state, switch without prompt" behavior;
/// trash-specific or snapshot-specific UI narrows on `mode.kind` directly.
function isReadOnlyBuffer(b: Buffer | null): boolean {
  return !!(b && b.mode.kind !== "file");
}

const language = new Compartment();
const readOnlyCompartment = new Compartment();
// status: live-preview-default-on
// Live preview rides its own compartment so the View menu's eventual
// `view-live-preview-toggle` can flip it without touching the language
// compartment. The two are still coupled at file-open: a non-md buffer
// reconfigures the live-preview slot to `[]` regardless of the toggle, so
// raw text never picks up md decorations. Default is on.
const livePreviewCompartment = new Compartment();
let livePreviewEnabled = true;
// status: view-show-chunk-boundaries
// Dedicated compartment so the View menu's toggle can swap the extension
// without touching language / live-preview state. Default off — this is a
// debugging-grade view; users opt in.
const chunkBoundariesCompartment = new Compartment();
let chunkBoundariesEnabled = false;
let chunkBoundariesRequestSeq = 0;
let chunkBoundariesDebounce: number | null = null;

// status: view-hide-frontmatter-toggle
// Dedicated compartment so the View menu can flip frontmatter folding
// without touching language / live-preview state. Default off — most users
// hand-curate their frontmatter and want it visible. Useful primarily when
// agent stamps (mcp-tool-set-frontmatter, apply_tag) accumulate enough
// fields to push body content off-screen.
const hideFrontmatterCompartment = new Compartment();
let hideFrontmatterEnabled = false;

// status: view-show-whitespace-toggle
// Default off. CM6's `highlightWhitespace` is a single extension wired into
// its own compartment so the View menu can flip it without touching anything
// else. Renders space/tab markers via the standard `cm-highlightSpace` /
// `cm-highlightTab` classes; theming is whatever CM6 ships.
const whitespaceCompartment = new Compartment();
let whitespaceEnabled = false;

// status: view-line-numbers-toggle
// `basicSetup` already includes the line-number gutter, so the toggle hides
// it via a CSS class on the editor root rather than swapping extensions —
// avoids fighting basicSetup's facet wiring. Default visible (matches
// basicSetup's default behavior).
let lineNumbersVisible = true;

// Hoisted above any top-level `updateStatus()` call: `renderIndexStatus`
// (invoked from `updateStatus`) reads both, and the first `updateStatus()`
// fires during module init before its original declaration site below.
// TDZ-throwing here would halt module init and skip every subsequent
// `listen(...)` registration, breaking the indexing label and progress
// handling.
let indexStatus: IndexStatus = {
  model_ready: false,
  queued: 0,
  total_notes: 0,
  last_error: null,
};
// scan_complete adds, every terminal event subtracts, Started is a no-op.
let outstandingCount = 0;
// Hoisted so `renderIndexStatus` (called during early init) can blank the
// index label before any vault is opened. The original declaration site is
// further down with the rest of the background-interval state.
let vaultIsOpen = false;

// status: txt-render-as-markdown-default
// Per-vault default loaded from `editor.render_txt_as_markdown` via
// `settings-vault-config-toml`. Flips during a session via
// `view-render-txt-as-markdown-toggle`; the change persists through
// `settings-write-back`.
let renderTxtAsMarkdown = true;

// status: view-word-wrap-toggle
// CM6's `EditorView.lineWrapping` is reconfigured via this compartment so
// the View menu can flip wrap state without rebuilding the editor. Default
// is loaded from `editor.word_wrap`.
const wordWrapCompartment = new Compartment();
let wordWrapEnabled = true;

function isMarkdownPath(rel: string): boolean {
  const ext = rel.split(".").pop()?.toLowerCase() ?? "";
  if (ext === "md" || ext === "markdown") return true;
  if (ext === "txt" && renderTxtAsMarkdown) return true;
  return false;
}

function languageExtensionForPath(rel: string): Extension {
  return isMarkdownPath(rel) ? markdown({ base: markdownLanguage }) : [];
}

function livePreviewExtensionForPath(rel: string): Extension {
  // status: live-preview-disabled-non-md
  // Non-markdown buffers always reconfigure to `[]` regardless of the toggle,
  // so raw text / future formats never pick up md decorations. The toggle
  // only governs whether live preview applies *when* the buffer is markdown.
  if (!isMarkdownPath(rel)) return [];
  return livePreviewEnabled ? livePreview() : [];
}

async function fetchAndApplyChunkBounds(rel: string): Promise<void> {
  // Per editor.md: "When the file isn't indexed (unsupported / skipped /
  // queued), toggling on shows nothing and a faint hint in the gutter
  // explains why." The index-state probe runs first; only Indexed files
  // get the chunks_for round trip.
  const seq = ++chunkBoundariesRequestSeq;
  let state;
  try {
    const ix = await invoke<IndexState>("index_state_for", { rel });
    if (seq !== chunkBoundariesRequestSeq) return;
    if (ix.kind === "unsupported") {
      state = chunkBoundariesHintState("unsupported file type");
    } else if (ix.kind === "skipped") {
      state = chunkBoundariesHintState(`skipped — ${ix.reason}`);
    } else if (ix.kind === "queued") {
      state = chunkBoundariesHintState("queued for indexing");
    } else {
      const bounds = await invoke<ChunkBounds[]>("chunks_for", { rel });
      if (seq !== chunkBoundariesRequestSeq) return;
      state = bounds.length === 0
        ? chunkBoundariesHintState("no chunks yet")
        : chunkBoundsToState(view, bounds);
    }
  } catch (err) {
    console.error("chunk_boundaries fetch failed:", err);
    state = chunkBoundariesHintState("error loading chunks");
  }
  if (seq !== chunkBoundariesRequestSeq) return;
  view.dispatch({ effects: setChunkBoundaries.of(state) });
}

function refreshChunkBoundaries(): void {
  if (!chunkBoundariesEnabled) {
    view.dispatch({ effects: setChunkBoundaries.of(clearChunkBoundariesState()) });
    return;
  }
  const rel = buffer?.path;
  if (!rel || isReadOnlyBuffer(buffer)) {
    view.dispatch({ effects: setChunkBoundaries.of(clearChunkBoundariesState()) });
    return;
  }
  void fetchAndApplyChunkBounds(rel);
}

function scheduleChunkBoundariesRefresh(delayMs: number): void {
  if (chunkBoundariesDebounce !== null) {
    window.clearTimeout(chunkBoundariesDebounce);
  }
  chunkBoundariesDebounce = window.setTimeout(() => {
    chunkBoundariesDebounce = null;
    refreshChunkBoundaries();
  }, delayMs);
}

export function setChunkBoundariesEnabled(on: boolean): void {
  chunkBoundariesEnabled = on;
  view.dispatch({
    effects: chunkBoundariesCompartment.reconfigure(on ? chunkBoundaries() : []),
  });
  refreshChunkBoundaries();
}

export function setHideFrontmatterEnabled(on: boolean): void {
  hideFrontmatterEnabled = on;
  view.dispatch({
    effects: hideFrontmatterCompartment.reconfigure(on ? hideFrontmatter() : []),
  });
}

export function setWhitespaceEnabled(on: boolean): void {
  whitespaceEnabled = on;
  view.dispatch({
    effects: whitespaceCompartment.reconfigure(on ? highlightWhitespace() : []),
  });
}

export function setLineNumbersVisible(on: boolean): void {
  lineNumbersVisible = on;
  view.dom.classList.toggle("hide-line-numbers", !on);
}

export function setWordWrapEnabled(on: boolean): void {
  wordWrapEnabled = on;
  view.dispatch({
    effects: wordWrapCompartment.reconfigure(on ? EditorView.lineWrapping : []),
  });
}

// status: view-render-txt-as-markdown-toggle
// Session-scope override of the per-vault default. Reconfigures the
// language and live-preview compartments for the *currently open* buffer
// so the user sees the change immediately for the file in front of them;
// future opens pick the new mode up via `languageExtensionForPath`.
export function setRenderTxtAsMarkdown(on: boolean): void {
  renderTxtAsMarkdown = on;
  const rel = buffer?.path;
  if (!rel) return;
  view.dispatch({
    effects: [
      language.reconfigure(languageExtensionForPath(rel)),
      livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
    ],
  });
}

export function setLivePreviewEnabled(on: boolean): void {
  // Hook for the future View menu's `view-live-preview-toggle`. Current
  // buffer's md-ness still gates whether the extension actually applies.
  livePreviewEnabled = on;
  const rel = buffer?.path;
  if (!rel) return;
  view.dispatch({
    effects: livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
  });
}

function isDirty(): boolean {
  if (!buffer || isReadOnlyBuffer(buffer)) return false;
  return view.state.doc.toString() !== buffer.loadedText;
}

function updateStatus(): void {
  const dirty = isDirty();
  // status: editor-preview-tab-promotion
  // Preview tabs are never dirty by construction. Any code path that
  // produces dirty state — user typing, mutation apply, programmatic
  // doc swap that doesn't reset loadedText — promotes the preview to
  // sticky. Keeps the invariant downstream (file-switch-guard-dirty,
  // window-close guard, etc.) without those sites needing to know.
  if (dirty) promotePreviewIfActive();
  const isTrash = buffer?.mode.kind === "trash";
  const isSnap = buffer?.mode.kind === "snapshot";
  const path =
    buffer?.mode.kind === "trash"
      ? buffer.mode.displayPath
      : (buffer?.path ?? "");
  const titleSuffix = isTrash ? " (in trash)" : isSnap ? " (snapshot)" : "";
  document.title =
    (dirty ? "• " : "") + (path ? `Hiker — ${path}${titleSuffix}` : "Hiker");
  // status: status-bar-path-basename-tooltip
  let basename = path ? (path.split("/").pop() ?? path) : "";
  if (isTrash) basename += " (in trash)";
  else if (isSnap) basename += " (snapshot)";
  statusPathEl.replaceChildren(document.createTextNode(basename));
  if (buffer?.mode.kind === "snapshot") {
    const idEl = document.createElement("span");
    idEl.className = "status-snapshot-id";
    idEl.textContent = `#${buffer.mode.changeId}`;
    idEl.title = `Snapshot id ${buffer.mode.changeId}`;
    statusPathEl.appendChild(idEl);
  }
  statusPathEl.title = isTrash ? (buffer as Buffer).path : path;
  // status: status-bar-path-reveal — clickable when a real (non-trash) file
  // is open. Trash-preview paths live under `.hiker/trash/` and revealing
  // them would expose internal state, so the gesture is suppressed there.
  // Snapshot previews share the live file's path so reveal stays sensible.
  const revealable = !!buffer && !isTrash;
  statusPathEl.classList.toggle("clickable", revealable);
  statusPathEl.style.cursor = revealable ? "pointer" : "";
  saveBtn.disabled = !buffer || !dirty || isReadOnlyBuffer(buffer);
  refreshDiffButton();

  const sel = view.state.selection.main;
  const line = view.state.doc.lineAt(sel.head);
  const col = sel.head - line.from + 1;
  statusCursorEl.textContent = `${line.number}:${col}`;
  const text = view.state.doc.toString();
  const words = text.trim() === "" ? 0 : text.trim().split(/\s+/).length;
  statusWordsEl.textContent = `${words} word${words === 1 ? "" : "s"}`;

  // status: dirty-tree-dot
  // Only the *active* buffer's LI carries the dirty class. Drop it from
  // any other LIs first — without this, switching tabs / opening another
  // note can leave a stale `.dirty` class on the outgoing tab's row
  // (`view.setState` + the follow-up effects dispatch fire `updateStatus`
  // while `buffer` still points at the outgoing tab, so `isDirty()` is
  // briefly true against `view.state.doc` containing the *target's*
  // content vs the outgoing buffer's `loadedText` — that one tick stamps
  // the wrong row).
  document.querySelectorAll("#tree li.dirty").forEach((el) => {
    if (!buffer || el.getAttribute("data-path") !== buffer.path) {
      el.classList.remove("dirty");
    }
  });
  if (buffer) {
    const li = document.querySelector(`#tree li[data-path="${cssEscape(buffer.path)}"]`);
    li?.classList.toggle("dirty", dirty);
  }
  // Center status label mirrors the active buffer's index state.
  renderIndexStatus();
  // status: note-mutations-menu
  // The wand button's enabled state depends on the active buffer's
  // path / mode / extension / dirtiness — re-evaluate whenever the
  // status bar refreshes (covers buffer swap, save, dirty toggle, mode
  // entry/exit). `mutationsMenu` is forward-declared with `let` because
  // the initial top-level `updateStatus()` paint runs before the mount;
  // a `const` reference would throw on the TDZ access and abort module
  // init (taking `taskQueueTile` and the rest of the file down with it).
  mutationsMenu?.refreshButtonState();
  // status: editor-diff-vs-disk-toggle
  // Mode-controls re-renders the `file`-mode renderer (which shows the
  // dirty-buffer Diff toggle when isDirty) on every doc edit so the
  // toggle appears/disappears with the dirty flag. Cheap (replaceChildren).
  // If the buffer goes clean while the diff is on, force the toggle off
  // so the editor returns to its live editable state.
  if (
    !dirty
    && buffer?.mode.kind === "file"
    && dirtyBufferDiff?.isActive()
  ) {
    dirtyBufferDiff?.forceOff();
  }
  modeControls?.render();
  // status: editor-tab-dirty-marker — tab strip mirrors per-tab dirty state.
  tabStrip?.render();
}

const statusUpdater = ViewPlugin.fromClass(
  class {
    update(u: ViewUpdate) {
      void u;
      updateStatus();
    }
  },
);

register({
  id: "editor.save",
  keys: "Mod-s",
  label: "Save current buffer",
  run: () => {
    void save();
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
    discovery.focusInput();
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
    void chatPanel.newSession();
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
    if (activePath) void closeTab(activePath);
    return true;
  },
});
register({
  id: "tab.next",
  keys: "Ctrl-Tab",
  label: "Next tab",
  run: () => {
    cycleTab(+1);
    return true;
  },
});
register({
  id: "tab.previous",
  keys: "Ctrl-Shift-Tab",
  label: "Previous tab",
  run: () => {
    cycleTab(-1);
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
      jumpToTab(idx);
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
    void nav?.back();
    return true;
  },
});
register({
  id: "navigation.forward",
  keys: "Mod-]",
  label: "Navigate forward",
  run: () => {
    void nav?.forward();
    return true;
  },
});
validate();

const view = new EditorView({
  parent: editorEl,
  state: EditorState.create({
    doc: "",
    extensions: [
      basicSetup,
      wordWrapCompartment.of(EditorView.lineWrapping),
      language.of(markdown()),
      livePreviewCompartment.of(livePreview()),
      chunkBoundariesCompartment.of([]),
      hideFrontmatterCompartment.of([]),
      whitespaceCompartment.of([]),
      readOnlyCompartment.of(EditorState.readOnly.of(false)),
      ...diffExtensions(),
      statusUpdater,
      toCMKeymap(),
    ],
  }),
});

saveBtn.addEventListener("click", async () => {
  const ok = await save();
  if (ok) {
    discovery.scheduleRelatedRefresh(buffer?.path ?? null, 500);
    scheduleChunkBoundariesRefresh(500);
  }
});

// status: editor-diff-vs-disk-toggle
// Toolbar diff button. Click toggles the only currently-implemented diff
// target ("on-disk"); right-click opens a target-picker menu so future
// targets (chunk boundaries, last save, snapshot, …) can slot in here
// without splitting buttons. Greyed when there's nothing to diff against
// — buffer not editable, file not on disk, or buffer clean.
function diffButtonAvailable(): boolean {
  if (!buffer || buffer.mode.kind !== "file") return false;
  if (buffer.loadedHash === "") return false;
  return isDirty();
}
function refreshDiffButton(): void {
  const available = diffButtonAvailable();
  const active = dirtyBufferDiff?.isActive() ?? false;
  diffBtn.disabled = !available && !active;
  diffBtn.classList.toggle("active", active);
  diffBtn.title = active
    ? "Hide diff"
    : available
      ? "Diff vs on-disk"
      : "Nothing to diff";
}
diffBtn.addEventListener("click", () => {
  if (diffBtn.disabled) return;
  void dirtyBufferDiff?.toggle();
});
diffBtn.addEventListener("contextmenu", (ev) => {
  ev.preventDefault();
  const available = diffButtonAvailable();
  const active = dirtyBufferDiff?.isActive() ?? false;
  const items: CtxMenuItem[] = [
    {
      label: active ? "Hide diff" : "Diff against on-disk",
      disabled: !available && !active,
      run: () => void dirtyBufferDiff?.toggle(),
    },
  ];
  openContextMenu(ev.clientX, ev.clientY, items);
});

// status: snapshot-preview-mode
// Mount the snapshot-preview module. Hosted state — `buffer`, the CM6 view,
// the dirty/save flow, render-mode-controls — flow in via the deps; the
// module owns the diff-toggle in-flight guard and orchestrates the open /
// close / toggle / restore lifecycle.
const snapshotPreview: SnapshotPreviewApi = mountSnapshotPreview({
  view,
  getBuffer: () => buffer,
  setBuffer: (b) => {
    buffer = b as Buffer | null;
  },
  language,
  livePreviewCompartment,
  hideFrontmatterCompartment,
  languageExtensionForPath,
  livePreviewExtensionForPath,
  getHideFrontmatterEnabled: () => hideFrontmatterEnabled,
  setReadOnly,
  updateStatus,
  refreshChunkBoundaries,
  renderModeControls: () => modeControls?.render(),
  isDirty,
  save,
  // Returning to the activity detail view if it's where the user came from;
  // otherwise fall back to the home overview.
  onClose: () => {
    vaultHome.setVisible(true);
    if (vaultHome.activeDetailView()?.kind !== "recent-activity") {
      vaultHome.showDetail("recent-activity");
    }
  },
  onRestore: (row) => vaultHome.doRestoreSnapshot(row),
  isVaultHomeVisible: () => vaultHome.isVisible(),
  setVaultHomeVisible: (on) => vaultHome.setVisible(on),
  formatError,
});

async function save(): Promise<boolean> {
  if (!buffer) return false;
  const contents = view.state.doc.toString();
  // status: note-mutation-stash-changes-tag
  // One-shot consume of the stash on a successful save; cleared post-
  // success so subsequent saves are tagless. Errors leave the stash in
  // place so a retry (e.g. after a drift-conflict "Keep mine") still
  // carries the tag.
  const stash = buffer.pendingChangesMetadata;
  try {
    const newHash = await invoke<string>("write_file_checked", {
      rel: buffer.path,
      expectedHash: buffer.loadedHash,
      contents,
      extraMetadata: stash,
    });
    buffer.loadedText = view.state.doc.toString();
    buffer.loadedHash = newHash;
    buffer.pendingChangesMetadata = null;
    updateStatus();
    return true;
  } catch (err) {
    return await handleSaveError(err);
  }
}

async function handleSaveError(err: unknown): Promise<boolean> {
  if (!buffer) return false;
  const e = err as { kind?: string; message?: unknown } | string;
  const kind = typeof e === "object" ? e.kind : undefined;
  if (kind === "disk_drift") {
    const choice = await confirm3(
      `${buffer.path} has changed on disk since you opened it.`,
      "Keep mine (overwrite disk)",
      "Take theirs (discard my edits)",
      "Cancel",
    );
    if (choice === "a") {
      const probe = await invoke<FileWithHash>("read_file_with_hash", { rel: buffer.path });
      buffer.loadedHash = probe.hash;
      return await save();
    }
    if (choice === "b") {
      const fresh = await invoke<FileWithHash>("read_file_with_hash", { rel: buffer.path });
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: fresh.contents } });
      buffer.loadedText = view.state.doc.toString();
      buffer.loadedHash = fresh.hash;
      // Discarding our edits also discards any pending mutation tag —
      // the bytes about to be persistent on the next save are disk's,
      // not the mutation's.
      buffer.pendingChangesMetadata = null;
      return true;
    }
    return false;
  }
  console.error("save failed:", err);
  alert(`save failed: ${typeof e === "string" ? e : JSON.stringify(e)}`);
  return false;
}

// status: multi-buffer-tree-click-switches-tab
// status: editor-tab-strip
// Tab activation. Saves the previously-active file buffer's CM6 state
// (so undo history / selection / scroll persist across tab switches)
// then restores the target buffer's content + state. If `target` has no
// saved state yet (freshly opened, never switched away from), we
// dispatch its loadedText into the live state instead of `setState`.
function activateTabInner(rel: string): void {
  const target = openBuffers.get(rel);
  if (!target) return;
  // Persist the outgoing tab's state.
  if (buffer && buffer.mode.kind === "file") {
    const out = openBuffers.get(buffer.path);
    if (out) out.savedState = view.state;
  }
  resetDiffDecorations(view);
  if (target.savedState) {
    view.setState(target.savedState);
    // setState restores compartments to whatever the target's saved
    // state had; we re-apply path-dependent + global compartments so
    // toggles (live preview, hide-frontmatter, word wrap, etc.) reflect
    // the user's *current* preferences rather than the snapshotted ones.
    view.dispatch({
      effects: [
        language.reconfigure(languageExtensionForPath(target.buffer.path)),
        livePreviewCompartment.reconfigure(
          livePreviewEnabled ? livePreviewExtensionForPath(target.buffer.path) : [],
        ),
        hideFrontmatterCompartment.reconfigure(
          hideFrontmatterEnabled ? hideFrontmatter() : [],
        ),
        readOnlyCompartment.reconfigure(
          EditorState.readOnly.of(inFlightMutationPaths.has(target.buffer.path)),
        ),
      ],
    });
  } else {
    // First activation — dispatch loadedText into the existing state.
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: target.buffer.loadedText },
      effects: [
        language.reconfigure(languageExtensionForPath(target.buffer.path)),
        livePreviewCompartment.reconfigure(
          livePreviewEnabled ? livePreviewExtensionForPath(target.buffer.path) : [],
        ),
        hideFrontmatterCompartment.reconfigure(
          hideFrontmatterEnabled ? hideFrontmatter() : [],
        ),
        readOnlyCompartment.reconfigure(
          EditorState.readOnly.of(inFlightMutationPaths.has(target.buffer.path)),
        ),
      ],
    });
    // The dispatch normalized loadedText through CM's doc — re-read so
    // isDirty() doesn't immediately flag the buffer dirty after open.
    target.buffer.loadedText = view.state.doc.toString();
  }
  buffer = target.buffer;
  activePath = rel;
  target.lastActivatedAt = ++activationCounter;
  if (vaultHome.isVisible()) vaultHome.setVisible(false);
  if (settingsPane.isVisible()) void settingsPane.setVisible(false);
  document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
  document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
  void revealInTree(rel);
  updateStatus();
  refreshChunkBoundaries();
  tabStrip?.render();
  checkpointNav();
  // status: note-access-tracking
  invoke("note_accessed", { rel }).catch((err) => {
    console.error("note_accessed failed:", err);
  });
}

// status: editor-preview-tab
// Swap the currently-previewed tab's buffer in place. Same tab DOM
// node, same activation order; only the path / contents / loadedHash
// change. The previously-previewed buffer drops from `openBuffers`
// under its old key (no other tab references it).
async function replacePreviewWith(newRel: string): Promise<void> {
  const oldPath = previewTabPath!;
  const file = await invoke<FileWithHash>("read_file_with_hash", { rel: newRel });
  const entry = openBuffers.get(oldPath);
  if (!entry) {
    // Stale state — fall back to the normal open path.
    previewTabPath = null;
    return;
  }
  resetDiffDecorations(view);
  buffer = null;
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: file.contents },
    effects: [
      language.reconfigure(languageExtensionForPath(newRel)),
      livePreviewCompartment.reconfigure(livePreviewExtensionForPath(newRel)),
      readOnlyCompartment.reconfigure(
        EditorState.readOnly.of(inFlightMutationPaths.has(newRel)),
      ),
    ],
  });
  const replaced: Buffer = {
    path: newRel,
    loadedText: view.state.doc.toString(),
    loadedHash: file.hash,
    mode: { kind: "file" },
    pendingChangesMetadata: null,
    preview: true,
  };
  openBuffers.delete(oldPath);
  openBuffers.set(newRel, {
    buffer: replaced,
    // Discard the prior buffer's savedState — it belongs to a different
    // file's content and would clobber the new doc on activation.
    savedState: null,
    lastActivatedAt: ++activationCounter,
  });
  previewTabPath = newRel;
  buffer = replaced;
  activePath = newRel;
  if (vaultHome.isVisible()) vaultHome.setVisible(false);
  if (settingsPane.isVisible()) void settingsPane.setVisible(false);
  document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
  document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
  await revealInTree(newRel);
  updateStatus();
  refreshChunkBoundaries();
  tabStrip?.render();
  // status: navigation-history-stack — preview-replace prunes the
  // displaced path from history so back/forward never tries to revive it.
  nav?.pruneTab(oldPath);
  checkpointNav();
  invoke("note_accessed", { rel: newRel }).catch((err) => {
    console.error("note_accessed failed:", err);
  });
}

// status: editor-preview-tab-promotion
// Flip the active preview tab to sticky. Idempotent — safe to call
// every doc-change tick. Re-renders the tab strip so the italic clears.
function promotePreviewIfActive(): void {
  if (!buffer || buffer.mode.kind !== "file" || !buffer.preview) return;
  buffer.preview = false;
  if (previewTabPath === buffer.path) previewTabPath = null;
  tabStrip?.render();
}

// status: editor-preview-tab-promotion
// Promote a specific preview tab (by path) to sticky. Used by the
// double-click and "Keep open" tab-context-menu paths so the user can
// promote a tab they aren't actively editing.
function promotePreviewByPath(rel: string): void {
  const entry = openBuffers.get(rel);
  if (!entry || !entry.buffer.preview) return;
  entry.buffer.preview = false;
  if (previewTabPath === rel) previewTabPath = null;
  tabStrip?.render();
}

async function openFile(
  rel: string,
  opts?: { preview?: boolean },
): Promise<void> {
  const wantPreview = opts?.preview === true;
  // status: multi-buffer-tree-click-switches-tab
  // If a tab for this path is already open, switch to it — never reload
  // from disk (would clobber any unsaved edits or in-buffer mutation).
  if (openBuffers.has(rel)) {
    activateTabInner(rel);
    return;
  }
  // status: editor-preview-tab
  // If a preview tab is open and the caller wants the preview slot,
  // replace the existing preview's buffer in place rather than spawning
  // a new tab. The tab DOM node + tab key in `openBuffers` persists
  // (after a path remap) so the slot reads as the same tab to the user.
  // No dirty guard: preview tabs are never dirty by construction.
  if (wantPreview && previewTabPath !== null && previewTabPath !== rel) {
    try {
      await replacePreviewWith(rel);
    } catch (err) {
      console.error("openFile (preview replace) failed:", rel, err);
      alert(`open failed: ${err}`);
    }
    return;
  }
  // status: multi-buffer-no-switch-guard — no dirty-modal on tab open.
  // Switching tabs leaves the prior buffer dirty in memory. The guard
  // only fires on explicit close (× / Cmd-W) or window-close.
  try {
    const file = await invoke<FileWithHash>("read_file_with_hash", { rel });
    // Persist outgoing tab's state before we dispatch into the view.
    if (buffer && buffer.mode.kind === "file") {
      const out = openBuffers.get(buffer.path);
      if (out) out.savedState = view.state;
    }
    resetDiffDecorations(view);
    buffer = null;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: file.contents },
      effects: [
        language.reconfigure(languageExtensionForPath(rel)),
        livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
        readOnlyCompartment.reconfigure(
          EditorState.readOnly.of(inFlightMutationPaths.has(rel)),
        ),
      ],
    });
    // Compare against CM's canonical doc representation (CRLF normalized),
    // not the raw file string, or the buffer reads dirty on open.
    const newBuf: Buffer = {
      path: rel,
      loadedText: view.state.doc.toString(),
      loadedHash: file.hash,
      mode: { kind: "file" },
      pendingChangesMetadata: null,
      preview: wantPreview,
    };
    openBuffers.set(rel, {
      buffer: newBuf,
      savedState: null,
      lastActivatedAt: ++activationCounter,
    });
    if (wantPreview) previewTabPath = rel;
    buffer = newBuf;
    activePath = rel;
    if (vaultHome.isVisible()) vaultHome.setVisible(false);
    if (settingsPane.isVisible()) void settingsPane.setVisible(false);
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
    await revealInTree(rel);
    updateStatus();
    refreshChunkBoundaries();
    tabStrip?.render();
    checkpointNav();
    invoke("note_accessed", { rel }).catch((err) => {
      console.error("note_accessed failed:", err);
    });
  } catch (err) {
    console.error("openFile failed:", rel, err);
    alert(`open failed: ${err}`);
  }
}

// status: editor-tab-strip
// Close a tab. If dirty, fires the existing close-time confirm3 modal
// (file-switch-guard-dirty's surviving entry point per multi-buffer-no-
// switch-guard). On confirmed close, removes the entry from the
// registry; if it was active, activates the most-recently-used remaining
// tab (or clears the editor when none remain).
async function closeTab(rel: string): Promise<void> {
  const entry = openBuffers.get(rel);
  if (!entry) return;
  const isActive = activePath === rel;
  const dirty = isActive ? isDirty() :
    entry.buffer.loadedText !==
      (entry.savedState?.doc.toString() ?? entry.buffer.loadedText);
  if (dirty) {
    // We need the dirty buffer's edits visible in the modal context —
    // the user wants to see what they're saving/discarding. If it's
    // not the active tab, switch to it first so the editor shows the
    // pending content while the modal is up.
    if (!isActive) activateTabInner(rel);
    const choice = await confirm3(
      `${rel} has unsaved changes.`,
      "Save & close",
      "Discard & close",
      "Cancel",
    );
    if (choice === "cancel") return;
    if (choice === "a") {
      const ok = await save();
      if (!ok) return;
    }
  }
  openBuffers.delete(rel);
  // status: editor-preview-tab — clear the slot if we just closed it.
  if (previewTabPath === rel) previewTabPath = null;
  // status: navigation-history-stack — drop history entries pointing at
  // the closed tab so back/forward never tries to revive a vanished buffer.
  nav?.pruneTab(rel);
  if (activePath === rel) {
    // Pick the most-recently-used remaining tab.
    let next: string | null = null;
    let bestSeen = -1;
    for (const [p, e] of openBuffers) {
      if (e.lastActivatedAt > bestSeen) {
        bestSeen = e.lastActivatedAt;
        next = p;
      }
    }
    if (next) {
      activateTabInner(next);
    } else {
      // No tabs left — clear the editor. Mirror what the existing
      // delete-from-tree path does.
      buffer = null;
      activePath = null;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
      setReadOnly(false);
      vaultHome.setVisible(true);
      updateStatus();
    }
  }
  tabStrip?.render();
  checkpointNav();
}

// status: editor-tab-keybinds
function cycleTab(delta: 1 | -1): void {
  const order = [...openBuffers.keys()];
  if (order.length === 0) return;
  const idx = activePath ? order.indexOf(activePath) : -1;
  const next =
    idx < 0
      ? order[0]
      : order[(idx + delta + order.length) % order.length];
  activateTabInner(next);
}

function jumpToTab(n: number): void {
  const order = [...openBuffers.keys()];
  if (order.length === 0) return;
  // Cmd/Ctrl-9 jumps to the last tab regardless of count (browser
  // convention) per editor-tab-keybinds.
  const idx = n === 9 ? order.length - 1 : Math.min(n - 1, order.length - 1);
  if (idx < 0) return;
  activateTabInner(order[idx]);
}

function tabSnapshots(): {
  path: string;
  basename: string;
  folder: string;
  dirty: boolean;
  preview: boolean;
}[] {
  const out: {
    path: string;
    basename: string;
    folder: string;
    dirty: boolean;
    preview: boolean;
  }[] = [];
  for (const [path, entry] of openBuffers) {
    const slash = path.lastIndexOf("/");
    const basename = slash >= 0 ? path.slice(slash + 1) : path;
    const folder = slash >= 0 ? path.slice(0, slash) : "";
    const isActive = path === activePath && buffer?.mode.kind === "file";
    const dirty = isActive
      ? isDirty()
      : entry.savedState !== null
        ? entry.savedState.doc.toString() !== entry.buffer.loadedText
        : false;
    out.push({ path, basename, folder, dirty, preview: entry.buffer.preview });
  }
  return out;
}

const cssEscape = (s: string): string => CSS.escape(s);

// status: tree-* (see ./tree)
// Sidebar tree owns its own state (expanded folders, sort order, debounce,
// index-state cache) inside the module; host wires DOM ids and editor-coupled
// callbacks via deps. The wrapper functions below preserve the old call-site
// shape (`refreshTree`, `revealInTree`, `scheduleTreeRefreshFromWatcher`).
const tree: TreeApi = mountTree({
  treeEl,
  newNoteBtn,
  treeActionsBtn,
  cssEscape,
  formatError,
  getBuffer: () => buffer,
  isReadOnlyBuffer: (b) => isReadOnlyBuffer(b as Buffer | null),
  setBufferPath: (newPath) => {
    if (buffer) {
      buffer.path = newPath;
      updateStatus();
    }
  },
  isDirty,
  openFile,
  clearOpenBufferIfWithin: (deletedRel) => {
    // status: editor-tab-strip — drop any tabs whose paths fall under
    // the deleted prefix so they don't linger as broken references.
    const drop = [...openBuffers.keys()].filter(
      (p) => p === deletedRel || p.startsWith(deletedRel + "/"),
    );
    for (const p of drop) openBuffers.delete(p);
    // status: editor-preview-tab — drop preview slot pointer if dropped.
    if (previewTabPath !== null && drop.includes(previewTabPath)) {
      previewTabPath = null;
    }
    if (
      buffer &&
      (buffer.path === deletedRel ||
        buffer.path.startsWith(deletedRel + "/"))
    ) {
      buffer = null;
      activePath = null;
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: "" },
      });
      updateStatus();
    }
    tabStrip?.render();
  },
  refreshTrashBin,
  renderIndexStatus,
  persistSetting,
});

function refreshTree(): Promise<void> {
  return tree.refresh();
}
function revealInTree(rel: string): Promise<void> {
  return tree.revealPath(rel);
}
function scheduleTreeRefreshFromWatcher(): void {
  tree.notifyWatcher();
}

function formatError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object" && "message" in err) {
    const m = (err as { message: unknown }).message;
    return typeof m === "string" ? m : JSON.stringify(err);
  }
  return JSON.stringify(err);
}

/// Show the OS folder picker via the JS dialog plugin and, on a
/// selection, open it through `open_vault_at`. The picker lives entirely
/// in the frontend per the spec — the backend has no dialog dependency.
async function openVault(): Promise<void> {
  let chosen: string | null;
  try {
    const picked = await openDialog({ directory: true, multiple: false });
    chosen = typeof picked === "string" ? picked : null;
  } catch (err) {
    console.error("folder picker failed:", err);
    return;
  }
  if (!chosen) return;
  try {
    const display = await invoke<string>("open_vault_at", { path: chosen });
    await applyOpenedVault(display);
  } catch (err) {
    handleOpenVaultError(err);
  }
}

function handleOpenVaultError(err: unknown): void {
  const msg = formatError(err);
  console.error("open vault failed:", err);
  // Surface schema-version mismatches with the canonical fix from
  // index.md's `store-version-fail-loud` policy. The error string is
  // shaped like "schema version mismatch: db is vN, binary expects vM".
  if (msg.includes("schema version mismatch")) {
    alert(
      `${msg}\n\nThis project's pre-real-use migration policy is to delete .hiker/index.db and re-index. Remove that file in your vault and try again.`,
    );
  } else {
    alert(`open vault failed: ${msg}`);
  }
}

async function applyOpenedVault(path: string): Promise<void> {
  const basename = path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() ?? path;
  vaultPathEl.textContent = basename;
  vaultPathEl.title = path;
  tree.setSelectedFolder("");
  vaultIsOpen = true;
  outstandingCount = 0;
  // status: task-queue-home-widget
  // Tile mounts pre-vault-open; re-fetch settings + snapshot now.
  void taskQueueTile.refresh();

  // status: settings-load-once-at-startup
  // Seed View menu / tree / panel state from the merged settings. Failures
  // here aren't fatal — fall back to whatever the in-memory defaults are.
  try {
    const s = await invoke<Settings>("get_settings");
    applySettingsToUi(s);
  } catch (err) {
    console.error("get_settings failed:", err);
  }
  // Stale per-path state from a prior vault must not leak into the new one
  // (paths can collide across vaults).
  tree.clearCaches();
  // status: multi-buffer-in-memory-only — open buffers don't persist
  // across vault swaps; clear them along with the rest of per-vault state.
  openBuffers.clear();
  // status: editor-preview-tab — preview slot doesn't survive vault swap.
  previewTabPath = null;
  activePath = null;
  buffer = null;
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
  tabStrip?.render();
  // Clear the related-notes panel so hits from the prior vault don't linger
  // until the next file open / save populates it for the new vault.
  void discovery.refreshRelated(null);
  // status: chat-panel-pinned-bottom — drop transcript and any in-flight
  // turn so the new vault starts clean.
  chatPanel.reset();
  // status: chat-session-resume-latest
  // Re-seed the panel from the most-recent on-disk session (if any).
  // The backend's `resume_latest_at_open` already adopted it as active;
  // we just paint the rendered transcript here.
  try {
    const active = await invoke<ActiveSessionDto | null>("chat_session_active");
    chatPanel.hydrate(active);
  } catch (err) {
    console.error("chat_session_active failed:", err);
  }
  // Likewise, blank the search input/results so prior-vault matches don't
  // surface in the new vault. status: search-discovery-panel
  discovery.clear();
  startBackgroundIntervals();
  await refreshTree();
  await refreshTrashBin();
  // status: navigation-history-stack — history is per-vault, so swapping
  // vaults drops the stack along with `openBuffers`. Cleared *before*
  // `vaultHome.setVisible(true)` below so the home page becomes the
  // first checkpoint on the new vault rather than landing on a stale tail.
  nav?.reset();
  // status: vault-home-screen — default landing surface on vault open
  // (no auto-resume of last buffer in v1).
  vaultHome.setVisible(true);
}

pickBtn.addEventListener("click", () => void openVault());

// status: settings-default-vault-autoopen
// Bootstrap: read `vault.default` from the user TOML; if non-empty, try
// `open_vault_at`. On `HikerError::NotFound` (path no longer resolves —
// drive unmounted, folder deleted) surface a non-fatal toast and fall
// through to the JS dialog. The configured `vault.default` is *not*
// auto-cleared — it represents user intent, not a transient circumstance.
async function bootstrapDefaultVault(): Promise<void> {
  let configured: string | null = null;
  try {
    configured = await invoke<string | null>("get_default_vault");
  } catch (err) {
    console.error("get_default_vault failed:", err);
  }
  if (configured && configured.length > 0) {
    try {
      const display = await invoke<string>("open_vault_at", { path: configured });
      await applyOpenedVault(display);
      return;
    } catch (err) {
      // HikerError is serialized as `{ kind, message }` (see core::error).
      // `not_found` is the "path no longer resolves" signal that the spec
      // says should fall through to the picker. Any other error is real
      // and surfaces as the standard alert.
      const kind = (err as { kind?: string } | null)?.kind;
      if (kind === "not_found") {
        showToast(`Default vault at ${configured} not found — pick a vault`);
      } else {
        handleOpenVaultError(err);
        return;
      }
    }
  }
  // No configured default, or fell through after a NotFound. Show picker.
  await openVault();
}

void bootstrapDefaultVault();

// New-note button, tree-actions menu (Refresh / Reindex / Sort by),
// inline rename, attachContextMenu, deleteFromTree, countNotesIn,
// sortOrderLabel, openSortByMenu — all moved to ./tree.

const win = getCurrentWindow();

// Custom window controls (decorations: false in tauri.conf.json — the
// top strip is the title bar, so we provide our own min/max/close +
// drag-to-move). Tauri 2's `data-tauri-drag-region` attribute only
// matches the exact event target, which makes clicks on inner
// containers (vault-path span, leading-cluster wrapper, empty tab-strip
// space) fall through and not initiate a drag. A mousedown listener on
// the whole strip that excludes interactive descendants gives us the
// behavior the OS title bar used to: drag to move, double-click to
// maximize, click on a button to do its action.
const topStripEl = document.getElementById("top-strip");
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
document.getElementById("win-min")?.addEventListener("click", () => {
  void win.minimize();
});
document.getElementById("win-max")?.addEventListener("click", () => {
  void win.toggleMaximize();
});
document.getElementById("win-close")?.addEventListener("click", () => {
  // Routes through the same `onCloseRequested` handler below so the
  // multi-buffer-window-close-guard fires.
  void win.close();
});
// status: window-close-guard-dirty, multi-buffer-window-close-guard
// Always preventDefault and drive the close ourselves via `win.destroy()`.
// Returning without preventDefault to "let Tauri default-close" is
// unreliable (X button becomes a no-op), and `win.close()` would re-enter
// this handler — `destroy()` skips the close-requested round-trip.
//
// Multi-buffer-aware: enumerate every dirty tab; if any are dirty, show
// the multi-buffer modal listing each path with per-tab Save / Discard
// radios plus Save All / Discard All / Cancel.
void win.onCloseRequested(async (event) => {
  event.preventDefault();
  // Persist the active tab's edits into its buffer's loadedText so the
  // dirty enumeration sees consistent state for it.
  const dirtyPaths: string[] = [];
  for (const [p, entry] of openBuffers) {
    let dirty: boolean;
    if (p === activePath && buffer?.mode.kind === "file") {
      dirty = isDirty();
    } else if (entry.savedState) {
      dirty = entry.savedState.doc.toString() !== entry.buffer.loadedText;
    } else {
      dirty = false;
    }
    if (dirty) dirtyPaths.push(p);
  }
  if (dirtyPaths.length > 0) {
    const choice = await confirmWindowClose(dirtyPaths);
    if (choice.kind === "cancel") return;
    // Save-helper: switches to each path then runs save() so drift
    // checks fire against the right buffer's hash.
    const saveOne = async (p: string): Promise<boolean> => {
      if (activePath !== p || buffer?.mode.kind !== "file") {
        activateTabInner(p);
      }
      return await save();
    };
    if (choice.kind === "save-all") {
      for (const p of dirtyPaths) {
        const ok = await saveOne(p);
        if (!ok) return; // user cancelled a drift modal — abort close
      }
    } else if (choice.kind === "per-tab") {
      for (const p of dirtyPaths) {
        if (choice.choices[p] === "save") {
          const ok = await saveOne(p);
          if (!ok) return;
        }
        // discard → no-op; we're about to destroy the window anyway
      }
    }
    // discard-all: nothing to do; fall through.
  }
  openBuffers.clear();
  previewTabPath = null;
  buffer = null;
  activePath = null;
  await win.destroy();
});

updateStatus();

// status: status-bar-path-reveal
statusPathEl.addEventListener("click", async () => {
  if (!buffer || isReadOnlyBuffer(buffer)) return;
  try {
    await invoke("reveal_in_file_manager", { rel: buffer.path });
  } catch (err) {
    console.error("reveal_in_file_manager failed:", err);
  }
});

// ---------- vault home view ----------
// Vault-home (overview tiles + recent-activity detail) lives in `./vaultHome`.
// `vaultHome` is defined below, after `settingsPane` (the home/settings
// mutual-exclusion uses `settingsPane.isVisible()` in `onBeforeShow`).
// Forward refs from earlier mounts (e.g. `snapshotPreview.onClose`) reach
// `vaultHome` via closures resolved at call time, after init completes.

// status: settings-pane-mode
// status: vault-bar-settings-icon
// Settings pane sub-mode of the editor pane. Mutually exclusive with the
// vault-home view; opening either drops the other. Dirty-buffer guard is
// the same `confirm3` modal `openFile` uses (file-switch-guard-dirty).
const settingsPane: SettingsPaneApi = mountSettingsPane({
  paneEl: settingsPaneEl,
  editorPaneEl,
  settingsBtn,
  vaultPathEl,
  guardDirtyBuffer: async () => {
    if (!buffer || !isDirty()) return true;
    const choice = await confirm3(
      `${buffer.path} has unsaved changes.`,
      "Save & switch",
      "Discard & switch",
      "Cancel",
    );
    if (choice === "cancel") return false;
    if (choice === "a") return await save();
    return true;
  },
  onEnter: () => {
    // Drop home view; the editor's CM6 view is hidden by the
    // `settings-view` CSS class, so no explicit teardown is needed.
    if (vaultHome.isVisible()) vaultHome.setVisible(false);
  },
  onSettingApplied: (cfg) => {
    // Mirror the seeding `applyOpenedVault` does on first vault open so
    // every UI surface that reads from settings stays in sync after a
    // pane-driven flip — same way the View menu's persistSetting calls
    // already do for their specific keys.
    applySettingsToUi(cfg);
  },
});

settingsBtn.addEventListener("click", () => {
  void settingsPane.toggle();
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
    void settingsPane.toggle();
    return true;
  },
});

window.addEventListener(
  "keydown",
  (e) => {
    // Match Mod-,: meta on macOS, ctrl elsewhere. Skip when the editor has
    // focus (the registry-side binding handles that case) so we don't
    // double-toggle.
    if (e.key !== "," || e.altKey || e.shiftKey) return;
    const isMac = navigator.platform.toLowerCase().includes("mac");
    const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
    if (!mod) return;
    if ((e.target as HTMLElement | null)?.closest(".cm-editor")) return;
    e.preventDefault();
    void settingsPane.toggle();
  },
  { capture: true },
);

// status: vault-home-screen
// Vault-home view (overview tiles + recent-activity detail) lives in
// `./vaultHome`. This module owns activity rows, recently-restored
// highlight, author filters, and the stats-refresh debounce. Snapshot
// preview / note open route back here via callbacks so the module never
// touches editor state directly.
const vaultHome: VaultHomeApi = mountVaultHome({
  editorPaneEl,
  vaultHomeEl,
  homeBtn,
  vaultPathEl,
  titleEl: document.getElementById("vault-home-title")!,
  statsBodyEl: document.getElementById("vault-home-stats-body")!,
  modifiedListEl: document.getElementById("vault-home-modified-list")!,
  accessedListEl: document.getElementById("vault-home-accessed-list")!,
  newNoteBtn: document.getElementById("vault-home-new-note") as HTMLButtonElement,
  overviewEl: document.getElementById("vault-home-overview")!,
  detailEl: document.getElementById("vault-home-detail")!,
  detailTitleEl: document.getElementById("vault-home-detail-title")!,
  detailCountEl: document.getElementById("vault-home-detail-count")!,
  detailListEl: document.getElementById("vault-home-detail-list")!,
  detailFiltersEl: document.getElementById("vault-home-detail-filters")!,
  activitySectionEl: document.getElementById("vault-home-activity")!,
  activityHeaderEl: document.getElementById("vault-home-activity-header")!,
  activityListEl: document.getElementById("vault-home-activity-list")!,
  formatError,
  getVaultIsOpen: () => vaultIsOpen,
  onOpenNote: (rel, opts) => openFile(rel, opts),
  onOpenSnapshot: (row) => snapshotPreview.open(row),
  onBeforeShow: () => {
    if (settingsPane.isVisible()) void settingsPane.setVisible(false);
    if (queueDetail.isVisible()) {
      queueDetail.setVisible(false);
      const ovEl = document.getElementById("vault-home-overview");
      if (ovEl) ovEl.hidden = false;
    }
  },
});

// status: task-queue-home-detail-view
const queueDetail: QueueDetailApi = mountQueueDetail({
  containerEl: document.getElementById("vault-home-queue-detail")!,
});

// status: navigation-history-stack
// status: top-strip-back-button, top-strip-forward-button
// status: navigation-trackpad-swipe, navigation-keybind
// Per-vault back/forward stack across editor-pane content surfaces.
// Mounted after the surfaces it observes (vault home / settings / queue
// detail / snapshot preview / trash) so `inferCurrent()` and `apply()`
// can read/drive them. Reset on vault swap; pruned on tab close + on
// preview-slot replacement.
const navBackBtn = document.getElementById("nav-back-btn") as HTMLButtonElement;
const navForwardBtn = document.getElementById("nav-forward-btn") as HTMLButtonElement;

function inferNavState(): NavState {
  if (queueDetail.isVisible()) return { kind: "queue-detail" };
  if (vaultHome.isVisible()) {
    const d = vaultHome.activeDetailView();
    if (d && d.kind === "recent-activity") {
      return { kind: "home-detail", view: "recent-activity" };
    }
    return { kind: "home" };
  }
  if (settingsPane.isVisible()) return { kind: "settings" };
  if (buffer && buffer.mode.kind === "trash") {
    const trashedName = buffer.path.replace(/^\.hiker\/trash\//, "");
    return { kind: "trash-preview", trashedName };
  }
  if (buffer && buffer.mode.kind === "snapshot") {
    return {
      kind: "snapshot-preview",
      changeId: buffer.mode.changeId,
      row: buffer.mode.row,
    };
  }
  if (activePath !== null && buffer && buffer.mode.kind === "file") {
    return { kind: "tab", path: activePath };
  }
  return { kind: "empty" };
}

async function applyNavState(s: NavState): Promise<boolean> {
  switch (s.kind) {
    case "tab": {
      if (!openBuffers.has(s.path)) return false;
      // If a preview / settings / home is currently up, activateTabInner
      // already drops them via its own setVisible(false) calls.
      activateTabInner(s.path);
      return true;
    }
    case "home": {
      vaultHome.setVisible(true);
      return true;
    }
    case "home-detail": {
      vaultHome.setVisible(true);
      vaultHome.showDetail(s.view);
      return true;
    }
    case "queue-detail": {
      vaultHome.setVisible(true);
      const ovEl = document.getElementById("vault-home-overview");
      if (ovEl) ovEl.hidden = true;
      queueDetail.setVisible(true);
      queueDetail.setFilter("tasks");
      return true;
    }
    case "settings": {
      const ok = await settingsPane.setVisible(true);
      // `setVisible` returns false when the dirty-buffer guard cancels
      // the entry; treat it as "couldn't restore" so navigate() skips on.
      return ok;
    }
    case "trash-preview": {
      const item = trash.items().find((i) => i.trashed_name === s.trashedName);
      if (!item) return false;
      await trash.openPreview(item);
      return true;
    }
    case "snapshot-preview": {
      await snapshotPreview.open(s.row);
      return true;
    }
    case "empty": {
      // Nothing to do — closeTab's no-tabs-left branch already routes to
      // the home view, so the empty state isn't actually re-enterable.
      return false;
    }
  }
}

function paintNavButtons(): void {
  navBackBtn.disabled = !nav!.canBack();
  navForwardBtn.disabled = !nav!.canForward();
}

nav = mountNavigation({
  inferCurrent: inferNavState,
  apply: applyNavState,
  onChange: paintNavButtons,
});

navBackBtn.addEventListener("click", () => {
  void nav!.back();
});
navForwardBtn.addEventListener("click", () => {
  void nav!.forward();
});

// status: navigation-history-stack
// Snapshot preview replaces the singleton `buffer` without mutating any
// observed DOM attribute, so the MutationObserver above can't detect the
// transition. Wrap its openers to checkpoint after the buffer flip lands.
// Trash gets the same treatment further down (after `trash` is mounted).
// The wrappers also fire on back/forward apply, where the nav module's
// `restoring` flag turns the checkpoint into a no-op.
{
  const _snapOpen = snapshotPreview.open;
  snapshotPreview.open = async (row) => {
    await _snapOpen.call(snapshotPreview, row);
    checkpointNav();
  };
  const _snapClose = snapshotPreview.close;
  snapshotPreview.close = () => {
    _snapClose.call(snapshotPreview);
    checkpointNav();
  };
}

// status: navigation-trackpad-swipe
// Two-finger horizontal trackpad swipe → back/forward. Right-swipe = back,
// left-swipe = forward (browser convention). Threshold ~120px accumulated
// `deltaX`. See `navigation/index.ts` for the wheel-event heuristic.
installNavigationSwipe({
  back: () => void nav!.back(),
  forward: () => void nav!.forward(),
});

// status: navigation-history-stack
// Observe DOM-driven content-surface flips (settings ↔ home ↔ queue-detail
// ↔ home-detail) so the navigation stack records them without each
// surface module having to call into nav directly. The `restoring` flag
// inside the navigation module suppresses checkpoints during back/forward
// apply, so this observer doesn't cause infinite recursion.
{
  const obs = new MutationObserver(() => checkpointNav());
  obs.observe(editorPaneEl, { attributes: true, attributeFilter: ["class"] });
  const homeOverviewEl = document.getElementById("vault-home-overview");
  const homeDetailEl = document.getElementById("vault-home-detail");
  const homeQueueDetailEl = document.getElementById("vault-home-queue-detail");
  if (homeOverviewEl) {
    obs.observe(homeOverviewEl, { attributes: true, attributeFilter: ["hidden"] });
  }
  if (homeDetailEl) {
    obs.observe(homeDetailEl, { attributes: true, attributeFilter: ["hidden"] });
  }
  if (homeQueueDetailEl) {
    obs.observe(homeQueueDetailEl, { attributes: true, attributeFilter: ["hidden"] });
  }
}

// status: editor-diff-vs-disk-toggle
// Dirty-buffer Diff toggle: per `diff.md`'s `editor-diff-vs-disk-toggle`,
// `#mode-controls` shows a single Diff toggle for any dirty editable
// buffer. Module owns selection/viewport save+restore, in-flight guard,
// and the markdown-compartment reconfigure (per the "Markdown-rendering
// coupling" rule in `diff.md`).
dirtyBufferDiff = mountDirtyBufferDiff({
  view,
  getBuffer: () => buffer,
  livePreviewCompartment,
  hideFrontmatterCompartment,
  livePreviewExtensionForPath,
  getHideFrontmatterEnabled: () => hideFrontmatterEnabled,
  setReadOnly: (ro) => setReadOnly(ro),
  renderModeControls: () => modeControls?.render(),
  refreshChunkBoundaries,
  formatError,
});

// status: note-mutations-menu
const mutationsMenuBtn = document.getElementById(
  "mutations-menu-btn",
) as HTMLButtonElement;
mutationsMenu = mountMutationsMenu(
  {
    buttonEl: mutationsMenuBtn,
    getBuffer: () => buffer,
    getActiveBufferText: () => {
      if (!buffer || isReadOnlyBuffer(buffer)) return null;
      return view.state.doc.toString();
    },
    formatError,
  },
  {
    // status: note-mutation-buffer-ro-while-in-flight
    onInFlightChanged: (path, inFlight) => {
      if (inFlight) {
        inFlightMutationPaths.add(path);
        // status: editor-preview-tab-promotion
        // Pin the source tab on submit so it can't be replaced by a
        // preview-slot swap mid-flight (the result needs an open
        // buffer to land on). Idempotent on already-sticky tabs.
        promotePreviewByPath(path);
      } else {
        inFlightMutationPaths.delete(path);
      }
      // If the active buffer is the one whose in-flight state just
      // changed, mirror it in the editor.
      if (buffer && buffer.mode.kind === "file" && buffer.path === path) {
        setReadOnly(inFlight);
      }
      modeControls?.render();
    },
  },
);

// status: note-mutation-applies-as-buffer-edit
// Apply a mutation result to the open file-mode buffer at `path` as a
// single CM6 transaction (one undo step). Works whether the target tab
// is the *active* one (dispatch through the live `view`) or a
// *background* tab (mutate the entry's saved CM6 state in place so the
// new content is there when the user activates it). Drift-checked
// against `expectedSourceHash`: if the buffer's `loadedHash` no longer
// matches, drop silently — the user's edits during the mutation flight
// trumped the LLM's output. Also stamps `pendingChangesMetadata` so
// the next save tags the `'modified'` row with `metadata.mutation`.
// Tab is left dirty (`loadedText` stays at pre-mutation), surfacing the
// dirty marker in the strip + tree. If the buffer isn't open at all
// (user closed the tab mid-flight), the result is dropped silently.
function applyMutationToBuffer(
  path: string,
  content: string,
  mutationKind: string,
  expectedSourceHash: string,
): void {
  const entry = openBuffers.get(path);
  if (!entry) return;
  if (entry.buffer.loadedHash !== expectedSourceHash) {
    // Drift: user edited during the in-flight window. Per spec the
    // buffer is RO during the flight, so this branch is rare but
    // possible (drift-from-disk via watcher, force-reload, etc.).
    return;
  }
  entry.buffer.pendingChangesMetadata = { mutation: mutationKind };
  const isActive = activePath === path && buffer?.path === path;
  if (isActive) {
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: content },
    });
    // Terminal queue event also clears RO via `onInFlightChanged`;
    // clearing here is idempotent + defensive.
    setReadOnly(false);
    updateStatus();
  } else if (entry.savedState) {
    // Background tab. Update the saved CM6 state in place via a
    // transaction off the existing state — preserves history so Ctrl-Z
    // on activation reverts the whole replacement as one undo step
    // (same shape as the active path).
    const tr = entry.savedState.update({
      changes: {
        from: 0,
        to: entry.savedState.doc.length,
        insert: content,
      },
    });
    entry.savedState = tr.state;
    // Re-render so the tab strip's dirty dot reflects the change.
    tabStrip?.render();
  }
}

interface NoteMutationAppliedEvent {
  task_id: string;
  source_path: string;
  mutation_kind: string;
  content: string;
  source_hash_at_submit: string;
}
void listen<NoteMutationAppliedEvent>("hiker:note-mutation-applied", (ev) => {
  const p = ev.payload;
  applyMutationToBuffer(
    p.source_path,
    p.content,
    p.mutation_kind,
    p.source_hash_at_submit,
  );
});

// status: task-queue-home-widget
// status: task-queue-home-widget-respects-llm-disable
// status: vault-bar-queue-button
// Wire the home-page Task queue tile + the new vault-bar queue button.
// The tile shows a one-line "X active · Y succeeded · Z failed" summary
// (middots) since vault open; the vault-bar button shows a pulsing blue
// dot when anything is active and a dim red dot if any task has failed
// since the last time the user viewed the queue. Hidden / inert entirely
// when `[llm] enabled = false`.
const taskQueueTile = (() => {
  const tasksSection = document.getElementById("vault-home-tasks");
  const tasksHeader = document.getElementById("vault-home-tasks-header");
  const tasksSummary = document.getElementById("vault-home-tasks-summary");
  const queueBtnEl = document.getElementById("queue-btn") as HTMLButtonElement | null;
  const queueIndicatorEl = document.getElementById("queue-btn-indicator");

  let activeCount = 0;
  let succeededCount = 0;
  let failedCount = 0;
  let unreadFailure = false;
  let llmEnabled = true;

  const SUMMARY_DOT = " · ";
  function paintSummary(): void {
    if (!tasksSection || !tasksSummary) return;
    if (!llmEnabled) {
      tasksSection.hidden = true;
      return;
    }
    tasksSection.hidden = false;
    if (activeCount + succeededCount + failedCount === 0) {
      tasksSummary.textContent = "No tasks queued";
      return;
    }
    tasksSummary.textContent = [
      `${activeCount} active`,
      `${succeededCount} succeeded`,
      `${failedCount} failed`,
    ].join(SUMMARY_DOT);
  }

  function paintIndicator(): void {
    if (!queueBtnEl || !queueIndicatorEl) return;
    if (!llmEnabled) {
      queueBtnEl.hidden = true;
      queueIndicatorEl.hidden = true;
      return;
    }
    queueBtnEl.hidden = false;
    if (activeCount > 0) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.classList.add("queue-indicator-active");
      queueIndicatorEl.classList.toggle("queue-indicator-failed", false);
      return;
    }
    queueIndicatorEl.classList.remove("queue-indicator-active");
    if (unreadFailure) {
      queueIndicatorEl.hidden = false;
      queueIndicatorEl.classList.add("queue-indicator-failed");
      return;
    }
    queueIndicatorEl.hidden = true;
    queueIndicatorEl.classList.remove("queue-indicator-failed");
  }

  function repaint(): void {
    paintSummary();
    paintIndicator();
  }

  function openQueueDetail(): void {
    if (!llmEnabled) return;
    // Vault home owns the overview ↔ detail toggle; show home first
    // and then swap into the queue detail. The home button stays the
    // back-out path.
    vaultHome.setVisible(true);
    const ovEl = document.getElementById("vault-home-overview");
    if (ovEl) ovEl.hidden = true;
    queueDetail.setVisible(true);
    queueDetail.setFilter("tasks");
    // Visiting the queue clears the "unread failure" indicator. Active
    // pulse stays — that's a live-state mirror, not a notification.
    unreadFailure = false;
    paintIndicator();
  }

  if (tasksHeader) {
    tasksHeader.style.cursor = "pointer";
    tasksHeader.addEventListener("click", openQueueDetail);
  }
  if (queueBtnEl) {
    queueBtnEl.addEventListener("click", openQueueDetail);
  }

  void listen<{ event: string }>("hiker:queue-event", (ev) => {
    const k = ev.payload.event;
    if (k === "task_queued") {
      activeCount += 1;
    } else if (k === "task_completed") {
      activeCount = Math.max(0, activeCount - 1);
      succeededCount += 1;
    } else if (k === "task_failed") {
      activeCount = Math.max(0, activeCount - 1);
      failedCount += 1;
      // Only flag the indicator red if the user isn't currently looking
      // at the queue — otherwise the dot would light up under their
      // cursor for no reason.
      if (!queueDetail.isVisible()) unreadFailure = true;
    } else if (k === "task_cancelled") {
      activeCount = Math.max(0, activeCount - 1);
    }
    repaint();
  });

  async function refresh(): Promise<void> {
    try {
      const cfg = await invoke<{ llm: { enabled: boolean } }>("get_settings");
      llmEnabled = cfg.llm.enabled;
    } catch {
      // No vault open yet — keep the tile + button hidden until
      // refresh() runs again.
      llmEnabled = false;
    }
    try {
      const rows = await invoke<Array<{ state: string }>>("tasks_snapshot");
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      for (const r of rows) {
        if (r.state === "queued" || r.state === "leased") activeCount += 1;
        else if (r.state === "completed") succeededCount += 1;
        else if (r.state === "failed") failedCount += 1;
      }
      // Fresh vault → no unread state to inherit.
      unreadFailure = false;
    } catch {
      activeCount = 0;
      succeededCount = 0;
      failedCount = 0;
      unreadFailure = false;
    }
    repaint();
  }
  repaint();
  void refresh();
  return { refresh };
})();

// Snapshot-preview lifecycle: callers go through the `snapshotPreview`
// API directly (e.g. `snapshotPreview.open(row)` from vault-home; the
// mode-controls renderer above wires its toolbar buttons in the same way).

// status: bug-activity-refresh-polled-not-pushed (fixed)
// vault-home owns the recents/activity refresh; the listener routes through
// `vaultHome.notifyChangesAppended()`.

// status: mcp-ui-refresh-on-agent-write
// Agent writes (per `mcp.md`) suppress the watcher around their fs writes for
// the same correctness reasons move/delete do, so `hiker:file-changed` never
// fires for them. Ride the changes broadcast instead: any row whose author
// is `agent` applies the same tree-refresh + active-buffer reload shape the
// watcher handler would have. Non-agent rows (user saves, rollbacks) keep
// flowing through the watcher path so we don't double-refresh.
void listen<ChangeRow>("hiker:changes-appended", (event) => {
  vaultHome.notifyChangesAppended();
  const row = event.payload;
  if (row.author_class !== "agent") return;
  void handleAgentChange(row);
});

async function handleAgentChange(row: ChangeRow): Promise<void> {
  if (row.op === "created" || row.op === "deleted" || row.op === "renamed") {
    scheduleTreeRefreshFromWatcher();
  } else if (
    row.op === "modified"
    && (tree.getSortOrder() === "mtime-newest" || tree.getSortOrder() === "mtime-oldest")
  ) {
    scheduleTreeRefreshFromWatcher();
  }

  if (!buffer || isReadOnlyBuffer(buffer)) return;

  if (row.op === "modified" && row.path === buffer.path) {
    if (isDirty()) {
      showToast(`${row.path} was rewritten by an agent; save to keep yours.`);
      return;
    }
    try {
      const fresh = await invoke<FileWithHash>("read_file_with_hash", { rel: row.path });
      if (fresh.hash !== buffer.loadedHash) {
        view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: fresh.contents },
        });
        buffer.loadedText = view.state.doc.toString();
        buffer.loadedHash = fresh.hash;
        updateStatus();
        scheduleChunkBoundariesRefresh(500);
      }
    } catch (err) {
      console.error("agent-change silent reload failed:", err);
    }
    return;
  }

  if (row.op === "deleted" && row.path === buffer.path) {
    if (isDirty()) {
      showToast(`${row.path} was removed by an agent; save to recreate.`);
    } else {
      // status: editor-tab-strip — drop the tab for the removed path.
      openBuffers.delete(row.path);
      if (previewTabPath === row.path) previewTabPath = null;
      buffer = null;
      activePath = null;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
      updateStatus();
      tabStrip?.render();
      showToast(`${row.path} was removed by an agent`);
    }
    return;
  }

  if (row.op === "renamed" && row.rename_from === buffer.path) {
    buffer.path = row.path;
    updateStatus();
  }
}

// ---------- panel toggles ----------

function syncToggleButtons(): void {
  toggleSidebarBtn.classList.toggle("active", !appEl.classList.contains("sidebar-collapsed"));
  toggleRelatedBtn.classList.toggle("active", !appEl.classList.contains("related-collapsed"));
}

toggleSidebarBtn.addEventListener("click", () => {
  appEl.classList.toggle("sidebar-collapsed");
  syncToggleButtons();
  if (!vaultIsOpen) return;
  void persistSetting(
    "vault",
    "vault.sidebar_open",
    !appEl.classList.contains("sidebar-collapsed"),
  );
});
toggleRelatedBtn.addEventListener("click", () => {
  appEl.classList.toggle("related-collapsed");
  syncToggleButtons();
  if (!vaultIsOpen) return;
  void persistSetting(
    "vault",
    "vault.related_open",
    !appEl.classList.contains("related-collapsed"),
  );
});

// Default: tree open, related collapsed (per editor.md). Overridden once
// `get_settings` lands in `openVault` for vaults that have explicit values.
appEl.classList.add("related-collapsed");
syncToggleButtons();

// status: editor-view-options-menu
// status: view-live-preview-toggle
// View ▾ menu on the editor toolbar. Hosts display-only toggles per
// editor.md's "View options menu" section. State is in-memory only in v1
// (per-vault / per-user persistence is a settings.md concern).
//
// Reserved entries appear as greyed-out rows with dependency tooltips —
// the spec calls this out as "a forcing function for designing each
// backing feature with the toggle in mind." When a backing feature
// lands, flip its row from disabled-stub to live without restructuring
// the menu.
const viewMenuBtn = document.getElementById("view-menu-btn") as HTMLButtonElement;

function buildViewMenuItems(): CtxMenuItem[] {
  return [
    {
      label: "Live preview",
      checked: livePreviewEnabled,
      run: () => {
        const on = !livePreviewEnabled;
        setLivePreviewEnabled(on);
        void persistSetting("vault", "editor.live_preview", on);
      },
    },
    {
      // status: view-show-chunk-boundaries
      label: "Show chunk boundaries",
      checked: chunkBoundariesEnabled,
      run: () => {
        const on = !chunkBoundariesEnabled;
        setChunkBoundariesEnabled(on);
        void persistSetting("vault", "editor.show_chunk_boundaries", on);
      },
    },
    {
      // status: view-hide-frontmatter-toggle
      label: "Hide frontmatter",
      checked: hideFrontmatterEnabled,
      run: () => {
        const on = !hideFrontmatterEnabled;
        setHideFrontmatterEnabled(on);
        void persistSetting("vault", "editor.hide_frontmatter", on);
      },
    },
    {
      // status: view-render-txt-as-markdown-toggle
      label: "Render .txt as markdown",
      checked: renderTxtAsMarkdown,
      run: () => {
        const on = !renderTxtAsMarkdown;
        setRenderTxtAsMarkdown(on);
        void persistSetting("vault", "editor.render_txt_as_markdown", on);
      },
    },
    {
      // status: view-word-wrap-toggle
      label: "Word wrap",
      checked: wordWrapEnabled,
      run: () => {
        const on = !wordWrapEnabled;
        setWordWrapEnabled(on);
        void persistSetting("vault", "editor.word_wrap", on);
      },
    },
    {
      label: "Show whitespace",
      checked: whitespaceEnabled,
      run: () => {
        const on = !whitespaceEnabled;
        setWhitespaceEnabled(on);
        void persistSetting("vault", "editor.show_whitespace", on);
      },
    },
    {
      label: "Show line numbers",
      checked: lineNumbersVisible,
      run: () => {
        const on = !lineNumbersVisible;
        setLineNumbersVisible(on);
        void persistSetting("vault", "editor.show_line_numbers", on);
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

// View menu button click handler installed by `mountModeControls`.

// ---------- discovery panel (search + related) ----------
// Search input + mode toggles + lexical/semantic results + related-notes
// panel + collapsible sections + roving-tabindex keyboard nav all live in
// `./discovery`. Host wires DOM ids and the editor-coupled callbacks
// (`onOpenNote`, `onScrollToChunk`).
const discovery: DiscoveryApi = mountDiscovery({
  appEl,
  inputEl: searchInputEl,
  clearBtn: searchClearBtn,
  toggleSemanticBtn: toggleModeSemanticBtn,
  toggleLexicalBtn: toggleModeLexicalBtn,
  searchSectionEl,
  searchListEl,
  searchCountEl,
  searchSpinnerEl,
  relatedSectionEl,
  relatedListEl,
  relatedCountEl,
  onOpenNote: (rel, opts) => openFile(rel, opts),
  onScrollToChunk: async (rel, chunkIndex) => {
    if (buffer?.path !== rel) return;
    try {
      const bounds = await invoke<ChunkBounds[]>("chunks_for", { rel });
      const target = bounds.find((b) => b.chunk_index === chunkIndex);
      if (!target) return;
      const safe = Math.min(target.char_start, view.state.doc.length);
      view.dispatch({
        selection: { anchor: safe },
        effects: EditorView.scrollIntoView(safe, { y: "start" }),
      });
      view.focus();
    } catch (err) {
      console.error("scroll-to-chunk failed:", err);
    }
  },
  persistSetting,
  expandPanelIfCollapsed: () => {
    const wasCollapsed = appEl.classList.contains("related-collapsed");
    if (wasCollapsed) {
      appEl.classList.remove("related-collapsed");
      void persistSetting("vault", "vault.related_open", true);
      syncToggleButtons();
    }
    return wasCollapsed;
  },
});

// status: search-keybind-ctrl-space (global half)
// Document-level Ctrl-Space handler — matches the spec's "every platform"
// rule by checking ctrlKey, *not* metaKey, so Cmd-Space on macOS stays
// Spotlight. Capture phase + preventDefault stops the browser's default
// (and CM6's startCompletion via the registry binding above when the
// editor has focus) before downstream handlers see it.
window.addEventListener(
  "keydown",
  (e) => {
    if (e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && e.code === "Space") {
      e.preventDefault();
      discovery.focusInput();
    }
  },
  { capture: true },
);

// status: editor-tab-keybinds
// Window-level listener for the tab keybinds so they fire even when
// focus is outside CM6 (file tree, status bar, sidebar). The CM6
// keymap registrations above cover the editor-focus case; this handler
// covers the rest. Skip when the user is typing into an input (so
// Cmd-W in a textarea doesn't hijack normal close-line behavior — but
// in Tauri there's no browser tab to close anyway, so we always handle
// it; we only skip the tab-cycle / number keys for inputs because
// those have meaningful in-input behavior).
window.addEventListener(
  "keydown",
  (e) => {
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
      if (activePath) void closeTab(activePath);
      return;
    }
    // Cmd/Ctrl-Tab cycle. Only when not typing — in an input the user
    // expects normal Tab behavior.
    if (e.key === "Tab" && !inInput) {
      e.preventDefault();
      cycleTab(e.shiftKey ? -1 : +1);
      return;
    }
    // Cmd/Ctrl-1..9 → jump to tab. Skip in inputs.
    if (!inInput && !e.shiftKey && !e.altKey) {
      const n = parseInt(e.key, 10);
      if (n >= 1 && n <= 9) {
        e.preventDefault();
        jumpToTab(n);
      }
    }
  },
  { capture: true },
);

let bufferPathInterval: number | null = null;
let lastSeenBufferPath: string | null = null;

// status: bug-index-status-polled-not-pushed (fixed)
// `index_status` is now pushed from the indexer over `hiker:index-status`
// (see the listener below). The 2s poll has been removed; the buffer-path
// watcher stays — that one's a separate UI concern (active-buffer changes
// drive the related-notes refresh).
function startBackgroundIntervals(): void {
  if (bufferPathInterval !== null) window.clearInterval(bufferPathInterval);
  bufferPathInterval = window.setInterval(() => {
    if (!vaultIsOpen) return;
    const cur = buffer?.path ?? null;
    if (cur !== lastSeenBufferPath) {
      lastSeenBufferPath = cur;
      discovery.scheduleRelatedRefresh(cur, 0);
    }
  }, 250);
}

// ---------- index status indicator ----------
// `indexStatus` and `outstandingCount` are declared near the top of the
// file so the first `updateStatus()` → `renderIndexStatus()` chain doesn't
// hit the TDZ during module init.

function renderIndexStatus(): void {
  // No vault → no indexer; blank the label rather than reporting state from
  // a previous vault (or a half-initialized "Model loading…" before any
  // vault has even been picked).
  if (!vaultIsOpen) {
    statusIndexEl.textContent = "";
    statusIndexEl.title = "";
    return;
  }
  if (indexStatus.last_error) {
    statusIndexEl.textContent = "Index error";
    statusIndexEl.title = indexStatus.last_error;
    return;
  }
  statusIndexEl.title = "";
  if (!indexStatus.model_ready) {
    statusIndexEl.textContent = "Model loading…";
    return;
  }
  // status: status-bar-active-file-index-state
  // Mirror the active buffer's per-file state when it's non-Indexed; fall
  // back to the aggregate label otherwise (or while previewing trash /
  // a snapshot — neither has live index state worth mirroring).
  if (buffer && !isReadOnlyBuffer(buffer)) {
    const cached = tree.getIndexState(buffer.path);
    if (!cached) {
      const path = buffer.path;
      void tree
        .fetchIndexState(path)
        .catch((err) => console.error("index_state_for failed:", path, err))
        .finally(() => {
          if (buffer && buffer.path === path) renderIndexStatus();
        });
    }
    const state = tree.getIndexState(buffer.path);
    if (state) {
      switch (state.kind) {
        case "unsupported":
          statusIndexEl.textContent = "Not indexed (unsupported filetype)";
          return;
        case "skipped":
          statusIndexEl.textContent = `Skipped — ${state.reason}`;
          return;
        case "queued":
          statusIndexEl.textContent = "Queued for indexing";
          return;
        case "indexed":
          break;
      }
    }
  }
  if (outstandingCount > 0) {
    statusIndexEl.textContent = `Indexing ${outstandingCount} pending`;
    return;
  }
  statusIndexEl.textContent = `Indexed (${indexStatus.total_notes} notes)`;
}

// status: bug-index-status-polled-not-pushed (fixed)
// Status snapshots arrive over `hiker:index-status` whenever the indexer's
// watch::Sender<IndexStatus> changes. The Tauri bridge emits the seeded
// value as soon as the vault opens and on every subsequent change.
void listen<IndexStatus>("hiker:index-status", (event) => {
  indexStatus = event.payload;
  renderIndexStatus();
  // Stats counts shift with model_ready / total_notes / last_error too.
  vaultHome.scheduleStatsRefresh();
});

void listen<ProgressEvent>("hiker:reindex-progress", (event) => {
  const ev = event.payload;
  switch (ev.kind) {
    case "model_loaded":
      indexStatus.model_ready = true;
      indexStatus.last_error = null;
      break;
    case "started":
      // No counter change — Started just marks "queued → processing", same
      // job, still outstanding. Marker stays Queued until terminal.
      updateIndexStateForPath(ev.path, { kind: "queued" });
      break;
    case "finished":
    case "skipped":
    case "deleted":
    case "renamed":
    case "error":
      // Any terminal event ends one outstanding job, regardless of whether
      // a prior Started fired (Delete and Rename don't emit Started).
      outstandingCount = Math.max(0, outstandingCount - 1);
      if (ev.kind === "error") {
        indexStatus.last_error = ev.message;
      } else {
        indexStatus.last_error = null;
      }
      if (ev.kind === "finished") {
        updateIndexStateForPath(ev.path, { kind: "indexed" });
        if (buffer && ev.path === buffer.path) {
          discovery.scheduleRelatedRefresh(buffer?.path ?? null, 100);
        }
      } else if (ev.kind === "skipped") {
        // "unchanged" is a no-op skip (file already indexed); only persist
        // the Skipped state for genuine refusals.
        if (ev.reason === "unchanged") {
          updateIndexStateForPath(ev.path, { kind: "indexed" });
        } else {
          updateIndexStateForPath(ev.path, { kind: "skipped", reason: ev.reason });
        }
      } else if (ev.kind === "deleted") {
        tree.deleteIndexState(ev.path);
      } else if (ev.kind === "renamed") {
        const prior = tree.getIndexState(ev.from);
        tree.deleteIndexState(ev.from);
        if (prior) updateIndexStateForPath(ev.to, prior);
      } else if (ev.kind === "error" && ev.path) {
        // Refetch on next render — error state isn't itself a marker.
        tree.deleteIndexState(ev.path);
      }
      break;
    case "scan_complete":
      outstandingCount += ev.queued;
      break;
  }
  renderIndexStatus();
  // status: vault-home-stats-widget — counts shift on every terminal event;
  // debounced so a flurry of progress events fires one stats fetch.
  // The full IndexStatus snapshot (model_ready / queued / total_notes /
  // last_error) rides `hiker:index-status` per
  // `bug-index-status-polled-not-pushed` (fixed); progress events only own
  // the per-path marker + outstanding-count bookkeeping.
  vaultHome.scheduleStatsRefresh();
});

function updateIndexStateForPath(path: string, state: IndexState): void {
  tree.setIndexState(path, state);
  // Force a re-render of the row(s) by toggling marker classes via DOM.
  // The tree module's lazy fetch path also writes the cache; this branch
  // covers progress events that resolve a state without a render trigger.
  document
    .querySelectorAll(`#tree li[data-path="${cssEscape(path)}"]`)
    .forEach((el) => {
      const li = el as HTMLElement;
      li.classList.remove("ix-unsupported", "ix-skipped", "ix-queued", "ix-indexed");
      li.removeAttribute("data-ix-reason");
      let marker = li.querySelector<HTMLSpanElement>(":scope > .ix-marker");
      if (state.kind !== "indexed") {
        if (!marker) {
          marker = document.createElement("span");
          marker.className = "ix-marker";
          li.append(marker);
        }
      } else if (marker) {
        marker.remove();
      }
      switch (state.kind) {
        case "unsupported":
          li.classList.add("ix-unsupported");
          li.removeAttribute("title");
          break;
        case "skipped":
          li.classList.add("ix-skipped");
          li.dataset.ixReason = state.reason;
          li.title = `Skipped — ${state.reason}`;
          break;
        case "queued":
          li.classList.add("ix-queued");
          li.removeAttribute("title");
          break;
        case "indexed":
          li.classList.add("ix-indexed");
          li.removeAttribute("title");
          break;
      }
    });
  if (buffer && !isReadOnlyBuffer(buffer) && buffer.path === path) {
    renderIndexStatus();
  }
}


// ---------- trash bin ----------
// Trash bin (sidebar collapsible, list rendering, row context menu,
// restore/purge, read-only preview) lives in `./trash`. The host wires it
// to DOM elements and the editor view via the deps below.
const trash: TrashApi = mountTrash({
  binEl: trashBinEl,
  headerEl: trashHeaderEl,
  listEl: trashListEl,
  chevronEl: trashChevronEl,
  labelEl: trashLabelEl,
  view,
  language,
  livePreviewCompartment,
  languageExtensionForPath,
  livePreviewExtensionForPath,
  getBuffer: () => buffer,
  setBuffer: (b) => {
    buffer = b as Buffer | null;
  },
  setReadOnly,
  updateStatus,
  refreshChunkBoundaries,
  isDirty,
  save,
  cssEscape,
  isVaultIsOpen: () => vaultIsOpen,
  persistSetting,
  isVaultHomeVisible: () => vaultHome.isVisible(),
  setVaultHomeVisible: (on) => vaultHome.setVisible(on),
  refreshTree,
  formatError,
});
function refreshTrashBin(): Promise<void> {
  return trash.refresh();
}

// status: navigation-history-stack
// Mirror of the snapshot wrap above — trash preview also bypasses the
// editor-pane MutationObserver, so checkpoint after open/close.
{
  const _trashOpen = trash.openPreview;
  trash.openPreview = async (item) => {
    await _trashOpen.call(trash, item);
    checkpointNav();
  };
  const _trashClose = trash.closePreview;
  trash.closePreview = () => {
    _trashClose.call(trash);
    checkpointNav();
  };
}
type ReadOnlyMode = "trash" | "snapshot" | "mutation" | null;

/// Set or clear the editor's read-only state. Mode-specific UI (the
/// label + action icons in the toolbar's center cluster) is driven by
/// `renderModeControls`, which inspects buffer state and diff visibility
/// directly — `mode` here is purely for legacy call-site clarity and
/// triggers a re-render after the read-only toggle takes effect.
function setReadOnly(ro: boolean, _mode: ReadOnlyMode = null): void {
  view.dispatch({
    effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(ro)),
  });
  modeControls?.render();
}

// status: editor-toolbar-mode-controls, mode-controls-diff-toggle
// Mode-specific controls (snapshot / trash labels + action icons) live in
// `./modeControls`. Each owning module registers its renderer here; the
// host swaps based on the active buffer's `mode.kind`. The View ▾ menu
// shares the same toolbar host and its menu items live in
// `buildViewMenuItems` above (kept here because they bind to host-level
// View settings).
modeControls = mountModeControls({
  hostEl: modeControlsEl,
  viewMenuBtn,
  buildViewMenuItems,
  getActiveMode: () => buffer?.mode.kind ?? null,
});

// status: editor-tab-strip
tabStrip = mountTabStrip({
  hostEl: document.getElementById("tab-strip")!,
  getTabs: () => tabSnapshots(),
  getActivePath: () =>
    buffer?.mode.kind === "file" ? activePath : null,
  onActivate: (path) => activateTabInner(path),
  onClose: (path) => void closeTab(path),
  onCloseOthers: (path) => {
    void (async () => {
      const others = [...openBuffers.keys()].filter((p) => p !== path);
      for (const p of others) {
        await closeTab(p);
        // closeTab may have aborted on Cancel — if the tab still exists,
        // bail out of the bulk operation.
        if (openBuffers.has(p)) return;
      }
    })();
  },
  onCloseToRight: (path) => {
    void (async () => {
      const order = [...openBuffers.keys()];
      const idx = order.indexOf(path);
      if (idx < 0) return;
      const targets = order.slice(idx + 1);
      for (const p of targets) {
        await closeTab(p);
        if (openBuffers.has(p)) return;
      }
    })();
  },
  onRevealInTree: (path) => {
    void revealInTree(path);
  },
  // status: editor-preview-tab-promotion
  onPromote: (path) => promotePreviewByPath(path),
});

modeControls?.register("snapshot", (host) => {
  if (buffer?.mode.kind !== "snapshot") return;
  const row = buffer.mode.row;
  const diffActive = buffer.mode.diffActive;
  const label = document.createElement("span");
  label.className = "mode-label";
  label.textContent = diffActive ? "Diff · snapshot ↔ current" : "Snapshot preview";
  if (row) {
    const when = new Date(row.timestamp_ms).toLocaleString();
    label.title = `${row.path} · ${when} · ${row.author} · #${row.id}`;
  }
  host.appendChild(label);
  // status: mode-controls-diff-toggle
  // Hidden for `op = "deleted"` rows — there's no `before` blob to diff
  // against, so the toggle's affordance lies. Other rows always offer it.
  if (row && row.op !== "deleted") {
    host.appendChild(
      iconButton({
        title: diffActive ? "Hide diff" : "Show diff vs current",
        pressed: diffActive,
        svg: ICON_DIFF,
        onClick: () => snapshotPreview.toggleDiff(),
      }),
    );
  }
  host.appendChild(
    iconButton({
      title: "Restore this version",
      svg: ICON_RESTORE,
      onClick: () => snapshotPreview.restore(),
    }),
  );
  host.appendChild(
    iconButton({
      title: "Close preview",
      svg: ICON_CLOSE,
      onClick: () => snapshotPreview.close(),
    }),
  );
});

// status: note-mutation-buffer-ro-while-in-flight
// `#mode-controls` renderer for the regular `file` buffer state.
// Surfaces the "Reformatting…" pill while a `NoteMutation` task is in
// flight on the active path (so the user knows why the buffer is RO).
// The dirty-buffer Diff toggle moved to the editor toolbar
// (`editor-diff-vs-disk-toggle`) so it's always visible alongside Save.
modeControls?.register("file", (host) => {
  if (!buffer || buffer.mode.kind !== "file") return;
  const path = buffer.path;
  if (inFlightMutationPaths.has(path)) {
    const pill = document.createElement("span");
    pill.className = "mode-label mode-label-pending";
    pill.textContent = "Reformatting…";
    pill.title = `${path} — note-mutation in progress`;
    host.appendChild(pill);
  }
});

modeControls?.register("trash", (host) => {
  const label = document.createElement("span");
  label.className = "mode-label";
  label.textContent = "Trash preview";
  const displayPath =
    buffer?.mode.kind === "trash"
      ? buffer.mode.displayPath
      : (buffer?.path ?? "");
  label.title = `${displayPath} — restore via the trash bin's right-click menu`;
  host.appendChild(label);
  host.appendChild(
    iconButton({
      title: "Close preview",
      svg: ICON_CLOSE,
      onClick: () => trash.closePreview(),
    }),
  );
});

// Watcher overflow toast; trash-changed listener lives inside the trash
// module now (it owns the cleanup of a previewed entry that vanished).
void listen("hiker:watcher-overflow", () => {
  showToast("Filesystem watcher fell behind — rescanning…");
});

interface LlmWarningPayload {
  kind: string;
  env?: string;
  message: string;
}
// status: llm-providers-config
// API-key preflight surface (per llm.md §Disable mode): the backend
// emits this on vault open when [llm].enabled = true and the configured
// api_key_env is unset, so the user sees the problem before they try to
// chat. Longer TTL than the default toast so the message is readable.
void listen<LlmWarningPayload>("hiker:llm-warning", (event) => {
  showToast(event.payload.message, undefined, 8000);
});

// ---------- watcher → editor integration ----------
// Reacts to external changes to the active buffer's file:
// - clean + modified → silent reload
// - dirty + modified → proactive conflict modal (Keep/Take/Cancel)
// - deleted (clean)  → close buffer + toast
// - deleted (dirty)  → keep buffer + toast ("save to recreate")
// - renamed          → buffer.path follows the new path silently
//
// status: watcher-editor-reload-clean
// status: watcher-editor-conflict-dirty
// status: watcher-editor-deleted-buffer
// status: watcher-editor-renamed-followup

type FileChangedEvent =
  | { kind: "created" | "modified" | "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string };

let watcherConflictPromptOpen = false;

void listen<FileChangedEvent>("hiker:file-changed", async (event) => {
  const ev = event.payload;
  // Tree shape changes don't depend on which buffer (if any) is active.
  // Schedule before buffer mutations so the rebuild reads the post-update
  // `buffer.path` (matters for the renamed branch's silent path follow).
  if (ev.kind === "created" || ev.kind === "deleted" || ev.kind === "renamed") {
    scheduleTreeRefreshFromWatcher();
    // status: vault-home-recent-modified — tree-shape changes can shift
    // which notes are in the top-N; modified-only events update mtimes.
    // External edits don't ride core::changes (deferred per `changes-write-path`
    // notes), so the watcher path keeps refreshing the recents widget directly
    // for that case. Internal saves are covered by `hiker:changes-appended` →
    // `refreshOnChangesAppended` upstream.
    vaultHome.notifyRecentModified();
  } else if (
    ev.kind === "modified"
    && (tree.getSortOrder() === "mtime-newest" || tree.getSortOrder() === "mtime-oldest")
  ) {
    // Tree *shape* doesn't change on Modified, but mtime-based sort orders
    // depend on per-entry mtime — a save reorders rows. Schedule a refresh
    // only when the chosen sort actually consumes mtime; under name sorts
    // we keep the existing no-op behavior.
    scheduleTreeRefreshFromWatcher();
  }
  if (ev.kind === "modified") {
    vaultHome.notifyRecentModified();
  }
  // Don't react while previewing a trash entry or a snapshot — both are
  // read-only views; mutating them would corrupt the user's intent. Trash
  // entries live under .hiker/trash/ which the watcher ignores anyway, but
  // snapshot previews share the live file path so this guard is the only
  // thing keeping a watcher event from clobbering the historic content.
  if (!buffer || isReadOnlyBuffer(buffer)) return;

  if (ev.kind === "modified" && ev.path === buffer.path) {
    if (isDirty()) {
      if (watcherConflictPromptOpen) return;
      watcherConflictPromptOpen = true;
      try {
        await handleWatcherConflictDirty(buffer.path);
      } finally {
        watcherConflictPromptOpen = false;
      }
      return;
    }
    try {
      const fresh = await invoke<FileWithHash>("read_file_with_hash", { rel: buffer.path });
      if (fresh.hash !== buffer.loadedHash) {
        view.dispatch({
          changes: { from: 0, to: view.state.doc.length, insert: fresh.contents },
        });
        buffer.loadedText = view.state.doc.toString();
        buffer.loadedHash = fresh.hash;
        updateStatus();
        scheduleChunkBoundariesRefresh(500);
      }
    } catch (err) {
      console.error("silent reload failed:", err);
    }
    return;
  }

  if (ev.kind === "deleted" && ev.path === buffer.path) {
    const path = buffer.path;
    if (isDirty()) {
      showToast(`${path} was removed on disk; save to recreate.`);
    } else {
      // status: editor-tab-strip — drop the tab for the removed path.
      openBuffers.delete(path);
      if (previewTabPath === path) previewTabPath = null;
      buffer = null;
      activePath = null;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
      updateStatus();
      tabStrip?.render();
      showToast(`${path} was removed externally`);
    }
    return;
  }

  if (ev.kind === "renamed" && ev.from === buffer.path) {
    buffer.path = ev.to;
    updateStatus();
    return;
  }
});

async function handleWatcherConflictDirty(path: string): Promise<void> {
  const choice = await confirm3(
    `${path} has been modified on disk while you have unsaved changes.`,
    "Keep mine",
    "Take theirs (reload from disk)",
    "Cancel",
  );
  // The buffer may have switched files (or closed) while the modal was up.
  if (!buffer || buffer.path !== path) return;
  // "Keep mine" / "Cancel": leave buffer untouched; the next save's pre-write
  // drift check will re-prompt because loadedHash no longer matches disk.
  if (choice !== "b") return;
  try {
    const fresh = await invoke<FileWithHash>("read_file_with_hash", { rel: path });
    if (!buffer || buffer.path !== path) return;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: fresh.contents },
    });
    buffer.loadedText = view.state.doc.toString();
    buffer.loadedHash = fresh.hash;
    updateStatus();
    scheduleChunkBoundariesRefresh(500);
  } catch (err) {
    console.error("watcher conflict reload failed:", err);
  }
}
