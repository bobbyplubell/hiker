# ZIM viewer

In-app viewer for offline `.zim` archives (Wikipedia exports, wikis, doc sites): opens an archive as an editor tab, renders its articles, and walks in-archive links like a browser. Read-only; `core` is uninvolved — the archive is parsed by `zxr` and rendered by `hiker-htmlview`.

The headline decisions:

- **Articles render through the no-JS `hiker-htmlview` renderer**, host-driven (the tab owns scroll / clip / input; the renderer lays out, paints, and hit-tests links). In-archive CSS / images are served offline by a ZIM-backed `ResourceProvider`. No browser engine, no JS.
- **An archive opens as a preview tab**, exactly like opening a note: it reuses the shared preview slot and is promotable to pinned by double-clicking the tab.
- **In-archive links follow note preview semantics** — a preview tab navigates in place (the link replaces it); a pinned tab opens the target as a new preview tab.
- **In-archive navigation rides the global Back/Forward stack** as `NavTarget::ZimArticle`, so the top-bar buttons walk article history browser-style, interleaved with note / snapshot history.
- **The "Jump to" title picker and the vault-search ZIM results run off the UI thread**, so typing stays smooth on a multi-million-title archive.

## Rendering [zim-view]
status:: done
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]move_into_folder]], [[code:hiker/bootstrap/impl#[AppState]restore_tab_state]]
touches:: [[code:hiker/panels/zim]]
note:: `app/src/panels/zim.rs` (`TabKind::ZimView`, `show`, `SubresourceProvider`, `parse_zim_url` + `normalize_dot_segments`). Offline `.zim` viewer; renders articles via `hiker-htmlview` (the `htmlview-render` renderer), subresources served offline against the `zim://` base, no JS. Archive read by `zxr`; `core` uninvolved

`TabKind::ZimView { zim_path, article }` — one tab per opened archive; `article = None` is the archive's main page. The tab body lays out and paints the article HTML through `hiker-htmlview` (the renderer behind `htmlview-render`) into the tab's painter; the host owns the `ScrollArea`, the clip rect, and pointer hit-testing (`link_at`).

Subresources (CSS, images an article references) are served offline by a ZIM-backed `ResourceProvider` resolved against the archive's `zim://` base. Relative refs arrive un-normalized from the renderer, so the provider collapses `.` / `..` segments before splitting off the ZIM namespace (without it MediaWiki articles render unstyled). Lookups try the parsed namespace, then the common content / image / style namespaces (`C` / `I` / `-` / `A` / `M`), so legacy and modern archive layouts both resolve.

Panes are `!Send` (they hold the renderer's stylo-styled document plus egui textures), so they live in a UI-thread-local store keyed by tab id and are dropped on tab close.

## Tabs and navigation

**Preview / pin** [zim-view-preview-tab]. Opening an archive (file tree, federated-search hit) lands it in the shared `session.preview_tab` slot (`sticky: false`) — the same slot notes use, so a fresh open replaces the standing preview rather than piling up tabs. Double-clicking the tab (or "Keep open") promotes it to pinned through the shared promotion path. Session restore reopens the archive on its main page; the current article is not persisted.
status:: done
touches:: [[code:hiker/panels/zim]]
note:: `app/src/panels/zim.rs::open`/`open_preview`. Opening an archive reuses the shared `session.preview_tab` slot (`sticky:false`) like a note; double-click promotes to pinned via the shared `on_preview_promoted` path

**Link / picker open** [zim-link-preview-open]. Clicking an in-archive link, or picking from the "Jump to" picker, resolves to a content article and:
status:: done
touches:: [[code:hiker/panels/zim]]
note:: `app/src/panels/zim.rs::navigate_within`. In-archive link / "Jump to" pick: preview tab navigates in place (link replaces it), pinned tab opens the target as a new preview tab

- **preview tab** → navigates in place (the link replaces the current article);
- **pinned tab** → opens the target as a new preview tab, leaving the pinned article undisturbed.

**Back / Forward** [zim-nav-stack]. Each article visit records a `NavTarget::ZimArticle { zim_path, article }` on the one global nav stack shared with notes and snapshots, so the top-bar Back/Forward buttons (and swipe-nav) walk article history. A back/forward landing navigates an existing tab for the archive in place, preferring the active tab. The original "no per-tab history" tradeoff was partially revisited 2026-06-12 (`bug-esc-no-back-in-focus-nav`, `interaction.md` [[spec:keyboard-esc-ladder]]): a surface that navigates in place must carry its own back affordance + the Esc rung, so each pane now ALSO keeps a local visit history (`zim.rs::History`) behind in-tab ⟵/⟶ toolbar buttons and Esc-as-back (a non-empty title picker consumes Esc first). The two layers compose: the global stack interleaves ZIM steps with notes/snapshots; the pane history walks only this pane's articles, and its landings don't re-push the global stack (a restore isn't a new visit).
status:: done
note:: `NavTarget::ZimArticle` (`state.rs`) + `editor_pane::navigate_to` + `zim.rs::push_nav`/`restore_nav` for the global stack; `zim.rs::History` (unit-tested) + the in-tab toolbar + Esc-as-back for the per-pane layer (recorded at the article-load seam so link clicks, picker picks, search jumps, and global restores all count)

## Title picker [title-picker-async]
status:: done
touches:: [[code:hiker/panels/zim]]
note:: `app/src/panels/zim.rs::title_picker` (`TitlePicker`). "Jump to" title-prefix picker debounced ~150ms + `spawn_blocking` `title_search` on an `Arc<Zim>`, epoch-keyed so stale hits drop — smooth typing on large archives

The in-tab "Jump to:" field title-prefix-searches the archive (`zxr` binary search over the title index, bounded). The search is debounced (~150ms) and runs on a background `spawn_blocking` task against an `Arc<Zim>` clone; results return over a channel tagged with a fire epoch, so hits from superseded typing are dropped. The lookup touches the archive memmap (which can fault to disk on a large `.zim`), so keeping it off the UI thread is what keeps typing smooth.

## Federated vault search [zim-federated-search]
status:: done
touches:: [[code:hiker/panels/zim]]
note:: `app/src/panels/zim.rs::federated_search`/`federated_fulltext_search` over a global `Mutex<ZimRegistry>`. Vault search folds in ZIM title-prefix + Xapian BM25 hits; runs on the search bg task ([[spec:search-query-embed-spawn-blocking]])

The vault search sidebar folds ZIM hits in beside note results, in two groups: title-prefix (instant binary search, bounded per archive) and full-text body (BM25 over each archive's embedded Xapian index). Both run on the search feature's background query task ([[spec:search-query-embed-spawn-blocking]]) against a process-global, `Mutex`-guarded registry of the vault's opened archives (kept warm across queries). Archives without an embedded index contribute no full-text hits.

## Out of scope

- **Editing / writing** — the viewer is read-only; `.zim` archives are immutable artifacts.
- **JavaScript / dynamic articles** — only static HTML/CSS renders, per `extract-web-no-js-stance`.
- **Ingesting `.zim` articles into the vault** as markdown sidecars for search / related coverage — a separate, unbuilt source-type idea ([[spec:zim-source-ingest]] in `ideas.md`), distinct from this viewer.
