//! Off-thread execution for the capture tab's Run / Cancel / live-progress.
//!
//! `crawl::run_default` and `feed::poll_note` are BLOCKING (network I/O), so
//! they must not run on the egui UI thread. A Run spawns a `std::thread` that
//! drives the engine with `crawl::Hooks { cancel, on_page }`; the worker posts
//! [`RunEvent`]s through an `mpsc` channel and watches a shared
//! `Arc<AtomicBool>` cancel flag. The UI polls the channel each frame in
//! [`drain`] and a Cancel button flips the flag. This mirrors the
//! cluster-review tab's run/cancel/progress shape — the task-queue IO lane
//! (`crawl-task-queue-lane`) that would host this is deferred.
//!
//! The op-log re-extract versioning step (`extract-version-oplog`) runs on the
//! same worker after the engine returns, exactly as the CLI driver does, so a
//! re-crawl / re-poll of a changed child lands as an `extractor` op rather than
//! a blind overwrite.
//
// status: crawl-job-form
// status: rss-subscription-lifecycle

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use eframe::egui;
use hiker_core::ops::op_writes;
use hiker_core::oplog::OpLog;
use hiker_core::vault::Vault;
use hiker_extract::capture::{CrawlParams, FeedParams};
use hiker_extract::crawl::{self, Hooks, PageRecord};
use hiker_extract::feed::{self, HttpFetcher};

use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;

use super::{PageRow, RunKind};

/// A progress event posted by the background worker. The UI folds these in
/// [`drain`] each frame.
pub enum RunEvent {
    /// One page was processed (crawl) — append to the captured-page index.
    Page(PageRow),
    /// The run finished; carries a one-line summary for the status line.
    Done(String),
    /// The run failed before completing.
    Failed(String),
}

/// A live (or just-finished) background run. The cancel flag is shared with
/// the worker; the receiver is drained each frame.
pub struct RunHandle {
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<RunEvent>,
    /// `false` once a terminal event (`Done`/`Failed`) has been drained, so
    /// the UI can flip the Cancel button back to Run.
    pub running: bool,
}

impl RunHandle {
    /// Build a handle around an externally-driven channel — the seam tests use
    /// to push canned [`RunEvent`]s without spawning a real engine worker.
    #[cfg(test)]
    pub const fn for_test(rx: mpsc::Receiver<RunEvent>, cancel: Arc<AtomicBool>) -> Self {
        Self { cancel, rx, running: true }
    }

    /// Flip the shared cancel flag so the worker stops at its next page
    /// boundary.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Whether the user has signalled cancel (button feedback).
    pub fn is_cancelling(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Launch a crawl on a background thread. Clears the prior page index and
/// stashes a fresh [`RunHandle`] on the pane.
///
/// status: crawl-job-form
pub fn start_crawl(app: &mut AppState, tab_id: TabId, note_path: &str) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    if pane.run.as_ref().is_some_and(|r| r.running) {
        return;
    }
    let Some(params) = pane.spec.as_ref().and_then(|s| s.crawl.clone()) else { return };
    let extractor = pane.spec.as_ref().and_then(|s| s.extractor.clone());
    if params.seeds.iter().all(|s| s.trim().is_empty()) {
        app.push_toast("Add at least one seed URL before running", ToastLevel::Warn);
        return;
    }
    let parent_ulid = pane.note_ulid.clone();
    let vault_root = app.vault_session.vault_root.clone();
    let job_note = vault_root.join(note_path);

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    spawn_crawl_worker(
        tx,
        cancel.clone(),
        params,
        extractor,
        job_note,
        vault_root,
        parent_ulid,
    );

    let pane = app.panels.captures.entry(tab_id).or_default();
    pane.pages.clear();
    pane.last_summary = None;
    pane.run = Some(RunHandle { cancel, rx, running: true });
}

/// Launch a feed poll on a background thread.
///
/// status: rss-subscription-lifecycle
pub fn start_feed_poll(app: &mut AppState, tab_id: TabId, note_path: &str) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    if pane.run.as_ref().is_some_and(|r| r.running) {
        return;
    }
    let Some(params) = pane.spec.as_ref().and_then(|s| s.feed.clone()) else { return };
    if params.url.trim().is_empty() {
        app.push_toast("Add a feed URL before polling", ToastLevel::Warn);
        return;
    }
    let vault_root = app.vault_session.vault_root.clone();
    let feed_note = vault_root.join(note_path);
    let default_retention = feed_item_retention(app);

    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    spawn_feed_worker(tx, params, feed_note, vault_root, default_retention);

    let pane = app.panels.captures.entry(tab_id).or_default();
    pane.pages.clear();
    pane.last_summary = None;
    pane.run = Some(RunHandle { cancel, rx, running: true });
}

/// Drain the in-flight run's progress channel into the pane each frame. On the
/// terminal event the note is re-read (children landed, `last_checked`
/// advanced) and the run is marked finished.
pub fn drain(app: &mut AppState, tab_id: TabId, note_path: &str, ctx: &egui::Context) {
    let outcome = {
        let Some(pane) = app.panels.captures.get_mut(&tab_id) else { return };
        if !pane.run.as_ref().is_some_and(|r| r.running) {
            return;
        }
        // Keep the frame loop ticking while a run is alive so progress paints
        // without user input.
        ctx.request_repaint();
        pane.fold_run_events()
    };
    match outcome {
        DrainOutcome::Pending => {}
        DrainOutcome::Done => {
            // Re-read the note next frame: a crawl wrote children, a feed
            // advanced `last_checked` and may have pruned items.
            super::invalidate(app, tab_id);
            let _ = note_path;
        }
        DrainOutcome::Failed(e) => {
            app.push_toast(format!("Capture run failed: {e}"), ToastLevel::Error);
            super::invalidate(app, tab_id);
        }
    }
}

/// The terminal result of folding a frame's worth of run events.
pub enum DrainOutcome {
    /// The run is still in flight (or had no terminal event this frame).
    Pending,
    /// The run finished successfully this frame.
    Done,
    /// The run failed this frame; carries the error for a toast.
    Failed(String),
}

impl super::Pane {
    /// Drain everything sitting in the run's receiver into this pane: append
    /// page rows, and on a terminal event mark the run finished + stash the
    /// summary. Pure over `(self.run.rx)` → `(self.pages, self.last_summary,
    /// self.run.running)`, so it's directly testable by pushing [`RunEvent`]s
    /// down a channel and asserting the pane state.
    pub fn fold_run_events(&mut self) -> DrainOutcome {
        let mut new_rows: Vec<PageRow> = Vec::new();
        let mut finished = false;
        let mut summary: Option<String> = None;
        let mut failure: Option<String> = None;
        {
            let Some(run) = self.run.as_mut() else { return DrainOutcome::Pending };
            while let Ok(ev) = run.rx.try_recv() {
                match ev {
                    RunEvent::Page(row) => new_rows.push(row),
                    RunEvent::Done(s) => {
                        summary = Some(s);
                        finished = true;
                    }
                    RunEvent::Failed(e) => {
                        failure = Some(e);
                        finished = true;
                    }
                }
            }
            if finished {
                run.running = false;
            }
        }
        self.pages.extend(new_rows);
        if finished && let Some(s) = summary {
            self.last_summary = Some(s);
        }
        match failure {
            Some(e) => DrainOutcome::Failed(e),
            None if finished => DrainOutcome::Done,
            None => DrainOutcome::Pending,
        }
    }
}

/// Cancel an in-flight run from the UI (Cancel button / tab close).
pub fn cancel(app: &mut AppState, tab_id: TabId) {
    if let Some(pane) = app.panels.captures.get(&tab_id)
        && let Some(run) = pane.run.as_ref()
    {
        run.cancel();
    }
}

// ---------------------------------------------------------------------------
// Background workers.
// ---------------------------------------------------------------------------

/// Spawn the crawl worker thread: open the op-log + bootstrap (so a re-crawl
/// versions changed pages), run the governed loop posting per-page progress,
/// then route each captured page through the op-log re-extract path.
fn spawn_crawl_worker(
    tx: mpsc::Sender<RunEvent>,
    cancel: Arc<AtomicBool>,
    params: CrawlParams,
    extractor: Option<String>,
    job_note: PathBuf,
    vault_root: PathBuf,
    parent_ulid: String,
) {
    std::thread::spawn(move || {
        let outcome = run_crawl_blocking(
            &tx, &cancel, &params, extractor, &job_note, &vault_root, &parent_ulid,
        );
        match outcome {
            Ok(summary) => {
                let _ = tx.send(RunEvent::Done(summary));
            }
            Err(e) => {
                let _ = tx.send(RunEvent::Failed(e));
            }
        }
    });
}

/// The crawl body, returning a summary string or an error message. Mirrors the
/// CLI `cmd_crawl` wiring exactly, minus the note-write (the form already
/// created the job note).
fn run_crawl_blocking(
    tx: &mpsc::Sender<RunEvent>,
    cancel: &Arc<AtomicBool>,
    params: &CrawlParams,
    extractor: Option<String>,
    job_note: &Path,
    vault_root: &Path,
    parent_ulid: &str,
) -> Result<String, String> {
    // Open the op-log + bootstrap BEFORE the crawl so a re-crawl's already
    // captured pages have their pre-crawl body as `accepted` state.
    let vault = Vault::open(vault_root).map_err(|e| e.to_string())?;
    let log = OpLog::open(vault_root).map_err(|e| e.to_string())?;
    op_writes::bootstrap(&vault, &log).map_err(|e| e.to_string())?;

    let mut on_page = |r: &PageRecord| {
        let row = PageRow {
            label: r.url.clone(),
            path: r.path.as_ref().map(|p| p.display().to_string()),
            note: r.note.clone(),
        };
        let _ = tx.send(RunEvent::Page(row));
    };
    let mut hooks = Hooks { cancel: Some(cancel), on_page: Some(&mut on_page) };

    let report = crawl::run_default(params, job_note, vault_root, parent_ulid, extractor, &mut hooks)
        .map_err(|e| e.to_string())?;

    // Re-crawl versioning: route every captured page through the op-log
    // re-extract path (no-op on first capture / unlinked sidecars).
    for page in &report.pages {
        let Some(path) = &page.path else { continue };
        let rel = clip_rel(vault_root, path);
        if let Err(e) = op_writes::reextract(&log, &vault, &rel, &child_body(path), "web") {
            tracing::warn!(error = %e, rel = %rel, "capture: version crawled page");
        }
    }
    Ok(format!(
        "{} captured, {} pages touched",
        report.captured_count(),
        report.pages.len()
    ))
}

/// Spawn the feed-poll worker thread: open the op-log + bootstrap, poll the
/// feed note, then version each changed child via the op-log re-extract path.
fn spawn_feed_worker(
    tx: mpsc::Sender<RunEvent>,
    params: FeedParams,
    feed_note: PathBuf,
    vault_root: PathBuf,
    default_retention: String,
) {
    std::thread::spawn(move || {
        let outcome =
            run_feed_blocking(&tx, &params, &feed_note, &vault_root, &default_retention);
        match outcome {
            Ok(summary) => {
                let _ = tx.send(RunEvent::Done(summary));
            }
            Err(e) => {
                let _ = tx.send(RunEvent::Failed(e));
            }
        }
    });
}

/// The feed-poll body. Mirrors the CLI `cmd_feed_poll` wiring for a single
/// note. The feed engine has no per-entry progress hook, so the index is
/// populated from the poll report once it returns.
fn run_feed_blocking(
    tx: &mpsc::Sender<RunEvent>,
    _params: &FeedParams,
    feed_note: &Path,
    vault_root: &Path,
    default_retention: &str,
) -> Result<String, String> {
    let vault = Vault::open(vault_root).map_err(|e| e.to_string())?;
    let log = OpLog::open(vault_root).map_err(|e| e.to_string())?;
    op_writes::bootstrap(&vault, &log).map_err(|e| e.to_string())?;

    let fetch = HttpFetcher;
    let report = feed::poll_note(feed_note, vault_root, default_retention, &fetch)
        .map_err(|e| e.to_string())?;

    for child in &report.new_children {
        let _ = tx.send(RunEvent::Page(PageRow {
            label: clip_rel(vault_root, child),
            path: Some(child.display().to_string()),
            note: "new".to_string(),
        }));
    }
    for child in &report.updated_children {
        let rel = clip_rel(vault_root, child);
        if let Err(e) = op_writes::reextract(&log, &vault, &rel, &child_body(child), "rss") {
            tracing::warn!(error = %e, rel = %rel, "capture: version updated feed entry");
        }
        let _ = tx.send(RunEvent::Page(PageRow {
            label: rel,
            path: Some(child.display().to_string()),
            note: "updated".to_string(),
        }));
    }
    Ok(format!(
        "{} new, {} updated, {} pruned, {} unchanged",
        report.new_children.len(),
        report.updated_children.len(),
        report.pruned_children.len(),
        report.unchanged
    ))
}

/// The vault-relative path of an absolute child path (forward-slashed).
fn clip_rel(vault_root: &Path, abs: &Path) -> String {
    abs.strip_prefix(vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().to_string())
}

/// Read the body (post-frontmatter) of a child note for the op-log re-extract
/// step. Empty when unreadable.
fn child_body(path: &Path) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    hiker_core::frontmatter::split(&content).body.to_string()
}

/// The vault `[extract].feed_item_retention` default for the retention
/// cascade, falling back to `keep:50` when unset.
fn feed_item_retention(app: &AppState) -> String {
    app.vault_session
        .config
        .read()
        .ok()
        .map(|c| c.extract.feed_item_retention.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "keep:50".to_string())
}

/// Discriminates which engine a Run drives (used by the layouts to pick the
/// right `start_*`). Kept here so both layout modules share it.
impl RunKind {
    /// Launch the matching engine off-thread.
    pub fn start(self, app: &mut AppState, tab_id: TabId, note_path: &str) {
        match self {
            RunKind::Crawl => start_crawl(app, tab_id, note_path),
            RunKind::Feed => start_feed_poll(app, tab_id, note_path),
        }
    }
}
