import { invoke } from "@tauri-apps/api/core";
import { EditorState, Compartment } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { basicSetup } from "codemirror";
import { markdown } from "@codemirror/lang-markdown";
import { register, validate, toCMKeymap } from "./editor/keybinds";

type EntryKind = "dir" | "file";
interface DirEntry {
  name: string;
  rel_path: string;
  kind: EntryKind;
}

const treeEl = document.getElementById("tree")!;
const editorEl = document.getElementById("editor")!;
const pickBtn = document.getElementById("pick-vault") as HTMLButtonElement;
const vaultPathEl = document.getElementById("vault-path")!;

let currentPath: string | null = null;
const language = new Compartment();

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
      language.of(markdown()),
      toCMKeymap(),
    ],
  }),
});

async function save() {
  if (!currentPath) return;
  const contents = view.state.doc.toString();
  await invoke("write_file", { rel: currentPath, contents });
}

async function openFile(rel: string) {
  try {
    const contents = await invoke<string>("read_file", { rel });
    currentPath = rel;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: contents },
    });
    document.querySelectorAll("#tree li.active").forEach((el) => el.classList.remove("active"));
    document.querySelector(`#tree li[data-path="${cssEscape(rel)}"]`)?.classList.add("active");
  } catch (err) {
    console.error("openFile failed:", rel, err);
    alert(`open failed: ${err}`);
  }
}

function cssEscape(s: string): string {
  return s.replace(/["\\]/g, "\\$&");
}

async function renderDir(rel: string, container: HTMLElement) {
  const entries = await invoke<DirEntry[]>("list_dir", { rel });
  const ul = document.createElement("ul");
  for (const entry of entries) {
    const li = document.createElement("li");
    li.dataset.path = entry.rel_path;
    li.textContent = (entry.kind === "dir" ? "▸ " : "  ") + entry.name;
    if (entry.kind === "dir") {
      let expanded = false;
      let childUl: HTMLElement | null = null;
      li.addEventListener("click", async (e) => {
        e.stopPropagation();
        if (expanded) {
          childUl?.remove();
          childUl = null;
          expanded = false;
          li.textContent = "▸ " + entry.name;
        } else {
          childUl = document.createElement("div");
          li.after(childUl);
          await renderDir(entry.rel_path, childUl);
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

async function openVault() {
  const path = await invoke<string | null>("pick_vault");
  if (!path) return;
  vaultPathEl.textContent = path;
  treeEl.innerHTML = "";
  await renderDir("", treeEl);
}

pickBtn.addEventListener("click", () => {
  void openVault();
});
