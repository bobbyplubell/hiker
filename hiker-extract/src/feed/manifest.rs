//! The feed dedup manifest: the persistent `guid → child` map that makes a
//! feed dedup ACROSS polls (`rss-guid-dedup`). The crawl loop's in-run visited
//! set only dedups within one run; a living subscription must remember every
//! guid it has ever turned into a child so the *next* poll only writes
//! genuinely new entries.
//!
//! It is stored as `.feed-manifest.json` inside the feed note's companion
//! folder — NOT in the feed note's frontmatter. The companion folder is chosen
//! deliberately: a busy feed accrues hundreds of guids, and parking that map in
//! the user-facing note's frontmatter would bloat a note the user reads and
//! edits (and would re-serialize the whole map on every poll through the
//! op-log). The manifest sits beside the children it maps, hidden like the
//! children's archives, and is regenerable in principle from the children's
//! `source_url` stamps. This mirrors the companion-folder precedent the crawl
//! `URL → sidecar` map already follows.
//
// status: rss-guid-dedup

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The on-disk manifest filename inside the companion folder.
const MANIFEST_FILE: &str = ".feed-manifest.json";

/// One mapped entry: which child file a guid produced, the content hash of the
/// body last written there (so an unchanged re-poll is a no-op and a changed
/// one re-extracts), and the entry's published timestamp (for retention
/// ordering — oldest pruned first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The child note's filename within the companion folder (e.g.
    /// `my-post.md`).
    pub child_file: String,
    /// blake3 hex of the body last written to the child — the change detector.
    pub content_hash: String,
    /// The entry's RFC-3339 published timestamp, if the feed supplied one.
    /// Drives oldest-first retention pruning; `None` sorts oldest.
    #[serde(default)]
    pub published: Option<String>,
}

/// The dedup map for one feed: `guid → Record`. A `BTreeMap` gives a stable
/// serialization order so the manifest file diffs cleanly across polls.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// guid → child record.
    pub entries: BTreeMap<String, Record>,
}

impl Manifest {
    /// Load the manifest from `companion_dir/.feed-manifest.json`, or an empty
    /// manifest when the file is absent (a never-polled feed). A malformed
    /// manifest is an error rather than a silent reset — losing the dedup map
    /// would re-create every child as "new".
    pub fn load(companion_dir: &Path) -> Result<Self, String> {
        let path = companion_dir.join(MANIFEST_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    }

    /// Persist the manifest to `companion_dir/.feed-manifest.json` (atomic
    /// write-then-rename), creating the companion folder if needed.
    pub fn save(&self, companion_dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(companion_dir).map_err(|e| format!("mkdir {}: {e}", companion_dir.display()))?;
        let path = companion_dir.join(MANIFEST_FILE);
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("serialize manifest: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    /// Look up the record for a guid, if this feed has seen it before.
    pub fn lookup(&self, guid: &str) -> Option<&Record> {
        self.entries.get(guid)
    }

    /// Record a freshly-written child for a new guid.
    pub fn insert(&mut self, guid: &str, child_file: &str, content_hash: &str, published: Option<&str>) {
        self.entries.insert(
            guid.to_string(),
            Record {
                child_file: child_file.to_string(),
                content_hash: content_hash.to_string(),
                published: published.map(str::to_string),
            },
        );
    }

    /// Update the stored content hash for a known guid after a re-extraction.
    pub fn update_hash(&mut self, guid: &str, content_hash: &str) {
        if let Some(rec) = self.entries.get_mut(guid) {
            rec.content_hash = content_hash.to_string();
        }
    }

    /// Drop a guid's record (after its child has been pruned to trash).
    pub fn remove(&mut self, guid: &str) {
        self.entries.remove(guid);
    }

    /// The guids ordered oldest-first by published timestamp (a missing
    /// timestamp sorts oldest, so undated entries are pruned before dated
    /// ones). Used by retention to pick which children to drop.
    pub fn guids_oldest_first(&self) -> Vec<String> {
        let mut guids: Vec<&String> = self.entries.keys().collect();
        guids.sort_by(|a, b| {
            let pa = self.entries[*a].published.as_deref().unwrap_or("");
            let pb = self.entries[*b].published.as_deref().unwrap_or("");
            pa.cmp(pb)
        });
        guids.into_iter().cloned().collect()
    }
}
