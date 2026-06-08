//! Parameter / DTO types for the MCP tool surface, plus the cross-cutting
//! helpers every tool handler shares: the `HikerError` -> MCP error
//! translation, the structured-result wrapper, the audit status/error
//! extractors, and the agent client id. These live together because they
//! are the handler module's shared vocabulary — the param structs carry
//! their hand-rolled `Serialize` impls (used by the audit log) alongside
//! the `JsonSchema` / `From` conversions. Inbound `Deserialize` comes from
//! the derive; outbound `Serialize` is hand-rolled so the `Deserialize`
//! derive stays scoped to JSON-RPC parsing.

use hiker_core::errors::HikerError;
use hiker_core::tasks::types::{Priority as TaskPriority, TaskShape as TaskShapeKind, TaskState};
use rmcp::model::{CallToolResult, ErrorCode, ErrorData};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------- shared handler helpers ----------

/// Identifier stamped into changelog rows + frontmatter provenance for any
/// agent-driven write. v3 uses a fixed value; spec leaves room to extract a
/// per-connection name from the rmcp `clientInfo` later.
pub(crate) const CLIENT_ID: &str = "mcp";

pub(crate) fn hiker_err(
    code: ErrorCode,
    msg: impl Into<std::borrow::Cow<'static, str>>,
) -> ErrorData {
    ErrorData::new(code, msg, None)
}

/// Map `HikerError` to MCP error codes per `mcp-error-model`. Hiker-specific
/// positive codes 1001–1005; standard JSON-RPC `-32602` for invalid params.
pub(crate) fn translate_hiker_err(e: HikerError) -> ErrorData {
    match e {
        HikerError::NotFound(p) => hiker_err(ErrorCode(1002), format!("note not found: {p}")),
        HikerError::DiskDrift { expected, found } => hiker_err(
            ErrorCode(1003),
            format!("drift: file changed since load (expected {expected}, found {found})"),
        ),
        HikerError::PathEscape(p) => {
            ErrorData::invalid_params(format!("path escapes vault: {p}"), None)
        }
        HikerError::AlreadyExists(p) => {
            ErrorData::invalid_params(format!("already exists: {p}"), None)
        }
        HikerError::NotUtf8(msg) => ErrorData::invalid_params(format!("not utf-8: {msg}"), None),
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

pub(crate) fn structured(v: serde_json::Value) -> CallToolResult {
    CallToolResult::structured(v)
}

pub(crate) const fn audit_status(r: &Result<CallToolResult, ErrorData>) -> &'static str {
    match r {
        Ok(_) => "ok",
        Err(_) => "error",
    }
}

pub(crate) fn audit_err(r: &Result<CallToolResult, ErrorData>) -> Option<String> {
    match r {
        Ok(_) => None,
        Err(e) => Some(e.message.to_string()),
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

pub(super) const fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchNotes {
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

/// status: diagram-agent-check
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct CheckDiagram {
    /// Diagram language: `mermaid`, `wavedrom` (alias `wavejson`), or `latex`
    /// (alias `math`). Matched case-insensitively.
    pub lang: String,
    /// The diagram source to syntax-check.
    pub src: String,
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
pub struct GetNote {
    /// Vault-relative path of the note to fetch.
    pub rel_path: String,
    /// Progressive disclosure level. Default `full` for explicit fetches.
    #[serde(default)]
    pub detail: NoteDetail,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RelatedNotes {
    pub rel_path: String,
    #[serde(default)]
    pub top_k: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteNote {
    pub rel_path: String,
    pub content: String,
    /// If provided, the write is drift-aware: errors `1003 drift` when the
    /// on-disk hash differs.
    #[serde(default)]
    pub expected_hash: Option<String>,
}

/// One span-anchored edit in an `edit_note` call. `replace_all = true`
/// allows the anchor to match N times (all replaced); the default requires
/// exactly one match.
///
/// status: mcp-tool-edit-note
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditSpec {
    pub old_str: String,
    pub new_str: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditNote {
    pub rel_path: String,
    pub edits: Vec<EditSpec>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetFrontmatter {
    pub rel_path: String,
    /// Object whose fields are deep-merged into the note's frontmatter.
    /// Typed as `Map` (rather than `Value`) so the JSON schema advertises
    /// `type: object` to MCP clients — without it some clients wrap the
    /// arg in a JSON string and the merge rejects non-object payloads.
    pub fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyTag {
    pub rel_path: String,
    pub tag: String,
}

// ---------- UI-context tool params ----------

/// Empty-args param for `get_active_note` — kept as a struct (rather than
/// `()`) so the dispatch arm and the rmcp `#[tool]` macro see a consistent
/// shape with the rest of the surface.
///
/// status: mcp-tool-get-active-note
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetActiveNote {}

/// status: mcp-tool-get-open-notes
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetOpenNotes {}

/// status: mcp-tool-get-selection
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetSelection {}

// ---------- board tool params (status: board-mcp-tools) ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardsList {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardGet {
    /// Vault-relative path of the board-doc to fetch.
    pub rel_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardAddCard {
    /// Vault-relative path of the board-doc to add the card to.
    pub board_rel_path: String,
    /// Name of the column to append the card to.
    pub column: String,
    /// Vault-relative path of the note to add as a card.
    pub source_rel_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardCreate {
    /// Basename for the new board-doc. Placed under `[boards] new_board_dir`;
    /// auto-suffixed `-N` on collision.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardAddTextCard {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Name of the column to append the freeform card to.
    pub column: String,
    /// The freeform card's text.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardMoveCard {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Board-local card id (from `board_get`).
    pub card_id: String,
    /// Destination column name.
    pub to_column: String,
    /// Target index in the destination column; appends at the tail when
    /// omitted or out of range.
    #[serde(default)]
    pub to_index: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardSetCardText {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Board-local card id of the freeform card to rewrite.
    pub card_id: String,
    /// New text.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardRemoveCard {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Board-local card id to remove (the referenced note is untouched).
    pub card_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardAddColumn {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Name of the new column (appended at the tail).
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardRenameColumn {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Current column name.
    pub old_name: String,
    /// New column name.
    pub new_name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardReorderColumn {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Name of the column to move.
    pub name: String,
    /// Target index in the column order (clamps to the tail).
    pub to_index: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoardDeleteColumn {
    /// Vault-relative path of the board-doc.
    pub board_rel_path: String,
    /// Name of the column to delete (drops its card refs; notes untouched).
    pub name: String,
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
pub struct TaskCheckout {
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
pub struct TaskSubmit {
    pub task_id: String,
    pub value: serde_json::Value,
}

impl JsonSchema for TaskSubmit {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "TaskSubmit".into()
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
pub struct TaskFail {
    pub task_id: String,
    pub error: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskHeartbeat {
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
pub struct TaskList {
    #[serde(default)]
    pub states: Option<Vec<TaskStateParam>>,
    #[serde(default)]
    pub types: Option<Vec<String>>,
}

// ---------- response shapes ----------

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetNoteDigest {
    pub(super) rel_path: String,
    pub(super) title: String,
    pub(super) detail: &'static str,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetNoteSnippet {
    pub(super) rel_path: String,
    pub(super) title: String,
    pub(super) detail: &'static str,
    pub(super) heading_path: Option<String>,
    pub(super) snippet: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetNoteFull {
    pub(super) rel_path: String,
    pub(super) title: String,
    pub(super) detail: &'static str,
    pub(super) content: String,
    pub(super) content_hash: String,
}

/// Response shape for `edit_note`. Either `status: "staged"` with the per-edit
/// `proposal_ids` (review mode) or `status: "written"` with the final
/// `content_hash` (direct mode). `edit_count` is the number of edits in the
/// originating call.
///
/// status: mcp-tool-edit-note
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct EditOutcome {
    pub(super) rel_path: String,
    pub(super) status: &'static str,
    pub(super) edit_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) proposal_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) batch_id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct WriteOutcome {
    pub(super) rel_path: String,
    pub(super) content_hash: String,
    /// When `review_required` is on, `"staged"` with the proposal id in
    /// `proposal_id`. When off, absent (the default is `"written"`).
    ///
    /// status: staging-review-pending-response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) proposal_id: Option<String>,
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
impl Serialize for SearchNotes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("SearchNotes", 3)?;
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
impl Serialize for GetNote {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("GetNote", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("detail", &self.detail)?;
        m.end()
    }
}
impl Serialize for RelatedNotes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("RelatedNotes", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("top_k", &self.top_k)?;
        m.end()
    }
}
impl Serialize for WriteNote {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("WriteNote", 3)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("content", &self.content)?;
        m.serialize_field("expected_hash", &self.expected_hash)?;
        m.end()
    }
}
impl Serialize for SetFrontmatter {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("SetFrontmatter", 2)?;
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
impl Serialize for TaskCheckout {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskCheckout", 4)?;
        m.serialize_field("types", &self.types)?;
        m.serialize_field("shapes", &self.shapes)?;
        m.serialize_field("min_priority", &self.min_priority)?;
        m.serialize_field("lease_secs", &self.lease_secs)?;
        m.end()
    }
}
impl Serialize for TaskSubmit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskSubmit", 2)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.serialize_field("value", &self.value)?;
        m.end()
    }
}
impl Serialize for TaskFail {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskFail", 2)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.serialize_field("error", &self.error)?;
        m.end()
    }
}
impl Serialize for TaskHeartbeat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskHeartbeat", 1)?;
        m.serialize_field("task_id", &self.task_id)?;
        m.end()
    }
}
impl Serialize for TaskList {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("TaskList", 2)?;
        m.serialize_field("states", &self.states)?;
        m.serialize_field("types", &self.types)?;
        m.end()
    }
}
impl Serialize for EditSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("EditSpec", 3)?;
        m.serialize_field("old_str", &self.old_str)?;
        m.serialize_field("new_str", &self.new_str)?;
        m.serialize_field("replace_all", &self.replace_all)?;
        m.end()
    }
}
impl Serialize for EditNote {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("EditNote", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("edits", &self.edits)?;
        m.end()
    }
}
impl Serialize for ApplyTag {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("ApplyTag", 2)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.serialize_field("tag", &self.tag)?;
        m.end()
    }
}
impl Serialize for BoardsList {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        s.serialize_struct("BoardsList", 0)?.end()
    }
}
impl Serialize for BoardGet {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardGet", 1)?;
        m.serialize_field("rel_path", &self.rel_path)?;
        m.end()
    }
}
impl Serialize for BoardAddCard {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardAddCard", 3)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("column", &self.column)?;
        m.serialize_field("source_rel_path", &self.source_rel_path)?;
        m.end()
    }
}
impl Serialize for BoardCreate {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardCreate", 1)?;
        m.serialize_field("name", &self.name)?;
        m.end()
    }
}
impl Serialize for BoardAddTextCard {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardAddTextCard", 3)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("column", &self.column)?;
        m.serialize_field("text", &self.text)?;
        m.end()
    }
}
impl Serialize for BoardMoveCard {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardMoveCard", 4)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("card_id", &self.card_id)?;
        m.serialize_field("to_column", &self.to_column)?;
        m.serialize_field("to_index", &self.to_index)?;
        m.end()
    }
}
impl Serialize for BoardSetCardText {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardSetCardText", 3)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("card_id", &self.card_id)?;
        m.serialize_field("text", &self.text)?;
        m.end()
    }
}
impl Serialize for BoardRemoveCard {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardRemoveCard", 2)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("card_id", &self.card_id)?;
        m.end()
    }
}
impl Serialize for BoardAddColumn {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardAddColumn", 2)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("name", &self.name)?;
        m.end()
    }
}
impl Serialize for BoardRenameColumn {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardRenameColumn", 3)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("old_name", &self.old_name)?;
        m.serialize_field("new_name", &self.new_name)?;
        m.end()
    }
}
impl Serialize for BoardReorderColumn {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardReorderColumn", 3)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("name", &self.name)?;
        m.serialize_field("to_index", &self.to_index)?;
        m.end()
    }
}
impl Serialize for BoardDeleteColumn {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut m = s.serialize_struct("BoardDeleteColumn", 2)?;
        m.serialize_field("board_rel_path", &self.board_rel_path)?;
        m.serialize_field("name", &self.name)?;
        m.end()
    }
}
