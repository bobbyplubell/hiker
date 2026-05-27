//! Shared test fixtures. Wired in from `lib.rs` under `#[cfg(test)]` so
//! production code can't accidentally depend on it.
//!
//! Each helper returns the `TempDir` first so callers can keep it bound
//! (the dir is cleaned up when the binding drops — losing it mid-test
//! deletes the vault out from under you).

#![cfg(test)]
#![allow(dead_code)]

use tempfile::TempDir;

use crate::store::Store;
use crate::vault::Vault;

/// A fresh vault on a tempdir. The `TempDir` is held alongside so the
/// directory survives until the tuple is dropped — callers must keep
/// both halves alive (or let-bind the tuple, not just the `Vault`).
pub(crate) fn test_vault() -> (TempDir, Vault) {
    let dir = tempfile::tempdir().expect("tempdir");
    let vault = Vault::open(dir.path()).expect("vault open");
    (dir, vault)
}

/// A vault pre-seeded with `(rel, contents)` notes. Useful for indexer /
/// trails / cluster tests that need notes on disk before opening other
/// handles.
pub(crate) fn test_vault_with_notes(notes: &[(&str, &str)]) -> (TempDir, Vault) {
    let (dir, vault) = test_vault();
    for (rel, contents) in notes {
        vault.write_file(rel, contents).expect("seed write");
    }
    (dir, vault)
}

/// A vault + a fresh `Store` opened against it (writer connection).
pub(crate) fn test_vault_with_store() -> (TempDir, Vault, Store) {
    let (dir, vault) = test_vault();
    let store = Store::open(vault.root()).expect("store open");
    (dir, vault, store)
}

/// Just a `Store` on a tempdir, for tests that exercise the store
/// without going through `Vault`.
pub(crate) fn test_store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("store open");
    (dir, store)
}
