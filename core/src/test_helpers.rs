//! Shared test fixtures. Wired in from `lib.rs` under `#[cfg(test)]` so
//! production code can't accidentally depend on it.
//!
//! Each helper returns the `TempDir` first so callers can keep it bound
//! (the dir is cleaned up when the binding drops — losing it mid-test
//! deletes the vault out from under you).

#![cfg(test)]
#![allow(dead_code)]

use tempfile::TempDir;

use crate::staging::Staging;
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

/// A vault + a fresh `Staging` opened against it. Mirrors the inline
/// `staged()` helper that used to live in `staging/tests.rs`.
pub(crate) fn test_vault_with_staging() -> (TempDir, Vault, Staging) {
    let (dir, vault) = test_vault();
    let staging = Staging::open(vault.root()).expect("staging open");
    (dir, vault, staging)
}

/// Just a `Staging` on a tempdir. Most staging tests don't touch the
/// vault directly — they only care about the staging surface.
pub(crate) fn test_staging() -> (TempDir, Staging) {
    let dir = tempfile::tempdir().expect("tempdir");
    let staging = Staging::open(dir.path()).expect("staging open");
    (dir, staging)
}

/// Just a `Store` on a tempdir, for tests that exercise the store
/// without going through `Vault`.
pub(crate) fn test_store() -> (TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path()).expect("store open");
    (dir, store)
}
