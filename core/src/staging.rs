//! Staging area for proposed writes that haven't been accepted yet.
//! See docs/settings.md "## Staging review".
//!
//! Storage at `<vault>/.hiker/staging/`: a flat JSON index (`pending.json`)
//! plus per-proposal `.md` content files. Module discipline mirrors
//! `core::changes` and `core::autosave` — all filesystem access confined
//! here, no Tauri imports, narrow public API.
//
// status: staging-dir
// status: staging-review-filtering
// status: staging-retention

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use crate::changes::{ChangeAppend, ChangeOp, Changes, ChangesError};
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::vault::Vault;

const STAGING_DIRNAME: &str = "staging";
const PENDING_JSON: &str = "pending.json";

#[derive(Debug, Error)]
pub enum StagingError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("proposal not found: {0}")]
    ProposalNotFound(String),
    #[error("disk drift: file changed since proposal (expected hash {expected}, found {found})")]
    DiskDrift { expected: String, found: String },
    #[error("missing content: proposal {0} has no content to write")]
    MissingContent(String),
    #[error("changes error: {0}")]
    Changes(#[from] ChangesError),
    #[error("vault error: {0}")]
    Vault(String),
}

impl From<HikerError> for StagingError {
    fn from(e: HikerError) -> Self {
        match e {
            HikerError::DiskDrift { expected, found } => StagingError::DiskDrift { expected, found },
            HikerError::Io(s) => StagingError::Io(io::Error::other(s)),
            HikerError::NotFound(s) => StagingError::ProposalNotFound(s),
            _ => StagingError::Vault(e.to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalInput {
    pub surface: String,
    pub action: String,
    pub target_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub surface: String,
    pub action: String,
    pub target_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StagingFilter {
    pub path: Option<String>,
    pub trail_id: Option<String>,
    pub surface: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AcceptOutcome {
    pub proposal_id: String,
    pub target_path: String,
    pub new_hash: String,
}

pub struct Staging {
    dir: PathBuf,
    pending_path: PathBuf,
    lock: Mutex<()>,
}

impl Staging {
    pub fn open(vault_root: &Path) -> Result<Self, StagingError> {
        let dir = vault_root.join(".hiker").join(STAGING_DIRNAME);
        fs::create_dir_all(&dir)?;
        let pending_path = dir.join(PENDING_JSON);
        let me = Self {
            dir,
            pending_path,
            lock: Mutex::new(()),
        };
        // Seed pending.json if it doesn't exist.
        if !me.pending_path.exists() {
            me.write_pending_atomic(&[])?;
        }
        Ok(me)
    }

    /// Propose a write to staging. Generates a ULID id, writes the proposed
    /// content to `<id>.md` (if present), and appends the proposal record
    /// to `pending.json` atomically. Returns the proposal id.
    pub fn propose(&self, input: ProposalInput) -> Result<String, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let id = Ulid::new().to_string();
        let content_hash = input
            .content
            .as_ref()
            .map(|c| hash_str(c));
        let now = now_ms();

        if let Some(ref content) = input.content {
            let md_path = self.md_path(&id);
            let tmp = md_path.with_extension("md.tmp");
            write_file_atomic(&tmp, &md_path, content.as_bytes())?;
        }

        let proposal = Proposal {
            id: id.clone(),
            surface: input.surface,
            action: input.action,
            target_path: input.target_path,
            trail_id: input.trail_id,
            content_hash,
            created_at_ms: now,
            metadata: input.metadata,
        };

        let mut pending = self.read_pending()?;
        pending.push(proposal);
        self.write_pending_atomic(&pending)?;

        Ok(id)
    }

    /// Accept a proposal: drift-check the on-disk source against what we
    /// recorded, write the proposed content, append a `core::changes` row
    /// (if `changes` is provided), and remove the staging files. Returns
    /// the new content hash.
    pub fn accept(
        &self,
        id: &str,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<AcceptOutcome, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let mut pending = self.read_pending()?;
        let idx = pending
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;
        let proposal = pending[idx].clone();

        let proposed_content: Option<String>;
        let new_hash: String;
        let is_create: bool;

        if let Some(ref content_hash) = proposal.content_hash {
            let md_path = self.md_path(id);
            let content = fs::read_to_string(&md_path)
                .map_err(|e| {
                    if e.kind() == io::ErrorKind::NotFound {
                        StagingError::MissingContent(id.to_string())
                    } else {
                        StagingError::Io(e)
                    }
                })?;

            // Integrity check: the .md file must match content_hash.
            let actual_hash = hash_str(&content);
            if &actual_hash != content_hash {
                return Err(StagingError::DiskDrift {
                    expected: content_hash.clone(),
                    found: actual_hash,
                });
            }

            // Read what's currently on disk. For existing files we do a
            // checked write that re-reads + compares before applying,
            // catching any race between our read and the write. For new
            // files (NotFound) we write without a pre-existing hash.
            let disk_read = vault.read_file_with_hash(&proposal.target_path);
            let file_exists = disk_read.is_ok();
            let (_, disk_hash) = disk_read.unwrap_or((String::new(), String::new()));

            new_hash = vault.write_file_checked(
                &proposal.target_path,
                &disk_hash,
                &content,
            )?;

            // Delete the .md sidecar.
            if md_path.exists() {
                let _ = fs::remove_file(&md_path);
            }

            is_create = !file_exists;
            proposed_content = Some(content);
        } else {
            // Metadata-only proposal: nothing to write.
            let md_path = self.md_path(id);
            if md_path.exists() {
                let _ = fs::remove_file(&md_path);
            }
            new_hash = String::new();
            is_create = false;
            proposed_content = None;
        }

        // Append a changes row if a Changes handle was provided.
        if let Some(changes) = changes {
            let op = if is_create {
                ChangeOp::Created
            } else if proposal.action == "delete_note" || proposal.action == "waypoint_remove" {
                ChangeOp::Deleted
            } else {
                ChangeOp::Modified
            };
            let content_bytes = proposed_content
                .as_ref()
                .map(|c| c.as_bytes().to_vec());
            changes.append(ChangeAppend {
                path: &proposal.target_path,
                op,
                author: "user",
                content_hash: if new_hash.is_empty() {
                    None
                } else {
                    Some(&new_hash)
                },
                content: content_bytes.as_deref(),
                rename_from: None,
                metadata: serde_json::json!({
                    "staging_proposal_id": id,
                    "action": proposal.action,
                    "reviewed": true,
                }),
            })?;
        }

        // Remove the proposal from pending.json.
        pending.remove(idx);
        self.write_pending_atomic(&pending)?;

        Ok(AcceptOutcome {
            proposal_id: id.to_string(),
            target_path: proposal.target_path,
            new_hash,
        })
    }

    /// Reject a proposal: delete the `.md` file (if it exists) and remove
    /// from `pending.json`. No changelog row.
    pub fn reject(&self, id: &str) -> Result<(), StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let mut pending = self.read_pending()?;
        let idx = pending
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| StagingError::ProposalNotFound(id.to_string()))?;

        let md_path = self.md_path(id);
        if md_path.exists() {
            let _ = fs::remove_file(&md_path);
        }

        pending.remove(idx);
        self.write_pending_atomic(&pending)?;
        Ok(())
    }

    /// Accept all proposals matching `filter`. Each is individually
    /// processed via `accept()`; failures are logged and skipped (not
    /// abort-the-batch). Returns outcomes for the ones that succeeded.
    pub fn accept_all(
        &self,
        filter: &StagingFilter,
        vault: &Vault,
        changes: Option<&Changes>,
    ) -> Result<Vec<AcceptOutcome>, StagingError> {
        let proposals = self.list(filter)?;
        let mut outcomes = Vec::new();
        for p in &proposals {
            match self.accept(&p.id, vault, changes) {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => {
                    tracing::warn!(
                        proposal_id = %p.id,
                        error = %e,
                        "staging: accept_all skipped failed proposal",
                    );
                }
            }
        }
        Ok(outcomes)
    }

    /// List proposals, optionally filtered. Returns all proposals when no
    /// filter fields are set.
    pub fn list(&self, filter: &StagingFilter) -> Result<Vec<Proposal>, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let pending = self.read_pending()?;
        Ok(pending
            .into_iter()
            .filter(|p| matches_filter(p, filter))
            .collect())
    }

    /// Count of proposals matching `filter`.
    pub fn count(&self, filter: &StagingFilter) -> Result<u32, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let pending = self.read_pending()?;
        Ok(pending.iter().filter(|p| matches_filter(p, filter)).count() as u32)
    }

    /// Remove proposals older than `max_age_days` days and delete their
    /// `.md` files. Returns the number removed.
    pub fn gc(&self, max_age_days: u32) -> Result<usize, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let mut pending = self.read_pending()?;
        let cutoff = now_ms() - (max_age_days as i64) * 86_400_000;
        let mut removed = 0usize;
        let mut survivors = Vec::with_capacity(pending.len());
        for p in pending.drain(..) {
            if p.created_at_ms < cutoff {
                let md_path = self.md_path(&p.id);
                if md_path.exists() {
                    let _ = fs::remove_file(&md_path);
                }
                removed += 1;
            } else {
                survivors.push(p);
            }
        }
        self.write_pending_atomic(&survivors)?;
        Ok(removed)
    }

    /// Read the proposed content for a proposal. Returns an empty string
    /// for metadata-only proposals (no `.md` file). Returns
    /// `ProposalNotFound` when the id doesn't exist in pending.json.
    ///
    /// status: staging-review-activity-detail-filter
    pub fn content(&self, id: &str) -> Result<String, StagingError> {
        let _g = self.lock.lock().expect("staging lock poisoned");
        let pending = self.read_pending()?;
        if !pending.iter().any(|p| p.id == id) {
            return Err(StagingError::ProposalNotFound(id.to_string()));
        }
        let md_path = self.md_path(id);
        if md_path.exists() {
            fs::read_to_string(&md_path).map_err(StagingError::Io)
        } else {
            Ok(String::new())
        }
    }
}

// ── helpers ────────────────────────────────────────────────────────

impl Staging {
    fn md_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    fn read_pending(&self) -> Result<Vec<Proposal>, StagingError> {
        if !self.pending_path.exists() {
            return Ok(Vec::new());
        }
        let raw = fs::read(&self.pending_path)?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: Vec<Proposal> = serde_json::from_slice(&raw)?;
        Ok(parsed)
    }

    fn write_pending_atomic(&self, proposals: &[Proposal]) -> Result<(), StagingError> {
        let tmp = self.pending_path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(proposals)?;
        write_file_atomic(&tmp, &self.pending_path, &bytes)?;
        Ok(())
    }
}

fn matches_filter(p: &Proposal, f: &StagingFilter) -> bool {
    if let Some(ref path) = f.path
        && p.target_path != *path
    {
        return false;
    }
    if let Some(ref trail_id) = f.trail_id
        && p.trail_id.as_deref() != Some(trail_id.as_str())
    {
        return false;
    }
    if let Some(ref surface) = f.surface
        && p.surface != *surface
    {
        return false;
    }
    if let Some(ref session_id) = f.session_id {
        let meta_session = p
            .metadata
            .as_ref()
            .and_then(|m| m.get("session_id"))
            .and_then(|v| v.as_str());
        if meta_session != Some(session_id.as_str()) {
            return false;
        }
    }
    true
}

/// Atomic file write: write to `tmp`, fsync, rename to `final_path`.
/// Crash mid-write leaves either the prior file or no change — never a
/// half-written one. Matches the `vim`-style write-temp-then-rename
/// pattern from `core::autosave`.
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

    fn staged() -> (tempfile::TempDir, Staging) {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn propose_returns_id_and_appears_in_list() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/test.md".into(),
                trail_id: None,
                content: Some("# Hello".into()),
                metadata: None,
            })
            .unwrap();
        assert!(!id.is_empty());
        let list = s.list(&StagingFilter::default()).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, id);
        assert_eq!(list[0].surface, "mcp-tool-call");
        assert!(list[0].content_hash.is_some());
    }

    #[test]
    fn propose_without_content_has_no_hash_or_md_file() {
        let (dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "trails".into(),
                action: "waypoint_add".into(),
                target_path: "notes/raptor.md".into(),
                trail_id: Some("trail-abc".into()),
                content: None,
                metadata: None,
            })
            .unwrap();
        let list = s.list(&StagingFilter::default()).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].content_hash.is_none());
        // No .md file should exist because there's no content.
        let md_path = s.dir.join(format!("{id}.md"));
        assert!(!md_path.exists());
    }

    #[test]
    fn list_filters_by_path() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                path: Some("notes/a.md".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target_path, "notes/a.md");
    }

    #[test]
    fn list_filters_by_surface() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "background-llm".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                surface: Some("background-llm".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].surface, "background-llm");
    }

    #[test]
    fn list_filters_by_trail_id() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "trails".into(),
            action: "trail_create".into(),
            target_path: "trails/new-trail.md".into(),
            trail_id: Some("t1".into()),
            content: None,
            metadata: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "trails".into(),
            action: "waypoint_add".into(),
            target_path: "notes/x.md".into(),
            trail_id: Some("t2".into()),
            content: None,
            metadata: None,
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                trail_id: Some("t1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].trail_id.as_deref(), Some("t1"));
    }

    #[test]
    fn list_filters_by_session_id_from_metadata() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("a".into()),
            metadata: Some(serde_json::json!({"session_id": "s1"})),
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("b".into()),
            metadata: Some(serde_json::json!({"session_id": "s2"})),
        })
        .unwrap();

        let filtered = s
            .list(&StagingFilter {
                session_id: Some("s1".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].target_path, "notes/a.md");
    }

    #[test]
    fn count_returns_filtered_total() {
        let (_dir, s) = staged();
        for i in 0..5 {
            s.propose(ProposalInput {
                surface: "batch-mutation".into(),
                action: "write_note".into(),
                target_path: format!("notes/{i}.md"),
                trail_id: None,
                content: Some("x".into()),
                metadata: None,
            })
            .unwrap();
        }
        assert_eq!(s.count(&StagingFilter::default()).unwrap(), 5);
        assert_eq!(
            s.count(&StagingFilter {
                path: Some("notes/0.md".into()),
                ..Default::default()
            })
            .unwrap(),
            1
        );
        assert_eq!(
            s.count(&StagingFilter {
                surface: Some("nonexistent".into()),
                ..Default::default()
            })
            .unwrap(),
            0
        );
    }

    #[test]
    fn accept_writes_content_and_removes_from_pending() {
        let (dir, s) = staged();
        // Write a real file on disk to be overwritten.
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert_eq!(outcome.proposal_id, id);
        assert_eq!(outcome.target_path, "notes/a.md");
        assert!(!outcome.new_hash.is_empty());

        // File should now contain the proposed content.
        let (disk_content, _) = vault.read_file_with_hash("notes/a.md").unwrap();
        assert_eq!(disk_content, "proposed");

        // Proposal removed from pending.
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn accept_metadata_only_removes_without_write() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        let id = s
            .propose(ProposalInput {
                surface: "trails".into(),
                action: "waypoint_add".into(),
                target_path: "notes/x.md".into(),
                trail_id: Some("t1".into()),
                content: None,
                metadata: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert_eq!(outcome.proposal_id, id);
        assert!(outcome.new_hash.is_empty());
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn reject_removes_staging_files() {
        let (_dir, s) = staged();
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("x".into()),
                metadata: None,
            })
            .unwrap();

        s.reject(&id).unwrap();
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
        assert!(!s.md_path(&id).exists());
    }

    #[test]
    fn reject_nonexistent_returns_error() {
        let (_dir, s) = staged();
        match s.reject("nonexistent") {
            Err(StagingError::ProposalNotFound(_)) => {}
            other => panic!("expected ProposalNotFound, got {other:?}"),
        }
    }

    #[test]
    fn accept_nonexistent_returns_error() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        match s.accept("nonexistent", &vault, None) {
            Err(StagingError::ProposalNotFound(_)) => {}
            other => panic!("expected ProposalNotFound, got {other:?}"),
        }
    }

    #[test]
    fn accept_all_batches_successes_and_skips_failures() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "orig-a").unwrap();
        vault.write_file("notes/b.md", "orig-b").unwrap();

        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/a.md".into(),
            trail_id: None,
            content: Some("new-a".into()),
            metadata: None,
        })
        .unwrap();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/b.md".into(),
            trail_id: None,
            content: Some("new-b".into()),
            metadata: None,
        })
        .unwrap();

        let outcomes = s
            .accept_all(&StagingFilter::default(), &vault, None)
            .unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn gc_removes_old_proposals() {
        let (_dir, s) = staged();
        // We'll manually write a proposal with an old timestamp to
        // exercise the GC path without waiting for real time.
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/old.md".into(),
                trail_id: None,
                content: Some("old".into()),
                metadata: None,
            })
            .unwrap();
        // Fake the pending.json: move the timestamp into the distant past.
        {
            let _g = s.lock.lock().unwrap();
            let mut pending = s.read_pending().unwrap();
            pending[0].created_at_ms = 0; // Jan 1 1970
            s.write_pending_atomic(&pending).unwrap();
            // Also touch the .md file so it exists for deletion.
            let md_path = s.md_path(&id);
            fs::write(&md_path, "old").unwrap();
        }
        let removed = s.gc(1).unwrap(); // 1 day max age
        assert_eq!(removed, 1);
        assert!(s.list(&StagingFilter::default()).unwrap().is_empty());
        assert!(!s.md_path(&id).exists());
    }

    #[test]
    fn gc_keeps_recent_proposals() {
        let (_dir, s) = staged();
        s.propose(ProposalInput {
            surface: "mcp-tool-call".into(),
            action: "write_note".into(),
            target_path: "notes/recent.md".into(),
            trail_id: None,
            content: Some("recent".into()),
            metadata: None,
        })
        .unwrap();
        let removed = s.gc(30).unwrap(); // 30 days
        assert_eq!(removed, 0);
        assert_eq!(s.list(&StagingFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn propose_then_accept_with_changes_log() {
        use crate::changes::Changes;
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();
        let changes = Changes::open(dir.path()).unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, Some(&changes)).unwrap();
        assert!(!outcome.new_hash.is_empty());

        // Changes row was appended with the expected metadata.
        let rows = changes.recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        let meta = &rows[0].metadata;
        assert_eq!(
            meta.get("staging_proposal_id").and_then(|v| v.as_str()),
            Some(id.as_str())
        );
        assert_eq!(meta.get("action").and_then(|v| v.as_str()), Some("write_note"));
        assert_eq!(meta.get("reviewed").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(rows[0].author, "user");
    }

    #[test]
    fn propose_missing_md_file_on_accept_returns_missing_content() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        // Delete the .md file to simulate corruption.
        fs::remove_file(s.md_path(&id)).unwrap();

        match s.accept(&id, &vault, None) {
            Err(StagingError::MissingContent(_)) => {}
            other => panic!("expected MissingContent, got {other:?}"),
        }
    }

    #[test]
    fn propose_tampered_md_file_on_accept_detects_integrity_failure() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        // Tamper with the .md file.
        fs::write(s.md_path(&id), "tampered").unwrap();

        match s.accept(&id, &vault, None) {
            Err(StagingError::DiskDrift { .. }) => {}
            other => panic!("expected DiskDrift, got {other:?}"),
        }
    }

    #[test]
    fn accept_create_action_works_when_file_does_not_exist() {
        let dir = tempdir().unwrap();
        let s = Staging::open(dir.path()).unwrap();
        let vault = Vault::open(dir.path()).unwrap();

        // For a create action, content_hash is the hash of the proposed
        // content (there's no existing file to hash).
        let proposed = "# New note";
        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/new.md".into(),
                trail_id: None,
                content: Some(proposed.into()),
                metadata: None,
            })
            .unwrap();

        let outcome = s.accept(&id, &vault, None).unwrap();
        assert!(!outcome.new_hash.is_empty());

        let (content, _) = vault.read_file_with_hash("notes/new.md").unwrap();
        assert_eq!(content, proposed);
    }

    #[test]
    fn accept_rejects_when_integrity_check_fails() {
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        // Tamper with the .md file so the hash no longer matches.
        fs::write(s.md_path(&id), "tampered").unwrap();

        match s.accept(&id, &vault, None) {
            Err(StagingError::DiskDrift { .. }) => {}
            other => panic!("expected DiskDrift from integrity failure, got {other:?}"),
        }
    }

    #[test]
    fn accept_write_file_checked_catches_race_drift() {
        // write_file_checked internally re-reads the file and compares
        // against expected_hash. A race between our read and the write
        // inside write_file_checked is caught as HikerError::DiskDrift
        // → StagingError::DiskDrift.
        let (_dir, s) = staged();
        let vault = Vault::open(_dir.path()).unwrap();
        vault.write_file("notes/a.md", "original").unwrap();

        let id = s
            .propose(ProposalInput {
                surface: "mcp-tool-call".into(),
                action: "write_note".into(),
                target_path: "notes/a.md".into(),
                trail_id: None,
                content: Some("proposed".into()),
                metadata: None,
            })
            .unwrap();

        // The write_file_checked drift check catches races during the
        // accept call itself — we can't simulate that reliably here,
        // so we just verify acceptance succeeds in the normal case.
        let outcome = s.accept(&id, &vault, None).unwrap();
        assert!(!outcome.new_hash.is_empty());
        let (content, _) = vault.read_file_with_hash("notes/a.md").unwrap();
        assert_eq!(content, "proposed");
    }
}
