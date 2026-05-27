//! In-process MCP server for hiker. See `docs/mcp.md`.
//!
//! status: mcp-server-crate
//! status: mcp-port-discovery
//! status: mcp-error-model
//! status: mcp-audit-log-mcp-calls
//! status: mcp-lifecycle-aware
//! status: mcp-dynamic-capabilities
//!
//! Spawned by the app on vault open with the same `Vault`,
//! `IndexJobTx`, read-`Store`, and `Watcher` the UI uses. The
//! server runs as one tokio task hosting an axum HTTP listener wrapping
//! rmcp's `StreamableHttpService` (per `mcp-transport-streamable-http`).
//! Handle is dropped at vault close — the cancellation token tears the
//! task down and removes the discovery file.

pub mod agent_bridge;
pub mod audit;
pub mod discovery;
pub mod handler;


use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hiker_core::audit::AgentLog;
use hiker_core::config::sections::{McpConfig, TasksConfig};
use hiker_core::embed::Embedder;
use hiker_core::indexer::IndexJobTx;
use hiker_core::store::Store;
use hiker_core::tasks::queue::Queue as TaskQueue;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::audit::Log;

/// All shared dependencies the MCP server needs from the rest of the app.
/// Constructed by the app's `open_vault_at` and consumed by `start`.
pub struct McpDeps {
    pub vault: Vault,
    pub vault_root: PathBuf,
    pub read_store: Arc<Mutex<Store>>,
    pub jobs: IndexJobTx,
    pub watcher: Arc<Watcher>,
    /// Pulls the currently-loaded embedder from the indexer's `OnceCell`.
    /// Returns `None` while the model is loading or after a load failure;
    /// search and related tools surface `1005 indexer_unavailable` in that
    /// case rather than blocking.
    pub embedder_provider: Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync>,
    pub config: McpConfig,
    /// status: mcp-tool-toggles
    /// Shared, mutable per-tool config. The `set_setting`
    /// command swaps the contents in place so the next dispatched tool
    /// call sees the new gate without a vault re-open. `host` and
    /// `port` aren't included here because they control the TCP bind
    /// and changing them mid-flight would require rebinding the
    /// listener — those stay restart-bound.
    pub tools: Arc<std::sync::RwLock<hiker_core::config::sections::McpToolsConfig>>,
    /// Shared agent-log writer (see `core::audit`). MCP tool calls
    /// record through this so all surfaces — `core::agent`, `core::llm`,
    /// `mcp-tool-call` — land in the same daily JSONL.
    pub audit: Arc<AgentLog>,
    /// Shared task queue. The `task_*` tools route through this.
    pub tasks: Arc<TaskQueue>,
    /// `[tasks]` config — drives lease defaults + the chat-agent
    /// expose toggle.
    pub tasks_config: TasksConfig,
    /// Mirror of `[llm] enabled`. The `task_*` tools guard on this; with
    /// LLM disabled, the queue's purpose is gone, so the tools answer
    /// `1004 disabled` rather than letting external agents check work
    /// out that no one will drain.
    pub llm_enabled: bool,
    /// The vault's op log, when open. Threaded into `HikerState` so the
    /// write tools queue agent edits into the pending queue
    /// (`op-log-ops-producer-helpers`). Review-mode writes stage a pending
    /// op the user accepts/rejects in the hiker UI.
    ///
    /// status: agent-write-review-mode
    pub oplog: Option<Arc<hiker_core::oplog::OpLog>>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("disabled in config")]
    Disabled,
    #[error("bind {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(String),
}

/// Live MCP server. Drop the handle (or call `shutdown`) to stop the task
/// and remove the discovery file.
pub struct McpServerHandle {
    addr: SocketAddr,
    discovery_path: PathBuf,
    cancel: CancellationToken,
    join: Option<JoinHandle<()>>,
    /// status: agent-tool-routing-via-mcp
    /// In-process handler used by the basic agent loop's `ToolDispatcher`.
    /// Shares the same `HikerState` (audit log, tool registry, error
    /// model) as the rmcp-side handlers, so the basic agent and external
    /// rmcp clients see one tool surface.
    agent_handler: Arc<handler::App>,
}

impl McpServerHandle {
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn discovery_path(&self) -> &Path {
        &self.discovery_path
    }
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }
    /// App for in-process tool dispatch (basic agent loop). Cheap to
    /// clone — wraps an `Arc<HikerState>`.
    pub fn agent_handler(&self) -> Arc<handler::App> {
        self.agent_handler.clone()
    }
    /// Stop the listener and wait briefly for the task to finish. Removing
    /// the discovery file is part of shutdown so a stale file never lingers
    /// across hiker restarts.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(join) = self.join.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), join).await;
        }
        let _ = std::fs::remove_file(&self.discovery_path);
    }
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        // Best-effort sync cleanup when the handle is just dropped without an
        // explicit `shutdown` (vault swap, panic). The cancel signal stops
        // accepting new requests; in-flight ones fall off when the task
        // exits naturally.
        self.cancel.cancel();
        let _ = std::fs::remove_file(&self.discovery_path);
    }
}

/// Bind the MCP server and start serving. Returns a handle whose drop tears
/// everything down.
pub async fn start(deps: McpDeps) -> Result<McpServerHandle, StartError> {
    if !deps.config.enabled {
        return Err(StartError::Disabled);
    }

    // status: mcp-bind-host-configurable
    // Default host is 127.0.0.1 — anything else exposes vault contents
    // to whoever can reach the listening port. The settings UI surfaces
    // a warning row when the user picks a non-loopback bind so the
    // consequences are visible at the choice site, not buried.
    let host = if deps.config.host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        deps.config.host.clone()
    };
    let bind_addr = if host.contains(':') && !host.starts_with('[') {
        // Bare IPv6 literal — bracket it for the "host:port" join.
        format!("[{host}]:{}", deps.config.port)
    } else {
        format!("{host}:{}", deps.config.port)
    };
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|source| StartError::Bind { addr: bind_addr.clone(), source })?;
    let addr = listener
        .local_addr()
        .map_err(|e| StartError::Io(e.to_string()))?;

    // Discovery file: write next to the vault so any agent that already has
    // filesystem access to the vault can find the URL.
    let discovery_path = deps.vault_root.join(&deps.config.discovery_file);
    discovery::write(
        &discovery_path,
        &format!("http://{addr}/mcp"),
        &deps.vault_root,
    )
    .map_err(|e| StartError::Io(format!("discovery file: {e}")))?;

    // MCP tool calls share the session's agent log via this thin
    // wrapper that carries the input-redaction toggle. `core::audit`
    // owns the on-disk JSONL writer.
    let audit = Arc::new(Log::new(deps.audit.clone(), deps.config.audit.log_full_input));

    let cancel = CancellationToken::new();
    let cancel_for_service = cancel.clone();

    // Build the rmcp Streamable HTTP service. Stateless + json-response keeps
    // the wire shape simple — every request gets a single JSON reply rather
    // than an SSE stream — sufficient for v3's synchronous tools (no
    // long-running ops yet per `mcp.md` "Out of scope").
    let handler_state = Arc::new(handler::HikerState {
        vault: deps.vault,
        read_store: deps.read_store,
        jobs: deps.jobs,
        watcher: deps.watcher,
        embedder_provider: deps.embedder_provider,
        config: deps.config.clone(),
        tools: deps.tools.clone(),
        oplog: deps.oplog.clone(),
        audit,
        tasks: deps.tasks,
        default_lease_secs: deps.tasks_config.lease.default_secs,
        max_lease_secs: deps.tasks_config.lease.max_secs,
        expose_tasks_to_chat_agent: deps.tasks_config.expose_to_chat_agent,
        llm_enabled: deps.llm_enabled,
    });
    let factory_state = handler_state.clone();
    let service: StreamableHttpService<handler::App, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(handler::App::new(factory_state.clone())),
            std::sync::Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true)
                .with_cancellation_token(cancel_for_service.clone()),
        );

    let router = axum::Router::new().nest_service("/mcp", service);

    let cancel_for_task = cancel.clone();
    let join = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { cancel_for_task.cancelled_owned().await })
            .await
        {
            tracing::error!(error = %e, "mcp: axum::serve exited with error");
        }
    });

    tracing::info!(
        url = %format!("http://{addr}/mcp"),
        discovery = %discovery_path.display(),
        "mcp: server bound",
    );

    let agent_handler = Arc::new(handler::App::new(handler_state));

    Ok(McpServerHandle {
        addr,
        discovery_path,
        cancel,
        join: Some(join),
        agent_handler,
    })
}
