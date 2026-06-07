//! `hiker-git` — the git transport's backend, with `git2`/libgit2 confined here.
//!
//! This crate is the git side of the pluggable sync transport seam
//! (`sync.md` `sync-transport-seam`, `git.md`). It exposes one
//! [`repo::GitBackend`] trait whose verbs the sync orchestration drives —
//! open/init a repo, write the `.hiker/` gitignore, commit a path-set with
//! `Hiker-Author` / `Hiker-Rename` trailers, a pure-rename commit, pull (fetch +
//! merge-base), push, read `git log` / `git show` for inspection, and detect
//! working-tree divergence from a known commit. The concrete
//! [`repo::Libgit2Backend`] implements it over libgit2.
//!
//! # Module discipline
//!
//! `git2` is **confined to this crate**, the same rule `core::oplog` applies to
//! `rusqlite` and `hiker-sync` to `libp2p`. Only plain Rust types cross the
//! public boundary — `String` paths, [`meta::CommitInfo`], [`meta::Author`],
//! [`meta::Trailers`], [`repo::Divergence`]. No `git2::Repository` /
//! `git2::Oid` / `git2::Error` ever leaks past it; a consumer links `hiker-git`
//! and never transitively sees `git2` in its own surface. The trait makes the
//! backend swappable (libgit2 today, `gix` once its push/merge mature —
//! `git-backend-trait`).
//!
//! # What this crate does NOT do
//!
//! It carries no policy: debounce/coalesce timing, when to commit vs amend,
//! how an inbound divergence feeds the 3-way merge, the libp2p-vs-git
//! single-bidirectional rule — all of that lives above the seam (in the sync
//! orchestration). This crate is a thin, testable libgit2 verb surface.

pub mod meta;
pub mod repo;

/// Crate-wide error. Every fallible public surface returns this so a consumer
/// never matches on a `git2::Error` directly (module discipline).
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The repository could not be opened or initialized at the given path.
    #[error("git repository open/init failed: {0}")]
    Open(String),

    /// A commit / index / tree operation failed.
    #[error("git commit failed: {0}")]
    Commit(String),

    /// A fetch / pull from the remote failed (network, auth, or no such ref).
    #[error("git pull failed: {0}")]
    Pull(String),

    /// A push to the remote failed (network, auth, or rejected ref).
    #[error("git push failed: {0}")]
    Push(String),

    /// Reading history (`log` / `show`) failed.
    #[error("git read failed: {0}")]
    Read(String),

    /// No remote is configured but a push/pull was requested.
    #[error("no git remote configured")]
    NoRemote,

    /// A path argument was not a valid vault-relative UTF-8 path.
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// The repository diverged in a way the caller must resolve out-of-band
    /// (e.g. an inbound merge that contends with local) — surfaced so the
    /// orchestration feeds it to the 3-way merge rather than letting libgit2
    /// leave conflict markers. Carries the contending paths.
    #[error("merge requires resolution: {0:?}")]
    NeedsMerge(Vec<String>),
}

/// `Result` alias for the crate's public surface.
pub type Result<T> = std::result::Result<T, GitError>;
