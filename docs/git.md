# Git transport

Git as a sync/versioning transport behind the pluggable seam (`sync.md` `sync-transport-seam`). Two modes — **integrated** (hiker drives git) and **manual** (the user drives git, hiker cooperates) — feed the same 3-way text merge and the same unified conflict surface as every other transport. Git is never the *substrate*; the substrate is the local op log (`op-log.md`). Git here is one way to move and version committed text, chosen for its transparency and interoperability: the vault is an ordinary git repo a user can inspect, push, and host anywhere.

The `.md` files are canonical; `.hiker/` is gitignored (its `.ops` history and `.pending` edits are hiker-local, and git supplies its own commit-graph history in parallel). [git-canonical-md, git-ignores-hiker]


## Modes

### Integrated git

Hiker drives commit + push/pull; the user brings a remote (a bare repo, a hosted git, or a git-aware synced folder). [git-integrated-mode]

- **Commit on save.** Save writes the `.md`, appends the local `.ops` frame (`op-log-save-policy`), and commits with the `Hiker-Author` trailer (below). Debounced/idle-coalesced so a burst of saves doesn't mint a commit per keystroke-burst; rapid saves may `--amend`-coalesce within the debounce window. An agent-accept can be its own commit. [git-commit-on-save]
- **Push/pull on the sync triggers.** The same triggers as the libp2p engine (startup, interval, on-poke) run `pull` then `push`; an inbound merge that diverges from local feeds the 3-way merge + conflict surface rather than git's own conflict markers being left in the file. [git-push-pull-rounds]
- **Backend trait.** Reached behind a `GitBackend` trait so the implementation is swappable: libgit2 today, `gix` once its push/merge mature. Plain Rust types cross the boundary; the git library is confined to the transport crate. [git-backend-trait]

### Manual git

The user drives git themselves (their own commit cadence, their own `push`/`pull`/`rebase`); hiker cooperates rather than competing. [git-manual-mode]

- **HEAD moves underneath.** A user `pull`/`checkout`/`rebase` rewrites the working tree out from under hiker. Hiker treats any working-tree divergence from its last-known `accepted` as an external edit (`op-log-external-edit-sync`) and folds it through the 3-way merge — the same path as a disk edit from another editor. It never assumes it owns HEAD. [git-tolerate-head-move]
- **Commit-or-not.** Hiker can either leave committing entirely to the user (write the `.md`, accrue local `.ops` history, never commit) or auto-commit only when the user hasn't (configurable). It never force-pushes, never rewrites the user's history, and never runs `pull`/`push` in manual mode. [git-manual-commit-policy]
- **No racing.** Because hiker neither pulls nor pushes in manual mode, there is no auto-commit-racing-manual-git hazard; the only coupling is the external-edit fold, which is hash-gated and idempotent.

The **single-bidirectional-sync rule** (`sync-single-bidirectional-transport`) means git-as-sync (integrated or manual-with-push) is mutually exclusive with libp2p sync. Manual git used purely as local versioning (commit-only, no remote) may run alongside libp2p — it's then a second local history, not a second sync path.


## Identity and renames

Path is identity (`op-log-path-identity`). Git stores no rename metadata — a commit is a snapshot and `git mv` is delete-old + add-new — so rename continuity is recovered at read time by content similarity (`git log --follow`, `-M`), reliable when the bytes are unchanged across the rename. A move hiker **observes** (`op-log-observed-move`) is therefore committed to make detection trivially correct: [git-observed-rename-commit]

1. **Pure-rename commit** — the new path carrying the *old* content (byte-identical to HEAD at the old path), so `--follow`/`-M` match it with certainty.
2. **Edit commit** — only if the content also changed: a normal modify at the new path, after the rename commit.

The rename commit carries `Hiker-Author` plus `Hiker-Rename: <from> -> <to>`, so the activity feed has an authoritative record of moves hiker made and never depends on git's heuristic. The heuristic is only the fallback for a rename + heavy rewrite performed outside hiker in one step (`op-log-rename-follow-heuristic`).


## Attribution trailers

Git carries author + timestamp per commit; hiker's finer authorship classes ride a commit trailer mirroring the `.ops` frame's `Author` (`op-log-attribution`): [git-attribution-trailer]

```
Hiker-Author: <class>            # user | agent:<id> | external | extractor:<id> | auto:<id> | sync:<device>
Hiker-Rename: <from> -> <to>     # only on an observed-move commit
```

The activity-feed projection in git mode can read either source — the local `.ops` frames or `git log` + trailers — and they agree by construction (a commit and its frame are written together). The trailer records who *authored* the change, not who accepted it.


## History

Git's commit graph is a parallel, transport-level history; the authoritative local history is still the `.ops` frames (`op-log-history-materialization`), which exist in every transport mode. In git mode the two agree per commit, and git additionally gives a globally-ordered cross-device commit graph (which the libp2p path does not — that's a fine trade, `op-log` history is per-device append). `git show <sha>:<path>` and `git log --follow` are available for inspection/interop, but the in-app version dropdown and activity feed read `.ops` so they work identically across transports. [git-parallel-history]

Rollback stays forward-correct (`changes-rollback-helper`): "restore this version" writes the old content as a new commit, never rewriting history.


## Co-tenancy

A user may run their own git over the same repo (hooks, CI, signing). The contract that keeps this safe: [git-co-tenancy]

- **The `.md` on disk is canonical**, not HEAD — so a commit hook that reformats, or a user amend, is seen on next reconcile as an external edit and folded, never lost.
- **Hiker never rewrites user history** (no force-push, no rebase of user commits, no history surgery).
- **Hooks run as the user configured them**; hiker doesn't suppress or require any. A reformatting hook just produces an external-edit fold on the next pass.


## `[git]` config section

[git-config-section]

```toml
[git]
mode = "integrated"     # "integrated" | "manual"
remote = ""             # integrated: push/pull target (empty = local-only versioning)
auto_commit = true      # commit on save
commit_debounce_ms = 1500
gc_interval_days = 30    # periodic `git gc`
```

| Key | Type | Default | Scope | Notes |
| --- | ---- | ------- | ----- | ----- |
| `mode` | enum | `integrated` | vault | `integrated` (hiker drives) or `manual` (user drives). |
| `remote` | string | `""` | vault | Integrated push/pull target; empty = commit-only local versioning. |
| `auto_commit` | bool | `true` | vault | Commit on save. In `manual` mode, only commits when the user hasn't (`git-manual-commit-policy`). |
| `commit_debounce_ms` | u32 | `1500` | vault | Coalesce rapid saves into one commit. |
| `gc_interval_days` | u32 | `30` | vault | Periodic `git gc` to keep packfiles compact. |

Selecting the git transport is `[sync].transport = "git"` (`sync-config-section`); this section configures it.


## Out of scope

- **Hosted-git account integration** (OAuth to a forge, PR flows). The remote is whatever git URL the user configures.
- **LFS / large-binary policy.** Attachments are committed as-is; keeping large blobs out of history is the user's `.gitattributes` job.
- **Submodules / monorepo layouts.** One vault = one repo.


## Forward refs

- `sync.md` — the transport seam, the 3-way merge, the unified conflict surface this transport feeds.
- `op-log.md` — the substrate: `accepted`, `.ops` history, observed moves, external-edit reconcile, the `Author` classes the trailers mirror.
- `settings.md` — config conventions for the `[git]` section.
