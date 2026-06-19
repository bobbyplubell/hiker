//! Patch review tab: the cross-vault listing of pending agent ops with
//! per-row + bulk accept/reject and a view-diff action. Backed by the layered doc
//! (`op_writes::list_pending_proposals` to list, the checked flip seam —
//! `AppState::flip_ops_checked` / `flip_batch_checked` — to accept/reject,
//! which re-verifies the one-sprint invariant at apply time per
//! `derived-status-rule`) — the sibling surface to the in-buffer inline
//! patch review (`patch-review.md`). Drifted ops disable Accept per
//! `patch-review-conflicted-accept-disabled`; Reject stays active.
//!
//! Ops sharing a multi-doc batch id (the `op-log-reorg-batch` /
//! `sprint-rollover` close shape) collapse into ONE row listing every
//! member doc, with batch-level Accept / Reject through
//! `op_writes::flip_batch_status`. There is no per-doc accept inside a
//! recognized batch — accepting half a sprint-close batch would split the
//! rollover across two boards (`pm.md`'s one-sprint invariant), so the
//! batch is the review unit; per-doc View diff stays.
//
// status: write-note-review-surface
// status: sprint-rollover

use eframe::egui;

use hiker_core::ops::op_writes::PendingProposal;

use crate::state::{AppState, ToastLevel};
use crate::tab::{Tab, TabKind};
use hiker_theme as theme;

/// A row action picked this frame, applied after the scroll closure
/// releases its borrows. Batch variants flip every op sharing the batch id
/// as one unit (`flip_batch_status`); op variants flip a single
/// non-batched proposal (`flip_op_status`).
enum RowAction {
    View { op_id: String, target_path: String },
    AcceptOp { op_id: String, target_path: String },
    RejectOp { op_id: String, target_path: String },
    AcceptBatch { batch_id: String },
    RejectBatch { batch_id: String },
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Patch review");
    ui.add_space(4.0);

    let log = app.vault_session.services.layered.clone();

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

    let groups = group_proposals(&proposals);
    let mut action: Option<RowAction> = None;
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
            for group in &groups {
                let picked = match group.as_slice() {
                    [single] => proposal_row(ui, single),
                    many => batch_row(ui, many),
                };
                if picked.is_some() {
                    action = picked;
                }
                ui.add_space(4.0);
            }
        });

    match action {
        Some(RowAction::View { op_id, target_path }) => {
            app.open_singleton_tab(TabKind::pending_preview(op_id, target_path));
        }
        Some(RowAction::AcceptOp { op_id, target_path }) => {
            flip_one(app, &op_id, &target_path, /* accept */ true);
        }
        Some(RowAction::RejectOp { op_id, target_path }) => {
            flip_one(app, &op_id, &target_path, /* accept */ false);
        }
        Some(RowAction::AcceptBatch { batch_id }) => {
            flip_batch(app, &batch_id, /* accept */ true);
        }
        Some(RowAction::RejectBatch { batch_id }) => {
            flip_batch(app, &batch_id, /* accept */ false);
        }
        None => {}
    }
    if accept_all {
        apply_bulk_flip(app, &groups, /* accept */ true);
    }
    if reject_all {
        apply_bulk_flip(app, &groups, /* accept */ false);
    }
}

/// Group the newest-first pending feed into review units: ops sharing a
/// batch id with at least one OTHER pending op collapse into one group
/// (the multi-doc reorg / sprint-close shape — reviewed and flipped as a
/// unit), every other op stands alone. Group order follows the feed (a
/// batch sits where its newest member sat).
fn group_proposals(proposals: &[PendingProposal]) -> Vec<Vec<&PendingProposal>> {
    use std::collections::HashMap;
    let mut member_count: HashMap<&str, usize> = HashMap::new();
    for p in proposals {
        if let Some(batch) = p.batch_id.as_deref() {
            *member_count.entry(batch).or_default() += 1;
        }
    }
    let mut out: Vec<Vec<&PendingProposal>> = Vec::new();
    let mut batch_slot: HashMap<&str, usize> = HashMap::new();
    for p in proposals {
        match p
            .batch_id
            .as_deref()
            .filter(|batch| member_count[batch] > 1)
        {
            Some(batch) => {
                if let Some(&slot) = batch_slot.get(batch) {
                    out[slot].push(p);
                } else {
                    batch_slot.insert(batch, out.len());
                    out.push(vec![p]);
                }
            }
            None => out.push(vec![p]),
        }
    }
    out
}

/// One single (non-batched) pending op as a review row: id / surface /
/// action header, target path, created stamp, and the per-op Accept /
/// Reject / View diff verbs. Accept disabled for drifted ops
/// (`patch-review-conflicted-accept-disabled`).
fn proposal_row(ui: &mut egui::Ui, p: &PendingProposal) -> Option<RowAction> {
    let mut action = None;
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
                        egui::RichText::new(created_stamp(p.created_at_ms))
                            .color(theme::muted())
                            .small(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Accept disabled for drifted ops
                    // (`patch-review-conflicted-accept-disabled`).
                    let accept = ui.add_enabled(
                        !p.drifted,
                        egui::Button::image_and_text(
                            crate::icons::ICONS.primary_check(),
                            egui::RichText::new("Accept").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                    );
                    if p.drifted {
                        accept.on_hover_text(
                            "Drifted: the note changed since this edit was proposed",
                        );
                    } else if accept.clicked() {
                        action = Some(RowAction::AcceptOp {
                            op_id: p.op_id.clone(),
                            target_path: p.target_path.clone(),
                        });
                    }
                    if ui
                        .add(
                            egui::Button::image_and_text(
                                crate::icons::ICONS.primary_cross(),
                                egui::RichText::new("Reject").color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                        )
                        .clicked()
                    {
                        action = Some(RowAction::RejectOp {
                            op_id: p.op_id.clone(),
                            target_path: p.target_path.clone(),
                        });
                    }
                    if ui.button("View diff").clicked() {
                        action = Some(RowAction::View {
                            op_id: p.op_id.clone(),
                            target_path: p.target_path.clone(),
                        });
                    }
                });
            });
        });
    action
}

/// A multi-doc batch as ONE review row (`op-log-reorg-batch` — the
/// sprint-close shape): the member docs listed under a shared header, one
/// batch-level Accept / Reject pair. No per-doc accept here — accepting
/// half the batch would split a machine-computed unit (a sprint close
/// landing on one board but not the other); per-doc View diff stays.
/// Accept disables when ANY member drifted, since the batch's partial
/// apply would skip the drifted member and split the unit anyway.
fn batch_row(ui: &mut egui::Ui, ops: &[&PendingProposal]) -> Option<RowAction> {
    let mut action = None;
    let batch_id = ops.first().and_then(|p| p.batch_id.clone())?;
    let any_drifted = ops.iter().any(|p| p.drifted);
    egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  · {}  · batch of {} docs{}",
                            &batch_id[..batch_id.len().min(8)],
                            ops[0].surface,
                            ops.len(),
                            if any_drifted { "  · drifted" } else { "" },
                        ))
                        .strong(),
                    );
                    for p in ops {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}  · {}{}",
                                    p.target_path,
                                    p.action,
                                    if p.drifted { "  · drifted" } else { "" },
                                ))
                                .color(theme::muted())
                                .small()
                                .monospace(),
                            );
                            if ui.small_button("View diff").clicked() {
                                action = Some(RowAction::View {
                                    op_id: p.op_id.clone(),
                                    target_path: p.target_path.clone(),
                                });
                            }
                        });
                    }
                    ui.label(
                        egui::RichText::new(created_stamp(ops[0].created_at_ms))
                            .color(theme::muted())
                            .small(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let accept = ui.add_enabled(
                        !any_drifted,
                        egui::Button::image_and_text(
                            crate::icons::ICONS.primary_check(),
                            egui::RichText::new("Accept batch").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                    );
                    if any_drifted {
                        accept.on_hover_text(
                            "A batch member drifted: accepting would apply only part of \
                             the batch — reject and re-run instead",
                        );
                    } else if accept.clicked() {
                        action = Some(RowAction::AcceptBatch { batch_id: batch_id.clone() });
                    }
                    if ui
                        .add(
                            egui::Button::image_and_text(
                                crate::icons::ICONS.primary_cross(),
                                egui::RichText::new("Reject batch").color(egui::Color32::WHITE),
                            )
                            .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                        )
                        .clicked()
                    {
                        action = Some(RowAction::RejectBatch { batch_id: batch_id.clone() });
                    }
                });
            });
        });
    action
}

/// Short readable HH:MM:SS form of an op's created stamp (chrono not in
/// deps; raw ms suffices for a same-day review queue).
fn created_stamp(created_at_ms: i64) -> String {
    let secs = created_at_ms / 1000;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let s = secs % 60;
    format!("created {:02}:{:02}:{:02}", h, m, s)
}

/// Flip every review unit: batches as one checked batch flip each (never
/// split), singles through the checked per-op flip, then toast the count.
/// The checked seam re-verifies the one-sprint invariant at apply time
/// (`derived-status-rule`) — a refused unit simply stays pending here.
/// Accept-all skips drifted singles AND any batch with a drifted member
/// (a partial batch apply would split the unit); Reject-all covers
/// everything. The single bulk-verb path the file-pill's Accept-all /
/// Reject-all mirror.
fn apply_bulk_flip(app: &mut AppState, groups: &[Vec<&PendingProposal>], accept: bool) {
    let mut n = 0usize;
    let mut skipped = 0usize;
    for group in groups {
        if accept && group.iter().any(|p| p.drifted) {
            skipped += group.len();
            continue;
        }
        if let [p] = group.as_slice() {
            if app
                .flip_ops_checked(&p.target_path, std::slice::from_ref(&p.op_id), accept)
                .is_ok()
            {
                n += 1;
            }
        } else if let Some(batch_id) = group.first().and_then(|p| p.batch_id.as_deref())
            && let Ok(ids) = app.flip_batch_checked(batch_id, accept)
        {
            n += ids.len();
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

/// Flip a single op via the checked seam (`AppState::flip_ops_checked`)
/// and toast the result — an apply-time one-sprint refusal surfaces its
/// reason in the accept-failed toast while the row stays pending.
fn flip_one(app: &mut AppState, op_id: &str, target_path: &str, accept: bool) {
    let ids = [op_id.to_string()];
    let res = app.flip_ops_checked(target_path, &ids, accept);
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

/// Flip an entire multi-doc batch via the checked seam
/// (`AppState::flip_batch_checked`) — every member op applies / discards
/// as one unit, with the apply-time one-sprint re-check on accept — and
/// toast the count.
fn flip_batch(app: &mut AppState, batch_id: &str, accept: bool) {
    let res = app.flip_batch_checked(batch_id, accept);
    match (res, accept) {
        (Ok(ids), true) => app.push_toast(
            format!("Accepted batch: {} op(s) applied", ids.len()),
            ToastLevel::Info,
        ),
        (Ok(ids), false) => app.push_toast(
            format!("Rejected batch: {} op(s) discarded", ids.len()),
            ToastLevel::Info,
        ),
        (Err(err), true) => {
            app.push_toast(format!("Batch accept failed: {}", err), ToastLevel::Error)
        }
        (Err(err), false) => {
            app.push_toast(format!("Batch reject failed: {}", err), ToastLevel::Error)
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
    state.session.tabs.push(Tab::new(id, kind, true));
    state.session.active_tab = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use hiker_core::ops::op_writes::PendingProposal;

    use super::group_proposals;

    fn proposal(op_id: &str, path: &str, batch: Option<&str>) -> PendingProposal {
        PendingProposal {
            op_id: op_id.to_string(),
            target_path: path.to_string(),
            surface: "sprint-close".to_string(),
            action: "write_note",
            created_at_ms: 0,
            drifted: false,
            batch_id: batch.map(str::to_string),
        }
    }

    /// Ops sharing a multi-doc batch id collapse into ONE review group
    /// (the sprint-close shape: both board-docs, one row); ops with a
    /// solo batch id (the common one-op staging) and ops with no batch id
    /// stay independent rows.
    #[test]
    fn group_proposals_collapses_multi_doc_batches_only() {
        let rows = vec![
            proposal("op-1", "boards/s1.md", Some("close-batch")),
            proposal("op-2", "notes/a.md", Some("solo-batch")),
            proposal("op-3", "boards/s2.md", Some("close-batch")),
            proposal("op-4", "notes/b.md", None),
        ];
        let groups = group_proposals(&rows);
        assert_eq!(groups.len(), 3, "close batch is one group");
        // The batch group sits where its newest member sat, listing both docs.
        let batch: Vec<&str> = groups[0].iter().map(|p| p.target_path.as_str()).collect();
        assert_eq!(batch, ["boards/s1.md", "boards/s2.md"]);
        assert_eq!(groups[1].len(), 1);
        assert_eq!(groups[1][0].op_id, "op-2");
        assert_eq!(groups[2].len(), 1);
        assert_eq!(groups[2][0].op_id, "op-4");
    }
}
