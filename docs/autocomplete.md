# Autocomplete

One shared autocomplete substrate behind every "type a few characters, pick a ranked candidate" surface in hiker: the editor's in-buffer `[[wikilink]]` completion, the chat composer's `@`-mention, and the standalone vault pickers (canvas **Insert from vault**, and board add-card as it adopts it). Today these are three separate implementations with duplicated ranking; this doc specs the package they converge on.


## Current state

Three surfaces, three implementations — the duplication this doc retires:

| Surface | Today | Trigger | Lives in |
| ------- | ----- | ------- | -------- |
| `[[wikilink]]` | `CompletionSource` + private `score_basename` | `[[` in a markdown buffer | `editor-view::autocomplete`, `app::completion_sources::WikilinkSource` |
| Chat `@`-mention | bespoke trailing-token scan + hand-built popup | `@` in the chat composer | `app::chat::render` (`active_at_mention`, `mention_suggestions`) |
| Canvas insert / board add-card | none (canvas: not built; board: a nested menu) | a toolbar / context action | — |

`editor-view::autocomplete` already defines `CompletionItem`, `CompletionKind`, and `CompletionState` (the open/selected/anchor state machine) — those are kept; the wikilink path keeps `CompletionSource`. What changes is that the *ranking* and the *list UI* stop being per-surface.


## The shared core

[[spec:autocomplete-shared-core]] — a pure function (no egui, no buffer) from `(query, candidates)` to a ranked, filtered `Vec<CompletionItem>`:

- **Matching.** Case-insensitive; a candidate matches when the query is a subsequence of its label, with score boosts for prefix matches, contiguous runs, and word/`/`-segment boundaries (so `arch` ranks `notes/architecture.md` well, and a basename match beats a deep-path match). For vault paths the basename is weighted above the folder prefix — the behavior the wikilink source hand-codes today.
- **Determinism.** Equal scores break ties by label, so results don't reshuffle frame to frame.
- **Bounded.** Caller passes a result cap; the core returns the top-N. Enumeration cost (walking the vault) is the candidate source's concern, not the core's.

Placement: a small `autocomplete` module reachable by both `editor-view` consumers and `app` standalone surfaces. If it can be egui-free it lives beside `CompletionItem` in `editor-view::autocomplete`; the picker *widget* (egui) layers on top in `app`.


## Candidate sources

[[spec:autocomplete-candidate-source]] — `CandidateSource { fn candidates(&self, query: &str, limit: usize) -> Vec<CompletionItem> }`, the query→items half with no buffer coupling. The existing `CompletionSource` (in-buffer: `triggers()` + `matches(state, pos)`) is expressed in terms of a `CandidateSource` plus the buffer-specific trigger/replace logic, so the ranking is shared and only the seam differs.

Concrete sources:

- **`VaultSource`** ([[spec:autocomplete-vault-source]]) — vault notes + other indexed sources, enumerated from the vault/indexer and ranked by the shared core. The single definition of "linkable / insertable vault item," consumed by wikilink completion and the canvas/board pickers. A scope flag selects notes-only (wikilink) vs. notes + sources (canvas insert).
- **`MentionSource`** ([[spec:autocomplete-mention]]) — the chat `@`-mention candidates, migrated off the bespoke scan onto `VaultSource` + the shared core (the `@`-trigger token scan stays chat-specific; the ranking does not).


## Picker widget

[[spec:autocomplete-picker-widget]] — surfaces not inside a text buffer (the canvas insert picker; a future command-style picker) use one egui widget: a query field + a ranked, keyboard-navigable list (↑/↓, Enter, Esc), rendered as a popup/overlay, returning the chosen item. The in-buffer surfaces keep their inline anchored popup but share the same list rendering + key handling.


## Consumers

- **Wikilink** ([[spec:autocomplete-wikilink]]) — keeps `CompletionSource` + the `[[`/`]]` close-fixup, but ranks through the shared core via `VaultSource`. Behavior (shortest-unambiguous path form, double-close fixup) is unchanged; the private `score_basename` is removed.
- **Chat `@`-mention** ([[spec:autocomplete-mention]]) — the composer's trailing-token detection stays, but suggestions and the popup list come from the shared core + the shared picker list rendering.
- **Canvas Insert from vault** ([[spec:canvas-insert-from-vault]] in `canvas.md`) — the standalone picker widget over `VaultSource` (notes + sources); the chosen item becomes a file-node pointer.
- **Board add-card** — may adopt the same picker in place of its nested menu; not required by this doc, noted so the convergence is intentional.


## Out of scope

- **Command palette / action search.** A fuzzy *action* runner is a separate surface; it could reuse [[spec:autocomplete-shared-core]], but its candidate set (commands) and execution model aren't this doc's concern.
- **Network / remote candidates.** All sources here enumerate local vault/index state. Remote completion (e.g. URL suggestions) is not modeled.
- **Replacing `CompletionState`.** The in-buffer open/anchor/selected state machine is kept as-is; this doc shares ranking + list UI, not that state model.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **autocomplete-shared-core** — one ranking fn (exact > prefix > boundary-aware contiguous run > scattered subsequence), basename-weighted for paths, deterministic tie-break; `rank_tests` (8) pass; replaces wikilink `score_basename` + chat's scan [autocomplete-shared-core]
  status:: done
  note:: evidence: `editor/editor-view/src/autocomplete.rs` (`rank`, `RankCandidate`)
- **autocomplete-candidate-source** — `candidates(query, limit)` half factored out of `CompletionSource`; in-buffer trait keeps `triggers`/`matches` and routes ranking through the core [autocomplete-candidate-source]
  status:: done
  note:: evidence: `editor/editor-view/src/autocomplete.rs` (`CandidateSource`)
- **autocomplete-picker-widget** — reusable standalone egui picker (query field + keyboard-nav ranked list, Up/Down/Enter/Esc); consumed by [[spec:canvas-insert-from-vault]] [autocomplete-picker-widget]
  status:: done
  touches:: [[code:hiker/widgets/autocomplete_picker]]
  note:: evidence: `app/src/widgets/autocomplete_picker.rs` (`PickerState`, `show`, `PickerOutcome`)
- **autocomplete-vault-source** — enumerates vault notes + sources via `walk_indexable_files`, ranked by the core; `Scope::{NotesOnly, NotesAndSources}`; `ranked_with` seam for custom insert forms [autocomplete-vault-source]
  status:: done
  touches:: [[code:hiker/autocomplete/vault_source]]
  note:: evidence: `app/src/autocomplete/vault_source.rs` (`VaultSource`, `Scope`)
- **autocomplete-wikilink** — migrated onto `VaultSource` (NotesOnly) + core; `score_basename` removed; `[[`/`]]` fixup + shortest-unambiguous form + double-close behavior unchanged (tests pass) [autocomplete-wikilink]
  status:: done
  touches:: [[code:hiker/completion_sources]]
  note:: evidence: `app/src/completion_sources.rs` (`WikilinkSource`)
- **autocomplete-mention** — ranking swapped to `VaultSource` (NotesAndSources) + core; `@`-token scan (`AtMentionScan`) + composer popup unchanged; `at_mention_tests` pass [autocomplete-mention]
  status:: done
  touches:: [[code:hiker/chat/render]], [[code:hiker/chat/sidebar]]
  note:: evidence: `app/src/chat/render.rs` (`mention_suggestions`), `app/src/chat/sidebar.rs`
