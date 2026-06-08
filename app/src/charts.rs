//! Shared `hiker-charts` ↔ Hiker bridge: the theme map and the Vault-backed
//! [`DataResolver`].
//!
//! Both chart surfaces (the inline ```` ```chart ```` block widget in
//! `panels/buffer/widgets` and the `.csv` builder tab in `panels/charts_tab`)
//! lean on this module so the Hiker-isms — the editor theme, the vault's
//! sandboxed file access, the same link-resolution semantics wikilinks use —
//! live in one place and `hiker-charts` itself stays host-agnostic
//! (`hiker_charts_core::host::DataResolver` is the seam). status: chart-render

use hiker_charts_core::data::Table;
use hiker_charts_core::host::{DataResolver, ResolveError};
use hiker_charts_core::theme::Theme as ChartTheme;
use hiker_core::vault::Vault;

/// Map a straight RGBA editor color onto a `hiker-charts` opaque color (alpha is
/// forced to 255 — the chart surface is painted, not blended).
const fn chart_color(c: editor_core::decoration::Color) -> hiker_charts_core::theme::Color {
    hiker_charts_core::theme::Color::rgb(c.r, c.g, c.b)
}

/// Perceived-luminance dark test (Rec. 601) on the editor background, so the
/// chart's categorical palette + default ink match the editor's light/dark mode.
const fn is_dark(c: editor_core::decoration::Color) -> bool {
    // (r*299 + g*587 + b*114) / 1000 < 128
    let lum = c.r as u32 * 299 + c.g as u32 * 587 + c.b as u32 * 114;
    lum < 128_000
}

/// Build the `hiker-charts` [`ChartTheme`] for the active editor theme: the
/// dark-mode-aware Category10 series palette from `hiker-charts`, with the
/// background, foreground (axis/label ink), and gridline pulled from the editor
/// palette so a rendered chart sits seamlessly on the editor surface and reads
/// in light or dark. status: chart-render, widget-render-theme-color
#[must_use]
pub fn hiker_to_chart_theme(theme: &editor_core::theme::Theme) -> ChartTheme {
    let p = &theme.palette;
    let mut t = ChartTheme::from_dark_mode(is_dark(p.bg));
    t.background = chart_color(p.bg);
    t.foreground = chart_color(p.fg);
    t.gridline = chart_color(p.dim);
    t
}

/// A [`DataResolver`] that maps a chart's `data:` identifier to a vault CSV,
/// resolved the way Hiker resolves links: **relative to the note first**, then
/// the vault root, then — for a bare name — the same by-name match wikilinks use
/// (`hiker_core::wikilink::resolve_path`, nearest-folder to the note). Every read
/// goes through [`Vault::read_file`], so the vault sandbox (no `..` escapes, no
/// symlink hops) is enforced exactly as it is for a note open. status: chart-data-resolver
pub struct VaultDataResolver {
    vault: Vault,
    /// The note's vault-relative path (the referrer), e.g. `analysis/report.md`.
    note_path: String,
    /// The note's directory (`""` for a vault-root note), so a bare `data:
    /// sales.csv` resolves to a sibling first.
    note_dir: String,
}

impl VaultDataResolver {
    /// Construct a resolver bound to the note at `note_path` (vault-relative), so
    /// a relative `data:` reference resolves against the note's own directory.
    #[must_use]
    pub fn new(vault: Vault, note_path: &str) -> Self {
        let note_dir = note_path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
        Self { vault, note_path: note_path.to_string(), note_dir }
    }

    /// Read a vault-relative path's raw text, or `None` if it can't be read.
    /// The vault enforces the sandbox.
    fn try_text(&self, rel: &str) -> Option<String> {
        self.vault.read_file(rel).ok()
    }

    /// The note-relative candidate path for `id` (`note_dir/id`, or just `id`
    /// at vault root). The vault normalizes any `..`/`.` and re-checks the
    /// sandbox, so `../shared/x.csv` is allowed iff it stays inside the vault.
    fn sibling_rel(&self, id: &str) -> String {
        if self.note_dir.is_empty() {
            id.to_string()
        } else {
            format!("{}/{}", self.note_dir, id)
        }
    }

    /// Bare-name fallback: enumerate the vault's CSV files and resolve `id` by
    /// the same nearest-folder name match wikilinks use, so `data: sales.csv`
    /// finds the closest `sales.csv` when no sibling exists. Walks lazily (only
    /// reached when the direct reads miss and `id` carries no `/`).
    fn resolve_by_name(&self, id: &str) -> Option<String> {
        use hiker_core::wikilink::{resolve_path, AmbiguityPolicy, Resolution};
        let paths = self.walk_csv_files();
        match resolve_path(&paths, id, AmbiguityPolicy::NearestFolder, Some(&self.note_path)) {
            Resolution::Resolved(rel) => self.try_text(&rel),
            _ => None,
        }
    }

    /// Every `.csv` file in the vault, vault-relative, honoring the watcher's
    /// ignore list (`target/`, `node_modules/`, dotdirs). Used only by the
    /// bare-name fallback.
    fn walk_csv_files(&self) -> Vec<String> {
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
            if std::path::Path::new(&rel)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("csv"))
            {
                out.push(rel);
            }
        }
        out
    }

    /// Resolve a `data:` identifier to the raw CSV **text** (the inline-block
    /// renderer hashes these bytes into its render-cache key, so it needs the
    /// text, not a parsed table). Resolution order mirrors a Hiker link:
    /// note-relative, then vault-root, then the bare-name wikilink match.
    /// Returns `None` when nothing resolves.
    #[must_use]
    pub fn resolve_text(&self, id: &str) -> Option<String> {
        // 1. Relative to the note's directory (the common case, sandboxed).
        if let Some(t) = self.try_text(&self.sibling_rel(id)) {
            return Some(t);
        }
        // 2. Vault-root-relative (an explicit `data: shared/x.csv` path form).
        if id != self.sibling_rel(id)
            && let Some(t) = self.try_text(id)
        {
            return Some(t);
        }
        // 3. Bare name, nowhere local: the wikilink nearest-folder name match.
        if !id.contains('/') {
            return self.resolve_by_name(id);
        }
        None
    }
}

impl DataResolver for VaultDataResolver {
    fn resolve(&self, id: &str) -> Result<Table, ResolveError> {
        let text =
            self.resolve_text(id).ok_or_else(|| ResolveError(format!("data file not found: {id}")))?;
        Table::from_csv(text.as_bytes()).map_err(|e| ResolveError(format!("data csv: {e}")))
    }
}
