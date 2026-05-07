import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
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
}

let buffer: Buffer | null = null;
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
  if (!rel || buffer?.preview) {
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
  if (!buffer || buffer.preview) return false;
  return view.state.doc.toString() !== buffer.loadedText;
}

function updateStatus(): void {
  const dirty = isDirty();
  const path = buffer?.preview
    ? (buffer.displayPath ?? buffer.path)
    : (buffer?.path ?? "");
  const titleSuffix = buffer?.preview ? " (in trash)" : "";
  document.title =
    (dirty ? "• " : "") + (path ? `Hiker — ${path}${titleSuffix}` : "Hiker");
  // status: status-bar-path-basename-tooltip
  let basename = path ? (path.split("/").pop() ?? path) : "";
  if (buffer?.preview) basename += " (in trash)";
  statusPathEl.textContent = basename;
  statusPathEl.title = buffer?.preview ? buffer.path : path;
  // status: status-bar-path-reveal — clickable when a real (non-trash) file
  // is open. Trash-preview paths live under `.hiker/trash/` and revealing
  // them would expose internal state, so the gesture is suppressed there.
  const revealable = !!buffer && !buffer.preview;
  statusPathEl.classList.toggle("clickable", revealable);
  statusPathEl.style.cursor = revealable ? "pointer" : "";
  saveBtn.disabled = !buffer || !dirty || buffer.preview === true;
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
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelectorAll(".trash-row.active").forEach((el) => el.classList.remove("active"));
    await revealInTree(rel);
    updateStatus();
    refreshChunkBoundaries();
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

// File extensions the indexer chunks. Keep in sync with
// `is_indexable_path` in core/src/indexer.rs — Unsupported derivation is
// duplicated client-side per index.md so we don't pay a Tauri round trip
// on every visible row.
const INDEXABLE_EXTS = new Set(["md", "markdown", "txt"]);
function isIndexableExt(rel: string): boolean {
  const dot = rel.lastIndexOf(".");
  if (dot <= rel.lastIndexOf("/")) return false;
  return INDEXABLE_EXTS.has(rel.slice(dot + 1).toLowerCase());
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
      if (buffer && !buffer.preview && buffer.path === path) {
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

async function openVault(): Promise<void> {
  let path: string | null;
  try {
    path = await invoke<string | null>("pick_vault");
  } catch (err) {
    handleOpenVaultError(err);
    return;
  }
  if (!path) return;
  await applyOpenedVault(path);
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
    treeSortOrder = sortOrderFromSettings(s.vault.tree.sort_by);
    appEl.classList.toggle("sidebar-collapsed", !s.vault.sidebar_open);
    appEl.classList.toggle("related-collapsed", !s.vault.related_open);
    trashBinEl.classList.toggle("collapsed", !s.vault.trash_expanded);
    trashChevronEl.textContent = s.vault.trash_expanded ? "▾" : "▸";
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
  startBackgroundIntervals();
  await refreshTree();
  await refreshTrashBin();
}

pickBtn.addEventListener("click", () => void openVault());

// status: settings-default-vault-autoopen
// Bootstrap: try the user-scope `vault.default` before falling back to
// the picker. The Tauri command returns null when no default is set or
// when the configured path no longer exists; either case leaves the user
// at the standard "click to open vault" surface.
async function bootstrapDefaultVault(): Promise<void> {
  let path: string | null;
  try {
    path = await invoke<string | null>("try_open_default_vault");
  } catch (err) {
    handleOpenVaultError(err);
    return;
  }
  if (!path) return;
  await applyOpenedVault(path);
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
    buffer && !buffer.preview ? buffer.path : null;
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
void win.onCloseRequested(async (event) => {
  if (!buffer || !isDirty()) return;
  event.preventDefault();
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
  buffer = null;
  await win.close();
});

updateStatus();

// status: status-bar-path-reveal
statusPathEl.addEventListener("click", async () => {
  if (!buffer || buffer.preview) return;
  try {
    await invoke("reveal_in_file_manager", { rel: buffer.path });
  } catch (err) {
    console.error("reveal_in_file_manager failed:", err);
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

// ---------- related-notes panel ----------

let relatedRequestSeq = 0;
let relatedDebounce: number | null = null;

async function refreshRelated(rel: string | null): Promise<void> {
  const seq = ++relatedRequestSeq;
  if (!rel) {
    relatedListEl.innerHTML = "";
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
  // back to the aggregate label otherwise (or while previewing trash).
  if (buffer && !buffer.preview) {
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
});

function updateIndexStateForPath(path: string, state: IndexState): void {
  indexStateCache.set(path, state);
  document
    .querySelectorAll(`#tree li[data-path="${cssEscape(path)}"]`)
    .forEach((el) => applyIndexMarker(el as HTMLElement, state));
  if (buffer && !buffer.preview && buffer.path === path) {
    renderIndexStatus();
  }
}


// ---------- trash bin ----------
// status: tree-trash-bin

let trashItems: TrashListItem[] = [];

function setReadOnly(ro: boolean): void {
  view.dispatch({
    effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(ro)),
  });
  trashBannerEl.hidden = !ro;
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
  trashLabelEl.textContent = n === 0 ? "🗑 Trash" : `🗑 Trash (${n})`;
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
    setReadOnly(true);
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
  // Don't react while previewing a trash entry — the read-only buffer's path
  // points inside .hiker/trash/ which the watcher already ignores, but guard
  // defensively so we never mutate a preview buffer.
  if (!buffer || buffer.preview) return;

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
