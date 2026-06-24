//! Wikilink navigation glue for the buffer panel: building the live-title
//! resolver the decoration layer renders pills from, and turning a clicked
//! wikilink pill into an opened note. Kept beside `mod.rs` rather than inside
//! it so the editor render path stays readable and within its length budget;
//! everything here is the app-layer seam between `editor_md::links` (which
//! emits the clickable pills) and `core::wikilink` + the store (which resolve
//! a target to a concrete vault path).

use std::sync::{Arc, Mutex};

use eframe::egui;
use editor_view::viewport::{ClickAction, ClickZone};
use hiker_core::store::Store;
use hiker_core::wikilink::{self, AmbiguityPolicy, Resolution};
use spec_engine::{DerivedNodeSource, SourceId};

use crate::state::{AppState, ToastLevel};

/// Build the wikilink live-title resolver. Under the path-form
/// (`wikilink-path-form`) the target *is* the path-or-name the user
/// typed; the resolver hands back a display label by stripping `.md`
/// from the basename. A click-time resolver (below) does the actual
/// path lookup. The returned closure owns an `Arc` clone, so it
/// borrows neither `AppState` nor the active buffer.
///
/// The `read_store` borrow is kept on the signature for forward
/// compatibility with the frontmatter-title path (`wikilink-render`
/// resolves the target's current `title` frontmatter when set);
/// today the resolver doesn't read it yet.
///
/// status: wikilink-render
pub(crate) fn title_resolver(
    _read_store: Arc<Mutex<Store>>,
) -> impl Fn(&str) -> Option<String> {
    move |target: &str| {
        // A `[[code:<repo_id>/<symbol>]]` spec→code link renders with a friendly label derived
        // from the moniker (`Builder::top_level_split` for an impl-qualified body) so the pill
        // shows resolved (not broken-red); the stored body stays the canonical short-sym form.
        // Cheap by design — no adapter binding per frame; the click path does the real
        // resolution. status: spec-code-link · status: wikilink-code-pretty-label
        if let Some((_, symbol)) = wikilink::parse_code_target(target) {
            return Some(wikilink::code_link_label(symbol));
        }
        // A `[[spec:<slug>]]` spec link renders with the slug as its label; the click path
        // resolves through the store's spec-anchor index. status: wikilink-spec-links
        if let Some(slug) = wikilink::parse_spec_target(target) {
            return Some(slug.to_string());
        }
        // Strip any `#section` anchor so the pill shows the page title, not the
        // raw `Page#Heading` body. status: wikilink-headings-blocks
        let (page, section) = wikilink::split_target_section(target);
        if page.is_empty() {
            // A same-document anchor: label it with the heading, or with the
            // block id (sans `^`) for a `[[#^block]]` anchor.
            // status: wikilink-block-anchors
            return section.map(|s| {
                wikilink::block_anchor_id(s).map_or_else(|| s.to_string(), str::to_string)
            });
        }
        Some(wikilink::title_for_path(page).to_string())
    }
}

/// Dispatch this frame's wikilink pill clicks. Each tagged id carries the
/// link's full-span start byte; re-parse the link there against the active
/// buffer's current text and open the target. status: wikilink-click-open
pub(crate) fn handle_clicks(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    clicks: &[u64],
    mod_click: bool,
) {
    if clicks.is_empty() {
        return;
    }
    let Some(text) = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    for &id in clicks {
        let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
        open_at(app, &text, offset, mod_click);
    }
    ctx.request_repaint();
}

/// Resolve and open the wikilink whose full `[[…]]` span starts at `offset`
/// in `text`. Path-form (`wikilink-resolve`): a bare-name target matches
/// by basename; an explicit-path target matches by exact path. Ambiguity
/// policy is read from `[wikilinks] ambiguous_resolution`.
/// `sticky` (Mod-click) opens a sticky tab instead of the preview slot.
///
/// status: wikilink-click-open
fn open_at(app: &mut AppState, text: &str, offset: usize, sticky: bool) {
    // The click id's offset is the start of either a `[[…]]` wikilink or a
    // `[label](dest)` vault-target markdown link (`markdown-link-vault-nav`).
    // Prefer the wikilink parse; fall back to the markdown-link dest.
    let target = wikilink::parse_links(text)
        .into_iter()
        .find(|l| l.span.start == offset)
        .map(|l| l.target)
        .or_else(|| markdown_link_dest_at(text, offset));
    let Some(target) = target else { return };
    // Spec→code link (`[[code:<repo_id>/<symbol>]]`): resolve through the code-intelligence port and
    // navigate to the code graph instead of a vault path. status: spec-code-link
    if let Some((repo_id, symbol)) = wikilink::parse_code_target(&target) {
        open_code_target(app, repo_id, symbol);
        return;
    }
    // Spec link (`[[spec:<slug>]]`): resolve the slug through the store's spec-anchor
    // index and open the defining note at the anchor line. status: wikilink-spec-links
    if let Some(slug) = wikilink::parse_spec_target(&target) {
        open_spec_target(app, slug, sticky);
        return;
    }
    open_target(app, text, &target, sticky);
}

/// Navigate a `[[spec:<slug>]]` link: look the slug up in the `spec_anchors` index (one
/// indexed query — no vault walk; the indexer re-derives anchors on every ingest), open
/// the defining note, and scroll to the `[slug]` anchor line. When more than one note
/// defines the anchor, the referrer's folder wins, else the lexicographically first path —
/// deterministic, mirroring the spec engine's resolution posture. A slug the index doesn't
/// know yields a toast (the vault may simply not have finished indexing).
/// status: wikilink-spec-links
fn open_spec_target(app: &mut AppState, slug: &str, sticky: bool) {
    let paths = match app.vault_session.services.read_store.lock() {
        Ok(store) => store.spec_anchor_paths(slug).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let Some(path) = pick_anchor_path(&paths, active_buffer_path(app).as_deref()) else {
        app.push_toast(format!("spec slug not found: {slug}"), ToastLevel::Warn);
        return;
    };
    crate::editor_pane::open_file(app, &path, sticky);
    // Land on the anchor line, found live in the (possibly just-opened) buffer text —
    // robust against any index/disk lag, same funnel as heading anchors.
    let Some(text) = app
        .session
        .buffers
        .get(&path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    if let Some(byte) = wikilink::find_slug_anchor_byte(&text, slug) {
        scroll_buffer_to_byte(app, &path, byte);
    }
}

/// Pick the note a multi-defined spec anchor resolves to: a path sharing the referrer's
/// parent folder first, else the first (the store returns them sorted).
fn pick_anchor_path(paths: &[String], referrer: Option<&str>) -> Option<String> {
    if let Some(referrer) = referrer {
        let dir = referrer.rsplit_once('/').map_or("", |(d, _)| d);
        if let Some(p) = paths
            .iter()
            .find(|p| p.rsplit_once('/').map_or("", |(d, _)| d) == dir)
        {
            return Some(p.clone());
        }
    }
    paths.first().cloned()
}

/// Navigate a `[[code:<repo_id>/<symbol>]]` link (`spec-code-link`, Phase A): bind the repo's SCIP
/// adapter (lazily, via the registry), resolve the symbol through the `DerivedNodeSource` port, open
/// the project's code-graph tab, and surface the resolved location via a toast (+ best-effort
/// preselect of the node). Authoring + drift UI are later phases.
fn open_code_target(app: &mut AppState, repo_id: &str, symbol: &str) {
    let Some((adapter, note)) = crate::code_sources::resolve_or_bind(app, repo_id) else {
        app.push_toast(format!("no project binds repo '{repo_id}'"), ToastLevel::Warn);
        return;
    };
    let src = SourceId(repo_id.to_string());
    let Some(handle) = adapter.resolve(symbol, &src) else {
        app.push_toast(
            format!("code symbol not found: {symbol} in {repo_id}"),
            ToastLevel::Warn,
        );
        return;
    };
    crate::panels::code_graph::open(app, crate::tab::CodeSource::Project(note.clone()));
    // Best-effort preselect: if the graph view for this source is already built, point its selection
    // at the resolved node so the detail line shows it. A not-yet-built (lazy) view is fine — the
    // toast below is sufficient for Phase A.
    let key = crate::tab::CodeSource::Project(note).key();
    if let Some(doc) = app.panels.code_graph_docs.get_mut(&key) {
        doc.selected = Some(handle.id.clone());
    }
    let loc = adapter
        .locate(&handle)
        .map(|l| format!("{}:{}", l.file, l.start_line + 1))
        .unwrap_or_else(|| "?".to_string());
    app.push_toast(format!("code: {symbol} @ {loc}"), ToastLevel::Info);
}

/// The destination of a `[label](dest)` markdown link whose `[` is at `offset`,
/// when one is well-formed there. Mirrors `editor_md::links::parse_md_link`'s
/// single-line rule so the click handler and the decoration agree on spans.
fn markdown_link_dest_at(text: &str, offset: usize) -> Option<String> {
    let rest = text.get(offset..)?;
    let bytes = rest.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let close = rest[1..].find([']', '\n'])?;
    if rest.as_bytes().get(1 + close) != Some(&b']') {
        return None;
    }
    let after = &rest[close + 2..];
    let dest = after.strip_prefix('(')?;
    let end = dest.find([')', '\n'])?;
    if dest.as_bytes().get(end) != Some(&b')') {
        return None;
    }
    let dest = dest[..end].trim();
    (!dest.is_empty()).then(|| dest.to_owned())
}

/// Resolve a wikilink / markdown-link `target` (which may carry a `#section`
/// anchor) against the active buffer's `text` and open it, scrolling to the
/// anchor when a section is present.
///
/// An anchor is either a heading slug (`#Heading`, `wikilink-headings-blocks`)
/// or a block id (`#^blockid`, `wikilink-block-anchors`); `anchor_byte` picks
/// the right finder. Three cases:
/// - A bare `#Section` / `#^block` (empty page) is a same-document anchor: stay
///   in the current buffer and scroll to the anchor; no note open.
/// - `Page#Section` / `Page#^block` opens the page (or creates it when
///   unresolved) and scrolls to the anchor once the new buffer's height map is
///   built.
/// - `Page` (no section) is the existing page-level open.
fn open_target(app: &mut AppState, text: &str, target: &str, sticky: bool) {
    let (page, section) = wikilink::split_target_section(target);

    // Same-document anchor: `[[#Section]]` / `[[#^block]]` / `[text](#Section)`.
    if page.is_empty() {
        if let Some(section) = section
            && let Some(active) = active_buffer_path(app)
            && let Some(byte) = anchor_byte(text, section)
        {
            scroll_buffer_to_byte(app, &active, byte);
        }
        return;
    }

    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    let policy = app
        .vault_session
        .config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(AmbiguityPolicy::Unresolved);
    let referrer = active_buffer_path(app);
    match wikilink::resolve_path(&paths, page, policy, referrer.as_deref()) {
        Resolution::Resolved(path) => {
            crate::editor_pane::open_file(app, &path, sticky);
            if let Some(section) = section {
                scroll_open_buffer_to_section(app, &path, section);
            }
        }
        Resolution::Ambiguous(_) => app.push_toast(
            format!(
                "Multiple notes named \u{201c}{page}\u{201d} \u{2014} pick one via the [[ menu",
            ),
            ToastLevel::Warn,
        ),
        Resolution::Unresolved => create_and_open(app, page, sticky),
    }
}

/// The active tab's buffer path, when it has one.
fn active_buffer_path(app: &AppState) -> Option<String> {
    app.session
        .active_tab
        .and_then(|id| app.tab_by_id(id).and_then(|t| t.buffer_path().map(str::to_string)))
}

/// Place the caret at `byte` in the buffer at `path` and request a
/// scroll-into-view, so the heading lands near the top of the viewport on the
/// next paint (the widget consumes `scroll_caret_into_view` after its measure
/// pass once the height map reflects the doc). status: wikilink-headings-blocks
fn scroll_buffer_to_byte(app: &mut AppState, path: &str, byte: usize) {
    if let Some(buffer) = app.session.buffers.get_mut(path) {
        let clamped = byte.min(buffer.editor.doc.len_bytes());
        buffer.editor.selection = editor_core::selection::Selection::single(clamped);
        buffer.view.scroll_caret_into_view = true;
    }
}

/// After opening `path`, find the anchor matching `section` in its (possibly
/// just-loaded) buffer text and scroll to it. A `section` that matches no
/// anchor is a graceful no-op — the note simply opens at the top.
/// status: wikilink-headings-blocks
/// status: wikilink-block-anchors
fn scroll_open_buffer_to_section(app: &mut AppState, path: &str, section: &str) {
    let Some(text) = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    if let Some(byte) = anchor_byte(&text, section) {
        scroll_buffer_to_byte(app, path, byte);
    }
}

/// Byte offset of the anchor `section` names in `text`: a block (`^blockid`)
/// when the anchor is `^`-prefixed and well-formed, otherwise a heading by
/// slug. One funnel so the same-document and post-open scroll paths agree on
/// how a `#section` is interpreted. `None` when nothing matches (graceful
/// no-op for the caller). status: wikilink-block-anchors
fn anchor_byte(text: &str, section: &str) -> Option<usize> {
    match wikilink::block_anchor_id(section) {
        Some(blockid) => wikilink::find_block_byte(text, blockid),
        None => wikilink::find_heading_byte(text, section),
    }
}

/// Create a new note for an unresolved wikilink name and open it. Under
/// path-based identity (`wikilink-path-form`) the link the user typed
/// (a name) resolves to the new note's path on the next decoration rebuild;
/// no save-time rewrite. status: wikilink-unresolved
///
/// Routes through the indexer-driven `core::ops::file::create_at` (watcher
/// suppression + `IndexJob::Upsert`) rather than the bare `vault::create_note`
/// so the new note is indexed without a duplicate watcher-driven ingest — the
/// same discipline as the `+` new-item button.
fn create_and_open(app: &mut AppState, name: &str, sticky: bool) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return;
    }
    let rel = if trimmed.ends_with(".md") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.md")
    };
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let rel_owned = rel.clone();
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::ops::file::create_at(&watcher, &jobs, &vault, &rel_owned, "").await
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
    };
    match result {
        Ok(actual) => {
            app.file_tree_state.invalidate_all();
            app.push_toast(format!("Created {actual}"), ToastLevel::Info);
            crate::editor_pane::open_file(app, &actual, sticky);
        }
        Err(e) => app.push_toast(format!("Couldn't create {rel}: {e}"), ToastLevel::Error),
    }
}

// ---------------------------------------------------------------------------
// Block-anchor auto-injection — [wikilink-block-anchor-autoinject]
// ---------------------------------------------------------------------------

/// A block-anchor reference parsed out of the active buffer: the page part
/// (empty = same-document) and the block id (sans `^`).
struct BlockRef {
    page: String,
    id: String,
}

/// After an edit authored a `[[Page#^id]]` / `[text](Page#^id)` link to a
/// not-yet-anchored block, inject ` ^id` onto the matching block in the target
/// note so the link resolves. The id is content-addressed
/// (`wikilink::fresh_block_id`), so the target block is re-located from the id
/// alone: the matching un-anchored block is the one whose freshly-derived id
/// equals the link's. An id that already marks a block — or matches no block —
/// is left untouched (the picker reuses existing ids; a hand-typed id the user
/// hasn't placed yet is none of our business). status: wikilink-block-anchor-autoinject
pub(crate) fn reconcile_block_anchors(app: &mut AppState, path: &str) {
    let Some(text) = app
        .session
        .buffers
        .get(path)
        .map(crate::buffer::Buffer::current_text)
    else {
        return;
    };
    // Cheap gate: most edits carry no block anchor at all.
    if !text.contains("#^") {
        return;
    }
    for block_ref in collect_block_refs(&text) {
        inject_for_ref(app, path, &block_ref);
    }
}

/// Every `#^id` block-anchor reference in `text`, from both `[[…]]` wikilinks
/// and `[label](dest)` markdown links.
fn collect_block_refs(text: &str) -> Vec<BlockRef> {
    let mut out = Vec::new();
    for link in wikilink::parse_links(text) {
        push_block_ref(&mut out, &link.target);
    }
    for dest in markdown_link_dests(text) {
        push_block_ref(&mut out, &dest);
    }
    out
}

/// If `target` carries a `#^id` block anchor, push its `(page, id)` onto `out`.
fn push_block_ref(out: &mut Vec<BlockRef>, target: &str) {
    let (page, section) = wikilink::split_target_section(target);
    if let Some(section) = section
        && let Some(id) = wikilink::block_anchor_id(section)
    {
        out.push(BlockRef { page: page.to_string(), id: id.to_string() });
    }
}

/// Inject the marker for one block reference: resolve its target note, find the
/// un-anchored block whose content-addressed id matches, and append ` ^id`
/// there. Same-document / open-target buffers are edited in place; a target
/// only on disk is rewritten through the core injection op.
fn inject_for_ref(app: &mut AppState, current: &str, block_ref: &BlockRef) {
    let target_path = if block_ref.page.is_empty() {
        current.to_string()
    } else {
        match resolve_page(app, current, &block_ref.page) {
            Some(p) => p,
            None => return,
        }
    };
    // The body to scan: the live buffer text if the target is open, else disk.
    let open_body = app
        .session
        .buffers
        .get(&target_path)
        .map(crate::buffer::Buffer::current_text);
    let body = match open_body
        .clone()
        .or_else(|| app.vault_session.vault.read_file(&target_path).ok())
    {
        Some(b) => b,
        None => return,
    };
    // Already anchored (the picker reused an existing id) → nothing to inject.
    if wikilink::find_block_byte(&body, &block_ref.id).is_some() {
        return;
    }
    let Some(range) = matching_block_range(&body, &block_ref.id) else {
        return;
    };
    if open_body.is_some() {
        inject_into_open_buffer(app, &target_path, &range, &block_ref.id);
    } else {
        inject_into_disk_note(app, &target_path, &range, &block_ref.id);
    }
}

/// Byte range of the un-anchored block in `body` whose freshly-derived id
/// equals `id`, or `None` when no such block exists.
fn matching_block_range(body: &str, id: &str) -> Option<std::ops::Range<usize>> {
    wikilink::scan_blocks(body).into_iter().find_map(|b| {
        if b.existing_id.is_none() && wikilink::fresh_block_id(body, &b.range) == id {
            Some(b.range)
        } else {
            None
        }
    })
}

/// Resolve a non-empty page part to a concrete vault path (lex-first on
/// ambiguity, matching the picker's resolution), or `None`.
fn resolve_page(app: &AppState, current: &str, page: &str) -> Option<String> {
    let paths = app.vault_session.vault.walk_indexable_files("").unwrap_or_default();
    match wikilink::resolve_path(&paths, page, AmbiguityPolicy::LexFirst, Some(current)) {
        Resolution::Resolved(p) => Some(p),
        Resolution::Unresolved | Resolution::Ambiguous(_) => None,
    }
}

/// Inject ` ^id` into an open buffer's editor text (same-document or an
/// already-loaded target) via an `Input` transaction, so the marker rides the
/// buffer's own undo / layered-doc path like any user edit (the next frame's
/// `editor_binding::run` mirrors it onto `working`) and the user's caret maps
/// through the insert rather than being clobbered.
fn inject_into_open_buffer(
    app: &mut AppState,
    target_path: &str,
    range: &std::ops::Range<usize>,
    id: &str,
) {
    let Some(buffer) = app.session.buffers.get_mut(target_path) else {
        return;
    };
    let body = buffer.editor.doc.to_string();
    // The marker is inserted after the block line's trailing-trimmed end.
    let line = match body.get(range.clone()) {
        Some(l) => l,
        None => return,
    };
    let insert_at = range.start + line.trim_end().len();
    let changes = editor_core::change::Set::of(
        buffer.editor.doc.len_bytes(),
        std::iter::once((insert_at..insert_at, format!(" ^{id}"))),
    );
    let tx = editor_core::transaction::Transaction::new(changes)
        .with_edit_type(editor_core::transaction::EditType::Input);
    buffer.editor = buffer.editor.apply(tx);
}

/// Inject ` ^id` into a target note that is only on disk, via the core op that
/// suppresses the watcher and reindexes — the same cross-note write path the
/// rename-rewrite pass uses.
fn inject_into_disk_note(
    app: &AppState,
    target_path: &str,
    range: &std::ops::Range<usize>,
    id: &str,
) {
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let vault = app.vault_session.vault.clone();
    let target = target_path.to_string();
    let range = range.clone();
    let id = id.to_string();
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let result = handle.block_on(async {
            hiker_core::ops::file::inject_block_marker(
                &watcher, &jobs, &vault, &target, &range, &id,
            )
            .await
        });
        if let Err(e) = result {
            tracing::warn!(error = %e, path = %target,
                "block-anchor auto-inject: write failed");
        }
    }
}

/// Every `[label](dest)` markdown-link destination in `text` (one-line links),
/// used to find block anchors authored in the markdown-link form.
fn markdown_link_dests(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && (i == 0 || bytes[i - 1] != b'[') {
            if let Some(dest) = markdown_link_dest_at(text, i) {
                out.push(dest);
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Hover preview — [wikilink-hover-preview]
// ---------------------------------------------------------------------------

/// Per-frame entry point from the buffer panel. If the pointer is over a
/// wikilink pill that resolves to a concrete note, register a hover on the
/// shared note-preview mechanism (`preview-note-hover`) — the same one the
/// file-tree / canvas sidebars use, which renders a live markdown preview
/// (with diagrams) of the target. The actual draw happens at the frame-loop
/// level in `widgets::preview::render_note_preview`, gated by the
/// `[ui].hover_previews_enabled` toggle, so there's no per-buffer lifecycle
/// state to keep here.
///
/// `editor_rect` is the screen-space rect of the editor body — click zones
/// are widget-local, so we translate them into screen coords here. `ui` is
/// borrowed read-only (the registration only reads `ui.ctx()` time/pointer).
pub(crate) fn track_hover(
    app: &AppState,
    ui: &egui::Ui,
    path: &str,
    editor_rect: egui::Rect,
    click_zones: &[ClickZone],
) {
    let pointer = ui.ctx().pointer_latest_pos();

    // Which wikilink pill (if any) is the pointer over? Translate the
    // widget-local zone rect to screen coords so the preview anchors on it.
    let pill_under_pointer: Option<(usize, egui::Rect)> = pointer.and_then(|p| {
        if !editor_rect.contains(p) {
            return None;
        }
        let lx = p.x - editor_rect.min.x;
        let ly = p.y - editor_rect.min.y;
        click_zones.iter().find_map(|z| {
            let ClickAction::WidgetClick(id) = z.action else {
                return None;
            };
            if id & editor_md::links::WIKILINK_WIDGET_TAG == 0 {
                return None;
            }
            if !z.rect.contains(lx, ly) {
                return None;
            }
            let offset = (id & !editor_md::links::WIKILINK_WIDGET_TAG) as usize;
            let screen = egui::Rect::from_min_max(
                egui::pos2(editor_rect.min.x + z.rect.x_min, editor_rect.min.y + z.rect.y_min),
                egui::pos2(editor_rect.min.x + z.rect.x_max, editor_rect.min.y + z.rect.y_max),
            );
            Some((offset, screen))
        })
    });

    let Some((offset, anchor)) = pill_under_pointer else { return };

    // A spec/code pill previews a 1-hop GRAPH of the link target's neighbourhood
    // (`spec-link-preview`) rather than a note body — a different mechanism, registered on
    // its own egui-memory slot and drawn by `render_link_graph_preview`. Spec links always
    // register (adapter-free); a code link registers only when a code-graph view for the
    // repo is already warm — otherwise nothing registers and the pill keeps its plain label.
    // status: spec-link-preview
    if let Some(kind) = link_graph_kind(app, path, offset) {
        crate::panels::link_graph_preview::register_link_graph_hover(ui, anchor, kind);
        return;
    }

    // A plain note link previews the target note's body. Only resolved links preview;
    // unresolved / ambiguous show nothing.
    if let Some(target) = resolve_target_path(app, path, offset) {
        // Interactive registration: the wikilink preview anchors at the pill,
        // scrolls under the wheel, and survives the cursor sliding onto it
        // (unlike the passive, side-anchored sidebar previews).
        crate::widgets::preview::register_note_hover_interactive(ui, anchor, &target);
    }
}

/// Classify the pill whose `[[…]]` span starts at `offset` in `path`'s buffer as a spec/code
/// link graph preview target, when it is one. A `[[spec:slug]]` always yields a preview
/// (adapter-free); a `[[code:repo/sym]]` yields one ONLY when a code-graph view bound to that
/// repo is already open (its adapter is warm — no SCIP bind per hover, per the spec's
/// fall-back-to-plain-label rule). Returns `None` for a plain note link. status: spec-link-preview
fn link_graph_kind(
    app: &AppState,
    path: &str,
    offset: usize,
) -> Option<crate::panels::link_graph_preview::LinkPreviewKind> {
    use crate::panels::link_graph_preview::LinkPreviewKind;
    let text = app.session.buffers.get(path).map(crate::buffer::Buffer::current_text)?;
    let link = wikilink::parse_links(&text).into_iter().find(|l| l.span.start == offset)?;
    if let Some(slug) = wikilink::parse_spec_target(&link.target) {
        return Some(LinkPreviewKind::Spec(slug.to_string()));
    }
    if let Some((repo_id, moniker)) = wikilink::parse_code_target(&link.target) {
        // Only register when a view for this repo is already open + bound — a not-yet-open
        // repo previews nothing (the plain label stands).
        let warm = app.panels.code_graph_docs.values().any(|v| v.src.0 == repo_id);
        if warm {
            return Some(LinkPreviewKind::Code {
                repo_id: repo_id.to_string(),
                moniker: moniker.to_string(),
            });
        }
    }
    None
}

/// Resolve the wikilink whose full `[[…]]` span starts at `offset` in the
/// active buffer to its concrete vault path, or `None` for unresolved /
/// ambiguous links (no preview).
fn resolve_target_path(app: &AppState, path: &str, offset: usize) -> Option<String> {
    let text = app.session.buffers.get(path).map(crate::buffer::Buffer::current_text)?;
    let link = wikilink::parse_links(&text)
        .into_iter()
        .find(|l| l.span.start == offset)?;
    // A spec link previews its defining note (resolved through the spec-anchor
    // index, same pick rule as the click path). status: wikilink-spec-links
    if let Some(slug) = wikilink::parse_spec_target(&link.target) {
        let anchor_paths = app
            .vault_session
            .services
            .read_store
            .lock()
            .ok()
            .and_then(|s| s.spec_anchor_paths(slug).ok())?;
        return pick_anchor_path(&anchor_paths, Some(path));
    }
    let paths = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .unwrap_or_default();
    let policy = app
        .vault_session
        .config
        .read()
        .map(|c| c.wikilinks.ambiguous_resolution.into())
        .unwrap_or(AmbiguityPolicy::Unresolved);
    match wikilink::resolve_path(&paths, &link.target, policy, Some(path)) {
        Resolution::Resolved(p) => Some(p),
        Resolution::Unresolved | Resolution::Ambiguous(_) => None,
    }
}

#[cfg(test)]
mod block_anchor_tests {
    use super::{collect_block_refs, matching_block_range, markdown_link_dests};

    #[test]
    fn collects_block_refs_from_both_link_forms() {
        let text = "see [[Page#^abc]] and [Doc](other#^xyz) and [[Plain]] and [[P#Head]]\n";
        let refs = collect_block_refs(text);
        let pairs: Vec<(&str, &str)> =
            refs.iter().map(|r| (r.page.as_str(), r.id.as_str())).collect();
        // Only the two `#^id` block anchors; the plain link and the heading
        // anchor are excluded.
        assert_eq!(pairs, vec![("Page", "abc"), ("other", "xyz")]);
    }

    #[test]
    fn collects_same_document_block_ref() {
        let refs = collect_block_refs("anchor [[#^local]] here\n");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].page, "");
        assert_eq!(refs[0].id, "local");
    }

    #[test]
    fn matching_block_range_finds_un_anchored_block_by_content_id() {
        let body = "intro\n\nThe target paragraph.\n\ntail\n";
        let blocks = hiker_core::wikilink::scan_blocks(body);
        let target = blocks.iter().find(|b| b.preview == "The target paragraph.").unwrap();
        let id = hiker_core::wikilink::fresh_block_id(body, &target.range);
        let found = matching_block_range(body, &id).expect("re-locates the block by id");
        assert_eq!(found, target.range);
        // An id matching no block's content yields nothing.
        assert!(matching_block_range(body, "nope99").is_none());
    }

    #[test]
    fn markdown_link_dests_extracts_destinations() {
        let dests = markdown_link_dests("a [x](one) b [[y]] c [z](two#^id)\n");
        assert_eq!(dests, vec!["one".to_string(), "two#^id".to_string()]);
    }
}
