//! Per-feature UI state for the search surface — the query string, the
//! cached result cards, the per-mode toggles + tuning options, and the
//! typeahead debounce machinery. Owned by `AppState::search_state`
//! (top-level, per `feature-state-ownership`). Split into its own file
//! (rather than inlined into `mod.rs` like backlinks/related) because the
//! struct + its `with_config`/`Default` impls run well over the 20-line
//! minimum `scripts/check-splits.py` enforces. [feature-search-migration]

use crate::search::DiscoveryHit;

pub struct State {
    pub query: String,
    /// Cached card-shaped results from the last query.
    pub results: Vec<DiscoveryHit>,
    /// Search-mode toggles. Both default-on; either can be disabled.
    pub lexical_on: bool,
    pub semantic_on: bool,
    /// Result count cap (5..=100).
    pub limit: usize,
    /// Comma-separated extensions ("md, txt"). Empty = no filter.
    pub source_types: String,
    /// Result ordering.
    pub order_by: OrderBy,
    /// Pending query change: when set, the panel debounces for ~250ms
    /// before issuing the next search. Mirrors the TS UI's
    /// `search-typeahead-debounce` rule: rapid typing collapses into one
    /// query rather than firing on every keystroke.
    pub pending_query_at: Option<std::time::Instant>,
    /// Last-fired query — used to skip re-firing when the debounce
    /// tick lands but the query text hasn't actually moved.
    pub last_fired_query: String,
    /// Monotonic epoch incremented on every fire so late-returning
    /// background results can be dropped (`search-typeahead-debounce`).
    pub query_epoch: u64,
    /// Lexical engine options (case sensitivity, prefix, phrase). Persisted
    /// per-vault via `set_setting` keys `search.lexical.*`.
    pub lexical_opts: hiker_core::search::LexicalOpts,
    /// Semantic engine options (min-similarity, recency bias, top-k). Persisted
    /// per-vault via `set_setting` keys `search.semantic.*`.
    pub semantic_opts: hiker_core::search::SemanticOpts,
    /// Selected result row for keyboard nav (0-based; clamped to results
    /// length each frame). `None` = no selection; arrow keys initialise to
    /// 0 on first press.
    pub selected_row: Option<usize>,
    /// Per-section collapse state. Persisted per-vault.
    pub results_expanded: bool,
    /// One-shot: when set true, the search TextEdit will request focus on
    /// the next frame and the flag clears. Driven by the Ctrl-Space
    /// keybind (`search-keybind-ctrl-space`).
    pub focus_query_next_frame: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderBy {
    Score,
    Recent,
}

impl State {
    /// Seed the in-memory state from persisted vault settings. Defaults
    /// match the spec — both modes on, no lexical flags, no min-similarity.
    pub const fn with_config(mut self, cfg: &hiker_core::config::Config) -> Self {
        self.lexical_on = cfg.search.modes.lexical;
        self.semantic_on = cfg.search.modes.semantic;
        self.results_expanded = cfg.search.sections.results_expanded;
        self.lexical_opts = hiker_core::search::LexicalOpts {
            case_sensitive: cfg.search.lexical.case_sensitive,
            diacritic_sensitive: cfg.search.lexical.diacritic_sensitive,
            prefix_match: cfg.search.lexical.prefix_match,
            phrase_mode: cfg.search.lexical.phrase_mode,
            top_k: 0, // 0 = defer to UI's `limit` slider
        };
        self.semantic_opts = hiker_core::search::SemanticOpts {
            min_similarity: cfg.search.semantic.min_similarity,
            top_k: cfg.search.semantic.top_k,
            recency_bias: cfg.search.semantic.recency_bias,
        };
        self
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            query: String::new(),
            results: Vec::new(),
            lexical_on: true,
            semantic_on: true,
            limit: 25,
            source_types: String::new(),
            order_by: OrderBy::Score,
            pending_query_at: None,
            last_fired_query: String::new(),
            query_epoch: 0,
            lexical_opts: hiker_core::search::LexicalOpts::default(),
            semantic_opts: hiker_core::search::SemanticOpts::default(),
            selected_row: None,
            results_expanded: true,
            focus_query_next_frame: false,
        }
    }
}
