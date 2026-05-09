// Shared YAML-frontmatter range detection for the CM6 editor surfaces
// (livePreview's pass-through styling, hideFrontmatter's block-replace
// widget). Mirrors `core::frontmatter::split` semantics: a well-formed
// block starts at byte 0 with `---\n` (line 1 is exactly `---`) and ends
// at the first subsequent `---` line; an unterminated block is ignored
// so users editing one mid-flight can still see what they're typing.
//
// The rule lives in TS rather than behind a Tauri command because both
// consumers are CM6 decoration-compute paths — synchronous, fired on
// every state change. IPC on every keystroke would regress what is now
// a trivial line scan. The authoritative parse for write-path semantics
// (the agent frontmatter merge) stays in `core::frontmatter::split`.

import type { Text } from "@codemirror/state";

export interface FrontmatterRange {
  /** Byte offset (inclusive) of the leading `---` line. Always 0. */
  from: number;
  /** Byte offset (exclusive) one past the trailing newline of the closing `---`. */
  to: number;
  /**
   * Number of lines spanned by the block (1-based: the opening `---`
   * counts as line 1, the closing as line N).
   */
  lineCount: number;
}

/**
 * Locate the leading frontmatter block in `doc`. Returns `null` when the
 * file has no frontmatter or the block is unterminated.
 *
 * `livePreview` accepts a closing `...` line in addition to `---`; that
 * variant is preserved here behind `acceptDotsClose` for backward
 * compatibility, off by default to match the stricter chunker rule.
 */
export function findFrontmatter(
  doc: Text,
  acceptDotsClose = false,
): FrontmatterRange | null {
  if (doc.lines < 2) return null;
  if (doc.line(1).text !== "---") return null;
  const cap = Math.min(doc.lines, 1000);
  for (let n = 2; n <= cap; n++) {
    const t = doc.line(n).text;
    if (t === "---" || (acceptDotsClose && t === "...")) {
      const close = doc.line(n);
      const to = Math.min(doc.length, close.to + 1);
      return { from: 0, to, lineCount: n };
    }
  }
  return null;
}
