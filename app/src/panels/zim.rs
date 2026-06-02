//! ZIM viewer tab: renders an offline `.zim` archive (e.g. a Wikipedia
//! export) as HTML through the from-scratch `hiker-htmlview` renderer.
//!
//! The renderer is host-driven: it parses, styles, and lays out HTML and
//! paints into an `egui::Painter`, while *this* module owns the scroll area,
//! clipping, and input. The archive is parsed by `zxr` (which also provides
//! full-text body search). `core` is entirely uninvolved.
//!
//! Navigation: we hit-test the pointer against the laid-out document on click
//! ([`HtmlView::link_at`]); a hit returns the link's href, which we resolve to
//! a content article in the same archive and reload the view in place (and
//! update the tab payload so the article survives tab bookkeeping).
//!
//! Subresources (in-archive images / CSS): served through a ZIM-backed
//! [`hiker_htmlview::ResourceProvider`] ([`SubresourceProvider`]). When the
//! renderer lays out an article it resolves `<link rel=stylesheet>` /
//! `<img src>` URLs against the article's `zim://` base; the provider maps
//! those back to archive entries (by namespace + url, following redirects) and
//! returns the bytes — entirely offline, never the network. So external CSS
//! and images that live as other archive entries render. The ZIM glue lives
//! here in `app`; `hiker-htmlview` stays archive-agnostic.
//!
//! status: zim-view

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use eframe::egui;

use hiker_htmlview::{HtmlView, ResourceProvider, Theme};
use zxr::zim::Zim;

use crate::state::{AppState, ToastLevel};
use crate::tab::{Tab, TabId, TabKind};

/// Base URL stamped on every article so the renderer resolves relative
/// subresource references against a stable `zim://` origin. `zim` is the
/// authority and
/// `C` the content-namespace dir, so an article link `Foo` becomes
/// `zim://zim/C/Foo` and a `../I/img.png` reference becomes
/// `zim://zim/I/img.png` — the leading path segment is the ZIM namespace.
const ZIM_BASE_URL: &str = "zim://zim/C/";

/// A ZIM-backed [`ResourceProvider`]: resolves `zim://` subresource URLs
/// (CSS / images referenced by an article) against an opened archive.
///
/// `hiker-htmlview` calls [`fetch`](ResourceProvider::fetch) synchronously
/// during layout with the absolute URL (already resolved against
/// [`ZIM_BASE_URL`]).
/// We parse the namespace + entry path out of that URL and look it up in the
/// archive. `zxr::Zim` is memmap-backed and `Send + Sync`, and `fetch` only
/// needs `&self`, so the provider holds the archive directly (no `Mutex`); it
/// opens its own handle to the same `.zim` so it never contends with the
/// viewer pane's archive.
pub struct SubresourceProvider {
    archive: Zim,
}

impl SubresourceProvider {
    /// Open a provider over the `.zim` at `abs` (absolute path). Returns
    /// `None` if the archive can't be opened — the view then renders without
    /// served subresources rather than failing.
    fn open(abs: &std::path::Path) -> Option<Self> {
        Zim::open(abs).ok().map(|archive| Self { archive })
    }
}

impl ResourceProvider for SubresourceProvider {
    fn fetch(&self, url: &str) -> Option<(Vec<u8>, String)> {
        let (namespace, entry_url) = parse_zim_url(url)?;
        // Try the parsed namespace first, then the common content/image/style
        // namespaces so both the legacy (`A`/`-`/`I`) and newer (`C`/`M`)
        // layouts resolve without the caller knowing which a given archive
        // uses.
        for ns in [namespace, b'C', b'I', b'-', b'A', b'M'] {
            if let Some(found) = self.archive.get_by_url(ns, &entry_url) {
                return Some(found);
            }
        }
        None
    }
}

/// Fallback provider used when the archive can't be reopened for subresources:
/// it serves nothing, so the article renders without external CSS/images
/// rather than failing to construct the view.
struct NullProvider;

impl ResourceProvider for NullProvider {
    fn fetch(&self, _url: &str) -> Option<(Vec<u8>, String)> {
        None
    }
}

/// Parse a `zim://zim/<NS>/<url>` subresource URL into `(namespace, url)`.
/// The first path segment is the single-char ZIM namespace; the remainder
/// (percent-decoded) is the entry url. Returns `None` for non-`zim` URLs.
///
/// The renderer resolves a relative `<link>` / `<img>` href by plain string
/// concatenation against the article's base (`zim://zim/C/`), so a reference
/// like `../-/style.css` arrives here as `zim://zim/C/../-/style.css` — the
/// `..`/`.` segments are NOT collapsed by the renderer. We normalize them
/// before splitting off the namespace, so `C/../-/style.css` resolves to
/// namespace `-`, entry `style.css` (the article's real content namespace is
/// `C`, and `..` walks up out of it). Without this the namespace is misread as
/// `C` and the lookup misses — articles then render unstyled (e.g. MediaWiki
/// tables fall back to their `width="100%"` attribute and span full width).
fn parse_zim_url(url: &str) -> Option<(u8, String)> {
    let rest = url.strip_prefix("zim://")?;
    // Drop the authority ("zim") — everything after the first '/'.
    let path = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
    let path = path.split('#').next().unwrap_or("");
    let path = path.split('?').next().unwrap_or("");
    let path = normalize_dot_segments(path);
    let (ns_seg, entry) = path.split_once('/')?;
    let namespace = *ns_seg.as_bytes().first()?;
    Some((namespace, percent_decode(entry)))
}

/// Collapse `.` / `..` (and empty) path segments, RFC-3986-style: `.` and
/// empty segments are dropped, `..` pops the previous kept segment. Used to
/// resolve the un-normalized relative URLs the HTML renderer hands the
/// provider (it concatenates rather than resolving). `C/../-/style.css` →
/// `-/style.css`; an already-clean `I/img.png` is returned unchanged.
fn normalize_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

// ZIM panes hold a `hiker_htmlview::HtmlView`, which is `!Send` (it caches
// egui types and a stylo-styled document, and stylo's style engine isn't
// thread-safe). `AppState` must stay `Send` (it's moved across the runtime
// during vault switches), so the panes can't live in it. They only ever touch
// the UI thread, so we park them in a thread-local store keyed by tab id
// instead. status: zim-view
thread_local! {
    static PANES: RefCell<HashMap<TabId, Pane>> = RefCell::new(HashMap::new());
}

/// How the ZIM article's color scheme is chosen. `Auto` follows the app's
/// egui theme (dark app → dark articles); `Light`/`Dark` pin it. The widget
/// itself only knows `Theme` (Light/Dark) — the Auto→theme mapping is
/// owned here in `app` (see [`show`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    Auto,
    Light,
    Dark,
}

/// Per-ZIM-tab state: the opened archive + the `hiker-htmlview` render widget
/// + the article currently shown. Lives in the thread-local `PANES` store (see
/// above), keyed by tab id.
pub struct Pane {
    /// The opened archive, shared (`Arc`) so the in-tab title picker can run
    /// `title_search` on a background thread (`title-picker-async`) without
    /// reopening the file; the UI thread uses the same handle for rendering.
    archive: Result<Arc<Zim>, String>,
    view: HtmlView,
    /// The article currently loaded into `view` (`None` = main page). Used
    /// to avoid reloading the same article every frame.
    loaded: Option<String>,
    /// Set once we've done the initial load for this pane.
    initialized: bool,
    /// Color-scheme choice for the rendered article (default [`Auto`], so a
    /// dark-themed app shows dark articles). Drives `view.set_theme`.
    theme: ThemeChoice,
    /// Content zoom for the rendered article (`1.0` = unzoomed). Drives
    /// `view.set_zoom`.
    zoom: f32,
    /// In-tab "Jump to" title picker: query + debounced, background-run hits
    /// (see [`title_picker`]).
    picker: TitlePicker,
}

/// Max title hits shown in the in-tab article picker.
const PICKER_LIMIT: usize = 30;
/// Quiet window before a title-picker keystroke fires a search, so rapid
/// typing collapses into one `title_search` instead of one per keystroke.
const PICKER_DEBOUNCE_MS: u64 = 150;

/// State for the in-tab "Jump to" title picker. The `title_search` runs on a
/// background `spawn_blocking` task (it touches the archive's memmap, which can
/// fault to disk on a large `.zim`); results return over the channel tagged
/// with the `epoch` they were fired at, so stale hits from superseded typing
/// are dropped. Lives on the (thread-local, `!Send`) [`Pane`], so the receiver
/// needs no `Mutex`. [title-picker-async]
struct TitlePicker {
    query: String,
    /// `(title, content-url)` hits for the last completed search.
    hits: Vec<(String, String)>,
    /// Last query a search was *scheduled* for, so an unchanged query doesn't
    /// reschedule every frame.
    last_query: String,
    /// Monotonic fire counter; a returning search whose epoch is stale is
    /// dropped.
    epoch: u64,
    /// Debounce deadline: set on a query change, cleared when the search fires.
    pending_at: Option<std::time::Instant>,
    tx: std::sync::mpsc::Sender<(u64, Vec<(String, String)>)>,
    rx: std::sync::mpsc::Receiver<(u64, Vec<(String, String)>)>,
}

impl TitlePicker {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        TitlePicker {
            query: String::new(),
            hits: Vec::new(),
            last_query: String::new(),
            epoch: 0,
            pending_at: None,
            tx,
            rx,
        }
    }
}

impl Pane {
    fn open_archive(app: &AppState, zim_path: &str) -> Self {
        let abs = app.vault_session.vault_root.join(zim_path);
        // `Arc` so the title picker can hand a clone to a background search
        // task; the viewer renders off the same handle on the UI thread.
        let archive = Zim::open(&abs).map(Arc::new).map_err(|e| e.to_string());
        // Wire a ZIM-backed subresource provider (CSS / images) over its own
        // handle to the same archive, so styling/images render offline. If the
        // archive can't be reopened, fall back to a provider that serves
        // nothing rather than failing to build the view.
        let provider: Arc<dyn ResourceProvider> = match SubresourceProvider::open(&abs) {
            Some(p) => Arc::new(p),
            None => Arc::new(NullProvider),
        };
        // Construct over empty HTML; the first `show` loads the real article
        // via `set_html`. The base URL + provider are fixed for the pane's life.
        let view = HtmlView::new("", Some(ZIM_BASE_URL), provider);
        Pane {
            archive,
            view,
            loaded: None,
            initialized: false,
            theme: ThemeChoice::Auto,
            zoom: 1.0,
            picker: TitlePicker::new(),
        }
    }
}

/// Open a ZIM viewer tab for `zim_path` (vault-relative) on its main page.
/// Focuses an existing tab for the archive if one's open; otherwise opens it
/// as a **preview** tab (like opening a note) — reusing the shared preview
/// slot, promotable by double-clicking the tab. Records the visit on the
/// global nav stack so Back/Forward include it. [zim-view-preview-tab]
pub fn open(app: &mut AppState, zim_path: &str) -> TabId {
    let id = match find_archive_tab(app, zim_path) {
        Some(id) => {
            app.session.active_tab = Some(id);
            id
        }
        None => open_preview(app, zim_path, None),
    };
    let article = current_article(app, id);
    push_nav(app, zim_path, &article);
    id
}

/// Find an open ZIM tab for `zim_path` (any article), if any.
fn find_archive_tab(app: &AppState, zim_path: &str) -> Option<TabId> {
    app.session
        .tabs
        .iter()
        .find(|t| matches!(&t.kind, TabKind::ZimView { zim_path: p, .. } if p == zim_path))
        .map(|t| t.id)
}

/// The article currently shown by ZIM tab `id` (`None` if not a ZIM tab).
fn current_article(app: &AppState, id: TabId) -> Option<String> {
    match app.tab_by_id(id).map(|t| &t.kind) {
        Some(TabKind::ZimView { article, .. }) => article.clone(),
        _ => None,
    }
}

/// Push a `ZimArticle` entry onto the global nav stack, unless we're mid
/// Back/Forward (`nav.locked`). [zim-nav-stack]
fn push_nav(app: &mut AppState, zim_path: &str, article: &Option<String>) {
    if !app.session.nav.locked {
        app.session.nav.push(crate::state::NavTarget::ZimArticle {
            zim_path: zim_path.to_string(),
            article: article.clone(),
        });
    }
}

/// Open `(zim_path, article)` as a **preview** tab: reuse the shared preview
/// slot if one's open (else allocate a fresh non-sticky tab), and make it
/// active. Does not touch the nav stack — callers push. Panes are keyed by tab
/// id, so when the reused slot held a *different* archive we drop its stale
/// pane first, forcing `show` to rebuild for the new one. [zim-view-preview-tab]
fn open_preview(app: &mut AppState, zim_path: &str, article: Option<String>) -> TabId {
    let kind = TabKind::ZimView { zim_path: zim_path.to_string(), article };
    if let Some(prev_id) = app.session.preview_tab {
        let stale_archive = app
            .tab_by_id(prev_id)
            .is_some_and(|t| matches!(&t.kind, TabKind::ZimView { zim_path: p, .. } if p != zim_path));
        if stale_archive {
            forget(prev_id);
        }
        if let Some(tab) = app.tab_by_id_mut(prev_id) {
            tab.kind = kind;
            tab.sticky = false;
        }
        app.session.active_tab = Some(prev_id);
        return prev_id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab { id, kind, sticky: false });
    app.session.active_tab = Some(id);
    app.session.preview_tab = Some(id);
    id
}

/// Navigate to `(zim_path, article)` from a link click / picker pick inside
/// `from_tab`, applying note-style preview semantics: a **preview** tab
/// navigates in place (links replace it); a **pinned** tab opens the target as
/// a new preview tab (links don't disturb the pinned article). Records the
/// visit on the global nav stack. [zim-link-preview-open]
fn navigate_within(app: &mut AppState, from_tab: TabId, zim_path: &str, article: &Option<String>) {
    let pinned = app.tab_by_id(from_tab).is_some_and(|t| t.sticky);
    if pinned {
        open_preview(app, zim_path, article.clone());
    } else {
        if let Some(tab) = app.tab_by_id_mut(from_tab) {
            if let TabKind::ZimView { article: a, .. } = &mut tab.kind {
                *a = article.clone();
            }
        }
        app.session.active_tab = Some(from_tab);
    }
    push_nav(app, zim_path, article);
}

/// Apply a Back/Forward landing on a [`NavTarget::ZimArticle`]: navigate an
/// existing tab for `zim_path` in place (preferring the active tab) or open one
/// as a preview if none is open. Runs under `nav.locked`, so it never
/// re-pushes. [zim-nav-stack]
pub fn restore_nav(app: &mut AppState, zim_path: &str, article: Option<String>) {
    let target = app
        .session
        .active_tab
        .filter(|&id| {
            app.tab_by_id(id)
                .is_some_and(|t| matches!(&t.kind, TabKind::ZimView { zim_path: p, .. } if p == zim_path))
        })
        .or_else(|| find_archive_tab(app, zim_path));
    match target {
        Some(id) => {
            if let Some(tab) = app.tab_by_id_mut(id) {
                if let TabKind::ZimView { article: a, .. } = &mut tab.kind {
                    *a = article;
                }
            }
            app.session.active_tab = Some(id);
        }
        None => {
            open_preview(app, zim_path, article);
        }
    }
}

/// Render the ZIM viewer tab body.
pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    zim_path: &str,
    article: &Option<String>,
) {
    crate::profile_function!();
    let want = article.clone();
    let mut nav_to: Option<Option<String>> = None;
    let mut toast: Option<String> = None;

    PANES.with(|panes| {
        let mut panes = panes.borrow_mut();
        // Lazily open this tab's archive.
        let pane = panes
            .entry(tab_id)
            .or_insert_with(|| Pane::open_archive(app, zim_path));

        match &pane.archive {
            Err(e) => {
                ui.colored_label(
                    egui::Color32::from_rgb(200, 60, 60),
                    format!("Failed to open ZIM archive: {e}"),
                );
            }
            Ok(archive) => {
                // View options (theme + zoom) menu, then the article picker.
                view_options_menu(ui, &mut pane.theme, &mut pane.zoom);

                // Article picker: title-prefix search within this archive.
                // A click navigates the view (mirrors a link click).
                if let Some(url) = title_picker(ui, archive, &mut pane.picker) {
                    nav_to = Some(Some(url));
                }

                // Feed the chosen theme + zoom into the widget every frame.
                // For `Auto` we map the app's egui theme to a renderer `Theme`
                // here (the widget stays theme-agnostic). The setters no-op
                // unless the value actually changed.
                let theme = match pane.theme {
                    ThemeChoice::Light => Theme::Light,
                    ThemeChoice::Dark => Theme::Dark,
                    ThemeChoice::Auto => {
                        if ui.visuals().dark_mode {
                            Theme::Dark
                        } else {
                            Theme::Light
                        }
                    }
                };
                pane.view.set_theme(theme);
                pane.view.set_zoom(pane.zoom);

                // (Re)load the requested article when it changed / on first
                // show.
                if !pane.initialized || pane.loaded != want {
                    match load_article(archive, want.as_deref()) {
                        Ok(html) => {
                            // Base URL + provider are set once at pane open
                            // (see `open_archive`); just swap the HTML.
                            pane.view.set_html(&html);
                            pane.loaded = want.clone();
                        }
                        Err(e) => {
                            toast = Some(format!("ZIM article not found: {e}"));
                        }
                    }
                    pane.initialized = true;
                }

                // Draw the article + capture link clicks. The renderer is
                // host-driven: we own the scroll area, allocate the laid-out
                // content rect, paint into a clipped painter, and hit-test the
                // pointer against the document ourselves.
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let width = ui.available_width();
                        let size = {
                            crate::profile_scope!("zim::layout");
                            pane.view.layout(ui.ctx(), width)
                        };
                        let (rect, response) =
                            ui.allocate_exact_size(size, egui::Sense::click());
                        // `rect.min` is the scroll-translated document origin.
                        let painter = ui.painter_at(rect);
                        {
                            crate::profile_scope!("zim::paint", format!("{} shapes", pane.view.shape_count()));
                            pane.view.paint(&painter, rect.min, painter.clip_rect());
                        }

                        // Pointer-over-link → hand cursor.
                        if let Some(pointer) = response.hover_pos() {
                            let doc_point = pointer - rect.min.to_vec2();
                            if pane.view.is_link_at(doc_point) {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                        }
                        // Click on a link → resolve + navigate in place.
                        // Pure-fragment / unsupported links resolve to `None`
                        // and are ignored.
                        if response.clicked() {
                            if let Some(pointer) = response.interact_pointer_pos() {
                                let doc_point = pointer - rect.min.to_vec2();
                                if let Some(href) = pane.view.link_at(doc_point) {
                                    if let Some(target) = resolve_href(&href) {
                                        nav_to = Some(Some(target));
                                    }
                                }
                            }
                        }
                    });
            }
        }
    });

    if let Some(msg) = toast {
        app.push_toast(msg, ToastLevel::Warn);
    }

    // Apply navigation from a link click / picker pick: preview tabs navigate
    // in place, pinned tabs open the target as a new preview tab, and the
    // visit lands on the global nav stack so Back/Forward walk it.
    // [zim-link-preview-open, zim-nav-stack]
    if let Some(new_article) = nav_to {
        navigate_within(app, tab_id, zim_path, &new_article);
        ui.ctx().request_repaint();
    }
}

/// Render the in-tab article picker: a search field that title-prefix
/// searches this archive (binary search over the title index, bounded to
/// [`PICKER_LIMIT`]) and lists matching titles. Returns the content url of a
/// clicked result, for the caller to navigate to.
///
/// The search runs off the UI thread (`title-picker-async`): a query change
/// schedules a debounced [`PICKER_DEBOUNCE_MS`] search that `spawn_blocking`
/// runs against an `Arc<Zim>` clone (the lookup touches the memmap and can
/// fault to disk on a large archive). Hits return over the picker's channel
/// tagged with the epoch they were fired at; stale results from superseded
/// typing are dropped. So typing stays smooth no matter how big the `.zim` is.
fn title_picker(
    ui: &mut egui::Ui,
    archive: &Arc<Zim>,
    picker: &mut TitlePicker,
) -> Option<String> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label("Jump to:");
        ui.add(
            egui::TextEdit::singleline(&mut picker.query)
                .hint_text("article title…")
                .desired_width(ui.available_width()),
        );
    });

    // Apply the most recent finished search that still matches the current
    // epoch; drop any superseded by newer typing.
    let mut newest: Option<Vec<(String, String)>> = None;
    while let Ok((epoch, hits)) = picker.rx.try_recv() {
        if epoch == picker.epoch {
            newest = Some(hits);
        }
    }
    if let Some(hits) = newest {
        picker.hits = hits;
    }

    // A query change (re)arms the debounce. An empty query clears immediately
    // and bumps the epoch so any in-flight search is discarded on return.
    if picker.query != picker.last_query {
        picker.last_query = picker.query.clone();
        if picker.query.trim().is_empty() {
            picker.hits.clear();
            picker.pending_at = None;
            picker.epoch = picker.epoch.wrapping_add(1);
        } else {
            picker.pending_at = Some(std::time::Instant::now());
        }
    }

    // Fire once the debounce window closes; otherwise schedule a repaint so it
    // fires even if the user stops interacting.
    if let Some(deadline) = picker.pending_at {
        let elapsed = deadline.elapsed().as_millis() as u64;
        if elapsed >= PICKER_DEBOUNCE_MS {
            picker.pending_at = None;
            picker.epoch = picker.epoch.wrapping_add(1);
            let epoch = picker.epoch;
            let query = picker.query.trim().to_string();
            let tx = picker.tx.clone();
            let archive = Arc::clone(archive);
            let egui_ctx = ui.ctx().clone();
            let job = move || {
                let hits = archive.title_search(&query, PICKER_LIMIT);
                if tx.send((epoch, hits)).is_ok() {
                    egui_ctx.request_repaint();
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn_blocking(job);
                }
                Err(_) => job(),
            }
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(
                PICKER_DEBOUNCE_MS.saturating_sub(elapsed),
            ));
        }
    }

    if !picker.hits.is_empty() {
        egui::ScrollArea::vertical()
            .max_height(160.0)
            .id_salt("zim-title-picker")
            .show(ui, |ui| {
                for (title, url) in picker.hits.iter() {
                    if ui.selectable_label(false, title).clicked() {
                        picked = Some(url.clone());
                    }
                }
            });
        ui.separator();
    }
    // A click navigates; clear the query so the picker collapses.
    if picked.is_some() {
        picker.query.clear();
        picker.last_query.clear();
        picker.hits.clear();
        picker.pending_at = None;
    }
    picked
}

/// Bounds the article zoom (matches the spec's 0.5–2.0 range).
const ZIM_ZOOM_MIN: f32 = 0.5;
const ZIM_ZOOM_MAX: f32 = 2.0;
/// Step applied by the − / + zoom buttons.
const ZIM_ZOOM_STEP: f32 = 0.1;

/// Render the per-tab view-options menu: a "View" button opening a small menu
/// with a Theme (Light / Dark / Auto) radio group and a Zoom (− / reset / +)
/// row. Mutates the pane's `theme` / `zoom` in place; the caller maps those
/// into the widget (incl. the `Auto`→app-theme mapping). The widget itself
/// stays generic — it only ever sees a concrete `ColorScheme` + zoom.
fn view_options_menu(ui: &mut egui::Ui, theme: &mut ThemeChoice, zoom: &mut f32) {
    ui.horizontal(|ui| {
        ui.menu_button("View", |ui| {
            ui.label("Theme");
            ui.radio_value(theme, ThemeChoice::Auto, "Auto");
            ui.radio_value(theme, ThemeChoice::Light, "Light");
            ui.radio_value(theme, ThemeChoice::Dark, "Dark");
            ui.separator();
            ui.label("Zoom");
            ui.horizontal(|ui| {
                if ui.button("−").clicked() {
                    *zoom = (*zoom - ZIM_ZOOM_STEP).max(ZIM_ZOOM_MIN);
                }
                if ui.button("reset").clicked() {
                    *zoom = 1.0;
                }
                if ui.button("+").clicked() {
                    *zoom = (*zoom + ZIM_ZOOM_STEP).min(ZIM_ZOOM_MAX);
                }
            });
            ui.add(
                egui::Slider::new(zoom, ZIM_ZOOM_MIN..=ZIM_ZOOM_MAX)
                    .text("scale"),
            );
        });
    });
}

/// Drop a closed tab's pane, freeing its archive + render texture. Call when
/// a ZIM tab closes so panes don't leak across the session.
pub fn forget(tab_id: TabId) {
    PANES.with(|panes| {
        panes.borrow_mut().remove(&tab_id);
    });
}

/// Drop every open ZIM pane (and the registry's archive handles) during
/// controlled app shutdown.
///
/// A `Pane` holds a `hiker_htmlview::HtmlView` (caching egui textures + a
/// stylo-styled document) and a memmap-backed `Zim`. Calling this from the
/// app's `on_exit` releases those textures and archive handles deterministically
/// while the main thread's TLS is still alive, rather than leaving them to be
/// torn down implicitly during process exit.
pub fn shutdown() {
    PANES.with(|panes| panes.borrow_mut().clear());
    registry().archives.clear();
}

// --- Federated ZIM search (title-prefix + full-text body) ------------------
// status: zim-federated-search
//
// A registry of the vault's `.zim` archives so the main search panel can fold
// ZIM hits in alongside note results. `zxr::Zim` is memmap-backed and
// `Send + Sync`, and federated search runs on the search feature's background
// query task (`search-query-embed-spawn-blocking`), so the registry is a
// process-global `Mutex<ZimRegistry>` (keyed by absolute path) callable from
// any thread and kept warm across queries. The set is rescanned lazily when
// the vault root changes.
//
// Two search paths over the registry:
//   * `federated_search` — instant title-prefix (binary search over the title
//     index), bounded per archive. No index decode.
//   * `federated_fulltext_search` — BM25 body search over each archive's
//     embedded Xapian glass index (`zxr::search::Searcher`). The per-archive
//     fulltext index location + parsed glass `Version` are cached at scan time
//     (decoding the version header once); the `Searcher` itself borrows the
//     archive's mmap so it's rebuilt per fired query (decodes the doclen list),
//     not per keystroke — federated search only runs when the debounced query
//     actually fires.

/// Vault `.zim` archive cache. Global (not thread-local) so the search
/// feature can run federated search on a background `spawn_blocking` task
/// (`search-query-embed-spawn-blocking`) while the cache stays warm across queries. The
/// `Zim` archives are memmap-backed and `Send + Sync`; the `Mutex` just
/// serializes the lazy rescan + lookups (queries are coalesced by epoch, so
/// lock contention is a non-issue).
static REGISTRY: LazyLock<Mutex<ZimRegistry>> =
    LazyLock::new(|| Mutex::new(ZimRegistry::default()));

/// Lock the global ZIM registry, recovering from a poisoned mutex — a prior
/// panic mid-scan shouldn't permanently disable ZIM search.
fn registry() -> std::sync::MutexGuard<'static, ZimRegistry> {
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A title hit from a registered ZIM archive, surfaced as a distinct search
/// result group. `zim_path` is vault-relative (opens the viewer);
/// `article_url` is the content url to navigate to.
pub struct TitleHit {
    pub zim_path: String,
    /// Human label for the archive (its file stem), for the group header.
    pub archive_label: String,
    pub title: String,
    pub article_url: String,
}

/// A full-text (body) hit from a registered ZIM archive's embedded Xapian
/// index, surfaced as a distinct "full-text" result group separate from the
/// title-prefix group. `article_url` opens the viewer at that article.
pub struct FullTextHit {
    pub zim_path: String,
    pub archive_label: String,
    pub title: String,
    pub article_url: String,
    pub score: f64,
}

/// Cached location + parsed glass version for one archive's embedded Xapian
/// fulltext index. Built once at scan time; the BM25 `Searcher` is constructed
/// per query over the archive's mmap (the `Searcher` borrows that mmap, so it
/// can't be cached alongside the owning `Zim`).
struct FtsIndex {
    /// Absolute byte offset of the glass DB within the archive's mmap.
    base: usize,
    /// Parsed glass version header (table roots + db stats).
    version: zxr::glass::Version,
}

/// One scanned archive: its paths, the opened reader, and (if present) its
/// cached fulltext index handle.
struct Archive {
    rel: String,
    label: String,
    zim: Zim,
    fts: Option<FtsIndex>,
}

/// UI-thread-local cache of opened `.zim` archives keyed by absolute path.
#[derive(Default)]
struct ZimRegistry {
    /// Vault root the cache was built for; a change triggers a rescan.
    root: Option<std::path::PathBuf>,
    archives: Vec<Archive>,
}

impl ZimRegistry {
    /// Rescan `root` for `*.zim` files if it changed since the last scan,
    /// opening (and caching) each one — plus its fulltext index location +
    /// glass version when the archive carries an embedded Xapian index. Cheap
    /// when unchanged.
    fn ensure_scanned(&mut self, root: &std::path::Path) {
        if self.root.as_deref() == Some(root) {
            return;
        }
        self.root = Some(root.to_path_buf());
        self.archives.clear();
        for entry in walkdir::WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("zim") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if let Ok(zim) = Zim::open(path) {
                let label = std::path::Path::new(&rel)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&rel)
                    .to_string();
                let fts = open_fts_index(&zim);
                self.archives.push(Archive { rel, label, zim, fts });
            }
        }
    }
}

/// Locate + parse an archive's embedded Xapian fulltext index, returning a
/// cached handle. `None` when the archive has no `X/fulltext/xapian` entry, the
/// index sits in a compressed cluster (no direct mmap offset), or the glass
/// header doesn't parse. Best-effort — a missing/odd index just disables
/// full-text for that archive.
fn open_fts_index(zim: &Zim) -> Option<FtsIndex> {
    let (_idx, loc) = zim.find_fulltext_index().ok()?;
    let off = loc.file_offset? as usize;
    let raw = zim.raw();
    let end = (off + loc.length as usize).min(raw.len());
    let data = raw.get(off..end)?;
    let version = zxr::glass::Version::parse(data).ok()?;
    Some(FtsIndex { base: off, version })
}

/// Query every registered ZIM in the vault for `query` (title-prefix, binary
/// search), returning up to `per_archive` hits per archive. Title-only, no
/// import / embedding. Runs on the search feature's background query task
/// (`search-query-embed-spawn-blocking`); the global registry makes it thread-safe.
pub fn federated_search(
    vault_root: &std::path::Path,
    query: &str,
    per_archive: usize,
) -> Vec<TitleHit> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut reg = registry();
    reg.ensure_scanned(vault_root);
    let mut out = Vec::new();
    for a in &reg.archives {
        for (title, url) in a.zim.title_search(query.trim(), per_archive) {
            out.push(TitleHit {
                zim_path: a.rel.clone(),
                archive_label: a.label.clone(),
                title,
                article_url: url,
            });
        }
    }
    out
}

/// Query every registered ZIM in the vault for `query` via **full-text body
/// search** (BM25 over the embedded Xapian index), returning up to
/// `per_archive` ranked hits per archive. The payoff of `zxr` over a
/// title-only reader: it matches words in article *bodies*, not just titles.
/// Runs on the search feature's background query task (`search-query-embed-spawn-blocking`)
/// alongside [`federated_search`]; archives without an embedded index
/// contribute nothing.
pub fn federated_fulltext_search(
    vault_root: &std::path::Path,
    query: &str,
    per_archive: usize,
) -> Vec<FullTextHit> {
    if query.trim().is_empty() || per_archive == 0 {
        return Vec::new();
    }
    let mut reg = registry();
    reg.ensure_scanned(vault_root);
    let mut out = Vec::new();
    for a in &reg.archives {
        let Some(fts) = a.fts.as_ref() else { continue };
        // The Searcher borrows the archive's mmap; build it per query
        // (decodes the doclen list once). `Version` is cheap to clone.
        let Ok(searcher) =
            zxr::search::Searcher::new(a.zim.raw(), fts.base, fts.version.clone())
        else {
            continue;
        };
        let Ok(hits) = searcher.search(query.trim(), per_archive) else {
            continue;
        };
        for h in hits {
            // `Hit.path` is the docdata path, e.g. `C/Article_Title`.
            // Strip the leading namespace dir to get the content url the
            // viewer navigates to; the display title de-underscores it.
            let url = h.path.split_once('/').map(|(_, p)| p).unwrap_or(&h.path);
            out.push(FullTextHit {
                zim_path: a.rel.clone(),
                archive_label: a.label.clone(),
                title: url.replace('_', " "),
                article_url: url.to_string(),
                score: h.score,
            });
        }
    }
    out
}

/// Map a `zim://` URL's archive authority to a vault-relative `.zim` path.
///
/// A `zim://<archive>/<article>` reference names its archive by an authority
/// segment (e.g. `zim` in `zim://zim/C/Foo`). That segment is matched against
/// the scanned archives by file stem (`label`), then by exact vault `rel` path,
/// so both `zim://wikipedia/...` (stem) and `zim://refs/wikipedia.zim/...`
/// (path) resolve. Returns `None` when no scanned archive matches — the caller
/// then surfaces "archive not found" rather than opening a blank viewer.
/// status: widget-mermaid-links
pub fn resolve_archive_path(vault_root: &std::path::Path, archive: &str) -> Option<String> {
    let mut reg = registry();
    reg.ensure_scanned(vault_root);
    reg.archives
        .iter()
        .find(|a| a.label == archive || a.rel == archive)
        .map(|a| a.rel.clone())
}

/// Open the ZIM viewer at a specific article (used by federated search result
/// clicks): open/focus the tab for `zim_path`, then set its article payload.
pub fn open_at_article(app: &mut AppState, zim_path: &str, article_url: &str) {
    let article = Some(article_url.to_string());
    match find_archive_tab(app, zim_path) {
        // An archive tab is already open: navigate it (preview → in place,
        // pinned → new preview tab), recording the visit.
        Some(id) => navigate_within(app, id, zim_path, &article),
        // Nothing open yet: open at the article as a preview, then record it.
        None => {
            open_preview(app, zim_path, article.clone());
            push_nav(app, zim_path, &article);
        }
    }
}

/// Read an article's HTML from the archive. `None` = the main page.
fn load_article(archive: &Zim, article: Option<&str>) -> Result<String, String> {
    let bytes = match article {
        None => archive.main_page().ok_or_else(|| "no resolvable main page".to_string())?,
        Some(url) => content_article(archive, url)?,
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Resolve a content-namespace article by URL, trying the modern `C`
/// namespace first then the legacy `A` namespace.
fn content_article(archive: &Zim, url: &str) -> Result<Vec<u8>, String> {
    archive
        .article_by_url(b'C', url)
        .or_else(|| archive.article_by_url(b'A', url))
        .ok_or_else(|| format!("article not found: {url}"))
}

/// Turn an in-archive `href` into a content-article URL, or `None` if it is
/// not an in-archive navigation (external scheme, or a pure `#fragment`).
///
/// ZIM article hrefs are archive-relative, e.g. `Article_Name`,
/// `./Article_Name`, or `../A/Article_Name`. We drop any `#fragment` /
/// `?query`, strip a leading namespace segment (`A/` or `C/`) and any
/// `./` / `../` prefixes, and decode percent-escapes so the URL matches the
/// archive's stored entry path.
fn resolve_href(href: &str) -> Option<String> {
    // External links: not in-archive. `core::url` is the single source of
    // truth for external-scheme detection; an `http(s)`/`mailto:` href leaves
    // the archive. Other `scheme://` URIs (e.g. `data:`) still bail via the
    // `://` check below, since they're likewise not archive-relative.
    if matches!(hiker_core::url::classify(href), hiker_core::url::LinkTarget::External(_))
        || href.contains("://")
    {
        return None;
    }
    // Pure fragment within the current page: nothing to navigate to.
    let no_frag = href.split('#').next().unwrap_or("");
    let path = no_frag.split('?').next().unwrap_or("");
    if path.is_empty() {
        return None;
    }

    // Normalize leading relative / namespace segments.
    let mut s = path;
    loop {
        if let Some(rest) = s.strip_prefix("./") {
            s = rest;
        } else if let Some(rest) = s.strip_prefix("../") {
            s = rest;
        } else {
            break;
        }
    }
    // Drop a leading single-letter namespace dir (`A/Foo`, `C/Foo`).
    if let Some(rest) = s.strip_prefix("A/").or_else(|| s.strip_prefix("C/")) {
        s = rest;
    }

    Some(percent_decode(s))
}

/// Minimal percent-decoding (`%XX` → byte) for article URLs. Pure-Rust, no
/// extra dep — ZIM hrefs use UTF-8 percent escapes for non-ASCII titles.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_simple_href() {
        assert_eq!(resolve_href("Article_Name"), Some("Article_Name".into()));
        assert_eq!(resolve_href("./Foo"), Some("Foo".into()));
        assert_eq!(resolve_href("../A/Foo"), Some("Foo".into()));
        assert_eq!(resolve_href("C/Bar"), Some("Bar".into()));
    }

    #[test]
    fn drops_fragment_and_query() {
        assert_eq!(resolve_href("Foo#section"), Some("Foo".into()));
        assert_eq!(resolve_href("Foo?x=1"), Some("Foo".into()));
        assert_eq!(resolve_href("#only-fragment"), None);
    }

    #[test]
    fn skips_external() {
        assert_eq!(resolve_href("https://example.com"), None);
        assert_eq!(resolve_href("mailto:a@b.c"), None);
    }

    #[test]
    fn percent_decodes() {
        assert_eq!(percent_decode("Foo%20Bar"), "Foo Bar");
        assert_eq!(resolve_href("Caf%C3%A9"), Some("Café".into()));
    }

    #[test]
    fn parses_zim_subresource_url() {
        // base `zim://zim/C/` + `../I/img.png` → `zim://zim/I/img.png`.
        assert_eq!(
            parse_zim_url("zim://zim/I/img.png"),
            Some((b'I', "img.png".to_string()))
        );
        assert_eq!(
            parse_zim_url("zim://zim/-/style.css"),
            Some((b'-', "style.css".to_string()))
        );
        // percent-decoding of the entry path.
        assert_eq!(
            parse_zim_url("zim://zim/C/Caf%C3%A9"),
            Some((b'C', "Café".to_string()))
        );
        // fragment / query stripped.
        assert_eq!(
            parse_zim_url("zim://zim/C/Foo#frag"),
            Some((b'C', "Foo".to_string()))
        );
        // non-zim url → None.
        assert!(parse_zim_url("https://example.com/x").is_none());
    }

    #[test]
    fn parse_zim_url_normalizes_dot_segments() {
        // The renderer concatenates a relative href onto the base without
        // resolving `..`, so a `<link href="../-/style.css">` from a `C/`
        // article arrives un-normalized. `..` must walk out of `C` so the
        // namespace reads as `-` (style), not `C`.
        assert_eq!(
            parse_zim_url("zim://zim/C/../-/style.css"),
            Some((b'-', "style.css".to_string()))
        );
        // Images live under `I`; same `..`-out-of-`C` shape.
        assert_eq!(
            parse_zim_url("zim://zim/C/../I/m/Foo.png"),
            Some((b'I', "m/Foo.png".to_string()))
        );
        // A leading `./` (current dir) is dropped, keeping the C namespace.
        assert_eq!(
            parse_zim_url("zim://zim/C/./Article"),
            Some((b'C', "Article".to_string()))
        );
        // Normalization composes with fragment/query stripping + percent-decode.
        assert_eq!(
            parse_zim_url("zim://zim/C/../-/Caf%C3%A9.css?v=1"),
            Some((b'-', "Café.css".to_string()))
        );
    }

    // Minimal single-content-entry uncompressed ZIM, just enough to exercise
    // the registry + federated-search mapping end-to-end.
    fn tiny_zim(title: &str, url: &str, body: &[u8]) -> Vec<u8> {
        const MAGIC: u32 = 0x044D_495A;
        let entry_count = 1u32;
        let cluster_count = 1u32;

        let mut mime_blob = Vec::new();
        mime_blob.extend_from_slice(b"text/html\0\0");

        // one content dir entry.
        let mut entry = Vec::new();
        entry.extend_from_slice(&0u16.to_le_bytes()); // mime id 0
        entry.push(0); // param len
        entry.push(b'C'); // namespace
        entry.extend_from_slice(&0u32.to_le_bytes()); // revision
        entry.extend_from_slice(&0u32.to_le_bytes()); // cluster
        entry.extend_from_slice(&0u32.to_le_bytes()); // blob
        entry.extend_from_slice(url.as_bytes());
        entry.push(0);
        entry.extend_from_slice(title.as_bytes());
        entry.push(0);

        // one uncompressed single-blob cluster.
        let mut cluster = vec![0u8]; // comp=0
        cluster.extend_from_slice(&8u32.to_le_bytes());
        cluster.extend_from_slice(&(8 + body.len() as u32).to_le_bytes());
        cluster.extend_from_slice(body);

        let mime_pos = 80u64;
        let url_ptr_pos = mime_pos + mime_blob.len() as u64;
        let title_ptr_pos = url_ptr_pos + entry_count as u64 * 8;
        let cluster_ptr_pos = title_ptr_pos + entry_count as u64 * 4;
        let entries_pos = cluster_ptr_pos + cluster_count as u64 * 8;
        let cluster_pos = entries_pos + entry.len() as u64;
        let checksum_pos = cluster_pos + cluster.len() as u64;

        let mut out = vec![0u8; 80];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[24..28].copy_from_slice(&entry_count.to_le_bytes());
        out[28..32].copy_from_slice(&cluster_count.to_le_bytes());
        out[32..40].copy_from_slice(&url_ptr_pos.to_le_bytes());
        out[40..48].copy_from_slice(&title_ptr_pos.to_le_bytes());
        out[48..56].copy_from_slice(&cluster_ptr_pos.to_le_bytes());
        out[56..64].copy_from_slice(&mime_pos.to_le_bytes());
        out[64..68].copy_from_slice(&0u32.to_le_bytes()); // main page = entry 0
        out[68..72].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
        out[72..80].copy_from_slice(&checksum_pos.to_le_bytes());

        out.extend_from_slice(&mime_blob);
        out.extend_from_slice(&entries_pos.to_le_bytes()); // url ptr[0]
        out.extend_from_slice(&0u32.to_le_bytes()); // title ptr[0] -> entry 0
        out.extend_from_slice(&cluster_pos.to_le_bytes()); // cluster ptr[0]
        out.extend_from_slice(&entry);
        out.extend_from_slice(&cluster);
        out.extend_from_slice(&[0u8; 16]); // checksum
        out
    }

    #[test]
    fn federated_search_maps_hits() {
        let dir = tempfile::tempdir().unwrap();
        let zim = tiny_zim("Rust (programming)", "Rust", b"<html>rust</html>");
        let sub = dir.path().join("wiki");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("wikipedia_en.zim"), &zim).unwrap();

        let hits = federated_search(dir.path(), "Rust", 20);
        assert_eq!(hits.len(), 1, "should find the one matching title");
        let h = &hits[0];
        assert_eq!(h.title, "Rust (programming)");
        assert_eq!(h.article_url, "Rust");
        // vault-relative path with forward slashes.
        assert_eq!(h.zim_path, "wiki/wikipedia_en.zim");
        // archive label is the file stem.
        assert_eq!(h.archive_label, "wikipedia_en");

        // Blank query and non-matching query → no hits.
        assert!(federated_search(dir.path(), "  ", 20).is_empty());
        assert!(federated_search(dir.path(), "Zzz", 20).is_empty());
    }
}
