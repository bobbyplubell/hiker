//! The crawl link-rewrite pass. The frontier loop holds the full
//! `URL → child-path` map as it captures pages; once the crawl finishes, this
//! pass rewrites links *among the crawled pages* into the wikilink path form
//! from `docs/wikilinks.md` (`wikilink-path-form`) so a crawled site becomes a
//! real subgraph (backlinks, graph, trails). Same-site links not in the crawl
//! set stay URLs; external links stay URLs. The rewrite emits the syntax
//! regardless of whether wikilink *rendering* has landed — the links become
//! clickable once it does. See `docs/extract.md` `crawl-link-rewrite-wikilinks`.
//!
//! The path form is the shortest-unambiguous one: a bare basename (`[[page]]`)
//! when that basename is unique across the crawl's child set, otherwise the
//! vault-relative path without the `.md` extension (`[[folder/page]]`). The
//! `.md` extension is dropped per `wikilink-path-form`.
//
// status: crawl-link-rewrite-wikilinks

use std::collections::HashMap;
use std::path::Path;

/// The `URL → vault-relative-child-path` map the frontier loop accumulates,
/// plus the wikilink target each path resolves to. Built once; rewrites every
/// captured page's body against it.
#[derive(Debug, Default)]
pub struct LinkMap {
    /// `url → vault-relative path` (e.g. `crawl-job/page.md`).
    by_url: HashMap<String, String>,
    /// `vault-relative path → wikilink target` (shortest-unambiguous, no
    /// `.md`). Derived once all paths are known.
    target: HashMap<String, String>,
}

impl LinkMap {
    /// Build the map from the loop's `(url, vault_rel_path)` pairs and resolve
    /// the shortest-unambiguous wikilink target for each path.
    pub fn new(entries: &[(String, String)]) -> Self {
        let mut by_url = HashMap::new();
        for (url, path) in entries {
            by_url.insert(normalize(url), path.clone());
        }
        let target = resolve_targets(entries.iter().map(|(_, p)| p.as_str()));
        Self { by_url, target }
    }

    /// The wikilink target for a URL that's in the crawl set, else `None`.
    fn target_for(&self, url: &str) -> Option<&str> {
        let path = self.by_url.get(&normalize(url))?;
        self.target.get(path).map(String::as_str)
    }

    /// Rewrite every in-crawl-set link in `markdown` into a wikilink. Markdown
    /// inline links `[text](url)` whose `url` is in the crawl set become
    /// `[[target|text]]` (label preserved per `wikilink-render`); bare URLs in
    /// the crawl set become `[[target]]`. Links not in the set are untouched.
    pub fn rewrite(&self, markdown: &str) -> String {
        let with_inline = self.rewrite_inline_links(markdown);
        self.rewrite_bare_urls(&with_inline)
    }

    /// Rewrite `[text](url)` forms whose target is a crawled page.
    fn rewrite_inline_links(&self, md: &str) -> String {
        let mut out = String::with_capacity(md.len());
        let bytes = md.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if let Some((rendered, end)) = self.try_rewrite_at(md, bytes, i) {
                out.push_str(&rendered);
                i = end;
                continue;
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        out
    }

    /// If a crawled-page inline link `[text](url)` begins at `i`, return its
    /// rendered wikilink and the index just past it; else `None`.
    fn try_rewrite_at(&self, md: &str, bytes: &[u8], i: usize) -> Option<(String, usize)> {
        if bytes[i] != b'[' || starts_wikilink(bytes, i) {
            return None;
        }
        let (text, url, end) = parse_inline_link(md, i)?;
        let target = self.target_for(url)?;
        Some((format_link(target, text), end))
    }

    /// Rewrite bare `http(s)://…` URLs (not already inside a markdown link)
    /// whose target is a crawled page into `[[target]]`.
    fn rewrite_bare_urls(&self, md: &str) -> String {
        let mut out = String::with_capacity(md.len());
        for token in split_keep(md) {
            match self.target_for(token) {
                Some(target) if is_url(token) => out.push_str(&format!("[[{target}]]")),
                _ => out.push_str(token),
            }
        }
        out
    }
}

/// Format a wikilink with a label: `[[target|text]]`, or `[[target]]` when the
/// label is just the target.
fn format_link(target: &str, text: &str) -> String {
    if text == target || text.is_empty() {
        format!("[[{target}]]")
    } else {
        format!("[[{target}|{text}]]")
    }
}

/// Resolve each vault-relative `.md` path to its shortest-unambiguous wikilink
/// target: a bare basename when unique across the set, else the full path
/// (sans `.md`).
fn resolve_targets<'a>(paths: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    let paths: Vec<&str> = paths.collect();
    let mut basename_counts: HashMap<&str, usize> = HashMap::new();
    for p in &paths {
        *basename_counts.entry(basename(p)).or_insert(0) += 1;
    }
    let mut out = HashMap::new();
    for p in &paths {
        let stem = basename(p);
        let target = if basename_counts.get(stem) == Some(&1) {
            stem.to_string()
        } else {
            drop_md_ext(p).to_string()
        };
        out.insert((*p).to_string(), target);
    }
    out
}

/// The basename of a vault-relative path without the `.md` extension.
fn basename(path: &str) -> &str {
    let file = Path::new(path).file_name().and_then(|f| f.to_str()).unwrap_or(path);
    file.strip_suffix(".md").unwrap_or(file)
}

/// Drop a trailing `.md` from a path, leaving the vault-relative form.
fn drop_md_ext(path: &str) -> &str {
    path.strip_suffix(".md").unwrap_or(path)
}

/// Normalize a URL for map keying: strip a trailing slash and the fragment so
/// `…/x`, `…/x/`, and `…/x#sec` resolve to the same captured page.
fn normalize(url: &str) -> String {
    let no_frag = url.split('#').next().unwrap_or(url);
    no_frag.strip_suffix('/').unwrap_or(no_frag).to_string()
}

/// Whether the bracket at `i` begins a `[[wikilink]]` (so we don't re-rewrite).
fn starts_wikilink(bytes: &[u8], i: usize) -> bool {
    bytes.get(i + 1) == Some(&b'[')
}

/// Parse a markdown inline link `[text](url)` starting at `open` (the `[`).
/// Returns `(text, url, end_index_exclusive)` or `None` if it isn't one.
fn parse_inline_link(md: &str, open: usize) -> Option<(&str, &str, usize)> {
    let rest = &md[open..];
    let close_text = rest.find(']')?;
    if rest.as_bytes().get(close_text + 1) != Some(&b'(') {
        return None;
    }
    let close_url_rel = rest[close_text + 2..].find(')')?;
    let text = &rest[1..close_text];
    let url = &rest[close_text + 2..close_text + 2 + close_url_rel];
    let end = open + close_text + 2 + close_url_rel + 1;
    Some((text, url, end))
}

/// Split text into alternating non-separator tokens and separator runs,
/// preserving everything so `concat()` over the result reconstructs the input
/// byte-for-byte. A separator is whitespace or one of `<>()[]"'`` — enough to
/// isolate a bare URL token from the punctuation around it.
fn split_keep(s: &str) -> Vec<&str> {
    const fn is_sep(ch: char) -> bool {
        ch.is_whitespace() || matches!(ch, '<' | '>' | '(' | ')' | '[' | ']' | '"' | '\'' | '`')
    }
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev_sep: Option<bool> = None;
    for (i, ch) in s.char_indices() {
        let sep = is_sep(ch);
        if prev_sep == Some(!sep) {
            out.push(&s[start..i]);
            start = i;
        }
        prev_sep = Some(sep);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Whether a token looks like an http(s) URL.
fn is_url(token: &str) -> bool {
    token.starts_with("http://") || token.starts_with("https://")
}
