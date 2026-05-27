//! Host tests over hand-written WebAssembly-text fixtures (compiled in-process
//! by `wat`, so no external wasm toolchain is needed). They exercise the fat-
//! pipe ABI round-trip, the permission gate (granted vs fail-closed vs unknown),
//! and a real `notes.query` against the structured metadata index.

use std::sync::{Arc, Mutex};

use super::dispatch::{HostApi, HostInvoker, StoreHostApi};
use super::runtime::{WasmEngine, WasmiEngine};
use super::manifest::{blake3_pin, verify_pin, Permissions};
use super::vdom::Node;
use crate::store::dto::{MetaEntry, NoteUpsert};
use crate::test_helpers::test_store;

/// Escape a byte string for a WAT data segment (`"..."`).
fn wat_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A fixture whose `init` returns the packed pointer to a static JSON blob —
/// the minimal "plugin renders a VDOM" shape, no host calls.
fn wat_returning(json: &str) -> Vec<u8> {
    let packed: i64 = (16_i64 << 32) | json.len() as i64;
    let src = format!(
        r#"(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (data (i32.const 16) "{data}")
  (func (export "plugin_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $p))
  (func (export "init") (param i32 i32) (result i64) (i64.const {packed}))
  (func (export "on_ui_event") (param i32 i32) (result i64) (i64.const {packed}))
)"#,
        data = wat_escape(json),
    );
    wat::parse_str(src).expect("valid wat")
}

/// A fixture whose `init` calls `host_call(name, args)` and returns its result
/// verbatim — used to drive the gate and a real host dispatch.
fn wat_calling(name: &str, args: &str) -> Vec<u8> {
    let name_off = 16;
    let args_off = name_off + name.len();
    let src = format!(
        r#"(module
  (import "hiker" "host_call" (func $host_call (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (data (i32.const {name_off}) "{name_data}")
  (data (i32.const {args_off}) "{args_data}")
  (func (export "plugin_alloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $p))
  (func (export "init") (param i32 i32) (result i64)
    (call $host_call (i32.const {name_off}) (i32.const {name_len})
                     (i32.const {args_off}) (i32.const {args_len})))
)"#,
        name_data = wat_escape(name),
        args_data = wat_escape(args),
        name_len = name.len(),
        args_len = args.len(),
    );
    wat::parse_str(src).expect("valid wat")
}

/// Records the last call and returns a fixed reply — lets the gate be tested
/// without a real store.
struct StubApi {
    last: Mutex<Option<(String, String)>>,
    reply: String,
}

impl HostApi for StubApi {
    fn call(&self, name: &str, args_json: &str) -> Result<String, String> {
        *self.last.lock().unwrap() = Some((name.to_string(), args_json.to_string()));
        Ok(self.reply.clone())
    }
}

fn invoker(perms: &[&str], api: Arc<dyn HostApi>) -> HostInvoker {
    HostInvoker {
        plugin_id: "test".to_string(),
        permissions: Permissions::from_strings(perms.iter().copied()),
        api,
    }
}

#[test]
fn init_returns_vdom_tree() {
    let json = r#"{"type":"vstack","children":[{"type":"text","value":"hi","style":"heading"}]}"#;
    let wasm = wat_returning(json);
    let api: Arc<dyn HostApi> = Arc::new(StubApi {
        last: Mutex::new(None),
        reply: String::new(),
    });
    let mut inst = WasmiEngine.instantiate(&wasm, invoker(&[], api)).unwrap();
    let out = inst.call_json("init", "").unwrap();
    let node: Node = serde_json::from_str(&out).unwrap();
    match node {
        Node::Vstack { children } => {
            assert_eq!(children.len(), 1);
            assert!(matches!(children[0], Node::Text { ref value, .. } if value == "hi"));
        }
        other => panic!("expected vstack, got {other:?}"),
    }
}

#[test]
fn host_call_succeeds_when_permission_granted() {
    let stub = Arc::new(StubApi {
        last: Mutex::new(None),
        reply: r#"{"ok":true}"#.to_string(),
    });
    let api: Arc<dyn HostApi> = stub.clone();
    let wasm = wat_calling("notes.query", r#"{"filters":[]}"#);
    let mut inst = WasmiEngine
        .instantiate(&wasm, invoker(&["read:notes"], api))
        .unwrap();
    let out = inst.call_json("init", "").unwrap();
    assert_eq!(out, r#"{"ok":true}"#);
    // The gate forwarded the real call to the api.
    let last = stub.last.lock().unwrap().clone().unwrap();
    assert_eq!(last.0, "notes.query");
}

#[test]
fn host_call_fails_closed_when_permission_missing() {
    let stub = Arc::new(StubApi {
        last: Mutex::new(None),
        reply: r#"{"ok":true}"#.to_string(),
    });
    let api: Arc<dyn HostApi> = stub.clone();
    let wasm = wat_calling("notes.query", "{}");
    // No `read:notes` grant.
    let mut inst = WasmiEngine.instantiate(&wasm, invoker(&[], api)).unwrap();
    let out = inst.call_json("init", "").unwrap();
    assert!(out.contains("permission denied"), "got: {out}");
    assert!(out.contains("read:notes"), "got: {out}");
    // The api was never reached — the gate refused before dispatch.
    assert!(stub.last.lock().unwrap().is_none());
}

#[test]
fn unknown_host_call_is_refused() {
    let api: Arc<dyn HostApi> = Arc::new(StubApi {
        last: Mutex::new(None),
        reply: String::new(),
    });
    let wasm = wat_calling("bogus.thing", "{}");
    let mut inst = WasmiEngine.instantiate(&wasm, invoker(&["read:notes"], api)).unwrap();
    let out = inst.call_json("init", "").unwrap();
    assert!(out.contains("unknown host call"), "got: {out}");
}

#[test]
fn log_call_is_always_free() {
    let stub = Arc::new(StubApi {
        last: Mutex::new(None),
        reply: r#"{"logged":true}"#.to_string(),
    });
    let api: Arc<dyn HostApi> = stub.clone();
    let wasm = wat_calling("log.info", r#""hello from plugin""#);
    // No permissions at all — logging is ungated, so the gate still forwards
    // it to the api (rather than refusing as it would a read:notes call).
    let mut inst = WasmiEngine.instantiate(&wasm, invoker(&[], api)).unwrap();
    let out = inst.call_json("init", "").unwrap();
    assert_eq!(out, r#"{"logged":true}"#);
    assert_eq!(stub.last.lock().unwrap().clone().unwrap().0, "log.info");
}

#[test]
fn notes_query_runs_against_the_structured_index() {
    // Real chain: plugin -> host_call -> gate -> StoreHostApi -> query_notes.
    let (_dir, mut store) = test_store();
    let id = crate::store::dto::new_id();
    store
        .upsert_note(&NoteUpsert {
            id: &id,
            path: "projects/a.md",
            content_hash: "h",
            mtime: 1,
            size: 1,
            indexed_at: 0,
            embedder_version: "t",
            chunks: Vec::new(),
        })
        .unwrap();
    store
        .replace_note_metadata(
            &id,
            &[MetaEntry {
                key: "status".to_string(),
                value: "active".to_string(),
                num: None,
            }],
        )
        .unwrap();

    let api: Arc<dyn HostApi> = Arc::new(StoreHostApi {
        store: Arc::new(Mutex::new(store)),
    });
    let args = r#"{"filters":[{"kind":"equals","key":"status","value":"active"}]}"#;
    let wasm = wat_calling("notes.query", args);
    let mut inst = WasmiEngine
        .instantiate(&wasm, invoker(&["read:notes"], api))
        .unwrap();
    let out = inst.call_json("init", "").unwrap();
    assert!(out.contains("projects/a.md"), "query result: {out}");
}

#[test]
fn hash_pin_round_trips_and_detects_change() {
    let bytes = b"plugin bytes";
    let pin = blake3_pin(bytes);
    assert!(pin.starts_with("blake3:"));
    assert!(verify_pin(bytes, &pin).is_ok());
    assert!(verify_pin(b"tampered", &pin).is_err());
}
