//! Docked chat sidebar surface (the `feature::Ctx` path).
//!
//! The docked chat region is migrated onto the `Chat` feature's
//! `View`. It renders through the narrow `feature::Ctx`
//! (`render_sidebar`) rather than `&mut AppState`: chat state comes from
//! `ctx.state` (the feature's `State`), services/vault/config/toasts from
//! the shared ctx, and broad mutations that need full `&mut AppState`
//! (open a linked note, accept/reject a pending op, the active-note send
//! injection, recording the composer's focus id, the quote-selection
//! read) are queued via `ctx.defer`. The picker + new/delete buttons live
//! in the secondary side bar's chrome title row (rendered with full
//! AppState by the workbench host), so the body here is transcript +
//! composer only — matching the old `Layout::SideBar` framing.

use eframe::egui;

use crate::chat::render::{
    AtMentionScan, Chat, LiveOp, ToolCardAction, active_context_label, mention_suggestions,
    render_turn,
};
use crate::chat::session;
use crate::chat::state::{ChatRegistry, ChatRole, State};
use crate::activity::Ctx;
use crate::state::AppState;
use hiker_theme as theme;

/// Render the docked chat sidebar body through the narrow feature `Ctx`.
/// Mirrors `Chat::render_full_tab(show_header=false)`: a transcript area
/// over a multiline composer.
pub(crate) fn render_sidebar(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    SideBar { ctx }.show(ui);
}

/// Per-frame docked-sidebar render context. Wraps the narrow feature
/// `Ctx` so the helpers can be `&mut self` methods on one receiver.
struct SideBar<'a, 'c> {
    ctx: &'a mut Ctx<'c>,
}

impl SideBar<'_, '_> {
    fn st(&mut self) -> &mut State {
        self.ctx.state.downcast_mut::<State>().expect("chat state")
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        // Lazy disk walk on first view (mirrors `ensure_chat_discovered`),
        // then fold in this frame's reply-task events.
        if !self.st().discovered {
            let root = self.ctx.vault.root().to_path_buf();
            let chats_dir = session::chats_dir(self.ctx.config);
            session::discover(&mut self.st().registry, &root, &chats_dir);
            self.st().discovered = true;
        }
        self.st().registry.pump_events();

        // Session picker chrome row — re-homed from the retired bespoke
        // secondary side bar title row. The new/delete buttons live in the
        // section header (`side_bar_action_buttons("chat")`); the picker is
        // here in the body so it has room for the combo. [feature-consumer-sidebar]
        self.session_picker(ui);
        ui.separator();

        // Reserve the composer at the bottom; transcript fills the rest.
        let composer_height = 90.0_f32;
        let avail = ui.available_height();
        let transcript_height = (avail - composer_height - 8.0).max(0.0);
        if transcript_height > 0.0 {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), transcript_height),
                egui::Sense::hover(),
            );
            let mut transcript_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            self.transcript(&mut transcript_ui);
        }
        ui.separator();
        self.composer(ui);
    }

    /// The active-session combobox, rendered against the narrow `Ctx`
    /// (chat registry via `ctx.state`). Mirrors `AppState::chat_session_picker`
    /// but without needing full `&mut AppState`.
    fn session_picker(&mut self, ui: &mut egui::Ui) {
        let active_id = self.st().registry.active.clone();
        let active_label = active_label_for(&self.st().registry, active_id.as_deref());
        let mut switch_to: Option<String> = None;
        let picker_width = ui.available_width().min(280.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add(crate::icons::ICONS.image(crate::icons::Icon::Chat));
            egui::ComboBox::from_id_salt("chat_picker_sidebar_body")
                .selected_text(active_label)
                .width(picker_width)
                .show_ui(ui, |ui| {
                    let mut rows: Vec<(String, String, i64)> = self
                        .st()
                        .registry
                        .sessions
                        .values()
                        .map(|s| (s.id.clone(), s.preview.clone(), s.mtime_unix))
                        .collect();
                    rows.sort_by_key(|r| std::cmp::Reverse(r.2));
                    if rows.is_empty() {
                        ui.label(
                            egui::RichText::new("(no sessions yet)")
                                .color(theme::muted())
                                .small(),
                        );
                    }
                    for (id, preview, _mtime) in rows {
                        let selected = active_id.as_deref() == Some(id.as_str());
                        if ui.selectable_label(selected, &preview).clicked() {
                            switch_to = Some(id);
                        }
                    }
                });
        });
        if let Some(id) = switch_to {
            session::set_active(&mut self.st().registry, &id);
        }
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        // Recompute the per-frame pending-op map off the op log (the tab
        // path reads `ui_cache.pending_snapshot`, which isn't on the
        // narrow ctx; this is the same `list_pending_proposals` walk that
        // populates it).
        let mut live_ops_by_path: std::collections::HashMap<String, Vec<LiveOp>> =
            std::collections::HashMap::new();
        if let Ok(props) =
            hiker_core::ops::op_writes::list_pending_proposals(&self.ctx.services.oplog)
        {
            for p in props {
                live_ops_by_path
                    .entry(p.target_path)
                    .or_default()
                    .push(LiveOp { op_id: p.op_id, drifted: p.drifted });
            }
        }

        let Some(s) = self.st().registry.active_session() else {
            ui.label(
                egui::RichText::new("(no session — press + or start typing)")
                    .color(theme::muted())
                    .small(),
            );
            return;
        };
        let pending = s.pending;
        let streaming = s.streaming_buf.clone();
        let turns = s.turns.clone();
        let session_id = s.id.clone();

        let mut card_action: Option<ToolCardAction> = None;
        let mut link_clicked: Option<String> = None;
        let previews = &mut self.st().registry.md_previews;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(pending)
            .show(ui, |ui| {
                for (turn_idx, turn) in turns.iter().enumerate() {
                    if let Some(tool) = &turn.tool {
                        if let Some(a) = tool.render_card(
                            ui, &session_id, turn_idx, &live_ops_by_path, previews,
                        ) {
                            card_action = Some(a);
                        }
                    } else if let Some(target) = render_turn(ui, turn.role, &turn.text) {
                        link_clicked = Some(target);
                    }
                }
                if !streaming.is_empty()
                    && let Some(target) = render_turn(ui, ChatRole::Assistant, &streaming)
                {
                    link_clicked = Some(target);
                }
                if streaming.is_empty() && pending {
                    typing_indicator(ui);
                }
                ui.add_space(8.0);
                if pending {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(80));
                }
            });

        // Broad effects need full `&mut AppState`; defer them.
        if let Some(action) = card_action {
            self.ctx
                .defer(move |app| (Chat { app }).apply_tool_card_action(action));
        }
        if let Some(target) = link_clicked {
            self.ctx.defer(move |app| {
                let rel = (Chat { app }).resolve_wikilink_target(&target);
                crate::editor_pane::open_file(app, &rel, /*sticky=*/ true);
            });
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        let active_id = self
            .st()
            .registry
            .active
            .clone()
            .unwrap_or_else(|| "_none".to_string());
        let mut draft: String = self
            .st()
            .registry
            .drafts
            .get(&active_id)
            .cloned()
            .unwrap_or_default();
        let pending = self
            .st()
            .registry
            .sessions
            .get(&active_id)
            .map(|s| s.pending)
            .unwrap_or(false);

        let mut send_now = false;
        let mut quote_selection = false;
        egui::Frame::default()
            .inner_margin(egui::Margin::symmetric(4, 0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::TOP),
                        |ui| {
                            if pending {
                                if ui.button("Stop").on_hover_text("Halt this turn").clicked() {
                                    if let Some(sig) =
                                        self.st().registry.stop_signals.get(&active_id)
                                    {
                                        sig.user_halt();
                                    }
                                }
                            } else if ui.button("Send").on_hover_text("Cmd-Enter").clicked() {
                                send_now = true;
                            }
                            // "Quote selection": the read of the active
                            // buffer's selection needs full AppState, so the
                            // button only flags intent here and the actual
                            // read + draft-append is deferred.
                            if ui
                                .add(egui::Button::image_and_text(
                                    crate::icons::ICONS.image(crate::icons::Icon::Plus),
                                    "selection",
                                ))
                                .on_hover_text(
                                    "Insert the editor's current selection as quoted context",
                                )
                                .clicked()
                            {
                                quote_selection = true;
                            }

                            let avail_w = ui.available_width().max(20.0);
                            let edit = egui::TextEdit::multiline(&mut draft)
                                .hint_text("Message the assistant…  (Enter to send, Shift-Enter for newline, @ to mention a note)")
                                .desired_rows(3)
                                .desired_width(avail_w)
                                .return_key(egui::KeyboardShortcut::new(
                                    egui::Modifiers::SHIFT,
                                    egui::Key::Enter,
                                ));
                            let resp = ui.add(edit);
                            // Record the composer's focus id so the editor
                            // pane keeps Ctrl-Z while this field owns focus.
                            let input_id = resp.id;
                            self.ctx.defer(move |app| app.ui.chat_input_id = Some(input_id));

                            if resp.has_focus() {
                                let plain_enter = ui.input(|i| {
                                    i.key_pressed(egui::Key::Enter)
                                        && !i.modifiers.shift
                                        && !i.modifiers.command
                                        && !i.modifiers.ctrl
                                });
                                let cmd_enter = ui.input(|i| {
                                    i.key_pressed(egui::Key::Enter)
                                        && (i.modifiers.command || i.modifiers.ctrl)
                                });
                                if plain_enter || cmd_enter {
                                    send_now = true;
                                }
                            }

                            self.mention_popup(ui, &resp, &mut draft);
                        },
                    );
                });
            });

        // Sync back the (possibly edited) draft.
        self.st().registry.drafts.insert(active_id.clone(), draft);

        if quote_selection {
            let id_for_quote = active_id.clone();
            self.ctx.defer(move |app| {
                if let Some(sel) = (Chat { app }).active_buffer_selection() {
                    quote_into_draft(&mut app.chat_state.registry, &id_for_quote, &sel);
                }
            });
        }

        if send_now {
            let text = std::mem::take(self.st().registry.drafts.entry(active_id).or_default());
            if !text.trim().is_empty() {
                // The active-note injection + the actual send need full
                // AppState (active tab kind, services.mcp), so run them in a
                // deferred closure. Reply tasks spawn on the ambient tokio
                // handle inside `send`.
                self.ctx.defer(move |app| send_with_context(app, text));
                ui.ctx().request_repaint();
            }
        }
    }

    /// @-mention autocomplete popup over the composer's trailing `@token`.
    fn mention_popup(&mut self, ui: &mut egui::Ui, resp: &egui::Response, draft: &mut String) {
        let mention_popup_id = ui.make_persistent_id("chat::mention_popup_sidebar");
        let mention_state = draft.active_at_mention();
        if let Some((_, ref query)) = mention_state {
            let has_any =
                !mention_suggestions(self.ctx.vault.clone(), query, 1).is_empty();
            if has_any {
                egui::Popup::open_id(ui.ctx(), mention_popup_id);
            } else if egui::Popup::is_id_open(ui.ctx(), mention_popup_id) {
                egui::Popup::close_id(ui.ctx(), mention_popup_id);
            }
        } else if egui::Popup::is_id_open(ui.ctx(), mention_popup_id) {
            egui::Popup::close_id(ui.ctx(), mention_popup_id);
        }

        let Some((prefix_start, query)) = mention_state else {
            return;
        };
        let suggestions = mention_suggestions(self.ctx.vault.clone(), &query, 8);
        let popup_w = resp.rect.width().max(160.0);
        egui::Popup::from_response(resp)
            .id(mention_popup_id)
            .open_memory(None)
            .align(egui::RectAlign::BOTTOM_START)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .width(popup_w)
            .show(|ui| {
                ui.set_min_width(popup_w);
                for s in &suggestions {
                    if ui
                        .add(egui::Button::new(s).min_size(egui::vec2(popup_w, 0.0)))
                        .clicked()
                    {
                        *draft = format!(
                            "{}@{} {}",
                            &draft[..prefix_start],
                            s,
                            &draft[prefix_start + 1 + query.len()..],
                        );
                        egui::Popup::close_id(ui.ctx(), mention_popup_id);
                    }
                }
            });
    }
}

/// Label for the active session in the picker combo's closed state.
fn active_label_for(reg: &ChatRegistry, id: Option<&str>) -> String {
    match id.and_then(|i| reg.sessions.get(i)) {
        Some(s) => s.preview.clone(),
        None => "(no active session)".to_string(),
    }
}

/// Animated "assistant is typing…" indicator. Shared shape with the tab
/// path; pulled out so both transcript renderers call one helper.
fn typing_indicator(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        let t = ui.ctx().input(|i| i.time);
        let phase = ((t / 0.4) as i64).rem_euclid(3) as usize + 1;
        let dots: String = ".".repeat(phase);
        ui.label(
            egui::RichText::new(format!("assistant is typing{}", dots))
                .color(theme::muted())
                .italics()
                .small(),
        );
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(400));
    });
}

/// Append the editor's current selection into `id`'s draft as a fenced
/// quote (`chat-input-at-selection`). Runs deferred with full AppState so
/// the next frame's composer shows the updated draft.
fn quote_into_draft(reg: &mut ChatRegistry, id: &str, sel: &str) {
    if sel.trim().is_empty() {
        return;
    }
    let draft = reg.drafts.entry(id.to_string()).or_default();
    let suffix = if draft.is_empty() || draft.ends_with("\n\n") {
        ""
    } else if draft.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    draft.push_str(suffix);
    draft.push_str("```\n");
    draft.push_str(sel);
    if !sel.ends_with('\n') {
        draft.push('\n');
    }
    draft.push_str("```\n");
}

/// Active-note-context injection (`chat-active-note-context-injection`) +
/// dispatch. Prepends a `[active note: …]` / `[active board: …]` block
/// when an editable note or board is focused and the user hasn't already
/// dropped an `@` reference, then hands the message to the registry's
/// `send`. Runs deferred with full AppState.
fn send_with_context(app: &mut AppState, mut text: String) {
    if !text.contains('@')
        && let Some(ctx) = app
            .session
            .active_tab
            .and_then(|id| app.tab_by_id(id))
            .and_then(|t| active_context_label(&t.kind))
    {
        text = format!("{}\n\n{}", ctx, text);
    }
    let vault_root = app.vault_session.vault_root.clone();
    let config = app.vault_session.config.clone();
    let mcp_handler = app.vault_session.services.mcp.as_ref().map(|h| h.agent_handler());
    app.chat_state.registry.send(&vault_root, config, &mcp_handler, &text);
}
