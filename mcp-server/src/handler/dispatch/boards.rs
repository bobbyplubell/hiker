use hiker_core::ops::op_writes;
use rmcp::model::{CallToolResult, ErrorData};

use super::App;
use crate::handler::params::{
    structured, translate_hiker_err, BoardAddCard, BoardAddColumn, BoardAddTextCard, BoardCreate,
    BoardDeleteColumn, BoardGet, BoardMoveCard, BoardRemoveCard, BoardRenameColumn,
    BoardReorderColumn, BoardSetCardText, BoardsList,
};

impl App {
    /// status: board-mcp-tools
    /// Enumerate every board-doc in the vault (read-only). Mirrors the trail
    /// `trails_list` shape: one row per board with id/title/path + column and
    /// card counts.
    pub(in crate::handler) async fn enumerate_boards(
        &self,
        _p: &BoardsList,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("boards_list")?;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let op_log = self.state.oplog.as_ref().ok_or_else(|| {
            ErrorData::internal_error("boards_list requires an open op log", None)
        })?;
        let rows = hiker_core::boards::list(&self.state.vault, &store, op_log)
            .map_err(translate_hiker_err)?;
        Ok(structured(
            serde_json::to_value(&rows).unwrap_or(serde_json::Value::Null),
        ))
    }

    /// status: board-mcp-tools
    /// Full detail for one board: body + resolved columns/cards (read-only).
    pub(in crate::handler) async fn fetch_board(&self, p: &BoardGet) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("board_get")?;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let op_log = self.state.oplog.as_ref().ok_or_else(|| {
            ErrorData::internal_error("board_get requires an open op log", None)
        })?;
        let detail = hiker_core::boards::get_board(
            &self.state.vault, &store, op_log, &p.rel_path,
        )
        .map_err(translate_hiker_err)?;
        Ok(structured(
            serde_json::to_value(&detail).unwrap_or(serde_json::Value::Null),
        ))
    }

    /// status: board-mcp-tools
    /// Add a note as a card to a board column. In review-required mode the
    /// board-doc frontmatter edit STAGES as one op-log pending op (like every
    /// other agent write); in direct mode it commits via the same
    /// `core::boards::ops::add_card` user-save path the UI uses. Idempotent
    /// per board — a note already on the board returns `status: "noop"`.
    pub(in crate::handler) async fn add_board_card(
        &self,
        p: &BoardAddCard,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("board_add_card")?;
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "review mode requires an open op log".to_string(),
                    None,
                )
            })?;
            // Compute the board-doc's new frontmatter (card appended) without
            // writing, then stage it as one anchorless pending op authored
            // `agent:<client>` — the same review path note writes use.
            let new_src = hiker_core::boards::add_card_preview(
                &self.state.vault,
                &p.board_rel_path,
                &p.column,
                &p.source_rel_path,
            )
            .map_err(translate_hiker_err)?;
            let Some(new_src) = new_src else {
                return Ok(structured(serde_json::json!({
                    "board_rel_path": p.board_rel_path,
                    "status": "noop",
                })));
            };
            let staging_id = self.stage_whole_body(op_log, &p.board_rel_path, &new_src)?;
            Ok(structured(serde_json::json!({
                "board_rel_path": p.board_rel_path,
                "status": "staged",
                "staging_id": staging_id,
            })))
        } else {
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "board_add_card requires an open op log".to_string(),
                    None,
                )
            })?;
            // Compute the new board source under the lock, then drop it before
            // the op-log write so no `!Send` `Store` guard crosses an await
            // (the rmcp tool future must be `Send`). The card stores the
            // source note's current ULID (empty when unstamped — the path half
            // still anchors it; the derived-table flow heals later).
            let new_src = hiker_core::boards::add_card_preview(
                &self.state.vault,
                &p.board_rel_path,
                &p.column,
                &p.source_rel_path,
            )
            .map_err(translate_hiker_err)?;
            let Some(new_src) = new_src else {
                return Ok(structured(serde_json::json!({
                    "board_rel_path": p.board_rel_path,
                    "status": "noop",
                })));
            };
            op_writes::user_save(op_log, &self.state.vault, &p.board_rel_path, &new_src)
                .map_err(translate_hiker_err)?;
            let _ = self
                .state
                .jobs
                .send(hiker_core::indexer::IndexJob::Upsert {
                    rel_path: p.board_rel_path.clone(),
                    force: false,
                })
                .await;
            Ok(structured(serde_json::json!({
                "board_rel_path": p.board_rel_path,
                "status": "written",
            })))
        }
    }

    /// Whether `review_required` is on. Reads the live shared config so a flip
    /// in the settings UI applies to the next dispatch.
    fn review_required(&self) -> bool {
        self.state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false)
    }

    /// The shared review-vs-direct staging path for the board write tools that
    /// edit an EXISTING board-doc. `new_src` is the post-edit board-doc source
    /// (`None` = idempotent no-op → `status: "noop"`). In review mode it stages
    /// one anchorless pending op and returns `status: "staged"` + `staging_id`;
    /// in direct mode it commits via `op_writes::user_save` + re-index and
    /// returns `status: "written"`. `extra` fields are merged into the response
    /// (e.g. a minted `card_id`). The board core verbs and the preview share
    /// one `apply_edit`, so staged and direct text can't diverge.
    ///
    /// status: board-mcp-tools
    async fn stage_or_commit_board(
        &self,
        board_rel_path: &str,
        new_src: Option<String>,
        extra: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        let op_log = self.state.oplog.as_ref().ok_or_else(|| {
            ErrorData::internal_error("board write requires an open op log".to_string(), None)
        })?;
        let Some(new_src) = new_src else {
            return Ok(structured(serde_json::json!({
                "board_rel_path": board_rel_path,
                "status": "noop",
            })));
        };
        let mut body = serde_json::json!({ "board_rel_path": board_rel_path });
        if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extra.as_object()) {
            for (k, v) in extra_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
        if self.review_required() {
            let staging_id = self.stage_whole_body(op_log, board_rel_path, &new_src)?;
            if let Some(obj) = body.as_object_mut() {
                obj.insert("status".into(), "staged".into());
                obj.insert("staging_id".into(), serde_json::to_value(staging_id).unwrap_or(serde_json::Value::Null));
            }
        } else {
            op_writes::user_save(op_log, &self.state.vault, board_rel_path, &new_src)
                .map_err(translate_hiker_err)?;
            let _ = self
                .state
                .jobs
                .send(hiker_core::indexer::IndexJob::Upsert {
                    rel_path: board_rel_path.to_string(),
                    force: false,
                })
                .await;
            if let Some(obj) = body.as_object_mut() {
                obj.insert("status".into(), "written".into());
            }
        }
        Ok(structured(body))
    }

    /// Compute a board edit's preview source under no held lock, then stage or
    /// commit it. The `preview` closure runs `core::boards::ops::preview_edit`
    /// (or a sibling) and returns the post-edit source (`None` = no-op). Keeps
    /// each board-edit tool a thin wrapper.
    async fn board_edit_tool(
        &self,
        tool: &str,
        board_rel_path: &str,
        new_src: Result<Option<String>, ErrorData>,
        extra: serde_json::Value,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool(tool)?;
        self.stage_or_commit_board(board_rel_path, new_src?, extra).await
    }

    /// status: mcp-tool-board-create
    /// Create a new board-doc (default Todo/Doing/Done columns) at the
    /// configured `[boards] new_board_dir`. Commits directly via
    /// `core::boards::ops::create_board`, even in review mode: the op-log
    /// whole-file-create staging path seeds the document by writing an empty
    /// `.md` to disk before queueing the content as a pending op (see
    /// `op_writes::doc_id_or_seed` → `OpLog::create_document` → `write_md_file`),
    /// which would leave a phantom empty board-doc visible in the vault until
    /// the user accepted. Creates are a structural action — there is no
    /// existing content to overwrite — so the safest choice is to commit
    /// directly and let the user delete the board if they reject the proposal.
    /// All subsequent board *edits* still stage in review mode. Returns the
    /// new `rel_path` + `board_id`.
    pub(in crate::handler) async fn create_board(
        &self,
        p: &BoardCreate,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("board_create")?;
        let op_log = self.state.oplog.as_ref().ok_or_else(|| {
            ErrorData::internal_error("board_create requires an open op log", None)
        })?;
        let outcome = hiker_core::boards::ops::create_board(
            &self.state.watcher,
            &self.state.jobs,
            op_log,
            &self.state.vault,
            &self.state.boards_config,
            &p.name,
        )
        .await
        .map_err(translate_hiker_err)?;
        Ok(structured(serde_json::json!({
            "rel_path": outcome.board_doc_rel,
            "board_id": outcome.board_id,
            "status": "written",
        })))
    }

    /// status: mcp-tool-board-add-text-card
    pub(in crate::handler) async fn add_board_text_card(
        &self,
        p: &BoardAddTextCard,
    ) -> Result<CallToolResult, ErrorData> {
        let card_id = hiker_core::store::dto::new_id();
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::AddTextCard {
                column: &p.column,
                card_id: card_id.clone(),
                text: &p.text,
            },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_add_text_card",
            &p.board_rel_path,
            preview,
            serde_json::json!({ "card_id": card_id }),
        )
        .await
    }

    /// status: mcp-tool-board-move-card
    pub(in crate::handler) async fn move_board_card(
        &self,
        p: &BoardMoveCard,
    ) -> Result<CallToolResult, ErrorData> {
        // status: board-card-references — the MCP `card_id` param now
        // accepts either a note card's vault path or a freeform card's
        // `card_id`; core disambiguates by shape.
        let preview = hiker_core::boards::ops::preview_move_card(
            &self.state.vault,
            &p.board_rel_path,
            &p.card_id,
            &p.to_column,
            p.to_index,
        )
        .map(Some)
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_move_card",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-set-card-text
    pub(in crate::handler) async fn set_board_card_text(
        &self,
        p: &BoardSetCardText,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::SetCardText {
                card_id: &p.card_id,
                text: &p.text,
            },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_set_card_text",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-remove-card
    pub(in crate::handler) async fn remove_board_card(
        &self,
        p: &BoardRemoveCard,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::RemoveCard { handle: &p.card_id },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_remove_card",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-add-column
    pub(in crate::handler) async fn add_board_column(
        &self,
        p: &BoardAddColumn,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::AddColumn { name: &p.name },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_add_column",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-rename-column
    pub(in crate::handler) async fn rename_board_column(
        &self,
        p: &BoardRenameColumn,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::RenameColumn {
                old_name: &p.old_name,
                new_name: &p.new_name,
            },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_rename_column",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-reorder-column
    pub(in crate::handler) async fn reorder_board_column(
        &self,
        p: &BoardReorderColumn,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::ReorderColumn {
                name: &p.name,
                to_index: p.to_index,
            },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_reorder_column",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }

    /// status: mcp-tool-board-delete-column
    pub(in crate::handler) async fn delete_board_column(
        &self,
        p: &BoardDeleteColumn,
    ) -> Result<CallToolResult, ErrorData> {
        let preview = hiker_core::boards::ops::preview_edit(
            &self.state.vault,
            &p.board_rel_path,
            &hiker_core::boards::ops::BoardEdit::DeleteColumn { name: &p.name },
        )
        .map_err(translate_hiker_err);
        self.board_edit_tool(
            "board_delete_column",
            &p.board_rel_path,
            preview,
            serde_json::Value::Null,
        )
        .await
    }
}
