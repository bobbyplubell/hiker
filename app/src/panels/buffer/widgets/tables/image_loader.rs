//! Vault-sandboxed image loader for an `![alt](path)` cell (`widget-table-render`).
//!
//! A pipe-table cell whose whole source is one markdown image resolves its path
//! the way Hiker resolves a link — note-relative first, then vault-root, then the
//! bare-name wikilink nearest-folder match — and reads the bytes through the
//! [`Vault`] sandbox (no `..` escapes, no symlink hops), exactly as the chart
//! data resolver does for CSV. The raw bytes + a stable cache key (path + mtime)
//! then drive [`super::super::render::render_image`].
//!
//! This is the binary-bytes counterpart to `crate::charts::VaultDataResolver`
//! (which reads UTF-8 text); an image isn't text, so it can't reuse that path.

use hiker_core::vault::Vault;

/// A vault-bound image resolver for a single note: resolves an `![alt](path)`
/// reference relative to the note, then the vault root, then by bare name, and
/// reads the file's bytes under the vault sandbox. Owned (a `Vault` clone + two
/// strings) so the decoration-rebuild closure needn't borrow the app.
#[derive(Clone)]
pub struct CellImageResolver {
    vault: Vault,
    /// The note's vault-relative path (the referrer), e.g. `notes/page.md`.
    note_path: String,
    /// The note's directory (`""` at vault root) — a relative image resolves to
    /// a sibling first.
    note_dir: String,
}

/// A resolved image: its raw file bytes and a stable identity key (vault path +
/// mtime) the render cache hashes on, so an edit elsewhere in the note doesn't
/// bust the texture but replacing the file on disk does.
pub struct ResolvedImage {
    pub bytes: Vec<u8>,
    pub key: String,
}

impl CellImageResolver {
    /// Bind a resolver to the note at `note_path` (vault-relative).
    #[must_use]
    pub fn new(vault: Vault, note_path: &str) -> Self {
        let note_dir = note_path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        Self { vault, note_path: note_path.to_string(), note_dir }
    }

    /// The note-relative candidate path for `id` (`note_dir/id`, or just `id` at
    /// vault root). The vault normalizes `..`/`.` and re-checks the sandbox.
    fn sibling_rel(&self, id: &str) -> String {
        if self.note_dir.is_empty() {
            id.to_string()
        } else {
            format!("{}/{}", self.note_dir, id)
        }
    }

    /// Read `rel`'s raw bytes through the vault sandbox, paired with a stable
    /// identity key (the vault path plus the file mtime, so an in-place replace
    /// changes the key and busts the texture cache). `None` if it can't be read.
    fn try_bytes(&self, rel: &str) -> Option<ResolvedImage> {
        let abs = self.vault.abs_path(rel).ok()?;
        let bytes = std::fs::read(&abs).ok()?;
        let mtime = std::fs::metadata(&abs)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        Some(ResolvedImage { bytes, key: format!("{rel}@{mtime}") })
    }

    /// Bare-name fallback: the wikilink nearest-folder name match over the
    /// vault's image files, so `![](logo.png)` finds the closest `logo.png` when
    /// no sibling exists. Walks lazily (only when the direct reads miss and `id`
    /// carries no `/`).
    fn resolve_by_name(&self, id: &str) -> Option<ResolvedImage> {
        use hiker_core::wikilink::{resolve_path, AmbiguityPolicy, Resolution};
        let paths = self.walk_image_files();
        match resolve_path(&paths, id, AmbiguityPolicy::NearestFolder, Some(&self.note_path)) {
            Resolution::Resolved(rel) => self.try_bytes(&rel),
            _ => None,
        }
    }

    /// Every image file in the vault, vault-relative, honoring the watcher's
    /// ignore list. Used only by the bare-name fallback.
    fn walk_image_files(&self) -> Vec<String> {
        let Ok(root) = self.vault.abs_path("") else { return Vec::new() };
        let mut out = Vec::new();
        for entry in walkdir::WalkDir::new(&root).follow_links(false).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&root) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if hiker_core::watcher::is_ignored(&rel) {
                continue;
            }
            if is_image_ext(&rel) {
                out.push(rel);
            }
        }
        out
    }

    /// Resolve an `![alt](path)` reference to its raw bytes + cache key, mirroring
    /// a Hiker link: note-relative, then vault-root, then the bare-name match.
    /// `None` when nothing resolves (the cell falls back to source).
    #[must_use]
    pub fn resolve(&self, path: &str) -> Option<ResolvedImage> {
        let path = path.trim();
        if path.is_empty() || !is_image_ext(path) {
            return None;
        }
        if let Some(img) = self.try_bytes(&self.sibling_rel(path)) {
            return Some(img);
        }
        if path != self.sibling_rel(path)
            && let Some(img) = self.try_bytes(path)
        {
            return Some(img);
        }
        if !path.contains('/') {
            return self.resolve_by_name(path);
        }
        None
    }
}

/// Whether `path` ends in an image extension the decoder supports (PNG / JPEG /
/// GIF / WebP / BMP — the `image` crate features enabled for the app).
fn is_image_ext(path: &str) -> bool {
    std::path::Path::new(path).extension().is_some_and(|e| {
        let e = e.to_ascii_lowercase();
        matches!(
            e.to_str(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{CellImageResolver, is_image_ext};
    use hiker_core::vault::Vault;

    #[test]
    fn image_ext_recognizes_web_formats() {
        for p in ["a.png", "dir/B.JPG", "x.jpeg", "y.gif", "z.webp", "q.bmp"] {
            assert!(is_image_ext(p), "{p} is an image");
        }
        for p in ["a.svg", "notes.md", "data.csv", "noext"] {
            assert!(!is_image_ext(p), "{p} is not a raster image");
        }
    }

    /// Write a tiny PNG at `rel` under `root`, creating parent dirs.
    fn write_png(root: &std::path::Path, rel: &str) {
        let abs = root.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let buf = image::RgbaImage::from_pixel(8, 6, image::Rgba([10, 200, 40, 255]));
        image::DynamicImage::ImageRgba8(buf).save(&abs).unwrap();
    }

    #[test]
    fn resolves_note_relative_image_under_sandbox() {
        // status: widget-table-render — `![alt](pic.png)` in a note resolves to a
        // sibling file, reads its bytes through the vault sandbox, and keys on
        // (path + mtime).
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "notes/pic.png");
        let vault = Vault::open(dir.path()).unwrap();
        let r = CellImageResolver::new(vault, "notes/page.md");
        let img = r.resolve("pic.png").expect("a sibling image resolves");
        assert!(!img.bytes.is_empty(), "image bytes read");
        assert!(img.key.starts_with("notes/pic.png@"), "key is path@mtime: {}", img.key);
    }

    #[test]
    fn rejects_non_image_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        let r = CellImageResolver::new(vault, "page.md");
        assert!(r.resolve("data.csv").is_none(), "a non-image extension is rejected");
        assert!(r.resolve("missing.png").is_none(), "a missing file resolves to None");
    }

    #[test]
    fn bare_name_fallback_finds_image_elsewhere() {
        // A bare `![](logo.png)` with no sibling falls back to the nearest-folder
        // wikilink name match over the vault's images.
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "assets/logo.png");
        let vault = Vault::open(dir.path()).unwrap();
        let r = CellImageResolver::new(vault, "notes/page.md");
        let img = r.resolve("logo.png").expect("bare-name match finds assets/logo.png");
        assert!(img.key.starts_with("assets/logo.png@"), "{}", img.key);
    }
}
