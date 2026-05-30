//! Native `std::fs`-backed `Vfs`. All ops jump to tokio's blocking pool
//! via `tokio::fs` so they're trivially async-compatible.

use std::path::PathBuf;

use async_trait::async_trait;

use super::{DirEntry, Vfs, VfsError, VfsPath, VfsResult};

pub struct Backend {
    root: PathBuf,
}

impl Backend {
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve(&self, path: &VfsPath) -> PathBuf {
        let mut out = self.root.clone();
        for c in path.components() {
            out.push(c);
        }
        out
    }
}

fn io_to_vfs(e: &std::io::Error, p: &VfsPath) -> VfsError {
    if e.kind() == std::io::ErrorKind::NotFound {
        VfsError::NotFound(p.to_string())
    } else {
        VfsError::Io(e.to_string())
    }
}

#[async_trait]
impl Vfs for Backend {
    async fn read(&self, path: &VfsPath) -> VfsResult<Vec<u8>> {
        let abs = self.resolve(path);
        tokio::fs::read(&abs).await.map_err(|e| io_to_vfs(&e, path))
    }

    async fn write(&self, path: &VfsPath, bytes: &[u8]) -> VfsResult<()> {
        let abs = self.resolve(path);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_to_vfs(&e, path))?;
        }
        tokio::fs::write(&abs, bytes)
            .await
            .map_err(|e| io_to_vfs(&e, path))
    }

    async fn list(&self, dir: &VfsPath) -> VfsResult<Vec<DirEntry>> {
        let abs = self.resolve(dir);
        let mut rd = tokio::fs::read_dir(&abs)
            .await
            .map_err(|e| io_to_vfs(&e, dir))?;
        let mut out = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| io_to_vfs(&e, dir))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden / VCS noise — Phase 1 keeps it simple. A
            // gitignore-aware walk via `ignore` is a Phase 1.5 polish.
            if name.starts_with('.') {
                continue;
            }
            let ft = entry
                .file_type()
                .await
                .map_err(|e| io_to_vfs(&e, dir))?;
            let is_dir = ft.is_dir();
            out.push(DirEntry {
                path: dir.join(&name),
                name,
                is_dir,
            });
        }
        out.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(out)
    }

}
