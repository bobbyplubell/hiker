//! The `[sync]` config section (per-vault, `vault/.hiker/config.toml`).
//!
//! Secrets are excluded — the device keypair and vault content key are
//! user-scope per `sync-secrets-user-scope`, so a synced vault can't carry the
//! key that decrypts it. See `docs/sync.md` "`[sync]` config section".
//! [sync-config-section]

use serde::{Deserialize, Serialize};

/// Sync topology mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Direct peer-to-peer on the LAN, no server.
    Peer,
    /// Through a decoupled relay / hub server.
    Server,
    /// Both peer and server paths.
    Both,
}

/// The `[sync]` config block. Mirrors the defaults in `docs/sync.md`:
/// `enabled = false`, `mode = "peer"`, `discovery = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Settings {
    /// Opt in per vault.
    pub enabled: bool,
    /// `peer` | `server` | `both`.
    pub mode: SyncMode,
    /// Relay / hub URL, when using a server.
    pub server_url: String,
    /// Allow the manual, time-boxed mDNS discovery window.
    pub discovery: bool,
    /// Enrolled device fingerprints.
    pub devices: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SyncMode::Peer,
            server_url: String::new(),
            discovery: true,
            devices: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Settings::default();
        assert!(!c.enabled);
        assert_eq!(c.mode, SyncMode::Peer);
        assert_eq!(c.server_url, "");
        assert!(c.discovery);
        assert!(c.devices.is_empty());
    }

    #[test]
    fn mode_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&SyncMode::Peer).unwrap(), "\"peer\"");
        assert_eq!(serde_json::to_string(&SyncMode::Both).unwrap(), "\"both\"");
    }

    #[test]
    fn config_round_trips() {
        let c = Settings {
            enabled: true,
            mode: SyncMode::Both,
            server_url: "wss://hub.example".into(),
            discovery: false,
            devices: vec!["DEV-ABC".into()],
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), c);
    }
}
