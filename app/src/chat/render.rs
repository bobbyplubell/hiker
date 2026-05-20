//! Chat panel renderer. One entry point (`show`) used by both the
//! full-tab `panels::agent` view and the docked region at the bottom
//! of the discovery panel. A `Layout` enum picks the framing — the
//! tab variant gets a session picker header strip + larger transcript
//! area; the docked variant collapses the picker to a single
//! current-session label + new-button.

use std::sync::Arc;

use eframe::egui;

use crate::chat::{session, state};
use crate::chat::state::{ChatRegistry, ChatRole};
use crate::state::AppState;
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Full-tab Agent view. Picker header + large transcript +
    /// composer.
    FullTab,
    /// Bottom-docked region in the discovery panel. Compact header,
    /// scrollable transcript, single-line composer.
    #[allow(dead_code)]
    Docked,
}

/// Render the chat panel. `session_id` is the tab's preferred
/// session; when `Some` and the id exists, it overrides the
/// registry's active pointer for the duration of the frame. The
/// docked variant passes `None` to follow the registry.
pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    session_id: Option<&str>,
    layout: Layout,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    // 1) Fold in any pending reply-task events from this frame.
    state::pump_events(&mut app.session.chat);

    // 2) Override active pointer if the tab specified one. Lazy-load
    //    historic sessions on first view in case discovery didn't run
    //    yet (e.g. tab restored from a saved layout).
    if let Some(id) = session_id
        && app.session.chat.sessions.contains_key(id)
    {
        app.session.chat.active = Some(id.to_string());
    }

    match layout {
        Layout::FullTab => render_full_tab(ui, app, rt),
        Layout::Docked => render_docked(ui, app, rt),
    }
}

fn render_full_tab(ui: &mut egui::Ui, app: &mut AppState, rt: &Arc<tokio::runtime::Runtime>) {
    // Header strip: session picker + new + delete.
    header(ui, app, /*full=*/ true);
    ui.separator();

    // Reserve the composer at the bottom; transcript fills the rest.
    // No min-height floor on the transcript — previously a `.max(120.0)`
    // floor combined with the composer reservation made the panel
    // un-shrinkable past ~210px tall and pushed the Send button off the
    // bottom edge.
    let composer_height = 90.0_f32;
    let avail = ui.available_height();
    let transcript_height = (avail - composer_height - 8.0).max(0.0);

    if transcript_height > 0.0 {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), transcript_height),
            egui::Sense::hover(),
        );
        let mut transcript_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        transcript(&mut transcript_ui, app);
    }

    ui.separator();
    composer(ui, app, rt, /*multiline=*/ true);
}

fn render_docked(ui: &mut egui::Ui, app: &mut AppState, rt: &Arc<tokio::runtime::Runtime>) {
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(6.0)
        .show(ui, |ui| {
            header(ui, app, /*full=*/ false);
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false, false])
                .show(ui, |ui| transcript(ui, app));
            ui.add_space(2.0);
            composer(ui, app, rt, /*multiline=*/ false);
        });
}

fn header(ui: &mut egui::Ui, app: &mut AppState, full: bool) {
    ui.horizontal(|ui| {
        ui.add(crate::icons::chat());
        if full {
            ui.label(egui::RichText::new("Chat").strong());
        }
        ui.add_space(4.0);

        // Session picker.
        let active_id = app.session.chat.active.clone();
        let active_label = active_label_for(&app.session.chat, active_id.as_deref());
        let mut switch_to: Option<String> = None;
        let mut delete: Option<String> = None;
        let mut create_new = false;

        // Reserve room for the +/trash buttons (~64px) so they don't
        // slide off-screen when the panel is narrow; clamp to a small
        // minimum so the picker stays usable.
        let picker_width = (ui.available_width() - 64.0)
            .clamp(80.0, if full { 280.0 } else { 180.0 });
        egui::ComboBox::from_id_salt(if full { "chat_picker_full" } else { "chat_picker_docked" })
            .selected_text(active_label)
            .width(picker_width)
            .show_ui(ui, |ui| {
                // Newest-first; clone+sort to dodge borrow churn.
                let mut rows: Vec<(String, String, i64)> = app
                    .session.chat
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

        if ui
            .add(egui::Button::image(crate::icons::plus()).small())
            .on_hover_text("New session")
            .clicked()
        {
            create_new = true;
        }
        if active_id.is_some()
            && ui
                .add(egui::Button::image(crate::icons::trash()).small())
                .on_hover_text("Delete this session")
                .clicked()
        {
            delete = active_id.clone();
        }

        // Apply.
        if let Some(id) = switch_to {
            session::set_active(&mut app.session.chat, &id);
        }
        if create_new {
            let vault_root = app.vault_session.vault_root.clone();
            let (model, provider) = app
                .vault_session.config
                .read()
                .map(|c| (c.llm.provider.model.clone(), c.llm.provider.backend.clone()))
                .unwrap_or_else(|_| ("stub-model".into(), "stub".into()));
            if let Err(err) = session::create_new(
                &mut app.session.chat,
                &vault_root,
                &model,
                &provider,
            ) {
                tracing::warn!(error = %err, "chat: create_new failed");
            }
        }
        if let Some(id) = delete {
            let vault_root = app.vault_session.vault_root.clone();
            if let Err(err) = session::delete(&mut app.session.chat, &vault_root, &id) {
                tracing::warn!(error = %err, "chat: delete failed");
            }
        }
    });
}

fn active_label_for(reg: &ChatRegistry, id: Option<&str>) -> String {
    match id.and_then(|i| reg.sessions.get(i)) {
        Some(s) => s.preview.clone(),
        None => "(no active session)".to_string(),
    }
}

fn transcript(ui: &mut egui::Ui, app: &mut AppState) {
    let Some(s) = app.session.chat.active_session() else {
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
    let mut card_action: Option<ToolCardAction> = None;
    let mut link_clicked: Option<String> = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for turn in &turns {
                if let Some(tool) = &turn.tool {
                    if let Some(a) = render_tool_card(ui, tool) {
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
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    // Cycle 1..=3 dots roughly every 400ms so the user
                    // sees motion. ctx.request_repaint_after keeps the
                    // animation ticking even without input.
                    let t = ui.ctx().input(|i| i.time);
                    let phase = ((t / 0.4) as i64).rem_euclid(3) as usize + 1;
                    let dots: String = ".".repeat(phase);
                    ui.label(
                        egui::RichText::new(format!("assistant is typing{}", dots))
                            .color(theme::muted())
                            .italics()
                            .small(),
                    );
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(400));
                });
            }
            ui.add_space(8.0);
        });
    // Apply tool-card actions on the AppState. Staging accept/reject
    // route through the live `Staging`; open-target hands off to the
    // tab-open machinery via `controllers::open_file`.
    if let Some(action) = card_action {
        apply_tool_card_action(app, action);
    }
    if let Some(target) = link_clicked {
        let rel = resolve_wikilink_target(app, &target);
        crate::editor_pane::open_file(app, &rel, /*sticky=*/ true);
    }
}

/// Resolve a wikilink target like `Some Note` or `notes/foo` into a
/// vault-relative path. Falls back to the literal `target.md` when no
/// indexed match exists.
fn resolve_wikilink_target(app: &AppState, target: &str) -> String {
    let direct = if target.ends_with(".md") {
        target.to_string()
    } else {
        format!("{}.md", target)
    };
    if app.vault_session.vault.read_file(&direct).is_ok() {
        return direct;
    }
    // Try basename match across walk — cheap enough for short transcripts.
    if let Ok(paths) = app.vault_session.vault.walk_indexable_files("") {
        let needle_md = direct.rsplit('/').next().unwrap_or(direct.as_str());
        let needle_base = target.rsplit('/').next().unwrap_or(target);
        for p in &paths {
            let base = p.rsplit('/').next().unwrap_or(p);
            if base == needle_md || base.strip_suffix(".md") == Some(needle_base) {
                return p.clone();
            }
        }
    }
    direct
}

fn apply_tool_card_action(app: &mut AppState, action: ToolCardAction) {
    use crate::state::ToastLevel;
    match action {
        ToolCardAction::AcceptStaging { proposal_id } => {
            let staging = app.vault_session.services.staging.clone();
            let changes = app.vault_session.services.changes.clone();
            match staging.accept(&proposal_id, &app.vault_session.vault, Some(changes.as_ref())) {
                Ok(o) => app.push_toast(
                    format!("Accepted proposal for {}", o.target_path),
                    ToastLevel::Info,
                ),
                Err(err) => app.push_toast(
                    format!("Accept failed: {err}"),
                    ToastLevel::Error,
                ),
            }
        }
        ToolCardAction::RejectStaging { proposal_id } => {
            let staging = app.vault_session.services.staging.clone();
            match staging.reject(&proposal_id) {
                Ok(()) => app.push_toast(
                    "Proposal rejected".to_string(),
                    ToastLevel::Info,
                ),
                Err(err) => app.push_toast(
                    format!("Reject failed: {err}"),
                    ToastLevel::Error,
                ),
            }
        }
        ToolCardAction::OpenTarget { rel_path } => {
            // Reuse the buffer-open path the file tree uses.
            crate::editor_pane::open_file(app, &rel_path, /*sticky=*/ true);
        }
    }
}

/// Action requested from a tool card's review buttons. Bubbles up to the
/// caller so accept/reject can run on the AppState (where `staging` lives)
/// rather than through borrow gymnastics here.
#[derive(Debug, Clone)]
pub enum ToolCardAction {
    AcceptStaging { proposal_id: String },
    RejectStaging { proposal_id: String },
    OpenTarget { rel_path: String },
}

fn render_tool_card(
    ui: &mut egui::Ui,
    tool: &crate::chat::state::ToolCard,
) -> Option<ToolCardAction> {
    let mut action: Option<ToolCardAction> = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(crate::icons::wrench().tint(theme::warn()));
        ui.label(
            egui::RichText::new("Tool")
                .color(theme::warn())
                .small()
                .strong(),
        );
        let name_resp = ui.label(
            egui::RichText::new(&tool.tool_name)
                .monospace()
                .small(),
        );
        // Click the tool name (or the target-path label below) to open
        // the touched note. Mirrors `TouchedNoteRouting` in
        // `ui/src/chat/toolCard.ts:23-41`.
        if let Some(path) = &tool.target_path
            && name_resp.interact(egui::Sense::click()).clicked()
        {
            action = Some(ToolCardAction::OpenTarget {
                rel_path: path.clone(),
            });
        }
        if tool.result.is_none() {
            ui.label(
                egui::RichText::new("running…")
                    .color(theme::muted())
                    .italics()
                    .small(),
            );
        } else if !tool.ok {
            ui.label(
                egui::RichText::new("failed")
                    .color(egui::Color32::RED)
                    .small()
                    .strong(),
            );
        } else {
            ui.label(
                egui::RichText::new("ok")
                    .color(egui::Color32::from_rgb(0x1f, 0x70, 0x4c))
                    .small()
                    .strong(),
            );
        }
    });
    // Tool-call collapsible (`chat-panel-tool-call-collapsible`). The
    // body (args + result + action row) is hidden behind a disclosure
    // so a long transcript doesn't drown in JSON. Default open while
    // the call is in flight so the user sees args as they stream; once
    // a result lands the user can click to collapse.
    let id_salt = format!("tool-card-{}-{}", tool.tool_name.as_str(), tool.args.len());
    egui::CollapsingHeader::new(
        egui::RichText::new("details").small().color(theme::muted()),
    )
    .id_salt(id_salt)
    .default_open(true)
    .show(ui, |ui| {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(0xff, 0xf7, 0xe6))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xe0, 0xc4, 0x70)))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width() - 12.0);
            if !tool.args.is_empty() {
                ui.label(
                    egui::RichText::new("args")
                        .color(theme::muted())
                        .small(),
                );
                ui.add(
                    egui::Label::new(egui::RichText::new(&tool.args).monospace().small())
                        .wrap(),
                );
            }
            if let Some(result) = &tool.result {
                if !tool.args.is_empty() {
                    ui.add_space(4.0);
                }
                ui.label(
                    egui::RichText::new("result")
                        .color(theme::muted())
                        .small(),
                );
                let snippet: String = result.chars().take(800).collect();
                ui.add(
                    egui::Label::new(egui::RichText::new(snippet).monospace().small())
                        .wrap(),
                );
            }
            // Review-mode affordance: when the tool returned a staged
            // proposal, surface inline Accept / Reject buttons paired with
            // each staging id. Matches `ui/src/chat/toolCard.ts`'s action
            // row layout.
            if !tool.staging_ids.is_empty() {
                ui.add_space(6.0);
                ui.separator();
                for sid in &tool.staging_ids {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "proposal {}",
                                short_id(sid)
                            ))
                            .color(theme::muted())
                            .small()
                            .monospace(),
                        );
                        if ui
                            .add(
                                egui::Button::image_and_text(
                                    crate::icons::primary_check(),
                                    egui::RichText::new("Accept")
                                        .color(egui::Color32::WHITE)
                                        .small(),
                                )
                                .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                            )
                            .clicked()
                        {
                            action = Some(ToolCardAction::AcceptStaging {
                                proposal_id: sid.clone(),
                            });
                        }
                        if ui
                            .add(
                                egui::Button::image_and_text(
                                    crate::icons::primary_cross(),
                                    egui::RichText::new("Reject")
                                        .color(egui::Color32::WHITE)
                                        .small(),
                                )
                                .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                            )
                            .clicked()
                        {
                            action = Some(ToolCardAction::RejectStaging {
                                proposal_id: sid.clone(),
                            });
                        }
                    });
                }
            }
        });
    });
    action
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Render a single transcript turn. Returns `Some(target)` when the user
/// clicked a `[[wikilink]]` inside the bubble — the caller hands that off
/// to the file-open routing (`chat-panel-note-link-render`).
fn render_turn(ui: &mut egui::Ui, role: ChatRole, text: &str) -> Option<String> {
    let mut clicked_link: Option<String> = None;
    let (label, color) = match role {
        ChatRole::User => ("You", theme::accent()),
        ChatRole::Assistant => ("Assistant", egui::Color32::from_rgb(0x1f, 0x70, 0x4c)),
        ChatRole::Tool => ("Tool", theme::warn()),
    };
    let user = matches!(role, ChatRole::User);
    let outer_layout = if user {
        egui::Layout::top_down(egui::Align::RIGHT)
    } else {
        egui::Layout::top_down(egui::Align::LEFT)
    };
    ui.add_space(6.0);
    ui.with_layout(outer_layout, |ui| {
        ui.label(
            egui::RichText::new(label)
                .color(color)
                .small()
                .strong(),
        );
        let bubble_width = (ui.available_width() * 0.85).max(120.0);
        egui::Frame::default()
            .fill(match role {
                ChatRole::User => theme::hover_bg(),
                ChatRole::Assistant => egui::Color32::from_rgb(0xff, 0xff, 0xff),
                ChatRole::Tool => egui::Color32::from_rgb(0xff, 0xf7, 0xe6),
            })
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_max_width(bubble_width);
                for chunk in split_code_fences(text) {
                    match chunk {
                        Chunk::Text(t) => {
                            ui.horizontal_wrapped(|ui| {
                                for part in split_wikilinks(t) {
                                    match part {
                                        TextPart::Plain(s) if !s.is_empty() => {
                                            ui.label(egui::RichText::new(s));
                                        }
                                        TextPart::Plain(_) => {}
                                        TextPart::Link(target) => {
                                            if ui
                                                .add(
                                                    egui::Label::new(
                                                        egui::RichText::new(format!(
                                                            "[[{}]]",
                                                            target
                                                        ))
                                                        .color(theme::accent())
                                                        .underline(),
                                                    )
                                                    .sense(egui::Sense::click()),
                                                )
                                                .clicked()
                                            {
                                                clicked_link = Some(target.to_string());
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Chunk::Code(t) => {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(t)
                                        .monospace()
                                        .background_color(egui::Color32::from_rgb(
                                            0xee, 0xf1, 0xf5,
                                        )),
                                )
                                .wrap(),
                            );
                        }
                    }
                }
            });
    });
    clicked_link
}

enum Chunk<'a> {
    Text(&'a str),
    Code(&'a str),
}

enum TextPart<'a> {
    Plain(&'a str),
    /// Inner target, e.g. `Some Note` for `[[Some Note]]` (drops any `|alias`).
    Link(&'a str),
}

/// Split a plain-text chunk on `[[wikilink]]` markers. The link target is
/// the substring before any pipe (matching the `wikilink_target_aliases`
/// resolution rules). Lone `[` or unterminated `[[` is preserved as text.
fn split_wikilinks(s: &str) -> Vec<TextPart<'_>> {
    let mut out: Vec<TextPart<'_>> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    let mut last = 0usize;
    while i + 3 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let inner_start = i + 2;
            let mut j = inner_start;
            while j + 1 < bytes.len()
                && !(bytes[j] == b']' && bytes[j + 1] == b']')
                && bytes[j] != b'\n'
            {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b']' && bytes[j + 1] == b']' {
                if last < i {
                    out.push(TextPart::Plain(&s[last..i]));
                }
                let inner = &s[inner_start..j];
                let target = inner.split('|').next().unwrap_or(inner).trim();
                out.push(TextPart::Link(target));
                i = j + 2;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    if last < s.len() {
        out.push(TextPart::Plain(&s[last..]));
    }
    out
}

/// Very small markdown helper: split on triple-backtick fences so code
/// blocks pick up monospace styling without a full parser. Inline
/// styling (bold/italic/links) is left for the real markdown
/// renderer.
fn split_code_fences(s: &str) -> Vec<Chunk<'_>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("```") {
        if start > 0 {
            out.push(Chunk::Text(&rest[..start]));
        }
        let after = &rest[start + 3..];
        let nl = after.find('\n').unwrap_or(after.len());
        let body_start = nl + 1;
        let body = &after[body_start.min(after.len())..];
        if let Some(end) = body.find("```") {
            out.push(Chunk::Code(body[..end].trim_end_matches('\n')));
            rest = &body[end + 3..];
        } else {
            out.push(Chunk::Code(body));
            rest = "";
        }
    }
    if !rest.is_empty() {
        out.push(Chunk::Text(rest));
    }
    out
}

#[cfg(test)]
mod at_mention_tests {
    use super::*;

    #[test]
    fn detects_bare_at() {
        assert_eq!(active_at_mention("@"), Some((0, "".to_string())));
    }

    #[test]
    fn detects_at_with_query() {
        assert_eq!(active_at_mention("@notes/f"), Some((0, "notes/f".to_string())));
    }

    #[test]
    fn detects_at_after_whitespace() {
        assert_eq!(
            active_at_mention("hi there @notes"),
            Some((9, "notes".to_string()))
        );
    }

    #[test]
    fn skips_email_like_at() {
        // `@` not preceded by whitespace shouldn't trigger.
        assert_eq!(active_at_mention("name@example"), None);
    }

    #[test]
    fn no_match_when_whitespace_after_at() {
        // Trailing whitespace breaks the in-flight token.
        assert_eq!(active_at_mention("@notes hello "), None);
    }
}

/// If the cursor-trailing token in `text` looks like a partial `@<query>`
/// mention (no whitespace between `@` and the end), return the byte
/// offset of the `@` plus the captured query. Otherwise `None`.
///
/// This is a deliberately cheap pre-check so the heavier suggestion fetch
/// only runs when an `@` is genuinely in flight.
/// Pull the active buffer's current selection out as a String. Returns
/// `None` when no buffer is focused, when the focused tab isn't a buffer,
/// or when the selection is empty (caret with no range).
fn active_buffer_selection(app: &AppState) -> Option<String> {
    let path = app
        .session.active_tab
        .and_then(|id| app.tab_by_id(id))
        .and_then(|t| t.buffer_path().map(str::to_string))?;
    let buffer = app.session.buffers.get(&path)?;
    let range = buffer.editor.selection.main().range();
    if range.is_empty() {
        return None;
    }
    let len = buffer.editor.doc.len_bytes();
    let start = range.start.min(len);
    let end = range.end.min(len);
    Some(buffer.editor.doc.slice(start..end).to_string())
}

fn active_at_mention(text: &str) -> Option<(usize, String)> {
    // Walk back from the end finding the last `@` not preceded by an
    // alphanumeric — bail out on whitespace before that.
    let bytes = text.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        let c = bytes[i - 1] as char;
        if c.is_whitespace() {
            return None;
        }
        if c == '@' {
            // `@` must be at the start of the input or follow whitespace
            // so we don't catch email-like substrings.
            let prev_is_ws = i == 1 || (bytes[i - 2] as char).is_whitespace();
            if !prev_is_ws {
                return None;
            }
            let query = text[i..].to_string();
            return Some((i - 1, query));
        }
        i -= 1;
    }
    None
}

/// Walk the vault root for `.md` paths whose basename (or full path)
/// matches `query` case-insensitively, returning up to `cap` results.
/// Cheap O(n) scan — good enough for the default vault size; future
/// work routes this through the indexer's lexical engine.
/// Path list cache for `@`-mention autocomplete (`chat-at-autocomplete`).
/// Walking the vault per frame on every keystroke is the legacy perf
/// regression the original port audit called out; we instead pull the
/// snapshot from the indexer's read store when available (cheap SQL
/// query), and fall back to `Vault::walk_indexable_files` only when the
/// store is offline. The result is cached in egui memory for ~2 seconds
/// so successive keystrokes share one fetch.
// TODO: wire into the @-mention completion path; kept around because the
// store-backed cache will replace the current vault-walk fallback.
#[allow(dead_code)]
#[derive(Clone)]
struct MentionPaths {
    paths: Vec<String>,
    built_at: std::time::Instant,
}

#[allow(dead_code)]
const MENTION_CACHE_TTL_MS: u128 = 2000;

#[allow(dead_code)]
fn mention_suggestions_ctx(
    ctx: &egui::Context,
    app: &AppState,
    query: &str,
    cap: usize,
) -> Vec<String> {
    let mem_id = egui::Id::new("chat-mention-paths");
    let cache: Option<MentionPaths> = ctx.data(|d| d.get_temp(mem_id));
    let fresh = cache
        .as_ref()
        .map(|c| c.built_at.elapsed().as_millis() < MENTION_CACHE_TTL_MS)
        .unwrap_or(false);
    let paths = if fresh {
        cache.unwrap().paths
    } else {
        let mut out: Vec<String> = Vec::new();
        if let Ok(store) = app.vault_session.services.read_store.lock()
            && let Ok(rows) = store.all_note_paths()
        {
            out = rows;
        }
        if out.is_empty()
            && let Ok(rows) = app.vault_session.vault.walk_indexable_files("")
        {
            out = rows;
        }
        ctx.data_mut(|d| {
            d.insert_temp(
                mem_id,
                MentionPaths {
                    paths: out.clone(),
                    built_at: std::time::Instant::now(),
                },
            );
        });
        out
    };
    let q = query.to_lowercase();
    paths
        .into_iter()
        .filter(|p| q.is_empty() || p.to_lowercase().contains(&q))
        .take(cap)
        .collect()
}

fn mention_suggestions(app: &AppState, query: &str, cap: usize) -> Vec<String> {
    let q = query.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    // Prefer the indexer's path list when it's online — no disk walk.
    if let Ok(store) = app.vault_session.services.read_store.lock()
        && let Ok(rows) = store.all_note_paths()
    {
        for rel in rows {
            if q.is_empty() || rel.to_lowercase().contains(&q) {
                out.push(rel);
                if out.len() >= cap {
                    return out;
                }
            }
        }
        return out;
    }
    // Fallback: vault walk. Same shape as before, kept for the
    // unindexed-vault case.
    let root = app.vault_session.vault.root();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !matches!(ext.to_ascii_lowercase().as_str(), "md" | "txt") {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if q.is_empty() || rel_str.to_lowercase().contains(&q) {
            out.push(rel_str);
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

fn composer(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
    multiline: bool,
) {
    let active_id = app.session.chat.active.clone().unwrap_or_else(|| "_none".to_string());
    // Clone draft into a local so we can hand &mut to the TextEdit and
    // still read app immutably below for @-mention suggestions. The clone
    // is cheap (draft is typically a short prompt) and we write the
    // updated value back at the end of the function.
    let mut draft: String = app
        .session.chat
        .drafts
        .get(&active_id)
        .cloned()
        .unwrap_or_default();

    let mut send_now = false;
    ui.horizontal(|ui| {
        // Reserve room for the Send/Stop button (and optional "selection"
        // button) on the right; clamp so the text box keeps a usable
        // minimum width instead of going negative and shoving the
        // buttons off-screen when the panel is narrow.
        let edit = if multiline {
            let w = (ui.available_width() - 80.0).max(60.0);
            egui::TextEdit::multiline(&mut draft)
                .hint_text("Message the assistant…  (use @ to mention a note)")
                .desired_rows(3)
                .desired_width(w)
        } else {
            let w = (ui.available_width() - 60.0).max(40.0);
            egui::TextEdit::singleline(&mut draft)
                .hint_text("Message…")
                .desired_width(w)
        };
        let resp = ui.add(edit);

        // Cmd-Enter / Ctrl-Enter to send from the editor. Single-line
        // also accepts plain Enter.
        if resp.has_focus() {
            let cmd_enter = ui.input(|i| {
                i.key_pressed(egui::Key::Enter) && (i.modifiers.command || i.modifiers.ctrl)
            });
            let plain_enter_singleline = !multiline
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if cmd_enter || plain_enter_singleline {
                send_now = true;
            }
        }
        // Pending: show Stop instead of Send. Clicking it signals the
        // in-flight turn to halt via the per-session StopSignal stored
        // in `ChatRegistry::stop_signals` (`chat-panel-stop-button`).
        let pending = app
            .session.chat
            .sessions
            .get(&active_id)
            .map(|s| s.pending)
            .unwrap_or(false);
        if pending {
            if ui.button("Stop").on_hover_text("Halt this turn").clicked()
                && let Some(sig) = app.session.chat.stop_signals.get(&active_id)
            {
                sig.user_halt();
            }
        } else if ui.button("Send").on_hover_text("Cmd-Enter").clicked() {
            send_now = true;
        }
        // "Quote selection" (`chat-input-at-selection`): pull the active
        // buffer's current text selection into the draft as a fenced
        // quote so the assistant sees exactly what the user is looking
        // at. Disabled when there's no selection or no active buffer.
        let selection_text = active_buffer_selection(app);
        let has_selection = selection_text
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_selection
            && ui
                .add(egui::Button::image_and_text(crate::icons::plus(), "selection"))
                .on_hover_text("Insert the editor's current selection as quoted context")
                .clicked()
            && let Some(sel) = selection_text
        {
            let suffix = if draft.is_empty() || draft.ends_with("\n\n") {
                ""
            } else if draft.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            draft.push_str(suffix);
            draft.push_str("```\n");
            draft.push_str(&sel);
            if !sel.ends_with('\n') {
                draft.push('\n');
            }
            draft.push_str("```\n");
        }

        // @-mention autocomplete: when the user types `@<query>` at the
        // tail of the draft, surface vault notes whose path matches the
        // query so they can be referenced in the prompt (`llm.md:247-268`
        // @-mentions). Picking a suggestion replaces the partial token
        // with `@path/to/note`.
        if resp.has_focus()
            && let Some((prefix_start, query)) = active_at_mention(&draft)
        {
            let suggestions = mention_suggestions(app, &query, 8);
            if !suggestions.is_empty() {
                egui::Popup::menu(&resp).show(|ui| {
                    for s in &suggestions {
                        if ui.button(s).clicked() {
                            // Replace the in-progress `@<query>` token
                            // with `@<full-path> ` so the user can
                            // continue typing immediately after.
                            let new_draft = format!(
                                "{}@{} {}",
                                &draft[..prefix_start],
                                s,
                                &draft[prefix_start + 1 + query.len()..],
                            );
                            draft = new_draft;
                            ui.close();
                        }
                    }
                });
            }
        }
    });
    // Sync back the (possibly edited) draft.
    app.session.chat.drafts.insert(active_id.clone(), draft);

    if send_now {
        let mut text = std::mem::take(app.session.chat.drafts.entry(active_id).or_default());
        if !text.trim().is_empty() {
            // Active-note context injection (`chat-active-note-context-injection`):
            // when a buffer tab is focused and the user hasn't already
            // dropped an `@` reference into the draft, prepend a context
            // block naming the active note. Keeps the assistant aware of
            // what the user is looking at without a manual mention.
            if !text.contains('@')
                && let Some(rel) = app
                    .session.active_tab
                    .and_then(|id| app.tab_by_id(id))
                    .and_then(|t| t.buffer_path().map(str::to_string))
            {
                text = format!(
                    "[active note: {}]\n\n{}",
                    rel, text
                );
            }
            let vault_root = app.vault_session.vault_root.clone();
            let config = app.vault_session.config.clone();
            let mcp_handler = app.vault_session.services.mcp.as_ref().map(|h| h.agent_handler());
            crate::chat::send::send_message(
                &mut app.session.chat,
                &vault_root,
                rt,
                config,
                mcp_handler,
                text,
            );
        }
    }
}
