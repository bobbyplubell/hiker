// status: bug-inline-svg-strings-scattered
//
// Single home for the inline SVG markup the UI hands to `innerHTML` /
// template strings. Each `Icons.*()` function returns a complete `<svg>`
// element string; `svgWrapper(content, opts?)` owns the shared
// `viewBox="0 0 16 16"` / `aria-hidden` / stroke defaults so individual
// icons only carry their distinguishing path data.
//
// Icons rely on `currentColor` for stroke/fill — the css-variable
// theming work has already landed, so colors flow from the surrounding
// text color without any per-icon hex literals.
//
// Sizes / stroke-widths vary per call site (toolbar buttons use 14px /
// 1.5 stroke; in-line pills use 12px / 1.4 stroke; the brain glyph
// wants a thinner 0.9 stroke to read at small size). Defaults below
// match the most common toolbar-icon shape; callers pass `{size,
// strokeWidth}` to override.
//
// `modeControls`'s prior `ICON_DIFF` / `ICON_RESTORE` / `ICON_CLOSE`
// constants and the `queueDetail` / `vaultHome` / `chat.ts` inline
// duplicates all import from here now.
//
// Adding a new icon: declare a function returning `svgWrapper(<paths>,
// {...overrides})`. Keep the path snippet only — the wrapper handles
// the `<svg>` tag itself.
//
// status: hardcoded-colors-bypass-css-variables (paired)

export interface SvgWrapperOptions {
  size?: number;
  strokeWidth?: number;
}

const DEFAULT_SIZE = 14;
const DEFAULT_STROKE_WIDTH = 1.5;

/// Wraps the given inner SVG `content` (paths / shapes only — no `<svg>`
/// tag) in the shared `<svg>` envelope: 16x16 viewBox, currentColor
/// stroke, round joins, aria-hidden. Caller-supplied size / stroke
/// width override the defaults.
export function svgWrapper(content: string, opts: SvgWrapperOptions = {}): string {
  const size = opts.size ?? DEFAULT_SIZE;
  const strokeWidth = opts.strokeWidth ?? DEFAULT_STROKE_WIDTH;
  return `<svg viewBox="0 0 16 16" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="${strokeWidth}" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true">${content}</svg>`;
}

export const Icons = {
  trash(opts: SvgWrapperOptions = { size: 12, strokeWidth: 1.5 }): string {
    return svgWrapper(
      `<path d="M3 4.5h10"/><path d="M6.5 4.5V3a1 1 0 0 1 1-1h1a1 1 0 0 1 1 1v1.5"/><path d="M4.5 4.5l.6 8.2a1 1 0 0 0 1 .9h3.8a1 1 0 0 0 1-.9l.6-8.2"/>`,
      opts,
    );
  },

  robot(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.4 }): string {
    return svgWrapper(
      `<rect x="3" y="6" width="10" height="7" rx="1.5"/><line x1="8" y1="3.5" x2="8" y2="6"/><circle cx="8" cy="3" r="0.6" fill="currentColor"/><circle cx="6" cy="9.2" r="0.7" fill="currentColor"/><circle cx="10" cy="9.2" r="0.7" fill="currentColor"/><line x1="6" y1="11.5" x2="10" y2="11.5"/>`,
      opts,
    );
  },

  brain(opts: SvgWrapperOptions = { size: 14, strokeWidth: 0.9 }): string {
    return svgWrapper(
      `<path d="M8 2.4c-1.4-.9-3.3-.4-3.9 1-1.3.1-2.1 1.3-1.7 2.4-.7.7-.6 1.9.2 2.4-.4.8 0 1.9 1 2.2.1 1.2 1.3 2 2.5 1.7.5.7 1.5.9 2.2.4"/><path d="M8 2.4c1.4-.9 3.3-.4 3.9 1 1.3.1 2.1 1.3 1.7 2.4.7.7.6 1.9-.2 2.4.4.8 0 1.9-1 2.2-.1 1.2-1.3 2-2.5 1.7-.5.7-1.5.9-2.2.4"/><path d="M8 2.7v11"/><path d="M5.2 4.6c.5.3.6 1 .2 1.5"/><path d="M3.7 6.9c.6.1 1 .7.8 1.3"/><path d="M4.2 9.3c.6-.1 1.2.3 1.3.9"/><path d="M6.4 11c.5-.3 1.2-.1 1.5.4"/><path d="M10.8 4.6c-.5.3-.6 1-.2 1.5"/><path d="M12.3 6.9c-.6.1-1 .7-.8 1.3"/><path d="M11.8 9.3c-.6-.1-1.2.3-1.3.9"/><path d="M9.6 11c-.5-.3-1.2-.1-1.5.4"/>`,
      opts,
    );
  },

  diff(opts?: SvgWrapperOptions): string {
    return svgWrapper(
      `<line x1="3" y1="8" x2="13" y2="8"/><polyline points="5,5 2,8 5,11"/><polyline points="11,5 14,8 11,11"/>`,
      opts,
    );
  },

  restore(opts?: SvgWrapperOptions): string {
    return svgWrapper(
      `<path d="M3 8a5 5 0 1 0 1.5-3.5"/><polyline points="2,2 2,5 5,5"/>`,
      opts,
    );
  },

  close(opts?: SvgWrapperOptions): string {
    return svgWrapper(
      `<line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/>`,
      opts,
    );
  },

  /// status: patch-review-per-hunk-accept — Accept (✓) icon.
  check(opts: SvgWrapperOptions = { size: 14, strokeWidth: 2 }): string {
    return svgWrapper(`<polyline points="3,8 7,12 13,4"/>`, opts);
  },

  /// status: patch-review-per-hunk-accept — Reject (×) icon. Distinct
  /// from `close` only in default stroke weight, so the pair reads as
  /// a balanced accept/reject affordance.
  cross(opts: SvgWrapperOptions = { size: 14, strokeWidth: 2 }): string {
    return svgWrapper(
      `<line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/>`,
      opts,
    );
  },

  user(opts: SvgWrapperOptions = { size: 12, strokeWidth: 1.4 }): string {
    return svgWrapper(
      `<circle cx="8" cy="5.5" r="2.4"/><path d="M3.5 13.5c0-2.4 2-4 4.5-4s4.5 1.6 4.5 4"/>`,
      opts,
    );
  },

  /// Solid filled dot — used as a generic "other author" glyph in the
  /// recent-activity pill. Doesn't take a stroke; renders via fill.
  dot(opts: SvgWrapperOptions = { size: 12 }): string {
    const size = opts.size ?? DEFAULT_SIZE;
    return `<svg viewBox="0 0 16 16" width="${size}" height="${size}" aria-hidden="true"><circle cx="8" cy="8" r="3" fill="currentColor"/></svg>`;
  },

  send(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.6 }): string {
    return svgWrapper(`<path d="M2 8L14 2L9 14L7 9L2 8z"/>`, opts);
  },

  /// Squiggly-trail glyph used by the Trails sidebar mode-switcher
  /// button and the trails-mode header trail-head icon. Path commands
  /// mirror the inline SVG at `#sidebar-mode-trails` in `index.html`.
  trail(opts: SvgWrapperOptions = { size: 14 }): string {
    const size = opts.size ?? DEFAULT_SIZE;
    return `<svg viewBox="0 0 16 16" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-linejoin="round" stroke-linecap="round" overflow="visible" aria-hidden="true"><path stroke-width="2.8" d="M8 15 C 20 12.5, -4 9, 8 7"/><path stroke-width="1.3" d="M8 7 C 13 5.5, 4 3.5, 8 2"/><path stroke-width="0.7" d="M8 2 C 8.8 1.4, 7.5 1, 8 0.5"/></svg>`;
  },

  stop(opts: SvgWrapperOptions = { size: 14 }): string {
    const size = opts.size ?? DEFAULT_SIZE;
    return `<svg viewBox="0 0 16 16" width="${size}" height="${size}" aria-hidden="true"><rect x="4" y="4" width="8" height="8" rx="1" fill="currentColor"/></svg>`;
  },

  /// Cluster-tree topology — three nodes connected as a small tree.
  /// Matches the `#sidebar-mode-clusters` mode-switcher glyph so the
  /// cluster-editor pane's Tree-view toggle reads as "same concept as
  /// the sidebar's Clusters mode".
  clusterTreeShape(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.5 }): string {
    return svgWrapper(
      `<circle cx="8" cy="3.25" r="1.5"/><circle cx="3.5" cy="12.5" r="1.5"/><circle cx="12.5" cy="12.5" r="1.5"/><path d="M8 4.75v2.5"/><path d="M8 7.25H3.5v3.75"/><path d="M8 7.25h4.5v3.75"/>`,
      opts,
    );
  },

  /// Network graph — diamond of four nodes around a central hub with
  /// curved edges. Reads as "interconnected web", distinct from the
  /// rigid parent→child tree topology in `clusterTreeShape`. Used by
  /// the cluster-editor pane's Graph-view toggle.
  graphNodes(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.3 }): string {
    return svgWrapper(
      `<path d="M8 3 Q 5.5 6 3.5 8.5"/><path d="M8 3 Q 10.5 6 12.5 8.5"/><path d="M3.5 8.5 Q 5.5 11.5 8 13"/><path d="M12.5 8.5 Q 10.5 11.5 8 13"/><path d="M3.5 8.5 Q 8 7.5 12.5 8.5"/><circle cx="8" cy="3" r="1.5" fill="currentColor" stroke="none"/><circle cx="3.5" cy="8.5" r="1.5" fill="currentColor" stroke="none"/><circle cx="12.5" cy="8.5" r="1.5" fill="currentColor" stroke="none"/><circle cx="8" cy="13" r="1.5" fill="currentColor" stroke="none"/>`,
      opts,
    );
  },

  /// Lowercase "md" stylized as a letter glyph — paired with the Aa
  /// lexical-search button (search-mode toggles). Used by the cluster
  /// editor pane's Markdown-view toggle so all three view buttons read
  /// as icon-only square chips.
  mdLabel(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.3 }): string {
    return svgWrapper(
      `<path d="M2 12.5 V 6.5"/><path d="M5.5 12.5 V 6.5"/><path d="M9 12.5 V 6.5"/><path d="M2 6.5 Q 3.75 4.7 5.5 6.5"/><path d="M5.5 6.5 Q 7.25 4.7 9 6.5"/><path d="M14 3 V 12.5"/><path d="M14 8 Q 9.5 8 9.5 10.5 Q 9.5 13 14 12.5"/>`,
      opts,
    );
  },

  /// Triage glyph — large 6-arm star-of-life (three crossing lines).
  /// Used by the cluster-editor pane's "Save as triage" toolbar button.
  /// Center (8,8); each arm ~5.5px so the star fills the 16x16 slot.
  triageStar(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.5 }): string {
    return svgWrapper(
      `<line x1="2.5" y1="8" x2="13.5" y2="8"/><line x1="10.75" y1="3.24" x2="5.25" y2="12.76"/><line x1="5.25" y1="3.24" x2="10.75" y2="12.76"/>`,
      opts,
    );
  },

  /// Diagonal "expand to fullscreen" icon — two arrows pointing NW and
  /// SE, joined by a diagonal. Used by the cluster-editor's tree-expand
  /// button (replacing the `⤢` unicode glyph). The chat panel's
  /// expand button uses the same shape rotated 90°.
  expand(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.4 }): string {
    return svgWrapper(
      `<polyline points="7 3,3 3,3 7"/><polyline points="9 13,13 13,13 9"/><line x1="3" y1="3" x2="13" y2="13"/>`,
      opts,
    );
  },

  /// Hammer — sledgehammer-shaped T: wide horizontal head across the
  /// top, narrow vertical handle dropping from its center. Reads cleanly
  /// at 16x16 without the prior tilted-handle silhouette (which read as
  /// a pistol). Used by the cluster-editor pane's "Rebuild" button.
  hammer(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.4 }): string {
    return svgWrapper(
      `<rect x="2" y="2" width="12" height="3.5" rx="0.6"/><rect x="7.2" y="5.5" width="1.6" height="8" rx="0.3"/>`,
      opts,
    );
  },

  /// Eye — same shape as the editor toolbar's view-options button
  /// (`#view-menu-btn`). Used by the cluster-editor pane's graph-view
  /// "View options" trigger so the two surfaces share the same glyph.
  eye(opts: SvgWrapperOptions = { size: 14, strokeWidth: 1.5 }): string {
    return svgWrapper(
      `<path d="M1.5 8s2.5-4.5 6.5-4.5S14.5 8 14.5 8s-2.5 4.5-6.5 4.5S1.5 8 1.5 8z"/><circle cx="8" cy="8" r="2"/>`,
      opts,
    );
  },
};
