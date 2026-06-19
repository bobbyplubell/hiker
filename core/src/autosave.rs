//! Crash-recovery autosave for dirty editor buffers, plus a tab-state
//! snapshot the next vault open round-trips. See docs/autosave.md.
//!
//! Storage at `<vault>/.hiker/autosave/`: one `<id>--<slug>.md` per dirty
//! buffer (overwritten in place each tick, NPP shape) plus an
//! `index.json` carrying the path↔id map, per-entry content hash, and
//! the authoritative tab-state snapshot. All filesystem writes for the
//! autosave directory live here — module discipline mirrors
//! `core::store`.
//
// status: autosave-backend-module
// status: autosave-store-layout
// status: autosave-one-per-buffer
// status: autosave-recover-cmd
// status: autosave-vault-swap-clears
// status: autosave-no-watcher-suppression
// status: autosave-backup-class

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

const AUTOSAVE_DIRNAME: &str = "autosave";
const INDEX_NAME: &str = "index.json";
// v2: dropped the `Capture` / `Plugins` dock-tab kinds (web-source
// acquisition + the plugin host left core). An index written by an older
// binary may carry those kinds in its tab-state snapshot, so a version
// mismatch resets the index to the bootstrap default rather than restoring a
// layout referencing tabs that no longer exist.
const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Tab-state snapshot. `open_paths` is the ordered list the frontend
/// reopens on next vault open; `active_path` is the tab that gets
/// activated; `preview_path` is the at-most-one preview slot's path
/// (or `None` when no preview tab existed at flush time).
/// `open_tab_kinds` records the `kind` discriminator per tab so the
/// restore path knows whether to restore as buffer or page-kind.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    #[serde(default)]
    pub open_paths: Vec<String>,
    #[serde(default)]
    pub active_path: Option<String>,
    #[serde(default)]
    pub preview_path: Option<String>,
    #[serde(default)]
    pub saved_at_ms: i64,
    // status: tab-kinds
    #[serde(default)]
    pub open_tab_kinds: HashMap<String, String>,
    /// Per-canvas view state (camera pan/zoom + per-card scroll/zoom),
    /// keyed by the canvas's vault-relative path. View state only — it
    /// rides this tab-state store rather than the layered doc / `.canvas` file
    /// (the camera is view state and never enters the layered doc). Restored on
    /// reopen and across restart. status: canvas-view-state-persist
    #[serde(default)]
    pub canvas_views: HashMap<String, CanvasViewState>,
    /// Per-graph-view persisted state (the vault link-graph engine's view:
    /// node positions, projection, focus mode, toggles, LOD, pan/zoom),
    /// keyed by the graph tab's persist key (`:graph` for the singleton vault
    /// graph). View-only — rides this tab-state store like `canvas_views`.
    /// status: graph-view-state-persist
    #[serde(default)]
    pub graph_views: HashMap<String, GraphViewState>,
    /// Per-code-graph-view persisted state (level / edge filters / focus +
    /// the underlying graph engine's view), keyed by the code source's
    /// `CodeSource::key()`. status: graph-view-state-persist
    #[serde(default)]
    pub code_graph_views: HashMap<String, CodeGraphViewState>,
}

/// One card's view state on a canvas: the per-card content zoom (font
/// multiplier) and vertical scroll offset, both decoupled from camera zoom.
/// Plain primitives so the app can convert to/from `canvas_view`'s `CardView`
/// without that crate needing serde. status: canvas-view-state-persist
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CardViewState {
    #[serde(default)]
    pub zoom: f32,
    #[serde(default)]
    pub scroll_y: f32,
}

/// A canvas's persisted view state: the camera pan (canvas-space point pinned
/// to the viewport top-left) + zoom scale, plus each touched card's view state
/// keyed by node id. Rides the tab-state store, NOT the layered doc / `.canvas` file
/// — consistent with the camera being view state. status: canvas-view-state-persist
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CanvasViewState {
    #[serde(default)]
    pub pan_x: f64,
    #[serde(default)]
    pub pan_y: f64,
    #[serde(default)]
    pub scale: f32,
    #[serde(default)]
    pub cards: HashMap<String, CardViewState>,
}

/// A graph-view engine's persisted view state. Plain primitives only so the
/// app can convert to/from the `hiker_graph_view` engine `State` without that
/// crate (or `hiker_projection`) needing serde — the conversion happens at the
/// app boundary, mirroring how `CanvasViewState` decouples from `canvas_view`.
///
/// Excludes every non-serializable / recomputed engine field (the layout
/// worker, edge routes, hover-preview cache, fly-to animation, the `Mobius`
/// nav, GPU handles): only the plain view bits below are snapshotted.
/// status: graph-view-state-persist
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphViewState {
    /// Force-layout node positions keyed by `Source::node_key` (the stable
    /// per-node identity, e.g. a note's rel-path). Seeded back on restore so
    /// the layout morphs to the saved shape. `(x, y)` world coords.
    #[serde(default)]
    pub positions: HashMap<String, (f32, f32)>,
    /// Affine pan/zoom view (`View::pan` + `View::zoom`).
    #[serde(default)]
    pub pan_x: f32,
    #[serde(default)]
    pub pan_y: f32,
    #[serde(default)]
    pub zoom: f32,
    /// Projection: kind discriminant (`"affine"` / `"fisheye"` / `"poincare"`),
    /// strength, size falloff. `ProjectionConfig`/`ProjectionKind` aren't serde,
    /// so store primitives and convert at the boundary.
    #[serde(default)]
    pub projection_kind: String,
    #[serde(default)]
    pub projection_strength: f32,
    #[serde(default)]
    pub projection_size_falloff: f32,
    /// Lens focus mode discriminant (`"center"` / `"cursor"` / `"selection"`).
    #[serde(default)]
    pub focus_mode: String,
    /// Common toggles.
    #[serde(default)]
    pub show_labels: bool,
    #[serde(default)]
    pub show_edges: bool,
    #[serde(default)]
    pub show_preview: bool,
    /// LOD magnification thresholds.
    #[serde(default)]
    pub lod_full_mag: f32,
    #[serde(default)]
    pub lod_marker_mag: f32,
    /// Vault-graph display filters, stored as the HIDDEN entries so a kind
    /// first appearing after a rebuild defaults to visible (mirrors
    /// `CodeGraphViewState::hidden_kinds`). Empty/unused when this struct
    /// rides inside `CodeGraphViewState` as the engine half.
    /// status: vault-graph-edge-toggles
    #[serde(default)]
    pub hidden_edge_kinds: Vec<String>,
    /// status: vault-graph-kind-filters
    #[serde(default)]
    pub hidden_node_kinds: Vec<String>,
    /// The vault graph's coarse detail dial, as a string discriminant
    /// (`"containers"` / `"everything"`; empty = everything).
    /// status: vault-graph-lod-containers
    #[serde(default)]
    pub detail: String,
    /// The vault graph's focus-navigation state: display scope as a string
    /// discriminant (`"overview"` / `"hops:1..3"`; empty = overview) plus
    /// the focus anchor's note rel-path — the vault twins of
    /// `CodeGraphViewState::scope`/`selected`. Empty/`None` when this struct
    /// rides inside `CodeGraphViewState` as the engine half.
    /// status: graph-nav-extract
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub focus: Option<String>,
    /// The vault graph's query scope: the scoping query-doc's rel-path, or
    /// `None` for the full vault. Only the doc path persists — the member
    /// set re-executes per rebuild, never from a snapshot.
    /// status: graph-scoped-query
    #[serde(default)]
    pub scope_query: Option<String>,
    /// Whether the vault graph's spec drift badges are on (the rollup data
    /// itself always reloads from the link-store baseline).
    /// status: vault-graph-spec-drift-badge
    #[serde(default)]
    pub drift_badges: bool,
}

/// A code-graph view's persisted state: its display controls (scope / selection /
/// kind filter / edge filters / orphans / size-by-loc) plus the underlying graph
/// engine's [`GraphViewState`] (positions + view). `scope` is stored as a String
/// discriminant (`"overview"` / `"hops:1..3"`) because the `Scope` enum lives in
/// the app crate, not here. The kind filter persists as the HIDDEN kinds, so a
/// kind that first appears after a reindex defaults to visible.
/// status: graph-view-state-persist
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CodeGraphViewState {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub hidden_kinds: Vec<String>,
    #[serde(default)]
    pub show_calls: bool,
    #[serde(default)]
    pub show_impls: bool,
    #[serde(default)]
    pub show_orphans: bool,
    #[serde(default)]
    pub size_by_loc: bool,
    /// The code graph's own engine view (positions + pan/zoom + projection …).
    #[serde(default)]
    pub engine: GraphViewState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub autosave_id: String,
    pub content_hash: String,
    pub saved_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, IndexEntry>,
    #[serde(default)]
    tab_state: Option<TabState>,
}

impl Default for IndexFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            entries: BTreeMap::new(),
            tab_state: None,
        }
    }
}

/// Recovered buffer surfaced by `recover()` — only entries whose
/// autosaved content differs from what's on disk for the same path
/// (or whose on-disk file is gone) make the cut.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveredEntry {
    pub path: String,
    pub autosave_id: String,
    pub autosaved_content: Vec<u8>,
    pub autosaved_hash: String,
    pub on_disk_hash: Option<String>,
    pub saved_at_ms: i64,
}

pub struct Autosave {
    vault_root: PathBuf,
    dir: PathBuf,
    /// Serializes index.json read-modify-writes + per-buffer file writes
    /// per spec: "concurrent ticks for the same path are serialized in
    /// the backend." Using a single mutex (rather than per-path) keeps
    /// the implementation simple — autosave is a low-frequency write
    /// path (one tick per 5s) and the lock is held across one file
    /// write + one index rewrite, both small.
    lock: Mutex<()>,
}

impl Autosave {
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        let dir = vault_root.join(".hiker").join(AUTOSAVE_DIRNAME);
        fs::create_dir_all(&dir)?;
        let me = Self {
            vault_root: vault_root.to_path_buf(),
            dir,
            lock: Mutex::new(()),
        };
        // Schema check: if index.json exists at a different version, reset it
        // to the bootstrap default. The tab-state snapshot it carries may
        // reference tab kinds this binary no longer knows; a clean reset means
        // a vanished tab reads as an expected reset, not a mysterious failure.
        if let Some(v) = me.read_index_for_version_check()?
            && v != SCHEMA_VERSION
        {
            me.write_index_atomic(&IndexFile::default())?;
        }
        Ok(me)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_NAME)
    }

    fn read_index_for_version_check(&self) -> Result<Option<u32>, Error> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(None);
        }
        let raw = fs::read(&p)?;
        if raw.is_empty() {
            return Ok(None);
        }
        #[derive(Deserialize)]
        struct VersionOnly {
            version: u32,
        }
        let v: VersionOnly = serde_json::from_slice(&raw)?;
        Ok(Some(v.version))
    }

    fn read_index(&self) -> Result<IndexFile, Error> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(IndexFile::default());
        }
        let raw = fs::read(&p)?;
        if raw.is_empty() {
            return Ok(IndexFile::default());
        }
        let parsed: IndexFile = serde_json::from_slice(&raw)?;
        Ok(parsed)
    }

    fn write_index_atomic(&self, idx: &IndexFile) -> Result<(), Error> {
        let p = self.index_path();
        let tmp = p.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(idx)?;
        write_file_atomic(&tmp, &p, &bytes)?;
        Ok(())
    }

    /// Write or overwrite a sidecar for `path`. Idempotent on the same
    /// `path` — overwrites the existing file in place per
    /// `autosave-one-per-buffer`. `buffer_hash` is the blake3 of
    /// `contents` (the frontend computes it once and passes it through;
    /// recomputing here would burn cycles on the hot tick path).
    pub fn write(
        &self,
        path: &str,
        contents: &[u8],
        buffer_hash: &str,
    ) -> Result<(), Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        let now = now_ms();
        let entry = match idx.entries.get(path) {
            Some(e) => IndexEntry {
                autosave_id: e.autosave_id.clone(),
                content_hash: buffer_hash.to_string(),
                saved_at_ms: now,
            },
            None => IndexEntry {
                autosave_id: Ulid::new().to_string(),
                content_hash: buffer_hash.to_string(),
                saved_at_ms: now,
            },
        };
        let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
        let tmp = file_path.with_extension("md.tmp");
        write_file_atomic(&tmp, &file_path, contents)?;
        idx.entries.insert(path.to_string(), entry);
        self.write_index_atomic(&idx)?;
        Ok(())
    }

    /// Drop the sidecar for `path`. No-op when no entry exists.
    pub fn clear(&self, path: &str) -> Result<(), Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        if let Some(entry) = idx.entries.remove(path) {
            let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
            if file_path.exists()
                && let Err(e) = fs::remove_file(&file_path)
            {
                tracing::warn!(error = %e, %path,
                    "autosave sidecar not removed on clear; orphaned file lingers");
            }
            self.write_index_atomic(&idx)?;
        }
        Ok(())
    }

    pub fn save_tab_state(&self, mut state: TabState) -> Result<(), Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        if state.saved_at_ms == 0 {
            state.saved_at_ms = now_ms();
        }
        idx.tab_state = Some(state);
        self.write_index_atomic(&idx)?;
        Ok(())
    }

    pub fn load_tab_state(&self) -> Result<Option<TabState>, Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let idx = self.read_index()?;
        Ok(idx.tab_state)
    }

    /// Walk the index, drop entries whose autosaved hash matches the
    /// live on-disk hash for the same path (stale snapshots from the
    /// last clean session), and return the genuine deltas. Matches are
    /// removed from the index file as a side effect so the next
    /// `recover()` doesn't re-do the work.
    pub fn recover(&self) -> Result<Vec<RecoveredEntry>, Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        let mut idx = self.read_index()?;
        let mut out: Vec<RecoveredEntry> = Vec::new();
        let mut drop_paths: Vec<String> = Vec::new();
        for (path, entry) in idx.entries.iter() {
            let live = self.live_disk_hash(path)?;
            let on_disk_matches = match &live {
                Some(h) => h == &entry.content_hash,
                None => false,
            };
            let file_path = self.dir.join(autosave_filename(&entry.autosave_id, path));
            if on_disk_matches {
                // Stale; drop the sidecar + index entry.
                drop_paths.push(path.clone());
                if file_path.exists()
                    && let Err(e) = fs::remove_file(&file_path)
                {
                    tracing::warn!(error = %e, %path,
                        "stale autosave sidecar not removed; orphaned file lingers");
                }
                continue;
            }
            // Genuine recovery candidate. Read the autosaved bytes.
            let autosaved_content = match fs::read(&file_path) {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Index points at a missing sidecar — drop the
                    // orphan; nothing to recover.
                    drop_paths.push(path.clone());
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            out.push(RecoveredEntry {
                path: path.clone(),
                autosave_id: entry.autosave_id.clone(),
                autosaved_content,
                autosaved_hash: entry.content_hash.clone(),
                on_disk_hash: live,
                saved_at_ms: entry.saved_at_ms,
            });
        }
        if !drop_paths.is_empty() {
            for p in &drop_paths {
                idx.entries.remove(p);
            }
            self.write_index_atomic(&idx)?;
        }
        Ok(out)
    }

    /// Discard the sidecar without restoring it. Same on-disk effect as
    /// `clear`; the separate verb mirrors the spec's recovery-modal API
    /// so the frontend can keep "Discard" / "successful save → clear"
    /// as distinct call sites.
    pub fn discard(&self, path: &str) -> Result<(), Error> {
        self.clear(path)
    }

    /// Wipe all on-disk autosave state for this vault. Called on vault
    /// swap to guarantee no cross-vault leakage of state — even if the
    /// per-buffer clear/save_tab_state path was skipped (force-quit
    /// during swap, etc.).
    ///
    /// status: autosave-vault-swap-clears
    pub fn vault_swap_reset(&self) -> Result<(), Error> {
        let _g = self.lock.lock().expect("autosave lock poisoned");
        if !self.dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file()
                && let Err(e) = fs::remove_file(&p)
            {
                tracing::warn!(error = %e, path = %p.display(),
                    "autosave reset left a file behind");
            }
        }
        Ok(())
    }

    fn live_disk_hash(&self, rel_path: &str) -> Result<Option<String>, Error> {
        let abs = self.vault_root.join(rel_path);
        match fs::read(&abs) {
            Ok(bytes) => Ok(Some(blake3::hash(&bytes).to_hex().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Build the on-disk filename for an autosave sidecar. The `<id>` half is
/// the canonical lookup (the index.json maps path → id); the `--<slug>`
/// suffix is purely debuggable. Slug is the path's basename with a
/// conservative character allowlist.
fn autosave_filename(id: &str, path: &str) -> String {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let mut slug = String::with_capacity(basename.len());
    for ch in basename.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            slug.push(ch);
        } else {
            slug.push('_');
        }
    }
    if slug.is_empty() {
        slug.push_str("buf");
    }
    format!("{id}--{slug}")
}

/// Atomic file write: create at `tmp`, fsync, rename to `final_path`.
/// Crash mid-write leaves either the prior file or no change — never a
/// half-written one. Matches the `vim`-style write-temp-then-rename
/// pattern called out in the spec.
fn write_file_atomic(tmp: &Path, final_path: &Path, bytes: &[u8]) -> io::Result<()> {
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(tmp, final_path)?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn hash(b: &[u8]) -> String {
        blake3::hash(b).to_hex().to_string()
    }

    #[test]
    fn write_clear_round_trip() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"draft", &hash(b"draft")).unwrap();
        let recovered = a.recover().unwrap();
        // No on-disk file at all → entry surfaces.
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].path, "a.md");
        assert_eq!(recovered[0].autosaved_content, b"draft");
        assert!(recovered[0].on_disk_hash.is_none());

        a.clear("a.md").unwrap();
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn recover_drops_matching_on_disk() {
        let dir = tempdir().unwrap();
        let body = b"clean save";
        std::fs::write(dir.path().join("a.md"), body).unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", body, &hash(body)).unwrap();
        // The autosave matches the live disk file → stale, dropped.
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn recover_surfaces_when_disk_differs() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), b"on-disk").unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"in-memory", &hash(b"in-memory")).unwrap();
        let recovered = a.recover().unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].autosaved_content, b"in-memory");
        assert_eq!(recovered[0].on_disk_hash, Some(hash(b"on-disk")));
    }

    #[test]
    fn write_overwrites_in_place() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"v1", &hash(b"v1")).unwrap();
        let id1 = a.read_index().unwrap().entries["a.md"].autosave_id.clone();
        a.write("a.md", b"v2", &hash(b"v2")).unwrap();
        let id2 = a.read_index().unwrap().entries["a.md"].autosave_id.clone();
        assert_eq!(id1, id2, "autosave_id stable across ticks for the same path");
        // Only one sidecar file under the dir for this path's id.
        let count = std::fs::read_dir(a.dir())
            .unwrap()
            .filter(|e| {
                let n = e.as_ref().unwrap().file_name();
                n.to_string_lossy().starts_with(&id1)
            })
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn tab_state_round_trips() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        assert!(a.load_tab_state().unwrap().is_none());
        let mut cards = HashMap::new();
        cards.insert("n1".to_string(), CardViewState { zoom: 1.5, scroll_y: 42.0 });
        let mut canvas_views = HashMap::new();
        canvas_views.insert(
            "boards/plan.canvas".to_string(),
            CanvasViewState { pan_x: -120.5, pan_y: 33.0, scale: 0.75, cards },
        );
        let s = TabState {
            open_paths: vec!["a.md".into(), "b.md".into()],
            active_path: Some("b.md".into()),
            preview_path: Some("c.md".into()),
            open_tab_kinds: HashMap::new(),
            saved_at_ms: 0,
            canvas_views,
            graph_views: HashMap::new(),
            code_graph_views: HashMap::new(),
        };
        a.save_tab_state(s.clone()).unwrap();
        let loaded = a.load_tab_state().unwrap().unwrap();
        assert_eq!(loaded.open_paths, s.open_paths);
        assert_eq!(loaded.active_path, s.active_path);
        assert_eq!(loaded.preview_path, s.preview_path);
        assert_eq!(loaded.canvas_views, s.canvas_views);
        let cv = &loaded.canvas_views["boards/plan.canvas"];
        assert!((cv.scale - 0.75).abs() < 1e-6);
        assert!((cv.pan_x - (-120.5)).abs() < 1e-9);
        assert_eq!(cv.cards["n1"].scroll_y, 42.0);
        assert!(loaded.saved_at_ms > 0);
    }

    #[test]
    fn tab_state_round_trips_graph_and_code_graph_views() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        let mut positions = HashMap::new();
        positions.insert("notes/a.md".to_string(), (12.0_f32, -34.0_f32));
        positions.insert("notes/b.md".to_string(), (5.5_f32, 6.5_f32));
        let gv = GraphViewState {
            positions: positions.clone(),
            pan_x: -1.0,
            pan_y: 2.0,
            zoom: 0.75,
            projection_kind: "poincare".into(),
            projection_strength: 1.5,
            projection_size_falloff: 0.3,
            focus_mode: "cursor".into(),
            show_labels: true,
            show_edges: false,
            show_preview: true,
            lod_full_mag: 0.5,
            lod_marker_mag: 0.15,
            // Vault display filters (hidden kinds + detail dial) and the
            // focus-nav location ride the same record.
            // status: vault-graph-edge-toggles, graph-nav-extract
            hidden_edge_kinds: vec!["board".into()],
            hidden_node_kinds: vec!["query".into()],
            detail: "containers".into(),
            scope: "hops:2".into(),
            focus: Some("boards/b.md".into()),
            // The query scope's doc path + the drift toggle ride it too.
            // status: graph-scoped-query, vault-graph-spec-drift-badge
            scope_query: Some("queries/rust.md".into()),
            drift_badges: true,
        };
        let mut graph_views = HashMap::new();
        graph_views.insert(":graph".to_string(), gv.clone());

        let cgv = CodeGraphViewState {
            scope: "hops:3".into(),
            selected: Some("scip:foo#bar".into()),
            hidden_kinds: vec!["code:field".into()],
            show_calls: true,
            show_impls: false,
            show_orphans: true,
            size_by_loc: true,
            engine: gv.clone(),
        };
        let mut code_graph_views = HashMap::new();
        code_graph_views.insert("project:proj.md".to_string(), cgv.clone());

        let s = TabState {
            open_paths: vec!["a.md".into()],
            graph_views,
            code_graph_views,
            ..Default::default()
        };
        a.save_tab_state(s.clone()).unwrap();
        let loaded = a.load_tab_state().unwrap().unwrap();
        assert_eq!(loaded.graph_views, s.graph_views);
        assert_eq!(loaded.code_graph_views, s.code_graph_views);
        let lgv = &loaded.graph_views[":graph"];
        assert_eq!(lgv.positions["notes/a.md"], (12.0, -34.0));
        assert_eq!(lgv.projection_kind, "poincare");
        assert_eq!(lgv.hidden_edge_kinds, vec!["board".to_string()]);
        assert_eq!(lgv.hidden_node_kinds, vec!["query".to_string()]);
        assert_eq!(lgv.detail, "containers");
        assert_eq!(lgv.scope_query.as_deref(), Some("queries/rust.md"));
        assert!(lgv.drift_badges);
        let lcgv = &loaded.code_graph_views["project:proj.md"];
        assert_eq!(lcgv.scope, "hops:3");
        assert_eq!(lcgv.selected.as_deref(), Some("scip:foo#bar"));
        assert_eq!(lcgv.hidden_kinds, vec!["code:field".to_string()]);
        assert_eq!(lcgv.engine.positions, positions);
    }

    #[test]
    fn old_snapshot_without_canvas_views_still_loads() {
        // A pre-canvas-view-state index.json has no `canvas_views` key;
        // `#[serde(default)]` keeps it loadable as an empty map.
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        let raw = serde_json::json!({
            "version": SCHEMA_VERSION,
            "entries": {},
            "tab_state": {
                "open_paths": ["a.md"],
                "active_path": "a.md",
                "saved_at_ms": 123,
            },
        });
        std::fs::write(a.index_path(), raw.to_string()).unwrap();
        let loaded = a.load_tab_state().unwrap().unwrap();
        assert_eq!(loaded.open_paths, vec!["a.md".to_string()]);
        assert!(loaded.canvas_views.is_empty());
    }

    #[test]
    fn vault_swap_reset_wipes_dir() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"x", &hash(b"x")).unwrap();
        a.save_tab_state(TabState {
            open_paths: vec!["a.md".into()],
            ..Default::default()
        })
        .unwrap();
        a.vault_swap_reset().unwrap();
        // Re-open against the same vault root; nothing left.
        let a2 = Autosave::open(dir.path()).unwrap();
        assert!(a2.load_tab_state().unwrap().is_none());
        assert!(a2.recover().unwrap().is_empty());
    }

    #[test]
    fn version_mismatch_resets_to_default() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        // Write a differing-version index by hand, carrying a stale tab-state
        // snapshot that an older binary might have persisted.
        let raw = serde_json::json!({
            "version": 99,
            "entries": {},
            "tab_state": { "open_paths": ["gone.md"] },
        });
        std::fs::write(a.index_path(), raw.to_string()).unwrap();
        // Re-open: the mismatch resets the index to the bootstrap default
        // (no error), so the stale tab-state is gone and the version is current.
        let a2 = Autosave::open(dir.path()).unwrap();
        assert!(a2.load_tab_state().unwrap().is_none());
        assert_eq!(
            a2.read_index_for_version_check().unwrap(),
            Some(SCHEMA_VERSION),
        );
    }

    #[test]
    fn recover_drops_orphan_sidecar_pointer() {
        let dir = tempdir().unwrap();
        let a = Autosave::open(dir.path()).unwrap();
        a.write("a.md", b"x", &hash(b"x")).unwrap();
        // Manually delete the sidecar file — index now points at nothing.
        for entry in std::fs::read_dir(a.dir()).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                std::fs::remove_file(&p).unwrap();
            }
        }
        let recovered = a.recover().unwrap();
        assert!(recovered.is_empty());
        // And the index was pruned of the orphan.
        assert!(a.read_index().unwrap().entries.is_empty());
    }
}
