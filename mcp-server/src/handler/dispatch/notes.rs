use hiker_core::frontmatter;
use hiker_core::ops;
use hiker_core::search::{self, Modes};
use hiker_core::ops::op_writes;
use hiker_core::textpatch::{apply_edit, find_all_matches, EditPayload};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};

use super::App;
use crate::handler::params::{
    hiker_err, structured, translate_hiker_err, ApplyTag, EditNote,
    GetNote, GetNoteDigest, GetNoteFull, GetNoteSnippet, NoteDetail, RelatedNotes, SearchNotes,
    SetFrontmatter, WriteNote, WriteOutcome, EditOutcome, CLIENT_ID,
};

impl App {
    pub(in crate::handler) async fn run_search(&self, p: &SearchNotes) -> Result<CallToolResult, ErrorData> {
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
    pub(super) fn agent_view_content(&self, rel: &str) -> Result<Option<(String, String)>, ErrorData> {
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

    pub(in crate::handler) async fn read_note(&self, p: &GetNote) -> Result<CallToolResult, ErrorData> {
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
        // status: store-id-from-oplog
        let id = match store
            .get_note_by_path(rel)
            .map_err(|e| HikerError::Io(e.to_string()))?
        {
            Some(row) => row.id,
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

    pub(in crate::handler) async fn find_related(
        &self,
        p: &RelatedNotes,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("related_notes")?;
        let max_top_k = self.state.config.max_top_k.max(1);
        let top_k = p.top_k.unwrap_or(10).clamp(1, max_top_k) as usize;
        let store = self.state.read_store.lock().map_err(|_| {
            ErrorData::internal_error("read_store mutex poisoned", None)
        })?;
        // status: store-id-from-oplog
        let row = store
            .get_note_by_path(&p.rel_path)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let hits = match row {
            Some(r) => store
                .related_notes(&r.id, top_k)
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
    pub(super) fn stage_whole_body(
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

    pub(in crate::handler) async fn save_note(&self, p: &WriteNote) -> Result<CallToolResult, ErrorData> {
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
    pub(in crate::handler) async fn apply_edits(&self, p: &EditNote) -> Result<CallToolResult, ErrorData> {
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

    pub(in crate::handler) async fn merge_frontmatter(
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

    pub(in crate::handler) async fn update_tag(
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
