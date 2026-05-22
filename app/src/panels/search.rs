//! Search sub-panel of the right-hand discovery pane.
//!
//! Owns the query input, the per-mode options menu, debounce machinery,
//! and the result list. Renders the matched-chunk result cards (shared
//! with the Related sub-panel via `result_card`).
//!
//! Search wires `core::search::query` against the read store. Falls back
//! to lexical-only when the embedder isn't loaded yet. Filename grep is
//! kept as a final fallback when the store hasn't seen the file (e.g.
//! brand-new vault with empty index).

use eframe::egui;

use hiker_core::search::Modes;

use crate::editor_pane;
use crate::state::AppState;
use crate::theme;

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

/// A card row in the discovery panel. Shared shape between search hits and
/// related-notes hits per `discovery-result-card`: title + score, path
/// subtitle, optional heading-path breadcrumb, matched-chunk excerpt.
#[derive(Debug, Clone)]
pub struct DiscoveryHit {
    pub path: String,
    pub title: String,
    pub heading_path: Option<String>,
    pub snippet: String,
    pub score: f32,
    pub source_tag: Option<&'static str>,
    /// Index of the matched chunk in the note. Drives the click-to-chunk
    /// navigation per `search-result-click-opens-chunk`.
    pub chunk_index: u32,
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

const SEARCH_DEBOUNCE_MS: u64 = 250;

/// Persist a search-related setting through `core::config::Config::set`
/// (per `search-mode-state-persisted`). Best-effort: a write failure logs
/// and is otherwise silent, so the option still applies in-session.
pub(crate) fn persist_search_setting(app: &AppState, key: &str, value: &serde_json::Value) {
    crate::state::set_setting_quiet(
        app,
        hiker_core::config::SettingsScope::Vault,
        key,
        value,
        "search",
    );
}

/// Per-frame render context for the search panel. Bundling `ui` + `app`
/// lets the panel's render steps be inherent methods rather than a row
/// of single-use free functions.
pub(crate) struct View<'a> {
    pub(crate) ui: &'a mut egui::Ui,
    pub(crate) app: &'a mut AppState,
}

impl View<'_> {
    pub(crate) fn show(&mut self) {
        self.search_input_and_run();
        self.ui.add_space(8.0);
        self.results_section();
    }
}

/// All search-mode toggles, per-mode option pickers, and the
/// Limit/Types/Order filters. Lifted out of the inline header rows and
/// served from a right-click on the search icon. Caller sets `run`
/// when any control changes so the panel re-fires the query.
fn search_options_menu(
    ui: &mut egui::Ui,
    app: &mut AppState,
    run: &mut bool,
) {
    use hiker_core::config::sections::RecencyBias;

    // Mode toggles.
    ui.label(egui::RichText::new("Modes").strong().small());
    if ui
        .checkbox(&mut app.panels.search.lexical_on, "Lexical")
        .on_hover_text("Substring / token matches from the index")
        .changed()
    {
        *run = true;
    }
    if ui
        .checkbox(&mut app.panels.search.semantic_on, "Semantic")
        .on_hover_text("Embedding-based similarity")
        .changed()
    {
        *run = true;
    }
    ui.horizontal(|ui| {
        if ui.small_button("Only lexical").clicked() {
            app.panels.search.lexical_on = true;
            app.panels.search.semantic_on = false;
            *run = true;
        }
        if ui.small_button("Only semantic").clicked() {
            app.panels.search.semantic_on = true;
            app.panels.search.lexical_on = false;
            *run = true;
        }
        if ui.small_button("Both").clicked() {
            app.panels.search.lexical_on = true;
            app.panels.search.semantic_on = true;
            *run = true;
        }
    });

    ui.separator();
    ui.label(egui::RichText::new("Lexical options").strong().small());
    let mut lex = app.panels.search.lexical_opts;
    let mut lex_changed = false;
    if ui.checkbox(&mut lex.case_sensitive, "Case sensitive").changed() {
        lex_changed = true;
    }
    if ui
        .checkbox(&mut lex.diacritic_sensitive, "Diacritic sensitive")
        .changed()
    {
        lex_changed = true;
    }
    if ui.checkbox(&mut lex.prefix_match, "Prefix match").changed() {
        lex_changed = true;
    }
    if ui.checkbox(&mut lex.phrase_mode, "Phrase mode").changed() {
        lex_changed = true;
    }
    if lex_changed {
        app.panels.search.lexical_opts = lex;
        persist_search_setting(app, "search.lexical.case_sensitive", &serde_json::json!(lex.case_sensitive));
        persist_search_setting(app, "search.lexical.diacritic_sensitive", &serde_json::json!(lex.diacritic_sensitive));
        persist_search_setting(app, "search.lexical.prefix_match", &serde_json::json!(lex.prefix_match));
        persist_search_setting(app, "search.lexical.phrase_mode", &serde_json::json!(lex.phrase_mode));
        *run = true;
    }

    ui.separator();
    ui.label(egui::RichText::new("Semantic options").strong().small());
    let mut sem = app.panels.search.semantic_opts;
    let mut sem_changed = false;
    ui.horizontal(|ui| {
        ui.label("Min similarity");
        if ui
            .add(egui::Slider::new(&mut sem.min_similarity, 0.0..=0.95))
            .changed()
        {
            sem_changed = true;
        }
    });
    ui.label(egui::RichText::new("Recency bias").small().color(theme::muted()));
    for (lbl, val) in [
        ("Off", RecencyBias::Off),
        ("Mild", RecencyBias::Mild),
        ("Strong", RecencyBias::Strong),
    ] {
        if ui.radio_value(&mut sem.recency_bias, val, lbl).changed() {
            sem_changed = true;
        }
    }
    if sem_changed {
        app.panels.search.semantic_opts = sem;
        persist_search_setting(
            app,
            "search.semantic.min_similarity",
            &serde_json::json!(sem.min_similarity),
        );
        let bias_str = match sem.recency_bias {
            RecencyBias::Off => "off",
            RecencyBias::Mild => "mild",
            RecencyBias::Strong => "strong",
        };
        persist_search_setting(
            app,
            "search.semantic.recency_bias",
            &serde_json::json!(bias_str),
        );
        *run = true;
    }

    ui.separator();
    ui.label(egui::RichText::new("Filters").strong().small());
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Limit").small().color(theme::muted()));
        let mut limit = app.panels.search.limit;
        if ui.add(egui::Slider::new(&mut limit, 5..=100)).changed() {
            app.panels.search.limit = limit;
            *run = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Types").small().color(theme::muted()));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut app.panels.search.source_types)
                .hint_text("md, txt")
                .desired_width(140.0),
        );
        if resp.lost_focus() {
            *run = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Order").small().color(theme::muted()));
        let cur = app.panels.search.order_by;
        let label = match cur {
            OrderBy::Score => "Score",
            OrderBy::Recent => "Recent",
        };
        let mut new_order = cur;
        egui::ComboBox::from_id_salt("discovery-order-by")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut new_order, OrderBy::Score, "Score");
                ui.selectable_value(&mut new_order, OrderBy::Recent, "Recent");
            });
        if new_order != cur {
            app.panels.search.order_by = new_order;
            *run = true;
        }
    });
}

/// Render the search input row (icon + text edit + keyboard nav), honour
/// the typeahead debounce, and fire the search when the debounce window
/// closes or Enter is pressed.
impl View<'_> {
    fn search_input_and_run(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let mut run = false;
    let mut run_search_immediate = false;
    ui.horizontal(|ui| {
        // Magnifying glass doubles as the options menu trigger
        // (right-click). All the previously-inline rows (Lexical /
        // Semantic toggles + their options, Limit, Types, Order) live
        // inside this popup so the panel header stays one line tall.
        let icon_resp = ui
            .add(crate::icons::ICONS.image(crate::icons::Icon::Search).sense(egui::Sense::click()))
            .on_hover_text("Right-click for search options");
        icon_resp.context_menu(|ui| {
            search_options_menu(ui, app, &mut run);
        });
        let resp = ui.add(
            egui::TextEdit::singleline(&mut app.panels.search.query)
                .hint_text("Search vault…"),
        );
        // Inline mode toggles (per the legacy TS UI's
        // toggleLexicalBtn/toggleSemanticBtn). Right-click on either still
        // opens the full options menu for per-mode tuning.
        let lex_on = app.panels.search.lexical_on;
        let lex_resp = ui
            .selectable_label(lex_on, "Lex")
            .on_hover_text("Lexical: substring / token matches");
        if lex_resp.clicked() {
            app.panels.search.lexical_on = !lex_on;
            persist_search_setting(
                app,
                "search.modes.lexical",
                &serde_json::json!(app.panels.search.lexical_on),
            );
            run = true;
        }
        lex_resp.context_menu(|ui| {
            search_options_menu(ui, app, &mut run);
        });
        let sem_on = app.panels.search.semantic_on;
        let sem_resp = ui
            .selectable_label(sem_on, "Sem")
            .on_hover_text("Semantic: embedding similarity");
        if sem_resp.clicked() {
            app.panels.search.semantic_on = !sem_on;
            persist_search_setting(
                app,
                "search.modes.semantic",
                &serde_json::json!(app.panels.search.semantic_on),
            );
            run = true;
        }
        sem_resp.context_menu(|ui| {
            search_options_menu(ui, app, &mut run);
        });
        if app.panels.search.focus_query_next_frame {
            app.panels.search.focus_query_next_frame = false;
            resp.request_focus();
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            // Enter forces an immediate run, bypassing the debounce.
            run_search_immediate = true;
        }
        if resp.changed() {
            // Defer the actual query — debounce via pending_query_at.
            app.panels.search.pending_query_at = Some(std::time::Instant::now());
        }
        // Keyboard nav: ↑/↓ shift the selected row; Enter on the focused
        // input opens the highlighted row (when present). Esc clears the
        // selection / query.
        if resp.has_focus() {
            let (up, down, esc) = ui.input(|i| (
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Escape),
            ));
            let results_len = app.panels.search.results.len();
            if results_len > 0 {
                if down {
                    let cur = app.panels.search.selected_row.unwrap_or(usize::MAX);
                    let next = if cur == usize::MAX { 0 } else { (cur + 1).min(results_len - 1) };
                    app.panels.search.selected_row = Some(next);
                }
                if up {
                    let cur = app.panels.search.selected_row.unwrap_or(0);
                    let next = cur.saturating_sub(1);
                    app.panels.search.selected_row = Some(next);
                }
            }
            if esc {
                if app.panels.search.selected_row.is_some() {
                    app.panels.search.selected_row = None;
                } else {
                    app.panels.search.query.clear();
                    app.panels.search.results.clear();
                }
            }
        }
    });
    // Honour the debounce window: only fire when the input has been
    // quiet for SEARCH_DEBOUNCE_MS *and* the query has actually changed
    // since the last fire. This keeps embedding-per-keystroke from
    // landing under heavy typing.
    if let Some(deadline) = app.panels.search.pending_query_at {
        if deadline.elapsed().as_millis() as u64 >= SEARCH_DEBOUNCE_MS {
            app.panels.search.pending_query_at = None;
            if app.panels.search.query != app.panels.search.last_fired_query {
                run = true;
            }
        } else {
            // Ensure egui repaints once the debounce window expires so
            // the deferred search actually fires.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(
                SEARCH_DEBOUNCE_MS
                    .saturating_sub(deadline.elapsed().as_millis() as u64),
            ));
        }
    }
    if run_search_immediate {
        app.panels.search.pending_query_at = None;
        run = true;
    }
    // Drop the local reborrow so the run block below can take `self`
    // again (needed by `run_query`).
    let _ = (ui, app);
    // Search-mode toggles + advanced filters live in the magnifying-glass
    // context menu (see `search_options_menu`); the header stays minimal.
    if run {
        let q = self.app.panels.search.query.clone();
        self.app.panels.search.query_epoch =
            self.app.panels.search.query_epoch.wrapping_add(1);
        self.app.panels.search.last_fired_query = q.clone();
        let results = self.run_query(&q);
        self.app.panels.search.results = results;
        // Reset row selection so the next ↓ press doesn't land in
        // stale territory.
        self.app.panels.search.selected_row = None;
    }
    }
}

impl View<'_> {
    /// Render the collapsible Results section: filename-fallback badge, the
    /// grouped hit list, and the Open/Copy/Reveal actions emitted by the
    /// per-row cards.
    fn results_section(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    // Results section — collapsible header per
    // `search-section-collapse-persisted`. The header click toggles the
    // expanded state and persists it to vault settings so the layout
    // sticks across vault re-opens.
    let results_expanded = app.panels.search.results_expanded;
    if app.panels.search.query.is_empty() {
        ui.label(
            egui::RichText::new("(type to search vault — semantic + lexical if indexed)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    // Filename-grep fallback badge: when the read store is unavailable
    // (indexer offline / vault not yet indexed), `run_query` falls back
    // to a basename grep. Surface that so the user understands why
    // semantic / lexical hits are missing (`search-filename-fallback-badge`).
    // Read store is always available now post-refactor; the "index
    // offline" badge that used to live here is dead.
    if crate::panels::discovery_pane::collapsible_header(
        ui,
        "search-results",
        "Results",
        results_expanded,
        app.panels.search.results.len(),
    ) {
        app.panels.search.results_expanded = !results_expanded;
        persist_search_setting(
            app,
            "search.sections.results_expanded",
            &serde_json::json!(app.panels.search.results_expanded),
        );
    }
    if !app.panels.search.results_expanded {
        return;
    }
    let results = app.panels.search.results.clone();
    let selected = app.panels.search.selected_row;
    let mut to_open: Option<(String, bool, u32)> = None;
    let mut copy: Option<String> = None;
    let mut reveal: Option<String> = None;
    let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
    if enter_pressed
        && let Some(idx) = selected
        && let Some(hit) = results.get(idx)
    {
        to_open = Some((hit.path.clone(), true, hit.chunk_index));
    }
    // Group hits by note (`search-result-grouped-by-note`). First
    // chunk per note carries the full card; subsequent chunks render
    // as compact indented rows underneath. Preserves the original
    // ordering of the first occurrence so the top-ranked note stays
    // first.
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, hit) in results.iter().enumerate() {
        if !groups.contains_key(&hit.path) {
            group_order.push(hit.path.clone());
        }
        groups.entry(hit.path.clone()).or_default().push(i);
    }
    for path in &group_order {
        let idxs = &groups[path];
        let first = idxs[0];
        let hit = &results[first];
        let highlighted = Some(first) == selected;
        // Active row gets an accent stroke for keyboard-nav feedback;
        // otherwise render the plain card.
        let action = if highlighted {
            let frame = egui::Frame::default()
                .stroke(egui::Stroke::new(2.0, theme::accent()))
                .inner_margin(egui::Margin::same(1));
            let mut action = CardAction::None;
            frame.show(ui, |ui| {
                action = result_card(ui, hit, /*allow_context=*/ true);
            });
            action
        } else {
            result_card(ui, hit, /*allow_context=*/ true)
        };
        match action {
            CardAction::None => {}
            CardAction::Open { sticky } => {
                to_open = Some((hit.path.clone(), sticky, hit.chunk_index))
            }
            CardAction::CopyPath => copy = Some(hit.path.clone()),
            CardAction::Reveal => reveal = Some(hit.path.clone()),
        }
        // Indented additional matches.
        if idxs.len() > 1 {
            ui.indent(("search-extra-chunks", path.as_str()), |ui| {
                for &i in &idxs[1..] {
                    let h = &results[i];
                    let highlighted = Some(i) == selected;
                    let label = if let Some(hp) = h.heading_path.as_deref() {
                        format!("> {} · chunk {}", hp, h.chunk_index)
                    } else {
                        format!("> chunk {}", h.chunk_index)
                    };
                    let resp = ui.add(egui::Button::selectable(highlighted, label));
                    if resp.clicked() {
                        let sticky = ui.input(|i| {
                            i.modifiers.command || i.modifiers.ctrl
                        });
                        to_open = Some((h.path.clone(), sticky, h.chunk_index));
                    }
                }
            });
        }
    }
    if let Some((rel, sticky, chunk_index)) = to_open {
        editor_pane::open_file(app, &rel, sticky);
        // Position the buffer selection at the start of `chunk_index`.
        // The indexer chunks at heading boundaries, so we re-chunk the
        // live buffer text to recover the byte offset. No-op when the
        // buffer isn't open yet or the chunk is the first one.
        if chunk_index != 0
            && let Some(buffer) = app.session.buffers.get_mut(&rel)
        {
            let text = buffer.editor.doc.to_string();
            let chunks = hiker_core::chunker::markdown::chunk(&text);
            if let Some(target) = chunks.get(chunk_index as usize) {
                buffer.editor.selection =
                    editor_core::selection::Selection::single(target.byte_start);
            }
        }
    }
    if let Some(path) = copy {
        ui.ctx().copy_text(path);
    }
    if let Some(rel) = reveal {
        // Expand every ancestor directory so the row's container is
        // visible, then arm a one-shot scroll target the files panel
        // honors on its next render (`reveal-in-sidebar-scroll`).
        let mut prefix_parts: Vec<&str> = rel.split('/').collect();
        prefix_parts.pop();
        let mut acc = String::new();
        for part in &prefix_parts {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            app.session.sidebar.expanded.insert(acc.clone());
        }
        crate::actions::ensure_panel_visible(
            app,
            crate::panels_registry::PANEL_FILES,
        );
        app.session.sidebar.scroll_target = Some(rel);
    }
    if results.is_empty() {
        ui.label(
            egui::RichText::new("(no matches)")
                .color(theme::muted())
                .small(),
        );
    }
    }
}

impl View<'_> {
    fn run_query(&self, q: &str) -> Vec<DiscoveryHit> {
    let app = &*self.app;
    if q.is_empty() {
        return Vec::new();
    }
    let store_mutex = &app.vault_session.services.read_store;

    // Each mode honours the user's toggle; semantic additionally requires
    // the embedder to be loaded.
    let mut modes = Modes {
        lexical: app.panels.search.lexical_on,
        semantic: false,
    };
    let embedding = if app.panels.search.semantic_on
        && let Some(emb) = app.vault_session.services.indexer.embedder()
    {
        modes.semantic = true;
        emb.embed_batch(&[q.to_string()])
            .ok()
            .and_then(|mut v| v.pop())
    } else {
        None
    };
    if !modes.lexical && !modes.semantic {
        return Vec::new();
    }

    let limit = app.panels.search.limit.clamp(5, 100) as u32;
    let mut lex_opts = app.panels.search.lexical_opts;
    if lex_opts.top_k == 0 {
        lex_opts.top_k = limit;
    }
    let mut sem_opts = app.panels.search.semantic_opts;
    if sem_opts.top_k == 0 {
        sem_opts.top_k = limit;
    }

    let store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return apply_post_filters(app, filename_search(app, q)),
    };
    let resp = hiker_core::search::query(
        &store,
        0,
        modes,
        Some(q),
        embedding.as_deref(),
        lex_opts,
        sem_opts,
    );
    let resp = match resp {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(error = %err, "search failed; falling back to filename grep");
            return filename_search(app, q);
        }
    };

    // Use the core-picked bucket (`resp.hits`) so RRF fusion (when both
    // modes are on) lands intact in the UI — `search-rrf-fusion`. The
    // per-bucket hits stay in lexical_hits/semantic_hits for future
    // "show what each backend found" affordances. The source tag we
    // attach is derived from which bucket the hit appeared in.
    let lex_paths: std::collections::HashSet<&str> =
        resp.lexical_hits.iter().map(|h| h.path.as_str()).collect();
    let sem_paths: std::collections::HashSet<&str> =
        resp.semantic_hits.iter().map(|h| h.path.as_str()).collect();
    let mut out: Vec<DiscoveryHit> = Vec::new();
    for hit in resp.hits {
        let tag = match (lex_paths.contains(hit.path.as_str()), sem_paths.contains(hit.path.as_str())) {
            (true, true) => "fused",
            (true, false) => "lex",
            (false, true) => "sem",
            (false, false) => "fused",
        };
        out.push(DiscoveryHit {
            path: hit.path,
            title: hit.title,
            heading_path: hit.heading_path,
            snippet: hit.snippet,
            score: hit.score,
            source_tag: Some(tag),
            chunk_index: hit.chunk_index,
        });
    }
    if out.is_empty() {
        return apply_post_filters(app, filename_search(app, q));
    }
    apply_post_filters(app, out)
    }
}

/// Apply source-types filter, order-by sort, and the global limit cap.
fn apply_post_filters(app: &AppState, mut hits: Vec<DiscoveryHit>) -> Vec<DiscoveryHit> {
    let exts: Vec<String> = app
        .panels
        .search
        .source_types
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if !exts.is_empty() {
        hits.retain(|h| {
            let ext = h
                .path
                .rsplit('.')
                .next()
                .map(str::to_ascii_lowercase)
                .unwrap_or_default();
            exts.iter().any(|e| e == &ext)
        });
    }
    if matches!(app.panels.search.order_by, OrderBy::Recent) {
        let mtime_for = |rel: &str| -> i64 {
            let Ok(abs) = app.vault_session.vault.abs_path(rel) else {
                return i64::MIN;
            };
            std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(i64::MIN)
        };
        let mut keyed: Vec<(i64, DiscoveryHit)> = hits
            .into_iter()
            .map(|h| (mtime_for(&h.path), h))
            .collect();
        keyed.sort_by_key(|x| std::cmp::Reverse(x.0));
        hits = keyed.into_iter().map(|(_, h)| h).collect();
    }
    hits.truncate(app.panels.search.limit.clamp(5, 100));
    hits
}

fn filename_search(app: &AppState, query: &str) -> Vec<DiscoveryHit> {
    let q = query.to_lowercase();
    let all = match app.vault_session.vault.walk_indexable_files("") {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    all.into_iter()
        .filter(|rel| rel.to_lowercase().contains(&q))
        .take(50)
        .map(|rel| {
            let title = rel
                .rsplit('/')
                .next()
                .map(|s| s.trim_end_matches(".md").to_string())
                .unwrap_or_else(|| rel.clone());
            DiscoveryHit {
                path: rel,
                title,
                heading_path: None,
                snippet: String::new(),
                score: 0.0,
                source_tag: Some("file"),
                chunk_index: 0,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CardAction {
    None,
    Open { sticky: bool },
    CopyPath,
    Reveal,
}

/// Render a single result-card row (per `discovery-result-card`). Layout:
/// title + score on the top line, vault-relative path subtitle below,
/// optional heading-path breadcrumb, then the matched-chunk excerpt. The
/// whole card is the click target.
pub(crate) fn result_card(
    ui: &mut egui::Ui,
    hit: &DiscoveryHit,
    allow_context: bool,
) -> CardAction {
    let mut action = CardAction::None;
    let frame_resp = egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Title row. Wrap in `ui.horizontal` to pin the layout to a
            // single-line row (the parent Frame measures height from the
            // child's min_size; nested `with_layout` without this wrapper
            // mis-reports height and the next card draws on top). Inside,
            // right-to-left lays out the score + source tag first so they
            // reserve their width, then the truncated title fills the
            // remainder via a nested left-to-right.
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if hit.score > 0.0 {
                        ui.label(
                            egui::RichText::new(format!("{:.2}", hit.score))
                                .small()
                                .color(theme::muted()),
                        );
                    }
                    if let Some(tag) = hit.source_tag {
                        ui.label(
                            egui::RichText::new(tag)
                                .small()
                                .monospace()
                                .color(theme::muted()),
                        );
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&hit.title).strong())
                                .truncate(),
                        );
                    });
                });
            });
            // Path subtitle.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&hit.path)
                        .small()
                        .color(theme::muted()),
                )
                .truncate(),
            );
            // Heading-path breadcrumb (omitted when none).
            if let Some(hp) = hit.heading_path.as_deref()
                && !hp.is_empty()
            {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(hp)
                            .small()
                            .italics()
                            .color(theme::muted()),
                    )
                    .truncate(),
                );
            }
            // Matched-chunk excerpt. `<mark>…</mark>` tokens from FTS5's
            // snippet() are rendered as a yellow background highlight per
            // `search-result-row` so users can see WHY the chunk matched.
            if !hit.snippet.trim().is_empty() {
                ui.add_space(2.0);
                // Split the snippet into plain text and `<mark>`-wrapped
                // runs so the highlighted portions get a yellow
                // background. Unbalanced `<mark>` opens trail off as plain.
                let mut segments: Vec<MarkPart<'_>> = Vec::new();
                let mut rest = hit.snippet.as_str();
                while let Some(open) = rest.find("<mark>") {
                    if open > 0 {
                        segments.push(MarkPart::Plain(&rest[..open]));
                    }
                    let after = &rest[open + 6..];
                    if let Some(close) = after.find("</mark>") {
                        segments.push(MarkPart::Highlighted(&after[..close]));
                        rest = &after[close + 7..];
                    } else {
                        segments.push(MarkPart::Plain(after));
                        rest = "";
                        break;
                    }
                }
                if !rest.is_empty() {
                    segments.push(MarkPart::Plain(rest));
                }
                ui.horizontal_wrapped(|ui| {
                    for part in segments {
                        match part {
                            MarkPart::Plain(s) if !s.is_empty() => {
                                ui.label(egui::RichText::new(s).small());
                            }
                            MarkPart::Plain(_) => {}
                            MarkPart::Highlighted(s) => {
                                ui.label(
                                    egui::RichText::new(s)
                                        .small()
                                        .background_color(egui::Color32::from_rgb(
                                            0xff, 0xf3, 0x88,
                                        )),
                                );
                            }
                        }
                    }
                });
            }
        });
    let resp = frame_resp.response.interact(egui::Sense::click());
    if resp.clicked() {
        let sticky = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
        action = CardAction::Open { sticky };
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if allow_context {
        resp.context_menu(|ui| {
            if ui.button("Open").clicked() {
                action = CardAction::Open { sticky: false };
                ui.close();
            }
            if ui.button("Open (sticky)").clicked() {
                action = CardAction::Open { sticky: true };
                ui.close();
            }
            if ui.button("Copy path").clicked() {
                action = CardAction::CopyPath;
                ui.close();
            }
            if ui.button("Reveal in sidebar").clicked() {
                action = CardAction::Reveal;
                ui.close();
            }
        });
    }
    ui.add_space(4.0);
    action
}

#[allow(dead_code)]
// TODO: use in copy/export paths to strip highlight markers.
fn strip_mark_tokens(s: &str) -> String {
    s.replace("<mark>", "").replace("</mark>", "")
}

enum MarkPart<'a> {
    Plain(&'a str),
    Highlighted(&'a str),
}
