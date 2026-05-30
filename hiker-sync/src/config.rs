//! The `[sync]` config section (per-vault, `vault/.hiker/config.toml`).
//!
//! Secrets are excluded — the device keypair and vault content key are
//! user-scope per `sync-secrets-user-scope`, so a synced vault can't carry the
//! key that decrypts it. See `docs/sync.md` "`[sync]` config section".
//! [sync-config-section]

use std::collections::HashMap;

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
    /// THIS device's SELF-set human name, carried on the `Hello`/`HelloAck`
    /// handshake so peers can render "synced from `laptop`" instead of a
    /// fingerprint. Empty / `None` means unnamed. A device only ever names
    /// itself. [sync-device-name]
    #[serde(default)]
    pub device_name: Option<String>,
    /// Learned `fingerprint -> name` map for enrolled peers: the name each peer
    /// self-reported in its handshake. Adopted on every handshake (last name a
    /// device reports for ITSELF wins); a device never writes another device's
    /// name here on that device's behalf. The UI renders this for a remote
    /// device (a local `aliases.json` override wins over it). [sync-device-name]
    #[serde(default)]
    pub device_names: HashMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: SyncMode::Peer,
            server_url: String::new(),
            discovery: true,
            devices: Vec::new(),
            device_name: None,
            device_names: HashMap::new(),
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
            device_name: Some("laptop".into()),
            device_names: HashMap::from([("DEV-ABC".into(), "phone".into())]),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), c);
    }

    /// Defaults leave the device-naming fields empty, and a config JSON that
    /// predates them still parses (additive). [sync-device-name]
    #[test]
    fn device_name_defaults_and_back_compat() {
        let d = Settings::default();
        assert!(d.device_name.is_none());
        assert!(d.device_names.is_empty());

        // A pre-device-name config omits both fields entirely.
        let legacy = r#"{"enabled":false,"mode":"peer","server_url":"","discovery":true,"devices":[]}"#;
        let parsed: Settings = serde_json::from_str(legacy).unwrap();
        assert!(parsed.device_name.is_none());
        assert!(parsed.device_names.is_empty());
    }
}
