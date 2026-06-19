//! LSP lifecycle on top of the blocking [`LspClient`]: `initialize` → `initialized` → ready-wait.
//!
//! **Ready-wait is the key risk.** rust-analyzer answers `workspace/symbol` only AFTER it has run
//! `cargo metadata` + built proc-macros + indexed the crate graph — seconds to minutes on a cold
//! project. The robust readiness signal is therefore *empirical*: poll `workspace/symbol` for a
//! caller-supplied probe query until it returns a non-empty result, draining `$/progress` between
//! polls, with a wall-clock timeout. A `$/progress` `end` token is used as an early nudge but the
//! poll-until-nonempty is the primary, trustworthy gate.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::protocol::ServerCapabilities;
use crate::transport::LspClient;

/// Default ready-wait budget: RA's first cold index (cargo metadata + proc-macro build) is slow.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// `initialize` the server with `rootUri = file://{root}`, advertising the client capabilities the
/// adapter relies on, send `initialized`, and return the parsed [`ServerCapabilities`].
pub fn initialize(client: &mut LspClient, root: &Path) -> io::Result<ServerCapabilities> {
    let root_uri = format!("file://{}", root.canonicalize().unwrap_or_else(|_| root.to_path_buf()).display());
    let params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "capabilities": {
            "textDocument": {
                "callHierarchy": { "dynamicRegistration": false },
                "references": { "dynamicRegistration": false },
                "definition": { "dynamicRegistration": false },
                "implementation": { "dynamicRegistration": false },
                "hover": { "dynamicRegistration": false },
                "documentSymbol": { "dynamicRegistration": false }
            },
            "workspace": {
                "symbol": { "dynamicRegistration": false },
                "workspaceFolders": true
            },
            "window": { "workDoneProgress": true }
        },
        "workspaceFolders": [ { "uri": root_uri, "name": "root" } ]
    });
    let result = client.request("initialize", params)?;
    let caps = result
        .get("capabilities")
        .map(ServerCapabilities::from_initialize)
        .unwrap_or_default();
    client.notify("initialized", json!({}))?;
    Ok(caps)
}

/// Poll `workspace/symbol` for `probe` until it returns a non-empty result, draining `$/progress`
/// in between, up to `timeout`. Returns `Ok(())` when ready, or a timeout error. The early-exit on
/// a `$/progress` `end` token only tightens the poll cadence; the non-empty result is the real gate.
pub fn wait_until_ready(client: &mut LspClient, probe: &str, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(250);
    loop {
        // The request itself drains `$/progress` notifications + answers server→client requests
        // while it blocks for the response (see `LspClient::read_until`); RA answers
        // `workspace/symbol` even mid-index, returning empty until the crate graph is built.
        let hits = client.request("workspace/symbol", json!({ "query": probe }))?;
        if hits.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("rust-analyzer not ready after {:?} (probe {probe:?} still empty)", timeout),
            ));
        }
        // A progress `end` means an indexing phase just finished — retry promptly.
        if client.take_progress_end() {
            backoff = Duration::from_millis(100);
        }
        std::thread::sleep(backoff.min(Duration::from_secs(2)));
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
}

/// Convenience: query `workspace/symbol` and return the raw array (helper used by the adapter).
pub fn workspace_symbol(client: &mut LspClient, query: &str) -> io::Result<Value> {
    client.request("workspace/symbol", json!({ "query": query }))
}
