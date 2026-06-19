//! Composed ignore matcher for the vault — Phase B of "code as read-only
//! reference content" (see `CODE-IN-VAULT.md` decision #4).
//!
//! The indexer walk, the watcher's watch-registration walk, and
//! `vault::list_dir` all need to skip build/vendor noise so a vault that
//! contains code (up to whole repos) isn't drowned in `target/`,
//! `node_modules/`, etc. Historically that was a single hard-coded list in
//! `watcher::is_ignored`. This module layers, on top of that cheap fast-path:
//!
//!   1. the existing hard-coded list (`watcher::is_ignored`) — fast-path for
//!      `target/`, `.git/`, `.hiker/`, editor temp files, …;
//!   2. a vault-root `.gitignore` (full ripgrep semantics: nesting, negation
//!      `!`, anchoring) via the `ignore` crate;
//!   3. a vault-root `.hikerignore` (same semantics, hiker-specific extras);
//!   4. `IndexingConfig.ignored_paths` from settings (vault-root-relative
//!      gitignore-style patterns), registered at `Config::load`.
//!
//! THE NOTE-PROTECTION INVARIANT (decision #4): `.md`/`.markdown` notes are
//! the authored content and must NEVER be gitignored away from indexing. So
//! layers 2–4 only ever exclude NON-note files; a markdown note can only be
//! excluded by the hard-coded `.git/`/`.hiker/` internals in layer 1. This is
//! enforced explicitly in [`Matcher::is_ignored`].
//!
//! The composed matcher is built once per vault (lazily, keyed by canonical
//! vault root, in a process-global registry) so the three consult sites can
//! reach it without threading new state through every call site. The legacy
//! free function `watcher::is_ignored` is left untouched for the many other
//! callers that only need the hard-coded list.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Filename of the vault-root hiker-specific ignore file.
const HIKERIGNORE: &str = ".hikerignore";
/// Filename of the vault-root git ignore file.
const GITIGNORE: &str = ".gitignore";

/// A composed, per-vault ignore matcher. Cheap to clone-by-`Arc`; the inner
/// `Gitignore` is immutable once built.
#[derive(Debug)]
pub struct Matcher {
    /// Composed gitignore + hikerignore + config patterns, rooted at the
    /// vault root. `None` when no ignore source contributed any pattern
    /// (so the hard-coded list is the only layer).
    gitignore: Option<Gitignore>,
}

impl Matcher {
    /// Build the matcher for `vault_root`, reading `.gitignore` and
    /// `.hikerignore` from the root and folding in `ignored_paths` config
    /// patterns. Missing ignore files are simply skipped (not an error).
    pub fn build(vault_root: &Path, ignored_paths: &[String]) -> Self {
        let mut builder = GitignoreBuilder::new(vault_root);
        let mut added_any = false;

        // `.gitignore` first, then `.hikerignore` (later sources win on
        // conflict via gitignore's last-match-wins, so hiker extras can
        // negate a project's .gitignore if needed).
        for file in [GITIGNORE, HIKERIGNORE] {
            let path = vault_root.join(file);
            if path.is_file() {
                // `add` returns Some(err) on a partial/parse problem but
                // still adds what it could; we keep going so one bad line
                // doesn't disable ignoring entirely.
                if let Some(err) = builder.add(&path) {
                    tracing::warn!(file = %path.display(), error = %err, "ignore: partial parse of ignore file");
                }
                added_any = true;
            }
        }

        // Config patterns (`[indexing] ignored_paths`). Each is a
        // gitignore-style line, matched relative to the vault root.
        for pat in ignored_paths {
            let pat = pat.trim();
            if pat.is_empty() {
                continue;
            }
            match builder.add_line(None, pat) {
                Ok(_) => added_any = true,
                Err(err) => {
                    tracing::warn!(pattern = %pat, error = %err, "ignore: bad ignored_paths pattern");
                }
            }
        }

        let gitignore = if added_any {
            match builder.build() {
                Ok(gi) => Some(gi),
                Err(err) => {
                    tracing::warn!(root = %vault_root.display(), error = %err, "ignore: failed to build matcher");
                    None
                }
            }
        } else {
            None
        };

        Self { gitignore }
    }

    /// An empty matcher: only the hard-coded list applies. Used as a safe
    /// fallback and for vaults with no ignore files / config patterns.
    pub const fn empty() -> Self {
        Self { gitignore: None }
    }

    /// Whether `rel` (vault-relative, forward-slash) should be excluded from
    /// indexing / the file tree. `is_dir` should be true for directories so
    /// gitignore directory semantics (`foo/`) apply.
    ///
    /// Layering: the hard-coded list always wins first (so `.git/`,
    /// `.hiker/`, `target/`, editor temp files are excluded regardless of
    /// ignore files). Then — and ONLY for non-note files — the composed
    /// gitignore/.hikerignore/config layer is consulted. A markdown note is
    /// never excluded by that layer (the note-protection invariant).
    pub fn is_ignored(&self, rel: &str, is_dir: bool) -> bool {
        // Layer 1: hard-coded list (cheap fast-path). This also covers the
        // `.git/`/`.hiker/` internals that protect markdown notes too.
        if crate::watcher::is_ignored(rel) {
            return true;
        }
        // Note-protection invariant: a markdown note is the authored content
        // and is NEVER excluded by the gitignore/.hikerignore/config layer.
        if !is_dir && is_note(rel) {
            return false;
        }
        // Layers 2–4: composed gitignore matcher.
        let Some(gi) = &self.gitignore else {
            return false;
        };
        // `matched_path_or_any_parents` walks the path up to the root, so a
        // file under an ignored directory is caught even when the caller
        // hands us a leaf path (e.g. `vault::list_dir`). It expects a
        // root-relative path, which `rel` already is.
        gi.matched_path_or_any_parents(rel, is_dir).is_ignore()
    }
}

/// Whether a vault-relative path is a markdown note (the authored,
/// never-gitignored content). Matches the indexer's note extensions
/// (`md`/`markdown`); `txt` is indexable but is treated as reference content
/// for ignore purposes — only true notes get the protection invariant.
fn is_note(rel: &str) -> bool {
    let basename = rel.rsplit('/').next().unwrap_or(rel);
    let Some(dot) = basename.rfind('.') else {
        return false;
    };
    let ext = &basename[dot + 1..];
    ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
}

/// Process-global registry of per-vault matchers, keyed by canonical vault
/// root. Built lazily on first consult; `register` (from `Config::load`)
/// refreshes it with the resolved `ignored_paths` config.
static REGISTRY: RwLock<Option<HashMap<PathBuf, Arc<Matcher>>>> = RwLock::new(None);

/// Canonicalize a vault root for use as a registry key. Falls back to the
/// path as-given if canonicalization fails (e.g. the dir was removed), so we
/// never panic on a transient FS state.
fn key_for(vault_root: &Path) -> PathBuf {
    vault_root.canonicalize().unwrap_or_else(|_| vault_root.to_path_buf())
}

/// Register (or refresh) the matcher for `vault_root`, folding in the
/// `ignored_paths` from `[indexing]`. Called from `Config::load` so the
/// config layer is wired in as soon as settings are read. Idempotent;
/// last write wins (so editing `ignored_paths` + reloading config updates
/// the live matcher).
pub fn register(vault_root: &Path, ignored_paths: &[String]) {
    let matcher = Arc::new(Matcher::build(vault_root, ignored_paths));
    let key = key_for(vault_root);
    let mut guard = REGISTRY.write().expect("ignore registry poisoned");
    guard.get_or_insert_with(HashMap::new).insert(key, matcher);
}

/// Get the matcher for `vault_root`, building it from disk ignore files (no
/// config patterns) if none was registered yet. The lazy build means the
/// consult sites work even before `Config::load` runs; once config loads,
/// `register` swaps in the fuller matcher.
pub fn matcher_for(vault_root: &Path) -> Arc<Matcher> {
    let key = key_for(vault_root);
    if let Some(map) = REGISTRY.read().expect("ignore registry poisoned").as_ref()
        && let Some(m) = map.get(&key)
    {
        return m.clone();
    }
    // Lazy build (no config patterns — those arrive via `register`).
    let matcher = Arc::new(Matcher::build(vault_root, &[]));
    let mut guard = REGISTRY.write().expect("ignore registry poisoned");
    guard
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| matcher.clone())
        .clone()
}

/// Convenience consult helper used by the indexer walk, the watcher route,
/// and `vault::list_dir`: resolve the vault's matcher and test `rel`.
pub fn is_ignored_in(vault_root: &Path, rel: &str, is_dir: bool) -> bool {
    matcher_for(vault_root).is_ignored(rel, is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn gitignored_code_excluded_note_protected_hardlist_excluded() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // A .gitignore that would exclude a code file AND a markdown note.
        write(root, ".gitignore", "secret.rs\nnotes-private.md\nlogs/\n");
        let m = Matcher::build(root, &[]);

        // (a) a gitignored CODE path is excluded.
        assert!(m.is_ignored("secret.rs", false), "gitignored code excluded");
        assert!(
            m.is_ignored("logs/app.log", false),
            "file under gitignored dir excluded"
        );

        // (b) a gitignored .md NOTE is NOT excluded (note-protection).
        assert!(
            !m.is_ignored("notes-private.md", false),
            "gitignored markdown note must still index"
        );

        // (c) a hard-listed `target/` path is excluded regardless.
        assert!(m.is_ignored("target/debug/foo", false), "hard-listed target/ excluded");
        assert!(m.is_ignored("crate/target", true), "nested target dir excluded");

        // A normal, non-ignored code file is kept.
        assert!(!m.is_ignored("src/main.rs", false), "unignored code kept");
    }

    #[test]
    fn hikerignore_layer_applies() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, ".hikerignore", "*.bin\nvendor/\n");
        let m = Matcher::build(root, &[]);
        assert!(m.is_ignored("blob.bin", false), ".hikerignore glob excludes");
        assert!(m.is_ignored("vendor/lib.js", false), ".hikerignore dir excludes");
        // Note protection still holds against .hikerignore.
        write(root, ".hikerignore", "*.bin\nREADME.md\n");
        let m = Matcher::build(root, &[]);
        assert!(!m.is_ignored("README.md", false), "note protected from .hikerignore");
    }

    #[test]
    fn config_ignored_paths_layer_applies() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let m = Matcher::build(root, &["generated/".to_string(), "*.snap".to_string()]);
        assert!(m.is_ignored("generated/out.rs", false), "config dir pattern excludes");
        assert!(m.is_ignored("test.snap", false), "config glob excludes");
        assert!(!m.is_ignored("keep.rs", false), "unmatched kept");
        // And still protects notes.
        let m = Matcher::build(root, &["*.md".to_string()]);
        assert!(!m.is_ignored("drafts/x.md", false), "config never excludes notes");
    }

    #[test]
    fn negation_pattern_supported() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // `generated/` is NOT in the hard-coded list, so the gitignore
        // negation layer can override it (a hard-listed dir like `build/`
        // could not be un-ignored — layer 1 always wins).
        write(root, ".gitignore", "generated/\n!generated/keep.js\n");
        let m = Matcher::build(root, &[]);
        assert!(m.is_ignored("generated/bundle.js", false), "generated/ excluded");
        assert!(!m.is_ignored("generated/keep.js", false), "negated path kept");
    }

    /// The CODE-IN-VAULT case: a nested repo can be excluded wholesale
    /// *except* its docs, using gitignore allowlist semantics — `dir/*`
    /// (exclude the children, NOT `dir/**` or `dir/` which would block
    /// re-inclusion) plus a `!dir/keep/` negation. This prunes the repo's
    /// non-doc subtrees (so the watcher doesn't watch them and the indexer
    /// doesn't descend them) while keeping the docs indexed — both ends
    /// satisfied without a separate allowlist feature. Verified at file AND
    /// directory granularity (the watcher consults `is_dir = true`).
    #[test]
    fn nested_repo_allowlist_keeps_docs_excludes_rest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        // "ignore the hiker/ repo but keep hiker/docs/" — the allowlist form.
        write(root, ".hikerignore", "hiker/*\n!hiker/docs/\n");
        let m = Matcher::build(root, &[]);

        // docs/ is kept — at the dir level (watcher descends/watches it) …
        assert!(!m.is_ignored("hiker/docs", true), "docs dir watched");
        // … and at the file level (indexer ingests its specs).
        assert!(!m.is_ignored("hiker/docs/trails.md", false), "spec note indexed");
        assert!(!m.is_ignored("hiker/docs/notes.txt", false), "doc txt kept");

        // The rest of the repo is excluded — dir level (pruned, not watched) …
        assert!(m.is_ignored("hiker/src", true), "src dir pruned");
        assert!(m.is_ignored("hiker/core", true), "core dir pruned");
        // … and file level (a stray file under an excluded sibling).
        assert!(m.is_ignored("hiker/src/main.rs", false), "src file excluded");
    }

    #[test]
    fn empty_matcher_only_hardlist() {
        let m = Matcher::empty();
        assert!(m.is_ignored("node_modules/x.js", false), "hard list still applies");
        assert!(!m.is_ignored("src/main.rs", false), "nothing else excluded");
    }

    #[test]
    fn registry_register_and_consult() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(root, ".gitignore", "dist/\n");
        register(root, &["*.tmp".to_string()]);
        assert!(is_ignored_in(root, "dist/app.js", false), "registered gitignore consulted");
        assert!(is_ignored_in(root, "scratch.tmp", false), "registered config consulted");
        assert!(!is_ignored_in(root, "doc.md", false), "note protected via registry");
    }
}
