//! Vault-side spec governance for the vault graph (Phase E of the
//! graph-unification plan): the drift-badge data (`vault-graph-spec-drift-badge`)
//! and the spec → code-graph jump (the bridge-by-navigation rule — the two
//! graphs never merge node sets).
//!
//! The drift state comes from the same baseline the code graph's governance
//! overlay reads: each in-vault project repo's `links.json`
//! (`spec-engine`'s [`LinkStore`]) drift-checked through its bound SCIP
//! adapter, folded PER SPEC SLUG here (the code-side rollup folds per code
//! moniker). A vault note's badge is then the worst state across the
//! `[slug]` anchors it defines. Loading is on demand (the toolbar "Drift"
//! toggle / the first jump), cached on the panel — the same lazy posture as
//! the code graph's overlay, since drift-checking re-fingerprints every
//! linked body.

use std::collections::HashMap;

use hiker_code::governance::classify;
use hiker_code::GovState;
use spec_engine::{LinkStore, SourceId};

use crate::state::{AppState, ToastLevel};

/// The folded vault-side governance read: per-spec drift state plus the
/// repo each spec's links live in (the code-graph jump target). Cached on
/// the vault panel once loaded.
#[derive(Clone, Default)]
pub(crate) struct DriftStates {
    /// Folded drift per spec slug, across every project repo's `links.json`.
    pub spec_states: HashMap<String, GovState>,
    /// The repo_id whose `links.json` carries each spec's links (first
    /// writer wins when several repos name one slug). Known mismatch with
    /// `spec_states`, which keeps the cross-repo WORST state: a jump may
    /// land in a clean repo while another repo holds the drift.
    pub repo_of: HashMap<String, String>,
}

/// Fold per-link drift reports `(spec, drifted, missing)` into one state
/// per spec slug — the spec-side twin of the code overlay's per-moniker
/// fold, reusing its severity rule (`Missing` > `Drifted` > `Ok`).
/// status: vault-graph-spec-drift-badge
pub(crate) fn fold_spec_states(
    reports: impl IntoIterator<Item = (String, bool, bool)>,
) -> HashMap<String, GovState> {
    let mut per_spec: HashMap<String, Vec<(bool, bool)>> = HashMap::new();
    for (spec, drifted, missing) in reports {
        per_spec.entry(spec).or_default().push((drifted, missing));
    }
    per_spec.into_iter().map(|(s, v)| (s, classify(v))).collect()
}

/// A note's badge state: the worst drift state across the anchors it
/// defines, `None` when none of them is governed by any link store (an
/// ungoverned spec note wears no badge — absence of links is not a state).
/// status: vault-graph-spec-drift-badge
pub(crate) fn note_state(
    slugs: &[String],
    states: &HashMap<String, GovState>,
) -> Option<GovState> {
    slugs.iter().filter_map(|s| states.get(s)).copied().max()
}

/// Fold the per-note badge map for a built graph: every note defining
/// anchors, mapped through [`note_state`] — notes whose anchors carry no
/// links drop out (no badge). Recomputed on rebuild and on badge enable.
/// status: vault-graph-spec-drift-badge
pub(crate) fn note_badges(
    data: &crate::panels::graph_data::VaultData,
    drift: &DriftStates,
) -> HashMap<String, GovState> {
    data.anchors_by_note
        .iter()
        .filter_map(|(path, slugs)| {
            note_state(slugs, &drift.spec_states).map(|s| (path.clone(), s))
        })
        .collect()
}

/// Load the vault-side rollup: every project repo declared in the vault
/// whose root carries a `links.json` contributes its drift reports (bound
/// lazily through the shared `code_sources` registry — the expensive SCIP
/// load is cached there). A vault with no project repos, or none with a
/// baseline, folds to an empty map — no badges, honestly.
pub(crate) fn load(app: &mut AppState) -> DriftStates {
    let repos = crate::code_sources::list_repos(
        &app.vault_session.vault,
        &app.vault_session.services.read_store,
    );
    let mut out = DriftStates::default();
    for (repo_id, _) in repos {
        let Some((adapter, _note)) = crate::code_sources::resolve_or_bind(app, &repo_id) else {
            continue;
        };
        let links_path = adapter.repo_root().join("links.json");
        if !links_path.exists() {
            continue;
        }
        let store = match LinkStore::load(&links_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, repo = %repo_id,
                    "vault graph: links.json unreadable; repo skipped from drift badges");
                continue;
            }
        };
        let src = SourceId(repo_id.clone());
        let reports = store
            .check_drift(&src, adapter.as_ref())
            .into_iter()
            .map(|r| (r.spec, r.drifted, r.missing));
        for (spec, state) in fold_spec_states(reports) {
            // Cross-repo merge keeps the worst state per spec.
            let slot = out.spec_states.entry(spec.clone()).or_insert(state);
            *slot = (*slot).max(state);
            out.repo_of.entry(spec).or_insert_with(|| repo_id.clone());
        }
    }
    out
}

/// Jump from a vault spec note to the CODE graph with `slug` preselected
/// (lit): resolve the repo whose baseline carries the spec, open/focus that
/// project's code-graph tab, and light the spec there — bridging the two
/// graphs by navigation, never by merging node sets. Loads the drift
/// rollup on demand when the badge toggle hasn't already.
/// status: vault-graph-spec-drift-badge
pub(crate) fn jump_to_spec(app: &mut AppState, slug: &str) {
    let drift = match app.panels.graph.as_ref().and_then(|vg| vg.drift.clone()) {
        Some(d) => d,
        None => {
            let loaded = load(app);
            if let Some(vg) = app.panels.graph.as_mut() {
                vg.drift = Some(loaded.clone());
            }
            loaded
        }
    };
    let Some(repo_id) = drift.repo_of.get(slug) else {
        app.push_toast(
            format!("spec {slug} has no code links in any project repo's links.json"),
            ToastLevel::Warn,
        );
        return;
    };
    let Some((_adapter, note)) = crate::code_sources::resolve_or_bind(app, repo_id) else {
        app.push_toast(format!("no project binds repo '{repo_id}'"), ToastLevel::Warn);
        return;
    };
    let source = crate::tab::CodeSource::Project(note);
    crate::panels::code_graph::open(app, source.clone());
    crate::panels::code_graph::select_spec(app, &source.key(), slug);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-spec fold mirrors the code overlay's severity rule: a spec
    /// with one missing link is Missing even when its other links are
    /// clean; clean-only specs are Ok; specs absent from the reports are
    /// absent from the map (ungoverned).
    #[test]
    fn fold_spec_states_keeps_the_worst_link_per_spec() {
        let states = fold_spec_states([
            ("s-ok".to_string(), false, false),
            ("s-hot".to_string(), false, false),
            ("s-hot".to_string(), true, false),
            ("s-gone".to_string(), true, false),
            ("s-gone".to_string(), false, true),
        ]);
        assert_eq!(states.get("s-ok"), Some(&GovState::Ok));
        assert_eq!(states.get("s-hot"), Some(&GovState::Drifted));
        assert_eq!(states.get("s-gone"), Some(&GovState::Missing));
        assert!(!states.contains_key("s-never"));
    }

    /// A note's badge is the worst state across ITS anchors; a note none
    /// of whose anchors carries links wears no badge at all.
    #[test]
    fn note_state_folds_across_the_notes_anchors() {
        let states: HashMap<String, GovState> = [
            ("a-one".to_string(), GovState::Ok),
            ("a-two".to_string(), GovState::Drifted),
        ]
        .into_iter()
        .collect();
        let slug = |s: &str| s.to_string();
        assert_eq!(note_state(&[slug("a-one")], &states), Some(GovState::Ok));
        assert_eq!(
            note_state(&[slug("a-one"), slug("a-two")], &states),
            Some(GovState::Drifted),
            "worst anchor wins"
        );
        assert_eq!(
            note_state(&[slug("a-one"), slug("ungoverned-x")], &states),
            Some(GovState::Ok),
            "ungoverned anchors don't drag the fold"
        );
        assert_eq!(note_state(&[slug("ungoverned-x")], &states), None);
        assert_eq!(note_state(&[], &states), None);
    }
}
