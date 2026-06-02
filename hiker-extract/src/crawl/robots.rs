//! A small, pure-Rust `robots.txt` matcher — no `robots`-crate dependency
//! (none is vendored, and the matcher hiker needs is tiny). It fetches
//! `<scheme>://<host>/robots.txt` once per host, parses the `User-agent` /
//! `Allow` / `Disallow` groups, and answers "may I fetch this path?" by the
//! standard longest-match rule. This is part of the centralized governance in
//! `docs/extract.md` `crawl-governance`: no extractor can runaway-crawl the
//! open web because the loop checks robots here before every fetch.
//!
//! The parse is the well-supported subset: per-agent groups, `Allow` /
//! `Disallow` path prefixes with `*` and `$` wildcards, longest-match-wins,
//! `Allow` breaking ties. `Crawl-delay` / `Sitemap` / host directives are
//! ignored (the loop owns its own rate limit). Fetching goes through an
//! injected fetcher so the unit tests run fully offline.
//
// status: crawl-governance

use std::collections::HashMap;

/// One robots ruleset for a single host, already filtered to the rules that
/// apply to our user-agent.
#[derive(Debug, Clone, Default)]
pub struct Rules {
    /// `(path-pattern, allow?)` rules in file order; longest match wins.
    directives: Vec<(String, bool)>,
}

impl Rules {
    /// A permissive ruleset (no robots.txt, or a fetch failure → allow all,
    /// the conventional default).
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Whether `path` (the URL path, e.g. `/docs/x`) is allowed. Standard
    /// rule: the longest matching pattern decides; an `Allow` of equal length
    /// beats a `Disallow`. No matching rule → allowed.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;
        for (pattern, allow) in &self.directives {
            if let Some(len) = match_len(pattern, path) {
                let better = match best {
                    None => true,
                    // Longer match wins; on a tie, Allow (true) wins.
                    Some((blen, ballow)) => len > blen || (len == blen && *allow && !ballow),
                };
                if better {
                    best = Some((len, *allow));
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }

    /// Parse a `robots.txt` body, selecting the rule group(s) that apply to
    /// `user_agent` (case-insensitive prefix-of-token match) falling back to
    /// the `*` group. Lines outside any matching group are ignored.
    pub fn parse(body: &str, user_agent: &str) -> Self {
        let ua = user_agent.to_ascii_lowercase();
        let groups = group_lines(body);
        // Prefer the most specific matching agent group; else the `*` group.
        let mut specific: Vec<(String, bool)> = Vec::new();
        let mut wildcard: Vec<(String, bool)> = Vec::new();
        for group in groups {
            if group.agents.iter().any(|a| a == "*") {
                wildcard.extend(group.directives.clone());
            }
            if group.agents.iter().any(|a| ua.starts_with(a) && a != "*") {
                specific.extend(group.directives.clone());
            }
        }
        let directives = if specific.is_empty() { wildcard } else { specific };
        Self { directives }
    }
}

/// One `User-agent` group from a robots.txt file.
struct Group {
    agents: Vec<String>,
    directives: Vec<(String, bool)>,
}

/// Split a robots.txt body into agent-keyed groups. A run of `User-agent`
/// lines shares the directives that follow until the next `User-agent` after
/// a directive (the standard grouping rule).
fn group_lines(body: &str) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    let mut agents: Vec<String> = Vec::new();
    let mut directives: Vec<(String, bool)> = Vec::new();
    let mut seen_directive = false;

    for raw in body.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        match key.as_str() {
            "user-agent" => {
                if seen_directive && !agents.is_empty() {
                    groups.push(Group { agents: std::mem::take(&mut agents), directives: std::mem::take(&mut directives) });
                    seen_directive = false;
                }
                agents.push(value.to_ascii_lowercase());
            }
            "allow" if !value.is_empty() => {
                directives.push((value, true));
                seen_directive = true;
            }
            "disallow" => {
                // An empty Disallow means "allow all" — record nothing.
                if !value.is_empty() {
                    directives.push((value, false));
                }
                seen_directive = true;
            }
            _ => {}
        }
    }
    if !agents.is_empty() {
        groups.push(Group { agents, directives });
    }
    groups
}

/// If `pattern` (a robots path pattern with `*`/`$` wildcards) matches a
/// prefix of `path`, return the number of pattern characters consumed (used
/// for longest-match). `None` if it doesn't match.
fn match_len(pattern: &str, path: &str) -> Option<usize> {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = path.chars().collect();
    robots_match(&p, 0, &t, 0).map(|_| pattern.chars().filter(|c| *c != '*').count())
}

/// Backtracking match of a robots pattern against the path. `*` matches any
/// run; `$` anchors the end. Returns `Some(())` on match.
fn robots_match(p: &[char], pi: usize, t: &[char], ti: usize) -> Option<()> {
    if pi == p.len() {
        return Some(());
    }
    match p[pi] {
        '*' => {
            for skip in ti..=t.len() {
                if robots_match(p, pi + 1, t, skip).is_some() {
                    return Some(());
                }
            }
            None
        }
        '$' => (ti == t.len()).then_some(()),
        c => {
            if ti < t.len() && t[ti] == c {
                robots_match(p, pi + 1, t, ti + 1)
            } else {
                None
            }
        }
    }
}

/// A per-host robots cache. Fetches and parses each host's `robots.txt` at
/// most once per crawl; subsequent checks hit the cache. The fetcher is
/// injected so tests run offline.
pub struct Cache<'a> {
    user_agent: String,
    /// Fetch a `robots.txt` URL → its body, or `None` on any failure (treated
    /// as "no robots.txt", allow all).
    fetch: &'a dyn Fn(&str) -> Option<String>,
    by_host: HashMap<String, Rules>,
}

impl<'a> Cache<'a> {
    /// New cache for `user_agent`, fetching via `fetch`.
    pub fn new(user_agent: impl Into<String>, fetch: &'a dyn Fn(&str) -> Option<String>) -> Self {
        Self { user_agent: user_agent.into(), fetch, by_host: HashMap::new() }
    }

    /// Whether the crawl may fetch `url`, consulting (and caching) the host's
    /// robots.txt. A URL that won't parse is allowed (the loop's scope check
    /// already vetted it).
    pub fn allows(&mut self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else { return true };
        let Some(host) = parsed.host_str() else { return true };
        let host = host.to_ascii_lowercase();
        if !self.by_host.contains_key(&host) {
            let robots_url = format!(
                "{}://{}/robots.txt",
                parsed.scheme(),
                parsed.port().map_or(host.clone(), |p| format!("{host}:{p}"))
            );
            let rules = match (self.fetch)(&robots_url) {
                Some(body) => Rules::parse(&body, &self.user_agent),
                None => Rules::allow_all(),
            };
            self.by_host.insert(host.clone(), rules);
        }
        let path = if parsed.path().is_empty() { "/" } else { parsed.path() };
        self.by_host[&host].allows(path)
    }
}
