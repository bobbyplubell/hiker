//! Wikilink syntax: parsing and path-form resolution for `[[<path>]]`
//! references.
//!
//! Under the path-based identity model (`wikilink-path-form`), a wikilink's
//! target is a vault-relative path: `[[Name]]` when the basename is unique
//! in the vault, `[[folder/sub/Name]]` (no `.md` extension) when it isn't.
//! There is no ULID form, no `|display` alias half — what the user types
//! is what's stored and what's on disk.
//!
//! This module is the single place that understands the `[[…]]` byte
//! syntax: shared by the editor decoration resolver, the backlinks scan,
//! and the rename-rewrite pass. It is pure (no vault, store, or watcher
//! access), so the same parsing rules apply across the app, CLI, and MCP
//! surfaces.

use std::ops::Range;

/// A `[[…]]` reference located in a note body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// Byte span of the full `[[…]]` token, including both bracket pairs.
    pub span: Range<usize>,
    /// The link body verbatim — either a bare basename (`Name`) or a
    /// vault-relative path without the `.md` extension (`folder/sub/Name`).
    pub target: String,
}

/// Scan `text` for every well-formed `[[…]]` reference. A reference is a `[[`
/// followed by a `]]` on the same line with no nested `]` in between — the
/// same lenient rule the live-preview decoration uses, so what parses here is
/// exactly what renders as a link. An empty body (`[[]]`) is skipped.
#[must_use]
pub fn parse_links(text: &str) -> Vec<ParsedLink> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let inner_start = i + 2;
        let mut j = inner_start;
        let mut close = None;
        while j + 1 < bytes.len() {
            if bytes[j] == b'\n' {
                break;
            }
            if bytes[j] == b']' && bytes[j + 1] == b']' {
                close = Some(j);
                break;
            }
            j += 1;
        }
        let Some(close_start) = close else {
            i += 1;
            continue;
        };
        let inner = &text[inner_start..close_start];
        if inner.is_empty() || inner.contains(']') {
            i = close_start + 2;
            continue;
        }
        out.push(ParsedLink {
            span: i..close_start + 2,
            target: inner.trim().to_string(),
        });
        i = close_start + 2;
    }
    out
}

/// The display title hiker shows for a vault-relative path: the filename stem
/// with the `.md` extension stripped. Until frontmatter-title parsing lands
/// this mirrors the related-notes / search title convention.
#[must_use]
pub fn title_for_path(rel: &str) -> &str {
    rel.rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".md")
}

/// Split a link target into its page part and an optional `#section` anchor.
///
/// A wikilink / markdown-link target may carry a heading anchor after a `#`:
/// `Page#Heading`, `folder/Page#Heading`, or a same-document `#Heading` (empty
/// page). The returned page is the part before the first `#` (trimmed); the
/// section is everything after it (trimmed), or `None` when there is no `#`.
/// An empty page (a bare `#Heading`) signals "this document" to the caller.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn split_target_section(target: &str) -> (&str, Option<&str>) {
    match target.split_once('#') {
        Some((page, section)) => (page.trim(), Some(section.trim())),
        None => (target.trim(), None),
    }
}

/// Slugify a heading's text the way GitHub anchors do: lowercase, drop every
/// character that is not an ASCII alphanumeric, a space, or a hyphen, then
/// turn runs of whitespace into single hyphens. Used to match a link's
/// `#section` anchor against the headings in a note body.
///
/// This is intentionally the GitHub algorithm rather than a bespoke scheme so a
/// `#section` authored against rendered-Markdown expectations (e.g. copied from
/// a web view) resolves the same way here.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn heading_slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_hyphen = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            // Collapse any whitespace run; emit one hyphen lazily so trailing
            // whitespace never produces a trailing hyphen.
            pending_hyphen = !out.is_empty();
        } else if ch == '-' || ch.is_ascii_alphanumeric() {
            if pending_hyphen {
                out.push('-');
                pending_hyphen = false;
            }
            out.extend(ch.to_lowercase());
        }
        // All other characters (punctuation, symbols) are dropped.
    }
    out
}

/// Byte offset of the start of the first ATX heading line (`#`…`######`) in
/// `text` whose slug equals `section`'s slug, or `None` when no heading
/// matches. The match is slug-based (`heading_slug`), so the link's `#section`
/// anchor need not be byte-identical to the heading text — only slug-equal.
///
/// Fenced code blocks are skipped so a `#`-prefixed line inside a ``` fence is
/// never mistaken for a heading. The returned offset is the byte index of the
/// heading line's first character (its leading `#`), suitable for placing the
/// caret + scrolling the heading to the top of the viewport.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn find_heading_byte(text: &str, section: &str) -> Option<usize> {
    let want = heading_slug(section);
    if want.is_empty() {
        return None;
    }
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let trimmed = trimmed.trim_end();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence
            && let Some(rest) = heading_text(trimmed)
            && heading_slug(rest) == want
        {
            return Some(offset + indent);
        }
        offset += line.len();
    }
    None
}

/// If `line` is an ATX heading (`#`…`######` followed by a space or end of
/// line), return the heading text after the markers; otherwise `None`. A run of
/// more than six `#` is not a heading per CommonMark.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some(rest);
    }
    rest.strip_prefix(' ').map(str::trim)
}

/// Ambiguity policy for bare-name links that match more than one path.
/// Mirrors `[wikilinks] ambiguous_resolution` in user/vault config.
///
/// status: wikilink-ambiguous-resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmbiguityPolicy {
    /// Render bare-name collisions as broken; the user disambiguates via
    /// the picker. The default — never guess silently.
    #[default]
    Unresolved,
    /// Resolve to the lexicographically-first matching path.
    LexFirst,
    /// Resolve to the match with the longest shared path prefix with the
    /// referrer; ties broken lex-first.
    NearestFolder,
}

/// Outcome of resolving a `[[…]]` target against the vault's indexable paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The target resolved to exactly this path.
    Resolved(String),
    /// No path matches the target. The link renders unresolved; clicking
    /// offers to create a note at the inferred location.
    Unresolved,
    /// The bare-name target matches multiple paths and the policy is
    /// `Unresolved`; the link renders broken with a disambiguation
    /// affordance. Never produced under `LexFirst` / `NearestFolder`
    /// (those always pick a winner when there's at least one match).
    Ambiguous(Vec<String>),
}

/// Resolve a wikilink `target` against the vault's indexable `paths`. An
/// explicit-path target (`folder/sub/Name`) resolves to exactly
/// `folder/sub/Name.md`; a bare-name target (`Name`) matches every note
/// whose basename equals it. Ambiguity is handled per `policy`.
/// `referrer` is the path of the linking note — only consulted by
/// `NearestFolder`; pass `None` if irrelevant.
///
/// status: wikilink-resolve
/// status: wikilink-ambiguous-resolution
#[must_use]
pub fn resolve_path(
    paths: &[String],
    target: &str,
    policy: AmbiguityPolicy,
    referrer: Option<&str>,
) -> Resolution {
    // Drop any `#section` anchor before resolving the page: the section only
    // affects where navigation lands, not which note the link points at.
    // status: wikilink-headings-blocks
    let (page, _section) = split_target_section(target);
    let needle = page.trim_end_matches(".md");
    if needle.is_empty() {
        return Resolution::Unresolved;
    }
    if needle.contains('/') {
        // Explicit-path: must match a vault path exactly (modulo `.md`).
        let with_ext = format!("{needle}.md");
        for rel in paths {
            if rel == &with_ext {
                return Resolution::Resolved(rel.clone());
            }
        }
        return Resolution::Unresolved;
    }
    // Bare-name: collect every path whose basename (without `.md`) equals.
    let mut matches: Vec<String> = paths
        .iter()
        .filter(|rel| title_for_path(rel) == needle)
        .cloned()
        .collect();
    match matches.len() {
        0 => Resolution::Unresolved,
        1 => Resolution::Resolved(matches.remove(0)),
        _ => match policy {
            AmbiguityPolicy::Unresolved => {
                matches.sort();
                Resolution::Ambiguous(matches)
            }
            AmbiguityPolicy::LexFirst => {
                matches.sort();
                Resolution::Resolved(matches.remove(0))
            }
            AmbiguityPolicy::NearestFolder => {
                matches.sort();
                let referrer_dir = referrer
                    .and_then(|r| r.rsplit_once('/'))
                    .map_or("", |(d, _)| d);
                // Pick the match sharing the longest path-segment prefix
                // with the referrer's folder; ties already lex-sorted.
                let best = matches
                    .iter()
                    .max_by_key(|m| shared_prefix_segments(referrer_dir, m))
                    .cloned()
                    .unwrap_or_else(|| matches.remove(0));
                Resolution::Resolved(best)
            }
        },
    }
}

/// Count of leading path segments shared by two vault-relative paths.
fn shared_prefix_segments(a: &str, b: &str) -> usize {
    let a_segs = a.split('/').filter(|s| !s.is_empty());
    let b_segs = b.split('/').filter(|s| !s.is_empty());
    a_segs.zip(b_segs).take_while(|(x, y)| x == y).count()
}

/// Shortest unambiguous insert form for `target` against the vault's
/// `paths`. Returns the bare basename when it's unique; otherwise
/// extends with the minimum number of leading folder segments needed
/// to disambiguate. Always returns the form without the `.md`
/// extension (the wikilink syntax doesn't carry it).
///
/// status: wikilink-autocomplete
#[must_use]
pub fn shortest_unambiguous(paths: &[String], target: &str) -> String {
    let target_clean = target.trim_end_matches(".md");
    let basename = title_for_path(target_clean);
    let collisions: Vec<&String> = paths
        .iter()
        .filter(|p| {
            p.as_str() != target_clean
                && p.as_str() != format!("{target_clean}.md")
                && title_for_path(p) == basename
        })
        .collect();
    if collisions.is_empty() {
        return basename.to_string();
    }
    // Walk the target's path segments from the right, growing the prefix
    // until no collision shares the same suffix.
    let target_segs: Vec<&str> = target_clean.split('/').collect();
    for take in 2..=target_segs.len() {
        let suffix = target_segs[target_segs.len() - take..].join("/");
        let collides = collisions.iter().any(|c| {
            let c_clean = c.trim_end_matches(".md");
            let c_segs: Vec<&str> = c_clean.split('/').collect();
            c_segs.len() >= take
                && c_segs[c_segs.len() - take..].join("/") == suffix
        });
        if !collides {
            return suffix;
        }
    }
    target_clean.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_links() {
        let links = parse_links("see [[Alpha]] and [[work/Beta]] end");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "Alpha");
        assert_eq!(links[1].target, "work/Beta");
        assert_eq!(&"see [[Alpha]] and [[work/Beta]] end"[links[0].span.clone()], "[[Alpha]]");
    }

    #[test]
    fn pipe_in_body_is_part_of_target_under_path_form() {
        // Pipe is no longer an alias separator: the entire body is the
        // target. Most real links won't contain `|`; if one does, it
        // simply renders unresolved (no path contains a pipe in normal use).
        let links = parse_links("[[Beta|the beta]]");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "Beta|the beta");
    }

    #[test]
    fn skips_unterminated_and_nested() {
        assert!(parse_links("[[no close").is_empty());
        assert!(parse_links("[[a]b]]").is_empty());
        assert!(parse_links("[[multi\nline]]").is_empty());
        assert!(parse_links("[[]]").is_empty());
    }

    #[test]
    fn resolves_bare_name_unique() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Beta.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("notes/Alpha.md".to_string()),
        );
    }

    #[test]
    fn resolves_explicit_path() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "work/Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("work/Alpha.md".to_string()),
        );
        assert_eq!(
            resolve_path(&paths, "missing/Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn ambiguous_unresolved_policy_lists_matches() {
        let paths = vec!["work/Beta.md".to_string(), "personal/Beta.md".to_string()];
        match resolve_path(&paths, "Beta", AmbiguityPolicy::Unresolved, None) {
            Resolution::Ambiguous(ms) => {
                assert_eq!(ms, vec!["personal/Beta.md", "work/Beta.md"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_lex_first_policy_picks_first() {
        let paths = vec!["work/Beta.md".to_string(), "personal/Beta.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Beta", AmbiguityPolicy::LexFirst, None),
            Resolution::Resolved("personal/Beta.md".to_string()),
        );
    }

    #[test]
    fn ambiguous_nearest_folder_uses_referrer() {
        let paths = vec![
            "work/projects/Beta.md".to_string(),
            "personal/Beta.md".to_string(),
        ];
        assert_eq!(
            resolve_path(
                &paths,
                "Beta",
                AmbiguityPolicy::NearestFolder,
                Some("work/projects/intro.md"),
            ),
            Resolution::Resolved("work/projects/Beta.md".to_string()),
        );
        assert_eq!(
            resolve_path(
                &paths,
                "Beta",
                AmbiguityPolicy::NearestFolder,
                Some("personal/notes/intro.md"),
            ),
            Resolution::Resolved("personal/Beta.md".to_string()),
        );
    }

    #[test]
    fn unresolved_when_no_match() {
        let paths = vec!["notes/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Gamma", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn shortest_unambiguous_picks_bare_when_unique() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Beta.md".to_string()];
        assert_eq!(shortest_unambiguous(&paths, "notes/Alpha.md"), "Alpha");
    }

    #[test]
    fn shortest_unambiguous_extends_to_disambiguate() {
        let paths = vec![
            "work/Beta.md".to_string(),
            "personal/Beta.md".to_string(),
        ];
        assert_eq!(shortest_unambiguous(&paths, "work/Beta.md"), "work/Beta");
        assert_eq!(shortest_unambiguous(&paths, "personal/Beta.md"), "personal/Beta");
    }

    #[test]
    fn splits_target_section() {
        assert_eq!(split_target_section("Page#Heading"), ("Page", Some("Heading")));
        assert_eq!(split_target_section("folder/Page#H"), ("folder/Page", Some("H")));
        assert_eq!(split_target_section("#Heading"), ("", Some("Heading")));
        assert_eq!(split_target_section("Page"), ("Page", None));
        assert_eq!(split_target_section(" Page # H "), ("Page", Some("H")));
    }

    #[test]
    fn heading_slug_matches_github_algorithm() {
        assert_eq!(heading_slug("Hello World"), "hello-world");
        assert_eq!(heading_slug("  Trim Me  "), "trim-me");
        assert_eq!(heading_slug("Mixed CASE 123"), "mixed-case-123");
        assert_eq!(heading_slug("Punctuation: drop! it?"), "punctuation-drop-it");
        assert_eq!(heading_slug("multiple   spaces"), "multiple-spaces");
        assert_eq!(heading_slug("already-hyphenated"), "already-hyphenated");
        assert_eq!(heading_slug("symbols *& removed"), "symbols-removed");
        assert_eq!(heading_slug(""), "");
        assert_eq!(heading_slug("!!!"), "");
    }

    #[test]
    fn finds_heading_byte_by_slug() {
        let text = "# Intro\nbody\n\n## Sub Section\nmore\n### Deep Heading!\nx\n";
        // Heading line offsets: "# Intro" at 0, "## Sub Section" after.
        assert_eq!(find_heading_byte(text, "Intro"), Some(0));
        let sub = text.find("## Sub Section").unwrap();
        assert_eq!(find_heading_byte(text, "Sub Section"), Some(sub));
        // Slug-equal but not byte-equal anchor still matches.
        assert_eq!(find_heading_byte(text, "sub-section"), Some(sub));
        let deep = text.find("### Deep Heading!").unwrap();
        // The anchor drops the `!` to match the heading's slug.
        assert_eq!(find_heading_byte(text, "Deep Heading"), Some(deep));
        // No matching heading.
        assert_eq!(find_heading_byte(text, "Missing"), None);
        // Empty section never matches.
        assert_eq!(find_heading_byte(text, ""), None);
    }

    #[test]
    fn find_heading_skips_fenced_code() {
        let text = "intro\n```\n# Not A Heading\n```\n# Real Heading\n";
        let real = text.find("# Real Heading").unwrap();
        assert_eq!(find_heading_byte(text, "Not A Heading"), None);
        assert_eq!(find_heading_byte(text, "Real Heading"), Some(real));
    }

    #[test]
    fn resolve_path_ignores_section_anchor() {
        let paths = vec!["notes/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Alpha#Some Heading", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("notes/Alpha.md".to_string()),
        );
        // A same-doc `#Heading` (empty page) has no note target.
        assert_eq!(
            resolve_path(&paths, "#Some Heading", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn shortest_unambiguous_grows_to_full_when_needed() {
        let paths = vec![
            "a/sub/Note.md".to_string(),
            "b/sub/Note.md".to_string(),
        ];
        assert_eq!(shortest_unambiguous(&paths, "a/sub/Note.md"), "a/sub/Note");
    }
}
