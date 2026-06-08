//! hiker — egui-based desktop app.
//!
//! Entry point: parses `--vault <path>` or falls back to the configured
//! default vault, opens it, and launches the eframe loop.

mod actions;
mod activity;
mod appears_in;
mod autocomplete;
mod backlinks;
mod buffer;
mod buffer_view;
mod bootstrap;
mod canvas_activity;
mod projects_activity;
mod charts;
mod chat;
mod clusters;
mod command_center;
mod completion_sources;
mod context;
mod editor_pane;
mod files;
mod icons;
mod item_menu;
mod keybinds;
// Old `palette.rs` was replaced by `panels::command_palette` per the
// `command-palette` spec; the new module re-exports `command_palette`
// on `AppState`, so the call site in `update()` is unchanged.
mod os_open;
mod panels;
mod git_sync;
mod profiling;
mod related;
mod search;
mod side_panel_persist;
mod sidebar;
mod state;
mod sync_service;
mod tab;
mod titlebar;
mod toolbar;
mod trails;
mod trash;
mod vault_view;
mod widgets;
mod workbench_host;

#[cfg(test)]
mod smoke_tests;

use std::path::PathBuf;

use eframe::egui;

use crate::state::{AppState, VaultSwitchState};

fn main() -> eframe::Result<()> {
    // Logging level/targets are runtime-configurable via `RUST_LOG`
    // (e.g. `RUST_LOG=info,hiker::widget_click=debug`); defaults to `info`
    // when the env var is unset or unparseable.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    profiling::Profiler.init_server();

    let vault_arg = std::env::args().nth(1).map(PathBuf::from);
    let vault_path = vault_arg
        .or_else(|| {
            // Default-vault resolution: pull from settings or fall
            // back to ~/notes. For v0 we just honour the explicit argument
            // and a hard-coded fallback in the CWD; settings-driven
            // resolution lands when we wire the settings store.
            std::env::var("HIKER_VAULT").ok().map(PathBuf::from)
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
        });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let mut state = runtime
        .block_on(async { bootstrap::open_vault(vault_path).await })
        .expect("open vault");

    // Custom-titlebar pref now lives on `Config::ui` (vault-scoped) and
    // round-trips through the standard settings persistence path. The
    // legacy `<vault>/.hiker/ui.json` sidecar is still consulted as a
    // one-shot migration so users who toggled it before this change
    // don't lose the bit.
    let custom_titlebar = state
        .vault_session
        .config
        .read()
        .map(|c| c.ui.custom_titlebar)
        .unwrap_or(true)
        || std::fs::read(state.vault_session.vault_root.join(".hiker/ui.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("custom_titlebar").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
    state.ui.custom_titlebar = custom_titlebar;
    // Reader-mode chrome preferences. [view-reader-hide-top-bar,
    // view-reader-hide-tabs, view-reader-hide-toolbar]
    if let Ok(cfg) = state.vault_session.config.read() {
        state.ui.reader_hide_top_bar = cfg.ui.reader_hide_top_bar;
        state.ui.reader_hide_tabs = cfg.ui.reader_hide_tabs;
        state.ui.reader_hide_toolbar = cfg.ui.reader_hide_toolbar;
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("hiker")
        .with_inner_size([1400.0, 900.0])
        .with_min_inner_size([800.0, 500.0]);
    if custom_titlebar {
        viewport = viewport
            .with_decorations(false)
            .with_titlebar_shown(false)
            .with_titlebar_buttons_shown(false);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // Bench knob: HIKER_NO_VSYNC=1 uncaps the frame rate so a profiling run
        // measures the real CPU+GPU frame cost instead of the vsync wait.
        vsync: std::env::var_os("HIKER_NO_VSYNC").is_none(),
        ..Default::default()
    };

    let app_runtime = std::sync::Arc::new(runtime);

    eframe::run_native(
        "hiker",
        native_options,
        // App construction is inlined here (rather than a `HikerApp::new`
        // associated fn) so the single call site doesn't trip
        // `single_call_fn`: install the theme + user fonts + image loaders
        // against the freshly-created egui context, then hand back the App.
        Box::new(move |cc| {
            hiker_theme::Theme.install(&cc.egui_ctx);
            state.install_user_fonts(&cc.egui_ctx);
            // Seed the per-frame `refresh_user_fonts` fingerprint so it
            // doesn't redundantly re-install on the first paint. Live-flip
            // detects changes by comparing this against the current config.
            // status: editor-three-fonts
            if let Ok(cfg) = state.vault_session.config.read() {
                state.ui.last_fonts_fp = Some(format!(
                    "{}\0{}\0{}",
                    cfg.editor.font_system, cfg.editor.font_editor, cfg.editor.font_code
                ));
            }
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(HikerApp {
                state,
                runtime: app_runtime,
                bench_focused: false,
            }))
        }),
    )
}

// (Tracing init is inlined at the only call site in `main`.)

pub(crate) struct HikerApp {
    state: AppState,
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
    /// One-shot latch for the `HIKER_FOCUS` bench helper (see `update`).
    bench_focused: bool,
}

impl AppState {
    /// Load user-configured font files (per `editor.font_*` settings)
    /// into egui's font registry, mapping them to the Proportional /
    /// Monospace families. Empty paths or unreadable files fall back to
    /// egui's bundled defaults. Best-effort; errors are logged, not
    /// surfaced.
    ///
    /// status: editor-three-fonts
    pub(crate) fn install_user_fonts(&self, ctx: &egui::Context) {
        let state = self;
        let cfg = match state.vault_session.config.read() {
            Ok(c) => c,
            Err(_) => return,
        };
        let e = &cfg.editor;
        let mut defs = egui::FontDefinitions::default();
        let mut load = |label: &str, path: &str, family: egui::FontFamily| {
            if path.is_empty() {
                return;
            }
            match std::fs::read(path) {
                Ok(bytes) => {
                    let name = format!("user-{label}");
                    defs.font_data.insert(
                        name.clone(),
                        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                    );
                    defs.families.entry(family).or_default().insert(0, name);
                }
                Err(err) => {
                    tracing::warn!(error = %err, path, "font load failed");
                }
            }
        };
        // System + editor fonts both feed Proportional; the editor body is
        // currently monospace-tied (see `format_for` in editor-egui) so the
        // editor entry effectively shadows the system one when both are set.
        load("system", &e.font_system, egui::FontFamily::Proportional);
        load("editor", &e.font_editor, egui::FontFamily::Proportional);
        load("code", &e.font_code, egui::FontFamily::Monospace);
        ctx.set_fonts(defs);
    }

    /// Re-install user fonts if any of the three slots changed since the
    /// last install. Called every frame; the common case (no changes)
    /// costs only a string compare against `ui.last_fonts_fp`.
    ///
    /// status: editor-three-fonts
    pub(crate) fn refresh_user_fonts(&mut self, ctx: &egui::Context) {
        let fp = match self.vault_session.config.read() {
            Ok(c) => format!(
                "{}\0{}\0{}",
                c.editor.font_system, c.editor.font_editor, c.editor.font_code
            ),
            Err(_) => return,
        };
        if self.ui.last_fonts_fp.as_deref() == Some(fp.as_str()) {
            return;
        }
        self.install_user_fonts(ctx);
        self.ui.last_fonts_fp = Some(fp);
    }
}

impl eframe::App for HikerApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Drop ZIM panes (each owning a `hiker-htmlview` view with cached egui
        // textures + a stylo-styled document, plus a memmap-backed archive)
        // while the main thread's TLS is still alive, releasing those resources
        // deterministically instead of during TLS teardown at process exit.
        // See `panels::zim::shutdown`.
        crate::panels::zim::shutdown();
        // Likewise drop the canvas node-content panes (read-only editor + HTML
        // view state) from their UI-thread-local store before TLS teardown.
        crate::panels::canvas::content::shutdown();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Mark a new puffin frame so the in-app viewer can slice the
        // timeline correctly. No-op when the `profiling` feature is
        // off. See `app/src/profiling.rs`.
        profiling::Profiler.new_frame();
        crate::profile_function!();
        // Always-on (env-gated) frame profiler: HIKER_FRAMELOG=1 prints per-frame
        // wall + section breakdown; HIKER_BENCH=1 also forces continuous repaint.
        crate::profiling::FrameProf::tick(ctx);
        // Whole-update CPU (everything below until this fn returns). `wall - this`
        // ≈ tessellation + GPU upload + draw + present, i.e. the non-CPU frame cost.
        let _update_total = crate::profiling::FrameProf::guard("update_total");

        // Bench helper: `HIKER_FOCUS=zim|editor|...` activates the first tab of
        // that kind once it exists (session restore is async), so a profiling run
        // measures a specific panel. No-op unless the env var is set.
        if !self.bench_focused {
            if let Some(want) = std::env::var_os("HIKER_FOCUS").and_then(|s| s.into_string().ok()) {
                let pick = self.state.session.tabs.iter().find_map(|t| {
                    let k = match &t.kind {
                        crate::tab::TabKind::ZimView { .. } => "zim",
                        crate::tab::TabKind::Editor { .. } => "editor",
                        crate::tab::TabKind::Canvas { .. } => "canvas",
                        crate::tab::TabKind::Graph => "graph",
                        _ => "",
                    };
                    (k.eq_ignore_ascii_case(&want)).then_some(t.id)
                });
                if let Some(id) = pick {
                    crate::state::activate_tab(&mut self.state, id);
                    self.bench_focused = true;
                } else if want.eq_ignore_ascii_case("zim") {
                    // No zim tab open — open one from HIKER_ZIM_PATH so a bench
                    // run can profile the ZIM viewer.
                    if let Some(zp) = std::env::var_os("HIKER_ZIM_PATH").and_then(|s| s.into_string().ok()) {
                        let id = crate::panels::zim::open(&mut self.state, &zp);
                        crate::state::activate_tab(&mut self.state, id);
                        self.bench_focused = true;
                    }
                }
            } else {
                self.bench_focused = true;
            }
        }

        // Enter the tokio runtime context for the whole frame. Several
        // crates we hand control to (e.g. `rfd`'s xdg-portal backend via
        // zbus) panic when they're called outside a Tokio reactor — this
        // guard makes any sync call that internally schedules onto Tokio
        // work safely from the egui thread.
        let _rt_guard = self.runtime.enter();

        // Vault switch: async state machine. `Requested` → spawn the
        // bootstrap on the runtime and transition to `InProgress`;
        // `InProgress` → poll a oneshot each frame for completion. The
        // UI keeps rendering against the OLD vault while the bootstrap
        // is in flight (DB opens + initial full scan can be slow) and
        // we never block the UI thread on `open_vault`.
        self.state.progress_vault_switch(&self.runtime, ctx);

        // Live-apply font slot changes from settings. No-op when nothing
        // changed (the common case) via a cheap fingerprint compare.
        // status: editor-three-fonts
        self.state.refresh_user_fonts(ctx);

        // Window keybindings (close tab, cycle tabs, jump to tab, nav).
        // Runs before this frame's renderers, so the swipe-nav handler
        // sees the *previous* frame's `swipe_skip_rects`. We clear them
        // immediately after, so this frame's renderers can refill them
        // for the next frame's keybinds read.
        {
            crate::profile_scope!("keybinds");
            let _g = crate::profiling::FrameProf::guard("keybinds");
            self.state.handle_keybinds(ctx);
        }
        self.state.session.nav.swipe_skip_rects.clear();

        {
            crate::profile_scope!("drains");
            let _g = crate::profiling::FrameProf::guard("drains");
            self.state.drain_fs_events();
            self.state.drain_indexer_events();
            self.state.drain_sync_events();
            self.state.drain_fork_diff_results();
            self.state.reconcile_sync();
            self.state.notify_sync_attention();
            self.state.drain_mutation_events();
        }

        {
            crate::profile_scope!("snapshots");
            let _g = crate::profiling::FrameProf::guard("snapshots");
            self.state.refresh_task_snapshot();
            self.state.refresh_pending_proposals();
            self.state.refresh_whole_file_proposals();
            self.state.refresh_skipped_paths();
            self.state.refresh_ui_context_snapshot();
            self.state.poll_cluster_llm_job();
        }

        // Tick autosave every ~5s — write dirty buffer sidecars so a
        // crash leaves at most that much typing on the floor.
        self.state.autosave_tick();

        // Indexer publishes status changes on a tokio watch channel —
        // poll cheaply each frame so the status bar shows the latest.
        // (Real implementation will register a wakeup on the channel and
        // request_repaint; for v0 we lean on egui's hover/keyboard-driven
        // repaint cadence.)
        ctx.request_repaint_after(std::time::Duration::from_millis(750));

        // Reader / focus mode (`view-reader-mode`): a workbench-level focus
        // mode that hides all chrome except the global top bar, so the
        // active tab fills the window. The workbench flag is the single
        // source of truth — it gates the side bars + activity bar + status
        // bar + panel area at render time (see `Workbench::ui`). Here we
        // only handle the top-bar chrome the host owns.
        let reader_view_active = self.state.workbench.reader_mode();
        // When reader mode also hides the top bar (`view-reader-hide-top-bar`,
        // the `ui.reader_hide_top_bar` setting): drop the titlebar / top
        // toolbar entirely. In frameless mode this removes the only window
        // controls, so resize handles are kept below and Ctrl+R is the exit.
        let hide_top_bar = reader_view_active && self.state.ui.reader_hide_top_bar;

        // Frameless mode (default): the merged custom titlebar claims the
        // top strip and hosts the centered command center; edge/corner
        // grips provide window resize (no OS border). When off, the
        // command center is inlined into the top toolbar instead (below).
        // In reader mode the command center is suppressed; when also hiding
        // the top bar, only the resize grips remain. [frameless-merged-titlebar]
        if self.state.ui.custom_titlebar {
            if !hide_top_bar {
                self.state.titlebar(ctx, !reader_view_active);
            }
            crate::titlebar::window_resize_handles(ctx);
        }

        // Toolbar: kept above the workbench for now. The plan is to
        // migrate its items into `HikerWbBehavior::status_bar_ui` /
        // activity-bar context menus, but that's a per-action port —
        // tracked separately. In reader mode the secondary toolbars are
        // skipped; the native top toolbar stays (the global top bar) unless
        // the top bar is also hidden.
        {
            crate::profile_scope!("toolbar");
            if self.state.ui.custom_titlebar {
                // Frameless: the first top toolbar + command center are
                // folded into the titlebar (rendered above). Only the
                // secondary (bottom/left/right) toolbars render here, and
                // they are skipped in reader mode.
                if !reader_view_active {
                    self.state.render_secondary_toolbars(ctx);
                }
            } else if !hide_top_bar {
                // Native chrome: inline the command center into the first
                // top toolbar (suppressed in reader mode); fall back to a
                // dedicated bar when there's no top toolbar to share.
                // [command-center-topbar]
                let placed = self.state.render_toolbars(ctx, !reader_view_active);
                if !placed && !reader_view_active {
                    self.state.command_center_bar(ctx);
                }
            }
        }

        // Central layout: egui_workbench owns the activity bar + side
        // bars + editor area + status bar in one render call.
        {
            crate::profile_scope!("workbench");
            let _g = crate::profiling::FrameProf::guard("workbench");
            // Reader mode optionally hides the tab strip (`view-reader-hide-tabs`):
            // a render-time gate on the workbench, shown by default.
            self.state
                .workbench
                .set_hide_tab_strip(reader_view_active && self.state.ui.reader_hide_tabs);
            self.state.sync_workbench_tabs();
            // `session.active_tab` is the single source of truth for which tab
            // is active. `sync_workbench_tabs` just pushed it into the strip
            // (state → workbench, one-way). The only thing flowing back is a
            // genuine user tab-click, which the workbench reports as a discrete
            // event drained below — NOT inferred by diffing the workbench's
            // active handle across the frame. That derived value also moves for
            // non-click reasons (tabs landing during async session restore, a
            // linked/driven group repointing), and treating those as clicks let
            // the strip fight `session.active_tab` — the tab-focus oscillation
            // this replaced.
            let mut behavior = workbench_host::HikerWbBehavior {
                app: &mut self.state,
                rt: &self.runtime,
            };
            // SAFETY-ish: `app.workbench` and the fields the behavior
            // reads from `app` (session, ui_cache, …) are disjoint
            // fields, but the borrow checker can't see that across the
            // function call without a `&mut` reborrow split. We split
            // it explicitly by taking the tree out, rendering, and
            // putting it back.
            let mut wb = std::mem::take(&mut behavior.app.workbench);
            wb.ui(ctx, &mut behavior);
            behavior.app.workbench = wb;
            // Consume the user's tab-strip click (if any) as the single
            // workbench → state channel: activate it and record nav history
            // (shared with the keyboard switch paths). Copy the id out first so
            // the `tab` borrow ends before the `&mut self.state` call.
            if let Some(handle) = self.state.workbench.take_user_activated()
                && let Some(tab) = self.state.workbench.editor_area.get(handle)
            {
                let tab_id = tab.id;
                crate::state::activate_tab(&mut self.state, tab_id);
            }
        }

        // Rich-row hover-expand previews: drawn AFTER the workbench (which renders
        // the sidebars), in non-interactable `Order::Tooltip` `Area`s so they never
        // occlude the row hitboxes that registered them
        // (`preview-hover-expand-side-anchor`). No-ops when no row is hovered. Both
        // are gated by `[ui].hover_previews_enabled` (the eye-menu toggle,
        // `preview-toggle`) — off suppresses the popups (any inline thumbnails
        // stay). Registration still runs cheaply; only the draw is gated.
        let hover_previews = self
            .state
            .vault_session
            .config
            .read()
            .map(|c| c.ui.hover_previews_enabled)
            .unwrap_or(true);
        if hover_previews {
            // Canvas live-paint / cluster-tree cached-image expand.
            crate::widgets::preview::render_expanded_preview(
                ctx,
                &self.state.vault_session.vault_root,
            );
            // Note rows: a read-only live-preview markdown render (with diagrams)
            // of the hovered note. Runs at the frame-loop level because rendering a
            // note needs `&mut AppState` (diagram cache, buffers, title resolver).
            crate::widgets::preview::render_note_preview(ctx, &mut self.state);
        }

        // Command palette overlay (Ctrl+K). Renders after panels so the
        // modal sits on top of the dock area.
        self.state.command_palette(ctx);

        // Modal + toast overlays render after panels so they layer on top.
        self.state.modal(ctx);
        self.state.toast_overlay(ctx);
        self.state.swipe_indicator_overlay(ctx);
        self.state.profiler_overlay(ctx);
        self.state.help_overlay(ctx);
    }
}

impl AppState {

/// Non-blocking help overlay listing window-level keybindings. Toggled
/// with F1 or `?`; the user can keep editing while it's open.
fn help_overlay(&mut self, ctx: &egui::Context) {
    let state = self;
    if !state.ui.show_help {
        return;
    }
    let mut open = true;
    egui::Window::new("Keybindings")
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(360.0)
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 48.0))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new("Window-level shortcuts").strong(),
            );
            ui.add_space(4.0);
            egui::Grid::new("help-keybinds")
                .num_columns(2)
                .spacing(egui::vec2(16.0, 4.0))
                .show(ui, |ui| {
                    for k in crate::keybinds::Keybinds.known_keybindings() {
                        ui.label(
                            egui::RichText::new(k.chord).monospace().small(),
                        );
                        ui.label(egui::RichText::new(k.label).small());
                        ui.end_row();
                    }
                });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Buffer-local chords (cursor motion, selection, undo) live inside the editor widget.",
                )
                .small()
                .italics()
                .color(hiker_theme::muted()),
            );
        });
    if !open {
        state.ui.show_help = false;
    }
}

/// Pull every queued `FileEvent` from the watcher relay and apply
/// app-level side effects: invalidate cached directory listings so the
/// sidebar shows new/deleted/renamed files; reload a buffer's
/// `loaded_hash` if the externally-modified path matches a clean buffer.
fn drain_fs_events(&mut self) {
    let state = self;
    use hiker_core::watcher::FileEvent;

    let events: Vec<FileEvent> = {
        let mut rx = state.vault_session.events.fs_events.lock().unwrap();
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    };

    if events.is_empty() {
        return;
    }

    for ev in &events {
        match ev {
            FileEvent::Created { path } | FileEvent::Modified { path } => {
                state.invalidate_for_path(path);
                state.maybe_reload_clean_buffer(path);
            }
            FileEvent::Deleted { path } => {
                state.invalidate_for_path(path);
                state.maybe_reload_clean_buffer(path);
            }
            FileEvent::Renamed { from, to } => {
                state.invalidate_for_path(from);
                state.invalidate_for_path(to);
                state.maybe_reload_clean_buffer(to);
            }
            FileEvent::Overflow => {
                state.file_tree_state.invalidate_all();
            }
        }
    }
}

fn invalidate_for_path(&mut self, path: &str) {
    let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    self.file_tree_state.invalidate_dir(parent);
}

/// Copy the latest task-queue snapshot out of the pollster's `watch`
/// channel. Cheap clone of an already-materialised `Vec<TaskRecord>`;
/// no tokio Mutex contention, no SQLite round-trip, no `block_on` on the
/// UI thread. See `bootstrap::spawn_snapshot_poller` for the producer.
fn refresh_task_snapshot(&mut self) {
    let state = self;
    let snap = state.vault_session.events.task_snapshot_rx.borrow().clone();
    state.ui_cache.task_snapshot = snap;
}

/// Copy the latest skipped-paths set out of the pollster's `watch`
/// channel. Pollster refreshes this every ~3s (the underlying query
/// requires the read-store mutex, which the indexer writer also wants).
/// UI thread never touches the mutex.
fn refresh_skipped_paths(&mut self) {
    let state = self;
    let snap = state.vault_session.events.skipped_paths_rx.borrow().clone();
    state.ui_cache.skipped_paths = snap;
}

/// Refresh the per-frame snapshot of the vault's pending agent ops from the
/// op log. Render-loop callers (toolbar badge, status-bar pending count, the
/// Patch-review tab badge, and the chat-card live-op-id set) read
/// `ui_cache.pending_snapshot` instead of each firing their own op-log walk.
/// The pending count is `op_writes::list_pending_proposals(...).len()`. A
/// failed walk leaves the prior snapshot in place so a transient I/O hiccup
/// doesn't blink the badge off.
fn refresh_pending_proposals(&mut self) {
    let state = self;
    let log = state.vault_session.services.oplog.clone();
    if let Ok(props) = hiker_core::ops::op_writes::list_pending_proposals(log.as_ref()) {
        state.ui_cache.pending_snapshot = props;
    }
}

/// Refresh the per-frame snapshot of pending whole-file (`write_note`-shaped)
/// proposals from the op log. The buffer review surface (version dropdown,
/// pending-rewrite banner, agent-diff toggle) reads
/// `ui_cache.whole_file_proposals` instead of each firing its own op-log walk.
/// A failed walk leaves the prior snapshot in place rather than clearing it,
/// so a transient I/O hiccup doesn't blink the review affordances off.
fn refresh_whole_file_proposals(&mut self) {
    let state = self;
    let log = state.vault_session.services.oplog.clone();
    if let Ok(props) = hiker_core::ops::op_writes::list_whole_file_proposals(log.as_ref()) {
        state.ui_cache.whole_file_proposals = props;
    }
}

/// Republish the per-frame snapshot of UI-context state for the MCP read
/// tools (`get_active_note`, `get_open_notes`, `get_selection`). MCP
/// handlers read this under a short RwLock instead of reaching into
/// `Session`. Mirrors the other `refresh_*` snapshot fns above.
///
/// status: mcp-tool-get-active-note
/// status: mcp-tool-get-open-notes
/// status: mcp-tool-get-selection
fn refresh_ui_context_snapshot(&mut self) {
    use crate::tab::TabKind;
    use hiker_mcp::ui_context::{ActiveBuffer, OpenBufferTab, Snapshot};
    let state = self;
    let active_id = state.session.active_tab;
    let mut snap = Snapshot::default();
    // Open-tabs list: only Editor tabs whose source has a vault-relative
    // path (Vault / Snapshot / PendingProposal / Trash all carry a
    // `path()`). Non-Editor kinds (Home/Settings/Queue/Board/Agent/etc.)
    // are omitted by the producer — the spec says "non-buffer kinds
    // omitted." Order follows `session.tabs`, which is the tab-strip's
    // visible order (see `sync_workbench_tabs`).
    for tab in &state.session.tabs {
        if let TabKind::Editor { buffer, .. } = &tab.kind {
            snap.open_tabs.push(OpenBufferTab {
                path: buffer.path().to_string(),
                active: Some(tab.id) == active_id,
            });
        }
    }
    // Active buffer: only when the focused tab is an Editor tab.
    if let Some(id) = active_id
        && let Some(tab) = state.session.tabs.iter().find(|t| t.id == id)
        && let TabKind::Editor { buffer, .. } = &tab.kind
    {
        let path = buffer.path().to_string();
        let mut ab = ActiveBuffer {
            path: path.clone(),
            cursor_byte: 0,
            selection: None,
        };
        // Look up the buffer's editor state. For non-vault sources the
        // buffer key isn't the bare path; we fall back to `None` for
        // those — the snapshot reports `cursor_byte: 0`, `selection: None`
        // rather than blocking the snapshot entirely.
        if let Some(buf) = state.session.buffers.get(&path) {
            let main = buf.editor.selection.main();
            ab.cursor_byte = main.head.offset();
            if !main.is_empty() {
                ab.selection = Some((main.start(), main.end()));
            }
        }
        snap.active_buffer = Some(ab);
    }
    // Best-effort write — a poisoned RwLock can't sensibly recover here,
    // and the prior snapshot stays in place.
    if let Ok(mut guard) = state.vault_session.services.mcp_ui_context.write() {
        *guard = snap;
    }
}

/// Drive the async vault-switch state machine. Called once per frame
/// from `update()`.
///
/// On `Requested(path)`: spawn `bootstrap::open_vault` on the tokio
/// runtime, store the oneshot receiver, transition to `InProgress`.
/// On `InProgress`: try_recv the oneshot. Done → cancel the OLD
/// session's spawned tasks, swap in the new state, repaint. NotYet →
/// keep rendering against the old vault. Errored/closed → toast,
/// return to `Idle`.
fn progress_vault_switch(
    &mut self,
    runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    ctx: &egui::Context,
) {
    let state = self;
    let current = std::mem::take(&mut state.vault_switch);
    match current {
        VaultSwitchState::Idle => {}
        VaultSwitchState::Picking(mut rx) => match rx.try_recv() {
            Ok(Some(path)) => state.request_vault_switch(path),
            // Dialog dismissed without a choice.
            Ok(None) => state.vault_switch = VaultSwitchState::Idle,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                // Dialog still open — keep polling without blocking the UI
                // thread, and force a repaint so we actually get polled
                // again even when the app is otherwise idle.
                state.vault_switch = VaultSwitchState::Picking(rx);
                ctx.request_repaint();
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                state.vault_switch = VaultSwitchState::Idle;
            }
        },
        VaultSwitchState::Requested(path) => {
            let (tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<AppState>>();
            let p = path.clone();
            runtime.spawn(async move {
                let res = bootstrap::open_vault(p).await;
                // If the receiver was dropped (concurrent switch superseded
                // us) the send fails — that's the cancellation contract.
                let _ = tx.send(res);
            });
            state.vault_switch = VaultSwitchState::InProgress { rx, path };
        }
        VaultSwitchState::InProgress { mut rx, path } => {
            match rx.try_recv() {
                Ok(Ok(new_state)) => {
                    // Cancel the OLD session's spawned tasks before the
                    // assignment replaces it so the watcher relay /
                    // indexer forwarder / direct LLM worker / pollster
                    // shut down cleanly.
                    state.vault_session.cancel.cancel();
                    *state = new_state;
                    ctx.request_repaint();
                }
                Ok(Err(err)) => {
                    state.push_toast(
                        format!("Failed to open {}: {}", path.display(), err),
                        crate::state::ToastLevel::Error,
                    );
                    state.vault_switch = VaultSwitchState::Idle;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                    // Bootstrap still running — keep rendering against
                    // the old vault.
                    state.vault_switch = VaultSwitchState::InProgress { rx, path };
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    state.push_toast(
                        format!("Vault switch to {} aborted", path.display()),
                        crate::state::ToastLevel::Warn,
                    );
                    state.vault_switch = VaultSwitchState::Idle;
                }
            }
        }
    }
}

/// Resolve a folder the user picked into the next switch state: ignore a
/// re-pick of the current vault, queue the switch outright when no buffer
/// is dirty, and otherwise raise a confirm modal so unsaved work isn't
/// discarded silently. (Lifted out of the toolbar action so it can run
/// when the async picker resolves rather than on the UI thread.)
fn request_vault_switch(&mut self, path: std::path::PathBuf) {
    let state = self;
    if path == state.vault_session.vault_root {
        state.vault_switch = VaultSwitchState::Idle;
        return;
    }
    let dirty: Vec<String> = state
        .session
        .buffers
        .iter()
        .filter(|(_, b)| b.is_dirty())
        .map(|(p, _)| p.clone())
        .collect();
    if dirty.is_empty() {
        state.push_toast(
            format!("Switching vault to {}", path.display()),
            crate::state::ToastLevel::Info,
        );
        state.vault_switch = VaultSwitchState::Requested(path);
        return;
    }
    let body = format!(
        "{} unsaved buffer{} will be discarded:\n  {}",
        dirty.len(),
        if dirty.len() == 1 { "" } else { "s" },
        dirty.join("\n  "),
    );
    state.session.modal = Some(crate::state::Modal::Confirm {
        title: "Switch vault?".into(),
        body,
        confirm_label: "Discard and switch".into(),
        cancel_label: "Cancel".into(),
        danger: true,
        intent: crate::state::ConfirmIntent::SwitchVault { path },
    });
    state.vault_switch = VaultSwitchState::Idle;
}

/// Pull queued indexer-progress lines into the bounded ring buffer used
/// by the Index tab.
fn drain_indexer_events(&mut self) {
    let state = self;
    let drained: Vec<String> = {
        let mut rx = state
            .vault_session
            .events
            .indexer_events_rx
            .lock()
            .unwrap();
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            out.push(line);
        }
        out
    };
    if drained.is_empty() {
        return;
    }
    // Indexing activity may have changed which notes carry the cluster-preset
    // frontmatter (a saved/imported/edited preset note); drop the cached preset
    // list so the Clusters `+` dropdown re-queries. status: cluster-preset
    state.clusters_state.preset_cache = None;
    for line in drained {
        state.vault_session.events.indexer_events.push_back(line);
    }
    while state.vault_session.events.indexer_events.len()
        > crate::state::INDEXER_EVENTS_MAX
    {
        state.vault_session.events.indexer_events.pop_front();
    }
}

/// Pull queued sync-progress lines into the bounded ring buffer used by the
/// Sync tab. Mirrors `drain_indexer_events`.
fn drain_sync_events(&mut self) {
    let state = self;
    let drained: Vec<String> = {
        let mut rx = state
            .vault_session
            .events
            .sync_events_rx
            .lock()
            .unwrap();
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            out.push(line);
        }
        out
    };
    if drained.is_empty() {
        return;
    }
    for line in drained {
        state.vault_session.events.sync_events.push_back(line);
    }
    while state.vault_session.events.sync_events.len()
        > crate::state::SYNC_EVENTS_MAX
    {
        state.vault_session.events.sync_events.pop_front();
    }
}

/// Drain on-demand fork-diff fetch results into the Sync page's "view diff"
/// cache (`panels.sync.fork_diffs`), keyed by the forked doc's path. Mirrors
/// `drain_sync_events`: a spawned `fetch_fork_diff` task feeds the channel,
/// this folds each `(path, Ok(text) | Err(msg))` into `Ready` / `Error` so the
/// render path reads the cache, never the node. [sync-fork-diff]
fn drain_fork_diff_results(&mut self) {
    let state = self;
    let drained: Vec<crate::sync_service::ForkDiffResult> = {
        let mut rx = state
            .vault_session
            .events
            .fork_diff_rx
            .lock()
            .unwrap();
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            out.push(item);
        }
        out
    };
    for (path, result) in drained {
        let entry = match result {
            Ok(text) => crate::panels::sync::ForkDiffState::Ready(text),
            Err(msg) => crate::panels::sync::ForkDiffState::Error(msg),
        };
        state.panels.sync.fork_diffs.insert(path, entry);
    }
}

/// Reconcile the live sync engine with `[sync].enabled` every frame — both
/// directions are live, no vault reopen. Turning the toggle ON builds + spawns
/// the engine immediately; turning it OFF cancels its responder loop, which
/// drops the swarm (closing the TCP listener and stopping mDNS). Cheap in
/// steady state (a config read + an `Option` check); it only does work on a
/// transition. Runs inside the frame's tokio runtime guard so the engine's
/// `tokio::spawn` has a reactor. The Settings toggle swaps the in-memory
/// config, so flipping `[sync]` there trips this on the next frame.
/// [sync-disable-kill-switch]
fn reconcile_sync(&mut self) {
    let state = self;
    let enabled = state
        .vault_session
        .config
        .read()
        .map(|c| c.sync.enabled)
        .unwrap_or(false);
    let running = state.vault_session.services.sync.is_some();
    match (enabled, running) {
        // Live disable: stop the running engine now.
        (false, true) => {
            if let Some(svc) = state.vault_session.services.sync.take() {
                svc.shutdown();
                state.push_toast(
                    "Sync disabled — engine stopped",
                    crate::state::ToastLevel::Info,
                );
            }
        }
        // Live enable: build + spawn without a reopen. A fresh progress channel
        // is swapped in — the boot-time one is closed when sync started disabled
        // — so the Sync page's log reads the live sender.
        (true, false) => {
            let section = match state.vault_session.config.read() {
                Ok(c) => c.sync.clone(),
                Err(_) => return,
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let (fork_tx, fork_rx) =
                tokio::sync::mpsc::unbounded_channel::<crate::sync_service::ForkDiffResult>();
            let oplog = state.vault_session.services.oplog.clone();
            let vault_root = state.vault_session.vault_root.clone();
            let cancel = state.vault_session.cancel.clone();
            let spawner = crate::bootstrap::Spawner { cancel };
            if let Some(svc) =
                spawner.spawn_sync_service(&vault_root, oplog, &section, tx, fork_tx)
            {
                state.vault_session.events.sync_events_rx = std::sync::Mutex::new(rx);
                state.vault_session.events.fork_diff_rx = std::sync::Mutex::new(fork_rx);
                state.vault_session.services.sync = Some(svc);
                state.push_toast("Sync enabled", crate::state::ToastLevel::Info);
            }
        }
        _ => {}
    }
}

/// Proactively surface sync attention beyond the Sync page: fire a toast the
/// FIRST time a doc newly blocks, a per-doc/per-peer error newly appears, or a
/// content-key change is newly held. Diffs the live engine snapshot against
/// `sync_attention_seen` so a steady-state round (the same items still
/// present) is silent — only a NEW item speaks, never every no-op round. The
/// Sync page + tab badge carry the standing state; this is the one-shot nudge
/// so the user knows without opening Sync. Cheap (a std-mutex snapshot read +
/// set diffs); no node lock. status: sync-attention-badge
fn notify_sync_attention(&mut self) {
    let state = self;
    let Some(service) = state.vault_session.services.sync.clone() else {
        // Sync off / torn down: forget what we'd seen so a re-enable starts
        // clean (and an old block doesn't re-toast on reconnect).
        state.sync_attention_seen = crate::state::SyncAttentionSeen::default();
        return;
    };
    let snap = service.state_snapshot();
    let seen = &mut state.sync_attention_seen;

    // Newly blocked docs (forks) — point the user at the Sync page to resolve.
    let mut new_blocks = Vec::new();
    for doc in &snap.blocked {
        if seen.blocked_paths.insert(doc.path.clone()) {
            new_blocks.push(doc.path.clone());
        }
    }
    // Drop entries that resolved so a future re-fork on the same path re-toasts.
    let live_blocked: std::collections::HashSet<String> =
        snap.blocked.iter().map(|d| d.path.clone()).collect();
    seen.blocked_paths.retain(|p| live_blocked.contains(p));

    // Newly errored items (per-doc/per-peer skips from the last round).
    let report_errored = snap
        .last_report
        .as_ref()
        .map(|r| r.errored.clone())
        .unwrap_or_default();
    let mut new_errors = Vec::new();
    for pair in &report_errored {
        if seen.errored.insert(pair.clone()) {
            new_errors.push(pair.clone());
        }
    }
    let live_errored: std::collections::HashSet<(String, String)> =
        report_errored.iter().cloned().collect();
    seen.errored.retain(|e| live_errored.contains(e));

    // A newly held content-key change (established key differs from a peer's).
    let new_key_change = match (&snap.pending_content_key_change, &seen.pending_key_peer) {
        (Some(peer), prev) if prev.as_deref() != Some(peer.as_str()) => Some(peer.clone()),
        _ => None,
    };
    seen.pending_key_peer = snap.pending_content_key_change.clone();

    // Fire at most one toast per kind per frame; the standing state lives on the
    // Sync page + tab badge.
    if !new_blocks.is_empty() {
        let msg = if new_blocks.len() == 1 {
            format!("Sync conflict: \"{}\" forked — open Sync to resolve", new_blocks[0])
        } else {
            format!("{} sync conflicts — open Sync to resolve", new_blocks.len())
        };
        state.push_toast(msg, crate::state::ToastLevel::Warn);
    }
    if !new_errors.is_empty() {
        let msg = if new_errors.len() == 1 {
            format!("Sync skipped an item: {} — {}", new_errors[0].0, new_errors[0].1)
        } else {
            format!("Sync skipped {} items — see Sync page", new_errors.len())
        };
        state.push_toast(msg, crate::state::ToastLevel::Error);
    }
    if let Some(peer) = new_key_change {
        state.push_toast(
            format!("Sync: peer {peer} uses a different content key — open Sync to confirm"),
            crate::state::ToastLevel::Warn,
        );
    }
}

/// Drain note-mutation outcomes posted by the wand-menu awaiter. Applied
/// outcomes replace the buffer body in-place (provided the source hash
/// still matches what we submitted with). Failed/Cancelled outcomes just
/// clear the `pending_mutations` gate and surface an error toast for the
/// Failed case.
fn drain_mutation_events(&mut self) {
    let state = self;
    use crate::state::MutationEvent;
    let drained: Vec<MutationEvent> = {
        let Ok(mut rx) = state.vault_session.events.mutation_events.lock() else {
            return;
        };
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    };
    for ev in drained {
        match ev {
            MutationEvent::Applied {
                source_path,
                mutation,
                content,
                source_hash_at_submit,
            } => {
                state.session.pending_mutations.remove(&source_path);
                let Some(buffer) = state.session.buffers.get_mut(&source_path) else {
                    state.push_toast(
                        format!(
                            "Mutation '{}' completed but {} is no longer open",
                            mutation, source_path
                        ),
                        crate::state::ToastLevel::Info,
                    );
                    continue;
                };
                if buffer.loaded_hash != source_hash_at_submit {
                    // The user edited the buffer since submitting; refuse
                    // to clobber. Surface the new content via toast so it's
                    // at least visible — the legacy UI showed a "review the
                    // result manually" notice in the same case.
                    state.push_toast(
                        format!(
                            "Mutation '{}' result discarded — {} was edited mid-flight",
                            mutation, source_path
                        ),
                        crate::state::ToastLevel::Error,
                    );
                    continue;
                }
                // Apply the mutation result in place so the user's scroll
                // position, cursor, folds, and view toggles survive — the
                // legacy CM6 UI did this as a transaction. Preserve the
                // existing `loaded_hash` so the buffer reads as dirty
                // against disk until the user saves.
                let preserved_loaded_hash = buffer.loaded_hash.clone();
                buffer.replace_text(content, preserved_loaded_hash);
                state.push_toast(
                    format!("Applied '{}' to {}", mutation, source_path),
                    crate::state::ToastLevel::Info,
                );
            }
            MutationEvent::Failed {
                source_path,
                mutation,
                error,
            } => {
                state.session.pending_mutations.remove(&source_path);
                state.push_toast(
                    format!("Mutation '{}' failed for {}: {}", mutation, source_path, error),
                    crate::state::ToastLevel::Error,
                );
            }
            MutationEvent::Cancelled { source_path } => {
                state.session.pending_mutations.remove(&source_path);
            }
        }
    }
}

fn autosave_tick(&mut self) {
    let state = self;
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    if state.session.last_autosave_tick.elapsed() < INTERVAL {
        return;
    }
    state.session.last_autosave_tick = std::time::Instant::now();

    let autosave = state.vault_session.services.autosave.clone();
    for (path, buffer) in &state.session.buffers {
        if !buffer.is_dirty() {
            let _ = autosave.clear(path);
            continue;
        }
        let text = buffer.current_text();
        let hash = buffer.current_hash();
        if let Err(err) = autosave.write(path, text.as_bytes(), &hash) {
            tracing::warn!(error = %err, path = %path, "autosave write failed");
        }
    }

    state.persist_tab_state(&autosave);

    // Primary side-panel accordion: writes only when the arrangement
    // changed since the last tick (no dirty flag — it mutates inside
    // egui_workbench). [feature-multi-region-sidebar]
    state.persist_side_panel();
}

fn persist_tab_state(&mut self, autosave: &std::sync::Arc<hiker_core::autosave::Autosave>) {
    let state = self;
    // Snapshot every currently-open Canvas tab's view state (camera pan/zoom +
    // per-card scroll/zoom) into the session map first, so the persisted
    // `canvas_views` below covers what's open now alongside what was captured on
    // earlier closes. status: canvas-view-state-persist
    let open_canvas_tabs: Vec<(crate::tab::TabId, String)> = state
        .session
        .tabs
        .iter()
        .filter_map(|t| match &t.kind {
            crate::tab::TabKind::Canvas { path } => Some((t.id, path.clone())),
            _ => None,
        })
        .collect();
    for (id, path) in open_canvas_tabs {
        crate::panels::canvas::capture_view(state, id, &path);
    }
    let mut open_paths = Vec::new();
    let mut open_tab_kinds = std::collections::HashMap::new();
    for tab in &state.session.tabs {
        // Buffer tabs use their vault-relative path as the key; singleton
        // page-kind tabs (Home, Queue, Settings, etc.) use a synthetic
        // `<kind>:` key so restore can recreate them without an
        // associated buffer. Tabs whose payload we don't round-trip
        // (TrashPreview, SnapshotPreview, etc.) return `None` from
        // `Tab::persist_key` and are skipped.
        let Some((key, kind_str)) = tab.persist_key() else { continue };
        open_paths.push(key.clone());
        open_tab_kinds.insert(key, kind_str);
    }
    // Persist the active/preview tab by their `persist_key` — the SAME key form
    // stored in `open_paths` — so restore matches them for every tab kind. Using
    // the raw `buffer_path` here only matched plain note tabs: a Canvas/Board key
    // is prefixed (`canvas:`/`board:`) and singleton page tabs (Home, Graph, …)
    // have no buffer path at all, so a non-note active tab was silently dropped
    // and restore fell back to the first tab.
    let active_path = state
        .session
        .active_tab
        .and_then(|id| state.tab_by_id(id))
        .and_then(crate::tab::Tab::persist_key)
        .map(|(key, _)| key);
    let preview_path = state
        .session
        .preview_tab
        .and_then(|id| state.tab_by_id(id))
        .and_then(crate::tab::Tab::persist_key)
        .map(|(key, _)| key);

    let snapshot = hiker_core::autosave::TabState {
        open_paths,
        active_path,
        preview_path,
        saved_at_ms: 0,
        open_tab_kinds,
        // status: canvas-view-state-persist
        canvas_views: state.session.canvas_views.clone(),
    };
    if let Err(err) = autosave.save_tab_state(snapshot) {
        tracing::warn!(error = %err, "tab-state snapshot failed");
    }
}

/// Reload a cached buffer's text from disk in place when it's clean (no
/// unsaved edits) and disk has advanced past the loaded bytes — preserving
/// scroll, cursor, and folds. Used by the watcher's external-edit path and by
/// the capture pane's Form → Markdown toggle (`capture-view-toggle`), where a
/// buffer cached in a prior Markdown session can lag the form's later direct
/// disk writes. A dirty buffer is left untouched so unsaved edits never vanish.
pub(crate) fn maybe_reload_clean_buffer(&mut self, path: &str) {
    let Some(buffer) = self.session.buffers.get_mut(path) else {
        return;
    };
    if buffer.is_dirty() {
        return;
    }
    if let Ok((contents, hash)) = self.vault_session.vault.read_file_with_hash(path) {
        if hash == buffer.loaded_hash {
            return;
        }
        // Reload the new on-disk content in place so the user's scroll
        // position, cursor, and folds survive an external edit. The new
        // hash is the on-disk hash, leaving the buffer clean.
        buffer.replace_text(contents, hash);
    }
}
}  // close impl AppState block
