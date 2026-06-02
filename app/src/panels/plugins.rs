//! Plugins manager — a master/detail surface over `<vault>/.hiker/plugins.json`.
//! Left: the installed plugins (+ a pending install under review). Right: the
//! selected plugin's manifest, its requested **permissions**, load status, and
//! enable / disable / reload / remove actions. "Install from folder" reads a
//! plugin dir, computes its two blake3 pins, presents the permissions for
//! consent, and on accept copies it under `.hiker/plugins/<id>/` and pins it.
//! Enable / disable load / unload the live instance through `PluginHost`.
//
// status: plugin-install-flow, plugin-permissions

use std::path::Path;

use eframe::egui;
use hiker_core::plugins::manifest::{blake3_pin, Manifest, PinnedEntry, PluginsFile};

use crate::state::{AppState, ToastLevel};
use hiker_theme as theme;

/// A plugin picked from disk, parsed + hashed, awaiting the user's consent
/// before it's copied into the vault and pinned. Stored in egui memory.
#[derive(Clone)]
struct PendingInstall {
    id: String,
    name: String,
    version: String,
    description: String,
    author: String,
    permissions: Vec<String>,
    entry: String,
    src_dir: String,
    manifest_hash: String,
    wasm_hash: String,
}

/// A user action collected during render, applied after so it doesn't fight
/// the borrow of the `plugins.json` snapshot.
enum Action {
    Select(String),
    Install,
    ConfirmInstall,
    CancelInstall,
    SetEnabled { id: String, enabled: bool },
    Reload(String),
    Remove(String),
}

fn sel_id() -> egui::Id {
    egui::Id::new("plugins-manager-selected")
}
fn pending_id() -> egui::Id {
    egui::Id::new("plugins-manager-pending")
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    let file = read_file(app);
    let selected: String = ui.ctx().data(|d| d.get_temp(sel_id())).unwrap_or_default();
    let pending: Option<PendingInstall> = ui
        .ctx()
        .data(|d| d.get_temp::<Option<PendingInstall>>(pending_id()))
        .flatten();

    let mut action: Option<Action> = None;
    ui.horizontal(|ui| {
        ui.heading("Plugins");
        if ui.button("Install from folder…").clicked() {
            action = Some(Action::Install);
        }
    });
    ui.label(
        egui::RichText::new("Capability-scoped WASM plugins, pinned by hash in .hiker/plugins.json")
            .color(theme::muted())
            .small(),
    );
    ui.separator();

    egui::SidePanel::left("plugins-manager-list")
        .resizable(true)
        .default_width(220.0)
        .show_inside(ui, |ui| {
            if let Some(a) = list_panel(ui, app, &file, &selected, pending.as_ref()) {
                action = Some(a);
            }
        });
    egui::ScrollArea::vertical().show(ui, |ui| {
        if let Some(a) = detail_panel(ui, app, &file, &selected, pending.as_ref()) {
            action = Some(a);
        }
    });

    if let Some(a) = action {
        apply(ui.ctx(), app, a, pending);
    }
}

/// Left list: a pending-install row (when present) above the installed plugins,
/// each with a status dot.
fn list_panel(
    ui: &mut egui::Ui,
    app: &AppState,
    file: &PluginsFile,
    selected: &str,
    pending: Option<&PendingInstall>,
) -> Option<Action> {
    let mut action = None;
    if let Some(p) = pending {
        let label = egui::RichText::new(format!("[review] {}", p.name)).strong();
        let _ = ui.selectable_label(true, label);
        ui.separator();
    }
    if file.plugins.is_empty() {
        ui.label(egui::RichText::new("No plugins installed.").color(theme::muted()));
        return action;
    }
    for entry in &file.plugins {
        let name = load_manifest_for(app, entry).map_or_else(|| entry.id.clone(), |m| m.name);
        let status = status_dot(app, entry);
        let label = format!("{status} {name}");
        if ui
            .selectable_label(selected == entry.id, label)
            .clicked()
        {
            action = Some(Action::Select(entry.id.clone()));
        }
    }
    action
}

/// A one-glyph status: loaded, disabled, or failed-to-load.
fn status_dot(app: &AppState, entry: &PinnedEntry) -> &'static str {
    if app.vault_session.plugins.is_loaded(&entry.id) {
        "[on]"
    } else if entry.enabled {
        "[!]" // enabled but not loaded — a load error
    } else {
        "[off]"
    }
}

/// Right detail: the pending review takes over when present, else the selected
/// installed plugin's info + permissions + actions.
fn detail_panel(
    ui: &mut egui::Ui,
    app: &mut AppState,
    file: &PluginsFile,
    selected: &str,
    pending: Option<&PendingInstall>,
) -> Option<Action> {
    if let Some(p) = pending {
        return detail_pending(ui, p);
    }
    let Some(entry) = file.plugins.iter().find(|e| e.id == selected) else {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new("Select a plugin.").color(theme::muted()));
        });
        return None;
    };
    detail_installed(ui, app, entry)
}

/// The consent screen for a picked-but-not-yet-installed plugin.
fn detail_pending(ui: &mut egui::Ui, p: &PendingInstall) -> Option<Action> {
    let mut action = None;
    ui.heading(&p.name);
    meta_line(ui, "Id", &p.id);
    meta_line(ui, "Version", &p.version);
    if !p.author.is_empty() {
        meta_line(ui, "Author", &p.author);
    }
    if !p.description.is_empty() {
        ui.label(&p.description);
    }
    ui.add_space(8.0);
    ui.label(egui::RichText::new("This plugin requests:").strong());
    permissions_list(ui, &p.permissions);
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(format!("manifest {}  ·  wasm {}", short_pin(&p.manifest_hash), short_pin(&p.wasm_hash)))
            .color(theme::muted())
            .small(),
    );
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Install").clicked() {
            action = Some(Action::ConfirmInstall);
        }
        if ui.button("Cancel").clicked() {
            action = Some(Action::CancelInstall);
        }
    });
    action
}

/// Detail for an installed plugin: manifest info, permissions, status, actions.
fn detail_installed(ui: &mut egui::Ui, app: &mut AppState, entry: &PinnedEntry) -> Option<Action> {
    let mut action = None;
    let manifest = load_manifest_for(app, entry);
    let name = manifest.as_ref().map_or(entry.id.as_str(), |m| m.name.as_str());
    ui.heading(name);
    if let Some(m) = &manifest {
        meta_line(ui, "Id", &m.id);
        meta_line(ui, "Version", &m.version);
        if !m.author.is_empty() {
            meta_line(ui, "Author", &m.author);
        }
        if !m.description.is_empty() {
            ui.label(&m.description);
        }
    }
    meta_line(ui, "Location", &entry.location);

    ui.add_space(6.0);
    let loaded = app.vault_session.plugins.is_loaded(&entry.id);
    let status = if loaded {
        ("loaded", theme::accent())
    } else if entry.enabled {
        ("enabled — failed to load (see logs)", theme::warn())
    } else {
        ("disabled", theme::muted())
    };
    ui.label(egui::RichText::new(format!("Status: {}", status.0)).color(status.1));

    ui.add_space(8.0);
    ui.label(egui::RichText::new("Permissions").strong());
    match &manifest {
        Some(m) => permissions_list(ui, &m.permissions.0.iter().cloned().collect::<Vec<_>>()),
        None => {
            ui.colored_label(theme::warn(), "manifest unreadable on disk");
        }
    }

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        if entry.enabled {
            if ui.button("Disable").clicked() {
                action = Some(Action::SetEnabled { id: entry.id.clone(), enabled: false });
            }
            if ui.button("Reload").clicked() {
                action = Some(Action::Reload(entry.id.clone()));
            }
        } else if ui.button("Enable").clicked() {
            action = Some(Action::SetEnabled { id: entry.id.clone(), enabled: true });
        }
        if ui.button("Remove").clicked() {
            action = Some(Action::Remove(entry.id.clone()));
        }
    });

    if loaded {
        ui.add_space(10.0);
        ui.separator();
        ui.label(egui::RichText::new("Panel").strong());
        crate::panels::plugin_panel::render_plugin(ui, app, &entry.id);
    }
    action
}

/// Render a permission per row, with a plain-language description and a warning
/// tint for the powerful ones (network, writes).
fn permissions_list(ui: &mut egui::Ui, permissions: &[String]) {
    if permissions.is_empty() {
        ui.label(egui::RichText::new("(none)").color(theme::muted()));
        return;
    }
    for perm in permissions {
        let powerful = perm.starts_with("net:") || perm.starts_with("write:");
        let color = if powerful { theme::warn() } else { theme::muted() };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(perm).monospace().color(color));
            ui.label(
                egui::RichText::new(describe_permission(perm))
                    .color(theme::muted())
                    .small(),
            );
        });
    }
}

fn describe_permission(perm: &str) -> &'static str {
    match perm {
        "read:notes" => "Read any note in the vault",
        "read:active-note" => "Read the currently open note",
        "read:metadata" => "Read note frontmatter and tags",
        "write:notes" => "Create, edit, or delete notes",
        "write:metadata" => "Change frontmatter and tags",
        "ui:sidebar-panel" => "Show a panel in the sidebar",
        "ui:status-bar" => "Show a status-bar item",
        "ui:command-palette" => "Add command-palette entries",
        "timer" => "Run periodic or delayed callbacks",
        "mcp:invoke" => "Call configured MCP tools",
        _ if perm.starts_with("net:") => "Outbound network to specific hosts",
        _ => "(custom capability)",
    }
}

fn meta_line(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("{label}:")).color(theme::muted()).small());
        ui.label(egui::RichText::new(value).small());
    });
}

fn short_pin(pin: &str) -> String {
    let hex = pin.strip_prefix("blake3:").unwrap_or(pin);
    format!("blake3:{}", &hex[..hex.len().min(10)])
}

// --- state mutation -------------------------------------------------------

fn apply(ctx: &egui::Context, app: &mut AppState, action: Action, pending: Option<PendingInstall>) {
    match action {
        Action::Select(id) => ctx.data_mut(|d| d.insert_temp(sel_id(), id)),
        Action::Install => {
            if let Some(p) = pick_plugin_dir(app) {
                ctx.data_mut(|d| d.insert_temp(pending_id(), Some(p)));
            }
        }
        Action::CancelInstall => {
            ctx.data_mut(|d| d.insert_temp(pending_id(), Option::<PendingInstall>::None));
        }
        Action::ConfirmInstall => {
            if let Some(p) = pending {
                confirm_install(app, &p);
                ctx.data_mut(|d| d.insert_temp(pending_id(), Option::<PendingInstall>::None));
                ctx.data_mut(|d| d.insert_temp(sel_id(), p.id));
            }
        }
        Action::SetEnabled { id, enabled } => set_enabled(app, &id, enabled),
        Action::Reload(id) => reload(app, &id),
        Action::Remove(id) => remove(app, &id),
    }
}

/// Pick a plugin directory and parse + hash it into a `PendingInstall`.
fn pick_plugin_dir(app: &mut AppState) -> Option<PendingInstall> {
    let dir = rfd::FileDialog::new().set_title("Select plugin folder").pick_folder()?;
    match build_pending(&dir) {
        Ok(p) => Some(p),
        Err(e) => {
            app.push_toast(format!("Not a plugin folder: {e}"), ToastLevel::Error);
            None
        }
    }
}

fn build_pending(src_dir: &Path) -> Result<PendingInstall, String> {
    let manifest_bytes =
        std::fs::read(src_dir.join("manifest.json")).map_err(|e| format!("manifest.json: {e}"))?;
    let manifest = Manifest::parse(&manifest_bytes).map_err(|e| e.to_string())?;
    let wasm_bytes = std::fs::read(src_dir.join(&manifest.entry))
        .map_err(|e| format!("{}: {e}", manifest.entry))?;
    Ok(PendingInstall {
        id: manifest.id,
        name: manifest.name,
        version: manifest.version,
        description: manifest.description,
        author: manifest.author,
        permissions: manifest.permissions.0.into_iter().collect(),
        entry: manifest.entry,
        src_dir: src_dir.display().to_string(),
        manifest_hash: blake3_pin(&manifest_bytes),
        wasm_hash: blake3_pin(&wasm_bytes),
    })
}

/// Copy the plugin into `.hiker/plugins/<id>/` and pin it (disabled until the
/// user enables it).
fn confirm_install(app: &mut AppState, p: &PendingInstall) {
    if let Err(e) = copy_and_pin(app, p) {
        app.push_toast(format!("Install failed: {e}"), ToastLevel::Error);
    } else {
        app.push_toast(format!("Installed {} (disabled)", p.name), ToastLevel::Info);
    }
}

fn copy_and_pin(app: &AppState, p: &PendingInstall) -> Result<(), String> {
    let rel = format!(".hiker/plugins/{}", p.id);
    let dest = app.vault_session.vault_root.join(&rel);
    std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
    let src = Path::new(&p.src_dir);
    std::fs::copy(src.join("manifest.json"), dest.join("manifest.json"))
        .map_err(|e| e.to_string())?;
    std::fs::copy(src.join(&p.entry), dest.join(&p.entry)).map_err(|e| e.to_string())?;
    let mut file = read_file(app);
    file.plugins.retain(|e| e.id != p.id);
    file.plugins.push(PinnedEntry {
        id: p.id.clone(),
        location: rel,
        manifest_hash: p.manifest_hash.clone(),
        wasm_hash: p.wasm_hash.clone(),
        enabled: false,
    });
    write_file(app, &file)
}

fn set_enabled(app: &mut AppState, id: &str, enabled: bool) {
    let mut file = read_file(app);
    if let Some(e) = file.plugins.iter_mut().find(|e| e.id == id) {
        e.enabled = enabled;
    }
    if let Err(e) = write_file(app, &file) {
        app.push_toast(format!("Save failed: {e}"), ToastLevel::Error);
        return;
    }
    if enabled {
        load_now(app, id, &file);
    } else {
        app.vault_session.plugins.unload(id);
    }
}

fn reload(app: &mut AppState, id: &str) {
    app.vault_session.plugins.unload(id);
    let file = read_file(app);
    load_now(app, id, &file);
}

fn load_now(app: &mut AppState, id: &str, file: &PluginsFile) {
    let Some(entry) = file.plugins.iter().find(|e| e.id == id).cloned() else {
        return;
    };
    let root = app.vault_session.vault_root.clone();
    if let Err(e) = app.vault_session.plugins.load(&root, &entry) {
        app.push_toast(format!("Load failed: {e}"), ToastLevel::Error);
    }
}

fn remove(app: &mut AppState, id: &str) {
    app.vault_session.plugins.unload(id);
    let mut file = read_file(app);
    file.plugins.retain(|e| e.id != id);
    if let Err(e) = write_file(app, &file) {
        app.push_toast(format!("Save failed: {e}"), ToastLevel::Error);
    }
}

fn load_manifest_for(app: &AppState, entry: &PinnedEntry) -> Option<Manifest> {
    if let Some(m) = app.vault_session.plugins.manifest(&entry.id) {
        return Some(m.clone());
    }
    let path = app
        .vault_session
        .vault_root
        .join(&entry.location)
        .join("manifest.json");
    std::fs::read(path).ok().and_then(|b| Manifest::parse(&b).ok())
}

fn plugins_json_path(app: &AppState) -> std::path::PathBuf {
    app.vault_session.vault_root.join(".hiker/plugins.json")
}

fn read_file(app: &AppState) -> PluginsFile {
    std::fs::read(plugins_json_path(app))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_file(app: &AppState, file: &PluginsFile) -> Result<(), String> {
    let path = plugins_json_path(app);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(file).map_err(|e| e.to_string())?;
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}
