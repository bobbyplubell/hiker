//! The manifest-import escape hatch. For JS-heavy sites that genuinely need a
//! real browser, the user runs their own crawler (Playwright, Scrapy, …) and
//! hands hiker a directory of `{ markdown + archives + manifest.json }`. Hiker
//! imports it: places the child notes into the job's companion folder, builds
//! the `URL → child-path` map from the manifest, runs the *same* wikilink
//! rewrite the in-process loop uses, and stamps `hiker.parent`. The external
//! tool owns the messy fetching; hiker owns the vault integration — JS never
//! runs in hiker. See `docs/extract.md` `extract-manifest-import`.
//
// status: extract-manifest-import

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::companion::{write_child, ChildWrite};

use super::wikilink::LinkMap;
use super::{Error, Report, PageRecord};

/// The `manifest.json` shape the external crawler produces: one entry per
/// captured page. `links` is the set of in-crawl URLs found on the page (used
/// to seed the wikilink rewrite, same as the in-process loop's `next_urls`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub pages: Vec<Page>,
}

/// One page entry in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page {
    /// The page's source URL (the wikilink map key).
    pub url: String,
    /// The markdown file for this page, relative to the manifest directory.
    pub output_file: String,
    /// The page title, if the external tool recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// An optional archive file (self-contained HTML), relative to the
    /// manifest directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_file: Option<String>,
    /// In-crawl links found on this page (for the wikilink rewrite). Unused
    /// links resolve to nothing and stay URLs.
    #[serde(default)]
    pub links: Vec<String>,
}

/// Import an external crawler's output directory into the job note's companion
/// folder. `import_dir` holds the `manifest.json` + markdown + archives;
/// `companion_dir` is the job note's `<name>/` folder; `parent_ulid` is the
/// job note's id stamped on every child. Builds the `URL → child-path` map,
/// runs the shared wikilink rewrite, and writes the children. Returns the
/// same [`Report`] the in-process loop produces, so callers handle both
/// paths identically.
///
/// status: extract-manifest-import
pub fn import_dir(
    import_dir: &Path,
    companion_dir: &Path,
    vault_root: &Path,
    parent_ulid: &str,
) -> Result<Report, Error> {
    let manifest_path = import_dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| Error::Manifest(format!("read {}: {e}", manifest_path.display())))?;
    let manifest: Manifest = serde_json::from_str(&raw)
        .map_err(|e| Error::Manifest(format!("parse manifest.json: {e}")))?;

    // First pass: read each page's markdown and assign it a child path, so the
    // URL → child-path map is complete before any rewrite runs.
    let mut staged: Vec<StagedPage> = Vec::new();
    let mut url_to_path: Vec<(String, String)> = Vec::new();
    for page in &manifest.pages {
        let md_path = import_dir.join(&page.output_file);
        let markdown = std::fs::read_to_string(&md_path)
            .map_err(|e| Error::Manifest(format!("read {}: {e}", md_path.display())))?;
        let stem = child_stem(page);
        let rel = companion_rel(companion_dir, vault_root, &stem);
        url_to_path.push((page.url.clone(), rel.clone()));
        staged.push(StagedPage { page, markdown, stem });
    }

    let link_map = LinkMap::new(&url_to_path);

    // Second pass: rewrite + write each child with the parent stamp.
    let mut report = Report::default();
    for s in staged {
        let rewritten = link_map.rewrite(&s.markdown);
        let archive_bytes = s
            .page
            .archive_file
            .as_ref()
            .and_then(|f| std::fs::read(import_dir.join(f)).ok());
        let child = ChildWrite {
            companion_dir,
            stem: &s.stem,
            markdown: &rewritten,
            title: s.page.title.as_deref(),
            source_url: &s.page.url,
            parent_ulid,
            provenance: "web-crawl",
            archive: archive_bytes.as_deref(),
        };
        let path = write_child(&child).map_err(|e| Error::Write(e.to_string()))?;
        report.pages.push(PageRecord {
            url: s.page.url.clone(),
            path: Some(path),
            depth: 0,
            note: "imported".to_string(),
        });
    }
    Ok(report)
}

/// A page read in the first pass, awaiting rewrite + write.
struct StagedPage<'a> {
    page: &'a Page,
    markdown: String,
    stem: String,
}

/// The filename stem for an imported child: a slug of the title, else of the
/// URL path, else the output filename stem.
fn child_stem(page: &Page) -> String {
    if let Some(title) = page.title.as_deref() {
        let s = crate::sidecar::slugify(title);
        if !s.is_empty() {
            return s;
        }
    }
    let s = crate::sidecar::slugify(&page.url);
    if !s.is_empty() {
        s
    } else {
        Path::new(&page.output_file)
            .file_stem()
            .and_then(|f| f.to_str())
            .unwrap_or("page")
            .to_string()
    }
}

/// The vault-relative path a child note at `<companion>/<stem>.md` will have
/// (for the wikilink map). Falls back to `<stem>.md` when the companion dir
/// isn't under the vault root.
fn companion_rel(companion_dir: &Path, vault_root: &Path, stem: &str) -> String {
    let abs = companion_dir.join(format!("{stem}.md"));
    abs.strip_prefix(vault_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| format!("{stem}.md"))
}

#[cfg(test)]
mod tests {
    use super::Manifest;

    #[test]
    fn manifest_round_trips_through_serde() {
        // A manifest with one fully-populated page and one minimal page (the
        // `Option` fields elided), so the round-trip exercises both the
        // `skip_serializing_if` elision and the `default` re-read.
        let json = r#"{
            "pages": [
                {
                    "url": "https://example.com/a",
                    "output_file": "a.md",
                    "title": "Page A",
                    "archive_file": "a.html",
                    "links": ["https://example.com/b"]
                },
                {
                    "url": "https://example.com/b",
                    "output_file": "b.md",
                    "links": []
                }
            ]
        }"#;

        let manifest: Manifest = serde_json::from_str(json).expect("parse");
        let serialized = serde_json::to_string(&manifest).expect("serialize");
        let reparsed: Manifest = serde_json::from_str(&serialized).expect("re-parse");

        assert_eq!(reparsed.pages.len(), 2);
        assert_eq!(reparsed.pages[0].url, "https://example.com/a");
        assert_eq!(reparsed.pages[0].title.as_deref(), Some("Page A"));
        assert_eq!(reparsed.pages[0].archive_file.as_deref(), Some("a.html"));
        assert_eq!(reparsed.pages[0].links, vec!["https://example.com/b"]);
        assert_eq!(reparsed.pages[1].title, None);
        assert_eq!(reparsed.pages[1].archive_file, None);

        // The elided fields must not appear in the wire form for the minimal page.
        assert!(!serialized.contains("\"title\":null"));
        assert!(!serialized.contains("\"archive_file\":null"));
    }
}
