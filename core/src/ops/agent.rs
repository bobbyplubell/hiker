//! Agent-side writes: full-body `write_note` plus the
//! frontmatter-only helpers (`set_frontmatter`, `apply_tag`,
//! `remove_tag`). Each appends a changelog row authored as
//! `agent:<client_id>` so the rollback substrate distinguishes agent vs.
//! user writes.
//!
//! When review mode is on, MCP routes writes through
//! `core::staging` first; this module only runs once the user has
//! accepted the proposal (or `review_required=false`).

use std::sync::Arc;

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::errors::HikerError;
use crate::hash_string;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::vault::Vault;
use crate::watcher::Watcher;

use super::append_change_best_effort;

/// Borrowed bundle for the four agent_* write helpers. The first six
/// arguments are identical across `write_note`,
/// `set_frontmatter`, `apply_tag`, and `remove_tag`;
/// bundling them keeps the signatures (and call sites) under the
/// `too_many_arguments` threshold without changing any behavior.
pub struct WriteCtx<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub changes: Option<&'a Arc<Changes>>,
    pub client_id: &'a str,
    pub tool: &'a str,
}

/// Agent write of a note's full body. Routes through the indexer so the
/// post-write upsert runs against the same writer the UI uses; appends an
/// `author='agent:<client_id>'` changelog row with the post-write content
/// blob (rollback substrate per `mcp.md`'s authorship + audit-trail spec).
///
/// `expected_hash` enables drift-aware writes (`write_file_checked` shape):
/// `Some(h)` runs the on-disk hash compare and errors `DiskDrift` if the
/// file has changed since the agent last read it; `None` is an unconditional
/// write. Returns the new content hash.
///
/// status: mcp-tool-write-note
pub async fn write_note(
    ctx: &WriteCtx<'_>,
    rel: &str,
    content: &str,
    expected_hash: Option<&str>,
) -> Result<String, HikerError> {
    ctx.watcher.suppress(rel.to_string());

    // Snapshot the pre-write content as a baseline if this is the first time
    // hiker has touched the path (mirrors the UI's `ensure_baseline` hook on
    // user saves so rollback of an agent-authored save has somewhere to go).
    if let (Some(c), Ok((pre_text, pre_hash))) = (ctx.changes, ctx.vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (agent write)");
    }

    let abs = ctx.vault.abs_path(rel)?;
    let existed = abs.exists();
    let new_hash = match expected_hash {
        Some(h) => ctx.vault.write_file_checked(rel, h, content)?,
        None => {
            ctx.vault.write_file(rel, content)?;
            hash_string(content)
        }
    };

    // Re-suppress so the TTL window starts close to when notify surfaces the
    // post-write event.
    ctx.watcher.suppress(rel.to_string());

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let author = format!("agent:{}", ctx.client_id);
    let metadata = serde_json::json!({"tool": ctx.tool});
    append_change_best_effort(
        ctx.changes,
        ChangeAppend {
            path: rel,
            op,
            author: &author,
            content_hash: Some(&new_hash),
            content: Some(content.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    // Re-index the new content so search/related see the agent's changes.
    let _ = ctx
        .jobs
        .send(IndexJob::Upsert {
            rel_path: rel.to_string(),
            force: false,
        })
        .await;

    Ok(new_hash)
}

/// Agent merge of frontmatter fields. Reads the existing file, merges
/// `fields` into the frontmatter (recursing into nested maps), stamps
/// `hiker.author: agent-authored`, and writes the result. Errors if the
/// note doesn't exist (`NotFound`) — frontmatter on a missing file would
/// require deciding whether to create it, and the spec defers that to
/// `write_note` instead.
///
/// status: mcp-tool-set-frontmatter
pub async fn set_frontmatter(
    ctx: &WriteCtx<'_>,
    rel: &str,
    fields: serde_json::Value,
) -> Result<String, HikerError> {
    let existing = ctx.vault.read_file(rel)?;
    let merged = crate::frontmatter::merge_agent_patch(&existing, fields)
        .map_err(|e| HikerError::Io(format!("frontmatter: {e}")))?;
    write_note(ctx, rel, &merged, None).await
}

/// Agent tag append. Convenience over `set_frontmatter` for the most
/// common case. Idempotent — re-applying an already-present tag is a no-op
/// (still writes since the file's content hash may differ on whitespace,
/// but the resulting tag list is unique).
///
/// status: mcp-tool-apply-tag-remove-tag
pub async fn apply_tag(
    ctx: &WriteCtx<'_>,
    rel: &str,
    tag: &str,
) -> Result<String, HikerError> {
    let existing_tags = read_existing_tags(ctx.vault, rel)?;
    let mut tags = existing_tags;
    if !tags.iter().any(|t| t == tag) {
        tags.push(tag.to_string());
    }
    set_frontmatter(ctx, rel, serde_json::json!({"tags": tags})).await
}

/// Agent tag removal. No-op if the tag isn't present. Mirrors `apply_tag`.
///
/// status: mcp-tool-apply-tag-remove-tag
pub async fn remove_tag(
    ctx: &WriteCtx<'_>,
    rel: &str,
    tag: &str,
) -> Result<String, HikerError> {
    let mut tags = read_existing_tags(ctx.vault, rel)?;
    tags.retain(|t| t != tag);
    set_frontmatter(ctx, rel, serde_json::json!({"tags": tags})).await
}

fn read_existing_tags(vault: &Vault, rel: &str) -> Result<Vec<String>, HikerError> {
    let src = vault.read_file(rel)?;
    let split = crate::frontmatter::split(&src);
    let Some(fm) = split.frontmatter else {
        return Ok(Vec::new());
    };
    let serde_yml::Value::Mapping(m) = fm else {
        return Ok(Vec::new());
    };
    let Some(serde_yml::Value::Sequence(seq)) = m.get("tags") else {
        return Ok(Vec::new());
    };
    Ok(seq
        .iter()
        .filter_map(|v| v.as_str().map(std::string::ToString::to_string))
        .collect())
}
