// status: note-properties-tab-content
// status: tree-context-properties
//
// Read-only note inspector rendered inside a `properties`-kind tab. One
// tab per path; opened via the file-tree "Properties" context-menu entry.
// Pulls every piece of tracked state from the index store + changelog and
// renders it as grouped labeled blocks.

import { Ipc, type NoteProperties } from "../ipc";
import { Logger } from "../logger";

export interface PropertiesPaneDeps {
  containerEl: HTMLElement;
}

export interface PropertiesPaneApi {
  update(rel: string): Promise<void>;
}

function escapeHtml(s: string): string {
  const el = document.createElement("span");
  el.textContent = s;
  return el.innerHTML;
}

function sectionHtml(heading: string, rows: [string, string][]): string {
  if (rows.length === 0) return "";
  let html = `<h3 class="props-section-heading">${escapeHtml(heading)}</h3>`;
  html += '<dl class="props-dl">';
  for (const [label, value] of rows) {
    html += `<dt class="props-dt">${escapeHtml(label)}</dt>`;
    html += `<dd class="props-dd">${escapeHtml(value)}</dd>`;
  }
  html += "</dl>";
  return html;
}

function renderProperties(props: NoteProperties): string {
  const parts: string[] = [];

  // Identity
  const idRows: [string, string][] = [
    ["Path", props.path],
    ["Note id", props.noteId ?? "—"],
    ["Path ids id", props.pathIdsId ?? "—"],
    ["Extension", props.extension ?? "—"],
  ];
  parts.push(sectionHtml("Identity", idRows));

  // File state
  const fileRows: [string, string][] = [
    ["Mtime", props.mtime != null ? new Date(props.mtime * 1000).toISOString() : "—"],
    ["Size", props.size != null ? `${props.size.toLocaleString()} bytes` : "—"],
    ["Content hash", props.contentHash ?? "—"],
  ];
  parts.push(sectionHtml("File state", fileRows));

  // Index state
  const idxRows: [string, string][] = [
    [
      "Indexed at",
      props.indexedAt != null
        ? new Date(props.indexedAt * 1000).toISOString()
        : "—",
    ],
    ["Embedder version", props.embedderVersion || "—"],
    [
      "Status",
      props.skipped
        ? `Skipped — ${props.skipReason || "unknown"}`
        : props.noteId != null
          ? "Indexed"
          : "Not indexed",
    ],
  ];
  parts.push(sectionHtml("Index state", idxRows));

  // Chunks
  parts.push(
    sectionHtml("Chunks", [["Chunk count", String(props.chunkCount)]]),
  );

  // Access tracking
  parts.push(
    sectionHtml("Access tracking", [
      [
        "Last accessed at",
        props.lastAccessedAt != null
          ? new Date(props.lastAccessedAt * 1000).toISOString()
          : "Never",
      ],
    ]),
  );

  // Changes
  parts.push(
    sectionHtml("Changes", [["Change count", String(props.changeCount)]]),
  );

  return parts.join("");
}

export function mountPropertiesPane(
  deps: PropertiesPaneDeps,
): PropertiesPaneApi {
  return {
    async update(rel: string): Promise<void> {
      try {
        const props = await Ipc.noteProperties({ rel });
        deps.containerEl.innerHTML =
          `<h2 class="props-title">${escapeHtml(rel)}</h2>` +
          renderProperties(props);
      } catch (err) {
        Logger.error("ui::properties-pane", "noteProperties failed", {
          err,
          rel,
        });
        deps.containerEl.innerHTML = `<p class="props-error">Failed to load properties for ${escapeHtml(rel)}.</p>`;
      }
    },
  };
}
