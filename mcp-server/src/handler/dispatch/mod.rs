//! The per-tool `guard_tool` / `guard_tasks` gates and the per-domain
//! workhorse implementations of every tool. The rmcp `#[tool]` methods in
//! `router.rs` (and the generated kind-tool routes) call these operation
//! methods directly — MCP is the sole agent surface, so there is no longer
//! an in-process name-routing dispatcher.
//!
//! Per-domain implementations live in sibling files as additional
//! `impl App` blocks, split by tool family so each stays under the
//! file-length budget: `notes` (search / read / save / edit / tag /
//! frontmatter), `boards` (board CRUD + card/column ops), `tasks`
//! (queue lease/result/failure/heartbeat/list).

mod boards;
mod diagram;
/// status: mcp-registry-tools
pub(in crate::handler) mod kinds;
mod notes;
mod queries;
mod tasks;
mod ui_context;
use rmcp::model::{ErrorCode, ErrorData};

use super::App;
use crate::handler::params::hiker_err;

impl App {
    /// status: mcp-tool-toggles
    /// Per-tool gate. Reads the shared `tools` config live so a flip in
    /// the settings UI applies to the next dispatch without a vault
    /// restart. Combines the per-tool flag with the `writes_enabled`
    /// master gate (write tools) — see `McpToolsConfig::tool_allowed`.
    pub(super) fn guard_tool(&self, name: &str) -> Result<(), ErrorData> {
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

    /// status: task-queue-respects-llm-disable
    /// The queue's only purpose is LLM work; with `[llm] enabled = false`
    /// the direct worker is force-off and the rmcp-side `task_*` tools
    /// answer `1004 disabled` so external agents see a coherent disabled
    /// state instead of being able to checkout work no in-process worker
    /// will ever drain.
    pub(super) fn guard_tasks(&self) -> Result<(), ErrorData> {
        if self.state.llm_enabled {
            Ok(())
        } else {
            Err(hiker_err(ErrorCode(1004), "tasks disabled (llm features off)"))
        }
    }
}
