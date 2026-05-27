# Live preview

Markdown live-preview rendering inside the editor — fade the syntax markers when the cursor is elsewhere, reveal them when the cursor is on the same line. Obsidian-style, deliberately narrow scope. Widget-based rendering (images, math, embeds, callouts) is a separate, later concern; see "Deferred" below.

The headline decisions:

- **Tier 1 only in v1** — inline styling + cursor-line marker reveal. No widgets, no media, no math. Tiers 2 and 3 are listed at the bottom for future work. [live-preview-tier1-scope]
- **Per-line reveal granularity.** Cursor on line N reveals all syntax markers on line N; markers everywhere else fade. Non-empty selection reveals every marker inside the selected range. [live-preview-cursor-line-reveal, live-preview-selection-reveal-all]
- **Default on.** First-launch users see live preview immediately — that's the editor's intended look. The toggle is `view-live-preview-toggle` in the View options menu (`editor.md`); flipping it off shows raw markdown for users who want it.[live-preview-default-on]
- **Built on the editor's markdown parse.** Decoration logic walks the existing markdown syntax tree (the `editor-md` crate); we don't depend on heavy rich-markdown packages. Full control over what fades and when. [live-preview-built-on-lang-markdown]
- **Disabled for non-markdown buffers.** When the buffer isn't rendered as markdown (i.e. plain `.txt` mode with `txt-render-as-markdown-default` off, or future formats), no live-preview decorations apply. [live-preview-disabled-non-md]


## Scope: tiers

Live preview can mean very different things; pinning the v1 scope explicitly so future work has clean handoffs.

**Tier 1 (v1 — this spec):** inline styling + cursor-line marker reveal. Headings/bold/italic/inline-code/links style in place; their syntax markers fade when the cursor is on a different line. Block elements (lists, blockquotes, code fences, frontmatter) keep their markers visible to the extent specified below. No widget replacement of any kind — images stay as `![alt](url)` text, math stays as `$x^2$` literal source.

**Tier 2 (deferred):** block widgets — render HRs as actual rules, checkboxes as toggles, tables as styled tables, callouts as colored gutters. Doable on top of Tier 1's decoration plumbing without rewriting it.

**Tier 3 (deferred):** media widgets — inline images, KaTeX-rendered math, embeds, transclusions, drag-to-resize. This is the inline-widget layer left for later in the editor's decoration pipeline (see `editor.md`). Much larger surface (image fetching, math runtime, alt-text fallback, security model for inline HTML); deliberately separated from live preview proper.

The split is doc-level too — Tiers 2 and 3 land as their own specs when they're built.


## Reveal mechanic

Per-line is the primary rule: marker decorations are conditioned on whether the active cursor's line equals the marker's line. The decoration provider recomputes the active-line set when the selection changes and rebuilds decorations from the syntax tree, scoping the rebuild to the affected ranges.

Two augmentations:

- **Selection range reveal.** When the selection is non-empty, every marker whose range intersects the selection is revealed — not just markers on the anchor or head line. This makes "select the whole bold span and check what you've got" work without surprises. [live-preview-selection-reveal-all]
- **Multi-cursor.** Multiple selections are common; reveal logic uses the union of all selection lines. Cheap — the editor's `Selection` (`editor-core`) already enumerates every range.

Block elements (code fences, frontmatter) extend the rule from per-line to per-block — see "Block elements" below.


## Inline markers

These markers fade when their line lacks the cursor and the line lacks selection overlap, and reveal otherwise. Styling (bold weight, italic slant, etc.) applies always — only the literal markup characters fade.

| Element | Marker | Style applied always |
|---|---|---|
| Bold | `**…**` | bold weight |
| Italic | `*…*` / `_…_` | italic slant |
| Bold+italic | `***…***` | both |
| Strikethrough | `~~…~~` | strike-through line-decoration |
| Inline code | `` `…` `` | monospace + subtle background |

[live-preview-marker-fade-inline]

**Links.** A link `[text](url)` fades the brackets *and* the `(url)` portion; what remains visible is `text`, styled as a link (color, underline). Cursor-on-line reveals the full source. The url-fade is the load-bearing decision — it's the visible difference between "this looks like a markdown editor" and "this looks like rendered prose with a marker peek." [live-preview-link-url-fade]

**Wikilinks (`[[id]]`)** are explicitly out of scope here — they're owned by a separate wikilink decoration layer (see `editor.md`). Live preview proper leaves wikilink syntax untouched.


## Block elements

- **Headings.** `#`, `##`, ... markers fade on lines without the cursor. Heading text always renders at the appropriate size and weight; the fade is just the marker itself plus the trailing space. ATX (`# heading`) is the only form supported in v1; setext-style underlined headings (`===` / `---`) keep their underline visible (they're a marker spread across two lines and per-line reveal logic gets weird). [live-preview-heading-style-fade-marker]
- **Code fences (` ``` `).** Per-block reveal, not per-line: when the cursor is anywhere *inside the fenced block* (between the opening and closing ` ``` ` lines, inclusive), the fences reveal. Outside, they fade and the block renders with a monospace background. Per-line would have shown a fence-line on top of an "indented" block which looks broken; per-block matches what Obsidian does and reads cleanly. Code-block content stays the literal source — language-specific syntax highlighting inside fenced blocks is a separate feature, not part of live preview. [live-preview-code-fence-block-reveal]
- **Blockquotes (`>`).** The `>` marker is *always* visible. It's a useful structural cue and fading it (turning quoted text into bare-but-italic prose) is more disorienting than helpful. The text after the marker can pick up subtle styling (muted color, italic) but the marker itself stays. [live-preview-block-markers-keep]
- **Lists.** Bullets (`-`/`*`/`+`) and numbered prefixes (`1.`, `2.`) are *always* visible. Same reasoning as blockquotes — they're structural, not decorative. v1 does not prettify bullets to `•` glyphs; the literal marker is what shows. [live-preview-block-markers-keep]
- **Horizontal rules (`---`).** v1 leaves them as literal text on a styled line (subtle muted color). Tier 2 will replace them with an actual rule decoration.
- **Tables.** v1 leaves them as literal source. Tables read tolerably as monospaced pipe-and-dash text, and a real table widget is Tier 2 work.
- **Frontmatter.** The `---` block at the top of a `.md` file is parsed by the editor's markdown parser and rendered as a styled-but-plain block (muted color, monospace). No marker fading; no key/value parsing in this layer. [live-preview-frontmatter-passthrough]


## Implementation

The live-preview decorations are produced by a decoration provider in the `editor-md` crate and aggregated onto the editor view's decoration set:

- A pure `&Editor → Set` function keyed off the markdown syntax tree. Visit syntax nodes; emit `Decoration::Range` styling spans, and hidden / `Decoration::Widget` ranges for the marker characters.
- The provider recomputes the "active lines" set when the selection or document changes, then flips the marker-hidden ranges on/off. The rebuild scopes to changed ranges, not the whole doc.
- Theme-token-driven styling so light/dark themes both work without a parallel stylesheet, using the editor's theme tokens.

The providers live in the `editor-md` crate (e.g. `editor/editor-md/src/styling.rs`, `links.rs`), wired into the buffer view in `app/src/panels/buffer/mod.rs`. Keeping it ours means the fade rules above are decisions in our code, not in some upstream config we have to fight.

**Toggle wiring.** `view-live-preview-toggle` (already reserved in `editor.md`) becomes live with this spec. The View menu entry is no longer greyed; clicking toggles whether the live-preview decorations are applied. State is in-memory only in v1, per the View options menu's persistence rule.

**Non-markdown gating.** Live preview only applies when the buffer is rendered as markdown (per `editor.md` and `txt-ingest.md`); when it isn't, the live-preview provider emits no decorations.


## Deferred

- **Tier 2 — block widgets.** Real `<hr>`, real checkbox toggles, real styled tables, callout gutters. Builds on Tier 1's decoration plumbing.
- **Tier 3 — media widgets.** Inline images, KaTeX math, embeds/transclusions, drag-to-resize. Left for later in the editor's decoration pipeline (see `editor.md`); gets its own spec when built.
- **Setext-style heading marker fade.** Skipped in v1 because per-line reveal misbehaves on multi-line markers; revisit if real content uses them.
- **Bullet-glyph prettification** (`-` → `•`). Out of v1 to keep "what you typed" visually present. Easy to add later as a styling-only change.
- **Code-block syntax highlighting** (language-specific colors inside fenced blocks). Separate language-injection feature, not part of live preview.
- **Per-vault / per-user persistence of the toggle.** Lands with `settings.md`'s editor section.


## Out of scope

- Wikilink rendering — owned by the future wikilinks extension.
- Anything that mutates the source file (live preview is decoration-only by definition).
- Spellcheck visuals, grammar squiggles, comment threads — separate extensions if they ever land.
- "Source mode" — the toggle off-state already serves this purpose; we don't need a third mode.
