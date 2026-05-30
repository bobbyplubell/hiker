//! `hiker-core` is the engine behind hiker: it owns the vault index store,
//! the chunker and embedding pipeline, semantic and lexical search, the
//! op-log change substrate and trails subsystems, note clustering, and the agent /
//! LLM plumbing. Everything that touches the SQLite index, the filesystem
//! vault, or model inference lives here so the CLI, MCP server, and desktop
//! app share one implementation.

pub mod acp;
pub mod activity;
pub mod agent;
pub mod audit;
pub mod autosave;
pub mod boards;
pub mod chunker;
pub mod cluster;
pub mod config;
pub mod diff;
pub mod embed;
pub mod errors;
pub mod frontmatter;
pub mod inbox;
pub mod indexer;
pub mod links_rename;
pub mod llm;
pub mod observability;
pub mod oplog;
pub mod plugins;
pub mod prompts;
pub mod ops;
pub mod search;
pub mod sessions;
pub mod store;
pub mod suggest;
pub mod tasks;
pub mod textpatch;
pub mod trails;
pub mod trees;
pub mod trash;
pub mod vault;
pub mod watcher;
pub mod wikilink;

#[cfg(test)]
pub(crate) mod test_helpers;

/// Stable content hash for a string, rendered as lowercase hex. Used
/// crate-wide as the canonical note/content fingerprint (index dedupe,
/// staging conflict detection, change tracking) and re-exported to the
/// app and CLI so every layer hashes content the same way.
pub fn hash_string(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}


/// Context block the frontend pre-resolves and sends alongside a chat
/// turn. Shared between the HTTP/ACP adapter and the basic agent loop.
/// Mirrors the frontend's `ChatContextBlock` interface from `chat.ts`.
///
/// status: chat-input-at-mentions
/// status: chat-active-note-context-injection
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatContextBlock {
    pub kind: String,
    pub rel_path: String,
    pub content: String,
    #[serde(default)]
    pub line_range: Option<String>,
}
