use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HikerError;
use crate::hash::hash_str;
use crate::store::Store;
use crate::watcher::Watcher;

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

    pub fn read_file_with_hash(&self, rel: &str) -> Result<(String, String), HikerError> {
        let contents = self.read_file(rel)?;
        let hash = hash_str(&contents);
        Ok((contents, hash))
    }

    pub fn write_file(&self, rel: &str, contents: &str) -> Result<(), HikerError> {
        let abs = self.resolve(rel)?;
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, contents)?;
        Ok(())
    }

    pub fn write_file_checked(
        &self,
        rel: &str,
        expected_hash: &str,
        contents: &str,
    ) -> Result<String, HikerError> {
        let abs = self.resolve(rel)?;
        match fs::read(&abs) {
            Ok(bytes) => {
                let on_disk = String::from_utf8(bytes)
                    .map_err(|e| HikerError::NotUtf8(e.to_string()))?;
                let found = hash_str(&on_disk);
                if found != expected_hash {
                    return Err(HikerError::DiskDrift {
                        expected: expected_hash.to_string(),
                        found,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !expected_hash.is_empty() {
                    return Err(HikerError::DiskDrift {
                        expected: expected_hash.to_string(),
                        found: String::new(),
                    });
                }
            }
            Err(e) => return Err(e.into()),
        }
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, contents)?;
        Ok(hash_str(contents))
    }

    /// Create an empty file at `rel`. Errors if the path already exists or
    /// the parent directory is missing — auto-suffixing on collision is the
    /// caller's job (the UI tree button retries with `new-note-1.md`,
    /// `new-note-2.md`, …; the CLI surfaces the error verbatim). Returns the
    /// rel path that was written so callers can chain on the result.
    ///
    /// Suppressing the watcher before calling this is the caller's
    /// responsibility (see editor.md "API & edge cases" and
    /// `watcher-suppress-self-writes`); skip it where there's no `Watcher`
    /// open (CLI, tests).
    ///
    /// status: create-note-core-cmd
    pub fn create_note(&self, rel: &str) -> Result<String, HikerError> {
        let abs = self.resolve(rel)?;
        if abs.exists() {
            return Err(HikerError::AlreadyExists(rel.to_string()));
        }
        match abs.parent() {
            Some(parent) if !parent.exists() => {
                return Err(HikerError::NotFound(format!("parent of {rel}")));
            }
            _ => {}
        }
        fs::write(&abs, "")?;
        Ok(rel.to_string())
    }
}

/// Atomic rename of a note: fs rename + index path update, with watcher
/// suppression around both writes so the move isn't re-observed as a
/// Deleted/Created pair. Errors leave the source untouched (or restored, if
/// the index update fails after the fs rename succeeded). Behaviors per
/// editor.md "API & edge cases":
///
/// - target collision → `AlreadyExists`, source untouched
/// - source missing → `NotFound`
/// - target parent missing → `NotFound`
/// - non-indexed source (e.g. a non-md file) → fs rename only, no error
///
/// Folder-level moves are out of scope here — the caller walks the folder
/// and invokes `move_note` per file (drag-and-drop-move handles the walk).
///
/// `watcher` is optional because the CLI runs without one. When present,
/// both `from` and `to` get suppressed before the rename so any
/// platform-specific ordering of the resulting events is filtered.
///
/// status: move-note-core-cmd
pub fn move_note(
    vault: &Vault,
    store: &mut Store,
    watcher: Option<&Watcher>,
    from: &str,
    to: &str,
) -> Result<(), HikerError> {
    if from == to {
        return Ok(());
    }
    let from_abs = vault.resolve(from)?;
    let to_abs = vault.resolve(to)?;
    if !from_abs.exists() {
        return Err(HikerError::NotFound(from.to_string()));
    }
    if to_abs.exists() {
        return Err(HikerError::AlreadyExists(to.to_string()));
    }
    match to_abs.parent() {
        Some(parent) if !parent.exists() => {
            return Err(HikerError::NotFound(format!("parent of {to}")));
        }
        _ => {}
    }

    if let Some(w) = watcher {
        w.suppress(from);
        w.suppress(to);
    }

    fs::rename(&from_abs, &to_abs)?;

    // Index update. If the source isn't in the index (e.g. a non-md file or
    // an md file not yet ingested), there's nothing to do — the fs rename
    // alone is the whole operation. Any store error after a successful
    // rename gets a best-effort fs rollback so we don't leave the index and
    // disk disagreeing.
    let id_lookup = store.id_for_path(from);
    let id_opt = match id_lookup {
        Ok(o) => o,
        Err(e) => {
            let _ = fs::rename(&to_abs, &from_abs);
            return Err(HikerError::Io(e.to_string()));
        }
    };
    if let Some(id) = id_opt {
        if let Err(e) = store.rename_note(&id, to) {
            let _ = fs::rename(&to_abs, &from_abs);
            return Err(HikerError::Io(e.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{new_id, NoteUpsert, Store};
    use tempfile::tempdir;

    #[test]
    fn create_note_writes_empty_file_and_returns_path() {
        let dir = tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let p = vault.create_note("alpha.md").unwrap();
        assert_eq!(p, "alpha.md");
        let bytes = fs::read(dir.path().join("alpha.md")).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn create_note_collision_errors_without_clobbering() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"existing").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        match vault.create_note("a.md") {
            Err(HikerError::AlreadyExists(p)) => assert_eq!(p, "a.md"),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        let still = fs::read(dir.path().join("a.md")).unwrap();
        assert_eq!(still, b"existing");
    }

    #[test]
    fn create_note_missing_parent_errors() {
        let dir = tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        assert!(matches!(
            vault.create_note("nope/a.md"),
            Err(HikerError::NotFound(_))
        ));
    }

    #[test]
    fn move_note_renames_fs_and_index() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("from.md"), b"hi").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();

        let id = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id,
                path: "from.md",
                content_hash: "h",
                mtime: 0,
                size: 2,
                indexed_at: 0,
                embedder_version: "mock",
                chunks: Vec::new(),
            })
            .unwrap();

        move_note(&vault, &mut store, None, "from.md", "to.md").unwrap();
        assert!(!dir.path().join("from.md").exists());
        assert!(dir.path().join("to.md").exists());
        assert!(store.get_note_by_path("from.md").unwrap().is_none());
        let row = store.get_note_by_path("to.md").unwrap().unwrap();
        assert_eq!(row.id, id);
    }

    #[test]
    fn move_note_target_exists_errors_and_keeps_source() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"a").unwrap();
        fs::write(dir.path().join("b.md"), b"b").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        match move_note(&vault, &mut store, None, "a.md", "b.md") {
            Err(HikerError::AlreadyExists(_)) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        assert_eq!(fs::read(dir.path().join("a.md")).unwrap(), b"a");
        assert_eq!(fs::read(dir.path().join("b.md")).unwrap(), b"b");
    }

    #[test]
    fn move_note_unindexed_source_renames_only_fs() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), b"x").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        move_note(&vault, &mut store, None, "note.txt", "renamed.txt").unwrap();
        assert!(dir.path().join("renamed.txt").exists());
        assert!(!dir.path().join("note.txt").exists());
    }

    #[test]
    fn move_note_source_missing_errors() {
        let dir = tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        assert!(matches!(
            move_note(&vault, &mut store, None, "nope.md", "x.md"),
            Err(HikerError::NotFound(_))
        ));
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
