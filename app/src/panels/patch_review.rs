//! Patch review tab: the cross-vault listing of pending agent ops with
//! per-row + bulk accept/reject and a view-diff action. Backed by the op log
//! (`op_writes::list_pending_proposals` to list, `op_writes::flip_op_status`
//! to accept/reject) — the sibling surface to the in-buffer inline patch
//! review (`patch-review.md`). Drifted ops disable Accept per
//! `patch-review-conflicted-accept-disabled`; Reject stays active.
//
// status: write-note-review-surface

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use crate::tab::{Tab, TabKind};
use hiker_theme as theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Patch review");
    ui.add_space(4.0);

    let log = app.vault_session.services.oplog.clone();

    let proposals = match hiker_core::ops::op_writes::list_pending_proposals(log.as_ref()) {
        Ok(v) => v,
        Err(err) => {
            ui.colored_label(egui::Color32::RED, format!("op-log list: {}", err));
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

    let mut to_view: Option<(String, String)> = None;
    // Each accept/reject carries (op_id, target_path) so `flip_op_status`
    // can resolve the op's doc id.
    let mut to_accept: Option<(String, String)> = None;
    let mut to_reject: Option<(String, String)> = None;
    let mut accept_all = false;
    let mut reject_all = false;

    // Bulk action bar. Mirrors the inline file pill's Accept all / Reject all
    // (`patch-review-file-pill`): blast through every non-drifted proposal
    // when satisfied, or clear the whole queue (drifted included) when
    // starting over.
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
                                        "{}  · {}  · {}{}",
                                        &p.op_id[..p.op_id.len().min(8)],
                                        p.surface,
                                        p.action,
                                        if p.drifted { "  · drifted" } else { "" },
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
                                    // Accept disabled for drifted ops
                                    // (`patch-review-conflicted-accept-disabled`).
                                    let accept = ui.add_enabled(
                                        !p.drifted,
                                        egui::Button::image_and_text(
                                            crate::icons::ICONS.primary_check(),
                                            egui::RichText::new("Accept")
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(egui::Color32::from_rgb(
                                            0x2f, 0x8f, 0x4d,
                                        )),
                                    );
                                    if p.drifted {
                                        accept.on_hover_text(
                                            "Drifted: the note changed since this edit was proposed",
                                        );
                                    } else if accept.clicked() {
                                        to_accept = Some((
                                            p.op_id.clone(),
                                            p.target_path.clone(),
                                        ));
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
                                        to_reject = Some((
                                            p.op_id.clone(),
                                            p.target_path.clone(),
                                        ));
                                    }
                                    if ui.button("View diff").clicked() {
                                        to_view = Some((
                                            p.op_id.clone(),
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

    if let Some((op_id, target_path)) = to_view {
        app.open_singleton_tab(TabKind::pending_preview(op_id, target_path));
    }
    if let Some((op_id, target_path)) = to_accept {
        flip_one(app, &op_id, &target_path, /* accept */ true);
    }
    if let Some((op_id, target_path)) = to_reject {
        flip_one(app, &op_id, &target_path, /* accept */ false);
    }
    if accept_all {
        apply_bulk_flip(app, &proposals, /* accept */ true);
    }
    if reject_all {
        apply_bulk_flip(app, &proposals, /* accept */ false);
    }
}

/// Flip every proposal in `proposals` through `op_writes::flip_op_status`,
/// then toast the count. Accept-all skips drifted ops (they can't apply
/// against current accepted state, per `patch-review-file-pill`); Reject-all
/// covers drifted ops too. The single bulk-verb path the file-pill's
/// Accept-all / Reject-all mirror.
fn apply_bulk_flip(
    app: &mut AppState,
    proposals: &[hiker_core::ops::op_writes::PendingProposal],
    accept: bool,
) {
    let log = app.vault_session.services.oplog.clone();
    let mut n = 0usize;
    let mut skipped = 0usize;
    for p in proposals {
        if accept && p.drifted {
            skipped += 1;
            continue;
        }
        if hiker_core::ops::op_writes::flip_op_status(
            log.as_ref(),
            &p.target_path,
            std::slice::from_ref(&p.op_id),
            accept,
        )
        .is_ok()
        {
            n += 1;
        }
    }
    if accept {
        let suffix = if skipped > 0 {
            format!(" ({skipped} drifted skipped)")
        } else {
            String::new()
        };
        app.push_toast(format!("Accepted {n} proposal(s){suffix}"), ToastLevel::Info);
    } else {
        app.push_toast(format!("Rejected {n} proposal(s)"), ToastLevel::Info);
    }
}

/// Flip a single op via `op_writes::flip_op_status` and toast the result.
fn flip_one(app: &mut AppState, op_id: &str, target_path: &str, accept: bool) {
    let log = app.vault_session.services.oplog.clone();
    let ids = [op_id.to_string()];
    let res = hiker_core::ops::op_writes::flip_op_status(
        log.as_ref(),
        target_path,
        &ids,
        accept,
    );
    match (res, accept) {
        (Ok(()), true) => app.push_toast(
            format!("Accepted proposal for {target_path}"),
            ToastLevel::Info,
        ),
        (Ok(()), false) => app.push_toast("Proposal rejected", ToastLevel::Info),
        (Err(err), true) => {
            app.push_toast(format!("Accept failed: {}", err), ToastLevel::Error)
        }
        (Err(err), false) => {
            app.push_toast(format!("Reject failed: {}", err), ToastLevel::Error)
        }
    }
}

impl AppState {
    /// Match the toolbar's open-singleton semantics: focus an existing tab
    /// by discriminant, except for proposal previews which carry a payload —
    /// there we keep one tab per op id.
    fn open_singleton_tab(&mut self, kind: TabKind) {
    let state = self;
    if let TabKind::Editor {
        buffer: crate::tab::BufferSource::PendingProposal { proposal_id, .. },
        ..
    } = &kind
    {
        if let Some(existing) = state.session.tabs.iter().find(|t| {
            matches!(
                &t.kind,
                TabKind::Editor {
                    buffer: crate::tab::BufferSource::PendingProposal { proposal_id: pid, .. },
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
