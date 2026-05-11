pub mod acp;
pub mod agent;
pub mod audit;
pub mod autosave;
pub mod changes;
pub mod chunker;
pub mod config;
pub mod diff;
pub mod embed;
pub mod error;
pub mod frontmatter;
pub mod hash;
pub mod indexer;
pub mod llm;
pub mod observability;
pub mod prompts;
pub mod ops;
pub mod search;
pub mod sessions;
pub mod store;
pub mod tasks;
pub mod trails;
pub mod trash;
pub mod vault;
pub mod watcher;

pub use error::HikerError;
pub use hash::hash_str;
pub use vault::{DirEntryDto, EntryKind, Vault};

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
