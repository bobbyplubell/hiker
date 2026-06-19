# Self-hosting — how Hiker develops Hiker

Hiker is built using Hiker. The project's own planning lives in a Hiker vault, the
project's own code is read by Hiker's code-intelligence, and the work is carried out by
agents that talk to the vault over MCP. This doc describes that loop — the topology, the
realms, the cycle, and the features that make it clean — so the dogfooding setup is a
deliberate, reproducible system rather than an ad-hoc pile of checkouts.

This is a **process + architecture** doc, not a feature spec. The features it leans on each
have their own slugs in their owning specs (`rules.md`, `pm.md`, `kinds.md`, `queries.md`,
`watcher.md`, `git.md`, `op-log.md`, `projects.md`, `mcp.md`); this doc wires them into one
workflow and names the gaps that still need building.

## The headline decisions

- **Two realms, one vault.** The *code* (the hiker repo) and the *planning* (PM notes) both
  live in the vault, but they are governed by different mechanisms: code by git, planning by
  the op-log. They meet only at spec slugs. [self-host-two-realms]
- **CODE-IN-VAULT.** The hiker repo is checked out *inside* the vault (`<vault>/hiker/`,
  working tree + `.git`) so its specs are indexed as notes and `[[code:hiker/…]]` links
  resolve against real files. The non-note parts of the repo are kept out of the index by
  `.hikerignore` ([[spec:watcher-config-ignore-file]]). [self-host-code-in-vault]
- **The in-vault copy is the integration bench**, not a passive mirror: agent branches merge
  here, spec updates are authored here, reconcile/scip/ack run here, and the integrated
  result is pushed back out from here. [self-host-bench]
- **Boards are the work queue.** A story's status is its board column ([[spec:derived-status-rule]]);
  an agent claiming work *is* a `board_move_card` to Doing. The `task_checkout` queue
  ([[spec:sys-task-queue]]) is for system jobs (embedding, summaries), never for dev work —
  it's in-memory and dies with the process. [self-host-boards-are-the-queue]
- **Agents are fed over MCP** for planning writes (claims, reports) so those stay attributed
  and reviewable, and they edit *code* on isolated worktrees merged via git. [self-host-agent-feeding]
- **Review mode is the human gate.** Agent writes stage as pending batches the human accepts
  ([[spec:agent-write-review-mode]]); nothing lands unreviewed while the loop is being trusted.

## Topology

One bare-ish hub and several working copies (spokes):

```
hiker-main/                         the hub (shared object store; all spokes fetch/push here)
~/projects/.../hiker/               dev checkout — human-driven edits, ad-hoc work
~/code-intel-vault/                 the Hiker vault (op-log governs; PM notes at root)
  hiker/                            the BENCH: in-vault working tree + .git (CODE-IN-VAULT)
  hiker.scip                        SCIP index of the bench, pinned at index_commit
  hiker-project.md                  project descriptor (repo root + .scip + repo_id)
  pm/                               planning surface — epics/ sprints/ stories/ backlog
~/agent-worktrees/<branch>/         disposable agent worktrees (OUTSIDE the vault), share
                                    the bench's .git so a merge is local, no remote needed
```

Why worktrees live *outside* the vault: a worktree inside the vault would get its
`docs/*.md` indexed a second time at a second path. Keep them out; only the bench is watched.

## The two realms

| | Code realm | Planning realm |
| --- | --- | --- |
| Lives at | `<vault>/hiker/` (a git repo) | `<vault>/pm/` (vault-root notes) |
| Governed by | git (branches, merges, `hiker-main`) | the op-log (vault history) |
| Edited by | agents on worktrees → `git merge` | MCP writes (board moves, story edits) |
| History | git commits | op-log frames + activity feed |
| Synced by | plain git to the hub | the vault's own mechanism (or none) |

They meet only at **spec slugs**: a story references the `[slug]`s it implements, and the
specs themselves live in `hiker/docs/` — so when an agent's code change also edits a spec,
that spec edit travels with the code through git, and the planning story that points at the
slug stays the stable work-side record. [self-host-two-realms]

A consequence worth stating: PM notes under `pm/` are op-log-versioned but **not**
git-versioned (they sit outside the `hiker/` repo, and the vault root is not itself a repo).
That is fine — the op-log is durable — unless you want planning history in git, which would
be a separate repo at the vault root. (Open decision below.)

## The bench

The in-vault `hiker/` copy is where integration happens. Its jobs, in order of a cycle:

1. **Merge** — fetch agent branches from the hub and `git merge` them into the bench on a
   clean tree.
2. **Author specs** — write new/updated specs *in Hiker's editor* against the bench's
   `docs/*.md`, with live `[[code:…]]` navigation. This is the CODE-IN-VAULT payoff.
3. **Reconcile** — regenerate `hiker.scip`, bump `index_commit`, run `code-cli drift`, fix
   drifted specs, `code-cli ack`. The bench is the **single place** reconcile/ack run, so
   `links.json` (the drift baseline, which lives inside the repo) never becomes a merge
   surface. Agents must not run `ack` on their branches.
4. **Push** — push the integrated result back to the hub; spokes sync from there.

The one tax, and it is manual: **Hiker-save ≠ git-commit.** There is no git transport on
this vault (the vault root is not a repo; the repo is the nested `hiker/`), so a spec edited
in Hiker is an op-log frame + a dirty working-tree file — git doesn't know. So: author specs
→ `git -C <vault>/hiker add docs && git commit` → *then* merge agent branches into a clean
tree. [self-host-bench]

Do **not** hand-edit the bench's code (`.rs`) through Hiker's editor while git is merging
into it — treat that subtree as git-driven, browse-and-link only in Hiker. Disjoint edits
would merge and same-region would surface a conflict (handled), but the clean discipline is
"code edits happen on the worktree side."

## The per-cycle runbook

```
# 1. an agent produces a branch on a worktree sharing the bench's .git
git -C <vault>/hiker worktree add ~/agent-worktrees/feat-x -b feat-x
#    … agent edits code there, commits …

# 2. integrate on the bench (clean tree first)
git -C <vault>/hiker add docs && git -C <vault>/hiker commit -m "specs: …"   # if you authored specs
git -C <vault>/hiker merge feat-x                                            # resolve conflicts here

# 3. reconcile code-intelligence (the bench is the only place this runs)
cd <vault>/hiker && rust-analyzer scip . --output ../hiker.scip
#    update index_commit in hiker-project.md
code-cli drift   # re-read drifted specs, fix prose/code
code-cli ack <spec>

# 4. push the integrated result to the hub
git -C <vault>/hiker push

# 5. close the loop in the planning realm (over MCP): move the story's card to Done,
#    append the agent's report to the story body
```

## Agent feeding (planning realm)

Planning work flows over MCP so it stays attributed and reviewable:

- **Pick** — `query` for todo-category stories in the active sprint, by priority
  ([[spec:query-mcp-tool]]).
- **Claim** — `board_move_card` the story to Doing. The move *is* the claim, authored
  `agent:<id>` in the op-log.
- **Work** — the agent does the code on its worktree (code realm), independent of MCP.
- **Report** — `edit_note` appends the completion report to the story body; `board_move_card`
  to Review.
- **Gate** — with `[op-log] review_required` on, each MCP write stages as a pending batch the
  human accepts in the Patch-review tab. [self-host-agent-feeding]

Code edits, by contrast, never go through MCP write tools (note-shaped, wrong for code) —
they ride git and land as `Author::External` op-log frames when merged. So **attribution
splits by realm**: planning writes keep `agent:<id>`, code edits read `external` in the
activity feed (the real authorship is in the git commits). [self-host-attribution]

## Keeping the nested repo clean

The bench is a whole repo inside the vault, so the ingest pipeline must not drown in it:

- **`.hikerignore`** ([[spec:watcher-config-ignore-file]]) excludes the non-note parts —
  build trees, vendored deps, test-fixture `.txt` — via the composed matcher in
  `core::ignore`, now enforced across every ingest seam (indexer scan, watcher registration +
  events, op-log seed/reconcile, `list_dir`, `process_upsert`). The **note-protection
  invariant** keeps `.md` notes indexable regardless. The matcher excludes only NON-note
  files at the file-glob level; a directory ignore prunes its whole subtree (including notes
  under it) — so to keep `hiker/docs/` you enumerate exclusions, you do not blanket-ignore
  `hiker/`.
- **Submodule-aware git transport** ([[spec:bug-git-transport-not-submodule-aware]]) — only
  relevant if vault-level git sync is ever turned on — lets the nested repo be a declared
  submodule (vault ships the pointer; submodule ships its content via its own remote) instead
  of colliding as an embedded repo. Orthogonal to `.hikerignore`: ignore controls what Hiker
  *indexes*, submodule-awareness controls what Hiker *git-ships*.

## Skills

The three hiker skills re-point onto this loop (they already exist; the change is what they
read/write):

- **`hiker-pm`** plans by `query`-ing the vault's stories/boards over MCP and audits
  implementations against specs — instead of auditing markdown plan files.
- **`hiker-dev`** starts by checking out its story (board move), reads the specs owning the
  slugs, implements on a worktree, and ends by moving the card to Review + writing its report
  into the story note.
- **`hiker-spec-writer`** authors specs on the bench (`hiker/docs/`), registers slugs, and
  the spec edits flow out through git like any code change.

## Open decisions (to solidify)

1. **Story grain.** One story per spec slug (hundreds of leaves) vs one per *slice* (what
   `hiker-dev` takes in one invocation, ~2–5 slugs) with slugs in frontmatter. Leaning slice.
2. **Single vs concurrent agents.** With one agent at a time, the board-move claim suffices.
   Concurrent agents need a `claimed_by` convention + a stale-claim sweep — a natural first
   real use of the rules engine (`date-passed` on a `claim_expires` key).
3. **PM notes in git?** Op-log-only (current) vs a separate git repo at the vault root for
   planning history. Affects backup/sync of the backlog.
4. **The allowlist hole.** Do we add an "index-only-these-globs" mode so a subtree can be
   blanket-ignored except its docs, or is enumerating exclusions in `.hikerignore` enough?
5. **Sequencing of the remaining ingest/sync features:** `.hikerignore` live-refresh
   ([[spec:bug-hikerignore-no-live-refresh]]) and the retroactive prune
   ([[spec:bug-ignored-docs-not-pruned-retroactively]]) are near-term; submodule-awareness is
   gated on adopting vault git sync.

## Build status

- **Done:** PM kinds + boards + derived status ([[spec:sys-pm]]), the rules engine
  ([[spec:sys-rules]]), the query grammar + MCP `query` ([[spec:sys-queries]]), the generated
  per-kind MCP tools ([[spec:mcp-registry-tools]]), CODE-IN-VAULT project binding
  ([[spec:projects-config-tab]]), `.hikerignore` + the unified ingest seams
  ([[spec:watcher-config-ignore-file]]).
- **Near-term:** seed the `dev` plan/epics/sprints/stories in `pm/` from the current plan
  docs; `.hikerignore` live-refresh; retroactive prune of already-seeded ignored docs;
  re-point the three skills.
- **Deferred:** submodule-aware git transport (gated on vault git sync); the allowlist ignore
  mode (if decided); concurrent-agent stale-claim rule.
