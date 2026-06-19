//! Comment-claimed spec edges: crawl `// status: <slug>` markers in source and resolve each to
//! the symbol it tags via [`ScipAdapter::def_at_line`]. Feeds reconcile's prune keep-set: a
//! store edge is authored if EITHER a doc link line OR a status comment claims it, so pruning on
//! doc links alone would silently delete every comment-claimed edge. (The one-time `seed_specs`
//! example that bootstrapped the docs from these markers is retired; the markers remain as
//! grep-able implementation pointers and prune protection.) status: spec-seed-from-comments

use std::path::{Path, PathBuf};

use spec_engine::NodeHandle;

use crate::ScipAdapter;

/// One `// status: <slug>` marker resolved to the symbol it tags.
pub struct CommentSeed {
    pub slug: String,
    pub handle: NodeHandle,
    /// "implements" | "verifies" | "touches" — see [`relation_of`].
    pub relation: &'static str,
    /// The tagged node's `code:*` kind.
    pub kind: String,
}

/// Everything a comment crawl saw. `seeds` are the markers that resolved to a symbol; `markers`
/// is EVERY marker as `(slug, repo-relative file)`, resolved or not — the difference is markers
/// the index can't place (stale index lines, or comments far from any def). Prune keys off
/// `markers`, not `seeds`: a marker that stopped resolving because the index lags the working
/// tree is still a live authored claim, not staleness to collect.
pub struct CommentCrawl {
    pub seeds: Vec<CommentSeed>,
    pub markers: Vec<(String, String)>,
}

impl CommentCrawl {
    pub fn seen(&self) -> usize {
        self.markers.len()
    }
    pub fn resolved(&self) -> usize {
        self.seeds.len()
    }
}

/// Classify the relation by the symbol a marker lands on: tagged tests *verify*, file-top module
/// markers *touch* (coarse), everything precise *implements*. Keeps the precise `implements`
/// drift signal clean of the noisy module-level mass. status: spec-relation-typing
pub fn relation_of(kind: &str, moniker: &str) -> &'static str {
    if moniker.contains("/tests/") {
        "verifies"
    } else if kind == "code:module" {
        "touches"
    } else {
        "implements"
    }
}

/// `// status: foo-bar` / `/// status: foo-bar` / `//! status: foo-bar`
/// → `foo-bar`. Accepts plain line comments and both doc-comment markers
/// (`///` outer, `//!` inner) so module-level `//! status:` / `//! touches:`
/// markers are crawled, not dropped.
pub fn status_slug_in(line: &str) -> Option<String> {
    let i = line.find("status:")?;
    let pre = line[..i].trim_end();
    // Accept the plain line comment `//` and both doc-comment markers,
    // `///` (outer) and `//!` (inner) — so module-level `//! status:`
    // markers are crawled, not dropped. (`pre` is everything before
    // `status:`, trimmed, so the marker is its trailing token.)
    let qualifies =
        pre.ends_with("//") || pre.ends_with("///") || pre.ends_with("//!");
    if !qualifies {
        return None;
    }
    let tok: String = line[i + 7..]
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    (!tok.is_empty()).then_some(tok)
}

fn walk_rs(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if !p.ends_with("target") {
                walk_rs(&p, out);
            }
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Crawl `.rs` files under `repo/<scope[i]>` for status markers and resolve each to a seed.
pub fn comment_seeds(ad: &ScipAdapter, repo: &Path, scope: &[&str]) -> CommentCrawl {
    let mut crawl = CommentCrawl { seeds: Vec::new(), markers: Vec::new() };
    for top in scope {
        let mut files = Vec::new();
        walk_rs(&repo.join(top), &mut files);
        for f in files {
            let rel = f.strip_prefix(repo).unwrap_or(&f).to_string_lossy().to_string();
            let Ok(text) = std::fs::read_to_string(&f) else { continue };
            for (ln, line) in text.lines().enumerate() {
                let Some(slug) = status_slug_in(line) else { continue };
                crawl.markers.push((slug.clone(), rel.clone()));
                if let Some(handle) = ad.def_at_line(&rel, ln as u32) {
                    let kind = ad.kind_of(&handle.id).unwrap_or_default().to_string();
                    let relation = relation_of(&kind, &handle.id);
                    crawl.seeds.push(CommentSeed { slug, handle, relation, kind });
                }
            }
        }
    }
    crawl
}

#[cfg(test)]
mod tests {
    use super::{relation_of, status_slug_in};

    #[test]
    fn status_slugs_parse_from_comment_lines_only() {
        assert_eq!(status_slug_in("// status: spec-code-link"), Some("spec-code-link".into()));
        assert_eq!(status_slug_in("/// status: a-b2"), Some("a-b2".into()));
        // Inner doc-comment (`//!`) markers — module-level `//! status:` /
        // `//! touches:` must be crawled, not dropped.
        assert_eq!(status_slug_in("//! status: spec-seed-from-comments"), Some("spec-seed-from-comments".into()));
        assert_eq!(status_slug_in("    //! status: mod-level trailing"), Some("mod-level".into()));
        assert_eq!(status_slug_in("    // status: x-y trailing words"), Some("x-y".into()));
        assert_eq!(status_slug_in("let status: u32 = 1;"), None, "not a comment");
        assert_eq!(status_slug_in("// status:"), None, "empty slug");
        assert_eq!(status_slug_in("//! status:"), None, "empty slug, inner doc");
    }

    #[test]
    fn relations_classify_tests_modules_and_bodies() {
        assert_eq!(relation_of("code:function", "pkg/tests/parse/round_trip()."), "verifies");
        assert_eq!(relation_of("code:module", "pkg/vault/"), "touches");
        assert_eq!(relation_of("code:function", "pkg/vault/resolve()."), "implements");
    }
}
