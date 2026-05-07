---
name: hiker-dev
description: Use when the user asks for implementation work on the hiker repo — landing one or more named slugs from `status.md` (features) or `bug_tracking.md` (bugs). Reads every spec under `docs/` end-to-end first so the implementation respects cross-doc constraints, then implements only what's asked, and ends with a concise report of what was done. Pairs with `hiker-pm` (chooses what to do next) and `hiker-spec-writer` (drafts new specs).
---

# hiker dev

Lands implementation work for slugs the user has already chosen. Doesn't decide *what* to build (that's `hiker-pm`) and doesn't draft specs (that's `hiker-spec-writer`).

## Before writing any code

1. **Read every spec in `docs/` in full.** Specs cross-reference each other; an implementation that ignores a constraint stated in another doc is the most common failure mode here. No skimming.
2. **Read `status.md` end-to-end.** Confirm the slugs the user named exist there, note their current status (`planned`/`partial`/`done`), and read any neighboring rows that look like dependencies.
3. **Read `bug_tracking.md`.** Catches active bugs that overlap with the area you're about to touch — the fix may already be specified.
4. **Read the code at every cited file:line in the relevant rows.** Status evidence drifts; verify the claim before building on top of it. If you find drift (status says `done` but code disagrees, or vice versa), surface it in the report — don't silently fix it.
5. **Look at recent git history** for the touched files (`git log --oneline -20 -- <path>`). Tells you what's actively in flight and whether the user is mid-edit.

## What the user has asked for

The user names one or more slugs. Implement *exactly those*. Don't:

- Add adjacent features that "feel related" — they have their own slugs and the user will ask for them when they want them.
- Refactor surrounding code unless the slug genuinely requires it. If a refactor is needed, surface it in the report and let the user approve before doing it.
- Drift into "while I was here, I noticed..." cleanups. Note them in the report instead.

If a slug is ambiguous, ask before coding. One clarifying question saves a wrong implementation.

## How to implement

- **Match the project's existing patterns.** Module discipline (`rusqlite` only in `store.rs`, `fastembed` only in `embed.rs`, etc.) is load-bearing — read `docs/index.md` and `docs/design.md` for the rules.
- **Respect the layer split.** `design.md` is explicit: frontend (TS) is UI state + editor + rendering, never touches the filesystem and never does heavy work; Tauri commands are 5–15 lines (parse args → call core → translate errors → return DTO); `core` owns vault model, indexer, search, watcher, trash, ops. Before writing code, decide which layer it belongs in. Defaults:
  - **Anything that mutates the vault, walks the tree, talks to the watcher, or coordinates the indexer goes in `core`** — usually `core::ops` for cross-component orchestration, or the relevant domain module (`vault.rs`, `trash.rs`, `indexer.rs`, `store.rs`, `config.rs`) for single-concern work. CLI/MCP adapters will need the same primitive; if the logic only exists in `ui/src-tauri/`, those adapters either re-implement it or skip it (silently breaking contracts like watcher suppression).
  - **Anything that's pure data-shaping policy** (list caps, dedupe rules, default values, name templates, sort orders that ride on stored data) goes in `core`, not the adapter. If it's a rule you'd want consistent across CLI / MCP / UI, it's not adapter-layer.
  - **Frontend `.ts` should not duplicate backend constants.** If the TS side needs a list the Rust side already owns (e.g. indexable extensions), surface it via a Tauri command at vault open and cache it; don't hand-maintain a mirror with a "keep in sync" comment. The mirror will drift the next time someone bumps the Rust side.
  - **Tauri commands are seams, not orchestrators.** If an `_inner` helper grows past ~15 lines, or starts doing pre/post sequences (suppress → send → await → re-suppress), or walks vault state to feed another core component, the logic belongs in `core`. Move it; the command becomes a wrapper.
  - **`core` has zero `tauri::` imports** and never emits frontend events. Adapter-layer concerns (the `hiker:trash-changed` emit, the `hiker:watcher-overflow` toast, anything UI-event-shaped) stay in `ui/src-tauri/`. If `core::ops` needs to signal "the trash changed," it returns enough information for the caller to decide; it doesn't reach for `Emitter`.
  - **OS-process invocation (`open -R`, `xdg-open`, etc.) currently lives in `ui/src-tauri/lib.rs`** as a defensible adapter concern. It moves to `core` only when a second adapter (CLI / MCP) needs the same affordance — don't preemptively relocate it.

  Quick gut-check before writing: "could the CLI use this exact function tomorrow?" If yes, it's core. If no, ask why not — usually because it's been written in the wrong layer.
- **Respect cross-spec hooks.** Many slugs explicitly require another slug as prerequisite (e.g. `move-note-core-cmd` needs `watcher-suppress-self-writes`). The spec usually states this — confirm the prerequisite is `done` before relying on it. If not, stop and surface the dependency.
- **Tag the implementation site.** For a feature slug: `// status: <slug>` (Rust) or `// status: <slug>` (TS) at the most natural anchor (the public function, the event handler, the module entry). One tag per feature; don't sprinkle. **Do not tag bug fixes** — bug slugs are short-lived and tagging them clutters source. See the note at the top of `bug_tracking.md`.
- **Update `status.md` before code if the slug changes shape.** If the work splits, renames, or merges a slug, edit the registry first per the file's own rules.
- **After landing a feature**, update its `status.md` row: status (`done` or `partial` with the gap named), evidence column (file:line of the new anchor), notes (anything non-obvious about the implementation).
- **After landing a bug fix**, move its row in `bug_tracking.md` from `Open` to `Resolved` with a one-line summary of the fix.
- **Tests.** Match the surrounding code's culture. Core has unit tests; UI is mostly type-checked. If the spec calls out specific edge cases, add tests for them. Don't demand tests where the codebase doesn't have them.
- **Verify locally.** Run `cargo test -p hiker-core --lib`, `cargo check -p hiker-ui`, and `npx tsc --noEmit` (in `ui/`) before reporting done. Run each command **once**. If it exits 0 with no output, it passed — don't re-run it under different working directories or with extra flags to "double-check." If the change is UI-visible, say in the report that the user still needs to reload the dev server to see it — you can't verify UI yourself.

## What the report looks like

Keep it short. The user has already read the specs; they don't need them re-explained.

```
## Slugs landed
- <slug>: <one-line what changed>
- <slug>: <one-line what changed>

## Files touched
- <path>: <one-line summary>
- <path>: <one-line summary>

## Spec compliance notes
- <anywhere the spec called out an edge case and how it was handled>
- <any prerequisite slug confirmed `done` before building on it>

## Drift surfaced
- <slug>: status.md / bug_tracking.md said <X> but code at <file:line> showed <Y>; left as-is for user to decide>

## Followups
- <anything noticed-but-not-touched: nearby cleanup, deferred edge case, possibly stale comment>

## Verification
- cargo test -p hiker-core --lib: <pass/fail/N tests>
- cargo check -p hiker-ui: <ok / errors>
- npx tsc --noEmit: <ok / errors>
- UI verification: <"reload dev server to see X" if applicable, else "n/a">
```

A 200–400 word report is the sweet spot. The diff is the source of truth for what changed; the report is for context the diff can't carry (why this approach, what was deferred, what drift was surfaced).

## Rules

- **One slug at a time mentally, even when the user names several.** Land each completely (code + status update + tests + verification) before starting the next. Stops half-done work from accumulating.
- **Stop and ask if a prerequisite isn't `done`.** Don't fake it by inlining the dependency.
- **Surface drift, don't silently fix it.** If you discover `status.md` lies, the report says so. Updating the registry without the user's eyes on it erodes trust in the file.
- **Don't pad the report.** "I read the docs and thought about it carefully" is not useful — the spec compliance section should cite specific edge cases, not vibes.
- **Don't ship work you can't verify.** If `cargo test` or `tsc --noEmit` fails, fix it before reporting done. If a UI change can't be verified without running the app, say so explicitly in the report rather than claiming it works.

## Common pitfalls

- **Skimming the specs.** This is the failure mode that produces "the implementation looks reasonable but ignores a constraint stated three docs over." Read everything.
- **Putting orchestration in `ui/src-tauri/lib.rs` because that's where the existing command lives.** Tauri commands are seams; if you're writing more than the 5–15 line wrapper shape (parse args → call core → translate errors → return DTO), the logic belongs in `core` (usually `core::ops`). This mistake previously compounded across five mutating ops before being cleaned up — don't re-introduce it.
- **Duplicating a backend list / constant on the frontend with a "keep in sync" comment.** It will drift. Surface it via a Tauri command at vault open instead.
- **Implementing more than asked.** Adjacent features have their own slugs; let the user invoke this skill again when they want them.
- **Tagging bug fixes in code.** Per `bug_tracking.md`, this clutters source. Git history is enough for short-lived references.
- **Forgetting to update `status.md` / `bug_tracking.md`.** The registry-first convention is load-bearing for `hiker-pm` and the user's mental model. Skipping it costs more than the 30 seconds it saves.
- **Reporting "done" when tests fail.** Trust collapses fast.
