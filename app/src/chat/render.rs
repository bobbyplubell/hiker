//! Chat panel renderer. One entry point (`show`) used by both the
//! full-tab `panels::agent` view and the docked region at the bottom
//! of the discovery panel. A `Layout` enum picks the framing — the
//! tab variant gets a session picker header strip + larger transcript
//! area; the docked variant collapses the picker to a single
//! current-session label + new-button.

use std::sync::Arc;

use eframe::egui;

use crate::chat::session;
use crate::chat::state::{ChatRegistry, ChatRole};
use crate::state::AppState;
use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Full-tab Agent view. Picker header + large transcript +
    /// composer.
    FullTab,
    /// Side-bar variant. Same as FullTab but the +/trash buttons live
    /// in the side bar's title row, so the in-body header just shows
    /// the picker.
    SideBar,
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
    Chat { app, rt }.show(ui, session_id, layout);
}

/// Per-frame chat render context. Bundles the mutable `AppState` borrow
/// with the tokio runtime handle so the render/action helpers can be
/// `&mut self` methods on a single receiver.
struct Chat<'a> {
    app: &'a mut AppState,
    rt: &'a Arc<tokio::runtime::Runtime>,
}

impl Chat<'_> {
    fn show(&mut self, ui: &mut egui::Ui, session_id: Option<&str>, layout: Layout) {
    // 1) Fold in any pending reply-task events from this frame.
    self.app.session.chat.pump_events();

    // 2) Override active pointer if the tab specified one. Lazy-load
    //    historic sessions on first view in case discovery didn't run
    //    yet (e.g. tab restored from a saved layout).
    if let Some(id) = session_id
        && self.app.session.chat.sessions.contains_key(id)
    {
        self.app.session.chat.active = Some(id.to_string());
    }

    match layout {
        Layout::FullTab => self.render_full_tab(ui, /*show_header=*/ true, /*show_header_buttons=*/ true),
        // SideBar variant has its picker + buttons hoisted into the
        // side bar's chrome title row (see `session_picker` +
        // `secondary_side_bar_action_buttons`), so the body skips the
        // header strip entirely.
        Layout::SideBar => self.render_full_tab(ui, /*show_header=*/ false, /*show_header_buttons=*/ false),
        Layout::Docked => self.render_docked(ui),
    }
    }

    fn render_full_tab(
        &mut self,
        ui: &mut egui::Ui,
        show_header: bool,
        show_header_buttons: bool,
    ) {
    if show_header {
        self.header(ui, /*full=*/ true, show_header_buttons);
        ui.separator();
    }

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
        self.transcript(&mut transcript_ui);
    }

    ui.separator();
    self.composer(ui, /*multiline=*/ true);
    }

    fn render_docked(&mut self, ui: &mut egui::Ui) {
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(6.0)
        .show(ui, |ui| {
            self.header(ui, /*full=*/ false, /*show_header_buttons=*/ true);
            ui.add_space(2.0);
            egui::ScrollArea::vertical()
                .max_height(180.0)
                .auto_shrink([false, false])
                .show(ui, |ui| self.transcript(ui));
            ui.add_space(2.0);
            self.composer(ui, /*multiline=*/ false);
        });
    }

    fn header(&mut self, ui: &mut egui::Ui, full: bool, show_buttons: bool) {
    let app = &mut *self.app;
    // Layout right-to-left so the +/trash buttons stay pinned to the
    // right edge and the picker shrinks to fill the leftover slot.
    // A left-to-right horizontal with a fixed-width ComboBox + trailing
    // buttons would inflate the side bar's min content width and shove
    // the buttons off-screen when the user resizes narrower.
    let active_id = app.session.chat.active.clone();
    let active_label = active_label_for(&app.session.chat, active_id.as_deref());
    let mut switch_to: Option<String> = None;
    let mut delete: Option<String> = None;
    let mut create_new = false;

    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if show_buttons {
                if active_id.is_some()
                    && ui
                        .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Trash)).small())
                        .on_hover_text("Delete this session")
                        .clicked()
                {
                    delete = active_id.clone();
                }
                if ui
                    .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Plus)).small())
                    .on_hover_text("New session")
                    .clicked()
                {
                    create_new = true;
                }
            }

            let cap = if full { 280.0 } else { 180.0 };
            let picker_width = ui.available_width().min(cap).max(0.0);
            egui::ComboBox::from_id_salt(if full { "chat_picker_full" } else { "chat_picker_docked" })
                .selected_text(active_label)
                .width(picker_width)
                .show_ui(ui, |ui| {
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
        });
    });

    // Apply picker actions (outside the layout closure so the borrows
    // on `app` don't conflict with the combo's render).
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
    }
}

impl AppState {
    /// Standalone session picker — the same combobox `Chat::header` builds,
    /// but rendered into an arbitrary ui so it can sit in the secondary
    /// side bar's chrome title row alongside the +/trash buttons that
    /// `secondary_side_bar_action_buttons` already places there.
    pub fn chat_session_picker(&mut self, ui: &mut egui::Ui) {
        let active_id = self.session.chat.active.clone();
        let active_label = active_label_for(&self.session.chat, active_id.as_deref());
        let mut switch_to: Option<String> = None;

        let picker_width = ui.available_width().min(280.0).max(0.0);
        egui::ComboBox::from_id_salt("chat_picker_sidebar_title")
            .selected_text(active_label)
            .width(picker_width)
            .show_ui(ui, |ui| {
                let mut rows: Vec<(String, String, i64)> = self
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

        if let Some(id) = switch_to {
            session::set_active(&mut self.session.chat, &id);
        }
    }
}

fn active_label_for(reg: &ChatRegistry, id: Option<&str>) -> String {
    match id.and_then(|i| reg.sessions.get(i)) {
        Some(s) => s.preview.clone(),
        None => "(no active session)".to_string(),
    }
}

impl Chat<'_> {
    fn transcript(&mut self, ui: &mut egui::Ui) {
    let Some(s) = self.app.session.chat.active_session() else {
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
    // Map of vault-relative path → its live op-log pending ops this frame
    // (op id + drift). A write-tool card resolves its review buttons by
    // looking up its `target_path` here. When the user accepts or rejects an
    // op elsewhere (inline patch-review surface, the bulk patch-review tab,
    // the activity widget) it drops out of the op-log pending queue and this
    // map shrinks, so the stale buttons on old tool cards vanish on the next
    // frame without any per-card bookkeeping.
    let mut live_ops_by_path: std::collections::HashMap<String, Vec<LiveOp>> =
        std::collections::HashMap::new();
    for p in &self.app.ui_cache.pending_snapshot {
        live_ops_by_path
            .entry(p.target_path.clone())
            .or_default()
            .push(LiveOp { op_id: p.op_id.clone(), drifted: p.drifted });
    }
    let mut card_action: Option<ToolCardAction> = None;
    let mut link_clicked: Option<String> = None;
    // Persistent read-only editor instances backing tool-card markdown
    // previews. Borrowed mutably here for the whole transcript render;
    // `turns` was cloned above so there's no aliasing with the session
    // list this lives alongside on the registry.
    let previews = &mut self.app.session.chat.md_previews;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        // Only pin to the bottom while a reply is streaming in. Pinning
        // unconditionally yanked the viewport to the bottom whenever a
        // card was expanded/collapsed (the content height changed), which
        // read as the scroll position "jumping" during idle review.
        .stick_to_bottom(pending)
        .show(ui, |ui| {
            for (turn_idx, turn) in turns.iter().enumerate() {
                if let Some(tool) = &turn.tool {
                    if let Some(a) =
                        tool.render_card(ui, &session_id, turn_idx, &live_ops_by_path, previews)
                    {
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
            // While the agent is working we want streaming text + tool
            // events to surface within ~one tick rather than waiting on
            // the 750ms idle cadence in main.rs. The reply task posts
            // ChatEvents into the mpsc, which pump_events drains at
            // frame start — without a fast repaint kick the events
            // sit in the channel until the next idle paint.
            if pending {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(80));
            }
        });
    // Apply tool-card actions on the AppState. Accept/reject flip the op-log
    // pending op via `op_writes::flip_op_status`; open-target hands off to the
    // tab-open machinery via `editor_pane::open_file`.
    if let Some(action) = card_action {
        self.apply_tool_card_action(action);
    }
    if let Some(target) = link_clicked {
        let rel = self.resolve_wikilink_target(&target);
        crate::editor_pane::open_file(self.app, &rel, /*sticky=*/ true);
    }
    }

    /// Resolve a wikilink target like `Some Note` or `notes/foo` into a
    /// vault-relative path. Falls back to the literal `target.md` when no
    /// indexed match exists.
    fn resolve_wikilink_target(&self, target: &str) -> String {
    let app = &*self.app;
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

    fn apply_tool_card_action(&mut self, action: ToolCardAction) {
    let app = &mut *self.app;
    use crate::state::ToastLevel;
    match action {
        ToolCardAction::AcceptOp { op_id, target_path } => {
            match hiker_core::ops::op_writes::flip_op_status(
                &app.vault_session.services.oplog,
                &target_path,
                &[op_id],
                /* accept */ true,
            ) {
                Ok(()) => app.push_toast(
                    format!("Accepted proposal for {target_path}"),
                    ToastLevel::Info,
                ),
                Err(err) => app.push_toast(
                    format!("Accept failed: {err}"),
                    ToastLevel::Error,
                ),
            }
        }
        ToolCardAction::RejectOp { op_id, target_path } => {
            match hiker_core::ops::op_writes::flip_op_status(
                &app.vault_session.services.oplog,
                &target_path,
                &[op_id],
                /* accept */ false,
            ) {
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
}

/// One live op-log pending op for a tool card's target path: the op id the
/// review buttons flip via `op_writes::flip_op_status`, plus its drift status
/// (Accept disabled for drifted ops per `patch-review-conflicted-accept-disabled`).
#[derive(Debug, Clone)]
pub struct LiveOp {
    pub op_id: String,
    pub drifted: bool,
}

/// Action requested from a tool card's review buttons. Bubbles up to the
/// caller so accept/reject can run on the AppState (where the op log lives)
/// rather than through borrow gymnastics here. Carries the resolved
/// `target_path` so `flip_op_status` can resolve the op's doc id.
#[derive(Debug, Clone)]
pub enum ToolCardAction {
    AcceptOp { op_id: String, target_path: String },
    RejectOp { op_id: String, target_path: String },
    OpenTarget { rel_path: String },
}

/// Cap on the height of an embedded markdown preview inside a tool card.
/// Past this the read-only editor scrolls internally so one huge note body
/// can't dominate the transcript.
const MD_MAX_H: f32 = 360.0;

/// Object keys whose string value is rendered as markdown rather than an
/// inline `key: value` row — the note bodies / patch text that tools hand
/// back. Anything multi-line or long is treated the same way regardless of
/// key (see [`is_mdish`]).
const MD_KEYS: &[&str] = &[
    "content", "text", "body", "new_string", "old_string", "new_content",
    "old_content", "new_text", "old_text", "markdown", "note", "message",
    "summary", "snippet", "diff", "patch",
];

/// Whether a string field should render as a markdown block instead of a
/// compact `key: value` row.
fn is_mdish(key: &str, val: &str) -> bool {
    MD_KEYS.contains(&key) || val.contains('\n') || val.chars().count() > 120
}

/// Render a tool `args` / `result` payload in structured form: parse JSON
/// and lay it out as `key: value` rows, markdown blocks for content-ish
/// string fields, and pretty-printed sub-blocks for nested objects/arrays.
/// Non-JSON payloads render as a single markdown block.
fn render_payload(
    ui: &mut egui::Ui,
    id: &str,
    payload: &str,
    previews: &mut crate::chat::md_preview::Cache,
) {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(serde_json::Value::Object(map)) => {
            for (k, v) in &map {
                render_field(ui, &format!("{id}:{k}"), k, v, previews);
            }
        }
        // Bare string payload → markdown. Other bare scalars / arrays →
        // pretty JSON.
        Ok(serde_json::Value::String(s)) => {
            crate::chat::md_preview::render(ui, id, &s, MD_MAX_H, previews);
        }
        Ok(other) => json_block(
            ui,
            &serde_json::to_string_pretty(&other).unwrap_or_else(|_| payload.to_string()),
        ),
        // Not JSON at all — most text-returning tools hand back a bare
        // string, which reads best as markdown.
        Err(_) => crate::chat::md_preview::render(ui, id, payload, MD_MAX_H, previews),
    }
}

/// Render one object field. Content-ish strings become markdown blocks;
/// short scalars become inline rows; nested values become pretty JSON.
fn render_field(
    ui: &mut egui::Ui,
    id: &str,
    key: &str,
    val: &serde_json::Value,
    previews: &mut crate::chat::md_preview::Cache,
) {
    match val {
        serde_json::Value::String(s) if is_mdish(key, s) => {
            ui.label(
                egui::RichText::new(key)
                    .color(theme::muted())
                    .monospace()
                    .small(),
            );
            crate::chat::md_preview::render(ui, id, s, MD_MAX_H, previews);
        }
        serde_json::Value::String(s) => kv_row(ui, key, s),
        serde_json::Value::Null => kv_row(ui, key, "null"),
        serde_json::Value::Bool(b) => kv_row(ui, key, &b.to_string()),
        serde_json::Value::Number(n) => kv_row(ui, key, &n.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            ui.label(
                egui::RichText::new(key)
                    .color(theme::muted())
                    .monospace()
                    .small(),
            );
            json_block(ui, &serde_json::to_string_pretty(val).unwrap_or_default());
        }
    }
}

/// Compact `key: value` row for a scalar field.
fn kv_row(ui: &mut egui::Ui, key: &str, val: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(
            egui::RichText::new(format!("{key}:"))
                .color(theme::accent())
                .monospace()
                .small(),
        );
        ui.label(egui::RichText::new(val).monospace().small());
    });
}

/// Pretty-printed JSON sub-block (nested objects / arrays) with a faint
/// code background — same treatment as a fenced code block in chat bubbles.
fn json_block(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .monospace()
                .small()
                .background_color(egui::Color32::from_rgb(0xee, 0xf1, 0xf5)),
        )
        .wrap(),
    );
}

/// Verbatim payload rendering for the per-card "raw" toggle.
fn raw_payload(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(egui::RichText::new(text).monospace()).wrap());
}

/// Review-mode affordance: when this card wrote (the tool result reported
/// `written` / `staged`), surface inline Accept / Reject buttons for the
/// op-log pending ops on the card's target path. Matches
/// `ui/src/chat/toolCard.ts`'s action row layout. The ops are resolved from
/// the live op-log snapshot keyed by path — when the user accepts/rejects
/// elsewhere (inline patch-review surface, bulk patch-review tab, activity
/// widget) the op drops out of the pending queue and the buttons vanish on
/// the next frame. Returns the action the user clicked, if any.
fn render_op_review(
    ui: &mut egui::Ui,
    tool: &crate::chat::state::ToolCard,
    live_ops_by_path: &std::collections::HashMap<String, Vec<LiveOp>>,
) -> Option<ToolCardAction> {
    let mut action: Option<ToolCardAction> = None;
    let live_ops: &[LiveOp] = tool
        .target_path
        .as_ref()
        .filter(|_| tool.produced_write)
        .and_then(|p| live_ops_by_path.get(p))
        .map_or(&[][..], Vec::as_slice);
    if !live_ops.is_empty() {
        let target = tool.target_path.clone().unwrap_or_default();
        ui.add_space(6.0);
        ui.separator();
        for op in live_ops {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "proposal {}",
                        &op.op_id[..op.op_id.len().min(8)]
                    ))
                    .color(theme::muted())
                    .small()
                    .monospace(),
                );
                // Accept is disabled for drifted ops — the anchor no
                // longer resolves against current accepted state
                // (`patch-review-conflicted-accept-disabled`). Reject
                // stays active.
                let accept = ui.add_enabled(
                    !op.drifted,
                    egui::Button::image_and_text(
                        crate::icons::ICONS.primary_check(),
                        egui::RichText::new("Accept").color(egui::Color32::WHITE).small(),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                );
                if op.drifted {
                    accept.on_hover_text("Drifted: the note changed since this edit was proposed");
                } else if accept.clicked() {
                    action = Some(ToolCardAction::AcceptOp {
                        op_id: op.op_id.clone(),
                        target_path: target.clone(),
                    });
                }
                if ui
                    .add(
                        egui::Button::image_and_text(
                            crate::icons::ICONS.primary_cross(),
                            egui::RichText::new("Reject").color(egui::Color32::WHITE).small(),
                        )
                        .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                    )
                    .clicked()
                {
                    action = Some(ToolCardAction::RejectOp {
                        op_id: op.op_id.clone(),
                        target_path: target.clone(),
                    });
                }
            });
        }
    } else if tool.produced_write {
        // The card wrote, but the op is no longer pending (accepted /
        // rejected). Leave a muted breadcrumb so the user knows the card
        // *did* propose an edit — just not waiting on input now.
        ui.add_space(6.0);
        ui.separator();
        ui.label(
            egui::RichText::new("proposal resolved")
                .color(theme::muted())
                .italics()
                .small(),
        );
    }
    action
}

impl crate::chat::state::ToolCard {
    fn render_card(
    &self,
    ui: &mut egui::Ui,
    session_id: &str,
    turn_idx: usize,
    live_ops_by_path: &std::collections::HashMap<String, Vec<LiveOp>>,
    previews: &mut crate::chat::md_preview::Cache,
) -> Option<ToolCardAction> {
    let tool = self;
    let mut action: Option<ToolCardAction> = None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(crate::icons::ICONS.image(crate::icons::Icon::Wrench).tint(theme::warn()));
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
    // id_salt must be stable across frames AND unique across cards. The
    // turn index makes it unique even when two tool calls of the same name
    // and same arg-string land in the transcript; tool_name is kept for
    // grep-ability in egui debug overlays.
    let id_salt = format!("{}-tool-card-{}-{}", session_id, turn_idx, tool.tool_name.as_str());
    // Manual collapsible — `CollapsingHeader` only treats the chevron +
    // label glyph as the toggle hit-region, which is a tiny click target.
    // We render a full-width clickable header strip as the toggle. The
    // body itself is deliberately NOT click-to-collapse: a body-level
    // interact sat on top of the raw toggle / inline links and stole
    // their clicks, and collapsing out from under the pointer felt janky.
    let open_id = egui::Id::new(("tool-card-open", &id_salt));
    let mut open = ui
        .ctx()
        .data(|d| d.get_temp::<bool>(open_id))
        .unwrap_or(true);
    // Per-card structured ⇄ raw toggle. Structured (default) pretty-prints
    // JSON into key/value rows and renders content fields as styled
    // markdown; raw shows the verbatim payload string for debugging /
    // copy. Persisted in egui temp data like `open`.
    let raw_id = egui::Id::new(("tool-card-raw", &id_salt));
    let mut raw = ui.ctx().data(|d| d.get_temp::<bool>(raw_id)).unwrap_or(false);
    let header = egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(if open {
                    crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
                } else {
                    crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
                });
                ui.label(egui::RichText::new("details").color(theme::muted()));
                ui.add_space(ui.available_width());
            });
        });
    let header_resp = ui.interact(
        header.response.rect,
        egui::Id::new(("tool-card-toggle-header", &id_salt)),
        egui::Sense::click(),
    );
    if header_resp.clicked() {
        open = !open;
        ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    }
    if open {
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(0xff, 0xf7, 0xe6))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xe0, 0xc4, 0x70)))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width() - 12.0);
            // Structured ⇄ raw switch, right-aligned above the payload. The
            // outer `horizontal` pins this to a single row; the inner
            // right-to-left layout hugs the label to the right edge with a
            // proper margin instead of overflowing past it.
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if raw { "structured" } else { "raw" };
                    let toggle = ui.add(
                        egui::Label::new(
                            egui::RichText::new(label)
                                .color(theme::accent())
                                .underline()
                                .small(),
                        )
                        .sense(egui::Sense::click()),
                    );
                    if toggle.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if toggle.clicked() {
                        raw = !raw;
                        ui.ctx().data_mut(|d| d.insert_temp(raw_id, raw));
                    }
                });
            });
            if !tool.args.is_empty() {
                ui.label(egui::RichText::new("args").color(theme::muted()).small());
                if raw {
                    raw_payload(ui, &tool.args);
                } else {
                    render_payload(ui, &format!("{}:args", id_salt), &tool.args, previews);
                }
            }
            if let Some(result) = &tool.result {
                if !tool.args.is_empty() {
                    ui.add_space(4.0);
                }
                ui.label(egui::RichText::new("result").color(theme::muted()).small());
                if raw {
                    // Verbatim payload inside a scroll shell so multi-KB
                    // tool outputs don't push every other turn out of view.
                    egui::ScrollArea::vertical()
                        .id_salt(format!("{}-result", id_salt))
                        .max_height(400.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| raw_payload(ui, result));
                } else {
                    render_payload(ui, &format!("{}:result", id_salt), result, previews);
                }
            }
            if let Some(a) = render_op_review(ui, tool, live_ops_by_path) {
                action = Some(a);
            }
        });
    }
    action
    }
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
        assert_eq!("@".active_at_mention(), Some((0, "".to_string())));
    }

    #[test]
    fn detects_at_with_query() {
        assert_eq!("@notes/f".active_at_mention(), Some((0, "notes/f".to_string())));
    }

    #[test]
    fn detects_at_after_whitespace() {
        assert_eq!(
            "hi there @notes".active_at_mention(),
            Some((9, "notes".to_string()))
        );
    }

    #[test]
    fn skips_email_like_at() {
        // `@` not preceded by whitespace shouldn't trigger.
        assert_eq!("name@example".active_at_mention(), None);
    }

    #[test]
    fn no_match_when_whitespace_after_at() {
        // Trailing whitespace breaks the in-flight token.
        assert_eq!("@notes hello ".active_at_mention(), None);
    }
}

/// If the cursor-trailing token in `text` looks like a partial `@<query>`
/// mention (no whitespace between `@` and the end), return the byte
/// offset of the `@` plus the captured query. Otherwise `None`.
///
/// This is a deliberately cheap pre-check so the heavier suggestion fetch
/// only runs when an `@` is genuinely in flight.
impl Chat<'_> {
/// Pull the active buffer's current selection out as a String. Returns
/// `None` when no buffer is focused, when the focused tab isn't a buffer,
/// or when the selection is empty (caret with no range).
fn active_buffer_selection(&self) -> Option<String> {
    let app = &*self.app;
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

}

/// Cheap `@<query>` mention scan over the trailing token of a draft.
/// A trait on `str` so the sole render call site and the unit tests share
/// one implementation without a free helper.
trait AtMentionScan {
    /// If the cursor-trailing token looks like a partial `@<query>`
    /// mention (no whitespace between `@` and the end), return the byte
    /// offset of the `@` plus the captured query. Otherwise `None`.
    ///
    /// A deliberately cheap pre-check so the heavier suggestion fetch only
    /// runs when an `@` is genuinely in flight.
    fn active_at_mention(&self) -> Option<(usize, String)>;
}

impl AtMentionScan for str {
    fn active_at_mention(&self) -> Option<(usize, String)> {
        // Walk back from the end finding the last `@` not preceded by an
        // alphanumeric — bail out on whitespace before that.
        let bytes = self.as_bytes();
        let mut i = bytes.len();
        while i > 0 {
            let c = bytes[i - 1] as char;
            if c.is_whitespace() {
                return None;
            }
            if c == '@' {
                // `@` must be at the start of the input or follow
                // whitespace so we don't catch email-like substrings.
                let prev_is_ws = i == 1 || (bytes[i - 2] as char).is_whitespace();
                if !prev_is_ws {
                    return None;
                }
                let query = self[i..].to_string();
                return Some((i - 1, query));
            }
            i -= 1;
        }
        None
    }
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

impl Chat<'_> {
fn composer(
    &mut self,
    ui: &mut egui::Ui,
    multiline: bool,
) {
    // "Quote selection" source — read the active buffer's selection up
    // front so the borrow doesn't overlap the `&mut self.app` reborrow
    // taken for the composer body.
    let selection_text = self.active_buffer_selection();
    let rt = self.rt;
    let app = &mut *self.app;
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
    // Wrap the composer row in a Frame with a small horizontal margin
    // so the TextEdit's focus border doesn't sit at x=0 of the side
    // panel — egui's `SidePanel::resizable(true)` draws a thin drag
    // handle on the inner edge that otherwise overlaps the border
    // (visible as a darker bar clipping the leftmost ~3px).
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(4, 0))
        .show(ui, |ui| {
    // Layout the row right-to-left so the Send/Stop (and optional
    // "selection") buttons are pinned to the right edge; the TextEdit
    // then fills whatever space is left. This keeps the buttons on
    // screen no matter how narrow the panel gets — the previous
    // left-to-right + `desired_width = available - 80` approach forced
    // the TextEdit to a floor that, combined with the trailing buttons,
    // exceeded the side bar's available width and shoved Send off the
    // right edge (and inflated the panel's content min-width).
    ui.horizontal(|ui| {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
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
        let has_selection = selection_text
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if has_selection
            && ui
                .add(egui::Button::image_and_text(crate::icons::ICONS.image(crate::icons::Icon::Plus), "selection"))
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

        // TextEdit fills whatever horizontal space the buttons didn't
        // claim. Using `available_width()` after the buttons are placed
        // (we're in right_to_left, so this is the leftover slot) lets
        // the field shrink with the panel instead of pushing the
        // buttons off-screen. A tiny floor keeps the caret visible.
        let avail_w = ui.available_width().max(20.0);
        let edit = if multiline {
            // Plain Enter = send (handled below). Shift-Enter is the
            // newline shortcut, which we tell the TextEdit to treat as
            // its return key so the multiline buffer still accepts
            // intentional newlines without inserting one on send.
            egui::TextEdit::multiline(&mut draft)
                .hint_text("Message the assistant…  (Enter to send, Shift-Enter for newline, @ to mention a note)")
                .desired_rows(3)
                .desired_width(avail_w)
                .return_key(egui::KeyboardShortcut::new(
                    egui::Modifiers::SHIFT,
                    egui::Key::Enter,
                ))
        } else {
            egui::TextEdit::singleline(&mut draft)
                .hint_text("Message…")
                .desired_width(avail_w)
        };
        let resp = ui.add(edit);
        // Record the composer's id so the editor panel can tell when this
        // field (not the editor) owns keyboard focus and should keep Ctrl-Z.
        app.ui.chat_input_id = Some(resp.id);

        // Send shortcuts: plain Enter (multiline + singleline) and
        // Cmd/Ctrl-Enter (still honored for muscle memory). Shift-Enter
        // is the multiline TextEdit's return key (see `.return_key`
        // above), so it inserts a newline without firing send.
        if resp.has_focus() {
            let plain_enter = ui.input(|i| {
                i.key_pressed(egui::Key::Enter)
                    && !i.modifiers.shift
                    && !i.modifiers.command
                    && !i.modifiers.ctrl
            });
            let cmd_enter = ui.input(|i| {
                i.key_pressed(egui::Key::Enter) && (i.modifiers.command || i.modifiers.ctrl)
            });
            if plain_enter || cmd_enter {
                send_now = true;
            }
        }

        // @-mention autocomplete: when the user types `@<query>` at the
        // tail of the draft, surface vault notes whose path matches the
        // query so they can be referenced in the prompt (`llm.md:247-268`
        // @-mentions). Picking a suggestion replaces the partial token
        // with `@path/to/note`.
        // @-mention popup: stable persistent id + memory-tracked open
        // state. Two failure modes the previous approach hit:
        //   1. Re-calling `.open(true)` every frame re-anchors the popup
        //      to the response rect — when the multiline TextEdit
        //      grows / shrinks (e.g. on Enter), the popup visibly
        //      jumps with it. A persistent `Popup::id` + `open_memory`
        //      lets egui pin the popup's first-frame rect.
        //   2. Gating render on `resp.has_focus()` killed the popup the
        //      moment focus moved into a suggestion button — the
        //      click never had a chance to register because the popup
        //      closure stopped being called.
        let mention_popup_id = ui.make_persistent_id("chat::mention_popup");
        let mention_state = draft.active_at_mention();
        if let Some((_, ref query)) = mention_state {
            let has_any = !mention_suggestions(app, query, 1).is_empty();
            if has_any {
                egui::Popup::open_id(ui.ctx(), mention_popup_id);
            } else {
                // No matches — close the popup (don't leave a stale
                // floating frame from a previous prefix).
                if egui::Popup::is_id_open(ui.ctx(), mention_popup_id) {
                    egui::Popup::close_id(ui.ctx(), mention_popup_id);
                }
            }
        } else if egui::Popup::is_id_open(ui.ctx(), mention_popup_id) {
            egui::Popup::close_id(ui.ctx(), mention_popup_id);
        }

        if let Some((prefix_start, query)) = mention_state {
            let suggestions = mention_suggestions(app, &query, 8);
            let popup_w = resp.rect.width().max(160.0);
            egui::Popup::from_response(&resp)
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
                            egui::Popup::close_id(ui.ctx(), mention_popup_id);
                        }
                    }
                });
        }
    });
    });
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
            app.session.chat.send(
                &vault_root,
                rt,
                config,
                &mcp_handler,
                &text,
            );
            // Force an immediate repaint so the next frame picks up
            // `s.pending = true` and the composer flips from Send to
            // Stop. Without this the click frame finishes rendering
            // with the old (pending=false) state, and the 750ms idle
            // cadence in `main.rs` is the only thing that schedules
            // the next paint — for fast operations the Stop button
            // never gets a chance to show.
            ui.ctx().request_repaint();
        }
    }
    }
}
