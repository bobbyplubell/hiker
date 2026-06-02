//! Item-retention pruning for a feed (`rss-item-retention`). Feeds grow
//! unbounded — every poll can add children — so a feed bounds its child count
//! with `feed.item_retention`, resolved as the same cascade shape
//! `extract-artifact-retention` uses (vault `[extract].feed_item_retention`
//! default → per-feed frontmatter; the per-feed value is resolved by the caller
//! and handed in here as a string). Values: `keep:N` (default) and `forever`.
//!
//! Pruned children go to **trash, not silent deletion** (`rss-item-retention`).
//! `hiker-extract` is a leaf crate that must not depend on `core`, so it cannot
//! call `core::trash`. Rather than add that dependency (the project's stated
//! preference — see `extract-crate-decoupled` and the trash note in the spec),
//! the prune moves the child into the vault's trash directory directly:
//! `<vault_root>/.hiker/trash/`, the same on-disk location `core::trash` owns.
//! This is the documented seam: the app's `core::trash` and this path write to
//! the same directory, so a child pruned here is recoverable through the normal
//! trash UI; if the app later wants prune-via-core, it passes a closure — for
//! now the direct move keeps the leaf crate `core`-free.
//
// status: rss-item-retention

use std::path::{Path, PathBuf};

use super::manifest::Manifest;

/// A parsed retention bound.
enum Bound {
    /// Keep at most `N` children; prune the oldest beyond that.
    Keep(usize),
    /// Never prune.
    Forever,
}

/// Parse a retention string (`keep:N` / `forever`) into a [`Bound`]. An
/// unrecognized string is treated as `forever` (fail-safe: never silently
/// delete a user's children because of a typo in config).
fn parse_bound(s: &str) -> Bound {
    let s = s.trim();
    if s.eq_ignore_ascii_case("forever") {
        return Bound::Forever;
    }
    if let Some(n) = s.strip_prefix("keep:").and_then(|n| n.trim().parse::<usize>().ok()) {
        return Bound::Keep(n);
    }
    Bound::Forever
}

/// Prune the feed's children to the retention bound, oldest first, moving each
/// pruned child (and its companion archive folder, if any) to the vault trash
/// dir. Updates `manifest` in place (drops the pruned guids) and returns the
/// original child paths that were pruned. A `forever` bound, or a child count
/// within the bound, prunes nothing.
///
/// status: rss-item-retention
pub fn prune(
    manifest: &mut Manifest,
    companion_dir: &Path,
    vault_root: &Path,
    retention: &str,
) -> Result<Vec<PathBuf>, String> {
    let keep = match parse_bound(retention) {
        Bound::Forever => return Ok(Vec::new()),
        Bound::Keep(n) => n,
    };
    let oldest_first = manifest.guids_oldest_first();
    if oldest_first.len() <= keep {
        return Ok(Vec::new());
    }
    let to_prune = oldest_first.len() - keep;
    let mut pruned = Vec::new();
    for guid in oldest_first.into_iter().take(to_prune) {
        let Some(record) = manifest.lookup(&guid).cloned() else { continue };
        let child = companion_dir.join(&record.child_file);
        trash_child(&child, vault_root)?;
        manifest.remove(&guid);
        pruned.push(child);
    }
    Ok(pruned)
}

/// Move one pruned child (and any sibling archive companion folder) into the
/// vault trash dir `<vault_root>/.hiker/trash/`, preserving the filename
/// (collision-suffixed). The seam `core::trash` shares; see the module doc.
fn trash_child(child: &Path, vault_root: &Path) -> Result<(), String> {
    if !child.exists() {
        return Ok(()); // already gone; nothing to trash
    }
    let trash = vault_root.join(".hiker").join("trash");
    std::fs::create_dir_all(&trash).map_err(|e| format!("mkdir trash: {e}"))?;
    let name = child.file_name().map(std::ffi::OsStr::to_os_string).unwrap_or_default();
    let dest = unique_in(&trash, &name.to_string_lossy());
    std::fs::rename(child, &dest).map_err(|e| format!("trash {}: {e}", child.display()))?;

    // The child's archive companion folder (`<stem>/`), if the entry had one.
    if let Some(stem) = child.file_stem() {
        let archive_dir = child.with_file_name(stem);
        if archive_dir.is_dir() {
            let adest = unique_in(&trash, &stem.to_string_lossy());
            let _ = std::fs::rename(&archive_dir, &adest);
        }
    }
    Ok(())
}

/// A non-colliding path for `name` inside `dir`, suffixing `-2`, `-3`, … on the
/// stem so a re-trashed filename doesn't overwrite an earlier one.
fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 2..100_000 {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem}-{n}.{ext}")),
            None => dir.join(format!("{stem}-{n}")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}
