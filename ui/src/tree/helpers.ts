// Helpers extracted from `./index.ts` to keep the mount module under the
// 1200-line cap. Pure functions + small DOM helpers only — no module-level
// state. Re-exported from `./index.ts` so external callers continue to
// import from `../tree`.

import { Classes, IX_STATE_CLASSES } from "../style/classes";

export type EntryKind = "dir" | "file";
export interface DirEntry {
  name: string;
  rel_path: string;
  kind: EntryKind;
  mtime: number;
}

export type IndexState =
  | { kind: "indexed" }
  | { kind: "unsupported" }
  | { kind: "skipped"; reason: string }
  | { kind: "queued" };

export type TreeSortOrder =
  | "name-asc"
  | "name-desc"
  | "mtime-newest"
  | "mtime-oldest";

type SortByConfig = "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc";

export function sortOrderFromSettings(s: SortByConfig): TreeSortOrder {
  switch (s) {
    case "name_asc": return "name-asc";
    case "name_desc": return "name-desc";
    case "mtime_desc": return "mtime-newest";
    case "mtime_asc": return "mtime-oldest";
  }
}

export function sortOrderToSettings(o: TreeSortOrder): SortByConfig {
  switch (o) {
    case "name-asc": return "name_asc";
    case "name-desc": return "name_desc";
    case "mtime-newest": return "mtime_desc";
    case "mtime-oldest": return "mtime_asc";
  }
}

export function sortOrderLabel(order: TreeSortOrder): string {
  switch (order) {
    case "name-asc": return "Name (A→Z)";
    case "name-desc": return "Name (Z→A)";
    case "mtime-newest": return "Modified (newest first)";
    case "mtime-oldest": return "Modified (oldest first)";
  }
}

export function parentOf(rel: string): string {
  const idx = rel.lastIndexOf("/");
  return idx >= 0 ? rel.slice(0, idx) : "";
}

export function sortTreeEntries(
  entries: DirEntry[],
  order: TreeSortOrder,
): DirEntry[] {
  return entries.slice().sort((a, b) => {
    const aDir = a.kind === "dir";
    const bDir = b.kind === "dir";
    if (aDir && !bDir) return -1;
    if (!aDir && bDir) return 1;
    switch (order) {
      case "name-asc":
        return a.name.localeCompare(b.name);
      case "name-desc":
        return b.name.localeCompare(a.name);
      case "mtime-newest":
        return b.mtime - a.mtime;
      case "mtime-oldest":
        return a.mtime - b.mtime;
    }
  });
}

export function applyIndexMarker(
  li: HTMLElement,
  state: IndexState | null,
): void {
  li.classList.remove(...IX_STATE_CLASSES);
  li.removeAttribute("data-ix-reason");
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
      li.classList.add(Classes.IX_UNSUPPORTED);
      li.removeAttribute("title");
      break;
    case "skipped":
      li.classList.add(Classes.IX_SKIPPED);
      li.dataset.ixReason = state.reason;
      li.title = `Skipped — ${state.reason}`;
      break;
    case "queued":
      li.classList.add(Classes.IX_QUEUED);
      li.removeAttribute("title");
      break;
    case "indexed":
      li.classList.add(Classes.IX_INDEXED);
      li.removeAttribute("title");
      break;
  }
}
