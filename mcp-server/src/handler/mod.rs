//! rmcp `ServerHandler` for hiker. Defines the tool surface, parameter
//! shapes, and the boundary that translates `HikerError` into JSON-RPC
//! errors per `mcp.md`'s error model.

use std::sync::{Arc, Mutex};

use hiker_core::config::sections::McpConfig;
use hiker_core::embed::Embedder;
use hiker_core::indexer::IndexJobTx;
use hiker_core::store::Store;
use hiker_core::tasks::queue::Queue as TaskQueue;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::audit::Log;

pub mod params;
mod dispatch;
mod router;

pub(crate) use params::*;

/// Shared state lives behind an `Arc` since `StreamableHttpService` builds
/// a fresh handler per session. Cheap to clone — every field is already
/// share-shaped.
pub struct HikerState {
    pub vault: Vault,
    pub read_store: Arc<Mutex<Store>>,
    pub jobs: IndexJobTx,
    pub watcher: Arc<Watcher>,
    pub embedder_provider: Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync>,
    pub config: McpConfig,
    /// status: mcp-tool-toggles
    /// Shared, mutable per-tool config. Each tool dispatch reads this
    /// to decide whether to refuse with `1004 disabled`. The
    /// `set_setting` command swaps the contents in place so flips in
    /// the settings UI apply without a vault restart.
    pub tools: Arc<std::sync::RwLock<hiker_core::config::sections::McpToolsConfig>>,
    /// The vault's layered doc, when open. Agent write tools route their edits
    /// into the pending queue here via `core::ops::op_writes`
    /// (`op-log-ops-producer-helpers`). Review-mode writes stage a pending
    /// op the user accepts/rejects in the hiker UI.
    ///
    /// status: staging-review-pending-response
    pub layered: Option<Arc<hiker_core::editing::LayeredDoc>>,
    pub audit: Arc<Log>,
    /// Shared task queue. When `[mcp] enabled`, the `task_*` tools are
    /// advertised; the queue itself lives in the UI layer and is plumbed
    /// in here so all surfaces (external rmcp clients, direct worker) see
    /// one in-memory queue.
    pub tasks: Arc<TaskQueue>,
    /// Default lease seconds when a checkout doesn't specify, capped by
    /// `max_lease_secs`.
    pub default_lease_secs: u64,
    pub max_lease_secs: u64,
    /// `[llm] enabled` — when false, the queue is meaningless (the
    /// direct worker can't run, and the queue's only purpose is LLM
    /// work), so the `task_*` tools are guarded with `1004 disabled`
    /// per `task-queue-respects-llm-disable`. Read once at server start.
    pub llm_enabled: bool,
    /// `[boards]` config — supplies `new_board_dir` for `board_create`'s
    /// default placement.
    ///
    /// status: board-mcp-tools
    pub boards_config: hiker_core::config::sections::BoardsConfig,
    /// status: mcp-tool-get-active-note, mcp-tool-get-open-notes,
    /// mcp-tool-get-selection
    ///
    /// Live snapshot of what the user is currently looking at, populated
    /// by the host's per-frame `refresh_ui_context_snapshot`. Read-only
    /// from the MCP handler's side; the host is the sole writer.
    pub ui_context: crate::ui_context::Shared,
    /// status: mcp-registry-tools
    /// The compiled kind registry. Each registered kind generates a typed
    /// `create_<kind>` / `update_<kind>` write pair, built into the tool
    /// router at handler construction so the pair advertises (and
    /// regenerates) with the registry the session loaded.
    pub kinds: Arc<hiker_core::kinds::Registry>,
}

#[derive(Clone)]
pub struct App {
    state: Arc<HikerState>,
    // Read by the `#[tool_handler(router = ...)]` expansion below: the
    // instance router is the static `#[tool_router]` surface plus the
    // registry-generated kind tools merged in at construction.
    tool_router: ToolRouter<Self>,
}

impl App {
    pub fn new(state: Arc<HikerState>) -> Self {
        // status: mcp-registry-tools
        // Merge the registry-generated kind tools into the static router so
        // they advertise through the same `tools/list` as every sibling.
        let mut tool_router = Self::tool_router();
        for route in dispatch::kinds::kind_tool_routes(&state.kinds) {
            tool_router.add_route(route);
        }
        Self { state, tool_router }
    }
}

// `router = self.tool_router` (vs. the default `Self::tool_router()`) so
// dispatch and tools/list both read the per-instance router carrying the
// generated kind tools. status: mcp-registry-tools
#[tool_handler(router = self.tool_router)]
impl ServerHandler for App {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hiker", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Hiker MCP server. Read: search_notes, get_note, related_notes, query. \
                 Write: write_note, edit_note, set_frontmatter, apply_tag, remove_tag. \
                 Review mode: when the server is configured with review_required = true, write tools STAGE a \
                 proposal instead of writing to disk — the response carries `status: \"staged\"` and a \
                 `proposal_id`. A follow-up get_note reflects your own staged edits (you read your pending \
                 replica), but the change does not reach disk for the user until they accept the proposal in \
                 the hiker UI. A brand-new note that does not yet exist on disk still returns 1002 not_found \
                 from get_note until accepted; that is expected, the write is pending human review, not lost.",
            )
    }
}

// ---------- helpers ----------

impl App {
    pub(crate) fn title_from_rel_path(&self, rel: &str) -> String {
        let last = rel.rsplit('/').next().unwrap_or(rel);
        let stem = last.strip_suffix(".md").unwrap_or(last);
        if stem.is_empty() {
            "Untitled".into()
        } else {
            stem.to_string()
        }
    }

    pub(crate) fn head_snippet(&self, text: &str) -> String {
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
}
