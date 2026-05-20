//! End-to-end coverage for the MCP server: each branch of the spec'd
//! tool surface, the error-code translation table, the config gates
//! (`max_top_k`, `writes_enabled`, `audit.log_full_input`), and the
//! discovery-file lifecycle.
//!
//! All tests share `boot()` to stand up a fresh vault + server + client.
//! The server's `tokio::net::TcpListener` binds an ephemeral port so the
//! tests can run in parallel without colliding.

use std::sync::{Arc, Mutex};

use hiker_core::changes::Changes;
use hiker_core::chunker::Chunk;
use hiker_core::config::{McpAuditConfig, McpConfig, McpToolsConfig};
use hiker_core::embed::{EmbedError, Embedder};
use hiker_core::indexer::{start_indexer, IndexerHandle};
use hiker_core::staging::Staging;
use hiker_core::store::{new_id, NoteUpsert, Store};
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use hiker_mcp::{start, McpDeps, McpServerHandle};
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

struct Booted {
    td: TempDir,
    handle: McpServerHandle,
    client: reqwest::Client,
    url: String,
    idx: IndexerHandle,
    read_store: Arc<Mutex<Store>>,
}

async fn boot(config: McpConfig) -> Booted {
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let read_store = Arc::new(Mutex::new(Store::open(td.path()).unwrap()));
    let watcher = Arc::new(Watcher::start(td.path()).unwrap());
    let changes = Arc::new(Changes::open(td.path()).unwrap());
    let idx = start_indexer(vault.clone(), store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    });
    let audit = Arc::new(hiker_core::audit::AgentLog::new(
        td.path().join(".hiker").join("agent-log"),
        config.audit.log_full_input,
    ));
    let tasks = std::sync::Arc::new(hiker_core::tasks::Queue::new(
        hiker_core::config::TasksConfig::default(),
    ));
    let mcp_tools = std::sync::Arc::new(std::sync::RwLock::new(config.tools.clone()));
    let staging = std::sync::Arc::new(Staging::open(td.path()).unwrap());
    let deps = McpDeps {
        vault,
        vault_root: td.path().to_path_buf(),
        read_store: read_store.clone(),
        jobs: idx.job_sender(),
        watcher,
        changes,
        embedder_provider: idx.embedder_provider(),
        config,
        tools: mcp_tools,
        staging,
        audit,
        tasks,
        tasks_config: hiker_core::config::TasksConfig::default(),
        llm_enabled: false,
    };
    let handle = start(deps).await.expect("start mcp");
    let url = handle.url();
    let client = reqwest::Client::new();

    // Drive the JSON-RPC handshake once so subsequent tools/call work.
    let init = serde_json::json!({
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
        .body(init.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.text().await.unwrap();

    Booted { td, handle, client, url, idx, read_store }
}

async fn rpc(b: &Booted, method: &str, params: serde_json::Value) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 99,
        "method": method,
        "params": params,
    });
    let resp = b.client
        .post(&b.url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    serde_json::from_str(&resp.text().await.unwrap()).unwrap()
}

async fn call_tool(b: &Booted, name: &str, args: serde_json::Value) -> serde_json::Value {
    rpc(b, "tools/call", serde_json::json!({"name": name, "arguments": args})).await
}

/// Pull `result.structuredContent` from a successful tool response.
fn structured(resp: &serde_json::Value) -> &serde_json::Value {
    &resp["result"]["structuredContent"]
}

async fn shutdown(b: Booted) {
    b.handle.shutdown().await;
    b.idx.shutdown().await;
}

// ---------- tools/list + discovery lifecycle ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_lists_expected_tools() {
    let b = boot(McpConfig::default()).await;
    let resp = rpc(&b, "tools/list", serde_json::json!({})).await;
    let tools: Vec<String> = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "search_notes",
        "get_note",
        "related_notes",
        "write_note",
        "edit_note",
        "set_frontmatter",
        "apply_tag",
        "remove_tag",
    ] {
        assert!(tools.contains(&expected.to_string()), "missing {expected} in {tools:?}");
    }
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_file_written_then_removed() {
    let b = boot(McpConfig::default()).await;
    let discovery = b.td.path().join(".hiker/mcp.json");
    assert!(discovery.exists(), "discovery file missing");
    let parsed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&discovery).unwrap()).unwrap();
    assert!(parsed["url"].as_str().unwrap().starts_with("http://127.0.0.1:"));
    assert_eq!(parsed["version"].as_str().unwrap(), "1");
    assert!(parsed["started_at"].is_string());
    assert_eq!(
        parsed["vault_root"].as_str().unwrap(),
        b.td.path().to_string_lossy(),
    );
    let td = b.td.path().to_path_buf();
    let idx = b.idx;
    b.handle.shutdown().await;
    assert!(!td.join(".hiker/mcp.json").exists(), "stale discovery file");
    idx.shutdown().await;
}

// ---------- get_note ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_note_digest_returns_minimal_payload() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "# Title\n\nbody\n").unwrap();
    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "a.md",
        "detail": "digest",
    })).await;
    let s = structured(&resp);
    assert_eq!(s["detail"], "digest");
    assert_eq!(s["rel_path"], "a.md");
    assert_eq!(s["title"], "a");
    // Digest carries no content/snippet/heading.
    assert!(s.get("content").is_none());
    assert!(s.get("snippet").is_none());
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_note_snippet_uses_indexed_chunk() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "# H\n\noriginal body\n").unwrap();

    // Manually upsert a chunk so get_note's snippet branch picks the chunk
    // text rather than falling back to head-of-file. Re-uses the read_store
    // handle (fine for tests; in prod the indexer owns the writer).
    {
        let mut s = b.read_store.lock().unwrap();
        let id = new_id();
        let chunk = Chunk {
            index: 0,
            byte_start: 0,
            byte_end: "indexed snippet text".len(),
            text: "indexed snippet text".to_string(),
            heading_path: Some("Setup > Database".into()),
        };
        s.upsert_note(NoteUpsert {
            id: &id,
            path: "a.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 1,
            embedder_version: "zero-test",
            chunks: vec![(chunk, vec![0.0; 384])],
        }).unwrap();
    }

    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "a.md",
        "detail": "snippet",
    })).await;
    let s = structured(&resp);
    assert_eq!(s["detail"], "snippet");
    assert_eq!(s["snippet"], "indexed snippet text");
    assert_eq!(s["heading_path"], "Setup > Database");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_note_snippet_falls_back_to_head_when_unindexed() {
    let b = boot(McpConfig::default()).await;
    let body = "head of file content used as snippet fallback\nmore lines\n";
    std::fs::write(b.td.path().join("a.md"), body).unwrap();
    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "a.md",
        "detail": "snippet",
    })).await;
    let s = structured(&resp);
    assert_eq!(s["detail"], "snippet");
    assert!(s["snippet"].as_str().unwrap().starts_with("head of file content"));
    assert!(s["heading_path"].is_null());
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_note_missing_returns_1002() {
    let b = boot(McpConfig::default()).await;
    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "nope.md",
    })).await;
    assert_eq!(resp["error"]["code"], 1002);
    shutdown(b).await;
}

// ---------- write_note: drift + disabled ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_note_with_wrong_expected_hash_returns_1003() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "original").unwrap();
    let resp = call_tool(&b, "write_note", serde_json::json!({
        "rel_path": "a.md",
        "content": "rewritten",
        "expected_hash": "deadbeef-not-the-actual-hash",
    })).await;
    assert_eq!(resp["error"]["code"], 1003, "unexpected response: {resp}");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_tools_disabled_returns_1004() {
    let cfg = McpConfig {
        tools: McpToolsConfig {
            writes_enabled: false,
            ..McpToolsConfig::default()
        },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    let resp = call_tool(&b, "write_note", serde_json::json!({
        "rel_path": "a.md",
        "content": "x",
    })).await;
    assert_eq!(resp["error"]["code"], 1004, "unexpected response: {resp}");

    // Read tools should still work with writes disabled.
    let resp = rpc(&b, "tools/list", serde_json::json!({})).await;
    let tools: Vec<String> = resp["result"]["tools"]
        .as_array().unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(tools.contains(&"search_notes".to_string()));
    shutdown(b).await;
}

// ---------- edit_note: validation + direct + staged ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_direct_applies_all_edits_and_writes_once() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "hello foo world baz").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [
            {"old_str": "foo", "new_str": "FOO"},
            {"old_str": "baz", "new_str": "BAZ"},
        ],
    })).await;
    let s = structured(&resp);
    assert_eq!(s["status"], "written", "resp: {resp}");
    assert_eq!(s["edit_count"], 2);
    assert!(!s["content_hash"].as_str().unwrap().is_empty());
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello FOO world BAZ");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_missing_path_returns_1002() {
    let b = boot(McpConfig::default()).await;
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "nope.md",
        "edits": [{"old_str": "x", "new_str": "y"}],
    })).await;
    assert_eq!(resp["error"]["code"], 1002, "resp: {resp}");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_anchor_missing_returns_1003() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "hello world").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [{"old_str": "missing", "new_str": "x"}],
    })).await;
    assert_eq!(resp["error"]["code"], 1003, "resp: {resp}");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_non_unique_anchor_returns_invalid_params() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "foo foo").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [{"old_str": "foo", "new_str": "x"}],
    })).await;
    assert_eq!(resp["error"]["code"], -32602, "resp: {resp}");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_replace_all_handles_multiple_matches() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "foo foo bar").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [{"old_str": "foo", "new_str": "X", "replace_all": true}],
    })).await;
    assert_eq!(structured(&resp)["status"], "written", "resp: {resp}");
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "X X bar");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_overlapping_edits_return_invalid_params() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("a.md"), "abcdef").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [
            {"old_str": "abcd", "new_str": "X"},
            {"old_str": "cdef", "new_str": "Y"},
        ],
    })).await;
    assert_eq!(resp["error"]["code"], -32602, "resp: {resp}");
    // Disk untouched.
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "abcdef");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_note_stages_per_edit_when_review_required() {
    let cfg = McpConfig {
        tools: McpToolsConfig {
            review_required: true,
            ..McpToolsConfig::default()
        },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    std::fs::write(b.td.path().join("a.md"), "hello foo bar baz").unwrap();
    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [
            {"old_str": "foo", "new_str": "FOO"},
            {"old_str": "baz", "new_str": "BAZ"},
        ],
    })).await;
    let s = structured(&resp);
    assert_eq!(s["status"], "staged", "resp: {resp}");
    assert_eq!(s["edit_count"], 2);
    let ids = s["staging_ids"].as_array().expect("staging_ids array");
    assert_eq!(ids.len(), 2);
    assert!(s["batch_id"].is_string());
    // Disk unchanged until accept.
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello foo bar baz");
    shutdown(b).await;
}

// ---------- search: max_top_k clamping ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_top_k_clamped_by_max_top_k() {
    let cfg = McpConfig {
        max_top_k: 2,
        ..McpConfig::default()
    };
    let b = boot(cfg).await;

    // Seed five notes with the same FTS-matchable token so the lexical
    // backend produces > 2 hits before clamping.
    {
        let mut s = b.read_store.lock().unwrap();
        for i in 0..5 {
            let id = new_id();
            s.upsert_note(NoteUpsert {
                id: &id,
                path: &format!("note{i}.md"),
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "zero-test",
                chunks: vec![(
                    Chunk {
                        index: 0,
                        byte_start: 0,
                        byte_end: 9,
                        text: format!("hiker hit {i}"),
                        heading_path: None,
                    },
                    vec![0.0; 384],
                )],
            }).unwrap();
        }
    }

    let resp = call_tool(&b, "search_notes", serde_json::json!({
        "query": "hiker",
        "top_k": 50,
        "modes": {"semantic": false, "lexical": true},
    })).await;
    let s = structured(&resp);
    let fused = s["fused"].as_array().expect("fused array");
    assert!(
        fused.len() <= 2,
        "fused len {} exceeds max_top_k=2",
        fused.len(),
    );
    shutdown(b).await;
}

// ---------- audit log redaction ----------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_redacts_input_when_log_full_input_off() {
    let b = boot(McpConfig::default()).await;
    let _ = call_tool(&b, "search_notes", serde_json::json!({
        "query": "redactedneedlephrase",
        "modes": {"semantic": false, "lexical": true},
    })).await;
    // Give the audit append a beat to land — best-effort writer behind a sync
    // mutex, but we still cross a tokio yield in the response path.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let log_dir = b.td.path().join(".hiker/agent-log");
    let entries = std::fs::read_dir(&log_dir).expect("audit dir missing");
    let mut found = false;
    for entry in entries {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        for line in raw.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            if row["feature"] == "search_notes" {
                found = true;
                assert_eq!(row["surface"], "mcp-tool-call");
                assert_eq!(row["status"], "ok");
                // Query should be redacted to a length descriptor, not echoed.
                let input = &row["details"]["input"];
                assert!(input["query"].is_object(), "expected redaction object: {row}");
                assert_eq!(input["query"]["redacted"], true);
                assert_eq!(input["query"]["len"], "redactedneedlephrase".len());
                assert!(
                    !line.contains("redactedneedlephrase"),
                    "raw query leaked: {line}",
                );
            }
        }
    }
    assert!(found, "no search_notes audit row found");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audit_logs_full_input_when_enabled() {
    let cfg = McpConfig {
        audit: McpAuditConfig { log_full_input: true },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    let _ = call_tool(&b, "search_notes", serde_json::json!({
        "query": "verbatimqueryneedle",
        "modes": {"semantic": false, "lexical": true},
    })).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let log_dir = b.td.path().join(".hiker/agent-log");
    let mut leaked = false;
    for entry in std::fs::read_dir(&log_dir).unwrap() {
        let raw = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        if raw.contains("verbatimqueryneedle") {
            leaked = true;
            break;
        }
    }
    assert!(leaked, "log_full_input=true should record the verbatim query");
    shutdown(b).await;
}
