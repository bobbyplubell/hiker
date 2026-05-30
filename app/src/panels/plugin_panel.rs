//! Renders a plugin's declarative VDOM (`core::plugins::vdom`) natively in
//! egui and routes interactions back to the plugin. The plugin never draws or
//! ships markup — it returns a tree of bounded primitives, the host paints it
//! here, and element events (`input`, `click`) drive `PluginHost::dispatch_event`
//! so the plugin can return a fresh tree. `note_link` clicks open the note
//! directly (no plugin round-trip, no permission needed).
//
// status: plugin-vdom-egui-renderer

use eframe::egui;
use hiker_core::plugins::vdom::{Node, Row, TextStyle};

use crate::state::AppState;
use crate::theme;

/// An interaction the renderer collected this frame, drained after painting so
/// the plugin host can be mutated without holding a borrow of the VDOM.
enum PluginUiEvent {
    /// A primitive fired: route to the plugin via `on_ui_event`.
    Element {
        element_id: String,
        kind: String,
        payload: serde_json::Value,
    },
    /// A `note_link` was clicked: open that note in the editor.
    OpenNote { note_id: String },
}

/// Render one loaded plugin's current panel and process the events it produced.
/// The manager's detail pane calls this for the selected plugin.
pub fn render_plugin(ui: &mut egui::Ui, app: &mut AppState, plugin_id: &str) {
    let mut events: Vec<PluginUiEvent> = Vec::new();
    egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            if let Some(node) = app.vault_session.plugins.current_vdom(plugin_id) {
                render_node(ui, plugin_id, node, &mut events);
            } else {
                ui.label(egui::RichText::new("(plugin returned no UI)").weak());
            }
        });
    process_events(app, plugin_id, events);
}

/// Drain a panel's collected events: forward element events to the plugin and
/// resolve note-link opens against the index.
fn process_events(app: &mut AppState, plugin_id: &str, events: Vec<PluginUiEvent>) {
    for ev in events {
        match ev {
            PluginUiEvent::Element {
                element_id,
                kind,
                payload,
            } => {
                if let Err(e) =
                    app.vault_session
                        .plugins
                        .dispatch_event(plugin_id, &element_id, &kind, payload)
                {
                    tracing::warn!(plugin = %plugin_id, error = %e, "plugin event dispatch failed");
                }
            }
            PluginUiEvent::OpenNote { note_id } => {
                // status: store-id-from-oplog
                let path = app
                    .vault_session
                    .services
                    .oplog
                    .path_for_doc(&note_id)
                    .ok()
                    .flatten();
                if let Some(path) = path {
                    crate::editor_pane::open_file(app, &path, true);
                }
            }
        }
    }
}

fn render_node(ui: &mut egui::Ui, plugin_id: &str, node: &Node, events: &mut Vec<PluginUiEvent>) {
    match node {
        Node::Vstack { children } => {
            ui.vertical(|ui| render_children(ui, plugin_id, children, events));
        }
        Node::Hstack { children } => {
            ui.horizontal(|ui| render_children(ui, plugin_id, children, events));
        }
        Node::Text { value, style } => {
            ui.label(styled(value, *style));
        }
        Node::TextInput {
            id,
            value,
            placeholder,
        } => render_text_input(ui, plugin_id, id, value, placeholder, events),
        Node::Button { id, label } => {
            if ui.button(label).clicked() {
                events.push(PluginUiEvent::Element {
                    element_id: id.clone(),
                    kind: "click".to_string(),
                    payload: serde_json::Value::Null,
                });
            }
        }
        Node::NoteLink { id, label } => {
            let text = if label.is_empty() { id } else { label };
            if ui.link(text).clicked() {
                events.push(PluginUiEvent::OpenNote {
                    note_id: id.clone(),
                });
            }
        }
        Node::List { columns, rows } => render_list(ui, plugin_id, columns, rows, events),
        Node::Divider => {
            ui.separator();
        }
        Node::Spacer => {
            ui.add_space(8.0);
        }
    }
}

fn render_children(
    ui: &mut egui::Ui,
    plugin_id: &str,
    children: &[Node],
    events: &mut Vec<PluginUiEvent>,
) {
    for child in children {
        render_node(ui, plugin_id, child, events);
    }
}

fn render_text_input(
    ui: &mut egui::Ui,
    plugin_id: &str,
    id: &str,
    value: &str,
    placeholder: &str,
    events: &mut Vec<PluginUiEvent>,
) {
    // The live draft lives in egui memory keyed by (plugin, element), so it
    // survives re-renders without the plugin round-tripping every keystroke.
    let mem_id = egui::Id::new(("plugin-input", plugin_id, id));
    let mut draft: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(mem_id))
        .unwrap_or_else(|| value.to_string());
    let resp = ui.add(egui::TextEdit::singleline(&mut draft).hint_text(placeholder));
    if resp.changed() {
        events.push(PluginUiEvent::Element {
            element_id: id.to_string(),
            kind: "input".to_string(),
            payload: serde_json::Value::String(draft.clone()),
        });
    }
    ui.ctx().data_mut(|d| d.insert_temp(mem_id, draft));
}

fn render_list(
    ui: &mut egui::Ui,
    plugin_id: &str,
    columns: &[String],
    rows: &[Row],
    events: &mut Vec<PluginUiEvent>,
) {
    egui::Grid::new(egui::Id::new(("plugin-list", plugin_id)))
        .striped(true)
        .show(ui, |ui| {
            for col in columns {
                ui.label(egui::RichText::new(col).strong());
            }
            if !columns.is_empty() {
                ui.end_row();
            }
            for row in rows {
                for cell in &row.cells {
                    render_node(ui, plugin_id, cell, events);
                }
                ui.end_row();
            }
        });
}

fn styled(value: &str, style: TextStyle) -> egui::RichText {
    let text = egui::RichText::new(value);
    match style {
        TextStyle::Normal => text,
        TextStyle::Heading => text.heading(),
        TextStyle::Muted => text.color(theme::muted()),
        TextStyle::Strong => text.strong(),
        TextStyle::Code => text.monospace(),
    }
}
