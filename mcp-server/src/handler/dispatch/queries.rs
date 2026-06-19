use hiker_core::queries::{self, Error as QueryError, Query};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};

use super::App;
use crate::handler::params::{
    hiker_err, structured, translate_hiker_err, RunQuery,
};

impl App {
    /// Row cap applied when the caller doesn't pass `limit`.
    const QUERY_DEFAULT_LIMIT: u32 = 100;
    /// Hard server-side cap on `limit`.
    const QUERY_MAX_LIMIT: u32 = 500;

    /// status: query-mcp-tool
    /// The generic `query` read tool: run a saved query-doc or an inline
    /// filter through `core::queries::run_query` — the same compile path
    /// smart folders use, so agent and UI results never diverge. Read-only;
    /// does not populate the per-session read set (only `get_note` does).
    pub(in crate::handler) async fn run_query_tool(
        &self,
        p: &RunQuery,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("query")?;
        let mut query = self.parse_query_arg(p)?;
        // Effective cap: the request's limit (default 100, max 500), with a
        // stricter limit inside the query itself still winning.
        let cap = p.limit.unwrap_or(Self::QUERY_DEFAULT_LIMIT).min(Self::QUERY_MAX_LIMIT);
        query.limit = Some(query.limit.map_or(cap, |own| own.min(cap)));
        let select = p.select.clone().unwrap_or_default();

        let store = self
            .state
            .read_store
            .lock()
            .map_err(|_| ErrorData::internal_error("read_store mutex poisoned", None))?;
        let rows = queries::run_query(&store, &self.state.kinds, &query, &select)
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        let rows: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path,
                    "title": r.title,
                    "mtime": r.mtime,
                    "fields": r.fields,
                })
            })
            .collect();
        Ok(structured(serde_json::json!({ "rows": rows })))
    }

    /// Resolve the tool args to a parsed query: exactly one of `query_doc`
    /// (load + parse the saved doc) or `filter` (inline, same grammar).
    fn parse_query_arg(&self, p: &RunQuery) -> Result<Query, ErrorData> {
        match (&p.query_doc, &p.filter) {
            (Some(_), Some(_)) | (None, None) => Err(ErrorData::invalid_params(
                "exactly one of `query_doc` or `filter` is required",
                None,
            )),
            (Some(rel), None) => {
                // Missing file surfaces as `1002 note_not_found` via the
                // standard translation.
                let src = self.state.vault.read_file(rel).map_err(translate_hiker_err)?;
                queries::parse_query_doc_for(rel, &src).map_err(|e| Self::query_doc_err(rel, &e))
            }
            (None, Some(filter)) => {
                queries::parse_filter_json(&serde_json::Value::Object(filter.clone())).map_err(
                    |e| {
                        ErrorData::invalid_params(
                            format!("filter outside the query grammar: {e}"),
                            None,
                        )
                    },
                )
            }
        }
    }

    /// Error mapping for a saved query-doc, per `mcp-error-model`: a path
    /// that isn't a query-doc at all (wrong extension, no frontmatter,
    /// wrong kind) is `1002 note_not_found`; a real query-doc whose filter
    /// falls outside the grammar is `invalid_params`.
    fn query_doc_err(rel: &str, e: &QueryError) -> ErrorData {
        match e {
            QueryError::NotMarkdown(_)
            | QueryError::MissingFrontmatter
            | QueryError::NotMapping
            | QueryError::KindMismatch { .. } => {
                hiker_err(ErrorCode(1002), format!("not a query-doc: {rel} ({e})"))
            }
            QueryError::MissingField(f) if *f != "hiker.query" => {
                hiker_err(ErrorCode(1002), format!("not a query-doc: {rel} ({e})"))
            }
            _ => {
                ErrorData::invalid_params(format!("query-doc filter outside the grammar: {e}"), None)
            }
        }
    }
}
