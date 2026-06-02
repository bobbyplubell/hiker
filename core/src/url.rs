//! Link/URL classification: the single place that decides *what kind of
//! target* any link string hiker encounters refers to.
//!
//! Hiker meets link strings from several surfaces — `[[wikilinks]]` in note
//! bodies, `http(s)`/`mailto:` URLs, `zim://` references inside an offline
//! archive, and bare or path-like note references (e.g. from a diagram label
//! or a federated search hit). Historically each surface re-derived "is this
//! external? is this a zim link? is this a note?" with its own ad-hoc string
//! checks (`href.contains("://")`, `strip_prefix("zim://")`, the `[[…]]`
//! scanner). [`classify`] unifies that shape detection so every caller maps the
//! same [`LinkTarget`] onto its own action (open file, ZimView tab, OS opener,
//! wikilink resolve).
//!
//! This module is pure: scheme/shape detection only. It performs no filesystem
//! access, touches no store or vault, and pulls in no UI. Anything that needs
//! the world — *does this note exist?*, *which path does this wikilink
//! resolve to?* — is the caller's resolve step. The optional [`resolve_path`]
//! helper keeps that boundary by taking the lookup as a closure rather than
//! depending on the store. This mirrors the "single place that understands the
//! syntax" discipline `core::wikilink` already follows for `[[…]]` bytes.
//!
//! status: url-classify

/// The kind of thing a link string points at, after pure shape detection.
///
/// The variant set is deliberately drawn from what the existing handlers
/// actually distinguish — nothing speculative:
/// - [`External`](LinkTarget::External): `http(s)`/`mailto:` — `extract.rs`
///   hands these to the OS opener; `zim.rs::resolve_href` bails out of
///   in-archive navigation on them.
/// - [`Zim`](LinkTarget::Zim): a `zim://zim/<NS>/<article>` reference — the
///   shape `zim.rs::parse_zim_url` already parses for subresources.
/// - [`Wikilink`](LinkTarget::Wikilink): a `[[Name]]` / `[[folder/Name]]`
///   reference, or a bare ambiguous note name — resolved via the index
///   (`wikilink::resolve_path`).
/// - [`VaultPath`](LinkTarget::VaultPath): a path-like, vault-relative
///   note/file reference — opened directly (`editor_pane::open_file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An external URL the OS should open: `http://…`, `https://…`, or
    /// `mailto:…`. Carries the raw string verbatim.
    External(String),
    /// A reference into an offline ZIM archive, parsed from the
    /// `zim://<archive>/<NS>/<article>` shape `zim.rs` uses. `archive` is the
    /// authority segment (the `zim` in `zim://zim/C/Foo`); `article` is the
    /// remainder after it (e.g. `C/Foo`), with any `#fragment` / `?query`
    /// stripped but namespace + relative segments left intact for the caller's
    /// archive lookup to resolve.
    Zim {
        /// Authority segment of the `zim://` URL (`zim://<archive>/…`).
        archive: String,
        /// Path remainder after the authority (`<NS>/<article>`), fragment and
        /// query removed.
        article: String,
    },
    /// A vault-relative note/file path to open directly (path-shaped: contains
    /// a `/`, or ends in a known note extension). Carries the path verbatim.
    VaultPath(String),
    /// A `[[name]]` / bare note name to resolve against the index. Carries the
    /// inner target with the `[[`/`]]` brackets removed and any `|alias` /
    /// `#section` trailer stripped, ready to hand to `wikilink::resolve_path`.
    Wikilink(String),
}

/// File extensions that, on their own, mark a bare string as a vault path
/// rather than an ambiguous wikilink name. Markdown is the note format; the
/// rest are the common attachment types a note links to directly. Kept here
/// (not pulled from `indexer::INDEXABLE_EXTENSIONS`) because this is a *link
/// shape* question, not an *is-this-indexed* question — a link to `notes.pdf`
/// is still a vault path even though PDFs aren't indexed.
const PATH_EXTENSIONS: &[&str] = &[
    "md", "markdown", "pdf", "png", "jpg", "jpeg", "gif", "svg", "webp", "txt", "csv",
];

/// Classify a raw link string into a [`LinkTarget`] by pure shape detection.
///
/// Precedence (first match wins):
/// 1. **Wikilink brackets** — `[[…]]`. A leading `[[` is unambiguous note
///    syntax, so it is decided before any scheme sniffing. The inner text is
///    unwrapped and its `|alias` / `#section` trailer stripped.
/// 2. **External scheme** — `http://`, `https://`, or a `mailto:` prefix
///    (case-insensitive on the scheme). These leave the vault entirely.
/// 3. **`zim://` scheme** — split into `{ archive, article }` mirroring
///    `zim.rs`'s `zim://<authority>/<rest>` shape; fragment/query trimmed.
/// 4. **Path shape** — a string containing `/` or ending in a known note/file
///    extension ([`PATH_EXTENSIONS`]) is a [`VaultPath`](LinkTarget::VaultPath).
/// 5. **Bare text** — anything else (including the empty string and a
///    fragment-only `#sec`) falls through to [`Wikilink`](LinkTarget::Wikilink);
///    the index gets the final say on whether it resolves.
///
/// Note on fragment-only input (`#section`): with no surrounding context this
/// is an in-document anchor, which this layer does not model; it falls through
/// to `Wikilink` (the index simply won't resolve it). Callers that have page
/// context (e.g. the ZIM viewer) keep handling pure fragments themselves.
#[must_use]
pub fn classify(raw: &str) -> LinkTarget {
    let s = raw.trim();

    // 1. Explicit wikilink brackets win outright.
    if let Some(inner) = strip_wikilink_brackets(s) {
        return LinkTarget::Wikilink(wikilink_body(inner).to_string());
    }

    // 2. External schemes.
    if has_external_scheme(s) {
        return LinkTarget::External(s.to_string());
    }

    // 3. zim:// references.
    if let Some(rest) = s.strip_prefix("zim://") {
        let (archive, article) = split_zim(rest);
        return LinkTarget::Zim { archive, article };
    }

    // 4. Path-shaped strings: a folder separator or a known file extension.
    if looks_like_path(s) {
        return LinkTarget::VaultPath(s.to_string());
    }

    // 5. Anything left over is a bare name for the index to resolve.
    LinkTarget::Wikilink(wikilink_body(s).to_string())
}

/// Resolve a classified [`LinkTarget`] toward a concrete vault path *when the
/// caller can supply the lookups*, without this module depending on the store.
///
/// `existing` answers "is this exact vault-relative path present?" and
/// `resolve_name` answers "what path does this bare/wikilink name map to?"
/// (typically a thin wrapper over `wikilink::resolve_path` against the index).
/// Both are closures so `core::url` stays store-free.
///
/// Returns the resolved vault-relative path for [`VaultPath`] /
/// [`Wikilink`] targets, or `None` for external/zim targets (which are not
/// vault paths) and for names the lookup can't resolve. Provided as a
/// convenience for callers that only need note resolution; the ZIM and OS-open
/// actions still live with their respective handlers.
pub fn resolve_path<E, R>(
    target: &LinkTarget,
    existing: E,
    resolve_name: R,
) -> Option<String>
where
    E: Fn(&str) -> bool,
    R: Fn(&str) -> Option<String>,
{
    match target {
        LinkTarget::External(_) | LinkTarget::Zim { .. } => None,
        LinkTarget::VaultPath(p) => {
            if existing(p) {
                Some(p.clone())
            } else {
                resolve_name(p)
            }
        }
        LinkTarget::Wikilink(name) => resolve_name(name),
    }
}

/// Strip a surrounding `[[ … ]]` pair, returning the inner text, or `None`
/// when the string is not bracket-wrapped. Requires both brackets so a bare
/// `[[` fragment isn't mistaken for a wikilink.
fn strip_wikilink_brackets(s: &str) -> Option<&str> {
    s.strip_prefix("[[")?.strip_suffix("]]")
}

/// Reduce a wikilink body to the resolvable target: drop an `|alias` display
/// half and a `#section` anchor, then trim. Mirrors the path-form rule that
/// what precedes `|`/`#` is the path-or-name the index resolves.
fn wikilink_body(inner: &str) -> &str {
    let no_alias = inner.split('|').next().unwrap_or(inner);
    let no_section = no_alias.split('#').next().unwrap_or(no_alias);
    no_section.trim()
}

/// True for the external schemes hiker opens via the OS: `http`, `https`, and
/// `mailto`. Scheme match is ASCII-case-insensitive; the rest is untouched.
fn has_external_scheme(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Split the post-`zim://` remainder into `(archive, article)`: the authority
/// is the first path segment, the article is everything after the first `/`
/// with any `#fragment` / `?query` removed. A bare authority (no slash) yields
/// an empty article. Mirrors `zim.rs::parse_zim_url`, which drops the authority
/// then keeps the namespace-qualified remainder.
fn split_zim(rest: &str) -> (String, String) {
    let (archive, after) = match rest.split_once('/') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    let article = after.split(['#', '?']).next().unwrap_or(after);
    (archive.to_string(), article.to_string())
}

/// Path-shape heuristic: a folder separator, or a basename ending in a known
/// note/file extension. Bare extension-less names are *not* paths — they fall
/// through to wikilink resolution so the index disambiguates them.
fn looks_like_path(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    if s.contains('/') {
        return true;
    }
    let ext = match s.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return false,
    };
    PATH_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(raw: &str) -> LinkTarget {
        classify(raw)
    }

    #[test]
    fn http_and_https_are_external() {
        assert_eq!(ext("http://example.com"), LinkTarget::External("http://example.com".into()));
        assert_eq!(
            ext("https://example.com/a/b?c=1#frag"),
            LinkTarget::External("https://example.com/a/b?c=1#frag".into())
        );
    }

    #[test]
    fn external_scheme_is_case_insensitive() {
        assert_eq!(ext("HTTPS://Example.com"), LinkTarget::External("HTTPS://Example.com".into()));
        assert_eq!(ext("MailTo:a@b.c"), LinkTarget::External("MailTo:a@b.c".into()));
    }

    #[test]
    fn mailto_is_external() {
        assert_eq!(ext("mailto:a@b.c"), LinkTarget::External("mailto:a@b.c".into()));
    }

    #[test]
    fn zim_with_namespace_and_article() {
        assert_eq!(
            ext("zim://zim/C/Foo"),
            LinkTarget::Zim { archive: "zim".into(), article: "C/Foo".into() }
        );
    }

    #[test]
    fn zim_strips_fragment_and_query() {
        assert_eq!(
            ext("zim://zim/C/Foo#section"),
            LinkTarget::Zim { archive: "zim".into(), article: "C/Foo".into() }
        );
        assert_eq!(
            ext("zim://zim/C/Foo?v=1"),
            LinkTarget::Zim { archive: "zim".into(), article: "C/Foo".into() }
        );
    }

    #[test]
    fn zim_without_article() {
        assert_eq!(
            ext("zim://zim"),
            LinkTarget::Zim { archive: "zim".into(), article: String::new() }
        );
        assert_eq!(
            ext("zim://zim/"),
            LinkTarget::Zim { archive: "zim".into(), article: String::new() }
        );
    }

    #[test]
    fn zim_keeps_relative_segments_for_caller() {
        // The renderer concatenates relative hrefs onto the base, so the
        // article half may carry `..`/`.`; we leave that for the archive
        // lookup (which normalizes) rather than collapsing it here.
        assert_eq!(
            ext("zim://zim/C/../-/style.css"),
            LinkTarget::Zim { archive: "zim".into(), article: "C/../-/style.css".into() }
        );
    }

    #[test]
    fn bracketed_wikilink_is_unwrapped() {
        assert_eq!(ext("[[Note Name]]"), LinkTarget::Wikilink("Note Name".into()));
        assert_eq!(ext("[[folder/Sub Note]]"), LinkTarget::Wikilink("folder/Sub Note".into()));
    }

    #[test]
    fn wikilink_alias_is_stripped() {
        assert_eq!(ext("[[Target|Display Text]]"), LinkTarget::Wikilink("Target".into()));
    }

    #[test]
    fn wikilink_section_is_stripped() {
        assert_eq!(ext("[[Target#Heading]]"), LinkTarget::Wikilink("Target".into()));
        assert_eq!(ext("[[Target#Heading|Shown]]"), LinkTarget::Wikilink("Target".into()));
    }

    #[test]
    fn path_with_slash_is_vault_path() {
        assert_eq!(ext("folder/note.md"), LinkTarget::VaultPath("folder/note.md".into()));
        assert_eq!(ext("a/b/c"), LinkTarget::VaultPath("a/b/c".into()));
    }

    #[test]
    fn known_extension_is_vault_path() {
        assert_eq!(ext("notes.md"), LinkTarget::VaultPath("notes.md".into()));
        assert_eq!(ext("diagram.PNG"), LinkTarget::VaultPath("diagram.PNG".into()));
        assert_eq!(ext("report.pdf"), LinkTarget::VaultPath("report.pdf".into()));
    }

    #[test]
    fn bare_name_is_wikilink() {
        assert_eq!(ext("Some Note"), LinkTarget::Wikilink("Some Note".into()));
        assert_eq!(ext("ProjectIdeas"), LinkTarget::Wikilink("ProjectIdeas".into()));
    }

    #[test]
    fn unknown_extension_is_not_a_path() {
        // No `/` and an extension we don't recognize → bare name, let the
        // index decide. `version 1.2` shouldn't be read as a file.
        assert_eq!(ext("version 1.2"), LinkTarget::Wikilink("version 1.2".into()));
    }

    #[test]
    fn empty_is_wikilink() {
        assert_eq!(ext(""), LinkTarget::Wikilink(String::new()));
        assert_eq!(ext("   "), LinkTarget::Wikilink(String::new()));
    }

    #[test]
    fn fragment_only_falls_through_to_wikilink() {
        // No page context here; a pure `#sec` has no note target once the
        // section trailer is stripped, so it becomes an empty wikilink the
        // index won't resolve. Page-aware callers handle anchors themselves.
        assert_eq!(ext("#section"), LinkTarget::Wikilink(String::new()));
    }

    #[test]
    fn bare_name_with_section_keeps_the_name() {
        // A bare `Foo#sec` (no brackets) still resolves to the note `Foo`.
        assert_eq!(ext("Foo#sec"), LinkTarget::Wikilink("Foo".into()));
    }

    #[test]
    fn relative_path_without_extension_needs_a_slash() {
        // A leading `./` makes it path-shaped via the slash rule.
        assert_eq!(ext("./Foo"), LinkTarget::VaultPath("./Foo".into()));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_before_classifying() {
        assert_eq!(ext("  https://x.y  "), LinkTarget::External("https://x.y".into()));
        assert_eq!(ext("  [[Note]]  "), LinkTarget::Wikilink("Note".into()));
    }

    #[test]
    fn resolve_path_uses_existing_then_lookup() {
        let existing = |p: &str| p == "folder/note.md";
        let resolve = |name: &str| {
            (name == "Other").then(|| "deep/Other.md".to_string())
        };

        // Exact existing path is returned as-is.
        assert_eq!(
            resolve_path(&LinkTarget::VaultPath("folder/note.md".into()), existing, resolve),
            Some("folder/note.md".into())
        );
        // A vault path that doesn't exist falls back to name resolution.
        assert_eq!(
            resolve_path(&LinkTarget::VaultPath("Other".into()), existing, resolve),
            Some("deep/Other.md".into())
        );
        // Wikilink names resolve via the lookup.
        assert_eq!(
            resolve_path(&LinkTarget::Wikilink("Other".into()), existing, resolve),
            Some("deep/Other.md".into())
        );
        // Unresolvable name → None.
        assert_eq!(
            resolve_path(&LinkTarget::Wikilink("Ghost".into()), existing, resolve),
            None
        );
        // External / zim targets are never vault paths.
        assert_eq!(
            resolve_path(&LinkTarget::External("https://x".into()), existing, resolve),
            None
        );
        assert_eq!(
            resolve_path(
                &LinkTarget::Zim { archive: "zim".into(), article: "C/Foo".into() },
                existing,
                resolve
            ),
            None
        );
    }
}
