//! Patch review tab: lists pending staging proposals with per-row
//! accept/reject + view-diff actions. Backed by `Staging::list_pending`.

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use crate::tab::{Tab, TabKind};
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Patch review");
    ui.add_space(4.0);

    let staging = app.vault_session.services.staging.clone();

    let proposals = match staging.list_pending() {
        Ok(v) => v,
        Err(err) => {
            ui.colored_label(egui::Color32::RED, format!("staging list: {}", err));
            return;
        }
    };

    if proposals.is_empty() {
        ui.add_space(20.0);
        ui.centered_and_justified(|ui| {
            ui.label(
                egui::RichText::new("No pending proposals.")
                    .color(theme::muted()),
            );
        });
        return;
    }

    let changes = app.vault_session.services.changes.clone();
    let mut to_view: Option<(String, String)> = None;
    let mut to_accept: Option<String> = None;
    let mut to_reject: Option<String> = None;
    let mut accept_all = false;
    let mut reject_all = false;

    // Bulk action bar (`staging-bulk-apply-reject`). Mirrors the legacy
    // patch-review tab header where the user can blast through all pending
    // proposals when satisfied or clear the queue when starting over.
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{} pending", proposals.len()))
                .color(theme::muted())
                .small(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Reject all").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                )
                .clicked()
            {
                reject_all = true;
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Accept all").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                )
                .clicked()
            {
                accept_all = true;
            }
        });
    });
    ui.add_space(6.0);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for p in &proposals {
                egui::Frame::default()
                    .fill(theme::active_bg())
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}  · {}  · {}",
                                        &p.id[..p.id.len().min(8)],
                                        p.surface,
                                        p.action,
                                    ))
                                    .strong(),
                                );
                                ui.label(
                                    egui::RichText::new(&p.target_path)
                                        .color(theme::muted())
                                        .small()
                                        .monospace(),
                                );
                                ui.label(
                                    egui::RichText::new({
                                        // chrono not in deps; print raw ms as
                                        // a short HH:MM:SS readable form.
                                        let secs = p.created_at_ms / 1000;
                                        let m = (secs / 60) % 60;
                                        let h = (secs / 3600) % 24;
                                        let s = secs % 60;
                                        format!("created {:02}:{:02}:{:02}", h, m, s)
                                    })
                                    .color(theme::muted())
                                    .small(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                crate::icons::ICONS.primary_check(),
                                                egui::RichText::new("Accept")
                                                    .color(egui::Color32::WHITE),
                                            )
                                            .fill(egui::Color32::from_rgb(
                                                0x2f, 0x8f, 0x4d,
                                            )),
                                        )
                                        .clicked()
                                    {
                                        to_accept = Some(p.id.clone());
                                    }
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                crate::icons::ICONS.primary_cross(),
                                                egui::RichText::new("Reject")
                                                    .color(egui::Color32::WHITE),
                                            )
                                            .fill(egui::Color32::from_rgb(
                                                0xb9, 0x3a, 0x3a,
                                            )),
                                        )
                                        .clicked()
                                    {
                                        to_reject = Some(p.id.clone());
                                    }
                                    if ui.button("View diff").clicked() {
                                        to_view = Some((
                                            p.id.clone(),
                                            p.target_path.clone(),
                                        ));
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
            }
        });

    if let Some((proposal_id, target_path)) = to_view {
        app.open_singleton_tab(TabKind::staging_preview(proposal_id, target_path));
    }
    if let Some(id) = to_accept {
        match staging.accept(&id, &app.vault_session.vault, Some(changes.as_ref())) {
            Ok(o) => app.push_toast(
                format!("Accepted proposal for {}", o.target_path),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(
                format!("Accept failed: {}", err),
                ToastLevel::Error,
            ),
        }
    }
    if let Some(id) = to_reject {
        match staging.reject(&id) {
            Ok(()) => app.push_toast("Proposal rejected", ToastLevel::Info),
            Err(err) => app.push_toast(
                format!("Reject failed: {}", err),
                ToastLevel::Error,
            ),
        }
    }
    if accept_all {
        let filter = hiker_core::staging::types::Filter::default();
        match staging.accept_all(&filter, &app.vault_session.vault, Some(changes.as_ref())) {
            Ok(outcomes) => app.push_toast(
                format!("Accepted {} proposal(s)", outcomes.len()),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(
                format!("Accept-all failed: {}", err),
                ToastLevel::Error,
            ),
        }
    }
    if reject_all {
        let mut n = 0usize;
        for p in &proposals {
            if staging.reject(&p.id).is_ok() {
                n += 1;
            }
        }
        app.push_toast(format!("Rejected {} proposal(s)", n), ToastLevel::Info);
    }
}

impl AppState {
    /// Match the toolbar's open-singleton semantics: focus an existing tab
    /// by discriminant, except for staging-proposal previews which carry a
    /// payload — there we keep one tab per `proposal_id`.
    fn open_singleton_tab(&mut self, kind: TabKind) {
    let state = self;
    if let TabKind::Editor {
        buffer: crate::tab::BufferSource::StagingProposal { proposal_id, .. },
        ..
    } = &kind
    {
        if let Some(existing) = state.session.tabs.iter().find(|t| {
            matches!(
                &t.kind,
                TabKind::Editor {
                    buffer: crate::tab::BufferSource::StagingProposal { proposal_id: pid, .. },
                    ..
                } if pid == proposal_id
            )
        }) {
            state.session.active_tab = Some(existing.id);
            return;
        }
    }
    let id = state.next_tab_id();
    state.session.tabs.push(Tab {
        id,
        kind,
        sticky: true,
    });
    state.session.active_tab = Some(id);
    }
}
