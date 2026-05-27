//! The host-call surface plugins reach the app through, and the permission
//! gate in front of it. Every call a plugin makes is `host_call(name,
//! args_json) -> result_json`; this module decides whether the plugin's
//! granted permissions allow `name` (failing closed otherwise) and, if so,
//! dispatches to the concrete [`HostApi`]. `notes.query` wraps the structured
//! metadata index (`store-note-query`). See `plugins.md`.
//
// status: plugin-host-call, plugin-permissions

use std::sync::{Arc, Mutex};

use super::manifest::Permissions;
use crate::store::dto::NoteQuery;
use crate::store::Store;

/// What permission a host-call name requires.
enum PermCheck {
    /// Always allowed — no data/network reach (e.g. logging).
    Free,
    /// Requires this exact permission string.
    Requires(&'static str),
    /// Not a host call the host knows — refused.
    Unknown,
}

/// Map a host-call name to its required permission. The single source of truth
/// for the gate; unknown names fail closed.
const fn required_permission(name: &str) -> PermCheck {
    match name.as_bytes() {
        b"log.info" | b"log.warn" | b"log.error" => PermCheck::Free,
        b"notes.query" | b"notes.list" => PermCheck::Requires("read:notes"),
        b"metadata.get" => PermCheck::Requires("read:metadata"),
        _ => PermCheck::Unknown,
    }
}

/// The concrete host services a plugin can invoke, *after* the permission gate
/// has approved the call. A trait so the data backend is swappable and tests
/// can stub it without a real store.
pub trait HostApi: Send + Sync {
    /// Perform an already-permission-checked call. `Ok(json)` / `Err(message)`
    /// — the error message is surfaced to the plugin as a JSON error string.
    fn call(&self, name: &str, args_json: &str) -> Result<String, String>;
}

/// Per-instance host entry point: the plugin's granted permissions plus the
/// shared [`HostApi`]. Stored as the wasm `Store`'s data; the engine's
/// `host_call` import forwards every call here.
pub struct HostInvoker {
    pub plugin_id: String,
    pub permissions: Permissions,
    pub api: Arc<dyn HostApi>,
}

impl HostInvoker {
    /// The gate. Resolve the required permission, fail closed on
    /// missing-grant or unknown-call, else dispatch to the [`HostApi`].
    /// Returns `Ok(json)` on success and `Err(message)` on refusal or error;
    /// both are delivered to the plugin (refusals are not silent).
    pub fn invoke(&self, name: &str, args_json: &str) -> Result<String, String> {
        match required_permission(name) {
            PermCheck::Unknown => Err(format!("unknown host call: {name}")),
            PermCheck::Requires(perm) if !self.permissions.grants(perm) => {
                Err(format!("permission denied: {name} requires `{perm}`"))
            }
            PermCheck::Free | PermCheck::Requires(_) => self.api.call(name, args_json),
        }
    }
}

/// The production [`HostApi`]: logging + reads over the index store. Holds the
/// shared read store so `notes.query` runs the same structured query the rest
/// of the app uses (`store-note-query`).
pub struct StoreHostApi {
    pub store: Arc<Mutex<Store>>,
}

impl HostApi for StoreHostApi {
    fn call(&self, name: &str, args_json: &str) -> Result<String, String> {
        match name {
            "log.info" => {
                tracing::info!(target: "plugin", "{}", log_message(args_json));
                Ok("null".to_string())
            }
            "log.warn" => {
                tracing::warn!(target: "plugin", "{}", log_message(args_json));
                Ok("null".to_string())
            }
            "log.error" => {
                tracing::error!(target: "plugin", "{}", log_message(args_json));
                Ok("null".to_string())
            }
            "notes.query" => {
                let query: NoteQuery = serde_json::from_str(args_json)
                    .map_err(|e| format!("notes.query: bad args: {e}"))?;
                let store = self.store.lock().map_err(|_| "store poisoned".to_string())?;
                let rows = store
                    .query_notes(&query)
                    .map_err(|e| format!("notes.query: {e}"))?;
                serde_json::to_string(&rows).map_err(|e| e.to_string())
            }
            "notes.list" => {
                let store = self.store.lock().map_err(|_| "store poisoned".to_string())?;
                let paths = store
                    .all_note_paths()
                    .map_err(|e| format!("notes.list: {e}"))?;
                serde_json::to_string(&paths).map_err(|e| e.to_string())
            }
            other => Err(format!("unimplemented host call: {other}")),
        }
    }
}

/// Pull a human message out of a `log.*` arg payload. Accepts either a bare
/// JSON string or an object with a `msg` field; falls back to the raw text.
fn log_message(args_json: &str) -> String {
    if let Ok(serde_json::Value::String(s)) = serde_json::from_str(args_json) {
        return s;
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(args_json)
        && let Some(serde_json::Value::String(s)) = map.get("msg")
    {
        return s.clone();
    }
    args_json.to_string()
}
