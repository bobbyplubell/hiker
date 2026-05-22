//! hiker — egui-based desktop app.
//!
//! Entry point: parses `--vault <path>` or falls back to the configured
//! default vault, opens it, and launches the eframe loop.

mod actions;
mod buffer;
mod bootstrap;
mod chat;
mod completion_sources;
mod editor_pane;
mod icons;
mod keybinds;
mod layout;
mod palette;
mod panels;
mod panels_registry;
mod profiling;
mod sidebar;
mod state;
mod tab;
mod tabs;
mod theme;
mod titlebar;
mod toolbar;
mod widgets;
mod workbench_host;

#[cfg(test)]
mod smoke_tests;

use std::path::PathBuf;

use eframe::egui;

use crate::state::{AppState, VaultSwitchState};

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
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
        .unwrap_or(false)
        || std::fs::read(state.vault_session.vault_root.join(".hiker/ui.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("custom_titlebar").and_then(serde_json::Value::as_bool))
            .unwrap_or(false);
    state.ui.custom_titlebar = custom_titlebar;

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
            theme::Theme.install(&cc.egui_ctx);
            state.install_user_fonts(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);
            Ok(Box::new(HikerApp {
                state,
                runtime: app_runtime,
            }))
        }),
    )
}

// (Tracing init is inlined at the only call site in `main`.)

pub(crate) struct HikerApp {
    state: AppState,
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
}

impl AppState {
    /// Load user-configured font files (per `editor.font_*` settings)
    /// into egui's font registry, mapping them to the Proportional /
    /// Monospace families. Empty paths or unreadable files fall back to
    /// egui's bundled defaults. Best-effort; errors are logged, not
    /// surfaced.
    fn install_user_fonts(&self, ctx: &egui::Context) {
        let state = self;
    let cfg = match state.vault_session.config.read() {
        Ok(c) => c,
        Err(_) => return,
    };
    let e = &cfg.editor;
    if e.font_system.is_empty() && e.font_editor.is_empty() && e.font_code.is_empty() {
        return;
    }
    let mut defs = egui::FontDefinitions::default();
    let mut load = |label: &str, path: &str, family: egui::FontFamily| {
        if path.is_empty() {
            return;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                let name = format!("user-{label}");
                defs.font_data
                    .insert(name.clone(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
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
}

impl eframe::App for HikerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Mark a new puffin frame so the in-app viewer can slice the
        // timeline correctly. No-op when the `profiling` feature is
        // off. See `app/src/profiling.rs`.
        profiling::Profiler.new_frame();
        crate::profile_function!();

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

        // Window keybindings (close tab, cycle tabs, jump to tab, nav).
        // Runs before this frame's renderers, so the swipe-nav handler
        // sees the *previous* frame's `swipe_skip_rects`. We clear them
        // immediately after, so this frame's renderers can refill them
        // for the next frame's keybinds read.
        {
            crate::profile_scope!("keybinds");
            self.state.handle_keybinds(ctx);
        }
        self.state.session.nav.swipe_skip_rects.clear();

        {
            crate::profile_scope!("drains");
            self.state.drain_fs_events();
            self.state.drain_indexer_events();
            self.state.drain_mutation_events();
        }

        {
            crate::profile_scope!("snapshots");
            self.state.refresh_task_snapshot();
            self.state.refresh_staging_snapshot();
            self.state.refresh_skipped_paths();
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

        // Custom titlebar (opt-in). Must render before everything else so
        // it claims the top strip.
        if self.state.ui.custom_titlebar {
            self.state.titlebar(ctx);
        }

        // Toolbar: kept above the workbench for now. The plan is to
        // migrate its items into `HikerWbBehavior::status_bar_ui` /
        // activity-bar context menus, but that's a per-action port —
        // tracked separately.
        {
            crate::profile_scope!("toolbar");
            self.state.render_toolbars(ctx);
        }

        // Central layout: egui_workbench owns the activity bar + side
        // bars + editor area + status bar in one render call.
        {
            crate::profile_scope!("workbench");
            self.state.sync_workbench_tabs();
            // Snapshot the workbench's active tab AFTER `sync_tabs`
            // pushed `session.active_tab` into the strip. Comparing
            // against the post-render snapshot lets us distinguish two
            // cases:
            //   - User clicked a tab in the workbench strip — the
            //     workbench's active changes, session.active_tab is
            //     stale. We push the new active back into session +
            //     nav history below.
            //   - User clicked a file in the sidebar — `open_file`
            //     mutated session.active_tab during render, but the
            //     workbench's active didn't change this frame. Next
            //     frame's `sync_tabs` calls `workbench.set_active` to
            //     pull the strip over.
            let prev_active_handle = self.state.workbench.active_handle();
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
            let new_active_handle = wb.active_handle();
            behavior.app.workbench = wb;
            if prev_active_handle != new_active_handle
                && let Some(handle) = new_active_handle
                && let Some(tab) = self.state.workbench.editor_area.get(handle)
            {
                let tab_id = tab.id;
                let prev_path = self
                    .state
                    .session
                    .active_tab
                    .and_then(|id| self.state.tab_by_id(id))
                    .and_then(|t| t.buffer_path())
                    .map(std::string::ToString::to_string);
                let next_path = self
                    .state
                    .tab_by_id(tab_id)
                    .and_then(|t| t.buffer_path())
                    .map(std::string::ToString::to_string);
                self.state.session.active_tab = Some(tab_id);
                if !self.state.session.nav.locked
                    && let Some(p) = next_path.as_deref()
                    && prev_path.as_deref() != Some(p)
                {
                    crate::state::nav_push(&mut self.state, p);
                }
            }
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
                    for (chord, desc) in crate::keybinds::Keybinds.known_keybindings() {
                        ui.label(
                            egui::RichText::new(*chord).monospace().small(),
                        );
                        ui.label(egui::RichText::new(*desc).small());
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
                .color(crate::theme::muted()),
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
            FileEvent::Created { path }
            | FileEvent::Modified { path }
            | FileEvent::Deleted { path } => {
                state.invalidate_for_path(path);
                state.maybe_reload_clean_buffer(path);
            }
            FileEvent::Renamed { from, to } => {
                state.invalidate_for_path(from);
                state.invalidate_for_path(to);
                state.maybe_reload_clean_buffer(to);
            }
            FileEvent::Overflow => {
                state.session.sidebar.dir_cache.clear();
            }
        }
    }
}

fn invalidate_for_path(&mut self, path: &str) {
    let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    self.session.sidebar.dir_cache.remove(parent);
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

/// Copy the latest staging snapshot out of the pollster's `watch`
/// channel. Render-loop callers (toolbar badge, buffer banner,
/// agent-diff toggle, status bar) read `ui_cache.staging_snapshot`
/// instead of each firing their own `Staging::list_pending` SQLite
/// query.
fn refresh_staging_snapshot(&mut self) {
    let state = self;
    let snap = state.vault_session.events.staging_snapshot_rx.borrow().clone();
    state.ui_cache.staging_snapshot = snap;
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
    for line in drained {
        state.vault_session.events.indexer_events.push_back(line);
    }
    while state.vault_session.events.indexer_events.len()
        > crate::state::INDEXER_EVENTS_MAX
    {
        state.vault_session.events.indexer_events.pop_front();
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

    if let Err(err) = bootstrap::save_trails(
        &state.vault_session.vault_root,
        &state.session.trails,
    ) {
        tracing::debug!(error = %err, "trails persist failed");
    }

    if state.session.dock_dirty {
        let bundle = layout::DockBundle {
            tree: state.session.dock.clone(),
            center_tile: state.session.center_tile,
            left_tile: state.session.left_tile,
            right_tile: state.session.right_tile,
        };
        if let Err(err) = bundle.save_for_vault(&state.vault_session.vault_root) {
            tracing::debug!(error = %err, "layout persist failed");
        } else {
            state.session.dock_dirty = false;
        }
    }
}

fn persist_tab_state(&self, autosave: &std::sync::Arc<hiker_core::autosave::Autosave>) {
    let state = self;
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
    let active_path = state
        .session
        .active_tab
        .and_then(|id| state.tab_by_id(id))
        .and_then(|t| t.buffer_path().map(str::to_string));
    let preview_path = state
        .session
        .preview_tab
        .and_then(|id| state.tab_by_id(id))
        .and_then(|t| t.buffer_path().map(str::to_string));

    let snapshot = hiker_core::autosave::TabState {
        open_paths,
        active_path,
        preview_path,
        saved_at_ms: 0,
        open_tab_kinds,
    };
    if let Err(err) = autosave.save_tab_state(snapshot) {
        tracing::warn!(error = %err, "tab-state snapshot failed");
    }
}

fn maybe_reload_clean_buffer(&mut self, path: &str) {
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
