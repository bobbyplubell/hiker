//! View-options and mutations popup menus rendered in the editor toolbar.
//! Pulled out of `buffer/mod.rs` to keep the parent file under the
//! workspace's per-file length cap.

use eframe::egui;

use crate::buffer::DecorationCache;
use crate::icons;
use crate::state::{AppState, ToastLevel};

pub(super) fn view_options_menu(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let mut wrap = false;
    let mut hide_gutter = false;
    let mut placeholder_special = false;
    let mut highlight_trailing_ws = false;
    let mut hide_frontmatter = false;
    let mut show_minimap = false;
    if let Some(buffer) = app.session.buffers.get(path) {
        wrap = buffer.view.wrap_map.enabled();
        hide_gutter = buffer.view.hide_gutter;
        placeholder_special = buffer.show_whitespace;
        highlight_trailing_ws = buffer.highlight_trailing_whitespace;
        hide_frontmatter = buffer.hide_frontmatter;
        show_minimap = buffer.show_minimap;
    }
    let resp = ui
        .add(egui::Button::image(icons::eye()))
        .on_hover_text("View options");
    egui::Popup::menu(&resp).show(|ui| {
        if ui.checkbox(&mut wrap, "Word wrap").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.view.wrap_map.set_enabled(wrap);
            }
            super::persist_view_setting(app, "editor.word_wrap", serde_json::json!(wrap));
        }
        let mut show_gutter = !hide_gutter;
        if ui.checkbox(&mut show_gutter, "Show line numbers").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.view.hide_gutter = !show_gutter;
            }
            super::persist_view_setting(
                app,
                "editor.show_line_numbers",
                serde_json::json!(show_gutter),
            );
        }
        if ui.checkbox(&mut placeholder_special, "Show whitespace").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.show_whitespace = placeholder_special;
            }
            super::persist_view_setting(
                app,
                "editor.show_whitespace",
                serde_json::json!(placeholder_special),
            );
        }
        if ui
            .checkbox(&mut highlight_trailing_ws, "Highlight trailing whitespace")
            .on_hover_text("Paint a red background over trailing spaces/tabs (view-highlight-trailing-whitespace-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.highlight_trailing_whitespace = highlight_trailing_ws;
                buffer.decoration_cache.trailing_ws = None;
            }
            super::persist_view_setting(
                app,
                "editor.highlight_trailing_whitespace",
                serde_json::json!(highlight_trailing_ws),
            );
        }
        if ui
            .checkbox(&mut show_minimap, "Show minimap")
            .on_hover_text("Structural minimap strip on the right of the editor")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.show_minimap = show_minimap;
            }
            super::persist_view_setting(app, "editor.show_minimap", serde_json::json!(show_minimap));
        }
        if ui.checkbox(&mut hide_frontmatter, "Hide frontmatter").changed() {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.hide_frontmatter = hide_frontmatter;
            }
            super::persist_view_setting(
                app,
                "editor.hide_frontmatter",
                serde_json::json!(hide_frontmatter),
            );
        }
        ui.separator();
        let mut live_preview = false;
        let mut chunk_boundaries = false;
        let mut render_txt_as_md = false;
        let mut intraline_diff = false;
        let mut heading_breadcrumb = false;
        if let Some(buffer) = app.session.buffers.get(path) {
            live_preview = buffer.live_preview;
            chunk_boundaries = buffer.chunk_boundaries;
            render_txt_as_md = buffer.render_txt_as_markdown;
            intraline_diff = buffer.intraline_diff;
            heading_breadcrumb = buffer.heading_breadcrumb;
        }
        if ui
            .checkbox(&mut live_preview, "Live preview")
            .on_hover_text("Inline-render wikilinks, math, callouts (view-live-preview-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.live_preview = live_preview;
                buffer.decoration_cache = DecorationCache::default();
            }
            super::persist_view_setting(
                app,
                "editor.live_preview",
                serde_json::json!(live_preview),
            );
        }
        if ui
            .checkbox(&mut chunk_boundaries, "Show chunk boundaries")
            .on_hover_text("Visualize how the indexer splits this note (view-show-chunk-boundaries)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.chunk_boundaries = chunk_boundaries;
            }
            super::persist_view_setting(
                app,
                "editor.show_chunk_boundaries",
                serde_json::json!(chunk_boundaries),
            );
        }
        let is_txt = path
            .rsplit_once('.')
            .map(|(_, ext)| ext.eq_ignore_ascii_case("txt"))
            .unwrap_or(false);
        ui.add_enabled_ui(is_txt, |ui| {
            if ui
                .checkbox(&mut render_txt_as_md, "Render .txt as markdown")
                .on_hover_text("Apply the markdown live-preview stack to .txt files (view-render-txt-as-markdown-toggle)")
                .changed()
            {
                if let Some(buffer) = app.session.buffers.get_mut(path) {
                    buffer.render_txt_as_markdown = render_txt_as_md;
                    buffer.live_preview = render_txt_as_md;
                    buffer.decoration_cache = DecorationCache::default();
                }
                super::persist_view_setting(
                    app,
                    "editor.render_txt_as_markdown",
                    serde_json::json!(render_txt_as_md),
                );
            }
        });
        if ui
            .checkbox(&mut intraline_diff, "Intraline diff highlights")
            .on_hover_text("Color diff changes at character granularity (view-intraline-diff-toggle)")
            .changed()
        {
            if let Some(buffer) = app.session.buffers.get_mut(path) {
                buffer.intraline_diff = intraline_diff;
            }
            super::persist_view_setting(
                app,
                "editor.intraline_diff",
                serde_json::json!(intraline_diff),
            );
        }
        if ui
            .checkbox(&mut heading_breadcrumb, "Show heading breadcrumb")
            .on_hover_text("Display the cursor's heading path in the status bar (view-heading-breadcrumb-toggle)")
            .changed()
            && let Some(buffer) = app.session.buffers.get_mut(path)
        {
            buffer.heading_breadcrumb = heading_breadcrumb;
        }
    });
}

/// Popup menu offering LLM-backed note mutations. Each pick builds a
/// `TaskKind::NoteMutation` task and submits it to the shared queue.
pub(super) fn mutations_menu(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let in_flight = mutation_in_flight(app, path);
    let resp = ui
        .add_enabled(!in_flight, egui::Button::image(icons::wand()))
        .on_hover_text(if in_flight {
            "Mutation in flight — wait for the queued task to finish."
        } else {
            "Mutations"
        });
    let mut chosen: Option<&'static str> = None;
    egui::Popup::menu(&resp).show(|ui| {
        if ui.button("Reformat as markdown").clicked() {
            chosen = Some("reformat-as-markdown");
            ui.close();
        }
        if ui.button("Summarize").clicked() {
            chosen = Some("summarize");
            ui.close();
        }
        if ui.button("Auto-tag").clicked() {
            chosen = Some("auto-tag");
            ui.close();
        }
        if ui.button("Improve clarity").clicked() {
            chosen = Some("improve-clarity");
            ui.close();
        }
    });
    if let Some(m) = chosen {
        submit_mutation(app, path, m);
    }
}

fn mutation_in_flight(app: &AppState, path: &str) -> bool {
    use hiker_core::tasks::{TaskKind, TaskState};
    let in_queue = app.ui_cache.task_snapshot.iter().any(|r| {
        matches!(r.state, TaskState::Queued | TaskState::Leased)
            && match &r.kind {
                TaskKind::NoteMutation { source_path, .. } => source_path == path,
                _ => false,
            }
    });
    in_queue || app.session.pending_mutations.contains(path)
}

fn submit_mutation(app: &mut AppState, path: &str, mutation: &str) {
    use hiker_core::tasks::{Priority, Task, TaskKind, TaskPayload, TaskShape};

    let Some(buffer) = app.session.buffers.get(path) else {
        return;
    };
    let text = buffer.editor.doc.to_string();
    let kind = TaskKind::NoteMutation {
        mutation: mutation.to_string(),
        source_path: path.to_string(),
    };
    let prompt = match mutation {
        "reformat-as-markdown" => "Reformat the following note as clean Markdown.",
        "summarize" => "Summarize the following note in 2-3 sentences.",
        "auto-tag" => "Propose 3-7 tags for the following note.",
        "improve-clarity" => "Rewrite for clarity, preserving meaning.",
        _ => "Apply the requested mutation.",
    };
    let task = Task {
        id: hiker_core::store::new_id(),
        kind,
        priority: Priority::Normal,
        shape: TaskShape::Direct,
        payload: TaskPayload {
            prompt: format!("{prompt}\n\n---\n{text}"),
            inputs: serde_json::Value::Null,
        },
        output_schema: None,
        submitted_at: std::time::SystemTime::now(),
        metadata: serde_json::json!({
            "source_path": path,
            "source_hash_at_submit": &buffer.loaded_hash,
        }),
    };
    let path_owned = path.to_string();
    app.session.pending_mutations.insert(path_owned.clone());
    let source_hash_at_submit = buffer.loaded_hash.clone();
    let mutation_kind = mutation.to_string();
    let event_tx = app.vault_session.events.mutation_events_tx.clone();
    let queue = app.vault_session.services.tasks.clone();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let path_for_await = path_owned.clone();
            handle.spawn(async move {
                let h = queue.submit(task).await;
                let outcome = h.await_outcome().await;
                let tx = event_tx;
                use hiker_core::tasks::TaskOutcome;
                let ev = match outcome {
                    TaskOutcome::Completed { value, .. } => {
                        let content = match value {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        };
                        crate::state::MutationEvent::Applied {
                            source_path: path_for_await,
                            mutation: mutation_kind,
                            content,
                            source_hash_at_submit,
                        }
                    }
                    TaskOutcome::Failed { error, .. } => crate::state::MutationEvent::Failed {
                        source_path: path_for_await,
                        mutation: mutation_kind,
                        error,
                    },
                    TaskOutcome::Cancelled { .. } => crate::state::MutationEvent::Cancelled {
                        source_path: path_for_await,
                    },
                };
                let _ = tx.send(ev);
            });
        }
        Err(err) => {
            tracing::warn!(error = %err, "no tokio runtime; mutation not submitted");
        }
    }
    app.push_toast(
        format!("Queued mutation '{mutation}' for {path}"),
        ToastLevel::Info,
    );
}
