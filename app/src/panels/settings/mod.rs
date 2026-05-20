//! Settings tab. Scope-aware (User / Vault) form over `core::config::Config`.
//!
//! Each section is a collapsing group. Common knobs (vault tree, indexing
//! model, llm provider, mcp, staging) render typed widgets and persist
//! through `Config::set`. Less-used sections fall back to a raw-TOML view of
//! the per-scope file so users can still hand-edit without leaving the tab.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use eframe::egui;

use hiker_core::config::{
    Config, IdStampingMode, RecencyBias, SettingsScope, TreeSortBy, WorkerPreferenceCfg,
};

use crate::state::{AppState, ToastLevel};
use crate::theme;

mod raw;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
enum Scope {
    User,
    #[default]
    Vault,
}

impl Scope {
    fn to_core(self) -> SettingsScope {
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
                egui::RichText::new(scope_path_hint(app, ui_state.scope))
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
            window_section(ui, app);
            vault_section(ui, app, &snapshot, &mut ui_state);
            indexing_section(ui, app, &snapshot, &mut ui_state);
            llm_section(ui, app, &snapshot, &mut ui_state);
            mcp_section(ui, app, &snapshot, &mut ui_state);
            staging_section(ui, app, &snapshot, &mut ui_state);
            editor_section(ui, app, &snapshot, &mut ui_state);
            search_section(ui, app, &snapshot, &mut ui_state);
            tasks_section(ui, app, &snapshot, &mut ui_state);
            acp_section(ui, app, &snapshot, &mut ui_state);
            trails_section(ui, app, &snapshot, &mut ui_state);
            suggestions_section(ui, app, &snapshot, &mut ui_state);

            ui.add_space(8.0);
            raw::show(ui, app, ui_state.scope.to_core());
        });

    ui.ctx().data_mut(|d| d.insert_temp(mem_id, ui_state));
}

fn scope_path_hint(app: &AppState, scope: Scope) -> String {
    let paths = hiker_core::config::ConfigPaths::resolve(&app.vault_session.vault_root);
    match scope {
        Scope::User => paths
            .user
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(no platform config dir)".to_string()),
        Scope::Vault => paths.vault.display().to_string(),
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
    let paths = hiker_core::config::ConfigPaths::resolve(&app.vault_session.vault_root);
    match scope {
        Scope::User => paths.user,
        Scope::Vault => Some(paths.vault),
    }
}

// ---------------------------------------------------------------------------
// Section renderers.
// ---------------------------------------------------------------------------

fn window_section(ui: &mut egui::Ui, app: &mut AppState) {
    egui::CollapsingHeader::new("Window")
        .default_open(false)
        .show(ui, |ui| {
            let mut v = app.ui.custom_titlebar;
            if ui
                .checkbox(&mut v, "Custom titlebar (restart required)")
                .changed()
            {
                app.ui.custom_titlebar = v;
                commit(app, Scope::Vault, "ui.custom_titlebar", serde_json::json!(v));
                app.push_toast(
                    "Custom titlebar setting saved (restart to apply)",
                    ToastLevel::Info,
                );
            }
        });
}

fn vault_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
    egui::CollapsingHeader::new("Vault")
        .default_open(matches!(st.scope, Scope::Vault))
        .show(ui, |ui| {
            enum_combo(
                ui, app, st,
                "Tree sort by",
                "vault.tree.sort_by",
                snap.vault.tree.sort_by,
                &[
                    (TreeSortBy::NameAsc, "Name (A-Z)", "name_asc"),
                    (TreeSortBy::NameDesc, "Name (Z-A)", "name_desc"),
                    (TreeSortBy::MtimeDesc, "Modified (newest)", "mtime_desc"),
                    (TreeSortBy::MtimeAsc, "Modified (oldest)", "mtime_asc"),
                ],
            );
            enum_combo(
                ui, app, st,
                "Sidebar mode",
                "vault.sidebar_mode",
                snap.vault.sidebar_mode,
                &[
                    (hiker_core::config::SidebarMode::Files, "Files", "files"),
                    (hiker_core::config::SidebarMode::Clusters, "Cluster trees", "clusters"),
                    (hiker_core::config::SidebarMode::Trails, "Trails", "trails"),
                ],
            );
            bool_row(ui, app, st, "Show sessions in tree", "vault.show_sessions_in_tree", snap.vault.show_sessions_in_tree);
            bool_row(ui, app, st, "Sidebar open at startup", "vault.sidebar_open", snap.vault.sidebar_open);
            bool_row(ui, app, st, "Related panel open at startup", "vault.related_open", snap.vault.related_open);
        });
}

fn indexing_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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

            enum_combo(
                ui, app, st,
                "Note ID stamping",
                "indexing.id_stamping",
                snap.indexing.id_stamping,
                &[
                    (IdStampingMode::Lazy, "Lazy (stamp on reference)", "lazy"),
                    (IdStampingMode::All, "All (stamp every note)", "all"),
                ],
            );
        });
}

fn llm_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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
            int_row(ui, app, st, "Max tokens", "llm.limits.max_tokens", snap.llm.limits.max_tokens as u64, 1, u32::MAX as u64);
            int_row(ui, app, st, "Timeout (s)", "llm.limits.timeout_secs", snap.llm.limits.timeout_secs, 1, u32::MAX as u64);

            ui.add_space(4.0);
            ui.label(egui::RichText::new("Agent").color(theme::muted()).small());
            int_row(ui, app, st, "Iteration cap", "llm.agent.iteration_cap", snap.llm.agent.iteration_cap as u64, 1, u32::MAX as u64);
            int_row(ui, app, st, "Tool timeout (s)", "llm.agent.tool_timeout_secs", snap.llm.agent.tool_timeout_secs, 1, u32::MAX as u64);

            bool_row(ui, app, st, "Audit: log full prompt", "llm.audit.log_full_prompt", snap.llm.audit.log_full_prompt);
            bool_row(ui, app, st, "Background writes need review", "llm.background.review_required", snap.llm.background.review_required);
        });
}

fn mcp_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
    egui::CollapsingHeader::new("MCP server")
        .default_open(true)
        .show(ui, |ui| {
            bool_row(ui, app, st, "Enabled", "mcp.enabled", snap.mcp.enabled);
            string_row(ui, app, st, "Bind host", "mcp.host", &snap.mcp.host);
            if snap.mcp.host != "127.0.0.1" && snap.mcp.host != "localhost" {
                ui.horizontal(|ui| {
                    ui.add(crate::icons::warning().tint(egui::Color32::from_rgb(0xb0, 0x4a, 0x00)));
                    ui.colored_label(
                        egui::Color32::from_rgb(0xb0, 0x4a, 0x00),
                        "non-loopback host exposes vault contents on the network",
                    );
                });
            }
            port_row(ui, app, st, "Port (0 = ephemeral)", "mcp.port", snap.mcp.port);
            int_row(ui, app, st, "Max top_k", "mcp.max_top_k", snap.mcp.max_top_k as u64, 1, u32::MAX as u64);

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

fn staging_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
    egui::CollapsingHeader::new("Staging")
        .default_open(true)
        .show(ui, |ui| {
            bool_row(ui, app, st, "Auto-reject on conflict", "staging.auto_reject_on_conflict", snap.staging.auto_reject_on_conflict);
            int_row(ui, app, st, "Retention (days)", "staging.retention_days", snap.staging.retention_days as u64, 1, u32::MAX as u64);
        });
}

fn editor_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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
            ui.collapsing("Minimap customization", |ui| {
                let m = &e.minimap;
                ui.label(egui::RichText::new("Layout").color(theme::muted()).small());
                int_row(ui, app, st, "Width (px)", "editor.minimap.width", m.width as u64, 16, 300);
                int_row(ui, app, st, "Bar padding left (px)", "editor.minimap.bar_padding_left", m.bar_padding_left as u64, 0, 24);
                int_row(ui, app, st, "Bar padding right (px)", "editor.minimap.bar_padding_right", m.bar_padding_right as u64, 0, 24);
                int_row(ui, app, st, "Bar corner radius (px)", "editor.minimap.bar_corner_radius", m.bar_corner_radius as u64, 0, 6);
                int_row(ui, app, st, "Minimum bar width (px)", "editor.minimap.min_bar_width", m.min_bar_width as u64, 1, 12);
                int_row(ui, app, st, "Bar vertical gap (×0.1px)", "editor.minimap.bar_gap_tenths", m.bar_gap_tenths as u64, 0, 20);
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
            string_row(ui, app, st, "System font", "editor.font_system", &e.font_system);
            string_row(ui, app, st, "Editor font", "editor.font_editor", &e.font_editor);
            string_row(ui, app, st, "Code font", "editor.font_code", &e.font_code);
            help(ui, "Restart required to pick up font changes.");
        });
}

fn search_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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
                    commit(app, st.scope, "search.semantic.min_similarity", json_f(sim));
                }
            });
            int_row(ui, app, st, "Semantic top_k", "search.semantic.top_k", snap.search.semantic.top_k as u64, 5, 100);
        });
}

fn tasks_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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
                int_row(ui, app, st, "Terminal retention (s)", "tasks.terminal_retention_secs", snap.tasks.terminal_retention_secs, 1, u32::MAX as u64);
                bool_row(ui, app, st, "Direct worker enabled", "tasks.direct_worker.enabled", snap.tasks.direct_worker.enabled);
                int_row(ui, app, st, "Direct worker parallelism", "tasks.direct_worker.parallelism", snap.tasks.direct_worker.parallelism as u64, 1, u32::MAX as u64);
                bool_row(ui, app, st, "Expose to chat agent", "tasks.expose_to_chat_agent", snap.tasks.expose_to_chat_agent);
                int_row(ui, app, st, "Lease default (s)", "tasks.lease.default_secs", snap.tasks.lease.default_secs, 1, u32::MAX as u64);
                int_row(ui, app, st, "Lease max (s)", "tasks.lease.max_secs", snap.tasks.lease.max_secs, 1, u32::MAX as u64);
            }
        });
}

fn acp_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
    egui::CollapsingHeader::new("ACP")
        .default_open(false)
        .show(ui, |ui| {
            string_row(ui, app, st, "Command", "acp.command", &snap.acp.command);
            help(ui, "Full command line (e.g. `auggie --acp`). Empty = built-in basic agent.");
        });
}

fn trails_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
    if matches!(st.scope, Scope::User) {
        return;
    }
    egui::CollapsingHeader::new("Trails")
        .default_open(false)
        .show(ui, |ui| {
            string_row(ui, app, st, "New trail directory", "trails.new_trail_dir", &snap.trails.new_trail_dir);
        });
}

fn suggestions_section(ui: &mut egui::Ui, app: &mut AppState, snap: &Config, st: &mut SettingsUi) {
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
                    commit(app, st.scope, "suggestions.triage.modified_rerun_cosine_guard", json_f(g));
                }
            });
        });
}

// ---------------------------------------------------------------------------
// Field widgets.
// ---------------------------------------------------------------------------

fn bool_row(ui: &mut egui::Ui, app: &mut AppState, st: &mut SettingsUi, label: &str, key: &str, current: bool) {
    let mut v = current;
    if ui.checkbox(&mut v, label).changed() {
        commit(app, st.scope, key, serde_json::Value::Bool(v));
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
            commit(app, st.scope, key, serde_json::Value::String(draft.clone()));
        }
        let _ = commit_now;
    });
}

#[allow(clippy::too_many_arguments)]
fn int_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    st: &mut SettingsUi,
    label: &str,
    key: &str,
    current: u64,
    min: u64,
    max: u64,
) {
    let mut v = current;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::DragValue::new(&mut v)
                .range(min..=max)
                .speed(1.0),
        );
        if (resp.drag_stopped() || resp.lost_focus()) && v != current {
            commit(app, st.scope, key, json_u(v));
        }
    });
}

fn port_row(ui: &mut egui::Ui, app: &mut AppState, st: &mut SettingsUi, label: &str, key: &str, current: u16) {
    let mut v = current as u64;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.add(
            egui::DragValue::new(&mut v)
                .range(0..=u16::MAX as u64)
                .speed(1.0),
        );
        if (resp.drag_stopped() || resp.lost_focus()) && v as u16 != current {
            commit(app, st.scope, key, json_u(v));
        }
    });
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

    let mut color = hex_to_color32(draft.as_str()).unwrap_or(egui::Color32::MAGENTA);
    let mut committed: Option<String> = None;
    ui.horizontal(|ui| {
        ui.label(label);
        let resp = ui.color_edit_button_srgba(&mut color);
        if resp.changed() {
            let new_hex = color32_to_hex(color);
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
        commit(app, st.scope, key, serde_json::Value::String(v));
    }
}

fn is_valid_hex(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b'#'))
        && (b.len() == 7 || b.len() == 9)
        && b[1..].iter().all(|c| c.is_ascii_hexdigit())
}

fn hex_to_color32(s: &str) -> Option<egui::Color32> {
    if !is_valid_hex(s) {
        return None;
    }
    let hex = &s[1..];
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let (r, g, b) = (byte(0)?, byte(2)?, byte(4)?);
    let a = if hex.len() == 8 { byte(6)? } else { 255 };
    Some(egui::Color32::from_rgba_unmultiplied(r, g, b, a))
}

fn color32_to_hex(c: egui::Color32) -> String {
    if c.a() == 255 {
        format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
    } else {
        format!("#{:02x}{:02x}{:02x}{:02x}", c.r(), c.g(), c.b(), c.a())
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
            commit(app, st.scope, key, serde_json::Value::String((*wire).to_string()));
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

fn commit(app: &mut AppState, scope: Scope, key: &str, value: serde_json::Value) {
    let core_scope = scope.to_core();
    let vault_root: PathBuf = app.vault_session.vault_root.clone();
    match Config::set(core_scope, key, value, &vault_root) {
        Ok(new_cfg) => {
            swap_in_place(&app.vault_session.config, new_cfg);
            app.push_toast(format!("Saved {key}"), ToastLevel::Info);
        }
        Err(e) => {
            app.push_toast(format!("Save {key} failed: {e}"), ToastLevel::Error);
        }
    }
}

fn swap_in_place(handle: &Arc<RwLock<Config>>, new_cfg: Config) {
    if let Ok(mut guard) = handle.write() {
        *guard = new_cfg;
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
