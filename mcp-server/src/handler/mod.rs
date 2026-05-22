//! rmcp `ServerHandler` for hiker. Defines the tool surface, parameter
//! shapes, and the boundary that translates `HikerError` into JSON-RPC
//! errors per `mcp.md`'s error model.

use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::config::sections::McpConfig;
use hiker_core::embed::Embedder;
use hiker_core::indexer::IndexJobTx;
use hiker_core::staging::Staging;
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
    pub changes: Arc<Changes>,
    pub embedder_provider: Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync>,
    pub config: McpConfig,
    /// status: mcp-tool-toggles
    /// Shared, mutable per-tool config. Each tool dispatch reads this
    /// to decide whether to refuse with `1004 disabled`. The
    /// `set_setting` command swaps the contents in place so flips in
    /// the settings UI apply without a vault restart.
    pub tools: Arc<std::sync::RwLock<hiker_core::config::sections::McpToolsConfig>>,
    /// Shared staging instance for proposal-based writes (see
    /// docs/settings.md "## Staging review"). When `[mcp.tools]
    /// .review_required` is true, write tools route through
    /// `staging.propose()`.
    ///
    /// status: staging-review-pending-response
    pub staging: Arc<Staging>,
    pub audit: Arc<Log>,
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
pub struct App {
    state: Arc<HikerState>,
    // Read by the `#[tool_handler]` macro expansion via `self.tool_router`;
    // dead-code lint can't see the macro-generated reference.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl App {
    pub fn new(state: Arc<HikerState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler]
impl ServerHandler for App {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hiker", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Hiker MCP server. Read: search_notes, get_note, related_notes. \
                 Write: write_note, edit_note, set_frontmatter, apply_tag, remove_tag. \
                 Review mode: when the server is configured with review_required = true, write tools STAGE a \
                 proposal instead of writing to disk — the response carries `status: \"staged\"` and a \
                 `staging_id`, and the affected file will NOT be readable via get_note until the user accepts \
                 the proposal in the hiker UI. If a write returns staged and a follow-up get_note returns \
                 1002 not_found, that is expected; the write is pending human review, not lost.",
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
