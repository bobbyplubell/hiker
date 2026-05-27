//! Typed op shapes and the author vocabulary the side table records.
//!
//! A Yrs update is an opaque position-delta; the op log layers a logical
//! [`OpKind`] over each one so the activity feed, rollback, and agent
//! introspection have a typed handle. The kind is born on the [`PendingOp`]
//! while pending and copied to the `op_metadata` row on accept. [`Author`]
//! is the same vocabulary the prior changelog used, with prefix-class
//! wildcard query support (`agent:%`).
//
// status: op-log-op-shape
// status: op-log-author-classes
// status: op-log-pending-queue

use serde::{Deserialize, Serialize};

/// The `Replace`'s `old_str` kept as a fingerprint rather than the full
/// text. Two consumers re-check it against `materialize(accepted)`: drift
/// detection (the pending op's intended content must survive) and the
/// MCP `get_pending_proposal` anchor-status surface. Whole-body `Replace`,
/// `SetFrontmatter`, and `Rename` carry no anchor.
///
/// status: op-log-op-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorHint {
    /// blake3 hash of the `old_str` the producer matched against.
    pub hash: String,
    /// A short prefix of `old_str` for human-facing surfaces.
    pub preview: String,
}

impl AnchorHint {
    /// Build an anchor hint from the matched `old_str`. The preview is the
    /// first 80 chars (char-boundary-safe) so the side-table metadata and
    /// MCP introspection have something to show without storing the whole
    /// matched region.
    pub fn from_old_str(old_str: &str) -> Self {
        let preview: String = old_str.chars().take(80).collect();
        Self {
            hash: crate::hash_string(old_str),
            preview,
        }
    }
}

/// The logical shape of one op. One logical op = one Yrs update = one
/// `op_metadata` row over one `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)`
/// range. `SetFrontmatter` is a *logical label* over a `Replace` whose byte
/// range lands inside the leading `---` frontmatter fence — not a distinct
/// mechanism (see [`is_frontmatter_range`]).
///
/// status: op-log-op-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpKind {
    /// A `text` Y.Text edit. `anchor` carries the `edit_note` `old_str`
    /// when the edit came from an anchored replace; `None` for whole-body
    /// rewrites.
    Replace { anchor: Option<AnchorHint> },
    /// A `text` edit whose byte range falls inside the frontmatter fence.
    SetFrontmatter,
    /// A `meta.path` change. `from` is the prior vault-relative path.
    Rename { from: String },
    /// The first op establishing a new document.
    Create,
    /// Sets `meta.tombstone = true`.
    Tombstone,
}

impl OpKind {
    /// Stable wire string for the `op_metadata.op_kind` column. The
    /// `Rename` variant's `from` is stored in the separate `rename_from`
    /// column, so the kind string stays a flat enum tag.
    pub const fn as_str(&self) -> &'static str {
        match self {
            OpKind::Replace { .. } => "replace",
            OpKind::SetFrontmatter => "set_frontmatter",
            OpKind::Rename { .. } => "rename",
            OpKind::Create => "create",
            OpKind::Tombstone => "tombstone",
        }
    }
}

/// Author of a Yrs operation range. Recorded in `op_metadata` for every
/// range the op log authors. The wire form is `class[:identifier]`;
/// [`Author::as_wire`] / [`Author::parse`] round-trip it. The class prefix
/// drives wildcard queries (`author LIKE 'agent:%'`).
///
/// status: op-log-author-classes
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Author {
    /// Keystroke / save / direct UI action.
    User,
    /// An MCP-attached agent's tool call; id from the MCP handshake.
    Agent(String),
    /// File on disk changed outside hiker; reconciled via external-edit-sync.
    External,
    /// A source extractor re-ran; id is the extractor/plugin id.
    Extractor(String),
    /// Unattended write from internal automation (e.g. `auto:triage`).
    Auto(String),
    /// Yrs operations received from another device via the sync transport.
    Sync(String),
}

impl Author {
    /// Render to the `class[:identifier]` wire form stored in the side table.
    pub fn as_wire(&self) -> String {
        match self {
            Author::User => "user".to_string(),
            Author::Agent(id) => format!("agent:{id}"),
            Author::External => "external".to_string(),
            Author::Extractor(id) => format!("extractor:{id}"),
            Author::Auto(producer) => format!("auto:{producer}"),
            Author::Sync(device) => format!("sync:{device}"),
        }
    }

    /// Parse the `class[:identifier]` wire form. Unknown classes map to
    /// `External` (the safest "came from outside this code path" default)
    /// only when there is no identifier; an unknown `class:id` is treated
    /// as an extractor-shaped foreign author so the id is not lost. Known
    /// classes always win.
    pub fn parse(wire: &str) -> Self {
        match wire.split_once(':') {
            Some(("agent", id)) => Author::Agent(id.to_string()),
            Some(("extractor", id)) => Author::Extractor(id.to_string()),
            Some(("auto", id)) => Author::Auto(id.to_string()),
            Some(("sync", id)) => Author::Sync(id.to_string()),
            Some((_, id)) => Author::Extractor(id.to_string()),
            None => match wire {
                "user" => Author::User,
                "external" => Author::External,
                _ => Author::External,
            },
        }
    }

    /// The class half of the wire form (`user`, `agent`, `external`,
    /// `extractor`, `auto`, `sync`). Used to build the `LIKE 'class:%'`
    /// wildcard for prefix-class queries.
    pub const fn class(&self) -> &'static str {
        match self {
            Author::User => "user",
            Author::Agent(_) => "agent",
            Author::External => "external",
            Author::Extractor(_) => "extractor",
            Author::Auto(_) => "auto",
            Author::Sync(_) => "sync",
        }
    }
}

/// One queued pending update awaiting accept/reject. Serialized as a
/// `Vec<PendingOp>` into `<doc-id>.pending` (JSON — self-describing, so the
/// tagged [`OpKind`] enum and the free-form `metadata` round-trip directly).
/// The `yrs_update` bytes apply against `accepted`'s *current* state; accept
/// applies them, reject discards. Pending ops never sync — they're editorial
/// state.
///
/// status: op-log-pending-queue
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingOp {
    /// ulid.
    pub op_id: String,
    /// Serialized Yrs update bytes (v2 format).
    pub yrs_update: Vec<u8>,
    /// Logical shape of the edit.
    pub op_kind: OpKind,
    /// Producer of the op (`agent:*` / `auto:*` / `extractor:*`).
    pub author: Author,
    pub session_id: Option<String>,
    /// Producer's surface name (`"mcp-tool-call"`, `"triage"`, ...).
    pub surface: String,
    /// Groups e.g. a multi-edit `edit_note` call's per-edit ops.
    pub batch_id: Option<String>,
    pub created_at_ms: i64,
    /// Free-form producer metadata.
    pub metadata: serde_json::Value,
}

/// Returns `true` when the byte range `[start, end)` falls entirely inside
/// the leading frontmatter fence of `text`. A frontmatter fence is a `---`
/// line at byte 0 followed by a closing `---` line; the fenced region runs
/// from the opening fence through (and including) the closing fence's
/// newline. An edit whose range lands wholly before the closing fence is
/// labeled `SetFrontmatter` rather than `Replace`.
///
/// status: op-log-op-shape
pub fn is_frontmatter_range(text: &str, start: usize, end: usize) -> bool {
    let Some(fence_end) = frontmatter_fence_end(text) else {
        return false;
    };
    start <= end && end <= fence_end
}

/// Whether *every* edit span (`(byte_start, removed_len, _)`) falls entirely
/// inside the leading frontmatter fence — the test that labels a multi-span
/// text edit `SetFrontmatter` rather than `Replace`. An empty span list is
/// not a frontmatter edit. Mixed edits (some in the fence, some in the body)
/// stay `Replace`, since the activity feed describes them as a body edit.
///
/// status: op-log-op-shape
pub fn spans_in_frontmatter(text: &str, spans: &[(usize, usize, String)]) -> bool {
    !spans.is_empty()
        && spans
            .iter()
            .all(|(start, removed_len, _)| is_frontmatter_range(text, *start, start + removed_len))
}

/// Byte offset of the end of the closing frontmatter fence (the byte just
/// past the closing `---` line, including its trailing newline if present),
/// or `None` when `text` has no leading frontmatter fence. Shared by
/// [`is_frontmatter_range`] and the body-region helpers in `doc`.
///
/// status: op-log-op-shape
pub fn frontmatter_fence_end(text: &str) -> Option<usize> {
    // Opening fence must be the very first line and be exactly "---"
    // (optionally with trailing CR), per YAML frontmatter convention.
    let first_line_end = text.find('\n').unwrap_or(text.len());
    let first_line = text[..first_line_end].trim_end_matches('\r');
    if first_line != "---" {
        return None;
    }
    // Scan subsequent lines for the closing "---".
    let mut cursor = first_line_end + 1; // byte just past the opening newline
    while cursor <= text.len() {
        let rest = &text[cursor..];
        let line_end_rel = rest.find('\n');
        let line_end = line_end_rel.map_or(text.len(), |i| cursor + i);
        let line = text[cursor..line_end].trim_end_matches('\r');
        if line == "---" {
            // Include the closing line's trailing newline if present.
            return Some(match line_end_rel {
                Some(_) => line_end + 1,
                None => line_end,
            });
        }
        match line_end_rel {
            Some(_) => cursor = line_end + 1,
            None => break,
        }
    }
    None
}
