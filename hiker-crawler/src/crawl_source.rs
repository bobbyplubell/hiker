//! The engine-backed [`PageSource`] for the in-app crawl run
//! (`crawler-crawl-run`).
//!
//! This is the whole reason the crawler reuses hiker's frontier loop verbatim:
//! `hiker_extract::crawl::run` already takes a `PageSource` fetcher trait, so
//! the crawler only has to supply a fetcher that renders in the browser engine
//! and runs the *shared* transform on the post-JS DOM. No new frontier
//! abstraction is introduced — all governance (scope/dedup/depth/robots/
//! wikilink-rewrite/companion writes) stays in `hiker-extract`.
//!
//! Under `--features cef`, [`CefPageSource`] drives a dedicated crawl
//! [`CefBrowser`] against the live [`CefRuntime`]: per fetched URL it navigates,
//! pumps the runtime + browser until the rendered (post-JS) HTML is available
//! and the load is idle (bounded by an iteration cap), then hands the HTML to
//! the same `extract_from_html` the preview and ingest use
//! (`crawler-preview-fidelity`). In the default build it wraps a [`NullEngine`]
//! that renders nothing, so `fetch` yields `Ok(None)` and the loop simply makes
//! no progress — proving the wiring is correct before the CEF engine lands.

use std::cell::RefCell;

use hiker_extract::ExtractError;
use hiker_extract::contract::Extracted;
use hiker_extract::crawl::PageSource;

#[cfg(not(feature = "cef"))]
use crate::engine::BrowserEngine;

/// Run the shared `extract_from_html` transform on a rendered page, clearing the
/// archive (the WARC is attached by the manifest path, not the crawl loop, for
/// now). Shared by both engine backends so a crawled page is byte-identical to
/// an imported one (`crawler-preview-fidelity`).
fn extract_page(html: &str, url: &str) -> Extracted {
    let mut extracted = hiker_extract::builtin::extract_from_html(html, url);
    extracted.archive = None;
    extracted
}

/// A [`PageSource`] that fetches by driving a [`NullEngine`] (default build).
///
/// `PageSource::fetch` borrows `&self`, but the engine needs `&mut self` to
/// navigate, so it lives behind a [`RefCell`]. Single threaded (the crawl loop
/// calls `fetch` sequentially), so the borrow is never contended.
#[cfg(not(feature = "cef"))]
pub struct CefPageSource {
    engine: RefCell<Box<dyn BrowserEngine>>,
}

#[cfg(not(feature = "cef"))]
impl CefPageSource {
    /// Wrap `engine` as a crawl page source.
    #[must_use]
    pub fn new(engine: Box<dyn BrowserEngine>) -> Self {
        Self { engine: RefCell::new(engine) }
    }
}

#[cfg(not(feature = "cef"))]
impl PageSource for CefPageSource {
    fn fetch(&self, url: &str) -> Result<Option<Extracted>, ExtractError> {
        let mut engine = self.engine.borrow_mut();
        engine.load(url);
        // `NullEngine` renders nothing, so `fetch` returns `Ok(None)` and the
        // frontier loop makes no progress; the CEF build renders for real.
        let Some(html) = engine.rendered_html() else {
            return Ok(None);
        };
        Ok(Some(extract_page(&html, url)))
    }
}

/// A [`PageSource`] that fetches by driving a live CEF crawl browser
/// (`crawler-crawl-run`). Holds a reference to the process-global
/// [`CefRuntime`] (to pump the message loop) and a dedicated windowless
/// [`CefBrowser`] (so the crawl run never disturbs the user's visible tabs).
#[cfg(feature = "cef")]
pub struct CefPageSource<'a> {
    runtime: &'a crate::engine::cef_impl::CefRuntime,
    browser: RefCell<crate::engine::cef_impl::CefBrowser>,
}

/// Upper bound on pump iterations per fetch before giving up on a page — a
/// seatbelt so a hung/never-idle page can't wedge the (synchronous) run.
#[cfg(feature = "cef")]
const MAX_PUMP_ITERS: usize = 6000;

#[cfg(feature = "cef")]
impl<'a> CefPageSource<'a> {
    /// Build a crawl page source over a fresh dedicated browser created against
    /// `runtime`.
    #[must_use]
    pub fn new(runtime: &'a crate::engine::cef_impl::CefRuntime) -> Self {
        Self {
            runtime,
            browser: RefCell::new(runtime.new_browser()),
        }
    }
}

#[cfg(feature = "cef")]
impl PageSource for CefPageSource<'_> {
    fn fetch(&self, url: &str) -> Result<Option<Extracted>, ExtractError> {
        use crate::engine::BrowserEngine;

        let mut browser = self.browser.borrow_mut();
        browser.load(url);

        // Bounded pump loop: drive the global message loop + this browser until
        // the post-JS DOM is captured and the load is idle, or the cap trips.
        // The engine auto-refreshes `rendered_html` on the load→idle edge.
        let mut html = None;
        for _ in 0..MAX_PUMP_ITERS {
            self.runtime.pump();
            browser.poll();
            if let Some(rendered) = browser.rendered_html() {
                html = Some(rendered);
                break;
            }
        }
        // TODO(crawler-crawl-run): the pump loop is a busy spin without a real
        // settle/idle signal beyond CEF's `is_loading`; a later pass should key
        // off a load-end event + a short quiet period for JS-driven content.
        let Some(html) = html else {
            return Ok(None);
        };
        Ok(Some(extract_page(&html, url)))
    }
}
