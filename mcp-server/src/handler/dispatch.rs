//! In-process tool dispatch (used by the basic chat agent) plus the
//! per-tool `guard_tool` gate. The dispatch entrypoint routes by tool
//! name to the same `_inner` helpers the rmcp `#[tool]` methods call,
//! so the basic agent loop and external rmcp clients share one tool
//! registry, audit shape, and error model.

use super::*;

impl HikerHandler {
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

    /// In-process tool dispatch entrypoint used by the basic agent loop
    /// (`agent-tool-routing-via-mcp`). Routes by tool name to the same
    /// `_inner` helpers the rmcp `#[tool]` methods call, so the basic
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
                let p: SearchNotesParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid search_notes args: {e}"))?;
                let r = self.search_notes_inner(&p).await;
                self.state.audit.record(
                    "search_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "get_note" => {
                let p: GetNoteParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid get_note args: {e}"))?;
                let r = self.get_note_inner(&p).await;
                self.state.audit.record(
                    "get_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "related_notes" => {
                let p: RelatedNotesParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid related_notes args: {e}"))?;
                let r = self.related_notes_inner(&p).await;
                self.state.audit.record(
                    "related_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "write_note" => {
                let p: WriteNoteParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid write_note args: {e}"))?;
                let r = self.write_note_inner(&p).await;
                self.state.audit.record(
                    "write_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "edit_note" => {
                let p: EditNoteParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid edit_note args: {e}"))?;
                let r = self.edit_note_inner(&p).await;
                self.state.audit.record(
                    "edit_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "set_frontmatter" => {
                let p: SetFrontmatterParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid set_frontmatter args: {e}"))?;
                let r = self.set_frontmatter_inner(&p).await;
                self.state.audit.record(
                    "set_frontmatter",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "apply_tag" => {
                let p: ApplyTagParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid apply_tag args: {e}"))?;
                let r = self.apply_tag_inner(&p, true).await;
                self.state.audit.record(
                    "apply_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "remove_tag" => {
                let p: ApplyTagParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid remove_tag args: {e}"))?;
                let r = self.apply_tag_inner(&p, false).await;
                self.state.audit.record(
                    "remove_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_checkout" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskCheckoutParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_checkout args: {e}"))?;
                let r = self.task_checkout_inner(&p, McpClientVia::InProcessChatAgent).await;
                self.state.audit.record(
                    "task_checkout",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_submit" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskSubmitParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_submit args: {e}"))?;
                let r = self.task_submit_inner(&p).await;
                self.state.audit.record(
                    "task_submit",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_fail" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskFailParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_fail args: {e}"))?;
                let r = self.task_fail_inner(&p).await;
                self.state.audit.record(
                    "task_fail",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_heartbeat" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskHeartbeatParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_heartbeat args: {e}"))?;
                let r = self.task_heartbeat_inner(&p).await;
                self.state.audit.record(
                    "task_heartbeat",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_list" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskListParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_list args: {e}"))?;
                let r = self.task_list_inner(&p).await;
                self.state.audit.record(
                    "task_list",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            other => return Err(format!("unknown tool: {other}")),
        };

        let result = outcome.map_err(|e| format!("{} (code {})", e.message, e.code.0))?;
        let payload = result
            .structured_content
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_string(&payload).map_err(|e| format!("serialize result: {e}"))
    }
}
