//! Plugin manifest, the capability/permission set, and the hash-pinned
//! `plugins.json` registry. A plugin gets ambient access to nothing: every
//! host call checks the manifest-declared permission and fails closed. A
//! plugin's bytes + manifest are pinned by blake3 in `plugins.json`, so a
//! change on disk is a distinct identity that must be re-consented, never a
//! silent re-run with new code or widened capabilities. See `plugins.md`.
//
// status: plugin-manifest, plugin-permissions, plugin-hash-pin

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::error::Error;

/// The capability set a plugin declared in its manifest. Stored as the raw
/// `domain:scope` strings (`read:notes`, `ui:sidebar-panel`, …); the host-call
/// gate consults it by exact string so the vocabulary stays open without a
/// closed enum to grow. A plugin is granted nothing it didn't list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permissions(pub BTreeSet<String>);

impl Permissions {
    /// Whether `perm` was granted. The whole point of the system: a host call
    /// that needs `perm` is refused unless this returns true.
    pub fn grants(&self, perm: &str) -> bool {
        self.0.contains(perm)
    }

    /// Build from a list of permission strings (manifest order, deduped).
    pub fn from_strings<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(items.into_iter().map(Into::into).collect())
    }
}

/// UI surfaces a plugin registers. v1 covers sidebar panels (the query-plugin
/// archetype); more surfaces (status bar, command palette) extend this.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiDecl {
    #[serde(default)]
    pub sidebar_panels: Vec<SidebarPanelDecl>,
}

/// One sidebar panel the plugin owns: a stable `id`, a title, an icon name
/// from the host icon set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidebarPanelDecl {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub icon: String,
}

/// A plugin's `manifest.json`, immutable for a given plugin identity (changing
/// it changes the hash → new identity → re-consent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version of this manifest format.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub ui: UiDecl,
    /// The wasm file relative to the plugin's location dir.
    #[serde(default = "default_entry")]
    pub entry: String,
}

const fn default_schema_version() -> u32 {
    1
}

fn default_entry() -> String {
    "plugin.wasm".to_string()
}

impl Manifest {
    /// Parse a manifest from raw JSON bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(bytes).map_err(|e| Error::Manifest(e.to_string()))
    }
}

/// One entry in `vault/.hiker/plugins.json`: where the plugin lives plus the
/// two blake3 pins (manifest + wasm) and the enable flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedEntry {
    pub id: String,
    /// Vault-relative directory holding `manifest.json` + the wasm entry.
    pub location: String,
    /// `blake3:<hex>` of the manifest bytes.
    pub manifest_hash: String,
    /// `blake3:<hex>` of the wasm bytes.
    pub wasm_hash: String,
    #[serde(default)]
    pub enabled: bool,
}

/// The vault-level `plugins.json` — the source of truth for which plugins load.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsFile {
    #[serde(default)]
    pub plugins: Vec<PinnedEntry>,
}

/// `blake3:<hex>` of `bytes`, the pin format used in `plugins.json`.
pub fn blake3_pin(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

/// Verify `bytes` matches a pinned `blake3:<hex>` value. A mismatch is the
/// "plugin changed on disk" abort — never load new code against an old pin.
pub fn verify_pin(bytes: &[u8], pinned: &str) -> Result<(), Error> {
    let actual = blake3_pin(bytes);
    if actual == pinned {
        Ok(())
    } else {
        Err(Error::HashMismatch {
            expected: pinned.to_string(),
            actual,
        })
    }
}
