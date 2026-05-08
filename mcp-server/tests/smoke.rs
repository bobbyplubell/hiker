//! End-to-end smoke: spin up the MCP server against a fresh vault, hit it
//! with a JSON-RPC initialize + tools/list, confirm we see hiker's tools.

use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::config::McpConfig;
use hiker_core::embed::{EmbedError, Embedder};
use hiker_core::indexer::{start_indexer, IndexerHandle};
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use hiker_mcp::{start, McpDeps};
use tempfile::TempDir;

struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(batch.iter().map(|_| vec![0.0; 384]).collect())
    }
    fn version(&self) -> &str {
        "zero-test"
    }
    fn dim(&self) -> usize {
        384
    }
}

fn fresh_session() -> (TempDir, Vault, Arc<Mutex<Store>>, Arc<Watcher>, Arc<Changes>, IndexerHandle) {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let read_store = Arc::new(Mutex::new(Store::open(td.path()).unwrap()));
    let watcher = Arc::new(Watcher::start(td.path()).unwrap());
    let changes = Arc::new(Changes::open(td.path()).unwrap());
    let idx = start_indexer(vault.clone(), store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    });
    (td, vault, read_store, watcher, changes, idx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_lists_expected_tools() {
    let (td, vault, read_store, watcher, changes, idx) = fresh_session();
    let deps = McpDeps {
        vault,
        vault_root: td.path().to_path_buf(),
        read_store,
        jobs: idx.job_sender(),
        watcher,
        changes,
        embedder_provider: idx.embedder_provider(),
        config: McpConfig::default(),
    };
    let handle = start(deps).await.expect("start mcp");

    let url = handle.url();
    let client = reqwest::Client::new();

    // 1. initialize
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0.0.0"},
        },
    });
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(init_body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    // 2. tools/list
    let list_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {},
    });
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(list_body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value =
        serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    let tools = body["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    for expected in [
        "search_notes",
        "get_note",
        "related_notes",
        "write_note",
        "set_frontmatter",
        "apply_tag",
        "remove_tag",
    ] {
        assert!(
            tools.iter().any(|t| t == expected),
            "missing tool {expected} in {tools:?}",
        );
    }

    // Discovery file written.
    let discovery = td.path().join(".hiker/mcp.json");
    assert!(discovery.exists());
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&discovery).unwrap()).unwrap();
    assert!(parsed["url"].as_str().unwrap().starts_with("http://127.0.0.1:"));

    // Shutdown removes the discovery file.
    handle.shutdown().await;
    assert!(
        !discovery.exists(),
        "discovery file should be removed on shutdown",
    );

    idx.shutdown().await;
}
