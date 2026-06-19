# Git (optional, user-driven)

Git is hiker's optional, user-driven version-control integration — the VSCode
model. It is **inert until the user opts in** (`[git] enabled`, default off) over a
vault that is already a git repo, and even then it never runs an automatic
push/pull round. The vault is an ordinary folder of plain `.md` files; the local
editing model (`op-log.md`) and the disposable snapshot history (`op-log.md`
"Local history") work with no git at all. Git is the *richer, shareable* history:
turn it on and you get a globally-ordered commit graph you can inspect, push, and
host anywhere.

There is no multi-device sync engine. The always-on libp2p sync engine and the
integrated git push/pull driver were removed; git here is a thin, *invoked*
capability over `hiker-git` (the libgit2 wrapper), not a transport that owns the
loop.

The `.md` files are canonical; `.hiker/` is gitignored (its `.pending` edits,
snapshot history, and index are hiker-local; git supplies its own commit-graph
history in parallel). [git-canonical-md, git-ignores-hiker]


## Modes

### Integrated

Hiker may drive commit-on-save; the user drives push/pull. [git-integrated-mode]
status:: done
implements:: [[code:hiker/bootstrap/impl#[Spawner]spawn_git_engine]]
touches:: [[code:hiker/git_sync]]
note:: `app/src/git_sync/mod.rs` (`GitSyncEngine` integrated mode): debounced commit-on-save only. No push/pull round driver, no automatic fetch→merge engine (both removed when git was demoted to optional/user-driven). Push/pull is the user's job — from their terminal or a future VSCode-style button.

- **Commit on save.** Save writes the `.md` (`op-log.md` [[spec:op-log-save-policy]])
  and, when `auto_commit` is on, commits with the `Hiker-Author` trailer (below).
  Debounced/idle-coalesced so a burst of saves doesn't mint a commit per
  keystroke-burst; rapid saves `--amend`-coalesce within the debounce window. An
  empty `remote` is fine — that is commit-only local versioning. [git-commit-on-save]
status:: done
implements:: [[code:hiker/git_sync/impl#[GitSyncEngine]commit_now]]
touches:: [[code:hiker/git_sync]]
note:: `app/src/git_sync/mod.rs::{spawn_commit_task,commit_for_save_burst,commit_now}` — save pokes `notify_local_change`, debounced (`commit_debounce_ms`) into one commit with the `Hiker-Author: user` trailer; `commit_now(_, amend=true)` `--amend`-coalesces. `hiker-git/src/repo.rs::commit_paths`. Tests `commit_on_save_produces_a_user_trailer_commit`, `amend_coalesces_into_one_commit`, `no_op_commit_returns_none`
- **Push/pull is the user's job.** Hiker does not push or pull on its own in either
  mode. The user runs `git push` / `git pull` from their terminal (or, later, a
  button that invokes the backend's `pull`/`push`); hiker then sees the resulting
  working-tree change like any external edit (below). There is no interval/poke/
  on-discovery trigger and no automatic fetch→merge engine. [git-no-auto-push-pull]
status:: done
touches:: [[code:hiker/git_sync]]
note:: the integrated push/pull round driver + the automatic `git merge`-driven inbound reconcile engine were deleted with the sync removal; `hiker-git` still exposes `pull`/`push`/`log`/`show`/`diff_paths` as invoked primitives
- **Backend trait.** Reached behind a `GitBackend` trait so the implementation is
  swappable: libgit2 today. Plain Rust types cross the boundary; the git library is
  confined to the `hiker-git` crate. [git-backend-trait]
status:: done
note:: `hiker-git` crate: `repo.rs::GitBackend` trait (open/init, gitignore, `commit_paths`, `commit_rename`, `pull`, `push`, `log`, `show`, `diff_paths`, `divergence_from`, `head_sha`) + `Libgit2Backend` impl. `git2`/libgit2 confined to this crate; only plain-Rust types (`CommitInfo`/`Author`/`Trailers`/`Divergence`/`ChangeStatus`) cross the boundary

### Manual

The user drives git themselves (their own commit cadence, their own
`push`/`pull`/`rebase`); hiker cooperates rather than competing. [git-manual-mode]
status:: done
touches:: [[code:hiker/git_sync]]
note:: `app/src/git_sync/mod.rs` manual mode: `commit_for_save_burst` reconciles a HEAD move then auto-commits only the user's still-uncommitted tree; hiker never pulls/pushes/rebases.

- **HEAD moves underneath.** A user `pull`/`checkout`/`rebase` rewrites the working
  tree out from under hiker. Hiker treats any working-tree divergence from its
  last-known commit as an external edit (`op-log.md` external edits) — the same path
  as a disk edit from another editor — and never assumes it owns HEAD.
  [git-tolerate-head-move]
status:: done
touches:: [[code:hiker/git_sync]]
note:: `app/src/git_sync/mod.rs::manual_reconcile` + `hiker-git/src/repo.rs::divergence_from` — detects HEAD moving / a dirty tree vs the last-known commit, folds each changed path's clean buffer through the reload path, then adopts the new HEAD as known (hash-gated + idempotent). Test `manual_head_move_folds_as_external_edit`
- **Commit-or-not.** Hiker can leave committing entirely to the user (write the
  `.md`, never commit) or auto-commit only when the user hasn't (configurable). It
  never force-pushes, never rewrites the user's history, and never runs
  `pull`/`push`. [git-manual-commit-policy]
status:: done
touches:: [[code:hiker/git_sync]]
note:: `app/src/git_sync/mod.rs::commit_for_save_burst` (manual branch) — reconcile-then-commit-only-if-uncommitted; manual mode never pushes/pulls/rebases/force-pushes


## Conflicts: the inline marker resolver

When the user's *own* `git pull`/`merge`/`rebase` leaves standard conflict markers
in a `.md` (`<<<<<<<` ours / `=======` / `>>>>>>>` theirs; the user's git config may
add a `|||||||` base section), hiker serves them with a VSCode-style in-editor
resolver. This is the only conflict surface git needs now that there is no automatic
inbound-merge engine. [git-conflict-inline-markers]
status:: done
implements:: [[code:hiker/panels/buffer/gitmerge/parse_conflicts]], [[code:hiker/panels/buffer/gitmerge/resolve_region]], [[code:hiker/panels/buffer/conflict_overlay/build_conflict_overlay]], [[code:hiker/panels/buffer/conflict_overlay/impl#[AppState]resolve_conflict_region]]
verifies:: [[code:hiker/panels/buffer/gitmerge/tests]]
touches:: [[code:hiker/git_sync]], [[code:hiker/panels/buffer]]
note:: `app/src/panels/buffer/gitmerge.rs` (pure marker parse/resolve — zdiff3 + classic, unit-tested) + `conflict_overlay.rs` (per-region decorations: ours/base/theirs tints + an action row with Accept Current/Incoming/Both, click → `resolve_conflict_region` rewrites the region via a Transaction). Live-preview is suppressed for a markered buffer so markers stay visible + editable; hand-editing markers works too. NOTE: the egui paint + click path isn't headless-testable — the marker logic is unit-tested

- Hiker shows the markered file in the editor (source view, markers visible and
  freely hand-editable) and decorates each conflict region with quick-resolve
  actions: **Accept Current** (ours), **Accept Incoming** (theirs), **Accept Both**.
  A button rewrites just that region (markers removed, chosen side kept); the user
  can also resolve by hand. Once the file has no markers left and is saved, it is a
  normal save — the user then `git add`s and commits in their own flow.


## Identity and renames

Path is identity (`op-log.md`). Git stores no rename metadata — a commit is a
snapshot and `git mv` is delete-old + add-new — so rename continuity is recovered at
read time by content similarity (`git log --follow`, `-M`), reliable when the bytes
are unchanged across the rename. A move hiker **observes** is committed to make
detection trivially correct: [git-observed-rename-commit]
status:: partial
touches:: [[code:hiker/git_sync]]
note:: `hiker-git/src/repo.rs::commit_rename` + `app/src/git_sync/mod.rs::commit_observed_move` — a pure-rename commit (new path, byte-identical old content) carrying `Hiker-Rename: <from> -> <to>`, then an edit commit if content also changed. Tests `observed_rename_is_pure_rename_commit_that_follows`, `observed_move_is_a_pure_rename_commit_with_trailer`. GAP: `commit_observed_move` is not yet wired to the file-tree/dnd rename trigger

1. **Pure-rename commit** — the new path carrying the *old* content (byte-identical
   to HEAD at the old path), so `--follow`/`-M` match it with certainty.
2. **Edit commit** — only if the content also changed: a normal modify at the new
   path, after the rename commit.

The rename commit carries `Hiker-Author` plus `Hiker-Rename: <from> -> <to>`, so the
move is self-describing and never depends on git's heuristic. The heuristic is only
the fallback for a rename + heavy rewrite performed outside hiker in one step.


## Attribution trailers

Git carries author + timestamp per commit; hiker's finer authorship classes ride a
commit trailer so the git history is self-describing — the sole attribution record
now that the `op_history`/activity-feed side table is gone: [git-attribution-trailer]
status:: done
note:: `hiker-git/src/meta.rs::{Author,Trailers}` — `Hiker-Author: <class>` (user/agent:id/external/extractor:id/auto:id), `Hiker-Rename: <from> -> <to>` on a move; render + parse round-trip, unknown author → external. Tests in `meta.rs` + `log` reads them back (`agent_author_trailer_round_trips_through_log`)

```
Hiker-Author: <class>            # user | agent:<id> | external | extractor:<id> | auto:<id>
Hiker-Rename: <from> -> <to>     # only on an observed-move commit
```

The trailer records who *authored* the change, not who accepted it.


## History reads

When git is integrated it is the richer, shareable history; the always-available
*local* history is the plain-file snapshots (`op-log.md` "Local history"). The git
read API is exposed for inspection/interop and to drive the version dropdown when
git is on:

- **`log` / `show`.** `git log --follow <path>` and `git show <sha>:<path>` for
  per-note history and content-at-a-revision. [git-parallel-history]
status:: partial
note:: `hiker-git/src/repo.rs::{log,show}` expose `git log` / `git show <sha>:<path>`. GAP: the in-app version dropdown currently reads snapshots; git-sourced history is the next wiring step
- **Changed-paths diff.** `diff_paths(base_rev, head_rev)` lists the paths that
  differ between two revisions (each tagged Added / Modified / Deleted / Renamed), or
  between a revision and the **working tree** when no head rev is given (through the
  index, untracked files included, `.hiker/` excluded). Revs resolve like
  `git rev-parse`; a byte-similar delete+add pair collapses to one Renamed row. Feeds
  the diff-summary viewer (`diff.md`). [diff-paths-trait-method]
status:: done
implements:: [[code:hiker/repo/impl#[Libgit2Backend][GitBackend]diff_paths]]
verifies:: [[code:hiker/diff_paths_between_revs_resolves_head_and_short_shas]], [[code:hiker/diff_paths_against_workdir_sees_uncommitted_changes]], [[code:hiker/diff_paths_reports_a_move_as_renamed]]
note:: the libgit2 impl rides `diff_tree_to_tree` (rev↔rev) / `diff_tree_to_workdir_with_index` (rev↔worktree) + `find_similar` for rename detection; the app reads it through the git engine's `diff_paths` pass-through

Rollback stays forward-correct (`op-log.md`): "restore this version" writes the old
content as a new save (and, with git on, a new commit), never rewriting history.


## Co-tenancy

A user may run their own git over the same repo (hooks, CI, signing). The contract
that keeps this safe: [git-co-tenancy]
status:: done
note:: `.md` canonical (the fold reads disk, not HEAD); hiker never force-pushes / rebases / rewrites history (`push` never forces, neither mode auto-pushes); user hooks run untouched (hiker drives libgit2 directly, suppresses nothing)

- **The `.md` on disk is canonical**, not HEAD — so a commit hook that reformats, or
  a user amend, is seen on next reconcile as an external edit and folded, never lost.
- **Hiker never rewrites user history** (no force-push, no rebase of user commits).
- **Hooks run as the user configured them**; hiker doesn't suppress or require any.


## `[git]` config section

[git-config-section]
status:: done
implements:: [[code:hiker/config/vcs/GitSection]], [[code:hiker/config/Config#git]], [[code:hiker/panels/settings/impl#[`SettingsCtx<'a>`]git_section]]
note:: `core/src/config/vcs.rs::GitSection` (`enabled`/`mode`/`remote`/`auto_commit`/`commit_debounce_ms`/`gc_interval_days`/`submodules`) + `GitMode`/`SubmoduleMode`; eligible-keys in `patch.rs`; settings rows in `app/src/panels/settings/mod.rs::git_section`. Default off — git is inert until the user enables it. Replaces the former `[sync].transport = "git"` selector (the `[sync]` section is gone).

```toml
[git]
enabled = false         # opt in to hiker's git integration for this vault (default off)
mode = "integrated"     # "integrated" (hiker may commit-on-save) | "manual" (user drives)
remote = ""             # push/pull target (user-driven); empty = commit-only local versioning
auto_commit = true      # commit on save (in manual mode, only when the user hasn't)
commit_debounce_ms = 1500
gc_interval_days = 30    # periodic `git gc`
submodules = "skip"     # "skip" | "submodule" — how a nested repo in the vault is handled
```

| Key | Type | Default | Scope | Notes |
| --- | ---- | ------- | ----- | ----- |
| `enabled` | bool | `false` | vault | Opt in over a vault that is already a git repo. Off = git inert. |
| `mode` | enum | `integrated` | vault | `integrated` (hiker may commit-on-save) or `manual` (user drives). |
| `remote` | string | `""` | vault | User-driven push/pull target; empty = commit-only local versioning. Hiker never auto-pushes. |
| `auto_commit` | bool | `true` | vault | Commit on save. In `manual` mode, only commits when the user hasn't. |
| `commit_debounce_ms` | u32 | `1500` | vault | Coalesce rapid saves into one commit. |
| `gc_interval_days` | u32 | `30` | vault | Periodic `git gc` to keep packfiles compact. |
| `submodules` | enum | `skip` | vault | Nested-repo handling (below). |


## Nested repositories

A vault may contain a whole git repo (the CODE-IN-VAULT pattern — `projects.md`: a
project's source checked out inside the vault so its specs index as notes and
`[[code:…]]` resolves against real files). `[git] submodules` decides how that
nested repo relates to the vault repo: [git-nested-repo-submodule]
status:: done
implements:: [[code:hiker/repo/impl#[Libgit2Backend]ensure_submodules_registered]], [[code:hiker/repo/impl#[Libgit2Backend]update_submodules]], [[code:hiker/config/vcs/SubmoduleMode]]
verifies:: [[code:hiker/tests/repo/submodule_mode_declares_and_tracks_a_nested_repo]], [[code:hiker/tests/repo/skip_mode_default_leaves_nested_repo_untracked]]
note:: default `skip` preserves one-vault-one-repo; the whole-tree stage dispatches on the policy: SKIP excludes nested-repo subtrees so an undeclared embedded repo is never swallowed as a stray gitlink; SUBMODULE registers a `.gitmodules` stanza then stages each nested repo as a gitlink at its current HEAD. `update_submodules` (`submodule update --init`) is implemented for the restore-on-another-machine flow

- **`skip` (default).** The nested repo is independent — excluded from the vault
  tree, managed by its own remote. One-vault-one-repo, the conservative posture.
- **`submodule`.** The nested repo is declared a git submodule: the vault commit pins
  its HEAD as a gitlink so a vault clone can restore planning state *and* the exact
  code commit it referenced.


## Out of scope

- **Multi-device sync as an engine.** Removed. Git is one *user-driven* way to move
  and version text; a third-party file sync of the vault folder also works because the
  vault is plain files.
- **Hosted-git account integration** (OAuth to a forge, PR flows). The remote is
  whatever git URL the user configures.
- **LFS / large-binary policy.** Attachments are committed as-is; large-blob policy is
  the user's `.gitattributes` job.


## Forward refs

- `op-log.md` — the local editing model and the plain-file snapshot history git parallels; the `Author` classes the trailers mirror; the external-edit reload path a HEAD move folds through.
- `settings.md` — config conventions for the `[git]` section.
- `diff.md` — the diff-summary viewer `diff_paths` feeds.
