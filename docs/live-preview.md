# Live preview

Markdown live-preview rendering inside the editor — fade the syntax markers when the cursor is elsewhere, reveal them when the cursor is on the same line. Inline live-preview, deliberately narrow scope. Widget-based rendering (images, math, embeds, callouts) is a separate, later concern; see "Deferred" below.

Key decisions (detailed below):

- **Tier 1 only in v1** — inline styling + cursor-line marker reveal; no widgets/media/math. [live-preview-tier1-scope]
status:: done
touches:: [[code:hiker/styling]]
note:: Tier-1 generator walks the markdown syntax tree and emits decorations; no widgets/media/math · evidence: `editor/editor-md/src/styling.rs` (`markdown_decorations()`)
- **Per-line reveal granularity** with selection-range reveal. [live-preview-cursor-line-reveal, live-preview-selection-reveal-all]
- **Default on.** That's the editor's intended look; the toggle is [[spec:view-live-preview-toggle]] in the View options menu (`editor.md`), and off shows raw markdown. [live-preview-default-on]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: live preview defaults on; the decoration provider activates whenever the active path is markdown. View menu's "Live preview" entry ([[spec:view-live-preview-toggle]]) flips it · evidence: `app/src/panels/buffer/mod.rs` (live-preview gating in `show_editor()`)
- **Built on the editor's markdown parse** (the `editor-md` crate's syntax tree); no heavy rich-markdown packages. [live-preview-built-on-lang-markdown]
status:: done
touches:: [[code:hiker/styling]]
note:: single generator over the editor crates' markdown tree; no external JS deps
- **Disabled for non-markdown buffers** (plain `.txt` mode with [[spec:txt-render-as-markdown-default]] off, or future formats). [live-preview-disabled-non-md]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: the markdown decoration provider only runs for markdown paths; non-md paths get no decorations as a side effect of language selection · evidence: `app/src/panels/buffer/mod.rs` (language selection by path)
- **Conflict regions render raw**, not preview-styled — a buffer carrying git conflict markers (the git conflict-marker resolver, `git.md` [[spec:git-conflict-inline-markers]]) shows its markers verbatim so the user resolves against the real text, not a fading-marker view. [live-preview-conflict-regions-raw]
status:: done
implements:: [[code:hiker/panels/buffer/decorations/rebuild_editor_layers]]
note:: a conflicted buffer renders its `<<<<<<< / ======= / >>>>>>>` markers verbatim — every live-preview styling layer is gated off when the buffer text still holds unresolved conflict markers, so the user resolves against real text. Predicate + verbs tested in `core::merge::conflict_surface_tests`; the visual suppression needs an in-app check · evidence: `app/src/panels/buffer/decorations.rs::rebuild_editor_decorations` (suppresses `live_preview` when `hiker_core::merge::has_unresolved_conflicts(doc)`)


## Scope: tiers

- **Tier 1 (v1 — this spec):** inline styling + cursor-line marker reveal. Headings/bold/italic/inline-code/links style in place; their syntax markers fade when the cursor is on a different line. Block elements (lists, blockquotes, code fences, frontmatter) keep their markers visible to the extent specified below. No widget replacement of any kind — images stay as `![alt](url)` text, math stays as `$x^2$` literal source.
- **Tier 2 (deferred):** block widgets — render HRs as actual rules, checkboxes as toggles, tables as styled tables, callouts as colored gutters. Doable on top of Tier 1's decoration plumbing without rewriting it.
- **Tier 3 (`editor-widgets.md`):** media widgets — rendered LaTeX math, Mermaid diagrams, tables, inline images. The rendered-widget layer over this Tier-1 plumbing; much larger surface (renderers, SVG→texture, alt-text fallback, security model for remote images), so it owns its own spec.

Tiers 2 and 3 land as their own specs when built (see "Deferred").


## Reveal mechanic

Per-line is the primary rule: marker decorations are conditioned on whether the active cursor's line equals the marker's line. The decoration provider recomputes the active-line set when the selection changes and rebuilds decorations from the syntax tree, scoping the rebuild to the affected ranges.

Two augmentations:

- **Selection range reveal.** When the selection is non-empty, every marker whose range intersects the selection is revealed — not just markers on the anchor or head line. This makes "select the whole bold span and check what you've got" work without surprises. [live-preview-selection-reveal-all]
status:: done
touches:: [[code:hiker/styling]]
note:: non-empty ranges tracked separately; selection-overlap check runs after the line check; multi-cursor unions naturally · evidence: `editor/editor-md/src/styling.rs` (`markdown_decorations()` active-range check)
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
status:: done
touches:: [[code:hiker/styling]]
note:: bold weight, italic, strike, monospace inline-code styling stays; the emphasis / strikethrough / code marks fade · evidence: `editor/editor-md/src/styling.rs` (strong / emphasis / strikethrough / inline-code branch)

**Links.** A link `[text](url)` fades the brackets *and* the `(url)` portion; what remains visible is `text`, styled as a link (color, underline). Cursor-on-line reveals the full source. [live-preview-link-url-fade]
status:: done
touches:: [[code:hiker/links]], [[code:hiker/styling]]
note:: link text styled; brackets + url + parens fade as one span · evidence: `editor/editor-md/src/links.rs` (`wikilink_decorations()`) + `editor/editor-md/src/styling.rs` (link branch)

**Wikilinks (`[[id]]`)** are explicitly out of scope here — they're owned by a separate wikilink decoration layer (see `editor.md`). Live preview proper leaves wikilink syntax untouched.


## Block elements

- **Headings.** `#`, `##`, ... markers fade on lines without the cursor. Heading text always renders at the appropriate size and weight; the fade is just the marker itself plus the trailing space. ATX (`# heading`) is the only form supported in v1; setext-style underlined headings (`===` / `---`) keep their underline visible (they're a marker spread across two lines and per-line reveal logic gets weird). [live-preview-heading-style-fade-marker]
status:: done
touches:: [[code:hiker/styling]]
note:: heading line decoration sets the level style; the header mark + trailing space fade off-line; setext intentionally untouched per spec · evidence: `editor/editor-md/src/styling.rs` (ATX-heading branch)
- **Code fences (` ``` `).** Per-block reveal, not per-line: when the cursor is anywhere *inside the fenced block* (between the opening and closing ` ``` ` lines, inclusive) **or a selection overlaps the block**, the fences reveal. Outside, they fade and the block renders with a monospace background. Per-line would have shown a fence-line on top of an "indented" block which looks broken; per-block reads cleanly. The selection-overlap path is the per-block form of [[spec:live-preview-selection-reveal-all]]: highlighting a fenced block (including a rendered Mermaid/WaveDrom widget's source, whose caret head can land past the closing fence) keeps the ` ``` ` delimiters visible — `style_fenced_code_block` keys its reveal on cursor-line OR selection-overlap, so the styling layer doesn't half-reveal a block the widget layer already expanded. Code-block content stays the literal source — language-specific syntax highlighting inside fenced blocks is a separate feature, not part of live preview. [live-preview-code-fence-block-reveal]
status:: done
touches:: [[code:hiker/styling]]
note:: per-block reveal: `block_active = on_cursor_line(range) OR selection_intersects(range)` — cursor anywhere inside the block OR a non-empty selection overlapping it keeps the fence delimiters visible. BUGFIX: the branch previously keyed on `on_cursor_line` only, so highlighting a fenced block (incl. a rendered Mermaid/WaveDrom widget's source, caret head past the closing fence) collapsed the ` ``` ` delimiters even though the widget layer revealed the body — a half-reveal. The `markdown` decoration cache now keys on `sel_fp` (was `cursor_line` only) so the selection-aware reveal invalidates. Regression: `styling.rs::selecting_code_fence_reveals_delimiters` · evidence: `editor/editor-md/src/styling.rs` (`style_fenced_code_block`, `selection_intersects`)
- **Blockquotes (`>`).** The `>` marker is *always* visible. It's a useful structural cue and fading it (turning quoted text into bare-but-italic prose) is more disorienting than helpful. The text after the marker can pick up subtle styling (muted color, italic) but the marker itself stays. [live-preview-block-markers-keep]
status:: done
touches:: [[code:hiker/styling]]
note:: blockquotes and lists are intentionally not visited; their markers render as raw source — no fade emitted
- **Lists.** Bullets (`-`/`*`/`+`) and numbered prefixes (`1.`, `2.`) are *always* visible. Same reasoning as blockquotes — they're structural, not decorative. v1 does not prettify bullets to `•` glyphs; the literal marker is what shows. [live-preview-block-markers-keep]
- **Horizontal rules (`---`).** v1 leaves them as literal text on a styled line (subtle muted color). Tier 2 will replace them with an actual rule decoration.
- **Tables.** v1 leaves them as literal source. Tables read tolerably as monospaced pipe-and-dash text, and a real table widget is Tier 2 work.
- **Frontmatter.** The `---` block at the top of a `.md` file is parsed by the editor's markdown parser and rendered as a styled-but-plain block at **body size** (not heading size) using the configured code font (per [[spec:editor-three-fonts]] in `editor.md`), with muted color. No marker fading; no key/value parsing in this layer. The body-size + code-font rule is what keeps a YAML frontmatter block from rendering as a tall stack of heading-sized lines when the parser's default heading-level inference would otherwise apply. [live-preview-frontmatter-passthrough] [editor-frontmatter-rendering-fix]
status:: done
touches:: [[code:hiker/styling]]
note:: detects leading `---` … `---`/`...` block and applies frontmatter line decorations (muted, monospace). 200-line scan cap; no marker fading; no kv parsing — all per spec · evidence: `editor/editor-md/src/styling.rs` (frontmatter detection in `markdown_decorations()`)

[editor-frontmatter-rendering-fix]
status:: done
touches:: [[code:hiker/styling]]
note:: frontmatter renders at **body** size in the code font — no longer leaks heading-size styling from a Setext-H2 misparse; closing-`---` scan capped at 1000 lines, unterminated blocks are a no-op · evidence: `editor/editor-md/src/styling.rs` (`detect_frontmatter_range`, monospace mark over the leading YAML range)

- **Fenced code blocks — syntax highlighting.** Code inside fenced blocks is syntax-highlighted by the language named on the opening fence (` ```rust `, ` ```py `, …). Highlighting renders as a styling layer over the literal source — the block's reveal behavior, monospace background, and the live-preview fence-marker rule above are unchanged; only the per-token color is added. Engine: `tree-sitter-highlight` (pure-Rust). Adding a language is cheap — one workspace dep + one registration in `editor-md::syntax`. Unrecognized info-string languages render the block as plain monospace (no error, no fallback guess). When `[code-source-ingest]` lands (per `ideas.md`), the same highlighter applies to code files opened as editor buffers, gated on the file's extension. [editor-code-syntax-highlight]

[editor-code-syntax-highlight]
status:: done
touches:: [[code:hiker/styling]], [[code:hiker/syntax]]
note:: per-language syntax highlighting inside fenced code blocks via `tree-sitter-highlight`. Languages shipped: rust, python, typescript, javascript, bash, json, toml, yaml, markdown, sql. Pure styling overlay; reveal / monospace background / fence-marker rules unchanged. Same highlighter applies to code-source files once [[spec:code-source-ingest]] lands. Unknown info-string → plain monospace, no error · evidence: `editor/editor-md/src/syntax.rs` (`tokenize_block`), `editor/editor-md/src/styling.rs` (`style_fenced_code_block` call site)


## Implementation

The live-preview decorations are produced by a decoration provider in the `editor-md` crate and aggregated onto the editor view's decoration set: a pure `&Editor → Set` function keyed off the markdown syntax tree, recomputing the "active lines" set when the selection or document changes and flipping the marker-hidden ranges on/off (rebuild scoped to changed ranges, not the whole doc). Styling is theme-token-driven so light/dark themes both work without a parallel stylesheet.

The providers live in the `editor-md` crate (e.g. `editor/editor-md/src/styling.rs`, `links.rs`), wired into the buffer view in `app/src/panels/buffer/mod.rs`.

**Toggle wiring.** [[spec:view-live-preview-toggle]] (already reserved in `editor.md`) becomes live with this spec. The View menu entry is no longer greyed; clicking toggles whether the live-preview decorations are applied. State is in-memory only in v1, per the View options menu's persistence rule.

**Non-markdown gating.** Live preview only applies when the buffer is rendered as markdown (per `editor.md` and `txt-ingest.md`); when it isn't, the live-preview provider emits no decorations.


## Deferred

- **Tiers 2 and 3** (block widgets; media widgets, the latter in `editor-widgets.md`) — see "Scope: tiers" above.
- **Setext-style heading marker fade.** Skipped in v1 because per-line reveal misbehaves on multi-line markers; revisit if real content uses them.
- **Bullet-glyph prettification** (`-` → `•`). Out of v1 to keep "what you typed" visually present. Easy to add later as a styling-only change.
- **Per-vault / per-user persistence of the toggle.** Lands with `settings.md`'s editor section.


## Out of scope

- Wikilink rendering — owned by the future wikilinks extension.
- Anything that mutates the source file (live preview is decoration-only by definition).
- Spellcheck visuals, grammar squiggles, comment threads — separate extensions if they ever land.
- "Source mode" — the toggle off-state already serves this purpose; we don't need a third mode.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **live-preview-cursor-line-reveal** — active-lines set rebuilt per selection change; the active-range check matches by line number first [live-preview-cursor-line-reveal]
  status:: done
  touches:: [[code:hiker/styling]]
  note:: evidence: `editor/editor-md/src/styling.rs` (`markdown_decorations()` active-range check)
