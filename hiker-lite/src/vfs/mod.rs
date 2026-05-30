//! Virtual filesystem trait. Every panel and the editor host code
//! against `Vfs`; the only place that touches `std::fs` is the native
//! backend in [`native`]. Phase 2 will add an OPFS backend behind the
//! same trait.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

#[cfg(not(target_arch = "wasm32"))]
pub mod native;

/// Path inside the Vfs root. Always forward-slash separated; the native
/// backend joins it against an OS root when materialising real paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VfsPath {
    components: Vec<String>,
}

impl VfsPath {
    pub const fn root() -> Self {
        Self { components: Vec::new() }
    }

    pub fn join(&self, name: &str) -> Self {
        let mut next = self.clone();
        next.components.push(name.to_owned());
        next
    }

    pub fn file_name(&self) -> Option<&str> {
        self.components.last().map(String::as_str)
    }

    pub fn components(&self) -> &[String] {
        &self.components
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.components.join("/"))
    }
}

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub path: VfsPath,
    pub name: String,
    pub is_dir: bool,
}

pub type VfsResult<T> = Result<T, VfsError>;

#[derive(Debug)]
pub enum VfsError {
    NotFound(String),
    Io(String),
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(p) => write!(f, "not found: {p}"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for VfsError {}

#[async_trait]
pub trait Vfs: Send + Sync + 'static {
    async fn read(&self, path: &VfsPath) -> VfsResult<Vec<u8>>;
    async fn write(&self, path: &VfsPath, bytes: &[u8]) -> VfsResult<()>;
    async fn list(&self, dir: &VfsPath) -> VfsResult<Vec<DirEntry>>;
}

pub type DynVfs = Arc<dyn Vfs>;
