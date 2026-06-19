# Style

Hiker's global visual language: the design tokens and component-styling rules every surface follows. One crate (`hiker-theme`) owns the egui `Style`/`Visuals`; surfaces read tokens from it rather than hardcoding colors, spacing, or borders.

The headline decisions:

- **One theme owns the tokens.** `hiker-theme` installs the egui `Style`/`Visuals` at startup and exposes the named color helpers (`accent`, `divider`, `muted`, `warn`, `active_bg`, `hover_bg`). Every surface reads from there; ad-hoc hardcoded colors/borders are the exception, not the norm. The crate depends only on `egui` so the app and the companion crawler render identically. [style-theme-install]
status:: done
note:: single install point for the egui `Style`/`Visuals` + named color tokens; egui-only dep so app + crawler match · evidence: `hiker-theme/src/lib.rs` (`Theme::install`, color helpers)
- **Light palette, single blue accent.** A light surface palette (`window`/`panel`/`faint`/`extreme` greys) with one blue accent used for selection, focus, links, dirty markers, and active emphasis. [style-palette]
status:: done
note:: light surface greys + one blue accent; spacing `(6,4)`/`(8,4)`; body 14 / mono 13 · evidence: `hiker-theme/src/lib.rs`
- **Ghost buttons.** Buttons carry no background or border at rest; hover/press/selected reveal state. Applied globally through the theme's widget visuals. See "Buttons". [style-ghost-button]
status:: done
touches:: [[code:hiker/widgets/split_button]]
note:: buttons have no bg/border at rest; hover reveals `hover_bg` + 1px `divider`; press `active_bg` + accent; selected shows accent. Global via the widget visuals; the split-add control follows · evidence: `hiker-theme/src/lib.rs` (`widgets.inactive/hovered/active`), `app/src/widgets/split_button.rs`
- **One small corner radius.** Buttons and button-like controls round to a single shared radius token. See "Buttons". [style-button-radius]
status:: done
touches:: [[code:hiker/widgets/split_button]]
note:: one shared 5px radius; `ImageButton` opts in via `.corner_radius` since its rounding comes from the image · evidence: `hiker-theme/src/lib.rs` (`BUTTON_CORNER_RADIUS`, `widgets.*.corner_radius`), `app/src/widgets/split_button.rs`
- **Active-over-inactive emphasis.** Where a strip shows one active item among peers (the activity bar), the active item reads at full strength while inactive peers grey out. See "Activity bar". [style-activity-emphasis]
status:: done
touches:: [[code:hiker/activity_bar]]
note:: active panel icon full-strength + accent rail/bg; inactive icons greyed to the weak-text color · evidence: `egui-workbench/src/activity_bar.rs` (`paint_activity_item`)

## Tokens

`hiker-theme` exposes these as `const fn` color helpers ([style-theme-install], [style-palette]):

| Token | Value | Used for |
| ---- | ---- | ---- |
| `accent` | blue `#2f6fed` | selection fill/stroke, focus, hyperlinks, dirty markers, active rail |
| `divider` | `#d6dae0` | panel/section dividers, the tab strip, hover borders |
| `active_bg` | `#e2e8f0` | active tab / selected row / pressed button fill |
| `hover_bg` | `#eaeef4` | hover background tint |
| `muted` | `#6a737d` | secondary labels (vault path, status bar), greyed icons |
| `warn` | amber `#c48600` | inline warning glyphs + matching text |
| panel greys | `window #fafbfc`, `panel #f4f6f8`, `faint #eceff3`, `extreme #ffffff` | surface backgrounds |

- **Spacing.** `item_spacing = (6, 4)`, `button_padding = (8, 4)`. Body text 14px, monospace 13px (editing wants more readability than chrome). [style-palette]
- **Selection / focus.** The accent at ~44% opacity fills selections; its solid form strokes focus rings and the selection border. Every text input and the editor read this one token. [style-selection]
status:: done
note:: accent @44% selection fill, solid accent stroke + links/focus · evidence: `hiker-theme/src/lib.rs` (`selection.bg_fill`/`stroke`, `hyperlink_color`)

## Buttons

The default button is a **ghost button**: transparent `weak_bg_fill` and no `bg_stroke` in the `inactive` widget state, so at rest it shows only its icon/label. [style-ghost-button]

| State | Background | Border |
| ---- | ---- | ---- |
| rest (`inactive`) | none (transparent) | none |
| hover (`hovered`) | `hover_bg` | 1px `divider` |
| press (`active`) | `active_bg` | 1px `accent` |
| selected/toggle | accent selection fill | accent |

- **Global, not per-call.** The rule lives in `hiker-theme`'s `Visuals.widgets`, so every control that paints from the interact visuals (text buttons, icon buttons, the split-add control) follows it without per-site styling. A site that needs emphasis opts in explicitly (see [style-filled-button], deferred). [style-ghost-button]
status:: planned
note:: opt-in prominent/filled button variant (accent fill, light text) for a per-screen primary call-to-action
- **Corner radius.** Buttons round to one shared token (`BUTTON_CORNER_RADIUS`, 5px). Text buttons inherit it from the widget visuals; image buttons (whose rounding comes from the image) opt in explicitly. [style-button-radius]
- **Split-add control.** The `+`-with-dropdown split-button ([[spec:split-add-button]] in `files.md`) is ghost too: at rest just the `+` and caret glyphs; on hover the engaged half fills, a seam divides the two halves, and a border wraps the control. [style-ghost-button]
- **Icon tint.** A button's icon/label uses the interact state's foreground color, so it dims with the widget on disable and stays legible on the hover fill. [style-ghost-button]

## Activity bar

The vertical activity strip shows one active panel among its peers. [style-activity-emphasis]

- **Active item:** full-strength icon (the normal foreground color), an accent leading-edge rail, and a faint accent background — unchanged emphasis.
- **Inactive items:** the icon (or its text-glyph fallback) is tinted to the **weak-text color** so it reads as greyed-out, making the active item pop. Hover still lifts an inactive item's background for affordance. [style-activity-emphasis]

## Deferred

- **Dark theme.** A dark palette swapped behind the same token names, selectable from settings; every surface already reads tokens so the swap is a `hiker-theme` change. [style-dark-theme]
status:: planned
note:: dark palette behind the same token names, settings-selectable; one `hiker-theme` swap since surfaces read tokens
- **Filled/prominent button variant.** An opt-in emphasis style (accent fill, light text) for a primary call-to-action that should read as filled rather than ghost, applied per-site where a screen needs a clear primary. [style-filled-button]
- **Themeable accent.** User-chosen accent hue, recolored through the `accent` token. [style-accent-config]
status:: planned
note:: user-chosen accent hue recolored through the `accent` token

## Out of scope

- **Per-widget bespoke styling.** Surfaces don't ship their own palettes; a one-off color is a smell that the token set is missing an entry — add the token instead.
- **Editor content theming.** Markdown live-preview / syntax colors are the editor's `Theme` (`editor-core`), fed the palette but specced in `editor.md` / `live-preview.md`, not here.
