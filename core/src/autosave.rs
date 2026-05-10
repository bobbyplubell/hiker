//! Crash-recovery autosave for dirty editor buffers, plus a tab-state
//! snapshot the next vault open round-trips. See docs/autosave.md.
//!
//! Storage at `<vault>/.hiker/autosave/`: one `<id>--<slug>.md` per dirty
//! buffer (overwritten in place each tick, NPP shape) plus an
//! `index.json` carrying the path↔id map, per-entry content hash, and
//! the authoritative tab-state snapshot. All filesystem writes for the
//! autosave directory live here — module discipline mirrors
//! `core::store` and `core::changes`.
//
// status: autosave-backend-module
// status: autosave-store-layout
// status: autosave-one-per-buffer
// status: autosave-recover-cmd
// status: autosave-vault-swap-clears
// status: autosave-no-watcher-suppression
// status: autosave-backup-class

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

const AUTOSAVE_DIRNAME: &str = "autosave";
const INDEX_NAME: &str = "index.json";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum AutosaveError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("schema version mismatch: index is v{found}, binary expects v{expected}")]
    VersionMismatch { found: u32, expected: u32 },
}

/// Tab-state snapshot. `open_paths` is the ordered list the frontend
/// reopens on next vault open; `active_path` is the tab that gets
/// activated; `preview_path` is the at-most-one preview slot's path
/// (or `None` when no preview tab existed at flush time).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabState {
    #[serde(default)]
    pub open_paths: Vec<String>,
    #[serde(default)]
    pub active_path: Option<String>,
    #[serde(default)]
    pub preview_path: Option<String>,
    #[serde(default)]
    pub saved_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub autosave_id: String,
    pub content_hash: String,
    pub saved_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, IndexEntry>,
    #[serde(default)]
    tab_state: Option<TabState>,
}

impl Default for IndexFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
            tab_state: None,
        }
    }
}

/// Recovered buffer surfaced by `recover()` — only entries whose
/// autosaved content differs from what's on disk for the same path
/// (or whose on-disk file is gone) make the cut.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredEntry {
    pub path: String,
    pub autosave_id: String,
    pub autosaved_content: Vec<u8>,
    pub autosaved_hash: String,
    pub on_disk_hash: Option<String>,
    pub saved_at_ms: i64,
}

pub struct Autosave {
    vault_root: PathBuf,
    dir: PathBuf,
    /// Serializes index.json read-modify-writes + per-buffer file writes
    /// per spec: "concurrent ticks for the same path are serialized in
    /// the backend." Using a single mutex (rather than per-path) keeps
    /// the implementation simple — autosave is a low-frequency write
    /// path (one tick per 5s) and the lock is held across one file
    /// write + one index rewrite, both small.
    lock: Mutex<()>,
}

impl Autosave {
    pub fn open(vault_root: &Path) -> Result<Self, AutosaveError> {
        let dir = vault_root.join(".hiker").join(AUTOSAVE_DIRNAME);
        fs::create_dir_all(&dir)?;
        let me = Self {
            vault_root: vault_root.to_path_buf(),
            dir,
            lock: Mutex::new(()),
        };
        // Best-effort schema check: if index.json exists with a future
        // version, fail loud (mirrors `store-version-fail-loud`).
        match me.read_index_for_version_check()? {
            Some(v) if v != SCHEMA_VERSION => {
                return Err(AutosaveError::VersionMismatch {
                    found: v,
                    expected: SCHEMA_VERSION,
                });
            }
            _ => {}
        }
        Ok(me)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_NAME)
    }

    fn read_index_for_version_check(&self) -> Result<Option<u32>, AutosaveError> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(None);
        }
        let raw = fs::read(&p)?;
        if raw.is_empty() {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct VersionOnly {
            version: u32,
        }
        let v: VersionOnly = serde_json::from_slice(&raw)?;
        Ok(Some(v.version))
    }

    fn read_index(&self) -> Result<IndexFile, AutosaveError> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(IndexFile::default());
        }
        let raw = fs::read(&p)?;
        if raw.is_empty() {
            return Ok(IndexFile::default());
        }
        let parsed: IndexFile = serde_json::from_slice(&raw)?;
        Ok(parsed)
    }

    fn write_index_atomic(&self, idx: &IndexFile) -> Result<(), AutosaveError> {
        let p = self.index_path();
        let tmp = p.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(idx)?;
        write_file_atomic(&tmp, &p, &bytes)?;
        Ok(())
    }

    /// Write or overwrite a sidecar for `path`. Idempotent on the same
    /// `path` — overwrites the existing file in place per
    /// `autosave-one-per-buffer`. `buffer_hash` is the blake3 of
    /// `contents` (the frontend computes it once and passes it through;
    /// recomputing here would burn cycles on the hot tick path).
    pub fn write(
        &self,
        path: &str,
        contents: &[u8],
        buffer_hash: &str,
    ) -> Result<(), AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        let now = now_ms();
        let entry = match idx.entries.get(path) {
            Some(e) => IndexEntry {
                autosave_id: e.autosave_id.clone(),
                content_hash: buffer_hash.to_string(),
                saved_at_ms: now,
            },
            None => IndexEntry {
                autosave_id: Ulid::new().to_string(),
                content_hash: buffer_hash.to_string(),
                saved_at_ms: now,
            },
        };
        let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
        let tmp = file_path.with_extension("md.tmp");
        write_file_atomic(&tmp, &file_path, contents)?;
        idx.entries.insert(path.to_string(), entry);
        self.write_index_atomic(&idx)?;
        Ok(())
    }

    /// Drop the sidecar for `path`. No-op when no entry exists.
    pub fn clear(&self, path: &str) -> Result<(), AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        if let Some(entry) = idx.entries.remove(path) {
            let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
            if file_path.exists() {
                let _ = fs::remove_file(&file_path);
            }
            self.write_index_atomic(&idx)?;
        }
        Ok(())
    }

    pub fn save_tab_state(&self, mut state: TabState) -> Result<(), AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        if state.saved_at_ms == 0 {
            state.saved_at_ms = now_ms();
        }
        idx.tab_state = Some(state);
        self.write_index_atomic(&idx)?;
        Ok(())
    }

    pub fn load_tab_state(&self) -> Result<Option<TabState>, AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let idx = self.read_index()?;
        Ok(idx.tab_state)
    }

    /// Walk the index, drop entries whose autosaved hash matches the
    /// live on-disk hash for the same path (stale snapshots from the
    /// last clean session), and return the genuine deltas. Matches are
    /// removed from the index file as a side effect so the next
    /// `recover()` doesn't re-do the work.
    pub fn recover(&self) -> Result<Vec<RecoveredEntry>, AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        let mut out: Vec<RecoveredEntry> = Vec::new();
        let mut drop_paths: Vec<String> = Vec::new();
        for (path, entry) in idx.entries.iter() {
            let live = self.live_disk_hash(path)?;
            let on_disk_matches = match &live {
                Some(h) => h == &entry.content_hash,
                None => false,
            };
            let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
            if on_disk_matches {
                // Stale; drop the sidecar + index entry.
                drop_paths.push(path.clone());
                if file_path.exists() {
                    let _ = fs::remove_file(&file_path);
                }
                continue;
            }
            // Genuine recovery candidate. Read the autosaved bytes.
            let autosaved_content = match fs::read(&file_path) {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Index points at a missing sidecar — drop the
                    // orphan; nothing to recover.
                    drop_paths.push(path.clone());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            out.push(RecoveredEntry {
                path: path.clone(),
                autosave_id: entry.autosave_id.clone(),
                autosaved_content,
                autosaved_hash: entry.content_hash.clone(),
                on_disk_hash: live,
                saved_at_ms: entry.saved_at_ms,
            });
        }
        if !drop_paths.is_empty() {
            for p in &drop_paths {
                idx.entries.remove(p);
            }
            self.write_index_atomic(&idx)?;
        }
        Ok(out)
    }

    /// Discard the sidecar without restoring it. Same on-disk effect as
    /// `clear`; the separate verb mirrors the spec's recovery-modal API
    /// so the frontend can keep "Discard" / "successful save → clear"
    /// as distinct call sites.
    pub fn discard(&self, path: &str) -> Result<(), AutosaveError> {
        self.clear(path)
    }

    /// Wipe all on-disk autosave state for this vault. Called on vault
    /// swap to guarantee no cross-vault leakage of state — even if the
    /// per-buffer clear/save_tab_state path was skipped (force-quit
    /// during swap, etc.).
    ///
    /// status: autosave-vault-swap-clears
    pub fn vault_swap_reset(&self) -> Result<(), AutosaveError> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file() {
                let _ = fs::remove_file(&p);
            }
        }
        Ok(())
    }

    fn live_disk_hash(&self, rel_path: &str) -> Result<Option<String>, AutosaveError> {
        let abs = self.vault_root.join(rel_path);
        match fs::read(&abs) {
            Ok(bytes) => Ok(Some(blake3::hash(&bytes).to_hex().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Build the on-disk filename for an autosave sidecar. The `<id>` half is
/// the canonical lookup (the index.json maps path → id); the `--<slug>`
/// suffix is purely debuggable. Slug is the path's basename with a
/// conservative character allowlist.
fn autosave_filename(id: &str, path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let mut slug = String::with_capacity(basename.len());
    for ch in basename.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            slug.push(ch);
        } else {
            slug.push('_');
        }
    }
    if slug.is_empty() {
        slug.push_str("buf");
    }
    format!("{id}--{slug}")
}

/// Atomic file write: create at `tmp`, fsync, rename to `final_path`.
/// Crash mid-write leaves either the prior file or no change — never a
/// half-written one. Matches the `vim`-style write-temp-then-rename
/// pattern called out in the spec.
fn write_file_atomic(tmp: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(tmp, final_path)?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hash(b: &[u8]) -> String {
        blake3::hash(b).to_hex().to_string()
    }

    #[test]
    fn write_clear_round_trip() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"draft", &hash(b"draft")).unwrap();
        let recovered = a.recover().unwrap();
        // No on-disk file at all → entry surfaces.
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].path, "a.md");
        assert_eq!(recovered[0].autosaved_content, b"draft");
        assert!(recovered[0].on_disk_hash.is_none());

        a.clear("a.md").unwrap();
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn recover_drops_matching_on_disk() {
        let dir = tempdir().unwrap();
        let body = b"clean save";
        std::fs::write(dir.path().join("a.md"), body).unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", body, &hash(body)).unwrap();
        // The autosave matches the live disk file → stale, dropped.
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn recover_surfaces_when_disk_differs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"on-disk").unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"in-memory", &hash(b"in-memory")).unwrap();
        let recovered = a.recover().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].autosaved_content, b"in-memory");
        assert_eq!(recovered[0].on_disk_hash, Some(hash(b"on-disk")));
    }

    #[test]
    fn write_overwrites_in_place() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"v1", &hash(b"v1")).unwrap();
        let id1 = a.read_index().unwrap().entries["a.md"].autosave_id.clone();
        a.write("a.md", b"v2", &hash(b"v2")).unwrap();
        let id2 = a.read_index().unwrap().entries["a.md"].autosave_id.clone();
        assert_eq!(id1, id2, "autosave_id stable across ticks for the same path");
        // Only one sidecar file under the dir for this path's id.
        let count = std::fs::read_dir(a.dir())
            .unwrap()
            .filter(|e| {
                let n = e.as_ref().unwrap().file_name();
                n.to_string_lossy().starts_with(&id1)
            })
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn tab_state_round_trips() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        assert!(a.load_tab_state().unwrap().is_none());
        let s = TabState {
            open_paths: vec!["a.md".into(), "b.md".into()],
            active_path: Some("b.md".into()),
            preview_path: Some("c.md".into()),
            saved_at_ms: 0,
        };
        a.save_tab_state(s.clone()).unwrap();
        let loaded = a.load_tab_state().unwrap().unwrap();
        assert_eq!(loaded.open_paths, s.open_paths);
        assert_eq!(loaded.active_path, s.active_path);
        assert_eq!(loaded.preview_path, s.preview_path);
        assert!(loaded.saved_at_ms > 0);
    }

    #[test]
    fn vault_swap_reset_wipes_dir() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"x", &hash(b"x")).unwrap();
        a.save_tab_state(TabState {
            open_paths: vec!["a.md".into()],
            ..Default::default()
        })
        .unwrap();
        a.vault_swap_reset().unwrap();
        // Re-open against the same vault root; nothing left.
        let a2 = Autosave::open(dir.path()).unwrap();
        assert!(a2.load_tab_state().unwrap().is_none());
        assert!(a2.recover().unwrap().is_empty());
    }

    #[test]
    fn version_mismatch_fails_loud() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        // Write a future-version index by hand.
        let raw = serde_json::json!({
            "version": 99,
            "entries": {},
            "tab_state": null,
        });
        std::fs::write(a.index_path(), raw.to_string()).unwrap();
        match Autosave::open(dir.path()) {
            Err(AutosaveError::VersionMismatch { found, expected }) => {
                assert_eq!(found, 99);
                assert_eq!(expected, SCHEMA_VERSION);
            }
            Err(e) => panic!("expected VersionMismatch, got {e:?}"),
            Ok(_) => panic!("expected VersionMismatch, got Ok"),
        }
    }

    #[test]
    fn recover_drops_orphan_sidecar_pointer() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"x", &hash(b"x")).unwrap();
        // Manually delete the sidecar file — index now points at nothing.
        for entry in std::fs::read_dir(a.dir()).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                std::fs::remove_file(&p).unwrap();
            }
        }
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
        // And the index was pruned of the orphan.
        assert!(a.read_index().unwrap().entries.is_empty());
    }
}
