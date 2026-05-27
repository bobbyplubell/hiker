//! The plugin host: load capability-scoped, hash-pinned WebAssembly plugins
//! and drive their declarative UI. A plugin runs sandboxed inside a swappable
//! WASM engine (`engine`), declares its permissions in a manifest (`manifest`),
//! reaches the app only through the gated `host_call` surface (`host`), and
//! renders by returning a VDOM tree (`vdom`) the app paints natively. See
//! `docs/plugins.md`. Submodules carry the public types; this root owns the
//! `PluginHost` orchestration and holds no `pub use` re-export farm.
//
// status: plugin-host

pub mod dispatch;
pub mod error;
pub mod manifest;
pub mod runtime;
pub mod vdom;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use dispatch::{HostApi, HostInvoker};
use error::Error;
use runtime::{PluginInstance, WasmEngine, WasmiEngine};
use manifest::{verify_pin, Manifest, PinnedEntry, PluginsFile};
use vdom::Node;

/// A loaded, running plugin: its manifest, the live instance, and the latest
/// VDOM it returned (from `init` or the last `on_ui_event`).
struct LoadedPlugin {
    manifest: Manifest,
    instance: Box<dyn PluginInstance>,
    vdom: Option<Node>,
}

/// Owns the loaded plugins for a vault and drives their lifecycle. Holds the
/// shared [`HostApi`] (so every plugin's gated calls hit the same backend) and
/// the engine that runs them.
pub struct PluginHost {
    engine: Box<dyn WasmEngine>,
    api: Arc<dyn HostApi>,
    plugins: HashMap<String, LoadedPlugin>,
}

impl PluginHost {
    /// Build a host over an explicit engine — used by tests to inject a stub
    /// engine, and the seam for a future wasmtime backend.
    pub fn new(engine: Box<dyn WasmEngine>, api: Arc<dyn HostApi>) -> Self {
        Self {
            engine,
            api,
            plugins: HashMap::new(),
        }
    }

    /// Build a host over the default pure-Rust `wasmi` engine.
    pub fn with_wasmi(api: Arc<dyn HostApi>) -> Self {
        Self::new(Box::new(WasmiEngine), api)
    }

    /// Load one pinned plugin: read its manifest + wasm from `location` under
    /// the vault, verify *both* blake3 pins (a mismatch aborts — never run
    /// changed code against an old pin), parse the manifest, instantiate it
    /// gated by its declared permissions, and call `init`. The plugin's
    /// initial VDOM (if any) is captured for the first render.
    pub fn load(&mut self, vault_root: &Path, entry: &PinnedEntry) -> Result<(), Error> {
        let dir = vault_root.join(&entry.location);
        let manifest_bytes = std::fs::read(dir.join("manifest.json"))?;
        verify_pin(&manifest_bytes, &entry.manifest_hash)?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        let wasm = std::fs::read(dir.join(&manifest.entry))?;
        verify_pin(&wasm, &entry.wasm_hash)?;

        let invoker = HostInvoker {
            plugin_id: manifest.id.clone(),
            permissions: manifest.permissions.clone(),
            api: self.api.clone(),
        };
        let mut instance = self.engine.instantiate(&wasm, invoker)?;
        let init = instance.call_json("init", "")?;
        let vdom = parse_vdom(&init)?;
        self.plugins.insert(
            entry.id.clone(),
            LoadedPlugin {
                manifest,
                instance,
                vdom,
            },
        );
        Ok(())
    }

    /// Load every `enabled` plugin listed in `vault/.hiker/plugins.json`.
    /// Returns the per-plugin load errors (id + error) so the UI can surface
    /// which plugins failed without aborting the others.
    pub fn load_enabled(&mut self, vault_root: &Path) -> Vec<(String, Error)> {
        let path = vault_root.join(".hiker/plugins.json");
        let file: PluginsFile = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let mut failures = Vec::new();
        for entry in file.plugins.iter().filter(|e| e.enabled) {
            if let Err(e) = self.load(vault_root, entry) {
                failures.push((entry.id.clone(), e));
            }
        }
        failures
    }

    /// The manifest of a loaded plugin.
    pub fn manifest(&self, plugin_id: &str) -> Option<&Manifest> {
        self.plugins.get(plugin_id).map(|p| &p.manifest)
    }

    /// Ids of every loaded plugin.
    pub fn loaded_ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Whether a plugin is currently loaded (instantiated + running).
    pub fn is_loaded(&self, plugin_id: &str) -> bool {
        self.plugins.contains_key(plugin_id)
    }

    /// Unload a plugin: drop its instance and cached VDOM. Returns whether one
    /// was loaded. Used by the manager's Disable action (the `plugins.json`
    /// flag is the durable state; this stops the live instance).
    pub fn unload(&mut self, plugin_id: &str) -> bool {
        self.plugins.remove(plugin_id).is_some()
    }

    /// The latest VDOM a plugin produced, for the renderer to paint.
    pub fn current_vdom(&self, plugin_id: &str) -> Option<&Node> {
        self.plugins.get(plugin_id).and_then(|p| p.vdom.as_ref())
    }

    /// Deliver a UI event to a plugin and capture the fresh VDOM it returns.
    /// `element_id` is the primitive's plugin-defined id; `kind` the event
    /// (`"click"`, `"input"`, …); `payload` any event data (e.g. the new
    /// input value).
    pub fn dispatch_event(
        &mut self,
        plugin_id: &str,
        element_id: &str,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<(), Error> {
        let plugin = self
            .plugins
            .get_mut(plugin_id)
            .ok_or_else(|| Error::NotFound(plugin_id.to_string()))?;
        // Build the event object, moving `payload` in so it's consumed by value.
        let mut event = serde_json::Map::new();
        event.insert("element_id".to_string(), element_id.into());
        event.insert("kind".to_string(), kind.into());
        event.insert("payload".to_string(), payload);
        let event = serde_json::Value::Object(event);
        let out = plugin.instance.call_json("on_ui_event", &event.to_string())?;
        plugin.vdom = parse_vdom(&out)?;
        Ok(())
    }
}

/// Parse a plugin's VDOM result: `null` → no UI, otherwise a [`Node`] tree. A
/// `{ "error": ... }` payload (the host's failure envelope) surfaces as an
/// ABI error rather than being mistaken for a node.
fn parse_vdom(json: &str) -> Result<Option<Node>, Error> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error::Abi(e.to_string()))?;
    if value.is_null() {
        return Ok(None);
    }
    if let Some(err) = value.get("error").and_then(serde_json::Value::as_str) {
        return Err(Error::Trap(err.to_string()));
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| Error::Abi(e.to_string()))
}
