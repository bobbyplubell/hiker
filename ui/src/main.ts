import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
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

const treeEl = document.getElementById("tree")!;
const editorEl = document.getElementById("editor")!;
const pickBtn = document.getElementById("pick-vault") as HTMLButtonElement;
const vaultPathEl = document.getElementById("vault-path")!;
const saveBtn = document.getElementById("save-btn") as HTMLButtonElement;
const statusPathEl = document.getElementById("status-path")!;
const statusCursorEl = document.getElementById("status-cursor")!;
const statusWordsEl = document.getElementById("status-words")!;

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
    buffer.loadedText = contents;
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
      buffer.loadedText = fresh.contents;
      buffer.loadedHash = fresh.hash;
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: fresh.contents } });
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
    buffer = { path: rel, loadedText: file.contents, loadedHash: file.hash };
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: file.contents },
    });
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

async function renderDir(rel: string, container: HTMLElement): Promise<void> {
  const entries = await invoke<DirEntry[]>("list_dir", { rel });
  const ul = document.createElement("ul");
  for (const entry of entries) {
    const li = document.createElement("li");
    li.dataset.path = entry.rel_path;
    li.textContent = (entry.kind === "dir" ? "▸ " : "  ") + entry.name;
    if (entry.kind === "dir") {
      let expanded = false;
      let childContainer: HTMLElement | null = null;
      li.addEventListener("click", async (e) => {
        e.stopPropagation();
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
        void openFile(entry.rel_path);
      });
    }
    ul.appendChild(li);
  }
  container.appendChild(ul);
}

async function openVault(): Promise<void> {
  const path = await invoke<string | null>("pick_vault");
  if (!path) return;
  vaultPathEl.textContent = path;
  treeEl.innerHTML = "";
  await renderDir("", treeEl);
}

pickBtn.addEventListener("click", () => void openVault());

async function confirm3(
  message: string,
  a: string,
  b: string,
  cancel: string,
): Promise<"a" | "b" | "cancel"> {
  const labels = `[1] ${a}\n[2] ${b}\n[Cancel] ${cancel}`;
  const ans = window.prompt(`${message}\n\n${labels}\n\nEnter 1 or 2 (Cancel to abort):`);
  if (ans === "1") return "a";
  if (ans === "2") return "b";
  return "cancel";
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
