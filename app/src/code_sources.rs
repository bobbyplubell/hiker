//! Repo-id → `ScipAdapter` registry for spec→code wikilinks (`spec-code-link`, Phase A).
//!
//! A `[[code:<repo_id>/<symbol>]]` wikilink names a repo by its portable, git-derived `repo_id`
//! (not a note path). To resolve the symbol through the code-intelligence port we must first find
//! the project note (`hiker.kind: project`) that *declares* that repo_id and bind its `.scip`
//! index to a [`ScipAdapter`]. This registry caches that binding so a hot link doesn't re-load the
//! index every click.
//!
//! Decoupling note: the registry owns its own `Arc<ScipAdapter>`, independent of the per-tab
//! code-graph view (`panels::code_graph::View`). A future cleanup can share one adapter
//! between the two; for Phase A a double-load on first navigation is acceptable. The registry also
//! records the project-note path per repo_id so navigation can open/focus the right graph tab.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hiker_code::ScipAdapter;
use hiker_core::store::dto::{MetaFilter, NoteQuery};
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_projects::{repo::Backend, Project};

use crate::state::AppState;

/// Lazily-populated map of `repo_id` → its bound SCIP adapter, plus the project note that declares
/// each repo (so navigation opens the matching graph tab). Default-constructed on `AppState`.
#[derive(Default)]
pub struct Registry {
    /// repo_id → the loaded adapter (shared; the per-tab view loads its own independently for now).
    bound: HashMap<String, Arc<ScipAdapter>>,
    /// repo_id → the vault-relative project-note path that declared it.
    note_of: HashMap<String, String>,
}

/// Build a [`CodeCompletionProvider`] from `app`'s vault session — the `[[code:` authoring
/// autocomplete helper handed to editor buffers at open. Cheap (clones two `Arc` handles).
/// status: spec-code-link
#[must_use]
pub fn completion_provider(app: &AppState) -> Arc<CodeCompletionProvider> {
    Arc::new(CodeCompletionProvider::new(
        app.vault_session.vault.clone(),
        app.vault_session.services.read_store.clone(),
    ))
}

/// Resolve `repo_id` to its bound adapter (+ the project note that declares it), binding lazily on
/// first use. Returns `None` when no project note in the vault declares that repo_id, or when
/// binding its index fails (a missing/unreadable `.scip`, an out-of-vault path, a non-SCIP backend).
///
/// A free function (not a method) so it can borrow `app.code_sources` mutably while reading the
/// disjoint `app.vault_session` fields (the store + vault). status: spec-code-link
pub fn resolve_or_bind(app: &mut AppState, repo_id: &str) -> Option<(Arc<ScipAdapter>, String)> {
    if let (Some(adapter), Some(note)) =
        (app.code_sources.bound.get(repo_id), app.code_sources.note_of.get(repo_id))
    {
        return Some((adapter.clone(), note.clone()));
    }
    let (adapter, note) = bind(app, repo_id)?;
    let adapter = Arc::new(adapter);
    app.code_sources.bound.insert(repo_id.to_string(), adapter.clone());
    app.code_sources.note_of.insert(repo_id.to_string(), note.clone());
    Some((adapter, note))
}

/// Discover + load the adapter for `repo_id`: scan the vault's project notes, parse each, find the
/// `repo` source whose `repo_id` matches, vault-clamp its index/root, and `ScipAdapter::load`.
/// Borrows only the disjoint `vault_session` fields, so the caller can hold `&mut app.code_sources`.
fn bind(app: &AppState, repo_id: &str) -> Option<(ScipAdapter, String)> {
    bind_in(&app.vault_session.vault, &app.vault_session.services.read_store, repo_id)
}

/// Discover + load the adapter for `repo_id` from the raw `(vault, store)` handles — the engine
/// behind both [`resolve_or_bind`] (navigation) and [`CodeCompletionProvider`] (authoring
/// autocomplete). Loading a `.scip` is the only expensive step; callers cache the result.
fn bind_in(vault: &Arc<Vault>, store: &Arc<Mutex<Store>>, repo_id: &str) -> Option<(ScipAdapter, String)> {
    let vault_root = vault.root();
    for note in project_notes_in(store) {
        let Ok(text) = vault.read_file(&note) else { continue };
        let Ok(project) = Project::parse(&text, std::path::Path::new(&note)) else { continue };
        let Some(repo) = project.repo_sources().find(|r| r.repo_id == repo_id) else { continue };
        if repo.backend != Backend::Scip {
            continue;
        }
        let index = crate::panels::code_graph::resolve_in_vault(vault_root, &repo.index);
        let root = crate::panels::code_graph::resolve_in_vault(vault_root, &repo.root);
        if crate::panels::code_graph::require_in_vault(vault_root, &index).is_err()
            || crate::panels::code_graph::require_in_vault(vault_root, &root).is_err()
        {
            continue;
        }
        let src = spec_engine::SourceId(repo.repo_id.clone());
        if let Ok(adapter) = ScipAdapter::load(&index, &root, src) {
            return Some((adapter, note));
        }
    }
    None
}

/// Every `hiker.kind: project` note's vault-relative path, via the store's frontmatter index — the
/// same `NoteQuery`/`MetaFilter` discovery `projects_activity::list_projects` uses.
fn project_notes_in(store: &Arc<Mutex<Store>>) -> Vec<String> {
    let Ok(store) = store.lock() else { return Vec::new() };
    let query = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "hiker.kind".to_string(),
            values: vec!["project".to_string()],
        }],
        ..Default::default()
    };
    store
        .query_notes(&query)
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.path)
        .collect()
}

/// Every SCIP-backed repo declared by a project note, as `(repo_id, label)`. The label is the
/// repo's vault-relative root path (what the autocomplete `detail` shows). Cheap: it only parses
/// the project notes' frontmatter — no `.scip` index is loaded. Used by the `[[code:` repo-stage
/// autocomplete. status: spec-code-link
#[must_use]
pub fn list_repos(vault: &Arc<Vault>, store: &Arc<Mutex<Store>>) -> Vec<(String, String)> {
    let vault_root = vault.root();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for note in project_notes_in(store) {
        let Ok(text) = vault.read_file(&note) else { continue };
        let Ok(project) = Project::parse(&text, std::path::Path::new(&note)) else { continue };
        for repo in project.repo_sources() {
            if repo.backend != Backend::Scip {
                continue;
            }
            if !seen.insert(repo.repo_id.clone()) {
                continue;
            }
            let root = crate::panels::code_graph::resolve_in_vault(vault_root, &repo.root);
            let label = root
                .strip_prefix(vault_root)
                .unwrap_or(&root)
                .to_string_lossy()
                .into_owned();
            out.push((repo.repo_id.clone(), label));
        }
    }
    out.sort();
    out
}

/// Self-contained authoring helper for `[[code:repo_id/symbol]]` wikilink autocomplete. Holds the
/// two `Arc` handles (`Vault` + read `Store`) the editor completion source needs and caches bound
/// adapters so repeated keystrokes against the same repo don't re-load its `.scip`.
///
/// Lives apart from [`Registry`] (the navigation cache on `AppState`) because the completion source
/// runs inside the editor command pipeline with no `&mut AppState` in hand: it must own everything
/// it touches. Repo listing is cheap (frontmatter scan); adapter binding is lazy — only triggered
/// once the user has typed `code:<repo_id>/` (committed to a repo). status: spec-code-link
pub struct CodeCompletionProvider {
    vault: Arc<Vault>,
    store: Arc<Mutex<Store>>,
    /// repo_id → bound adapter (`None` = bind attempted and failed; cached so we don't retry the
    /// expensive load every keystroke).
    cache: Mutex<HashMap<String, Option<Arc<ScipAdapter>>>>,
}

impl CodeCompletionProvider {
    /// A provider over the given vault + read-store handles. Cheap (clones two `Arc`s).
    #[must_use]
    pub fn new(vault: Arc<Vault>, store: Arc<Mutex<Store>>) -> Self {
        Self { vault, store, cache: Mutex::new(HashMap::new()) }
    }

    /// SCIP repos declared by project notes, as `(repo_id, label)`. Cheap frontmatter scan.
    #[must_use]
    pub fn repos(&self) -> Vec<(String, String)> {
        list_repos(&self.vault, &self.store)
    }

    /// The bound adapter for `repo_id`, loading + caching its `.scip` on first use. Returns `None`
    /// when no project note declares the repo or its index fails to load (cached so we don't retry).
    /// EXPENSIVE on a cache miss — only call once the user has committed to a repo.
    #[must_use]
    pub fn adapter(&self, repo_id: &str) -> Option<Arc<ScipAdapter>> {
        if let Ok(cache) = self.cache.lock() {
            if let Some(hit) = cache.get(repo_id) {
                return hit.clone();
            }
        }
        let bound = bind_in(&self.vault, &self.store, repo_id).map(|(a, _note)| Arc::new(a));
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(repo_id.to_string(), bound.clone());
        }
        bound
    }
}
