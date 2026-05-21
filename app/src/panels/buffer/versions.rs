//! Version dropdown for the buffer toolbar. Lists pending staging
//! proposals and changelog snapshots for the active path, and opens the
//! corresponding preview tab when the user picks one.
//!
//! Extracted from `buffer.rs` to keep that file under the project's
//! file-length cap. Entry point is `version_dropdown`.

use eframe::egui;

use crate::state::AppState;
use crate::theme;

pub(super) fn version_dropdown(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    label: &str,
) {
    use crate::tab::{Tab, TabKind};
    let resp = ui
        .add(
            egui::Button::new(
                egui::RichText::new(format!("v {label}"))
                    .color(theme::muted())
                    .small(),
            )
            .small(),
        )
        .on_hover_text("Versions");
    enum Pick {
        Live,
        Snapshot { change_id: String },
        Proposal { id: String },
    }
    let mut pick: Option<Pick> = None;
    egui::Popup::menu(&resp).show(|ui| {
        ui.label(
            egui::RichText::new(format!("Versions of {label}"))
                .small()
                .strong(),
        );
        ui.separator();
        if ui
            .button(egui::RichText::new("* Live buffer (current)"))
            .clicked()
        {
            pick = Some(Pick::Live);
            ui.close();
        }
        // Pending staging proposals first — they're the most actionable
        // versions and the user is most likely to be looking for them.
        // Reads the per-frame cache populated in `refresh_staging_snapshot`.
        let mine: Vec<_> = app
            .ui_cache.staging_snapshot
            .iter()
            .filter(|p| p.target_path == path)
            .cloned()
            .collect();
        if !mine.is_empty() {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Pending proposals")
                    .small()
                    .color(theme::muted()),
            );
            for p in &mine {
                let short = &p.id[..p.id.len().min(8)];
                if ui
                    .add(egui::Button::image_and_text(
                        crate::icons::robot(),
                        egui::RichText::new(format!("{short} · {}", p.action)).small(),
                    ))
                    .clicked()
                {
                    pick = Some(Pick::Proposal { id: p.id.clone() });
                    ui.close();
                }
            }
        }
        // Changelog entries.
        let history = app
            .vault_session.services.changes
            .history_for_path(path, 20)
            .ok()
            .unwrap_or_default();
        if !history.is_empty() {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("Snapshots")
                    .small()
                    .color(theme::muted()),
            );
            for row in &history {
                let ts = chrono_short(row.timestamp_ms);
                let badge = if row.is_current { " · current" } else { "" };
                if ui
                    .button(
                        egui::RichText::new(format!(
                            "{ts}  {}{badge}",
                            row.author,
                        ))
                        .small(),
                    )
                    .clicked()
                {
                    pick = Some(Pick::Snapshot { change_id: row.id.to_string() });
                    ui.close();
                }
            }
        }
        if mine.is_empty() && history.is_empty() {
            ui.label(
                egui::RichText::new("(no prior versions)")
                    .small()
                    .italics()
                    .color(theme::muted()),
            );
        }
    });
    match pick {
        Some(Pick::Live) => {}
        Some(Pick::Proposal { id }) => {
            let kind = TabKind::staging_preview(id.clone(), path.to_string());
            let existing = app.session.tabs.iter().find(|t| {
                matches!(
                    &t.kind,
                    TabKind::Editor {
                        buffer: crate::tab::BufferSource::StagingProposal { proposal_id, .. },
                        ..
                    } if *proposal_id == id
                )
            }).map(|t| t.id);
            match existing {
                Some(tab_id) => app.session.active_tab = Some(tab_id),
                None => {
                    let tab_id = app.next_tab_id();
                    app.session.tabs.push(Tab { id: tab_id, kind, sticky: true });
                    app.session.active_tab = Some(tab_id);
                }
            }
        }
        Some(Pick::Snapshot { change_id }) => {
            let cid_for_find = change_id.clone();
            let kind = TabKind::snapshot_preview(path.to_string(), change_id);
            let existing = app.session.tabs.iter().find(|t| {
                matches!(
                    &t.kind,
                    TabKind::Editor {
                        buffer: crate::tab::BufferSource::Snapshot { change_id: cid, path: p },
                        ..
                    } if *cid == cid_for_find && p == path
                )
            }).map(|t| t.id);
            match existing {
                Some(tab_id) => app.session.active_tab = Some(tab_id),
                None => {
                    let tab_id = app.next_tab_id();
                    app.session.tabs.push(Tab { id: tab_id, kind, sticky: true });
                    app.session.active_tab = Some(tab_id);
                }
            }
        }
        None => {}
    }
}

/// `HH:MM:SS` from a unix-millisecond timestamp. Cheap and avoids
/// pulling chrono in for a single dropdown row label.
fn chrono_short(ms: i64) -> String {
    let secs = ms / 1000;
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod chrono_short_tests {
    use super::chrono_short;

    #[test]
    fn formats_zero() {
        assert_eq!(chrono_short(0), "00:00:00");
    }

    #[test]
    fn formats_one_hour_one_minute_one_second() {
        let ms = (3600 + 60 + 1) * 1000;
        assert_eq!(chrono_short(ms), "01:01:01");
    }

    #[test]
    fn wraps_at_24_hours() {
        // 25 hours → 01 hours displayed (mod 24)
        let ms = 25 * 3600 * 1000;
        assert_eq!(chrono_short(ms), "01:00:00");
    }
}
