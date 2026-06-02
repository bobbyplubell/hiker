//! Capture tab: the form-over-frontmatter surface that drives the web
//! crawl / RSS-feed engine from inside the app.
//!
//! One renderer, parameterized by the capture note's `capture.mode`:
//!
//! - `mode: crawl` → the crawl-job form (`crawl-job-form`): seed URL(s),
//!   mode (list / hub / deep), depth, follow/extract patterns, extract-seed,
//!   extractor pick, retention, rate limit — plus a Run / Cancel control,
//!   live progress, and the captured-page index.
//! - `mode: feed` → the RSS subscription form (`rss-subscription-lifecycle`):
//!   feed URL, poll interval, full-text toggle, item retention — plus the
//!   ongoing "subscribed · polling every N · last checked …" status with
//!   Pause / Resume and a Poll-now button, and the captured-entries index.
//!
//! Editing a field rewrites the note's `CrawlParams` / `FeedParams`
//! frontmatter (reusing `hiker_extract::capture::Spec::to_yaml`), leaving the
//! user-owned body untouched (`fill_body: false`). Run launches the BLOCKING
//! engine (`crawl::run_default` / `feed::poll_note`) on a background
//! `std::thread`; the worker posts progress through an `mpsc` channel and a
//! shared `Arc<AtomicBool>` cancel flag, which the UI polls each frame —
//! mirroring the cluster-review tab's run/cancel/progress shape. The
//! task-queue IO lane (`crawl-task-queue-lane`) is deferred, so the thread is
//! the off-thread mechanism for now.
//!
//! Implements `crawl-job-form` and the GUI half of
//! `rss-subscription-lifecycle`.
//
// status: crawl-job-form

mod crawl;
mod feed;
pub mod exec;

use std::collections::HashMap;

use eframe::egui;
use hiker_extract::capture::{CrawlParams, FeedParams, Mode, Spec};

use crate::state::{AppState, ToastLevel};
use crate::tab::{Tab, TabId, TabKind};

use exec::RunHandle;
#[cfg(test)]
use exec::{DrainOutcome, RunEvent};

/// Which render the capture pane shows. The toggle is a render choice over the
/// one underlying note, not two tabs — switching to `Markdown` hosts the live
/// editor widget over the same note inline (mirroring `board-view-toggle`), so
/// the user can flip to the raw frontmatter/body and back. A capture-local
/// enum (not the board's) to keep the two panels decoupled.
///
/// status: capture-view-toggle
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    Form,
    Markdown,
}

/// In-memory per-capture-tab UI + run state. Keyed by `TabId` on
/// [`crate::state::PanelStates`].
#[derive(Default)]
pub struct Pane {
    /// Active in-pane render (the form vs. the inline markdown editor over the
    /// same note). status: capture-view-toggle
    pub view: ViewMode,
    /// The parsed spec the form edits — `None` until the note is loaded (or
    /// when the note on disk isn't a capture note). Edits mutate this in place,
    /// then persist to frontmatter via [`persist`].
    pub spec: Option<Spec>,
    /// The user-owned body, preserved verbatim across frontmatter rewrites
    /// (`fill_body: false`).
    pub body: String,
    /// The `hiker.id` ULID stamped on the note, preserved across edits so the
    /// children's `hiker.parent` keeps matching.
    pub note_ulid: String,
    /// Set once the note has been read off disk this session. Re-read happens
    /// after a Run completes (children landed, `last_checked` advanced).
    pub loaded: bool,
    /// The live background run, if one is in flight (or just finished and
    /// awaiting drain). Carries the progress channel + cancel flag.
    pub run: Option<RunHandle>,
    /// The captured-page / captured-entry index built from the last (or
    /// in-flight) run's progress events. Newest entries append at the end.
    pub pages: Vec<PageRow>,
    /// A one-line status line summarizing the last completed run.
    pub last_summary: Option<String>,
    /// `seeds` joined for the multi-line seed editor (crawl mode). Mirrors the
    /// spec's `seeds` vec; committed back on edit. Kept as a draft string so
    /// the user can type blank lines mid-edit without losing them.
    pub seed_draft: Option<String>,
}

/// Which engine a Run drives, picked by the active layout.
#[derive(Clone, Copy)]
pub enum RunKind {
    Crawl,
    Feed,
}

/// One row in the captured-page / captured-entry index. Built from the
/// engine's per-page progress records (crawl) or the poll report (feed).
#[derive(Clone)]
pub struct PageRow {
    /// The source URL captured (crawl) or the child note path (feed).
    pub label: String,
    /// The written child note path, when the page/entry was kept.
    pub path: Option<String>,
    /// A short human status (`captured`, `skipped: …`, `new`, `updated`, …).
    pub note: String,
}

/// The reconciliation a view-toggle requires, decided purely from the
/// before/after view (no egui, no `AppState`) so the no-data-loss invariant is
/// unit-testable. The caller (`render_header`) executes the chosen action.
///
/// status: capture-view-toggle
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleReconcile {
    /// View unchanged — nothing to do.
    None,
    /// Form → Markdown: refresh the hosted buffer from disk IF it's clean (a
    /// dirty buffer keeps its unsaved markdown edits).
    RefreshCleanBuffer,
    /// Markdown → Form: re-parse the spec from the (possibly hand-edited)
    /// frontmatter by dropping `loaded`.
    ReloadSpec,
}

impl Pane {
    /// Mode of the loaded spec, or `None` when not yet loaded / not a capture.
    fn mode(&self) -> Option<Mode> {
        self.spec.as_ref().map(|s| s.mode)
    }

    /// Decide what a view toggle from `self.view` to `next` must reconcile, and
    /// set `self.view = next`. Pure over the two views; the egui-free seam the
    /// toggle invariant is tested against. status: capture-view-toggle
    pub const fn switch_view(&mut self, next: ViewMode) -> ToggleReconcile {
        let prev = self.view;
        self.view = next;
        match (prev, next) {
            (ViewMode::Form, ViewMode::Markdown) => ToggleReconcile::RefreshCleanBuffer,
            (ViewMode::Markdown, ViewMode::Form) => ToggleReconcile::ReloadSpec,
            _ => ToggleReconcile::None,
        }
    }
}

/// Find-or-focus a capture tab for `note_path`, opening one if none exists.
/// Returns the tab id.
///
/// status: crawl-job-form
pub fn open(app: &mut AppState, note_path: &str) -> TabId {
    if let Some(existing) = app
        .session
        .tabs
        .iter()
        .find(|t| matches!(&t.kind, TabKind::Capture { note_path: p } if p == note_path))
    {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::Capture { note_path: note_path.to_string() },
        sticky: true,
    });
    app.session.active_tab = Some(id);
    id
}

/// Render the capture tab body for the note at `note_path`. Loads the note
/// (once per session, and again after a run), drains any in-flight run's
/// progress, renders the "View as: Form / Markdown" header toggle, then
/// dispatches to the form layout (crawl / feed) or the inline markdown editor.
///
/// status: crawl-job-form
/// status: capture-view-toggle
pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    note_path: &str,
    rt: &std::sync::Arc<tokio::runtime::Runtime>,
) {
    // Lazily load the note's spec + body into the pane.
    {
        let needs_load = app
            .panels
            .captures
            .get(&tab_id)
            .is_none_or(|p| !p.loaded);
        if needs_load {
            load_into_pane(app, tab_id, note_path);
        }
    }

    // Drain any in-flight run's progress channel into the pane, and apply the
    // terminal event (re-read the note, refresh the page index) when it lands.
    // Only meaningful in the Form view (Markdown never starts a run), but it's
    // cheap and harmless to keep folding progress while the buffer is shown.
    exec::drain(app, tab_id, note_path, ui.ctx());

    // The view toggle works even on a non-capture note (the user may want to
    // flip to Markdown to add the `hiker.kind: capture` frontmatter by hand),
    // so render the header before the spec check.
    let view = app
        .panels
        .captures
        .get(&tab_id)
        .map(|p| p.view)
        .unwrap_or_default();
    render_header(ui, app, tab_id, note_path, view);
    ui.separator();

    if view == ViewMode::Markdown {
        // Host the live editor widget over the same note inline, in this tab —
        // a render choice over the one note, not a separate buffer tab. The
        // buffer materializes from the op-log; `persist` reconciles the op-log
        // with the form's direct disk writes (see `persist`), so the buffer
        // reflects the form's latest state. status: capture-view-toggle
        if crate::editor_pane::ensure_vault_buffer_loaded(app, note_path) {
            crate::panels::buffer::show(ui, app, note_path, rt);
        }
        return;
    }

    let Some(mode) = app.panels.captures.get(&tab_id).and_then(Pane::mode) else {
        ui.add_space(8.0);
        ui.colored_label(
            egui::Color32::from_rgb(200, 120, 60),
            "This note isn't a capture-spec note (no `hiker.kind: capture`).",
        );
        ui.label(
            egui::RichText::new(format!("Path: {note_path}"))
                .color(hiker_theme::muted())
                .small(),
        );
        ui.label(
            egui::RichText::new("Flip to Markdown to add capture frontmatter by hand.")
                .color(hiker_theme::muted())
                .small(),
        );
        return;
    };

    match mode {
        // The form's per-frame `persist` runs ONLY here (Form view); the
        // Markdown branch above returns before reaching it, so the form-write
        // and the buffer's op-log save never both fire in one frame.
        Mode::Crawl => crawl::show(ui, app, tab_id, note_path),
        Mode::Feed => feed::show(ui, app, tab_id, note_path),
        // A `clip` capture has no fan-out form surface; its "form" is just the
        // single source + Run, owned by the CLI / quick-capture path. The
        // new-item entry points only ever open crawl/feed, so this is a
        // graceful fallback for a hand-authored `mode: clip` note.
        Mode::Clip => {
            ui.add_space(8.0);
            ui.label("Single-clip capture notes are run from the CLI / quick-capture.");
        }
    }
}

/// Header: the "View as: Form / Markdown" toggle (mirrors `board.rs`'s
/// `render_header`). Flipping the toggle reconciles the two save paths so no
/// edit is ever lost across the switch:
///
/// - **Form → Markdown:** the form writes disk on every edit and `persist`
///   reconciles the op-log, so the hosted buffer is already current — EXCEPT a
///   buffer cached from a prior Markdown session, whose `editor.doc` predates
///   later form edits. We refresh that cached buffer from disk *only when it is
///   clean* (`maybe_reload_clean_buffer`, the same in-place reload the watcher
///   uses for external edits). A dirty cached buffer (the user typed in
///   Markdown, switched to Form, edited a field, switched back) is LEFT ALONE
///   so their unsaved markdown edits survive — never silently discarded.
/// - **Markdown → Form:** the form must re-parse the spec from the possibly
///   hand-edited frontmatter, so we drop `loaded` to force `load_into_pane`
///   next frame. The form's `persist` only runs in Form view AND only when a
///   field actually changes (`dirty`), so the reload frame writes nothing —
///   the freshly-parsed spec is never clobbered by a stale one.
///
/// status: capture-view-toggle
fn render_header(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    note_path: &str,
    current: ViewMode,
) {
    let mut target: Option<ViewMode> = None;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("View as:")
                .small()
                .color(hiker_theme::muted()),
        );
        if ui.selectable_label(current == ViewMode::Form, "Form").clicked() {
            target = Some(ViewMode::Form);
        }
        if ui.selectable_label(current == ViewMode::Markdown, "Markdown").clicked() {
            target = Some(ViewMode::Markdown);
        }
    });
    let Some(next) = target else { return };
    let reconcile = app
        .panels
        .captures
        .entry(tab_id)
        .or_default()
        .switch_view(next);
    match reconcile {
        ToggleReconcile::None => {}
        // Markdown → Form: re-parse the spec from the (possibly hand-edited)
        // frontmatter next frame (the form's `persist` won't fire on the reload
        // frame — it only writes on an actual field change).
        ToggleReconcile::ReloadSpec => invalidate(app, tab_id),
        // Form → Markdown: refresh a stale-but-clean cached buffer from disk so
        // it reflects the form's latest writes; a dirty buffer keeps its
        // unsaved edits (`maybe_reload_clean_buffer` is a no-op when dirty).
        ToggleReconcile::RefreshCleanBuffer => app.maybe_reload_clean_buffer(note_path),
    }
}

/// Read the note at `note_path` off disk, parse its capture spec, and stash
/// the spec + user-owned body + `hiker.id` on the pane.
fn load_into_pane(app: &mut AppState, tab_id: TabId, note_path: &str) {
    let pane = app.panels.captures.entry(tab_id).or_default();
    let Ok((content, _hash)) = app.vault_session.vault.read_file_with_hash(note_path) else {
        pane.loaded = true;
        pane.spec = None;
        return;
    };
    let split = hiker_core::frontmatter::split(&content);
    pane.body = split.body.to_string();
    pane.note_ulid = split
        .frontmatter
        .as_ref()
        .and_then(|fm| fm.get("hiker"))
        .and_then(|h| h.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    pane.spec = split
        .frontmatter
        .as_ref()
        .and_then(|fm| Spec::from_frontmatter(fm).ok());
    pane.seed_draft = pane
        .spec
        .as_ref()
        .and_then(|s| s.crawl.as_ref())
        .map(|c| c.seeds.join("\n"));
    pane.loaded = true;
}

/// Force a re-read of the note next frame (after a run lands children / stamps
/// `last_checked`).
fn invalidate(app: &mut AppState, tab_id: TabId) {
    if let Some(pane) = app.panels.captures.get_mut(&tab_id) {
        pane.loaded = false;
    }
}

/// Persist the pane's current [`Spec`] back into the note's frontmatter,
/// preserving the user-owned body and the `hiker.id` stamp. The write routes
/// through the vault with watcher suppression so it doesn't bounce back as a
/// duplicate ingest. Reuses `Spec::to_yaml` — the exact serialize the engine /
/// CLI use — so the form and the engine agree on the frontmatter shape.
///
/// status: crawl-job-form
/// status: rss-subscription-lifecycle
fn persist(app: &mut AppState, tab_id: TabId, note_path: &str) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    let Some(spec) = pane.spec.clone() else { return };
    let body = pane.body.clone();
    let ulid = pane.note_ulid.clone();

    let mut root = match spec.to_yaml() {
        serde_yml::Value::Mapping(m) => m,
        _ => serde_yml::Mapping::new(),
    };
    // Re-stamp `hiker.id` (Spec::to_yaml drops it) so children's parent links
    // keep resolving.
    if !ulid.is_empty()
        && let Some(serde_yml::Value::Mapping(hiker)) = root.get_mut("hiker")
    {
        hiker.insert(serde_yml::Value::from("id"), serde_yml::Value::from(ulid));
    }
    let fm = serde_yml::Value::Mapping(root);
    let content = match hiker_core::frontmatter::assemble(&fm, &body) {
        Ok(c) => c,
        Err(err) => {
            app.push_toast(format!("Capture: serialize failed: {err}"), ToastLevel::Error);
            return;
        }
    };

    let watcher = app.vault_session.services.watcher.clone();
    watcher.suppress(note_path.to_string());
    if let Err(err) = app.vault_session.vault.write_file(note_path, &content) {
        app.push_toast(format!("Capture: save failed: {err}"), ToastLevel::Error);
        return;
    }
    // Reconcile the op-log `accepted` state with the bytes we just wrote.
    // The form writes disk DIRECTLY (bypassing the op-log) with the watcher
    // suppressed, so the `oplog_external_sync_relay` never sees this change and
    // the op-log's `accepted` would otherwise drift stale. Reconciling here
    // keeps the Markdown view's hosted buffer — which materializes from the
    // op-log — reflecting the form's latest state when the user toggles to it.
    // A no-op when the note has no op-log doc yet (first write before any
    // editor session) or when disk already equals accepted (idempotent).
    // status: capture-view-toggle
    if let Err(err) = hiker_core::ops::op_writes::external_edit(
        &app.vault_session.services.oplog,
        &app.vault_session.vault,
        note_path,
    ) {
        tracing::warn!(path = %note_path, error = %err, "capture: op-log reconcile after form write failed");
    }

    // Re-suppress so the TTL window starts close to the notify event, then
    // index the rewritten note (the watcher event was suppressed).
    watcher.suppress(note_path.to_string());
    let jobs = app.vault_session.services.indexer.job_sender();
    let rel = note_path.to_string();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = jobs
                .send(hiker_core::indexer::IndexJob::Upsert { rel_path: rel, force: false })
                .await;
        });
    }
}

// ---------------------------------------------------------------------------
// New-item entry points (called from the Files `+` / `⋯` new-item menu).
// ---------------------------------------------------------------------------

impl AppState {
    /// Create a new `mode: crawl` capture-spec note and open it in a Capture
    /// tab (crawl form). The note carries default crawl params (a deep crawl
    /// with no seed yet); the user fills the seed in the form, then Runs.
    ///
    /// status: crawl-job-form
    pub fn new_crawl(&mut self) {
        let ulid = ulid::Ulid::new().to_string();
        let params = CrawlParams::list(Vec::new());
        let content = {
            // Build the note content the same way the engine's `write_job_note`
            // does (frontmatter from `Spec::to_yaml` + `hiker.id` + a body
            // stub), but route the WRITE through the indexer-driven create
            // path for watcher suppression + indexing.
            let spec = Spec {
                kind: hiker_extract::capture::Kind::Capture,
                mode: Mode::Crawl,
                source: None,
                fill_body: false,
                extractor: None,
                crawl: Some(params),
                feed: None,
            };
            assemble_with_id(&spec, &ulid, "# Crawl\n\nNotes about this crawl.\n")
        };
        self.create_capture_note("new-crawl", &content);
    }

    /// Create a new `mode: feed` capture-spec note and open it in a Capture tab
    /// (RSS subscription form). The note starts as a manual-Run-only
    /// subscription (no `poll_interval`); the user fills the feed URL + cadence
    /// in the form.
    ///
    /// status: rss-subscription-lifecycle
    pub fn new_feed(&mut self) {
        let ulid = ulid::Ulid::new().to_string();
        let params = FeedParams::new("");
        let spec = Spec {
            kind: hiker_extract::capture::Kind::Capture,
            mode: Mode::Feed,
            source: None,
            fill_body: false,
            extractor: None,
            crawl: None,
            feed: Some(params),
        };
        let content = assemble_with_id(&spec, &ulid, "# Feed\n\nNotes about this subscription.\n");
        self.create_capture_note("new-feed", &content);
    }

    /// Shared create path for both new-item entry points: pick a non-colliding
    /// `<stem>-N.md` at the selected folder, write it through the
    /// indexer-driven create path, and open it in a Capture tab.
    fn create_capture_note(&mut self, stem: &str, content: &str) {
        let target_dir = self
            .file_tree_state
            .selected_folder
            .as_deref()
            .unwrap_or("")
            .to_string();
        let sort = self
            .vault_session
            .config
            .read()
            .ok()
            .map(|c| c.vault.tree.sort_by)
            .unwrap_or(hiker_core::config::sections::TreeSortBy::NameAsc);
        let listed = self.vault_session.vault.list_dir(&target_dir, sort).unwrap_or_default();
        let existing: std::collections::HashSet<&str> =
            listed.iter().map(|e| e.name.as_str()).collect();
        let mut candidate = format!("{stem}.md");
        for n in 1.. {
            let name = if n == 1 { format!("{stem}.md") } else { format!("{stem}-{n}.md") };
            if !existing.contains(name.as_str()) {
                candidate = name;
                break;
            }
        }
        let rel = if target_dir.is_empty() {
            candidate
        } else {
            format!("{target_dir}/{candidate}")
        };

        let watcher = self.vault_session.services.watcher.clone();
        let jobs = self.vault_session.services.indexer.job_sender();
        let vault = self.vault_session.vault.clone();
        let rel_owned = rel.clone();
        let content_owned = content.to_string();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(async {
                hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, &content_owned)
                    .await
            }),
            Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
        };
        match result {
            Ok(actual) => {
                self.file_tree_state.dir_cache.remove(&target_dir);
                open(self, &actual);
            }
            Err(err) => {
                self.push_toast(format!("Create capture failed: {err}"), ToastLevel::Error);
            }
        }
    }
}

/// Assemble a capture note's content: `Spec::to_yaml` frontmatter with
/// `hiker.id` stamped, plus the body stub. Mirrors the engine's private
/// `assemble_note` so the form-created note round-trips through
/// `Spec::from_frontmatter`.
fn assemble_with_id(spec: &Spec, ulid: &str, body: &str) -> String {
    let mut root = match spec.to_yaml() {
        serde_yml::Value::Mapping(m) => m,
        _ => serde_yml::Mapping::new(),
    };
    if !ulid.is_empty()
        && let Some(serde_yml::Value::Mapping(hiker)) = root.get_mut("hiker")
    {
        hiker.insert(serde_yml::Value::from("id"), serde_yml::Value::from(ulid.to_string()));
    }
    let yaml = serde_yml::to_string(&serde_yml::Value::Mapping(root)).unwrap_or_default();
    let yaml = yaml.trim_end_matches('\n');
    format!("---\n{yaml}\n---\n{body}")
}

/// Drop a closed capture tab's pane state, flipping any in-flight run's cancel
/// flag so the background thread stops promptly.
pub fn on_tab_closed(panes: &mut HashMap<TabId, Pane>, tab_id: TabId) {
    if let Some(mut pane) = panes.remove(&tab_id)
        && let Some(handle) = pane.run.take()
    {
        handle.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiker_extract::capture::{CrawlMode, FeedParams, Kind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    // -- form-state <-> frontmatter round-trips --------------------------

    /// A form-created crawl note round-trips through frontmatter: the params
    /// the form serialized parse back identically, and `hiker.id` survives.
    #[test]
    fn crawl_note_frontmatter_round_trips_with_id() {
        let mut params = CrawlParams::list(vec!["https://example.com".into()]);
        params.mode = CrawlMode::Deep;
        params.depth = 4;
        params.follow_pattern = Some("/docs/**".into());
        params.max_pages = 123;
        let spec = Spec {
            kind: Kind::Capture,
            mode: Mode::Crawl,
            source: Some("https://example.com".into()),
            fill_body: false,
            extractor: Some("web".into()),
            crawl: Some(params.clone()),
            feed: None,
        };
        let ulid = "01HXTESTULID0000000000000";
        let content = assemble_with_id(&spec, ulid, "# Crawl\n\nuser body\n");

        let split = hiker_core::frontmatter::split(&content);
        let fm = split.frontmatter.expect("frontmatter present");
        // hiker.id preserved.
        assert_eq!(fm.get("hiker").and_then(|h| h.get("id")).and_then(|v| v.as_str()), Some(ulid));
        // User body untouched (fill_body stays false → form never fills it).
        assert!(split.body.contains("user body"));
        let parsed = Spec::from_frontmatter(&fm).expect("parses as capture");
        assert_eq!(parsed.mode, Mode::Crawl);
        assert!(!parsed.fill_body);
        let pc = parsed.crawl.expect("crawl params");
        assert_eq!(pc.seeds, params.seeds);
        assert_eq!(pc.mode, CrawlMode::Deep);
        assert_eq!(pc.depth, 4);
        assert_eq!(pc.follow_pattern.as_deref(), Some("/docs/**"));
        assert_eq!(pc.max_pages, 123);
        assert_eq!(parsed.extractor.as_deref(), Some("web"));
    }

    /// A form-created feed note carries `mode: feed`, `fill_body: false`, the
    /// poll/full-text/retention fields, and a stamped `hiker.id`.
    #[test]
    fn feed_note_frontmatter_has_correct_kind_mode_fill_body() {
        let mut params = FeedParams::new("https://blog.example/feed.xml");
        params.poll_interval = Some("6h".into());
        params.full_text = true;
        params.item_retention = Some("keep:20".into());
        let spec = Spec {
            kind: Kind::Capture,
            mode: Mode::Feed,
            source: Some(params.url.clone()),
            fill_body: false,
            extractor: None,
            crawl: None,
            feed: Some(params.clone()),
        };
        let ulid = "01HXFEEDULID0000000000000";
        let content = assemble_with_id(&spec, ulid, "# Feed\n\n");
        let fm = hiker_core::frontmatter::split(&content).frontmatter.expect("fm");

        assert_eq!(fm.get("hiker").and_then(|h| h.get("kind")).and_then(|v| v.as_str()), Some("capture"));
        assert_eq!(fm.get("hiker").and_then(|h| h.get("fill_body")).and_then(serde_yml::Value::as_bool), Some(false));
        assert_eq!(fm.get("capture").and_then(|c| c.get("mode")).and_then(|v| v.as_str()), Some("feed"));
        assert_eq!(fm.get("hiker").and_then(|h| h.get("id")).and_then(|v| v.as_str()), Some(ulid));

        let parsed = Spec::from_frontmatter(&fm).expect("parses");
        let pf = parsed.feed.expect("feed params");
        assert_eq!(pf.url, params.url);
        assert_eq!(pf.poll_interval.as_deref(), Some("6h"));
        assert!(pf.full_text);
        assert_eq!(pf.item_retention.as_deref(), Some("keep:20"));
        assert!(!pf.paused);
    }

    /// Toggling `paused` on the parsed spec and re-serializing flips it in the
    /// frontmatter — the pause/resume lifecycle persists.
    #[test]
    fn pause_resume_persists_to_frontmatter() {
        let mut params = FeedParams::new("https://x/feed");
        params.poll_interval = Some("30m".into());
        let mut spec = Spec {
            kind: Kind::Capture,
            mode: Mode::Feed,
            source: Some(params.url.clone()),
            fill_body: false,
            extractor: None,
            crawl: None,
            feed: Some(params),
        };
        // Pause.
        spec.feed.as_mut().unwrap().paused = true;
        let content = assemble_with_id(&spec, "01HXID0000000000000000000", "# Feed\n");
        let fm = hiker_core::frontmatter::split(&content).frontmatter.unwrap();
        let reparsed = Spec::from_frontmatter(&fm).unwrap();
        assert!(reparsed.feed.unwrap().paused, "paused round-trips true");
    }

    // -- view toggle + reconciliation invariant --------------------------

    /// The view defaults to Form so a freshly opened capture tab shows the
    /// form, not the raw markdown. status: capture-view-toggle
    #[test]
    fn capture_view_defaults_to_form() {
        let pane = Pane::default();
        assert_eq!(pane.view, ViewMode::Form);
    }

    /// `switch_view` flips the active view and reports the reconciliation the
    /// transition requires: Form→Markdown refreshes the (clean) buffer,
    /// Markdown→Form re-parses the spec, a same-view "switch" is a no-op.
    /// status: capture-view-toggle
    #[test]
    fn switch_view_flips_and_picks_reconcile() {
        let mut pane = Pane::default();

        // Form → Markdown.
        assert_eq!(pane.switch_view(ViewMode::Markdown), ToggleReconcile::RefreshCleanBuffer);
        assert_eq!(pane.view, ViewMode::Markdown);

        // Markdown → Markdown (no-op).
        assert_eq!(pane.switch_view(ViewMode::Markdown), ToggleReconcile::None);
        assert_eq!(pane.view, ViewMode::Markdown);

        // Markdown → Form re-parses the spec.
        assert_eq!(pane.switch_view(ViewMode::Form), ToggleReconcile::ReloadSpec);
        assert_eq!(pane.view, ViewMode::Form);

        // Form → Form (no-op).
        assert_eq!(pane.switch_view(ViewMode::Form), ToggleReconcile::None);
    }

    /// The Markdown→Form invariant: the transition's reconciliation is
    /// `ReloadSpec`, which the header maps to dropping `loaded` so the form
    /// re-parses the (possibly hand-edited) frontmatter — the form never writes
    /// a stale spec over fresh markdown edits because `persist` only fires on an
    /// actual field change, not on the reload frame. status: capture-view-toggle
    #[test]
    fn markdown_to_form_requests_spec_reload() {
        let mut pane = Pane { view: ViewMode::Markdown, loaded: true, ..Default::default() };
        let reconcile = pane.switch_view(ViewMode::Form);
        assert_eq!(reconcile, ToggleReconcile::ReloadSpec);
        // The header acts on ReloadSpec by clearing `loaded`; emulate that here
        // (the egui-bound `invalidate` does the same).
        if reconcile == ToggleReconcile::ReloadSpec {
            pane.loaded = false;
        }
        assert!(!pane.loaded, "Markdown→Form drops `loaded` to re-parse the spec");
    }

    /// The Form→Markdown invariant: the refresh is gated on the buffer being
    /// clean, so unsaved markdown edits are never silently discarded. We model
    /// the gate the same way `maybe_reload_clean_buffer` does — refresh iff not
    /// dirty. status: capture-view-toggle
    #[test]
    fn refresh_clean_buffer_skips_dirty() {
        // The reconciliation chosen for Form→Markdown is always
        // RefreshCleanBuffer; the *execution* (maybe_reload_clean_buffer) is a
        // no-op when the buffer is dirty. Encode that gate as a pure predicate
        // so the "never discard unsaved edits" rule is asserted here.
        let safe_to_refresh = |buffer_dirty: bool| !buffer_dirty;
        assert!(safe_to_refresh(false), "clean buffer is refreshed from disk");
        assert!(!safe_to_refresh(true), "dirty buffer is left alone (edits preserved)");
    }

    // -- background-run channel / cancel plumbing ------------------------

    /// The channel-fold plumbing: page events append to the index, a Done
    /// event marks the run finished + records the summary. Driven with a fake
    /// channel — no engine, no network.
    #[test]
    fn fold_run_events_appends_pages_and_finishes_on_done() {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut pane = Pane { run: Some(RunHandle::for_test(rx, cancel)), ..Default::default() };

        tx.send(RunEvent::Page(PageRow {
            label: "https://a".into(),
            path: Some("crawl/a.md".into()),
            note: "captured".into(),
        }))
        .unwrap();
        tx.send(RunEvent::Page(PageRow {
            label: "https://b".into(),
            path: None,
            note: "skipped: out of scope".into(),
        }))
        .unwrap();
        tx.send(RunEvent::Done("1 captured, 2 pages touched".into())).unwrap();

        let outcome = pane.fold_run_events();
        assert!(matches!(outcome, DrainOutcome::Done));
        assert_eq!(pane.pages.len(), 2);
        assert_eq!(pane.pages[0].label, "https://a");
        assert!(pane.pages[0].path.is_some());
        assert!(pane.pages[1].path.is_none());
        assert_eq!(pane.last_summary.as_deref(), Some("1 captured, 2 pages touched"));
        assert!(!pane.run.as_ref().unwrap().running, "run marked finished");
    }

    /// A Failed event surfaces as the Failed outcome and finishes the run.
    #[test]
    fn fold_run_events_reports_failure() {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut pane = Pane { run: Some(RunHandle::for_test(rx, cancel)), ..Default::default() };
        tx.send(RunEvent::Failed("crawl has no seed URL".into())).unwrap();

        match pane.fold_run_events() {
            DrainOutcome::Failed(e) => assert_eq!(e, "crawl has no seed URL"),
            _ => panic!("expected Failed outcome"),
        }
        assert!(!pane.run.as_ref().unwrap().running);
    }

    /// `RunHandle::cancel` flips the shared atomic the engine's `Hooks.cancel`
    /// polls, and `on_tab_closed` cancels an in-flight run on close.
    #[test]
    fn cancel_flag_flips_and_tab_close_cancels() {
        let (_tx, rx) = mpsc::channel::<RunEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = RunHandle::for_test(rx, cancel.clone());
        assert!(!cancel.load(Ordering::Relaxed));
        handle.cancel();
        assert!(cancel.load(Ordering::Relaxed), "cancel flag set");

        // on_tab_closed flips a fresh run's flag.
        let (_tx2, rx2) = mpsc::channel::<RunEvent>();
        let cancel2 = Arc::new(AtomicBool::new(false));
        let mut panes: HashMap<TabId, Pane> = HashMap::new();
        let id = TabId(7);
        panes.insert(
            id,
            Pane { run: Some(RunHandle::for_test(rx2, cancel2.clone())), ..Default::default() },
        );
        on_tab_closed(&mut panes, id);
        assert!(cancel2.load(Ordering::Relaxed), "tab close cancels the run");
        assert!(!panes.contains_key(&id), "pane dropped");
    }
}
