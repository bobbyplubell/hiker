//! Buffer toolbar strip rendering: the editable-vault toolbar (save / dirty
//! marker / diff toggles / format bar / add-to-trail pill / menus), the
//! read-only source toolbar for snapshots, staging proposals and trash, the
//! pending-rewrite agent banner, and the show/hide-diff toggle button. Split
//! out of `panel/mod.rs` as a continuation of the `BufCtx` impl so that file
//! stays within its per-file line budget; every item here is a method on
//! [`super::BufCtx`].

use eframe::egui;

use super::{format, open_diff_vs_disk, toolbar_menus, BufCtx};
use crate::editor_pane;
use crate::icons;
use crate::state::ToastLevel;
use hiker_theme as theme;

/// The pending-rewrite agent banner shows a compact strip
/// just under the toolbar (`patch-review.md:138-148`) with Accept,
/// Reject, and View-diff actions — *not* the larger half-page banner the
/// old TS UI used.
impl BufCtx<'_> {
    pub(super) fn pending_rewrite_banner(&mut self) {
        let ui = &mut *self.ui;
        let app = &mut *self.app;
        let path: &str = self.path;
        // Reads the per-frame op-log cache populated in
        // `main::refresh_whole_file_proposals`. The most recent whole-file op for
        // the path is the one surfaced (`note-open-routes-to-pending-review`); the
        // list is already sorted newest-first.
        let Some(prop) = app
            .ui_cache.whole_file_proposals
            .iter()
            .find(|p| p.target_path == path)
            .cloned()
        else {
            return;
        };
        let mut accept = false;
        let mut reject = false;
        let mut view = false;
        egui::Frame::default()
            .fill(egui::Color32::from_rgb(0xff, 0xf3, 0xc4))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xd9, 0xb8, 0x4e)))
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(crate::icons::ICONS.image(crate::icons::Icon::Robot));
                    ui.label(
                        egui::RichText::new(if prop.action == "create" {
                            "Agent proposed a new note"
                        } else {
                            "Agent proposed a full-note rewrite"
                        })
                        .small()
                        .strong(),
                    );
                    ui.label(
                        egui::RichText::new(format!("({})", &prop.op_id[..prop.op_id.len().min(8)]))
                            .color(theme::muted())
                            .monospace()
                            .small(),
                    );
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            // Drifted whole-file ops: Accept disabled, reason in
                            // tooltip; Reject + View stay active per
                            // `write-note-review-conflicted-display`.
                            let accept_resp = ui.add_enabled(
                                !prop.drifted,
                                egui::Button::new("Accept").small(),
                            );
                            if accept_resp
                                .on_hover_text(if prop.drifted {
                                    "Proposal drifted from the current note — reject or re-run"
                                } else {
                                    "Apply this rewrite to the note"
                                })
                                .clicked()
                            {
                                accept = true;
                            }
                            if ui.small_button("Reject").clicked() {
                                reject = true;
                            }
                            if ui.small_button("View diff").clicked() {
                                view = true;
                            }
                        },
                    );
                });
            });
        if accept {
            app.accept_staging_proposal(&prop.op_id, &prop.target_path);
        }
        if reject {
            app.reject_staging_proposal(&prop.op_id, &prop.target_path);
        }
        if view {
            use crate::tab::TabKind;
            let pid = prop.op_id.clone();
            let target = prop.target_path.clone();
            let pid_for_build = pid.clone();
            app.find_or_open_tab(
                |k| matches!(
                    k,
                    TabKind::Editor {
                        buffer: crate::tab::BufferSource::PendingProposal { proposal_id, .. },
                        ..
                    } if *proposal_id == pid
                ),
                || TabKind::pending_preview(pid_for_build, target),
            );
        }
    }

    pub(super) fn toolbar(&mut self) {
        let ui = &mut *self.ui;
        let app = &mut *self.app;
        let path: &str = self.path;
        let source = app.session.buffers.get(path).map(|b| b.source.clone());
        let is_vault = matches!(&source, Some(crate::tab::BufferSource::Vault { .. }));
        egui::Frame::default()
            .inner_margin(egui::Margin::symmetric(4, 2))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if !is_vault {
                        // Reconstruct a `BufCtx` inside this closure — the
                        // outer `&mut self` is split into `ui`/`app`/`path`
                        // locals so we can call the read-only sibling as a
                        // method via a fresh borrow.
                        BufCtx { ui, app, path }.render_readonly_source_toolbar(source.as_ref());
                        return;
                    }
                    if ui
                        .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Check)))
                        .on_hover_text("Save (Mod-S)")
                        .clicked()
                    {
                        if let Err(err) = editor_pane::save_buffer(app, path) {
                            app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                        }
                    }
                    let dirty = app.session.buffers.get(path).map(crate::buffer::Buffer::is_dirty).unwrap_or(false);
                    if dirty {
                        ui.add(icons::ICONS.current_dot());
                    }
                    ui.separator();
                    let diff_resp = ui
                        .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Diff)))
                        .on_hover_text("Diff vs disk — right-click to show changes\u{2026}");
                    if diff_resp.clicked() {
                        open_diff_vs_disk(app, path);
                    }
                    diff_resp.context_menu(|ui| {
                        app.show_diff_source_menu(ui, path);
                    });
                    // Agent-diff toggle: jump to the whole-file review-preview
                    // tab when a write-shaped proposal is in flight against this
                    // note. Reads the op-log-backed whole-file-proposal cache
                    // (anchored `edit_note` hunks already review inline via
                    // `agent_proposal`; this button is the whole-file surface).
                    // Mutually-exclusive with the user-diff button above per
                    // `patch-review.md:17-27` — both toggle the same buffer
                    // tab strip into a single diff mode at a time.
                    let has_agent_proposal = app
                        .ui_cache
                        .whole_file_proposals
                        .iter()
                        .any(|p| p.target_path == path);
                    ui.add_enabled_ui(has_agent_proposal, |ui| {
                        if ui
                            .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Robot)))
                            .on_hover_text(if has_agent_proposal {
                                "Agent diff (pending proposal)"
                            } else {
                                "No pending agent proposal for this note"
                            })
                            .clicked()
                        {
                            // Open the whole-file preview for the first (most
                            // recent) matching proposal. Done via singleton tab
                            // semantics so repeated clicks just focus the tab.
                            if let Some(p) = app
                                .ui_cache
                                .whole_file_proposals
                                .iter()
                                .find(|p| p.target_path == path)
                            {
                                use crate::tab::TabKind;
                                let pid = p.op_id.clone();
                                let tpath = p.target_path.clone();
                                let pid_for_build = pid.clone();
                                app.find_or_open_tab(
                                    |k| matches!(
                                        k,
                                        TabKind::Editor {
                                            buffer: crate::tab::BufferSource::PendingProposal { proposal_id, .. },
                                            ..
                                        } if *proposal_id == pid
                                    ),
                                    || TabKind::pending_preview(pid_for_build, tpath),
                                );
                            }
                        }
                    });
                    toolbar_menus::Menus { ui, app, path }.view_options_menu();
                    toolbar_menus::Menus { ui, app, path }.mutations_menu();

                    // Markdown formatting button group (bold / italic / … / color).
                    format::FormatBar { ui: &mut *ui, app: &mut *app, path }.render();

                    // "Add to trail" pill — legacy `addToTrailPill.ts`,
                    // `trail-add-to-active-from-editor-verb`. Hidden when
                    // no active trail or when the buffer path isn't a
                    // regular indexable extension. Disabled (with tooltip)
                    // when the path is already a waypoint at any depth.
                    BufCtx { ui: &mut *ui, app: &mut *app, path }.add_to_trail_pill();

                    // Centered mode-controls slot — empty in plain editing mode.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |_ui| {
                        // (right side reserved for future view-mode badges)
                    });
                });
            });
    }

    /// Toolbar for the read-only source kinds — snapshot blob, staging
    /// proposal, trash entry. Each renders a source-specific verb pair
    /// (Restore / Accept-Reject / nothing) plus the diff toggle when a
    /// `DiffSource` is in play. No Save, no Mutations, no dirty marker —
    fn render_readonly_source_toolbar(&mut self, source: Option<&crate::tab::BufferSource>) {
        let ui = &mut *self.ui;
        let app = &mut *self.app;
        let key: &str = self.path;
        use crate::tab::BufferSource;
        let active_id = app.session.active_tab;
        let diff_active = active_id
            .and_then(|id| app.tab_by_id(id))
            .and_then(|t| t.kind.diff_source())
            .is_some();
        match source {
            Some(BufferSource::HistoryVersion { path, op_id }) => {
                let path = path.clone();
                let cid = op_id.clone();
                if ui
                    .add(
                        egui::Button::image_and_text(
                            icons::ICONS.primary_restore(),
                            egui::RichText::new("Restore").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(0x2f, 0x6f, 0xed)),
                    )
                    .on_hover_text("Write this snapshot back to disk")
                    .clicked()
                {
                    app.restore_snapshot_to_disk(&path, &cid);
                }
                BufCtx { ui: &mut *ui, app: &mut *app, path: key }.render_diff_toggle_button(key, diff_active);
            }
            Some(BufferSource::PendingProposal { proposal_id, target_path }) => {
                let pid = proposal_id.clone();
                let target = target_path.clone();
                // Drift: Accept disabled with reason in tooltip, Reject active —
                // per `write-note-review-conflicted-display`. Read off the
                // op-log cache so the gate matches the listing.
                let drifted = app
                    .ui_cache
                    .whole_file_proposals
                    .iter()
                    .find(|p| p.op_id == pid)
                    .is_some_and(|p| p.drifted);
                let accept_resp = ui.add_enabled(
                    !drifted,
                    egui::Button::new(
                        egui::RichText::new("Accept").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                );
                if accept_resp
                    .on_hover_text(if drifted {
                        "Proposal drifted from the current note — reject or re-run"
                    } else {
                        "Write this proposal to disk"
                    })
                    .clicked()
                {
                    app.accept_staging_proposal(&pid, &target);
                }
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Reject").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                    )
                    .on_hover_text("Discard this proposal")
                    .clicked()
                {
                    app.reject_staging_proposal(&pid, &target);
                }
                BufCtx { ui: &mut *ui, app: &mut *app, path: key }.render_diff_toggle_button(key, diff_active);
            }
            Some(BufferSource::Trash { .. }) => {
                ui.label(egui::RichText::new("In trash · read-only").color(theme::muted()));
            }
            _ => {}
        }
    }

    fn render_diff_toggle_button(&mut self, _key: &str, diff_active: bool) {
        let ui = &mut *self.ui;
        let app = &mut *self.app;
        let label = if diff_active { "Hide diff" } else { "Show diff" };
        if ui.button(label).clicked() {
            // Flip the active tab's diff field between None and Disk(path).
            let Some(active_id) = app.session.active_tab else { return };
            let Some(tab) = app.tab_by_id_mut(active_id) else { return };
            if let crate::tab::TabKind::Editor { buffer, diff } = &mut tab.kind {
                *diff = match diff {
                    Some(_) => None,
                    None => Some(crate::tab::DiffSource::Disk { path: buffer.path().to_string() }),
                };
            }
        }
    }
}
