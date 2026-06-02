//! `hiker-crawler` — the JS-capable companion app.
//!
//! `extract.md` draws hiker's extraction boundary at *no JavaScript, no
//! embedded browser engine* (`extract-web-no-js-stance`) and externalizes
//! JS-heavy sites to "an external browser-driven tool" via the manifest-import
//! seam (`extract-manifest-import`). This app *is* that tool, made
//! first-class: load a live site in a real engine, point-and-click the DOM you
//! want, and emit one of three artifacts hiker already consumes — a crawl-job
//! note, a source-plugin extractor, or a manifest-import directory.
//!
//! The dangerous, heavyweight machinery (a full Chromium with JS, open-web
//! fetching, the C++ CEF binding + bundled distribution) lives ONLY here,
//! quarantined out of hiker's clean-SBOM core (`crawler-quarantine`). See
//! `docs/hiker-crawler.md`.

mod app;
mod crawl_source;
mod emit;
mod engine;
mod llm_author;
mod picker;
mod preview;
// WARC assembly (crawler-warc-archive) is only exercised by the CEF engine's
// CDP Network tap, so the default *release* build links no unused code. It is
// also compiled under `test` (with `warc` as a dev-dependency) so its pure
// assembly logic is unit-tested in the default build.
#[cfg(any(feature = "cef", test))]
mod warc;
mod workbench;

use anyhow::Result;

fn main() -> Result<()> {
    // CEF re-execs this binary as its helper processes (renderer/GPU/utility).
    // The subprocess gate MUST run before anything else: helper processes run
    // CEF's process executor and exit here; only the browser (main) process
    // falls through to start the egui app. No-op in the default build.
    #[cfg(feature = "cef")]
    let _ = engine::cef_impl::subprocess_entry();

    tracing_subscriber::fmt::init();

    let options = eframe::NativeOptions::default();
    // eframe's error type isn't `Send + Sync + 'static`, so map it to a string
    // for `anyhow` rather than `?`-propagating it directly.
    eframe::run_native(
        "hiker-crawler",
        options,
        Box::new(|cc| Ok(Box::new(app::CrawlerApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}
