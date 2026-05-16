//! The `#[tool_router]` impl block — every method here is a public MCP
//! tool entry point that audits + delegates to the matching `_inner`
//! helper. Kept as one block because the `#[tool_router]` macro
//! expansion wires up `Self::tool_router()` across the whole block.

use super::*;

// ---------- tool router ----------

#[tool_router(vis = "pub(super)")]
impl HikerHandler {
    /// Search the vault. Wraps `core::search::query` and returns the same
    /// three-bucket payload (lexical, semantic, fused) the UI consumes.
    /// Empty query returns empty buckets without erroring.
    ///
    /// status: mcp-tool-search-notes
    #[tool(
        name = "search_notes",
        description = "Hybrid search across the vault (lexical FTS5 + semantic vec). Returns lexical_hits, semantic_hits, and fused buckets."
    )]
    pub async fn search_notes(
        &self,
        params: Parameters<SearchNotesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.search_notes_inner(&p).await;
        self.state.audit.record(
            "search_notes",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Fetch a single note by rel_path with progressive disclosure.
    ///
    /// status: mcp-tool-get-note
    #[tool(
        name = "get_note",
        description = "Fetch a note by vault-relative path. detail=digest|snippet|full controls the payload size."
    )]
    pub async fn get_note(
        &self,
        params: Parameters<GetNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.get_note_inner(&p).await;
        self.state.audit.record(
            "get_note",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Notes most related to the given source note. Wraps the existing
    /// related-notes algorithm.
    ///
    /// status: mcp-tool-related-notes
    #[tool(
        name = "related_notes",
        description = "Top-K notes whose chunks are most semantically similar to a given note."
    )]
    pub async fn related_notes(
        &self,
        params: Parameters<RelatedNotesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.related_notes_inner(&p).await;
        self.state.audit.record(
            "related_notes",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Create or replace a note's body. Stamps the changelog with
    /// `author=agent:<client>` and re-indexes. Drift-aware when
    /// `expected_hash` is provided.
    #[tool(
        name = "write_note",
        description = "Create or replace a note's body. Returns the new content hash on a direct write. \
                       NOTE: when the server is in review-required mode, the write is STAGED as a pending proposal \
                       instead of hitting disk — the response is `{ status: \"staged\", staging_id }` and the file \
                       will NOT be visible via `get_note` (which returns 1002 not_found) until the user accepts the \
                       proposal. Use `list_pending_proposals` to confirm a staged write landed."
    )]
    pub async fn write_note(
        &self,
        params: Parameters<WriteNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.write_note_inner(&p).await;
        self.state.audit.record(
            "write_note",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Apply one or more span-anchored patches to an existing note.
    /// Validates anchors at receive time; on any failure the whole call
    /// rejects and nothing stages.
    ///
    /// status: mcp-tool-edit-note
    /// status: mcp-edit-note-validation
    #[tool(
        name = "edit_note",
        description = "Apply one or more span-anchored patches to an existing note. Each `old_str` must match \
                       exactly once unless `replace_all: true`. Refuses non-existent paths (use write_note to create). \
                       Validation (path exists, per-edit anchor uniqueness, no textual overlap, anchors resolve against \
                       the pre-application file) runs atomically — on any failure the whole call rejects. \
                       NOTE: in review-required mode each edit STAGES as its own pending proposal (sharing a batch_id); \
                       response carries `status: \"staged\"` + `staging_ids` and disk is unchanged until accepts."
    )]
    pub async fn edit_note(
        &self,
        params: Parameters<EditNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.edit_note_inner(&p).await;
        self.state.audit.record(
            "edit_note",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Merge fields into a note's YAML frontmatter. Deep-merge for nested
    /// maps; auto-stamps `hiker.author: agent-authored`.
    #[tool(
        name = "set_frontmatter",
        description = "Deep-merge fields into a note's YAML frontmatter (auto-stamps hiker.author=agent-authored). \
                       NOTE: in review-required mode the merged result is STAGED as a pending proposal — the file on \
                       disk is unchanged until the user accepts. Response carries `status: \"staged\"` + `staging_id` \
                       in that case. Use `list_pending_proposals` to confirm."
    )]
    pub async fn set_frontmatter(
        &self,
        params: Parameters<SetFrontmatterParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.set_frontmatter_inner(&p).await;
        self.state.audit.record(
            "set_frontmatter",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Append a tag to a note's `tags` frontmatter list. Idempotent.
    #[tool(
        name = "apply_tag",
        description = "Append a tag to a note's tags frontmatter list (idempotent). \
                       NOTE: in review-required mode the tagged result is STAGED as a pending proposal — disk is \
                       unchanged until the user accepts. Response carries `status: \"staged\"` + `staging_id` in that case."
    )]
    pub async fn apply_tag(
        &self,
        params: Parameters<ApplyTagParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.apply_tag_inner(&p, true).await;
        self.state.audit.record(
            "apply_tag",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Remove a tag from a note's `tags` frontmatter list. No-op if absent.
    #[tool(
        name = "remove_tag",
        description = "Remove a tag from a note's tags frontmatter list (no-op if absent). \
                       NOTE: in review-required mode the result is STAGED as a pending proposal — disk is unchanged \
                       until the user accepts. Response carries `status: \"staged\"` + `staging_id` in that case."
    )]
    pub async fn remove_tag(
        &self,
        params: Parameters<ApplyTagParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.apply_tag_inner(&p, false).await;
        self.state.audit.record(
            "remove_tag",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Take the next eligible task from the queue. Stamps a lease against
    /// the calling rmcp client id. Returns `null` when nothing is
    /// available.
    ///
    /// status: tasks-mcp-tool-checkout
    #[tool(
        name = "task_checkout",
        description = "Return the next eligible task from the work queue (or null). Stamps a lease."
    )]
    pub async fn task_checkout(
        &self,
        params: Parameters<TaskCheckoutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_checkout_inner(&p, McpClientVia::External).await;
        self.state.audit.record(
            "task_checkout",
            &serde_json::Value::Null,
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Submit a task's result. Validates `value` against the task's
    /// `output_schema` if any.
    ///
    /// status: tasks-mcp-tool-submit
    #[tool(
        name = "task_submit",
        description = "Submit a result for a leased task. Validates against the task's output_schema if any."
    )]
    pub async fn task_submit(
        &self,
        params: Parameters<TaskSubmitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_submit_inner(&p).await;
        self.state.audit.record(
            "task_submit",
            &serde_json::json!({"task_id": p.task_id}),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Fail a leased task. Producer handle resolves to Failed.
    ///
    /// status: tasks-mcp-tool-fail
    #[tool(
        name = "task_fail",
        description = "Mark a leased task as failed (no auto-requeue)."
    )]
    pub async fn task_fail(
        &self,
        params: Parameters<TaskFailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_fail_inner(&p).await;
        self.state.audit.record(
            "task_fail",
            &serde_json::json!({"task_id": p.task_id}),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Extend the current lease.
    ///
    /// status: tasks-mcp-tool-heartbeat
    #[tool(
        name = "task_heartbeat",
        description = "Extend the current lease on a leased task."
    )]
    pub async fn task_heartbeat(
        &self,
        params: Parameters<TaskHeartbeatParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_heartbeat_inner(&p).await;
        self.state.audit.record(
            "task_heartbeat",
            &serde_json::json!({"task_id": p.task_id}),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Read-only inspection of the queue.
    ///
    /// status: tasks-mcp-tool-list
    #[tool(
        name = "task_list",
        description = "List current tasks (read-only). Optional filters by state and kind."
    )]
    pub async fn task_list(
        &self,
        params: Parameters<TaskListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_list_inner(&p).await;
        self.state.audit.record(
            "task_list",
            &serde_json::Value::Null,
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }
}
