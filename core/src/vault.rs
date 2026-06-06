#![allow(clippy::items_after_test_module)]

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::sections::TreeSortBy;
use crate::errors::HikerError;
use crate::hash_string;
use crate::store::Store;
use crate::trash::{Trash, Entry};
use crate::watcher::Watcher;

/// Stable per-vault identifier, stored at `<root>/.hiker/vault-id` (a ULID),
/// generated on first call when absent. It lives INSIDE the vault, so it
/// survives the vault directory being moved or renamed on disk — unlike a
/// path-derived id, which changes on every move. Used to key user-scope
/// per-vault state (the sync key store) so a moved vault keeps its identity
/// and its sync keys instead of silently regenerating them. [sync-vault-stable-id]
pub fn stable_id(root: &Path) -> std::io::Result<String> {
    let path = root.join(".hiker").join("vault-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let id = existing.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = ulid::Ulid::new().to_string();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &id)?;
    Ok(id)
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
    /// Filesystem mtime in unix seconds. Same field the watcher and indexer
    /// use; surfaced to the frontend so the tree can offer mtime-based
    /// sort orders without a second round trip.
    ///
    /// status: tree-sort-options
    pub mtime: i64,
}

#[derive(Clone)]
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
        let rel_path = Path::new(rel);
        for comp in rel_path.components() {
            use std::path::Component::*;
            if matches!(comp, RootDir | Prefix(_)) {
                return Err(HikerError::PathEscape(format!(
                    "expected vault-relative path, got absolute: {rel}"
                )));
            }
        }
        let candidate = self.root.join(rel_path);
        // Normalize by collapsing `..` / `.` components without touching
        // the disk. We deliberately don't `canonicalize` here — that would
        // follow symlinks, which is the very thing the next check rejects.
        let normalized = {
            let mut out = PathBuf::new();
            for comp in candidate.components() {
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
        };
        if !normalized.starts_with(&self.root) {
            return Err(HikerError::PathEscape(rel.to_string()));
        }
        // `starts_with` only checks logical components; a symlink anywhere
        // in the chain could still let fs::write follow it outside the
        // vault. Reject any path whose existing ancestors include a
        // symlink. Components that don't exist yet (typical for
        // create_note) are fine — we only check what's currently on disk.
        let mut current = PathBuf::new();
        for comp in normalized.components() {
            current.push(comp);
            // The vault root itself was canonicalized at Vault::open, so
            // it can't be a symlink. Skip checking ancestors above the
            // root for the same reason.
            if !current.starts_with(&self.root) || current == self.root {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(HikerError::PathEscape(format!(
                        "symlink in path: {}",
                        current.display()
                    )));
                }
                Ok(_) => {}
                // Component doesn't exist yet — fine, the rest of the
                // path can't exist either, so stop walking.
                Err(_) => break,
            }
        }
        Ok(normalized)
    }

    /// Resolve a vault-relative path to an absolute path on disk, applying
    /// the same path-escape and symlink-ancestor rejections that
    /// `read_file` / `write_file` use. Public so the host can hand
    /// absolute paths to OS commands (e.g. reveal-in-file-manager) without
    /// duplicating the validation logic.
    pub fn abs_path(&self, rel: &str) -> Result<PathBuf, HikerError> {
        self.resolve(rel)
    }

    /// List a directory's immediate children, pre-sorted to the configured
    /// order. Folders are always grouped before files (display invariant
    /// shared across UI / CLI / MCP); the chosen order applies *within*
    /// each group.
    pub fn list_dir(
        &self,
        rel: &str,
        sort_by: TreeSortBy,
    ) -> Result<Vec<DirEntryDto>, HikerError> {
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
            // Skip entries the watcher/indexer ignore (target/,
            // node_modules/, etc.). Without this, opening a project
            // root as a vault lets the user expand into a build tree
            // from the sidebar, which then caches a `DirEntryDto` per
            // file in `SidebarState.dir_cache` — easily millions of
            // entries on a Rust monorepo.
            if crate::watcher::is_ignored(&rel_path) {
                continue;
            }
            // mtime: best-effort. A failed metadata/system-time call is not a
            // reason to drop the row — fall back to 0 and let the frontend
            // sort it as the oldest entry.
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs() as i64)
                })
                .unwrap_or(0);
            out.push(DirEntryDto { name, rel_path, kind, mtime });
        }
        out.sort_by(|a, b| {
            use std::cmp::Ordering;
            match (&a.kind, &b.kind) {
                (EntryKind::Dir, EntryKind::File) => Ordering::Less,
                (EntryKind::File, EntryKind::Dir) => Ordering::Greater,
                _ => match sort_by {
                    TreeSortBy::NameAsc => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    TreeSortBy::NameDesc => b.name.to_lowercase().cmp(&a.name.to_lowercase()),
                    TreeSortBy::MtimeDesc => match b.mtime.cmp(&a.mtime) {
                        Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        o => o,
                    },
                    TreeSortBy::MtimeAsc => match a.mtime.cmp(&b.mtime) {
                        Ordering::Equal => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                        o => o,
                    },
                },
            }
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
        let hash = hash_string(&contents);
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
                let found = hash_string(&on_disk);
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
        Ok(hash_string(contents))
    }

    /// Walk a vault subtree for `.md` files and return their vault-relative
    /// paths. Used by the delete command to pre-suppress watcher events
    /// for every member before the folder rename, and by future restore /
    /// re-index flows that need the same enumeration. `follow_links(false)`
    /// matches `walker-symlink-policy`. Returns an empty vec if `rel` is a
    /// file (callers can branch on dir vs file via fs::metadata first).
    ///
    /// Filters via `crate::indexer::is_indexable_path` so the same allowlist
    /// drives bulk operations (folder-move pre-suppression, folder-delete
    /// member walk, store-side rename batching) that drives single-file
    /// ingest. Without this, a `.txt` file inside a moved/deleted folder
    /// would skip pre-suppression and the watcher could race a stale
    /// `Modified`/`Deleted` past the bulk path remap.
    pub fn walk_indexable_files(&self, rel: &str) -> Result<Vec<String>, HikerError> {
        let abs = self.resolve(rel)?;
        if !abs.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        // Prune subtrees the watcher already excludes. Without this, a
        // walk that crosses `target/` or `node_modules/` would visit
        // every file inside before the per-file `is_indexable_path`
        // filter rejects them — and each visit allocates a `String`
        // path. Pre-pruning by directory keeps the walker out of those
        // subtrees entirely.
        let walker = walkdir::WalkDir::new(&abs)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let path = e.path();
                let rel_to_vault = match path.strip_prefix(&self.root) {
                    Ok(r) => r,
                    Err(_) => return true,
                };
                let rel_str = rel_to_vault.to_string_lossy().replace('\\', "/");
                if rel_str.is_empty() {
                    return true; // root entry
                }
                !crate::watcher::is_ignored(&rel_str)
            });
        for entry in walker {
            let entry = entry.map_err(|e| HikerError::Io(e.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let rel_to_vault = path
                .strip_prefix(&self.root)
                .map_err(|e| HikerError::Io(format!("strip_prefix: {e}")))?;
            let rel_str = rel_to_vault.to_string_lossy().replace('\\', "/");
            if !crate::indexer::is_indexable_path(&rel_str) {
                continue;
            }
            out.push(rel_str);
        }
        Ok(out)
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

/// A note's *companion folder* path (`note-companion-folder` in
/// `files.md`): a note at `<dir>/<name>.md` pairs with a sibling folder
/// `<dir>/<name>/` that physically holds its child notes (trail
/// waypoints, crawl/feed captures). Returns the vault-relative folder
/// path (no trailing slash) for any `.md` path; `None` for non-`.md`
/// paths, which never own a companion folder.
///
/// This computes the path only — it does NOT imply the folder exists on
/// disk (creation is lazy, on first child write) and does NOT define
/// nesting authority (that's `hiker.parent` / the trail waypoint tree,
/// not folder membership). The folder is just the physical home.
///
/// status: note-companion-folder
#[must_use]
pub fn companion_folder_for(rel: &str) -> Option<String> {
    let stem = rel.strip_suffix(".md")?;
    if stem.is_empty() || stem.ends_with('/') {
        return None;
    }
    Some(stem.to_string())
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
/// **Companion-folder pairing** (`note-companion-folder`): when the moved
/// note owns a companion folder on disk (`<dir>/<name>/` beside
/// `<dir>/<name>.md`), that folder is fs-renamed to the destination's
/// companion folder (`<dir2>/<new>/`) in the same op and every contained
/// indexed note's store path is bulk-remapped. The list of contained
/// `(old, new)` member pairs is returned so the caller (the indexer's
/// `IndexJob::Move` handler) can run the shared reference-rewrite pass
/// (`wikilink-rename-rewrite`) over each moved child — rewriting a moved
/// trail-doc's waypoint `in_trail` paths and `hiker.waypoints[].path`
/// entries. A note with no companion folder returns an empty vec.
///
/// Folder-level moves of arbitrary directories are still out of scope
/// here — `move_folder` handles those; this only pairs the *companion*
/// folder a note owns.
///
/// `watcher` is optional because the CLI runs without one. When present,
/// both `from` and `to` (plus every companion-folder member at its old
/// and new path) get suppressed before the rename so any platform-specific
/// ordering of the resulting events is filtered.
///
/// status: move-note-core-cmd
/// status: note-companion-folder
pub fn move_note(
    vault: &Vault,
    store: &mut Store,
    watcher: Option<&Watcher>,
    from: &str,
    to: &str,
) -> Result<Vec<(String, String)>, HikerError> {
    if from == to {
        return Ok(Vec::new());
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

    // status: note-companion-folder
    // Detect a companion folder beside the moved note and compute the
    // (old, new) member pairs *before* the rename so we can pre-suppress
    // and bulk-remap the store after the fs move.
    let companion = companion_folder_for(from)
        .zip(companion_folder_for(to))
        .filter(|(from_dir, _)| {
            vault
                .resolve(from_dir)
                .map(|p| p.is_dir())
                .unwrap_or(false)
        });
    let companion_members: Vec<(String, String)> = match &companion {
        Some((from_dir, to_dir)) => {
            let members = vault.walk_indexable_files(from_dir).unwrap_or_default();
            let from_prefix = format!("{from_dir}/");
            members
                .iter()
                .map(|m| {
                    let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
                    (m.clone(), format!("{to_dir}/{suffix}"))
                })
                .collect()
        }
        None => Vec::new(),
    };

    if let Some(w) = watcher {
        w.suppress(from);
        w.suppress(to);
        if let Some((from_dir, to_dir)) = &companion {
            w.suppress(from_dir.clone());
            w.suppress(to_dir.clone());
        }
        for (old, new) in &companion_members {
            w.suppress(old.clone());
            w.suppress(new.clone());
        }
    }

    fs::rename(&from_abs, &to_abs)?;

    // Move the companion folder alongside the note in the same op. A
    // failure here rolls the note rename back so the pair never desyncs.
    if let Some((from_dir, to_dir)) = &companion {
        let from_dir_abs = vault.resolve(from_dir)?;
        let to_dir_abs = vault.resolve(to_dir)?;
        if let Err(e) = fs::rename(&from_dir_abs, &to_dir_abs) {
            let _ = fs::rename(&to_abs, &from_abs);
            return Err(HikerError::Io(format!(
                "move companion folder {from_dir} -> {to_dir}: {e}"
            )));
        }
    }

    // Re-register suppression so the TTL window starts close to when notify
    // surfaces its events (post-rename + debounce), not at function entry.
    if let Some(w) = watcher {
        w.suppress(from);
        w.suppress(to);
        if let Some((from_dir, to_dir)) = &companion {
            w.suppress(from_dir.clone());
            w.suppress(to_dir.clone());
        }
        for (old, new) in &companion_members {
            w.suppress(old.clone());
            w.suppress(new.clone());
        }
    }

    // Index update. If the source isn't in the index (e.g. a non-md file or
    // an md file not yet ingested), there's nothing to do — the fs rename
    // alone is the whole operation. Any store error after a successful
    // rename gets a best-effort fs rollback so we don't leave the index and
    // disk disagreeing.
    // status: store-id-from-oplog
    // No `id_for_path` indirection — the indexer rename targets the row
    // directly by its old path. Non-indexed `from` is a silent no-op
    // (`rename_note_by_path` returns false).
    if let Err(e) = store.rename_note_by_path(from, to) {
        rollback_companion(vault, &companion, &to_abs, &from_abs);
        return Err(HikerError::Io(e.to_string()));
    }
    // Bulk-remap every companion-folder member's store path. A failure
    // rolls back both the folder and the note rename.
    if !companion_members.is_empty()
        && let Err(e) = store.rename_notes_by_paths(&companion_members)
    {
        let _ = store.rename_note_by_path(to, from);
        rollback_companion(vault, &companion, &to_abs, &from_abs);
        return Err(HikerError::Io(e.to_string()));
    }
    Ok(companion_members)
}

/// Best-effort fs rollback of a companion-folder move + the note rename,
/// used when a store remap fails after the fs renames committed.
fn rollback_companion(
    vault: &Vault,
    companion: &Option<(String, String)>,
    to_abs: &Path,
    from_abs: &Path,
) {
    if let Some((from_dir, to_dir)) = companion
        && let (Ok(from_dir_abs), Ok(to_dir_abs)) =
            (vault.resolve(from_dir), vault.resolve(to_dir))
    {
        let _ = fs::rename(&to_dir_abs, &from_dir_abs);
    }
    let _ = fs::rename(to_abs, from_abs);
}

/// Atomic folder rename: fs rename of the whole folder + bulk index path
/// update for every contained `.md` file in a single store transaction. Empty
/// subfolders move with the rename for free (it's a single fs::rename).
/// Errors leave both sides untouched (or rolled back if the index update
/// fails after the fs rename succeeded). Behaviors per editor.md "API & edge
/// cases":
///
/// - target collision → `AlreadyExists`, source untouched
/// - source missing or not a directory → `NotFound`
/// - target parent missing → `NotFound`
/// - non-indexed `.md` files inside (never ingested) and non-md files → fs
///   rename only, store skips them
///
/// Watcher suppression covers the folder root, every contained `.md` member
/// at its old path, and every member at its new path so cross-platform
/// notify ordering can't surface a stale Created/Deleted pair.
pub fn move_folder(
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
    let meta = match fs::symlink_metadata(&from_abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HikerError::NotFound(from.to_string()));
        }
        Err(e) => return Err(e.into()),
    };
    if !meta.is_dir() {
        return Err(HikerError::NotFound(format!("not a directory: {from}")));
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
    // Don't allow moving a folder into itself or a descendant.
    if to == from || to.starts_with(&format!("{from}/")) {
        return Err(HikerError::PathEscape(format!(
            "cannot move {from} into its own subtree at {to}"
        )));
    }

    // Collect indexed-eligible members before the rename so we can build the
    // (old, new) path pairs the store needs.
    let members = vault.walk_indexable_files(from)?;
    let from_prefix = format!("{from}/");
    let renames: Vec<(String, String)> = members
        .iter()
        .map(|m| {
            let suffix = m.strip_prefix(&from_prefix).unwrap_or(m);
            (m.clone(), format!("{to}/{suffix}"))
        })
        .collect();

    if let Some(w) = watcher {
        w.suppress(from);
        w.suppress(to);
        for (old, new) in &renames {
            w.suppress(old.clone());
            w.suppress(new.clone());
        }
    }

    fs::rename(&from_abs, &to_abs)?;

    // Re-register suppression so the TTL window starts close to when notify
    // surfaces its events (post-rename + debounce).
    if let Some(w) = watcher {
        w.suppress(from);
        w.suppress(to);
        for (old, new) in &renames {
            w.suppress(old.clone());
            w.suppress(new.clone());
        }
    }

    if let Err(e) = store.rename_notes_by_paths(&renames) {
        let _ = fs::rename(&to_abs, &from_abs);
        return Err(HikerError::Io(e.to_string()));
    }
    Ok(())
}

/// Soft-delete a note (or folder of notes). Moves the source into the vault
/// trash, removes any matching entries from the index, and appends a manifest
/// entry recording the original path so a future `vault-trash-restore` can
/// put it back. Watcher is suppressed around the move so the resulting
/// Deleted event doesn't trigger a redundant index purge.
///
/// Behaviors per editor.md "Delete semantics":
///
/// - file source → moves single file into trash, drops its index entry
/// - folder source → moves folder into a timestamped trash root preserving
///   relative structure; drops every contained `.md` file from the index in
///   a single transaction
/// - source missing → `NotFound`, nothing touched
/// - non-md source → fs move only, no index work
/// - trash dir missing → auto-created
///
/// On store failure after the fs move succeeds, attempts a best-effort
/// rollback (rename trash entry back to its original path) so disk and index
/// don't disagree. The same pattern `move_note` uses.
///
/// Returns the manifest entry so the caller can drive an undo toast or CLI
/// confirmation without a second roundtrip.
///
/// status: delete-note-core-cmd
pub fn delete_note(
    vault: &Vault,
    store: &mut Store,
    watcher: Option<&Watcher>,
    trash: &Trash,
    rel: &str,
    doc_id: Option<String>,
) -> Result<Entry, HikerError> {
    let abs = vault.resolve(rel)?;
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(HikerError::NotFound(rel.to_string()));
        }
        Err(e) => return Err(e.into()),
    };

    if meta.is_dir() {
        // Folder soft-delete: move the whole subtree to trash, then
        // batch-drop every `.md` member from the index. Rollback on
        // store failure restores the folder verbatim.
        if let Some(w) = watcher {
            w.suppress(rel);
        }
        let entry = trash.move_folder_in(vault.root(), rel)?;
        if let Some(w) = watcher {
            w.suppress(rel);
            if let Some(members) = &entry.members {
                for m in members {
                    w.suppress(m.clone());
                }
            }
        }
        let members = entry.members.clone().unwrap_or_default();
        if let Err(e) = store.delete_notes_by_paths(&members) {
            // Best-effort rollback: rename the folder back to its
            // original path so disk and index don't disagree.
            let from = trash.dir().join(&entry.trashed_name);
            let to = vault.root().join(&entry.original_path);
            let _ = fs::rename(from, to);
            return Err(HikerError::Io(e.to_string()));
        }
        trash.append(&entry)?;
        Ok(entry)
    } else if meta.is_file() {
        // Single-file soft-delete: move the file to trash, drop its
        // matching index row (if any), then append a manifest entry so
        // it shows up in the Trash panel and is `restore_note`-able.
        if let Some(w) = watcher {
            w.suppress(rel);
        }
        let mut entry = trash.move_file_in(vault.root(), rel)?;
        // Record the op-log doc_id (when the note was tracked) so a later
        // restore can rebind `path → doc_id` and recover the doc's retained
        // Yrs history rather than minting a fresh import. status: vault-trash-restore
        entry.doc_id = doc_id;
        if let Some(w) = watcher {
            // Re-suppress so the TTL window starts close to when notify
            // surfaces its events post-rename.
            w.suppress(rel);
        }
        // Index cleanup. Non-indexed files (e.g. `.md` files we haven't
        // ingested yet, or non-md files) just have nothing to remove.
        // status: store-id-from-oplog
        if let Err(e) = store.delete_note_by_path(rel) {
            let _ = rollback_file(vault, trash, &entry);
            return Err(HikerError::Io(e.to_string()));
        }
        // Manifest write failed after the file is already in trash and
        // the index is updated. Rolling back the index would require
        // re-ingest; leaving the file in trash without a manifest entry
        // leaves it unrestorable. Surface the error — the file is still
        // recoverable by hand from `.hiker/trash/`.
        trash.append(&entry)?;
        Ok(entry)
    } else {
        // Symlinks, fifos, etc. — vault::resolve already rejects symlink
        // ancestors, so this should be unreachable in practice.
        Err(HikerError::NotFound(format!("unsupported file type: {rel}")))
    }
}

/// Restore a previously soft-deleted note (or folder) from the vault trash.
/// fs-only: this moves the entry back to its original path and removes it
/// from the trash manifest. Re-ingestion (so the index picks the restored
/// notes back up) is the indexer task's job — see `IndexJob::RestoreFromTrash`.
///
/// Behaviors per editor.md "Restore":
///
/// - id not in manifest → `NotFound`
/// - original path now occupied → `AlreadyExists`, manifest untouched
/// - parent of original path missing → re-created (`fs::create_dir_all`); a
///   restore is allowed to bring its containing folder back if the user
///   deleted it after deleting the file
///
/// Returns the manifest entry that was just restored so the caller can drive
/// re-ingest using `entry.members` for folders or the original path for files.
///
/// status: vault-trash-restore
pub fn restore_note(
    vault: &Vault,
    watcher: Option<&Watcher>,
    trash: &Trash,
    id: &str,
) -> Result<Entry, HikerError> {
    let entry = trash
        .find(id)?
        .ok_or_else(|| HikerError::NotFound(format!("trash entry: {id}")))?;

    let dest = vault.resolve(&entry.original_path)?;
    if dest.exists() {
        return Err(HikerError::AlreadyExists(entry.original_path.clone()));
    }
    if let Some(parent) = dest.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    if let Some(w) = watcher {
        w.suppress(entry.original_path.clone());
        if let Some(members) = &entry.members {
            for m in members {
                w.suppress(m.clone());
            }
        }
    }

    let src = trash.entry_path(&entry);
    fs::rename(&src, &dest)?;

    if let Some(w) = watcher {
        w.suppress(entry.original_path.clone());
        if let Some(members) = &entry.members {
            for m in members {
                w.suppress(m.clone());
            }
        }
    }

    // Drop the manifest entry only after the fs move has succeeded; on
    // failure the entry stays so the user can retry restore.
    if let Err(e) = trash.remove(id) {
        // Best-effort rollback: put the file back in trash so manifest +
        // disk stay consistent.
        let _ = fs::rename(&dest, &src);
        return Err(e);
    }

    Ok(entry)
}

fn rollback_file(vault: &Vault, trash: &Trash, entry: &Entry) -> Result<(), HikerError> {
    let from = trash.dir().join(&entry.trashed_name);
    let to = vault.root().join(&entry.original_path);
    fs::rename(from, to)?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::dto::{new_id, NoteUpsert};
use crate::store::Store;
    use crate::test_helpers;
    use tempfile::tempdir;

    #[test]
    fn create_note_writes_empty_file_and_returns_path() {
        let (dir, vault) = test_helpers::test_vault();
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
        let (_dir, vault) = test_helpers::test_vault();
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
            .upsert_note(&NoteUpsert {
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
        let (dir, vault) = test_helpers::test_vault();
        let mut store = Store::open(dir.path()).unwrap();
        assert!(matches!(
            move_note(&vault, &mut store, None, "nope.md", "x.md"),
            Err(HikerError::NotFound(_))
        ));
    }

    fn upsert_stub(store: &mut Store, path: &str) -> String {
        let id = new_id();
        store
            .upsert_note(&NoteUpsert {
                id: &id,
                path,
                content_hash: "h",
                mtime: 0,
                size: 0,
                indexed_at: 0,
                embedder_version: "mock",
                chunks: Vec::new(),
            })
            .unwrap();
        id
    }

    // status: note-companion-folder
    #[test]
    fn companion_folder_path_computation() {
        assert_eq!(
            companion_folder_for("trails/my-trail.md").as_deref(),
            Some("trails/my-trail")
        );
        assert_eq!(
            companion_folder_for("note.md").as_deref(),
            Some("note")
        );
        // Non-.md paths never own a companion folder.
        assert_eq!(companion_folder_for("note.txt"), None);
        assert_eq!(companion_folder_for("dir/"), None);
        // A bare ".md" has no name half.
        assert_eq!(companion_folder_for(".md"), None);
    }

    // status: note-companion-folder
    #[test]
    fn move_note_moves_companion_folder_and_returns_members() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("trails")).unwrap();
        fs::write(dir.path().join("trails/t.md"), b"trail").unwrap();
        // Companion folder beside the note, holding a child note.
        fs::create_dir(dir.path().join("trails/t")).unwrap();
        fs::write(dir.path().join("trails/t/child--AAAAAA.md"), b"child").unwrap();

        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "trails/t.md");
        upsert_stub(&mut store, "trails/t/child--AAAAAA.md");

        let members =
            move_note(&vault, &mut store, None, "trails/t.md", "trails/renamed.md").unwrap();

        // Note + its companion folder both moved.
        assert!(!dir.path().join("trails/t.md").exists());
        assert!(!dir.path().join("trails/t").exists());
        assert!(dir.path().join("trails/renamed.md").exists());
        assert!(dir.path().join("trails/renamed/child--AAAAAA.md").exists());

        // Store paths remapped for both the note and the child.
        assert!(store.get_note_by_path("trails/t.md").unwrap().is_none());
        assert!(store.get_note_by_path("trails/renamed.md").unwrap().is_some());
        assert!(store
            .get_note_by_path("trails/t/child--AAAAAA.md")
            .unwrap()
            .is_none());
        assert!(store
            .get_note_by_path("trails/renamed/child--AAAAAA.md")
            .unwrap()
            .is_some());

        // The returned member pairs let the caller rewrite child references.
        assert_eq!(
            members,
            vec![(
                "trails/t/child--AAAAAA.md".to_string(),
                "trails/renamed/child--AAAAAA.md".to_string()
            )]
        );
    }

    // status: note-companion-folder
    #[test]
    fn move_note_without_companion_folder_returns_empty() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"a").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "a.md");
        let members = move_note(&vault, &mut store, None, "a.md", "b.md").unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn move_folder_renames_dir_and_remaps_indexed_members() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("proj")).unwrap();
        fs::write(dir.path().join("proj/a.md"), b"a").unwrap();
        fs::create_dir(dir.path().join("proj/sub")).unwrap();
        fs::write(dir.path().join("proj/sub/b.md"), b"b").unwrap();
        // c.md exists on disk but isn't in the index — rename should still
        // move the file (via fs::rename of the parent) and the store call
        // simply skips it.
        fs::write(dir.path().join("proj/sub/c.md"), b"c").unwrap();
        // An empty subfolder should travel with the rename.
        fs::create_dir(dir.path().join("proj/empty")).unwrap();

        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "proj/a.md");
        upsert_stub(&mut store, "proj/sub/b.md");

        move_folder(&vault, &mut store, None, "proj", "renamed").unwrap();

        assert!(!dir.path().join("proj").exists());
        assert!(dir.path().join("renamed/a.md").exists());
        assert!(dir.path().join("renamed/sub/b.md").exists());
        assert!(dir.path().join("renamed/sub/c.md").exists());
        assert!(dir.path().join("renamed/empty").is_dir());

        assert!(store.get_note_by_path("proj/a.md").unwrap().is_none());
        assert!(store.get_note_by_path("proj/sub/b.md").unwrap().is_none());
        assert!(store.get_note_by_path("renamed/a.md").unwrap().is_some());
        assert!(store.get_note_by_path("renamed/sub/b.md").unwrap().is_some());
    }

    #[test]
    fn move_folder_into_subfolder_collision_errors() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::create_dir(dir.path().join("b")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        match move_folder(&vault, &mut store, None, "a", "b") {
            Err(HikerError::AlreadyExists(_)) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        // Source untouched.
        assert!(dir.path().join("a").exists());
    }

    #[test]
    fn move_folder_into_own_subtree_errors() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        match move_folder(&vault, &mut store, None, "a", "a/sub") {
            Err(HikerError::PathEscape(_)) => {}
            other => panic!("expected PathEscape, got {other:?}"),
        }
        assert!(dir.path().join("a").exists());
    }

    #[test]
    fn move_folder_source_missing_errors() {
        let (dir, vault) = test_helpers::test_vault();
        let mut store = Store::open(dir.path()).unwrap();
        match move_folder(&vault, &mut store, None, "ghost", "x") {
            Err(HikerError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn move_folder_source_is_file_errors() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("file.md"), b"x").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        match move_folder(&vault, &mut store, None, "file.md", "renamed.md") {
            Err(HikerError::NotFound(msg)) => assert!(msg.contains("not a directory")),
            other => panic!("expected NotFound(not a directory), got {other:?}"),
        }
    }

    #[test]
    fn delete_note_moves_file_to_trash_and_drops_index() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"hi").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "a.md");
        let trash = crate::trash::Trash::open(vault.root());

        let entry = delete_note(&vault, &mut store, None, &trash, "a.md", None).unwrap();

        assert!(!dir.path().join("a.md").exists());
        assert!(dir.path().join(".hiker/trash").join(&entry.trashed_name).exists());
        assert!(store.get_note_by_path("a.md").unwrap().is_none());
        assert!(!store.note_exists("a.md").unwrap());

        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original_path, "a.md");
    }

    #[test]
    fn delete_note_unindexed_file_still_moves_to_trash() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("note.txt"), b"x").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let trash = crate::trash::Trash::open(vault.root());

        let entry = delete_note(&vault, &mut store, None, &trash, "note.txt", None).unwrap();

        assert!(!dir.path().join("note.txt").exists());
        assert!(dir.path().join(".hiker/trash").join(&entry.trashed_name).exists());
        assert_eq!(trash.list().unwrap().len(), 1);
    }

    #[test]
    fn delete_note_source_missing_errors() {
        let (dir, vault) = test_helpers::test_vault();
        let mut store = Store::open(dir.path()).unwrap();
        let trash = crate::trash::Trash::open(vault.root());
        match delete_note(&vault, &mut store, None, &trash, "ghost.md", None) {
            Err(HikerError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn delete_note_folder_recurses_and_purges_indexed_members() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("proj")).unwrap();
        fs::write(dir.path().join("proj/a.md"), b"a").unwrap();
        fs::create_dir(dir.path().join("proj/sub")).unwrap();
        fs::write(dir.path().join("proj/sub/b.md"), b"b").unwrap();
        // c.md is on disk but never indexed — fs move handles it, store cleanup skips it.
        fs::write(dir.path().join("proj/sub/c.md"), b"c").unwrap();

        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "proj/a.md");
        upsert_stub(&mut store, "proj/sub/b.md");
        let trash = crate::trash::Trash::open(vault.root());

        let entry = delete_note(&vault, &mut store, None, &trash, "proj", None).unwrap();

        assert!(!dir.path().join("proj").exists());
        let trash_root = dir.path().join(".hiker/trash").join(&entry.trashed_name);
        assert!(trash_root.join("a.md").exists());
        assert!(trash_root.join("sub/b.md").exists());
        assert!(trash_root.join("sub/c.md").exists());
        assert!(store.get_note_by_path("proj/a.md").unwrap().is_none());
        assert!(store.get_note_by_path("proj/sub/b.md").unwrap().is_none());

        let listed = trash.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].original_path, "proj");
        let mut members = listed[0].members.clone().unwrap();
        members.sort();
        assert_eq!(members, vec![
            "proj/a.md".to_string(),
            "proj/sub/b.md".to_string(),
            "proj/sub/c.md".to_string(),
        ]);
    }

    #[test]
    fn restore_note_round_trips_a_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"hi").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let trash = crate::trash::Trash::open(vault.root());
        let deleted = delete_note(&vault, &mut store, None, &trash, "a.md", None).unwrap();

        let restored = restore_note(&vault, None, &trash, &deleted.id).unwrap();
        assert_eq!(restored.original_path, "a.md");
        assert!(dir.path().join("a.md").exists());
        // Manifest entry is gone.
        assert!(trash.find(&deleted.id).unwrap().is_none());
        // Trashed file no longer present on disk.
        assert!(!trash.entry_path(&deleted).exists());
    }

    #[test]
    fn restore_note_errors_when_original_path_occupied() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), b"hi").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let trash = crate::trash::Trash::open(vault.root());
        let deleted = delete_note(&vault, &mut store, None, &trash, "a.md", None).unwrap();
        // User created a new file at the same path before restoring.
        fs::write(dir.path().join("a.md"), b"new").unwrap();
        match restore_note(&vault, None, &trash, &deleted.id) {
            Err(HikerError::AlreadyExists(_)) => {}
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        // Manifest entry preserved so the user can resolve the conflict.
        assert!(trash.find(&deleted.id).unwrap().is_some());
    }

    #[test]
    fn restore_note_recreates_missing_parent() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/a.md"), b"hi").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        let trash = crate::trash::Trash::open(vault.root());
        let deleted = delete_note(&vault, &mut store, None, &trash, "sub/a.md", None).unwrap();
        // User removed the empty parent folder after deleting.
        fs::remove_dir(dir.path().join("sub")).unwrap();
        let restored = restore_note(&vault, None, &trash, &deleted.id).unwrap();
        assert_eq!(restored.original_path, "sub/a.md");
        assert!(dir.path().join("sub/a.md").exists());
    }

    #[test]
    fn restore_note_brings_folder_back_with_members() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("proj")).unwrap();
        fs::write(dir.path().join("proj/a.md"), b"a").unwrap();
        fs::create_dir(dir.path().join("proj/sub")).unwrap();
        fs::write(dir.path().join("proj/sub/b.md"), b"b").unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let mut store = Store::open(dir.path()).unwrap();
        upsert_stub(&mut store, "proj/a.md");
        upsert_stub(&mut store, "proj/sub/b.md");
        let trash = crate::trash::Trash::open(vault.root());
        let deleted = delete_note(&vault, &mut store, None, &trash, "proj", None).unwrap();

        let restored = restore_note(&vault, None, &trash, &deleted.id).unwrap();
        assert_eq!(restored.kind, crate::trash::Kind::Folder);
        assert!(dir.path().join("proj/a.md").exists());
        assert!(dir.path().join("proj/sub/b.md").exists());
        assert!(trash.find(&deleted.id).unwrap().is_none());
    }

    #[test]
    fn restore_note_unknown_id_errors() {
        let (_dir, vault) = test_helpers::test_vault();
        let trash = crate::trash::Trash::open(vault.root());
        match restore_note(&vault, None, &trash, "no-such-id") {
            Err(HikerError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn resolve_rejects_absolute_path() {
        let (_dir, vault) = test_helpers::test_vault();
        let err = vault.resolve("/etc/passwd").unwrap_err();
        match err {
            HikerError::PathEscape(msg) => assert!(msg.contains("absolute")),
            other => panic!("expected PathEscape with absolute hint, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlinked_file() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"shh").unwrap();
        symlink(outside.path().join("secret.txt"), dir.path().join("link.md")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        match vault.resolve("link.md") {
            Err(HikerError::PathEscape(msg)) => assert!(msg.contains("symlink")),
            other => panic!("expected PathEscape for symlink, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlinked_directory_ancestor() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(outside.path().join("d")).unwrap();
        fs::write(outside.path().join("d/note.md"), b"x").unwrap();
        symlink(outside.path().join("d"), dir.path().join("d")).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        match vault.resolve("d/note.md") {
            Err(HikerError::PathEscape(msg)) => assert!(msg.contains("symlink")),
            other => panic!("expected PathEscape for symlinked ancestor, got {other:?}"),
        }
    }
}

