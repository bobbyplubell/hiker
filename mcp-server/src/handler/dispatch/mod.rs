//! In-process tool dispatch (used by the basic chat agent), the per-tool
//! `guard_tool` gate, and the per-domain workhorse implementations of
//! every tool. The dispatch entrypoint here routes by tool name to the
//! same operation methods that the rmcp `#[tool]` methods in
//! `router.rs` call, so the basic agent loop and external rmcp clients
//! share one tool registry, audit shape, and error model.
//!
//! Per-domain implementations live in sibling files as additional
//! `impl App` blocks, split by tool family so each stays under the
//! file-length budget: `notes` (search / read / save / edit / tag /
//! frontmatter), `boards` (board CRUD + card/column ops), `tasks`
//! (queue lease/result/failure/heartbeat/list).

mod boards;
mod diagram;
mod notes;
mod tasks;
mod ui_context;


use hiker_core::tasks::types::McpClientVia;
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};

use super::App;
use crate::handler::params::{
    audit_err, audit_status, hiker_err, ApplyTag, BoardAddCard, BoardAddColumn, BoardAddTextCard,
    BoardCreate, BoardDeleteColumn, BoardGet, BoardMoveCard, BoardRemoveCard, BoardRenameColumn,
    BoardReorderColumn, BoardSetCardText, BoardsList, CheckDiagram, EditNote, GetActiveNote,
    GetNote, GetOpenNotes, GetSelection, RelatedNotes, SearchNotes, SetFrontmatter, TaskCheckout,
    TaskFail, TaskHeartbeat, TaskList, TaskSubmit, WriteNote,
};

impl App {
    /// status: mcp-tool-toggles
    /// Per-tool gate. Reads the shared `tools` config live so a flip in
    /// the settings UI applies to the next dispatch without a vault
    /// restart. Combines the per-tool flag with the `writes_enabled`
    /// master gate (write tools) — see `McpToolsConfig::tool_allowed`.
    pub(super) fn guard_tool(&self, name: &str) -> Result<(), ErrorData> {
        let cfg = self
            .state
            .tools
            .read()
            .map_err(|_| hiker_err(ErrorCode(1004), "mcp tools cfg poisoned"))?;
        if cfg.tool_allowed(name) {
            Ok(())
        } else {
            // `hiker_err` requires a `'static` Cow; build the message into
            // an owned String and lean on Cow::from(String) for the conversion.
            let msg: std::borrow::Cow<'static, str> =
                std::borrow::Cow::Owned(format!("tool `{name}` is disabled"));
            Err(hiker_err(ErrorCode(1004), msg))
        }
    }

    /// status: task-queue-respects-llm-disable
    /// The queue's only purpose is LLM work; with `[llm] enabled = false`
    /// the direct worker is force-off and the rmcp-side `task_*` tools
    /// answer `1004 disabled` so external agents see a coherent disabled
    /// state instead of being able to checkout work no in-process worker
    /// will ever drain.
    pub(super) fn guard_tasks(&self) -> Result<(), ErrorData> {
        if self.state.llm_enabled {
            Ok(())
        } else {
            Err(hiker_err(ErrorCode(1004), "tasks disabled (llm features off)"))
        }
    }

    /// In-process tool dispatch entrypoint used by the basic agent loop
    /// (`agent-tool-routing-via-mcp`). Routes by tool name to the same
    /// operation methods the rmcp `#[tool]` methods call, so the basic
    /// agent loop and the external rmcp client see one tool registry,
    /// one set of audit-log shapes, and one set of error codes.
    ///
    /// Returns the structured JSON payload as a string (the agent feeds it
    /// back into the model's context) on success, or a short error reason
    /// on failure. Failures here are *not* turn-killing — the agent loop
    /// folds them into a `ToolResult { ok: false }`, so the model can
    /// retry, try a different tool, or give up.
    pub async fn dispatch_tool(
        &self,
        name: &str,
        args_json: &str,
    ) -> Result<String, String> {
        let parse = |raw: &str| -> Result<serde_json::Value, String> {
            serde_json::from_str(raw).map_err(|e| format!("invalid arguments json: {e}"))
        };
        let raw_value = parse(args_json)?;

        // status: mcp-tool-toggles
        // Per-tool gate. Reads the shared `tools` config live so a flip
        // in the settings UI applies to the next dispatch. Returns
        // `1004 disabled` matching the existing `writes_enabled` shape.
        let tool_allowed = {
            let cfg = self
                .state
                .tools
                .read()
                .map_err(|_| "mcp tools cfg poisoned".to_string())?;
            cfg.tool_allowed(name)
        };
        if !tool_allowed {
            return Err(format!("tool `{name}` is disabled (code 1004)"));
        }

        let outcome: Result<CallToolResult, ErrorData> = match name {
            "search_notes" => {
                let p: SearchNotes = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid search_notes args: {e}"))?;
                let r = self.run_search(&p).await;
                self.state.audit.record(
                    "search_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "get_note" => {
                let p: GetNote = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid get_note args: {e}"))?;
                let r = self.read_note(&p).await;
                self.state.audit.record(
                    "get_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "related_notes" => {
                let p: RelatedNotes = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid related_notes args: {e}"))?;
                let r = self.find_related(&p).await;
                self.state.audit.record(
                    "related_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "check_diagram" => {
                let p: CheckDiagram = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid check_diagram args: {e}"))?;
                let r = self.run_check_diagram(&p);
                self.state.audit.record(
                    "check_diagram",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "get_active_note" | "get_open_notes" | "get_selection" => {
                self.dispatch_ui_context_tool(name, &raw_value).await
            }
            "write_note" => {
                let p: WriteNote = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid write_note args: {e}"))?;
                let r = self.save_note(&p).await;
                self.state.audit.record(
                    "write_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "edit_note" => {
                let p: EditNote = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid edit_note args: {e}"))?;
                let r = self.apply_edits(&p).await;
                self.state.audit.record(
                    "edit_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "set_frontmatter" => {
                let p: SetFrontmatter = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid set_frontmatter args: {e}"))?;
                let r = self.merge_frontmatter(&p).await;
                self.state.audit.record(
                    "set_frontmatter",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "apply_tag" => {
                let p: ApplyTag = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid apply_tag args: {e}"))?;
                let r = self.update_tag(&p, true).await;
                self.state.audit.record(
                    "apply_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "remove_tag" => {
                let p: ApplyTag = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid remove_tag args: {e}"))?;
                let r = self.update_tag(&p, false).await;
                self.state.audit.record(
                    "remove_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_checkout" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskCheckout = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_checkout args: {e}"))?;
                let r = self.lease_task(&p, McpClientVia::InProcessChatAgent).await;
                self.state.audit.record(
                    "task_checkout",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_submit" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskSubmit = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_submit args: {e}"))?;
                let r = self.record_task_result(&p).await;
                self.state.audit.record(
                    "task_submit",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_fail" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskFail = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_fail args: {e}"))?;
                let r = self.record_task_failure(&p).await;
                self.state.audit.record(
                    "task_fail",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_heartbeat" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskHeartbeat = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_heartbeat args: {e}"))?;
                let r = self.extend_task_lease(&p).await;
                self.state.audit.record(
                    "task_heartbeat",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_list" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskList = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_list args: {e}"))?;
                let r = self.list_tasks(&p).await;
                self.state.audit.record(
                    "task_list",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            other => match self.dispatch_board_tool(other, &raw_value).await? {
                Some(r) => r,
                None => return Err(format!("unknown tool: {other}")),
            },
        };

        let result = outcome.map_err(|e| format!("{} (code {})", e.message, e.code.0))?;
        let payload = result
            .structured_content
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_string(&payload).map_err(|e| format!("serialize result: {e}"))
    }

    /// Dispatch the board-* tools, split out of `dispatch_tool` to keep that
    /// function within the length budget. Returns `Ok(None)` when `name` is not
    /// a board tool, so the caller falls through to its unknown-tool error;
    /// `Err` carries an argument-parse failure.
    // status: board-mcp-tools
    async fn dispatch_board_tool(
        &self,
        name: &str,
        raw_value: &serde_json::Value,
    ) -> Result<Option<Result<CallToolResult, ErrorData>>, String> {
        let r = match name {
            "boards_list" => {
                let p: BoardsList =
                    serde_json::from_value(raw_value.clone()).unwrap_or(BoardsList {});
                self.enumerate_boards(&p).await
            }
            "board_get" => {
                let p: BoardGet = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_get args: {e}"))?;
                self.fetch_board(&p).await
            }
            "board_add_card" => {
                let p: BoardAddCard = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_add_card args: {e}"))?;
                self.add_board_card(&p).await
            }
            "board_create" => {
                let p: BoardCreate = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_create args: {e}"))?;
                self.create_board(&p).await
            }
            "board_add_text_card" => {
                let p: BoardAddTextCard = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_add_text_card args: {e}"))?;
                self.add_board_text_card(&p).await
            }
            "board_move_card" => {
                let p: BoardMoveCard = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_move_card args: {e}"))?;
                self.move_board_card(&p).await
            }
            "board_set_card_text" => {
                let p: BoardSetCardText = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_set_card_text args: {e}"))?;
                self.set_board_card_text(&p).await
            }
            "board_remove_card" => {
                let p: BoardRemoveCard = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_remove_card args: {e}"))?;
                self.remove_board_card(&p).await
            }
            "board_add_column" => {
                let p: BoardAddColumn = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_add_column args: {e}"))?;
                self.add_board_column(&p).await
            }
            "board_rename_column" => {
                let p: BoardRenameColumn = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_rename_column args: {e}"))?;
                self.rename_board_column(&p).await
            }
            "board_reorder_column" => {
                let p: BoardReorderColumn = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_reorder_column args: {e}"))?;
                self.reorder_board_column(&p).await
            }
            "board_delete_column" => {
                let p: BoardDeleteColumn = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid board_delete_column args: {e}"))?;
                self.delete_board_column(&p).await
            }
            _ => return Ok(None),
        };
        self.state
            .audit
            .record(name, raw_value, audit_status(&r), audit_err(&r));
        Ok(Some(r))
    }
}
