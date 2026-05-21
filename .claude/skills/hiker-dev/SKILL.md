---
name: hiker-dev
description: Use when the user asks for implementation work on the hiker repo — landing one or more named slugs from `status.md` (features) or `bug_tracking.md` (bugs). Reads the relevant specs under `docs/` (the ones owning the slug, plus anything they cross-reference) so the implementation respects cross-doc constraints, then implements only what's asked, and ends with a concise report of what was done. Pairs with `hiker-pm` (chooses what to do next) and `hiker-spec-writer` (drafts new specs).
---

# hiker dev

Lands implementation work for slugs the user has already chosen. Doesn't decide *what* to build (that's `hiker-pm`) and doesn't draft specs (that's `hiker-spec-writer`).

## Before writing any code

The docs collection has grown too large to read end-to-end. Read what's relevant; don't skim everything.

1. **Always read in full:**
   - `docs/index.md` and `docs/design.md` — the foundational rules (module discipline, layer split, store conventions). Every implementation has to respect these.
   - `status.md` — the registry. Confirm the slugs the user named exist there, note their current status (`planned`/`partial`/`done`), and check neighboring rows for stated dependencies.
   - `bug_tracking.md` — catches active bugs that overlap with the area you're about to touch.
2. **Read the spec doc(s) that own the named slugs in full.** `status.md` rows are grouped under `## <Topic> (<file>.md)` headings — that names the owning doc. Don't skim; the spec is the contract for the slug.
3. **Follow cross-references.** If the owning spec cites another slug as a prerequisite (e.g. `requires watcher-suppress-self-writes`) or references another doc, open that doc and read the relevant section. Repeat until the citation graph is exhausted for the work at hand.
4. **Read the code at every cited file:line in the relevant rows.** Status evidence drifts; verify the claim before building on top of it. If you find drift (status says `done` but code disagrees, or vice versa), surface it in the report — don't silently fix it.
5. **Look at recent git history** for the touched files (`git log --oneline -20 -- <path>`). Tells you what's actively in flight and whether the user is mid-edit.

Specs not on the citation graph for this slice can stay unread. If something later in implementation makes you suspect a constraint lives in an unread doc (e.g. you're about to emit a frontend event and haven't read `observability.md`), open it then.

## What the user has asked for

The user names one or more slugs. Implement *exactly those*. Don't:

- Add adjacent features that "feel related" — they have their own slugs and the user will ask for them when they want them.
- Refactor surrounding code unless the slug genuinely requires it. If a refactor is needed, surface it in the report and let the user approve before doing it.
- Drift into "while I was here, I noticed..." cleanups. Note them in the report instead.

If a slug is ambiguous, ask before coding. One clarifying question saves a wrong implementation.

## How to implement

- **Match the project's existing patterns.** Module discipline (`rusqlite` only in `store.rs`, `fastembed` only in `embed.rs`, etc.) is load-bearing — read `docs/index.md` and `docs/design.md` for the rules.
- **Respect the layer split.** `design.md` is explicit: the app layer is UI state + editor + rendering, never does heavy work; command wrappers are 5–15 lines (parse args → call core → translate errors → return DTO); `core` owns vault model, indexer, search, watcher, trash, ops. Before writing code, decide which layer it belongs in. Defaults:
  - **Anything that mutates the vault, walks the tree, talks to the watcher, or coordinates the indexer goes in `core`** — usually `core::ops` for cross-component orchestration, or the relevant domain module (`vault.rs`, `trash.rs`, `indexer.rs`, `store.rs`, `config.rs`) for single-concern work. CLI/MCP adapters will need the same primitive; if the logic only exists in the app layer, those adapters either re-implement it or skip it (silently breaking contracts like watcher suppression).
  - **Anything that's pure data-shaping policy** (list caps, dedupe rules, default values, name templates, sort orders that ride on stored data) goes in `core`, not the adapter. If it's a rule you'd want consistent across CLI / MCP / UI, it's not adapter-layer.
  - **Command wrappers are seams, not orchestrators.** If an `_inner` helper grows past ~15 lines, or starts doing pre/post sequences (suppress → send → await → re-suppress), or walks vault state to feed another core component, the logic belongs in `core`. Move it; the command becomes a wrapper.
  - **`core` never emits frontend events.** Adapter-layer concerns (the `hiker:trash-changed` emit, the `hiker:watcher-overflow` toast, anything UI-event-shaped) stay in the app layer. If `core::ops` needs to signal "the trash changed," it returns enough information for the caller to decide; it doesn't reach for an emitter.
  - **OS-process invocation (`open -R`, `xdg-open`, etc.) currently lives in the app layer** as a defensible adapter concern. It moves to `core` only when a second adapter (CLI / MCP) needs the same affordance — don't preemptively relocate it.

  Quick gut-check before writing: "could the CLI use this exact function tomorrow?" If yes, it's core. If no, ask why not — usually because it's been written in the wrong layer.
- **Respect cross-spec hooks.** Many slugs explicitly require another slug as prerequisite (e.g. `move-note-core-cmd` needs `watcher-suppress-self-writes`). The spec usually states this — confirm the prerequisite is `done` before relying on it. If not, stop and surface the dependency.
- **Tag the implementation site.** For a feature slug: `// status: <slug>` (Rust) or `// status: <slug>` (TS) at the most natural anchor (the public function, the event handler, the module entry). One tag per feature; don't sprinkle. **Do not tag bug fixes** — bug slugs are short-lived and tagging them clutters source. See the note at the top of `bug_tracking.md`.
- **Update `status.md` before code if the slug changes shape.** If the work splits, renames, or merges a slug, edit the registry first per the file's own rules.
- **After landing a feature**, update its `status.md` row: status (`done` or `partial` with the gap named), evidence column (file:line of the new anchor), notes (anything non-obvious about the implementation).
- **After landing a bug fix**, move its row in `bug_tracking.md` from `Open` to `Resolved` with a one-line summary of the fix.
- **Tests.** Match the surrounding code's culture. Core has unit tests; UI is mostly type-checked. If the spec calls out specific edge cases, add tests for them. Don't demand tests where the codebase doesn't have them.
- **Do not split files or functions to dodge the length budget.** `check-lengths.py` caps Rust files at 1500 lines; `clippy::too_many_lines` caps functions at 200. These are pressure to *redesign*, not to shard. Splits that the toolchain treats as illegitimate (caught by `scripts/check-splits.py` and the anti-split clippy lints in `check.sh`):
  - `fn foo_part_2`, `fn foo_helper`, `fn foo_inner`, `fn foo_impl`, `fn foo_a` — suffix-named extractions.
  - `foo_part2.rs`, `foo_extra.rs`, `foo_misc.rs`, `foo_util2.rs` — suffix-named files.
  - A new file in a multi-file module that is not exposed via `pub mod` or `pub use` in `mod.rs` and whose `pub` items are referenced only from siblings — pure shard.
  - `use super::*` (denied by `clippy::wildcard_imports`); or `use super::{a, b, c, d, e}` pulling 5+ names — file is a slice of its parent, not a module.
  - Tiny files (< 20 non-comment lines) in `src/` other than `lib.rs` / `main.rs` / `mod.rs` / `build.rs` / `error.rs` — inline them instead.
  - Module roots (`mod.rs` / `lib.rs`) without a `//!` doc of 15+ words explaining the module's purpose. If you cannot write that sentence, the module's existence is arbitrary.
  - `pub use` re-export farms (`clippy::pub_use`), `clippy::module_name_repetitions` (a module named `foo` exporting `FooThing`), `clippy::single_call_fn` (functions with one caller), `clippy::unnecessary_wraps`, `clippy::needless_pass_by_value` — these typically indicate an extraction that didn't earn its keep.
  Legitimate reasons to extract a helper or split a file: the helper is reused from a non-sibling location, the helper has independent test value, or it represents a distinct concept that can be named meaningfully on its own (not "FooHelper" / "FooInner" / "FooExtra"). If you cannot give the extracted unit a meaningful name without referring to its parent, do not extract it — ask whether the original function should be redesigned instead. The rules and their rationale live in `scripts/check-splits.py`'s module docstring.
- **Verify locally.** Run `./scripts/check.sh` from the repo root. **Exactly once.** See the "Verification" section below — it is non-negotiable.
- **Never run JS toolchain on the host.** No `npx`, no `npm`, no `node`, no `tsc`, no `vite` outside the docker container. The npm supply chain is treated as untrusted; every JS toolchain call goes through `docker compose run --rm ui <command>` (one-shot) or `docker compose up ui` (the long-running dev server). Host `npx` is doubly bad — it will silently fetch from the registry on a typo or missing local copy. If a Dockerfile or compose tweak is needed, do it; don't fall back to host npm. `scripts/check.sh` already routes the typecheck through docker — just use it.
- **UI verification.** If the change is UI-visible, say so in the report and tell the user to reload the dev server to see it. You cannot verify UI yourself.

## Verification

There is one verification command. Run it. Once.

```
./scripts/check.sh
```

It runs `cargo test -p hiker-core --lib`, `cargo check -p hiker-ui`, and the dockerized `tsc --noEmit` in sequence and exits non-zero on the first failure. The script prints an `==>` banner before each step and `==> all checks passed` at the end on success.

**Rules — these are absolute:**

- Run `./scripts/check.sh` exactly **once** per verification. Not twice. Not "once more to confirm."
- Do **not** invoke the underlying commands directly. No bare `tsc --noEmit`, no `cargo test`, no `docker compose run ...` for verification purposes.
- Do **not** get clever with the invocation. No `2>&1 | tail -50`, no `| head`, no piping into `grep`, no redirecting to a file, no wrapping in `timeout`, no `--quiet`, no extra flags. Run the script bare and read what it prints.
- The script's exit code is the only signal that matters. Exit 0 = passed, regardless of how quiet the output was. `tsc --noEmit` produces no output on success — that is **expected**, not suspicious. Do not re-run to "see if something happened."
- If it fails, fix the underlying issue and run the script once more. That second run is a new verification, not a re-check of the previous one.
- If the script itself is broken or missing, stop and tell the user. Do not fall back to running the individual commands.

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
- ./scripts/check.sh: <pass / fail at <step>>
- UI verification: <"reload dev server to see X" if applicable, else "n/a">
```

A 200–400 word report is the sweet spot. The diff is the source of truth for what changed; the report is for context the diff can't carry (why this approach, what was deferred, what drift was surfaced).

## Rules

- **One slug at a time mentally, even when the user names several.** Land each completely (code + status update + tests + verification) before starting the next. Stops half-done work from accumulating.
- **Stop and ask if a prerequisite isn't `done`.** Don't fake it by inlining the dependency.
- **Surface drift, don't silently fix it.** If you discover `status.md` lies, the report says so. Updating the registry without the user's eyes on it erodes trust in the file.
- **Don't pad the report.** "I read the docs and thought about it carefully" is not useful — the spec compliance section should cite specific edge cases, not vibes.
- **Don't ship work you can't verify.** If `./scripts/check.sh` fails, fix it before reporting done. If a UI change can't be verified without running the app, say so explicitly in the report rather than claiming it works.

## Common pitfalls

- **Skimming the specs you do read.** The selection-by-relevance rule above is not permission to skim — every doc you open should be read in full. The failure mode is "the implementation looks reasonable but ignores a constraint stated in the very spec that owns the slug."
- **Stopping at the owning spec.** Cross-references are not optional. If `move-note-core-cmd` cites `watcher-suppress-self-writes`, the watcher doc is now in scope. Don't pretend the citation isn't there.
- **Putting orchestration in a command wrapper because that's where the existing command lives.** Command wrappers are seams; if you're writing more than the 5–15 line wrapper shape (parse args → call core → translate errors → return DTO), the logic belongs in `core` (usually `core::ops`). This mistake previously compounded across five mutating ops before being cleaned up — don't re-introduce it.
- **Duplicating a backend list / constant on the frontend with a "keep in sync" comment.** It will drift. Surface it via a command at vault open instead.
- **Implementing more than asked.** Adjacent features have their own slugs; let the user invoke this skill again when they want them.
- **Tagging bug fixes in code.** Per `bug_tracking.md`, this clutters source. Git history is enough for short-lived references.
- **Forgetting to update `status.md` / `bug_tracking.md`.** The registry-first convention is load-bearing for `hiker-pm` and the user's mental model. Skipping it costs more than the 30 seconds it saves.
- **Reporting "done" when tests fail.** Trust collapses fast.
- **Running the verification commands by hand instead of `./scripts/check.sh`.** The script exists *because* the bare commands invite re-running ("did it actually run? `tsc --noEmit` printed nothing — let me try `2>&1 | tail`"). Use the script. Once.
- **Re-running `./scripts/check.sh` to "double-check."** A second run is only justified after a fix. Quiet success on `tsc --noEmit` is expected; the script's exit code is the truth.
- **Running `npm`/`npx`/`node`/`tsc`/`vite` on the host.** The npm supply chain is treated as untrusted; every JS toolchain call goes through `docker compose run --rm ui …` (one-shot) or `docker compose up ui` (dev server). Host invocation defeats the isolation and reintroduces the `npx` registry-fetch footgun.
