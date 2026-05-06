pub mod chunker;
pub mod embed;
pub mod error;
pub mod hash;
pub mod indexer;
pub mod observability;
pub mod store;
pub mod trash;
pub mod vault;
pub mod watcher;

pub use error::HikerError;
pub use hash::hash_str;
pub use vault::{DirEntryDto, EntryKind, Vault};
