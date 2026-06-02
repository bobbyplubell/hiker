//! WYSIWYG-with-hiker preview (`crawler-preview-fidelity`, `crawler-render-preview`).
//!
//! Two previews share this module, both proving the crawler shows exactly what
//! the vault will:
//!
//! - [`preview_markdown`] runs the *same* `hiker-extract` readability + `htmd`
//!   path hiker's ingest uses on the engine's rendered DOM, so the markdown is
//!   byte-identical to what hiker produces on import (`crawler-preview-fidelity`).
//! - [`RenderedPreview`] renders the captured page's HTML/CSS through the *same*
//!   `hiker-render` renderer (`hiker_htmlview`) the vault displays the HTML/CSS
//!   rendition with (`htmlview-render`), so the rendition is WYSIWYG across the
//!   boundary (`crawler-shared-render` / `crawler-render-preview`). No JS runs —
//!   the page already rendered in the engine; this is the static rendition.
//!
//! The full in-app crawl run (`crawler-crawl-run`) reuses `hiker-extract`'s
//! frontier loop (`crawl-frontier-loop`) for scope/dedup/depth/robots, swapping
//! the static fetcher for the engine so JS pages resolve.

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;
use hiker_htmlview::{HtmlView, ResourceProvider, Theme};

/// Extract preview markdown from a rendered HTML string using the shared hiker
/// pipeline. Returns the markdown hiker would ingest for this page.
///
/// Routes `rendered_html` through `hiker_extract`'s public
/// [`extract_from_html`](hiker_extract::builtin::extract_from_html) — the same
/// readability + htmd transform `extract-web-readability` runs on ingest — so
/// the markdown shown here is byte-identical to what hiker produces on import
/// (`crawler-preview-fidelity`). `base_url` resolves the page's relative links.
#[must_use]
pub fn preview_markdown(rendered_html: &str, base_url: &str) -> String {
    hiker_extract::builtin::extract_from_html(rendered_html, base_url).markdown
}

/// An offline [`ResourceProvider`] backed by an in-memory `url → (bytes, mime)`
/// map — the resources the engine captured off the wire for this navigation
/// (`crawler-warc-archive`). Serving the preview's CSS/images from the *same*
/// wire responses the WARC archive is built from is what makes the rendition
/// match what was captured. Empty in the default build (no engine taps), so the
/// preview renders the page's own inline styling only.
struct CapturedResources {
    by_url: HashMap<String, (Vec<u8>, String)>,
}

impl CapturedResources {
    fn new(resources: Vec<(String, Vec<u8>, String)>) -> Self {
        let by_url = resources
            .into_iter()
            .map(|(url, body, mime)| (url, (body, mime)))
            .collect();
        Self { by_url }
    }
}

impl ResourceProvider for CapturedResources {
    fn fetch(&self, url: &str) -> Option<(Vec<u8>, String)> {
        self.by_url.get(url).cloned()
    }
}

/// A captured page rendered through `hiker-render` for the in-app preview
/// (`crawler-render-preview`). Owns the renderer's `!Send` [`HtmlView`] (parsed
/// DOM + computed style + egui texture caches), so it lives on the UI thread in
/// the per-tab state. Host-driven: the caller owns the scroll area, this lays
/// out and paints into the host painter.
// status: crawler-shared-render
pub struct RenderedPreview {
    view: HtmlView,
}

impl RenderedPreview {
    /// Build a preview over `html` (the engine's rendered, post-JS DOM), with
    /// relative URLs resolved against `base_url` and subresources served from
    /// the engine's `captured` wire responses.
    #[must_use]
    pub fn new(html: &str, base_url: &str, captured: Vec<(String, Vec<u8>, String)>) -> Self {
        let provider: Arc<dyn ResourceProvider> = Arc::new(CapturedResources::new(captured));
        let view = HtmlView::new(html, Some(base_url), provider);
        Self { view }
    }

    /// Lay out and paint the rendered page into the current `ui` (the caller
    /// supplies the scroll viewport). `dark` selects the renderer theme to match
    /// the host's light/dark mode.
    pub fn show(&mut self, ui: &mut egui::Ui, dark: bool) {
        let theme = if dark { Theme::Dark } else { Theme::Light };
        self.view.set_theme(theme);
        let bg = hiker_htmlview::page_bg_color(theme);
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let width = ui.available_width();
                let size = self.view.layout(ui.ctx(), width);
                let (rect, _response) = ui.allocate_exact_size(size, egui::Sense::hover());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, bg);
                self.view.paint(&painter, rect.min, painter.clip_rect());
            });
    }
}
