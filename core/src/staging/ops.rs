use super::*;

impl Staging {
    pub fn propose(&self, input: ProposalInput) -> Result<String, StagingError> {
        let id = Ulid::new().to_string();
        let content_hash = input.content.as_ref().map(|c| hash_str(c));
        let encoded_content: Option<Vec<u8>> = match input.content.as_ref() {
            Some(c) => Some(zstd::encode_all(c.as_bytes(), ZSTD_LEVEL)?),
            None => None,
        };
        let metadata_str = input
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.with_conn(|conn| {
            conn.execute(
                INSERT_SQL,
                params![
                    id,
                    input.surface,
                    input.action,
                    input.target_path,
                    input.trail_id,
                    content_hash,
                    encoded_content,
                    now_ms(),
                    Option::<String>::None,         // batch_id
                    Option::<String>::None,         // edit_old_str
                    Option::<String>::None,         // edit_new_str
                    Option::<i64>::None,            // edit_replace_all
                    ProposalState::Applyable.as_str(),
                    Option::<String>::None,         // conflict_reason
                    input.source_hash,
                    metadata_str,
                    Option::<i64>::None,            // amended_at_ms
                    0i64,                           // amend_count
                    input.source_path,
                ],
            )?;
            Ok(())
        })?;
        let _ = self.changed_tx.send(());
        Ok(id)
    }

    /// status: staging-per-edit-proposals
    pub fn propose_batch(
        &self,
        inputs: Vec<EditProposalInput>,
    ) -> Result<BatchOutcome, StagingError> {
        let batch_id = Ulid::new().to_string();
        let mut ids = Vec::with_capacity(inputs.len());

        self.with_conn_mut(|conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(INSERT_SQL)?;
                for input in &inputs {
                    let id = Ulid::new().to_string();
                    let content_hash = input.content.as_ref().map(|c| hash_str(c));
                    let encoded_content: Option<Vec<u8>> = match input.content.as_ref() {
                        Some(c) => Some(zstd::encode_all(c.as_bytes(), ZSTD_LEVEL)?),
                        None => None,
                    };
                    let metadata_str = input
                        .metadata
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?;

                    stmt.execute(params![
                        id,
                        input.surface,
                        input.action,
                        input.target_path,
                        Option::<String>::None,
                        content_hash,
                        encoded_content,
                        now_ms(),
                        Some(&batch_id),
                        Some(&input.edit.old_str),
                        Some(&input.edit.new_str),
                        Some(if input.edit.replace_all { 1i64 } else { 0i64 }),
                        ProposalState::Applyable.as_str(),
                        Option::<String>::None,
                        input.source_hash,
                        metadata_str,
                        Option::<i64>::None,
                        0i64,
                        // Edit-batch rows are content-shaped, never move_note.
                        Option::<String>::None,
                    ])?;
                    ids.push(id);
                }
            }
            tx.commit()?;
            Ok(())
        })?;
        let _ = self.changed_tx.send(());
        Ok(BatchOutcome { batch_id, ids })
    }

    pub fn accept(
        &self,
        id: &str,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<AcceptOutcome, StagingError> {
        let proposal = self
            .get_full(id)?
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;

        // status: staging-action-move-note
        // Route move_note rows separately from content writes — the
        // accept path is a filesystem rename, not a buffer write. The
        // safety-net re-anchor here matches the write-row hash recheck:
        // we re-verify source/target existence at accept time, since
        // disk state may have drifted between the eager-recheck and
        // the user clicking Accept.
        if proposal.action == ACTION_MOVE_NOTE {
            return self.accept_move(id, &proposal, vault, changes);
        }

        let proposed_content: Option<String>;
        let new_hash: String;
        let is_create: bool;

        if let Some(ref edit) = proposal.edit {
            // status: staging-per-edit-proposals
            let (disk_content, disk_hash) =
                vault.read_file_with_hash(&proposal.target_path)?;
            // Baseline-on-first-touch: snapshot pre-write state so rollback
            // of this user-accepted agent edit has somewhere to go. Mirrors
            // `ops::agent_write_note` and `ops::commit_buffer`.
            if let Some(c) = changes
                && let Err(e) = c.ensure_baseline(
                    &proposal.target_path,
                    "user",
                    disk_content.as_bytes(),
                    &disk_hash,
                )
            {
                tracing::warn!(error = %e, "changes: ensure_baseline failed (staging accept edit)");
            }
            let applied = apply_edit(&disk_content, edit)?;
            new_hash = vault.write_file_checked(
                &proposal.target_path,
                &disk_hash,
                &applied,
            )?;
            is_create = false;
            proposed_content = Some(applied);
        } else if let Some(ref content_hash) = proposal.content_hash {
            let content = self
                .read_content(id)?
                .ok_or_else(|| StagingError::MissingContent(id.to_string()))?;

            let actual_hash = hash_str(&content);
            if &actual_hash != content_hash {
                return Err(StagingError::DiskDrift {
                    expected: content_hash.clone(),
                    found: actual_hash,
                });
            }

            let disk_read = vault.read_file_with_hash(&proposal.target_path);
            let file_exists = disk_read.is_ok();
            let (disk_text, disk_hash) =
                disk_read.unwrap_or((String::new(), String::new()));

            // Baseline-on-first-touch: snapshot pre-write state for existing
            // files so rollback of this user-accepted agent write has
            // somewhere to go. Skipped for creates — there's no prior state.
            if file_exists
                && let Some(c) = changes
                && let Err(e) = c.ensure_baseline(
                    &proposal.target_path,
                    "user",
                    disk_text.as_bytes(),
                    &disk_hash,
                )
            {
                tracing::warn!(error = %e, "changes: ensure_baseline failed (staging accept write)");
            }

            new_hash = vault.write_file_checked(
                &proposal.target_path,
                &disk_hash,
                &content,
            )?;
            is_create = !file_exists;
            proposed_content = Some(content);
        } else {
            new_hash = String::new();
            is_create = false;
            proposed_content = None;
        }

        if let Some(changes) = changes {
            let op = if is_create {
                ChangeOp::Created
            } else if proposal.action == "delete_note" || proposal.action == "waypoint_remove" {
                ChangeOp::Deleted
            } else {
                ChangeOp::Modified
            };
            let content_bytes = proposed_content.as_ref().map(|c| c.as_bytes().to_vec());
            changes.append(ChangeAppend {
                path: &proposal.target_path,
                op,
                author: "user",
                content_hash: if new_hash.is_empty() {
                    None
                } else {
                    Some(&new_hash)
                },
                content: content_bytes.as_deref(),
                rename_from: None,
                metadata: {
                    let mut m = serde_json::json!({
                        "staging_proposal_id": id,
                        "action": proposal.action,
                        "reviewed": true,
                    });
                    if let Some(ref bid) = proposal.batch_id {
                        m["batch_id"] = serde_json::Value::String(bid.clone());
                    }
                    m
                },
            })?;
        }

        self.delete_row(id)?;
        let _ = self.changed_tx.send(());
        Ok(AcceptOutcome {
            proposal_id: id.to_string(),
            target_path: proposal.target_path,
            new_hash,
        })
    }

    pub fn reject(&self, id: &str) -> Result<(), StagingError> {
        let removed = self.with_conn(|conn| {
            Ok(conn.execute("DELETE FROM proposals WHERE id = ?1", params![id])?)
        })?;
        if removed == 0 {
            return Err(StagingError::ProposalNotFound(id.to_string()));
        }
        let _ = self.changed_tx.send(());
        Ok(())
    }

    pub fn accept_all(
        &self,
        filter: &StagingFilter,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<Vec<AcceptOutcome>, StagingError> {
        let proposals = self.list(filter)?;
        let mut outcomes = Vec::new();
        for p in &proposals {
            match self.accept(&p.id, vault, changes) {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %p.id,
                        error = %e,
                        "staging: accept_all skipped failed proposal",
                    );
                }
            }
        }
        Ok(outcomes)
    }

    pub fn gc(&self, max_age_days: u32) -> Result<usize, StagingError> {
        let cutoff = now_ms() - (max_age_days as i64) * 86_400_000;
        let removed = self.with_conn(|conn| {
            Ok(conn.execute(
                "DELETE FROM proposals WHERE created_at_ms < ?1",
                params![cutoff],
            )?)
        })?;
        if removed > 0 {
            let _ = self.changed_tx.send(());
        }
        Ok(removed)
    }

    /// Accept a `move_note` proposal: rename the file on disk and
    /// append a `Renamed` change row. Re-verifies source/target
    /// existence at call time as the spec's safety net
    /// (`staging-action-move-note`'s "recheck flips to source_missing /
    /// target_occupied on drift" — the recheck task runs eagerly, but
    /// the user can race between an Applyable display and clicking
    /// Accept).
    ///
    /// Indexer integration (store-side path remap) is left to the
    /// watcher's rename-event handling per `ingest-rename-preserve-id`;
    /// staging deliberately does not hold an indexer handle. Producers
    /// that need atomic store-side renames go through
    /// `core::ops::move_note` directly.
    ///
    /// status: staging-action-move-note
    fn accept_move(
        &self,
        id: &str,
        proposal: &Proposal,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<AcceptOutcome, StagingError> {
        let source = proposal
            .source_path
            .as_deref()
            .ok_or_else(|| {
                StagingError::Vault(
                    "move_note proposal missing source_path".to_string(),
                )
            })?;
        let target = proposal.target_path.as_str();

        let source_abs = vault.abs_path(source)?;
        let target_abs = vault.abs_path(target)?;

        // Drift safety net — mirrors the write path's `source_hash`
        // recheck. Anything the eager-recheck would have caught also
        // gets caught here, with the same error names.
        let source_exists = source_abs.exists();
        let target_exists = target_abs.exists();
        if !source_exists {
            return Err(StagingError::Vault(format!(
                "{}: source not found",
                ConflictReason::SourceMissing.as_str()
            )));
        }
        if target_exists {
            return Err(StagingError::Vault(format!(
                "{}: target {} already exists",
                ConflictReason::TargetOccupied.as_str(),
                target,
            )));
        }
        if let Some(parent) = target_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Read the body now so we can record it on the change row's
        // post-op content blob (per `core::changes`'s "store post-op
        // content" rule). Errors here are non-fatal — we still want
        // the rename to land even if the body's unreadable (binary,
        // permission edge case); the change row simply omits content.
        let pre_body = vault.read_file_with_hash(source).ok();

        std::fs::rename(&source_abs, &target_abs)?;

        // Same baseline-on-first-touch discipline as the write path,
        // so a follow-up rollback of this auto-accepted move has
        // somewhere to land. Baseline is stamped against the OLD path
        // since that's the path that previously existed.
        if let (Some(c), Some((body, hash))) = (changes, &pre_body)
            && let Err(e) = c.ensure_baseline(source, "user", body.as_bytes(), hash)
        {
            tracing::warn!(error = %e, "changes: ensure_baseline failed (staging accept move)");
        }

        let (post_body, new_hash) = match pre_body {
            Some((body, hash)) => (Some(body), hash),
            // Body wasn't readable as utf-8 pre-rename; we still
            // append a Renamed row with NULL content + empty hash.
            None => (None, String::new()),
        };

        if let Some(c) = changes {
            c.append(ChangeAppend {
                path: target,
                op: ChangeOp::Renamed,
                author: "user",
                content_hash: if new_hash.is_empty() {
                    None
                } else {
                    Some(&new_hash)
                },
                content: post_body.as_deref().map(|s| s.as_bytes()),
                rename_from: Some(source),
                metadata: serde_json::json!({
                    "staging_proposal_id": id,
                    "action": ACTION_MOVE_NOTE,
                    "reviewed": true,
                }),
            })?;
        }

        self.delete_row(id)?;
        let _ = self.changed_tx.send(());
        Ok(AcceptOutcome {
            proposal_id: id.to_string(),
            target_path: target.to_string(),
            new_hash,
        })
    }
}
