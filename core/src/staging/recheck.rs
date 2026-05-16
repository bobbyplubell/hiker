use super::*;

impl Staging {
    /// Recheck a `move_note` proposal against the live filesystem.
    /// Callers (the eager-recheck task, accept-time safety net) pass in
    /// the current existence of the source and target paths — both
    /// are vault-relative and resolved through `Vault::abs_path`
    /// upstream. Returns the same `RecheckOutcome` shape as `recheck`.
    ///
    /// status: staging-action-move-note
    pub fn recheck_move(
        &self,
        id: &str,
        source_exists: bool,
        target_exists: bool,
    ) -> Result<RecheckOutcome, StagingError> {
        let proposal = self
            .get_full(id)?
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;
        let (new_state, new_reason) = derive_move_state(source_exists, target_exists);
        let prior_state = proposal.state;
        let prior_reason = proposal.conflict_reason;
        if prior_state == new_state && prior_reason == new_reason {
            return Ok(RecheckOutcome {
                prior_state,
                new_state,
                new_reason,
            });
        }
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE proposals SET state = ?1, conflict_reason = ?2 WHERE id = ?3",
                params![new_state.as_str(), new_reason.map(|r| r.as_str()), id],
            )?;
            Ok(())
        })?;
        let _ = self.changed_tx.send(());
        Ok(RecheckOutcome {
            prior_state,
            new_state,
            new_reason,
        })
    }

    /// status: staging-proposal-state
    pub fn recheck(
        &self,
        id: &str,
        current_disk: Option<&str>,
    ) -> Result<RecheckOutcome, StagingError> {
        let proposal = self
            .get_full(id)?
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;

        let (new_state, new_reason) = derive_state(&proposal, current_disk);
        let prior_state = proposal.state;
        let prior_reason = proposal.conflict_reason;

        if prior_state == new_state && prior_reason == new_reason {
            return Ok(RecheckOutcome {
                prior_state,
                new_state,
                new_reason,
            });
        }

        self.with_conn(|conn| {
            conn.execute(
                "UPDATE proposals SET state = ?1, conflict_reason = ?2 WHERE id = ?3",
                params![new_state.as_str(), new_reason.map(|r| r.as_str()), id],
            )?;
            Ok(())
        })?;
        let _ = self.changed_tx.send(());
        Ok(RecheckOutcome {
            prior_state,
            new_state,
            new_reason,
        })
    }
}
