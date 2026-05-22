use rusqlite::{params_from_iter, types::Value};

use super::error::Error;
use super::types::{map_row, Filter, Proposal, SELECT_COLS};
use super::Staging;

impl Staging {
    pub fn list(&self, filter: &Filter) -> Result<Vec<Proposal>, Error> {
        // Build the dynamic `SELECT ... FROM proposals [WHERE ...]
        // ORDER BY` for the requested filter. ORDER BY rowid (insertion
        // order) is the tiebreaker — ULID ids share a timestamp prefix
        // but have random tails, so `id ASC` doesn't preserve the order
        // propose_batch saw inputs in.
        let (sql, params) = {
            let mut sql = format!("SELECT {SELECT_COLS} FROM proposals");
            let mut clauses: Vec<&str> = Vec::new();
            let mut params: Vec<Value> = Vec::new();
            if let Some(ref path) = filter.path {
                clauses.push("target_path = ?");
                params.push(Value::Text(path.clone()));
            }
            if let Some(ref trail_id) = filter.trail_id {
                clauses.push("trail_id = ?");
                params.push(Value::Text(trail_id.clone()));
            }
            if let Some(ref surface) = filter.surface {
                clauses.push("surface = ?");
                params.push(Value::Text(surface.clone()));
            }
            if let Some(ref session_id) = filter.session_id {
                clauses.push("json_extract(metadata, '$.session_id') = ?");
                params.push(Value::Text(session_id.clone()));
            }
            if let Some(state) = filter.state {
                clauses.push("state = ?");
                params.push(Value::Text(state.as_str().to_string()));
            }
            if !clauses.is_empty() {
                sql.push_str(" WHERE ");
                sql.push_str(&clauses.join(" AND "));
            }
            sql.push_str(" ORDER BY created_at_ms ASC, rowid ASC");
            (sql, params)
        };
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(params.iter()), map_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn count(&self, filter: &Filter) -> Result<u32, Error> {
        // Reuse the list query rather than maintaining a second SELECT —
        // proposal counts are small and the filter shape is identical.
        Ok(self.list(filter)?.len() as u32)
    }

    /// Convenience: every proposal still in the applyable state. Wraps
    /// `list` with the canonical filter so callers don't have to know
    /// the `state: Some(ProposalState::Applyable)` shape. Conflicted
    /// proposals are surfaced separately by `list_conflicted`.
    pub fn list_pending(&self) -> Result<Vec<Proposal>, Error> {
        let filter = Filter {
            state: Some(super::types::ProposalState::Applyable),
            ..Default::default()
        };
        self.list(&filter)
    }

    /// Every proposal — applyable or conflicted — whose `target_path`
    /// matches `rel`. Used by the in-editor patch-review surface which
    /// needs to render decorations for conflicted hunks too (greyed
    /// instead of green/red); the staging-snapshot cache holds only
    /// applyable rows so we re-query when wider visibility is needed.
    pub fn list_for_path(&self, rel: &str) -> Result<Vec<Proposal>, Error> {
        let filter = Filter {
            path: Some(rel.to_string()),
            ..Default::default()
        };
        self.list(&filter)
    }
}
