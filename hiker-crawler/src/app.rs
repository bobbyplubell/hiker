//! The `egui_workbench`-based shell (`crawler-app-shell`).
//!
//! The crawler adopts hiker-app's workbench chrome (activity bar + primary
//! side bar + tabbed central area + status bar) so it looks/feels like hiker.
//! Browsing is multi-tab via workbench tabs: each central tab is one live
//! browser page (windowless CEF OSR under `--features cef`, or the
//! [`NullEngine`] placeholder in the default build). There is no Chromium
//! chrome — every browser is windowless and its texture is painted into the
//! tab body.
//!
//! State split: the [`Workbench`] owns the tab strip / layout; the per-tab
//! browser + selection + OSR texture live in [`TabState`], keyed by the
//! workbench [`TabId`] in [`CrawlerApp::tabs`]. The side panel (picked fields,
//! link strategy, emit buttons, output) acts on the *active* tab.

use std::collections::BTreeMap;

use eframe::egui;
use egui_workbench::workspace::{OpenTabOptions, TabId, Workbench};

use hiker_extract::crawl::manifest;

use crate::crawl_source::CefPageSource;
use crate::emit::{self, AuthorMode};
use crate::engine::BrowserEngine;
#[cfg(not(feature = "cef"))]
use crate::engine::NullEngine;
use crate::picker::{LinkStrategy, Selection};
use crate::preview;
use crate::workbench::{CrawlerBehavior, CrawlerTab, MODE_CONTROLS, Mode};

/// The per-tab engine type. The default build holds the trait-object
/// [`NullEngine`]; the `cef` build holds the concrete [`CefBrowser`] so the
/// page area can reach its OSR-specific accessors (frame upload, resize, input
/// forwarding) that aren't on the swap-able [`BrowserEngine`] trait.
///
/// [`CefBrowser`]: crate::engine::cef_impl::CefBrowser
#[cfg(feature = "cef")]
type TabEngine = crate::engine::cef_impl::CefBrowser;
#[cfg(not(feature = "cef"))]
type TabEngine = Box<dyn BrowserEngine>;

/// All state for one browser tab: its engine, the URL bar buffer, the picked
/// selection, and (under `cef`) the egui texture backing the OSR page plus the
/// picker bookkeeping. Owned by [`CrawlerApp::tabs`], keyed by the workbench
/// [`TabId`]. Per-tab selection lives here (the ideal choice from the task):
/// each tab authors its own selection independently.
pub struct TabState {
    /// The live-page engine for this tab.
    engine: TabEngine,
    /// The URL bar buffer for this tab.
    url: String,
    /// The picked selection authored against this tab's page.
    selection: Selection,
    /// The captured page rendered through `hiker-render` for the in-app preview
    /// (`crawler-render-preview`); `None` until the user opens a rendered
    /// preview, then shown in place of the text output. Holds the renderer's
    /// `!Send` view, which is why it lives here (UI-thread per-tab state).
    rendered_preview: Option<preview::RenderedPreview>,
    /// The egui texture backing this tab's live OSR page (rebuilt per frame).
    #[cfg(feature = "cef")]
    page_texture: Option<egui::TextureHandle>,
    /// Whether a page click picks a DOM element (vs. interacting with the page).
    #[cfg(feature = "cef")]
    pick_mode: bool,
    /// Names the next auto-labelled picked field (`field_0`, `field_1`, …).
    field_counter: usize,
    /// The selector currently outlined on the page by the hover re-highlight,
    /// so it's only (re)injected/cleared on a change (`crawler-element-picker`).
    #[cfg(feature = "cef")]
    highlighted: Option<String>,
}

impl TabState {
    /// A fresh tab over `engine`.
    fn new(engine: TabEngine) -> Self {
        Self {
            engine,
            url: String::new(),
            selection: Selection::default(),
            rendered_preview: None,
            field_counter: 0,
            #[cfg(feature = "cef")]
            page_texture: None,
            #[cfg(feature = "cef")]
            pick_mode: true,
            #[cfg(feature = "cef")]
            highlighted: None,
        }
    }

    /// The URL the tab's engine currently has loaded, if any.
    pub fn engine_url(&self) -> Option<String> {
        self.engine.current_url().map(str::to_owned)
    }
}

/// The application state: the workbench shell, the global CEF runtime, and the
/// per-tab browser state.
pub struct CrawlerApp {
    /// The workbench shell (activity bar, side bar, tabbed editor area).
    workbench: Workbench<CrawlerTab, Mode>,
    /// Per-tab browser state, keyed by the workbench tab handle.
    pub tabs: BTreeMap<TabId, TabState>,
    /// The most recent emitted/preview text, shown in the controls panel.
    output: String,
    /// The process-global CEF runtime (init + message pump). Initialized once;
    /// per-tab browsers are created against it.
    #[cfg(feature = "cef")]
    runtime: crate::engine::cef_impl::CefRuntime,
}

impl CrawlerApp {
    /// Construct the app against the eframe creation context.
    #[must_use]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Match hiker's colors/fonts/spacing exactly (crawler-shared-theme).
        hiker_theme::Theme.install(&cc.egui_ctx);
        // Image loaders back the workbench chrome's SVG icons.
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let mut workbench = Workbench::new();
        workbench.open_primary_panel(MODE_CONTROLS.to_string());

        let mut app = Self {
            workbench,
            tabs: BTreeMap::new(),
            output: String::new(),
            #[cfg(feature = "cef")]
            runtime: crate::engine::cef_impl::CefRuntime::new(),
        };
        // Open one browser tab to start.
        app.open_new_tab();
        app
    }

    /// Create a fresh engine for a new tab. CEF browsers are created against
    /// the global runtime (never before init); the default build returns a
    /// [`NullEngine`].
    fn new_engine(&self) -> TabEngine {
        #[cfg(feature = "cef")]
        {
            self.runtime.new_browser()
        }
        #[cfg(not(feature = "cef"))]
        {
            Box::new(NullEngine::new())
        }
    }

    /// Open a new browser tab: allocate its engine + state and a workbench tab,
    /// then focus it. Wired to the side bar's "+ New tab" button.
    pub fn open_new_tab(&mut self) {
        let engine = self.new_engine();
        // Open the workbench tab first to mint its handle, then key the state.
        let tab = CrawlerTab {
            id: TabId(0), // placeholder, overwritten below
            cached_title: "New tab".to_string(),
        };
        let handle = self.workbench.open_tab(
            tab,
            &OpenTabOptions { focus: true, ..OpenTabOptions::default() },
        );
        if let Some(t) = self.workbench.editor_area.get_mut(handle) {
            t.id = handle;
        }
        self.tabs.insert(handle, TabState::new(engine));
    }

    /// The active tab's state, if any.
    pub fn active_tab(&self) -> Option<&TabState> {
        self.workbench.active_handle().and_then(|h| self.tabs.get(&h))
    }

    /// The active tab's state mutably, if any.
    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        let handle = self.workbench.active_handle()?;
        self.tabs.get_mut(&handle)
    }

    /// The primary side bar: per-active-tab picked fields + link strategy + the
    /// emit actions + the output pane. Operates on the active tab's selection.
    pub fn side_panel(&mut self, ui: &mut egui::Ui) {
        // The crawl run needs `self.runtime` (cef) but the tab borrow below
        // holds `&mut self`; capture the request while the tab is borrowed and
        // run it after the borrow ends. `None` = no run requested this frame.
        let mut crawl_request: Option<Selection> = None;
        let emitted = self.side_panel_tab(ui, &mut crawl_request);
        let emitted = match crawl_request {
            #[cfg(feature = "cef")]
            Some(sel) => Some(run_crawl(&sel, &self.runtime)),
            #[cfg(not(feature = "cef"))]
            Some(sel) => Some(run_crawl(&sel)),
            None => emitted,
        };

        // Any text-producing action supersedes a standing rendered preview, so
        // the output pane shows the freshest result.
        if let Some(text) = emitted {
            self.output = text;
            if let Some(tab) = self.active_tab_mut() {
                tab.rendered_preview = None;
            }
        }

        ui.separator();
        let dark = ui.visuals().dark_mode;
        let showing_preview = self.active_tab().is_some_and(|t| t.rendered_preview.is_some());
        if showing_preview {
            // The `hiker-render` rendition (`crawler-render-preview`), shown in
            // place of the text output; a close button drops back to text.
            let mut close = false;
            ui.horizontal(|ui| {
                ui.label("Rendered preview (hiker-render)");
                if ui.button("✕ Close").clicked() {
                    close = true;
                }
            });
            if let Some(prev) = self.active_tab_mut().and_then(|t| t.rendered_preview.as_mut()) {
                prev.show(ui, dark);
            }
            if close && let Some(tab) = self.active_tab_mut() {
                tab.rendered_preview = None;
            }
        } else {
            ui.label("Output");
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.output.as_str())
                        .desired_width(f32::INFINITY)
                        .code_editor(),
                );
            });
        }
    }

    /// The tab-scoped part of the side panel: picked fields, link strategy, and
    /// the emit buttons. Returns the text to show in the output pane (if a
    /// non-crawl button fired); a requested crawl run is handed back via
    /// `crawl_request` so the caller can run it without the tab borrow held.
    fn side_panel_tab(
        &mut self,
        ui: &mut egui::Ui,
        crawl_request: &mut Option<Selection>,
    ) -> Option<String> {
        let Some(tab) = self.active_tab_mut() else {
            ui.weak("No tab open.");
            return None;
        };
        ui.heading("Picked fields");
        #[cfg(feature = "cef")]
        ui.checkbox(&mut tab.pick_mode, "Pick mode (click page to capture)");
        if tab.selection.fields.is_empty() {
            ui.label("Click the page to pick elements.");
        }
        // Track which picked field the pointer is over so the live re-highlight
        // (cef) can outline its selector on the page (crawler-element-picker).
        let mut hovered_selector: Option<String> = None;
        for f in &tab.selection.fields {
            let label = ui.label(format!("{} → {}", f.name, f.selector));
            if label.hovered() {
                hovered_selector = Some(f.selector.clone());
            }
        }
        #[cfg(feature = "cef")]
        Self::sync_highlight(tab, hovered_selector.as_deref());
        #[cfg(not(feature = "cef"))]
        let _ = hovered_selector;
        ui.separator();

        ui.heading("Link following");
        let link = &mut tab.selection.link;
        egui::ComboBox::from_id_salt("link-strategy")
            .selected_text(link_label(*link))
            .show_ui(ui, |ui| {
                ui.selectable_value(link, LinkStrategy::StaticList, "Static list");
                ui.selectable_value(link, LinkStrategy::Dynamic, "Dynamic discovery");
                ui.selectable_value(link, LinkStrategy::PluginDriven, "Plugin-driven");
            });
        ui.separator();

        ui.heading("Emit");
        let mut emitted: Option<String> = None;
        if ui.button("Crawl-job note").clicked() {
            emitted = Some(write_crawl_job(&tab.selection));
        }
        if ui.button("Source plugin (deterministic)").clicked() {
            emitted = Some(emit::source_plugin(&tab.selection, AuthorMode::Deterministic));
        }
        if ui
            .button("Source plugin (LLM)")
            .on_hover_text("Author via the shared LLM client (blocks the UI during the call)")
            .clicked()
        {
            emitted = Some(emit::source_plugin(&tab.selection, AuthorMode::Llm));
        }
        if ui.button("Preview (shared transform)").clicked() {
            emitted = Some(preview_or_hint(&tab.engine, &tab.url));
        }
        if ui
            .button("Rendered preview (hiker-render)")
            .on_hover_text("Render the captured page through the shared hiker-render engine")
            .clicked()
        {
            // status: crawler-render-preview
            // Kick a fresh render-HTML capture, then snapshot the rendered DOM +
            // the wire resources into a `hiker-render` view shown in place of the
            // text output. NullEngine renders nothing, so the default build falls
            // through to the build-with-cef hint.
            tab.engine.request_render_html();
            match tab.engine.rendered_html() {
                Some(html) => {
                    let base = tab.url.clone();
                    #[cfg(feature = "cef")]
                    let captured = tab.engine.captured_resources();
                    #[cfg(not(feature = "cef"))]
                    let captured: Vec<(String, Vec<u8>, String)> = Vec::new();
                    tab.rendered_preview =
                        Some(preview::RenderedPreview::new(&html, &base, captured));
                }
                None => {
                    emitted = Some("(no rendered page — build with `--features cef`)".to_owned());
                }
            }
        }
        if ui.button("Direct WARC + manifest").clicked() {
            emitted = Some(write_manifest_dir(&tab.engine, &tab.selection));
        }
        if ui.button("In-app crawl run").clicked() {
            *crawl_request = Some(tab.selection.clone());
        }
        emitted
    }

    /// One browser tab's body: a per-tab URL bar above the live page.
    pub fn page(&mut self, ui: &mut egui::Ui, tab_id: TabId) {
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
            ui.centered_and_justified(|ui| ui.weak("(missing tab)"));
            return;
        };
        Self::url_bar(tab, ui);
        // Drain any pending picker hit into this tab's selection. Engine-agnostic
        // (it reads the `BrowserEngine` trait): the CEF engine returns hits, the
        // NullEngine never does, so the default build compiles and no-ops here
        // (`crawler-element-picker`).
        drain_hit(tab);
        page_body(tab, ui);
    }

    /// Reconcile the live re-highlight on the page with the hovered picked
    /// field (`crawler-element-picker`): inject an outline for a newly-hovered
    /// selector, clear it when the pointer leaves the list, and do nothing while
    /// the same field stays hovered (so the page isn't re-styled every frame).
    #[cfg(feature = "cef")]
    fn sync_highlight(tab: &mut TabState, hovered: Option<&str>) {
        if tab.highlighted.as_deref() == hovered {
            return;
        }
        match hovered {
            Some(selector) => tab.engine.highlight_selector(selector),
            None => tab.engine.clear_highlight(),
        }
        tab.highlighted = hovered.map(str::to_owned);
    }

    /// The per-tab URL bar: enter a URL + Load to navigate this tab's browser.
    /// Resets the tab's selection to the freshly-loaded seed.
    fn url_bar(tab: &mut TabState, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("URL");
            let entry = ui.add(
                egui::TextEdit::singleline(&mut tab.url)
                    .desired_width(ui.available_width() - 60.0)
                    .hint_text("https://example.com/article"),
            );
            let go = ui.button("Load").clicked();
            if go || (entry.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                tab.engine.load(&tab.url);
                tab.selection = Selection::new(tab.url.clone());
            }
        });
    }
}

/// Render one tab's live page area: the engine's OSR texture (under `cef`), or
/// a placeholder when no engine can render JS (default build). Free function so
/// it doesn't fight `CrawlerApp`'s borrow contract.
#[cfg(not(feature = "cef"))]
fn page_body(tab: &mut TabState, ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(40.0);
        ui.label("No browser engine compiled in.");
        ui.label("Build with `--features cef` to load live pages.");
        if let Some(u) = tab.engine.current_url() {
            ui.label(format!("Requested: {u}"));
        }
    });
}

/// The live page area for the CEF build: size the off-screen browser to the
/// pane, upload the latest `OnPaint` BGRA frame as an egui texture (BGRA→RGBA),
/// draw it, and forward egui pointer/scroll/key input to the browser so the
/// page is interactive.
#[cfg(feature = "cef")]
fn page_body(tab: &mut TabState, ui: &mut egui::Ui) {
    // `view_rect` is DIP (logical points), matching egui's logical sizes; CEF
    // multiplies by the device scale (pixels_per_point) for the backing buffer,
    // so `on_paint` delivers physical pixels and the page is crisp.
    let scale = ui.ctx().pixels_per_point();
    tab.engine.set_scale(scale);

    let size = ui.available_size();
    let (w, h) = (size.x.round().max(1.0) as i32, size.y.round().max(1.0) as i32);
    tab.engine.set_size(w, h);

    // Rebuild the texture only when CEF painted a new frame.
    if let Some(frame) = tab.engine.take_frame() {
        let pixels = bgra_to_rgba(&frame.bgra);
        let image = egui::ColorImage::new([frame.width as usize, frame.height as usize], pixels);
        let opts = egui::TextureOptions::LINEAR;
        match &mut tab.page_texture {
            Some(tex) => tex.set(image, opts),
            None => {
                tab.page_texture = Some(ui.ctx().load_texture("cef-page", image, opts));
            }
        }
    }

    let response = if let Some(tex) = &tab.page_texture {
        ui.add(
            egui::Image::new((tex.id(), size))
                .sense(egui::Sense::click_and_drag())
                .fit_to_exact_size(size),
        )
    } else {
        ui.allocate_response(size, egui::Sense::click_and_drag())
    };

    if tab.pick_mode {
        handle_pick(tab, &response);
    } else {
        forward_input(tab, ui, &response);
    }
    // Keep painting while the page may still be loading/animating.
    ui.ctx().request_repaint();
}

/// Drain any picker hit the engine has returned since the last frame into a new
/// named field on this tab's selection (`crawler-element-picker`). Engine-
/// agnostic — it rides the `BrowserEngine` trait — so it runs in every build:
/// the CEF engine produces hits from [`request_pick`](BrowserEngine::request_pick),
/// the [`NullEngine`] never does, so the default build compiles and no-ops.
fn drain_hit(tab: &mut TabState) {
    if let Some(hit) = tab.engine.take_hit() {
        let selector = hit.selectors.first().cloned().unwrap_or_default();
        let name = format!("field_{}", tab.field_counter);
        tab.field_counter += 1;
        tab.selection.push(crate::picker::Field {
            name,
            selector,
            repeat: hit.repeat,
            sample: hit.text,
        });
    }
}

/// Pick-mode input for one tab (CEF only): forward the hover position and, on a
/// left click over the page, translate the egui position into page CSS-px
/// coordinates and fire an async hit-test. The returned hit is drained into the
/// selection by the engine-agnostic [`drain_hit`].
#[cfg(feature = "cef")]
fn handle_pick(tab: &mut TabState, response: &egui::Response) {
    let origin = response.rect.min;
    if let Some(pos) = response.hover_pos() {
        tab.engine.mouse_move(pos.x - origin.x, pos.y - origin.y, false);
    }
    if let Some(pos) = response.interact_pointer_pos()
        && response.clicked()
    {
        tab.engine.request_pick(pos.x - origin.x, pos.y - origin.y);
    }
}

/// Translate egui pointer/scroll/key input over one tab's page widget into CEF
/// browser-host events. Coordinates are relative to the widget's top-left.
#[cfg(feature = "cef")]
fn forward_input(tab: &mut TabState, ui: &egui::Ui, response: &egui::Response) {
    use crate::engine::cef_impl::PointerButton as Btn;

    let origin = response.rect.min;
    let local = |p: egui::Pos2| (p.x - origin.x, p.y - origin.y);
    let left_down = ui.input(|i| i.pointer.primary_down());

    if let Some(pos) = response.hover_pos() {
        let (x, y) = local(pos);
        tab.engine.mouse_move(x, y, left_down);
        let scroll = ui.input(|i| i.smooth_scroll_delta);
        if scroll != egui::Vec2::ZERO {
            tab.engine.mouse_wheel(x, y, scroll.x, scroll.y);
        }
    }

    if response.clicked() || response.drag_started() {
        tab.engine.set_focus(true);
    }

    for (egui_btn, cef_btn) in [
        (egui::PointerButton::Primary, Btn::Left),
        (egui::PointerButton::Middle, Btn::Middle),
        (egui::PointerButton::Secondary, Btn::Right),
    ] {
        if let Some(pos) = response.interact_pointer_pos() {
            let (x, y) = local(pos);
            if response.is_pointer_button_down_on()
                && ui.input(|i| i.pointer.button_pressed(egui_btn))
            {
                tab.engine.mouse_click(x, y, cef_btn, true);
            }
            if ui.input(|i| i.pointer.button_released(egui_btn)) {
                tab.engine.mouse_click(x, y, cef_btn, false);
            }
        }
    }

    // Keyboard: printable text → CHAR events; editing/navigation keys → raw
    // key-down/up (CEF wants Windows virtual-key codes for those).
    ui.input(|i| {
        for event in &i.events {
            match event {
                egui::Event::Text(t) => {
                    for ch in t.chars() {
                        tab.engine.key_char(ch);
                    }
                }
                egui::Event::Key { key, pressed, .. } => {
                    if let Some(vk) = windows_vk(*key) {
                        tab.engine.key_raw(vk, *pressed);
                    }
                }
                _ => {}
            }
        }
    });
}

/// Emit the direct-WARC handoff directory (`crawler-direct-warc`): the rendered
/// page's markdown + (when the engine supports it) its WARC archive, indexed by
/// a `manifest.json` written with the *shared* manifest type so `import_dir`
/// ingests it unchanged. Asks for a destination folder.
fn write_manifest_dir(engine: &TabEngine, selection: &Selection) -> String {
    let Some(html) = engine.rendered_html() else {
        return "(no rendered page — build with `--features cef`)".to_owned();
    };
    let Some(dir) = rfd::FileDialog::new()
        .set_title("Choose manifest output folder")
        .pick_folder()
    else {
        return "(direct WARC: cancelled)".to_owned();
    };
    let extracted = hiker_extract::builtin::extract_from_html(&html, &selection.seed_url);
    let output_file = "page-0.md";
    let archive_file = engine.capture_warc().and_then(|bytes| {
        let name = "page-0.warc";
        std::fs::write(dir.join(name), bytes).ok().map(|()| name.to_owned())
    });
    if let Err(e) = std::fs::write(dir.join(output_file), &extracted.markdown) {
        return format!("Failed to write page markdown: {e}");
    }
    let manifest = manifest::Manifest {
        pages: vec![manifest::Page {
            url: selection.seed_url.clone(),
            output_file: output_file.to_owned(),
            title: extracted.frontmatter.and_then(|m| m.title),
            archive_file,
            links: extracted.next_urls,
        }],
    };
    match emit::write_manifest(&dir, &manifest) {
        Ok(()) => format!("Wrote manifest directory:\n{}", dir.display()),
        Err(e) => format!("Failed to write manifest: {e}"),
    }
}

/// Write the crawl-job note for `selection`: ask for a destination via a save
/// dialog, mint a job ULID, and write through the canonical shared writer.
/// Returns the text to show in the output pane.
fn write_crawl_job(selection: &Selection) -> String {
    let Some(dest) = rfd::FileDialog::new()
        .set_title("Save crawl-job note")
        .add_filter("Markdown", &["md"])
        .set_file_name("crawl-job.md")
        .save_file()
    else {
        return "(crawl-job note: save cancelled)".to_owned();
    };
    let job_ulid = ulid::Ulid::new().to_string();
    match emit::write_crawl_job(&dest, selection, &job_ulid) {
        Ok(()) => format!("Wrote crawl-job note (id {job_ulid}):\n{}", dest.display()),
        Err(e) => format!("Failed to write crawl-job note: {e}"),
    }
}

/// Run a full in-app crawl (`crawler-crawl-run`): build [`emit::crawl_params`]
/// from the selection and drive `hiker_extract::crawl::run` with a live
/// CEF-backed [`CefPageSource`], to a user-chosen output dir, reporting the
/// captured-page count. Synchronous/UI-blocking for v1.
//
// TODO(crawler-crawl-run): background the run so the UI stays responsive.
// status: crawler-crawl-run
#[cfg(feature = "cef")]
fn run_crawl(selection: &Selection, runtime: &crate::engine::cef_impl::CefRuntime) -> String {
    let Some(dir) = rfd::FileDialog::new()
        .set_title("Choose crawl output folder")
        .pick_folder()
    else {
        return "(in-app crawl run: cancelled)".to_owned();
    };
    let source = CefPageSource::new(runtime);
    run_crawl_to(selection, &dir, &source)
}

/// Default-build crawl run: the [`NullEngine`]-backed source renders nothing, so
/// the frontier loop makes no progress — proving the wiring without a browser.
#[cfg(not(feature = "cef"))]
fn run_crawl(selection: &Selection) -> String {
    let Some(dir) = rfd::FileDialog::new()
        .set_title("Choose crawl output folder")
        .pick_folder()
    else {
        return "(in-app crawl run: cancelled)".to_owned();
    };
    let source = CefPageSource::new(Box::new(NullEngine::new()));
    run_crawl_to(selection, &dir, &source)
}

/// Shared crawl driver: run the shared frontier loop for `selection`'s params
/// into `dir`, anchoring the job note at `<dir>/crawl.md` and treating `dir` as
/// the vault root for the wikilink rewrite. Robots is allow-all here (the user
/// explicitly chose the seed; the crawler has no HTTP client of its own).
fn run_crawl_to(
    selection: &Selection,
    dir: &std::path::Path,
    source: &dyn hiker_extract::crawl::PageSource,
) -> String {
    use hiker_extract::crawl::{Hooks, run};

    let params = emit::crawl_params(selection);
    let job_note_path = dir.join("crawl.md");
    let parent_ulid = ulid::Ulid::new().to_string();
    // Allow-all robots for the in-app run; the crawler links no HTTP client and
    // the seed is a deliberate user choice.
    let robots = |_: &str| None;
    match run(&params, &job_note_path, dir, &parent_ulid, source, &robots, &mut Hooks::none()) {
        Ok(report) => format!(
            "In-app crawl run complete: {} of {} pages captured into\n{}",
            report.captured_count(),
            report.pages.len(),
            dir.display(),
        ),
        Err(e) => format!("In-app crawl run failed: {e}"),
    }
}

/// Preview the rendered page, or a hint when there is nothing to render. Runs
/// the shared `hiker-extract` transform so the markdown is byte-identical to
/// ingest (`crawler-preview-fidelity`).
fn preview_or_hint(engine: &TabEngine, url: &str) -> String {
    engine.rendered_html().map_or_else(
        || "(no rendered page — build with `--features cef`)".to_owned(),
        |html| preview::preview_markdown(&html, url),
    )
}

/// Map the editing/navigation keys CEF needs as raw key events to their Windows
/// virtual-key codes. Printable characters are handled separately via
/// `Event::Text`, so only the non-text keys are mapped here.
#[cfg(feature = "cef")]
const fn windows_vk(key: egui::Key) -> Option<i32> {
    let vk = match key {
        egui::Key::Backspace => 0x08,
        egui::Key::Tab => 0x09,
        egui::Key::Enter => 0x0D,
        egui::Key::Escape => 0x1B,
        egui::Key::PageUp => 0x21,
        egui::Key::PageDown => 0x22,
        egui::Key::End => 0x23,
        egui::Key::Home => 0x24,
        egui::Key::ArrowLeft => 0x25,
        egui::Key::ArrowUp => 0x26,
        egui::Key::ArrowRight => 0x27,
        egui::Key::ArrowDown => 0x28,
        egui::Key::Delete => 0x2E,
        _ => return None,
    };
    Some(vk)
}

/// Convert a tightly-packed BGRA byte buffer (CEF's `OnPaint` format) into the
/// RGBA pixels egui's `ColorImage` expects.
#[cfg(feature = "cef")]
fn bgra_to_rgba(bgra: &[u8]) -> Vec<egui::Color32> {
    bgra.chunks_exact(4)
        .map(|px| egui::Color32::from_rgba_premultiplied(px[2], px[1], px[0], px[3]))
        .collect()
}

/// Human label for a [`LinkStrategy`] in the combo box.
const fn link_label(s: LinkStrategy) -> &'static str {
    match s {
        LinkStrategy::StaticList => "Static list",
        LinkStrategy::Dynamic => "Dynamic discovery",
        LinkStrategy::PluginDriven => "Plugin-driven",
    }
}

impl eframe::App for CrawlerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1) Pump the GLOBAL CEF message loop once for the whole process.
        #[cfg(feature = "cef")]
        self.runtime.pump();

        // 2) Per-frame work for the ACTIVE tab's browser only — background tabs
        //    stay paused (no `send_external_begin_frame`) to save work.
        if let Some(handle) = self.workbench.active_handle()
            && let Some(tab) = self.tabs.get_mut(&handle)
        {
            tab.engine.poll();
        }

        // 3) Refresh cached tab titles (the page host, or "New tab") so the tab
        //    strip is self-sufficient.
        sync_tab_titles(&mut self.workbench, &self.tabs);

        // 4) Render the workbench shell. The behavior borrows `&mut self`, but
        //    `Workbench::ui` needs `&mut self.workbench` too. They are disjoint
        //    fields; the borrow checker can't see that across the call, so we
        //    split the borrow by taking the workbench out (it's `Default`),
        //    rendering, and putting it back — the same pattern hiker-app uses.
        let mut behavior = CrawlerBehavior { app: self };
        let mut wb = std::mem::take(&mut behavior.app.workbench);
        wb.ui(ctx, &mut behavior);
        behavior.app.workbench = wb;
    }
}

/// Refresh each workbench tab's cached title from its backing engine URL.
fn sync_tab_titles(
    workbench: &mut Workbench<CrawlerTab, Mode>,
    tabs: &BTreeMap<TabId, TabState>,
) {
    let handles: Vec<TabId> = workbench.iter_tabs().map(|(h, _)| h).collect();
    for h in handles {
        let title = tabs
            .get(&h)
            .and_then(TabState::engine_url)
            .map_or_else(|| "New tab".to_string(), |u| short_title(&u));
        if let Some(tab) = workbench.editor_area.get_mut(h) {
            tab.cached_title = title;
        }
    }
}

/// A compact tab-strip label from a URL: the host, falling back to the raw URL.
fn short_title(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| url.to_owned())
}
