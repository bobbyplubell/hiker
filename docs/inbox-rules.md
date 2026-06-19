# Inbox rules

Deterministic, user-authored rules that auto-place and auto-tag newly-created notes by matching their basename and/or initial body content. The non-AI sibling to the agentic Trees system (per `docs/trees.md` and `cluster-editor.md`): trees lets an agent propose placements; inbox rules lets the user write the placement themselves and have hiker apply it without any model in the loop.

The headline decisions:

- **Rules live in vault config.** `[inbox].rules` in `vault/.hiker/config.toml` per [[spec:settings-section-inbox]]. The rule list travels with the vault. No separate rules database. [inbox-rules]
status:: partial
implements:: [[code:hiker/config/sections/InboxConfig]], [[code:hiker/config/Config#inbox]], [[code:hiker/config/validate_cross_field]], [[code:hiker/indexer/jobs/JobCtx#inbox_cell]], [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/indexer/jobs/run_inbox_rules]], [[code:hiker/indexer/scheduler/IndexerLoop#inbox_cell]], [[code:hiker/indexer/scheduler/LoopState#inbox_cell]], [[code:hiker/indexer/IndexJob#Created]], [[code:hiker/indexer/ProgressEvent#InboxApplied]], [[code:hiker/indexer/Handle#inbox_cell]], [[code:hiker/indexer/impl#[Handle]attach_inbox_rules]], [[code:hiker/indexer/route_watcher_events]], [[code:hiker/bootstrap/impl#[ProgressLine]format]], [[code:hiker/bootstrap/open_vault]]
touches:: [[code:hiker/inbox]]
note:: first-match-wins rule list in `[inbox].rules` (per [[spec:settings-section-inbox]]); each rule matches basename regex and/or first-4KB body regex, action is `move_to` (folder) and/or `add_tag` (frontmatter tag). Engine in `core/src/inbox.rs` (`Rules::compile` / `apply_to_created`), fired from the **indexer's** Created handler (`core/src/indexer/jobs.rs::run_inbox_rules`) for indexable extensions; routes through [[spec:move-note-core-cmd]] + the frontmatter merge. Both actions are op-log-attributed `auto:inbox` (the [[spec:op-log-author-classes]] wire form): `add_tag` stages + auto-flips through `op_writes::stage_auto_content` (no review — see Execution), `move_to` records the logical rename when the note has a doc. Toast with Undo. Unaffected by [[spec:llm-features-disable-entirely]]. **Partial: no UI** — rules are config-only (raw `[inbox].rules` TOML); no settings editor / rule-management surface yet
- **First match wins.** Rules are evaluated top-down on a single create event; the first match's action applies and evaluation stops. No multi-rule composition, no chained rewrites.
- **Match by basename regex, body regex, or both.** When both are present they AND-combine. Body matching reads at most the first 4KB of the new file so the rule pass stays cheap on large dumps.
- **Actions are `move_to` and `add_tag`** (one or both per rule). `move_to` is a vault-relative folder path; the note is renamed into that folder via the existing [[spec:move-note-core-cmd]]. `add_tag` is a frontmatter tag merged via the existing `core::frontmatter::merge` path (creates the frontmatter block if absent). No "run this command" action — rules stay declarative and inspectable.
- **No-match disposition is "leave alone."** A note that matches no rule stays at its original create path. There is no implicit "send everything else to inbox/".
- **Triggers on the watcher's create event only.** Rules don't re-fire on edit, on rename, or on indexer reingest. A note's path/tags are set at creation; later edits stay the user's domain.


## Rule shape

```toml
[[inbox.rules]]
# Optional: basename regex matched against the file's basename (with extension)
match.basename = "^TODO-\\d{4}\\.md$"
# Optional: body regex matched against the first 4KB of file content
match.body = "^# Meeting"
# Action: at least one of move_to / add_tag
action.move_to = "inbox/todos/"
action.add_tag = "todo"
```

Strict-load rules (per [[spec:settings-strict-load]]):

- At least one of `match.basename` / `match.body` must be present.
- At least one of `action.move_to` / `action.add_tag` must be present.
- Regexes are validated at load time; invalid regex aborts startup naming the offending rule index.
- `move_to` must be vault-relative; absolute paths or `..` traversal abort.


## Execution

The rule engine lives in `core::inbox`, called from the watcher pipeline on the `Created` event for `.md` / `.txt` files ([[spec:watcher-event-normalized]]). For a created path it reads up to 4 KB of body, walks the rules top-down, and applies the first match's action (`move_to` first, then `add_tag` against the new path):

- `move_to` routes through `core::vault::move_note` so watcher suppression + index update are correct and the buffer-follows-rename rule holds. When the moved note already has an op-log doc, the logical rename is recorded authored `auto:inbox` (the indexer move path's `record_oplog_rename` posture) so history follows the move; a just-created file usually has no doc yet, and the rename helper no-ops.
- `add_tag` computes the frontmatter merge and stages it through the op-log staged-content seam authored `auto:inbox` (`op_writes::stage_auto_content`), flipped immediately — inbox rules are user-configured pre-index placement and apply **without review**, the behavior this spec has always defined; the attribution is what changed (the write used to be a bare `vault.write_file`). Staging seeds the just-created note's op-log doc from its disk bytes when none exists, so the tag frame lands on a real doc. Without an op-log handle (CLI / bare tests) it degrades to the suppressed direct write.
- Both actions happen on the indexer task — same writer discipline as every other vault mutation; no two-writer race.

A toast confirms the action with an Undo button — same pattern as [[spec:vault-trash-restore]]'s post-delete toast — so an over-eager rule can be reverted in one click without hunting the file down.


## Interaction with other systems

- **Relation to vault rules (`rules.md`).** Inbox rules are *pre-index* — they fire on the
  watcher's create event, before the note has meaningful frontmatter; vault rules are *post-index*,
  firing on derived-state change with conditions over the indexes ([[spec:rule-inbox-relation]]).
  No behavior change here; whether inbox rules become a preset of the general system is a post-v1
  question.
- **Ordering with trees.** A note created in a vault with both systems active is offered to inbox rules first (cheap, sync); only if no rule matches does the trees system get a shot. The trees path produces a *proposal* the user reviews, while inbox rules apply directly — different trust levels, different UX.
- **Settings pane.** A small editor for the rule list lands when the settings UI's array-of-tables row control grows up; until then the TOML is the surface (consistent with the rest of v1 settings posture). The settings pane shows the rule count in the `[inbox]` section header.
- **Disable AI master switch ([[spec:llm-features-disable-entirely]])** does not affect inbox rules — they're deterministic, not AI-driven. Users who turn AI off still get rule-based auto-org.


## Out of scope

- **Chained rules / multi-action rules.** First-match-wins is the model; a note can't ride two rules in sequence. If real workflows need it, revisit.
- **Re-firing rules on edit / rename.** Once the create event applies a rule, that's it; later edits don't re-evaluate.
- **Cross-vault rule sharing.** Rules are per-vault, full stop. Copy the TOML if you want them in another vault.
- **Capture-group rewriting in `move_to`** (e.g. `move_to = "inbox/$1/"` using a regex capture). Useful but adds non-trivial surface; v1 keeps `move_to` literal. Revisit when there's a concrete workflow.
- **Body-content size beyond 4KB.** The cap exists to keep rule passes cheap; if a workflow needs deeper matching, it likely wants a tag the user types into the file, not a rule.
