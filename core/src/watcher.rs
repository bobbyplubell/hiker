//! Filesystem watcher for the open vault. See docs/watcher.md.
//!
//! Wraps notify-debouncer-full (which handles native event sources +
//! debouncing + rename pairing on Linux/macOS/Windows) and exposes a
//! normalized `FileEvent` stream via a tokio broadcast channel. Multiple
//! consumers (the indexer, the frontend bridge) can subscribe.
//!
//! Self-write suppression (`Watcher::suppress`) lets explicit-mutation paths
//! like `core::vault::move_note` and `create_note` register a short-lived TTL
//! on a vault-relative path; events for that path are dropped from the
//! normalized stream until the TTL expires. See `watcher-suppress-self-writes`
//! in docs/status.md.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use walkdir::WalkDir;

/// The concrete debouncer type. Shared (behind `Arc<Mutex<_>>`) between the
/// `Watcher` handle and the bridge thread so the latter can register watches
/// on directories that appear after startup.
type Deb = Debouncer<notify::RecommendedWatcher, notify_debouncer_full::RecommendedCache>;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);
const BROADCAST_CAPACITY: usize = 1024;
/// How long a `suppress(path)` registration stays effective. Sized to
/// outlast the debounce window (200ms) plus worst-case sqlite contention and
/// fs latency on slower machines.
const SUPPRESS_TTL: Duration = Duration::from_secs(2);

/// Map of vault-relative paths → instant of last suppression registration.
/// Shared between `Watcher::suppress` (writers) and the bridge thread
/// (filtering). Bounded by lazy eviction on every access.
type SuppressMap = Arc<Mutex<HashMap<String, Instant>>>;

/// Normalized filesystem event. Paths are vault-relative (forward-slash on
/// all platforms) so consumers don't need to know the vault root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileEvent {
    Created { path: String },
    Modified { path: String },
    Deleted { path: String },
    Renamed { from: String, to: String },
    Overflow,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("notify: {0}")]
    Notify(#[from] notify::Error),
}

/// Owns the debouncer + a background thread that translates raw events into
/// normalized `FileEvent`s and broadcasts them. Drop the watcher to stop
/// watching and close the broadcast channel.
pub struct Watcher {
    // The sole strong owner of the debouncer: when the `Watcher` drops, this
    // Arc's refcount hits zero (the bridge thread holds only a `Weak`), the
    // debouncer drops, its `raw_tx` closes, and the bridge thread's
    // `for batch in raw_rx` ends — so drop still stops watching cleanly.
    _debouncer: Arc<Mutex<Deb>>,
    tx: broadcast::Sender<FileEvent>,
    suppressed: SuppressMap,
}

impl Watcher {
    pub fn start(vault_root: impl Into<PathBuf>) -> Result<Self, Error> {
        let vault_root = vault_root.into();
        let (broadcast_tx, _) = broadcast::channel::<FileEvent>(BROADCAST_CAPACITY);
        let (raw_tx, raw_rx) = mpsc::channel::<DebounceEventResult>();
        let suppressed: SuppressMap = Arc::new(Mutex::new(HashMap::new()));

        let debouncer = Arc::new(Mutex::new(new_debouncer(DEBOUNCE_WINDOW, None, raw_tx)?));
        // Register a NonRecursive watch per non-ignored directory rather than
        // one Recursive watch on the root. Recursive mode would place inotify
        // watches on `target/`, `.git/`, `.hiker/`, etc., whose churn overflows
        // the kernel event queue; pruning at registration is the only thing
        // that actually prevents that. See `watch_tree`.
        watch_tree(
            &mut debouncer.lock().expect("debouncer lock poisoned"),
            &vault_root,
            &vault_root,
        )?;

        // Bridge thread: translates raw events into normalized form and
        // forwards into the broadcast channel. Owns no async runtime. It holds
        // a `Weak` to the debouncer (to register watches on new directories)
        // so it never keeps the debouncer — and thus the watch — alive past
        // the `Watcher` handle's own drop.
        let bcast = broadcast_tx.clone();
        let root_for_thread = vault_root.clone();
        let suppressed_for_thread = suppressed.clone();
        let debouncer_weak = Arc::downgrade(&debouncer);
        std::thread::spawn(move || {
            let wctx = WatcherCtx {
                vault_root: &root_for_thread,
                suppressed: &suppressed_for_thread,
                debouncer: debouncer_weak,
            };
            wctx.run_bridge_thread(raw_rx, &bcast);
        });

        Ok(Self {
            _debouncer: debouncer,
            tx: broadcast_tx,
            suppressed,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<FileEvent> {
        self.tx.subscribe()
    }

    /// Register a vault-relative path for self-write suppression. Events
    /// referencing this path emitted within `SUPPRESS_TTL` of this call are
    /// dropped before reaching subscribers. Call this immediately *before*
    /// performing the fs mutation so the entry is in place when notify
    /// surfaces the event after the debounce window.
    ///
    /// status: watcher-suppress-self-writes
    pub fn suppress(&self, rel_path: impl Into<String>) {
        let mut map = self.suppressed.lock().expect("suppress lock poisoned");
        map.insert(rel_path.into(), Instant::now());
        evict_expired(&mut map);
    }
}

/// Per-thread context bundling the vault root + suppress map. Methods on
/// this struct stay exempt from `single_call_fn` while sharing the watcher
/// dispatch state.
struct WatcherCtx<'a> {
    vault_root: &'a Path,
    suppressed: &'a SuppressMap,
    /// Weak handle to the shared debouncer, used to register watches on
    /// directories created after startup. `Weak` (not `Arc`) so the bridge
    /// thread never extends the debouncer's lifetime past the `Watcher` drop.
    debouncer: Weak<Mutex<Deb>>,
}

impl<'a> WatcherCtx<'a> {
    /// Bridge thread body: pull raw debounced batches off `raw_rx`, normalize
    /// each, drop suppressed self-writes, and forward survivors into
    /// `bcast`. Holds no async state — owns no runtime.
    fn run_bridge_thread(
        &self,
        raw_rx: std::sync::mpsc::Receiver<notify_debouncer_full::DebounceEventResult>,
        bcast: &broadcast::Sender<FileEvent>,
    ) {
        for batch in raw_rx {
            let events = match batch {
                Ok(evs) => evs,
                Err(errs) => {
                    for err in errs {
                        tracing::error!(error = %err, "watcher: notify error");
                    }
                    continue;
                }
            };
            for ev in events {
                self.dispatch_one(&ev, bcast);
            }
        }
    }

    fn dispatch_one(&self, ev: &DebouncedEvent, bcast: &broadcast::Sender<FileEvent>) {
        if ev.event.need_rescan() {
            tracing::warn!("watcher: kernel reported event-queue overflow");
            let _ = bcast.send(FileEvent::Overflow);
            return;
        }
        let Some(file_event) = self.normalize(ev) else { return };
        if self.is_suppressed_event(&file_event) {
            tracing::debug!(event = ?file_event, "watcher: suppressed self-write");
            return;
        }
        // A directory that just appeared needs its own NonRecursive watch (and
        // its existing contents surfaced) — see `register_new_subtree`.
        self.register_new_subtree(&file_event, bcast);
        tracing::debug!(event = ?file_event, "watcher: debounced event");
        // send returns Err when no receivers; that's fine.
        let _ = bcast.send(file_event);
    }

    /// When a directory is created or moved into the vault, register a watch on
    /// it and every non-ignored subdir — recursive watching is deliberately not
    /// used (we prune ignored subtrees at registration), so notify won't pick
    /// up new directories on its own. Also surface any files ALREADY inside it
    /// as `Created`: a bulk `mv folder/` into the vault yields a single inotify
    /// event for the directory and none for its contents, so without this the
    /// moved-in notes would stay unindexed until the next full scan. A no-op for
    /// non-directory events.
    fn register_new_subtree(&self, ev: &FileEvent, bcast: &broadcast::Sender<FileEvent>) {
        let rel = match ev {
            FileEvent::Created { path } => path,
            FileEvent::Renamed { to, .. } => to,
            _ => return,
        };
        let abs = self.vault_root.join(rel);
        if !abs.is_dir() {
            return;
        }
        if let Some(debouncer) = self.debouncer.upgrade() {
            match debouncer.lock() {
                Ok(mut guard) => {
                    if let Err(e) = watch_tree(&mut guard, self.vault_root, &abs) {
                        tracing::warn!(error = %e, dir = %rel, "watcher: failed to watch new directory");
                    }
                }
                Err(_) => return,
            }
        } else {
            // Watcher has been dropped; nothing to register against.
            return;
        }
        for entry in WalkDir::new(&abs)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| should_watch(self.vault_root, e.path()))
        {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() {
                continue;
            }
            let Some(child) = to_rel(self.vault_root, entry.path()) else { continue };
            if is_ignored(&child) {
                continue;
            }
            let _ = bcast.send(FileEvent::Created { path: child });
        }
    }

    /// Drop a normalized event if any of its referenced paths is currently
    /// suppressed. Renames suppress on either side: callers register both
    /// `from` and `to`, so seeing a fresh registration on either is enough.
    fn is_suppressed_event(&self, ev: &FileEvent) -> bool {
        let mut guard = self.suppressed.lock().expect("suppress lock poisoned");
        evict_expired(&mut guard);
        match ev {
            FileEvent::Created { path }
            | FileEvent::Modified { path }
            | FileEvent::Deleted { path } => guard.contains_key(path),
            FileEvent::Renamed { from, to } => {
                guard.contains_key(from) || guard.contains_key(to)
            }
            FileEvent::Overflow => false,
        }
    }

    /// status: watcher-symlink-policy
    ///
    /// Test whether any existing ancestor of `abs_path` (under the vault
    /// root) is a symlink. Components above the vault root are ignored —
    /// the root was canonicalized at vault open. Non-existent leaves
    /// (typical on Deleted events) walk fine: `symlink_metadata` errors
    /// stop the walk early.
    fn has_symlink_ancestor(&self, abs_path: &Path) -> bool {
        let mut current = PathBuf::new();
        for comp in abs_path.components() {
            current.push(comp);
            if !current.starts_with(self.vault_root) || current == self.vault_root {
                continue;
            }
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => return true,
                Ok(_) => {}
                // Component doesn't exist on disk (typical mid-rename /
                // deleted path). The rest of the path can't exist either;
                // stop walking.
                Err(_) => break,
            }
        }
        false
    }

    /// Translate one debounced raw event into our normalized form. Returns
    /// None if the event is filtered (ignore list, unknown kind, paths
    /// outside root, or paths whose existing ancestors include a symlink —
    /// see `watcher-symlink-policy`; notify follows symlinks platform-
    /// dependently when watching recursively, so we drop those events at
    /// the normalize step so the indexer never sees content from outside
    /// the canonical vault tree).
    fn normalize(&self, ev: &DebouncedEvent) -> Option<FileEvent> {
        let vault_root = self.vault_root;
        let paths = &ev.paths;
        if paths.iter().any(|p| self.has_symlink_ancestor(p)) {
            return None;
        }
        match ev.event.kind {
            EventKind::Create(_) => {
                let p = paths.first()?;
                let rel = to_rel(vault_root, p)?;
                (!is_ignored(&rel)).then_some(FileEvent::Created { path: rel })
            }
            EventKind::Modify(notify::event::ModifyKind::Name(mode)) => {
                // notify-debouncer-full pairs From+To into a single
                // `Name(Both)` event with paths=[from, to]; unpaired sides
                // surface as `Name(From)` (deleted source) or `Name(To)`
                // (created destination).
                use notify::event::RenameMode;
                match mode {
                    RenameMode::Both if paths.len() >= 2 => {
                        let from = to_rel(vault_root, &paths[0])?;
                        let to = to_rel(vault_root, &paths[1])?;
                        if is_ignored(&from) && is_ignored(&to) {
                            return None;
                        }
                        Some(FileEvent::Renamed { from, to })
                    }
                    RenameMode::From => {
                        let p = paths.first()?;
                        let rel = to_rel(vault_root, p)?;
                        (!is_ignored(&rel)).then_some(FileEvent::Deleted { path: rel })
                    }
                    RenameMode::To => {
                        let p = paths.first()?;
                        let rel = to_rel(vault_root, p)?;
                        (!is_ignored(&rel)).then_some(FileEvent::Created { path: rel })
                    }
                    // RenameMode::Any / Other / Both-without-2-paths:
                    // best-effort. The two explicit cases above
                    // (From/To/Both) cover every platform where notify
                    // reports a definite rename direction. This fallback
                    // inherits the original paths[0]=from / paths[1]=to
                    // ordering assumption — fine for the common cases (the
                    // platforms hitting this branch tend to mirror what
                    // Both would give) but technically still ambiguous;
                    // treat the lone-path case as Modified rather than
                    // guessing direction.
                    _ => self.normalize_rename_fallback(ev),
                }
            }
            EventKind::Modify(_) => {
                let p = paths.first()?;
                let rel = to_rel(vault_root, p)?;
                (!is_ignored(&rel)).then_some(FileEvent::Modified { path: rel })
            }
            EventKind::Remove(_) => {
                let p = paths.first()?;
                let rel = to_rel(vault_root, p)?;
                (!is_ignored(&rel)).then_some(FileEvent::Deleted { path: rel })
            }
            _ => None,
        }
    }

    fn normalize_rename_fallback(&self, ev: &DebouncedEvent) -> Option<FileEvent> {
        let vault_root = self.vault_root;
        let paths = &ev.paths;
        if paths.len() >= 2 {
            let from = to_rel(vault_root, &paths[0])?;
            let to = to_rel(vault_root, &paths[1])?;
            if is_ignored(&from) && is_ignored(&to) {
                return None;
            }
            Some(FileEvent::Renamed { from, to })
        } else {
            let p = paths.first()?;
            let rel = to_rel(vault_root, p)?;
            (!is_ignored(&rel)).then_some(FileEvent::Modified { path: rel })
        }
    }
}

fn evict_expired(map: &mut HashMap<String, Instant>) {
    let now = Instant::now();
    map.retain(|_, t| now.duration_since(*t) < SUPPRESS_TTL);
}

/// Register a NonRecursive watch on `dir` and every non-ignored directory
/// beneath it. Pruning ignored directories (`target/`, `.git/`, `.hiker/`, …)
/// at REGISTRATION — rather than filtering their events after the kernel has
/// already queued them — is what actually stops inotify's event queue from
/// overflowing when a high-churn tree (a Rust `target/` with 600k+ files, a
/// busy `.git/`) sits under the vault root. Recursive mode is deliberately not
/// used: it would watch every ignored subtree. Symlinked directories are not
/// followed (`follow_links(false)` → they surface as non-dir entries and are
/// never watched), matching `watcher-symlink-policy`.
fn watch_tree(debouncer: &mut Deb, vault_root: &Path, dir: &Path) -> Result<(), Error> {
    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| should_watch(vault_root, e.path()))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "watcher: skipping unreadable entry during watch registration");
                continue;
            }
        };
        if entry.file_type().is_dir() {
            debouncer.watch(entry.path(), RecursiveMode::NonRecursive)?;
        }
    }
    Ok(())
}

/// Walk/registration predicate: keep (and, for directories, descend into)
/// every entry except the ignored set. The vault root itself is always kept.
/// Shared by the startup registration walk and the new-subtree walk so both
/// prune identically.
fn should_watch(vault_root: &Path, path: &Path) -> bool {
    if path == vault_root {
        return true;
    }
    match to_rel(vault_root, path) {
        Some(rel) => !is_ignored(&rel),
        None => false,
    }
}

/// Hard-coded ignore list. Mirrors docs/watcher.md.
pub fn is_ignored(rel: &str) -> bool {
    // Everything under `.hiker/` is ignored — nothing indexable lives there
    // anymore. Chat sessions live in the visible `<chats_dir>/` folder and
    // accepted trails in a trail-doc's visible companion folder
    // (`subsystem-notes-visible` in `design.md`); trail *drafts*
    // (`.hiker/trails/drafts/`) are pre-acceptance machinery and stay
    // unindexed by design. No per-subsystem carve-out is needed.
    if rel.starts_with(".hiker/") || rel == ".hiker" {
        return true;
    }
    if rel.starts_with(".git/") || rel == ".git" {
        return true;
    }
    // Build-artifact / dependency directories. These can hold hundreds of
    // thousands of files (a Rust `target/` for this project tops 600k+
    // entries) and a single full scan over them allocates a path String
    // per file plus an IndexJob enum per `.md`/`.txt` survivor. The
    // watcher also fires events for every cargo build, which would queue
    // a FullScan via Overflow. Skipping these directories outright is
    // the largest single memory + CPU win for projects-as-vaults.
    //
    // We match by path component so a folder NAMED `target` anywhere in
    // the tree is ignored, not just at the root (a JS monorepo can have
    // a `packages/foo/node_modules/`). Match against the full segment to
    // avoid false positives like `targeting.md`.
    for component in rel.split('/') {
        match component {
            "target" | "node_modules" | ".venv" | "venv" | "__pycache__"
            | "dist" | "build" | "out" | ".cache" | ".next" | ".parcel-cache"
            | ".turbo" | ".gradle" | ".idea" | ".vscode" | ".tox"
            | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" => return true,
            _ => {}
        }
    }
    let last = rel.rsplit('/').next().unwrap_or(rel);
    if last.starts_with(".#") || last.starts_with("4913") {
        return true;
    }
    if last.ends_with(".tmp") || last.ends_with(".swp") || last.ends_with('~') {
        return true;
    }
    if last == ".DS_Store" {
        return true;
    }
    // Top-level dotfiles other than markdown are skipped; nested dotfiles
    // are handled via the .hiker/.git rules above.
    if !rel.contains('/') && rel.starts_with('.') && !rel.ends_with(".md") {
        return true;
    }
    false
}

/// Convert an absolute path under `vault_root` into a vault-relative,
/// forward-slash path. Returns None if the path is outside the vault.
fn to_rel(vault_root: &Path, abs: &Path) -> Option<String> {
    let stripped = abs.strip_prefix(vault_root).ok()?;
    let mut out = String::new();
    for (i, comp) in stripped.components().enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(&comp.as_os_str().to_string_lossy());
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;
    use tempfile::tempdir;
    use tokio::time::timeout;

    #[test]
    fn ignore_list_covers_documented_paths() {
        assert!(is_ignored(".hiker/index.db"));
        assert!(is_ignored(".hiker"));
        assert!(is_ignored(".git/HEAD"));
        assert!(is_ignored("note.md.tmp"));
        assert!(is_ignored("note.md.swp"));
        assert!(is_ignored("note~"));
        assert!(is_ignored(".DS_Store"));
        assert!(is_ignored(".#emacs-lock"));
        assert!(is_ignored("4913"));
        assert!(is_ignored(".obsidian"));

        assert!(!is_ignored("note.md"));
        assert!(!is_ignored("project/notes.md"));
        assert!(!is_ignored("inbox/today.md"));
        // Sessions are now visible notes in `chats/`, indexed like any
        // other note — not under `.hiker/`.
        assert!(!is_ignored("chats/2026-05-01-abc.md"));
        assert!(!is_ignored("chats/imported/claude-code-2026-05-01-xyz.md"));
        // No subsystem carve-out: everything under `.hiker/` is ignored,
        // including the former sessions/trails locations. Trail drafts at
        // `.hiker/trails/drafts/` are correctly unindexed (pre-acceptance
        // machinery).
        assert!(is_ignored(".hiker/sessions/2026-05-01-abc.md"));
        assert!(is_ignored(".hiker/trails/01HRX/waypoints/0001--note.md"));
        assert!(is_ignored(".hiker/trails/drafts/01DRAFT.md"));
        // Hidden-dir contents are NOT ignored unless under .hiker/ or .git/.
        // That's intentional: a user vault might have legitimate dotted
        // subdirs we shouldn't silently skip beyond the documented two.
        assert!(!is_ignored(".obsidian/config.json"));

        // Build-artifact / dependency directories. These can hold
        // hundreds of thousands of files and quickly OOM the indexer
        // when a project root is opened as a vault.
        assert!(is_ignored("target/debug/build/something.rs"));
        assert!(is_ignored("packages/foo/node_modules/react/index.js"));
        assert!(is_ignored(".venv/lib/python3.12/site-packages/x.py"));
        assert!(is_ignored("dist/index.html"));
        assert!(is_ignored("build/output.bin"));
        assert!(is_ignored(".next/server/pages/_app.js"));
        assert!(is_ignored("__pycache__/foo.cpython-312.pyc"));
        // Exact-segment match: `targeting.md` is NOT under a `target/`.
        assert!(!is_ignored("targeting.md"));
        assert!(!is_ignored("a/targeting/notes.md"));
        // But a folder literally named `target` anywhere in the tree is.
        assert!(is_ignored("a/target/b.md"));
    }

    /// Integration test: write a file, expect a Modified or Created event.
    #[tokio::test]
    async fn watcher_picks_up_writes() {
        let dir = tempdir().unwrap();
        let watcher = Watcher::start(dir.path()).unwrap();
        let mut rx = watcher.subscribe();

        // Give the watcher a moment to register before we trigger.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let target = dir.path().join("note.md");
        fs::write(&target, b"hello").unwrap();

        let started = Instant::now();
        let mut saw_event = false;
        // Allow up to 2s for the debounced event to arrive.
        while started.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let p = match &ev {
                        FileEvent::Created { path } => path,
                        FileEvent::Modified { path } => path,
                        _ => continue,
                    };
                    if p == "note.md" {
                        saw_event = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_event, "expected a Created/Modified event for note.md");
    }

    #[tokio::test]
    async fn suppress_swallows_event_for_path_within_ttl() {
        let dir = tempdir().unwrap();
        let watcher = Watcher::start(dir.path()).unwrap();
        let mut rx = watcher.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Suppress before the write — the resulting event must not surface.
        watcher.suppress("hidden.md");
        fs::write(dir.path().join("hidden.md"), b"x").unwrap();
        // Also a non-suppressed write so we have something positive to wait on.
        fs::write(dir.path().join("visible.md"), b"y").unwrap();

        let started = Instant::now();
        let mut saw_visible = false;
        while started.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let p = match &ev {
                        FileEvent::Created { path } | FileEvent::Modified { path } => path.clone(),
                        _ => continue,
                    };
                    assert_ne!(p, "hidden.md", "suppressed path leaked: {ev:?}");
                    if p == "visible.md" {
                        saw_visible = true;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_visible, "expected to see the unsuppressed write");
    }

    #[cfg(unix)]
    #[test]
    fn has_symlink_ancestor_detects_symlinked_dir_inside_vault() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(outside.path().join("d")).unwrap();
        fs::write(outside.path().join("d/leaf.md"), b"x").unwrap();
        let vault_root = dir.path().canonicalize().unwrap();
        symlink(outside.path().join("d"), vault_root.join("linked")).unwrap();
        // Path under the in-vault symlink → must be detected.
        let suppressed_for_test = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let ctx = WatcherCtx {
            vault_root: &vault_root,
            suppressed: &suppressed_for_test,
            debouncer: Weak::new(),
        };
        assert!(ctx.has_symlink_ancestor(&vault_root.join("linked").join("leaf.md")));
        // Path through a real directory → must not be flagged.
        fs::create_dir(vault_root.join("real")).unwrap();
        fs::write(vault_root.join("real/leaf.md"), b"y").unwrap();
        assert!(!ctx.has_symlink_ancestor(&vault_root.join("real").join("leaf.md")));
        // Non-existent leaf below a real dir → still not flagged.
        assert!(!ctx.has_symlink_ancestor(&vault_root.join("real").join("never-existed.md")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_drops_events_for_symlinked_paths() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(outside.path().join("d")).unwrap();
        // The vault root needs to be canonicalized for `has_symlink_ancestor`'s
        // starts_with check; tempdir paths are already canonical on Linux.
        let vault_root = dir.path().canonicalize().unwrap();
        symlink(outside.path().join("d"), vault_root.join("linked")).unwrap();

        let watcher = Watcher::start(&vault_root).unwrap();
        let mut rx = watcher.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Write through the symlink — events for paths under `linked/` must
        // be dropped so the indexer never sees content from outside the
        // canonical vault tree.
        fs::write(outside.path().join("d/leaked.md"), b"shh").unwrap();
        // Drive a positive control so we can stop waiting cleanly.
        fs::write(vault_root.join("real.md"), b"y").unwrap();

        let started = Instant::now();
        let mut saw_real = false;
        while started.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let p = match &ev {
                        FileEvent::Created { path } | FileEvent::Modified { path } => path.clone(),
                        FileEvent::Deleted { path } => path.clone(),
                        FileEvent::Renamed { to, .. } => to.clone(),
                        FileEvent::Overflow => continue,
                    };
                    assert!(
                        !p.starts_with("linked"),
                        "watcher leaked symlinked-path event: {ev:?}",
                    );
                    if p == "real.md" {
                        saw_real = true;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_real, "expected at least one event for real.md");
    }

    /// A directory created AFTER startup must get its own watch (we register
    /// per-dir NonRecursive, not one recursive watch), so a file written inside
    /// the new subdir still surfaces an event.
    #[tokio::test]
    async fn watcher_picks_up_writes_in_subdir_created_after_start() {
        let dir = tempdir().unwrap();
        let watcher = Watcher::start(dir.path()).unwrap();
        let mut rx = watcher.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create the subdir, let its Created event register a watch, then write
        // a note inside it.
        fs::create_dir(dir.path().join("sub")).unwrap();
        tokio::time::sleep(Duration::from_millis(350)).await;
        fs::write(dir.path().join("sub/note.md"), b"hi").unwrap();

        let started = Instant::now();
        let mut saw = false;
        while started.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let p = match &ev {
                        FileEvent::Created { path } | FileEvent::Modified { path } => path.clone(),
                        _ => continue,
                    };
                    if p == "sub/note.md" {
                        saw = true;
                        break;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw, "expected an event for a note in a subdir created post-startup");
    }

    /// A folder moved into the vault as a unit (one inotify event for the dir,
    /// none for its pre-existing contents) must still surface its files as
    /// Created so they get indexed.
    #[tokio::test]
    async fn watcher_surfaces_files_of_moved_in_folder() {
        let dir = tempdir().unwrap();
        let watcher = Watcher::start(dir.path()).unwrap();
        let mut rx = watcher.subscribe();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Build a populated folder OUTSIDE the vault, then rename it in as a
        // unit — the contents already exist, so only the dir move is observed.
        let staging = tempdir().unwrap();
        fs::create_dir(staging.path().join("incoming")).unwrap();
        fs::write(staging.path().join("incoming/a.md"), b"a").unwrap();
        fs::write(staging.path().join("incoming/b.md"), b"b").unwrap();
        fs::rename(staging.path().join("incoming"), dir.path().join("incoming")).unwrap();

        let started = Instant::now();
        let mut saw_a = false;
        let mut saw_b = false;
        while started.elapsed() < Duration::from_secs(2) && !(saw_a && saw_b) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    if let FileEvent::Created { path } | FileEvent::Modified { path } = &ev {
                        if path == "incoming/a.md" {
                            saw_a = true;
                        }
                        if path == "incoming/b.md" {
                            saw_b = true;
                        }
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_a && saw_b, "expected Created for both files of the moved-in folder (a={saw_a}, b={saw_b})");
    }

    #[tokio::test]
    async fn watcher_ignores_hiker_dir() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".hiker")).unwrap();
        let watcher = Watcher::start(dir.path()).unwrap();
        let mut rx = watcher.subscribe();

        tokio::time::sleep(Duration::from_millis(50)).await;
        fs::write(dir.path().join(".hiker/index.db"), b"x").unwrap();
        // Also write a regular file so we have something positive to wait on.
        fs::write(dir.path().join("real.md"), b"y").unwrap();

        let started = Instant::now();
        let mut saw_real = false;
        while started.elapsed() < Duration::from_secs(2) {
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Ok(ev)) => {
                    let p = match &ev {
                        FileEvent::Created { path } => path.clone(),
                        FileEvent::Modified { path } => path.clone(),
                        _ => continue,
                    };
                    // No event should ever reference .hiker/.
                    assert!(
                        !p.starts_with(".hiker"),
                        "watcher leaked .hiker event: {ev:?}",
                    );
                    if p == "real.md" {
                        saw_real = true;
                    }
                }
                _ => continue,
            }
        }
        assert!(saw_real, "expected at least one event for real.md");
    }
}
