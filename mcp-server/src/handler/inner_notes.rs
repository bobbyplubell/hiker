//! Inner helpers for the note-shaped tools (search / get / related / write
//! / edit / set_frontmatter / apply_tag). These are the actual workhorse
//! implementations; the `#[tool_router]` methods in `router.rs` simply
//! audit + delegate here.

use super::*;

// ---------- inner helpers ----------

impl HikerHandler {
    pub(super) async fn search_notes_inner(
        &self,
        p: &SearchNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("search_notes")?;
        let modes = match &p.modes {
            Some(m) => SearchModes { semantic: m.semantic, lexical: m.lexical },
            None => SearchModes { semantic: true, lexical: true },
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

        let effective = SearchModes {
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

    pub(super) async fn get_note_inner(&self, p: &GetNoteParams) -> Result<CallToolResult, ErrorData> {
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
        let title = title_from_rel_path(&p.rel_path);
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
                        let raw = self
                            .state
                            .vault
                            .read_file(&p.rel_path)
                            .map_err(translate_hiker_err)?;
                        (head_snippet(&raw), None)
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
                let (content, hash) = self
                    .state
                    .vault
                    .read_file_with_hash(&p.rel_path)
                    .map_err(translate_hiker_err)?;
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
    ) -> Result<Option<hiker_core::store::ChunkRow>, HikerError> {
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

    pub(super) async fn related_notes_inner(
        &self,
        p: &RelatedNotesParams,
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

    pub(super) async fn write_note_inner(&self, p: &WriteNoteParams) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("write_note")?;

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
            // status: staging-proposal-state — capture propose-time disk hash
            // so eager recheck can detect drift before accept. `None` here is
            // the create-shaped case (target path doesn't yet exist).
            let source_hash = self
                .state
                .vault
                .read_file_with_hash(&p.rel_path)
                .ok()
                .map(|(_, h)| h);
            let staging_id = self
                .state
                .staging
                .propose(ProposalInput {
                    surface: "mcp-tool-call".into(),
                    action: "write_note".into(),
                    target_path: p.rel_path.clone(),
                    trail_id: None,
                    content: Some(p.content.clone()),
                    metadata: Some(serde_json::json!({
                        "tool": "write_note",
                        "session_id": CLIENT_ID,
                    })),
                    source_hash,
                    source_path: None,
                })
                .map_err(|e| {
                    tracing::error!(error = %e, "staging: propose failed");
                    ErrorData::internal_error(e.to_string(), None)
                })?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id: Some(staging_id),
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::AgentWriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                changes: Some(&self.state.changes),
                client_id: CLIENT_ID,
                tool: "write_note",
            };
            let new_hash = ops::agent_write_note(
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
    pub(super) async fn edit_note_inner(
        &self,
        p: &EditNoteParams,
    ) -> Result<CallToolResult, ErrorData> {
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

        // Read the pre-application file once; every anchor resolves against
        // this content (rule 4).
        let (pre_content, pre_hash) = self
            .state
            .vault
            .read_file_with_hash(&p.rel_path)
            .map_err(translate_hiker_err)?;

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
                        "edits[{}] and edits[{}] resolve to overlapping byte ranges; merge them into one edit with a wider span",
                        a_idx, b_idx,
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
            // Split into N staging proposals, one per edit, sharing a batch_id.
            // `.md` sidecar stores `new_str` (the post-edit span content) for
            // UI preview; accept re-resolves the anchor against current disk.
            let mut inputs = Vec::with_capacity(p.edits.len());
            for e in &p.edits {
                let edit_payload = EditPayload {
                    old_str: e.old_str.clone(),
                    new_str: e.new_str.clone(),
                    replace_all: e.replace_all,
                };
                inputs.push(EditProposalInput {
                    surface: "mcp-tool-call".into(),
                    action: "edit_note".into(),
                    target_path: p.rel_path.clone(),
                    content: Some(e.new_str.clone()),
                    metadata: Some(serde_json::json!({
                        "tool": "edit_note",
                        "session_id": CLIENT_ID,
                        "pre_content_hash": pre_hash,
                    })),
                    edit: edit_payload,
                    // status: staging-proposal-state
                    source_hash: Some(pre_hash.clone()),
                });
            }
            let batch = self.state.staging.propose_batch(inputs).map_err(|e| {
                tracing::error!(error = %e, "staging: propose_batch failed");
                ErrorData::internal_error(e.to_string(), None)
            })?;
            return Ok(structured(
                serde_json::to_value(EditOutcome {
                    rel_path: p.rel_path.clone(),
                    status: "staged",
                    edit_count: p.edits.len() as u32,
                    content_hash: None,
                    staging_ids: batch.ids,
                    batch_id: Some(batch.batch_id),
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
        let ctx = ops::AgentWriteCtx {
            watcher: &self.state.watcher,
            jobs: &self.state.jobs,
            vault: &self.state.vault,
            changes: Some(&self.state.changes),
            client_id: CLIENT_ID,
            tool: "edit_note",
        };
        let new_hash = ops::agent_write_note(
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

    pub(super) async fn set_frontmatter_inner(
        &self,
        p: &SetFrontmatterParams,
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
            // status: staging-proposal-state — capture propose-time disk hash
            // alongside the read used for the frontmatter merge.
            let (existing, source_hash) = self
                .state
                .vault
                .read_file_with_hash(&p.rel_path)
                .map_err(translate_hiker_err)?;
            let merged = frontmatter::merge_agent_patch(
                &existing,
                serde_json::Value::Object(p.fields.clone()),
            )
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
            let staging_id = self
                .state
                .staging
                .propose(ProposalInput {
                    surface: "mcp-tool-call".into(),
                    action: "set_frontmatter".into(),
                    target_path: p.rel_path.clone(),
                    trail_id: None,
                    content: Some(merged),
                    metadata: Some(serde_json::json!({
                        "tool": "set_frontmatter",
                        "session_id": CLIENT_ID,
                    })),
                    source_hash: Some(source_hash),
                    source_path: None,
                })
                .map_err(|e| {
                    tracing::error!(error = %e, "staging: propose failed");
                    ErrorData::internal_error(e.to_string(), None)
                })?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id: Some(staging_id),
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::AgentWriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                changes: Some(&self.state.changes),
                client_id: CLIENT_ID,
                tool: "set_frontmatter",
            };
            let new_hash = ops::agent_set_frontmatter(
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

    pub(super) async fn apply_tag_inner(
        &self,
        p: &ApplyTagParams,
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
            // status: staging-proposal-state — propose-time disk hash for eager
            // drift recheck.
            let (existing, source_hash) = self
                .state
                .vault
                .read_file_with_hash(&p.rel_path)
                .map_err(translate_hiker_err)?;
            // Read existing tags from the source so staging captures the
            // full resolved content (same merge-into-write shape as the
            // direct path, but routed through staging).
            let split = frontmatter::split(&existing);
            let existing_tags: Vec<String> = match split.frontmatter {
                Some(serde_yml::Value::Mapping(ref m)) => match m.get("tags") {
                    Some(serde_yml::Value::Sequence(seq)) => seq
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
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
            let staging_id = self
                .state
                .staging
                .propose(ProposalInput {
                    surface: "mcp-tool-call".into(),
                    action: tool_name.into(),
                    target_path: p.rel_path.clone(),
                    trail_id: None,
                    content: Some(merged),
                    metadata: Some(serde_json::json!({
                        "tool": tool_name,
                        "session_id": CLIENT_ID,
                    })),
                    source_hash: Some(source_hash),
                    source_path: None,
                })
                .map_err(|e| {
                    tracing::error!(error = %e, "staging: propose failed");
                    ErrorData::internal_error(e.to_string(), None)
                })?;
            Ok(structured(
                serde_json::to_value(WriteOutcome {
                    rel_path: p.rel_path.clone(),
                    content_hash: String::new(),
                    status: Some("staged".into()),
                    staging_id: Some(staging_id),
                })
                .unwrap_or(serde_json::Value::Null),
            ))
        } else {
            let ctx = ops::AgentWriteCtx {
                watcher: &self.state.watcher,
                jobs: &self.state.jobs,
                vault: &self.state.vault,
                changes: Some(&self.state.changes),
                client_id: CLIENT_ID,
                tool: tool_name,
            };
            let result = if add {
                ops::agent_apply_tag(&ctx, &p.rel_path, &p.tag).await
            } else {
                ops::agent_remove_tag(&ctx, &p.rel_path, &p.tag).await
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
