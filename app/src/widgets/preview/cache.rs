//! Persisted on-disk cache for rendered rich-preview thumbnails (canvas
//! diagrams, cluster-tree force layouts). Mirrors the diagram disk-cache
//! (`panels::buffer::widgets::disk_cache`): a rendered preview is keyed by its
//! [`PreviewKey`] (content hash + kind + pixel size, with the render version
//! folded into the hash) and persisted as one self-describing PNG, so the
//! `resvg` blit survives across sessions.
//!
//! Storage lives under `<vault>/.hiker/previews/`, regenerable / losable data
//! per `docs/design.md` §`subsystem-notes-visible`. Each entry is one PNG:
//!
//!   `<kind>-<content_hash:016x>-<size>.png`  — straight RGBA8 (PNG RGBA),
//!                                               width/height self-described.
//!
//! Small and large thumbnails are separate `size` buckets, so a hover-expand
//! never evicts the inline thumbnail it grew from. Best-effort throughout —
//! any I/O / decode error degrades to a live render, never a panic.
//!
//! status: preview-disk-cache

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{PreviewKey, PreviewKind};

/// Cache directory budget before the LRU sweep starts evicting. 64 MB of
/// rasterized previews is generous yet bounded; the sweep runs at most once per
/// session (see [`maybe_sweep`]), mirroring the diagram disk-cache.
const BUDGET_BYTES: u64 = 64 * 1024 * 1024;

impl PreviewKind {
    /// Filename prefix for this kind. Part of the on-disk filename so two kinds
    /// can't collide even if their `content_hash`es did, and so the directory
    /// stays human-readable.
    const fn prefix(self) -> &'static str {
        match self {
            PreviewKind::Canvas => "canvas",
            PreviewKind::Tree => "tree",
        }
    }
}

/// A rendered preview's pixels plus its dimensions — the load/store payload.
#[derive(Clone, Debug)]
pub(super) struct CachedImage {
    /// Tightly-packed straight (un-premultiplied) RGBA8, `width * height * 4`.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Where preview PNGs live for the open vault, plus the budget sweep. Built at
/// the thumbnail call site (which can reach the vault root); `None` keeps the
/// whole disk layer out of the path in non-vault contexts.
#[derive(Clone, Debug)]
pub(super) struct PreviewCache {
    /// `<vault>/.hiker/previews`.
    dir: PathBuf,
}

impl PreviewCache {
    /// Build a cache rooted at `vault_root`.
    pub(super) fn new(vault_root: &Path) -> Self {
        Self {
            dir: vault_root.join(".hiker").join("previews"),
        }
    }

    fn png_path(&self, key: PreviewKey) -> PathBuf {
        self.dir.join(format!(
            "{}-{:016x}-{}.png",
            key.kind.prefix(),
            key.content_hash,
            key.size,
        ))
    }

    /// Look up a previously-stored render for `key`. `None` on a miss or any
    /// decode error (the caller then renders live).
    pub(super) fn load(&self, key: PreviewKey) -> Option<CachedImage> {
        let bytes = std::fs::read(self.png_path(key)).ok()?;
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
        let rgba = img.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();
        if width == 0 || height == 0 {
            return None;
        }
        Some(CachedImage {
            rgba: rgba.into_raw(),
            width,
            height,
        })
    }

    /// Persist `img` under `key`. Best-effort: a write failure (read-only vault,
    /// full disk) is logged and swallowed so rendering still succeeds from the
    /// in-memory result. Triggers the one-per-session LRU sweep before writing.
    pub(super) fn store(&self, key: PreviewKey, img: &image::RgbaImage) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        maybe_sweep(&self.dir);
        let png = self.png_path(key);
        let Some(encoded) = encode_png(img) else { return };
        if let Err(e) = std::fs::write(&png, &encoded) {
            tracing::debug!(path = %png.display(), error = %e, "preview-cache write failed");
        }
    }
}

/// Encode an `RgbaImage` to PNG bytes. `None` on an encode failure or a
/// degenerate size (the caller then skips the disk write).
fn encode_png(img: &image::RgbaImage) -> Option<Vec<u8>> {
    if img.width() == 0 || img.height() == 0 {
        return None;
    }
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// Run the byte-budget LRU sweep at most once per `dir` per session. Guarded by
/// a `OnceLock` so the (cheap, best-effort) directory scan happens once on first
/// cache use rather than blocking vault open or every render.
fn maybe_sweep(dir: &Path) {
    static SWEPT: OnceLock<()> = OnceLock::new();
    SWEPT.get_or_init(|| {
        sweep_to_budget(dir, BUDGET_BYTES);
    });
}

/// Evict oldest-by-mtime entries until the directory's total byte size is at or
/// under `budget`. Best-effort: scan errors abort the sweep. Keeps the most
/// recently *touched* renders (filesystem mtime is the LRU proxy — cheap, no
/// access-log bookkeeping). Mirrors the diagram disk-cache's `sweep_to_budget`.
fn sweep_to_budget(dir: &Path, budget: u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total: u64 = 0;
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let size = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total += size;
        entries.push((mtime, size, path));
    }
    if total <= budget {
        return;
    }
    entries.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, size, png) in entries {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&png).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> image::RgbaImage {
        image::RgbaImage::from_pixel(w, h, image::Rgba([12, 34, 56, 255]))
    }

    #[test]
    fn round_trips_png() {
        let tmp = std::env::temp_dir().join(format!("hiker-pc-{}", ulid::Ulid::new()));
        let cache = PreviewCache::new(&tmp);
        let key = PreviewKey {
            content_hash: 0xABCD,
            kind: PreviewKind::Canvas,
            size: 16,
        };
        cache.store(key, &img(4, 3));
        let got = cache.load(key).expect("hit");
        assert_eq!(got.width, 4);
        assert_eq!(got.height, 3);
        assert_eq!(got.rgba.len(), 4 * 3 * 4);
        let _ = std::fs::remove_dir_all(tmp.join(".hiker"));
    }

    #[test]
    fn size_buckets_are_distinct_files() {
        let tmp = std::env::temp_dir().join(format!("hiker-pc-{}", ulid::Ulid::new()));
        let cache = PreviewCache::new(&tmp);
        let small = PreviewKey { content_hash: 0x7, kind: PreviewKind::Tree, size: 16 };
        let large = PreviewKey { content_hash: 0x7, kind: PreviewKind::Tree, size: 256 };
        cache.store(small, &img(2, 2));
        cache.store(large, &img(8, 8));
        // Same hash + kind, different size → distinct entries.
        assert_eq!(cache.load(small).unwrap().width, 2);
        assert_eq!(cache.load(large).unwrap().width, 8);
        let _ = std::fs::remove_dir_all(tmp.join(".hiker"));
    }

    #[test]
    fn kinds_dont_collide_on_equal_hash() {
        let tmp = std::env::temp_dir().join(format!("hiker-pc-{}", ulid::Ulid::new()));
        let cache = PreviewCache::new(&tmp);
        let canvas = PreviewKey { content_hash: 0x9, kind: PreviewKind::Canvas, size: 16 };
        let tree = PreviewKey { content_hash: 0x9, kind: PreviewKind::Tree, size: 16 };
        cache.store(canvas, &img(2, 2));
        cache.store(tree, &img(5, 5));
        assert_eq!(cache.load(canvas).unwrap().width, 2);
        assert_eq!(cache.load(tree).unwrap().width, 5);
        let _ = std::fs::remove_dir_all(tmp.join(".hiker"));
    }

    #[test]
    fn miss_returns_none() {
        let tmp = std::env::temp_dir().join(format!("hiker-pc-{}", ulid::Ulid::new()));
        let cache = PreviewCache::new(&tmp);
        let key = PreviewKey { content_hash: 0xDEAD, kind: PreviewKind::Canvas, size: 16 };
        assert!(cache.load(key).is_none());
    }

    #[test]
    fn sweep_evicts_oldest_over_budget() {
        let tmp = std::env::temp_dir().join(format!("hiker-pc-sweep-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();
        let old = tmp.join("tree-0000000000000001-16.png");
        let new = tmp.join("tree-0000000000000002-16.png");
        std::fs::write(&old, vec![0u8; 100]).unwrap();
        loop {
            std::fs::write(&new, vec![0u8; 100]).unwrap();
            let old_m = std::fs::metadata(&old).unwrap().modified().unwrap();
            let new_m = std::fs::metadata(&new).unwrap().modified().unwrap();
            if new_m > old_m {
                break;
            }
            std::hint::spin_loop();
        }
        sweep_to_budget(&tmp, 150);
        assert!(!old.exists(), "older entry evicted");
        assert!(new.exists(), "newer entry kept");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
