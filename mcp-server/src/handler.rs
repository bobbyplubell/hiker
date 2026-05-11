//! rmcp `ServerHandler` for hiker. Defines the tool surface, parameter
//! shapes, and the boundary that translates `HikerError` into JSON-RPC
//! errors per `mcp.md`'s error model.

use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::config::McpConfig;
use hiker_core::embed::Embedder;
use hiker_core::error::HikerError;
use hiker_core::frontmatter;
use hiker_core::indexer::IndexJobTx;
use hiker_core::ops;
use hiker_core::search::{self, SearchModes};
use hiker_core::staging::{ProposalInput, Staging};
use hiker_core::store::Store;
use hiker_core::tasks::{
    McpClientVia, Priority as TaskPriority, Queue as TaskQueue, QueueError,
    TaskShape as TaskShapeKind, TaskState,
};
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ErrorCode, ErrorData, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_yml;

use crate::audit::AuditLog;

/// Identifier stamped into changelog rows + frontmatter provenance for any
/// agent-driven write. v3 uses a fixed value; spec leaves room to extract a
/// per-connection name from the rmcp `clientInfo` later.
const CLIENT_ID: &str = "mcp";

/// Shared state lives behind an `Arc` since `StreamableHttpService` builds
/// a fresh handler per session. Cheap to clone — every field is already
/// share-shaped.
pub struct HikerState {
    pub vault: Vault,
    pub read_store: Arc<Mutex<Store>>,
    pub jobs: IndexJobTx,
    pub watcher: Arc<Watcher>,
    pub changes: Arc<Changes>,
    pub embedder_provider: Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync>,
    pub config: McpConfig,
    /// status: mcp-tool-toggles
    /// Shared, mutable per-tool config. Each tool dispatch reads this
    /// to decide whether to refuse with `1004 disabled`. The Tauri
    /// `set_setting` command swaps the contents in place so flips in
    /// the settings UI apply without a vault restart.
    pub tools: Arc<std::sync::RwLock<hiker_core::config::McpToolsConfig>>,
    /// Shared staging instance for proposal-based writes (see
    /// docs/settings.md "## Staging review"). When `[mcp.tools]
    /// .review_required` is true, write tools route through
    /// `staging.propose()`.
    ///
    /// status: staging-review-pending-response
    pub staging: Arc<Staging>,
    pub audit: Arc<AuditLog>,
    /// Shared task queue. When `[mcp] enabled`, the `task_*` tools are
    /// advertised; the queue itself lives in the UI layer and is plumbed
    /// in here so all surfaces (basic chat agent, external rmcp clients)
    /// see one in-memory queue.
    pub tasks: Arc<TaskQueue>,
    /// Default lease seconds when a checkout doesn't specify, capped by
    /// `max_lease_secs`.
    pub default_lease_secs: u64,
    pub max_lease_secs: u64,
    /// `[tasks] expose_to_chat_agent` — when false, the in-process
    /// dispatcher silently omits `task_*` from `dispatch_tool`'s allowed
    /// set so the chat agent can't pull queue work.
    pub expose_tasks_to_chat_agent: bool,
    /// `[llm] enabled` — when false, the queue is meaningless (the
    /// direct worker can't run, and the queue's only purpose is LLM
    /// work), so the `task_*` tools are guarded with `1004 disabled`
    /// per `task-queue-respects-llm-disable`. Read once at server start.
    pub llm_enabled: bool,
}

#[derive(Clone)]
pub struct HikerHandler {
    state: Arc<HikerState>,
    // Read by the `#[tool_handler]` macro expansion via `self.tool_router`;
    // dead-code lint can't see the macro-generated reference.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl HikerHandler {
    pub fn new(state: Arc<HikerState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

// ---------- parameter types ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchModesParam {
    #[serde(default = "yes")]
    pub semantic: bool,
    #[serde(default = "yes")]
    pub lexical: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotesParams {
    /// Free-text query. Empty queries return empty buckets without erroring.
    pub query: String,
    /// Optional toggles for which backends to run. Both default on for
    /// hybrid search via reciprocal rank fusion.
    #[serde(default)]
    pub modes: Option<SearchModesParam>,
    /// Cap on the fused bucket size. Default `FUSED_TOP_K = 20`. Capped
    /// server-side at `[mcp] max_top_k`.
    #[serde(default)]
    pub top_k: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum NoteDetail {
    Digest,
    Snippet,
    #[default]
    Full,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNoteParams {
    /// Vault-relative path of the note to fetch.
    pub rel_path: String,
    /// Progressive disclosure level. Default `full` for explicit fetches.
    #[serde(default)]
    pub detail: NoteDetail,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RelatedNotesParams {
    pub rel_path: String,
    #[serde(default)]
    pub top_k: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteNoteParams {
    pub rel_path: String,
    pub content: String,
    /// If provided, the write is drift-aware: errors `1003 drift` when the
    /// on-disk hash differs.
    #[serde(default)]
    pub expected_hash: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetFrontmatterParams {
    pub rel_path: String,
    /// Object whose fields are deep-merged into the note's frontmatter.
    /// Typed as `Map` (rather than `Value`) so the JSON schema advertises
    /// `type: object` to MCP clients — without it some clients wrap the
    /// arg in a JSON string and the merge rejects non-object payloads.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyTagParams {
    pub rel_path: String,
    pub tag: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriorityParam {
    Low,
    Normal,
    High,
}

impl From<PriorityParam> for TaskPriority {
    fn from(p: PriorityParam) -> Self {
        match p {
            PriorityParam::Low => TaskPriority::Low,
            PriorityParam::Normal => TaskPriority::Normal,
            PriorityParam::High => TaskPriority::High,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShapeParam {
    Direct,
    Agent,
}

impl From<ShapeParam> for TaskShapeKind {
    fn from(p: ShapeParam) -> Self {
        match p {
            ShapeParam::Direct => TaskShapeKind::Direct,
            ShapeParam::Agent => TaskShapeKind::Agent,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCheckoutParams {
    /// Filter by `TaskKind` variant name (e.g. `auto_tag`).
    #[serde(default)]
    pub types: Option<Vec<String>>,
    #[serde(default)]
    pub shapes: Option<Vec<ShapeParam>>,
    #[serde(default)]
    pub min_priority: Option<PriorityParam>,
    /// Lease window in seconds. Capped server-side at `[tasks.lease] max_secs`.
    #[serde(default)]
    pub lease_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct TaskSubmitParams {
    pub task_id: String,
    pub value: serde_json::Value,
}

impl JsonSchema for TaskSubmitParams {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TaskSubmitParams".into()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = schemars::Schema::default();
        let obj = schema.ensure_object();
        obj.insert("type".into(), "object".into());
        obj.insert(
            "description".into(),
            "Submit a result for a leased task. `value` can be any JSON.".into(),
        );
        let mut properties = serde_json::Map::new();
        properties.insert(
            "task_id".to_string(),
            generator.subschema_for::<String>().into(),
        );
        // Use an empty schema object (not boolean true) so MCP clients
        // see a valid object schema for the value field.
        properties.insert("value".to_string(), schemars::Schema::default().into());
        obj.insert("properties".into(), serde_json::Value::Object(properties));
        obj.insert(
            "required".into(),
            serde_json::Value::Array(vec!["task_id".into(), "value".into()]),
        );
        schema
    }
    fn _schemars_private_non_optional_json_schema(
        generator: &mut schemars::SchemaGenerator,
    ) -> schemars::Schema {
        Self::json_schema(generator)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskFailParams {
    pub task_id: String,
    pub error: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskHeartbeatParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateParam {
    Queued,
    Leased,
    Completed,
    Failed,
    Cancelled,
}

impl From<TaskStateParam> for TaskState {
    fn from(p: TaskStateParam) -> Self {
        match p {
            TaskStateParam::Queued => TaskState::Queued,
            TaskStateParam::Leased => TaskState::Leased,
            TaskStateParam::Completed => TaskState::Completed,
            TaskStateParam::Failed => TaskState::Failed,
            TaskStateParam::Cancelled => TaskState::Cancelled,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskListParams {
    #[serde(default)]
    pub states: Option<Vec<TaskStateParam>>,
    #[serde(default)]
    pub types: Option<Vec<String>>,
}

// ---------- response shapes ----------

#[derive(Debug, Serialize, JsonSchema)]
struct GetNoteDigest {
    rel_path: String,
    title: String,
    detail: &'static str,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetNoteSnippet {
    rel_path: String,
    title: String,
    detail: &'static str,
    heading_path: Option<String>,
    snippet: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetNoteFull {
    rel_path: String,
    title: String,
    detail: &'static str,
    content: String,
    content_hash: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct WriteOutcome {
    rel_path: String,
    content_hash: String,
    /// When `review_required` is on, `"staged"` with the proposal id in
    /// `staging_id`. When off, absent (the default is `"written"`).
    ///
    /// status: staging-review-pending-response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staging_id: Option<String>,
}

// ---------- tool router ----------

#[tool_router]
impl HikerHandler {
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
        params: Parameters<SearchNotesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.search_notes_inner(&p).await;
        self.state.audit.record(
            "search_notes",
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
        params: Parameters<GetNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.get_note_inner(&p).await;
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
        params: Parameters<RelatedNotesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.related_notes_inner(&p).await;
        self.state.audit.record(
            "related_notes",
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
        description = "Create or replace a note's body. Returns the new content hash."
    )]
    pub async fn write_note(
        &self,
        params: Parameters<WriteNoteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.write_note_inner(&p).await;
        self.state.audit.record(
            "write_note",
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
        description = "Deep-merge fields into a note's YAML frontmatter (auto-stamps hiker.author=agent-authored)."
    )]
    pub async fn set_frontmatter(
        &self,
        params: Parameters<SetFrontmatterParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.set_frontmatter_inner(&p).await;
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
        description = "Append a tag to a note's tags frontmatter list (idempotent)."
    )]
    pub async fn apply_tag(
        &self,
        params: Parameters<ApplyTagParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.apply_tag_inner(&p, true).await;
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
        description = "Remove a tag from a note's tags frontmatter list (no-op if absent)."
    )]
    pub async fn remove_tag(
        &self,
        params: Parameters<ApplyTagParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.apply_tag_inner(&p, false).await;
        self.state.audit.record(
            "remove_tag",
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
        params: Parameters<TaskCheckoutParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_checkout_inner(&p, McpClientVia::External).await;
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
        params: Parameters<TaskSubmitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_submit_inner(&p).await;
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
        params: Parameters<TaskFailParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_fail_inner(&p).await;
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
        params: Parameters<TaskHeartbeatParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_heartbeat_inner(&p).await;
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
        params: Parameters<TaskListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let Parameters(p) = params;
        let outcome = self.task_list_inner(&p).await;
        self.state.audit.record(
            "task_list",
            &serde_json::Value::Null,
            audit_status(&outcome),
            audit_err(&outcome),
        );
        outcome
    }
}

// ---------- inner helpers ----------

impl HikerHandler {
    async fn search_notes_inner(
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

    async fn get_note_inner(&self, p: &GetNoteParams) -> Result<CallToolResult, ErrorData> {
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

    async fn related_notes_inner(
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

    async fn write_note_inner(&self, p: &WriteNoteParams) -> Result<CallToolResult, ErrorData> {
        self.guard_tool("write_note")?;

        // status: staging-review-pending-response
        let review_required = self
            .state
            .tools
            .read()
            .map(|cfg| cfg.review_required)
            .unwrap_or(false);

        if review_required {
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
            let new_hash = ops::agent_write_note(
                &self.state.watcher,
                &self.state.jobs,
                &self.state.vault,
                Some(&self.state.changes),
                CLIENT_ID,
                "write_note",
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

    async fn set_frontmatter_inner(
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
            let new_hash = ops::agent_set_frontmatter(
                &self.state.watcher,
                &self.state.jobs,
                &self.state.vault,
                Some(&self.state.changes),
                CLIENT_ID,
                "set_frontmatter",
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

    async fn apply_tag_inner(
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
            let existing = self
                .state
                .vault
                .read_file(&p.rel_path)
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
            let result = if add {
                ops::agent_apply_tag(
                    &self.state.watcher,
                    &self.state.jobs,
                    &self.state.vault,
                    Some(&self.state.changes),
                    CLIENT_ID,
                    tool_name,
                    &p.rel_path,
                    &p.tag,
                )
                .await
            } else {
                ops::agent_remove_tag(
                    &self.state.watcher,
                    &self.state.jobs,
                    &self.state.vault,
                    Some(&self.state.changes),
                    CLIENT_ID,
                    tool_name,
                    &p.rel_path,
                    &p.tag,
                )
                .await
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

    fn guard_tasks(&self) -> Result<(), ErrorData> {
        if self.state.llm_enabled {
            Ok(())
        } else {
            // status: task-queue-respects-llm-disable
            // The queue's only purpose is LLM work; with `[llm] enabled =
            // false` the direct worker is force-off and the rmcp-side
            // `task_*` tools answer `1004 disabled` so external agents
            // see a coherent disabled state instead of being able to
            // checkout work no in-process worker will ever drain.
            Err(hiker_err(ErrorCode(1004), "tasks disabled (llm features off)"))
        }
    }

    async fn task_checkout_inner(
        &self,
        p: &TaskCheckoutParams,
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

    async fn task_submit_inner(
        &self,
        p: &TaskSubmitParams,
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

    async fn task_fail_inner(
        &self,
        p: &TaskFailParams,
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

    async fn task_heartbeat_inner(
        &self,
        p: &TaskHeartbeatParams,
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

    async fn task_list_inner(
        &self,
        p: &TaskListParams,
    ) -> Result<CallToolResult, ErrorData> {
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

    /// status: mcp-tool-toggles
    /// Per-tool gate. Reads the shared `tools` config live so a flip in
    /// the settings UI applies to the next dispatch without a vault
    /// restart. Combines the per-tool flag with the `writes_enabled`
    /// master gate (write tools) — see `McpToolsConfig::tool_allowed`.
    fn guard_tool(&self, name: &str) -> Result<(), ErrorData> {
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

    /// In-process tool dispatch entrypoint used by the basic agent loop
    /// (`agent-tool-routing-via-mcp`). Routes by tool name to the same
    /// `_inner` helpers the rmcp `#[tool]` methods call, so the basic
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
                let p: SearchNotesParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid search_notes args: {e}"))?;
                let r = self.search_notes_inner(&p).await;
                self.state.audit.record(
                    "search_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "get_note" => {
                let p: GetNoteParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid get_note args: {e}"))?;
                let r = self.get_note_inner(&p).await;
                self.state.audit.record(
                    "get_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "related_notes" => {
                let p: RelatedNotesParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid related_notes args: {e}"))?;
                let r = self.related_notes_inner(&p).await;
                self.state.audit.record(
                    "related_notes",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "write_note" => {
                let p: WriteNoteParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid write_note args: {e}"))?;
                let r = self.write_note_inner(&p).await;
                self.state.audit.record(
                    "write_note",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "set_frontmatter" => {
                let p: SetFrontmatterParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid set_frontmatter args: {e}"))?;
                let r = self.set_frontmatter_inner(&p).await;
                self.state.audit.record(
                    "set_frontmatter",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "apply_tag" => {
                let p: ApplyTagParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid apply_tag args: {e}"))?;
                let r = self.apply_tag_inner(&p, true).await;
                self.state.audit.record(
                    "apply_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "remove_tag" => {
                let p: ApplyTagParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid remove_tag args: {e}"))?;
                let r = self.apply_tag_inner(&p, false).await;
                self.state.audit.record(
                    "remove_tag",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_checkout" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskCheckoutParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_checkout args: {e}"))?;
                let r = self.task_checkout_inner(&p, McpClientVia::InProcessChatAgent).await;
                self.state.audit.record(
                    "task_checkout",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_submit" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskSubmitParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_submit args: {e}"))?;
                let r = self.task_submit_inner(&p).await;
                self.state.audit.record(
                    "task_submit",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_fail" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskFailParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_fail args: {e}"))?;
                let r = self.task_fail_inner(&p).await;
                self.state.audit.record(
                    "task_fail",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_heartbeat" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskHeartbeatParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_heartbeat args: {e}"))?;
                let r = self.task_heartbeat_inner(&p).await;
                self.state.audit.record(
                    "task_heartbeat",
                    &serde_json::json!({"task_id": p.task_id}),
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            "task_list" if self.state.expose_tasks_to_chat_agent => {
                let p: TaskListParams = serde_json::from_value(raw_value.clone())
                    .map_err(|e| format!("invalid task_list args: {e}"))?;
                let r = self.task_list_inner(&p).await;
                self.state.audit.record(
                    "task_list",
                    &raw_value,
                    audit_status(&r),
                    audit_err(&r),
                );
                r
            }
            other => return Err(format!("unknown tool: {other}")),
        };

        let result = outcome.map_err(|e| format!("{} (code {})", e.message, e.code.0))?;
        let payload = result
            .structured_content
            .unwrap_or(serde_json::Value::Null);
        serde_json::to_string(&payload).map_err(|e| format!("serialize result: {e}"))
    }
}

#[tool_handler]
impl ServerHandler for HikerHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hiker", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Hiker MCP server. Read: search_notes, get_note, related_notes. \
                 Write: write_note, set_frontmatter, apply_tag, remove_tag.",
            )
    }
}

// ---------- error translation ----------

fn hiker_err(code: ErrorCode, msg: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::new(code, msg, None)
}

/// Map `HikerError` to MCP error codes per `mcp-error-model`. Hiker-specific
/// positive codes 1001–1005; standard JSON-RPC `-32602` for invalid params.
fn translate_hiker_err(e: HikerError) -> ErrorData {
    match e {
        HikerError::NotFound(p) => hiker_err(ErrorCode(1002), format!("note not found: {p}")),
        HikerError::DiskDrift { expected, found } => hiker_err(
            ErrorCode(1003),
            format!(
                "drift: file changed since load (expected {expected}, found {found})"
            ),
        ),
        HikerError::PathEscape(p) => {
            ErrorData::invalid_params(format!("path escapes vault: {p}"), None)
        }
        HikerError::AlreadyExists(p) => {
            ErrorData::invalid_params(format!("already exists: {p}"), None)
        }
        HikerError::NotUtf8(msg) => {
            ErrorData::invalid_params(format!("not utf-8: {msg}"), None)
        }
        HikerError::Config(msg) => ErrorData::internal_error(format!("config: {msg}"), None),
        HikerError::Io(msg) => {
            // ENOENT bubbles up as Io from read_file; surface that as 1002 so
            // agents see a consistent "note not found" code rather than a
            // generic internal error.
            if msg.contains("No such file or directory") || msg.contains("not found") {
                hiker_err(ErrorCode(1002), msg)
            } else {
                ErrorData::internal_error(msg, None)
            }
        }
    }
}

// ---------- helpers ----------

fn structured(v: serde_json::Value) -> CallToolResult {
    CallToolResult::structured(v)
}

fn title_from_rel_path(rel: &str) -> String {
    let last = rel.rsplit('/').next().unwrap_or(rel);
    let stem = last.strip_suffix(".md").unwrap_or(last);
    if stem.is_empty() {
        "Untitled".into()
    } else {
        stem.to_string()
    }
}

fn head_snippet(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.len() <= 200 {
        collapsed
    } else {
        let cutoff = collapsed
            .char_indices()
            .nth(200)
            .map(|(i, _)| i)
            .unwrap_or(collapsed.len());
        format!("{}…", &collapsed[..cutoff])
    }
}

fn audit_status(r: &Result<CallToolResult, ErrorData>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(_) => "error",
    }
}

fn audit_err(r: &Result<CallToolResult, ErrorData>) -> Option<String> {
    match r {
        Ok(_) => None,
        Err(e) => Some(e.message.to_string()),
    }
}

// ---------- params Serialize for audit ----------
//
// Each param struct is `Serialize`d into the audit log; without this impl
// `serde_json::to_value` fails on the params handed to record(). Done as
// a separate impl so the Deserialize derive on the param structs stays
// scoped to inbound JSON-RPC parsing.
impl Serialize for SearchModesParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_struct("SearchModesParam", 2)?;
        serde::ser::SerializeStruct::serialize_field(&mut m, "semantic", &self.semantic)?;
        serde::ser::SerializeStruct::serialize_field(&mut m, "lexical", &self.lexical)?;
        serde::ser::SerializeStruct::end(m)
    }
}
impl Serialize for SearchNotesParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("SearchNotesParams", 3)?;
        m.serialize_field("query", &self.query)?;
        m.serialize_field("modes", &self.modes)?;
        m.serialize_field("top_k", &self.top_k)?;
        m.end()
    }
}
impl Serialize for NoteDetail {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            NoteDetail::Digest => "digest",
            NoteDetail::Snippet => "snippet",
            NoteDetail::Full => "full",
        })
    }
}
impl Serialize for GetNoteParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("GetNoteParams", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("detail", &self.detail)?;
        m.end()
    }
}
impl Serialize for RelatedNotesParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("RelatedNotesParams", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("top_k", &self.top_k)?;
        m.end()
    }
}
impl Serialize for WriteNoteParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("WriteNoteParams", 3)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("content", &self.content)?;
        m.serialize_field("expected_hash", &self.expected_hash)?;
        m.end()
    }
}
impl Serialize for SetFrontmatterParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("SetFrontmatterParams", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("fields", &serde_json::Value::Object(self.fields.clone()))?;
        m.end()
    }
}
impl Serialize for PriorityParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            PriorityParam::Low => "low",
            PriorityParam::Normal => "normal",
            PriorityParam::High => "high",
        })
    }
}
impl Serialize for ShapeParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            ShapeParam::Direct => "direct",
            ShapeParam::Agent => "agent",
        })
    }
}
impl Serialize for TaskStateParam {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            TaskStateParam::Queued => "queued",
            TaskStateParam::Leased => "leased",
            TaskStateParam::Completed => "completed",
            TaskStateParam::Failed => "failed",
            TaskStateParam::Cancelled => "cancelled",
        })
    }
}
impl Serialize for TaskCheckoutParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskCheckoutParams", 4)?;
        m.serialize_field("types", &self.types)?;
        m.serialize_field("shapes", &self.shapes)?;
        m.serialize_field("min_priority", &self.min_priority)?;
        m.serialize_field("lease_secs", &self.lease_secs)?;
        m.end()
    }
}
impl Serialize for TaskSubmitParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskSubmitParams", 2)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.serialize_field("value", &self.value)?;
        m.end()
    }
}
impl Serialize for TaskFailParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskFailParams", 2)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.serialize_field("error", &self.error)?;
        m.end()
    }
}
impl Serialize for TaskHeartbeatParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskHeartbeatParams", 1)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.end()
    }
}
impl Serialize for TaskListParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskListParams", 2)?;
        m.serialize_field("states", &self.states)?;
        m.serialize_field("types", &self.types)?;
        m.end()
    }
}
impl Serialize for ApplyTagParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("ApplyTagParams", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("tag", &self.tag)?;
        m.end()
    }
}
