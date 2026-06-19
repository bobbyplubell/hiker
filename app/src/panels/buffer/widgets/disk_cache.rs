//! Persisted on-disk cache for rasterized diagram widgets (LaTeX math,
//! Mermaid, WaveDrom). Sits *below* the in-memory `CachedDeco` / texture
//! caches (docs/editor-widgets.md §"Caching and invalidation"): on the first
//! open of a note, the in-memory cache misses and the render path would pay
//! the full `resvg` blit. This layer lets that blit's result survive across
//! sessions — keyed by the render's existing `content_hash` (which already
//! folds in source, kind/style, font size, dpr, and theme colors), so a hit
//! restores the exact same pixels the live render would have produced.
//!
//! Storage lives under `<vault>/.hiker/diagram-cache/`, regenerable / losable
//! data per `docs/design.md` §`subsystem-notes-visible`. Each entry is one
//! self-describing PNG plus a tiny sidecar for the inline-math baseline metric
//! (PNG carries width/height; baseline is the only thing it can't):
//!
//!   `<domain>-r<rev>-<hash:016x>.png`  — straight RGBA8 pixels (PNG RGBA, no
//!                                  alpha premultiply), width/height
//!                                  self-described.
//!   `<domain>-r<rev>-<hash:016x>.base` — present only for inline math: 4 raw
//!                                  little-endian bytes of the `f32` baseline.
//!                                  Absent ⇒ block widget (no baseline).
//!
//! `<rev>` is the domain's renderer revision ([`Domain::revision`]): the
//! `content_hash` folds in source/style/theme but knows nothing about the
//! *code* that turns source into pixels, so without it a renderer fix keeps
//! serving pre-fix pixels forever. Bumping the revision makes every old entry
//! unreachable (different filename), and the once-per-session sweep deletes
//! entries whose revision no longer matches before doing its byte-budget
//! pass.
//!
//! Gated by `[render] cache_diagrams` (default on); when off the render path
//! skips this layer entirely and the in-memory caches carry the session.
//! Best-effort throughout — any I/O / decode error degrades to a live render,
//! never a panic or a user-visible failure.
//!
//! status: widget-render-disk-cache

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::render::RenderedWidget;

/// Which widget family a cached entry belongs to. Part of the on-disk
/// filename so two domains can't collide even if their `content_hash`es did
/// (they already domain-tag their hashes, but the prefix keeps the directory
/// human-readable and defends the key independently).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Domain {
    Math,
    Mermaid,
    WaveDrom,
    Chart,
    /// A vault image (`![alt](path)`) loaded + decoded to a texture for a table
    /// cell. status: widget-table-render
    Image,
}

impl Domain {
    const fn prefix(self) -> &'static str {
        match self {
            Domain::Math => "math",
            Domain::Mermaid => "mermaid",
            Domain::WaveDrom => "wavedrom",
            Domain::Chart => "chart",
            Domain::Image => "image",
        }
    }

    /// Renderer revision, part of the on-disk filename. **Bump a domain's
    /// number whenever its renderer starts producing different pixels for
    /// identical input** — including changes in the renderer's dependencies
    /// (Mermaid's output shifts with `hiker-graph` layout changes, Math's
    /// with `hiker-math`, charts' with the `hiker-charts` crates). Stale
    /// entries become unreachable immediately and are deleted by the next
    /// session sweep. Cosmetic-only refactors don't need a bump; when in
    /// doubt, bump — the only cost is one cold re-render per diagram.
    const fn revision(self) -> u32 {
        match self {
            Domain::Math => 1,
            Domain::Mermaid => 1,
            Domain::WaveDrom => 1,
            Domain::Chart => 1,
            Domain::Image => 1,
        }
    }
}

/// `<prefix>-r<rev>-` — the filename prefix every *current* entry of a domain
/// starts with; anything carrying a known domain prefix but not matching its
/// current revision prefix is stale and gets purged by the sweep.
fn rev_prefix(domain: Domain) -> String {
    format!("{}-r{}-", domain.prefix(), domain.revision())
}

/// Cache directory budget before the LRU sweep starts evicting. 64 MB of
/// rasterized diagrams is generous for a vault yet bounded; the sweep runs
/// at most once per session (see [`maybe_sweep`]).
const BUDGET_BYTES: u64 = 64 * 1024 * 1024;

/// Threaded-down disk-cache context for the (otherwise vault-agnostic) render
/// module: where the cache lives and whether it's enabled. Built at the
/// decoration-emit call sites (which can reach the vault root + config) and
/// passed by reference into the render helpers. `None` everywhere the cache
/// shouldn't run (non-vault contexts, the toggle off) keeps the render path
/// unchanged.
#[derive(Clone, Debug)]
pub struct DiagramCacheCtx {
    /// `<vault>/.hiker/diagram-cache`.
    dir: PathBuf,
}

impl DiagramCacheCtx {
    /// Build a context rooted at `vault_root`, or `None` when `enabled` is
    /// false so callers can `as_ref()` it into the render helpers and have the
    /// whole disk layer compile out of the hot path when the toggle is off.
    pub fn new(vault_root: &Path, enabled: bool) -> Option<Self> {
        if !enabled {
            return None;
        }
        Some(Self {
            dir: vault_root.join(".hiker").join("diagram-cache"),
        })
    }

    fn png_path(&self, domain: Domain, hash: u64) -> PathBuf {
        self.dir.join(format!("{}{hash:016x}.png", rev_prefix(domain)))
    }

    fn base_path(&self, domain: Domain, hash: u64) -> PathBuf {
        self.dir.join(format!("{}{hash:016x}.base", rev_prefix(domain)))
    }

    /// Look up a previously-stored render for `(domain, hash)`. Returns `None`
    /// on a miss or any decode error (the caller then renders live). Reads
    /// straight RGBA8 back out of the PNG and the baseline out of its sidecar.
    pub fn load(&self, domain: Domain, hash: u64) -> Option<RenderedWidget> {
        let png = self.png_path(domain, hash);
        let bytes = std::fs::read(&png).ok()?;
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?;
        let rgba = img.to_rgba8();
        let width = rgba.width();
        let height = rgba.height();
        if width == 0 || height == 0 {
            return None;
        }
        let baseline = self.read_baseline(domain, hash);
        Some(RenderedWidget {
            rgba: rgba.into_raw(),
            width,
            height,
            baseline,
            content_hash: hash,
        })
    }

    /// Persist `rendered` under `(domain, hash)`. Best-effort: a write failure
    /// (read-only vault, full disk) is logged and swallowed so rendering still
    /// succeeds from the in-memory result. Triggers the one-per-session LRU
    /// sweep before writing so the directory stays under budget.
    pub fn store(&self, domain: Domain, rendered: &RenderedWidget) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        maybe_sweep(&self.dir);
        let png = self.png_path(domain, rendered.content_hash);
        let encoded = encode_png(rendered);
        let Some(encoded) = encoded else { return };
        if let Err(e) = std::fs::write(&png, &encoded) {
            tracing::debug!(path = %png.display(), error = %e, "diagram-cache write failed");
            return;
        }
        let base = self.base_path(domain, rendered.content_hash);
        match rendered.baseline {
            Some(b) => {
                let _ = std::fs::write(&base, b.to_le_bytes());
            }
            // A block widget has no baseline; make sure a stale sidecar from a
            // hash collision-free overwrite never lingers.
            None => {
                let _ = std::fs::remove_file(&base);
            }
        }
    }

    fn read_baseline(&self, domain: Domain, hash: u64) -> Option<f32> {
        let bytes = std::fs::read(self.base_path(domain, hash)).ok()?;
        let arr: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
        Some(f32::from_le_bytes(arr))
    }
}

/// Encode a render's straight RGBA8 to PNG bytes. `None` on an encode failure
/// or a degenerate size (the caller then skips the disk write).
fn encode_png(rendered: &RenderedWidget) -> Option<Vec<u8>> {
    let expected = (rendered.width as usize) * (rendered.height as usize) * 4;
    if rendered.width == 0 || rendered.height == 0 || rendered.rgba.len() != expected {
        return None;
    }
    let buf = image::RgbaImage::from_raw(rendered.width, rendered.height, rendered.rgba.clone())?;
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    Some(out.into_inner())
}

/// Run the stale-revision purge + byte-budget LRU sweep at most once per `dir`
/// per session. Guarded by a `OnceLock` so the (cheap, best-effort) directory
/// scan happens once on first cache use rather than blocking vault open or
/// every render.
fn maybe_sweep(dir: &Path) {
    static SWEPT: OnceLock<()> = OnceLock::new();
    SWEPT.get_or_init(|| {
        purge_stale_revisions(dir);
        sweep_to_budget(dir, BUDGET_BYTES);
    });
}

/// Delete every entry written under an outdated renderer revision (or the
/// pre-revision filename scheme): any `.png`/`.base` whose name starts with a
/// known domain prefix but not with that domain's current `<prefix>-r<rev>-`.
/// Files that match no domain are left alone (not ours). Best-effort.
fn purge_stale_revisions(dir: &Path) {
    const DOMAINS: [Domain; 5] = [
        Domain::Math,
        Domain::Mermaid,
        Domain::WaveDrom,
        Domain::Chart,
        Domain::Image,
    ];
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("png") | Some("base") => {}
            _ => continue,
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let stale = DOMAINS.iter().any(|&d| {
            name.starts_with(&format!("{}-", d.prefix())) && !name.starts_with(&rev_prefix(d))
        });
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Evict oldest-by-mtime entries until the directory's total byte size is at
/// or under `budget`. Best-effort: scan errors abort the sweep, and each
/// `.png` carries its `.base` sidecar with it on eviction. Keeps the most
/// recently *touched* renders (filesystem mtime is the LRU proxy — cheap, no
/// access-log bookkeeping).
fn sweep_to_budget(dir: &Path, budget: u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    // (mtime, size, png path). Only `.png` entries carry the payload weight;
    // `.base` sidecars are tiny and evicted alongside their png, so the budget
    // math tracks pngs and ignores sidecar bytes (negligible).
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
    // Oldest first; evict from the front until under budget.
    entries.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, size, png) in entries {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&png).is_ok() {
            total = total.saturating_sub(size);
            let base = png.with_extension("base");
            let _ = std::fs::remove_file(base);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widget(hash: u64, baseline: Option<f32>) -> RenderedWidget {
        // 2x1 straight RGBA8: one opaque red, one semi-transparent green.
        RenderedWidget {
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 128],
            width: 2,
            height: 1,
            baseline,
            content_hash: hash,
        }
    }

    #[test]
    fn round_trips_block_widget() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-{}", ulid::Ulid::new()));
        let ctx = DiagramCacheCtx::new(&tmp, true).expect("enabled");
        let w = widget(0xABCD, None);
        ctx.store(Domain::Mermaid, &w);
        let got = ctx.load(Domain::Mermaid, 0xABCD).expect("hit");
        assert_eq!(got.rgba, w.rgba);
        assert_eq!(got.width, 2);
        assert_eq!(got.height, 1);
        assert_eq!(got.baseline, None);
        assert_eq!(got.content_hash, 0xABCD);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn round_trips_inline_baseline() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-{}", ulid::Ulid::new()));
        let ctx = DiagramCacheCtx::new(&tmp, true).expect("enabled");
        let w = widget(0x12, Some(7.5));
        ctx.store(Domain::Math, &w);
        let got = ctx.load(Domain::Math, 0x12).expect("hit");
        assert_eq!(got.baseline, Some(7.5));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn miss_returns_none() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-{}", ulid::Ulid::new()));
        let ctx = DiagramCacheCtx::new(&tmp, true).expect("enabled");
        assert!(ctx.load(Domain::WaveDrom, 0xDEAD).is_none());
    }

    #[test]
    fn disabled_yields_no_ctx() {
        let tmp = std::env::temp_dir().join("hiker-dc-disabled");
        assert!(DiagramCacheCtx::new(&tmp, false).is_none());
    }

    #[test]
    fn domains_dont_collide_on_equal_hash() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-{}", ulid::Ulid::new()));
        let ctx = DiagramCacheCtx::new(&tmp, true).expect("enabled");
        ctx.store(Domain::Math, &widget(0x7, Some(1.0)));
        ctx.store(Domain::Mermaid, &widget(0x7, None));
        // Same hash, different domain → distinct files, distinct baselines.
        assert_eq!(ctx.load(Domain::Math, 0x7).unwrap().baseline, Some(1.0));
        assert_eq!(ctx.load(Domain::Mermaid, 0x7).unwrap().baseline, None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn purge_removes_stale_revisions_only() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-purge-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Current-revision entry: kept.
        let current = tmp.join(format!("{}00000000000000aa.png", rev_prefix(Domain::Mermaid)));
        // Outdated revision and pre-revision filename scheme: purged.
        let old_rev = tmp.join("mermaid-r0-00000000000000bb.png");
        let old_fmt = tmp.join("mermaid-00000000000000cc.png");
        let old_base = tmp.join("math-00000000000000dd.base");
        // Not ours (unknown prefix / extension): left alone.
        let foreign = tmp.join("notes-00000000000000ee.png");
        let other_ext = tmp.join("mermaid-r0-00000000000000ff.txt");
        for f in [&current, &old_rev, &old_fmt, &old_base, &foreign, &other_ext] {
            std::fs::write(f, b"x").unwrap();
        }
        purge_stale_revisions(&tmp);
        assert!(current.exists(), "current revision kept");
        assert!(!old_rev.exists(), "outdated revision purged");
        assert!(!old_fmt.exists(), "pre-revision scheme purged");
        assert!(!old_base.exists(), "stale sidecar purged");
        assert!(foreign.exists(), "unknown prefix untouched");
        assert!(other_ext.exists(), "non-cache extension untouched");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sweep_evicts_oldest_over_budget() {
        let tmp = std::env::temp_dir().join(format!("hiker-dc-sweep-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Two png files; budget of 1 byte forces eviction of the older one.
        let old = tmp.join("mermaid-0000000000000001.png");
        let new = tmp.join("mermaid-0000000000000002.png");
        std::fs::write(&old, vec![0u8; 100]).unwrap();
        std::fs::write(&new, vec![0u8; 100]).unwrap();
        // Make `new` newer than `old`.
        filetime_bump(&new, &old);
        sweep_to_budget(&tmp, 150);
        assert!(!old.exists(), "older entry evicted");
        assert!(new.exists(), "newer entry kept");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Ensure `new`'s mtime is strictly after `old`'s without depending on a
    /// filetime crate — rewrite `new` after a short spin so its mtime advances.
    fn filetime_bump(new: &Path, old: &Path) {
        let old_m = std::fs::metadata(old).unwrap().modified().unwrap();
        loop {
            std::fs::write(new, vec![0u8; 100]).unwrap();
            let new_m = std::fs::metadata(new).unwrap().modified().unwrap();
            if new_m > old_m {
                break;
            }
            std::hint::spin_loop();
        }
    }
}
