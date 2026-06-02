//! Search feature — sidebar surface owning the query input, the per-mode
//! options menus, the typeahead debounce machinery, and the result list.
//! Migrated off `panels::search` + `panels_registry` to a real `Feature`
//! rendering through the narrow `feature::Ctx`: UI state lives on
//! `AppState::search_state` (reached via `ctx.state`), the index queries
//! run against `ctx.services`/`ctx.vault`, and every broad mutation
//! (open a note, reveal in the file tree, persist a setting, stash the
//! search-box id) is queued via `ctx.defer`. [feature-search-migration]
//!
//! Search wires `core::search::query` against the read store. Falls back
//! to lexical-only when the embedder isn't loaded yet. Filename grep is
//! kept as a final fallback when the store hasn't seen the file (e.g.
//! brand-new vault with empty index).
//!
//! The shared discovery-card shapes (`DiscoveryHit`, `CardAction`,
//! `result_card`) live here and are imported by `crate::related`, which
//! renders the same card row for vector-similar notes.

use std::sync::{Arc, Mutex};

use eframe::egui;

use hiker_core::embed::Embedder;
use hiker_core::search::Modes;
use hiker_core::store::Store;
use hiker_core::vault::Vault;

use crate::editor_pane;
use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::panels::zim;
use crate::search::state::{OrderBy, State};
use crate::state::AppState;
use hiker_theme as theme;

pub mod state;

const SEARCH_DEBOUNCE_MS: u64 = 250;

/// Zero-sized `Feature` descriptor for search. State lives in
/// `AppState::search_state`; the surface reaches it via
/// `Ctx::state.downcast_mut::<State>()`.
pub struct Search;

impl Feature for Search {
    fn id(&self) -> &'static str {
        "search"
    }
    fn label(&self) -> &'static str {
        "Search"
    }
    fn icon(&self) -> egui::Image<'static> {
        crate::icons::ICONS.image(crate::icons::Icon::Search)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&SearchSidebar)
    }
}

struct SearchSidebar;

impl SidebarSurface for SearchSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        // Apply any background-query results that landed since last frame
        // (`search-query-embed-spawn-blocking`) before drawing, so the freshest hits show.
        drain_results(ctx.state.downcast_mut::<State>().expect("search state"));
        // The workbench accordion owns the section header + collapse;
        // the body is the search input followed by the result list.
        // [feature-panel-single-accordion]
        search_input_and_run(ui, ctx);
        ui.add_space(8.0);
        results_section(ui, ctx);
    }
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

/// The finished product of one background query, sent from the
/// `spawn_blocking` task back to the panel (`search-query-embed-spawn-blocking`). `epoch` is
/// the `query_epoch` the query was fired at, so the panel can drop outcomes
/// that newer typing has already superseded.
pub struct SearchOutcome {
    pub epoch: u64,
    pub results: Vec<DiscoveryHit>,
    pub zim_results: Vec<zim::TitleHit>,
    pub zim_fulltext_results: Vec<zim::FullTextHit>,
}

/// Persist a search-related setting through `core::config::Config::set`
/// (per `search-mode-state-persisted`). Best-effort: a write failure logs
/// and is otherwise silent, so the option still applies in-session. Runs
/// as a deferred effect so it can reach the full `&AppState` (vault root
/// + config guards) the narrow `Ctx` doesn't expose.
fn persist_search_setting(app: &AppState, key: &str, value: &serde_json::Value) {
    crate::state::set_setting_quiet(
        app,
        hiker_core::config::SettingsScope::Vault,
        key,
        value,
        "search",
    );
}

/// Queue a deferred persist of `key`=`value`. Owns its strings so the
/// closure is `'static`.
fn defer_persist(ctx: &mut Ctx<'_>, key: &'static str, value: serde_json::Value) {
    ctx.defer(move |app| persist_search_setting(app, key, &value));
}

/// Lexical-mode option picker, anchored under the `Aa` toggle's
/// right-click (`search-lexical-options`). Case sensitivity, diacritics,
/// prefix and phrase matching — every row persists per-vault to
/// `search.lexical.*`. Caller sets `run` when any control changes so the
/// panel re-fires the query.
fn lexical_options_menu(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) {
    ui.label(egui::RichText::new("Lexical options").strong().small());
    let mut lex = ctx.state.downcast_ref::<State>().expect("search state").lexical_opts;
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
        ctx.state.downcast_mut::<State>().expect("search state").lexical_opts = lex;
        defer_persist(ctx, "search.lexical.case_sensitive", serde_json::json!(lex.case_sensitive));
        defer_persist(ctx, "search.lexical.diacritic_sensitive", serde_json::json!(lex.diacritic_sensitive));
        defer_persist(ctx, "search.lexical.prefix_match", serde_json::json!(lex.prefix_match));
        defer_persist(ctx, "search.lexical.phrase_mode", serde_json::json!(lex.phrase_mode));
        *run = true;
    }
}

/// Semantic-mode option picker, anchored under the brain toggle's
/// right-click (`search-semantic-options`). Minimum-similarity floor and
/// recency bias, persisted per-vault to `search.semantic.*`.
fn semantic_options_menu(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) {
    use hiker_core::config::sections::RecencyBias;

    ui.label(egui::RichText::new("Semantic options").strong().small());
    let mut sem = ctx.state.downcast_ref::<State>().expect("search state").semantic_opts;
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
        ctx.state.downcast_mut::<State>().expect("search state").semantic_opts = sem;
        defer_persist(
            ctx,
            "search.semantic.min_similarity",
            serde_json::json!(sem.min_similarity),
        );
        let bias_str = match sem.recency_bias {
            RecencyBias::Off => "off",
            RecencyBias::Mild => "mild",
            RecencyBias::Strong => "strong",
        };
        defer_persist(ctx, "search.semantic.recency_bias", serde_json::json!(bias_str));
        *run = true;
    }
}

/// Cross-mode controls served from the magnifying-glass right-click: the
/// Only-lexical / Only-semantic / Both convenience switches plus the
/// Limit / Types / Order filters that apply regardless of which backends
/// are active. Per-mode tuning lives on the toggles themselves
/// (`lexical_options_menu` / `semantic_options_menu`).
fn filters_menu(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) {
    // Mode toggles — duplicate the left-click flip on the toggle buttons
    // so the menu is a complete control surface on its own.
    ui.label(egui::RichText::new("Modes").strong().small());
    let st = ctx.state.downcast_mut::<State>().expect("search state");
    if ui
        .checkbox(&mut st.lexical_on, "Lexical")
        .on_hover_text("Substring / token matches from the index")
        .changed()
    {
        *run = true;
    }
    if ui
        .checkbox(&mut st.semantic_on, "Semantic")
        .on_hover_text("Embedding-based similarity")
        .changed()
    {
        *run = true;
    }
    ui.horizontal(|ui| {
        if ui.small_button("Only lexical").clicked() {
            st.lexical_on = true;
            st.semantic_on = false;
            *run = true;
        }
        if ui.small_button("Only semantic").clicked() {
            st.semantic_on = true;
            st.lexical_on = false;
            *run = true;
        }
        if ui.small_button("Both").clicked() {
            st.lexical_on = true;
            st.semantic_on = true;
            *run = true;
        }
    });

    ui.separator();
    ui.label(egui::RichText::new("Filters").strong().small());
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Limit").small().color(theme::muted()));
        if ui.add(egui::Slider::new(&mut st.limit, 5..=100)).changed() {
            *run = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Types").small().color(theme::muted()));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut st.source_types)
                .hint_text("md, txt")
                .desired_width(140.0),
        );
        if resp.lost_focus() {
            *run = true;
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Order").small().color(theme::muted()));
        let cur = st.order_by;
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
            st.order_by = new_order;
            *run = true;
        }
    });
}

/// Render the search input row (icon + text edit + keyboard nav), honour
/// the typeahead debounce, and fire the search when the debounce window
/// closes or Enter is pressed.
fn search_input_and_run(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    let mut run = false;
    let mut run_search_immediate = false;
    let resp = input_row(ui, ctx, &mut run);
    // Stash the search box's id so the editor panel can tell when this
    // field (not the editor) owns keyboard focus and should keep Ctrl-Z.
    let input_id = resp.id;
    ctx.defer(move |app| app.ui.search_input_id = Some(input_id));
    {
        let st = ctx.state.downcast_mut::<State>().expect("search state");
        if st.focus_query_next_frame {
            st.focus_query_next_frame = false;
            resp.request_focus();
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            // Enter forces an immediate run, bypassing the debounce.
            run_search_immediate = true;
        }
        if resp.changed() {
            // Defer the actual query — debounce via pending_query_at.
            st.pending_query_at = Some(std::time::Instant::now());
        }
    }
    keyboard_nav(ui, ctx, &resp);
    honour_debounce(ui, ctx, &mut run);
    if run_search_immediate {
        ctx.state.downcast_mut::<State>().expect("search state").pending_query_at = None;
        run = true;
    }
    if run {
        fire_query(ctx, ui.ctx());
    }
    // While a background query is in flight, show a small spinner + hint. The
    // spinner self-animates (egui repaints each frame it's shown), which also
    // keeps `drain_results` polling until the outcome lands. Previous results
    // stay visible underneath so the list doesn't flash empty mid-type.
    if ctx.state.downcast_ref::<State>().expect("search state").in_flight {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(egui::RichText::new("Searching…").small().color(theme::muted()));
        });
    }
}

/// Lay out the input row (magnifying-glass filter menu, the two mode
/// toggles, and the query text field) and return the text field's
/// response. Sets `run` when a toggle flips.
fn input_row(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) -> egui::Response {
    ui.horizontal(|ui| {
        // Magnifying glass carries the cross-mode controls (mode
        // convenience switches + Limit / Types / Order) on right-click, so
        // the header stays one line tall. Per-mode tuning lives on the two
        // toggles to its right.
        let icon_resp = ui
            .add(crate::icons::ICONS.image(crate::icons::Icon::Search).sense(egui::Sense::click()))
            .on_hover_text("Right-click for filters");
        icon_resp.context_menu(|ui| {
            filters_menu(ui, ctx, run);
        });
        // Lay the row out so the toggles reserve their width on the right
        // and the text input fills whatever's left — otherwise a full-width
        // input pushes the toggles past the panel edge, where the
        // vertical-only ScrollArea clips them out of sight. Right-to-left
        // places the semantic toggle rightmost, then lexical to its left,
        // then the input claims the remaining space.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            mode_toggles(ui, ctx, run);
            // Input fills the width left after the toggles.
            let query = &mut ctx.state.downcast_mut::<State>().expect("search state").query;
            ui.add(
                egui::TextEdit::singleline(query)
                    .hint_text("Search vault…")
                    .desired_width(ui.available_width()),
            )
        })
        .inner
    })
    .inner
}

/// The two icon-only mode toggles (semantic brain + lexical `Aa`) with
/// their per-mode option context menus. Left-click flips the mode;
/// right-click opens that mode's tuning menu (`search-mode-options-menu`).
fn mode_toggles(ui: &mut egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) {
    let sem_on = ctx.state.downcast_ref::<State>().expect("search state").semantic_on;
    let sem_resp = ui
        .add(
            egui::ImageButton::new(crate::icons::ICONS.image(crate::icons::Icon::Brain))
                .selected(sem_on),
        )
        .on_hover_text("Semantic search");
    if sem_resp.clicked() {
        let st = ctx.state.downcast_mut::<State>().expect("search state");
        st.semantic_on = !sem_on;
        let now = st.semantic_on;
        defer_persist(ctx, "search.modes.semantic", serde_json::json!(now));
        *run = true;
    }
    sem_resp.context_menu(|ui| {
        semantic_options_menu(ui, ctx, run);
    });
    let lex_on = ctx.state.downcast_ref::<State>().expect("search state").lexical_on;
    // Match the brain toggle's active treatment: when on, the button
    // carries the selection fill + accent border so the pressed state
    // reads at a glance. A bare `selectable_label` only paints the
    // (near-transparent) selection fill with no border, so its active
    // state is invisible on the light panel.
    let mut lex_btn = egui::Button::new(egui::RichText::new("Aa").strong());
    if lex_on {
        lex_btn = lex_btn
            .fill(ui.visuals().selection.bg_fill)
            .stroke(ui.visuals().selection.stroke);
    }
    let lex_resp = ui.add(lex_btn).on_hover_text("Lexical search");
    if lex_resp.clicked() {
        let st = ctx.state.downcast_mut::<State>().expect("search state");
        st.lexical_on = !lex_on;
        let now = st.lexical_on;
        defer_persist(ctx, "search.modes.lexical", serde_json::json!(now));
        *run = true;
    }
    lex_resp.context_menu(|ui| {
        lexical_options_menu(ui, ctx, run);
    });
}

/// Keyboard nav on the focused input: ↑/↓ move the selected row, Esc
/// clears the selection (then the query) per `search-keyboard-nav`.
fn keyboard_nav(ui: &egui::Ui, ctx: &mut Ctx<'_>, resp: &egui::Response) {
    if !resp.has_focus() {
        return;
    }
    let (up, down, esc) = ui.input(|i| {
        (
            i.key_pressed(egui::Key::ArrowUp),
            i.key_pressed(egui::Key::ArrowDown),
            i.key_pressed(egui::Key::Escape),
        )
    });
    let st = ctx.state.downcast_mut::<State>().expect("search state");
    let results_len = st.results.len();
    if results_len > 0 {
        if down {
            let cur = st.selected_row.unwrap_or(usize::MAX);
            let next = if cur == usize::MAX { 0 } else { (cur + 1).min(results_len - 1) };
            st.selected_row = Some(next);
        }
        if up {
            let cur = st.selected_row.unwrap_or(0);
            st.selected_row = Some(cur.saturating_sub(1));
        }
    }
    if esc {
        if st.selected_row.is_some() {
            st.selected_row = None;
        } else {
            st.query.clear();
            st.results.clear();
        }
    }
}

/// Honour the debounce window: only fire when the input has been quiet for
/// `SEARCH_DEBOUNCE_MS` *and* the query has actually changed since the
/// last fire. Keeps embedding-per-keystroke from landing under heavy
/// typing. Sets `run` when the window closes on a changed query.
fn honour_debounce(ui: &egui::Ui, ctx: &mut Ctx<'_>, run: &mut bool) {
    let st = ctx.state.downcast_mut::<State>().expect("search state");
    let Some(deadline) = st.pending_query_at else {
        return;
    };
    let elapsed = deadline.elapsed().as_millis() as u64;
    if elapsed >= SEARCH_DEBOUNCE_MS {
        st.pending_query_at = None;
        if st.query != st.last_fired_query {
            *run = true;
        }
    } else {
        // Ensure egui repaints once the debounce window expires so the
        // deferred search actually fires.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(
            SEARCH_DEBOUNCE_MS.saturating_sub(elapsed),
        ));
    }
}

/// Fire the current query off the UI thread (`search-query-embed-spawn-blocking`): bump the
/// epoch, snapshot the query + params, and hand the work to a `spawn_blocking`
/// task. The task embeds the query, runs the index + federated-ZIM searches,
/// and ships a [`SearchOutcome`] back over the result channel; [`drain_results`]
/// applies it on a later frame. The UI never blocks on the query (embedding is
/// the slow part), so typing stays smooth.
///
/// Outside a Tokio runtime (tests / headless) the work runs inline; the result
/// still flows through the channel and is drained on the next `render`.
fn fire_query(ctx: &mut Ctx<'_>, egui_ctx: &egui::Context) {
    let (q, epoch, params, tx) = {
        let st = ctx.state.downcast_mut::<State>().expect("search state");
        st.query_epoch = st.query_epoch.wrapping_add(1);
        st.last_fired_query = st.query.clone();
        st.in_flight = true;
        (
            st.query.clone(),
            st.query_epoch,
            QueryParams::from_state(st),
            st.result_tx.clone(),
        )
    };

    // Clone the `Send` handles the query needs so it can run on another thread.
    // Embedding only happens in semantic mode, so skip grabbing the embedder
    // otherwise.
    let read_store = Arc::clone(&ctx.services.read_store);
    let embedder = if params.semantic_on {
        ctx.services.indexer.embedder()
    } else {
        None
    };
    let vault = Arc::clone(ctx.vault);
    let egui_ctx = egui_ctx.clone();

    let job = move || {
        let outcome = compute_outcome(epoch, &read_store, embedder.as_deref(), &vault, &params, &q);
        // A send error means the panel (and its receiver) is gone — drop it.
        if tx.send(outcome).is_ok() {
            // Wake the UI so the next frame drains the result.
            egui_ctx.request_repaint();
        }
    };

    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(job);
        }
        Err(_) => job(),
    }
}

/// Run the full query (index + federated ZIM) and package it as a
/// [`SearchOutcome`] tagged with `epoch`. Pure of any UI/`Ctx` state so it can
/// run on a background `spawn_blocking` task.
fn compute_outcome(
    epoch: u64,
    read_store: &Mutex<Store>,
    embedder: Option<&dyn Embedder>,
    vault: &Vault,
    params: &QueryParams,
    q: &str,
) -> SearchOutcome {
    let results = run_query(read_store, embedder, vault, params, q);
    // Federated ZIM search over the vault's `.zim` archives (now against the
    // global, thread-safe registry). Two complementary paths:
    //   * title-prefix (instant binary search), bounded per archive;
    //   * full-text body search (BM25 over the embedded Xapian index),
    //     bounded per archive — matches words in article bodies, not titles.
    // Both no-op on an empty query / archives that lack the relevant index.
    let (zim_results, zim_fulltext_results) = if q.trim().is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (
            zim::federated_search(vault.root(), q, 20),
            zim::federated_fulltext_search(vault.root(), q, 10),
        )
    };
    SearchOutcome {
        epoch,
        results,
        zim_results,
        zim_fulltext_results,
    }
}

/// Drain finished background queries, applying the one matching the current
/// `query_epoch` (the latest) and dropping any the user's newer typing has
/// already superseded (`search-query-embed-spawn-blocking`). Clears the row selection so the
/// next ↓ press lands fresh.
fn drain_results(st: &mut State) {
    let mut latest: Option<SearchOutcome> = None;
    if let Ok(rx) = st.result_rx.lock() {
        while let Ok(outcome) = rx.try_recv() {
            if outcome.epoch == st.query_epoch {
                latest = Some(outcome);
            }
        }
    }
    if let Some(outcome) = latest {
        st.results = outcome.results;
        st.zim_results = outcome.zim_results;
        st.zim_fulltext_results = outcome.zim_fulltext_results;
        st.selected_row = None;
        st.in_flight = false;
    }
}

/// Render the Results section: the grouped hit list and the Open / Copy /
/// Reveal actions emitted by the per-row cards. Mutations (open a note,
/// reveal in the file tree) are deferred.
fn results_section(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    // The workbench accordion is the panel's single header; results render
    // directly under the search input — no inner collapsible.
    // [feature-panel-single-accordion]
    let st = ctx.state.downcast_ref::<State>().expect("search state");
    if st.query.is_empty() {
        ui.label(
            egui::RichText::new("(type to search vault — semantic + lexical if indexed)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    let results = st.results.clone();
    let selected = st.selected_row;
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
    render_groups(ui, &results, selected, &mut to_open, &mut copy, &mut reveal);
    if let Some((rel, sticky, chunk_index)) = to_open {
        ctx.defer(move |app| open_at_chunk(app, &rel, sticky, chunk_index));
    }
    if let Some(path) = copy {
        ui.ctx().copy_text(path);
    }
    if let Some(rel) = reveal {
        ctx.defer(move |app| reveal_in_files(app, &rel));
    }
    let zim_empty = render_zim_results(ui, ctx);
    if results.is_empty() && zim_empty {
        ui.label(
            egui::RichText::new("(no matches)")
                .color(theme::muted())
                .small(),
        );
    }
}

/// Render the federated ZIM hit group(s): per archive, a title-prefix section
/// ("<archive>  ·  ZIM") and a full-text body section ("<archive>  ·
/// full-text"), each row a clickable title that opens the ZIM viewer at that
/// article. Returns `true` when there were no ZIM hits of either kind (so the
/// caller can decide whether to show "(no matches)").
fn render_zim_results(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) -> bool {
    let st = ctx.state.downcast_ref::<State>().expect("search state");
    if st.zim_results.is_empty() && st.zim_fulltext_results.is_empty() {
        return true;
    }
    // (title, zim_path, article_url) rows, grouped under a section header.
    type Row = (String, String, String);
    let mut order: Vec<String> = Vec::new();
    let mut by_section: std::collections::HashMap<String, Vec<Row>> =
        std::collections::HashMap::new();
    let mut push = |header: String, row: Row,
                    order: &mut Vec<String>,
                    by: &mut std::collections::HashMap<String, Vec<Row>>| {
        if !by.contains_key(&header) {
            order.push(header.clone());
        }
        by.entry(header).or_default().push(row);
    };
    // Title-prefix sections first, then full-text — keeps the instant matches
    // on top and the body matches clearly distinguished below.
    for hit in &st.zim_results {
        push(
            format!("{}  ·  ZIM", hit.archive_label),
            (hit.title.clone(), hit.zim_path.clone(), hit.article_url.clone()),
            &mut order,
            &mut by_section,
        );
    }
    for hit in &st.zim_fulltext_results {
        push(
            format!("{}  ·  full-text", hit.archive_label),
            (hit.title.clone(), hit.zim_path.clone(), hit.article_url.clone()),
            &mut order,
            &mut by_section,
        );
    }
    let mut to_open: Option<(String, String)> = None;
    ui.add_space(6.0);
    for header in &order {
        ui.label(
            egui::RichText::new(header).strong().small().color(theme::muted()),
        );
        for (title, zim_path, url) in &by_section[header] {
            if ui.add(egui::Button::selectable(false, title)).clicked() {
                to_open = Some((zim_path.clone(), url.clone()));
            }
        }
        ui.add_space(4.0);
    }
    if let Some((zim_path, url)) = to_open {
        ctx.defer(move |app| zim::open_at_article(app, &zim_path, &url));
    }
    false
}

/// Group hits by note (`search-result-grouped-by-note`): the first chunk
/// per note carries the full card; subsequent chunks render as compact
/// indented rows underneath. The first-occurrence order is preserved so
/// the top-ranked note stays first. Card actions land in the out-params.
fn render_groups(
    ui: &mut egui::Ui,
    results: &[DiscoveryHit],
    selected: Option<usize>,
    to_open: &mut Option<(String, bool, u32)>,
    copy: &mut Option<String>,
    reveal: &mut Option<String>,
) {
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
                *to_open = Some((hit.path.clone(), sticky, hit.chunk_index));
            }
            CardAction::CopyPath => *copy = Some(hit.path.clone()),
            CardAction::Reveal => *reveal = Some(hit.path.clone()),
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
                        let sticky = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
                        *to_open = Some((h.path.clone(), sticky, h.chunk_index));
                    }
                }
            });
        }
    }
}

/// Open `rel` and position the buffer selection at the start of
/// `chunk_index`. The indexer chunks at heading boundaries, so we
/// re-chunk the live buffer text to recover the byte offset. No-op when
/// the buffer isn't open yet or the chunk is the first one.
fn open_at_chunk(app: &mut AppState, rel: &str, sticky: bool, chunk_index: u32) {
    editor_pane::open_file(app, rel, sticky);
    if chunk_index != 0
        && let Some(buffer) = app.session.buffers.get_mut(rel)
    {
        let text = buffer.editor.doc.to_string();
        let chunks = hiker_core::chunker::markdown::chunk(&text);
        if let Some(target) = chunks.get(chunk_index as usize) {
            buffer.editor.selection =
                editor_core::selection::Selection::single(target.byte_start);
        }
    }
}

/// Reveal `rel` in the files panel: expand every ancestor directory so the
/// row's container is visible, make the files panel visible, then arm a
/// one-shot scroll target the files panel honors on its next render
/// (`reveal-in-sidebar-scroll`).
fn reveal_in_files(app: &mut AppState, rel: &str) {
    let mut prefix_parts: Vec<&str> = rel.split('/').collect();
    prefix_parts.pop();
    let mut acc = String::new();
    for part in &prefix_parts {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        app.file_tree_state.expanded.insert(acc.clone());
    }
    crate::actions::ensure_panel_visible(app, crate::tab::PANEL_FILES);
    app.file_tree_state.scroll_target = Some(rel.to_string());
}

/// Immutable snapshot of the query-affecting state slice, cloned out
/// before the index query so the read borrow on `ctx.state` is released
/// while the query runs against `ctx.services`/`ctx.vault`.
#[derive(Clone)]
struct QueryParams {
    lexical_on: bool,
    semantic_on: bool,
    limit: usize,
    source_types: String,
    order_by: OrderBy,
    lexical_opts: hiker_core::search::LexicalOpts,
    semantic_opts: hiker_core::search::SemanticOpts,
}

impl QueryParams {
    fn from_state(st: &State) -> Self {
        Self {
            lexical_on: st.lexical_on,
            semantic_on: st.semantic_on,
            limit: st.limit,
            source_types: st.source_types.clone(),
            order_by: st.order_by,
            lexical_opts: st.lexical_opts,
            semantic_opts: st.semantic_opts,
        }
    }
}

/// Run the configured search against the read store (per the active mode
/// toggles + tuning), falling back to a filename grep when the store is
/// unavailable, errors, or returns nothing. Reads the index via
/// `ctx.services` and the vault via `ctx.vault`.
fn run_query(
    read_store: &Mutex<Store>,
    embedder: Option<&dyn Embedder>,
    vault: &Vault,
    params: &QueryParams,
    q: &str,
) -> Vec<DiscoveryHit> {
    if q.is_empty() {
        return Vec::new();
    }

    // Each mode honours the user's toggle; semantic additionally requires
    // the embedder to be loaded. The embed call (ONNX inference) is the
    // slow part — it runs here on the background task, never the UI thread.
    let mut modes = Modes {
        lexical: params.lexical_on,
        semantic: false,
    };
    let embedding = if params.semantic_on
        && let Some(emb) = embedder
    {
        modes.semantic = true;
        emb.embed_batch(&[q.to_string()]).ok().and_then(|mut v| v.pop())
    } else {
        None
    };
    if !modes.lexical && !modes.semantic {
        return Vec::new();
    }

    let limit = params.limit.clamp(5, 100) as u32;
    let mut lex_opts = params.lexical_opts;
    if lex_opts.top_k == 0 {
        lex_opts.top_k = limit;
    }
    let mut sem_opts = params.semantic_opts;
    if sem_opts.top_k == 0 {
        sem_opts.top_k = limit;
    }

    let store = match read_store.lock() {
        Ok(s) => s,
        Err(_) => return apply_post_filters(vault, params, filename_search(vault, q)),
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
            drop(store);
            tracing::warn!(error = %err, "search failed; falling back to filename grep");
            return filename_search(vault, q);
        }
    };

    // Use the core-picked bucket (`resp.hits`) so RRF fusion (when both
    // modes are on) lands intact in the UI — `search-rrf-fusion`. The
    // source tag is derived from which bucket the hit appeared in.
    let lex_paths: std::collections::HashSet<&str> =
        resp.lexical_hits.iter().map(|h| h.path.as_str()).collect();
    let sem_paths: std::collections::HashSet<&str> =
        resp.semantic_hits.iter().map(|h| h.path.as_str()).collect();
    let mut out: Vec<DiscoveryHit> = Vec::new();
    for hit in resp.hits {
        let tag = match (
            lex_paths.contains(hit.path.as_str()),
            sem_paths.contains(hit.path.as_str()),
        ) {
            (true, false) => "lex",
            (false, true) => "sem",
            _ => "fused",
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
    drop(store);
    if out.is_empty() {
        return apply_post_filters(vault, params, filename_search(vault, q));
    }
    apply_post_filters(vault, params, out)
}

/// Apply source-types filter, order-by sort, and the global limit cap.
fn apply_post_filters(
    vault: &hiker_core::vault::Vault,
    params: &QueryParams,
    mut hits: Vec<DiscoveryHit>,
) -> Vec<DiscoveryHit> {
    let exts: Vec<String> = params
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
    if matches!(params.order_by, OrderBy::Recent) {
        let mtime_for = |rel: &str| -> i64 {
            let Ok(abs) = vault.abs_path(rel) else {
                return i64::MIN;
            };
            std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(i64::MIN)
        };
        let mut keyed: Vec<(i64, DiscoveryHit)> =
            hits.into_iter().map(|h| (mtime_for(&h.path), h)).collect();
        keyed.sort_by_key(|x| std::cmp::Reverse(x.0));
        hits = keyed.into_iter().map(|(_, h)| h).collect();
    }
    hits.truncate(params.limit.clamp(5, 100));
    hits
}

/// Final-fallback filename grep against the vault's indexable files, used
/// when the store is unavailable or returns no hits (e.g. a brand-new
/// vault with an empty index).
fn filename_search(vault: &hiker_core::vault::Vault, query: &str) -> Vec<DiscoveryHit> {
    let q = query.to_lowercase();
    let all = match vault.walk_indexable_files("") {
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
pub enum CardAction {
    None,
    Open { sticky: bool },
    CopyPath,
    Reveal,
}

/// Render a single result-card row (per `discovery-result-card`). Layout:
/// title + score on the top line, vault-relative path subtitle below,
/// optional heading-path breadcrumb, then the matched-chunk excerpt. The
/// whole card is the click target.
pub fn result_card(ui: &mut egui::Ui, hit: &DiscoveryHit, allow_context: bool) -> CardAction {
    let mut action = CardAction::None;
    let frame_resp = egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            card_title_row(ui, hit);
            // Path subtitle.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&hit.path).small().color(theme::muted()),
                )
                .truncate(),
            );
            // Heading-path breadcrumb (omitted when none).
            if let Some(hp) = hit.heading_path.as_deref()
                && !hp.is_empty()
            {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(hp).small().italics().color(theme::muted()),
                    )
                    .truncate(),
                );
            }
            card_snippet(ui, &hit.snippet);
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

/// The card's top line: title (truncated, fills the row) with the score +
/// source tag pinned to the right. Wrapped in `ui.horizontal` to pin a
/// single-line row — the parent Frame measures height from the child's
/// min_size, and a nested `with_layout` without this wrapper mis-reports
/// height so the next card draws on top.
fn card_title_row(ui: &mut egui::Ui, hit: &DiscoveryHit) {
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
                    egui::RichText::new(tag).small().monospace().color(theme::muted()),
                );
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&hit.title).strong()).truncate(),
                );
            });
        });
    });
}

/// The matched-chunk excerpt. `<mark>…</mark>` tokens from FTS5's
/// snippet() render as a yellow background highlight per
/// `search-result-row` so users can see WHY the chunk matched. Unbalanced
/// `<mark>` opens trail off as plain text.
fn card_snippet(ui: &mut egui::Ui, snippet: &str) {
    if snippet.trim().is_empty() {
        return;
    }
    ui.add_space(2.0);
    let mut segments: Vec<MarkPart<'_>> = Vec::new();
    let mut rest = snippet;
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
                            .background_color(egui::Color32::from_rgb(0xff, 0xf3, 0x88)),
                    );
                }
            }
        }
    });
}

enum MarkPart<'a> {
    Plain(&'a str),
    Highlighted(&'a str),
}
