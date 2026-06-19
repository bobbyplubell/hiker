//! The `#[tool_router]` impl block — every method here is a public MCP
//! tool entry point that audits + delegates to the matching operation
//! method in `dispatch.rs`. Kept as one block because the `#[tool_router]`
//! macro expansion wires up `Self::tool_router()` across the whole block.

use hiker_core::tasks::types::McpClientVia;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ErrorData};
use rmcp::{tool, tool_router};

use super::App;
use crate::handler::params::{
    audit_err, audit_status, ApplyTag, BoardAddCard, BoardAddColumn, BoardAddTextCard,
    BoardCreate, BoardDeleteColumn, BoardGet, BoardMoveCard, BoardRemoveCard, BoardRenameColumn,
    BoardReorderColumn, BoardSetCardText, BoardsList, CheckDiagram, EditNote, GetActiveNote,
    GetNote, GetOpenNotes, GetSelection, RunQuery, RelatedNotes, SearchNotes, SetFrontmatter,
    TaskCheckout, TaskFail, TaskHeartbeat, TaskList, TaskSubmit, WriteNote,
};

// ---------- tool router ----------

#[tool_router(vis = "pub(super)")]
impl App {
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
        params: Parameters<SearchNotes>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.run_search(&p).await;
        self.state.audit.record(
            "search_notes",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Syntax-check a diagram before writing it into a note.
    ///
    /// status: diagram-agent-check
    #[tool(
        name = "check_diagram",
        description = "Validate diagram syntax (mermaid/wavedrom/latex) and return diagnostics before writing it into a note."
    )]
    pub async fn check_diagram(
        &self,
        params: Parameters<CheckDiagram>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.run_check_diagram(&p);
        self.state.audit.record(
            "check_diagram",
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
        params: Parameters<GetNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.read_note(&p).await;
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
        params: Parameters<RelatedNotes>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.find_related(&p).await;
        self.state.audit.record(
            "related_notes",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// status: mcp-tool-get-active-note
    #[tool(
        name = "get_active_note",
        description = "Return the currently-focused editor tab's path, cursor byte offset, and (if non-empty) selection range. Returns { path: null } when the active tab is an app page (Home / Settings / Queue / etc.). Read-only; does NOT count as having read the note for mcp-read-before-write — only get_note does."
    )]
    pub async fn get_active_note(
        &self,
        params: Parameters<GetActiveNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.read_active_note(&p).await;
        self.state.audit.record(
            "get_active_note",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// status: mcp-tool-get-open-notes
    #[tool(
        name = "get_open_notes",
        description = "Return the ordered list of currently-open buffer tabs as [{path, active}]. Non-buffer tabs (Home / Settings / Queue / Board / Agent) are omitted. Read-only; does NOT count as having read any note for mcp-read-before-write."
    )]
    pub async fn get_open_notes(
        &self,
        params: Parameters<GetOpenNotes>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.read_open_notes(&p).await;
        self.state.audit.record(
            "get_open_notes",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// status: mcp-tool-get-selection
    #[tool(
        name = "get_selection",
        description = "Return the active buffer's current selection as { path, start_byte, end_byte, text } when non-empty; otherwise { path: null }. Text comes from the same source as get_note(detail='full'). Read-only; does NOT count as having read the note."
    )]
    pub async fn get_selection(
        &self,
        params: Parameters<GetSelection>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.read_selection(&p).await;
        self.state.audit.record(
            "get_selection",
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
                       instead of hitting disk — the response is `{ status: \"staged\", proposal_id }`. A follow-up \
                       `get_note` reflects your own staged edits (you read your pending replica), but the change does \
                       not reach disk for the user until they accept the proposal. A brand-new note that does not yet \
                       exist on disk still returns 1002 not_found from `get_note` until accepted. Use \
                       `list_pending_proposals` to confirm a staged write landed."
    )]
    pub async fn write_note(
        &self,
        params: Parameters<WriteNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.save_note(&p).await;
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
                       response carries `status: \"staged\"` + `proposal_ids` and disk is unchanged until accepts."
    )]
    pub async fn edit_note(
        &self,
        params: Parameters<EditNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.apply_edits(&p).await;
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
                       disk is unchanged until the user accepts. Response carries `status: \"staged\"` + `proposal_id` \
                       in that case. Use `list_pending_proposals` to confirm."
    )]
    pub async fn set_frontmatter(
        &self,
        params: Parameters<SetFrontmatter>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.merge_frontmatter(&p).await;
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
                       unchanged until the user accepts. Response carries `status: \"staged\"` + `proposal_id` in that case."
    )]
    pub async fn apply_tag(
        &self,
        params: Parameters<ApplyTag>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.update_tag(&p, true).await;
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
                       until the user accepts. Response carries `status: \"staged\"` + `proposal_id` in that case."
    )]
    pub async fn remove_tag(
        &self,
        params: Parameters<ApplyTag>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.update_tag(&p, false).await;
        self.state.audit.record(
            "remove_tag",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Run a saved query-doc or an inline filter over the note-metadata
    /// index (read-only).
    ///
    /// status: query-mcp-tool
    #[tool(
        name = "query",
        description = "Run a saved query-doc (by vault-relative path) or an inline filter over the \
                       note-metadata index. Exactly one of query_doc / filter. The filter grammar is \
                       closed: kind, tags, path glob, fields ({key, eq | exists | min/max} — dates as \
                       ISO-8601 strings), board membership ({path, column?}); clauses AND, list values \
                       OR. `select` packs named frontmatter keys into each row; `limit` defaults to 100 \
                       (max 500). Returns { rows: [{ path, title, mtime, fields }] }. Read-only."
    )]
    pub async fn query(
        &self,
        params: Parameters<RunQuery>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.run_query_tool(&p).await;
        self.state.audit.record(
            "query",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Enumerate every board-doc in the vault (read-only).
    ///
    /// status: board-mcp-tools
    #[tool(
        name = "boards_list",
        description = "List every board-doc in the vault. Returns rel_path, board_id, title, column_count, card_count per board."
    )]
    pub async fn boards_list(
        &self,
        params: Parameters<BoardsList>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.enumerate_boards(&p).await;
        self.state.audit.record(
            "boards_list",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Fetch one board's body + resolved columns/cards (read-only).
    ///
    /// status: board-mcp-tools
    #[tool(
        name = "board_get",
        description = "Fetch a board by its vault-relative path. Returns the board-doc body plus resolved columns, each with its cards (title + reference resolution)."
    )]
    pub async fn board_get(
        &self,
        params: Parameters<BoardGet>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.fetch_board(&p).await;
        self.state.audit.record(
            "board_get",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Add a note as a card to a board column. Gated by
    /// `agent-write-review-mode` like every other write tool.
    ///
    /// status: board-mcp-tools
    #[tool(
        name = "board_add_card",
        description = "Append a note (by source_rel_path) as a card to a board column. \
                       Idempotent per board — a note already on the board returns status=\"noop\". \
                       NOTE: in review-required mode the board-doc edit is STAGED as a pending proposal \
                       (status=\"staged\" + proposal_id); disk is unchanged until the user accepts."
    )]
    pub async fn board_add_card(
        &self,
        params: Parameters<BoardAddCard>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.add_board_card(&p).await;
        self.state.audit.record(
            "board_add_card",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Create a new board-doc (default Todo/Doing/Done columns) at the
    /// configured `[boards] new_board_dir`. Review-gated like every write tool.
    ///
    /// status: mcp-tool-board-create
    #[tool(
        name = "board_create",
        description = "Create a new board-doc with default Todo/Doing/Done columns under the configured new_board_dir. \
                       Returns rel_path + board_id. NOTE: in review-required mode the new board-doc is STAGED as a \
                       pending proposal (status=\"staged\" + proposal_id); it does not reach disk until the user accepts."
    )]
    pub async fn board_create(
        &self,
        params: Parameters<BoardCreate>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.create_board(&p).await;
        self.state.audit.record(
            "board_create",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Append a freeform text card to a board column.
    ///
    /// status: mcp-tool-board-add-text-card
    #[tool(
        name = "board_add_text_card",
        description = "Append a freeform (non-note) text card to a board column. Returns the new card_id. \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id) \
                       and unchanged on disk until the user accepts."
    )]
    pub async fn board_add_text_card(
        &self,
        params: Parameters<BoardAddTextCard>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.add_board_text_card(&p).await;
        self.state.audit.record(
            "board_add_text_card",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Move/reorder a card to another column (or within a column).
    ///
    /// status: mcp-tool-board-move-card
    #[tool(
        name = "board_move_card",
        description = "Move/reorder a card (by card_id from board_get) to to_column at to_index (tail when omitted). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_move_card(
        &self,
        params: Parameters<BoardMoveCard>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.move_board_card(&p).await;
        self.state.audit.record(
            "board_move_card",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Rewrite a freeform card's text (errors on a note card).
    ///
    /// status: mcp-tool-board-set-card-text
    #[tool(
        name = "board_set_card_text",
        description = "Rewrite a freeform card's text in place (errors on a note card). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_set_card_text(
        &self,
        params: Parameters<BoardSetCardText>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.set_board_card_text(&p).await;
        self.state.audit.record(
            "board_set_card_text",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Drop a card from the board (referenced note untouched).
    ///
    /// status: mcp-tool-board-remove-card
    #[tool(
        name = "board_remove_card",
        description = "Remove a card from the board by card_id (the referenced note is untouched). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_remove_card(
        &self,
        params: Parameters<BoardRemoveCard>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.remove_board_card(&p).await;
        self.state.audit.record(
            "board_remove_card",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Add a new empty column to a board (idempotent on name collision).
    ///
    /// status: mcp-tool-board-add-column
    #[tool(
        name = "board_add_column",
        description = "Add a new empty column to a board (appended at the tail; no-op if the name exists). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_add_column(
        &self,
        params: Parameters<BoardAddColumn>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.add_board_column(&p).await;
        self.state.audit.record(
            "board_add_column",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Rename a column in place (cards keep order/membership).
    ///
    /// status: mcp-tool-board-rename-column
    #[tool(
        name = "board_rename_column",
        description = "Rename a board column in place (cards keep their order and membership). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_rename_column(
        &self,
        params: Parameters<BoardRenameColumn>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.rename_board_column(&p).await;
        self.state.audit.record(
            "board_rename_column",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Move a column to a new index in the column order.
    ///
    /// status: mcp-tool-board-reorder-column
    #[tool(
        name = "board_reorder_column",
        description = "Move a board column to a new index in the column order (clamps to the tail). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_reorder_column(
        &self,
        params: Parameters<BoardReorderColumn>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.reorder_board_column(&p).await;
        self.state.audit.record(
            "board_reorder_column",
            &serde_json::to_value(&p).unwrap_or(serde_json::Value::Null),
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }

    /// Delete a column (drops its card refs; notes untouched).
    ///
    /// status: mcp-tool-board-delete-column
    #[tool(
        name = "board_delete_column",
        description = "Delete a board column (drops that column's card references; referenced notes are untouched). \
                       NOTE: in review-required mode the board-doc edit is STAGED (status=\"staged\" + proposal_id); \
                       disk is unchanged until the user accepts."
    )]
    pub async fn board_delete_column(
        &self,
        params: Parameters<BoardDeleteColumn>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.delete_board_column(&p).await;
        self.state.audit.record(
            "board_delete_column",
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
        params: Parameters<TaskCheckout>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.lease_task(&p, McpClientVia::External).await;
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
        params: Parameters<TaskSubmit>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.record_task_result(&p).await;
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
        params: Parameters<TaskFail>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.record_task_failure(&p).await;
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
        params: Parameters<TaskHeartbeat>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.extend_task_lease(&p).await;
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
        params: Parameters<TaskList>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.list_tasks(&p).await;
        self.state.audit.record(
            "task_list",
            &serde_json::Value::Null,
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }
}
