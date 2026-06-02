//! URL scope matching for the crawl frontier: the **follow-pattern** ("only
//! continue into links matching X") and the **extract-pattern** ("only keep
//! pages matching Y") from `docs/extract.md` `crawl-scope-patterns`. Both are
//! gitignore-style globs over a URL's host+path (reusing the hand-rolled glob
//! matcher Phase 2 added in [`crate::trigger`]), with a `re:<regex>` escape
//! hatch for when globs prove too blunt on URLs.
//!
//! A `Scope` also owns the **same-site** rule the loop applies before either
//! user pattern: a deep crawl never wanders off the seed host unless the
//! follow-pattern explicitly opts another host in. The matchers operate on a
//! normalized "host/path" string (scheme + query + fragment stripped) so a
//! glob like `example.com/docs/**` reads naturally.
//
// status: crawl-scope-patterns

use regex::Regex;

use crate::trigger::glob_matches;

/// A compiled scope pattern: either a gitignore-style glob or a regex (the
/// `re:` escape hatch). Built from a single frontmatter string.
#[derive(Debug, Clone)]
pub enum Pattern {
    /// gitignore-style glob over the normalized `host/path` form.
    Glob(String),
    /// A regex matched against the *full* URL (the escape hatch for when
    /// globs are too blunt — auth tokens, query-string scoping, etc.).
    Regex(Regex),
}

impl Pattern {
    /// Parse one pattern string. A `re:` prefix selects the regex escape
    /// hatch; an invalid regex falls back to treating the (post-prefix) text
    /// as a glob so a typo never silently matches nothing surprising.
    pub fn parse(raw: &str) -> Self {
        if let Some(rest) = raw.strip_prefix("re:") {
            match Regex::new(rest) {
                Ok(re) => Pattern::Regex(re),
                Err(_) => Pattern::Glob(rest.to_string()),
            }
        } else {
            Pattern::Glob(raw.to_string())
        }
    }

    /// Whether `url` matches. Globs match the normalized `host/path`; regexes
    /// match the full URL string.
    pub fn matches(&self, url: &str) -> bool {
        match self {
            Pattern::Glob(g) => glob_matches(g, &host_path(url)),
            Pattern::Regex(re) => re.is_match(url),
        }
    }
}

/// The scope a crawl applies to candidate links: an optional same-site host
/// restriction plus the optional follow / extract patterns. Built once from
/// the crawl params and consulted by governance for every candidate.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// The seed host. A deep crawl stays on this host unless a follow-pattern
    /// explicitly admits another. `None` (a bare list crawl) imposes no
    /// host restriction — the explicit seed set is the whole scope.
    pub same_site: Option<String>,
    /// "Only continue into links matching X." `None` follows everything else
    /// in scope.
    pub follow: Option<Pattern>,
    /// "Only keep pages matching Y." `None` extracts every reached page.
    pub extract: Option<Pattern>,
}

impl Scope {
    /// Build the scope from the raw follow/extract pattern strings and the
    /// seed host (the same-site anchor for a deep crawl). A `None`
    /// `seed_host` leaves the crawl host-unrestricted (list mode).
    pub fn new(
        seed_host: Option<String>,
        follow_pattern: Option<&str>,
        extract_pattern: Option<&str>,
    ) -> Self {
        Self {
            same_site: seed_host,
            follow: follow_pattern.map(Pattern::parse),
            extract: extract_pattern.map(Pattern::parse),
        }
    }

    /// Whether the loop may **follow** `url` (admit it to the frontier). The
    /// follow-pattern wins when set: a URL the pattern matches is followed even
    /// off-host (the pattern is the explicit opt-in). Otherwise a same-site
    /// anchor restricts following to the seed host.
    pub fn may_follow(&self, url: &str) -> bool {
        if let Some(p) = &self.follow {
            return p.matches(url);
        }
        match &self.same_site {
            Some(host) => host_of(url).as_deref() == Some(host.as_str()),
            None => true,
        }
    }

    /// Whether a reached `url` should be **kept** (written as a sidecar). The
    /// extract-pattern gates this; with no pattern every reached page is kept.
    pub fn may_extract(&self, url: &str) -> bool {
        self.extract.as_ref().is_none_or(|p| p.matches(url))
    }
}

/// The host of a URL, lowercased, or `None` when it can't be parsed.
pub fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_ascii_lowercase)
}

/// Normalize a URL to the `host/path` form globs match against: lowercased
/// host + the path, with scheme, query, and fragment dropped. A parse failure
/// falls back to the raw string so a malformed candidate still gets *some*
/// chance to match a literal glob.
fn host_path(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").to_ascii_lowercase();
            format!("{host}{}", u.path())
        }
        Err(_) => url.to_string(),
    }
}
