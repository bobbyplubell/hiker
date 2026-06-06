# Inbox rules

Deterministic, user-authored rules that auto-place and auto-tag newly-created notes by matching their basename and/or initial body content. The non-AI sibling to the agentic Trees system (per `docs/trees.md` and `cluster-editor.md`): trees lets an agent propose placements; inbox rules lets the user write the placement themselves and have hiker apply it without any model in the loop.

The headline decisions:

- **Rules live in vault config.** `[inbox].rules` in `vault/.hiker/config.toml` per `settings-section-inbox`. The rule list travels with the vault. No separate rules database. [inbox-rules]
- **First match wins.** Rules are evaluated top-down on a single create event; the first match's action applies and evaluation stops. No multi-rule composition, no chained rewrites.
- **Match by basename regex, body regex, or both.** When both are present they AND-combine. Body matching reads at most the first 4KB of the new file so the rule pass stays cheap on large dumps.
- **Actions are `move_to` and `add_tag`** (one or both per rule). `move_to` is a vault-relative folder path; the note is renamed into that folder via the existing `move-note-core-cmd`. `add_tag` is a frontmatter tag merged via the existing `core::frontmatter::merge` path (creates the frontmatter block if absent). No "run this command" action — rules stay declarative and inspectable.
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

Strict-load rules (per `settings-strict-load`):

- At least one of `match.basename` / `match.body` must be present.
- At least one of `action.move_to` / `action.add_tag` must be present.
- Regexes are validated at load time; invalid regex aborts startup naming the offending rule index.
- `move_to` must be vault-relative; absolute paths or `..` traversal abort.


## Execution

The rule engine lives in `core::inbox`, called from the watcher pipeline on the `Created` event for `.md` / `.txt` files (`watcher-event-normalized`). For a created path it reads up to 4 KB of body, walks the rules top-down, and applies the first match's action (`move_to` first, then `add_tag` against the new path):

- `move_to` routes through `core::vault::move_note` so watcher suppression + index update are correct and the buffer-follows-rename rule holds.
- `add_tag` routes through the existing frontmatter-merge primitive (same one `mcp-tool-apply-tag` calls) so the change is recorded as a regular user save (`author = "user"`).
- Both actions happen on the indexer task — same writer discipline as every other vault mutation; no two-writer race.

A toast confirms the action with an Undo button — same pattern as `vault-trash-restore`'s post-delete toast — so an over-eager rule can be reverted in one click without hunting the file down.


## Interaction with other systems

- **Ordering with trees.** A note created in a vault with both systems active is offered to inbox rules first (cheap, sync); only if no rule matches does the trees system get a shot. The trees path produces a *proposal* the user reviews, while inbox rules apply directly — different trust levels, different UX.
- **Settings pane.** A small editor for the rule list lands when the settings UI's array-of-tables row control grows up; until then the TOML is the surface (consistent with the rest of v1 settings posture). The settings pane shows the rule count in the `[inbox]` section header.
- **Disable AI master switch (`llm-features-disable-entirely`)** does not affect inbox rules — they're deterministic, not AI-driven. Users who turn AI off still get rule-based auto-org.


## Out of scope

- **Chained rules / multi-action rules.** First-match-wins is the model; a note can't ride two rules in sequence. If real workflows need it, revisit.
- **Re-firing rules on edit / rename.** Once the create event applies a rule, that's it; later edits don't re-evaluate.
- **Cross-vault rule sharing.** Rules are per-vault, full stop. Copy the TOML if you want them in another vault.
- **Capture-group rewriting in `move_to`** (e.g. `move_to = "inbox/$1/"` using a regex capture). Useful but adds non-trivial surface; v1 keeps `move_to` literal. Revisit when there's a concrete workflow.
- **Body-content size beyond 4KB.** The cap exists to keep rule passes cheap; if a workflow needs deeper matching, it likely wants a tag the user types into the file, not a rule.
