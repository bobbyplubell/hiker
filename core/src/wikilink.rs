//! Wikilink syntax: parsing, ULID detection, and name→note resolution for
//! `[[<target>]]` / `[[<target>|<display>]]` references.
//!
//! The durable stored target is a note's ULID (stable across renames and
//! hiker's own auto-moves); links may also be authored as plain names
//! (hand-typed or written in an external editor) and get normalized to the
//! ULID form on save. This module is the single place that understands the
//! `[[…]]` byte syntax — shared by the editor decoration resolver, the
//! save-time normalizer (`core::ops::buffer::normalize_wikilinks`), and the
//! backlinks scan. It is pure: no vault, store, or watcher access, so the
//! same parsing rules apply across the app, CLI, and MCP surfaces.

use std::ops::Range;

/// A `[[…]]` reference located in a note body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// Byte span of the full `[[…]]` token, including both bracket pairs.
    pub span: Range<usize>,
    /// Text before the `|` — a ULID (durable form) or a name (to normalize).
    pub target: String,
    /// Text after the `|`, if any — the at-write-time display fallback.
    pub display: Option<String>,
}

impl ParsedLink {
    /// True when the target is a stored ULID rather than a hand-typed name.
    #[must_use]
    pub fn is_id_form(&self) -> bool {
        looks_like_ulid(&self.target)
    }
}

/// Scan `text` for every well-formed `[[…]]` reference. A reference is a `[[`
/// followed by a `]]` on the same line with no nested `]` in between — the
/// same lenient rule the live-preview decoration uses, so what parses here is
/// exactly what renders as a link.
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
        let (target, display) = match inner.split_once('|') {
            Some((t, d)) => (t.trim().to_string(), Some(d.trim().to_string())),
            None => (inner.trim().to_string(), None),
        };
        out.push(ParsedLink {
            span: i..close_start + 2,
            target,
            display,
        });
        i = close_start + 2;
    }
    out
}

/// True if `s` is a syntactically valid Crockford-base32 ULID: 26 chars, each
/// a digit or an uppercase letter excluding `I`, `L`, `O`, `U`. Used to tell a
/// stored id target apart from a hand-typed name without a store round-trip.
#[must_use]
pub fn looks_like_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes().all(|b| {
            b.is_ascii_digit()
                || (b.is_ascii_uppercase() && !matches!(b, b'I' | b'L' | b'O' | b'U'))
        })
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

/// Outcome of resolving a hand-typed name to a vault note by filename-stem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameResolution {
    /// Exactly one note's title matches.
    Unique(String),
    /// No note matches the name.
    None,
    /// More than one note shares the title — hiker never guesses between
    /// same-titled notes; the user disambiguates via the picker.
    Ambiguous,
}

/// Resolve a typed `name` against the vault's indexable paths by
/// case-insensitive filename-stem match. The match also accepts a name that
/// already carries a `.md` suffix or that names a full vault-relative path, so
/// links authored in other editors against paths still resolve.
#[must_use]
pub fn resolve_name(paths: &[String], name: &str) -> NameResolution {
    let needle = name.trim().trim_end_matches(".md").to_lowercase();
    if needle.is_empty() {
        return NameResolution::None;
    }
    let mut hit: Option<&String> = None;
    for rel in paths {
        let stem = title_for_path(rel).to_lowercase();
        let full = rel.trim_end_matches(".md").to_lowercase();
        if stem == needle || full == needle {
            match hit {
                None => hit = Some(rel),
                Some(prev) if prev == rel => {}
                Some(_) => return NameResolution::Ambiguous,
            }
        }
    }
    match hit {
        Some(rel) => NameResolution::Unique(rel.clone()),
        None => NameResolution::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_aliased_links() {
        let links = parse_links("see [[Alpha]] and [[Beta|the beta]] end");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "Alpha");
        assert_eq!(links[0].display, None);
        assert_eq!(&"see [[Alpha]] and [[Beta|the beta]] end"[links[0].span.clone()], "[[Alpha]]");
        assert_eq!(links[1].target, "Beta");
        assert_eq!(links[1].display.as_deref(), Some("the beta"));
    }

    #[test]
    fn skips_unterminated_and_nested() {
        assert!(parse_links("[[no close").is_empty());
        assert!(parse_links("[[a]b]]").is_empty());
        assert!(parse_links("[[multi\nline]]").is_empty());
    }

    #[test]
    fn ulid_detection() {
        assert!(looks_like_ulid("01HRX3ABCDEFGHJKMNPQRSTVWX"));
        assert!(!looks_like_ulid("Some Name"));
        assert!(!looks_like_ulid("01hrx3abcdefghjkmnpqrstvwx")); // lowercase
        assert!(!looks_like_ulid("01HRX3")); // too short
        assert!(!looks_like_ulid("01HRX3ABCDEFGHJKMNPQRSTVWI")); // contains I
    }

    #[test]
    fn id_form_classifies_links() {
        let links = parse_links("[[01HRX3ABCDEFGHJKMNPQRSTVWX|Title]] [[Plain Name]]");
        assert!(links[0].is_id_form());
        assert!(!links[1].is_id_form());
    }

    #[test]
    fn resolves_name_by_stem_path_and_ambiguity() {
        let paths = vec![
            "notes/Alpha.md".to_string(),
            "work/Beta.md".to_string(),
            "personal/Beta.md".to_string(),
        ];
        assert_eq!(resolve_name(&paths, "Alpha"), NameResolution::Unique("notes/Alpha.md".to_string()));
        assert_eq!(resolve_name(&paths, "notes/Alpha"), NameResolution::Unique("notes/Alpha.md".to_string()));
        assert_eq!(resolve_name(&paths, "Beta"), NameResolution::Ambiguous);
        assert_eq!(resolve_name(&paths, "Gamma"), NameResolution::None);
    }
}
