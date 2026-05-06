import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { EditorState, Compartment } from "@codemirror/state";
import { EditorView, ViewPlugin } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { markdown } from "@codemirror/lang-markdown";
import { register, validate, toCMKeymap } from "./editor/keybinds";

type EntryKind = "dir" | "file";
interface DirEntry {
  name: string;
  rel_path: string;
  kind: EntryKind;
}
interface FileWithHash {
  contents: string;
  hash: string;
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
const refreshTreeBtn = document.getElementById("refresh-tree-btn") as HTMLButtonElement;

interface Buffer {
  path: string;
  loadedText: string;
  loadedHash: string;
}

let buffer: Buffer | null = null;
const language = new Compartment();

function isDirty(): boolean {
  if (!buffer) return false;
  return view.state.doc.toString() !== buffer.loadedText;
}

function updateStatus(): void {
  const dirty = isDirty();
  const path = buffer?.path ?? "";
  document.title = (dirty ? "• " : "") + (path ? `Hiker — ${path}` : "Hiker");
  statusPathEl.textContent = path;
  saveBtn.disabled = !buffer || !dirty;
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
      EditorView.lineWrapping,
      language.of(markdown()),
      statusUpdater,
      toCMKeymap(),
    ],
  }),
});

saveBtn.addEventListener("click", () => void save());

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
    });
    // Compare against CM's canonical doc representation (it normalizes CRLF
    // and similar on input), not the raw file string, or the buffer reads
    // dirty immediately on open.
    buffer = { path: rel, loadedText: view.state.doc.toString(), loadedHash: file.hash };
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelector(`#tree li[data-path="${cssEscape(rel)}"]`)?.classList.add("active");
    updateStatus();
  } catch (err) {
    console.error("openFile failed:", rel, err);
    alert(`open failed: ${err}`);
  }
}

function cssEscape(s: string): string {
  return s.replace(/["\\]/g, "\\$&");
}

// Tracks the folder a "+ New note" click should target. Updated when the
// user clicks a folder row or a file row (file → its parent). Empty string
// means vault root.
let selectedFolder: string = "";

function parentOf(rel: string): string {
  const idx = rel.lastIndexOf("/");
  return idx >= 0 ? rel.slice(0, idx) : "";
}

async function renderDir(rel: string, container: HTMLElement): Promise<void> {
  const entries = await invoke<DirEntry[]>("list_dir", { rel });
  const ul = document.createElement("ul");
  for (const entry of entries) {
    const li = document.createElement("li");
    li.dataset.path = entry.rel_path;
    li.dataset.kind = entry.kind;
    li.draggable = true;
    li.textContent = (entry.kind === "dir" ? "▸ " : "  ") + entry.name;
    attachDnd(li, entry);
    if (entry.kind === "dir") {
      let expanded = false;
      let childContainer: HTMLElement | null = null;
      li.addEventListener("click", async (e) => {
        e.stopPropagation();
        selectedFolder = entry.rel_path;
        if (expanded) {
          childContainer?.remove();
          childContainer = null;
          expanded = false;
          li.textContent = "▸ " + entry.name;
        } else {
          childContainer = document.createElement("div");
          li.after(childContainer);
          await renderDir(entry.rel_path, childContainer);
          expanded = true;
          li.textContent = "▾ " + entry.name;
        }
      });
    } else {
      li.addEventListener("click", (e) => {
        e.stopPropagation();
        selectedFolder = parentOf(entry.rel_path);
        void openFile(entry.rel_path);
      });
    }
    ul.appendChild(li);
  }
  container.appendChild(ul);
}

// status: drag-and-drop-move
function attachDnd(li: HTMLLIElement, entry: DirEntry): void {
  // Folder DnD is deferred — folder moves require walking contents and
  // calling move_note per file in a single tx. Disable drag on dirs for v1
  // so users don't silently desync the index by renaming a folder on disk
  // without updating the indexed paths inside it.
  if (entry.kind === "dir") {
    li.draggable = false;
  }
  li.addEventListener("dragstart", (e) => {
    if (entry.kind === "dir") {
      e.preventDefault();
      return;
    }
    e.dataTransfer?.setData("text/plain", entry.rel_path);
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
    // Drop onto folder → move into folder. Drop onto file → file's parent.
    const targetFolder = entry.kind === "dir" ? entry.rel_path : parentOf(entry.rel_path);
    await performDrop(from, targetFolder);
  });
}

async function performDrop(from: string, targetFolder: string): Promise<void> {
  if (from === targetFolder) return;
  const fromParent = parentOf(from);
  if (fromParent === targetFolder) return; // same parent → no-op per spec
  const name = from.split("/").pop()!;
  // Don't allow dropping a folder into itself or its descendants.
  if (targetFolder === from || targetFolder.startsWith(from + "/")) return;
  const to = targetFolder ? `${targetFolder}/${name}` : name;
  try {
    await invoke("move_note", { from, to });
    if (buffer && buffer.path === from) {
      buffer.path = to;
      updateStatus();
    }
    await refreshTree();
  } catch (err) {
    console.error("move_note failed:", err);
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
  await performDrop(from, "");
});

async function openVault(): Promise<void> {
  const path = await invoke<string | null>("pick_vault");
  if (!path) return;
  const basename = path.replace(/[/\\]+$/, "").split(/[/\\]/).pop() ?? path;
  vaultPathEl.textContent = basename;
  vaultPathEl.title = path;
  selectedFolder = "";
  await refreshTree();
}

pickBtn.addEventListener("click", () => void openVault());

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

// status: tree-refresh-manual
refreshTreeBtn.addEventListener("click", () => void refreshTree());

async function beginInlineRename(li: HTMLLIElement, currentPath: string): Promise<void> {
  const name = currentPath.split("/").pop()!;
  const dotIdx = name.lastIndexOf(".");
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
        try {
          await invoke("move_note", { from: currentPath, to });
          if (buffer && buffer.path === currentPath) {
            buffer.path = to;
            updateStatus();
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

// ---------- panel toggles ----------

function syncToggleButtons(): void {
  toggleSidebarBtn.classList.toggle("active", !appEl.classList.contains("sidebar-collapsed"));
  toggleRelatedBtn.classList.toggle("active", !appEl.classList.contains("related-collapsed"));
}

toggleSidebarBtn.addEventListener("click", () => {
  appEl.classList.toggle("sidebar-collapsed");
  syncToggleButtons();
});
toggleRelatedBtn.addEventListener("click", () => {
  appEl.classList.toggle("related-collapsed");
  syncToggleButtons();
});

// Default: tree open, related collapsed (per editor.md).
appEl.classList.add("related-collapsed");
syncToggleButtons();

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

// Re-fetch related notes whenever the active buffer changes.
let lastSeenBufferPath: string | null = null;
window.setInterval(() => {
  const cur = buffer?.path ?? null;
  if (cur !== lastSeenBufferPath) {
    lastSeenBufferPath = cur;
    scheduleRelatedRefresh(0);
  }
}, 250);

// Refresh shortly after a save (debounced 500ms per index.md).
const _origSaveBtnClick = () => void save();
saveBtn.removeEventListener("click", _origSaveBtnClick);
saveBtn.addEventListener("click", async () => {
  const ok = await save();
  if (ok) scheduleRelatedRefresh(500);
});

// ---------- index status indicator ----------

let indexStatus: IndexStatus = { model_ready: false, queued: 0, total_notes: 0, last_error: null };
// `pending` = jobs queued but not yet processing.
// `inFlight` = job currently processing (0 or 1 since the indexer is serial).
let pendingCount = 0;
let inFlightCount = 0;

function renderIndexStatus(): void {
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
  const total = pendingCount + inFlightCount;
  if (total > 0) {
    statusIndexEl.textContent = `Indexing ${total} pending`;
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
      // A queued job just transitioned to in-flight.
      pendingCount = Math.max(0, pendingCount - 1);
      inFlightCount += 1;
      break;
    case "finished":
    case "skipped":
    case "deleted":
    case "renamed":
      inFlightCount = Math.max(0, inFlightCount - 1);
      // A successful job clears any previously-pinned error.
      indexStatus.last_error = null;
      if (ev.kind === "finished" && buffer && (ev as { path: string }).path === buffer.path) {
        scheduleRelatedRefresh(100);
      }
      break;
    case "scan_complete":
      // The full scan just enqueued `queued` jobs. They'll surface as
      // started → finished pairs from here on; we just record the new
      // pending depth.
      pendingCount += ev.queued;
      break;
    case "error":
      inFlightCount = Math.max(0, inFlightCount - 1);
      indexStatus.last_error = ev.message;
      break;
  }
  renderIndexStatus();
  void pollIndexStatus();
});

window.setInterval(() => {
  void pollIndexStatus();
}, 2000);

// ---------- watcher → editor integration ----------
// Silent reload for clean buffers when their file changes externally.
// Conflict prompts for dirty buffers stay in the existing pre-write check.

void listen<{ kind: string; path?: string; from?: string; to?: string }>(
  "hiker:file-changed",
  async (event) => {
    const ev = event.payload;
    if (!buffer) return;
    if (ev.kind === "modified" && ev.path === buffer.path && !isDirty()) {
      try {
        const fresh = await invoke<FileWithHash>("read_file_with_hash", { rel: buffer.path });
        if (fresh.hash !== buffer.loadedHash) {
          view.dispatch({
            changes: { from: 0, to: view.state.doc.length, insert: fresh.contents },
          });
          buffer.loadedText = view.state.doc.toString();
          buffer.loadedHash = fresh.hash;
          updateStatus();
        }
      } catch (err) {
        console.error("silent reload failed:", err);
      }
    }
  },
);

void _origSaveBtnClick;
