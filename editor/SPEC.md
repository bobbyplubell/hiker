# egui_editor — Product Spec

A reusable code + markdown editing **widget** for Rust UI. Primary host: egui. Modeled on
modern decoration-based editors in capability and extensibility. Built for a note-taking app with a native
Rust UI.

This document is the **user-facing requirements** — what the editor does, not how it is
built. The how lives in `IMPLEMENTATION.md`.

---

## 1. What this is

A drop-in editing widget that a host application embeds. The widget owns text editing —
input, layout, decorations, undo, multi-cursor, syntax highlighting, markdown live preview,
diff display. The host owns everything around it — files, tabs, settings UI, LSP wiring,
command palette.

It is **not** an editor application. It is the embeddable engine that an editor
application would be built on.

---

## 2. Editing capabilities

### 2.1 Core editing
- Insert, delete, replace text via keyboard and paste.
- Cut / copy / paste with system clipboard.
- Undo / redo with intelligent coalescing (typing merges; typing+paste breaks the group).
- Undo *tree* — branching history, not just a linear stack. Can navigate back to a previous
  branch after diverging.
- Time-based undo: jump back N seconds/minutes, not just N steps.

### 2.2 Selection
- Click, click-drag, shift-click for range selection.
- Double-click selects word; triple-click selects line.
- Keyboard selection (shift+arrow, shift+word, shift+line, shift+page).
- Selection survives edits (anchors stay attached to logical positions).

### 2.3 Multi-cursor
- Add cursor at click point with Ctrl/Cmd-click.
- Column / box selection with Ctrl-Alt-drag and Ctrl-Alt-Up/Down.
- "Add next occurrence" of the current selection (Cmd-D style).
- "Add all occurrences" of the current selection.
- All cursors edit, move, and delete in sync; overlapping cursors merge.

### 2.4 Selection-occurrence highlighting
- When a non-empty word/range is selected, every other occurrence of that text in the
  visible viewport is highlighted with a subtle background tint (the VSCode behavior).
- Only scans the visible region — cheap on large files.

---

## 3. Display & decorations

### 3.1 Syntax highlighting
- Per-language colorization driven by pluggable language adapters.
- Tree-sitter is the v1 reference implementation.
- Highlighting is incremental: edits reparse only the affected region.
- Languages can be injected (e.g. ```rust``` inside a markdown file gets Rust highlighting).

### 3.2 Inline text styling (decorations)
- Bold, italic, strikethrough, underline (with variants: solid, dotted, wavy).
- Foreground and background colors per range.
- Per-range font size and font family override.
- Letter-spacing override.
- Marks compose: bold + italic = both.

### 3.3 Per-row styling
- Background color per line (e.g. current-line highlight, diff-hunk shading).
- Per-line height scale (a heading row renders taller).
- Gutter markers per line (diff added/removed/modified, breakpoints, error markers).
- Indent visualization.

### 3.4 Inline widgets
- Custom widgets that occupy inline space within a row (e.g. a checkbox glyph, a colored
  badge, a fold-region pill).
- Widgets are measured by the editor and laid out among the glyphs.
- A widget may opt to render as plain styled text via the `display()` trait method
  (returning text + bg/fg/strikethrough). The painter renders the supplied text as the
  segment's galley with the bg fill instead of the bordered placeholder. Used by hosts
  that want a small textual insertion at a byte position (patch-review's intraline
  `new_str`, future inline diagnostics, etc.) without introducing a new decoration variant.
- End-of-line widgets — an inline widget whose range starts at the line's last byte
  (i.e. at the newline / EOF position) renders as a trailing segment after the line's
  text rather than being clipped to zero width. Lets a host append visible text past
  the line's final character without a Block widget.

### 3.5 Block widgets
- Widgets that occupy a full row of their own above or below a buffer line (e.g. an inline
  image, a diff-context expander, an embedded chart).
- Block widgets supply their own height.
- Pure-data action-row variant — `BlockKind::ActionRow { label, glyph, tone, buttons }`
  renders a thin horizontal strip with a label on the left and a row of clickable
  buttons on the right. Each enabled button registers a `ClickAction::WidgetClick(id)`
  on its own rect. Used by patch-review for the per-hunk Accept / Reject row and the
  unanchored-pin Reject row; available to any host that wants a generic button-bar block.

### 3.6 Folding / replace
- Hide a range of text behind a placeholder widget (the standard code-folding behavior).
- Cursor motion treats a folded region as a single step.
- **Expansion controls** (the "dropdown thingies" common in code/note editors): clickable
  chevron / triangle markers in the gutter (or inline) that toggle a fold
  open/closed. Two specific shapes the widget must support:
  - **Per-language fold regions**: code blocks/braces/markdown sections that the
    user can collapse. Chevron in the gutter.
  - **Diff context expansion**: at the boundary between a shown hunk and
    elided unchanged context, an inline "▼ N unchanged lines" affordance that
    reveals the hidden context when clicked (and a corresponding "▲" to recollapse).
    VSCode shows these between hunks; note editors use similar dropdowns for
    collapsible sections.

Both shapes are the same underlying primitive: a fold model that owns a set of
`(range, collapsed: bool)` entries, surfaced as `Replace` decorations when
collapsed and as a chevron widget at the boundary regardless of state.

### 3.7 Gutter
- Line numbers (optional, configurable: absolute / relative / both / off).
- Per-line marker column for breakpoints, diagnostics, diff status.
- Fold-toggle controls.

### 3.8 Soft line wrapping
- First-class capability, on/off per editor instance (and per document at the host's discretion).
- When on, long lines wrap visually at the widget width. Buffer line indexes are preserved;
  visual lines (VLines) are addressable separately for cursor up/down.
- Reflows on width changes and on font-size changes without invalidating selections.
- Wrap-aware cursor motion: up/down moves by visual line; home/end snap to visual line edges
  by default with a configurable "logical line" mode.
- Off by default for code mode; on by default for markdown.

### 3.9 Atomic ranges
- Any decoration may declare itself "atomic" — cursor motion treats the range as a single
  step (skipping over its interior). Folded regions are the canonical example, but inline
  widgets (bullet glyphs, task checkboxes, link chips, wikilink badges) also want this.
- Lifted out of folding so any extension can opt in via a flag on `Mark` / `Replace` / widget
  decorations.

### 3.10 Inline widget event semantics
- Widgets declare whether mouse events pass through (cursor lands on the underlying buffer
  position) or are consumed (widget handles click/drag, e.g. checkbox toggle).
- Widgets supply a hit-test method so non-rectangular widgets (chips with rounded corners)
  can pass through their padding area.

---

## 4. Markdown live preview

When the markdown language extension is enabled, the editor renders source markdown with
inline formatting *applied* while keeping the source text editable.

Live-preview behaviors:

- **Headings** render at heading sizes (h1 largest, h6 base). The `#` markers are hidden
  on lines where the cursor is not present, shown when editing that line.
- **Bold / italic / strike** render with the styling applied. Surrounding `**` / `*` / `~~`
  are hidden when the cursor is not on the line.
- **Inline code** renders monospace with a tinted background; backticks hidden when not on the line.
- **Links** render as colored, underlined text; the `[text](url)` syntax collapses to just
  the text when the cursor is elsewhere.
- **Lists** (unordered and ordered): the marker character (`-`, `*`, `1.`) is replaced by
  a bullet/number glyph widget, with proper indentation. Nested lists indent visually.
- **Task lists** (`- [ ]`, `- [x]`): rendered as a clickable checkbox widget.
- **Blockquotes** render with a left bar and indented text.
- **Code fences** render with a tinted background block and syntax highlighting for the
  fenced language (uses injection).
- **Horizontal rules** render as a thin horizontal line.
- **Images** render as inline or block image widgets (with the source text hidden until cursor is near).

### 4.0 Wiki-style extensions (v1)
- **Wikilinks**: `[[Page]]` and `[[Page|alt]]` render as chips (Inline widget) with the
  page title styled, target hidden. Click → host emits a `WikilinkClicked(target)` event.
- **Transclusion**: `![[Page]]` and `![[Page#section]]` render as block-embed widgets;
  the host supplies the rendered content.
- **Callouts**: `> [!note]`, `> [!warning]`, `> [!tip]` etc. — render as styled blockquote
  variants with an icon + colored left border per type.
- **Footnotes**: `[^1]` inline becomes a superscript chip; `[^1]: …` definition gets dimmed
  styling. Hover (Tooltip §10.2) shows the definition inline.
- **Frontmatter folding**: YAML `---…---` at the top of the document gets a `Frontmatter`
  fold region, collapsed by default.

### 4.1 Tables
- GFM pipe-table syntax supported.
- Tables render as proper table widgets when not being edited — aligned columns, optional
  header styling.
- Cursor entering the table region transitions back to source view for editing, or stays
  in a structured cell-edit mode (TBD which feels better — see open questions in
  IMPLEMENTATION.md).
- Wiki-style table extensions: cell alignment markers, simple column-resize cues.

### 4.2 Live-preview UX rules
- "Reveal source on cursor line" is the universal rule: whichever line the cursor is on
  shows raw markdown; everywhere else shows the rendered form. This is the inline live-preview
  Live Preview mode, and it should be the default. A pure-source-view mode and a
  pure-render-view mode are both available toggles.

---

## 5. Diff view

The widget can render two versions of a document as a diff. Two display modes, both
backed by the same diff engine:

### 5.1 Side-by-side
- Two editor panes, scroll-locked.
- Removed lines on the left, added lines on the right, unchanged context aligned across
  both panes.
- Each pane is a fully functional editor view (can be made read-only).

### 5.2 Unified (inline) with intraline emphasis
- Single editor pane.
- Removed lines shown above their replacement, tinted red; added lines tinted green.
- Within a modified line, the specific characters that changed are emphasized with a
  stronger background — the VSCode intraline-diff behavior.

### 5.3 Diff features
- Expand / collapse unchanged context, with the chevron / "▼ N unchanged lines"
  affordance described in §3.6.
- Per-hunk "accept" / "reject" controls (the host wires these up; the widget provides the
  click affordance via block widgets in the gutter).
- Works on arbitrary text pairs, not just files (e.g. show diff of an AI edit suggestion).

---

## 6. Search & navigation

- Search-in-buffer (regex optional).
- Replace and replace-all (respecting multi-cursor / selection scope).
- "Highlight all matches" of the search query — same engine as selection-occurrence
  highlighting.
- Jump to line / position by API.

---

## 7. Input handling

### 7.1 Keyboard
- Configurable keymap stack: defaults → language-specific → user overrides.
- Chord support (e.g. `Ctrl-K Ctrl-F`).
- A command system that any extension can register into.

### 7.2 IME (international input)
- Full support for IME composition (CJK input, dead keys, accent composition, emoji picker).
- Preedit text renders inline at the cursor with an underline; commits as a normal edit.
- IME is **not** hardwired to any windowing framework — the host adapts platform IME
  events into the widget's input pipeline. The egui adapter ships a default translator.

### 7.3 Mouse / touch
- Click to position cursor; drag to select.
- Word/line select on double/triple click.
- Scroll via wheel, touchpad, or scrollbar.
- Hover events exposed for tooltip-style extensions.
- **Drag-and-drop of selected text**: drag from inside a selection moves the text (or
  copies with modifier) within the same document; drag out is surfaced as a host event.

---

## 8. Performance expectations

- Files up to ~10 MB / ~100k lines edit smoothly (typing, scrolling, syntax highlighting).
- Decoration updates do not relayout the entire document — only affected lines.
- Scrolling is virtualized: only visible rows pay layout cost.
- Undo memory stays bounded across long sessions (snapshots share storage; we do not clone
  the whole document per edit).
- Soft-wrap reflow on width change is incremental — only rewraps visible lines + a margin
  on either side; offscreen lines lazily rewrap as they enter the viewport.
- The minimap (§9.23) costs O(1) draw work per frame regardless of document length: it
  rasterizes to an offscreen image once and re-rasterizes only on content / decoration /
  theme / size change, never on scroll. Enabling it must not regress scroll smoothness.

---

## 9. Extensibility surface

A host adds capability by composing **extensions**. An extension can:

- Register a **language** (parser + highlight rules + indent rules + bracket matching).
- Register **decoration providers** (functions that read state and emit styled ranges).
- Register **state fields** (extension-owned data that updates in response to transactions).
- Register **commands** and bind them in the keymap.
- Filter or extend transactions before they apply (e.g. read-only mode, auto-indent).
- Observe the viewport (the visible range) for viewport-scoped work like search highlighting.

Extensions are pure data + functions — no global state, no UI side effects outside what
the widget renders. Live reconfiguration (swap language, swap theme) is supported.

---

## 9.5 Tooltips
- The widget ships a tooltip primitive: extensions register `Tooltip { anchor, content,
  placement }`, the widget handles positioning, viewport clipping, and dismiss-on-blur.
- Positioning modes: above / below / smart (avoid clipping), with arrow indicator.
- Content is a small render callback that draws into a sub-painter, so extensions can put
  arbitrary egui UI (buttons, lists, code snippets) inside.
- Use cases: hover for diagnostic messages, hover for footnote definitions, completion popup.
- In egui (immediate mode) this is non-trivial — done once in the widget instead of
  re-solved by every extension.

## 9.6 Autocomplete
- Pluggable provider model: a `CompletionSource` produces `CompletionItem`s for a given
  state + cursor position. Sources are facets, layered (snippets + language + wikilinks).
- The widget owns the popup UI (a Tooltip variant), selection navigation, and commit.
- Triggers: explicit (Ctrl-Space) and contextual (typed character matches a source's
  trigger characters, e.g. `[[` for wikilinks, `.` for member access).
- v1 ships the framework + a wikilink source for the markdown extension. Real LSP-style
  completion is the host's job (the host registers a `CompletionSource` that calls LSP).

## 9.7 Diagnostics / linting
- Typed `Diagnostic { range, severity, message, source }` produced by `DiagnosticProvider`
  facets.
- Renderings:
  - Gutter marker (severity icon) per affected line.
  - Squiggly underline mark over the range (Mark decoration with `wavy` underline style).
  - Tooltip on hover showing the message.
- Diagnostics survive edits via Anchors; outdated diagnostics collapse and disappear.
- v1 ships the types + UI; the host feeds them in (from LSP, from tree-sitter queries, etc.).

## 9.8 Auto-close / auto-pair / snippets
- **Auto-pair**: typing an opening bracket/quote inserts the matching close and positions
  the cursor between them. Configurable per language. Implemented as a transaction filter,
  so it composes cleanly with multi-cursor and undo.
- **Skip-over-close**: typing the close char right before its auto-inserted match advances
  the cursor instead of inserting.
- **Snippets** (post-v1): `${1:placeholder}` syntax with tab-stop navigation. Snippet
  expansion produces a transaction; tab-stops live in a `SnippetField`.

## 9.10 Bracket matching
- When the cursor is adjacent to one of `()[]{}` (and a few language-extensible
  others), the matching partner is highlighted via a `Mark` with a thin border
  or bg tint. If no match exists in the current scope, both unmatched
  brackets are highlighted in a warning color.
- Match search is bounded (default ~10k chars) so it stays cheap on big files.

## 9.11 Active-line highlight
- The buffer line containing the main cursor receives a subtle bg tint.
- When multiple cursors are active, each cursor's line gets the tint.
- Off by default; configurable per editor.

## 9.12 Placeholder text
- A configurable "Type something…" string renders dimmed when the document is
  empty. Disappears as soon as the doc has any content.

## 9.13 Search panel
- Built-in find / find-and-replace panel docked at the bottom of the editor
  area when active. Triggered by Cmd-F / Cmd-Alt-F. Closed via Escape.
- Features: case sensitivity toggle, whole-word toggle, regex toggle,
  in-selection-only toggle. Up/Down to navigate matches. Enter to commit a
  replace. Cmd-Alt-Enter for replace-all.
- All visible matches highlighted; current match has a stronger highlight.

## 9.14 Indent-on-input
- Pressing Enter inside a markdown list item (`-`, `*`, `1.`, `- [ ]`)
  continues the list with the same indent + marker. Pressing Enter on an
  *empty* list item exits the list (removes the marker).
- For code: per-language indent rules (default: copy previous line's leading
  whitespace; +1 indent after `{`, `:`, `(` at end of line). Pluggable as a
  `transaction_filter` extension.
- Tab inserts indent (tabs or N spaces, per editor config) at the start of
  the line; in the middle of a line it acts like a regular Tab. Shift-Tab
  removes one indent step at line start.

## 9.15 Rectangular / column selection
- Alt-click + drag (or Cmd-Alt-drag on macOS) creates a rectangular
  selection: a column of cursors with matching start/end rows.
- Visually a rectangle; logically N cursors, one per row.
- Cmd-Alt-Up / Cmd-Alt-Down still works as the keyboard equivalent (already
  shipped).

## 9.16 Special-character rendering
- Optional rendering of normally-invisible characters: tabs as `→`, spaces
  as `·` (when toggled), zero-width characters as `·`, NBSP as a tinted box,
  CRLF as `↵`. Configurable per category.

## 9.17 Trailing-whitespace highlight
- Trailing whitespace on each line is rendered with a tinted bg so it's
  visible. Toggle via config.

## 9.18 Scroll past end
- When enabled, the editor allows scrolling so the last line can sit
  anywhere from the bottom of the viewport up to (configurable) the middle.
  Makes editing the end of a long doc less cramped.

## 9.19 Drop cursor indicator
- During a text drag (SPEC §7.3), a thin vertical bar renders at the would-
  be drop position as the cursor moves. Helps the user see where the moved
  text will land before releasing.

## 9.20 Themes as data
- A `Theme` is a typed extension that supplies a palette and a set of token
  → color mappings. Hosts can register multiple themes and switch via a
  compartment without rebuilding extensions.
- Bundled themes: `light_default`, `dark_default`. Hosts can register their
  own with arbitrary color tokens (e.g. for matching the parent app).
- All built-in extensions (markdown, diff, diagnostics, …) consume colors
  through the active theme rather than hardcoding RGB.

## 9.21 Panels (top / bottom UI strips)
- The widget reserves slots for UI strips above and below the text area.
  Extensions register `Panel` providers; the search panel (§9.13) is the
  first consumer. Future: status bar, info bar, breadcrumb.
- Each panel declares a height and a render callback; the widget stacks
  them and adjusts the text viewport accordingly.

## 9.22 Snippet expansion
- Snippets are parameterized text templates: `for $1 in $2:\n  $0`. After
  expansion, the cursor lands at `$1`; Tab cycles to `$2`, then `$0` (final
  position).
- `${1:placeholder}` syntax provides default text the user can replace.
- Each tab stop is an active range with multi-cursor; editing all stops with
  the same number updates them in sync (mirror).
- Triggered by a `CompletionItem` of kind `Snippet` or via direct API.

## 9.23 Minimap
- A narrow strip docked at the side of the editor that mirrors the whole
  document, with a translucent thumb marking the slice currently visible in
  the viewport. Click or drag the strip to scroll there; the wheel over the
  strip scrolls the document. Off by default; the host toggles it and owns
  its width.
- Two render **styles**, selectable by the host:
  - **Glyphs** — a literal scaled-down view of the text: one cell per
    character, each tinted by the same syntax/decoration color the editor
    paints that span with. It renders from the editor's *display* model, so
    it mirrors live preview (hidden markdown markers, heading styling, list
    bullets/checkboxes) and **soft-wrap** — a line that wraps into three rows
    in the editor wraps into three rows here. Scaled uniformly so wrapped
    rows fit the strip width at true aspect (a short doc occupies the top of
    the strip rather than stretching to fill it). This is the default — it
    reads as a true miniature of the page.
  - **Bars** — a structural abstraction: one bar per line, width set by the
    line's visible (non-whitespace) length and color by its structural role
    (heading / code / quote / emphasis / plain), derived from decoration
    layers. Denser and more schematic than glyphs.
- Both styles share one projection with the editor: soft-wrapped lines take
  proportionally more height, headings scale up, folded/hidden lines vanish —
  so the strip and the thumb stay in lockstep with what's on screen.
- Lines touched by a non-empty selection, and (when find is active) search
  matches, are marked along the strip.
- Classification and color come entirely from the decoration layers the
  editor already paints from, so any provider the host wired up (markdown,
  diff, search…) participates automatically — the minimap reads decorations,
  never produces them.
- **Performance is a requirement, not a tuning detail**: per-frame cost is
  independent of document length. The strip is rasterized once into an
  offscreen image and redrawn as a single textured quad; it only re-rasterizes
  when the document, decorations, theme, or strip size change — never on
  scroll. (See §8 and IMPLEMENTATION §16.6.18.)
- Host-configurable: style, width, palette, and the per-feature toggles
  (viewport thumb, section rules, left edge). The renderer is swappable — a
  host could supply an alternative strip without touching the editor core.

## 9.9 Serialization
- `EditorState` and individual `StateField`s are `Serialize`/`Deserialize` (serde) so the
  host can persist editor sessions and restore them.
- Per-field opt-in: each `StateField` declares whether it's serialized (e.g. the History
  field can be omitted to save space; fold state should be persisted).
- Anchors round-trip; selection and fold state survive a save/load.

## 10. Out of scope (v1)

- Bidirectional text (RTL languages). LTR-only in v1.
- Accessibility / screen-reader integration (AccessKit) — possible later behind a feature flag.
- Collaborative editing (CRDT sync). The internal model is friendly to adding it later but
  it is not wired.
- LSP, file watching, project management — these belong to the host.
- Terminal / TUI backend.
- Macro recording.
- **Math rendering** (KaTeX/MathML) — primitives exist via block widgets; bundling a math
  renderer is out of v1 scope. The widget detects `$…$` / `$$…$$` regions and ships
  visual placeholder styling; rendering math glyphs / TeX output is the host's
  responsibility (typically by registering a `BlockWidget` that calls into a TeX
  renderer like `mathemascii` or a WebView).
- **Diagram embedding** (Mermaid, PlantUML) — same: the widget detects ` ```mermaid `
  fences and applies visual styling; the host registers a `BlockWidget` to render
  the actual diagram (e.g. via `mermaid-cli` to SVG → egui image, or a JS bridge in
  a WebView). The widget does NOT bundle a Mermaid runtime — that would add a JS
  engine dep and is rightly a host concern in a native UI.
- **Snippet expansion with tab-stops** — primitives in place (transaction filter,
  decoration-based highlighted ranges) but the snippet syntax engine is post-v1.
- **LSP-driven completion / hover / go-to-definition** — the widget exposes the surface
  (completion source, tooltip, command); the host wires LSP up.

---

## 11. Defaults & themes

- Light and dark default themes, selectable.
- All colors and font choices configurable by the host.
- Default keymap is VSCode-flavored (the audience expects this); language extensions can
  override.

---

## 12. Backend portability

v1 ships an egui widget. The internal architecture keeps state, layout logic, and command
dispatch independent of egui, so an alternative backend (parley + wgpu, for example) could
be added later without rewriting the editor. See `IMPLEMENTATION.md` §11.

## 13. Architectural extension surface (host-facing)

These aren't end-user features but they're the host-facing API surface that
makes the extension surface feel familiar to extension authors.

### 13.1 Facet<Input, Output>
Typed multi-provider channels. An extension declares a facet (typed by its
input and output); other extensions register providers; the editor combines
them via the facet's `combine` function and exposes the result through
state. Required for: theme providers, completion sources (already shipped
as a Vec), tooltip providers, diagnostic providers, keymap layers, panel
providers.

### 13.2 StateField<T>
Extension-owned reactive state slot. Each field declares a `create` and
`update(state, transaction) -> T`. Replaces the current pattern where
features like history, folds, and IME state live as named fields on
`EditorState`/`ViewState`.

### 13.3 ViewPlugin
A per-view stateful plugin that gets `create(view)` once and `update(view,
transactionsSinceLastUpdate)` per frame. Used for things like
tooltip-reposition-on-selection-change, lazy decoration computation, scroll
syncing.

### 13.4 Transaction hooks
- `changeFilter` — veto or reshape a `ChangeSet` before it applies (e.g.
  read-only ranges, max line length).
- `transactionFilter` — modify the whole `Transaction` before apply (e.g.
  auto-indent, paste sanitization).
- `transactionExtender` — add `StateEffect`s without changing the doc
  (e.g. analytics, telemetry annotations).

### 13.5 Tree-sitter language adapter
The `Language` trait already exists in spirit (parsers + indent + bracket
rules). The implementation surface adds a Tree-sitter binding crate
(`editor-ts`) with:
- Incremental reparse driven by `ChangeSet → InputEdit`
- Highlight token → theme color via `theme.tokens`
- Per-language fold detection from query files
- Indent detection from query files
- Bracket-match pair list per language

Bundles parsers as optional Cargo features per language. The host enables
the parsers it wants; the rest don't ship.

### 12.1 WebAssembly
The widget compiles to `wasm32-unknown-unknown`. `Instant` is provided by the `web-time`
crate (zero-cost on native; backed by `performance.now()` on wasm). The included demos use
`eframe` with `NativeOptions` — to ship a wasm build, swap that for `eframe::WebRunner`
in a `wasm-bindgen` entry point. The editor library itself needs no changes.
