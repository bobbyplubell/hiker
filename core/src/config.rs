//! User and per-vault TOML settings. See `docs/settings.md`.
//!
//! Two TOML files (per-user at the platform config dir, per-vault at
//! `vault/.hiker/config.toml`) are deep-merged with vault winning, then
//! deserialized into a frozen `Config`. Missing files are auto-created with
//! the current defaults serialized in full so users have a self-documenting
//! file to edit. `set_setting` uses `toml_edit` to patch in place so users'
//! comments and key ordering survive in-app writes.
//
// status: settings-user-config-toml
// status: settings-vault-config-toml
// status: settings-load-once-at-startup
// status: settings-strict-load
// status: settings-defaults-in-code
// status: settings-auto-create-defaults
// status: settings-write-back
// status: settings-section-editor
// status: settings-section-indexing
// status: settings-section-vault
// status: settings-schema-version
// status: search-mode-state-persisted

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HikerError;

pub const SCHEMA_VERSION: u32 = 1;

/// Cap on the `vault.recent` list. Older entries past this point fall off
/// when a new vault open pushes onto the front.
pub const RECENT_VAULTS_CAP: usize = 10;

/// Push `root` to the front of `current`, dedupe by string equality, cap at
/// `RECENT_VAULTS_CAP` entries. Returns the new list. Pure policy — caller
/// is responsible for persisting it via `Config::set("vault.recent", ...)`
/// if needed.
///
/// Lives in `core::config` rather than at adapter level so any future
/// adapter (CLI / MCP) that opens a vault gets the same recent-list shape
/// without re-implementing the dedupe + cap.
pub fn push_recent_vault(current: &[String], root: &Path) -> Vec<String> {
    let display = root.to_string_lossy().into_owned();
    let mut out = Vec::with_capacity(current.len() + 1);
    out.push(display.clone());
    for entry in current {
        if entry != &display {
            out.push(entry.clone());
        }
        if out.len() >= RECENT_VAULTS_CAP {
            break;
        }
    }
    out
}

/// Top-level config struct loaded from the merged user+vault TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub editor: EditorConfig,
    #[serde(default)]
    pub indexing: IndexingConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub tasks: TasksConfig,
    #[serde(default)]
    pub trails: TrailsConfig,
    #[serde(default)]
    pub acp: AcpConfig,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            editor: EditorConfig::default(),
            indexing: IndexingConfig::default(),
            vault: VaultConfig::default(),
            search: SearchConfig::default(),
            mcp: McpConfig::default(),
            llm: LlmConfig::default(),
            tasks: TasksConfig::default(),
            trails: TrailsConfig::default(),
            acp: AcpConfig::default(),
        }
    }
}

/// `[trails]` section. Configures trail-doc placement and other vault-wide
/// trail policy. See `docs/trails.md`.
///
/// status: trails-default-location
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailsConfig {
    /// Default directory for newly-created trail-docs. Vault-relative.
    /// Empty string places trails at vault root. Auto-created on first
    /// trail. Vault-scope eligible per `settings-write-back`.
    #[serde(default = "default_new_trail_dir")]
    pub new_trail_dir: String,
}

impl Default for TrailsConfig {
    fn default() -> Self {
        Self {
            new_trail_dir: default_new_trail_dir(),
        }
    }
}

fn default_new_trail_dir() -> String {
    "trails/".to_string()
}

/// `[acp]` section. Configures the optional Agent Client Protocol backend
/// for the chat panel. When `command` is non-empty, the chat panel routes
/// through an external ACP agent instead of the built-in basic agent loop.
/// The `command` value is the full command line to launch the agent (e.g.
/// `"auggie --acp"`, `"gemini --acp"`, `"cursor --acp"`). The first
/// whitespace-delimited word is the binary; the rest are arguments.
/// The agent binary must be installed and on PATH independently.
/// See `docs/acp.md`.
///
/// status: llm-acp-client-optional
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcpConfig {
    /// Full command line to launch the ACP agent. Empty string means
    /// ACP is disabled. Split on whitespace; first token = binary,
    /// remainder = args.
    #[serde(default)]
    pub command: String,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self { command: String::new() }
    }
}

/// `[tasks]` section. Configures the unified work queue (`core::tasks`).
/// See `docs/task-queue.md`.
///
/// status: task-queue-settings-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TasksConfig {
    #[serde(default)]
    pub worker_preference: WorkerPreferenceCfg,
    #[serde(default = "default_terminal_retention_secs")]
    pub terminal_retention_secs: u64,
    #[serde(default)]
    pub direct_worker: DirectWorkerConfig,
    /// Whether the in-process basic chat agent gets the `task_*` MCP
    /// tools advertised in its tool set. External rmcp clients are
    /// unaffected — they always see `task_*` when `[mcp] enabled = true`.
    #[serde(default = "yes")]
    pub expose_to_chat_agent: bool,
    #[serde(default)]
    pub lease: LeaseConfig,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            worker_preference: WorkerPreferenceCfg::default(),
            terminal_retention_secs: default_terminal_retention_secs(),
            direct_worker: DirectWorkerConfig::default(),
            expose_to_chat_agent: true,
            lease: LeaseConfig::default(),
        }
    }
}

impl TasksConfig {
    /// Grace window the direct worker waits before becoming eligible for
    /// a newly-queued task, per `worker_preference`. `internal` = 0,
    /// `auto` = 1s, `external` = 5s.
    pub fn direct_grace(&self) -> std::time::Duration {
        match self.worker_preference {
            WorkerPreferenceCfg::Internal => std::time::Duration::from_secs(0),
            WorkerPreferenceCfg::Auto => std::time::Duration::from_secs(1),
            WorkerPreferenceCfg::External => std::time::Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPreferenceCfg {
    Auto,
    Internal,
    External,
}

impl Default for WorkerPreferenceCfg {
    fn default() -> Self {
        WorkerPreferenceCfg::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectWorkerConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_direct_parallelism")]
    pub parallelism: u8,
}

impl Default for DirectWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            parallelism: default_direct_parallelism(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseConfig {
    #[serde(default = "default_lease_default_secs")]
    pub default_secs: u64,
    #[serde(default = "default_lease_max_secs")]
    pub max_secs: u64,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            default_secs: default_lease_default_secs(),
            max_secs: default_lease_max_secs(),
        }
    }
}

fn default_terminal_retention_secs() -> u64 {
    60
}

fn default_direct_parallelism() -> u8 {
    1
}

fn default_lease_default_secs() -> u64 {
    60
}

fn default_lease_max_secs() -> u64 {
    600
}

/// `[llm]` section. Configures generative LLM access for `core::llm` and
/// the basic agent loop (`core::agent`). Mirrors the shape pinned in
/// `docs/llm.md` §`core::llm` and the v3.5 stub in `docs/settings.md`.
///
/// status: llm-providers-config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// Master switch. When false, `core::llm` is treated as unavailable
    /// (background / fan-out / chat panel all no-op). Reserved for
    /// `llm-disable-mode`; left here so the loader doesn't reject the key
    /// once that slug lands.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub provider: LlmProviderConfig,
    #[serde(default)]
    pub limits: LlmLimitsConfig,
    #[serde(default)]
    pub agent: LlmAgentConfig,
    #[serde(default)]
    pub audit: LlmAuditConfig,
    #[serde(default)]
    pub background: LlmBackgroundConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: LlmProviderConfig::default(),
            limits: LlmLimitsConfig::default(),
            agent: LlmAgentConfig::default(),
            audit: LlmAuditConfig::default(),
            background: LlmBackgroundConfig::default(),
        }
    }
}

/// `[llm.background]` — config for debounced background LLM features
/// (auto-tagging, summarization, etc.). Stub in v3 — `review_required`
/// is the only field that ships now; the rest land when their backing
/// features do.
///
/// status: agent-write-review-mode
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmBackgroundConfig {
    /// When true, debounced background LLM features write to staging
    /// instead of mutating frontmatter directly. Default false (off).
    #[serde(default = "no")]
    pub review_required: bool,
}

/// `[llm.provider]` — backend selection and connection-shaped knobs. API
/// keys are *never* stored in TOML; `api_key_env` names an environment
/// variable that the runtime reads at provider construction time. Empty
/// strings on `api_key_env` / `base_url` are treated as unset (the TOML
/// auto-create writes the field even when blank, so the loader has to
/// tolerate that case rather than aborting).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmProviderConfig {
    /// Backend id (`anthropic` / `openai` / `ollama` / `google` / ...). The
    /// canonical list lives in graniet/`llm`'s `LLMBackend::FromStr`; the
    /// runtime validates this when the client is built.
    #[serde(default = "default_llm_backend")]
    pub backend: String,
    /// Model id within the chosen backend.
    #[serde(default = "default_llm_model")]
    pub model: String,
    /// Name of the env var holding the API key. Empty = no key (e.g. local
    /// Ollama). Used as the fallback when `api_key` (user-scope literal)
    /// is empty.
    #[serde(default)]
    pub api_key_env: String,
    /// User-scope literal API key. Plain text on disk in the platform
    /// config dir; takes precedence over `api_key_env` when set. The
    /// eligibility list refuses writes to this field from the vault
    /// TOML so a synced vault can't carry the secret. Empty by default;
    /// users who want shell-managed keys should leave this empty and
    /// set `api_key_env`. See `llm.md` §`[llm-providers-config]`.
    #[serde(default)]
    pub api_key: String,
    /// Optional override (Ollama URL, OpenAI-compatible proxy, etc.).
    #[serde(default)]
    pub base_url: String,
}

impl Default for LlmProviderConfig {
    fn default() -> Self {
        Self {
            backend: default_llm_backend(),
            model: default_llm_model(),
            api_key_env: String::new(),
            api_key: String::new(),
            base_url: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmLimitsConfig {
    #[serde(default = "default_llm_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_llm_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for LlmLimitsConfig {
    fn default() -> Self {
        Self {
            max_tokens: default_llm_max_tokens(),
            timeout_secs: default_llm_timeout_secs(),
        }
    }
}

/// `[llm.agent]` — basic agent loop tunables. Defaults match the
/// circuit-breaker numbers pinned in `llm.md`.
///
/// status: agent-iteration-cap-prompt
/// status: agent-tool-call-timeout
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmAgentConfig {
    #[serde(default = "default_iteration_cap")]
    pub iteration_cap: u32,
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_secs: u64,
}

impl Default for LlmAgentConfig {
    fn default() -> Self {
        Self {
            iteration_cap: default_iteration_cap(),
            tool_timeout_secs: default_tool_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmAuditConfig {
    /// Mirror of `[mcp.audit] log_full_input`. When false (default), the
    /// JSONL audit log records call metadata but redacts large prompt
    /// bodies.
    #[serde(default = "no")]
    pub log_full_prompt: bool,
}

fn default_llm_backend() -> String {
    "anthropic".to_string()
}

fn default_llm_model() -> String {
    "claude-sonnet-4-7".to_string()
}

fn default_llm_max_tokens() -> u32 {
    4096
}

fn default_llm_timeout_secs() -> u64 {
    60
}

fn default_iteration_cap() -> u32 {
    10
}

fn default_tool_timeout_secs() -> u64 {
    30
}

/// `[mcp]` section. Configures the in-process MCP server (see `docs/mcp.md`).
/// Loader lands alongside the v3 milestone — until then the section is
/// recognized so users can enable/disable the server and tune top_k caps.
///
/// status: mcp-config-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Bind host. Defaults to `127.0.0.1` (loopback-only, matching the
    /// localhost-trust auth model). Anything else exposes vault contents
    /// to whoever can reach the listening port — the settings UI shows
    /// a warning. See `mcp-bind-host-configurable`.
    #[serde(default = "default_mcp_host")]
    pub host: String,
    /// `0` = ephemeral OS-assigned port; otherwise bind that fixed port.
    #[serde(default)]
    pub port: u16,
    /// Vault-relative path to the JSON discovery file written on bind and
    /// removed on shutdown.
    #[serde(default = "default_mcp_discovery_file")]
    pub discovery_file: String,
    /// Cap on agent-requested `top_k` for `search_notes` / `related_notes`.
    #[serde(default = "default_mcp_max_top_k")]
    pub max_top_k: u32,
    #[serde(default)]
    pub tools: McpToolsConfig,
    #[serde(default)]
    pub audit: McpAuditConfig,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: default_mcp_host(),
            port: 0,
            discovery_file: default_mcp_discovery_file(),
            max_top_k: default_mcp_max_top_k(),
            tools: McpToolsConfig::default(),
            audit: McpAuditConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolsConfig {
    /// Master gate for the write tools (`write_note`, `set_frontmatter`,
    /// `apply_tag`, `remove_tag`). Kept for backwards compatibility with
    /// existing TOMLs — when false, every write tool refuses with `1004
    /// disabled` regardless of the per-tool flags below.
    #[serde(default = "yes")]
    pub writes_enabled: bool,
    /// If true, agents passing `scope` can fetch redacted bodies.
    /// Conservative default (false) per spec.
    #[serde(default = "no")]
    pub allow_redacted_lookup: bool,
    // status: agent-write-review-mode
    /// When true, MCP write tools (`write_note`, `set_frontmatter`,
    /// `apply_tag`, `remove_tag`) route through `core::staging::propose()`
    /// instead of applying directly. Default false — agents write directly
    /// + append a changelog row (existing behavior).
    #[serde(default = "no")]
    pub review_required: bool,
    // Per-tool toggles (status: mcp-tool-toggles). Default true.
    // Reads:
    #[serde(default = "yes")] pub search_notes_enabled: bool,
    #[serde(default = "yes")] pub get_note_enabled: bool,
    #[serde(default = "yes")] pub related_notes_enabled: bool,
    // Writes (also gated by `writes_enabled` master flag):
    #[serde(default = "yes")] pub write_note_enabled: bool,
    #[serde(default = "yes")] pub set_frontmatter_enabled: bool,
    #[serde(default = "yes")] pub apply_tag_enabled: bool,
    #[serde(default = "yes")] pub remove_tag_enabled: bool,
    // Task-queue tools (also gated by `[tasks] expose_to_chat_agent`
    // for the in-process chat agent path):
    #[serde(default = "yes")] pub task_checkout_enabled: bool,
    #[serde(default = "yes")] pub task_submit_enabled: bool,
    #[serde(default = "yes")] pub task_fail_enabled: bool,
    #[serde(default = "yes")] pub task_heartbeat_enabled: bool,
    #[serde(default = "yes")] pub task_list_enabled: bool,
}

impl McpToolsConfig {
    /// Live check used by the dispatch path. Combines the per-tool
    /// flag with the relevant master gate.
    pub fn tool_allowed(&self, name: &str) -> bool {
        let is_write = matches!(
            name,
            "write_note" | "set_frontmatter" | "apply_tag" | "remove_tag"
        );
        if is_write && !self.writes_enabled {
            return false;
        }
        match name {
            "search_notes" => self.search_notes_enabled,
            "get_note" => self.get_note_enabled,
            "related_notes" => self.related_notes_enabled,
            "write_note" => self.write_note_enabled,
            "set_frontmatter" => self.set_frontmatter_enabled,
            "apply_tag" => self.apply_tag_enabled,
            "remove_tag" => self.remove_tag_enabled,
            "task_checkout" => self.task_checkout_enabled,
            "task_submit" => self.task_submit_enabled,
            "task_fail" => self.task_fail_enabled,
            "task_heartbeat" => self.task_heartbeat_enabled,
            "task_list" => self.task_list_enabled,
            _ => true, // unknown tools fall through to the dispatcher's "unknown tool" error
        }
    }
}

impl Default for McpToolsConfig {
    fn default() -> Self {
        Self {
            writes_enabled: true,
            allow_redacted_lookup: false,
            review_required: false,
            search_notes_enabled: true,
            get_note_enabled: true,
            related_notes_enabled: true,
            write_note_enabled: true,
            set_frontmatter_enabled: true,
            apply_tag_enabled: true,
            remove_tag_enabled: true,
            task_checkout_enabled: true,
            task_submit_enabled: true,
            task_fail_enabled: true,
            task_heartbeat_enabled: true,
            task_list_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuditConfig {
    /// Mirror of `[llm.audit] log_full_prompt`. When false (default), the
    /// JSONL audit log records call metadata but redacts large input bodies.
    #[serde(default = "no")]
    pub log_full_input: bool,
}

fn default_mcp_host() -> String {
    "127.0.0.1".to_string()
}

fn default_mcp_discovery_file() -> String {
    ".hiker/mcp.json".to_string()
}

fn default_mcp_max_top_k() -> u32 {
    50
}

/// `[search]` section. Holds discovery-panel state: which backends run by
/// default (mode toggles), and the per-section collapsed/expanded state
/// inside the panel. Vault-scoped via `settings-write-back`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default)]
    pub modes: SearchModesConfig,
    #[serde(default)]
    pub sections: SearchSectionsConfig,
    #[serde(default)]
    pub lexical: SearchLexicalConfig,
    #[serde(default)]
    pub semantic: SearchSemanticConfig,
}

/// `[search.lexical]` — per-mode option flags surfaced via the right-click
/// options menu on the Lexical (`Aa`) toggle. Defaults preserve current
/// behavior (everything off) so existing users see no change until they
/// reach into the menu.
///
/// status: search-lexical-options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchLexicalConfig {
    #[serde(default = "no")]
    pub case_sensitive: bool,
    #[serde(default = "no")]
    pub diacritic_sensitive: bool,
    #[serde(default = "no")]
    pub prefix_match: bool,
    #[serde(default = "no")]
    pub phrase_mode: bool,
}

impl Default for SearchLexicalConfig {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            diacritic_sensitive: false,
            prefix_match: false,
            phrase_mode: false,
        }
    }
}

/// `[search.semantic]` — per-mode option flags surfaced via the right-click
/// options menu on the Semantic (brain) toggle.
///
/// status: search-semantic-options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSemanticConfig {
    /// Cosine-similarity floor; hits below this are dropped before fusion.
    /// Range 0.00–0.95 in 0.05 steps; default 0.00 (no filter).
    /// status: search-semantic-min-similarity
    #[serde(default = "default_min_similarity")]
    pub min_similarity: f32,
    /// Override of `PER_BACKEND_TOP_K` for the semantic side only. Range
    /// 5–100; default 25 (matches the lexical side).
    /// status: search-semantic-top-k-override
    #[serde(default = "default_semantic_top_k")]
    pub top_k: u32,
    /// RRF blend of `notes.mtime` rank into the semantic score; off/mild/strong.
    /// status: search-semantic-recency-bias
    #[serde(default)]
    pub recency_bias: RecencyBias,
}

impl Default for SearchSemanticConfig {
    fn default() -> Self {
        Self {
            min_similarity: default_min_similarity(),
            top_k: default_semantic_top_k(),
            recency_bias: RecencyBias::default(),
        }
    }
}

fn default_min_similarity() -> f32 { 0.0 }
fn default_semantic_top_k() -> u32 { 25 }

/// Recency-bias weighting for the semantic engine. `Off` = no recency
/// blend (default); `Mild` / `Strong` mix mtime rank into the score via
/// the same RRF k=60 shape as cross-mode fusion (weights 0.5 / 1.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecencyBias {
    Off,
    Mild,
    Strong,
}

impl Default for RecencyBias {
    fn default() -> Self {
        RecencyBias::Off
    }
}

impl RecencyBias {
    pub fn weight(self) -> f32 {
        match self {
            RecencyBias::Off => 0.0,
            RecencyBias::Mild => 0.5,
            RecencyBias::Strong => 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchModesConfig {
    #[serde(default = "yes")]
    pub semantic: bool,
    #[serde(default = "yes")]
    pub lexical: bool,
}

impl Default for SearchModesConfig {
    fn default() -> Self {
        Self { semantic: true, lexical: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSectionsConfig {
    #[serde(default = "yes")]
    pub results_expanded: bool,
    #[serde(default = "yes")]
    pub related_expanded: bool,
}

impl Default for SearchSectionsConfig {
    fn default() -> Self {
        Self { results_expanded: true, related_expanded: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorConfig {
    #[serde(default = "yes")]
    pub render_txt_as_markdown: bool,
    #[serde(default = "yes")]
    pub live_preview: bool,
    #[serde(default = "yes")]
    pub word_wrap: bool,
    #[serde(default = "yes")]
    pub show_line_numbers: bool,
    #[serde(default = "no")]
    pub show_whitespace: bool,
    #[serde(default = "no")]
    pub show_chunk_boundaries: bool,
    #[serde(default = "no")]
    pub hide_frontmatter: bool,
    #[serde(default = "default_tab_size")]
    pub tab_size: u8,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            render_txt_as_markdown: true,
            live_preview: true,
            word_wrap: true,
            show_line_numbers: true,
            show_whitespace: false,
            show_chunk_boundaries: false,
            hide_frontmatter: false,
            tab_size: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: u16,
    #[serde(default)]
    pub ignored_paths: Vec<String>,
    /// Controls when notes get their `hiker.id` stamped into frontmatter.
    /// `lazy` (default) stamps only when a note becomes a reference target
    /// (e.g. trail waypoint, future wikilink). `all` stamps every note on
    /// first ingest. Both modes share the invariant that any *referenced*
    /// note is stamped.
    ///
    /// status: note-id-stamping
    #[serde(default)]
    pub id_stamping: IdStampingMode,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            batch_size: default_batch_size(),
            ignored_paths: Vec::new(),
            id_stamping: IdStampingMode::default(),
        }
    }
}

/// Note-id stamping policy. See `docs/trails.md` §"Note ID stamping policy".
///
/// status: note-id-stamping
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdStampingMode {
    /// Stamp every note on first ingest (lazy backfill via reindex).
    All,
    /// Stamp only when a note becomes a reference target. Default.
    Lazy,
}

impl Default for IdStampingMode {
    fn default() -> Self {
        IdStampingMode::Lazy
    }
}

fn default_model() -> String {
    "bge-small-en-v1.5".to_string()
}

fn default_batch_size() -> u16 {
    64
}

fn default_tab_size() -> u8 {
    2
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    #[serde(default)]
    pub recent: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default = "yes")]
    pub sidebar_open: bool,
    #[serde(default = "no")]
    pub related_open: bool,
    #[serde(default = "no")]
    pub trash_expanded: bool,
    /// Chat region height as a fraction of the discovery panel height.
    /// status: chat-panel-default-height
    #[serde(default = "default_chat_height")]
    pub chat_height: f32,
    /// Sidebar column width in CSS pixels.
    /// status: side-panel-resize
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u32,
    /// Discovery (right) column width in CSS pixels.
    /// status: side-panel-resize
    #[serde(default = "default_discovery_width")]
    pub discovery_width: u32,
    /// Surface `.hiker/sessions/` as a virtual top-level "Sessions" group
    /// in the tree. Default off — sessions stay hidden alongside other
    /// `.hiker/` sidecars. Search and related-notes always include
    /// sessions regardless of this toggle.
    /// status: chat-session-show-in-tree-toggle
    #[serde(default = "no")]
    pub show_sessions_in_tree: bool,
    /// Active sidebar mode (Files / Cluster trees / Trails).
    /// status: sidebar-mode-switcher
    #[serde(default)]
    pub sidebar_mode: SidebarMode,
    /// Vault-relative path of the currently active trail-doc, or `None`
    /// if no trail is active. At most one active trail per vault. Persisted
    /// via `set_setting` so it survives restarts.
    ///
    /// status: active-trail-state
    #[serde(default)]
    pub active_trail: Option<String>,
    /// User-set chat input height in CSS pixels, or 0 for auto-grow mode.
    #[serde(default)]
    pub chat_input_height: u32,
    #[serde(default)]
    pub tree: TreeConfig,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            recent: Vec::new(),
            default: None,
            sidebar_open: true,
            related_open: false,
            trash_expanded: false,
            chat_height: default_chat_height(),
            sidebar_width: default_sidebar_width(),
            discovery_width: default_discovery_width(),
            show_sessions_in_tree: false,
            sidebar_mode: SidebarMode::default(),
            active_trail: None,
            chat_input_height: 0,
            tree: TreeConfig::default(),
        }
    }
}

fn default_chat_height() -> f32 {
    0.30
}

fn default_sidebar_width() -> u32 {
    280
}

fn default_discovery_width() -> u32 {
    320
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    #[serde(default)]
    pub sort_by: TreeSortBy,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            sort_by: TreeSortBy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeSortBy {
    NameAsc,
    NameDesc,
    MtimeDesc,
    MtimeAsc,
}

impl Default for TreeSortBy {
    fn default() -> Self {
        TreeSortBy::NameAsc
    }
}

/// Active sidebar mode. Persisted per-vault under `vault.sidebar_mode`.
/// status: sidebar-mode-switcher
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarMode {
    Files,
    Clusters,
    Trails,
}

impl Default for SidebarMode {
    fn default() -> Self {
        SidebarMode::Files
    }
}

/// Whether a write-back targets the per-user TOML or the per-vault TOML.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsScope {
    User,
    Vault,
}

/// Resolved file paths for the two TOMLs. `user` is `None` when the
/// platform config dir can't be resolved (rare — sandboxed test envs).
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub vault: PathBuf,
}

impl ConfigPaths {
    pub fn resolve(vault_root: &Path) -> Self {
        let user = directories::ProjectDirs::from("", "", "hiker")
            .map(|p| p.config_dir().join("config.toml"));
        let vault = vault_root.join(".hiker").join("config.toml");
        Self { user, vault }
    }
}

impl Config {
    /// Read only the per-user TOML and return its `vault.default` field if
    /// set. Used at app bootstrap (before any vault is open) to decide
    /// whether to auto-open a default vault. Returns `Ok(None)` if the
    /// platform config dir can't be resolved, the user TOML doesn't exist
    /// yet, or the field is unset. Errors only on real I/O / parse
    /// failures so a malformed TOML still aborts loudly.
    pub fn user_default_vault() -> Result<Option<String>, HikerError> {
        let user_path = match directories::ProjectDirs::from("", "", "hiker") {
            Some(p) => p.config_dir().join("config.toml"),
            None => return Ok(None),
        };
        if !user_path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&user_path).map_err(|e| {
            tracing::error!(file = %user_path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", user_path.display()))
        })?;
        let doc: toml::Value = toml::from_str(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(file = %user_path.display(), error = %e, "settings parse failed");
            HikerError::Config(format!("parse {}: {e}", user_path.display()))
        })?;
        Ok(doc
            .get("vault")
            .and_then(|v| v.get("default"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }

    /// Read a single file (user or vault TOML) without merging or
    /// triggering auto-create. Missing files return `Config::default()` so
    /// the settings UI's per-section scope toggle can show "what this file
    /// alone contributes" against the current schema's defaults. Parse
    /// errors and unknown-field errors bubble up — the same strict-load
    /// posture as `Config::load`.
    ///
    /// status: settings-pane-scope-toggle
    pub fn read_file_only(scope: SettingsScope, vault_root: &Path) -> Result<Self, HikerError> {
        let paths = ConfigPaths::resolve(vault_root);
        let path = match scope {
            SettingsScope::User => match paths.user.as_ref() {
                Some(p) => p.clone(),
                None => return Ok(Self::default()),
            },
            SettingsScope::Vault => paths.vault.clone(),
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path).map_err(|e| {
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    }

    /// Load and merge the user + vault TOMLs. Auto-creates either file with
    /// the current defaults if missing. Strict: any unknown key, type
    /// mismatch, or schema-version mismatch aborts with a clear error.
    pub fn load(vault_root: &Path) -> Result<Self, HikerError> {
        let paths = ConfigPaths::resolve(vault_root);

        // User file: best-effort. If we couldn't resolve the platform config
        // dir, treat it as empty rather than failing — vault TOML can still
        // carry everything the user needs.
        let user_doc = match paths.user.as_ref() {
            Some(p) => Some(read_or_create(p, &Self::default())?),
            None => None,
        };

        let vault_doc = read_or_create_minimal(&paths.vault)?;

        // Deep-merge user under vault (vault wins per-key). Tables recurse;
        // arrays and scalars replace.
        let mut merged: toml::Value = match user_doc {
            Some(u) => u,
            None => toml::Value::Table(toml::map::Map::new()),
        };
        deep_merge(&mut merged, vault_doc);

        // Schema-version check fires before deserialization so users get a
        // helpful "schema N, expected M" instead of an unknown-field error
        // from a future binary's keys.
        if let Some(toml::Value::Integer(v)) = merged.get("schema_version") {
            if *v as u32 != SCHEMA_VERSION {
                let user_disp = display_path(paths.user.as_deref());
                let vault_disp = paths.vault.display().to_string();
                tracing::error!(
                    user_file = %user_disp,
                    vault_file = %vault_disp,
                    found = *v,
                    expected = SCHEMA_VERSION,
                    "settings schema_version mismatch",
                );
                return Err(HikerError::Config(format!(
                    "settings schema_version {v}, this binary expects {SCHEMA_VERSION} (user={user_disp}, vault={vault_disp})"
                )));
            }
        }

        // Both files have already parsed cleanly via `read_or_create`; if
        // try_into fails here it's an unknown key or type mismatch from the
        // *merged* view, so we can't single out which file contributed it
        // without a per-file trial-deserialize. Surface both paths so the
        // user can grep.
        let cfg: Config = merged.try_into().map_err(|e: toml::de::Error| {
            let user_disp = display_path(paths.user.as_deref());
            let vault_disp = paths.vault.display().to_string();
            tracing::error!(
                user_file = %user_disp,
                vault_file = %vault_disp,
                error = %e,
                "settings strict-load rejected merged config",
            );
            HikerError::Config(format!(
                "invalid settings (user={user_disp}, vault={vault_disp}): {e}"
            ))
        })?;

        // Cross-field validation: model is the only supported value in v1
        // (the field exists for forward compatibility). batch_size must be
        // non-zero.
        if cfg.indexing.model != default_model() {
            tracing::error!(
                key = "indexing.model",
                value = %cfg.indexing.model,
                "unsupported settings value",
            );
            return Err(HikerError::Config(format!(
                "indexing.model = \"{}\" — only \"{}\" is supported in v1",
                cfg.indexing.model,
                default_model(),
            )));
        }
        if cfg.indexing.batch_size == 0 {
            tracing::error!(key = "indexing.batch_size", "value must be > 0");
            return Err(HikerError::Config(
                "indexing.batch_size must be > 0".to_string(),
            ));
        }

        Ok(cfg)
    }

    /// Write the new value through to the appropriate TOML on disk and
    /// return the freshly-loaded merged Config so the caller can swap its
    /// in-memory copy. The eligible-key set is closed: only the keys with a
    /// real-time UI control accept writes. Anything else returns
    /// `HikerError::Config`.
    pub fn set(
        scope: SettingsScope,
        key: &str,
        value: serde_json::Value,
        vault_root: &Path,
    ) -> Result<Self, HikerError> {
        let allowed = eligible_key(scope, key)?;
        validate_value(&allowed, &value)?;

        let paths = ConfigPaths::resolve(vault_root);
        let target = match scope {
            SettingsScope::User => paths
                .user
                .clone()
                .ok_or_else(|| HikerError::Config("no platform config dir available".into()))?,
            SettingsScope::Vault => paths.vault.clone(),
        };

        // Read-or-create the target file. For user-scope writes we seed
        // full defaults so the user can see available keys; for vault-scope
        // writes we seed only schema_version to avoid auto-created defaults
        // silently overriding user settings (e.g. LLM provider backend).
        let mut doc = match scope {
            SettingsScope::User => read_or_create_doc(&target, &Self::default())?,
            SettingsScope::Vault => read_or_create_minimal_doc(&target)?,
        };
        apply_patch(&mut doc, key, &value);
        atomic_write(&target, doc.to_string().as_bytes())?;

        // Reload through the normal path so the returned Config reflects
        // the merged state across both files.
        Self::load(vault_root)
    }
}

/// Deep-merge `override_v` onto `base` in place. Tables recurse; arrays
/// and scalars replace.
fn deep_merge(base: &mut toml::Value, override_v: toml::Value) {
    use toml::Value;
    match (base, override_v) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other;
        }
    }
}

fn display_path(p: Option<&Path>) -> String {
    match p {
        Some(p) => p.display().to_string(),
        None => "<unset>".to_string(),
    }
}

/// Read the file as a `toml::Value`. If missing, write the defaults
/// serialized in full and return that.
fn read_or_create(path: &Path, defaults: &Config) -> Result<toml::Value, HikerError> {
    if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| {
            tracing::error!(file = %path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            // toml::de::Error::span() is private, but the Display impl
            // already includes line/col when known. Fields kept structured
            // per `obs-error-context`; the stringified message preserves
            // the parser's positional info.
            tracing::error!(
                file = %path.display(),
                error = %e,
                "settings parse failed",
            );
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    } else {
        write_defaults(path, defaults)?;
        Ok(toml_value_from_serde(defaults))
    }
}

/// Like `read_or_create` but seeds only `schema_version` in the
/// auto-created file. Used for the vault TOML so auto-created vault
/// defaults don't silently override user-scope settings (e.g. LLM
/// provider backend). If the file already exists, reads it normally.
fn read_or_create_minimal(path: &Path) -> Result<toml::Value, HikerError> {
    if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| {
            tracing::error!(file = %path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(
                file = %path.display(),
                error = %e,
                "settings parse failed",
            );
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                HikerError::Config(format!("mkdir {}: {e}", parent.display()))
            })?;
        }
        let header = format!(
            "# Hiker vault settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n\
             # This file was auto-generated. Add per-vault overrides here;\n\
             # user-scope settings (LLM provider, API keys, etc.) live in your user config.toml.\n\n"
        );
        let body = format!("schema_version = {SCHEMA_VERSION}\n");
        let bytes = format!("{header}{body}");
        atomic_write(path, bytes.as_bytes())?;
        let mut map = toml::map::Map::new();
        map.insert("schema_version".into(), toml::Value::Integer(SCHEMA_VERSION as i64));
        Ok(toml::Value::Table(map))
    }
}

/// Same as `read_or_create` but returns a `toml_edit::DocumentMut` for
/// in-place patching.
fn read_or_create_doc(
    path: &Path,
    defaults: &Config,
) -> Result<toml_edit::DocumentMut, HikerError> {
    if !path.exists() {
        write_defaults(path, defaults)?;
    }
    let raw = fs::read_to_string(path).map_err(|e| {
        HikerError::Config(format!("read {}: {e}", path.display()))
    })?;
    raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        HikerError::Config(format!("parse {}: {e}", path.display()))
    })
}

/// Same as `read_or_create_doc` but seeds only `schema_version`.
/// Used for vault-scope write-back to avoid auto-created defaults
/// overriding user settings.
fn read_or_create_minimal_doc(path: &Path) -> Result<toml_edit::DocumentMut, HikerError> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                HikerError::Config(format!("mkdir {}: {e}", parent.display()))
            })?;
        }
        let header = format!(
            "# Hiker vault settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n\
             # This file was auto-generated. Add per-vault overrides here;\n\
             # user-scope settings (LLM provider, API keys, etc.) live in your user config.toml.\n\n"
        );
        let body = format!("schema_version = {SCHEMA_VERSION}\n");
        atomic_write(path, format!("{header}{body}").as_bytes())?;
    }
    let raw = fs::read_to_string(path).map_err(|e| {
        HikerError::Config(format!("read {}: {e}", path.display()))
    })?;
    raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        HikerError::Config(format!("parse {}: {e}", path.display()))
    })
}

fn toml_value_from_serde(cfg: &Config) -> toml::Value {
    toml::Value::try_from(cfg).expect("Config serializes cleanly")
}

fn write_defaults(path: &Path, defaults: &Config) -> Result<(), HikerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            HikerError::Config(format!("mkdir {}: {e}", parent.display()))
        })?;
    }
    let header = format!(
        "# Hiker settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n# This file was auto-generated with the current defaults; edit freely.\n\n"
    );
    let body = toml::to_string_pretty(defaults).map_err(|e| {
        HikerError::Config(format!("serialize defaults: {e}"))
    })?;
    let bytes = format!("{header}{body}");
    atomic_write(path, bytes.as_bytes())
}

/// Atomic write: write to `<path>.tmp`, then rename. Avoids leaving a
/// half-written file if the process dies mid-write.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), HikerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            HikerError::Config(format!("mkdir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, bytes).map_err(|e| {
        HikerError::Config(format!("write {}: {e}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|e| {
        HikerError::Config(format!("rename {} → {}: {e}", tmp.display(), path.display()))
    })?;
    Ok(())
}

/// One node of the eligible-key set: dotted path + expected JSON-side type.
#[derive(Debug, Clone, Copy)]
struct EligibleKey {
    /// Dotted path, e.g. `"editor.live_preview"`. The first component is
    /// the section, then leaf or sub-table.
    path: &'static str,
    ty: ValueType,
}

#[derive(Debug, Clone, Copy)]
enum ValueType {
    Bool,
    String,
    StringArray,
    /// `name_asc | name_desc | mtime_desc | mtime_asc`.
    TreeSortBy,
    /// `files | clusters | trails`.
    SidebarMode,
    /// Floating-point fraction in `[0.0, 1.0]`.
    UnitFraction,
    /// Positive integer (fits in u32). Used for the LLM/agent knobs
    /// (`max_tokens`, `iteration_cap`, etc.) where 0 is meaningless.
    PositiveInt,
    /// Non-negative integer (fits in u32). 0 is meaningful — used for
    /// `vault.chat_input_height` (0 = auto-grow, >0 = user-set px).
    NonNegativeInt,
    /// `auto | internal | external` for `[tasks] worker_preference`.
    WorkerPreference,
    /// `0..=65535` — used for `[mcp] port`. Distinct from `PositiveInt`
    /// because port `0` means "ephemeral / OS-assigned" and is valid.
    Port,
    /// Floating-point in `[0.0, 0.95]` — `[search.semantic] min_similarity`.
    SemanticMinSim,
    /// `5..=100` — `[search.semantic] top_k`.
    SemanticTopK,
    /// `off | mild | strong` for `[search.semantic] recency_bias`.
    RecencyBias,
    /// `all | lazy` for `[indexing] id_stamping`.
    IdStamping,
}

const ELIGIBLE_VAULT: &[EligibleKey] = &[
    EligibleKey { path: "editor.render_txt_as_markdown", ty: ValueType::Bool },
    EligibleKey { path: "editor.live_preview",           ty: ValueType::Bool },
    EligibleKey { path: "editor.word_wrap",              ty: ValueType::Bool },
    EligibleKey { path: "editor.show_line_numbers",      ty: ValueType::Bool },
    EligibleKey { path: "editor.show_whitespace",        ty: ValueType::Bool },
    EligibleKey { path: "editor.show_chunk_boundaries",  ty: ValueType::Bool },
    EligibleKey { path: "editor.hide_frontmatter",       ty: ValueType::Bool },
    EligibleKey { path: "vault.sidebar_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.related_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.trash_expanded",          ty: ValueType::Bool },
    EligibleKey { path: "vault.chat_height",             ty: ValueType::UnitFraction },
    EligibleKey { path: "vault.chat_input_height",       ty: ValueType::NonNegativeInt },
    EligibleKey { path: "vault.sidebar_width",           ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.discovery_width",         ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.show_sessions_in_tree",   ty: ValueType::Bool },
    EligibleKey { path: "vault.sidebar_mode",            ty: ValueType::SidebarMode },
    // status: active-trail-state
    EligibleKey { path: "vault.active_trail",            ty: ValueType::String },
    EligibleKey { path: "vault.tree.sort_by",            ty: ValueType::TreeSortBy },
    // status: trails-default-location
    EligibleKey { path: "trails.new_trail_dir",          ty: ValueType::String },
    // status: note-id-stamping
    EligibleKey { path: "indexing.id_stamping",          ty: ValueType::IdStamping },
    EligibleKey { path: "search.modes.semantic",         ty: ValueType::Bool },
    EligibleKey { path: "search.modes.lexical",          ty: ValueType::Bool },
    EligibleKey { path: "search.sections.results_expanded", ty: ValueType::Bool },
    EligibleKey { path: "search.sections.related_expanded", ty: ValueType::Bool },
    // status: search-lexical-options, search-semantic-options
    EligibleKey { path: "search.lexical.case_sensitive",     ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.diacritic_sensitive",ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.prefix_match",       ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.phrase_mode",        ty: ValueType::Bool },
    EligibleKey { path: "search.semantic.min_similarity",    ty: ValueType::SemanticMinSim },
    EligibleKey { path: "search.semantic.top_k",             ty: ValueType::SemanticTopK },
    EligibleKey { path: "search.semantic.recency_bias",      ty: ValueType::RecencyBias },
    // LLM section. Per-vault override (provider key / model / cap can
    // be tuned per workspace) shares the same eligibility set as user
    // scope so the per-section [User]/[Vault] toggle in the settings
    // pane can write either side.
    EligibleKey { path: "llm.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "llm.provider.backend",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.model",               ty: ValueType::String },
    EligibleKey { path: "llm.provider.api_key_env",         ty: ValueType::String },
    // `llm.provider.api_key` deliberately omitted from the vault list:
    // the literal key must never travel with a synced vault TOML. See
    // `llm.md` §`[llm-providers-config]` and `ELIGIBLE_USER` below.
    EligibleKey { path: "llm.provider.base_url",            ty: ValueType::String },
    EligibleKey { path: "llm.limits.max_tokens",            ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.limits.timeout_secs",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.iteration_cap",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.tool_timeout_secs",      ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.audit.log_full_prompt",        ty: ValueType::Bool },
    // [tasks] section. Per-vault override: every key is eligible at
    // vault scope per `task-queue-settings-section`.
    EligibleKey { path: "tasks.worker_preference",          ty: ValueType::WorkerPreference },
    EligibleKey { path: "tasks.terminal_retention_secs",    ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.direct_worker.enabled",      ty: ValueType::Bool },
    EligibleKey { path: "tasks.direct_worker.parallelism",  ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.expose_to_chat_agent",       ty: ValueType::Bool },
    EligibleKey { path: "tasks.lease.default_secs",         ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.lease.max_secs",             ty: ValueType::PositiveInt },
    // status: mcp-settings-ui-section
    // [mcp] section. Vault-scope by default — the discovery file lives
    // in the vault, so per-vault overrides are the natural shape.
    EligibleKey { path: "mcp.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "mcp.host",                         ty: ValueType::String },
    EligibleKey { path: "mcp.port",                         ty: ValueType::Port },
    EligibleKey { path: "mcp.max_top_k",                    ty: ValueType::PositiveInt },
    // [mcp.tools] — master gates + per-tool toggles
    // (status: mcp-tool-toggles).
    EligibleKey { path: "mcp.tools.writes_enabled",         ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.allow_redacted_lookup",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.search_notes_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.get_note_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.related_notes_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.write_note_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.set_frontmatter_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.apply_tag_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.remove_tag_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_checkout_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_submit_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_fail_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_heartbeat_enabled", ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_list_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.review_required",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.audit.log_full_input",         ty: ValueType::Bool },
    // status: agent-write-review-mode
    EligibleKey { path: "llm.background.review_required",   ty: ValueType::Bool },
    // ACP section. The agent can be overridden per vault.
    // Also eligible at user scope for a global default.
    EligibleKey { path: "acp.command",                      ty: ValueType::String },
];

const ELIGIBLE_USER: &[EligibleKey] = &[
    EligibleKey { path: "vault.recent",  ty: ValueType::StringArray },
    EligibleKey { path: "vault.default", ty: ValueType::String },
    // LLM section. Default scope for the settings pane is `user` so
    // API-key env name + provider live in the platform config dir; the
    // vault TOML can still override per-workspace via the eligible-vault
    // duplicates above.
    EligibleKey { path: "llm.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "llm.provider.backend",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.model",               ty: ValueType::String },
    EligibleKey { path: "llm.provider.api_key_env",         ty: ValueType::String },
    // `api_key` (literal) is user-scope only — see the spec posture in
    // `llm.md`. The vault eligibility list above intentionally omits it
    // so a `set_setting(Vault, "llm.provider.api_key", ...)` call is
    // rejected with the standard "not user-mutable in v1" error.
    EligibleKey { path: "llm.provider.api_key",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.base_url",            ty: ValueType::String },
    EligibleKey { path: "llm.limits.max_tokens",            ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.limits.timeout_secs",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.iteration_cap",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.tool_timeout_secs",      ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.audit.log_full_prompt",        ty: ValueType::Bool },
    // worker_preference is also valid at user scope (per `task-queue.md`'s
    // settings eligibility note); the rest of `[tasks]` is vault-only.
    EligibleKey { path: "tasks.worker_preference",          ty: ValueType::WorkerPreference },
    // [mcp] — user scope is supported as a global default (the vault
    // table above wins per `core::config` merge order).
    EligibleKey { path: "mcp.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "mcp.host",                         ty: ValueType::String },
    EligibleKey { path: "mcp.port",                         ty: ValueType::Port },
    EligibleKey { path: "mcp.max_top_k",                    ty: ValueType::PositiveInt },
    EligibleKey { path: "mcp.tools.writes_enabled",         ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.allow_redacted_lookup",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.search_notes_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.get_note_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.related_notes_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.write_note_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.set_frontmatter_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.apply_tag_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.remove_tag_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_checkout_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_submit_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_fail_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_heartbeat_enabled", ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_list_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.review_required",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.audit.log_full_input",         ty: ValueType::Bool },
    // status: agent-write-review-mode
    EligibleKey { path: "llm.background.review_required",   ty: ValueType::Bool },
    // ACP section. Also eligible at user scope for a global default.
    EligibleKey { path: "acp.command",                      ty: ValueType::String },
];

fn eligible_key(scope: SettingsScope, key: &str) -> Result<EligibleKey, HikerError> {
    let table = match scope {
        SettingsScope::User => ELIGIBLE_USER,
        SettingsScope::Vault => ELIGIBLE_VAULT,
    };
    table
        .iter()
        .copied()
        .find(|k| k.path == key)
        .ok_or_else(|| {
            HikerError::Config(format!(
                "setting `{key}` is not user-mutable in v1 (scope: {scope:?})"
            ))
        })
}

fn validate_value(key: &EligibleKey, value: &serde_json::Value) -> Result<(), HikerError> {
    use serde_json::Value as J;
    let ok = match (key.ty, value) {
        (ValueType::Bool, J::Bool(_)) => true,
        (ValueType::String, J::String(_)) => true,
        (ValueType::String, J::Null) => true,
        (ValueType::StringArray, J::Array(arr)) => arr.iter().all(|v| v.is_string()),
        (ValueType::TreeSortBy, J::String(s)) => matches!(
            s.as_str(),
            "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc"
        ),
        (ValueType::SidebarMode, J::String(s)) => {
            matches!(s.as_str(), "files" | "clusters" | "trails")
        }
        (ValueType::UnitFraction, J::Number(n)) => n
            .as_f64()
            .map(|f| (0.0..=1.0).contains(&f))
            .unwrap_or(false),
        // Positive integer that fits u32. JSON.stringify on a JS
        // number-without-fraction parses back as an integer, so
        // `as_u64` returns Some only for true integer values; floats
        // are rejected, which is what we want for `max_tokens` etc.
        (ValueType::PositiveInt, J::Number(n)) => n
            .as_u64()
            .map(|u| u >= 1 && u <= u32::MAX as u64)
            .unwrap_or(false),
        (ValueType::NonNegativeInt, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= u32::MAX as u64)
            .unwrap_or(false),
        (ValueType::WorkerPreference, J::String(s)) => {
            matches!(s.as_str(), "auto" | "internal" | "external")
        }
        (ValueType::Port, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= u16::MAX as u64)
            .unwrap_or(false),
        (ValueType::SemanticMinSim, J::Number(n)) => n
            .as_f64()
            .map(|f| (0.0..=0.95).contains(&f))
            .unwrap_or(false),
        (ValueType::SemanticTopK, J::Number(n)) => n
            .as_u64()
            .map(|u| (5..=100).contains(&u))
            .unwrap_or(false),
        (ValueType::RecencyBias, J::String(s)) => {
            matches!(s.as_str(), "off" | "mild" | "strong")
        }
        (ValueType::IdStamping, J::String(s)) => {
            matches!(s.as_str(), "all" | "lazy")
        }
        _ => false,
    };
    if !ok {
        return Err(HikerError::Config(format!(
            "setting `{}` got invalid value `{value}`",
            key.path
        )));
    }
    Ok(())
}

/// Patch `doc` so the dotted-path key resolves to `value`. Creates any
/// intermediate tables that don't exist.
fn apply_patch(doc: &mut toml_edit::DocumentMut, key: &str, value: &serde_json::Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let item = json_to_toml_item(value);

    // Walk to the parent table, creating intermediate tables as we go.
    let mut cursor: &mut toml_edit::Item = doc.as_item_mut();
    for part in &parts[..parts.len() - 1] {
        // If the slot is missing or not a table, replace with an empty table.
        let needs_replace = !matches!(cursor.get(part), Some(toml_edit::Item::Table(_)));
        if needs_replace {
            // `cursor` here may be the root document or a sub-table.
            match cursor {
                toml_edit::Item::Table(t) => {
                    t.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                _ => {
                    // The parent isn't a table — replace it wholesale.
                    *cursor = toml_edit::Item::Table(toml_edit::Table::new());
                    if let toml_edit::Item::Table(t) = cursor {
                        t.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
                    }
                }
            }
        }
        cursor = cursor
            .get_mut(part)
            .expect("intermediate slot was just ensured to be a Table");
    }

    let leaf = parts[parts.len() - 1];
    match cursor {
        toml_edit::Item::Table(t) => {
            t.insert(leaf, item);
        }
        _ => {
            // Same fallback as above: ensure the parent is a table.
            *cursor = toml_edit::Item::Table(toml_edit::Table::new());
            if let toml_edit::Item::Table(t) = cursor {
                t.insert(leaf, item);
            }
        }
    }
}

fn json_to_toml_item(value: &serde_json::Value) -> toml_edit::Item {
    use serde_json::Value as J;
    match value {
        J::Bool(b) => toml_edit::value(*b),
        J::String(s) => toml_edit::value(s.as_str()),
        J::Number(n) => {
            // Try integer before float — `serde_json::Number::as_f64`
            // succeeds for both shapes, and routing every integer
            // through the float branch would write `4096.0` to TOML
            // and then fail strict-load against `u32` fields. JSON
            // produced by JS for an integer (no decimal point) parses
            // here with `as_i64() = Some(_)`, so this branch wins for
            // PositiveInt rows; floats (e.g. `vault.chat_height = 0.3`)
            // fall through to the float branch as before.
            if let Some(i) = n.as_i64() {
                toml_edit::value(i)
            } else if let Some(f) = n.as_f64() {
                toml_edit::value(f)
            } else {
                toml_edit::Item::None
            }
        }
        J::Null => toml_edit::Item::None,
        J::Array(arr) => {
            let mut a = toml_edit::Array::new();
            for v in arr {
                if let J::String(s) = v {
                    a.push(s.as_str());
                }
            }
            toml_edit::value(a)
        }
        J::Object(_) => {
            // validate_value rejects this for our eligible-key set, so this
            // branch is unreachable in practice. Falling back to None keeps
            // the function total without panicking.
            toml_edit::Item::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_round_trip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg.editor.live_preview, back.editor.live_preview);
        assert_eq!(cfg.indexing.batch_size, back.indexing.batch_size);
    }

    #[test]
    fn unknown_key_rejected() {
        let bad = "schema_version = 1\nmystery_key = true\n";
        let err = toml::from_str::<Config>(bad).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mystery_key"), "got: {msg}");
    }

    #[test]
    fn unknown_section_key_rejected() {
        let bad = "[editor]\nrandom = true\n";
        let err = toml::from_str::<Config>(bad).unwrap_err();
        assert!(err.to_string().contains("random"));
    }

    #[test]
    fn auto_create_writes_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".hiker").join("config.toml");
        assert!(!path.exists());
        let _ = read_or_create(&path, &Config::default()).unwrap();
        assert!(path.exists());
        let raw = fs::read_to_string(&path).unwrap();
        // Header comment + serialized defaults.
        assert!(raw.contains("# Hiker settings"));
        assert!(raw.contains("[editor]"));
        assert!(raw.contains("render_txt_as_markdown = true"));
    }

    #[test]
    fn deep_merge_vault_wins() {
        let mut base: toml::Value = toml::from_str(
            r#"schema_version = 1
[editor]
live_preview = true
word_wrap = false
"#,
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            r#"[editor]
word_wrap = true
"#,
        )
        .unwrap();
        deep_merge(&mut base, over);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.editor.live_preview, true);
        assert_eq!(cfg.editor.word_wrap, true);
    }

    #[test]
    fn deep_merge_arrays_replace() {
        let mut base: toml::Value = toml::from_str(
            r#"[indexing]
model = "bge-small-en-v1.5"
ignored_paths = ["foo/"]
"#,
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            r#"[indexing]
ignored_paths = ["bar/"]
"#,
        )
        .unwrap();
        deep_merge(&mut base, over);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.indexing.ignored_paths, vec!["bar/".to_string()]);
    }

    #[test]
    fn schema_version_mismatch_errors() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join(".hiker").join("config.toml");
        fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        fs::write(&vault_path, "schema_version = 999\n").unwrap();
        // Force the user-side path empty by using a separate vault dir; the
        // user TOML will auto-create defaults at the platform config dir
        // which is fine — vault wins on schema_version.
        let err = Config::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version 999"));
    }

    #[test]
    fn write_back_patches_in_place_preserving_comments() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join(".hiker").join("config.toml");
        fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        // Hand-written TOML with a comment that must survive.
        fs::write(
            &vault_path,
            "schema_version = 1\n\n# my preferred toggles\n[editor]\nlive_preview = true\n",
        )
        .unwrap();
        Config::set(
            SettingsScope::Vault,
            "editor.live_preview",
            serde_json::Value::Bool(false),
            dir.path(),
        )
        .unwrap();
        let raw = fs::read_to_string(&vault_path).unwrap();
        assert!(raw.contains("# my preferred toggles"), "comment lost: {raw}");
        assert!(raw.contains("live_preview = false"), "value not patched: {raw}");
    }

    #[test]
    fn write_back_rejects_non_eligible_key() {
        let dir = tempdir().unwrap();
        let err = Config::set(
            SettingsScope::Vault,
            "editor.tab_size",
            serde_json::Value::Number(4.into()),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not user-mutable"));
    }

    #[test]
    fn write_back_rejects_wrong_type() {
        let dir = tempdir().unwrap();
        let err = Config::set(
            SettingsScope::Vault,
            "editor.live_preview",
            serde_json::Value::String("yes please".into()),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid value"));
    }

    #[test]
    fn write_back_creates_new_table_path() {
        let dir = tempdir().unwrap();
        // Brand-new vault, no TOML yet. Set a nested key and confirm it
        // landed at the correct path.
        let cfg = Config::set(
            SettingsScope::Vault,
            "vault.tree.sort_by",
            serde_json::Value::String("mtime_desc".into()),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.vault.tree.sort_by, TreeSortBy::MtimeDesc);
    }

    #[test]
    fn write_back_positive_int_persists_as_integer_not_float() {
        // Regression test: a JS-side `4096` arrives as a JSON integer.
        // We need it written to TOML as `4096`, not `4096.0`, so the
        // strict-load `u32`-typed reader doesn't reject it on the next
        // launch.
        let dir = tempdir().unwrap();
        let cfg = Config::set(
            SettingsScope::Vault,
            "llm.limits.max_tokens",
            serde_json::json!(4096),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.llm.limits.max_tokens, 4096);
        // Confirm the on-disk shape is integer-valued.
        let raw = fs::read_to_string(dir.path().join(".hiker").join("config.toml")).unwrap();
        assert!(
            raw.contains("max_tokens = 4096") && !raw.contains("max_tokens = 4096.0"),
            "expected `max_tokens = 4096` in TOML, got:\n{raw}"
        );
    }

    #[test]
    fn write_back_positive_int_rejects_zero_and_floats() {
        let dir = tempdir().unwrap();
        // Zero is not a positive integer.
        assert!(Config::set(
            SettingsScope::Vault,
            "llm.agent.iteration_cap",
            serde_json::json!(0),
            dir.path(),
        )
        .is_err());
        // Float is rejected even when its value is integer-equivalent;
        // serde_json carries the no-decimal vs. decimal distinction.
        assert!(Config::set(
            SettingsScope::Vault,
            "llm.agent.iteration_cap",
            serde_json::json!(10.5),
            dir.path(),
        )
        .is_err());
    }

    #[test]
    fn write_back_api_key_refused_in_vault_scope() {
        // Spec posture: the literal API key must never live in the
        // vault TOML (which travels with Syncthing/git). The
        // eligibility list refuses the write.
        let dir = tempdir().unwrap();
        let err = Config::set(
            SettingsScope::Vault,
            "llm.provider.api_key",
            serde_json::json!("sk-secret"),
            dir.path(),
        )
        .expect_err("vault scope must refuse api_key");
        let msg = err.to_string();
        assert!(
            msg.contains("api_key") && msg.contains("not user-mutable"),
            "got: {msg}",
        );
        // User scope still lists the key even though the actual on-disk
        // write goes to the platform config dir (skipped here for test
        // isolation; see write_back_llm_keys_eligible_via_vault_scope).
        assert!(eligible_key(SettingsScope::User, "llm.provider.api_key").is_ok());
    }

    #[test]
    fn write_back_llm_keys_eligible_via_vault_scope() {
        let dir = tempdir().unwrap();
        // The settings pane's per-section [User]/[Vault] toggle relies
        // on the LLM keys being writable from either side. We assert
        // the vault-scope path here — the user-scope write goes to the
        // platform config dir which isn't isolated per test, but
        // `eligible_key` covers both scopes uniformly via ELIGIBLE_USER
        // / ELIGIBLE_VAULT (both lists carry the LLM keys).
        let cfg = Config::set(
            SettingsScope::Vault,
            "llm.provider.model",
            serde_json::json!("claude-haiku-4-5"),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.llm.provider.model, "claude-haiku-4-5");
        // Spot-check the eligibility lookup directly so the both-scope
        // promise doesn't regress.
        assert!(eligible_key(SettingsScope::User, "llm.provider.api_key_env").is_ok());
        assert!(eligible_key(SettingsScope::Vault, "llm.audit.log_full_prompt").is_ok());
    }
}
