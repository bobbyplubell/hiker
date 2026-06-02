//! App-side autocomplete: candidate sources built on the shared ranking
//! core in `editor_view::autocomplete`. The ranking + the in-buffer
//! `CompletionItem`/`CompletionState`/`CompletionSource` contract live in
//! `editor-view`; this module supplies the vault candidate enumeration that
//! both the in-buffer wikilink source and the standalone pickers share.
//!
//! status: autocomplete-vault-source

pub mod vault_source;
