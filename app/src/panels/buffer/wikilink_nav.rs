//! Wikilink navigation glue for the buffer panel: building the live-title
//! resolver the decoration layer renders pills from, and turning a clicked
//! wikilink pill into an opened note. Kept beside `mod.rs` rather than inside
//! it so the editor render path stays readable and within its length budget;
//! everything here is the app-layer seam between `editor_md::links` (which
//! emits the clickable pills) and `core::wikilink` + the store (which resolve
//! a target to a concrete vault path).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use editor_view::viewport::{ClickAction, ClickZone};
use hiker_core::store::Store;
use hiker_core::wikilink::{self, AmbiguityPolicy, Resolution};

use crate::state::{AppState, ToastLevel};

/// Delay before a sustained hover surfaces the preview card. Mirrors the
/// "transient sweeps don't spam previews" rule in `wikilink-hover-preview`.
const HOVER_OPEN_DELAY: Duration = Duration::from_millis(400);
/// Grace window after the pointer leaves the pill (or the card) before the
/// card dismisses — lets the user slide the cursor from pill → card to scroll.
const HOVER_LEAVE_GRACE: Duration = Duration::from_millis(200);
/// Cap on how many leading body lines the card renders, per spec.
const PREVIEW_BODY_LINES: usize = 30;

/// Build the wikilink live-title resolver. Under the path-form
/// (`wikilink-path-form`) the target *is* the path-or-name the user
/// typed; the resolver hands back a display label by stripping `.md`
/// from the basename. A click-time resolver (below) does the actual
/// path lookup. The returned closure owns an `Arc` clone, so it
/// borrows neither `AppState` nor the active buffer.
///
/// The `read_store` borrow is kept on the signature for forward
/// compatibility with the frontmatter-title path (`wikilink-render`
/// resolves the target's current `title` frontmatter when set);
/// today the resolver doesn't read it yet.
///
/// status: wikilink-render
pub(crate) fn title_resolver(
    _read_store: Arc<Mutex<Store>>,
) -> impl Fn(&str) -> Option<String> {
    move |target: &str| Some(wikilink::title_for_path(target).to_string())
}

/// Dispatch this frame's wikilink pill clicks. Each tagged id carries the
/// link's full-span start byte; re-parse the link there against the active
/// buffer's current text and open the target. status: wikilink-click-open
pub(crate) fn handle_clicks(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    clicks: &[u64],
    mod_click: bool,
) {
    if clicks.is_empty() {
        return;
    }
    let Some(text) = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    for &id in clicks {
        let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
        // Per spec: Mod-click on a pill while its card is showing closes
        // the card and opens the target sticky (same as without the card).
        if mod_click {
            close_card(&mut app.panels.wikilink_hover);
        }
        open_at(app, &text, offset, mod_click);
    }
    ctx.request_repaint();
}

/// Resolve and open the wikilink whose full `[[…]]` span starts at `offset`
/// in `text`. Path-form (`wikilink-resolve`): a bare-name target matches
/// by basename; an explicit-path target matches by exact path. Ambiguity
/// policy is read from `[wikilinks] ambiguous_resolution`.
/// `sticky` (Mod-click) opens a sticky tab instead of the preview slot.
///
/// status: wikilink-click-open
fn open_at(app: &mut AppState, text: &str, offset: usize, sticky: bool) {
    let Some(link) = wikilink::parse_links(text)
        .into_iter()
        .find(|l| l.span.start == offset)
    else {
        return;
    };

    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    let policy = app
        .vault_session
        .config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(AmbiguityPolicy::Unresolved);
    let referrer = app
        .session
        .active_tab
        .and_then(|id| app.tab_by_id(id).and_then(|t| t.buffer_path().map(str::to_string)));
    match wikilink::resolve_path(&paths, &link.target, policy, referrer.as_deref()) {
        Resolution::Resolved(path) => crate::editor_pane::open_file(app, &path, sticky),
        Resolution::Ambiguous(_) => app.push_toast(
            format!(
                "Multiple notes named \u{201c}{}\u{201d} \u{2014} pick one via the [[ menu",
                link.target
            ),
            ToastLevel::Warn,
        ),
        Resolution::Unresolved => create_and_open(app, &link.target, sticky),
    }
}

/// Create a new note for an unresolved wikilink name and open it. Under
/// path-based identity (`wikilink-path-form`) the link the user typed
/// (a name) resolves to the new note's path on the next decoration rebuild;
/// no save-time rewrite. status: wikilink-unresolved
///
/// Routes through the indexer-driven `core::ops::file::create_at` (watcher
/// suppression + `IndexJob::Upsert`) rather than the bare `vault::create_note`
/// so the new note is indexed without a duplicate watcher-driven ingest — the
/// same discipline as the `+` new-item button.
fn create_and_open(app: &mut AppState, name: &str, sticky: bool) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return;
    }
    let rel = if trimmed.ends_with(".md") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.md")
    };
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let rel_owned = rel.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, "").await
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    match result {
        Ok(actual) => {
            app.file_tree_state.dir_cache.clear();
            app.push_toast(format!("Created {actual}"), ToastLevel::Info);
            crate::editor_pane::open_file(app, &actual, sticky);
        }
        Err(e) => app.push_toast(format!("Couldn't create {rel}: {e}"), ToastLevel::Error),
    }
}

// ---------------------------------------------------------------------------
// Hover preview card — [wikilink-hover-preview]
// ---------------------------------------------------------------------------

/// Hover lifecycle for the wikilink preview card. Owned at the app level
/// (one card at a time, regardless of which buffer the pill lives in)
/// and queried each frame from the buffer panel. Idle state is "no pill
/// under the pointer, nothing showing."
#[derive(Default)]
pub struct HoverState {
    /// Pill currently under the pointer (identified by the link's
    /// full-span start byte in its buffer), and the frame instant when
    /// the pointer entered it. `None` while the pointer is off every pill.
    hovered: Option<HoveredPill>,
    /// While `true`, paint the card and accept scroll input over it.
    showing: bool,
    /// The pill the showing card belongs to. Decoupled from `hovered`
    /// so the card survives the brief gap as the user slides the cursor
    /// from pill → card.
    shown_for: Option<HoveredPill>,
    /// Last instant the pointer was either over the showing card or its
    /// originating pill. The grace window dismisses the card once the
    /// pointer has been away from both for `HOVER_LEAVE_GRACE`.
    last_pointer_at: Option<Instant>,
    /// Cached body text for the currently showing card (post-frontmatter,
    /// capped at `PREVIEW_BODY_LINES`). Filled the first frame the card
    /// goes up so repeated frames don't re-read the file.
    cached_title: String,
    cached_body: String,
    /// Scroll offset (px) inside the card body. Clamped against the
    /// painter's reported `max_scroll_y` each frame.
    scroll_y: f32,
    /// The card's screen rect from the previous frame. Used to hit-test
    /// the pointer for grace-window refresh and scroll capture.
    shown_card_rect: Option<egui::Rect>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct HoveredPill {
    /// Identifies the buffer the pill lives in. Hashed `path` rather
    /// than a borrowed `&str` so the struct can sit on app state across
    /// frames without lifetime gymnastics.
    buffer_path_hash: u64,
    /// Full-span byte offset of `[[…]]` in the buffer text — the same
    /// id wikilink WidgetClicks carry (sans the tag bit).
    link_offset: usize,
    /// When the pointer first landed on this pill this hover-session.
    entered_at: Instant,
}

fn hash_path(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Per-frame entry point from the buffer panel. Reads any wikilink
/// pills the painter recorded as click zones, walks the hover state
/// machine, and (when armed) paints the preview card over the editor
/// on a tooltip-order layer.
///
/// `editor_rect` is the screen-space rect of the editor body — click
/// zones are widget-local, so we translate them into screen coords
/// here. `view` is borrowed read-only for `click_zones`; the buffer
/// text is read from `app.session.buffers` for body resolution.
pub(crate) fn track_hover(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    editor_rect: egui::Rect,
    click_zones: &[ClickZone],
) {
    let path_hash = hash_path(path);
    let now = Instant::now();
    let pointer = ctx.pointer_latest_pos();

    // Find which wikilink pill (if any) the pointer is over in this
    // buffer. Translate widget-local zone rects to screen coords.
    let pill_under_pointer: Option<(usize, egui::Rect)> = pointer.and_then(|p| {
        if !editor_rect.contains(p) {
            return None;
        }
        let lx = p.x - editor_rect.min.x;
        let ly = p.y - editor_rect.min.y;
        click_zones.iter().find_map(|z| {
            let id = match z.action {
                ClickAction::WidgetClick(id) => id,
                _ => return None,
            };
            if id & editor_md::links::WIKILINK_WIDGET_TAG == 0 {
                return None;
            }
            if !z.rect.contains(lx, ly) {
                return None;
            }
            let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
            let screen = egui::Rect::from_min_max(
                egui::pos2(editor_rect.min.x + z.rect.x_min, editor_rect.min.y + z.rect.y_min),
                egui::pos2(editor_rect.min.x + z.rect.x_max, editor_rect.min.y + z.rect.y_max),
            );
            Some((offset, screen))
        })
    });

    // Phase 1: update hover-on-pill tracking. Scoped `hs` borrow so the
    // promote step below can re-borrow `app` for `resolve_preview`.
    {
        let hs = &mut app.panels.wikilink_hover;
        match pill_under_pointer {
            Some((offset, _rect)) => {
                let entering = !matches!(
                    hs.hovered,
                    Some(h) if h.buffer_path_hash == path_hash && h.link_offset == offset,
                );
                if entering {
                    hs.hovered = Some(HoveredPill {
                        buffer_path_hash: path_hash,
                        link_offset: offset,
                        entered_at: now,
                    });
                }
                hs.last_pointer_at = Some(now);
            }
            None => {
                hs.hovered = None;
            }
        }
    }

    // Pointer-over-card hit-test from the previously-recorded card rect.
    let pointer_over_card = pointer
        .zip(app.panels.wikilink_hover.shown_card_rect)
        .map(|(p, r)| r.contains(p))
        .unwrap_or(false);
    if app.panels.wikilink_hover.showing && pointer_over_card {
        app.panels.wikilink_hover.last_pointer_at = Some(now);
    }

    // Phase 2: promote to "showing" once the timer expires on a steady
    // hover. `resolve_preview` reads `app` immutably, so the hover state
    // borrow must drop before we call it.
    let promote: Option<HoveredPill> = {
        let hs = &app.panels.wikilink_hover;
        if !hs.showing
            && let Some(h) = hs.hovered
            && now.saturating_duration_since(h.entered_at) >= HOVER_OPEN_DELAY
        {
            Some(h)
        } else {
            None
        }
    };
    if let Some(h) = promote {
        if let Some((title, body)) = resolve_preview(app, path, h.link_offset) {
            let hs = &mut app.panels.wikilink_hover;
            hs.cached_title = title;
            hs.cached_body = body;
            hs.scroll_y = 0.0;
            hs.shown_for = Some(h);
            hs.showing = true;
        } else {
            // Don't keep retrying every frame for an unresolved pill.
            // Clear `hovered` so we only revisit when the user leaves
            // and re-enters the pill.
            app.panels.wikilink_hover.hovered = None;
        }
    }

    // Phase 3: dismiss after grace if neither pill nor card is under
    // the pointer.
    {
        let hs = &mut app.panels.wikilink_hover;
        if hs.showing {
            let still_on_origin = hs
                .hovered
                .zip(hs.shown_for)
                .map(|(a, b)| a == b)
                .unwrap_or(false);
            let alive = still_on_origin || pointer_over_card;
            if alive {
                hs.last_pointer_at = Some(now);
            } else if let Some(last) = hs.last_pointer_at
                && now.saturating_duration_since(last) >= HOVER_LEAVE_GRACE
            {
                close_card(hs);
            }
        }
    }

    // Phase 4: paint the card if we're showing one anchored in this buffer.
    let shown_for = app.panels.wikilink_hover.shown_for;
    if app.panels.wikilink_hover.showing
        && let Some(h) = shown_for
        && h.buffer_path_hash == path_hash
    {
        // Anchor at the pill's current screen rect if it's still in the
        // viewport (it almost always is — the user is hovering it).
        // Fall back to the pointer position otherwise so the card has
        // *some* anchor instead of vanishing when the pill scrolls off.
        let anchor = click_zones
            .iter()
            .find_map(|z| match z.action {
                ClickAction::WidgetClick(id)
                    if id & editor_md::links::WIKILINK_WIDGET_TAG != 0
                        && (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize
                            == h.link_offset =>
                {
                    Some(egui::pos2(
                        editor_rect.min.x + z.rect.x_min,
                        editor_rect.min.y + z.rect.y_max,
                    ))
                }
                _ => None,
            })
            .or_else(|| pointer.map(|p| egui::pos2(p.x, p.y + 4.0)));

        if let Some(anchor) = anchor {
            // Eat scroll wheel input when the pointer is in the card so
            // the editor underneath doesn't scroll along with the card body.
            let scroll_delta_y = if pointer_over_card {
                let dy = ctx.input(|i| i.smooth_scroll_delta.y);
                if dy != 0.0 {
                    ctx.input_mut(|i| i.smooth_scroll_delta.y = 0.0);
                }
                -dy
            } else {
                0.0
            };

            let layer = egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("wikilink-hover-preview"),
            );
            let painter = ctx.layer_painter(layer);
            let title = app.panels.wikilink_hover.cached_title.clone();
            let body = app.panels.wikilink_hover.cached_body.clone();
            let scroll_y_in = app.panels.wikilink_hover.scroll_y + scroll_delta_y;
            let geom = crate::panels::graph::paint_preview_card_with(
                &painter,
                editor_rect,
                &title,
                &body,
                anchor,
                scroll_y_in,
            );
            let hs = &mut app.panels.wikilink_hover;
            if let Some(geom) = geom {
                hs.scroll_y = scroll_y_in.clamp(0.0, geom.max_scroll_y);
                hs.shown_card_rect = Some(geom.card_rect);
                // Repaint while the card is up so the grace-timer logic
                // gets a chance to dismiss it once the pointer leaves.
                ctx.request_repaint();
            } else {
                hs.shown_card_rect = None;
            }
        }
    } else {
        app.panels.wikilink_hover.shown_card_rect = None;
    }

    // While the pill is hovered but the card hasn't appeared yet, ask
    // for a repaint at the open-delay so we surface on time even when
    // the user has stopped moving the cursor (egui only repaints on
    // input by default).
    if !app.panels.wikilink_hover.showing && app.panels.wikilink_hover.hovered.is_some() {
        ctx.request_repaint_after(HOVER_OPEN_DELAY);
    }
}

/// Close the card and forget the originating pill, leaving `hovered`
/// alone. Called on grace-timeout and on a Mod-click that opened the
/// target sticky (per spec: "Mod-click … closes the card and opens the
/// target sticky").
pub(crate) fn close_card(hs: &mut HoverState) {
    hs.showing = false;
    hs.shown_for = None;
    hs.shown_card_rect = None;
    hs.cached_title.clear();
    hs.cached_body.clear();
    hs.scroll_y = 0.0;
    hs.last_pointer_at = None;
}

/// Resolve the wikilink at `offset` in the active buffer to a `(title,
/// body)` pair, or `None` for unresolved / ambiguous links (no preview).
fn resolve_preview(app: &AppState, path: &str, offset: usize) -> Option<(String, String)> {
    let text = app.session.buffers.get(path).map(crate::buffer::Buffer::current_text)?;
    let link = wikilink::parse_links(&text)
        .into_iter()
        .find(|l| l.span.start == offset)?;
    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    let policy = app
        .vault_session
        .config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(AmbiguityPolicy::Unresolved);
    let referrer = Some(path);
    let resolved = match wikilink::resolve_path(&paths, &link.target, policy, referrer) {
        Resolution::Resolved(p) => p,
        Resolution::Unresolved | Resolution::Ambiguous(_) => return None,
    };
    let source = app.vault_session.vault.read_file(&resolved).ok()?;
    Some(preview_from_source(&resolved, &source))
}

/// Extract a `(title, body)` pair from a note's on-disk source. Title is
/// the frontmatter `title` if present, else the basename without `.md`.
/// Body is the post-frontmatter content, cropped to the first
/// `PREVIEW_BODY_LINES` lines.
fn preview_from_source(rel_path: &str, source: &str) -> (String, String) {
    let title = frontmatter_title(source)
        .unwrap_or_else(|| wikilink::title_for_path(rel_path).to_string());
    let body_full = crate::panels::graph::skip_frontmatter(source);
    let mut body = String::new();
    for (i, line) in body_full.lines().take(PREVIEW_BODY_LINES).enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(line);
    }
    (title, body)
}

/// Pull a `title:` field out of a YAML frontmatter block. Cheap enough
/// to scan inline rather than depend on a YAML parser — the preview
/// card only needs the human-display title, so unquoted scalar values
/// (`title: My Note`) and the two flavors of quoted form
/// (`"My Note"`, `'My Note'`) are what we recognize. Anything fancier
/// falls through to the basename fallback.
fn frontmatter_title(source: &str) -> Option<String> {
    let trimmed = source.trim_start_matches('\u{feff}');
    let rest = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))?;
    // Walk lines up to the closing `---` fence; first matching `title:`
    // wins. Stop at the fence so a body `# title:` heading doesn't leak.
    for line in rest.lines() {
        let t = line.trim_end();
        if t == "---" || t == "..." {
            break;
        }
        if let Some(rest) = line.strip_prefix("title:") {
            let v = rest.trim();
            if v.is_empty() {
                return None;
            }
            // Strip a matching pair of surrounding quotes if present.
            let bytes = v.as_bytes();
            if bytes.len() >= 2
                && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                    || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
            {
                return Some(v[1..v.len() - 1].to_string());
            }
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_title_from_frontmatter() {
        let src = "---\ntitle: Hello World\nid: 42\n---\nbody line 1\nbody line 2\n";
        let (title, body) = preview_from_source("notes/foo.md", src);
        assert_eq!(title, "Hello World");
        assert!(body.starts_with("body line 1"));
        assert!(body.contains("body line 2"));
    }

    #[test]
    fn preview_title_falls_back_to_basename() {
        let src = "no frontmatter here\nsecond line\n";
        let (title, body) = preview_from_source("notes/foo.md", src);
        assert_eq!(title, "foo");
        assert!(body.starts_with("no frontmatter"));
    }

    #[test]
    fn preview_body_caps_at_thirty_lines() {
        let mut src = String::new();
        for i in 0..50 {
            src.push_str(&format!("line {i}\n"));
        }
        let (_title, body) = preview_from_source("a.md", &src);
        assert_eq!(body.lines().count(), PREVIEW_BODY_LINES);
        assert!(body.contains("line 0"));
        assert!(body.contains("line 29"));
        assert!(!body.contains("line 30"));
    }

    #[test]
    fn preview_title_strips_quotes() {
        let src = "---\ntitle: \"Quoted\"\n---\nbody\n";
        let (title, _) = preview_from_source("x.md", src);
        assert_eq!(title, "Quoted");
    }
}
