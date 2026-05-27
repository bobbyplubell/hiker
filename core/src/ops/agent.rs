//! Agent-side writes: full-body `write_note` plus the
//! frontmatter-only helpers (`set_frontmatter`, `apply_tag`,
//! `remove_tag`). Each queues a pending op-log op authored as
//! `agent:<client_id>` so the review surfaces and rollback substrate
//! distinguish agent vs. user writes.

use crate::errors::HikerError;
use crate::hash_string;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Borrowed bundle for the four agent_* write helpers. The arguments are
/// identical across `write_note`, `set_frontmatter`, `apply_tag`, and
/// `remove_tag`; bundling them keeps the signatures (and call sites) under
/// the `too_many_arguments` threshold without changing any behavior.
pub struct WriteCtx<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    /// The op log this vault session rides on, when open. Agent writes
    /// queue as pending ops here (`op-log-ops-producer-helpers`). `None` for
    /// callers with no op log open (early CLI, some tests).
    pub op_log: Option<&'a super::op_writes::OpLogHandle>,
    pub client_id: &'a str,
}

/// Agent write of a note's full body. Routes through the indexer so the
/// post-write upsert runs against the same writer the UI uses; queues an
/// `author='agent:<client_id>'` pending op-log op (the rollback / review
/// substrate per `mcp.md`'s authorship + audit-trail spec).
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

    // Record the agent edit in the op log's pending queue when one is open.
    // Whole-body rewrite (`old_str = None`); the op stays pending until the
    // user accepts via `flip_op_status`. Best-effort: a staging failure logs.
    if let Some(op_log) = ctx.op_log
        && let Err(e) = super::op_writes::stage_agent_edits(
            op_log,
            ctx.vault,
            ctx.client_id,
            "mcp-tool-call",
            rel,
            &[super::op_writes::AgentEdit { old_str: None, new_str: content.to_string() }],
        )
    {
        tracing::warn!(error = %e, path = %rel, "op-log: stage_pending failed (agent write)");
    }

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
