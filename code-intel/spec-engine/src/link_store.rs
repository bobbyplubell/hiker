//! Authored spec→entity links + drift (`link-store`, `drift-fingerprint`).
//!
//! Links live on the authored (spec) side, version-controlled. Each stores the target's
//! **fingerprint at link time**; drift = recompute via the provider and compare. The store is
//! backend-agnostic — it only talks to [`DerivedNodeSource`].

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{DerivedNodeSource, NodeHandle, SourceId};

/// One authored edge from a spec (or any authored node) to a derived node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub spec: String,        // authored node id (e.g. a spec slug)
    pub relation: String,    // "implements" | "touches" | …
    pub source: String,      // SourceId.0 (the repo id)
    pub target: String,      // NodeHandle.id (the symbol moniker)
    pub fingerprint: String, // target fingerprint captured at link time
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LinkStore {
    pub links: Vec<Link>,
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

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).expect("serialize link store");
        std::fs::write(path, json)
    }

    /// Author a link, capturing the target's current fingerprint via the provider.
    pub fn add_link(
        &mut self,
        spec: &str,
        relation: &str,
        handle: &NodeHandle,
        provider: &dyn DerivedNodeSource,
    ) {
        let fingerprint = provider.fingerprint(handle).map(|f| f.0).unwrap_or_default();
        self.links.push(Link {
            spec: spec.to_string(),
            relation: relation.to_string(),
            source: handle.source.0.clone(),
            target: handle.id.clone(),
            fingerprint,
        });
    }

    pub fn for_spec<'a>(&'a self, spec: &'a str) -> impl Iterator<Item = &'a Link> {
        self.links.iter().filter(move |l| l.spec == spec)
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
                let current = provider.fingerprint(&handle).map(|f| f.0);
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
