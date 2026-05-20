# Release & branching

How versions are numbered, how branches flow into releases, and what a "release" actually means for hiker.

The headline decisions:

- SemVer, with a `0.x.y` prefix until v1. Minor bumps may break; patch bumps may not.
- Trunk-based: `main` is always shippable. Work happens on short-lived feature branches that squash-merge back.
- Releases are git tags (`v0.3.1`) on a commit on `main`. The tag is the source of truth; CHANGELOG and the embedded version must match it.
- A `dev` branch is not used in v1. Add one only when there's a concrete reason (stabilization window, multi-contributor staging) — not preemptively.


## Versioning

Pre-stable SemVer (`0.MAJOR.MINOR`-style use of the slots), interpreted as:

- **`0.x.0` (minor bump while pre-1.0)** — may break: vault format, `vault/.hiker/` layout, DB schema, config schema, CLI flags, host command signatures consumed by the UI. The `store-version-fail-loud` rule applies — if the user's existing vault is incompatible, we bail loudly and provide a migration path or a `hiker reindex --rebuild`.
- **`0.x.y` (patch bump)** — bug fixes, internal refactors, additive features that don't change persisted formats or public surfaces. Safe to upgrade in place.
- **`1.0.0`** — when the persisted formats and public surfaces are stable enough to defend across a year. Not on the near-term roadmap.

What counts as a breaking change at 0.x:

| Surface | Counts as breaking |
| ------- | ------------------ |
| Vault `.hiker/` directory layout | yes |
| SQLite schema (without auto-migration path) | yes |
| `vault/.hiker/config.toml` schema | yes |
| CLI flags / subcommand names | yes |
| Host command names or signatures | yes |
| Embedder model / dimension change | yes (forces reindex; bumps `embedder_version`) |
| Default keybinds | no — annoying but not breaking |
| Internal Rust API between modules | no — there are no external consumers |

When in doubt: a change that requires a returning user to do anything other than launch the new build is breaking.


## Where the version lives

Two declarations, kept in sync at release time:

- `core/Cargo.toml` → `package.version`
- `app/Cargo.toml` → `package.version`

The build embeds the version into the binary; `hiker --version` prints it, and the UI surfaces it in the about dialog / status bar tooltip. A pre-release build off `main` reports the version with a `-dev` suffix or commit short SHA so a bug report identifies the exact build.

A `release.sh` script (or a justfile target) bumps both files in one commit. Manual edits across two files invite drift.


## Branching

Trunk-based with short-lived feature branches:

```
main ───●───────●───────●───────●───────●─── (tags: v0.3.0, v0.3.1, ...)
         \     / \     / \     / \     /
          ●---●   ●---●   ●---●   ●---●
          feat/x  feat/y  fix/z   feat/w
```

Rules:

- **`main` is always shippable.** Every commit on `main` should pass the test suite and be tag-able if needed.
- **Feature branches are short-lived.** Hours to a few days, not weeks. If a branch is going to live longer, it's probably hiding a too-big change that should be split.
- **Squash-merge into `main`.** The squashed commit is the unit of history; its message describes the *change*, not the path that got there.
- **Branch naming:** `feat/<slug>`, `fix/<slug>`, `chore/<slug>`. Where possible the `<slug>` is a status.md slug so the branch name itself is greppable.

No `dev` branch, no long-running release branches. If a release needs stabilization, cut a `release/0.x` branch *at tag time* from `main`, fix-forward there, and cherry-pick fixes back to `main`. This is a tool we reach for when a release needs it — not a permanent branch.


## Squash-merge message convention

Because the squashed commit is the only record on `main`, its message has to carry the whole change. Format:

```
<verb>: <one-line summary> (<status-slug-if-applicable>)

<paragraph or bullets describing what changed and why,
referencing relevant slugs and any breaking implications>
```

Verbs: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `breaking`.

Examples:

- `feat: tree drag-and-drop note moves (drag-and-drop-move)`
- `fix: watcher self-write loop on rapid renames (watcher-suppress-self-writes)`
- `breaking: switch chunk vector storage to sqlite-vec virtual tables (store-schema-v1)`

Anything labelled `breaking:` requires a minor (`0.x.0`) bump, a CHANGELOG entry under "Breaking," and notes on how a returning user gets back to a working vault.


## Tagging a release

1. Decide the version number per the bump rules above.
2. Bump `core/Cargo.toml` and `app/Cargo.toml` in one squash-merged commit titled `chore: release v0.x.y`.
3. Update `CHANGELOG.md` in the same commit.
4. Tag the commit: `git tag -a v0.x.y -m "v0.x.y"` and push the tag.
5. Build release artifacts from the tagged commit, not from `main` after-the-fact (avoids drift if `main` moves before you build).

The tag is what users, bug reports, and download URLs reference. If a release is broken, fix forward and tag a new patch — never re-point an existing tag.


## CHANGELOG

`CHANGELOG.md` at repo root, keep-a-changelog-style:

```
## [0.3.1] - 2026-05-12
### Fixed
- Watcher self-write loop on rapid renames (watcher-suppress-self-writes)

## [0.3.0] - 2026-05-06
### Added
- Tree drag-and-drop note moves (drag-and-drop-move)
### Breaking
- Vault `.hiker/trash/` directory introduced; existing vaults gain it on first run (vault-trash)
```

Entries reference status.md slugs where relevant — that's the cross-link from "what shipped" to "what was specced."


## Pre-release builds

Local development builds and one-off shares of `main` between tags get a version like `0.3.1-dev.<short-sha>`. The `-dev.` suffix makes it sort below the eventual `0.3.1` release per SemVer pre-release rules, and the SHA pins the exact source. Generated automatically in the build script; not committed to the version files.


## Deferred

- **Code signing / notarization** for macOS + Windows builds. Required before any public distribution; out of scope until there's a public distribution channel.
- **Auto-update / in-app update channel.** Not needed until there are users on builds we don't hand to them ourselves.
- **Release artifacts (CI-built)** uploaded to GitHub Releases on tag push. Deferred until we want a download URL.


## Out of scope

- Public stability promises about Rust APIs between internal modules. There are no external consumers; the only contract is at the persisted-format / CLI / host-command boundary.
- Linux distro packaging (deb/rpm/flatpak). Not on the near-term roadmap.
- A `dev` long-lived branch — explicitly rejected for v1; revisit if contributor count or release cadence justifies it.
