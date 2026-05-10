---
name: hiker-spec-writer
description: Use when writing or substantially editing a spec doc for the hiker project (anything in /home/bobby/projects/notes/docs/). Triggers on phrases like "spec out X", "draft a spec for X", "add to <doc>.md", or "write docs/<name>.md". Enforces the project's spec conventions: read all existing docs first, match the established voice, assign kebab-case slugs for each feature, and register them in status.md.
---

# hiker spec writer

The hiker project keeps its design decisions in `docs/` as opinionated, decisive spec documents. New specs must fit that house style and plug into the slug registry in `status.md`. This skill is the checklist for doing that correctly.

## Before writing anything

1. **List `docs/` and read every spec that already exists, in full.** No skimming — the specs are short and they cross-reference each other heavily (e.g. anything touching the filesystem references `watcher.md`'s `watcher-suppress-self-writes`; anything touching persisted format references `index.md`'s `store-version-fail-loud`). Missing those links is the #1 way new specs come out wrong.
2. **Read `status.md` end-to-end.** It is the registry of every feature slug. New slugs must not collide; existing slugs are the right reference for any feature that already has one. Note the "How to use this file" section at the top — that's authoritative for slug behavior.
3. **Decide if the doc is feature spec or process doc.** Feature specs (`editor.md`, `index.md`, `clustering.md`, `observability.md`, etc.) describe things that get implemented in code → they get slugs. Process docs (`release.md`, hypothetical `contributing.md`) describe how we *work* → they do *not* get slugs. Slugs exist for grep-ability between spec, registry, and code; process docs have no code anchor.

## Voice and structure

Match the existing docs. Concretely:

- **Open with a 1–2 sentence framing** of what the doc covers and the goal.
- **"The headline decisions:" block as a bulleted list.** 3–6 bullets, each a decisive statement of a design choice with an inline `[slug]` marker. This is the TL;DR.
- **Then sections drilling in.** H2 per major topic. Use H3 for sub-features inside a section. Each feature definition gets an inline `[slug]` marker either in the heading or at the end of the bullet/sentence that defines it.
- **Tradeoffs are stated, not hidden.** When a decision rejects an alternative, say what was rejected and why in one or two sentences.
- **End with "Deferred" and/or "Out of scope".** Deferred = considered, postponed, may revisit. Out of scope = explicitly not this doc's problem. Both sections frequently include slugs for things we *might* do later.

Tone: opinionated, precise, conversational where it helps, no corporate hedging. Read `clustering.md`, `observability.md`, and `txt-ingest.md` as examples — these are recent and embody the current style.

**Specs describe the present desired state, not the history of how we got there.** This is load-bearing — the user has been explicit that the docs are not a running change log. The git log, PRs, and squash-merge messages cover history; the spec is what we want the project *to be*.

Do **not**:
- Write multi-paragraph docstrings or filler.
- Add change-log notes inside the doc ("added 2026-05-06"). The git history is the change log.
- Use emojis.
- Write "we used to do X but now…" — the spec describes the present state.
- Include "Affected slugs in other docs" / "Migrations" / "Cross-doc edits" change-log sections. When a new spec changes how an existing feature works, edit the existing spec to describe the *new* behavior; don't leave the old description in place with a "superseded by" pointer.
- Write headlines or paragraphs framed as "X replaces / supersedes / rewrites Y." State what X *is*; if the reader cares about Y they'll see it's gone from the spec.
- Keep slugs whose only purpose is marking a supersession event (e.g. `feature-x-supersedes-feature-y`). The replacement feature has its own slugs; a supersession marker is pure history.
- Reference removed slugs by name in spec prose. If a slug isn't in `status.md` anymore, it shouldn't be cited as either active or "previously known as."
- Write narratives about implementation drift, prior bugs that motivated a decision, or "the gap let X happen." If the rule is load-bearing, just state the rule.

Future ideas are different from history. **Future / deferred items belong in the doc** — they're forward-looking ("we might do this later") and they get a slug + a planned row. The only time to remove a future idea is when it's been *explicitly rejected* (the user has said "we won't do that"); at that point delete the entry rather than turning it into negative-space commentary about what won't happen. Forward-looking "Out of scope" lists are also fine — they describe the spec's boundaries, not its history.

## Slugs

Slugs are kebab-case, positional-free identifiers that name the *feature*, not its location. They survive doc reorganization. Treat them like function names.

Rules:

- **kebab-case-with-dashes**, lowercase, ASCII only.
- **Name the feature, not the file or section.** `pre-write-drift-check` not `editor-section-3-2`.
- **Cluster by topic prefix when natural.** `cluster-*`, `obs-*`, `txt-*`, `watcher-*`, `tree-*`, `store-*`. Helps with grep and reads as a namespace.
- **Bug slugs prefixed `bug-`.** Tracked in `bug_tracking.md`, not `status.md`.
- **One slug per atomic feature.** If you find yourself writing "and" inside the slug, split it.
- **Reuse, don't rename.** If a feature already has a slug in `status.md`, use that slug. Renaming a slug means updating `status.md` first, then every doc and code reference.
- **Inline marker form is `[slug-name]`** with square brackets, placed at the end of the line/bullet/heading that defines the feature.

Examples of good slugs from the existing registry: `pre-write-drift-check`, `drag-and-drop-move`, `vault-trash-restore`, `cluster-stable-identity`, `obs-log-ring-buffer`, `txt-chunker-sentence-pack`.

Examples of bad slugs (don't write these): `editor-feature-1`, `the-thing-that-watches-files-and-emits-events`, `cluster_build_recursive` (underscores), `ClusterBuildRecursive` (camel).

## Updating `status.md`

For every new feature slug introduced in a spec:

1. Add a row in the appropriate section table (or create a new `## Section (newdoc.md)` heading if the doc is new).
2. Status starts as `planned` unless code already exists.
3. The "Notes" column gets a one-liner — what the feature is, in <120 chars. Cross-reference related slugs by name where it adds value.
4. If the spec changes a slug that's already in `status.md` (rename, split, merge), update `status.md` *first* so the registry stays canonical.

For specs that include slugs deferred to "we may do this later" (e.g. `obs-perf-flamegraph`), add them as `planned` rows too — being deferred isn't a different status, it's just one of the planned items.

## Where ideas live

Three homes, picked by maturity:

- **`docs/<feature>.md`** — concrete enough for a spec doc with slug taxonomy and registered status rows. Default destination once an idea is well-formed.
- **`docs/design.md`'s "Future / deferred" section** — for cross-doc future ideas that have a name and a sketch but aren't ready for their own spec doc yet. Captures the idea in canonical project docs without inflating it into premature commitment.
- **`scratch/<topic>.md`** — for context discussed in chat that *isn't yet ironed out enough* to land in `docs/`. Decisions are still evolving; contradictions and open questions are expected. Currently used for trails (until `trails.md` exists). Pool conversation context here, then promote to `design.md` (as deferred) or to its own `docs/<feature>.md` when the design firms up.

When in doubt, prefer `design.md` over `scratch/` — design.md is the canonical doc, scratch is a holding pen. Only park in `scratch/` if the idea is genuinely too unstable for `design.md`'s deferred section.

## When the user asks for a new spec

1. Read all of `docs/` in full, plus `status.md`.
2. Confirm which file the spec lives in. Usually a new spec → new file. If it's small or tightly coupled to an existing one, fold into that doc rather than creating a new one.
3. Draft the doc following the voice + structure above.
4. Assign slugs as you go; verify against `status.md` for collisions.
5. Update `status.md`: add a `## <Topic> (<file>.md)` section if new, plus a row per slug.
6. Report back to the user with: what file changed, what slugs were added, and any cross-doc links worth flagging.

## When editing an existing spec

1. Read the doc plus `status.md` plus any spec it references.
2. If the edit adds new features, treat each as a new slug and follow the registry rules.
3. If the edit reshapes an existing feature, decide: still the same slug, or genuinely a new feature? When in doubt, keep the slug and update its row in `status.md`.
4. Never silently drop a slug — if a feature is removed, mark its row in `status.md` removed (or delete the row and note in the squash-merge commit message).

## When a new spec changes existing behavior

When a new spec materially changes how an existing feature works (the cluster editor's per-node policies replacing global confidence tiers; the task queue absorbing fan-out + note-mutation routing):

1. **Rewrite the existing spec to describe the new behavior** in place. The old description goes; the new description takes its slot.
2. **Update `status.md` rows** for the affected slugs to describe the *current* feature, not the change. If a slug's described feature no longer exists in the spec, delete the row entirely — don't leave a "superseded" or "removed" tombstone (the git log carries that).
3. **Drop inline slug markers** in the rewritten spec for any slug whose feature is now described elsewhere.
4. **Don't write change-log paragraphs** in the new spec ("X repoints Y", "Z collapses into W", "this section supersedes that section"). The new spec just says what the feature is; the old spec doesn't say it anymore. Anyone reading the diff to understand the change reads the git diff.

The exceptions are `status.md` and `bug_tracking.md` — both reflect *current code state vs. spec state*, so they legitimately track the gap and the work needed to close it. Specs themselves describe the desired state only.

## Common pitfalls

- **Writing the spec without reading `status.md`.** Leads to slug collisions, missed reuse, and contradictions with already-decided rules.
- **Slugs that encode location.** `editor-section-status-bar-3` will rot the moment `editor.md` reorganizes. Name the feature.
- **Inline slugs only in headings, missing on bullet-level features.** Every distinct feature gets an inline marker at the closest definition point, not just at the section heading.
- **Process docs gaining slugs.** `release.md` deliberately has none. If the doc describes how the team works rather than what the code does, no slugs.
- **Forgetting to register the slug.** A slug in a spec but not in `status.md` defeats the registry. Both must be updated in the same change.
