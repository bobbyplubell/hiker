//! Vault trash. Soft-delete storage at `<vault>/.hiker/trash/`. Files and
//! folders moved here by `vault::delete_note` are restorable until
//! `vault-trash-empty` runs. Manifest at `manifest.yaml` records each entry's
//! original path and metadata so restore knows where to put things back.
//!
//! See docs/editor.md "Delete semantics" for the user-visible contract.
//!
//! status: vault-trash

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::errors::HikerError;

const TRASH_DIRNAME: &str = "trash";
const MANIFEST_NAME: &str = "manifest.yaml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// Vault-relative path the file/folder lived at before deletion.
    pub original_path: String,
    /// Basename within the trash dir (`2026-05-06T14-22-31_myNote.md` or
    /// `2026-05-06T14-22-31_myFolder`). Combine with the trash dir to find
    /// the on-disk location.
    pub trashed_name: String,
    /// Unix seconds.
    pub original_mtime: i64,
    /// Unix seconds.
    pub deleted_at: i64,
    pub kind: Kind,
    /// For folders: vault-relative paths of `.md` files that were inside, in
    /// walk order. Used by future restore + as documentation of what got
    /// removed from the index. None for files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<String>>,
    /// The op-log `doc_id` this entry's note was tracked under, when known.
    /// A tracked note's Yrs history is retained keyed by `doc_id` rather than
    /// purged on delete (per `op-log.md`'s "Offline delete and rename"), so
    /// restore can rebind `path → doc_id` to recover full history instead of
    /// minting a fresh document. `None` for a hand-dropped file or a note that
    /// was never seeded into the op-log (its restore takes the fresh-import
    /// path). Folder entries leave this `None` — their members are rebound
    /// individually by path on re-ingest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
}

/// Disk-derived view of one trash entry. Built by joining a directory walk
/// of `<vault>/.hiker/trash/` against the manifest. `id` and
/// `original_path` are `None` for orphaned entries (file present on disk
/// but no manifest row); the UI uses `trashed_name` as the durable
/// identifier in that case.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItem {
    pub id: Option<String>,
    pub trashed_name: String,
    pub original_path: Option<String>,
    pub deleted_at: i64,
    pub kind: Kind,
    /// Member count for folders, when known. `None` means either "this is a
    /// file" (the UI infers from `kind`) or "we can't tell" (orphan folder).
    pub member_count: Option<usize>,
    pub orphaned: bool,
    /// The op-log `doc_id` recorded for this entry, when known. Carried so a
    /// history-preserving restore can rebind `path → doc_id`. `None` for files
    /// never seeded into the op-log, folder entries, and orphans.
    pub doc_id: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    #[serde(default)]
    entries: Vec<Entry>,
}

pub struct Trash {
    dir: PathBuf,
}

impl Trash {
    pub fn open(vault_root: &Path) -> Self {
        Self {
            dir: vault_root.join(".hiker").join(TRASH_DIRNAME),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_NAME)
    }

    fn ensure_dir(&self) -> Result<(), HikerError> {
        if !self.dir.exists() {
            fs::create_dir_all(&self.dir)?;
        }
        Ok(())
    }

    /// Move a single file from the vault into the trash. The caller is
    /// responsible for: watcher suppression, store cascade, and (after this
    /// returns) appending the returned entry via `append`. Returning the
    /// entry without writing it lets the caller bundle the manifest write
    /// with its store update.
    pub fn move_file_in(&self, vault_root: &Path, rel: &str) -> Result<Entry, HikerError> {
        self.ensure_dir()?;
        let src = vault_root.join(rel);
        let meta = fs::metadata(&src)?;
        if !meta.is_file() {
            return Err(HikerError::NotFound(format!("not a file: {rel}")));
        }
        let original_mtime = mtime_secs(&meta);
        let now = OffsetDateTime::now_utc();
        let basename = Path::new(rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.to_string());
        let trashed_name = pick_unique_name(&self.dir, &timestamp_prefix(now), &basename)?;
        let dest = self.dir.join(&trashed_name);
        fs::rename(&src, &dest)?;
        Ok(Entry {
            id: crate::store::dto::new_id(),
            original_path: rel.to_string(),
            trashed_name,
            original_mtime,
            deleted_at: now.unix_timestamp(),
            kind: Kind::File,
            members: None,
            doc_id: None,
        })
    }

    /// Create a trash entry for a file whose disk bytes are *already gone* —
    /// the offline-delete case (`op-log-startup-disk-reconcile`). The original
    /// `.md` vanished while hiker was closed, so the recoverable artifact is
    /// the document's last known content (`materialize(accepted)`), supplied by
    /// the caller, rather than an fs move. Mirrors [`move_file_in`]'s naming
    /// and manifest shape so restore treats it identically; only the byte
    /// source differs. The caller appends the returned entry via [`append`].
    ///
    /// status: op-log-startup-disk-reconcile
    pub fn capture_content_in(
        &self,
        rel: &str,
        content: &str,
        doc_id: Option<String>,
    ) -> Result<Entry, HikerError> {
        self.ensure_dir()?;
        let now = OffsetDateTime::now_utc();
        let basename = Path::new(rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.to_string());
        let trashed_name = pick_unique_name(&self.dir, &timestamp_prefix(now), &basename)?;
        let dest = self.dir.join(&trashed_name);
        fs::write(&dest, content.as_bytes())?;
        Ok(Entry {
            id: crate::store::dto::new_id(),
            original_path: rel.to_string(),
            trashed_name,
            // No surviving source file, so deletion-time is the best mtime we
            // can record for the artifact.
            original_mtime: now.unix_timestamp(),
            deleted_at: now.unix_timestamp(),
            kind: Kind::File,
            members: None,
            doc_id,
        })
    }

    /// Move a folder (recursively) into the trash. Walks the folder *before*
    /// moving to record `.md` member paths so the caller can clean up the
    /// index for those notes.
    pub fn move_folder_in(
        &self,
        vault_root: &Path,
        rel: &str,
    ) -> Result<Entry, HikerError> {
        self.ensure_dir()?;
        let src = vault_root.join(rel);
        let meta = fs::metadata(&src)?;
        if !meta.is_dir() {
            return Err(HikerError::NotFound(format!("not a directory: {rel}")));
        }
        let original_mtime = mtime_secs(&meta);
        let now = OffsetDateTime::now_utc();
        let basename = Path::new(rel)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| rel.to_string());
        let trashed_name = pick_unique_name(&self.dir, &timestamp_prefix(now), &basename)?;
        let dest = self.dir.join(&trashed_name);

        // Collect *.md members (vault-relative) before moving so the caller
        // can purge them from the index. walkdir without follow_links to
        // match walker-symlink-policy.
        let mut members: Vec<String> = Vec::new();
        for entry in walkdir::WalkDir::new(&src).follow_links(false) {
            let entry = entry.map_err(|e| HikerError::Io(e.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let is_md = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("md"))
                .unwrap_or(false);
            if !is_md {
                continue;
            }
            let rel_to_vault = path
                .strip_prefix(vault_root)
                .map_err(|e| HikerError::Io(format!("strip_prefix: {e}")))?;
            members.push(rel_to_vault.to_string_lossy().replace('\\', "/"));
        }

        fs::rename(&src, &dest)?;

        Ok(Entry {
            id: crate::store::dto::new_id(),
            original_path: rel.to_string(),
            trashed_name,
            original_mtime,
            deleted_at: now.unix_timestamp(),
            kind: Kind::Folder,
            members: Some(members),
            doc_id: None,
        })
    }

    /// Append an entry to the manifest. Atomic via tmp+rename so a crash
    /// mid-write can't truncate the existing list.
    pub fn append(&self, entry: &Entry) -> Result<(), HikerError> {
        self.ensure_dir()?;
        let mut manifest = self.read_manifest()?;
        manifest.entries.push(entry.clone());
        self.write_manifest(&manifest)
    }

    pub fn list(&self) -> Result<Vec<Entry>, HikerError> {
        Ok(self.read_manifest()?.entries)
    }

    pub fn find(&self, id: &str) -> Result<Option<Entry>, HikerError> {
        Ok(self.list()?.into_iter().find(|e| e.id == id))
    }

    /// Remove an entry from the manifest. Returns the removed entry, or None
    /// if no entry matched. Does not touch the on-disk trash files — callers
    /// are expected to either move the file out (restore) or delete it
    /// separately (empty would just blow away the whole dir).
    pub fn remove(&self, id: &str) -> Result<Option<Entry>, HikerError> {
        let mut manifest = self.read_manifest()?;
        let pos = manifest.entries.iter().position(|e| e.id == id);
        let Some(pos) = pos else { return Ok(None) };
        let removed = manifest.entries.remove(pos);
        self.write_manifest(&manifest)?;
        Ok(Some(removed))
    }

    /// Absolute on-disk path of a trashed entry.
    pub fn entry_path(&self, entry: &Entry) -> PathBuf {
        self.dir.join(&entry.trashed_name)
    }

    /// Walk the trash dir and produce one `ListItem` per top-level
    /// entry, joining against the manifest where a row is present and
    /// marking entries without one as orphaned. Disk is the source of truth
    /// — entries dropped in by hand or whose manifest row got corrupted
    /// still appear so the user can clean them up.
    ///
    /// status: tree-trash-disk-listing
    pub fn list_from_disk(&self) -> Result<Vec<ListItem>, HikerError> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let manifest = self.read_manifest()?;
        let by_name: std::collections::HashMap<&str, &Entry> = manifest
            .entries
            .iter()
            .map(|e| (e.trashed_name.as_str(), e))
            .collect();

        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Skip the manifest itself and any tmp leftover from atomic writes.
            if name == MANIFEST_NAME || name == "manifest.yaml.tmp" {
                continue;
            }
            let ft = entry.file_type()?;
            let kind = if ft.is_dir() {
                Kind::Folder
            } else if ft.is_file() {
                Kind::File
            } else {
                continue;
            };
            let item = match by_name.get(name.as_str()) {
                Some(m) => ListItem {
                    id: Some(m.id.clone()),
                    trashed_name: name,
                    original_path: Some(m.original_path.clone()),
                    deleted_at: m.deleted_at,
                    kind,
                    member_count: m.members.as_ref().map(std::vec::Vec::len),
                    orphaned: false,
                    doc_id: m.doc_id.clone(),
                },
                None => {
                    // Orphan — pull deletion time from fs metadata as a fallback.
                    let deleted_at = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    ListItem {
                        id: None,
                        trashed_name: name,
                        original_path: None,
                        deleted_at,
                        kind,
                        member_count: None,
                        orphaned: true,
                        doc_id: None,
                    }
                }
            };
            out.push(item);
        }
        // Newest first.
        out.sort_by_key(|t| std::cmp::Reverse(t.deleted_at));
        Ok(out)
    }

    /// Permanently delete one trash entry by its on-disk basename. Removes
    /// the file or folder from disk and drops the matching manifest row (if
    /// any). Used by the per-row "Delete permanently" action and works on
    /// orphaned entries (no manifest row) too.
    pub fn permanent_delete(&self, trashed_name: &str) -> Result<(), HikerError> {
        let path = self.dir.join(trashed_name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
            Ok(_) => fs::remove_file(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone on disk; still try to drop the manifest row.
            }
            Err(e) => return Err(e.into()),
        }
        let mut manifest = self.read_manifest()?;
        let before = manifest.entries.len();
        manifest.entries.retain(|e| e.trashed_name != trashed_name);
        if manifest.entries.len() != before {
            self.write_manifest(&manifest)?;
        }
        Ok(())
    }

    /// Permanently delete every trash entry. Removes the trash dir entirely
    /// (manifest included) and recreates it empty. The caller is expected to
    /// have already confirmed with the user — this method does not prompt.
    pub fn empty(&self) -> Result<(), HikerError> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir)?;
        }
        Ok(())
    }

    fn read_manifest(&self) -> Result<Manifest, HikerError> {
        let path = self.manifest_path();
        match fs::read(&path) {
            Ok(bytes) if bytes.is_empty() => Ok(Manifest::default()),
            Ok(bytes) => serde_yml::from_slice::<Manifest>(&bytes)
                .map_err(|e| HikerError::Io(format!("trash manifest parse: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn write_manifest(&self, manifest: &Manifest) -> Result<(), HikerError> {
        let yaml = serde_yml::to_string(manifest)
            .map_err(|e| HikerError::Io(format!("trash manifest serialize: {e}")))?;
        let path = self.manifest_path();
        let tmp = path.with_extension("yaml.tmp");
        fs::write(&tmp, yaml.as_bytes())?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// `2026-05-06T14-22-31` — colons replaced with dashes since some filesystems
/// (and Windows) reject `:` in filenames.
fn timestamp_prefix(now: OffsetDateTime) -> String {
    let fmt = format_description!(
        "[year]-[month]-[day]T[hour]-[minute]-[second]"
    );
    now.format(&fmt).unwrap_or_else(|_| now.unix_timestamp().to_string())
}

fn pick_unique_name(
    dir: &Path,
    ts_prefix: &str,
    basename: &str,
) -> Result<String, HikerError> {
    // First try `<ts>_<basename>`; on collision (same path deleted twice in
    // the same second) suffix `_2`, `_3`, ... before any extension.
    let first = format!("{ts_prefix}_{basename}");
    if !dir.join(&first).exists() {
        return Ok(first);
    }
    let (stem, ext) = match basename.rfind('.') {
        // Don't split a leading-dot dotfile like `.gitignore` — treat the
        // whole thing as the stem.
        Some(i) if i > 0 => (&basename[..i], Some(&basename[i + 1..])),
        _ => (basename, None),
    };
    for n in 2..1000 {
        let candidate = match ext {
            Some(e) => format!("{ts_prefix}_{stem}_{n}.{e}"),
            None => format!("{ts_prefix}_{stem}_{n}"),
        };
        if !dir.join(&candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(HikerError::AlreadyExists(format!(
        "trash: too many collisions for {basename}"
    )))
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn move_file_records_entry_and_relocates() {
        let vault = tempdir().unwrap();
        fs::write(vault.path().join("a.md"), b"hello").unwrap();
        let trash = Trash::open(vault.path());
        let entry = trash.move_file_in(vault.path(), "a.md").unwrap();
        assert_eq!(entry.original_path, "a.md");
        assert_eq!(entry.kind, Kind::File);
        assert!(entry.members.is_none());
        assert!(entry.trashed_name.ends_with("_a.md"));
        assert!(!vault.path().join("a.md").exists());
        assert!(vault.path().join(".hiker/trash").join(&entry.trashed_name).exists());
    }

    #[test]
    fn append_round_trips_through_list() {
        let vault = tempdir().unwrap();
        fs::write(vault.path().join("a.md"), b"x").unwrap();
        let trash = Trash::open(vault.path());
        let entry = trash.move_file_in(vault.path(), "a.md").unwrap();
        trash.append(&entry).unwrap();
        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, entry.id);
        assert_eq!(listed[0].original_path, "a.md");
        assert_eq!(trash.find(&entry.id).unwrap().unwrap().original_path, "a.md");
    }

    #[test]
    fn collision_in_same_second_gets_suffix() {
        let vault = tempdir().unwrap();
        let trash = Trash::open(vault.path());
        // Pre-seed an existing entry with the prefix that pick_unique_name
        // will compute, simulating a same-second second delete.
        trash.ensure_dir().unwrap();
        let now = OffsetDateTime::now_utc();
        let prefix = timestamp_prefix(now);
        fs::write(vault.path().join(".hiker/trash").join(format!("{prefix}_a.md")), b"first").unwrap();
        let name = pick_unique_name(&trash.dir, &prefix, "a.md").unwrap();
        assert_eq!(name, format!("{prefix}_a_2.md"));
    }

    #[test]
    fn move_folder_records_md_members_and_relocates() {
        let vault = tempdir().unwrap();
        fs::create_dir(vault.path().join("proj")).unwrap();
        fs::write(vault.path().join("proj/a.md"), b"a").unwrap();
        fs::create_dir(vault.path().join("proj/sub")).unwrap();
        fs::write(vault.path().join("proj/sub/b.md"), b"b").unwrap();
        // Non-md should be moved with the folder but not listed as a member.
        fs::write(vault.path().join("proj/sub/c.txt"), b"c").unwrap();
        let trash = Trash::open(vault.path());
        let entry = trash.move_folder_in(vault.path(), "proj").unwrap();
        assert_eq!(entry.kind, Kind::Folder);
        let mut members = entry.members.clone().unwrap();
        members.sort();
        assert_eq!(members, vec!["proj/a.md".to_string(), "proj/sub/b.md".to_string()]);
        assert!(!vault.path().join("proj").exists());
        let trash_root = vault.path().join(".hiker/trash").join(&entry.trashed_name);
        assert!(trash_root.join("a.md").exists());
        assert!(trash_root.join("sub/b.md").exists());
        assert!(trash_root.join("sub/c.txt").exists());
    }

    #[test]
    fn empty_removes_trash_dir() {
        let vault = tempdir().unwrap();
        fs::write(vault.path().join("a.md"), b"x").unwrap();
        let trash = Trash::open(vault.path());
        let entry = trash.move_file_in(vault.path(), "a.md").unwrap();
        trash.append(&entry).unwrap();
        trash.empty().unwrap();
        assert!(!trash.dir.exists());
        // After empty, list reports zero entries (auto-creates a fresh state).
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn list_from_disk_joins_manifest_and_marks_orphans() {
        let vault = tempdir().unwrap();
        fs::write(vault.path().join("a.md"), b"a").unwrap();
        fs::write(vault.path().join("b.md"), b"b").unwrap();
        let trash = Trash::open(vault.path());
        let e1 = trash.move_file_in(vault.path(), "a.md").unwrap();
        trash.append(&e1).unwrap();
        let e2 = trash.move_file_in(vault.path(), "b.md").unwrap();
        trash.append(&e2).unwrap();
        // Drop a hand-placed file to exercise the orphan branch.
        fs::write(trash.dir().join("orphan.md"), b"x").unwrap();

        let items = trash.list_from_disk().unwrap();
        assert_eq!(items.len(), 3);
        let by_name: std::collections::HashMap<&str, &ListItem> =
            items.iter().map(|i| (i.trashed_name.as_str(), i)).collect();
        let it1 = by_name[e1.trashed_name.as_str()];
        assert!(!it1.orphaned);
        assert_eq!(it1.original_path.as_deref(), Some("a.md"));
        let orphan = by_name["orphan.md"];
        assert!(orphan.orphaned);
        assert!(orphan.id.is_none());
        assert!(orphan.original_path.is_none());
    }

    #[test]
    fn permanent_delete_removes_disk_and_manifest() {
        let vault = tempdir().unwrap();
        fs::write(vault.path().join("a.md"), b"a").unwrap();
        let trash = Trash::open(vault.path());
        let e = trash.move_file_in(vault.path(), "a.md").unwrap();
        trash.append(&e).unwrap();
        assert!(trash.entry_path(&e).exists());

        trash.permanent_delete(&e.trashed_name).unwrap();
        assert!(!trash.entry_path(&e).exists());
        assert!(trash.list().unwrap().is_empty());
    }

    #[test]
    fn permanent_delete_works_on_orphan_with_no_manifest_row() {
        let vault = tempdir().unwrap();
        let trash = Trash::open(vault.path());
        trash.ensure_dir().unwrap();
        fs::write(trash.dir().join("orphan.md"), b"x").unwrap();
        trash.permanent_delete("orphan.md").unwrap();
        assert!(!trash.dir().join("orphan.md").exists());
    }

    #[test]
    fn split_ext_handles_dotfiles_and_extless() {
        fn split(name: &str) -> (&str, Option<&str>) {
            match name.rfind('.') {
                Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..])),
                _ => (name, None),
            }
        }
        assert_eq!(split("a.md"), ("a", Some("md")));
        assert_eq!(split("a"), ("a", None));
        assert_eq!(split(".gitignore"), (".gitignore", None));
        assert_eq!(split("a.tar.gz"), ("a.tar", Some("gz")));
    }
}
