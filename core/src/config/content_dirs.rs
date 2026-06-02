//! Config sections that name **where vault content folders live**.
//!
//! Both `[extract]` and `[chat]` are sections `core` loads only so the
//! strict-load `Config` accepts the table — the work each governs lives
//! outside `core` (the decoupled `hiker-extract` crate for extraction; the
//! app-layer chat runtime for sessions). What `core` cares about is the
//! folder paths: where captures/clips land, where chat-session notes are
//! written. Grouping them here keeps `sections.rs` focused on the sections
//! whose behavior `core` actually drives.

use serde::{Deserialize, Serialize};

/// `[extract]` section. Extraction tunables (`docs/extract.md`). Vault-level
/// is the natural scope (which folders hold extractable sources is per-vault),
/// with a user-level default fine for the common case. The extraction work
/// itself lives in the decoupled `hiker-extract` crate (`extract-crate-decoupled`);
/// `core` only loads this config so the strict-load `Config` accepts an
/// `[extract]` table. See `docs/settings.md` §`[extract]`.
///
/// status: settings-section-extract
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractConfig {
    /// Folders/globs whose non-md sources auto-extract on appear/change
    /// (`extract-trigger-auto-glob`). gitignore-style globs over vault-
    /// relative paths; replaces, doesn't concatenate, per the array-merge
    /// rule. Default empty = no auto-extraction; non-md elsewhere extracts
    /// only on the explicit "Make searchable" action (`extract-trigger-on-demand`).
    #[serde(default)]
    pub auto_globs: Vec<String>,
    /// Destination folder for one-off URL captures (`scrape-cmd`);
    /// `hiker scrape --into` overrides per call.
    #[serde(default = "default_clip_folder")]
    pub clip_folder: String,
    /// Vault default for the binary-artifact retention cascade
    /// (`extract-artifact-retention`): `latest` / `keep:N` / `forever`;
    /// per-crawl/per-feed/per-source frontmatter overrides it.
    #[serde(default = "default_artifact_retention")]
    pub artifact_retention: String,
    /// Default `poll_interval` for a new RSS/feed capture note when none is
    /// set (`rss-poll-schedule`); a feed may set its own.
    #[serde(default = "default_feed_poll")]
    pub feed_default_poll: String,
    /// Vault default child-count bound for feeds (`rss-item-retention`):
    /// `keep:N` / `forever`; per-feed frontmatter overrides.
    #[serde(default = "default_feed_item_retention")]
    pub feed_item_retention: String,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            auto_globs: Vec::new(),
            clip_folder: default_clip_folder(),
            artifact_retention: default_artifact_retention(),
            feed_default_poll: default_feed_poll(),
            feed_item_retention: default_feed_item_retention(),
        }
    }
}

fn default_clip_folder() -> String {
    "clips/".to_string()
}

fn default_artifact_retention() -> String {
    "latest".to_string()
}

fn default_feed_poll() -> String {
    "6h".to_string()
}

fn default_feed_item_retention() -> String {
    "keep:200".to_string()
}

/// `[chat]` section. Loaded so the strict-load `Config` accepts a `[chat]`
/// table. Owns the location of the visible chat-session note folder; the
/// chat *runtime* lives in the app layer and reads this field to build
/// session paths. See `docs/settings.md` §`[chat]`.
///
/// status: settings-section-chat
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    /// Visible folder holding native + imported chat-session notes
    /// (`chat-session-markdown-store`); imports land in its `imported/`
    /// subfolder. Default `"chats/"`. Sessions are ordinary indexed notes,
    /// not hidden under `.hiker/` (`subsystem-notes-visible`).
    #[serde(default = "default_chats_dir")]
    pub chats_dir: String,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            chats_dir: default_chats_dir(),
        }
    }
}

fn default_chats_dir() -> String {
    "chats/".to_string()
}
