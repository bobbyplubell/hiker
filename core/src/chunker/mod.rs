//! Chunking entry point. The `Chunker` trait is the minimum shape every
//! per-format chunker shares; format-specific logic lives in submodules
//! (`markdown`, `txt`). The ingest pipeline picks the right chunker by
//! extension — see `core::indexer`.

pub mod markdown;
pub mod txt;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub index: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
    /// "Section > Subsection" breadcrumb of the enclosing heading, or None for
    /// content above any heading.
    pub heading_path: Option<String>,
}

pub trait Chunker: Send + Sync {
    fn chunk(&self, source: &str) -> Vec<Chunk>;
}

pub use markdown::{chunk_markdown, MarkdownChunker};
pub use txt::{chunk_txt, TxtChunker};
