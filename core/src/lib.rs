use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HikerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("path escapes vault: {0}")]
    PathEscape(String),
    #[error("not utf-8: {0}")]
    NotUtf8(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Dir,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntryDto {
    pub name: String,
    pub rel_path: String,
    pub kind: EntryKind,
}

pub struct Vault {
    root: PathBuf,
}

impl Vault {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, HikerError> {
        let root = root.into().canonicalize()?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, rel: &str) -> Result<PathBuf, HikerError> {
        let candidate = self.root.join(rel);
        let normalized = normalize(&candidate);
        if !normalized.starts_with(&self.root) {
            return Err(HikerError::PathEscape(rel.to_string()));
        }
        Ok(normalized)
    }

    pub fn list_dir(&self, rel: &str) -> Result<Vec<DirEntryDto>, HikerError> {
        let abs = self.resolve(rel)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&abs)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let ft = entry.file_type()?;
            let kind = if ft.is_dir() {
                EntryKind::Dir
            } else if ft.is_file() {
                EntryKind::File
            } else {
                continue;
            };
            let rel_path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel.trim_end_matches('/'), name)
            };
            out.push(DirEntryDto { name, rel_path, kind });
        }
        out.sort_by(|a, b| match (&a.kind, &b.kind) {
            (EntryKind::Dir, EntryKind::File) => std::cmp::Ordering::Less,
            (EntryKind::File, EntryKind::Dir) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });
        Ok(out)
    }

    pub fn read_file(&self, rel: &str) -> Result<String, HikerError> {
        let abs = self.resolve(rel)?;
        let bytes = fs::read(&abs)?;
        String::from_utf8(bytes).map_err(|e| HikerError::NotUtf8(e.to_string()))
    }

    pub fn write_file(&self, rel: &str, contents: &str) -> Result<(), HikerError> {
        let abs = self.resolve(rel)?;
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, contents)?;
        Ok(())
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            ParentDir => {
                out.pop();
            }
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
