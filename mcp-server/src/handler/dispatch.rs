//! In-process tool dispatch (used by the basic chat agent), the per-tool
//! `guard_tool` gate, and the workhorse implementations of every note- and
//! task-shaped tool. The dispatch entrypoint routes by tool name to these
//! same operation methods that the rmcp `#[tool]` methods in `router.rs`
//! call, so the basic agent loop and external rmcp clients share one tool
//! registry, audit shape, and error model.

use hiker_core::frontmatter;
use hiker_core::ops;
use hiker_core::search::{self, Modes};
use hiker_core::ops::op_writes;
use hiker_core::textpatch::{apply_edit, find_all_matches, EditPayload};
use hiker_core::tasks::types::{
    McpClientVia, Priority as TaskPriority, QueueError, TaskShape as TaskShapeKind, TaskState,
};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};

use super::App;
use crate::handler::params::{
    audit_err, audit_status, hiker_err, structured, translate_hiker_err, ApplyTag, BoardAddCard,
    BoardGet, BoardsList, EditNote, GetNote, GetNoteDigest, GetNoteFull, GetNoteSnippet, NoteDetail,
    RelatedNotes, SearchNotes, SetFrontmatter, TaskCheckout, TaskFail, TaskHeartbeat, TaskList,
    TaskSubmit, WriteNote, WriteOutcome, EditOutcome, CLIENT_ID,
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
            _ => return Ok(None),
        };
        self.state
            .audit
            .record(name, raw_value, audit_status(&r), audit_err(&r));
        Ok(Some(r))
    }
}

// ---------- note-shaped tool implementations ----------

impl App {
    pub(super) async fn run_search(&self, p: &SearchNotes) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("search_notes")?;
        let modes = match &p.modes {
            Some(m) => Modes { semantic: m.semantic, lexical: m.lexical },
            None => Modes { semantic: true, lexical: true },
        };
        let max_top_k = self.state.config.max_top_k.max(1);
        let requested = p.top_k.unwrap_or(20).clamp(1, max_top_k);

        if p.query.trim().is_empty() || (!modes.lexical && !modes.semantic) {
            return Ok(structured(serde_json::json!({
                "epoch": 0,
                "lexical_hits": [],
                "semantic_hits": [],
                "fused": [],
                "hits": [],
            })));
        }

        // Embed the query when semantic is on, off the loaded indexer
        // embedder. Match the UI's spawn_blocking hop.
        let embedding = if modes.semantic {
            match (self.state.embedder_provider)() {
                Some(e) => {
                    let q = p.query.clone();
                    let res =
                        tokio::task::spawn_blocking(move || e.embed_batch(&[q]))
                            .await
                            .map_err(|e| {
                                ErrorData::internal_error(e.to_string(), None)
                            })?;
                    match res {
                        Ok(v) => v.into_iter().next(),
                        Err(_) => return Err(hiker_err(ErrorCode(1005), "embedder unavailable")),
                    }
                }
                None => return Err(hiker_err(ErrorCode(1005), "embedder not yet loaded")),
            }
        } else {
            None
        };

        let effective = Modes {
            lexical: modes.lexical,
            semantic: modes.semantic && embedding.is_some(),
        };

        let read_store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let mut resp = search::query(
            &read_store,
            0,
            effective,
            Some(&p.query),
            embedding.as_deref(),
            search::LexicalOpts::default(),
            search::SemanticOpts::default(),
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if (resp.fused.len() as u32) > requested {
            resp.fused.truncate(requested as usize);
        }
        if (resp.hits.len() as u32) > requested {
            resp.hits.truncate(requested as usize);
        }

        Ok(structured(
            serde_json::to_value(&resp)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
        ))
    }

    /// The note content the agent should see on a read: its own op-log
    /// replica — `materialize_pending_view(session = agent)`, i.e. accepted
    /// plus the agent's own queued pending ops — so a follow-up `get_note`
    /// reflects edits the agent just staged, even before the user accepts
    /// them (`op-log-agent-replica`). Returns `Ok(None)` when there is no op
    /// log or the path has no doc yet, so callers fall back to on-disk bytes.
    fn agent_view_content(&self, rel: &str) -> Result<Option<(String, String)>, ErrorData> {
        let Some(op_log) = self.state.oplog.as_ref() else {
            return Ok(None);
        };
        // `review_materializations` resolves the path → doc_id and returns
        // `(accepted, pending_view(session))`; the pending view scoped to the
        // agent's own session is the agent replica. `None` when the path has
        // no op-log doc yet.
        let Some((_accepted, pending_view)) =
            op_writes::review_materializations(op_log, rel, Some(CLIENT_ID))
                .map_err(translate_hiker_err)?
        else {
            return Ok(None);
        };
        let hash = hiker_core::hash_string(&pending_view);
        Ok(Some((pending_view, hash)))
    }

    pub(super) async fn read_note(&self, p: &GetNote) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("get_note")?;
        // Existence check up front so we can return 1002 cleanly rather
        // than relying on the io error from read_file.
        let abs = self
            .state
            .vault
            .abs_path(&p.rel_path)
            .map_err(translate_hiker_err)?;
        if !abs.exists() {
            return Err(hiker_err(
                ErrorCode(1002),
                format!("note not found: {}", p.rel_path),
            ));
        }
        let title = self.title_from_rel_path(&p.rel_path);
        match p.detail {
            NoteDetail::Digest => Ok(structured(
                serde_json::to_value(GetNoteDigest {
                    rel_path: p.rel_path.clone(),
                    title,
                    detail: "digest",
                })
                .unwrap_or(serde_json::Value::Null),
            )),
            NoteDetail::Snippet => {
                // Pull the highest-scoring chunk (chunk 0) for snippet mode
                // when an indexed row exists; fall back to the head of the
                // file otherwise. Spec: "top-1 chunk + heading_path".
                let (snippet, heading_path) = match self.first_chunk(&p.rel_path) {
                    Ok(Some(c)) => (c.text, c.heading_path),
                    _ => {
                        let raw = match self.agent_view_content(&p.rel_path)? {
                            Some((text, _)) => text,
                            None => self
                                .state
                                .vault
                                .read_file(&p.rel_path)
                                .map_err(translate_hiker_err)?,
                        };
                        (self.head_snippet(&raw), None)
                    }
                };
                Ok(structured(
                    serde_json::to_value(GetNoteSnippet {
                        rel_path: p.rel_path.clone(),
                        title,
                        detail: "snippet",
                        heading_path,
                        snippet,
                    })
                    .unwrap_or(serde_json::Value::Null),
                ))
            }
            NoteDetail::Full => {
                let (content, hash) = match self.agent_view_content(&p.rel_path)? {
                    Some(cv) => cv,
                    None => self
                        .state
                        .vault
                        .read_file_with_hash(&p.rel_path)
                        .map_err(translate_hiker_err)?,
                };
                Ok(structured(
                    serde_json::to_value(GetNoteFull {
                        rel_path: p.rel_path.clone(),
                        title,
                        detail: "full",
                        content,
                        content_hash: hash,
                    })
                    .unwrap_or(serde_json::Value::Null),
                ))
            }
        }
    }

    fn first_chunk(
        &self,
        rel: &str,
    ) -> Result<Option<hiker_core::store::dto::ChunkRow>, hiker_core::errors::HikerError> {
        use hiker_core::errors::HikerError;
        let store = self
            .state
            .read_store
            .lock()
            .map_err(|_| HikerError::Io("read_store mutex poisoned".into()))?;
        let id = match store.id_for_path(rel).map_err(|e| HikerError::Io(e.to_string()))? {
            Some(id) => id,
            None => return Ok(None),
        };
        let mut chunks = store
            .get_note_chunks(&id)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        if chunks.is_empty() {
            Ok(None)
        } else {
            Ok(Some(chunks.swap_remove(0)))
        }
    }

    pub(super) async fn find_related(
        &self,
        p: &RelatedNotes,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("related_notes")?;
        let max_top_k = self.state.config.max_top_k.max(1);
        let top_k = p.top_k.unwrap_or(10).clamp(1, max_top_k) as usize;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let id = store
            .id_for_path(&p.rel_path)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = match id {
            Some(id) => store
                .related_notes(&id, top_k)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
            None => Vec::new(),
        };
        Ok(structured(
            serde_json::to_value(&hits)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
        ))
    }

    /// Stage a whole-body rewrite (`write_note` / `set_frontmatter` /
    /// `apply_tag` / `remove_tag` review shapes) as one anchorless op-log
    /// pending op authored `agent:<client_id>`, returning the minted op id
    /// for the tool's `staging_id` field. The op-log diffs the new text
    /// against current accepted, so an unchanged whole-body produces no op
    /// (returns `None`).
    fn stage_whole_body(
        &self,
        op_log: &hiker_core::oplog::OpLog,
        rel_path: &str,
        new_content: &str,
    ) -> Result<Option<String>, ErrorData> {
        let outcome = op_writes::stage_agent_edits(
            op_log,
            &self.state.vault,
            CLIENT_ID,
            "mcp-tool-call",
            rel_path,
            &[op_writes::AgentEdit {
                old_str: None,
                new_str: new_content.to_string(),
            }],
        )
        .map_err(translate_hiker_err)?;
        Ok(outcome.op_ids.into_iter().next())
    }

    pub(super) async fn save_note(&self, p: &WriteNote) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("write_note")?;

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            // Review mode: stage the whole-body rewrite as one anchorless
            // op-log pending op (`write_note` → whole-body `Replace`), the
            // same op-log review path `edit_note` uses. Nothing reaches disk
            // until the user accepts; the returned op id is the review handle.
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "review mode requires an open op log".to_string(),
                    None,
                )
            })?;
            let staging_id = self.stage_whole_body(op_log, &p.rel_path, &p.content)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::agent::WriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                op_log: self.state.oplog.as_ref(),
                client_id: CLIENT_ID,
            };
            let new_hash = ops::agent::write_note(
                &ctx,
                &p.rel_path,
                &p.content,
                p.expected_hash.as_deref(),
            )
            .await
            .map_err(translate_hiker_err)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: new_hash,
                    status: None,
                    staging_id: None,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        }
    }

    /// status: mcp-tool-edit-note
    /// status: mcp-edit-note-validation
    pub(super) async fn apply_edits(&self, p: &EditNote) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("edit_note")?;
        if p.edits.is_empty() {
            return Err(ErrorData::invalid_params(
                "edits: must contain at least one edit",
                None,
            ));
        }

        // Rule 1: path must exist. Creates flow through `write_note`.
        let abs = self
            .state
            .vault
            .abs_path(&p.rel_path)
            .map_err(translate_hiker_err)?;
        if !abs.exists() {
            return Err(hiker_err(
                ErrorCode(1002),
                format!("note not found (use write_note to create): {}", p.rel_path),
            ));
        }

        // Read the pre-application content once; every anchor resolves against
        // it (rule 4). This is the agent's own op-log replica — accepted plus
        // the agent's queued pending ops — the same view `get_note` returns, so
        // a follow-up edit can anchor on text the agent staged in a prior,
        // not-yet-accepted edit (`op-log-agent-replica`). Falls back to disk
        // when the path has no op-log doc.
        let (pre_content, pre_hash) = match self.agent_view_content(&p.rel_path)? {
            Some(cv) => cv,
            None => self
                .state
                .vault
                .read_file_with_hash(&p.rel_path)
                .map_err(translate_hiker_err)?,
        };

        // Rule 2 + 3: per-edit anchor uniqueness, then cross-edit overlap.
        // Collect ranges in input order so error messages name the offending
        // edit by its source index.
        let mut per_edit_ranges: Vec<Vec<(usize, usize)>> = Vec::with_capacity(p.edits.len());
        for (idx, e) in p.edits.iter().enumerate() {
            if e.old_str.is_empty() {
                return Err(ErrorData::invalid_params(
                    format!("edit[{idx}]: old_str must not be empty"),
                    None,
                ));
            }
            let matches = find_all_matches(&pre_content, &e.old_str);
            if matches.is_empty() {
                return Err(hiker_err(
                    ErrorCode(1003),
                    format!("drift: edit[{idx}].old_str not found in {}", p.rel_path),
                ));
            }
            if matches.len() > 1 && !e.replace_all {
                return Err(ErrorData::invalid_params(
                    format!(
                        "edit[{idx}].old_str matches {} ranges; pass replace_all=true to replace all",
                        matches.len(),
                    ),
                    None,
                ));
            }
            per_edit_ranges.push(matches);
        }

        // No-overlap check across edits. Flatten (range, edit_idx) and
        // sort by start; consecutive overlapping ranges expose the pair.
        let mut flat: Vec<(usize, usize, usize)> = Vec::new();
        for (idx, ranges) in per_edit_ranges.iter().enumerate() {
            for (s, e) in ranges {
                flat.push((*s, *e, idx));
            }
        }
        flat.sort_by_key(|r| r.0);
        for w in flat.windows(2) {
            let (_, a_end, a_idx) = w[0];
            let (b_start, _, b_idx) = w[1];
            if b_start < a_end {
                return Err(ErrorData::invalid_params(
                    format!(
                        "edits[{a_idx}] and edits[{b_idx}] resolve to overlapping byte ranges; merge them into one edit with a wider span",
                    ),
                    None,
                ));
            }
        }

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            // Review mode: stage the edits as op-log pending ops sharing a
            // batch_id (per `op-log.md`'s `edit_note([e1,e2,…])` → one
            // `Replace` per edit). Each edit becomes one anchored
            // `AgentEdit { old_str, new_str }`; accept/reject each
            // independently flow through the op-log review surfaces re-homed
            // in Phases 3b–3d. A `replace_all` edit with more than one match
            // can't be expressed as a single anchored op (the anchor must
            // resolve uniquely), so the whole call collapses to one anchorless
            // whole-body rewrite carrying the cumulative result — the user
            // still reviews the net change, just not per-edit.
            //
            // status: mcp-tool-edit-note
            // status: op-log-ops-producer-helpers
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "review mode requires an open op log".to_string(),
                    None,
                )
            })?;
            let any_multi_replace_all = p.edits.iter().any(|e| {
                e.replace_all && find_all_matches(&pre_content, &e.old_str).len() > 1
            });
            let edits: Vec<op_writes::AgentEdit> = if any_multi_replace_all {
                let mut current = pre_content.clone();
                for e in &p.edits {
                    let payload = EditPayload {
                        old_str: e.old_str.clone(),
                        new_str: e.new_str.clone(),
                        replace_all: e.replace_all,
                    };
                    current = apply_edit(&current, &payload).map_err(|err| {
                        hiker_err(ErrorCode(1003), format!("drift: {err}"))
                    })?;
                }
                vec![op_writes::AgentEdit { old_str: None, new_str: current }]
            } else {
                p.edits
                    .iter()
                    .map(|e| op_writes::AgentEdit {
                        old_str: Some(e.old_str.clone()),
                        new_str: e.new_str.clone(),
                    })
                    .collect()
            };
            let outcome = op_writes::stage_agent_edits(
                op_log,
                &self.state.vault,
                CLIENT_ID,
                "mcp-tool-call",
                &p.rel_path,
                &edits,
            )
            .map_err(translate_hiker_err)?;
            return Ok(structured(
                serde_json::to_value(EditOutcome {
                    rel_path: p.rel_path.clone(),
                    status: "staged",
                    edit_count: p.edits.len() as u32,
                    content_hash: None,
                    staging_ids: outcome.op_ids,
                    batch_id: Some(outcome.batch_id),
                })
                .unwrap_or(serde_json::Value::Null),
            ));
        }

        // Direct mode: apply all edits transactionally and write once.
        let mut current = pre_content.clone();
        for e in &p.edits {
            let payload = EditPayload {
                old_str: e.old_str.clone(),
                new_str: e.new_str.clone(),
                replace_all: e.replace_all,
            };
            current = apply_edit(&current, &payload).map_err(|err| {
                // Validation above means this should be unreachable; if a
                // race occurred between validation and apply, surface it as
                // 1003 drift rather than internal-error.
                hiker_err(ErrorCode(1003), format!("drift: {err}"))
            })?;
        }
        let ctx = ops::agent::WriteCtx {
            watcher: &self.state.watcher,
            jobs: &self.state.jobs,
            vault: &self.state.vault,
            op_log: self.state.oplog.as_ref(),
            client_id: CLIENT_ID,
        };
        let new_hash = ops::agent::write_note(
            &ctx,
            &p.rel_path,
            &current,
            Some(&pre_hash),
        )
        .await
        .map_err(translate_hiker_err)?;
        Ok(structured(
            serde_json::to_value(EditOutcome {
                rel_path: p.rel_path.clone(),
                status: "written",
                edit_count: p.edits.len() as u32,
                content_hash: Some(new_hash),
                staging_ids: Vec::new(),
                batch_id: None,
            })
            .unwrap_or(serde_json::Value::Null),
        ))
    }

    pub(super) async fn merge_frontmatter(
        &self,
        p: &SetFrontmatter,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("set_frontmatter")?;

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            // Review mode: merge the frontmatter patch into the current
            // content and stage the whole-body result as one op-log pending
            // op. The op-log labels it `SetFrontmatter` automatically when the
            // change lands inside the frontmatter fence.
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "review mode requires an open op log".to_string(),
                    None,
                )
            })?;
            let existing = self
                .state
                .vault
                .read_file(&p.rel_path)
                .map_err(translate_hiker_err)?;
            let merged = frontmatter::merge_agent_patch(
                &existing,
                serde_json::Value::Object(p.fields.clone()),
            )
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let staging_id = self.stage_whole_body(op_log, &p.rel_path, &merged)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::agent::WriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                op_log: self.state.oplog.as_ref(),
                client_id: CLIENT_ID,
            };
            let new_hash = ops::agent::set_frontmatter(
                &ctx,
                &p.rel_path,
                serde_json::Value::Object(p.fields.clone()),
            )
            .await
            .map_err(translate_hiker_err)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: new_hash,
                    status: None,
                    staging_id: None,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        }
    }

    pub(super) async fn update_tag(
        &self,
        p: &ApplyTag,
        add: bool,
    ) -> Result<CallToolResult, ErrorData> {
        let tool_name = if add { "apply_tag" } else { "remove_tag" };
        self.guard_tool(tool_name)?;

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            // Review mode: resolve the new tag list, merge into frontmatter,
            // and stage the whole-body result as one op-log pending op (same
            // merge-into-write shape as the direct path, but staged for
            // review). The op-log labels it `SetFrontmatter` when the change
            // lands inside the frontmatter fence.
            let op_log = self.state.oplog.as_ref().ok_or_else(|| {
                ErrorData::internal_error(
                    "review mode requires an open op log".to_string(),
                    None,
                )
            })?;
            let existing = self
                .state
                .vault
                .read_file(&p.rel_path)
                .map_err(translate_hiker_err)?;
            let split = frontmatter::split(&existing);
            let existing_tags: Vec<String> = match split.frontmatter {
                Some(serde_yml::Value::Mapping(ref m)) => match m.get("tags") {
                    Some(serde_yml::Value::Sequence(seq)) => seq
                        .iter()
                        .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
                        .collect(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let mut tags = existing_tags;
            if add {
                if !tags.iter().any(|t| t == &p.tag) {
                    tags.push(p.tag.clone());
                }
            } else {
                tags.retain(|t| t != &p.tag);
            }
            let merged = frontmatter::merge_agent_patch(
                &existing,
                serde_json::json!({"tags": tags}),
            )
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let staging_id = self.stage_whole_body(op_log, &p.rel_path, &merged)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::agent::WriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                op_log: self.state.oplog.as_ref(),
                client_id: CLIENT_ID,
            };
            let result = if add {
                ops::agent::apply_tag(&ctx, &p.rel_path, &p.tag).await
            } else {
                ops::agent::remove_tag(&ctx, &p.rel_path, &p.tag).await
            };
            let new_hash = result.map_err(translate_hiker_err)?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: new_hash,
                    status: None,
                    staging_id: None,
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        }
    }
}

// ---------- board-shaped tool implementations ----------

impl App {
    /// status: board-mcp-tools
    /// Enumerate every board-doc in the vault (read-only). Mirrors the trail
    /// `trails_list` shape: one row per board with id/title/path + column and
    /// card counts.
    pub(super) async fn enumerate_boards(
        &self,
        _p: &BoardsList,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("boards_list")?;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let rows = hiker_core::boards::list(&self.state.vault, &store)
            .map_err(translate_hiker_err)?;
        Ok(structured(
            serde_json::to_value(&rows).unwrap_or(serde_json::Value::Null),
        ))
    }

    /// status: board-mcp-tools
    /// Full detail for one board: body + resolved columns/cards (read-only).
    pub(super) async fn fetch_board(&self, p: &BoardGet) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("board_get")?;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        let detail = hiker_core::boards::get_board(&self.state.vault, &store, &p.rel_path)
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
    pub(super) async fn add_board_card(
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
            let new_src = {
                let store = self.state.read_store.lock().map_err(|_| {
                    ErrorData::internal_error("read_store mutex poisoned", None)
                })?;
                hiker_core::boards::add_card_preview(
                    &self.state.vault,
                    &store,
                    &p.board_rel_path,
                    &p.column,
                    &p.source_rel_path,
                )
                .map_err(translate_hiker_err)?
            };
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
            let new_src = {
                let store = self.state.read_store.lock().map_err(|_| {
                    ErrorData::internal_error("read_store mutex poisoned", None)
                })?;
                hiker_core::boards::add_card_preview(
                    &self.state.vault,
                    &store,
                    &p.board_rel_path,
                    &p.column,
                    &p.source_rel_path,
                )
                .map_err(translate_hiker_err)?
            };
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
}

// ---------- task-shaped tool implementations ----------

impl App {
    pub(super) async fn lease_task(
        &self,
        p: &TaskCheckout,
        via: McpClientVia,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_checkout")?;
        let lease_secs = p
            .lease_secs
            .unwrap_or(self.state.default_lease_secs)
            .min(self.state.max_lease_secs)
            .max(1);
        let shapes: Option<Vec<TaskShapeKind>> = p
            .shapes
            .as_ref()
            .map(|v| v.iter().map(|s| (*s).into()).collect());
        let min_priority: TaskPriority = p
            .min_priority
            .as_ref()
            .map(|m| (*m).into())
            .unwrap_or(TaskPriority::Low);
        let kinds: Option<&[String]> = p.types.as_deref();

        let task = self
            .state
            .tasks
            .checkout_mcp(
                CLIENT_ID,
                via,
                kinds,
                shapes.as_deref(),
                min_priority,
                lease_secs,
            )
            .await;
        let payload = match task {
            None => serde_json::Value::Null,
            Some(t) => {
                let lease_expires_ms = (std::time::SystemTime::now()
                    + std::time::Duration::from_secs(lease_secs))
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
                serde_json::json!({
                    "task_id": t.id,
                    "kind": t.kind,
                    "shape": t.shape,
                    "priority": t.priority,
                    "payload": t.payload,
                    "output_schema": t.output_schema,
                    "metadata": t.metadata,
                    "lease_expires_at_ms": lease_expires_ms,
                })
            }
        };
        Ok(structured(payload))
    }

    pub(super) async fn record_task_result(
        &self,
        p: &TaskSubmit,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_submit")?;
        match self
            .state
            .tasks
            .submit_result(&p.task_id, p.value.clone())
            .await
        {
            Ok(()) => Ok(structured(serde_json::json!({"ok": true}))),
            Err(QueueError::SchemaViolation(msg)) => {
                Err(hiker_err(ErrorCode(1007), format!("schema_violation: {msg}")))
            }
            Err(QueueError::StaleLease) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(QueueError::NotFound(id)) => Err(hiker_err(
                ErrorCode(1006),
                format!("stale_lease: task not found: {id}"),
            )),
            Err(QueueError::InvalidState(s)) => {
                Err(ErrorData::internal_error(s, None))
            }
        }
    }

    pub(super) async fn record_task_failure(
        &self,
        p: &TaskFail,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_fail")?;
        match self
            .state
            .tasks
            .fail(&p.task_id, p.error.clone())
            .await
        {
            Ok(()) => Ok(structured(serde_json::json!({"ok": true}))),
            Err(QueueError::StaleLease) | Err(QueueError::NotFound(_)) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    pub(super) async fn extend_task_lease(
        &self,
        p: &TaskHeartbeat,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_heartbeat")?;
        let lease_secs = self.state.default_lease_secs;
        match self.state.tasks.heartbeat(&p.task_id, lease_secs).await {
            Ok(expires) => {
                let expires_ms = expires
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Ok(structured(
                    serde_json::json!({"lease_expires_at_ms": expires_ms}),
                ))
            }
            Err(QueueError::StaleLease) | Err(QueueError::NotFound(_)) => {
                Err(hiker_err(ErrorCode(1006), "stale_lease"))
            }
            Err(e) => Err(ErrorData::internal_error(e.to_string(), None)),
        }
    }

    pub(super) async fn list_tasks(&self, p: &TaskList) -> Result<CallToolResult, ErrorData> {
        self.guard_tasks()?;
        self.guard_tool("task_list")?;
        let states: Option<Vec<TaskState>> = p
            .states
            .as_ref()
            .map(|v| v.iter().map(|s| (*s).into()).collect());
        let rows = self
            .state
            .tasks
            .list(states.as_deref(), p.types.as_deref())
            .await;
        Ok(structured(
            serde_json::to_value(&rows).unwrap_or(serde_json::Value::Null),
        ))
    }
}
