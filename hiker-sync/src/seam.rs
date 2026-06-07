//! The pluggable transport seam (`sync.md` `sync-transport-seam`).
//!
//! Sync is reached behind one transport abstraction so the merge + conflict
//! logic above it is transport-agnostic. This module defines that seam as a
//! plain-Rust trait ([`Transport`]) plus the kind discriminant
//! ([`TransportKind`]) and the single-bidirectional rule
//! (`sync-single-bidirectional-transport`). The libp2p engine, the git engine,
//! and the no-op `none` engine each implement [`Transport`]; the orchestration
//! drives only these verbs.
//!
//! The trait carries no libp2p / git types — it is the neutral verb surface the
//! app's orchestration sits above. The concrete engines (the app's
//! `SyncService` for libp2p, the `git_sync` module for git) adapt to it.

/// Which transport carries cross-device sync this session. Mirrors
/// `core::config::sections::SyncTransport` but lives here so the seam doesn't
/// depend on `core::config` shapes; the app maps between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// libp2p file-blobs over the authenticated P2P / relay channel.
    Libp2p,
    /// Integrated or manual git (`git.md`).
    Git,
    /// Local-only; no cross-device sync.
    None,
}

impl TransportKind {
    /// Whether this transport moves content bidirectionally across devices.
    /// `none` is not bidirectional. Git is bidirectional only when it has a
    /// push/pull remote configured; the caller supplies that with
    /// [`is_bidirectional_with`](Self::is_bidirectional_with).
    #[must_use]
    pub const fn is_inherently_bidirectional(self) -> bool {
        matches!(self, TransportKind::Libp2p)
    }

    /// Whether this transport is a bidirectional cross-device sync given
    /// whether a remote / peer path is configured. libp2p is always
    /// bidirectional when selected; git is bidirectional only with a remote
    /// (commit-only local versioning is NOT a sync path —
    /// `sync-single-bidirectional-transport`); `none` never is.
    #[must_use]
    pub const fn is_bidirectional_with(self, has_remote: bool) -> bool {
        match self {
            TransportKind::Libp2p => true,
            TransportKind::Git => has_remote,
            TransportKind::None => false,
        }
    }
}

/// The single-bidirectional-transport rule (`sync-single-bidirectional-
/// transport`): no two bidirectional cross-device syncs may run at once. Given
/// the selected transport and whether a git remote is configured, returns
/// `Err(reason)` for an invalid combination so the caller can reject/ignore it
/// with a clear signal, or `Ok(())` for a legal one.
///
/// This is a *selection* check, not a runtime one: the config picks exactly one
/// `[sync].transport`, so the only way to get two bidirectional syncs is to
/// also enable a second engine out-of-band. The rule is stated here as the
/// single authority both the app's construction path and the docs reference.
///
/// `libp2p_also_enabled` is whether the libp2p engine is (or would be) running
/// alongside the selected transport. The legal combinations:
/// - libp2p alone — fine.
/// - git-as-sync (remote set) alone — fine.
/// - git-as-local-versioning (no remote) + libp2p — fine (git is a second local
///   history, not a second sync path).
/// - git-as-sync (remote set) + libp2p — REJECTED (two bidirectional syncs).
///
/// status: sync-single-bidirectional-transport
pub fn check_single_bidirectional(
    selected: TransportKind,
    git_has_remote: bool,
    libp2p_also_enabled: bool,
) -> Result<(), String> {
    let selected_is_sync = selected.is_bidirectional_with(git_has_remote);
    if selected == TransportKind::Git && selected_is_sync && libp2p_also_enabled {
        return Err(
            "git-as-sync (a push/pull remote is configured) is mutually exclusive with libp2p \
             sync — only one bidirectional cross-device transport may run at a time. Use git \
             as local-only versioning (clear [git].remote) to run it alongside libp2p, or set \
             [sync].transport to a single value."
                .to_string(),
        );
    }
    Ok(())
}

/// The verbs the sync orchestration drives, regardless of transport
/// (`sync-transport-seam`). The orchestration sits above this; merge + conflict
/// handling stays transport-agnostic. Each engine (libp2p / git / none)
/// implements it. Verbs are deliberately coarse — round-grained, not op-grained
/// — because the wire ships whole-file content, not ops.
///
/// All methods take `&self` (engines hold their mutable state behind interior
/// mutability / async locks) so the orchestration can call from the UI thread
/// without an exclusive borrow, matching the existing `SyncService` shape.
pub trait Transport: Send + Sync {
    /// Which transport this engine is.
    fn kind(&self) -> TransportKind;

    /// Whether this engine performs bidirectional cross-device sync right now
    /// (libp2p: yes; git: yes iff a remote is configured; none: no). Used to
    /// enforce the single-bidirectional rule and to decide whether a round does
    /// anything.
    fn is_bidirectional(&self) -> bool;

    /// A change was just committed (saved) locally — nudge the transport to
    /// propagate it. For libp2p this pokes enrolled peers; for integrated git
    /// it schedules a debounced commit-on-save (and a push if a remote is set);
    /// for none it is a no-op. Non-blocking; safe to call from the UI thread at
    /// every commit site. [git-commit-on-save, sync-poke-on-commit]
    fn notify_local_change(&self);

    /// Stop the engine (cancel its background tasks). Idempotent.
    fn shutdown(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libp2p_alone_is_legal() {
        assert!(check_single_bidirectional(TransportKind::Libp2p, false, false).is_ok());
    }

    #[test]
    fn git_with_remote_alone_is_legal() {
        // git-as-sync selected, no libp2p running alongside.
        assert!(check_single_bidirectional(TransportKind::Git, true, false).is_ok());
    }

    #[test]
    fn git_local_versioning_plus_libp2p_is_legal() {
        // git with NO remote is just a second local history, not a sync path —
        // it may run alongside libp2p.
        assert!(check_single_bidirectional(TransportKind::Git, false, true).is_ok());
    }

    #[test]
    fn git_as_sync_plus_libp2p_is_rejected() {
        // Two bidirectional cross-device syncs at once — the one illegal combo.
        let err = check_single_bidirectional(TransportKind::Git, true, true);
        assert!(err.is_err(), "git-as-sync + libp2p must be rejected");
        assert!(err.unwrap_err().contains("mutually exclusive"));
    }

    #[test]
    fn none_is_never_bidirectional() {
        assert!(!TransportKind::None.is_bidirectional_with(true));
        assert!(!TransportKind::None.is_bidirectional_with(false));
    }

    #[test]
    fn git_is_bidirectional_only_with_a_remote() {
        assert!(TransportKind::Git.is_bidirectional_with(true));
        assert!(!TransportKind::Git.is_bidirectional_with(false));
    }
}
