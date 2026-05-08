//! In-process MCP server for hiker. See `docs/mcp.md`.
//!
//! status: mcp-server-crate
//! status: mcp-port-discovery
//! status: mcp-error-model
//! status: mcp-audit-log-mcp-calls
//! status: mcp-lifecycle-aware
//! status: mcp-dynamic-capabilities
//!
//! Spawned by `ui/src-tauri` on vault open with the same `Vault`,
//! `IndexJobTx`, read-`Store`, `Watcher`, and `Changes` the UI uses. The
//! server runs as one tokio task hosting an axum HTTP listener wrapping
//! rmcp's `StreamableHttpService` (per `mcp-transport-streamable-http`).
//! Handle is dropped at vault close — the cancellation token tears the
//! task down and removes the discovery file.

pub mod audit;
pub mod discovery;
pub mod handler;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::config::McpConfig;
use hiker_core::embed::Embedder;
use hiker_core::indexer::IndexJobTx;
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::audit::AuditLog;
use crate::handler::HikerHandler;

/// All shared dependencies the MCP server needs from the rest of the app.
/// Constructed by `ui/src-tauri::open_vault_at` and consumed by `start`.
pub struct McpDeps {
    pub vault: Vault,
    pub vault_root: PathBuf,
    pub read_store: Arc<Mutex<Store>>,
    pub jobs: IndexJobTx,
    pub watcher: Arc<Watcher>,
    pub changes: Arc<Changes>,
    /// Pulls the currently-loaded embedder from the indexer's `OnceCell`.
    /// Returns `None` while the model is loading or after a load failure;
    /// search and related tools surface `1005 indexer_unavailable` in that
    /// case rather than blocking.
    pub embedder_provider: Arc<dyn Fn() -> Option<Arc<dyn Embedder>> + Send + Sync>,
    pub config: McpConfig,
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
}

impl McpServerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    pub fn discovery_path(&self) -> &Path {
        &self.discovery_path
    }
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
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

    let bind_addr = format!("127.0.0.1:{}", deps.config.port);
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

    // Audit log lives at vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl. Shared
    // across this server's tools; the per-day file is opened on demand.
    let audit = Arc::new(AuditLog::new(
        deps.vault_root.join(".hiker").join("agent-log"),
        deps.config.audit.log_full_input,
    ));

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
        changes: deps.changes,
        embedder_provider: deps.embedder_provider,
        config: deps.config.clone(),
        audit,
    });
    let factory_state = handler_state.clone();
    let service: StreamableHttpService<HikerHandler, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(HikerHandler::new(factory_state.clone())),
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

    Ok(McpServerHandle {
        addr,
        discovery_path,
        cancel,
        join: Some(join),
    })
}
