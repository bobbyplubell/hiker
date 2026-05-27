//! Notes Query — the dataview-style example plugin. Renders a sidebar panel
//! with a query input and a live results table over the vault's structured
//! metadata index. It requests only `read:notes` + `read:metadata` + a sidebar
//! panel — no write, no network — so the grant is legible and safe.
//!
//! Everything here is plain Rust; the host boundary is JSON over a fat pipe.
//! The pure logic (`build_query_args`, `build_vdom`, `event_value`) is unit-
//! tested on the host target; the `#[cfg(target_arch = "wasm32")]` shim is the
//! thin ABI glue (`plugin_alloc`, `init`, `on_ui_event`, the `host_call`
//! import). Build the artifact with:
//!
//! ```text
//! cargo build --release --target wasm32-unknown-unknown -p hiker-plugin-notes-query
//! ```

use serde_json::{json, Value};

/// Parse the query box into a `notes.query` request. Tokens are `key:value`
/// (`tag:project`, `status:active`); `tag:` targets the `tags` list. Results
/// are newest-first, capped, and project the non-tag keys as table columns.
pub fn build_query_args(input: &str) -> String {
    let (filters, select) = parse_input(input);
    json!({
        "filters": filters,
        "order": { "by": "mtime", "dir": "desc" },
        "limit": 50,
        "select": select,
    })
    .to_string()
}

/// Split the input into `notes.query` filters and the list of (non-tag) keys to
/// surface as columns.
fn parse_input(input: &str) -> (Vec<Value>, Vec<String>) {
    let mut filters = Vec::new();
    let mut select = Vec::new();
    for token in input.split_whitespace() {
        let Some((raw_key, value)) = token.split_once(':') else {
            continue;
        };
        if raw_key.is_empty() || value.is_empty() {
            continue;
        }
        let key = if raw_key == "tag" { "tags" } else { raw_key };
        filters.push(json!({ "kind": "equals", "key": key, "value": value }));
        if key != "tags" && !select.iter().any(|k| k == key) {
            select.push(key.to_string());
        }
    }
    (filters, select)
}

/// Build the panel VDOM from the query input and the `notes.query` response
/// (a JSON array of `NoteQueryRow`). A query input on top, a results table
/// below — note titles as clickable `note_link`s, one column per selected key.
pub fn build_vdom(input: &str, rows_json: &str) -> String {
    let rows: Value = serde_json::from_str(rows_json).unwrap_or_else(|_| json!([]));
    let (_filters, select) = parse_input(input);

    let mut columns = vec![Value::from("Note")];
    columns.extend(select.iter().map(|k| Value::from(k.as_str())));

    let mut table_rows = Vec::new();
    if let Some(arr) = rows.as_array() {
        for row in arr {
            table_rows.push(build_row(row, &select));
        }
    }
    let count = table_rows.len();

    json!({
        "type": "vstack",
        "children": [
            { "type": "text", "value": "Notes Query", "style": "heading" },
            { "type": "text_input", "id": "q", "value": input,
              "placeholder": "tag:project status:active" },
            { "type": "text", "value": format!("{count} result(s)"), "style": "muted" },
            { "type": "list", "columns": columns, "rows": table_rows },
        ],
    })
    .to_string()
}

/// One table row: a `note_link` cell for the title, then a `text` cell per
/// selected key pulled from the row's `fields` map.
fn build_row(row: &Value, select: &[String]) -> Value {
    let id = row.get("noteId").and_then(Value::as_str).unwrap_or_default();
    let title = row.get("title").and_then(Value::as_str).unwrap_or(id);
    let mut cells = vec![json!({ "type": "note_link", "id": id, "label": title })];
    let fields = row.get("fields");
    for key in select {
        let value = fields
            .and_then(|f| f.get(key))
            .and_then(Value::as_str)
            .unwrap_or("");
        cells.push(json!({ "type": "text", "value": value }));
    }
    json!({ "id": id, "cells": cells })
}

/// Pull the changed value out of an `on_ui_event` payload (the query string for
/// our single text input).
pub fn event_value(event_json: &str) -> String {
    serde_json::from_str::<Value>(event_json)
        .ok()
        .and_then(|v| v.get("payload").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

// --- wasm ABI shim --------------------------------------------------------
// Only compiled for the wasm target; on the host these symbols don't exist, so
// `cargo test` builds just the pure logic above.
#[cfg(target_arch = "wasm32")]
mod abi {
    use super::{build_query_args, build_vdom, event_value};

    #[link(wasm_import_module = "hiker")]
    extern "C" {
        fn host_call(name_ptr: i32, name_len: i32, args_ptr: i32, args_len: i32) -> i64;
    }

    /// Host-writable allocation. Leaks (one short-lived buffer per call); a
    /// production plugin would track + free. Returns a pointer the host fills.
    #[no_mangle]
    pub extern "C" fn plugin_alloc(len: i32) -> i32 {
        let mut buf = Vec::<u8>::with_capacity(len.max(0) as usize);
        let ptr = buf.as_mut_ptr();
        core::mem::forget(buf);
        ptr as i32
    }

    /// Read a `(ptr, len)` slice of our own memory as a String.
    fn read(ptr: i32, len: i32) -> String {
        let slice = unsafe { core::slice::from_raw_parts(ptr as *const u8, len.max(0) as usize) };
        String::from_utf8_lossy(slice).into_owned()
    }

    /// Leak a String into memory and return its packed `ptr<<32 | len` for the
    /// host to read.
    fn pack(s: String) -> i64 {
        let bytes = s.into_bytes();
        let len = bytes.len() as i64;
        let ptr = bytes.as_ptr() as i64;
        core::mem::forget(bytes);
        (ptr << 32) | len
    }

    /// Invoke the host and read back its JSON result.
    fn call(name: &str, args: &str) -> String {
        let packed = unsafe {
            host_call(
                name.as_ptr() as i32,
                name.len() as i32,
                args.as_ptr() as i32,
                args.len() as i32,
            )
        };
        read(((packed >> 32) & 0xffff_ffff) as i32, (packed & 0xffff_ffff) as i32)
    }

    fn render(input: &str) -> i64 {
        let rows = call("notes.query", &build_query_args(input));
        pack(build_vdom(input, &rows))
    }

    #[no_mangle]
    pub extern "C" fn init(_ptr: i32, _len: i32) -> i64 {
        render("")
    }

    #[no_mangle]
    pub extern "C" fn on_ui_event(ptr: i32, len: i32) -> i64 {
        let input = event_value(&read(ptr, len));
        render(&input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_args_builds_filters_and_select() {
        let args = build_query_args("tag:project status:active");
        let v: Value = serde_json::from_str(&args).unwrap();
        let filters = v["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert!(filters
            .iter()
            .any(|f| f["key"] == "tags" && f["value"] == "project"));
        assert!(filters
            .iter()
            .any(|f| f["key"] == "status" && f["value"] == "active"));
        assert_eq!(v["order"]["by"], "mtime");
        assert_eq!(v["order"]["dir"], "desc");
        // `tag` is not a display column; `status` is.
        let select: Vec<&str> = v["select"].as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect();
        assert_eq!(select, vec!["status"]);
    }

    #[test]
    fn empty_query_has_no_filters() {
        let v: Value = serde_json::from_str(&build_query_args("")).unwrap();
        assert!(v["filters"].as_array().unwrap().is_empty());
    }

    #[test]
    fn vdom_renders_rows_as_table() {
        let rows = r#"[{"noteId":"01H","path":"projects/a.md","title":"Roadmap",
                        "mtime":1,"fields":{"status":"active"}}]"#;
        let vdom: Value = serde_json::from_str(&build_vdom("status:active", rows)).unwrap();
        assert_eq!(vdom["type"], "vstack");
        let children = vdom["children"].as_array().unwrap();
        // heading, input, count, list
        assert_eq!(children.len(), 4);
        let list = children.last().unwrap();
        assert_eq!(list["type"], "list");
        assert_eq!(list["columns"], json!(["Note", "status"]));
        let row0 = &list["rows"][0];
        assert_eq!(row0["cells"][0]["type"], "note_link");
        assert_eq!(row0["cells"][0]["label"], "Roadmap");
        assert_eq!(row0["cells"][1]["value"], "active");
    }

    #[test]
    fn event_value_extracts_payload() {
        let ev = r#"{"element_id":"q","kind":"input","payload":"tag:x"}"#;
        assert_eq!(event_value(ev), "tag:x");
    }
}
