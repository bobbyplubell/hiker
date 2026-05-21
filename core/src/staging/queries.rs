use super::*;

impl Staging {
    pub fn list(&self, filter: &StagingFilter) -> Result<Vec<Proposal>, StagingError> {
        let (sql, params) = build_list_query(filter);
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(params.iter()), map_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn count(&self, filter: &StagingFilter) -> Result<u32, StagingError> {
        // Reuse the list query rather than maintaining a second SELECT —
        // proposal counts are small and the filter shape is identical.
        Ok(self.list(filter)?.len() as u32)
    }

    /// Convenience: every proposal still in the applyable state. Wraps
    /// `list` with the canonical filter so callers don't have to know
    /// the `state: Some(ProposalState::Applyable)` shape. Conflicted
    /// proposals are surfaced separately by `list_conflicted`.
    pub fn list_pending(&self) -> Result<Vec<Proposal>, StagingError> {
        let filter = StagingFilter {
            state: Some(crate::staging::ProposalState::Applyable),
            ..Default::default()
        };
        self.list(&filter)
    }

    /// Every proposal — applyable or conflicted — whose `target_path`
    /// matches `rel`. Used by the in-editor patch-review surface which
    /// needs to render decorations for conflicted hunks too (greyed
    /// instead of green/red); the staging-snapshot cache holds only
    /// applyable rows so we re-query when wider visibility is needed.
    pub fn list_for_path(&self, rel: &str) -> Result<Vec<Proposal>, StagingError> {
        let filter = StagingFilter {
            path: Some(rel.to_string()),
            ..Default::default()
        };
        self.list(&filter)
    }
}
