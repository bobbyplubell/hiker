//! Read-only preview of a single staging proposal with Accept / Reject
//! actions. Shows the on-disk text vs the proposed text as a unified diff
//! in an editor widget; Accept routes through `Staging::accept` and Reject
//! through `Staging::reject`.
//!
//! Per-hunk review: the legacy TS frontend let the user accept individual
//! hunks within a single proposal. We approximate that by computing the
//! line-level diff between disk and proposed text, grouping it into hunks
//! (runs of changes + adjacent context), and offering per-hunk
//! Accept/Reject buttons. The result is materialized into a "partial after"
//! string and written via `vault.write_file_checked`, then the proposal
//! is marked accepted in staging. Whole-proposal Accept/Reject are
//! preserved as the fast path.

use eframe::egui;

use hiker_core::staging::{apply_edit, EditPayload};

use crate::panels::diff_view::{self, PreviewBuffer};
use crate::panels::preview_common::{banner, close_active};
use crate::state::{AppState, ToastLevel};
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState, proposal_id: &str, target_path: &str) {
    let staging = app.vault_session.services.staging.clone();

    let key = format!("staging:{}", proposal_id);

    if !ensure_preview_buffer(ui, app, &staging, &key, proposal_id, target_path) {
        return;
    }

    let mut accept_clicked = false;
    let mut reject_clicked = false;
    let mut toggle_diff = false;

    banner(
        ui,
        "Staging proposal",
        target_path,
        |ui| {
            if ui
                .add(egui::Button::image_and_text(
                    crate::icons::primary_check(),
                    egui::RichText::new("Accept").color(egui::Color32::WHITE),
                ).fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)))
                .on_hover_text("Accept proposal and write to disk")
                .clicked()
            {
                accept_clicked = true;
            }
            if ui
                .add(egui::Button::image_and_text(
                    crate::icons::primary_cross(),
                    egui::RichText::new("Reject").color(egui::Color32::WHITE),
                ).fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)))
                .on_hover_text("Reject and drop proposal")
                .clicked()
            {
                reject_clicked = true;
            }
            if ui.button("Toggle diff").clicked() {
                toggle_diff = true;
            }
        },
    );

    ui.separator();

    let apply_partial = per_hunk_review(ui, app, proposal_id, &key);
    if let Some(partial) = apply_partial {
        apply_partial_write(app, &staging, proposal_id, target_path, &key, &partial);
    }

    if let Some(buf) = app.panels.preview_buffers.get_mut(&key) {
        if toggle_diff {
            buf.diff_active = !buf.diff_active;
        }
        diff_view::show(ui, buf);
    }

    if accept_clicked {
        handle_accept(app, &staging, proposal_id, &key);
    } else if reject_clicked {
        handle_reject(app, &staging, proposal_id, &key);
    }
}

/// Lazily build the preview buffer for this proposal id. Returns `false`
/// if the caller should stop rendering (proposal missing, list error, or
/// resolve-after failed — the helper has already emitted the appropriate
/// UI).
fn ensure_preview_buffer(
    ui: &mut egui::Ui,
    app: &mut AppState,
    staging: &hiker_core::staging::Staging,
    key: &str,
    proposal_id: &str,
    target_path: &str,
) -> bool {
    // If the cache already holds a different key for this slot, blow it away.
    if app
        .panels.preview_buffers
        .get(key)
        .map(|b| b.key != key)
        .unwrap_or(false)
    {
        app.panels.preview_buffers.remove(key);
    }
    if app.panels.preview_buffers.contains_key(key) {
        return true;
    }
    let proposal = match staging.list(&Default::default()) {
        Ok(list) => list.into_iter().find(|p| p.id == proposal_id),
        Err(err) => {
            ui.colored_label(egui::Color32::RED, format!("staging list: {}", err));
            return false;
        }
    };
    let Some(proposal) = proposal else {
        ui.label(format!("Proposal {} not found (already accepted or rejected).", proposal_id));
        if ui.button("Close preview").clicked() {
            close_active(app);
        }
        return false;
    };
    let before = app.vault_session.vault.read_file(target_path).unwrap_or_default();
    let after = match resolve_after(&before, &proposal, staging) {
        Ok(s) => s,
        Err(err) => {
            ui.colored_label(egui::Color32::RED, format!("resolve proposed content: {}", err));
            return false;
        }
    };
    let buf = PreviewBuffer::new(key.to_string(), before, after, true);
    app.panels.preview_buffers.insert(key.to_string(), buf);
    true
}

/// Per-hunk picker (`patch-review-per-hunk-accept`). Collapsible so the
/// default UX stays the whole-proposal flow; users who want hunk
/// granularity expand the section and check the rows they want. Returns
/// the materialized "before + accepted hunks" string when the Apply
/// button is clicked.
fn per_hunk_review(
    ui: &mut egui::Ui,
    app: &mut AppState,
    proposal_id: &str,
    key: &str,
) -> Option<String> {
    let mut apply_partial: Option<String> = None;
    let buf = app.panels.preview_buffers.get(key)?;
    let before = buf.before_text.clone();
    let after = buf.after_text.clone();
    egui::CollapsingHeader::new("Per-hunk review")
        .id_salt(("hunk-picker", proposal_id))
        .default_open(false)
        .show(ui, |ui| {
            let hunks = compute_review_hunks(&before, &after);
            if hunks.is_empty() {
                ui.label(
                    egui::RichText::new("(no textual differences)")
                        .color(theme::muted())
                        .small(),
                );
                return;
            }
            let mem_id = egui::Id::new(("hunk-accept", proposal_id));
            let mut accepted: Vec<bool> = ui
                .ctx()
                .data(|d| d.get_temp::<Vec<bool>>(mem_id))
                .unwrap_or_default();
            if accepted.len() != hunks.len() {
                accepted = vec![true; hunks.len()];
            }
            for (i, h) in hunks.iter().enumerate() {
                render_hunk_row(ui, h, i, &mut accepted);
            }
            ui.ctx().data_mut(|d| d.insert_temp(mem_id, accepted.clone()));
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Apply selected hunks")
                            .color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x6f, 0xb9)),
                )
                .on_hover_text(
                    "Materialize before + checked hunks, write to disk, mark proposal accepted",
                )
                .clicked()
            {
                apply_partial = Some(materialize_partial(&before, &hunks, &accepted));
            }
        });
    apply_partial
}

/// Render one hunk: the header row with checkbox + `@@` summary, followed
/// by a unified-diff body framed in green (accepted) or red (rejected).
fn render_hunk_row(
    ui: &mut egui::Ui,
    h: &ReviewHunk,
    i: usize,
    accepted: &mut [bool],
) {
    ui.horizontal(|ui| {
        ui.checkbox(&mut accepted[i], format!("hunk {}", i + 1));
        ui.label(
            egui::RichText::new(format!(
                "@@ -{},{} +{},{} @@ · −{} +{}",
                h.before_line, h.delete_count.max(1),
                h.after_line, h.insert_count.max(1),
                h.delete_count, h.insert_count
            ))
            .small()
            .monospace()
            .color(theme::muted()),
        );
    });
    let bg = if accepted[i] {
        egui::Color32::from_rgba_unmultiplied(0x2f, 0x8f, 0x4d, 18)
    } else {
        egui::Color32::from_rgba_unmultiplied(0xb9, 0x3a, 0x3a, 18)
    };
    egui::Frame::default()
        .fill(bg)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            for line in &h.body {
                let (glyph, color, before_ln, after_ln) = match line.op {
                    ReviewOp::Equal => (
                        " ",
                        theme::muted(),
                        line.before_line.map(|n| n.to_string()).unwrap_or_default(),
                        line.after_line.map(|n| n.to_string()).unwrap_or_default(),
                    ),
                    ReviewOp::Delete => (
                        "-",
                        egui::Color32::from_rgb(0xb9, 0x3a, 0x3a),
                        line.before_line.map(|n| n.to_string()).unwrap_or_default(),
                        String::new(),
                    ),
                    ReviewOp::Insert => (
                        "+",
                        egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                        String::new(),
                        line.after_line.map(|n| n.to_string()).unwrap_or_default(),
                    ),
                };
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{:>4}", before_ln))
                                .monospace()
                                .small()
                                .color(theme::muted()),
                        )
                        .selectable(false),
                    );
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{:>4}", after_ln))
                                .monospace()
                                .small()
                                .color(theme::muted()),
                        )
                        .selectable(false),
                    );
                    ui.label(
                        egui::RichText::new(format!("{glyph} {}", line.text))
                            .monospace()
                            .small()
                            .color(color),
                    );
                });
            }
        });
    ui.add_space(4.0);
}

/// Write the partial-hunk acceptance to disk and mark the proposal
/// rejected (so it leaves the queue — accept would re-write the full
/// after-text, which we don't want here).
fn apply_partial_write(
    app: &mut AppState,
    staging: &hiker_core::staging::Staging,
    proposal_id: &str,
    target_path: &str,
    key: &str,
    partial: &str,
) {
    let current_hash = hiker_core::hash_str(
        &app.vault_session.vault.read_file(target_path).unwrap_or_default(),
    );
    match app
        .vault_session.vault
        .write_file_checked(target_path, &current_hash, partial)
    {
        Ok(_) => {
            let _ = staging.reject(proposal_id);
            app.push_toast(
                format!("Wrote partial-hunk acceptance to {}", target_path),
                ToastLevel::Info,
            );
            app.panels.preview_buffers.remove(key);
            close_active(app);
        }
        Err(err) => app.push_toast(
            format!("Partial write failed: {}", err),
            ToastLevel::Error,
        ),
    }
}

fn handle_accept(
    app: &mut AppState,
    staging: &hiker_core::staging::Staging,
    proposal_id: &str,
    key: &str,
) {
    let changes = app.vault_session.services.changes.clone();
    match staging.accept(proposal_id, &app.vault_session.vault, Some(changes.as_ref())) {
        Ok(outcome) => {
            app.push_toast(
                format!("Accepted proposal for {}", outcome.target_path),
                ToastLevel::Info,
            );
            app.panels.preview_buffers.remove(key);
            close_active(app);
        }
        Err(err) => app.push_toast(
            format!("Accept failed: {}", err),
            ToastLevel::Error,
        ),
    }
}

fn handle_reject(
    app: &mut AppState,
    staging: &hiker_core::staging::Staging,
    proposal_id: &str,
    key: &str,
) {
    match staging.reject(proposal_id) {
        Ok(()) => {
            app.push_toast("Proposal rejected", ToastLevel::Info);
            app.panels.preview_buffers.remove(key);
            close_active(app);
        }
        Err(err) => app.push_toast(
            format!("Reject failed: {}", err),
            ToastLevel::Error,
        ),
    }
}

/// Resolve the proposal's `after`-side text. For edit proposals, re-apply
/// the patch against the current disk content; for write proposals, fetch
/// the proposal's stored content blob. Mirrors what `Staging::accept`
/// does internally, minus the actual write.
fn resolve_after(
    disk: &str,
    proposal: &hiker_core::staging::Proposal,
    staging: &hiker_core::staging::Staging,
) -> Result<String, String> {
    if let Some(ref edit) = proposal.edit {
        let edit = EditPayload {
            old_str: edit.old_str.clone(),
            new_str: edit.new_str.clone(),
            replace_all: edit.replace_all,
        };
        apply_edit(disk, &edit).map_err(|e| e.to_string())
    } else {
        staging.content(&proposal.id).map_err(|e| e.to_string())
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewOp {
    Equal,
    Delete,
    Insert,
}

/// One reviewable hunk: contiguous run of edits (deletes + inserts) plus
/// the equal-context lines that precede them. The `body` field is the
/// rendered view; `b_start`, `delete_count`, `insert_count`, and the
/// `replace_with` payload drive the partial-apply pipeline.
struct ReviewHunk {
    /// 0-based byte position in the before text where this hunk's
    /// delete-range starts. For pure-insert hunks this is the position
    /// where the insert is anchored.
    b_start: usize,
    delete_count: usize,
    insert_count: usize,
    /// 1-based starting line numbers for the unified-diff header
    /// (`@@ -before_line,N +after_line,M @@`).
    before_line: u32,
    after_line: u32,
    /// Bytes from `before` that this hunk replaces (length = delete_count
    /// in lines, but stored as a string for direct slicing).
    delete_bytes: String,
    /// Bytes to substitute when this hunk is accepted.
    replace_with: String,
    /// Full unified-diff body: context + deletes + inserts, in order, with
    /// 1-based line numbers from `before` / `after` respectively.
    body: Vec<DiffBodyLine>,
}

struct DiffBodyLine {
    op: ReviewOp,
    text: String,
    before_line: Option<u32>,
    after_line: Option<u32>,
}

/// Decompose a before/after diff into hunks suitable for per-hunk review.
/// Equal runs are surfaced only as 1–2 lines of context to keep the list
/// compact; the user accepts or rejects each hunk independently.
fn compute_review_hunks(before: &str, after: &str) -> Vec<ReviewHunk> {
    let diff = hiker_core::diff::compute(before, after);
    let lines: Vec<hiker_core::diff::DiffLine> = diff
        .hunks
        .into_iter()
        .flat_map(|h| h.lines)
        .collect();

    const CONTEXT: usize = 2;
    let mut out: Vec<ReviewHunk> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        while i < lines.len()
            && matches!(lines[i].op, hiker_core::diff::DiffOp::Equal)
        {
            i += 1;
        }
        if i >= lines.len() {
            break;
        }
        let region_start = i;
        let mut deletes: Vec<&hiker_core::diff::DiffLine> = Vec::new();
        let mut inserts: Vec<&hiker_core::diff::DiffLine> = Vec::new();
        while i < lines.len()
            && !matches!(lines[i].op, hiker_core::diff::DiffOp::Equal)
        {
            match lines[i].op {
                hiker_core::diff::DiffOp::Delete => deletes.push(&lines[i]),
                hiker_core::diff::DiffOp::Insert => inserts.push(&lines[i]),
                _ => {}
            }
            i += 1;
        }
        let region_end = i;

        let anchor_before_line = deletes
            .first()
            .and_then(|l| l.before_line_no)
            .or_else(|| {
                lines[..region_start]
                    .iter()
                    .rev()
                    .find_map(|l| l.before_line_no.map(|n| n + 1))
            })
            .unwrap_or(1) as usize;
        let b_start = byte_offset_of_line(before, anchor_before_line.saturating_sub(1));
        let delete_bytes = deletes
            .iter()
            .map(|l| l.line.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let mut delete_bytes_with_nl = delete_bytes.clone();
        if !deletes.is_empty() {
            delete_bytes_with_nl.push('\n');
        }
        let replace_with_lines: Vec<String> =
            inserts.iter().map(|l| l.line.clone()).collect();
        let mut replace_with = replace_with_lines.join("\n");
        if !inserts.is_empty() {
            replace_with.push('\n');
        }

        // Body: a few lines of leading Equal context, the deletes, the
        // inserts, then a few lines of trailing Equal context. Mirrors a
        // standard unified-diff hunk so users can read the change in
        // place without flipping to the full file.
        let mut body: Vec<DiffBodyLine> = Vec::new();
        let lead_start = region_start.saturating_sub(CONTEXT);
        for l in &lines[lead_start..region_start] {
            if matches!(l.op, hiker_core::diff::DiffOp::Equal) {
                body.push(DiffBodyLine {
                    op: ReviewOp::Equal,
                    text: l.line.clone(),
                    before_line: l.before_line_no,
                    after_line: l.after_line_no,
                });
            }
        }
        for d in &deletes {
            body.push(DiffBodyLine {
                op: ReviewOp::Delete,
                text: d.line.clone(),
                before_line: d.before_line_no,
                after_line: None,
            });
        }
        for ins in &inserts {
            body.push(DiffBodyLine {
                op: ReviewOp::Insert,
                text: ins.line.clone(),
                before_line: None,
                after_line: ins.after_line_no,
            });
        }
        let tail_end = (region_end + CONTEXT).min(lines.len());
        for l in &lines[region_end..tail_end] {
            if matches!(l.op, hiker_core::diff::DiffOp::Equal) {
                body.push(DiffBodyLine {
                    op: ReviewOp::Equal,
                    text: l.line.clone(),
                    before_line: l.before_line_no,
                    after_line: l.after_line_no,
                });
            }
        }

        // Hunk-header line numbers: pull from the body's first numbered
        // entry on each side.
        let before_line = body
            .iter()
            .find_map(|l| l.before_line)
            .or_else(|| deletes.first().and_then(|l| l.before_line_no))
            .unwrap_or(1);
        let after_line = body
            .iter()
            .find_map(|l| l.after_line)
            .or_else(|| inserts.first().and_then(|l| l.after_line_no))
            .unwrap_or(1);

        out.push(ReviewHunk {
            b_start,
            delete_count: deletes.len(),
            insert_count: inserts.len(),
            before_line,
            after_line,
            delete_bytes: delete_bytes_with_nl,
            replace_with,
            body,
        });
    }
    out
}

/// Build the partial-accept output by replacing each accepted hunk's
/// delete-range in `before` with its `replace_with`, leaving rejected
/// hunks as the original bytes. Walks hunks back-to-front so byte offsets
/// stay valid as we mutate.
fn materialize_partial(before: &str, hunks: &[ReviewHunk], accepted: &[bool]) -> String {
    let mut out = before.to_string();
    let mut events: Vec<(usize, usize, &str)> = Vec::new();
    for (i, h) in hunks.iter().enumerate() {
        if !accepted.get(i).copied().unwrap_or(false) {
            continue;
        }
        events.push((h.b_start, h.delete_bytes.len(), h.replace_with.as_str()));
    }
    // Apply back-to-front so earlier offsets stay valid through later
    // splice operations.
    events.sort_by_key(|(s, _, _)| *s);
    for (start, del_len, replacement) in events.into_iter().rev() {
        let end = (start + del_len).min(out.len());
        let start = start.min(out.len());
        out.replace_range(start..end, replacement);
    }
    out
}

fn byte_offset_of_line(s: &str, line_idx: usize) -> usize {
    let mut count = 0usize;
    let mut last = 0usize;
    for (idx, b) in s.bytes().enumerate() {
        if count == line_idx {
            return idx;
        }
        if b == b'\n' {
            count += 1;
            last = idx + 1;
        }
    }
    last
}
