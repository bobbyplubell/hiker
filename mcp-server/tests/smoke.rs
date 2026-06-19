//! End-to-end coverage for the MCP server: each branch of the spec'd
//! tool surface, the error-code translation table, the config gates
//! (`max_top_k`, `writes_enabled`, `audit.log_full_input`), and the
//! discovery-file lifecycle.
//!
//! All tests share `boot()` to stand up a fresh vault + server + client.
//! The server's `tokio::net::TcpListener` binds an ephemeral port so the
//! tests can run in parallel without colliding.

use std::sync::{Arc, Mutex};

use hiker_core::chunker::Chunk;
use hiker_core::config::sections::{McpAuditConfig, McpConfig, McpToolsConfig};
use hiker_core::embed::{Error, Embedder};
use hiker_core::indexer::{start as start_indexer, Handle};
use hiker_core::store::dto::NoteUpsert;
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;
use hiker_mcp::ui_context::{ActiveBuffer, OpenBufferTab, Shared, Snapshot};
use hiker_mcp::{start, McpDeps, McpServerHandle};
use tempfile::TempDir;

struct ZeroEmbedder;
impl Embedder for ZeroEmbedder {
    fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, Error> {
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
    idx: Handle,
    read_store: Arc<Mutex<Store>>,
    vault: Vault,
    layered: std::sync::Arc<hiker_core::editing::LayeredDoc>,
    ui_context: Shared,
}

async fn boot(config: McpConfig) -> Booted {
    // The MCP server defaults to `enabled = false` (opt-in localhost listener).
    // These tests exercise the *running* server, so force it on regardless of
    // what the caller passed.
    let config = McpConfig { enabled: true, ..config };
    let td = TempDir::new().unwrap();
    let vault = Vault::open(td.path()).unwrap();
    let store = Store::open(td.path()).unwrap();
    let read_store = Arc::new(Mutex::new(Store::open(td.path()).unwrap()));
    let watcher = Arc::new(Watcher::start(td.path()).unwrap());
    let idx = start_indexer(vault.clone(), store, || {
        Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
    });
    let audit = Arc::new(hiker_core::audit::AgentLog::new(
        td.path().join(".hiker").join("agent-log"),
        config.audit.log_full_input,
    ));
    let tasks = std::sync::Arc::new(hiker_core::tasks::queue::Queue::new(
        hiker_core::config::sections::TasksConfig::default(),
    ));
    let mcp_tools = std::sync::Arc::new(std::sync::RwLock::new(config.tools.clone()));
    let layered = std::sync::Arc::new(hiker_core::editing::LayeredDoc::open(td.path()).unwrap());
    let ui_context = hiker_mcp::ui_context::shared_empty();
    let deps = McpDeps {
        vault: vault.clone(),
        vault_root: td.path().to_path_buf(),
        read_store: read_store.clone(),
        jobs: idx.job_sender(),
        watcher,
        embedder_provider: idx.embedder_provider(),
        config,
        tools: mcp_tools,
        audit,
        tasks,
        tasks_config: hiker_core::config::sections::TasksConfig::default(),
        boards_config: hiker_core::config::sections::BoardsConfig::default(),
        llm_enabled: false,
        layered: Some(layered.clone()),
        ui_context: ui_context.clone(),
        // status: mcp-registry-tools — the built-in PM set, compiled the
        // same way the app host compiles `[kinds]` at vault open, so the
        // generated create_<kind>/update_<kind> pairs exist in every boot.
        kinds: Arc::new(hiker_core::kinds::builtin_registry()),
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

    Booted { td, handle, client, url, idx, read_store, vault, layered, ui_context }
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
        "boards_list",
        "board_get",
        "board_add_card",
        "board_create",
        "board_add_text_card",
        "board_move_card",
        "board_set_card_text",
        "board_remove_card",
        "board_add_column",
        "board_rename_column",
        "board_reorder_column",
        "board_delete_column",
        "check_diagram",
        "query",
    ] {
        assert!(tools.contains(&expected.to_string()), "missing {expected} in {tools:?}");
    }
    shutdown(b).await;
}

/// `check_diagram` is a stateless syntax check: a valid mermaid block returns
/// `ok:true` with no diagnostics; a broken one returns `ok:false` with at least
/// one error diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_diagram_reports_syntax() {
    let b = boot(McpConfig::default()).await;

    let ok = call_tool(
        &b,
        "check_diagram",
        serde_json::json!({"lang": "mermaid", "src": "graph TD\nA-->B"}),
    )
    .await;
    let ok = structured(&ok);
    assert_eq!(ok["ok"], serde_json::json!(true), "valid diagram is ok");
    assert_eq!(ok["diagnostics"].as_array().unwrap().len(), 0);

    let bad = call_tool(
        &b,
        "check_diagram",
        serde_json::json!({"lang": "mermaid", "src": "pie title\n: notanumber"}),
    )
    .await;
    let bad = structured(&bad);
    assert_eq!(bad["ok"], serde_json::json!(false), "broken diagram is not ok");
    let diags = bad["diagnostics"].as_array().unwrap();
    assert!(!diags.is_empty(), "broken diagram yields diagnostics");
    assert_eq!(diags[0]["severity"], serde_json::json!("error"));

    shutdown(b).await;
}

/// Direct-mode (review off) round-trip across the new board write tools:
/// create a board, add a text card, move it to another column, add+rename a
/// column, then assert the result via `board_get`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_write_tools_round_trip_direct() {
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;

    // Create the board (default Todo/Doing/Done under boards/).
    let created = call_tool(&b, "board_create", serde_json::json!({ "name": "plan" })).await;
    let cs = structured(&created);
    assert_eq!(cs["status"], "written");
    let rel = cs["rel_path"].as_str().unwrap().to_string();
    assert_eq!(rel, "boards/plan.md");

    // Add a freeform text card to Todo; capture its card_id.
    let added = call_tool(&b, "board_add_text_card", serde_json::json!({
        "board_rel_path": rel,
        "column": "Todo",
        "text": "ship boards",
    })).await;
    let as_ = structured(&added);
    assert_eq!(as_["status"], "written");
    let card_id = as_["card_id"].as_str().unwrap().to_string();

    // Move it to Doing.
    let moved = call_tool(&b, "board_move_card", serde_json::json!({
        "board_rel_path": rel,
        "card_id": card_id,
        "to_column": "Doing",
    })).await;
    assert_eq!(structured(&moved)["status"], "written");

    // Add a column, then rename it.
    let added_col = call_tool(&b, "board_add_column", serde_json::json!({
        "board_rel_path": rel,
        "name": "Backlog",
    })).await;
    assert_eq!(structured(&added_col)["status"], "written");
    let renamed = call_tool(&b, "board_rename_column", serde_json::json!({
        "board_rel_path": rel,
        "old_name": "Backlog",
        "new_name": "Icebox",
    })).await;
    assert_eq!(structured(&renamed)["status"], "written");

    // board_get reflects all of it (direct writes hit disk).
    let got = call_tool(&b, "board_get", serde_json::json!({ "rel_path": rel })).await;
    let g = structured(&got);
    let columns = g["columns"].as_array().expect("columns");
    let names: Vec<&str> = columns.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Todo", "Doing", "Done", "Icebox"]);
    // The card moved out of Todo into Doing.
    let todo = columns.iter().find(|c| c["name"] == "Todo").unwrap();
    assert!(todo["cards"].as_array().unwrap().is_empty(), "card left Todo");
    let doing = columns.iter().find(|c| c["name"] == "Doing").unwrap();
    let doing_cards = doing["cards"].as_array().unwrap();
    assert_eq!(doing_cards.len(), 1);
    assert_eq!(doing_cards[0]["text"], "ship boards");

    shutdown(b).await;
}

/// `board_create` commits directly EVEN under `review_required` — the layered-doc
/// staging path for a new file would seed the document by writing an empty
/// `.md` to disk, leaving a phantom board-doc in the vault. Creates are
/// structural; the safer fallback is direct-commit with the user deleting on
/// reject. The subsequent board *edits* still stage as pending ops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_create_commits_directly_even_in_review_mode() {
    let b = boot(McpConfig::default()).await; // review_required defaults true
    let created = call_tool(&b, "board_create", serde_json::json!({ "name": "draft" })).await;
    let cs = structured(&created);
    assert_eq!(cs["status"], "written");
    let rel = cs["rel_path"].as_str().unwrap();
    // The new board-doc DID hit disk — this is the documented fallback.
    assert!(b.td.path().join(rel).exists(), "create commits directly");

    // A subsequent edit on that board still STAGES in review mode (does not
    // hit disk) — only create is direct-only.
    let added = call_tool(&b, "board_add_column", serde_json::json!({
        "board_rel_path": rel,
        "name": "Review",
    })).await;
    let as_ = structured(&added);
    assert_eq!(as_["status"], "staged", "edits still stage in review mode: {added}");
    assert!(as_["proposal_id"].is_string());
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_get_returns_columns_and_cards() {
    let b = boot(McpConfig::default()).await;
    // Hand-write a board-doc; `board_get` reads the file directly and
    // resolves each card against the index (an unindexed card resolves as an
    // orphan, which is fine for this read-path assertion).
    std::fs::create_dir_all(b.td.path().join("boards")).unwrap();
    let board_src = "---\nhiker:\n  kind: board\n  id: 01BOARD\n  columns:\n    - name: Todo\n      cards:\n        - { id: 01CARD, path: \"note.md\" }\n    - name: Done\n      cards: []\n---\n# Roadmap\n\nframing\n";
    std::fs::write(b.td.path().join("boards/roadmap.md"), board_src).unwrap();
    // Hand-written file bypassed the watcher/indexer path that would normally
    // seed the layered doc; do the bootstrap walk so `board_get`'s
    // `doc_id_for_path` lookup resolves. status: op-log-doc-id-bootstrap
    hiker_core::ops::op_writes::bootstrap(&b.vault, &b.layered).unwrap();

    let resp = call_tool(&b, "board_get", serde_json::json!({
        "rel_path": "boards/roadmap.md",
    })).await;
    let s = structured(&resp);
    assert_eq!(s["rel_path"], "boards/roadmap.md");
    // board_id comes from the layered-doc path→doc_id mapping (`store-path-is-identity`),
    // not the frontmatter `hiker.id`. The seeded id is a fresh ULID; just
    // assert presence.
    let expected_id = b.layered.doc_id_for_path("boards/roadmap.md").unwrap().unwrap();
    assert_eq!(s["board_id"], expected_id);
    let columns = s["columns"].as_array().expect("columns array");
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0]["name"], "Todo");
    assert_eq!(columns[0]["cards"].as_array().unwrap().len(), 1);
    assert_eq!(columns[1]["name"], "Done");
    assert!(columns[1]["cards"].as_array().unwrap().is_empty());
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
        let chunk = Chunk {
            index: 0,
            byte_start: 0,
            byte_end: "indexed snippet text".len(),
            text: "indexed snippet text".to_string(),
            heading_path: Some("Setup > Database".into()),
        };
        s.upsert_note(&NoteUpsert {
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
async fn get_note_reflects_agents_own_staged_write() {
    // op-log-agent-replica: in review mode a staged write never touches disk,
    // but the authoring agent's get_note must read its own pending replica
    // (accepted + the agent's queued ops) so a follow-up read sees the edit.
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: true, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    std::fs::write(b.td.path().join("a.md"), "original body\n").unwrap();

    let resp = call_tool(&b, "write_note", serde_json::json!({
        "rel_path": "a.md",
        "content": "agent rewrote this\n",
    })).await;
    assert_eq!(structured(&resp)["status"], "staged", "resp: {resp}");

    // Disk still holds the pre-edit content (nothing accepted).
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "original body\n");

    // The agent's own read reflects its staged edit.
    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "a.md",
        "detail": "full",
    })).await;
    assert_eq!(
        structured(&resp)["content"], "agent rewrote this\n",
        "agent get_note should return its own pending replica: {resp}",
    );
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_note_reflects_agents_own_staged_edit() {
    // Same agent-replica guarantee for an anchored edit_note staged for review.
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: true, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    std::fs::write(b.td.path().join("a.md"), "hello foo world\n").unwrap();

    let resp = call_tool(&b, "edit_note", serde_json::json!({
        "rel_path": "a.md",
        "edits": [{"old_str": "foo", "new_str": "FOO"}],
    })).await;
    assert_eq!(structured(&resp)["status"], "staged", "resp: {resp}");

    // Disk unchanged until accept.
    let on_disk = std::fs::read_to_string(b.td.path().join("a.md")).unwrap();
    assert_eq!(on_disk, "hello foo world\n");

    // The agent reads its own staged replacement.
    let resp = call_tool(&b, "get_note", serde_json::json!({
        "rel_path": "a.md",
        "detail": "full",
    })).await;
    assert_eq!(
        structured(&resp)["content"], "hello FOO world\n",
        "agent get_note should reflect its staged edit: {resp}",
    );
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
    // Direct-write path: expected_hash drift only fires when the tool is
    // applying directly. Under review_required (the default) the proposal
    // stages and accept-time does the hash check instead.
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
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
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
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
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
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
    let ids = s["proposal_ids"].as_array().expect("proposal_ids array");
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
            s.upsert_note(&NoteUpsert {
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

// ---------- UI-context tool smoke tests ----------
// status: mcp-tool-get-active-note
// status: mcp-tool-get-open-notes
// status: mcp-tool-get-selection

fn set_ui_context(b: &Booted, snap: Snapshot) {
    *b.ui_context.write().unwrap() = snap;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_context_tools_appear_in_tool_listing() {
    let b = boot(McpConfig::default()).await;
    let resp = rpc(&b, "tools/list", serde_json::json!({})).await;
    let tools: Vec<String> = resp["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    for expected in ["get_active_note", "get_open_notes", "get_selection"] {
        assert!(
            tools.contains(&expected.to_string()),
            "missing {expected} in {tools:?}"
        );
    }
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_active_note_returns_null_when_nothing_active() {
    let b = boot(McpConfig::default()).await;
    let resp = call_tool(&b, "get_active_note", serde_json::json!({})).await;
    let s = structured(&resp);
    assert!(s["path"].is_null(), "expected path:null, got {s}");
    let opens = call_tool(&b, "get_open_notes", serde_json::json!({})).await;
    let arr = structured(&opens).as_array().expect("array");
    assert!(arr.is_empty(), "expected empty open_notes, got {arr:?}");
    let sel = call_tool(&b, "get_selection", serde_json::json!({})).await;
    assert!(structured(&sel)["path"].is_null());
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_active_note_buffer_with_no_selection() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("note.md"), "hello world").unwrap();
    set_ui_context(
        &b,
        Snapshot {
            open_tabs: vec![OpenBufferTab { path: "note.md".into(), active: true }],
            active_buffer: Some(ActiveBuffer {
                path: "note.md".into(),
                cursor_byte: 5,
                selection: None,
            }),
        },
    );
    let resp = call_tool(&b, "get_active_note", serde_json::json!({})).await;
    let s = structured(&resp);
    assert_eq!(s["path"], serde_json::json!("note.md"));
    assert_eq!(s["cursor_byte"], serde_json::json!(5));
    assert!(s["selection"].is_null(), "expected null selection, got {s}");
    let opens = structured(&call_tool(&b, "get_open_notes", serde_json::json!({})).await)
        .clone();
    assert_eq!(
        opens,
        serde_json::json!([{"path": "note.md", "active": true}])
    );
    let sel = structured(&call_tool(&b, "get_selection", serde_json::json!({})).await)
        .clone();
    assert!(sel["path"].is_null(), "empty selection → null path, got {sel}");
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_selection_returns_text_and_range_for_non_empty_selection() {
    let b = boot(McpConfig::default()).await;
    std::fs::write(b.td.path().join("note.md"), "hello world").unwrap();
    set_ui_context(
        &b,
        Snapshot {
            open_tabs: vec![OpenBufferTab { path: "note.md".into(), active: true }],
            active_buffer: Some(ActiveBuffer {
                path: "note.md".into(),
                cursor_byte: 5,
                selection: Some((0, 5)),
            }),
        },
    );
    let active = structured(&call_tool(&b, "get_active_note", serde_json::json!({})).await)
        .clone();
    assert_eq!(active["path"], serde_json::json!("note.md"));
    assert_eq!(active["selection"]["start_byte"], serde_json::json!(0));
    assert_eq!(active["selection"]["end_byte"], serde_json::json!(5));
    let sel = structured(&call_tool(&b, "get_selection", serde_json::json!({})).await)
        .clone();
    assert_eq!(sel["path"], serde_json::json!("note.md"));
    assert_eq!(sel["start_byte"], serde_json::json!(0));
    assert_eq!(sel["end_byte"], serde_json::json!(5));
    assert_eq!(sel["text"], serde_json::json!("hello"));
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_active_note_returns_null_for_app_page_tab() {
    let b = boot(McpConfig::default()).await;
    set_ui_context(
        &b,
        Snapshot { open_tabs: vec![], active_buffer: None },
    );
    let resp = call_tool(&b, "get_active_note", serde_json::json!({})).await;
    assert!(structured(&resp)["path"].is_null());
    let sel = call_tool(&b, "get_selection", serde_json::json!({})).await;
    assert!(structured(&sel)["path"].is_null());
    shutdown(b).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ui_context_tools_respect_per_tool_disable() {
    let mut cfg = McpConfig::default();
    cfg.tools.get_active_note_enabled = false;
    let b = boot(cfg).await;
    let resp = call_tool(&b, "get_active_note", serde_json::json!({})).await;
    let err = &resp["result"]["isError"];
    let code = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let msg = resp
        .get("error")
        .and_then(|e| e["message"].as_str())
        .map(str::to_string)
        .unwrap_or(code);
    assert!(
        msg.contains("disabled") || err.as_bool().unwrap_or(false),
        "expected disabled error, got {resp}"
    );
    shutdown(b).await;
}

// ---------- query tool (query-mcp-tool) ----------

/// Seed the read store the way the indexer would: note rows plus flattened
/// `note_meta` entries, keyed on the note's path.
fn seed_meta_note(b: &Booted, path: &str, meta: &[(&str, &str, Option<f64>)]) {
    let mut s = b.read_store.lock().unwrap();
    s.upsert_note(&NoteUpsert {
        path,
        content_hash: "h",
        mtime: 1,
        size: 1,
        indexed_at: 1,
        embedder_version: "zero-test",
        chunks: vec![],
    })
    .unwrap();
    let entries: Vec<hiker_core::store::dto::MetaEntry> = meta
        .iter()
        .map(|(k, v, n)| hiker_core::store::dto::MetaEntry {
            key: (*k).to_string(),
            value: (*v).to_string(),
            num: *n,
        })
        .collect();
    s.replace_note_metadata(path, &entries).unwrap();
}

/// The `query` tool runs both shapes — an inline filter and a saved
/// query-doc — through the same compile path and returns
/// `{ rows: [{path, title, mtime, fields}] }`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_tool_runs_inline_filter_and_saved_doc() {
    let b = boot(McpConfig::default()).await;
    seed_meta_note(&b, "notes/lang.md", &[("tags", "rust", None), ("status", "active", None)]);
    seed_meta_note(&b, "notes/other.md", &[("tags", "go", None)]);

    // Inline filter.
    let resp = call_tool(
        &b,
        "query",
        serde_json::json!({"filter": {"tags": "rust"}, "select": ["status"]}),
    )
    .await;
    let rows = structured(&resp)["rows"].as_array().expect("rows array").clone();
    assert_eq!(rows.len(), 1, "resp: {resp}");
    assert_eq!(rows[0]["path"], "notes/lang.md");
    assert_eq!(rows[0]["title"], "lang");
    assert_eq!(rows[0]["fields"]["status"], "active");

    // Saved query-doc (read from disk by path; indexed enumeration isn't
    // needed to *run* one).
    std::fs::create_dir_all(b.td.path().join("queries")).unwrap();
    std::fs::write(
        b.td.path().join("queries/rust.md"),
        "---\nhiker:\n  kind: query\n  query:\n    tags: rust\n---\nprose\n",
    )
    .unwrap();
    let resp = call_tool(&b, "query", serde_json::json!({"query_doc": "queries/rust.md"})).await;
    let rows = structured(&resp)["rows"].as_array().expect("rows array").clone();
    assert_eq!(rows.len(), 1, "resp: {resp}");
    assert_eq!(rows[0]["path"], "notes/lang.md");

    shutdown(b).await;
}

/// Error model: both/neither of `query_doc`/`filter` and an out-of-grammar
/// filter are `invalid_params`; a missing or non-query `query_doc` path is
/// `1002 note_not_found`; the per-tool toggle answers `1004 disabled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_tool_error_codes() {
    let b = boot(McpConfig::default()).await;

    let resp = call_tool(&b, "query", serde_json::json!({})).await;
    assert_eq!(resp["error"]["code"], -32602, "neither arg: {resp}");
    let resp = call_tool(
        &b,
        "query",
        serde_json::json!({"query_doc": "q.md", "filter": {"kind": "story"}}),
    )
    .await;
    assert_eq!(resp["error"]["code"], -32602, "both args: {resp}");

    // A clause outside the closed grammar is a loud invalid_params.
    let resp = call_tool(&b, "query", serde_json::json!({"filter": {"nope": 1}})).await;
    assert_eq!(resp["error"]["code"], -32602, "unknown clause: {resp}");

    // Missing doc, and an existing note that is not a query-doc -> 1002.
    let resp = call_tool(&b, "query", serde_json::json!({"query_doc": "missing.md"})).await;
    assert_eq!(resp["error"]["code"], 1002, "missing doc: {resp}");
    std::fs::write(b.td.path().join("plain.md"), "# not a query\n").unwrap();
    let resp = call_tool(&b, "query", serde_json::json!({"query_doc": "plain.md"})).await;
    assert_eq!(resp["error"]["code"], 1002, "non-query doc: {resp}");

    shutdown(b).await;
}

/// Per-tool toggle: `[mcp.tools] query_enabled = false` refuses with
/// `1004 disabled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_tool_respects_per_tool_disable() {
    let mut cfg = McpConfig::default();
    cfg.tools.query_enabled = false;
    let b = boot(cfg).await;
    let resp = call_tool(&b, "query", serde_json::json!({"filter": {"kind": "story"}})).await;
    assert_eq!(resp["error"]["code"], 1004, "resp: {resp}");
    shutdown(b).await;
}

// ---------- registry-generated kind tools (mcp-registry-tools) ----------

/// Every registered kind advertises its generated create/update pair
/// through the same `tools/list` as the static surface, with typed param
/// schemas derived from the field schema (number -> number, date -> ISO
/// string; create requires the kind's required fields).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_tools_advertise_with_typed_param_schemas() {
    let b = boot(McpConfig::default()).await;
    let resp = rpc(&b, "tools/list", serde_json::json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "create_story", "update_story",
        "create_task", "update_task",
        "create_epic", "update_epic",
        "create_sprint", "update_sprint",
        "create_plan", "update_plan",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    let schema_of = |name: &str| -> &serde_json::Value {
        &tools.iter().find(|t| t["name"] == name).unwrap()["inputSchema"]
    };
    // create_sprint: rel_path + body + the kind's typed fields; required
    // carries rel_path plus the schema's `required = true` fields.
    let create_sprint = schema_of("create_sprint");
    let props = &create_sprint["properties"];
    assert_eq!(props["rel_path"]["type"], "string");
    assert_eq!(props["start"]["type"], "string");
    assert_eq!(props["start"]["format"], "date");
    assert_eq!(props["goal"]["type"], "string");
    let required: Vec<&str> = create_sprint["required"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(required, vec!["rel_path", "start", "end"]);
    // create_story: numbers are JSON numbers; nothing required beyond path.
    let create_story = schema_of("create_story");
    assert_eq!(create_story["properties"]["priority"]["type"], "number");
    assert_eq!(
        create_story["required"].as_array().unwrap().len(),
        1,
        "story has no required fields: {create_story}"
    );
    // update_* has no body param and only rel_path required.
    let update_sprint = schema_of("update_sprint");
    assert!(update_sprint["properties"]["body"].is_null());
    let required: Vec<&str> = update_sprint["required"]
        .as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(required, vec!["rel_path"]);
    shutdown(b).await;
}

/// Direct mode: create writes a typed-frontmatter note to disk; update
/// merges fields through the frontmatter path; a wrong-kind target refuses
/// to retype.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_tools_create_update_round_trip_direct() {
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;

    let resp = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "work/login.md",
        "priority": 2,
        "due": "2026-07-01",
        "body": "# Login story\n",
    })).await;
    let s = structured(&resp);
    assert!(s["status"].is_null(), "direct write, not staged: {resp}");
    let on_disk = std::fs::read_to_string(b.td.path().join("work/login.md")).unwrap();
    assert!(on_disk.contains("kind: story"), "{on_disk}");
    assert!(on_disk.contains("priority: 2"), "{on_disk}");
    assert!(on_disk.contains("2026-07-01"), "{on_disk}");
    assert!(on_disk.ends_with("# Login story\n"), "{on_disk}");

    // Creating over an existing path refuses (use update_<kind>).
    let resp = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "work/login.md",
    })).await;
    assert_eq!(resp["error"]["code"], -32602, "resp: {resp}");

    // Update merges typed fields into the existing frontmatter.
    let resp = call_tool(&b, "update_story", serde_json::json!({
        "rel_path": "work/login.md",
        "priority": 5,
    })).await;
    assert!(structured(&resp)["status"].is_null(), "resp: {resp}");
    let on_disk = std::fs::read_to_string(b.td.path().join("work/login.md")).unwrap();
    assert!(on_disk.contains("priority: 5"), "{on_disk}");
    assert!(on_disk.contains("2026-07-01"), "merge keeps siblings: {on_disk}");

    // A target of another kind errors rather than silently retyping.
    let resp = call_tool(&b, "update_sprint", serde_json::json!({
        "rel_path": "work/login.md",
        "goal": "nope",
    })).await;
    assert_eq!(resp["error"]["code"], -32602, "resp: {resp}");
    assert!(
        resp["error"]["message"].as_str().unwrap().contains("retype"),
        "resp: {resp}"
    );
    // Updating a missing note is 1002.
    let resp = call_tool(&b, "update_story", serde_json::json!({
        "rel_path": "work/missing.md",
        "priority": 1,
    })).await;
    assert_eq!(resp["error"]["code"], 1002, "resp: {resp}");
    shutdown(b).await;
}

/// Review mode: both halves of the pair stage a layered-doc pending proposal —
/// the same staged path every other write tool rides — and disk stays
/// unchanged until the user accepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_tools_stage_when_review_required() {
    let b = boot(McpConfig::default()).await; // review_required defaults true

    let resp = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "work/staged.md",
        "priority": 1,
    })).await;
    let s = structured(&resp);
    assert_eq!(s["status"], "staged", "resp: {resp}");
    assert!(s["proposal_id"].as_str().is_some(), "resp: {resp}");
    // The layered-doc whole-file-create staging path seeds an EMPTY .md on disk
    // (the same `LayeredDoc::create_document` behavior `board_create` documents);
    // the staged typed content itself must not land until the user accepts.
    let on_disk =
        std::fs::read_to_string(b.td.path().join("work/staged.md")).unwrap_or_default();
    assert!(on_disk.is_empty(), "staged content must not reach disk: {on_disk}");

    // Update against an existing note stages too.
    std::fs::write(
        b.td.path().join("existing.md"),
        "---\nhiker:\n  kind: story\n---\nbody\n",
    )
    .unwrap();
    let resp = call_tool(&b, "update_story", serde_json::json!({
        "rel_path": "existing.md",
        "priority": 4,
    })).await;
    assert_eq!(structured(&resp)["status"], "staged", "resp: {resp}");
    let on_disk = std::fs::read_to_string(b.td.path().join("existing.md")).unwrap();
    assert!(!on_disk.contains("priority"), "disk must be unchanged: {on_disk}");
    shutdown(b).await;
}

/// The boundary is strict even though on-disk validation is lenient:
/// malformed dates, non-number numbers, and unknown fields are
/// `invalid_params`; the family toggle (and the writes master gate)
/// answers `1004 disabled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kind_tools_strict_boundary_and_family_toggle() {
    let cfg = McpConfig {
        tools: McpToolsConfig { review_required: false, ..McpToolsConfig::default() },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;

    let bad_date = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "a.md", "due": "someday",
    })).await;
    assert_eq!(bad_date["error"]["code"], -32602, "resp: {bad_date}");
    let bad_number = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "a.md", "priority": "high",
    })).await;
    assert_eq!(bad_number["error"]["code"], -32602, "resp: {bad_number}");
    let unknown_field = call_tool(&b, "create_story", serde_json::json!({
        "rel_path": "a.md", "points": 3,
    })).await;
    assert_eq!(unknown_field["error"]["code"], -32602, "resp: {unknown_field}");
    let missing_required = call_tool(&b, "create_sprint", serde_json::json!({
        "rel_path": "s.md", "start": "2026-07-01",
    })).await;
    assert_eq!(missing_required["error"]["code"], -32602, "resp: {missing_required}");
    // Nothing reached disk through any of the rejected calls.
    assert!(!b.td.path().join("a.md").exists());
    shutdown(b).await;

    // Family toggle off -> 1004 for the whole generated family.
    let cfg = McpConfig {
        tools: McpToolsConfig {
            review_required: false,
            kind_tools_enabled: false,
            ..McpToolsConfig::default()
        },
        ..McpConfig::default()
    };
    let b = boot(cfg).await;
    let resp = call_tool(&b, "create_story", serde_json::json!({ "rel_path": "a.md" })).await;
    assert_eq!(resp["error"]["code"], 1004, "resp: {resp}");
    // The master writes gate covers the family too.
    let cfg = McpConfig {
        tools: McpToolsConfig {
            review_required: false,
            writes_enabled: false,
            ..McpToolsConfig::default()
        },
        ..McpConfig::default()
    };
    let b2 = boot(cfg).await;
    let resp = call_tool(&b2, "update_story", serde_json::json!({
        "rel_path": "a.md", "priority": 1,
    })).await;
    assert_eq!(resp["error"]["code"], 1004, "resp: {resp}");
    shutdown(b).await;
    shutdown(b2).await;
}
