//! Editor decoration assembly for the buffer panel: the per-frame
//! `rebuild_editor_decorations` driver that fingerprints and caches every
//! decoration layer, plus the app-side providers (chunk-boundary tints, the
//! index-diff gutter markers) that hang off `editor_core`'s `Editor` rather
//! than the panel's `BufCtx` — they're computed inside the `cached!` closures
//! in the rebuild, which already hold a `&mut buffer.decoration_cache` and so
//! can only borrow `buffer.editor` immutably. Pulled out of `buffer/mod.rs` to
//! keep that file under the workspace's per-file length cap; the panel calls
//! `decorations::rebuild_editor_decorations` through the widget's
//! decoration-rebuild hook.

use editor_md::admonitions::callout_decorations;
use editor_md::folds::fold_decorations;
use editor_md::notes::footnote_decorations;
use editor_md::meta::frontmatter_fold;
use editor_md::styling::markdown_decorations;
use editor_md::equations::math_decorations;
use editor_md::diagrams::{mermaid_decorations, wavedrom_decorations};
use editor_md::embeds::transclusion_decorations;
use editor_md::links::wikilink_decorations;
use editor_view::brackets::DEFAULT_BRACKETS;
use editor_view::highlight::occurrence_decorations;
use editor_view::brackets::bracket_match_decorations;
use editor_view::highlights::active_line_decorations;
use editor_view::highlights::trailing_whitespace_decorations;
use editor_view::whitespace::special_chars_decorations;
use editor_view::whitespace::SpecialCharsFlags;

use crate::buffer::DecorationCache;

use super::diff_overlay;
use super::find;
use super::widgets;

/// Buffer-derived inputs the decoration rebuild needs that are *not* the
/// editor state or view (those arrive as the widget hook's two args). Bundling
/// them keeps `rebuild_editor_decorations` under the `too_many_arguments` cap
/// while preserving identical behavior — every field is read exactly where the
/// old inline block read the matching `buffer.*` field.
pub(crate) struct DecoRebuildCtx<'a> {
    pub(crate) cache: &'a mut DecorationCache,
    pub(crate) folds: &'a std::collections::HashSet<u64>,
    pub(crate) loaded_text: &'a str,
    pub(crate) theme: Option<&'a editor_core::theme::Theme>,
    pub(crate) live_preview: bool,
    /// When true (default), the math/widget render layer replaces source with
    /// rasterized widgets; off shows the tinted-source marks
    /// (`widget-render-toggle`).
    pub(crate) render_widgets: bool,
    /// True when this buffer renders as markdown (`.md`, or `.txt` with the
    /// render-txt-as-markdown flag). Gates the widget provider independently of
    /// `live_preview` (`widget-render-gating`).
    pub(crate) is_markdown: bool,
    /// Device pixel ratio for the widget raster (`ui.ctx().pixels_per_point()`),
    /// captured before the rebuild closure runs. Part of the math-widget
    /// fingerprint so a DPI change re-renders (`widget-render-cache`).
    pub(crate) dpr: f32,
    /// Editor body font size in logical points; drives the math render size.
    pub(crate) font_px: f32,
    pub(crate) chunk_boundaries: bool,
    pub(crate) show_whitespace: bool,
    pub(crate) highlight_trailing_whitespace: bool,
    pub(crate) diff: Option<&'a diff_overlay::DiffOverlay>,
    /// Maps a wikilink target (ULID or name) to the note's current title for
    /// live-title rendering; `None` falls back to plain (non-clickable) link
    /// pills (read-only previews). status: wikilink-render-live-title
    pub(crate) resolve_title: Option<&'a editor_md::links::TitleResolver<'a>>,
    /// Persisted diagram-cache context (`<vault>/.hiker/diagram-cache`), or
    /// `None` when `[render] cache_diagrams` is off. Owned (it's just a
    /// `PathBuf`) so the rebuild closure doesn't have to borrow `app`; passed
    /// by reference into the math/mermaid/wavedrom widget providers, which read
    /// it below the in-memory `cached!` layer. status: widget-render-disk-cache
    pub(crate) diagram_cache: Option<widgets::disk_cache::DiagramCacheCtx>,
}

/// Rebuild every decoration layer for the editor against the *current* doc
/// state. Invoked through `EditorWidget::with_decoration_rebuild` so it runs
/// AFTER the widget applies this frame's input but BEFORE it measures heights /
/// paints — keeping marker-hiding / block decorations aligned with the
/// post-edit text (no one-frame live-preview flash per keystroke).
///
/// `editor` / `view` are the post-edit editor state + view the widget hands
/// back; everything else rides in `ctx`.
pub(crate) fn rebuild_editor_decorations(
    editor: &editor_core::state::Editor,
    view: &mut editor_view::viewport::ViewState,
    ctx: &mut DecoRebuildCtx<'_>,
) {
    let DecoRebuildCtx {
        cache,
        folds,
        loaded_text,
        theme,
        live_preview,
        render_widgets,
        is_markdown,
        dpr,
        font_px,
        chunk_boundaries,
        show_whitespace,
        highlight_trailing_whitespace,
        diff,
        resolve_title,
        diagram_cache,
    } = ctx;
    let theme = *theme;
    let resolve_title = *resolve_title;
    let diagram_cache = diagram_cache.as_ref();
    // Compute the visible byte range up-front so we can scope paint-only
    // providers to the viewport.
    let visible = view.visible_lines();
    let last_line = editor.doc.len_lines().saturating_sub(1);
    let visible_start = editor.doc.line_to_byte(visible.start.min(last_line));
    let visible_end_line = visible.end.min(last_line);
    let visible_end = if visible_end_line + 1 < editor.doc.len_lines() {
        editor.doc.line_to_byte(visible_end_line + 1)
    } else {
        editor.doc.len_bytes()
    };
    let visible_range = visible_start..visible_end;

    // Fingerprint inputs for memoized providers. `content_id` is an Arc
    // pointer into the rope tree — changes only on doc edits, so idle / pure
    // scroll frames hit the cache.
    let doc_id = editor.doc.content_id() as u64;
    let sel = editor.selection.main().head.offset() as u64;
    // Layers whose only cursor dependence is "is the cursor on this line?"
    // (markdown reveal, wikilink reveal) key on the line index instead of
    // the byte offset — otherwise a selection drag busts the cache on every
    // byte and reparses the whole doc per frame.
    let cursor_line = editor.doc.byte_to_line(sel as usize) as u64;
    // Full multi-cursor selection fingerprint: the widget layer reveals per
    // exact cursor/selection (delimiter-inclusive, all ranges), so a per-line
    // key like `cursor_line` would miss a cursor moving within the same line
    // into/out of a span. Order-independent XOR over every range's [start,end).
    let sel_fp: u64 = {
        let mut h: u64 = 0;
        for r in editor.selection.ranges() {
            let packed = (r.start() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (r.end() as u64).rotate_left(32);
            h ^= packed;
        }
        h
    };
    // Inlined `folds_hash`: XOR-mix the fold ids in an order-independent
    // way. Cheap and stable for memoization keys (HashSet iteration order
    // isn't deterministic).
    let folds_id: u64 = {
        let mut h: u64 = 0;
        for &id in folds.iter() {
            h ^= id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        h
    };
    let vp_lo = visible_start as u64;
    let vp_hi = visible_end as u64;
    let vp_fp = mix(vp_lo, vp_hi);

    crate::profile_scope!("rebuild decorations");
    view.decorations.clear();

    // Per-layer caching follows the same shape everywhere: gate on a flag,
    // mix a fingerprint, either reuse the cached `Set` or rebuild
    // it via the supplied closure, then push (optionally with heights for
    // layers that emit Line decorations the heightmap needs to see).
    //
    // `cached!(slot, fp, build, heights?)` keeps the per-layer code to a
    // single line each. `heights` is the optional fourth arg — when present,
    // the layer goes through `push_with_heights`; otherwise plain `push`.
    macro_rules! cached {
        ($slot:ident, $fp:expr, $build:expr) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            view.decorations.push(v);
        }};
        ($slot:ident, $fp:expr, $build:expr, heights) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            view.decorations.push_with_heights(v);
        }};
        // Viewport-scoped paint-only layers: rebuilt fresh each frame with
        // `vp_fp` in the cache key, so their Arc churns on every scroll. We
        // route them through `push_viewport_scoped` so that churn flips
        // `decorations.signature` (still invalidates the minimap / per-line
        // galley cache where the visible-band content really did change) but
        // *not* `decorations.geometry_epoch` — letting the wrap cache skip
        // the off-viewport rescan on pure scroll.
        ($slot:ident, $fp:expr, $build:expr, vp_scoped) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            view.decorations.push_viewport_scoped(v);
        }};
    }

    // active_line is cheap to BUILD, but it's not cheap to *not cache* —
    // each fresh `RangeSet::from_iter` call hands back a new `Arc`, which
    // flips `view.decorations.signature` (mix of every layer's
    // `content_id`). That signature is part of the per-line galley cache
    // key, so an unstable signature invalidates every visible line's
    // layout every frame. Cache on `(doc_id, sel)` — the inputs the
    // provider actually depends on — so idle / pure-scroll frames return
    // the same Arc and the galley cache holds.
    cached!(active_line, mix(doc_id, sel), || {
        active_line_decorations(editor)
    }, vp_scoped);

    // Paint-only, viewport-scoped, doc-only-dependent. Gated on its
    // own View-menu toggle (`view-highlight-trailing-whitespace-toggle`).
    if *highlight_trailing_whitespace {
        cached!(trailing_ws, mix(doc_id, vp_fp), || {
            trailing_whitespace_decorations(editor, Some(&visible_range))
        }, vp_scoped);
    }

    // Index-diff gutter (`compute_diff` parity). Cached on (doc content
    // id, loaded-text length + ptr hash) — `loaded_text` is only swapped
    // on disk reads/writes, so its address + length together act as a
    // cheap identity fingerprint that survives across paints. Without
    // this cache, every paint runs a full line-level `diff::compute`
    // over the buffer + on-disk snapshot, which is the dominant scroll
    // cost on non-trivial files.
    let loaded_fp = mix(loaded_text.as_ptr() as u64, loaded_text.len() as u64);
    cached!(index_diff, mix(doc_id, loaded_fp), || {
        editor.index_diff_decorations(loaded_text)
    });

    // markdown / fold / fold emit Line decorations with
    // `hide: true` or `height_scale`, so they go through `push_with_heights`
    // to reach the heightmap driver. markdown depends on cursor line
    // (code blocks reveal on cursor-on-line); fold/frontmatter on the fold
    // set. Live-preview layers stay gated on `buffer.live_preview`; the
    // structural fold layer is unconditional so manual folds keep working
    // when previews are off.
    if *live_preview {
        // Keyed on the FULL selection (`sel_fp`), not just `cursor_line`:
        // code-fence reveal is now per-block on selection overlap, not just
        // cursor-line, so a selection growing within one line must still
        // invalidate (`live-preview-code-fence-block-reveal`).
        cached!(markdown, mix(mix(mix(doc_id, cursor_line), folds_id), sel_fp),
            || markdown_decorations(editor, theme), heights);
    }
    cached!(fold, mix(doc_id, folds_id),
        || fold_decorations(editor, folds), heights);

    if *live_preview {
        // wikilink reveals when the cursor isn't on the same line —
        // selection-dependent on top of doc + viewport.
        cached!(wikilink, mix(mix(doc_id, cursor_line), vp_fp),
            || wikilink_decorations(editor, theme, Some(&visible_range), resolve_title), vp_scoped);
        cached!(callout, mix(doc_id, vp_fp),
            || callout_decorations(editor, theme, Some(&visible_range)), vp_scoped);
    }

    cached!(frontmatter, mix(doc_id, folds_id),
        || frontmatter_fold(editor, folds, theme), heights);

    if *live_preview {
        cached!(transclusion, mix(doc_id, vp_fp),
            || transclusion_decorations(editor, theme, Some(&visible_range)), vp_scoped);
        cached!(footnote, mix(doc_id, vp_fp),
            || footnote_decorations(editor, theme, Some(&visible_range)), vp_scoped);
        cached!(math, mix(doc_id, vp_fp),
            || math_decorations(editor, theme, Some(&visible_range)), vp_scoped);
        cached!(mermaid, mix(doc_id, vp_fp),
            || mermaid_decorations(editor, theme, Some(&visible_range)), vp_scoped);
        cached!(wavedrom, mix(doc_id, vp_fp),
            || wavedrom_decorations(editor, theme, Some(&visible_range)), vp_scoped);
    }

    // Rendered LaTeX math widgets (`widget-render-providers`). Independent of
    // live preview — gated on its own `Render widgets` toggle and markdown.
    // The fingerprint folds in everything reveal + render depend on: doc id,
    // the full multi-cursor selection (reveal flips per cursor move), the
    // viewport (viewport-scoped render), font size, dpr, and the theme fg
    // (colors are baked into the render) (`widget-render-cache`). Emits `hide`
    // lines + `BlockWidget` for display math, so it goes through
    // `push_with_heights`.
    if *render_widgets && *is_markdown {
        let theme_fg = theme
            .map(|t| {
                let c = t.palette.fg;
                u32::from_le_bytes([c.r, c.g, c.b, c.a]) as u64
            })
            .unwrap_or(0);
        let dpr_bits = dpr.to_bits() as u64;
        let font_bits = font_px.to_bits() as u64;
        let render_fp = mix(
            mix(mix(doc_id, sel_fp), vp_fp),
            mix(mix(theme_fg, dpr_bits), font_bits),
        );
        let dpr = *dpr;
        let font_px = *font_px;
        cached!(math_widget, render_fp,
            || widgets::math_widget_decorations(editor, theme, Some(&visible_range), font_px, dpr, diagram_cache),
            heights);
        // Rendered Mermaid diagram widgets (`widget-mermaid-render`). Same gate,
        // same render fingerprint as the math-widget layer; emits `hide` lines +
        // a `BlockWidget` per fence so it goes through `push_with_heights`.
        cached!(mermaid_widget, render_fp,
            || widgets::mermaid_widget_decorations(editor, theme, Some(&visible_range), font_px, dpr, diagram_cache),
            heights);
        // Rendered WaveDrom diagram widgets (`widget-wavedrom-render`). Same
        // gate + render fingerprint as math / mermaid; emits `hide` lines + a
        // `BlockWidget` per fence, so it goes through `push_with_heights`.
        cached!(wavedrom_widget, render_fp,
            || widgets::wavedrom_widget_decorations(editor, theme, Some(&visible_range), font_px, dpr, diagram_cache),
            heights);
        // Natively-painted pipe-table widgets (`widget-table-render`). Same gate
        // + render fingerprint as math / mermaid; emits `hide` lines + a
        // `BlockWidget` per table (painted from a `paint_list`, no raster), so it
        // goes through `push_with_heights`. status: widget-table-render
        cached!(table_widget, render_fp,
            || widgets::tables::table_widget_decorations(editor, theme, Some(&visible_range), font_px, dpr),
            heights);
    }

    // Chunk-boundary visualisation: a gutter marker + faint background at
    // every chunk start, so the user can see how the indexer slices this
    // note (`view-show-chunk-boundaries`).
    if *chunk_boundaries {
        cached!(chunk_boundaries, doc_id, || {
            editor.chunk_boundary_decorations()
        });
    }

    // Whitespace overlay (view-menu toggle). Doc-dependent only; cache
    // on doc_id so the layer's Arc stays stable across scroll frames and
    // doesn't flip `layers_sig`.
    if *show_whitespace {
        cached!(special_chars, doc_id, || {
            let flags = SpecialCharsFlags {
                tabs: true,
                spaces: true,
                nbsp: true,
                zero_width: true,
                crlf: true,
            };
            special_chars_decorations(editor, flags)
        });
    }

    // Diff overlay: view zones for removed lines + line backgrounds for
    // added/modified ranges, computed once at the top of `show`. Pushed
    // last so the diff stacks above other decoration layers; goes through
    // `push_with_heights` because the Block entries reserve space in the
    // line-height map.
    if let Some(ov) = diff {
        view.decorations.push_with_heights(ov.decorations.clone());
    }

    // Find-in-note match highlights (`editor-find-in-note`). Pure
    // paint layer driven off `view.search`; recomputed each frame
    // because the match list is small and the call is gated on
    // `search.active`. Pushed before occurrence / bracket-match so it
    // layers cleanly under cursor-derived emphasis.
    find::push_decorations(editor, view);

    // Viewport-scoped layers (occurrence highlight, bracket match). Both
    // are cheap to build, but constructing a fresh `RangeSet` every frame
    // flips `view.decorations.signature` (Arc-pointer-based content_id)
    // and forces the per-line galley cache to rebuild every visible row.
    // Cache them on the inputs the provider actually depends on so the
    // signature stays stable on idle/scroll frames.
    cached!(occurrence, mix(mix(doc_id, sel), vp_fp), || {
        occurrence_decorations(editor, visible_range.clone())
    }, vp_scoped);
    // bracket_match is doc+selection-keyed (not viewport-scoped), but it's
    // paint-only and changes on every cursor move; routing it through the
    // viewport-scoped lane keeps cursor moves from forcing a full prewrap of
    // the whole document (matching the behavior of all the other
    // selection/cursor-derived layers).
    cached!(bracket_match, mix(doc_id, sel), || {
        bracket_match_decorations(editor, DEFAULT_BRACKETS, 5000)
    }, vp_scoped);
}

/// Combine multiple u64 values into a single fingerprint via splitmix-style
/// hashing. Order-dependent.
const fn mix(seed: u64, x: u64) -> u64 {
    let mut z = seed.wrapping_add(x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Decoration-layer methods that hang off an `Editor` rather than
/// `BufCtx` — they're called from inside the `cached!` closures in
/// `show_editor`, which already hold a `&mut buffer.decoration_cache`
/// and so can only borrow `buffer.editor` immutably. Trait methods on
/// `&self` are exempt from `clippy::single_call_fn`.
pub(super) trait EditorDecorations {
    /// Build a decoration layer that paints a subtle line tint plus a
    /// gutter marker on every chunk-start line, matching the indexer's
    /// heading-aware chunk boundaries (`view-show-chunk-boundaries`).
    fn chunk_boundary_decorations(&self) -> editor_core::decoration::Set;
    /// Compute a `Set` that places `GutterMarker::DiffAdded`,
    /// `DiffRemoved`, or `DiffModified` on every line of the live
    /// buffer that diverges from `loaded_text` (the most recent disk
    /// read / write).
    fn index_diff_decorations(&self, loaded_text: &str) -> editor_core::decoration::Set;
}

impl EditorDecorations for editor_core::state::Editor {
    fn chunk_boundary_decorations(&self) -> editor_core::decoration::Set {
        use editor_core::decoration::Color;
        use editor_core::decoration::Decoration;
        use editor_core::decoration::GutterMarker;
        use editor_core::decoration::LineStyle;
        let editor = self;
        let text = editor.doc.to_string();
        let chunks = hiker_core::chunker::markdown::chunk(&text);
        let mut set = editor_core::decoration::Set::empty();
        // Faint stripe color (light blue) — picked to be visible against
        // both light and dark themes.
        let stripe = Color::rgba(0x66, 0x99, 0xff, 0x18);
        for (idx, chunk) in chunks.iter().enumerate() {
            if idx == 0 {
                continue; // The first chunk starts at the doc head — skip.
            }
            let byte = chunk.byte_start;
            if byte >= text.len() {
                continue;
            }
            let line = editor.doc.byte_to_line(byte);
            let line_start = editor.doc.line_to_byte(line);
            let line_end = if line + 1 < editor.doc.len_lines() {
                editor.doc.line_to_byte(line + 1)
            } else {
                editor.doc.len_bytes()
            };
            let style = LineStyle {
                bg: Some(stripe),
                gutter_marker: Some(GutterMarker::Custom(smol_str::SmolStr::new("S"))),
                ..LineStyle::default()
            };
            set = set.insert(line_start..line_end, Decoration::Line(style));
        }
        set
    }

    /// Strategy: line-level diff via `hiker_core::diff::compute`. Each
    /// Insert in the diff is a line in `after` that has no exact
    /// counterpart in `before`. We emit `DiffModified` when a Delete on
    /// the same after-line preceded the Insert (i.e. a replace),
    /// otherwise `DiffAdded`. Pure Deletes don't have a corresponding
    /// `after` line to mark, so we collapse adjacent Delete-only runs
    /// onto the nearest following surviving line as `DiffRemoved`
    /// (matches the legacy gutter behavior).
    fn index_diff_decorations(&self, loaded_text: &str) -> editor_core::decoration::Set {
        use editor_core::decoration::Decoration;
        use editor_core::decoration::GutterMarker;
        use editor_core::decoration::LineStyle;
        use editor_core::rangeset::RangeSet;
        use hiker_core::diff::Op;
        let state = self;
        let live = state.doc.to_string();
        if loaded_text == live {
            return RangeSet::empty();
        }
        let diff = hiker_core::diff::compute(loaded_text, &live);
        let mut per_after_line: std::collections::BTreeMap<u32, GutterMarker> =
            std::collections::BTreeMap::new();
        let mut pending_delete = false;
        let mut last_after_seen: u32 = 0;
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                match line.op {
                    Op::Equal => {
                        if let Some(an) = line.after_line_no {
                            last_after_seen = an;
                            if pending_delete {
                                per_after_line.entry(an).or_insert(GutterMarker::DiffRemoved);
                                pending_delete = false;
                            }
                        }
                    }
                    Op::Insert => {
                        if let Some(an) = line.after_line_no {
                            let marker = if pending_delete {
                                pending_delete = false;
                                GutterMarker::DiffModified
                            } else {
                                GutterMarker::DiffAdded
                            };
                            per_after_line.insert(an, marker);
                            last_after_seen = an;
                        }
                    }
                    Op::Delete => {
                        pending_delete = true;
                    }
                }
            }
        }
        if pending_delete && last_after_seen > 0 {
            per_after_line
                .entry(last_after_seen)
                .or_insert(GutterMarker::DiffRemoved);
        }

        let doc = &state.doc;
        let total_bytes = doc.len_bytes();
        let total_lines = doc.len_lines();
        let mut entries: Vec<(std::ops::Range<usize>, Decoration)> =
            Vec::with_capacity(per_after_line.len());
        for (line1, marker) in per_after_line {
            let line0 = line1.saturating_sub(1) as usize;
            if line0 >= total_lines {
                continue;
            }
            let line_start = doc.line_to_byte(line0);
            let line_end = if line0 + 1 < total_lines {
                doc.line_to_byte(line0 + 1)
            } else {
                total_bytes
            };
            let range = if line_start == line_end {
                line_start..line_start + 1
            } else {
                line_start..line_end
            };
            entries.push((
                range,
                Decoration::Line(LineStyle {
                    gutter_marker: Some(marker),
                    ..LineStyle::default()
                }),
            ));
        }
        RangeSet::from_iter(entries)
    }
}
