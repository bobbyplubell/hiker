//! Settings tab. Scope-aware (User / Vault) form over `core::config::Config`.
//!
//! Each section is a collapsing group. Common knobs (vault tree, indexing
//! model, llm provider, mcp) render typed widgets and persist
//! through `Config::set`. Less-used sections fall back to a raw-TOML view of
//! the per-scope file so users can still hand-edit without leaving the tab.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use eframe::egui;

use hiker_core::config::{Config, SettingsScope};
use hiker_core::config::vcs::{GitMode, SyncTransport};
use hiker_core::config::sections::{RecencyBias, SyncMode, TreeSortBy, WorkerPreferenceCfg};

use crate::state::{AppState, ToastLevel};
use hiker_theme as theme;

mod raw;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
enum Scope {
    User,
    #[default]
    Vault,
}

impl Scope {
    const fn to_core(self) -> SettingsScope {
        match self {
            Scope::User => SettingsScope::User,
            Scope::Vault => SettingsScope::Vault,
        }
    }
}

/// Transient UI state held in `egui::Memory` so it survives across frames
/// without leaking into `AppState`.
#[derive(Clone, Default)]
struct SettingsUi {
    scope: Scope,
    /// Buffered text edits keyed by `(scope, dotted_key)`. We commit on
    /// focus-loss / Enter so DragValue-free strings don't fire a disk
    /// write on every keystroke.
    text_drafts: std::collections::HashMap<(Scope, String), String>,
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Settings");
    ui.add_space(4.0);

    // Pull the persistent UI state for this tab out of egui memory.
    let mem_id = egui::Id::new("settings_tab_ui");
    let mut ui_state: SettingsUi = ui
        .ctx()
        .data_mut(|d| d.get_temp::<SettingsUi>(mem_id).unwrap_or_default());

    // Scope toggle + scope-file affordances (Open / Reveal / Refresh).
    // Mirrors `settings-pane-open-toml-link` and
    // `settings-pane-manual-refresh` from the legacy pane.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Scope").color(theme::muted()).small());
        ui.selectable_value(&mut ui_state.scope, Scope::User, "User");
        ui.selectable_value(&mut ui_state.scope, Scope::Vault, "Vault");
        // Reset-scope action (`settings-pane-reset-row` adapted to whole
        // scope): truncates the per-scope TOML to empty so every value
        // falls back to compile-time defaults. Confirms first because the
        // change is irreversible from the UI.
        if ui
            .small_button("Reset to defaults")
            .on_hover_text("Empty this scope's TOML — every setting falls back to defaults")
            .clicked()
        {
            let scope_path = scope_path_pathbuf(app, ui_state.scope);
            let scope_label = match ui_state.scope {
                Scope::User => "user",
                Scope::Vault => "vault",
            };
            if let Some(p) = scope_path {
                app.session.modal = Some(crate::state::Modal::Confirm {
                    title: format!("Reset {} settings?", scope_label),
                    body: format!(
                        "Truncates the {} TOML to empty. Every setting will fall back to defaults.",
                        scope_label
                    ),
                    confirm_label: "Reset".into(),
                    cancel_label: "Cancel".into(),
                    danger: true,
                    intent: crate::state::ConfirmIntent::ResetScope { scope_path: p },
                });
            }
        }
        if ui.small_button("Refresh").on_hover_text("Reload config from disk").clicked() {
            match Config::load(&app.vault_session.vault_root) {
                Ok(fresh) => {
                    if let Ok(mut g) = app.vault_session.config.write() {
                        *g = fresh;
                    }
                    app.push_toast("Config reloaded from disk", ToastLevel::Info);
                }
                Err(err) => {
                    app.push_toast(format!("Refresh failed: {err}"), ToastLevel::Error);
                }
            }
        }
        let scope_path = scope_path_pathbuf(app, ui_state.scope);
        if let Some(p) = scope_path.as_ref() {
            if ui.small_button("Open").on_hover_text("Open the TOML file in the system editor").clicked() {
                launch_external(p);
            }
            if ui.small_button("Reveal").on_hover_text("Reveal the TOML file in the file manager").clicked() {
                let parent = p.parent().unwrap_or(p.as_path());
                launch_external(parent);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(app.scope_path_hint(ui_state.scope))
                    .color(theme::muted())
                    .small()
                    .monospace(),
            );
        });
    });
    ui.separator();

    // Pull a snapshot of the file-only config for the active scope. The
    // merged in-memory `Config` is what the runtime uses; the per-file view
    // is what the UI edits, mirroring the old TS pane's behavior.
    let snapshot = match Config::read_file_only(ui_state.scope.to_core(), &app.vault_session.vault_root) {
        Ok(c) => c,
        Err(e) => {
            ui.colored_label(egui::Color32::DARK_RED, format!("Load failed: {e}"));
            ui.ctx().data_mut(|d| d.insert_temp(mem_id, ui_state));
            return;
        }
    };

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            {
                let mut ctx = SettingsCtx {
                    ui: &mut *ui,
                    app: &mut *app,
                    snap: &snapshot,
                    st: &mut ui_state,
                };
                ctx.window_section();
                ctx.vault_section();
                ctx.indexing_section();
                ctx.llm_section();
                ctx.mcp_section();
                ctx.editor_section();
                ctx.render_section();
                ctx.search_section();
                ctx.tasks_section();
                ctx.acp_section();
                ctx.trails_section();
                ctx.sync_section();
                ctx.git_section();
                ctx.suggestions_section();
            }

            ui.add_space(8.0);
            raw::Raw { ui, app }.show(ui_state.scope.to_core());
        });

    ui.ctx().data_mut(|d| d.insert_temp(mem_id, ui_state));
}

impl AppState {
    fn scope_path_hint(&self, scope: Scope) -> String {
        let app = self;
    let paths = hiker_core::config::Paths::resolve(&app.vault_session.vault_root);
    match scope {
        Scope::User => paths
            .user
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no platform config dir)".to_string()),
        Scope::Vault => paths.vault.display().to_string(),
    }
    }
}

/// Cross-platform "open this path in the OS default handler". Used by the
/// scope-file Open / Reveal buttons. Best-effort — failures land as toasts
/// upstream; here we just spawn and forget.
fn launch_external(p: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

fn scope_path_pathbuf(app: &AppState, scope: Scope) -> Option<PathBuf> {
    let paths = hiker_core::config::Paths::resolve(&app.vault_session.vault_root);
    match scope {
        Scope::User => paths.user,
        Scope::Vault => Some(paths.vault),
    }
}

// ---------------------------------------------------------------------------
// Section renderers.
// ---------------------------------------------------------------------------

/// Bundles the four args every section helper shares so they can all
/// hang off `&mut self` and skirt `clippy::single_call_fn`.
struct SettingsCtx<'a> {
    ui: &'a mut egui::Ui,
    app: &'a mut AppState,
    snap: &'a Config,
    st: &'a mut SettingsUi,
}

impl<'a> SettingsCtx<'a> {
fn window_section(&mut self) {
    let (ui, app) = (&mut *self.ui, &mut *self.app);
    egui::CollapsingHeader::new("Window")
        .default_open(false)
        .show(ui, |ui| {
            let mut v = app.ui.custom_titlebar;
            if ui
                .checkbox(&mut v, "Custom titlebar (restart required)")
                .changed()
            {
                app.ui.custom_titlebar = v;
                commit(app, Scope::Vault, "ui.custom_titlebar", &serde_json::json!(v));
                app.push_toast(
                    "Custom titlebar setting saved (restart to apply)",
                    ToastLevel::Info,
                );
            }
            // Reader-mode chrome toggles. Unlike the custom titlebar these
            // take effect on the next frame — no restart needed.
            let mut hide = app.ui.reader_hide_top_bar;
            if ui
                .checkbox(&mut hide, "Hide top bar in reader mode")
                .changed()
            {
                app.ui.reader_hide_top_bar = hide;
                commit(app, Scope::Vault, "ui.reader_hide_top_bar", &serde_json::json!(hide));
            }
            let mut hide_tabs = app.ui.reader_hide_tabs;
            if ui.checkbox(&mut hide_tabs, "Hide tabs in reader mode").changed() {
                app.ui.reader_hide_tabs = hide_tabs;
                commit(app, Scope::Vault, "ui.reader_hide_tabs", &serde_json::json!(hide_tabs));
            }
            let mut hide_toolbar = app.ui.reader_hide_toolbar;
            if ui.checkbox(&mut hide_toolbar, "Hide toolbar in reader mode").changed() {
                app.ui.reader_hide_toolbar = hide_toolbar;
                commit(app, Scope::Vault, "ui.reader_hide_toolbar", &serde_json::json!(hide_toolbar));
            }
            ui.separator();
            // Input / navigation. Read live config (no `app.ui` mirror — the
            // canvas widget + swipe handler read `[ui]` config directly).
            // Canvas scroll behavior: Auto (mouse zooms, touchpad pans) / Pan /
            // Zoom. Shares the canvas gear menu's selector. [canvas-scroll-mode]
            ui.horizontal(|ui| {
                ui.label("Canvas scroll:");
                crate::panels::canvas::render::canvas_scroll_mode_selector(ui, app);
            });
            let mut swipe = app
                .vault_session
                .config
                .read()
                .map(|c| c.ui.swipe_nav_enabled)
                .unwrap_or(true);
            if ui
                .checkbox(&mut swipe, "Two-finger horizontal swipe navigates Back/Forward")
                .changed()
            {
                commit(app, Scope::Vault, "ui.swipe_nav_enabled", &serde_json::json!(swipe));
            }
        });
}

fn vault_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("Vault")
        .default_open(matches!(st.scope, Scope::Vault))
        .show(ui, |ui| {
            enum_combo(
                ui, app, st,
                "Tree sort by",
                "vault.tree.sort_by",
                snap.vault.tree.sort_by,
                &[
                    (TreeSortBy::NameAsc, "Name (A-Z)", TreeSortBy::NameAsc.as_str()),
                    (TreeSortBy::NameDesc, "Name (Z-A)", TreeSortBy::NameDesc.as_str()),
                    (TreeSortBy::MtimeDesc, "Modified (newest)", TreeSortBy::MtimeDesc.as_str()),
                    (TreeSortBy::MtimeAsc, "Modified (oldest)", TreeSortBy::MtimeAsc.as_str()),
                ],
            );
            bool_row(ui, app, st, "Sidebar open at startup", "vault.sidebar_open", snap.vault.sidebar_open);
            bool_row(ui, app, st, "Related panel open at startup", "vault.related_open", snap.vault.related_open);
        });
}

fn indexing_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("Indexing")
        .default_open(true)
        .show(ui, |ui| {
            // Embedder model — picks from the supported set.
            let models = hiker_core::embed::supported_model_ids();
            let mut current = snap.indexing.model.clone();
            ui.horizontal(|ui| {
                ui.label("Embedder model");
                let resp = egui::ComboBox::from_id_salt("indexing.model")
                    .selected_text(&current)
                    .show_ui(ui, |ui| {
                        let mut changed = false;
                        for m in &models {
                            if ui
                                .selectable_value(&mut current, m.to_string(), *m)
                                .changed()
                            {
                                changed = true;
                            }
                        }
                        changed
                    });
                if resp.inner.unwrap_or(false) {
                    // Route through a confirm modal — model changes
                    // re-embed every note (and a dim change forces a
                    // full schema migration of `chunk_vecs`). The user
                    // should *know* before they trigger that.
                    let chosen = current.clone();
                    let previous = snap.indexing.model.clone();
                    let total = app.vault_session.services.indexer.status().total_notes;
                    let scope = st.scope;
                    app.session.modal = Some(crate::state::Modal::Confirm {
                        title: "Change embedder model".to_string(),
                        body: format!(
                            "Switching to `{chosen}` re-embeds all {total} indexed notes and may take several minutes. If the new model's vector dimension differs from `{previous}` the existing index will be reset before the rebuild.",
                        ),
                        confirm_label: "Re-embed".to_string(),
                        cancel_label: "Cancel".to_string(),
                        danger: false,
                        intent: crate::state::ConfirmIntent::ReloadEmbedder {
                            scope: scope.to_core(),
                            model_id: chosen,
                        },
                    });
                }
            });
            help(ui, "Changing the model re-embeds the entire vault on next index pass.");

            // Note-ID-stamping setting retired with `note-id-stamping` —
            // under path-as-identity (`store-path-is-identity`), notes are
            // addressed by their vault path and the op-log keeps an
            // internal `path → doc_id` map. There's nothing to stamp.
        });
}

fn llm_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("LLM")
        .default_open(true)
        .show(ui, |ui| {
            bool_row(ui, app, st, "Enabled", "llm.enabled", snap.llm.enabled);
            string_row(ui, app, st, "Provider", "llm.provider.backend", &snap.llm.provider.backend);
            help(ui, "anthropic | openai | ollama | google | …");
            string_row(ui, app, st, "Model", "llm.provider.model", &snap.llm.provider.model);
            string_row(ui, app, st, "API key env var", "llm.provider.api_key_env", &snap.llm.provider.api_key_env);
            if matches!(st.scope, Scope::User) {
                string_row(ui, app, st, "API key (literal, user-only)", "llm.provider.api_key", &snap.llm.provider.api_key);
                help(ui, "Stored plain-text in the platform config dir. Prefer the env var route when possible.");
            }
            string_row(ui, app, st, "Base URL", "llm.provider.base_url", &snap.llm.provider.base_url);

            ui.add_space(4.0);
            ui.label(egui::RichText::new("Limits").color(theme::muted()).small());
            int_row(ui, app, st, "Max tokens", "llm.limits.max_tokens", &IntField { current: snap.llm.limits.max_tokens as u64, min: 1, max: u32::MAX as u64 });
            int_row(ui, app, st, "Timeout (s)", "llm.limits.timeout_secs", &IntField { current: snap.llm.limits.timeout_secs, min: 1, max: u32::MAX as u64 });

            ui.add_space(4.0);
            ui.label(egui::RichText::new("Agent").color(theme::muted()).small());
            int_row(ui, app, st, "Iteration cap", "llm.agent.iteration_cap", &IntField { current: snap.llm.agent.iteration_cap as u64, min: 1, max: u32::MAX as u64 });
            int_row(ui, app, st, "Tool timeout (s)", "llm.agent.tool_timeout_secs", &IntField { current: snap.llm.agent.tool_timeout_secs, min: 1, max: u32::MAX as u64 });

            bool_row(ui, app, st, "Audit: log full prompt", "llm.audit.log_full_prompt", snap.llm.audit.log_full_prompt);
            bool_row(ui, app, st, "Background writes need review", "llm.background.review_required", snap.llm.background.review_required);
        });
}

fn mcp_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("MCP server")
        .default_open(true)
        .show(ui, |ui| {
            bool_row(ui, app, st, "Enabled", "mcp.enabled", snap.mcp.enabled);
            string_row(ui, app, st, "Bind host", "mcp.host", &snap.mcp.host);
            if snap.mcp.host != "127.0.0.1" && snap.mcp.host != "localhost" {
                ui.horizontal(|ui| {
                    ui.add(crate::icons::ICONS.image(crate::icons::Icon::Warning).tint(egui::Color32::from_rgb(0xb0, 0x4a, 0x00)));
                    ui.colored_label(
                        egui::Color32::from_rgb(0xb0, 0x4a, 0x00),
                        "non-loopback host exposes vault contents on the network",
                    );
                });
            }
            SettingsCtx { ui: &mut *ui, app: &mut *app, snap, st: &mut *st }
                .port_row("Port (0 = ephemeral)", "mcp.port", snap.mcp.port);
            int_row(ui, app, st, "Max top_k", "mcp.max_top_k", &IntField { current: snap.mcp.max_top_k as u64, min: 1, max: u32::MAX as u64 });

            ui.add_space(4.0);
            ui.label(egui::RichText::new("Tools").color(theme::muted()).small());
            bool_row(ui, app, st, "Writes enabled (master)", "mcp.tools.writes_enabled", snap.mcp.tools.writes_enabled);
            bool_row(ui, app, st, "Writes need review", "mcp.tools.review_required", snap.mcp.tools.review_required);
            bool_row(ui, app, st, "Allow redacted lookup", "mcp.tools.allow_redacted_lookup", snap.mcp.tools.allow_redacted_lookup);

            ui.collapsing("Per-tool toggles", |ui| {
                let t = &snap.mcp.tools;
                bool_row(ui, app, st, "search_notes", "mcp.tools.search_notes_enabled", t.search_notes_enabled);
                bool_row(ui, app, st, "get_note", "mcp.tools.get_note_enabled", t.get_note_enabled);
                bool_row(ui, app, st, "related_notes", "mcp.tools.related_notes_enabled", t.related_notes_enabled);
                bool_row(ui, app, st, "write_note", "mcp.tools.write_note_enabled", t.write_note_enabled);
                bool_row(ui, app, st, "edit_note", "mcp.tools.edit_note_enabled", t.edit_note_enabled);
                bool_row(ui, app, st, "set_frontmatter", "mcp.tools.set_frontmatter_enabled", t.set_frontmatter_enabled);
                bool_row(ui, app, st, "apply_tag", "mcp.tools.apply_tag_enabled", t.apply_tag_enabled);
                bool_row(ui, app, st, "remove_tag", "mcp.tools.remove_tag_enabled", t.remove_tag_enabled);
                bool_row(ui, app, st, "task_checkout", "mcp.tools.task_checkout_enabled", t.task_checkout_enabled);
                bool_row(ui, app, st, "task_submit", "mcp.tools.task_submit_enabled", t.task_submit_enabled);
                bool_row(ui, app, st, "task_fail", "mcp.tools.task_fail_enabled", t.task_fail_enabled);
                bool_row(ui, app, st, "task_heartbeat", "mcp.tools.task_heartbeat_enabled", t.task_heartbeat_enabled);
                bool_row(ui, app, st, "task_list", "mcp.tools.task_list_enabled", t.task_list_enabled);
            });

            bool_row(ui, app, st, "Audit: log full input", "mcp.audit.log_full_input", snap.mcp.audit.log_full_input);
        });
}

fn editor_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        // Editor flags are vault-scope only. Show a hint instead of the form.
        egui::CollapsingHeader::new("Editor")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Editor preferences live in vault scope.")
                        .color(theme::muted())
                        .small(),
                );
            });
        return;
    }
    egui::CollapsingHeader::new("Editor")
        .default_open(false)
        .show(ui, |ui| {
            let e = &snap.editor;
            bool_row(ui, app, st, "Render .txt as markdown", "editor.render_txt_as_markdown", e.render_txt_as_markdown);
            bool_row(ui, app, st, "Live preview", "editor.live_preview", e.live_preview);
            bool_row(ui, app, st, "Word wrap", "editor.word_wrap", e.word_wrap);
            bool_row(ui, app, st, "Show line numbers", "editor.show_line_numbers", e.show_line_numbers);
            bool_row(ui, app, st, "Show whitespace", "editor.show_whitespace", e.show_whitespace);
            bool_row(ui, app, st, "Highlight trailing whitespace", "editor.highlight_trailing_whitespace", e.highlight_trailing_whitespace);
            bool_row(ui, app, st, "Show chunk boundaries", "editor.show_chunk_boundaries", e.show_chunk_boundaries);
            bool_row(ui, app, st, "Hide frontmatter", "editor.hide_frontmatter", e.hide_frontmatter);
            bool_row(ui, app, st, "Intraline diff", "editor.intraline_diff", e.intraline_diff);
            bool_row(ui, app, st, "Show minimap", "editor.show_minimap", e.show_minimap);
            bool_row(
                ui,
                app,
                st,
                "Hide editor scrollbar (when minimap is off)",
                "editor.hide_scrollbar",
                e.hide_scrollbar,
            );
            SettingsCtx { ui: &mut *ui, app: &mut *app, snap, st: &mut *st }.float_row(
                "Scroll speed",
                "editor.scroll_speed",
                e.scroll_speed as f64,
                0.25,
                10.0,
                0.05,
            );
            ui.collapsing("Minimap customization", |ui| {
                use hiker_core::config::sections::MinimapStyle;
                let m = &e.minimap;
                enum_combo(ui, app, st, "Style", "editor.minimap.style", m.style, &[
                    (MinimapStyle::Glyphs, "Glyphs (scaled text)", "glyphs"),
                    (MinimapStyle::Bars, "Bars (structural)", "bars"),
                ]);
                help(ui, "Glyphs renders a literal mini-version of the text; bars is the structural overview. Bar-* options below apply to the bars style.");
                ui.label(egui::RichText::new("Layout").color(theme::muted()).small());
                int_row(ui, app, st, "Width (px)", "editor.minimap.width", &IntField { current: m.width as u64, min: 16, max: 300 });
                int_row(ui, app, st, "Bar padding left (px)", "editor.minimap.bar_padding_left", &IntField { current: m.bar_padding_left as u64, min: 0, max: 24 });
                int_row(ui, app, st, "Bar padding right (px)", "editor.minimap.bar_padding_right", &IntField { current: m.bar_padding_right as u64, min: 0, max: 24 });
                int_row(ui, app, st, "Bar corner radius (px)", "editor.minimap.bar_corner_radius", &IntField { current: m.bar_corner_radius as u64, min: 0, max: 6 });
                int_row(ui, app, st, "Minimum bar width (px)", "editor.minimap.min_bar_width", &IntField { current: m.min_bar_width as u64, min: 1, max: 12 });
                int_row(ui, app, st, "Bar vertical gap (×0.1px)", "editor.minimap.bar_gap_tenths", &IntField { current: m.bar_gap_tenths as u64, min: 0, max: 20 });
                ui.separator();
                ui.label(egui::RichText::new("Toggles").color(theme::muted()).small());
                bool_row(ui, app, st, "Apply per-kind colors", "editor.minimap.colored", m.colored);
                bool_row(ui, app, st, "Heading section rules", "editor.minimap.show_section_rules", m.show_section_rules);
                bool_row(ui, app, st, "Viewport thumb", "editor.minimap.show_viewport", m.show_viewport);
                bool_row(ui, app, st, "Left edge rule", "editor.minimap.show_left_edge", m.show_left_edge);
                ui.separator();
                ui.label(egui::RichText::new("Colors (hex #RRGGBB or #RRGGBBAA)").color(theme::muted()).small());
                color_row(ui, app, st, "Heading", "editor.minimap.color_heading", &m.color_heading);
                color_row(ui, app, st, "Code", "editor.minimap.color_code", &m.color_code);
                color_row(ui, app, st, "Emphasis", "editor.minimap.color_emphasis", &m.color_emphasis);
                color_row(ui, app, st, "Quote", "editor.minimap.color_quote", &m.color_quote);
                color_row(ui, app, st, "Plain text", "editor.minimap.color_plain", &m.color_plain);
                color_row(ui, app, st, "Background", "editor.minimap.color_background", &m.color_background);
                color_row(ui, app, st, "Section rule / edge", "editor.minimap.color_section_rule", &m.color_section_rule);
                color_row(ui, app, st, "Viewport thumb", "editor.minimap.color_viewport", &m.color_viewport);
                color_row(ui, app, st, "Viewport thumb (hover)", "editor.minimap.color_viewport_hover", &m.color_viewport_hover);
            });
            ui.separator();
            ui.label(
                egui::RichText::new("Fonts (paths to .ttf / .otf — empty = default)")
                    .small()
                    .color(theme::muted()),
            );
            font_row(ui, app, st, "System font", "editor.font_system", &e.font_system);
            font_row(ui, app, st, "Editor font", "editor.font_editor", &e.font_editor);
            font_row(ui, app, st, "Code font", "editor.font_code", &e.font_code);
            help(ui, "Font changes apply live; unresolved paths fall back to the platform default.");
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("Preview")
                    .small()
                    .color(theme::muted()),
            );
            render_font_preview(ui);
            ui.separator();
            ui.label(
                egui::RichText::new("Click selection (regex matches the line under the cursor)")
                    .small()
                    .color(theme::muted()),
            );
            string_row(ui, app, st, "Double-click pattern", "editor.double_click_pattern", &e.double_click_pattern);
            string_row(ui, app, st, "Triple-click pattern", "editor.triple_click_pattern", &e.triple_click_pattern);
            help(ui, r#"Defaults: "\w+" (Unicode word, so foo-bar splits at "-") and ".*\n?" (whole line incl. newline). Try "[\w-]+" to select hyphenated words whole, or "\S+" for runs of non-whitespace. Empty resets to default; an invalid regex falls back to default. Takes effect on the next click in any open buffer."#);
        });
}

// status: settings-section-render
fn render_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        // `[render]` is vault-scope (the cache lives in the vault). Show a hint
        // instead of the form, mirroring the editor / sync sections.
        egui::CollapsingHeader::new("Rendering")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Rendering settings live in vault scope.")
                        .color(theme::muted())
                        .small(),
                );
            });
        return;
    }
    egui::CollapsingHeader::new("Rendering")
        .default_open(false)
        .show(ui, |ui| {
            // status: render-cache-diagrams-toggle
            bool_row(
                ui,
                app,
                st,
                "Cache rendered diagrams to disk",
                "render.cache_diagrams",
                snap.render.cache_diagrams,
            );
            help(
                ui,
                "Persists rasterized math / Mermaid / WaveDrom widgets under .hiker/diagram-cache so reopening a note skips re-rendering. Off uses in-memory caching only. Takes effect immediately.",
            );
        });
}

fn search_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        return;
    }
    egui::CollapsingHeader::new("Search")
        .default_open(false)
        .show(ui, |ui| {
            bool_row(ui, app, st, "Semantic mode", "search.modes.semantic", snap.search.modes.semantic);
            bool_row(ui, app, st, "Lexical mode", "search.modes.lexical", snap.search.modes.lexical);
            enum_combo(
                ui, app, st,
                "Recency bias",
                "search.semantic.recency_bias",
                snap.search.semantic.recency_bias,
                &[
                    (RecencyBias::Off, "Off", "off"),
                    (RecencyBias::Mild, "Mild", "mild"),
                    (RecencyBias::Strong, "Strong", "strong"),
                ],
            );
            // Semantic min_similarity is bounded 0.0..=0.95.
            let mut sim = snap.search.semantic.min_similarity as f64;
            ui.horizontal(|ui| {
                ui.label("Semantic min similarity");
                let resp = ui.add(
                    egui::Slider::new(&mut sim, 0.0..=0.95)
                        .step_by(0.05)
                        .fixed_decimals(2),
                );
                if resp.drag_stopped() || resp.lost_focus() {
                    commit(app, st.scope, "search.semantic.min_similarity", &json_f(sim));
                }
            });
            int_row(ui, app, st, "Semantic top_k", "search.semantic.top_k", &IntField { current: snap.search.semantic.top_k as u64, min: 5, max: 100 });
        });
}

fn tasks_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("Tasks")
        .default_open(false)
        .show(ui, |ui| {
            enum_combo(
                ui, app, st,
                "Worker preference",
                "tasks.worker_preference",
                snap.tasks.worker_preference,
                &[
                    (WorkerPreferenceCfg::Auto, "Auto", "auto"),
                    (WorkerPreferenceCfg::Internal, "Internal", "internal"),
                    (WorkerPreferenceCfg::External, "External", "external"),
                ],
            );
            if matches!(st.scope, Scope::Vault) {
                int_row(ui, app, st, "Terminal retention (s)", "tasks.terminal_retention_secs", &IntField { current: snap.tasks.terminal_retention_secs, min: 1, max: u32::MAX as u64 });
                bool_row(ui, app, st, "Direct worker enabled", "tasks.direct_worker.enabled", snap.tasks.direct_worker.enabled);
                int_row(ui, app, st, "Direct worker parallelism", "tasks.direct_worker.parallelism", &IntField { current: snap.tasks.direct_worker.parallelism as u64, min: 1, max: u32::MAX as u64 });
                bool_row(ui, app, st, "Expose to chat agent", "tasks.expose_to_chat_agent", snap.tasks.expose_to_chat_agent);
                int_row(ui, app, st, "Lease default (s)", "tasks.lease.default_secs", &IntField { current: snap.tasks.lease.default_secs, min: 1, max: u32::MAX as u64 });
                int_row(ui, app, st, "Lease max (s)", "tasks.lease.max_secs", &IntField { current: snap.tasks.lease.max_secs, min: 1, max: u32::MAX as u64 });
            }
        });
}

fn acp_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("ACP")
        .default_open(false)
        .show(ui, |ui| {
            string_row(ui, app, st, "Command", "acp.command", &snap.acp.command);
            help(ui, "Full command line (e.g. `auggie --acp`). Empty = built-in basic agent.");
        });
}

fn trails_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        return;
    }
    egui::CollapsingHeader::new("Trails")
        .default_open(false)
        .show(ui, |ui| {
            string_row(ui, app, st, "New trail directory", "trails.new_trail_dir", &snap.trails.new_trail_dir);
        });
}

fn sync_section(&mut self) {
    // status: sync-config-section
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        // [sync] is per-vault (the config lives in the vault TOML). Show a
        // hint instead of the form, mirroring the editor/search sections.
        egui::CollapsingHeader::new("Sync")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Sync settings live in vault scope.")
                        .color(theme::muted())
                        .small(),
                );
            });
        return;
    }
    egui::CollapsingHeader::new("Sync")
        .default_open(false)
        .show(ui, |ui| {
            let s = &snap.sync;
            bool_row(ui, app, st, "Enabled", "sync.enabled", s.enabled);
            // status: sync-transport-seam
            enum_combo(
                ui, app, st,
                "Transport",
                "sync.transport",
                s.transport,
                &[
                    (SyncTransport::Libp2p, "libp2p (P2P file blobs)", "libp2p"),
                    (SyncTransport::Git, "git (commit + push/pull)", "git"),
                    (SyncTransport::None, "None (local only)", "none"),
                ],
            );
            // libp2p-only rows. They still serialize for git/none, but the
            // active engine ignores them — keep them visible so a user
            // switching transports doesn't lose their values.
            enum_combo(
                ui, app, st,
                "Mode (libp2p)",
                "sync.mode",
                s.mode,
                &[
                    (SyncMode::Peer, "Peer (P2P/LAN)", "peer"),
                    (SyncMode::Server, "Server relay", "server"),
                    (SyncMode::Both, "Both", "both"),
                ],
            );
            string_row(ui, app, st, "Server URL (libp2p)", "sync.server_url", &s.server_url);
            bool_row(ui, app, st, "Discovery (libp2p)", "sync.discovery", s.discovery);

            // Enrolled devices are read-only here — enrollment (the
            // fingerprint swap) happens on the Sync page, not in settings.
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("Enrolled devices ({})", s.devices.len()))
                    .color(theme::muted())
                    .small(),
            );
            if s.devices.is_empty() {
                help(ui, "No devices enrolled yet.");
            } else {
                for fp in &s.devices {
                    ui.label(egui::RichText::new(fp).monospace().small());
                }
            }
            help(ui, "Enroll devices on the Sync page; this list is read-only.");

            ui.add_space(4.0);
            help(
                ui,
                "Secrets (device key + per-vault content key) live in user scope and never travel with the synced vault.",
            );
        });
}

// status: git-config-section
fn git_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    if matches!(st.scope, Scope::User) {
        // [git] is per-vault, like [sync].
        egui::CollapsingHeader::new("Git transport")
            .default_open(false)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("Git transport settings live in vault scope.")
                        .color(theme::muted())
                        .small(),
                );
            });
        return;
    }
    egui::CollapsingHeader::new("Git transport")
        .default_open(false)
        .show(ui, |ui| {
            let g = &snap.git;
            help(
                ui,
                "Active only when Sync transport is set to git. The .md files are canonical; .hiker/ is gitignored.",
            );
            enum_combo(
                ui, app, st,
                "Mode",
                "git.mode",
                g.mode,
                &[
                    (GitMode::Integrated, "Integrated (hiker drives)", "integrated"),
                    (GitMode::Manual, "Manual (you drive git)", "manual"),
                ],
            );
            string_row(ui, app, st, "Remote (push/pull URL)", "git.remote", &g.remote);
            help(ui, "Empty remote = commit-only local versioning (no push/pull).");
            bool_row(ui, app, st, "Commit on save", "git.auto_commit", g.auto_commit);
            int_row(
                ui, app, st,
                "Commit debounce (ms)",
                "git.commit_debounce_ms",
                &IntField { current: u64::from(g.commit_debounce_ms), min: 0, max: u32::MAX as u64 },
            );
            int_row(
                ui, app, st,
                "git gc interval (days)",
                "git.gc_interval_days",
                &IntField { current: u64::from(g.gc_interval_days), min: 0, max: u32::MAX as u64 },
            );
            help(
                ui,
                "Credentials come from your git credential helper / SSH agent — never stored by hiker.",
            );
        });
}

fn suggestions_section(&mut self) {
    let (ui, app, snap, st) = (&mut *self.ui, &mut *self.app, self.snap, &mut *self.st);
    egui::CollapsingHeader::new("Suggestions / Triage")
        .default_open(false)
        .show(ui, |ui| {
            let t = &snap.suggestions.triage;
            bool_row(ui, app, st, "Review required", "suggestions.triage.review_required", t.review_required);
            string_row(ui, app, st, "Scope folder", "suggestions.triage.scope", &t.scope);
            string_row(ui, app, st, "Scheduled rerun", "suggestions.triage.scheduled_rerun", &t.scheduled_rerun);
            help(ui, "Duration grammar: 30m / 1h / 6h / 24h / 7d. Empty = disabled.");
            bool_row(ui, app, st, "Modified rerun", "suggestions.triage.modified_rerun", t.modified_rerun);
            let mut g = t.modified_rerun_cosine_guard as f64;
            ui.horizontal(|ui| {
                ui.label("Modified rerun cosine guard");
                let resp = ui.add(
                    egui::Slider::new(&mut g, 0.0..=1.0)
                        .step_by(0.01)
                        .fixed_decimals(2),
                );
                if resp.drag_stopped() || resp.lost_focus() {
                    commit(app, st.scope, "suggestions.triage.modified_rerun_cosine_guard", &json_f(g));
                }
            });
        });
}
}

// ---------------------------------------------------------------------------
// Field widgets.
// ---------------------------------------------------------------------------

fn bool_row(ui: &mut egui::Ui, app: &mut AppState, st: &mut SettingsUi, label: &str, key: &str, current: bool) {
    let mut v = current;
    if ui.checkbox(&mut v, label).changed() {
        commit(app, st.scope, key, &serde_json::Value::Bool(v));
    }
}

fn string_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    current: &str,
) {
    let draft_key = (st.scope, key.to_string());
    let draft = st
        .text_drafts
        .entry(draft_key.clone())
        .or_insert_with(|| current.to_string());
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::TextEdit::singleline(draft).desired_width(ui.available_width() - 4.0),
        );
        let commit_now = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) || !i.focused);
        // We commit on focus-loss regardless of Enter — focus loss is the
        // standard "I'm done editing" gesture in egui forms.
        if resp.lost_focus() && draft.as_str() != current {
            commit(app, st.scope, key, &serde_json::Value::String(draft.clone()));
        }
        let _ = commit_now;
    });
}

/// Font-path row: a `string_row` plus a red "Font not installed" hint
/// when the path is non-empty and doesn't resolve to a readable file.
/// Backs the `editor-three-fonts` settings UI's unresolved-marker rule.
fn font_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    current: &str,
) {
    string_row(ui, app, st, label, key, current);
    if !current.is_empty() && !std::path::Path::new(current).is_file() {
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("Font not installed")
                    .small()
                    .color(egui::Color32::from_rgb(200, 64, 64)),
            );
        });
    }
}

/// Small read-only markdown preview rendered with the live editor stack
/// so the user can see their font picks applied in context (body prose
/// uses `editor_font`, code spans + frontmatter + fenced blocks use
/// `code_font`). The font slots are live-applied via
/// `AppState::refresh_user_fonts` so flipping a setting reflects here on
/// the next frame.
///
/// status: editor-three-fonts
fn render_font_preview(ui: &mut egui::Ui) {
    use editor_core::state::Editor as EditorState;
    use editor_core::theme::light_default;
    use editor_egui::widget::Widget as EditorWidget;
    use editor_md::styling::markdown_decorations;
    use editor_view::viewport::ViewState;
    use std::cell::RefCell;

    /// Frame-persistent preview backing. `thread_local!` keeps the editor
    /// + view state alive across frames so the layout cache doesn't get
    /// thrown away every paint — but it costs nothing when settings isn't
    /// shown. Read-only by construction.
    struct Backing {
        editor: EditorState,
        view: ViewState,
    }

    thread_local! {
        static CACHE: RefCell<Option<Backing>> = const { RefCell::new(None) };
    }

    // Content exercising every font slot: a body-size paragraph (editor
    // font), inline `code` (code font), a fenced code block (code font),
    // and a leading YAML frontmatter block (code font, body size — per
    // `editor-frontmatter-rendering-fix`).
    const SAMPLE: &str = "\
---
title: Font preview
tags: [demo]
---

The quick brown fox jumps over the lazy dog. Body prose uses the editor font; inline `code spans` use the code font.

```rust
fn main() {
    println!(\"hello, world\");
}
```
";

    CACHE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut editor = EditorState::new(SAMPLE);
            // Park the cursor at end of doc so no line reveals its raw
            // markdown markers (same trick the chat md_preview uses).
            editor.selection = editor_core::selection::Selection::single(editor.doc.len_bytes());
            let mut view = ViewState {
                read_only: true,
                hide_gutter: true,
                ..ViewState::default()
            };
            view.wrap_map.set_enabled(true);
            let theme = light_default();
            view.decorations.clear();
            view.decorations.push(markdown_decorations(&editor, Some(&theme)));
            *slot = Some(Backing { editor, view });
        }
        let backing = slot.as_mut().unwrap();
        // Allocate a fixed-ish height — preview content is ~10 lines.
        let width = ui.available_width();
        let intrinsic = backing.view.height_map.total_height();
        let height = if intrinsic <= 0.0 {
            // First-frame estimate; settles on the next paint.
            (SAMPLE.lines().count() as f32) * backing.view.line_height + 16.0
        } else {
            intrinsic.min(280.0)
        };
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        EditorWidget::new(&mut backing.editor, &mut backing.view).show(&mut child);
        if intrinsic <= 0.0 {
            ui.ctx().request_repaint();
        }
    });
}

/// A bounded integer setting: its current value plus the inclusive
/// `[min, max]` range the drag-value clamps to. Grouped so `int_row`
/// takes one field descriptor instead of three loose `u64`s.
struct IntField {
    current: u64,
    min: u64,
    max: u64,
}

fn int_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    field: &IntField,
) {
    let &IntField { current, min, max } = field;
    let mut v = current;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::DragValue::new(&mut v)
                .range(min..=max)
                .speed(1.0),
        );
        if (resp.drag_stopped() || resp.lost_focus()) && v != current {
            commit(app, st.scope, key, &json_u(v));
        }
    });
}

impl<'a> SettingsCtx<'a> {
fn float_row(
    &mut self,
    label: &str,
    key: &str,
    current: f64,
    min: f64,
    max: f64,
    speed: f64,
) {
    let (ui, app, st) = (&mut *self.ui, &mut *self.app, &mut *self.st);
    let mut v = current;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::DragValue::new(&mut v)
                .range(min..=max)
                .speed(speed)
                .max_decimals(2),
        );
        if (resp.drag_stopped() || resp.lost_focus()) && (v - current).abs() > f64::EPSILON {
            commit(app, st.scope, key, &json_f(v));
        }
    });
}

fn port_row(&mut self, label: &str, key: &str, current: u16) {
    let (ui, app, st) = (&mut *self.ui, &mut *self.app, &mut *self.st);
    let mut v = current as u64;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::DragValue::new(&mut v)
                .range(0..=u16::MAX as u64)
                .speed(1.0),
        );
        if (resp.drag_stopped() || resp.lost_focus()) && v as u16 != current {
            commit(app, st.scope, key, &json_u(v));
        }
    });
}
}

/// Hex color row: a swatch button that opens egui's color picker, plus
/// a text field so users can paste exact `#RRGGBBAA` values. Commits on
/// picker drag-stop and on text-field focus-loss.
fn color_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    current: &str,
) {
    let draft_key = (st.scope, key.to_string());
    // Resync the draft to disk if the user hasn't been actively editing it
    // (i.e. the cached draft equals the previous on-disk value); this
    // lets the picker drive the field too.
    if let Some(d) = st.text_drafts.get(&draft_key)
        && d.as_str() != current
    {
        // Keep editing mid-flight — don't clobber.
    } else {
        st.text_drafts.insert(draft_key.clone(), current.to_string());
    }
    let draft = st.text_drafts.entry(draft_key.clone()).or_default();

    let mut color = draft.parse_hex_color().unwrap_or(egui::Color32::MAGENTA);
    let mut committed: Option<String> = None;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.color_edit_button_srgba(&mut color);
        if resp.changed() {
            let new_hex = color.to_hex_string();
            *draft = new_hex.clone();
            committed = Some(new_hex);
        }
        let text_resp =
            ui.add(egui::TextEdit::singleline(draft).desired_width(110.0));
        if text_resp.lost_focus() && draft.as_str() != current && is_valid_hex(draft) {
            committed = Some(draft.clone());
        }
    });
    if let Some(v) = committed {
        commit(app, st.scope, key, &serde_json::Value::String(v));
    }
}

fn is_valid_hex(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b'#'))
        && (b.len() == 7 || b.len() == 9)
        && b[1..].iter().all(u8::is_ascii_hexdigit)
}

/// Parse a `#RRGGBB` or `#RRGGBBAA` string into an egui Color32. Trait
/// methods with `&self` are exempt from `single_call_fn`.
trait ParseHexColor {
    fn parse_hex_color(&self) -> Option<egui::Color32>;
}

impl ParseHexColor for str {
    fn parse_hex_color(&self) -> Option<egui::Color32> {
        let s = self;
        if !is_valid_hex(s) {
            return None;
        }
        let hex = &s[1..];
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
        let a = if hex.len() == 8 { byte(6)? } else { 255 };
        Some(egui::Color32::from_rgba_unmultiplied(r, g, b, a))
    }
}

/// Render an egui Color32 as a `#RRGGBB` / `#RRGGBBAA` string.
trait HexString {
    fn to_hex_string(&self) -> String;
}

impl HexString for egui::Color32 {
    fn to_hex_string(&self) -> String {
        let c = *self;
        if c.a() == 255 {
            format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", c.r(), c.g(), c.b(), c.a())
        }
    }
}

fn enum_combo<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    current: T,
    options: &[(T, &str, &str)],
) {
    let mut selected = current;
    let display = options
        .iter()
        .find(|(v, _, _)| *v == selected)
        .map(|(_, d, _)| *d)
        .unwrap_or("?");
    ui.horizontal(|ui| {
        ui.label(label);
        egui::ComboBox::from_id_salt(key)
            .selected_text(display)
            .show_ui(ui, |ui| {
                for (val, display, _) in options {
                    ui.selectable_value(&mut selected, *val, *display);
                }
            });
    });
    if selected != current {
        if let Some((_, _, wire)) = options.iter().find(|(v, _, _)| *v == selected) {
            commit(app, st.scope, key, &serde_json::Value::String((*wire).to_string()));
        }
    }
}

fn help(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(theme::muted()).small());
}

// ---------------------------------------------------------------------------
// Plumbing — write through `Config::set`, then swap the merged in-memory copy
// and toast the result.
// ---------------------------------------------------------------------------

fn commit(app: &mut AppState, scope: Scope, key: &str, value: &serde_json::Value) {
    let core_scope = scope.to_core();
    let vault_root: PathBuf = app.vault_session.vault_root.clone();
    match Config::set(core_scope, key, value, &vault_root) {
        Ok(new_cfg) => {
            app.vault_session.config.swap_in_place(new_cfg);
            app.push_toast(format!("Saved {key}"), ToastLevel::Info);
        }
        Err(e) => {
            app.push_toast(format!("Save {key} failed: {e}"), ToastLevel::Error);
        }
    }
}

/// Extension trait so the merge-swap can be a method on the shared
/// config handle. Trait methods with `&self` are exempt from
/// `clippy::single_call_fn`.
trait ConfigHandleExt {
    fn swap_in_place(&self, new_cfg: Config);
}

impl ConfigHandleExt for Arc<RwLock<Config>> {
    fn swap_in_place(&self, new_cfg: Config) {
        let handle = self;
    if let Ok(mut guard) = handle.write() {
        *guard = new_cfg;
    }
    }
}

fn json_u(v: u64) -> serde_json::Value {
    serde_json::Value::Number(serde_json::Number::from(v))
}

fn json_f(v: f64) -> serde_json::Value {
    serde_json::Number::from_f64(v)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}
