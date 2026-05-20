//! Full-tab chat view. Delegates to the shared chat renderer in
//! `crate::chat::render`; the docked variant in the discovery panel
//! uses the same entry point with `Layout::Docked`.

use std::sync::Arc;

use eframe::egui;

use crate::chat::render::{self, Layout};
use crate::chat::session as chat_session;
use crate::state::AppState;

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    session_id: &str,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    ensure_discovered(app);
    let id = if session_id.is_empty() { None } else { Some(session_id) };
    render::show(ui, app, id, Layout::FullTab, rt);
}

/// First-render disk walk. Kept out of `bootstrap::open_vault` so the
/// vault-open path stays minimal and async-free; the cost (one
/// `read_dir` + per-file `read_to_string`) is paid the first time the
/// user actually opens a chat surface.
fn ensure_discovered(app: &mut AppState) {
    if app.session.chat_discovered {
        return;
    }
    let vault_root = app.vault_session.vault_root.clone();
    chat_session::discover(&mut app.session.chat, &vault_root);
    app.session.chat_discovered = true;
}
