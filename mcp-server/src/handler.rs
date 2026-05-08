//! rmcp `ServerHandler` for hiker. Defines the tool surface, parameter
//! shapes, and the boundary that translates `HikerError` into JSON-RPC
//! errors per `mcp.md`'s error model.

use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::config::McpConfig;
use hiker_core::embed::Embedder;
use hiker_core::error::HikerError;
use hiker_core::indexer::IndexJobTx;
use hiker_core::ops;
use hiker_core::search::{self, SearchModes};
use hiker_core::store::Store;
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
    pub audit: Arc<AuditLog>,
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
}

// ---------- inner helpers ----------

impl HikerHandler {
    async fn search_notes_inner(
        &self,
        p: &SearchNotesParams,
    ) -> Result<CallToolResult, ErrorData> {
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
        )
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        if (resp.fused.len() as u32) > requested {
            resp.fused.truncate(requested as usize);
        }

        Ok(structured(
            serde_json::to_value(&resp)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
        ))
    }

    async fn get_note_inner(&self, p: &GetNoteParams) -> Result<CallToolResult, ErrorData> {
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
        self.guard_writes()?;
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
            })
            .unwrap_or(serde_json::Value::Null),
        ))
    }

    async fn set_frontmatter_inner(
        &self,
        p: &SetFrontmatterParams,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_writes()?;
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
            })
            .unwrap_or(serde_json::Value::Null),
        ))
    }

    async fn apply_tag_inner(
        &self,
        p: &ApplyTagParams,
        add: bool,
    ) -> Result<CallToolResult, ErrorData> {
        self.guard_writes()?;
        let tool_name = if add { "apply_tag" } else { "remove_tag" };
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
            })
            .unwrap_or(serde_json::Value::Null),
        ))
    }

    fn guard_writes(&self) -> Result<(), ErrorData> {
        if self.state.config.tools.writes_enabled {
            Ok(())
        } else {
            Err(hiker_err(ErrorCode(1004), "write tools disabled"))
        }
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
impl Serialize for ApplyTagParams {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("ApplyTagParams", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("tag", &self.tag)?;
        m.end()
    }
}
