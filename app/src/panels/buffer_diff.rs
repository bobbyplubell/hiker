//! Live diff between the in-memory buffer and its on-disk version.
//!
//! Behaves like a read-only `SnapshotPreview` but rebuilds the preview
//! buffer every frame the underlying buffer text changes, so the diff
//! tracks edits as the user types.

use eframe::egui;

use crate::panels::diff_view;
use crate::state::AppState;
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    ui.heading(format!("Diff · {}", path));
    ui.label(
        egui::RichText::new("Buffer vs disk — read-only preview")
            .color(theme::muted())
            .small(),
    );
    ui.add_space(6.0);

    let Some(buffer) = app.session.buffers.get(path) else {
        ui.label(format!("buffer not loaded: {}", path));
        return;
    };
    let after = buffer.editor.doc.to_string();
    let before = match app.vault_session.vault.read_file(path) {
        Ok(s) => s,
        Err(err) => {
            ui.colored_label(
                egui::Color32::RED,
                format!("Couldn't read disk version of {}: {}", path, err),
            );
            return;
        }
    };
    if before == after {
        ui.label(
            egui::RichText::new("Buffer matches disk — nothing to diff.")
                .color(theme::muted())
                .italics(),
        );
        return;
    }

    // Key the preview buffer on a hash of (before, after) so any edit on
    // either side rebuilds the diff.
    let key = format!(
        "buffer-diff:{}::{}::{}",
        path,
        hash_str(&before),
        hash_str(&after)
    );
    if !app.panels.preview_buffers.contains_key(&key) {
        let buf = diff_view::PreviewBuffer::new(key.clone(), before, after, /* diff_active */ true);
        app.panels.preview_buffers.insert(key.clone(), buf);
    }
    let intraline = app.session.buffers.get(path).map(|b| b.intraline_diff).unwrap_or(false);
    if let Some(buf) = app.panels.preview_buffers.get_mut(&key) {
        diff_view::show_with(ui, buf, intraline);
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
