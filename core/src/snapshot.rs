//! Plain-file note snapshots — the local version-history mechanism. Each
//! snapshot is a whole `.md` file (no deltas, no codec,
//! `cat`-able) written under `<vault>/.hiker/history/<rel-path>/<ts>.md` on
//! every save. Snapshots are a DISPOSABLE cache: `rm -rf .hiker/history` loses
//! only local version history, nothing canonical — the canonical content lives
//! in the note itself (+ git when integrated).
//!
//! This module is deliberately config-free: callers pass a [`RetentionPolicy`]
//! explicitly so the snapshot machinery has no dependency on `core::config`
//! (avoiding a cycle through the layered doc) and stays trivially testable. The
//! `[history]` config section maps onto `RetentionPolicy` at the wiring layer.
//!
//! Snapshots are written ALWAYS, independent of `[git]` — they are cheap,
//! uniform local history that exists whether or not the vault is a git repo.
//!
//! status: plain-file-snapshots

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Subdirectory under `.hiker/` that holds the snapshot tree. One child
/// directory per snapshotted note (mirroring the note's vault-relative path),
/// each holding `<timestamp_ms>.md` whole-file snapshots.
const HISTORY_DIR: &str = "history";

/// Default cap on the number of retained snapshots per note.
pub const DEFAULT_MAX_SNAPSHOTS: u32 = 50;
/// Default age (in days) past which a snapshot is pruned.
pub const DEFAULT_MAX_AGE_DAYS: u32 = 30;

/// Retention policy for a note's snapshot set: keep at most `max_snapshots`
/// (newest wins) AND drop anything older than `max_age_days`. A value of `0`
/// on either knob disables that dimension of pruning (keep-unbounded /
/// never-age-out), matching the "0 = unbounded" convention used elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_snapshots: u32,
    pub max_age_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
        }
    }
}

/// One snapshot in a note's history: the millisecond timestamp encoded in its
/// filename and the absolute path to the whole-file `.md` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Milliseconds since the unix epoch, parsed from the `<ts>.md` filename.
    /// Lexically-sortable filenames mean newest-first ordering is a reverse
    /// sort on this field.
    pub timestamp_ms: u64,
    /// Absolute path to the snapshot `.md` on disk.
    pub path: PathBuf,
}

/// Absolute path to a note's snapshot directory:
/// `<vault>/.hiker/history/<rel_path>/`. The note's vault-relative path is
/// used verbatim as a subtree so the layout is human-navigable and a rename
/// is a single directory move (see [`move_snapshots`]).
fn snapshot_dir(vault: &Path, rel_path: &str) -> PathBuf {
    vault.join(".hiker").join(HISTORY_DIR).join(rel_path)
}

/// Current wall-clock time as milliseconds since the unix epoch. A clock set
/// before the epoch yields `0` rather than panicking — snapshots are a
/// best-effort cache, never worth aborting a save over.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse the `<timestamp_ms>` out of a snapshot filename (`<ts>.md`). Returns
/// `None` for anything that isn't a bare millisecond-timestamp `.md` file, so
/// stray files dropped into the directory are ignored rather than mis-sorted.
fn timestamp_of(path: &Path) -> Option<u64> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".md"))
        .and_then(|stem| stem.parse::<u64>().ok())
}

/// Write a new whole-file snapshot for `rel_path`, then prune per `policy`.
///
/// Idempotent-ish: if the newest existing snapshot is byte-identical to
/// `content`, no new snapshot is written (a no-op save must not churn the
/// history). Returns `true` when a snapshot was written, `false` on the
/// identical-content skip.
///
/// The write itself is plain (not atomic-renamed): a snapshot is disposable
/// cache, and a torn snapshot from a crash mid-write is simply a junk file the
/// next prune or a manual `rm` discards — there is no canonical data at risk.
///
/// status: plain-file-snapshots
pub fn snapshot(
    vault: &Path,
    rel_path: &str,
    content: &str,
    policy: RetentionPolicy,
) -> std::io::Result<bool> {
    // Skip the write when the newest snapshot already holds these exact bytes.
    if let Some(newest) = list_snapshots(vault, rel_path)?.first()
        && let Ok(existing) = fs::read_to_string(&newest.path)
        && existing == content
    {
        return Ok(false);
    }

    let dir = snapshot_dir(vault, rel_path);
    fs::create_dir_all(&dir)?;
    // Millisecond timestamps are filesystem-safe and lexically sort in
    // chronological order (fixed-width within any realistic span). On the rare
    // collision (two saves within the same millisecond) we bump until free so a
    // snapshot is never silently overwritten.
    //
    // Enforce MONOTONICITY against the newest existing snapshot: the dedup
    // (`list_snapshots().first()`) and "restore previous" (index 1) both key off
    // descending timestamp, so a newly-written snapshot MUST sort newest. If the
    // wall clock has moved backward (NTP step / VM resume) the computed `ts` can
    // be <= an existing snapshot's; clamp to `newest + 1` so the freshest content
    // always sorts first regardless of clock direction.
    let mut ts = now_ms();
    if let Some(newest) = list_snapshots(vault, rel_path)?.first()
        && ts <= newest.timestamp_ms
    {
        ts = newest.timestamp_ms + 1;
    }
    let mut file = dir.join(format!("{ts}.md"));
    while file.exists() {
        ts += 1;
        file = dir.join(format!("{ts}.md"));
    }
    fs::write(&file, content.as_bytes())?;

    prune(vault, rel_path, policy)?;
    Ok(true)
}

/// List a note's snapshots newest-first (descending timestamp). A missing
/// snapshot directory is not an error — a note simply has no history yet, so
/// this returns an empty vec.
///
/// status: plain-file-snapshots
pub fn list_snapshots(vault: &Path, rel_path: &str) -> std::io::Result<Vec<Snapshot>> {
    let dir = snapshot_dir(vault, rel_path);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out: Vec<Snapshot> = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if let Some(timestamp_ms) = timestamp_of(&path) {
            out.push(Snapshot { timestamp_ms, path });
        }
    }
    // Newest first.
    out.sort_by_key(|e| std::cmp::Reverse(e.timestamp_ms));
    Ok(out)
}

/// Read a single snapshot's whole-file content back as a `String`.
///
/// status: plain-file-snapshots
pub fn read(path: &Path) -> std::io::Result<String> {
    fs::read_to_string(path)
}

/// Relocate a note's snapshot directory on rename/move so its history follows
/// the note (`<history>/<from> -> <history>/<to>`). A no-op when the source
/// directory doesn't exist (the note had no snapshots yet). Parent directories
/// of the destination are created as needed. A stale destination directory is
/// not clobbered — if `to` already has a history dir the move errors loudly
/// rather than silently merging two histories.
///
/// status: plain-file-snapshots
pub fn move_snapshots(vault: &Path, from_rel: &str, to_rel: &str) -> std::io::Result<()> {
    if from_rel == to_rel {
        return Ok(());
    }
    let from = snapshot_dir(vault, from_rel);
    if !from.exists() {
        return Ok(());
    }
    let to = snapshot_dir(vault, to_rel);
    // A bare `fs::rename` onto an EXISTING (empty) destination directory
    // succeeds on Unix — it replaces the target, silently discarding whatever
    // history already lived at `to_rel`. That contradicts the contract ("errors
    // loudly rather than silently merging two histories"), so refuse explicitly
    // when the destination already exists.
    if to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "snapshot history already exists at destination {}; refusing to clobber",
                to.display()
            ),
        ));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&from, &to)
}

/// Remove a note's entire snapshot history directory (`<history>/<rel_path>/`).
/// A no-op when the directory doesn't exist (the note had no snapshots). Used
/// when a document is FORGOTTEN — dropped from tracking entirely — so a later
/// file created at the same vault path cannot inherit the old file's snapshots
/// via [`list_snapshots`] (which would let the version dropdown / restore roll
/// the new file back to unrelated content).
///
/// status: plain-file-snapshots
pub fn remove_snapshots(vault: &Path, rel_path: &str) -> std::io::Result<()> {
    let dir = snapshot_dir(vault, rel_path);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Enforce a note's [`RetentionPolicy`]: drop snapshots beyond `max_snapshots`
/// (keeping the newest) and any older than `max_age_days`. Logs the pruned
/// count so retention is observable (no silent truncation). Called on every
/// [`snapshot`] write; safe to call standalone.
///
/// status: plain-file-snapshots
pub fn prune(vault: &Path, rel_path: &str, policy: RetentionPolicy) -> std::io::Result<usize> {
    let snapshots = list_snapshots(vault, rel_path)?; // newest-first
    let now = now_ms();
    // Age cutoff in ms; `max_age_days = 0` disables age-based pruning.
    let age_cutoff_ms = if policy.max_age_days == 0 {
        None
    } else {
        Some(u64::from(policy.max_age_days) * 24 * 60 * 60 * 1000)
    };

    let mut pruned = 0usize;
    for (idx, entry) in snapshots.iter().enumerate() {
        // Over the count cap? (`max_snapshots = 0` disables count pruning.)
        let over_count =
            policy.max_snapshots != 0 && idx >= policy.max_snapshots as usize;
        // Older than the age cutoff?
        let too_old = age_cutoff_ms
            .is_some_and(|cutoff| now.saturating_sub(entry.timestamp_ms) > cutoff);
        if over_count || too_old {
            match fs::remove_file(&entry.path) {
                Ok(()) => pruned += 1,
                Err(e) => {
                    // A failed prune is non-fatal (the snapshot is just cache)
                    // but must never be silent.
                    tracing::warn!(
                        path = %entry.path.display(),
                        error = %e,
                        "snapshot prune: failed to remove stale snapshot",
                    );
                }
            }
        }
    }
    if pruned > 0 {
        tracing::debug!(
            rel_path,
            pruned,
            max_snapshots = policy.max_snapshots,
            max_age_days = policy.max_age_days,
            "snapshot prune: dropped stale snapshots",
        );
    }
    Ok(pruned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A fresh save writes one plain `.md` snapshot under
    /// `.hiker/history/<rel>/` and `read_snapshot` round-trips it verbatim.
    #[test]
    fn snapshot_writes_plain_md_and_round_trips() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let wrote = snapshot(vault, "notes/a.md", "# hello", RetentionPolicy::default()).unwrap();
        assert!(wrote);

        let list = list_snapshots(vault, "notes/a.md").unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].path.starts_with(vault.join(".hiker/history/notes/a.md")));
        assert_eq!(read(&list[0].path).unwrap(), "# hello");
    }

    /// An identical-content save is a no-op — the newest snapshot already holds
    /// those exact bytes, so no second file is created.
    #[test]
    fn identical_content_save_is_noop() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        assert!(snapshot(vault, "a.md", "same", RetentionPolicy::default()).unwrap());
        assert!(!snapshot(vault, "a.md", "same", RetentionPolicy::default()).unwrap());
        assert_eq!(list_snapshots(vault, "a.md").unwrap().len(), 1);

        // A changed save does write a second snapshot.
        assert!(snapshot(vault, "a.md", "different", RetentionPolicy::default()).unwrap());
        assert_eq!(list_snapshots(vault, "a.md").unwrap().len(), 2);
    }

    /// `list_snapshots` returns newest-first (descending timestamp).
    #[test]
    fn list_is_newest_first() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let sdir = snapshot_dir(vault, "a.md");
        fs::create_dir_all(&sdir).unwrap();
        // Hand-place snapshots with known timestamps.
        fs::write(sdir.join("100.md"), b"old").unwrap();
        fs::write(sdir.join("300.md"), b"new").unwrap();
        fs::write(sdir.join("200.md"), b"mid").unwrap();
        // A non-timestamp file is ignored, not mis-sorted.
        fs::write(sdir.join("notes.txt"), b"junk").unwrap();

        let list = list_snapshots(vault, "a.md").unwrap();
        let stamps: Vec<u64> = list.iter().map(|e| e.timestamp_ms).collect();
        assert_eq!(stamps, vec![300, 200, 100]);
    }

    /// Count-based prune keeps the newest `max_snapshots` and drops the rest.
    #[test]
    fn prune_drops_by_count() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let sdir = snapshot_dir(vault, "a.md");
        fs::create_dir_all(&sdir).unwrap();
        for ts in [10u64, 20, 30, 40, 50] {
            fs::write(sdir.join(format!("{ts}.md")), b"x").unwrap();
        }
        let policy = RetentionPolicy { max_snapshots: 2, max_age_days: 0 };
        let pruned = prune(vault, "a.md", policy).unwrap();
        assert_eq!(pruned, 3);

        let list = list_snapshots(vault, "a.md").unwrap();
        let stamps: Vec<u64> = list.iter().map(|e| e.timestamp_ms).collect();
        assert_eq!(stamps, vec![50, 40]);
    }

    /// Age-based prune drops snapshots older than `max_age_days`, regardless of
    /// count.
    #[test]
    fn prune_drops_by_age() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let sdir = snapshot_dir(vault, "a.md");
        fs::create_dir_all(&sdir).unwrap();
        let now = now_ms();
        let day_ms = 24 * 60 * 60 * 1000u64;
        // One recent, one 40 days old.
        let recent = now - day_ms; // 1 day ago
        let ancient = now - 40 * day_ms; // 40 days ago
        fs::write(sdir.join(format!("{recent}.md")), b"recent").unwrap();
        fs::write(sdir.join(format!("{ancient}.md")), b"ancient").unwrap();

        let policy = RetentionPolicy { max_snapshots: 0, max_age_days: 30 };
        let pruned = prune(vault, "a.md", policy).unwrap();
        assert_eq!(pruned, 1);

        let list = list_snapshots(vault, "a.md").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].timestamp_ms, recent);
    }

    /// `snapshot` prunes on each write — the set stays capped at the count.
    #[test]
    fn snapshot_caps_set_on_write() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let policy = RetentionPolicy { max_snapshots: 3, max_age_days: 0 };
        for i in 0..10 {
            // Distinct content each time so none are skipped as no-ops.
            snapshot(vault, "a.md", &format!("v{i}"), policy).unwrap();
        }
        assert_eq!(list_snapshots(vault, "a.md").unwrap().len(), 3);
    }

    /// `move_snapshots` relocates the note's whole history directory.
    #[test]
    fn move_snapshots_relocates_dir() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        snapshot(vault, "old/a.md", "content", RetentionPolicy::default()).unwrap();
        assert_eq!(list_snapshots(vault, "old/a.md").unwrap().len(), 1);

        move_snapshots(vault, "old/a.md", "new/b.md").unwrap();
        assert!(list_snapshots(vault, "old/a.md").unwrap().is_empty());
        let moved = list_snapshots(vault, "new/b.md").unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(read(&moved[0].path).unwrap(), "content");
    }

    /// `move_snapshots` is a no-op when the source has no history.
    #[test]
    fn move_snapshots_noop_without_source() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        // Should not error even though `ghost.md` was never snapshotted.
        move_snapshots(vault, "ghost.md", "elsewhere.md").unwrap();
        assert!(list_snapshots(vault, "elsewhere.md").unwrap().is_empty());
    }

    /// `list_snapshots` on a never-snapshotted note returns empty, not an error.
    #[test]
    fn list_missing_dir_is_empty() {
        let dir = tempdir().unwrap();
        assert!(list_snapshots(dir.path(), "never.md").unwrap().is_empty());
    }

    /// Regression (finding 4): a backward clock step must not let a newer
    /// snapshot sort BEHIND an older one. We simulate the backward jump by
    /// hand-placing a snapshot with a far-future timestamp, then writing a fresh
    /// snapshot (whose `now_ms()` is necessarily smaller). The new write must be
    /// clamped to `newest + 1` so it sorts FIRST — otherwise `list().first()`
    /// (dedup) and `list()[1]` ("restore previous") both key off the wrong file.
    #[test]
    fn snapshot_enforces_monotonic_timestamp_under_backward_clock() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        let sdir = snapshot_dir(vault, "a.md");
        fs::create_dir_all(&sdir).unwrap();
        // An existing snapshot stamped far in the FUTURE (clock was ahead then).
        let future_ts = now_ms() + 10 * 365 * 24 * 60 * 60 * 1000; // ~10 years ahead
        fs::write(sdir.join(format!("{future_ts}.md")), b"old-current").unwrap();

        // Now the clock has stepped back: a fresh save's `now_ms()` is smaller
        // than `future_ts`. The new content must still become the newest.
        assert!(snapshot(vault, "a.md", "new-current", RetentionPolicy::default()).unwrap());

        let list = list_snapshots(vault, "a.md").unwrap();
        assert_eq!(list.len(), 2);
        // The freshly-written content sorts FIRST (newest), not the stale future
        // snapshot — the timestamp was clamped to `future_ts + 1`.
        assert_eq!(read(&list[0].path).unwrap(), "new-current");
        assert_eq!(list[0].timestamp_ms, future_ts + 1);
        // And dedup keys off the real current content: re-saving identical bytes
        // is a no-op now that the new snapshot sorts newest.
        assert!(!snapshot(vault, "a.md", "new-current", RetentionPolicy::default()).unwrap());
        assert_eq!(list_snapshots(vault, "a.md").unwrap().len(), 2);
    }

    /// Regression (finding 5): `move_snapshots` must NOT silently replace an
    /// existing destination history dir (a bare `fs::rename` onto an empty dir
    /// succeeds on Unix). It must error loudly instead, preserving both
    /// histories for the caller to reconcile.
    #[test]
    fn move_snapshots_errors_on_existing_destination() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        snapshot(vault, "from.md", "source-history", RetentionPolicy::default()).unwrap();
        // Destination already has its own history.
        snapshot(vault, "to.md", "dest-history", RetentionPolicy::default()).unwrap();

        let err = move_snapshots(vault, "from.md", "to.md")
            .expect_err("move onto an existing history dir must error, not clobber");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        // Both histories survive untouched.
        assert_eq!(
            read(&list_snapshots(vault, "from.md").unwrap()[0].path).unwrap(),
            "source-history"
        );
        assert_eq!(
            read(&list_snapshots(vault, "to.md").unwrap()[0].path).unwrap(),
            "dest-history"
        );
    }

    /// Even an EMPTY destination dir blocks the move (the Unix `rename`
    /// foot-gun): the contract is "no silent merge/replace".
    #[test]
    fn move_snapshots_errors_on_empty_existing_destination() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        snapshot(vault, "from.md", "x", RetentionPolicy::default()).unwrap();
        fs::create_dir_all(snapshot_dir(vault, "to.md")).unwrap(); // empty dest dir

        let err = move_snapshots(vault, "from.md", "to.md").expect_err("empty dest still errors");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    /// Regression (finding 2 helper): `remove_snapshots` drops a note's whole
    /// history dir, and is a no-op (Ok) when there is none.
    #[test]
    fn remove_snapshots_drops_history_dir() {
        let dir = tempdir().unwrap();
        let vault = dir.path();
        snapshot(vault, "gone.md", "content", RetentionPolicy::default()).unwrap();
        assert_eq!(list_snapshots(vault, "gone.md").unwrap().len(), 1);

        remove_snapshots(vault, "gone.md").unwrap();
        assert!(list_snapshots(vault, "gone.md").unwrap().is_empty());
        assert!(!snapshot_dir(vault, "gone.md").exists());

        // Idempotent — removing a never-snapshotted note is Ok.
        remove_snapshots(vault, "never.md").unwrap();
    }
}
