//! Buffer tab body: editor toolbar strip, the editor widget itself, then
//! the status bar. Buffer-only chrome stays here (hidden for all
//! non-buffer kinds).
#![allow(clippy::items_after_test_module)]

use std::sync::Arc;

use eframe::egui;

use editor_core::light_default;
use editor_egui::{EditorWidget, MinimapOptions, MinimapWidget};
use editor_md::{
    callout_decorations, fold_decorations, footnote_decorations, frontmatter_fold,
    markdown_decorations, math_decorations, mermaid_decorations, transclusion_decorations,
    wikilink_decorations,
};
use editor_view::{
    active_line_decorations, bracket_match_decorations, brackets::DEFAULT_BRACKETS,
    occurrence::occurrence_decorations, special_chars_decorations, trailing_whitespace_decorations,
    SpecialCharsFlags,
};

use crate::buffer::DecorationCache;
use crate::editor_pane;
use crate::icons;
use crate::state::{AppState, ToastLevel};
use crate::theme;

/// XOR-mix the fold ids in an order-independent way. Cheap and stable for
/// memoization keys (HashSet iteration order isn't deterministic).
fn folds_hash(folds: &std::collections::HashSet<u64>) -> u64 {
    let mut h: u64 = 0;
    for &id in folds {
        h ^= id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    h
}

/// Combine multiple u64 values into a single fingerprint via splitmix-style
/// hashing. Order-dependent.
fn mix(seed: u64, x: u64) -> u64 {
    let mut z = seed.wrapping_add(x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    // Breathing room between the window-level top strip (where tabs live)
    // and the buffer's own toolbar so they don't visually collide.
    ui.add_space(6.0);

    // Toolbar across the top of the buffer tab body.
    toolbar(ui, app, path);

    // Pending-rewrite banner: thin row that surfaces a write-shaped
    // proposal targeting this note. Per `patch-review.md:138-148` —
    // single-line, dismissable inline, click to Accept/Reject/View diff.
    pending_rewrite_banner(ui, app, path);

    ui.add_space(4.0);

    // Pin the status bar to the bottom of the pane via a
    // TopBottomPanel. The previous "manual subtract status-bar height
    // from available_height + allocate_exact_size" approach overflowed
    // by `item_spacing.y + 2` pixels because allocate_exact_size adds
    // egui's automatic item-spacing gap AFTER it returns, and there
    // was an additional 2-px add_space between the body and the
    // status bar — neither of which the body-height math accounted for.
    // The result was the status bar getting pushed past the pane
    // bottom into the window's edge. TopBottomPanel handles the
    // geometry exactly: claim the bottom strip from the Ui's
    // max_rect, body fills the remaining region above.
    egui::TopBottomPanel::bottom(ui.id().with("buffer-status-bar"))
        .resizable(false)
        .show_separator_line(false)
        .show_inside(ui, |ui| {
            status_bar(ui, app, path);
        });

    // The editor body fills whatever the bottom panel didn't claim.
    egui::Frame::default().show(ui, |ui| {
        let body_height = ui.available_height().max(80.0);
        let (rect, _resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), body_height),
            egui::Sense::hover(),
        );
        // Editor owns its own horizontal scroll (long lines, code
        // blocks). Register the rect so the swipe-nav handler skips
        // gestures that originate inside the editor body.
        app.session.nav.swipe_skip_rects.push(rect);
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        show_editor(&mut body_ui, app, path);
    });
}

fn show_editor(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    crate::profile_function!();
    // Cmd-S / Ctrl-S save shortcut at the buffer level. We intercept
    // before the editor consumes the event since the editor doesn't bind
    // it (save is a host concern).
    let save_pressed = ui.input(|i| {
        i.key_pressed(egui::Key::S) && i.modifiers.command_only()
    });
    if save_pressed
        && let Err(err) = editor_pane::save_buffer(app, path)
    {
        app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
    }

    let Some(buffer) = app.session.buffers.get_mut(path) else {
        ui.label(format!("buffer {} not loaded", path));
        return;
    };

    // Rebuild decoration layers from current state. Most decoration
    // providers take an Option<&Theme> so they can fall back to a
    // built-in palette when the host hasn't supplied one.
    let theme_owned = light_default();
    let theme = Some(&theme_owned);
    // Compute the visible byte range up-front so we can scope paint-only
    // providers to the viewport.
    let visible = buffer.view.visible_lines();
    let last_line = buffer.editor.doc.len_lines().saturating_sub(1);
    let visible_start = buffer
        .editor
        .doc
        .line_to_byte(visible.start.min(last_line));
    let visible_end_line = visible.end.min(last_line);
    let visible_end = if visible_end_line + 1 < buffer.editor.doc.len_lines() {
        buffer.editor.doc.line_to_byte(visible_end_line + 1)
    } else {
        buffer.editor.doc.len_bytes()
    };
    let visible_range = visible_start..visible_end;

    // Fingerprint inputs for memoized providers. `content_id` is an Arc
    // pointer into the rope tree — changes only on doc edits, so idle / pure
    // scroll frames hit the cache.
    let doc_id = buffer.editor.doc.content_id() as u64;
    let sel = buffer.editor.selection.main().head.offset() as u64;
    // Layers whose only cursor dependence is "is the cursor on this line?"
    // (markdown reveal, wikilink reveal) key on the line index instead of
    // the byte offset — otherwise a selection drag busts the cache on every
    // byte and reparses the whole doc per frame.
    let cursor_line = buffer.editor.doc.byte_to_line(sel as usize) as u64;
    let folds_id = folds_hash(&buffer.folds);
    let vp_lo = visible_start as u64;
    let vp_hi = visible_end as u64;
    let vp_fp = mix(vp_lo, vp_hi);
    let cache: &mut DecorationCache = &mut buffer.decoration_cache;

    crate::profile_scope!("rebuild decorations");
    buffer.view.decorations.clear();

    // Per-layer caching follows the same shape everywhere: gate on a flag,
    // mix a fingerprint, either reuse the cached `DecorationSet` or rebuild
    // it via the supplied closure, then push (optionally with heights for
    // layers that emit Line decorations the heightmap needs to see).
    //
    // `cached!(slot, fp, build, heights?)` keeps the per-layer code to a
    // single line each. `heights` is the optional fourth arg — when present,
    // the layer goes through `push_with_heights`; otherwise plain `push`.
    macro_rules! cached {
        ($slot:ident, $fp:expr, $build:expr) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            buffer.view.decorations.push(v);
        }};
        ($slot:ident, $fp:expr, $build:expr, heights) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            buffer.view.decorations.push_with_heights(v);
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
        active_line_decorations(&buffer.editor)
    });

    // Paint-only, viewport-scoped, doc-only-dependent. Gated on its
    // own View-menu toggle (`view-highlight-trailing-whitespace-toggle`).
    if buffer.highlight_trailing_whitespace {
        cached!(trailing_ws, mix(doc_id, vp_fp), || {
            trailing_whitespace_decorations(&buffer.editor, Some(visible_range.clone()))
        });
    }

    // Index-diff gutter (`compute_diff` parity). Cached on (doc content
    // id, loaded-text length + ptr hash) — `loaded_text` is only swapped
    // on disk reads/writes, so its address + length together act as a
    // cheap identity fingerprint that survives across paints. Without
    // this cache, every paint runs a full line-level `diff::compute`
    // over the buffer + on-disk snapshot, which is the dominant scroll
    // cost on non-trivial files.
    let loaded_fp = mix(
        buffer.loaded_text.as_ptr() as u64,
        buffer.loaded_text.len() as u64,
    );
    cached!(index_diff, mix(doc_id, loaded_fp), || {
        index_diff_decorations(&buffer.loaded_text, &buffer.editor)
    });

    // markdown / fold / frontmatter_fold emit Line decorations with
    // `hide: true` or `height_scale`, so they go through `push_with_heights`
    // to reach the heightmap driver. markdown depends on cursor line
    // (code blocks reveal on cursor-on-line); fold/frontmatter on the fold
    // set. Live-preview layers stay gated on `buffer.live_preview`; the
    // structural fold layer is unconditional so manual folds keep working
    // when previews are off.
    if buffer.live_preview {
        cached!(markdown, mix(mix(doc_id, cursor_line), folds_id),
            || markdown_decorations(&buffer.editor, theme), heights);
    }
    cached!(fold, mix(doc_id, folds_id),
        || fold_decorations(&buffer.editor, &buffer.folds), heights);

    if buffer.live_preview {
        // wikilink reveals when the cursor isn't on the same line —
        // selection-dependent on top of doc + viewport.
        cached!(wikilink, mix(mix(doc_id, cursor_line), vp_fp),
            || wikilink_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(callout, mix(doc_id, vp_fp),
            || callout_decorations(&buffer.editor, theme, Some(visible_range.clone())));
    }

    cached!(frontmatter, mix(doc_id, folds_id),
        || frontmatter_fold(&buffer.editor, &buffer.folds, theme), heights);

    if buffer.live_preview {
        cached!(transclusion, mix(doc_id, vp_fp),
            || transclusion_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(footnote, mix(doc_id, vp_fp),
            || footnote_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(math, mix(doc_id, vp_fp),
            || math_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(mermaid, mix(doc_id, vp_fp),
            || mermaid_decorations(&buffer.editor, theme, Some(visible_range.clone())));
    }

    // Chunk-boundary visualisation: a gutter marker + faint background at
    // every chunk start, so the user can see how the indexer slices this
    // note (`view-show-chunk-boundaries`).
    if buffer.chunk_boundaries {
        cached!(chunk_boundaries, doc_id, || {
            chunk_boundary_decorations(&buffer.editor)
        });
    }

    // Whitespace overlay (view-menu toggle). Doc-dependent only; cache
    // on doc_id so the layer's Arc stays stable across scroll frames and
    // doesn't flip `layers_sig`.
    if buffer.show_whitespace {
        cached!(special_chars, doc_id, || {
            let flags = SpecialCharsFlags {
                tabs: true,
                spaces: true,
                nbsp: true,
                zero_width: true,
                crlf: true,
            };
            special_chars_decorations(&buffer.editor, flags)
        });
    }

    // Viewport-scoped layers (occurrence highlight, bracket match). Both
    // are cheap to build, but constructing a fresh `RangeSet` every frame
    // flips `view.decorations.signature` (Arc-pointer-based content_id)
    // and forces the per-line galley cache to rebuild every visible row.
    // Cache them on the inputs the provider actually depends on so the
    // signature stays stable on idle/scroll frames.
    cached!(occurrence, mix(mix(doc_id, sel), vp_fp), || {
        occurrence_decorations(&buffer.editor, visible_range.clone())
    });
    cached!(bracket_match, mix(doc_id, sel), || {
        bracket_match_decorations(&buffer.editor, DEFAULT_BRACKETS, 5000)
    });

    // Render the editor (left) and the structural minimap (right). The
    // minimap reads the same `ViewState.decorations` the editor paints
    // from, so heading/code/quote classification follows whatever syntax
    // pipeline the host has wired up.
    // Resolve minimap options from the live config snapshot. Cheap each
    // frame — a few field copies + 9 hex parses. Hex parses default back
    // to the built-in palette if the user typed something invalid.
    let mini_opts: Option<MinimapOptions> = if buffer.show_minimap {
        app.vault_session.config
            .read()
            .ok()
            .map(|c| minimap_options_from_config(&c.editor.minimap))
    } else {
        None
    };

    let click_buffer = &mut buffer.click_buffer;
    let paint_cache = &mut buffer.paint_cache;
    let body = ui.available_rect_before_wrap();
    let minimap_w: f32 = mini_opts.as_ref().map(|o| o.width).unwrap_or(0.0);
    let split_x = (body.right() - minimap_w).max(body.left());
    let editor_rect = egui::Rect::from_min_max(body.min, egui::pos2(split_x, body.max.y));
    {
        crate::profile_scope!("EditorWidget::show");
        let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
        EditorWidget::new(&mut buffer.editor, &mut buffer.view)
            .with_click_sink(click_buffer)
            .with_paint_cache(paint_cache)
            .show(&mut editor_ui);
    }
    if let Some(opts) = mini_opts {
        crate::profile_scope!("MinimapWidget::show");
        let minimap_rect =
            egui::Rect::from_min_max(egui::pos2(split_x, body.min.y), body.max);
        let mut mini_ui = ui.new_child(egui::UiBuilder::new().max_rect(minimap_rect));
        MinimapWidget::new(&buffer.editor, &mut buffer.view)
            .with_options(opts)
            .show(&mut mini_ui);
    }

    // Apply fold toggles from this frame's clicks.
    buffer.drain_fold_clicks();
}

/// Surface a thin banner whenever there's a pending write-shaped
/// proposal targeting the open buffer. Spec mandates a single-line strip
/// just under the toolbar (`patch-review.md:138-148`) with Accept,
/// Reject, and View-diff actions — *not* the larger half-page banner the
/// old TS UI used.
fn pending_rewrite_banner(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let staging = app.vault_session.services.staging.clone();
    // Reads the per-frame cache populated in `main::refresh_staging_snapshot`.
    let Some(prop) = app
        .ui_cache.staging_snapshot
        .iter()
        .find(|p| p.target_path == path && p.action == "write_note")
        .cloned()
    else {
        return;
    };
    let mut accept = false;
    let mut reject = false;
    let mut view = false;
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(0xff, 0xf3, 0xc4))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xd9, 0xb8, 0x4e)))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(crate::icons::robot());
                ui.label(
                    egui::RichText::new("Agent proposed a full-note rewrite")
                        .small()
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(format!("({})", &prop.id[..prop.id.len().min(8)]))
                        .color(theme::muted())
                        .monospace()
                        .small(),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.small_button("Accept").clicked() {
                            accept = true;
                        }
                        if ui.small_button("Reject").clicked() {
                            reject = true;
                        }
                        if ui.small_button("View diff").clicked() {
                            view = true;
                        }
                    },
                );
            });
        });
    if accept {
        let changes = app.vault_session.services.changes.clone();
        match staging.accept(&prop.id, &app.vault_session.vault, Some(changes.as_ref())) {
            Ok(o) => app.push_toast(
                format!("Accepted proposal for {}", o.target_path),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(format!("Accept failed: {err}"), ToastLevel::Error),
        }
    }
    if reject {
        match staging.reject(&prop.id) {
            Ok(()) => app.push_toast(
                "Proposal rejected".to_string(),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(format!("Reject failed: {err}"), ToastLevel::Error),
        }
    }
    if view {
        use crate::tab::TabKind;
        let pid = prop.id.clone();
        let target = prop.target_path.clone();
        let pid_for_build = pid.clone();
        app.find_or_open_tab(
            |k| matches!(k, TabKind::StagingPreview { proposal_id, .. } if *proposal_id == pid),
            || TabKind::StagingPreview {
                proposal_id: pid_for_build,
                target_path: target,
            },
        );
    }
}

fn toolbar(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::image(icons::check()))
                    .on_hover_text("Save (Mod-S)")
                    .clicked()
                {
                    if let Err(err) = editor_pane::save_buffer(app, path) {
                        app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                    }
                }
                let dirty = app.session.buffers.get(path).map(|b| b.is_dirty()).unwrap_or(false);
                if dirty {
                    ui.add(icons::current_dot());
                }
                ui.separator();
                if ui
                    .add(egui::Button::image(icons::diff()))
                    .on_hover_text("User diff vs disk")
                    .clicked()
                {
                    open_diff_vs_disk(app, path);
                }
                // Agent-diff toggle: jump to the staging-preview tab when
                // a write-shaped proposal is in flight against this note.
                // Mutually-exclusive with the user-diff button above per
                // `patch-review.md:17-27` — both toggle the same buffer
                // tab strip into a single diff mode at a time.
                let has_agent_proposal = app.ui_cache.staging_snapshot.iter().any(|p| {
                    p.target_path == path
                        && (p.action == "write_note" || p.action == "edit_note")
                });
                ui.add_enabled_ui(has_agent_proposal, |ui| {
                    if ui
                        .add(egui::Button::image(crate::icons::robot()))
                        .on_hover_text(if has_agent_proposal {
                            "Agent diff (pending proposal)"
                        } else {
                            "No pending agent proposal for this note"
                        })
                        .clicked()
                    {
                        // Open the staging preview for the first matching
                        // proposal. Done via singleton tab semantics so
                        // repeated clicks just focus the existing tab.
                        if let Some(p) = app.ui_cache.staging_snapshot.iter().find(|p| {
                            p.target_path == path
                                && (p.action == "write_note" || p.action == "edit_note")
                        }) {
                            use crate::tab::TabKind;
                            let pid = p.id.clone();
                            let tpath = p.target_path.clone();
                            let pid_for_build = pid.clone();
                            app.find_or_open_tab(
                                |k| matches!(k, TabKind::StagingPreview { proposal_id, .. } if *proposal_id == pid),
                                || TabKind::StagingPreview {
                                    proposal_id: pid_for_build,
                                    target_path: tpath,
                                },
                            );
                        }
                    }
                });
                view_options_menu(ui, app, path);
                mutations_menu(ui, app, path);

                // "Add to trail" pill — legacy `addToTrailPill.ts`,
                // `trail-add-to-active-from-editor-verb`. Hidden when
                // no active trail or when the buffer path isn't a
                // regular indexable extension. Disabled (with tooltip)
                // when the path is already a waypoint at any depth.
                add_to_trail_pill(ui, app, path);

                // Centered mode-controls slot — empty in plain editing mode.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |_ui| {
                    // (right side reserved for future view-mode badges)
                });
            });
        });
}

/// "Add to trail" pill in the editor toolbar. Legacy parity:
/// `ui/src/trails/addToTrailPill.ts`. Hidden unless an indexable buffer
/// is open AND there is an active (or fallback Recent) trail to append
/// to. Disabled when the path is already a waypoint at any depth
/// (idempotency, same as legacy membership cache).
fn add_to_trail_pill(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let lower = path.to_lowercase();
    if !lower.ends_with(".md") && !lower.ends_with(".txt") {
        return;
    }
    // Resolve target trail without mutating state: prefer the
    // explicitly-active trail, else fall back to Recent if it exists.
    let target_id: Option<String> = app
        .session.active_trail
        .clone()
        .filter(|id| app.session.trails.iter().any(|t| &t.id == id))
        .or_else(|| {
            app.session.trails
                .iter()
                .find(|t| t.name == crate::state::RECENT_TRAIL)
                .map(|t| t.id.clone())
        });
    let Some(trail_id) = target_id else {
        return;
    };
    let trail = match app.session.trails.iter().find(|t| t.id == trail_id) {
        Some(t) => t,
        None => return,
    };
    let trail_name = trail.name.clone();
    let already = trail_contains_path(&trail.waypoints, path);

    ui.separator();
    let label = format!("+ {}", trail_name);
    let tooltip = if already {
        format!("Already in '{}'", trail_name)
    } else {
        format!("Add to trail '{}'", trail_name)
    };
    let resp = ui.add_enabled(!already, egui::Button::new(label));
    let resp = resp.on_hover_text(tooltip);
    if resp.clicked() {
        crate::state::note_visited(app, path);
        let _ = crate::bootstrap::save_trails(&app.vault_session.vault_root, &app.session.trails);
        app.push_toast(
            format!("Added to '{}'", trail_name),
            ToastLevel::Info,
        );
    }
}

fn trail_contains_path(waypoints: &[crate::state::Waypoint], path: &str) -> bool {
    for w in waypoints {
        if w.path == path {
            return true;
        }
        if trail_contains_path(&w.children, path) {
            return true;
        }
    }
    false
}

/// Popup menu offering view toggles. Mirrors the old `ui/src/app/viewMenu.ts`
/// — flips that map directly onto live editor state apply immediately;
/// flips for not-yet-wired features (chunk boundaries, intraline diff,
/// heading breadcrumb) are surfaced as disabled rows so the menu is
/// feature-complete by shape.
fn view_options_menu(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let mut wrap = false;
    let mut hide_gutter = false;
    let mut placeholder_special = false;
    let mut highlight_trailing_ws = false;
    let mut hide_frontmatter = false;
    let mut show_minimap = false;
    if let Some(buffer) = app.session.buffers.get(path) {
        wrap = buffer.view.wrap_map.enabled();
        hide_gutter = buffer.view.hide_gutter;
        placeholder_special = buffer.show_whitespace;
        highlight_trailing_ws = buffer.highlight_trailing_whitespace;
        hide_frontmatter = buffer.hide_frontmatter;
        show_minimap = buffer.show_minimap;
    }
    let resp = ui
        .add(egui::Button::image(icons::eye()))
        .on_hover_text("View options");
    egui::Popup::menu(&resp).show(|ui| {
        if ui.checkbox(&mut wrap, "Word wrap").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.view.wrap_map.set_enabled(wrap);
            }
            persist_view_setting(app, "editor.word_wrap", serde_json::json!(wrap));
        }
        let mut show_gutter = !hide_gutter;
        if ui.checkbox(&mut show_gutter, "Show line numbers").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.view.hide_gutter = !show_gutter;
            }
            persist_view_setting(app, "editor.show_line_numbers", serde_json::json!(show_gutter));
        }
        if ui.checkbox(&mut placeholder_special, "Show whitespace").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.show_whitespace = placeholder_special;
            }
            persist_view_setting(
                app,
                "editor.show_whitespace",
                serde_json::json!(placeholder_special),
            );
        }
        if ui
            .checkbox(&mut highlight_trailing_ws, "Highlight trailing whitespace")
            .on_hover_text("Paint a red background over trailing spaces/tabs (view-highlight-trailing-whitespace-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.highlight_trailing_whitespace = highlight_trailing_ws;
                buffer.decoration_cache.trailing_ws = None;
            }
            persist_view_setting(
                app,
                "editor.highlight_trailing_whitespace",
                serde_json::json!(highlight_trailing_ws),
            );
        }
        if ui
            .checkbox(&mut show_minimap, "Show minimap")
            .on_hover_text("Structural minimap strip on the right of the editor")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.show_minimap = show_minimap;
            }
            persist_view_setting(app, "editor.show_minimap", serde_json::json!(show_minimap));
        }
        if ui.checkbox(&mut hide_frontmatter, "Hide frontmatter").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.hide_frontmatter = hide_frontmatter;
            }
            persist_view_setting(
                app,
                "editor.hide_frontmatter",
                serde_json::json!(hide_frontmatter),
            );
        }
        ui.separator();
        let mut live_preview = false;
        let mut chunk_boundaries = false;
        let mut render_txt_as_md = false;
        let mut intraline_diff = false;
        let mut heading_breadcrumb = false;
        if let Some(buffer) = app.session.buffers.get(path) {
            live_preview = buffer.live_preview;
            chunk_boundaries = buffer.chunk_boundaries;
            render_txt_as_md = buffer.render_txt_as_markdown;
            intraline_diff = buffer.intraline_diff;
            heading_breadcrumb = buffer.heading_breadcrumb;
        }
        if ui
            .checkbox(&mut live_preview, "Live preview")
            .on_hover_text("Inline-render wikilinks, math, callouts (view-live-preview-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.live_preview = live_preview;
                buffer.decoration_cache = DecorationCache::default();
            }
            persist_view_setting(app, "editor.live_preview", serde_json::json!(live_preview));
        }
        if ui
            .checkbox(&mut chunk_boundaries, "Show chunk boundaries")
            .on_hover_text("Visualize how the indexer splits this note (view-show-chunk-boundaries)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.chunk_boundaries = chunk_boundaries;
            }
            persist_view_setting(
                app,
                "editor.show_chunk_boundaries",
                serde_json::json!(chunk_boundaries),
            );
        }
        let is_txt = path
            .rsplit_once('.')
            .map(|(_, ext)| ext.eq_ignore_ascii_case("txt"))
            .unwrap_or(false);
        ui.add_enabled_ui(is_txt, |ui| {
            if ui
                .checkbox(&mut render_txt_as_md, "Render .txt as markdown")
                .on_hover_text("Apply the markdown live-preview stack to .txt files (view-render-txt-as-markdown-toggle)")
                .changed()
            {
                if let Some(buffer) = app.session.buffers.get_mut(path) {
                    buffer.render_txt_as_markdown = render_txt_as_md;
                    // For an open .txt buffer, the live-preview flag
                    // also flips so the change takes effect immediately.
                    buffer.live_preview = render_txt_as_md;
                    buffer.decoration_cache = DecorationCache::default();
                }
                persist_view_setting(
                    app,
                    "editor.render_txt_as_markdown",
                    serde_json::json!(render_txt_as_md),
                );
            }
        });
        if ui
            .checkbox(&mut intraline_diff, "Intraline diff highlights")
            .on_hover_text("Color diff changes at character granularity (view-intraline-diff-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.intraline_diff = intraline_diff;
            }
            persist_view_setting(
                app,
                "editor.intraline_diff",
                serde_json::json!(intraline_diff),
            );
        }
        if ui
            .checkbox(&mut heading_breadcrumb, "Show heading breadcrumb")
            .on_hover_text("Display the cursor's heading path in the status bar (view-heading-breadcrumb-toggle)")
            .changed()
            && let Some(buffer) = app.session.buffers.get_mut(path)
        {
            buffer.heading_breadcrumb = heading_breadcrumb;
        }
    });
}

/// Popup menu offering LLM-backed note mutations. Mirrors the old TS
/// `mutations` module (`note-mutations-menu`): each pick builds a
/// `TaskKind::NoteMutation` task and submits it to the shared queue.
/// A backend worker drains the queue, runs the LLM, and (eventually)
/// surfaces the rewritten content back via the staging system.
fn mutations_menu(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let in_flight = mutation_in_flight(app, path);
    let resp = ui
        .add_enabled(!in_flight, egui::Button::image(icons::wand()))
        .on_hover_text(if in_flight {
            "Mutation in flight — wait for the queued task to finish."
        } else {
            "Mutations"
        });
    let mut chosen: Option<&'static str> = None;
    egui::Popup::menu(&resp).show(|ui| {
        if ui.button("Reformat as markdown").clicked() {
            chosen = Some("reformat-as-markdown");
            ui.close();
        }
        if ui.button("Summarize").clicked() {
            chosen = Some("summarize");
            ui.close();
        }
        if ui.button("Auto-tag").clicked() {
            chosen = Some("auto-tag");
            ui.close();
        }
        if ui.button("Improve clarity").clicked() {
            chosen = Some("improve-clarity");
            ui.close();
        }
    });
    if let Some(m) = chosen {
        submit_mutation(app, path, m);
    }
}

/// True when the active task queue already has a `NoteMutation` task
/// targeting `path` in flight (queued or leased). Mirrors the
/// `note-mutation-one-in-flight-per-path` rule. Reads the live snapshot
/// from the shared task queue — `app.session.pending_mutations` is just a
/// belt-and-suspenders cache that catches the gap between submit and
/// the first snapshot tick.
fn mutation_in_flight(app: &AppState, path: &str) -> bool {
    use hiker_core::tasks::{TaskKind, TaskState};
    // Read the per-frame snapshot cache (`main::refresh_task_snapshot`)
    // instead of blocking on `tasks.snapshot()` here.
    let in_queue = app.ui_cache.task_snapshot.iter().any(|r| {
        matches!(r.state, TaskState::Queued | TaskState::Leased)
            && match &r.kind {
                TaskKind::NoteMutation { source_path, .. } => source_path == path,
                _ => false,
            }
    });
    in_queue || app.session.pending_mutations.contains(path)
}

/// Submit a `NoteMutation` task to the queue. We don't yet have an
/// in-process worker draining the queue here, so the user sees the task
/// land in the Queue panel and the menu disables until it terminates.
fn submit_mutation(app: &mut AppState, path: &str, mutation: &str) {
    use hiker_core::tasks::{Priority, Task, TaskKind, TaskPayload, TaskShape};

    let Some(buffer) = app.session.buffers.get(path) else {
        return;
    };
    let text = buffer.editor.doc.to_string();
    let kind = TaskKind::NoteMutation {
        mutation: mutation.to_string(),
        source_path: path.to_string(),
    };
    let prompt = match mutation {
        "reformat-as-markdown" => "Reformat the following note as clean Markdown.",
        "summarize" => "Summarize the following note in 2-3 sentences.",
        "auto-tag" => "Propose 3-7 tags for the following note.",
        "improve-clarity" => "Rewrite for clarity, preserving meaning.",
        _ => "Apply the requested mutation.",
    };
    let task = Task {
        id: hiker_core::store::new_id(),
        kind,
        priority: Priority::Normal,
        shape: TaskShape::Direct,
        payload: TaskPayload {
            prompt: format!("{prompt}\n\n---\n{text}"),
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        // Include `source_hash_at_submit` so a downstream consumer can
        // detect mid-flight edits via metadata alone — matches the legacy
        // mutation submit payload.
        metadata: serde_json::json!({
            "source_path": path,
            "source_hash_at_submit": &buffer.loaded_hash,
        }),
    };
    let path_owned = path.to_string();
    app.session.pending_mutations.insert(path_owned.clone());
    // Capture the buffer hash at submit time so the awaiter can refuse to
    // clobber a buffer the user has edited since submitting.
    let source_hash_at_submit = buffer.loaded_hash.clone();
    let mutation_kind = mutation.to_string();
    let event_tx = app.vault_session.events.mutation_events_tx.clone();
    let queue = app.vault_session.services.tasks.clone();
    // Use the host's existing tokio runtime (entered at the top of
    // every frame via `_rt_guard = self.runtime.enter()`), not a
    // throwaway one — otherwise the task lands in a queue worker pool
    // that nothing else can see, and the in-process direct-LLM worker
    // never picks it up.
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let path_for_await = path_owned.clone();
            handle.spawn(async move {
                let h = queue.submit(task).await;
                let outcome = h.await_outcome().await;
                let tx = event_tx;
                    use hiker_core::tasks::TaskOutcome;
                    let ev = match outcome {
                        TaskOutcome::Completed { value, .. } => {
                            let content = match value {
                                serde_json::Value::String(s) => s,
                                other => other.to_string(),
                            };
                            crate::state::MutationEvent::Applied {
                                source_path: path_for_await,
                                mutation: mutation_kind,
                                content,
                                source_hash_at_submit,
                            }
                        }
                        TaskOutcome::Failed { error, .. } => {
                            crate::state::MutationEvent::Failed {
                                source_path: path_for_await,
                                mutation: mutation_kind,
                                error,
                            }
                        }
                        TaskOutcome::Cancelled { .. } => {
                            crate::state::MutationEvent::Cancelled {
                                source_path: path_for_await,
                            }
                        }
                    };
                    let _ = tx.send(ev);
                });
            }
        Err(err) => {
            tracing::warn!(error = %err, "no tokio runtime; mutation not submitted");
        }
    }
    app.push_toast(
        format!("Queued mutation '{mutation}' for {path}"),
        ToastLevel::Info,
    );
}

/// Persist a `editor.*` view toggle into vault-scoped settings + swap the
/// merged copy in `AppState::config` so subsequent buffer opens pick up
/// the change. Mirrors the `set_setting` helper used by the settings tab.
/// Parse `#RRGGBB` / `#RRGGBBAA` into an egui `Color32`. Falls back to
/// fully-opaque magenta on a malformed value so a bad config entry is
/// visually obvious instead of silently transparent.
fn parse_hex_color(s: &str) -> egui::Color32 {
    let bytes = s.as_bytes();
    if !matches!(bytes.first(), Some(b'#')) {
        return egui::Color32::from_rgb(0xff, 0x00, 0xff);
    }
    let hex = &s[1..];
    let hex_byte = |i: usize| -> Option<u8> {
        u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()
    };
    match hex.len() {
        6 => {
            let (Some(r), Some(g), Some(b)) = (hex_byte(0), hex_byte(2), hex_byte(4)) else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgb(r, g, b)
        }
        8 => {
            let (Some(r), Some(g), Some(b), Some(a)) =
                (hex_byte(0), hex_byte(2), hex_byte(4), hex_byte(6))
            else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgba_unmultiplied(r, g, b, a)
        }
        _ => egui::Color32::from_rgb(0xff, 0x00, 0xff),
    }
}

fn minimap_options_from_config(
    cfg: &hiker_core::config::MinimapConfig,
) -> MinimapOptions {
    MinimapOptions {
        width: cfg.width as f32,
        bar_padding_left: cfg.bar_padding_left as f32,
        bar_padding_right: cfg.bar_padding_right as f32,
        bar_corner_radius: cfg.bar_corner_radius as f32,
        min_bar_width: cfg.min_bar_width as f32,
        bar_gap: (cfg.bar_gap_tenths as f32) / 10.0,
        colored: cfg.colored,
        show_section_rules: cfg.show_section_rules,
        show_viewport: cfg.show_viewport,
        show_left_edge: cfg.show_left_edge,
        color_heading: parse_hex_color(&cfg.color_heading),
        color_code: parse_hex_color(&cfg.color_code),
        color_emphasis: parse_hex_color(&cfg.color_emphasis),
        color_quote: parse_hex_color(&cfg.color_quote),
        color_plain: parse_hex_color(&cfg.color_plain),
        color_background: parse_hex_color(&cfg.color_background),
        color_section_rule: parse_hex_color(&cfg.color_section_rule),
        color_viewport: parse_hex_color(&cfg.color_viewport),
        color_viewport_hover: parse_hex_color(&cfg.color_viewport_hover),
    }
}

fn persist_view_setting(app: &mut AppState, key: &str, value: serde_json::Value) {
    let label = format!("Save {key} failed");
    app.set_setting(
        hiker_core::config::SettingsScope::Vault,
        key,
        value,
        &label,
    );
}

/// "Diff vs disk" — open (or focus) a `BufferDiff` tab that shows a
/// read-only side-by-side diff between the buffer text and the on-disk
/// version. The preview tab updates as the user keeps typing.
fn open_diff_vs_disk(app: &mut AppState, path: &str) {
    use crate::tab::{Tab, TabKind};
    // Focus an existing diff tab for this path if one's open.
    if let Some(existing) = app.session.tabs.iter().find(|t| {
        matches!(&t.kind, TabKind::BufferDiff { path: p } if p == path)
    }) {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::BufferDiff {
            path: path.to_string(),
        },
        sticky: true,
    });
    app.session.active_tab = Some(id);
}

// Status-bar version dropdown lives in the sibling `versions` submodule
// to keep this file under the project's file-length cap. The dropdown
// surfaces the live buffer, every changelog entry for the path (newest
// first, capped at 20), and every pending staging proposal targeting
// the path.
mod versions;
use versions::version_dropdown;

/// Build a decoration layer that paints a subtle line tint plus a gutter
/// marker on every chunk-start line, matching the indexer's heading-aware
/// chunk boundaries (`view-show-chunk-boundaries`).
fn chunk_boundary_decorations(
    editor: &editor_core::EditorState,
) -> editor_core::DecorationSet {
    use editor_core::decoration::{
        Color, Decoration, GutterMarker, LineStyle,
    };
    let text = editor.doc.to_string();
    let chunks = hiker_core::chunker::chunk_markdown(&text);
    let mut set = editor_core::DecorationSet::empty();
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
            gutter_marker: Some(GutterMarker::Custom(
                smol_str::SmolStr::new("S"),
            )),
            ..LineStyle::default()
        };
        set = set.insert(line_start..line_end, Decoration::Line(style));
    }
    set
}

#[cfg(test)]
mod breadcrumb_tests {
    use super::*;
    use crate::buffer::Buffer;

    fn make(text: &str, cursor_byte: usize) -> Buffer {
        let mut buf = Buffer::from_disk(
            "test.md".to_string(),
            text.to_string(),
            String::new(),
        );
        buf.editor.selection = editor_core::Selection::single(cursor_byte);
        buf
    }

    #[test]
    fn empty_when_no_headings() {
        let buf = make("just some text\nno heads here\n", 0);
        assert_eq!(heading_breadcrumb_for(&buf), "");
    }

    #[test]
    fn picks_up_h1() {
        let buf = make("# Title\nbody\n", 9); // cursor on `body`
        assert_eq!(heading_breadcrumb_for(&buf), "Title");
    }

    #[test]
    fn stacks_deeper_headings() {
        let text = "# A\n## B\n### C\nbody\n";
        // cursor on `body` line — should see A /B /C
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(heading_breadcrumb_for(&buf), "A /B /C");
    }

    #[test]
    fn higher_heading_resets_deeper_stack() {
        let text = "# A\n## B\n### C\n## D\nbody\n";
        // After `## D`, deeper headings get popped; expect A /D
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(heading_breadcrumb_for(&buf), "A /D");
    }
}

/// Walk the document from the start up through the cursor's line and
/// return a `>`-joined breadcrumb of the active heading stack. Higher
/// levels override deeper ones, so an H2 resets any prior H3/H4 entries.
/// Returns an empty string when no heading precedes the cursor.
fn heading_breadcrumb_for(buffer: &crate::buffer::Buffer) -> String {
    let cursor_line =
        buffer.editor.doc.byte_to_line(buffer.editor.selection.main().head.byte as usize);
    let mut stack: Vec<(u8, String)> = Vec::new();
    let total_lines = buffer.editor.doc.len_lines();
    for line_idx in 0..=cursor_line {
        let start = buffer.editor.doc.line_to_byte(line_idx);
        let end = if line_idx + 1 < total_lines {
            buffer.editor.doc.line_to_byte(line_idx + 1)
        } else {
            buffer.editor.doc.len_bytes()
        };
        let line: String = buffer.editor.doc.slice(start..end).to_string();
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let mut depth: u8 = 1;
            let mut chars = rest.chars();
            for c in chars.by_ref() {
                if c == '#' && depth < 6 {
                    depth += 1;
                } else if c == ' ' || c == '\t' {
                    break;
                } else {
                    // not a real heading (# followed by non-space)
                    depth = 0;
                    break;
                }
            }
            if depth == 0 {
                continue;
            }
            let title = chars.as_str().trim_end_matches(['\n', '\r']).trim();
            stack.retain(|(d, _)| *d < depth);
            stack.push((depth, title.to_string()));
        }
    }
    stack
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" /")
}

/// Compute a `DecorationSet` that places `GutterMarker::DiffAdded`,
/// `DiffRemoved`, or `DiffModified` on every line of the live buffer that
/// diverges from `loaded_text` (the most recent disk read / write).
///
/// Strategy: line-level diff via `hiker_core::diff::compute`. Each Insert
/// in the diff is a line in `after` that has no exact counterpart in
/// `before`. We emit `DiffModified` when a Delete on the same after-line
/// preceded the Insert (i.e. a replace), otherwise `DiffAdded`. Pure
/// Deletes don't have a corresponding `after` line to mark, so we collapse
/// adjacent Delete-only runs onto the nearest following surviving line as
/// `DiffRemoved` (matches the legacy gutter behavior).
fn index_diff_decorations(
    loaded_text: &str,
    state: &editor_core::EditorState,
) -> editor_core::DecorationSet {
    use editor_core::{Decoration, GutterMarker, LineStyle, RangeSet};
    use hiker_core::diff::DiffOp;
    let live = state.doc.to_string();
    if loaded_text == live {
        return RangeSet::empty();
    }
    let diff = hiker_core::diff::compute(loaded_text, &live);
    // Per-after-line op summary. We mark each Insert line in the live
    // buffer; pure Deletes get pushed onto the next surviving line.
    let mut per_after_line: std::collections::BTreeMap<u32, GutterMarker> =
        std::collections::BTreeMap::new();
    let mut pending_delete = false;
    let mut last_after_seen: u32 = 0;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.op {
                DiffOp::Equal => {
                    if let Some(an) = line.after_line_no {
                        last_after_seen = an;
                        if pending_delete {
                            per_after_line.entry(an).or_insert(GutterMarker::DiffRemoved);
                            pending_delete = false;
                        }
                    }
                }
                DiffOp::Insert => {
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
                DiffOp::Delete => {
                    pending_delete = true;
                }
            }
        }
    }
    if pending_delete {
        // Trailing deletes at EOF — pin to the last seen line so the
        // marker is at least visible somewhere. Bias by 1 since lines are
        // 1-indexed but the rope is 0-indexed below.
        if last_after_seen > 0 {
            per_after_line
                .entry(last_after_seen)
                .or_insert(GutterMarker::DiffRemoved);
        }
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

fn status_bar(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Left: version dropdown — unifies the active buffer,
                // changelog snapshots, and any pending agent proposals so
                // the user can flip between them without leaving the
                // editor. Spec: status-bar version dropdown spanning the
                // unified activity feed.
                let basename = path.rsplit('/').next().unwrap_or(path);
                let label = basename.to_string();
                if app.session.buffers.get(path).map(|b| b.is_dirty()).unwrap_or(false) {
                    ui.add(icons::current_dot());
                }
                version_dropdown(ui, app, path, &label);

                // Center: index status from the indexer, optionally
                // followed by the heading breadcrumb when the per-buffer
                // toggle is on.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_space(20.0);
                    let label = {
                        let idx = app.vault_session.services.indexer.as_ref();
                        let s = idx.status();
                        if let Some(err) = &s.last_error {
                            format!("Index error: {}", err)
                        } else if !s.model_ready {
                            "Model loading…".to_string()
                        } else if s.queued > 0 {
                            format!("Indexing ({} queued)", s.queued)
                        } else {
                            format!("Indexed ({} notes)", s.total_notes)
                        }
                    };
                    ui.label(
                        egui::RichText::new(label)
                            .color(theme::muted())
                            .small(),
                    );
                    // Index-diff awareness (`compute_diff` legacy hook):
                    // when the indexer's stored content hash for this path
                    // diverges from the buffer's current hash, the user
                    // sees a "buffer ahead of index" badge so they know
                    // search hits may be stale.
                    // Coarse refresh of the indexer's stored hash for
                    // this buffer. The status bar runs every frame; a
                    // per-frame SQLite query + std-mutex lock against
                    // the shared read store contributes measurably to
                    // scroll latency. The hash only flips when the
                    // indexer commits a re-index, so a 2s refresh
                    // window is plenty responsive.
                    let path_owned = path.to_string();
                    let now = std::time::Instant::now();
                    let needs_refresh = app
                        .session.buffers
                        .get(&path_owned)
                        .map(|b| {
                            b.index_hash_refreshed_at
                                .map(|t| {
                                    now.duration_since(t)
                                        > std::time::Duration::from_secs(2)
                                })
                                .unwrap_or(true)
                        })
                        .unwrap_or(false);
                    if needs_refresh
                        && let Ok(store) = app.vault_session.services.read_store.lock()
                    {
                        let h = store
                            .note_properties(&path_owned)
                            .ok()
                            .flatten()
                            .and_then(|p| p.content_hash);
                        drop(store);
                        if let Some(buf) = app.session.buffers.get_mut(&path_owned) {
                            buf.index_hash_cache = h;
                            buf.index_hash_refreshed_at = Some(now);
                        }
                    }
                    if let Some(buffer) = app.session.buffers.get(&path_owned)
                        && let Some(idx_hash) = buffer.index_hash_cache.as_deref()
                        && idx_hash != buffer.current_hash()
                    {
                        ui.add_space(12.0);
                        ui.add(crate::icons::warn());
                        ui.label(
                            egui::RichText::new("buffer ahead of index")
                                .color(theme::warn())
                                .small(),
                        )
                        .on_hover_text(
                            "Live text differs from the most recently indexed version — \
                             save the buffer to refresh.",
                        );
                    }
                    if let Some(buffer) = app.session.buffers.get(path)
                        && buffer.heading_breadcrumb
                    {
                        let crumbs = heading_breadcrumb_for(buffer);
                        if !crumbs.is_empty() {
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new(format!(": {}", crumbs))
                                    .color(theme::muted())
                                    .small(),
                            );
                        }
                    }
                });

                // Right: line:col + word count + extension badge.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ext = path.rsplit('.').next().unwrap_or("");
                    ui.label(
                        egui::RichText::new(ext)
                            .color(theme::muted())
                            .small()
                            .monospace(),
                    );
                    if let Some(buffer) = app.session.buffers.get(path) {
                        let wc = buffer.current_text().split_whitespace().count();
                        ui.label(
                            egui::RichText::new(format!("{} words", wc))
                                .color(theme::muted())
                                .small(),
                        );
                        // Line:col — derived from main selection. Click to
                        // open a goto-line popover (per spec).
                        let main = buffer.editor.selection.main();
                        let line = buffer.editor.doc.byte_to_line(main.head.byte as usize) + 1;
                        let line_start = buffer.editor.doc.line_to_byte(line - 1);
                        let col = (main.head.byte as usize).saturating_sub(line_start) + 1;
                        let total_lines = buffer.editor.doc.len_lines().max(1);
                        let resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("Ln {}, Col {}", line, col))
                                    .color(theme::muted())
                                    .small(),
                            )
                            .sense(egui::Sense::click()),
                        );
                        let mut goto_target: Option<usize> = None;
                        egui::Popup::menu(&resp).show(|ui| {
                            ui.label(
                                egui::RichText::new("Go to line")
                                    .small()
                                    .strong(),
                            );
                            let mut draft = ui.ctx().data_mut(|d| {
                                d.get_persisted_mut_or_default::<String>(
                                    egui::Id::new(("goto-line", path.to_string())),
                                )
                                .clone()
                            });
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut draft)
                                    .hint_text(format!("1–{total_lines}"))
                                    .desired_width(80.0),
                            );
                            edit.request_focus();
                            ui.ctx().data_mut(|d| {
                                d.insert_persisted(
                                    egui::Id::new(("goto-line", path.to_string())),
                                    draft.clone(),
                                );
                            });
                            let go = edit.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if go
                                && let Ok(n) = draft.trim().parse::<usize>()
                                && n >= 1
                                && n <= total_lines
                            {
                                goto_target = Some(n - 1);
                                ui.close();
                            }
                        });
                        if let Some(zero_idx) = goto_target
                            && let Some(b) = app.session.buffers.get_mut(path)
                        {
                            let target_byte = b.editor.doc.line_to_byte(zero_idx);
                            b.editor.selection = editor_core::Selection::single(target_byte);
                        }
                    }
                });
            });
        });
}
