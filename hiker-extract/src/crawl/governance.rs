//! Centralized crawl governance — the seatbelt. ALL the rules that keep a
//! crawl from runaway-walking the open web live here, written once, so no
//! extractor can escape them: in-scope check, dedup (visited set), depth cap,
//! page-count cap, rate limit (politeness delay), and `robots.txt` respect.
//! The frontier loop ([`super`]) calls [`Governor::admit`] for every candidate
//! link and [`Governor::may_fetch`] before every fetch; the extractor only ever
//! *proposes* links — the governor decides. See `docs/extract.md`
//! `crawl-governance`.
//
// status: crawl-governance

use std::collections::HashSet;
use std::time::Duration;

use super::scope::{host_of, Scope};

/// Why a candidate link was rejected (for the run log / report). An `Admitted`
/// link is enqueued; everything else is dropped with a recorded reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Enqueue it.
    Admitted,
    /// Already visited / already queued (dedup).
    Duplicate,
    /// Beyond the depth cap.
    TooDeep,
    /// The page-count cap is already reached.
    PageCapReached,
    /// Outside the follow scope (same-site / follow-pattern).
    OutOfScope,
}

/// The governance state for one crawl run. Owns the visited set and the caps;
/// the [`Scope`] (same-site + follow/extract patterns) and the rate limit are
/// consulted on every admission. Robots is a separate concern checked at fetch
/// time (it needs an injected fetcher) — see [`super::robots::Cache`].
pub struct Governor {
    scope: Scope,
    /// URLs already enqueued or visited — the dedup set. Normalized so
    /// `…/x` and `…/x/` don't double-fetch.
    seen: HashSet<String>,
    /// Maximum link-depth from a seed (`0` = follow nothing).
    max_depth: u32,
    /// Maximum number of pages to *capture* before stopping.
    max_pages: u32,
    /// Pages captured so far (drives the page-cap verdict).
    captured: u32,
    /// Politeness delay between fetches.
    rate_limit: Duration,
}

impl Governor {
    /// New governor for a crawl with the given scope and caps.
    pub fn new(scope: Scope, max_depth: u32, max_pages: u32, rate_limit: Duration) -> Self {
        Self {
            scope,
            seen: HashSet::new(),
            max_depth,
            max_pages,
            captured: 0,
            rate_limit,
        }
    }

    /// Mark a URL seen up-front (the seeds, so they don't get re-admitted as
    /// links). Returns whether it was newly inserted.
    pub fn mark_seen(&mut self, url: &str) -> bool {
        self.seen.insert(normalize(url))
    }

    /// Decide whether a candidate `url` discovered at `depth` should be
    /// admitted to the frontier. Applies, in order: page-cap → dedup → depth →
    /// scope. The first failing check yields its verdict; an admitted URL is
    /// recorded in the visited set so a later duplicate is rejected.
    ///
    /// Robots is checked separately at fetch time (it needs the injected
    /// fetcher and a cache), not here — see [`super::robots`].
    pub fn admit(&mut self, url: &str, depth: u32) -> Verdict {
        if self.captured >= self.max_pages {
            return Verdict::PageCapReached;
        }
        if depth > self.max_depth {
            return Verdict::TooDeep;
        }
        if !self.scope.may_follow(url) {
            return Verdict::OutOfScope;
        }
        if !self.seen.insert(normalize(url)) {
            return Verdict::Duplicate;
        }
        Verdict::Admitted
    }

    /// Whether a fetched page should be *kept* (extract-pattern gate). The
    /// loop fetches every admitted URL (its links may be useful even when its
    /// body isn't kept), but only writes a sidecar when this returns true.
    pub fn may_extract(&self, url: &str) -> bool {
        self.scope.may_extract(url)
    }

    /// Record that a page was captured (a sidecar written). Drives the
    /// page-count cap.
    pub const fn record_capture(&mut self) {
        self.captured = self.captured.saturating_add(1);
    }

    /// Whether the page-count cap has been reached (the loop's stop signal).
    pub const fn page_cap_reached(&self) -> bool {
        self.captured >= self.max_pages
    }

    /// The politeness delay to sleep between fetches.
    pub const fn rate_limit(&self) -> Duration {
        self.rate_limit
    }
}

/// Build a [`Scope`] from a seed URL and the crawl's follow/extract patterns,
/// anchoring same-site on the (first) seed's host. A `list` crawl with depth 0
/// passes `None` for `seed_host` so it isn't host-restricted — its explicit
/// seed set is the whole scope.
pub fn scope_for(
    seed: &str,
    same_site: bool,
    follow_pattern: Option<&str>,
    extract_pattern: Option<&str>,
) -> Scope {
    let seed_host = same_site.then(|| host_of(seed)).flatten();
    Scope::new(seed_host, follow_pattern, extract_pattern)
}

/// Normalize a URL for the dedup set: drop the fragment and a single trailing
/// slash so trivially-different forms of the same page collapse.
fn normalize(url: &str) -> String {
    let no_frag = url.split('#').next().unwrap_or(url);
    no_frag.strip_suffix('/').unwrap_or(no_frag).to_string()
}
