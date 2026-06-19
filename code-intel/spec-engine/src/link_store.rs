//! Authored spec→entity links + drift (`link-store`, `drift-fingerprint`).
//!
//! Links live on the authored (spec) side, version-controlled. Each stores the target's
//! **fingerprint at link time**; drift = recompute via the provider and compare. The store is
//! backend-agnostic — it only talks to [`DerivedNodeSource`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{DerivedNodeSource, NodeHandle, Resolution, SourceId};

/// One authored edge from a spec (or any authored node) to a derived node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub spec: String,        // authored node id (e.g. a spec slug)
    pub relation: String,    // "implements" | "touches" | …
    pub source: String,      // SourceId.0 (the repo id)
    pub target: String,      // NodeHandle.id (the symbol moniker)
    pub fingerprint: String, // target fingerprint captured at link time, at `resolution`
    /// C4 resolution this link drifts at (`spec-resolution-c4`). Defaults to `Code` for links
    /// authored before resolution existed.
    #[serde(default)]
    pub resolution: Resolution,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LinkStore {
    pub links: Vec<Link>,
}

/// What [`LinkStore::add_link`] did. The store is merge-preserving: re-adding an existing edge
/// never moves its baseline, so seeding/reconciling can re-run without resetting drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// New edge stored, baseline fingerprint captured now.
    Added,
    /// Edge already present — its stored (verified-at) fingerprint was kept untouched.
    Existing,
    /// Edge already present but its declared resolution changed (a deliberate spec edit) — the
    /// link was moved to the new altitude and its baseline re-captured there.
    Rescoped,
    /// The target wouldn't fingerprint — refused, nothing stored. An empty baseline would
    /// silently report DRIFTED forever once the target becomes fingerprintable.
    NoFingerprint,
}

#[derive(Debug)]
pub struct DriftReport {
    pub spec: String,
    pub target: String,
    pub stored: String,
    pub current: Option<String>,
    /// target changed since link time.
    pub drifted: bool,
    /// target no longer resolvable in the current index.
    pub missing: bool,
}

impl LinkStore {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomic: write a sibling temp file, then rename over `path`. The store is the durable drift
    /// baseline — a crash mid-write must not truncate it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }

    /// Author a link at a C4 `resolution`, capturing the target's fingerprint *at that resolution*.
    ///
    /// Upsert semantics on `(spec, relation, source, target)`: an existing edge keeps its stored
    /// baseline (re-baselining is [`Self::rebaseline`], a deliberate act — not a side effect of
    /// re-seeding). An unfingerprintable target is refused.
    pub fn add_link(
        &mut self,
        spec: &str,
        relation: &str,
        handle: &NodeHandle,
        resolution: Resolution,
        provider: &dyn DerivedNodeSource,
    ) -> AddOutcome {
        let existing = self.links.iter_mut().find(|l| {
            l.spec == spec
                && l.relation == relation
                && l.source == handle.source.0
                && l.target == handle.id
        });
        if let Some(l) = existing {
            if l.resolution == resolution {
                return AddOutcome::Existing;
            }
            // Resolution changed (frontmatter edit): re-pin at the new altitude. The old baseline
            // is meaningless at a different grain, so this is the one add path that recaptures.
            let Some(fingerprint) = provider.fingerprint_at(handle, resolution).map(|f| f.0) else {
                return AddOutcome::NoFingerprint; // keep the old link intact
            };
            l.resolution = resolution;
            l.fingerprint = fingerprint;
            return AddOutcome::Rescoped;
        }
        let Some(fingerprint) = provider.fingerprint_at(handle, resolution).map(|f| f.0) else {
            return AddOutcome::NoFingerprint;
        };
        self.links.push(Link {
            spec: spec.to_string(),
            relation: relation.to_string(),
            source: handle.source.0.clone(),
            target: handle.id.clone(),
            fingerprint,
            resolution,
        });
        AddOutcome::Added
    }

    /// Re-capture baselines — "I re-verified this spec." Recomputes the fingerprint of every link
    /// of `spec` (every link in `source` when `None`) and overwrites the stored one. Links whose
    /// target no longer resolves are left untouched (they stay MISSING in drift, which is the
    /// signal the user must act on). Returns how many baselines moved.
    pub fn rebaseline(
        &mut self,
        spec: Option<&str>,
        source: &SourceId,
        provider: &dyn DerivedNodeSource,
    ) -> usize {
        let mut updated = 0;
        for l in &mut self.links {
            if l.source != source.0 || spec.is_some_and(|s| s != l.spec) {
                continue;
            }
            let handle = NodeHandle { source: source.clone(), id: l.target.clone() };
            if let Some(current) = provider.fingerprint_at(&handle, l.resolution) {
                if current.0 != l.fingerprint {
                    l.fingerprint = current.0;
                    updated += 1;
                }
            }
        }
        updated
    }

    pub fn for_spec<'a>(&'a self, spec: &'a str) -> impl Iterator<Item = &'a Link> {
        self.links.iter().filter(move |l| l.spec == spec)
    }

    /// Drop every link of `source` whose `(spec, relation, target)` is not in `keep` — the prune
    /// half of reconcile (markdown owns the edges; the store mirrors them). Returns pruned count.
    pub fn prune(&mut self, source: &SourceId, keep: &dyn Fn(&Link) -> bool) -> usize {
        let before = self.links.len();
        self.links.retain(|l| l.source != source.0 || keep(l));
        before - self.links.len()
    }

    /// Recompute fingerprints for every link in `source` and report drift.
    pub fn check_drift(
        &self,
        source: &SourceId,
        provider: &dyn DerivedNodeSource,
    ) -> Vec<DriftReport> {
        self.links
            .iter()
            .filter(|l| l.source == source.0)
            .map(|l| {
                let handle = NodeHandle { source: source.clone(), id: l.target.clone() };
                let current = provider.fingerprint_at(&handle, l.resolution).map(|f| f.0);
                DriftReport {
                    spec: l.spec.clone(),
                    target: l.target.clone(),
                    stored: l.fingerprint.clone(),
                    drifted: current.as_deref().is_some_and(|c| c != l.fingerprint),
                    missing: current.is_none(),
                    current,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;
    use crate::{EdgeKind, Fingerprint, SourceCaps, SourceLoc};

    /// A provider whose fingerprints are a mutable map — lets a test "edit code" between calls.
    struct Mock {
        fps: RefCell<HashMap<String, Option<String>>>,
    }

    impl Mock {
        fn new(fps: &[(&str, &str)]) -> Self {
            let fps = fps
                .iter()
                .map(|(k, v)| (k.to_string(), Some(v.to_string())))
                .collect();
            Mock { fps: RefCell::new(fps) }
        }
        fn set(&self, target: &str, fp: Option<&str>) {
            self.fps.borrow_mut().insert(target.to_string(), fp.map(String::from));
        }
    }

    impl DerivedNodeSource for Mock {
        fn resolve(&self, _q: &str, _s: &SourceId) -> Option<NodeHandle> {
            None
        }
        fn locate(&self, _h: &NodeHandle) -> Option<SourceLoc> {
            None
        }
        fn content(&self, _h: &NodeHandle) -> Option<String> {
            None
        }
        fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint> {
            self.fps.borrow().get(&h.id).cloned().flatten().map(Fingerprint)
        }
        fn neighbors(&self, _h: &NodeHandle, _k: &[EdgeKind]) -> Vec<NodeHandle> {
            Vec::new()
        }
        fn capabilities(&self) -> SourceCaps {
            SourceCaps::default()
        }
    }

    fn handle(id: &str) -> NodeHandle {
        NodeHandle { source: SourceId("src".into()), id: id.into() }
    }

    #[test]
    fn add_link_is_merge_preserving() {
        let mock = Mock::new(&[("f", "v1")]);
        let mut store = LinkStore::default();
        assert_eq!(store.add_link("s", "implements", &handle("f"), Resolution::Code, &mock), AddOutcome::Added);

        // The target "changes"; re-seeding the same edge must NOT move the baseline.
        mock.set("f", Some("v2"));
        assert_eq!(store.add_link("s", "implements", &handle("f"), Resolution::Code, &mock), AddOutcome::Existing);
        assert_eq!(store.links.len(), 1);
        assert_eq!(store.links[0].fingerprint, "v1");

        // … so drift still fires against the original baseline.
        let reports = store.check_drift(&SourceId("src".into()), &mock);
        assert!(reports[0].drifted && !reports[0].missing);
    }

    #[test]
    fn resolution_change_rescopes_and_recaptures() {
        let mock = Mock::new(&[("f", "v1")]);
        let mut store = LinkStore::default();
        store.add_link("s", "touches", &handle("f"), Resolution::Component, &mock);

        mock.set("f", Some("v2"));
        // Same edge, new declared altitude → re-pin + recapture at the new grain.
        assert_eq!(
            store.add_link("s", "touches", &handle("f"), Resolution::Container, &mock),
            AddOutcome::Rescoped
        );
        assert_eq!(store.links.len(), 1);
        assert_eq!(store.links[0].resolution, Resolution::Container);
        assert_eq!(store.links[0].fingerprint, "v2", "baseline recaptured at the new altitude");
    }

    #[test]
    fn relation_floor_pins_implements_and_clamps_touches() {
        // implements/verifies are body-level claims: declared coarseness is ignored.
        assert_eq!(Resolution::for_relation("implements", Some(Resolution::Container)), Resolution::Code);
        assert_eq!(Resolution::for_relation("verifies", Some(Resolution::Context)), Resolution::Code);
        // The bug-row relations make body-level claims too (tracker-relation-links):
        // a bug manifests in a body, a fix is vouched for by a test body.
        assert_eq!(Resolution::for_relation("manifests-in", Some(Resolution::Container)), Resolution::Code);
        assert_eq!(Resolution::for_relation("verifies-fix", Some(Resolution::Context)), Resolution::Code);
        // touches takes the declared altitude, clamped no finer than Component.
        assert_eq!(Resolution::for_relation("touches", None), Resolution::Component);
        assert_eq!(Resolution::for_relation("touches", Some(Resolution::Container)), Resolution::Container);
        assert_eq!(Resolution::for_relation("touches", Some(Resolution::Code)), Resolution::Component);
    }

    #[test]
    fn add_link_refuses_unfingerprintable_targets() {
        let mock = Mock::new(&[]);
        let mut store = LinkStore::default();
        assert_eq!(
            store.add_link("s", "implements", &handle("ghost"), Resolution::Code, &mock),
            AddOutcome::NoFingerprint
        );
        assert!(store.links.is_empty());
    }

    #[test]
    fn rebaseline_clears_drift_for_one_spec() {
        let mock = Mock::new(&[("f", "v1"), ("g", "w1")]);
        let mut store = LinkStore::default();
        store.add_link("s1", "implements", &handle("f"), Resolution::Code, &mock);
        store.add_link("s2", "implements", &handle("g"), Resolution::Code, &mock);

        mock.set("f", Some("v2"));
        mock.set("g", Some("w2"));
        let src = SourceId("src".into());
        assert_eq!(store.rebaseline(Some("s1"), &src, &mock), 1);

        let by_spec: HashMap<_, _> =
            store.check_drift(&src, &mock).into_iter().map(|r| (r.spec.clone(), r)).collect();
        assert!(!by_spec["s1"].drifted, "acked spec is clean");
        assert!(by_spec["s2"].drifted, "other spec still drifted");
    }

    #[test]
    fn rebaseline_leaves_missing_targets_untouched() {
        let mock = Mock::new(&[("f", "v1")]);
        let mut store = LinkStore::default();
        store.add_link("s", "implements", &handle("f"), Resolution::Code, &mock);

        mock.set("f", None); // target gone from the index
        let src = SourceId("src".into());
        assert_eq!(store.rebaseline(None, &src, &mock), 0);
        assert_eq!(store.links[0].fingerprint, "v1");
        assert!(store.check_drift(&src, &mock)[0].missing);
    }

    #[test]
    fn prune_drops_unkept_edges_only_in_source() {
        let mock = Mock::new(&[("f", "v1"), ("g", "w1")]);
        let mut store = LinkStore::default();
        store.add_link("s1", "implements", &handle("f"), Resolution::Code, &mock);
        store.add_link("s2", "implements", &handle("g"), Resolution::Code, &mock);
        // Same target in ANOTHER source — prune must never reach across sources.
        store.links.push(Link {
            spec: "s2".into(),
            relation: "implements".into(),
            source: "other".into(),
            target: "g".into(),
            fingerprint: "w1".into(),
            resolution: Resolution::Code,
        });

        let pruned = store.prune(&SourceId("src".into()), &|l| l.spec == "s1");
        assert_eq!(pruned, 1, "only s2's edge in 'src' is dropped");
        assert_eq!(store.links.len(), 2);
        assert!(store.links.iter().any(|l| l.spec == "s1" && l.source == "src"));
        assert!(store.links.iter().any(|l| l.spec == "s2" && l.source == "other"));
    }

    #[test]
    fn save_load_roundtrip() {
        let mock = Mock::new(&[("f", "v1")]);
        let mut store = LinkStore::default();
        store.add_link("s", "implements", &handle("f"), Resolution::Component, &mock);

        let dir = std::env::temp_dir().join("spec-engine-test-store");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("links.json");
        store.save(&path).unwrap();
        let loaded = LinkStore::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.links.len(), 1);
        assert_eq!(loaded.links[0].fingerprint, "v1");
        assert_eq!(loaded.links[0].resolution, Resolution::Component);
    }
}
