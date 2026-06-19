//! Sub-configuration sections: structs, `Default` impls, and `default_*`
//! helpers used as serde defaults. Split out of `core::config` for file-length
//! reasons; the parent module re-exports everything here as `hiker_core::config::*`.

use serde::{Deserialize, Serialize};

pub(super) const fn yes() -> bool {
    true
}

pub(super) const fn no() -> bool {
    false
}

/// `[suggestions]` + `[suggestions.triage]` config. See
/// `docs/suggestions.md` §"`[suggestions.triage]` config section". The
/// outer table currently only carries the triage subsection; the
/// `tag_field` and other knobs land alongside their feature wiring.
///
/// status: triage-review-required
/// status: triage-staging-proposals
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuggestionsConfig {
    #[serde(default)]
    pub triage: TriageConfig,
}

/// `[suggestions.triage]` subsection. Triage-level behavior — auto-accept
/// gating, source-folder safety boundary, optional scheduled re-run.
///
/// status: triage-review-required
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageConfig {
    /// When `true`, every triage match stays pending in the layered doc
    /// until the user accepts. When `false`, `auto-*` matches auto-accept
    /// at insert time. Live-applied.
    #[serde(default = "no")]
    pub review_required: bool,
    /// Source-folder boundary for triage moves. Triage never produces a
    /// `move_note` row whose `source_path` is outside this folder. Also
    /// drives the on-save trigger's default folder.
    #[serde(default = "default_triage_scope")]
    pub scope: String,
    /// Duration-string grammar (`30m` / `1h` / `6h` / `24h` / `7d`); empty
    /// disables. Per `cluster-editor-triage-scheduled-rerun`. NOTE: the spec
    /// calls for cron-shape values (e.g. `"0 3 * * *"`); the runtime currently
    /// accepts only the duration grammar above and silently logs + disables
    /// anything else. Cron-shape support is tracked as
    /// `cluster-editor-triage-scheduled-rerun-cron-syntax`.
    #[serde(default)]
    pub scheduled_rerun: String,
    /// Opt-in: re-run triage when a note's embedding shifts beyond
    /// `modified_rerun_cosine_guard` distance from its last triaged
    /// state. Per `cluster-editor-triage-modified-rerun`. Default off.
    #[serde(default = "no")]
    pub modified_rerun: bool,
    /// Cosine-distance threshold gating the modified-note rerun
    /// (0.0 = always re-run; 1.0 = never). Default 0.15 — typical save
    /// noise sits below this; meaningful edits clear it.
    #[serde(default = "default_modified_rerun_cosine_guard")]
    pub modified_rerun_cosine_guard: f32,
}

const fn default_modified_rerun_cosine_guard() -> f32 {
    0.15
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            review_required: false,
            scope: default_triage_scope(),
            scheduled_rerun: String::new(),
            modified_rerun: false,
            modified_rerun_cosine_guard: default_modified_rerun_cosine_guard(),
        }
    }
}

fn default_triage_scope() -> String {
    "inbox/".to_string()
}

/// `[inbox]` section. Declarative routing rules evaluated on filesystem
/// `Created` events for indexable files (`.md` / `.txt`). Each rule matches
/// by basename regex and/or body regex (first ~4 KB) and emits one or both
/// of: a `move_to` folder, or an `add_tag` frontmatter append. First match
/// wins; non-matching files stay put. See `docs/inbox-rules.md`.
///
/// Strict-load (`Rules::compile`) validates that each rule has at least one
/// match and one action, that regexes compile, and that `move_to` is vault-
/// relative with no `..` traversal. Default is an empty rule list.
///
/// status: inbox-rules
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxConfig {
    #[serde(default)]
    pub rules: Vec<InboxRule>,
}

/// Single inbox routing rule. Both `match` and `action` are required; the
/// loader validates that `match` has at least one field set and `action`
/// has at least one field set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxRule {
    #[serde(rename = "match", default)]
    pub match_: InboxMatch,
    #[serde(default)]
    pub action: InboxAction,
}

/// Match predicate for a rule. Fields are AND-combined when both are
/// present. Both are regex strings; the loader compiles them via the
/// `regex` crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxMatch {
    /// Regex matched against the file's basename (the final path segment,
    /// extension included).
    #[serde(default)]
    pub basename: Option<String>,
    /// Regex matched against the first ~4 KB of the file body (UTF-8
    /// decoded; non-UTF-8 files are skipped).
    #[serde(default)]
    pub body: Option<String>,
}

/// Action to apply when a rule matches. At least one field must be set.
/// Both can be set; `move_to` runs first, then `add_tag` against the
/// post-move path so the tag-append edits the file at its new location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxAction {
    /// Vault-relative destination directory. The file's basename is
    /// preserved; the rule moves `<rel>` → `<move_to>/<basename>`.
    /// Capture-group rewriting is out of scope. Loader rejects values
    /// containing `..` traversal or starting with `/`.
    #[serde(default)]
    pub move_to: Option<String>,
    /// Tag to append to the file's frontmatter `tags` list. Idempotent —
    /// re-applying an already-present tag is a no-op (still writes if the
    /// frontmatter would otherwise change).
    #[serde(default)]
    pub add_tag: Option<String>,
}

/// `[editing]` section. Tunables for the layered editing model
/// (`core::editing::LayeredDoc` — the in-memory `accepted`/`working`/`pending`
/// document with pending-edit staging and per-hunk review). See
/// `docs/op-log.md` §`[editing]` config section. (Renamed from `[op-log]` in
/// K4; keys unchanged.)
///
/// status: op-log-config-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditingConfig {
    /// Retention horizon (days) for pending-edit metadata. Currently advisory:
    /// pending proposals are disposable and there is no metadata side-table to
    /// sweep (local history is plain-file snapshots in `core::snapshot`); kept
    /// as a stable config key with a sensible default.
    #[serde(default = "default_metadata_retention_days")]
    pub metadata_retention_days: u32,
    /// Retention horizon (days) for rejected pending proposals.
    #[serde(default = "default_rejected_retention_days")]
    pub rejected_retention_days: u32,
    /// When a pending edit's anchor no longer resolves (the document drifted),
    /// flip it to rejected automatically rather than surfacing it for review.
    #[serde(default = "no")]
    pub auto_reject_on_drift: bool,
    /// Default `status` for agent-authored edits (`true` = require review).
    /// Surface-specific overrides (`[mcp.tools]`, `[llm.background]`) win.
    #[serde(default = "yes")]
    pub review_required: bool,
}

impl Default for EditingConfig {
    fn default() -> Self {
        Self {
            metadata_retention_days: default_metadata_retention_days(),
            rejected_retention_days: default_rejected_retention_days(),
            auto_reject_on_drift: false,
            review_required: true,
        }
    }
}

const fn default_metadata_retention_days() -> u32 { 365 }
const fn default_rejected_retention_days() -> u32 { 14 }

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

/// `[wikilinks]` section. Vault-wide wikilink policy. See
/// `docs/wikilinks.md`.
///
/// status: wikilink-ambiguous-resolution
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikilinksConfig {
    /// What to do when a bare-name link has more than one matching
    /// note. `Unresolved` (default) renders the link as broken and
    /// surfaces a disambiguation picker. `LexFirst` resolves to the
    /// lexicographically-first matching path. `NearestFolder` picks
    /// the match with the longest shared folder prefix with the
    /// referrer. Vault-scope eligible.
    #[serde(default)]
    pub ambiguous_resolution: AmbiguousResolution,
}

/// Wire form of [`crate::wikilink::AmbiguityPolicy`]. Lives in `config`
/// so the TOML round-trip stays decoupled from the resolver crate; a
/// `From` impl below converts to the resolver enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AmbiguousResolution {
    #[default]
    Unresolved,
    LexFirst,
    NearestFolder,
}

impl From<AmbiguousResolution> for crate::wikilink::AmbiguityPolicy {
    fn from(value: AmbiguousResolution) -> Self {
        match value {
            AmbiguousResolution::Unresolved => Self::Unresolved,
            AmbiguousResolution::LexFirst => Self::LexFirst,
            AmbiguousResolution::NearestFolder => Self::NearestFolder,
        }
    }
}

/// `[clustering]` section. Vault-wide clustering-pipeline policy that
/// isn't a per-build parameter (build params live on the saved-tree
/// `method` JSON). Currently just the draft-trail proposal opt-in. See
/// `docs/clustering.md` and `docs/trails.md` §"Draft sources".
///
/// status: trail-draft-from-clustering
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusteringConfig {
    /// When `true`, the clustering pipeline emits a DRAFT trail proposal
    /// alongside its existing reorganization output whenever a cluster
    /// contains an implicit reading-order chain (per
    /// `core::cluster::detect_reading_order_chain`). Off by default — the
    /// reorganization flow is the primary clustering product; trail
    /// proposals are an opt-in extra. [trail-draft-from-clustering]
    ///
    /// status: trail-draft-from-clustering
    #[serde(default = "no")]
    pub propose_trails: bool,
    /// Default directory for newly-created cluster-tree `.md` files.
    /// Vault-relative; empty string places trees at vault root. Auto-created
    /// on first tree by `vault.write_file`. Discovery is by `hiker.kind:
    /// cluster-tree` frontmatter, so the user can move a tree anywhere after
    /// creation. Vault-scope eligible per `settings-write-back`.
    ///
    /// status: cluster-tree-visible-note
    #[serde(default = "default_new_cluster_tree_dir")]
    pub new_cluster_tree_dir: String,
}

impl Default for ClusteringConfig {
    fn default() -> Self {
        Self {
            propose_trails: no(),
            new_cluster_tree_dir: default_new_cluster_tree_dir(),
        }
    }
}

fn default_new_cluster_tree_dir() -> String {
    "cluster-trees/".to_string()
}

/// `[boards]` section. Mirrors `[trails]`: default placement for newly
/// created board-docs. See `docs/kanban.md` §"Creating a board".
///
/// status: board-default-location
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardsConfig {
    /// Default directory for newly-created board-docs. Vault-relative.
    /// Empty string places boards at vault root. Auto-created on first
    /// board. Vault-scope eligible per `settings-write-back`.
    #[serde(default = "default_new_board_dir")]
    pub new_board_dir: String,
}

impl Default for BoardsConfig {
    fn default() -> Self {
        Self {
            new_board_dir: default_new_board_dir(),
        }
    }
}

fn default_new_board_dir() -> String {
    "boards/".to_string()
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
    #[serde(default)]
    pub lease: LeaseConfig,
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            worker_preference: WorkerPreferenceCfg::default(),
            terminal_retention_secs: default_terminal_retention_secs(),
            direct_worker: DirectWorkerConfig::default(),
            lease: LeaseConfig::default(),
        }
    }
}

impl TasksConfig {
    /// Grace window the direct worker waits before becoming eligible for
    /// a newly-queued task, per `worker_preference`. `internal` = 0,
    /// `auto` = 1s, `external` = 5s.
    pub const fn direct_grace(&self) -> std::time::Duration {
        match self.worker_preference {
            WorkerPreferenceCfg::Internal => std::time::Duration::from_secs(0),
            WorkerPreferenceCfg::Auto => std::time::Duration::from_secs(1),
            WorkerPreferenceCfg::External => std::time::Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPreferenceCfg {
    #[default]
    Auto,
    Internal,
    External,
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

const fn default_terminal_retention_secs() -> u64 {
    60
}

const fn default_direct_parallelism() -> u8 {
    1
}

const fn default_lease_default_secs() -> u64 {
    60
}

const fn default_lease_max_secs() -> u64 {
    600
}

/// `[llm]` section. Configures generative LLM access for `core::llm`
/// background / fan-out features. Mirrors the shape pinned in
/// `docs/llm.md` §`core::llm` and the v3.5 stub in `docs/settings.md`.
///
/// status: llm-providers-config
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfig {
    /// Master switch. When false, `core::llm` is treated as unavailable
    /// (background / fan-out all no-op). Reserved for `llm-disable-mode`;
    /// left here so the loader doesn't reject the key once that slug lands.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub provider: LlmProviderConfig,
    #[serde(default)]
    pub limits: LlmLimitsConfig,
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
    /// When true, debounced background LLM features stage a pending layered-doc
    /// op instead of mutating frontmatter directly. Default false (off).
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

const fn default_llm_max_tokens() -> u32 {
    4096
}

const fn default_llm_timeout_secs() -> u64 {
    60
}

/// `[mcp]` section. Configures the in-process MCP server (see `docs/mcp.md`).
/// Loader lands alongside the v3 milestone — until then the section is
/// recognized so users can enable/disable the server and tune top_k caps.
///
/// status: mcp-config-section
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Default `false`: the MCP server binds a write-capable localhost listener
    /// at every vault open, so it must be a deliberate opt-in. Flip to `true`
    /// (user-scope) to let an external agent (e.g. Claude Code) reach the vault.
    #[serde(default = "no")]
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
            enabled: false,
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
    /// `apply_tag`, `remove_tag`) — when false, every write tool refuses
    /// with `1004 disabled` regardless of the per-tool flags below.
    #[serde(default = "yes")]
    pub writes_enabled: bool,
    /// If true, agents passing `scope` can fetch redacted bodies.
    /// Conservative default (false) per spec.
    #[serde(default = "no")]
    pub allow_redacted_lookup: bool,
    // status: agent-write-review-mode
    /// When true, MCP write tools (`write_note`, `edit_note`,
    /// `set_frontmatter`, `apply_tag`, `remove_tag`) stage a pending layered-doc
    /// op so the user reviews each change before it lands on disk. Default
    /// true — the in-buffer patch-review surface expects edits as pending
    /// ops; turning this off bypasses the review UI and writes straight to
    /// disk.
    #[serde(default = "yes")]
    pub review_required: bool,
    // Per-tool toggles (status: mcp-tool-toggles). Default true.
    // Reads:
    #[serde(default = "yes")] pub search_notes_enabled: bool,
    #[serde(default = "yes")] pub get_note_enabled: bool,
    #[serde(default = "yes")] pub related_notes_enabled: bool,
    /// status: mcp-tool-get-active-note
    #[serde(default = "yes")] pub get_active_note_enabled: bool,
    /// status: mcp-tool-get-open-notes
    #[serde(default = "yes")] pub get_open_notes_enabled: bool,
    /// status: mcp-tool-get-selection
    #[serde(default = "yes")] pub get_selection_enabled: bool,
    /// status: diagram-agent-check
    /// Stateless syntax-check of a diagram source (mermaid / wavedrom /
    /// latex). Read-only — no vault access.
    #[serde(default = "yes")] pub check_diagram_enabled: bool,
    /// status: query-mcp-tool
    /// The generic `query` read tool: run a saved query-doc or an inline
    /// filter over the note-metadata index (`docs/queries.md`).
    #[serde(default = "yes")] pub query_enabled: bool,
    // Writes (also gated by `writes_enabled` master flag):
    #[serde(default = "yes")] pub write_note_enabled: bool,
    #[serde(default = "yes")] pub edit_note_enabled: bool,
    #[serde(default = "yes")] pub set_frontmatter_enabled: bool,
    #[serde(default = "yes")] pub apply_tag_enabled: bool,
    #[serde(default = "yes")] pub remove_tag_enabled: bool,
    // Board tools (status: board-mcp-tools). Reads + writes (every write is
    // also gated by `writes_enabled`):
    #[serde(default = "yes")] pub boards_list_enabled: bool,
    #[serde(default = "yes")] pub board_get_enabled: bool,
    #[serde(default = "yes")] pub board_add_card_enabled: bool,
    #[serde(default = "yes")] pub board_create_enabled: bool,
    #[serde(default = "yes")] pub board_add_text_card_enabled: bool,
    #[serde(default = "yes")] pub board_move_card_enabled: bool,
    #[serde(default = "yes")] pub board_set_card_text_enabled: bool,
    #[serde(default = "yes")] pub board_remove_card_enabled: bool,
    #[serde(default = "yes")] pub board_add_column_enabled: bool,
    #[serde(default = "yes")] pub board_rename_column_enabled: bool,
    #[serde(default = "yes")] pub board_reorder_column_enabled: bool,
    #[serde(default = "yes")] pub board_delete_column_enabled: bool,
    /// status: mcp-registry-tools
    /// Family toggle for the registry-generated `create_<kind>` /
    /// `update_<kind>` tools (one gate for the whole family, not per-kind
    /// keys — this struct is a closed strict-load shape). Also gated by
    /// `writes_enabled`.
    #[serde(default = "yes")] pub kind_tools_enabled: bool,
    // Task-queue tools. Advertised to external rmcp clients whenever the
    // MCP server runs (`[mcp] enabled`); each gated by its per-tool flag
    // below and the runtime `[llm] enabled` guard.
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
            "write_note"
                | "edit_note"
                | "set_frontmatter"
                | "apply_tag"
                | "remove_tag"
                | "board_add_card"
                | "board_create"
                | "board_add_text_card"
                | "board_move_card"
                | "board_set_card_text"
                | "board_remove_card"
                | "board_add_column"
                | "board_rename_column"
                | "board_reorder_column"
                | "board_delete_column"
        );
        if is_write && !self.writes_enabled {
            return false;
        }
        match name {
            "search_notes" => self.search_notes_enabled,
            "get_note" => self.get_note_enabled,
            "related_notes" => self.related_notes_enabled,
            "get_active_note" => self.get_active_note_enabled,
            "get_open_notes" => self.get_open_notes_enabled,
            "get_selection" => self.get_selection_enabled,
            "check_diagram" => self.check_diagram_enabled,
            "query" => self.query_enabled,
            "write_note" => self.write_note_enabled,
            "edit_note" => self.edit_note_enabled,
            "set_frontmatter" => self.set_frontmatter_enabled,
            "apply_tag" => self.apply_tag_enabled,
            "remove_tag" => self.remove_tag_enabled,
            "boards_list" => self.boards_list_enabled,
            "board_get" => self.board_get_enabled,
            "board_add_card" => self.board_add_card_enabled,
            "board_create" => self.board_create_enabled,
            "board_add_text_card" => self.board_add_text_card_enabled,
            "board_move_card" => self.board_move_card_enabled,
            "board_set_card_text" => self.board_set_card_text_enabled,
            "board_remove_card" => self.board_remove_card_enabled,
            "board_add_column" => self.board_add_column_enabled,
            "board_rename_column" => self.board_rename_column_enabled,
            "board_reorder_column" => self.board_reorder_column_enabled,
            "board_delete_column" => self.board_delete_column_enabled,
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
            review_required: true,
            search_notes_enabled: true,
            get_note_enabled: true,
            related_notes_enabled: true,
            get_active_note_enabled: true,
            get_open_notes_enabled: true,
            get_selection_enabled: true,
            check_diagram_enabled: true,
            query_enabled: true,
            write_note_enabled: true,
            edit_note_enabled: true,
            set_frontmatter_enabled: true,
            apply_tag_enabled: true,
            remove_tag_enabled: true,
            boards_list_enabled: true,
            board_get_enabled: true,
            board_add_card_enabled: true,
            board_create_enabled: true,
            board_add_text_card_enabled: true,
            board_move_card_enabled: true,
            board_set_card_text_enabled: true,
            board_remove_card_enabled: true,
            board_add_column_enabled: true,
            board_rename_column_enabled: true,
            board_reorder_column_enabled: true,
            board_delete_column_enabled: true,
            kind_tools_enabled: true,
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

const fn default_mcp_max_top_k() -> u32 {
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

const fn default_min_similarity() -> f32 { 0.0 }
const fn default_semantic_top_k() -> u32 { 25 }

/// Recency-bias weighting for the semantic engine. `Off` = no recency
/// blend (default); `Mild` / `Strong` mix mtime rank into the score via
/// the same RRF k=60 shape as cross-mode fusion (weights 0.5 / 1.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecencyBias {
    #[default]
    Off,
    Mild,
    Strong,
}

impl RecencyBias {
    pub const fn weight(self) -> f32 {
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
    pub highlight_trailing_whitespace: bool,
    #[serde(default = "no")]
    pub show_chunk_boundaries: bool,
    #[serde(default = "no")]
    pub hide_frontmatter: bool,
    // status: view-intraline-diff-toggle
    #[serde(default = "no")]
    pub intraline_diff: bool,
    #[serde(default = "yes")]
    pub show_minimap: bool,
    /// When the minimap is hidden, draw a thin auto-hiding scrollbar
    /// on the right edge of the editor. `hide_scrollbar = true` opts
    /// out and leaves the editor with no visible scroll affordance.
    /// Setting this has no effect while the minimap is visible — the
    /// minimap already acts as the scroll affordance.
    #[serde(default = "no")]
    pub hide_scrollbar: bool,
    /// Multiplier applied to wheel / trackpad scroll deltas in the
    /// editor body. `1.0` is the egui default; bump for faster scroll.
    #[serde(default = "default_scroll_speed")]
    pub scroll_speed: f32,
    #[serde(default)]
    pub minimap: MinimapConfig,
    #[serde(default = "default_tab_size")]
    pub tab_size: u8,
    /// System / chrome font. Used for non-editor UI text (toolbar,
    /// sidebar, dialogs). Empty = use the egui default.
    #[serde(default)]
    pub font_system: String,
    /// Editor body font. Used for the main prose in notes. Empty = use
    /// the egui default monospace.
    #[serde(default)]
    pub font_editor: String,
    /// Code font. Used for fenced code blocks, inline code, YAML
    /// frontmatter, and any decoration that sets `monospace`. Empty =
    /// use the egui default monospace.
    #[serde(default)]
    pub font_code: String,
    /// Regex deciding what a double-click selects: the match on the clicked
    /// line that contains the cursor becomes the selection. Default `\w+`
    /// reproduces the historic Unicode-word behavior (so `foo-bar` splits at
    /// `-`). Override e.g. `"[\\w-]+"` to select hyphenated words whole, or
    /// `"\\S+"` to select runs of non-whitespace. Empty value resets to the
    /// default; an invalid regex logs once and falls back to the default.
    /// Must match `editor_view::viewport::DEFAULT_DOUBLE_CLICK_PATTERN`.
    #[serde(default = "default_double_click_pattern")]
    pub double_click_pattern: String,
    /// Regex deciding what a triple-click selects (matched against the line
    /// **with** its trailing newline). Default `.*\n?` reproduces the
    /// historic whole-line-incl-newline behavior. Override e.g. `".*"` to
    /// drop the trailing newline. Must match
    /// `editor_view::viewport::DEFAULT_TRIPLE_CLICK_PATTERN`.
    #[serde(default = "default_triple_click_pattern")]
    pub triple_click_pattern: String,
}

fn default_double_click_pattern() -> String {
    // Keep in sync with `editor_view::viewport::DEFAULT_DOUBLE_CLICK_PATTERN`.
    r"\w+".to_string()
}

fn default_triple_click_pattern() -> String {
    // Keep in sync with `editor_view::viewport::DEFAULT_TRIPLE_CLICK_PATTERN`.
    r".*\n?".to_string()
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            render_txt_as_markdown: true,
            live_preview: true,
            word_wrap: true,
            show_line_numbers: true,
            show_whitespace: false,
            highlight_trailing_whitespace: false,
            show_chunk_boundaries: false,
            hide_frontmatter: false,
            intraline_diff: false,
            show_minimap: true,
            hide_scrollbar: false,
            scroll_speed: default_scroll_speed(),
            minimap: MinimapConfig::default(),
            tab_size: 2,
            font_system: String::new(),
            font_editor: String::new(),
            font_code: String::new(),
            double_click_pattern: default_double_click_pattern(),
            triple_click_pattern: default_triple_click_pattern(),
        }
    }
}

/// Visual + behavior knobs for the structural minimap strip rendered to
/// the right of the editor body. All defaults match the unconfigured
/// look so absence of `[editor.minimap]` in TOML produces the same
/// minimap a fresh install would draw.
/// Which minimap renderer to use. `glyphs` is a literal scaled-down view of
/// the text (one cell per character); `bars` is the structural abstraction
/// (one bar per line). Both are rasterized to a single cached texture, so
/// the choice is purely visual — neither costs per-line draw work per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MinimapStyle {
    #[default]
    Glyphs,
    Bars,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimapConfig {
    /// Which renderer to use (`glyphs` or `bars`).
    #[serde(default)]
    pub style: MinimapStyle,
    /// Strip width in pixels.
    #[serde(default = "minimap_default_width")]
    pub width: u16,
    /// Left padding inside the strip before bars start.
    #[serde(default = "minimap_default_pad")]
    pub bar_padding_left: u16,
    /// Right padding inside the strip after the longest bar.
    #[serde(default = "minimap_default_pad")]
    pub bar_padding_right: u16,
    /// Corner radius applied to each bar.
    #[serde(default = "minimap_default_radius")]
    pub bar_corner_radius: u16,
    /// Minimum bar width in pixels (very short lines get this).
    #[serde(default = "minimap_default_min_bar_w")]
    pub min_bar_width: u16,
    /// Vertical sliver subtracted from each bar so neighbours don't blur.
    /// Stored as tenths of a pixel; e.g. `5` = 0.5px.
    #[serde(default = "minimap_default_bar_gap_tenths")]
    pub bar_gap_tenths: u16,
    /// Apply per-kind colors. When false the minimap renders every bar
    /// in `color_plain` for a monochrome look.
    #[serde(default = "yes")]
    pub colored: bool,
    /// Draw the faint horizontal rule above heading lines.
    #[serde(default = "yes")]
    pub show_section_rules: bool,
    /// Draw the viewport thumb. Off = the minimap is a pure overview
    /// with no scroll affordance.
    #[serde(default = "yes")]
    pub show_viewport: bool,
    /// Draw the 1px gutter rule on the left edge of the strip.
    #[serde(default = "yes")]
    pub show_left_edge: bool,

    /// Colors. Stored as `#RRGGBB` or `#RRGGBBAA`. Defaults match the
    /// built-in palette tuned for both light and dark themes.
    #[serde(default = "minimap_default_color_heading")]
    pub color_heading: String,
    #[serde(default = "minimap_default_color_code")]
    pub color_code: String,
    #[serde(default = "minimap_default_color_emphasis")]
    pub color_emphasis: String,
    #[serde(default = "minimap_default_color_quote")]
    pub color_quote: String,
    #[serde(default = "minimap_default_color_plain")]
    pub color_plain: String,
    #[serde(default = "minimap_default_color_background")]
    pub color_background: String,
    #[serde(default = "minimap_default_color_section_rule")]
    pub color_section_rule: String,
    #[serde(default = "minimap_default_color_viewport")]
    pub color_viewport: String,
    #[serde(default = "minimap_default_color_viewport_hover")]
    pub color_viewport_hover: String,
}

const fn minimap_default_width() -> u16 { 72 }
const fn minimap_default_pad() -> u16 { 5 }
const fn minimap_default_radius() -> u16 { 1 }
const fn minimap_default_min_bar_w() -> u16 { 2 }
const fn minimap_default_bar_gap_tenths() -> u16 { 5 }
fn minimap_default_color_heading() -> String { "#3c7adc".into() }
fn minimap_default_color_code() -> String { "#3c95c5".into() }
fn minimap_default_color_emphasis() -> String { "#c98a3c".into() }
fn minimap_default_color_quote() -> String { "#7a85a5".into() }
fn minimap_default_color_plain() -> String { "#6a6f80".into() }
fn minimap_default_color_background() -> String { "#00000014".into() }
fn minimap_default_color_section_rule() -> String { "#0000001c".into() }
fn minimap_default_color_viewport() -> String { "#3c64b41c".into() }
fn minimap_default_color_viewport_hover() -> String { "#3c64b432".into() }

impl Default for MinimapConfig {
    fn default() -> Self {
        Self {
            style: MinimapStyle::default(),
            width: minimap_default_width(),
            bar_padding_left: minimap_default_pad(),
            bar_padding_right: minimap_default_pad(),
            bar_corner_radius: minimap_default_radius(),
            min_bar_width: minimap_default_min_bar_w(),
            bar_gap_tenths: minimap_default_bar_gap_tenths(),
            colored: true,
            show_section_rules: true,
            show_viewport: true,
            show_left_edge: true,
            color_heading: minimap_default_color_heading(),
            color_code: minimap_default_color_code(),
            color_emphasis: minimap_default_color_emphasis(),
            color_quote: minimap_default_color_quote(),
            color_plain: minimap_default_color_plain(),
            color_background: minimap_default_color_background(),
            color_section_rule: minimap_default_color_section_rule(),
            color_viewport: minimap_default_color_viewport(),
            color_viewport_hover: minimap_default_color_viewport_hover(),
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
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            batch_size: default_batch_size(),
            ignored_paths: Vec::new(),
        }
    }
}

fn default_model() -> String {
    "bge-small-en-v1.5".to_string()
}

const fn default_batch_size() -> u16 {
    64
}

const fn default_tab_size() -> u8 {
    2
}

const fn default_scroll_speed() -> f32 {
    2.5
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
            active_trail: None,
            chat_input_height: 0,
            tree: TreeConfig::default(),
        }
    }
}

const fn default_chat_height() -> f32 {
    0.30
}

const fn default_sidebar_width() -> u32 {
    280
}

const fn default_discovery_width() -> u32 {
    320
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    #[serde(default)]
    pub sort_by: TreeSortBy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeSortBy {
    #[default]
    NameAsc,
    NameDesc,
    MtimeDesc,
    MtimeAsc,
}

impl TreeSortBy {
    /// Canonical wire string for each variant — the single source of truth
    /// for the `TreeSortBy` ↔ string mapping. Matches the serde
    /// `rename_all = "snake_case"` encoding exactly so UI code (sort menus,
    /// settings dropdown) and serde agree. Route every hand-written match
    /// through this rather than re-spelling the strings.
    pub const fn as_str(self) -> &'static str {
        match self {
            TreeSortBy::NameAsc => "name_asc",
            TreeSortBy::NameDesc => "name_desc",
            TreeSortBy::MtimeDesc => "mtime_desc",
            TreeSortBy::MtimeAsc => "mtime_asc",
        }
    }
}

/// `[render]` section. Vault-wide policy for the editor's rendered-widget
/// layer (LaTeX math, Mermaid / WaveDrom diagrams, tables). See
/// `docs/editor-widgets.md` §"Caching and invalidation" and
/// `docs/settings.md` §`[render]`.
///
/// status: render-cache-diagrams-toggle
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    /// When `true` (default), rasterized diagram widgets are persisted to
    /// `<vault>/.hiker/diagram-cache/` keyed by the render's `content_hash`,
    /// so reopening a note skips the resvg blit on a cache hit. The in-memory
    /// `CachedDeco` / texture caches are unaffected; off skips only the disk
    /// layer. Live-applied. status: widget-render-disk-cache
    #[serde(default = "yes")]
    pub cache_diagrams: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self { cache_diagrams: true }
    }
}
