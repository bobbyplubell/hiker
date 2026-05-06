// status: view-show-chunk-boundaries
//
// Visualizes the chunker's output: a thin horizontal rule between chunks
// and the chunk index in the gutter at each chunk's start line. See
// docs/editor.md "View options menu" for the spec — this is a debugging-
// grade view, useful for sanity-checking chunker behavior.
//
// Backend (`tauri-cmd-chunks-for-path`) returns byte offsets. CM6 works in
// JS string positions (UTF-16 code units), so byte_start is mapped onto a
// CM line by walking the doc once, accumulating UTF-8 byte counts.

import { StateEffect, StateField, type Extension } from "@codemirror/state";
import {
  Decoration,
  EditorView,
  gutter,
  GutterMarker,
} from "@codemirror/view";

export interface ChunkBounds {
  chunk_index: number;
  byte_start: number;
  byte_end: number;
  heading_path: string | null;
}

interface ChunkLineEntry {
  chunkIndex: number;
  lineNumber: number;
}

interface ChunkBoundariesState {
  // Per-line entries (one per chunk start line) keyed by line number — the
  // doc-line that the chunk starts on. Stored as a sorted array so the
  // gutter lookup is a small linear scan.
  entries: ChunkLineEntry[];
  // Faint gutter hint shown when the file has no chunks (unindexed,
  // skipped, queued, empty). null when chunks are present or the feature
  // hasn't been activated for this buffer.
  hint: string | null;
}

const emptyState: ChunkBoundariesState = { entries: [], hint: null };

export const setChunkBoundaries = StateEffect.define<ChunkBoundariesState>();

const chunkBoundariesField = StateField.define<ChunkBoundariesState>({
  create: () => emptyState,
  update(value, tr) {
    for (const ef of tr.effects) {
      if (ef.is(setChunkBoundaries)) return ef.value;
    }
    return value;
  },
});

const chunkBoundaryDeco = Decoration.line({ class: "cm-chunk-boundary" });

const chunkBoundaryDecorations = EditorView.decorations.compute(
  [chunkBoundariesField],
  (state) => {
    const { entries } = state.field(chunkBoundariesField);
    if (entries.length === 0) return Decoration.none;
    const builder: { from: number; deco: Decoration }[] = [];
    for (const e of entries) {
      // Skip the first chunk's start: the rule is *between* chunks.
      if (e.chunkIndex === 0) continue;
      if (e.lineNumber < 1 || e.lineNumber > state.doc.lines) continue;
      const line = state.doc.line(e.lineNumber);
      builder.push({ from: line.from, deco: chunkBoundaryDeco });
    }
    return Decoration.set(
      builder.map((b) => b.deco.range(b.from)),
      true,
    );
  },
);

class ChunkIndexMarker extends GutterMarker {
  constructor(public readonly index: number) {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return other instanceof ChunkIndexMarker && other.index === this.index;
  }
  override toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-chunk-gutter-index";
    el.textContent = String(this.index);
    return el;
  }
}

class ChunkHintMarker extends GutterMarker {
  constructor(public readonly hint: string) {
    super();
  }
  override eq(other: GutterMarker): boolean {
    return other instanceof ChunkHintMarker && other.hint === this.hint;
  }
  override toDOM(): HTMLElement {
    const el = document.createElement("span");
    el.className = "cm-chunk-gutter-hint";
    el.textContent = this.hint;
    return el;
  }
}

const chunkGutter = gutter({
  class: "cm-chunk-gutter",
  lineMarker(view, line) {
    const state = view.state.field(chunkBoundariesField, false);
    if (!state) return null;
    if (state.entries.length === 0) {
      // Hint shows once, at line 1, when chunks aren't available.
      if (state.hint && line.from === 0) return new ChunkHintMarker(state.hint);
      return null;
    }
    const ln = view.state.doc.lineAt(line.from).number;
    for (const e of state.entries) {
      if (e.lineNumber === ln) return new ChunkIndexMarker(e.chunkIndex);
    }
    return null;
  },
  initialSpacer: () => new ChunkIndexMarker(0),
});

const chunkTheme = EditorView.baseTheme({
  ".cm-chunk-boundary": {
    borderTop: "1px solid #d97757",
  },
  ".cm-chunk-gutter": {
    minWidth: "1.4em",
  },
  ".cm-chunk-gutter .cm-gutterElement": {
    color: "#d97757",
    fontSize: "0.75em",
    paddingRight: "4px",
    textAlign: "right",
  },
  ".cm-chunk-gutter-hint": {
    color: "#999",
    fontStyle: "italic",
    fontSize: "0.7em",
    paddingRight: "4px",
  },
});

/**
 * Convert a UTF-8 byte offset to a CM6 line number (1-indexed). Walks the
 * doc accumulating per-line byte counts; the chunker uses LF-counted byte
 * offsets, so we use `+ 1` per line break (matching the on-disk shape).
 *
 * If the offset falls past the doc, returns the last line.
 */
export function byteOffsetToLine(view: EditorView, byteOffset: number): number {
  if (byteOffset <= 0) return 1;
  const doc = view.state.doc;
  const enc = new TextEncoder();
  let bytesSoFar = 0;
  for (let n = 1; n <= doc.lines; n++) {
    const ln = doc.line(n);
    const lineBytes = enc.encode(ln.text).length;
    // The boundary lands within this line (inclusive of the leading position).
    if (bytesSoFar + lineBytes >= byteOffset) return n;
    bytesSoFar += lineBytes + 1;
    if (bytesSoFar >= byteOffset) return Math.min(n + 1, doc.lines);
  }
  return doc.lines;
}

export function chunkBoundsToState(
  view: EditorView,
  bounds: ChunkBounds[],
): ChunkBoundariesState {
  if (bounds.length === 0) return emptyState;
  const entries: ChunkLineEntry[] = [];
  for (const b of bounds) {
    entries.push({
      chunkIndex: b.chunk_index,
      lineNumber: byteOffsetToLine(view, b.byte_start),
    });
  }
  entries.sort((a, b) => a.lineNumber - b.lineNumber);
  return { entries, hint: null };
}

export function chunkBoundariesHintState(hint: string): ChunkBoundariesState {
  return { entries: [], hint };
}

export function clearChunkBoundariesState(): ChunkBoundariesState {
  return emptyState;
}

export function chunkBoundaries(): Extension {
  return [chunkBoundariesField, chunkBoundaryDecorations, chunkGutter, chunkTheme];
}
