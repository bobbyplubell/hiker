import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { EditorState, Compartment, type Extension } from "@codemirror/state";
import { EditorView, ViewPlugin, highlightWhitespace } from "@codemirror/view";
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

type EntryKind = "dir" | "file";
interface DirEntry {
  name: string;
  rel_path: string;
  kind: EntryKind;
  mtime: number;
}
interface FileWithHash {
  contents: string;
  hash: string;
}
interface TrashEntry {
  id: string;
  original_path: string;
  trashed_name: string;
  original_mtime: number;
  deleted_at: number;
  kind: "file" | "folder";
  members?: string[] | null;
}
interface TrashListItem {
  id: string | null;
  trashed_name: string;
  original_path: string | null;
  deleted_at: number;
  kind: "file" | "folder";
  member_count: number | null;
  orphaned: boolean;
}
interface RelatedHit {
  note_id: string;
  path: string;
  title: string;
  score: number;
  best_heading_path: string | null;
  snippet: string;
}
// Mirrors `core::search::NoteHit`. Snippet may carry literal `<mark>...
// </mark>` substrings for lexical hits; the renderer parses these into
// styled spans rather than via `innerHTML`.
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
}
interface IndexStatus {
  model_ready: boolean;
  queued: number;
  total_notes: number;
  last_error: string | null;
}
type IndexState =
  | { kind: "indexed" }
  | { kind: "unsupported" }
  | { kind: "skipped"; reason: string }
  | { kind: "queued" };

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
    tree: { sort_by: "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc" };
  };
  search: {
    modes: { semantic: boolean; lexical: boolean };
    sections: { results_expanded: boolean; related_expanded: boolean };
  };
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
const trashBannerEl = document.getElementById("trash-banner")!;
const snapshotBannerEl = document.getElementById("snapshot-banner")!;
const snapshotBannerTextEl = document.getElementById("snapshot-banner-text")!;
const snapshotBannerRestoreBtn = document.getElementById(
  "snapshot-banner-restore",
) as HTMLButtonElement;
const snapshotBannerCloseBtn = document.getElementById(
  "snapshot-banner-close",
) as HTMLButtonElement;
const homeBtn = document.getElementById("home-btn") as HTMLButtonElement;
const editorPaneEl = document.getElementById("editor-pane")!;
const vaultHomeEl = document.getElementById("vault-home")!;

interface Buffer {
  path: string;
  loadedText: string;
  loadedHash: string;
  /// True when the buffer is a read-only preview of a trash entry. Save is
  /// disabled and the file-switch dirty guard skips this buffer.
  preview?: boolean;
  /// Display path for a trash preview (the original_path before deletion).
  /// Used by updateStatus so the basename in the status bar makes sense.
  displayPath?: string;
  /// True when the buffer is a read-only preview of a changelog snapshot
  /// (vault-home-recent-activity-detail). Save is disabled; banner shows
  /// snapshot metadata + Restore + Close. The dirty-switch guard treats
  /// snapshot previews like trash previews — there's nothing to discard.
  snapshotPreview?: boolean;
  /// The change_id whose content this preview is showing. Captured so the
  /// banner's [Restore] button can write it back without a second lookup.
  snapshotChangeId?: number;
}

let buffer: Buffer | null = null;

/// True for any read-only preview buffer (trash entry or changelog
/// snapshot). Most code paths that previously checked `buffer.preview`
/// want this broader check — both modes share the "no save, no dirty
/// state, switch without prompt" behavior. Trash-specific UI (the
/// "(in trash)" status suffix) keeps the narrower `buffer.preview` check.
function isReadOnlyBuffer(b: Buffer | null): boolean {
  return !!(b && (b.preview || b.snapshotPreview));
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
  const path = buffer?.preview
    ? (buffer.displayPath ?? buffer.path)
    : (buffer?.path ?? "");
  const titleSuffix = buffer?.preview
    ? " (in trash)"
    : buffer?.snapshotPreview
      ? " (snapshot)"
      : "";
  document.title =
    (dirty ? "• " : "") + (path ? `Hiker — ${path}${titleSuffix}` : "Hiker");
  // status: status-bar-path-basename-tooltip
  let basename = path ? (path.split("/").pop() ?? path) : "";
  if (buffer?.preview) basename += " (in trash)";
  else if (buffer?.snapshotPreview) basename += " (snapshot)";
  statusPathEl.textContent = basename;
  statusPathEl.title = buffer?.preview ? buffer.path : path;
  // status: status-bar-path-reveal — clickable when a real (non-trash) file
  // is open. Trash-preview paths live under `.hiker/trash/` and revealing
  // them would expose internal state, so the gesture is suppressed there.
  // Snapshot previews share the live file's path so reveal stays sensible.
  const revealable = !!buffer && !buffer.preview;
  statusPathEl.classList.toggle("clickable", revealable);
  statusPathEl.style.cursor = revealable ? "pointer" : "";
  saveBtn.disabled = !buffer || !dirty || isReadOnlyBuffer(buffer);
  saveBtn.classList.toggle("dirty", dirty);

  const sel = view.state.selection.main;
  const line = view.state.doc.lineAt(sel.head);
  const col = sel.head - line.from + 1;
  statusCursorEl.textContent = `${line.number}:${col}`;
  const text = view.state.doc.toString();
  const words = text.trim() === "" ? 0 : text.trim().split(/\s+/).length;
  statusWordsEl.textContent = `${words} word${words === 1 ? "" : "s"}`;

  if (buffer) {
    const li = document.querySelector(`#tree li[data-path="${cssEscape(buffer.path)}"]`);
    li?.classList.toggle("dirty", dirty);
  }
  // Center status label mirrors the active buffer's index state.
  renderIndexStatus();
}

const statusUpdater = ViewPlugin.fromClass(
  class {
    update() {
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
    focusSearchInput();
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
      statusUpdater,
      toCMKeymap(),
    ],
  }),
});

saveBtn.addEventListener("click", async () => {
  const ok = await save();
  if (ok) {
    scheduleRelatedRefresh(500);
    scheduleChunkBoundariesRefresh(500);
  }
});

async function save(): Promise<boolean> {
  if (!buffer) return false;
  const contents = view.state.doc.toString();
  try {
    const newHash = await invoke<string>("write_file_checked", {
      rel: buffer.path,
      expectedHash: buffer.loadedHash,
      contents,
    });
    buffer.loadedText = view.state.doc.toString();
    buffer.loadedHash = newHash;
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
      return true;
    }
    return false;
  }
  console.error("save failed:", err);
  alert(`save failed: ${typeof e === "string" ? e : JSON.stringify(e)}`);
  return false;
}

async function openFile(rel: string): Promise<void> {
  if (buffer && isDirty()) {
    const choice = await confirm3(
      `${buffer.path} has unsaved changes.`,
      "Save & switch",
      "Discard & switch",
      "Cancel",
    );
    if (choice === "cancel") return;
    if (choice === "a") {
      const ok = await save();
      if (!ok) return;
    }
  }
  try {
    const file = await invoke<FileWithHash>("read_file_with_hash", { rel });
    // Clear buffer before dispatch: the dispatch fires a synchronous
    // ViewPlugin update, and if `buffer` still pointed at the previous note
    // that update would compare the new doc text against the old note's
    // loadedText and flag the old note as dirty in the tree.
    buffer = null;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: file.contents },
      effects: [
        language.reconfigure(languageExtensionForPath(rel)),
        livePreviewCompartment.reconfigure(livePreviewExtensionForPath(rel)),
      ],
    });
    // Compare against CM's canonical doc representation (it normalizes CRLF
    // and similar on input), not the raw file string, or the buffer reads
    // dirty immediately on open.
    buffer = { path: rel, loadedText: view.state.doc.toString(), loadedHash: file.hash };
    setReadOnly(false);
    // status: vault-home-button — opening a note exits home view per spec
    // ("clicking any tree row, recents entry, or search result restores the
    // editor onto whichever note").
    if (isVaultHomeVisible()) setVaultHomeVisible(false);
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
    await revealInTree(rel);
    updateStatus();
    refreshChunkBoundaries();
    // status: note-access-tracking — fire-and-forget; the indexer task
    // stamps `notes.last_accessed_at` if the note is in the index. Errors
    // here aren't user-visible — recents will simply not include this open.
    invoke("note_accessed", { rel }).catch((err) => {
      console.error("note_accessed failed:", err);
    });
  } catch (err) {
    console.error("openFile failed:", rel, err);
    alert(`open failed: ${err}`);
  }
}

const cssEscape = (s: string): string => CSS.escape(s);

// Tracks the folder a "+ New note" click should target. Updated when the
// user clicks a folder row or a file row (file → its parent). Empty string
// means vault root.
let selectedFolder: string = "";

// Persists folder expansion state across `refreshTree` calls so a delete /
// rename / refresh doesn't collapse every open folder.
const expandedFolders = new Set<string>();

// status: tree-sort-options
// Default loaded from `vault.tree.sort_by` (per `settings-section-vault`);
// flips persist via `settings-write-back`.
type TreeSortOrder = "name-asc" | "name-desc" | "mtime-newest" | "mtime-oldest";
let treeSortOrder: TreeSortOrder = "name-asc";

function sortOrderFromSettings(s: Settings["vault"]["tree"]["sort_by"]): TreeSortOrder {
  switch (s) {
    case "name_asc": return "name-asc";
    case "name_desc": return "name-desc";
    case "mtime_desc": return "mtime-newest";
    case "mtime_asc": return "mtime-oldest";
  }
}

function sortOrderToSettings(o: TreeSortOrder): Settings["vault"]["tree"]["sort_by"] {
  switch (o) {
    case "name-asc": return "name_asc";
    case "name-desc": return "name_desc";
    case "mtime-newest": return "mtime_desc";
    case "mtime-oldest": return "mtime_asc";
  }
}

function sortTreeEntries(entries: DirEntry[]): DirEntry[] {
  // Folders always grouped first; the chosen order applies within folders
  // and within files (per editor.md `tree-sort-options`).
  const dirs = entries.filter((e) => e.kind === "dir");
  const files = entries.filter((e) => e.kind === "file");
  const cmp = sortComparator(treeSortOrder);
  dirs.sort(cmp);
  files.sort(cmp);
  return [...dirs, ...files];
}

function sortComparator(order: TreeSortOrder): (a: DirEntry, b: DirEntry) => number {
  switch (order) {
    case "name-asc":
      return (a, b) => a.name.toLowerCase().localeCompare(b.name.toLowerCase());
    case "name-desc":
      return (a, b) => b.name.toLowerCase().localeCompare(a.name.toLowerCase());
    case "mtime-newest":
      return (a, b) => b.mtime - a.mtime;
    case "mtime-oldest":
      return (a, b) => a.mtime - b.mtime;
  }
}

function parentOf(rel: string): string {
  const idx = rel.lastIndexOf("/");
  return idx >= 0 ? rel.slice(0, idx) : "";
}

async function renderDir(rel: string, container: HTMLElement): Promise<void> {
  const entries = sortTreeEntries(await invoke<DirEntry[]>("list_dir", { rel }));
  const ul = document.createElement("ul");
  // Track pending nested renders so `await renderDir(...)` only resolves once
  // every reachable expanded subtree is in the DOM. `revealInTree` relies on
  // this to look up its target row after a refresh.
  const pendingChildren: Promise<void>[] = [];
  for (const entry of entries) {
    const li = document.createElement("li");
    li.dataset.path = entry.rel_path;
    li.dataset.kind = entry.kind;
    li.draggable = true;
    renderTreeRowLabel(li, entry);
    attachDnd(li, entry);
    attachContextMenu(li, entry);
    if (entry.kind === "dir") {
      let expanded = expandedFolders.has(entry.rel_path);
      let childContainer: HTMLElement | null = null;
      if (expanded) {
        // Render children deferred until after the li is in the DOM (the
        // append below) — `li.after(...)` needs `li` to have a parent.
        renderTreeRowLabel(li, entry, true);
        const path = entry.rel_path;
        pendingChildren.push(
          new Promise<void>((resolve) => {
            queueMicrotask(() => {
              if (!expanded) {
                resolve();
                return;
              }
              childContainer = document.createElement("div");
              li.after(childContainer);
              renderDir(path, childContainer).then(resolve, resolve);
            });
          }),
        );
      }
      li.addEventListener("click", async (e) => {
        e.stopPropagation();
        // Skip the second click of a double-click; the dblclick handler
        // below (rename) takes over.
        if ((e as MouseEvent).detail >= 2) return;
        selectedFolder = entry.rel_path;
        if (expanded) {
          childContainer?.remove();
          childContainer = null;
          expanded = false;
          expandedFolders.delete(entry.rel_path);
          renderTreeRowLabel(li, entry, false);
        } else {
          childContainer = document.createElement("div");
          li.after(childContainer);
          await renderDir(entry.rel_path, childContainer);
          expanded = true;
          expandedFolders.add(entry.rel_path);
          renderTreeRowLabel(li, entry, true);
        }
      });
    } else {
      li.addEventListener("click", (e) => {
        e.stopPropagation();
        if ((e as MouseEvent).detail >= 2) return;
        selectedFolder = parentOf(entry.rel_path);
        void openFile(entry.rel_path);
      });
    }
    // status: tree-double-click-rename
    li.addEventListener("dblclick", (e) => {
      e.preventDefault();
      e.stopPropagation();
      void beginInlineRename(li, entry.rel_path, entry.kind);
    });
    ul.appendChild(li);
  }
  container.appendChild(ul);
  await Promise.all(pendingChildren);
}

// File extensions the indexer chunks. Fetched once at vault open from the
// `indexable_extensions` Tauri command (single source of truth =
// `core::indexer::INDEXABLE_EXTENSIONS`) and cached here for the
// client-side `tree-row-unsupported-marker` derivation so we don't pay a
// Tauri round trip on every visible row.
let indexableExts = new Set<string>(["md", "markdown", "txt"]);
function isIndexableExt(rel: string): boolean {
  const dot = rel.lastIndexOf(".");
  if (dot <= rel.lastIndexOf("/")) return false;
  return indexableExts.has(rel.slice(dot + 1).toLowerCase());
}

// Per-path index-state cache so re-renders don't re-fetch on every paint.
// Cleared on tree refresh and updated by progress-event handlers.
const indexStateCache = new Map<string, IndexState>();
const inflightStateFetches = new Set<string>();

async function fetchIndexState(rel: string): Promise<IndexState> {
  // status: tree-row-skipped-marker / tree-row-queued-marker
  const state = await invoke<IndexState>("index_state_for", { rel });
  indexStateCache.set(rel, state);
  return state;
}

function applyIndexMarker(li: HTMLElement, state: IndexState | null): void {
  li.classList.remove("ix-unsupported", "ix-skipped", "ix-queued", "ix-indexed");
  li.removeAttribute("data-ix-reason");
  // Suffix dot lives in a child span so it sits on the same side as the
  // dirty dot (li::after) without fighting it for the single ::after slot.
  let marker = li.querySelector<HTMLSpanElement>(":scope > .ix-marker");
  if (state && state.kind !== "indexed") {
    if (!marker) {
      marker = document.createElement("span");
      marker.className = "ix-marker";
      li.append(marker);
    }
  } else if (marker) {
    marker.remove();
  }
  if (!state) {
    li.removeAttribute("title");
    return;
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
}

// status: tree-row-unsupported-marker / tree-row-skipped-marker / tree-row-queued-marker
function renderTreeRowLabel(
  li: HTMLLIElement,
  entry: DirEntry,
  expanded = false,
): void {
  li.textContent = "";
  // Folders: just the chevron + name. Spec is explicit that folders are
  // never marked.
  if (entry.kind === "dir") {
    li.append(document.createTextNode((expanded ? "▾ " : "▸ ") + entry.name));
    return;
  }
  li.append(document.createTextNode(entry.name));

  const cached = indexStateCache.get(entry.rel_path);
  if (cached) {
    applyIndexMarker(li, cached);
    return;
  }
  // Cheap client-side Unsupported derivation: if the extension has no
  // chunker, set the marker immediately and skip the round trip.
  if (!isIndexableExt(entry.rel_path)) {
    const state: IndexState = { kind: "unsupported" };
    indexStateCache.set(entry.rel_path, state);
    applyIndexMarker(li, state);
    return;
  }
  // Lazy fetch for the rest. Multiple visible rows can request the same
  // path during a refresh; coalesce.
  const path = entry.rel_path;
  if (inflightStateFetches.has(path)) return;
  inflightStateFetches.add(path);
  void fetchIndexState(path)
    .then((state) => {
      // Re-find the row (it may have been re-rendered); apply.
      document
        .querySelectorAll(`#tree li[data-path="${cssEscape(path)}"]`)
        .forEach((el) => applyIndexMarker(el as HTMLElement, state));
      if (buffer && !isReadOnlyBuffer(buffer) && buffer.path === path) {
        renderIndexStatus();
      }
    })
    .catch((err) => {
      console.error("index_state_for failed:", path, err);
    })
    .finally(() => {
      inflightStateFetches.delete(path);
    });
}

// status: drag-and-drop-move
function attachDnd(li: HTMLLIElement, entry: DirEntry): void {
  // Folder rows are draggable too — the drop calls `move_folder`, which does
  // a single fs rename + bulk index remap for every contained `.md` file.
  // Empty subfolders move with the rename for free.
  li.addEventListener("dragstart", (e) => {
    e.dataTransfer?.setData("text/plain", entry.rel_path);
    e.dataTransfer?.setData(
      "application/x-hiker-kind",
      entry.kind === "dir" ? "dir" : "file",
    );
    if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
    li.classList.add("dragging");
  });
  li.addEventListener("dragend", () => li.classList.remove("dragging"));

  li.addEventListener("dragover", (e) => {
    const src = e.dataTransfer?.types.includes("text/plain");
    if (!src) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
    li.classList.add("drop-target");
  });
  li.addEventListener("dragleave", () => li.classList.remove("drop-target"));

  li.addEventListener("drop", async (e) => {
    e.preventDefault();
    e.stopPropagation();
    li.classList.remove("drop-target");
    const from = e.dataTransfer?.getData("text/plain");
    if (!from) return;
    const fromKind = e.dataTransfer?.getData("application/x-hiker-kind") === "dir"
      ? "dir"
      : "file";
    // Drop onto folder → move into folder. Drop onto file → file's parent.
    const targetFolder = entry.kind === "dir" ? entry.rel_path : parentOf(entry.rel_path);
    await performDrop(from, fromKind, targetFolder);
  });
}

async function performDrop(
  from: string,
  fromKind: "dir" | "file",
  targetFolder: string,
): Promise<void> {
  if (from === targetFolder) return;
  const fromParent = parentOf(from);
  if (fromParent === targetFolder) return; // same parent → no-op per spec
  const name = from.split("/").pop()!;
  // Don't allow dropping a folder into itself or its descendants.
  if (targetFolder === from || targetFolder.startsWith(from + "/")) return;
  const to = targetFolder ? `${targetFolder}/${name}` : name;
  const cmd = fromKind === "dir" ? "move_folder" : "move_note";
  try {
    await invoke(cmd, { from, to });
    // If the open buffer was inside the moved subtree, follow its new path.
    if (buffer) {
      if (buffer.path === from) {
        buffer.path = to;
        updateStatus();
      } else if (fromKind === "dir" && buffer.path.startsWith(from + "/")) {
        buffer.path = to + buffer.path.slice(from.length);
        updateStatus();
      }
    }
    await refreshTree();
  } catch (err) {
    console.error(`${cmd} failed:`, err);
    alert(`move failed: ${formatError(err)}`);
  }
}

function formatError(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object" && "message" in err) {
    const m = (err as { message: unknown }).message;
    return typeof m === "string" ? m : JSON.stringify(err);
  }
  return JSON.stringify(err);
}

/// Ensure the tree row for `rel` is visible (ancestor folders expanded) and
/// marked active, then scroll it into view. Used by `openFile` so opening a
/// note from the related-notes list, search results, etc. expands the
/// folders that contain it instead of silently failing the highlight.
async function revealInTree(rel: string): Promise<void> {
  // Add every ancestor folder to the expansion set.
  let added = false;
  let cursor = parentOf(rel);
  while (cursor !== "") {
    if (!expandedFolders.has(cursor)) {
      expandedFolders.add(cursor);
      added = true;
    }
    cursor = parentOf(cursor);
  }
  if (added) {
    await refreshTree();
  }
  const row = document.querySelector(
    `#tree li[data-path="${cssEscape(rel)}"]`,
  );
  row?.classList.add("active");
  row?.scrollIntoView({ block: "nearest" });
}

async function refreshTree(): Promise<void> {
  treeEl.innerHTML = "";
  await renderDir("", treeEl);
  // Restore active highlight on the open file, if any.
  if (buffer) {
    document
      .querySelector(`#tree li[data-path="${cssEscape(buffer.path)}"]`)
      ?.classList.add("active");
  }
}

// status: tree-refresh-watcher
// Debounce a single tree rebuild across bursts of watcher events (git
// checkout, mass copy, multi-file rename). 200ms matches the watcher's own
// debounce window so a single logical fs change → at most one rebuild.
let treeRefreshDebounce: number | null = null;
function scheduleTreeRefreshFromWatcher(): void {
  if (treeRefreshDebounce !== null) window.clearTimeout(treeRefreshDebounce);
  treeRefreshDebounce = window.setTimeout(() => {
    treeRefreshDebounce = null;
    void refreshTree();
  }, 200);
}

// Tree-root drop zone: dropping on empty space below the tree moves to root.
treeEl.addEventListener("dragover", (e) => {
  if (!e.dataTransfer?.types.includes("text/plain")) return;
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
});
treeEl.addEventListener("drop", async (e) => {
  // Only handle drops that fell through every li (li handlers stopPropagation).
  e.preventDefault();
  const from = e.dataTransfer?.getData("text/plain");
  if (!from) return;
  const fromKind = e.dataTransfer?.getData("application/x-hiker-kind") === "dir"
    ? "dir"
    : "file";
  await performDrop(from, fromKind, "");
});

// status: tree-context-menu — empty-space menu (right-click below the rows)
treeEl.addEventListener("contextmenu", (e) => {
  // li handlers stopPropagation, so this only fires on real empty space.
  e.preventDefault();
  openContextMenu(e.clientX, e.clientY, [
    {
      label: "New note here",
      run: async () => {
        selectedFolder = "";
        newNoteBtn.click();
      },
    },
  ]);
});

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
  selectedFolder = "";
  vaultIsOpen = true;
  outstandingCount = 0;

  // Refresh the indexable-extension allowlist so the tree's Unsupported
  // marker derivation matches the backend without per-row round trips.
  // Failures aren't fatal — the seeded fallback is the v1 set.
  try {
    const exts = await invoke<string[]>("indexable_extensions");
    indexableExts = new Set(exts.map((e) => e.toLowerCase()));
  } catch (err) {
    console.error("indexable_extensions failed:", err);
  }

  // status: settings-load-once-at-startup
  // Seed View menu / tree / panel state from the merged settings. Failures
  // here aren't fatal — fall back to whatever the in-memory defaults are.
  try {
    const s = await invoke<Settings>("get_settings");
    renderTxtAsMarkdown = s.editor.render_txt_as_markdown;
    setLivePreviewEnabled(s.editor.live_preview);
    setWordWrapEnabled(s.editor.word_wrap);
    setLineNumbersVisible(s.editor.show_line_numbers);
    setWhitespaceEnabled(s.editor.show_whitespace);
    setChunkBoundariesEnabled(s.editor.show_chunk_boundaries);
    setHideFrontmatterEnabled(s.editor.hide_frontmatter);
    treeSortOrder = sortOrderFromSettings(s.vault.tree.sort_by);
    appEl.classList.toggle("sidebar-collapsed", !s.vault.sidebar_open);
    appEl.classList.toggle("related-collapsed", !s.vault.related_open);
    trashBinEl.classList.toggle("collapsed", !s.vault.trash_expanded);
    trashChevronEl.textContent = s.vault.trash_expanded ? "▾" : "▸";
    // status: search-mode-state-persisted, search-section-collapsible
    setSearchModeSemantic(s.search.modes.semantic, false);
    setSearchModeLexical(s.search.modes.lexical, false);
    setSearchSectionExpanded(s.search.sections.results_expanded, false);
    setRelatedSectionExpanded(s.search.sections.related_expanded, false);
    syncToggleButtons();
  } catch (err) {
    console.error("get_settings failed:", err);
  }
  // Stale per-path state from a prior vault must not leak into the new one
  // (paths can collide across vaults).
  indexStateCache.clear();
  inflightStateFetches.clear();
  // Clear the related-notes panel so hits from the prior vault don't linger
  // until the next file open / save populates it for the new vault.
  void refreshRelated(null);
  // Likewise, blank the search input/results so prior-vault matches don't
  // surface in the new vault. status: search-discovery-panel
  clearSearchPanel();
  startBackgroundIntervals();
  await refreshTree();
  await refreshTrashBin();
  // status: vault-home-screen — default landing surface on vault open
  // (no auto-resume of last buffer in v1).
  setVaultHomeVisible(true);
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

// status: create-note-button
newNoteBtn.addEventListener("click", async () => {
  try {
    const created = await invoke<string>("create_note", { folder: selectedFolder });
    await refreshTree();
    await openFile(created);
    const li = document.querySelector(`#tree li[data-path="${cssEscape(created)}"]`) as
      | HTMLLIElement
      | null;
    if (li) await beginInlineRename(li, created);
  } catch (err) {
    console.error("create_note failed:", err);
    alert(`new note failed: ${formatError(err)}`);
  }
});

// status: tree-toolbar-actions-menu
treeActionsBtn.addEventListener("click", (e) => {
  e.stopPropagation();
  const rect = treeActionsBtn.getBoundingClientRect();
  const activePath =
    buffer && !isReadOnlyBuffer(buffer) ? buffer.path : null;
  openContextMenu(rect.right, rect.bottom, [
    {
      // status: tree-refresh-manual
      label: "Refresh tree",
      run: async () => {
        await refreshTree();
        await refreshTrashBin();
      },
    },
    {
      // status: reindex-all-action
      label: "Reindex all",
      run: async () => {
        try {
          await invoke("index", { scope: { kind: "all" } });
        } catch (err) {
          console.error("reindex all failed:", err);
          alert(`reindex failed: ${formatError(err)}`);
        }
      },
    },
    {
      // status: reindex-current-file-action
      label: "Reindex this file",
      disabled: activePath === null,
      run: async () => {
        if (!activePath) return;
        try {
          await invoke("index", { scope: { kind: "path", rel: activePath } });
        } catch (err) {
          console.error("reindex file failed:", err);
          alert(`reindex failed: ${formatError(err)}`);
        }
      },
    },
    {
      // status: tree-sort-options
      label: `Sort by  ▸  ${sortOrderLabel(treeSortOrder)}`,
      run: () => openSortByMenu(rect.right, rect.bottom),
    },
  ]);
});

function sortOrderLabel(order: TreeSortOrder): string {
  switch (order) {
    case "name-asc": return "Name (A→Z)";
    case "name-desc": return "Name (Z→A)";
    case "mtime-newest": return "Modified (newest first)";
    case "mtime-oldest": return "Modified (oldest first)";
  }
}

function openSortByMenu(x: number, y: number): void {
  const orders: TreeSortOrder[] = [
    "name-asc",
    "name-desc",
    "mtime-newest",
    "mtime-oldest",
  ];
  openContextMenu(
    x,
    y,
    orders.map((o) => ({
      label: sortOrderLabel(o),
      checked: treeSortOrder === o,
      run: async () => {
        if (treeSortOrder === o) return;
        treeSortOrder = o;
        await refreshTree();
        void persistSetting("vault", "vault.tree.sort_by", sortOrderToSettings(o));
      },
    })),
  );
}

async function beginInlineRename(
  li: HTMLLIElement,
  currentPath: string,
  kind: "file" | "dir" = "file",
): Promise<void> {
  const name = currentPath.split("/").pop()!;
  // Folders have no extension to exclude — pre-select the whole basename.
  // Files preserve the existing "select stem, leave .md" behavior.
  const dotIdx = kind === "file" ? name.lastIndexOf(".") : -1;
  const stemEnd = dotIdx > 0 ? dotIdx : name.length;
  const input = document.createElement("input");
  input.type = "text";
  input.className = "tree-rename-input";
  input.value = name;
  li.textContent = "";
  li.appendChild(input);
  input.focus();
  input.setSelectionRange(0, stemEnd);

  await new Promise<void>((resolve) => {
    let done = false;
    const finish = async (commit: boolean) => {
      if (done) return;
      done = true;
      const newName = input.value.trim();
      if (commit && newName && newName !== name) {
        const parent = parentOf(currentPath);
        const to = parent ? `${parent}/${newName}` : newName;
        const cmd = kind === "dir" ? "move_folder" : "move_note";
        try {
          await invoke(cmd, { from: currentPath, to });
          if (kind === "dir") {
            // Preserve expansion state across the rename: any path under
            // the old folder needs its prefix swapped, and the renamed
            // folder itself stays expanded if it was before.
            const fromPrefix = currentPath + "/";
            const remapped = new Set<string>();
            for (const p of expandedFolders) {
              if (p === currentPath) {
                remapped.add(to);
              } else if (p.startsWith(fromPrefix)) {
                remapped.add(to + p.slice(currentPath.length));
              } else {
                remapped.add(p);
              }
            }
            expandedFolders.clear();
            for (const p of remapped) expandedFolders.add(p);
          }
          if (buffer) {
            if (buffer.path === currentPath) {
              buffer.path = to;
              updateStatus();
            } else if (kind === "dir" && buffer.path.startsWith(currentPath + "/")) {
              buffer.path = to + buffer.path.slice(currentPath.length);
              updateStatus();
            }
          }
        } catch (err) {
          console.error("rename failed:", err);
          alert(`rename failed: ${formatError(err)}`);
        }
      }
      await refreshTree();
      resolve();
    };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        void finish(true);
      } else if (e.key === "Escape") {
        e.preventDefault();
        void finish(false);
      }
    });
    input.addEventListener("blur", () => void finish(true));
  });
}

// status: tree-context-menu
interface CtxMenuItem {
  label: string;
  run?: () => void | Promise<void>;
  disabled?: boolean;
  danger?: boolean;
  checked?: boolean;
  tooltip?: string;
}

let openMenuEl: HTMLElement | null = null;

function closeContextMenu(): void {
  if (openMenuEl) {
    openMenuEl.remove();
    openMenuEl = null;
  }
}

function openContextMenu(x: number, y: number, items: CtxMenuItem[]): void {
  closeContextMenu();
  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  menu.setAttribute("role", "menu");
  for (const item of items) {
    const btn = document.createElement("button");
    let cls = "ctx-menu-item";
    if (item.danger) cls += " danger";
    if (item.checked !== undefined) cls += " checkable";
    if (item.checked) cls += " checked";
    btn.className = cls;
    btn.textContent = item.label;
    btn.disabled = item.disabled === true;
    if (item.tooltip) btn.title = item.tooltip;
    btn.addEventListener("click", async () => {
      closeContextMenu();
      if (item.run) await item.run();
    });
    menu.appendChild(btn);
  }
  document.body.appendChild(menu);
  // Position: clamp inside the viewport so the menu doesn't get clipped
  // when the click lands near the right/bottom edge.
  const rect = menu.getBoundingClientRect();
  const left = Math.min(x, window.innerWidth - rect.width - 4);
  const top = Math.min(y, window.innerHeight - rect.height - 4);
  menu.style.left = `${Math.max(4, left)}px`;
  menu.style.top = `${Math.max(4, top)}px`;
  openMenuEl = menu;

  const onDocDown = (ev: MouseEvent) => {
    if (!menu.contains(ev.target as Node)) closeContextMenu();
  };
  const onKey = (ev: KeyboardEvent) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeContextMenu();
    }
  };
  // mousedown so a click outside dismisses before its own click handler fires.
  setTimeout(() => {
    document.addEventListener("mousedown", onDocDown, true);
    document.addEventListener("keydown", onKey, true);
  });
  const cleanup = new MutationObserver(() => {
    if (!document.body.contains(menu)) {
      document.removeEventListener("mousedown", onDocDown, true);
      document.removeEventListener("keydown", onKey, true);
      cleanup.disconnect();
    }
  });
  cleanup.observe(document.body, { childList: true });
}

function attachContextMenu(li: HTMLLIElement, entry: DirEntry): void {
  li.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const items: CtxMenuItem[] = [];
    if (entry.kind === "file") {
      items.push({ label: "Open", run: () => openFile(entry.rel_path) });
    }
    items.push({
      label: "Rename",
      run: () => beginInlineRename(li, entry.rel_path, entry.kind),
    });
    items.push({
      label: "Delete",
      danger: true,
      run: () => deleteFromTree(entry),
    });
    // status: tree-context-properties — greyed-out stub until frontmatter
    // editing exists.
    items.push({ label: "Properties", disabled: true });
    openContextMenu(e.clientX, e.clientY, items);
  });
}

// status: tree-context-delete
async function deleteFromTree(entry: DirEntry): Promise<void> {
  let memberCount = 0;
  if (entry.kind === "dir") {
    try {
      memberCount = await countNotesIn(entry.rel_path);
    } catch (err) {
      console.error("countNotesIn failed:", err);
    }
  }
  const bufferUnderEntry =
    !!buffer &&
    (buffer.path === entry.rel_path ||
      buffer.path.startsWith(entry.rel_path + "/"));
  const dirtyTail = bufferUnderEntry && isDirty()
    ? " Unsaved changes will be discarded."
    : "";
  const message =
    entry.kind === "dir"
      ? `Move ${entry.rel_path} and ${memberCount} note${memberCount === 1 ? "" : "s"} inside it to trash?${dirtyTail}`
      : `Move ${entry.rel_path} to trash?${dirtyTail}`;

  const ok = await confirmDanger(message, "Move to trash");
  if (!ok) return;

  try {
    const result = await invoke<TrashEntry>("delete_note", { rel: entry.rel_path });
    // If the deleted file (or a folder containing it) was the open buffer, clear it.
    if (
      buffer &&
      (buffer.path === entry.rel_path || buffer.path.startsWith(entry.rel_path + "/"))
    ) {
      buffer = null;
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: "" },
      });
      updateStatus();
    }
    await refreshTree();
    const message =
      result.kind === "folder"
        ? `Moved ${result.original_path} to trash (${result.members?.length ?? 0} notes)`
        : `Moved ${result.original_path} to trash`;
    showToast(message, {
      label: "Undo",
      run: async () => {
        try {
          const restored = await invoke<TrashEntry>("restore_trash_entry", { id: result.id });
          await refreshTree();
          showToast(`Restored ${restored.original_path}`);
        } catch (err) {
          console.error("restore_trash_entry failed:", err);
          alert(`restore failed: ${formatError(err)}`);
        }
      },
    });
  } catch (err) {
    console.error("delete_note failed:", err);
    alert(`delete failed: ${formatError(err)}`);
  }
}

async function countNotesIn(rel: string): Promise<number> {
  let count = 0;
  const entries = await invoke<DirEntry[]>("list_dir", { rel });
  for (const e of entries) {
    if (e.kind === "file" && e.name.toLowerCase().endsWith(".md")) {
      count += 1;
    } else if (e.kind === "dir") {
      count += await countNotesIn(e.rel_path);
    }
  }
  return count;
}

function confirmDanger(message: string, dangerLabel: string): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    const msg = document.createElement("p");
    msg.className = "modal-message";
    msg.textContent = message;
    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "modal-btn";
    cancelBtn.textContent = "Cancel";
    const dangerBtn = document.createElement("button");
    dangerBtn.className = "modal-btn modal-btn-danger";
    dangerBtn.textContent = dangerLabel;
    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (v: boolean) => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(v);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); finish(false); }
      else if (e.key === "Enter") { e.preventDefault(); finish(false); }
    };
    cancelBtn.addEventListener("click", () => finish(false));
    dangerBtn.addEventListener("click", () => finish(true));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish(false);
    });
    document.addEventListener("keydown", onKey, true);
    btnRow.append(dangerBtn, cancelBtn);
    dialog.append(msg, btnRow);
    overlay.append(dialog);
    document.body.append(overlay);
    // Default focus on Cancel — destructive action requires a deliberate move.
    cancelBtn.focus();
  });
}

let toastTimer: number | null = null;
interface ToastAction {
  label: string;
  run: () => void | Promise<void>;
}
function showToast(message: string, action?: ToastAction, ttlMs = 5000): void {
  let toast = document.getElementById("toast") as HTMLDivElement | null;
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "toast";
    document.body.appendChild(toast);
  }
  toast.innerHTML = "";
  const msgEl = document.createElement("span");
  msgEl.className = "toast-message";
  msgEl.textContent = message;
  toast.appendChild(msgEl);
  if (action) {
    const btn = document.createElement("button");
    btn.className = "toast-action";
    btn.textContent = action.label;
    btn.addEventListener("click", async () => {
      // Hide immediately on click so a slow restore doesn't leave the toast lingering.
      toast?.classList.remove("visible");
      if (toastTimer !== null) window.clearTimeout(toastTimer);
      await action.run();
    });
    toast.appendChild(btn);
  }
  toast.classList.add("visible");
  if (toastTimer !== null) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toast?.classList.remove("visible");
  }, ttlMs);
}

function confirm3(
  message: string,
  a: string,
  b: string,
  cancel: string,
): Promise<"a" | "b" | "cancel"> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";

    const dialog = document.createElement("div");
    dialog.className = "modal-dialog";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const msg = document.createElement("p");
    msg.className = "modal-message";
    msg.textContent = message;

    const btnRow = document.createElement("div");
    btnRow.className = "modal-buttons";

    const aBtn = document.createElement("button");
    aBtn.className = "modal-btn modal-btn-primary";
    aBtn.textContent = a;
    const bBtn = document.createElement("button");
    bBtn.className = "modal-btn";
    bBtn.textContent = b;
    const cBtn = document.createElement("button");
    cBtn.className = "modal-btn";
    cBtn.textContent = cancel;

    const previouslyFocused = document.activeElement as HTMLElement | null;
    const finish = (choice: "a" | "b" | "cancel") => {
      document.removeEventListener("keydown", onKey, true);
      overlay.remove();
      previouslyFocused?.focus?.();
      resolve(choice);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); finish("cancel"); }
      else if (e.key === "Enter") { e.preventDefault(); finish("a"); }
      else if (e.key === "1") { e.preventDefault(); finish("a"); }
      else if (e.key === "2") { e.preventDefault(); finish("b"); }
    };

    aBtn.addEventListener("click", () => finish("a"));
    bBtn.addEventListener("click", () => finish("b"));
    cBtn.addEventListener("click", () => finish("cancel"));
    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) finish("cancel");
    });
    document.addEventListener("keydown", onKey, true);

    btnRow.append(cBtn, bBtn, aBtn);
    dialog.append(msg, btnRow);
    overlay.append(dialog);
    document.body.append(overlay);
    aBtn.focus();
  });
}

const win = getCurrentWindow();
// status: window-close-guard-dirty
// Always preventDefault and drive the close ourselves via `win.destroy()`.
// Returning without preventDefault to "let Tauri close" was unreliable in
// practice (the X button became a no-op), and `win.close()` would re-enter
// this handler — `destroy()` skips the close-requested round-trip and
// terminates the window directly.
void win.onCloseRequested(async (event) => {
  event.preventDefault();
  if (buffer && isDirty()) {
    const choice = await confirm3(
      `${buffer.path} has unsaved changes.`,
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
  buffer = null;
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

// status: vault-home-button
// Icon-only home button in the vault bar that toggles the editor pane to a
// vault-home view. View toggle, not buffer close — the active buffer stays
// in memory; opening any note (tree click, search hit, etc.) restores the
// editor onto whichever note via `setVaultHomeVisible(false)` below.
//
// Reserved keybind id: `vault.go-home` (chord TBD per editor.md). Not
// registered in `keybind-registry` until the chord is decided, matching the
// "reserved IDs not registered as no-ops in v0" convention in editor.md.
function isVaultHomeVisible(): boolean {
  return editorPaneEl.classList.contains("home-view");
}
function setVaultHomeVisible(on: boolean): void {
  editorPaneEl.classList.toggle("home-view", on);
  vaultHomeEl.hidden = !on;
  homeBtn.classList.toggle("active", on);
  if (on) void refreshVaultHome();
}
homeBtn.addEventListener("click", () => {
  setVaultHomeVisible(!isVaultHomeVisible());
});

// status: vault-home-screen
// Three stacked widgets: stats / recently modified / recently accessed.
// Refreshes are coalesced via tiny debounce timers so a flurry of
// hiker:reindex-progress events doesn't fire one Tauri call per event.
const vaultHomeTitleEl = document.getElementById("vault-home-title")!;
const vaultHomeStatsBodyEl = document.getElementById("vault-home-stats-body")!;
const vaultHomeModifiedListEl = document.getElementById("vault-home-modified-list")!;
const vaultHomeAccessedListEl = document.getElementById("vault-home-accessed-list")!;
const vaultHomeNewNoteBtn = document.getElementById("vault-home-new-note") as HTMLButtonElement;

interface VaultHomeStats {
  total_notes: number;
  total_chunks: number;
  indexed: number;
  skipped: number;
  queued: number;
}
interface RecentNote {
  path: string;
  title: string;
  mtime: number;
  last_accessed_at: number | null;
}

async function refreshVaultHome(): Promise<void> {
  if (!vaultIsOpen) return;
  vaultHomeTitleEl.textContent = vaultPathEl.textContent || "Vault";
  // status: vault-home-detail-views — the Home button always returns to
  // the overview; clicking the home button while in a detail view exits
  // detail mode rather than re-rendering it.
  showHomeOverview();
  await Promise.all([
    refreshVaultHomeStats(),
    refreshVaultHomeRecentModified(),
    refreshVaultHomeRecentAccessed(),
    refreshActivityWidget(),
  ]);
}

async function refreshVaultHomeStats(): Promise<void> {
  try {
    const stats = await invoke<VaultHomeStats>("vault_home_stats");
    renderVaultHomeStats(stats);
  } catch (err) {
    console.error("vault_home_stats failed:", err);
    vaultHomeStatsBodyEl.replaceChildren(
      buildStatEmpty(`Failed to load stats: ${formatError(err)}`),
    );
  }
}

function renderVaultHomeStats(stats: VaultHomeStats): void {
  const cells: Array<[string, number]> = [
    ["Notes", stats.total_notes],
    ["Indexed", stats.indexed],
    ["Chunks", stats.total_chunks],
    ["Queued", stats.queued],
    ["Skipped", stats.skipped],
  ];
  vaultHomeStatsBodyEl.replaceChildren(
    ...cells.map(([label, num]) => {
      const cell = document.createElement("div");
      cell.className = "vault-home-stat";
      const numEl = document.createElement("div");
      numEl.className = "num";
      numEl.textContent = String(num);
      const lbl = document.createElement("div");
      lbl.className = "label";
      lbl.textContent = label;
      cell.append(numEl, lbl);
      return cell;
    }),
  );
}

function buildStatEmpty(text: string): HTMLElement {
  const el = document.createElement("div");
  el.className = "vault-home-stat-empty";
  el.textContent = text;
  return el;
}

async function refreshVaultHomeRecentModified(): Promise<void> {
  try {
    const rows = await invoke<RecentNote[]>("recent_notes_modified", { limit: 10 });
    renderRecentList(vaultHomeModifiedListEl, rows, "mtime", "No notes indexed yet.");
  } catch (err) {
    console.error("recent_notes_modified failed:", err);
    renderRecentList(vaultHomeModifiedListEl, [], "mtime", `Error: ${formatError(err)}`);
  }
}

async function refreshVaultHomeRecentAccessed(): Promise<void> {
  try {
    const rows = await invoke<RecentNote[]>("recent_notes_accessed", { limit: 10 });
    renderRecentList(
      vaultHomeAccessedListEl,
      rows,
      "accessed",
      "No recently opened notes.",
    );
  } catch (err) {
    console.error("recent_notes_accessed failed:", err);
    renderRecentList(vaultHomeAccessedListEl, [], "accessed", `Error: ${formatError(err)}`);
  }
}

function renderRecentList(
  ul: HTMLElement,
  rows: RecentNote[],
  field: "mtime" | "accessed",
  emptyText: string,
): void {
  if (rows.length === 0) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = emptyText;
    ul.replaceChildren(li);
    return;
  }
  ul.replaceChildren(
    ...rows.map((r) => {
      const li = document.createElement("li");
      li.dataset.path = r.path;
      const ts = field === "mtime" ? r.mtime : (r.last_accessed_at ?? r.mtime);
      const when = relativeTime(ts);
      const nameEl = document.createElement("span");
      nameEl.className = "name";
      nameEl.textContent = r.title;
      const relEl = document.createElement("span");
      relEl.className = "rel";
      const parent = r.path.includes("/") ? r.path.slice(0, r.path.lastIndexOf("/")) : "";
      relEl.textContent = parent;
      const whenEl = document.createElement("span");
      whenEl.className = "when";
      whenEl.textContent = when;
      whenEl.title = new Date(ts * 1000).toLocaleString();
      li.append(nameEl, relEl, whenEl);
      li.addEventListener("click", () => void openFile(r.path));
      ul.appendChild(li);
      return li;
    }),
  );
}

let vaultHomeStatsRefreshTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleVaultHomeStatsRefresh(delay = 250): void {
  if (!isVaultHomeVisible()) return;
  if (vaultHomeStatsRefreshTimer !== null) clearTimeout(vaultHomeStatsRefreshTimer);
  vaultHomeStatsRefreshTimer = setTimeout(() => {
    vaultHomeStatsRefreshTimer = null;
    void refreshVaultHomeStats();
  }, delay);
}

let vaultHomeModifiedRefreshTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleVaultHomeModifiedRefresh(delay = 400): void {
  if (!isVaultHomeVisible()) return;
  if (vaultHomeModifiedRefreshTimer !== null) clearTimeout(vaultHomeModifiedRefreshTimer);
  vaultHomeModifiedRefreshTimer = setTimeout(() => {
    vaultHomeModifiedRefreshTimer = null;
    void refreshVaultHomeRecentModified();
  }, delay);
}

// status: vault-home-recent-activity-widget
// status: vault-home-recent-activity-detail
// status: vault-home-recent-activity-author-filter
// status: vault-home-recent-activity-unrollback
//
// Append-only changelog UI: a fourth home-page widget that shows the most
// recent vault writes (user saves; eventually agent writes via MCP). The
// widget is hidden when the changelog is empty so a fresh post-upgrade
// vault doesn't render a confusing zero-count tile. Click the header or
// any preview row → detail view (overview-body swap; Home button returns).
//
// Rollback flow per docs/changes.md "Rollback":
//   1. resolve prior_content via Tauri `previous_content_for_path`
//   2. write it back via `rollback_change`, which appends a new modified
//      row stamped with metadata.rolled_back_from = <id>
//   3. UI re-fetches `recent_changes` on the next event.
//
// Un-rollback is the same primitive — just a rollback to a more recent
// prior state — so the affordance is "click rollback on a newer row." We
// add a one-click shortcut: immediately after a rollback, the detail view
// shows a small "Recently rolled back — restore?" prompt next to the row
// whose state was reverted away from. Clicking it rolls back to that row.
type ChangeOp = "created" | "modified" | "deleted" | "renamed";
interface ChangeRow {
  id: number;
  timestamp_ms: number;
  path: string;
  op: ChangeOp;
  author: string;
  content_hash: string | null;
  rename_from: string | null;
  metadata: Record<string, unknown>;
}
interface RollbackOutcome {
  prior_change_id: number;
  path: string;
  new_hash: string;
}

const vaultHomeOverviewEl = document.getElementById("vault-home-overview")!;
const vaultHomeDetailEl = document.getElementById("vault-home-detail")!;
const vaultHomeDetailTitleEl = document.getElementById("vault-home-detail-title")!;
const vaultHomeDetailCountEl = document.getElementById("vault-home-detail-count")!;
const vaultHomeDetailListEl = document.getElementById("vault-home-detail-list")!;
const vaultHomeDetailFiltersEl = document.getElementById("vault-home-detail-filters")!;
const vaultHomeActivitySectionEl = document.getElementById("vault-home-activity")!;
const vaultHomeActivityHeaderEl = document.getElementById("vault-home-activity-header")!;
const vaultHomeActivityListEl = document.getElementById("vault-home-activity-list")!;

type DetailView = null | { kind: "recent-activity" };
let activeDetailView: DetailView = null;

// Persisted per session, not per-vault — spec says detail-view filter
// state persists per-vault but a session lifetime is fine for v1; the
// settings key isn't yet plumbed and the widget itself is fresh.
const activeAuthorFilters: Set<string> = new Set();
let allFiltersOnce = false;

// After a Restore, the row that was the *current* state for the path
// immediately before the action gets a soft highlight + "← previous
// state" caption so the user can one-click their way back. The behavior
// is the same Restore button as anywhere else — no separate primitive.
// Cleared on next refresh so it doesn't haunt subsequent visits.
let recentlyRestoredFromId: number | null = null;

function showHomeOverview(): void {
  activeDetailView = null;
  vaultHomeOverviewEl.hidden = false;
  vaultHomeDetailEl.hidden = true;
}

function showHomeDetail(kind: "recent-activity"): void {
  activeDetailView = { kind };
  vaultHomeOverviewEl.hidden = true;
  vaultHomeDetailEl.hidden = false;
  if (kind === "recent-activity") {
    vaultHomeDetailTitleEl.textContent = "Recent activity";
    void refreshActivityDetail();
  }
}

function opLabel(op: ChangeOp): string {
  return op;
}

function authorClass(author: string): string {
  // "user" → "user"; "agent:claude-code" → "agent"; etc. The class prefix
  // is the load-bearing distinguishing feature per changes.md.
  const colon = author.indexOf(":");
  return colon === -1 ? author : author.slice(0, colon);
}

function authorPillIcon(cls: string): string {
  // status: recent-activity-human-icon, recent-activity-agent-icon
  if (cls === "user") {
    return `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><circle cx="8" cy="5.5" r="2.4"/><path d="M3.5 13.5c0-2.4 2-4 4.5-4s4.5 1.6 4.5 4"/></svg>`;
  }
  if (cls === "agent") {
    return `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><rect x="3" y="6" width="10" height="7" rx="1.5"/><line x1="8" y1="3.5" x2="8" y2="6"/><circle cx="8" cy="3" r="0.6" fill="currentColor"/><circle cx="6" cy="9.2" r="0.7" fill="currentColor"/><circle cx="10" cy="9.2" r="0.7" fill="currentColor"/><line x1="6" y1="11.5" x2="10" y2="11.5"/></svg>`;
  }
  // Future: sync, import. Placeholder dot.
  return `<svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true"><circle cx="8" cy="8" r="3" fill="currentColor"/></svg>`;
}

async function refreshActivityWidget(): Promise<void> {
  if (!vaultIsOpen) return;
  if (!isVaultHomeVisible()) return;
  let count = 0;
  try {
    count = await invoke<number>("changes_count");
  } catch (err) {
    console.error("changes_count failed:", err);
  }
  if (count <= 0) {
    vaultHomeActivitySectionEl.hidden = true;
    return;
  }
  vaultHomeActivitySectionEl.hidden = false;
  vaultHomeActivityHeaderEl.textContent = `Recent activity (${count})`;
  // Tile: top 5 rows. Click anywhere → detail view.
  let rows: ChangeRow[] = [];
  try {
    rows = await invoke<ChangeRow[]>("recent_changes", { limit: 5 });
  } catch (err) {
    console.error("recent_changes failed:", err);
  }
  vaultHomeActivityListEl.replaceChildren(
    ...rows.map((r) => buildActivityPreviewRow(r)),
  );
  vaultHomeActivityHeaderEl.style.cursor = "pointer";
  vaultHomeActivityHeaderEl.onclick = () => showHomeDetail("recent-activity");
}

function buildActivityPreviewRow(r: ChangeRow): HTMLElement {
  const li = document.createElement("li");
  const op = document.createElement("span");
  op.className = "activity-op";
  op.textContent = opLabel(r.op);
  const name = document.createElement("span");
  name.className = "name";
  name.textContent = r.path.split("/").pop() ?? r.path;
  const rel = document.createElement("span");
  rel.className = "rel";
  rel.textContent = r.path.includes("/")
    ? r.path.slice(0, r.path.lastIndexOf("/"))
    : "";
  // Right-pinned cluster mirrors the detail row: [when] [author-icon]
  // [#id]. The `.rel` element fills the space between, pushing the
  // cluster to the right edge.
  const right = document.createElement("span");
  right.className = "row-right";
  const when = document.createElement("span");
  when.className = "when";
  when.textContent = relativeTime(Math.floor(r.timestamp_ms / 1000));
  when.title = new Date(r.timestamp_ms).toLocaleString();
  const cls = authorClass(r.author);
  const author = document.createElement("span");
  author.className = "activity-author";
  author.innerHTML = authorPillIcon(cls);
  author.title = r.author;
  const idEl = document.createElement("span");
  idEl.className = "activity-id";
  idEl.textContent = `#${r.id}`;
  idEl.title = `Snapshot id ${r.id}`;
  right.append(when, author, idEl);

  // Order: [op] [name] [badge?] [rel-grows] [right-cluster]. Badge sits
  // before .rel so it rides with the name on the left, matching the
  // detail-view layout; .rel's flex:1 then absorbs the slack and pushes
  // the right cluster to the row edge.
  li.append(op, name);
  const meta = r.metadata as Record<string, unknown>;
  const src = (meta?.["restored_from"] ?? meta?.["rolled_back_from"]) as
    | number
    | undefined;
  if (src !== undefined) {
    const badge = document.createElement("span");
    badge.className = "rollback-badge";
    badge.textContent = `↩ #${src}`;
    badge.title = `This save was a Restore of snapshot #${src}`;
    li.appendChild(badge);
  }
  li.append(rel, right);
  li.addEventListener("click", () => showHomeDetail("recent-activity"));
  return li;
}

let activityRows: ChangeRow[] = [];
async function refreshActivityDetail(): Promise<void> {
  try {
    activityRows = await invoke<ChangeRow[]>("recent_changes", { limit: 200 });
  } catch (err) {
    console.error("recent_changes failed:", err);
    activityRows = [];
  }
  renderActivityDetail();
}

function renderActivityDetail(): void {
  // Build the set of present author classes from the loaded rows.
  const presentClasses = new Set<string>();
  for (const r of activityRows) presentClasses.add(authorClass(r.author));

  // Canonical classes that always get a pill, even when no rows of that
  // class exist in the visible window. Predictable affordances beat
  // surprise pills appearing as agents start writing — users can reason
  // about "where would the agent filter be" before it ever has rows.
  // Other classes (sync, import) appear dynamically once they have rows.
  const ALWAYS_SHOW: readonly string[] = ["user", "agent"];
  const allClasses = new Set<string>([...ALWAYS_SHOW, ...presentClasses]);

  // First render: seed the active-filters set with every visible class so
  // the default is "all on." Subsequent renders preserve user toggle
  // state — including the all-off state.
  if (!allFiltersOnce) {
    activeAuthorFilters.clear();
    for (const c of allClasses) activeAuthorFilters.add(c);
    allFiltersOnce = true;
  }

  // status: vault-home-recent-activity-author-filter
  vaultHomeDetailFiltersEl.replaceChildren();
  const sortedClasses = [...allClasses].sort();
  for (const cls of sortedClasses) {
    const pill = document.createElement("button");
    pill.className = "filter-pill toolbar-btn";
    pill.type = "button";
    if (activeAuthorFilters.has(cls)) pill.classList.add("active");
    const hasRows = presentClasses.has(cls);
    if (!hasRows) pill.classList.add("empty");
    pill.innerHTML = authorPillIcon(cls);
    const lbl = document.createElement("span");
    lbl.textContent = cls.toUpperCase();
    pill.appendChild(lbl);
    pill.title = hasRows
      ? `Show ${cls} activity`
      : `No ${cls} activity in the recent window yet`;
    pill.addEventListener("click", () => {
      if (activeAuthorFilters.has(cls)) {
        activeAuthorFilters.delete(cls);
      } else {
        activeAuthorFilters.add(cls);
      }
      renderActivityDetail();
    });
    vaultHomeDetailFiltersEl.appendChild(pill);
  }

  const visible = activityRows.filter((r) =>
    activeAuthorFilters.has(authorClass(r.author)),
  );
  vaultHomeDetailCountEl.textContent = `${visible.length} of ${activityRows.length}`;

  // Pre-compute per-path latest so each row build doesn't rescan.
  latestPerPath = buildLatestPerPath(activityRows);

  vaultHomeDetailListEl.replaceChildren(
    ...visible.map((r) => buildActivityDetailRow(r)),
  );
}

// Compute "is this row the current state on disk for its path?" The most
// recent (highest id) row per path is the current state. Pre-computed once
// per render so per-row builders don't each scan `activityRows`.
function buildLatestPerPath(rows: ChangeRow[]): Map<string, number> {
  const out = new Map<string, number>();
  for (const r of rows) {
    const cur = out.get(r.path);
    if (cur === undefined || r.id > cur) out.set(r.path, r.id);
  }
  return out;
}

let latestPerPath: Map<string, number> = new Map();

function buildActivityDetailRow(r: ChangeRow): HTMLElement {
  const li = document.createElement("li");
  const isRestoreRow =
    r.metadata && typeof r.metadata === "object" &&
    ("restored_from" in r.metadata || "rolled_back_from" in r.metadata);
  const isCurrent = latestPerPath.get(r.path) === r.id;
  // status: vault-home-recent-activity-unrollback
  // After a Restore, the row that *was* the current state immediately
  // before the action gets a soft highlight + caption so the user can
  // one-click their way back. The behavior is the same Restore button as
  // anywhere else — no separate primitive — so this is purely a hint.
  if (recentlyRestoredFromId === r.id) {
    li.classList.add("recently-rolled-back");
  }

  // Click anywhere on the row → open the snapshot read-only in the editor.
  // The deleted-row case has no content blob, so click is a no-op there
  // (we still show the row so the history reads honestly).
  const canPreview = r.op !== "deleted";
  if (canPreview) {
    li.classList.add("clickable");
    li.style.cursor = "pointer";
    li.addEventListener("click", (e) => {
      // Don't hijack clicks on the action buttons.
      if ((e.target as HTMLElement).closest("button")) return;
      void openSnapshotPreview(r);
    });
  }

  const line = document.createElement("div");
  line.className = "row-line";

  const op = document.createElement("span");
  op.className = "activity-op";
  op.textContent = opLabel(r.op);

  const name = document.createElement("span");
  name.className = "name";
  name.textContent = r.path;
  if (r.rename_from) name.title = `renamed from ${r.rename_from}`;

  // Right-pinned cluster: [when] [author-icon] [#id]. Wrapped in a single
  // span with margin-left:auto so the layout stays stable regardless of
  // how long the path is.
  const right = document.createElement("span");
  right.className = "row-right";

  const when = document.createElement("span");
  when.className = "when";
  when.textContent = relativeTime(Math.floor(r.timestamp_ms / 1000));
  when.title = new Date(r.timestamp_ms).toLocaleString();

  const cls = authorClass(r.author);
  const author = document.createElement("span");
  author.className = "activity-author";
  author.innerHTML = authorPillIcon(cls);
  author.title = r.author; // full author string on hover (e.g. agent:claude-code)

  const idEl = document.createElement("span");
  idEl.className = "activity-id";
  idEl.textContent = `#${r.id}`;
  idEl.title = `Snapshot id ${r.id}`;

  // Badges sit between the name and the right-pinned cluster so they
  // ride with the name's left-aligned content rather than displacing the
  // right meta.
  line.append(op, name);
  if (isRestoreRow) {
    const meta = r.metadata as Record<string, unknown>;
    const src = (meta["restored_from"] ?? meta["rolled_back_from"]) as
      | number
      | undefined;
    const badge = document.createElement("span");
    badge.className = "rollback-badge";
    badge.textContent =
      src !== undefined ? `↩ restored from #${src}` : "↩ restored";
    badge.title =
      src !== undefined
        ? `This save wrote the content of snapshot #${src} back to disk`
        : "This save was a Restore";
    line.appendChild(badge);
  }
  if (isCurrent) {
    const cur = document.createElement("span");
    cur.className = "rollback-badge";
    cur.textContent = "current";
    cur.title = "This is the file's current state on disk";
    line.appendChild(cur);
  }
  right.append(when, author, idEl);
  line.append(right);
  li.appendChild(line);

  // Actions row. One primary button: Restore — writes THIS row's content
  // back to disk. Restoring the current state is a no-op (writes the same
  // bytes back and logs a new row), so we hide the button there.
  if (canPreview && !isCurrent) {
    const actions = document.createElement("div");
    actions.className = "row-actions";

    const restoreBtn = document.createElement("button");
    restoreBtn.className = "row-action";
    restoreBtn.textContent = "Restore this version";
    restoreBtn.title =
      "Write this snapshot's contents back to the file. Append-only — the restore is itself logged as a new modified event.";
    restoreBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      void doRestoreSnapshot(r);
    });
    actions.appendChild(restoreBtn);

    if (recentlyRestoredFromId === r.id) {
      const prompt = document.createElement("span");
      prompt.className = "un-rollback-prompt";
      prompt.textContent = "← previous state — click Restore to undo";
      actions.appendChild(prompt);
    }

    li.appendChild(actions);
  }
  return li;
}

async function doRestoreSnapshot(row: ChangeRow): Promise<void> {
  if (
    !confirm(
      `Restore ${row.path} to the version saved at ${new Date(
        row.timestamp_ms,
      ).toLocaleString()}?\n\nThe current state stays in the log; this Restore is itself a new logged event.`,
    )
  ) {
    return;
  }
  // Capture the row that *was* the current state for this path, before the
  // restore writes a new one. After refresh, that row gets the "previous
  // state" highlight so the user can one-click back.
  const wasCurrentId = latestPerPath.get(row.path) ?? null;
  try {
    await invoke<RollbackOutcome>("restore_snapshot", { changeId: row.id });
    recentlyRestoredFromId = wasCurrentId;
    await refreshActivityDetail();
  } catch (err) {
    alert(`restore failed: ${formatError(err)}`);
  }
}

// Open `row` as a read-only preview in the editor. Reuses the trash-preview
// machinery — same readOnlyCompartment, dirty-switch guard, banner pattern,
// just different banner element + content. Restore from the banner writes
// the snapshot back via the same path as per-row Restore.
async function openSnapshotPreview(row: ChangeRow): Promise<void> {
  if (buffer && isDirty()) {
    const choice = await confirm3(
      `${buffer.path} has unsaved changes.`,
      "Save & switch",
      "Discard & switch",
      "Cancel",
    );
    if (choice === "cancel") return;
    if (choice === "a") {
      const ok = await save();
      if (!ok) return;
    }
  }
  let contents: string | null = null;
  try {
    contents = await invoke<string | null>("change_content", {
      changeId: row.id,
    });
  } catch (err) {
    alert(`snapshot preview failed: ${formatError(err)}`);
    return;
  }
  if (contents === null) {
    alert(
      "This change has no recorded content (delete events carry no body — preview the prior version to see what was deleted).",
    );
    return;
  }
  buffer = null;
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: contents },
    effects: [
      language.reconfigure(languageExtensionForPath(row.path)),
      livePreviewCompartment.reconfigure(livePreviewExtensionForPath(row.path)),
    ],
  });
  setReadOnly(true, "snapshot");
  if (isVaultHomeVisible()) setVaultHomeVisible(false);
  buffer = {
    path: row.path,
    loadedText: view.state.doc.toString(),
    loadedHash: row.content_hash ?? "",
    snapshotPreview: true,
    snapshotChangeId: row.id,
  };
  // Banner copy: when, who, what, id.
  const when = new Date(row.timestamp_ms).toLocaleString();
  snapshotBannerTextEl.replaceChildren();
  const main = document.createElement("span");
  main.textContent = `Snapshot of ${row.path} · ${when} · ${row.author} · ${row.op}`;
  const idSpan = document.createElement("span");
  idSpan.className = "activity-id";
  idSpan.style.marginLeft = "8px";
  idSpan.textContent = `#${row.id}`;
  idSpan.title = `Snapshot id ${row.id}`;
  snapshotBannerTextEl.append(main, idSpan);
  updateStatus();
  refreshChunkBoundaries();
}

function exitSnapshotPreview(): void {
  if (!buffer?.snapshotPreview) return;
  buffer = null;
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: "" },
  });
  setReadOnly(false, null);
  // Return to the activity detail view if it's where the user came from;
  // otherwise fall back to the home overview.
  setVaultHomeVisible(true);
  if (activeDetailView?.kind !== "recent-activity") {
    showHomeDetail("recent-activity");
  }
  updateStatus();
}

snapshotBannerCloseBtn.addEventListener("click", () => exitSnapshotPreview());
snapshotBannerRestoreBtn.addEventListener("click", async () => {
  if (!buffer?.snapshotPreview || buffer.snapshotChangeId === undefined) return;
  const row = activityRows.find((r) => r.id === buffer?.snapshotChangeId);
  if (!row) {
    alert("Snapshot row no longer in view — refresh and try again.");
    return;
  }
  await doRestoreSnapshot(row);
  // After a successful restore, return to the detail view so the user
  // sees the new "current" row + the highlighted "previous state" hint.
  exitSnapshotPreview();
});

// Live refresh on every changelog append. Light debounce so a save burst
// → one repaint of the widget. Spec: "few hundred ms" debounce.
let activityRefreshTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleActivityRefresh(delay = 300): void {
  if (!isVaultHomeVisible()) return;
  if (activityRefreshTimer !== null) clearTimeout(activityRefreshTimer);
  activityRefreshTimer = setTimeout(() => {
    activityRefreshTimer = null;
    void refreshActivityWidget();
    if (activeDetailView?.kind === "recent-activity") {
      void refreshActivityDetail();
    }
  }, delay);
}

// status: mcp-ui-refresh-on-agent-write
// Agent writes (per `mcp.md`) suppress the watcher around their fs writes for
// the same correctness reasons move/delete do, so `hiker:file-changed` never
// fires for them. Ride the changes broadcast instead: any row whose author
// starts with "agent:" applies the same tree-refresh + active-buffer reload
// shape the watcher handler would have. Non-agent rows (user saves,
// rollbacks) keep flowing through the watcher path so we don't double-refresh.
void listen<ChangeRow>("hiker:changes-appended", (event) => {
  scheduleActivityRefresh();
  const row = event.payload;
  if (!row.author.startsWith("agent:")) return;
  void handleAgentChange(row);
});

async function handleAgentChange(row: ChangeRow): Promise<void> {
  // Tree-shape changes mirror the watcher handler's branches.
  if (row.op === "created" || row.op === "deleted" || row.op === "renamed") {
    scheduleTreeRefreshFromWatcher();
    scheduleVaultHomeModifiedRefresh();
  } else if (
    row.op === "modified"
    && (treeSortOrder === "mtime-newest" || treeSortOrder === "mtime-oldest")
  ) {
    // Same rationale as the watcher path: mtime-based sorts depend on per-row
    // mtime and a save reorders rows.
    scheduleTreeRefreshFromWatcher();
  }
  if (row.op === "modified") {
    scheduleVaultHomeModifiedRefresh();
  }

  // Active-buffer reload. Skip read-only previews (snapshot / trash) for the
  // same reason the watcher handler does — they're historic views the agent
  // shouldn't be allowed to clobber.
  if (!buffer || isReadOnlyBuffer(buffer)) return;

  if (row.op === "modified" && row.path === buffer.path) {
    if (isDirty()) {
      // The user is mid-edit on a file the agent just rewrote. Don't silently
      // overwrite their buffer — surface a toast and let the next save's
      // drift check resolve it. Same posture as `handleWatcherConflictDirty`
      // but synchronous: agent writes are server-driven so a modal prompt
      // would interrupt the user without warning.
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
      buffer = null;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
      updateStatus();
      showToast(`${row.path} was removed by an agent`);
    }
    return;
  }

  if (row.op === "renamed" && row.rename_from === buffer.path) {
    buffer.path = row.path;
    updateStatus();
  }
}

vaultHomeNewNoteBtn.addEventListener("click", async () => {
  try {
    const created = await invoke<string>("create_note", { folder: "" });
    await openFile(created);
  } catch (err) {
    console.error("vault-home new note failed:", err);
    alert(`new note failed: ${formatError(err)}`);
  }
});

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

viewMenuBtn.addEventListener("click", (e) => {
  e.preventDefault();
  e.stopPropagation();
  const rect = viewMenuBtn.getBoundingClientRect();
  openContextMenu(rect.left, rect.bottom + 2, buildViewMenuItems());
});

// ---------- discovery panel (search + related) ----------
//
// status: search-discovery-panel
// status: search-mode-toggles
//
// Mode toggles + section collapse state. Defaults match `core::config`'s
// `SearchConfig::default()`; the values are overwritten from `get_settings`
// on vault open and persisted via `settings-write-back` on every flip.

let searchModeSemantic = true;
let searchModeLexical = true;

function applySearchInputDisabledState(): void {
  // status: search-modes-both-off-disabled
  // Both toggles off → input is disabled with the hint text. Matches the
  // spec exactly: explicit failure beats silent fallback. The placeholder
  // is the visible affordance once disabled.
  const bothOff = !searchModeSemantic && !searchModeLexical;
  searchInputEl.disabled = bothOff;
  searchInputEl.placeholder = bothOff
    ? "Enable Semantic or Lexical to search"
    : "Search vault…";
}

function syncSearchModeButtons(): void {
  toggleModeSemanticBtn.classList.toggle("active", searchModeSemantic);
  toggleModeLexicalBtn.classList.toggle("active", searchModeLexical);
  applySearchInputDisabledState();
}

function setSearchModeSemantic(on: boolean, persist: boolean): void {
  searchModeSemantic = on;
  syncSearchModeButtons();
  if (persist) {
    void persistSetting("vault", "search.modes.semantic", on);
    maybeRerunSearchAfterModeChange();
  }
}

function setSearchModeLexical(on: boolean, persist: boolean): void {
  searchModeLexical = on;
  syncSearchModeButtons();
  if (persist) {
    void persistSetting("vault", "search.modes.lexical", on);
    maybeRerunSearchAfterModeChange();
  }
}

toggleModeSemanticBtn.addEventListener("click", () => {
  setSearchModeSemantic(!searchModeSemantic, true);
});
toggleModeLexicalBtn.addEventListener("click", () => {
  setSearchModeLexical(!searchModeLexical, true);
});

// status: search-empty-collapses-results
// Empty query hides the search-results section; non-empty shows it.
function applySearchSectionVisibility(): void {
  const hasQuery = searchInputEl.value.trim().length > 0;
  searchSectionEl.hidden = !hasQuery;
}

// status: search-typeahead-debounce
// 250ms debounce + monotonically-increasing epoch. Stale responses (whose
// epoch is below the current one) are dropped on the frontend before
// render. Mirrors the cancel-on-file-switch pattern already used by
// `refreshRelated`. Empty query short-circuits without scheduling.
const SEARCH_DEBOUNCE_MS = 250;
let searchEpoch = 0;
let searchDebounceTimer: number | null = null;

function applySearchClearButtonVisibility(): void {
  searchClearBtn.hidden = searchInputEl.value.length === 0;
}

// status: search-keybind-ctrl-space
// Focuses the search input. Opens the discovery panel if collapsed
// (matching the spec's "Opens the discovery panel if collapsed"). Selects
// existing input contents so a quick re-search retypes naturally.
function focusSearchInput(): void {
  // If the panel was collapsed, expand it first. The expand toggles a
  // CSS class with a `transition: visibility 0.1s` rule on `#discovery`;
  // calling `.focus()` on the input *during* that transition is a no-op
  // because the element is still computed-visibility:hidden. Defer the
  // focus to the next animation frame so the new style is settled.
  const wasCollapsed = appEl.classList.contains("related-collapsed");
  if (wasCollapsed) {
    appEl.classList.remove("related-collapsed");
    void persistSetting("vault", "vault.related_open", true);
    syncToggleButtons();
  }
  const doFocus = () => {
    searchInputEl.focus();
    searchInputEl.select();
  };
  if (wasCollapsed) {
    requestAnimationFrame(doFocus);
  } else {
    doFocus();
  }
}

// status: search-keyboard-nav
//
// ↑/↓ move within the focused result list, with vertical wraparound at
// the boundary between sections (↓ at the bottom of Search jumps to the
// top of Related; ↑ at the top of Related jumps to the bottom of Search).
// Stops at panel boundaries — no wrap from Related's bottom or Search's
// top.
// Enter opens the focused row.
// Tab is handled by the browser via roving tabindex: each section keeps
// exactly one row with tabindex=0 (the first by default; the most recent
// arrow target after that), so Tab from the input lands on the first
// search row, then the first related row, then leaves the panel.
// Esc in the input clears the query (which collapses the search
// section via the existing empty-query rule) and blurs.
// Esc in a result list returns focus to the input.

function discoveryRows(list: HTMLElement): HTMLElement[] {
  return Array.from(list.querySelectorAll<HTMLElement>(".related-item"));
}

/// Set tabindex=0 on the row at `idx` and tabindex=-1 on every other row
/// in the same list. Idempotent. Only one row in a list is Tab-reachable
/// at a time (roving tabindex pattern).
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

// Handle ↑ / ↓ / Enter / Esc on the result lists.
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
      } else if (list === searchListEl) {
        // Bottom of search → top of related.
        if (!focusRow(relatedListEl, 0)) {
          // No related rows; stay put.
        }
      }
      // Bottom of related: stop. No wrap to top of search.
      break;
    }
    case "ArrowUp": {
      e.preventDefault();
      if (idx > 0) {
        focusRow(list, idx - 1);
      } else if (list === relatedListEl) {
        // Top of related → bottom of search.
        const searchRows = discoveryRows(searchListEl);
        if (searchRows.length > 0) {
          focusRow(searchListEl, searchRows.length - 1);
        }
      }
      // Top of search: stop. No wrap to bottom of related.
      break;
    }
    case "Enter": {
      e.preventDefault();
      // The row's click handler is the open path; trigger it.
      target.click();
      break;
    }
    case "Escape": {
      e.preventDefault();
      searchInputEl.focus();
      break;
    }
  }
}

searchListEl.addEventListener("keydown", onResultListKeydown);
relatedListEl.addEventListener("keydown", onResultListKeydown);

// Esc in the input clears + blurs (clearing collapses the search section
// via `applySearchSectionVisibility`).
searchInputEl.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    if (searchInputEl.value.length > 0) {
      searchInputEl.value = "";
      onSearchInput();
    } else {
      searchInputEl.blur();
    }
  } else if (e.key === "ArrowDown") {
    // Down from the input should jump into the first available result
    // section (search if visible, else related). Lets Enter-from-keyboard
    // flows skip Tab when the user just typed and wants to pick a result.
    const searchRows = discoveryRows(searchListEl);
    if (searchRows.length > 0) {
      e.preventDefault();
      focusRow(searchListEl, 0);
      return;
    }
    const relatedRows = discoveryRows(relatedListEl);
    if (relatedRows.length > 0) {
      e.preventDefault();
      focusRow(relatedListEl, 0);
    }
  }
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
      focusSearchInput();
    }
  },
  { capture: true },
);

searchClearBtn.addEventListener("click", () => {
  searchInputEl.value = "";
  onSearchInput();
  searchInputEl.focus();
});

function onSearchInput(): void {
  applySearchClearButtonVisibility();
  applySearchSectionVisibility();
  if (searchDebounceTimer !== null) {
    window.clearTimeout(searchDebounceTimer);
    searchDebounceTimer = null;
  }
  const raw = searchInputEl.value;
  const trimmed = raw.trim();
  if (trimmed.length === 0) {
    // Bump epoch so any in-flight call drops its results, then clear UI.
    searchEpoch += 1;
    searchSpinnerEl.hidden = true;
    searchListEl.innerHTML = "";
    searchCountEl.textContent = "";
    return;
  }
  // Both modes off: input is disabled — nothing to do — but the listener
  // still fires for programmatic value changes. Be defensive.
  if (!searchModeSemantic && !searchModeLexical) {
    return;
  }
  searchSpinnerEl.hidden = false;
  searchDebounceTimer = window.setTimeout(() => {
    searchDebounceTimer = null;
    const epoch = ++searchEpoch;
    void runSearch(trimmed, epoch);
  }, SEARCH_DEBOUNCE_MS);
}

searchInputEl.addEventListener("input", onSearchInput);

async function runSearch(query: string, epoch: number): Promise<void> {
  try {
    const resp = await invoke<SearchResponse>("search_vault", {
      query,
      modes: { semantic: searchModeSemantic, lexical: searchModeLexical },
      epoch,
    });
    // status: search-typeahead-debounce — drop stale results.
    if (resp.epoch !== searchEpoch) return;
    // Pick which bucket to render. Both modes on → fused; one mode on →
    // that engine's native ranking (already in the bucket).
    const hits = pickResultBucket(resp);
    searchSpinnerEl.hidden = true;
    renderSearchResults(hits);
  } catch (err) {
    if (epoch !== searchEpoch) return;
    console.error("search_vault failed:", err);
    searchSpinnerEl.hidden = true;
    searchListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
    searchCountEl.textContent = "";
  }
}

function pickResultBucket(resp: SearchResponse): SearchNoteHit[] {
  if (searchModeSemantic && searchModeLexical) return resp.fused;
  if (searchModeLexical) return resp.lexical_hits;
  if (searchModeSemantic) return resp.semantic_hits;
  return [];
}

// status: search-result-row, search-result-grouped-by-note (UI side),
// search-section-counts
function renderSearchResults(hits: SearchNoteHit[]): void {
  searchListEl.innerHTML = "";
  searchCountEl.textContent = hits.length > 0 ? `(${hits.length})` : "";
  if (hits.length === 0) {
    const empty = document.createElement("div");
    empty.className = "related-empty";
    empty.textContent = "No matches.";
    searchListEl.appendChild(empty);
    return;
  }
  for (const hit of hits) {
    const item = document.createElement("div");
    item.className = "related-item search-item";
    // Roving tabindex set after the loop; default to -1 so Tab won't
    // hit every row.
    item.tabIndex = -1;
    item.setAttribute("role", "option");
    item.addEventListener("click", () => void openSearchHit(hit));

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

    searchListEl.appendChild(item);
  }
  // status: search-keyboard-nav — first row is Tab-reachable; others -1.
  setRovingTabIndex(searchListEl, 0);
}

// Render an FTS5 snippet that may contain literal `<mark>` / `</mark>`
// substrings as text + styled spans. Never `innerHTML` — the rest of the
// app avoids raw HTML rendering and FTS5's snippet output is the only
// source of these markers, so a structural parse is enough. Any other
// `<` is treated as plain text.
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
      // Unterminated <mark> — treat the rest as plain text rather than
      // dropping anything. Defensive against malformed FTS5 output.
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

// status: search-result-click-opens-chunk
// Open the note, then look up its chunk bounds and scroll to the matched
// chunk's byte range. Conversion is byte → char (UTF-8 → UTF-16) because
// `chunks_for` returns byte offsets while CM6 indexes characters.
async function openSearchHit(hit: SearchNoteHit): Promise<void> {
  await openFile(hit.path);
  // openFile may abort on dirty-buffer cancel; bail if we're not actually
  // looking at the requested file now.
  if (buffer?.path !== hit.path) return;
  try {
    const bounds = await invoke<ChunkBounds[]>("chunks_for", { rel: hit.path });
    const target = bounds.find((b) => b.chunk_index === hit.chunk_index);
    if (!target) return;
    const docText = view.state.doc.toString();
    const charOffset = byteOffsetToCharOffset(docText, target.byte_start);
    const safe = Math.min(charOffset, view.state.doc.length);
    view.dispatch({
      selection: { anchor: safe },
      effects: EditorView.scrollIntoView(safe, { y: "start" }),
    });
    view.focus();
  } catch (err) {
    console.error("scroll-to-chunk failed:", err);
  }
}

function byteOffsetToCharOffset(text: string, byteOffset: number): number {
  if (byteOffset <= 0) return 0;
  const enc = new TextEncoder();
  const bytes = enc.encode(text);
  if (byteOffset >= bytes.length) return text.length;
  const dec = new TextDecoder();
  return dec.decode(bytes.subarray(0, byteOffset)).length;
}

// Re-run search when a vault is opened: existing query (if any) loses its
// results when the panel rebinds; clear UI so we don't show prior-vault
// results against a new vault.
function clearSearchPanel(): void {
  searchInputEl.value = "";
  searchEpoch += 1;
  searchSpinnerEl.hidden = true;
  searchListEl.innerHTML = "";
  searchCountEl.textContent = "";
  applySearchClearButtonVisibility();
  applySearchSectionVisibility();
}

// Re-run when the user flips a mode toggle while a query is active. Without
// this, switching from "both" to "lexical only" (or back) would leave stale
// results showing under a now-different mode label until the next keystroke.
function maybeRerunSearchAfterModeChange(): void {
  if (searchInputEl.disabled) return;
  if (searchInputEl.value.trim().length === 0) return;
  const epoch = ++searchEpoch;
  searchSpinnerEl.hidden = false;
  void runSearch(searchInputEl.value.trim(), epoch);
}

// status: search-section-collapsible
// Per-section collapsed/expanded state, persisted per-vault. Clicking the
// header (anywhere on it, not just the chevron — bigger hit target) toggles
// the corresponding `[hidden]` on the section's body.
function applySectionCollapsed(
  section: HTMLElement,
  body: HTMLElement,
  expanded: boolean,
): void {
  section.classList.toggle("collapsed", !expanded);
  body.hidden = !expanded;
}

let searchSectionExpanded = true;
let relatedSectionExpanded = true;

function setSearchSectionExpanded(expanded: boolean, persist: boolean): void {
  searchSectionExpanded = expanded;
  applySectionCollapsed(searchSectionEl, searchListEl, expanded);
  if (persist) {
    void persistSetting("vault", "search.sections.results_expanded", expanded);
  }
}

function setRelatedSectionExpanded(expanded: boolean, persist: boolean): void {
  relatedSectionExpanded = expanded;
  applySectionCollapsed(relatedSectionEl, relatedListEl, expanded);
  if (persist) {
    void persistSetting("vault", "search.sections.related_expanded", expanded);
  }
}

searchSectionEl
  .querySelector(".discovery-section-header")!
  .addEventListener("click", () => {
    setSearchSectionExpanded(!searchSectionExpanded, true);
  });
relatedSectionEl
  .querySelector(".discovery-section-header")!
  .addEventListener("click", () => {
    setRelatedSectionExpanded(!relatedSectionExpanded, true);
  });

// ---------- related-notes panel ----------

let relatedRequestSeq = 0;
let relatedDebounce: number | null = null;

async function refreshRelated(rel: string | null): Promise<void> {
  const seq = ++relatedRequestSeq;
  if (!rel) {
    relatedListEl.innerHTML = "";
    relatedCountEl.textContent = "";
    return;
  }
  try {
    const hits = await invoke<RelatedHit[]>("related_notes", { rel, topK: 10 });
    if (seq !== relatedRequestSeq) return;
    renderRelated(hits);
  } catch (err) {
    if (seq !== relatedRequestSeq) return;
    console.error("related_notes failed:", err);
    relatedListEl.innerHTML = `<div class="related-empty">Error: ${String(err)}</div>`;
  }
}

function renderRelated(hits: RelatedHit[]): void {
  relatedListEl.innerHTML = "";
  // status: search-section-counts — header reflects live count.
  relatedCountEl.textContent = hits.length > 0 ? `(${hits.length})` : "";
  if (hits.length === 0) {
    const empty = document.createElement("div");
    empty.className = "related-empty";
    empty.textContent = "No related notes yet.";
    relatedListEl.appendChild(empty);
    return;
  }
  for (const hit of hits) {
    const item = document.createElement("div");
    item.className = "related-item";
    item.tabIndex = -1;
    item.setAttribute("role", "option");
    item.addEventListener("click", () => void openFile(hit.path));

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

    relatedListEl.appendChild(item);
  }
  // status: search-keyboard-nav
  setRovingTabIndex(relatedListEl, 0);
}

function scheduleRelatedRefresh(delayMs: number): void {
  if (relatedDebounce !== null) {
    window.clearTimeout(relatedDebounce);
  }
  relatedDebounce = window.setTimeout(() => {
    void refreshRelated(buffer?.path ?? null);
  }, delayMs);
}

let bufferPathInterval: number | null = null;
let indexStatusInterval: number | null = null;
let lastSeenBufferPath: string | null = null;

function startBackgroundIntervals(): void {
  if (bufferPathInterval !== null) window.clearInterval(bufferPathInterval);
  if (indexStatusInterval !== null) window.clearInterval(indexStatusInterval);
  bufferPathInterval = window.setInterval(() => {
    if (!vaultIsOpen) return;
    const cur = buffer?.path ?? null;
    if (cur !== lastSeenBufferPath) {
      lastSeenBufferPath = cur;
      scheduleRelatedRefresh(0);
    }
  }, 250);
  indexStatusInterval = window.setInterval(() => {
    if (!vaultIsOpen) return;
    void pollIndexStatus();
  }, 2000);
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
    const cached = indexStateCache.get(buffer.path);
    if (!cached) {
      if (!isIndexableExt(buffer.path)) {
        const s: IndexState = { kind: "unsupported" };
        indexStateCache.set(buffer.path, s);
      } else if (!inflightStateFetches.has(buffer.path)) {
        const path = buffer.path;
        inflightStateFetches.add(path);
        void fetchIndexState(path)
          .catch((err) => console.error("index_state_for failed:", path, err))
          .finally(() => {
            inflightStateFetches.delete(path);
            // Re-render once the fetch resolves so the label can switch.
            if (buffer && buffer.path === path) renderIndexStatus();
          });
      }
    }
    const state = indexStateCache.get(buffer.path);
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

async function pollIndexStatus(): Promise<void> {
  try {
    indexStatus = await invoke<IndexStatus>("index_status");
    renderIndexStatus();
  } catch {
    // No vault open — leave label empty.
  }
}

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
          scheduleRelatedRefresh(100);
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
        indexStateCache.delete(ev.path);
      } else if (ev.kind === "renamed") {
        const prior = indexStateCache.get(ev.from);
        indexStateCache.delete(ev.from);
        if (prior) updateIndexStateForPath(ev.to, prior);
      } else if (ev.kind === "error" && ev.path) {
        // Refetch on next render — error state isn't itself a marker.
        indexStateCache.delete(ev.path);
      }
      break;
    case "scan_complete":
      outstandingCount += ev.queued;
      break;
  }
  renderIndexStatus();
  void pollIndexStatus();
  // status: vault-home-stats-widget — counts shift on every terminal event;
  // debounced so a flurry of progress events fires one stats fetch.
  scheduleVaultHomeStatsRefresh();
});

function updateIndexStateForPath(path: string, state: IndexState): void {
  indexStateCache.set(path, state);
  document
    .querySelectorAll(`#tree li[data-path="${cssEscape(path)}"]`)
    .forEach((el) => applyIndexMarker(el as HTMLElement, state));
  if (buffer && !isReadOnlyBuffer(buffer) && buffer.path === path) {
    renderIndexStatus();
  }
}


// ---------- trash bin ----------
// status: tree-trash-bin

let trashItems: TrashListItem[] = [];

type ReadOnlyMode = "trash" | "snapshot" | null;

/// Set or clear the editor's read-only state. `mode` selects which banner
/// to show — only one banner is visible at a time. Pass `null` (or omit)
/// to leave the editor writable and hide both banners.
function setReadOnly(ro: boolean, mode: ReadOnlyMode = null): void {
  view.dispatch({
    effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(ro)),
  });
  trashBannerEl.hidden = !(ro && mode === "trash");
  snapshotBannerEl.hidden = !(ro && mode === "snapshot");
  editorPaneEl.classList.toggle("snapshot-preview", ro && mode === "snapshot");
}

function relativeTime(unixSecs: number): string {
  const now = Math.floor(Date.now() / 1000);
  const d = now - unixSecs;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  if (d < 86400 * 2) return "yesterday";
  if (d < 86400 * 7) return `${Math.floor(d / 86400)}d ago`;
  const date = new Date(unixSecs * 1000);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

async function refreshTrashBin(): Promise<void> {
  if (!vaultIsOpen) return;
  try {
    trashItems = await invoke<TrashListItem[]>("list_trash");
  } catch (err) {
    console.error("list_trash failed:", err);
    trashItems = [];
  }
  renderTrashBin();
}

// status: tree-trash-flat-by-deleted
function renderTrashBin(): void {
  const n = trashItems.length;
  trashLabelEl.textContent = n === 0 ? "Trash" : `Trash (${n})`;
  trashListEl.innerHTML = "";
  for (const item of trashItems) {
    const row = document.createElement("div");
    row.className = "trash-row";
    if (item.orphaned) row.classList.add("orphaned");
    row.dataset.trashedName = item.trashed_name;

    const main = document.createElement("div");
    main.className = "trash-row-main";
    const glyph = item.kind === "folder" ? "▸ " : "  ";
    const display = item.original_path
      ? (item.original_path.split("/").pop() ?? item.original_path)
      : item.trashed_name;
    let label = `${glyph}${display}`;
    if (item.kind === "folder") {
      const count =
        item.member_count === null ? "?" : String(item.member_count);
      label += ` (${count} note${item.member_count === 1 ? "" : "s"})`;
    }
    main.textContent = label;
    row.appendChild(main);

    const meta = document.createElement("div");
    meta.className = "trash-row-meta";
    const when = relativeTime(item.deleted_at);
    const orig = item.original_path ?? "(no original location recorded)";
    meta.textContent = `${when} · ${orig}`;
    row.appendChild(meta);

    // Click → preview (file rows only). Folders are no-op per default.
    if (item.kind === "file") {
      row.addEventListener("click", () => void openTrashPreview(item));
    }
    row.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      e.stopPropagation();
      openTrashRowMenu(e.clientX, e.clientY, item);
    });

    trashListEl.appendChild(row);
  }
}

trashHeaderEl.addEventListener("click", () => {
  trashBinEl.classList.toggle("collapsed");
  const expanded = !trashBinEl.classList.contains("collapsed");
  trashChevronEl.textContent = expanded ? "▾" : "▸";
  if (vaultIsOpen) {
    void persistSetting("vault", "vault.trash_expanded", expanded);
  }
});

// status: tree-trash-empty-action
trashHeaderEl.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  e.stopPropagation();
  const n = trashItems.length;
  openContextMenu(e.clientX, e.clientY, [
    {
      label: n === 0 ? "Empty trash" : `Empty trash (${n} entries)`,
      danger: true,
      disabled: n === 0,
      run: async () => {
        const ok = await confirmDanger(
          `Permanently delete ${n} trash ${n === 1 ? "entry" : "entries"}? This cannot be undone.`,
          "Empty trash",
        );
        if (!ok) return;
        try {
          await invoke("empty_trash");
        } catch (err) {
          console.error("empty_trash failed:", err);
          alert(`empty failed: ${formatError(err)}`);
        }
      },
    },
  ]);
});

// status: tree-trash-restore-action
function openTrashRowMenu(x: number, y: number, item: TrashListItem): void {
  const items: CtxMenuItem[] = [];
  if (item.id) {
    items.push({
      label: "Restore",
      run: async () => {
        try {
          const restored = await invoke<TrashEntry>("restore_trash_entry", { id: item.id });
          showToast(`Restored ${restored.original_path}`);
          await refreshTree();
        } catch (err) {
          console.error("restore_trash_entry failed:", err);
          alert(`restore failed: ${formatError(err)}`);
        }
      },
    });
  } else {
    // status: tree-trash-orphan-recovery
    items.push({
      label: "Restore (no original location recorded)",
      disabled: true,
    });
  }
  items.push({
    label: "Delete permanently",
    danger: true,
    run: async () => {
      const target = item.original_path ?? item.trashed_name;
      const ok = await confirmDanger(
        `Permanently delete ${target}? This cannot be undone.`,
        "Delete permanently",
      );
      if (!ok) return;
      try {
        await invoke("permanent_delete_trash_entry", { trashedName: item.trashed_name });
      } catch (err) {
        console.error("permanent_delete_trash_entry failed:", err);
        alert(`delete failed: ${formatError(err)}`);
      }
    },
  });
  openContextMenu(x, y, items);
}

// status: tree-trash-preview
async function openTrashPreview(item: TrashListItem): Promise<void> {
  // Same dirty-switch guard as openFile, since opening a preview replaces
  // the active buffer.
  if (buffer && isDirty()) {
    const choice = await confirm3(
      `${buffer.path} has unsaved changes.`,
      "Save & switch",
      "Discard & switch",
      "Cancel",
    );
    if (choice === "cancel") return;
    if (choice === "a") {
      const ok = await save();
      if (!ok) return;
    }
  }
  const trashRel = `.hiker/trash/${item.trashed_name}`;
  try {
    const file = await invoke<FileWithHash>("read_file_with_hash", { rel: trashRel });
    buffer = null;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: file.contents },
      effects: [
        language.reconfigure(
          languageExtensionForPath(item.original_path ?? item.trashed_name),
        ),
        livePreviewCompartment.reconfigure(
          livePreviewExtensionForPath(item.original_path ?? item.trashed_name),
        ),
      ],
    });
    setReadOnly(true, "trash");
    if (isVaultHomeVisible()) setVaultHomeVisible(false);
    buffer = {
      path: trashRel,
      loadedText: view.state.doc.toString(),
      loadedHash: file.hash,
      preview: true,
      displayPath: item.original_path ?? item.trashed_name,
    };
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
    const row = trashListEl.querySelector(
      `.trash-row[data-trashed-name="${cssEscape(item.trashed_name)}"]`,
    );
    row?.classList.add("active");
    updateStatus();
    refreshChunkBoundaries();
  } catch (err) {
    console.error("openTrashPreview failed:", err);
    alert(`preview failed: ${formatError(err)}`);
  }
}

// Listen for any trash-changing op and re-render. Also clear the preview
// buffer if the entry being previewed was emptied/restored under us.
void listen("hiker:watcher-overflow", () => {
  showToast("Filesystem watcher fell behind — rescanning…");
});

void listen("hiker:trash-changed", async () => {
  await refreshTrashBin();
  if (buffer?.preview) {
    const stillThere = trashItems.some(
      (i) => `.hiker/trash/${i.trashed_name}` === buffer?.path,
    );
    if (!stillThere) {
      buffer = null;
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: "" },
      });
      setReadOnly(false);
      updateStatus();
    }
  }
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
    scheduleVaultHomeModifiedRefresh();
  } else if (
    ev.kind === "modified"
    && (treeSortOrder === "mtime-newest" || treeSortOrder === "mtime-oldest")
  ) {
    // Tree *shape* doesn't change on Modified, but mtime-based sort orders
    // depend on per-entry mtime — a save reorders rows. Schedule a refresh
    // only when the chosen sort actually consumes mtime; under name sorts
    // we keep the existing no-op behavior.
    scheduleTreeRefreshFromWatcher();
  }
  if (ev.kind === "modified") {
    scheduleVaultHomeModifiedRefresh();
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
      buffer = null;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: "" } });
      updateStatus();
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
