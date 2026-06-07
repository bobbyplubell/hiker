//! Row-shaped DTOs returned across the store boundary plus the pure
//! path/title helpers that derive their display fields. These types are
//! the store's data model: owned-string structs so they cross module and
//! crate boundaries cleanly, shared by the store root and every concern
//! split (`notes`, `chunks`, `search`, `trails`).

use serde::{Deserialize, Serialize};

/// Row-shaped DTO for a single note. Owned strings so it crosses module
/// boundaries cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    /// The note's vault-relative path — its identity (`store-path-is-identity`).
    pub path: String,
    pub content_hash: String,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: String,
    /// True when the indexer attempted but refused this file (file too large,
    /// non-UTF-8, ...). The row exists so the UI can mark the file as
    /// Skipped across launches; chunks/vecs are not written for skipped rows.
    pub skipped: bool,
    /// Short, stable, human-readable reason — used directly in tooltips and
    /// the status bar (`"file too large"`, `"not UTF-8"`).
    pub skip_reason: Option<String>,
    /// Unix seconds; updated each time the note becomes the active buffer.
    /// `None` until the note is first opened. See `note-access-tracking`.
    pub last_accessed_at: Option<i64>,
}

/// Per-note relationship/provenance metadata, projected from the
/// `note_meta` index for the Vault-view lens (`vault-view.md`). One row
/// per indexed, non-skipped note; the four optional fields are the
/// `hiker.*` frontmatter keys the lens groups and nests on, pulled in a
/// single query so the lens never reads frontmatter from disk per render.
///
/// `parent` is the logical-nesting authority for crawl/feed children
/// (`hiker.parent`, a job-note ULID) — not companion-folder membership.
/// `author` is the coarse authorship trichotomy (`user-authored` /
/// `agent-authored` / `imported`); `provenance` the fine-grained label;
/// `kind` the note discriminator (`capture` / `trail` / `waypoint` /
/// `session` / ...). All `None` when the note carries no such frontmatter.
///
/// status: vault-view-source-groups
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteMetaRow {
    pub id: String,
    pub path: String,
    pub parent: Option<String>,
    pub author: Option<String>,
    pub provenance: Option<String>,
    pub kind: Option<String>,
}

/// One row in the chat `@`-mention autocomplete popover. Vault-relative
/// path with the indexable extension stripped (token format is
/// `@<rel-path-without-extension>`), plus the basename and parent
/// directory for two-line rendering, and `last_accessed_at` so the
/// frontend can format a recency hint if it wants.
///
/// status: chat-input-at-autocomplete-cmd
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtSuggestion {
    /// Vault-relative path with the file extension stripped — exactly
    /// what gets inserted into the chat input as `@<rel_path>`.
    pub rel_path: String,
    /// Filename minus extension; rendered as the primary label.
    pub basename: String,
    /// Containing folder, vault-relative; empty when the note sits at
    /// the vault root. Rendered as a muted hint to disambiguate notes
    /// with the same basename.
    pub parent_dir: String,
    /// Unix seconds; `None` until the note has been opened at least
    /// once. The popover orders by recency on the backend, so the UI
    /// usually doesn't need to consult this directly.
    pub last_accessed_at: Option<i64>,
}

/// Compact note row for the vault-home recents widgets. Same shape for both
/// "recently modified" (sorted by `mtime`) and "recently accessed" (sorted by
/// `last_accessed_at`); the UI picks the relevant timestamp per widget.
///
/// status: vault-home-recent-modified
/// status: vault-home-recent-accessed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentNote {
    pub path: String,
    pub title: String,
    pub mtime: i64,
    pub last_accessed_at: Option<i64>,
}

/// Counts surfaced by the vault-home stats widget. `queued` is filled by the
/// indexer handle, not the store; the rest come straight off the notes /
/// chunks tables.
///
/// status: vault-home-stats-widget
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultStats {
    pub total_notes: u32,
    pub total_chunks: u32,
    pub indexed: u32,
    pub skipped: u32,
}

// status: note-properties-tab-content
/// Read-only snapshot of everything hiker knows about a note from
/// `index.db`. Consumed by the `properties`-kind tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteProperties {
    pub path: String,
    pub note_id: Option<String>,
    pub mtime: Option<i64>,
    pub size: Option<i64>,
    pub content_hash: Option<String>,
    pub extension: Option<String>,
    pub indexed_at: Option<i64>,
    pub embedder_version: Option<String>,
    pub skipped: Option<bool>,
    pub skip_reason: Option<String>,
    pub chunk_count: Option<i64>,
    pub last_accessed_at: Option<i64>,
    pub change_count: Option<i64>,
}

/// Stored chunk metadata (without the embedding — fetch via knn_chunks for
/// scored retrieval).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRow {
    pub id: String,
    pub note_id: String,
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub text: String,
    pub heading_path: Option<String>,
}

/// Public chunk-bounds DTO returned by `chunk_bounds_for` — the wire shape
/// for the chunk-boundary editor decoration. Omits `text` and `note_id` to
/// keep the payload small; the UI only needs offsets + heading_path.
///
/// `char_start` / `char_end` are UTF-16 code-unit offsets into the source
/// note, computed in core via `enrich_char_offsets`. The chunker emits
/// byte offsets natively; the UI used to convert them via `TextEncoder`,
/// which made the UI the seam for a representation translation that
/// belongs next to the data. Both are populated here so frontend / CLI /
/// MCP all see the same shape and the conversion only ever happens once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkBounds {
    pub chunk_index: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    /// UTF-16 code unit offset into the note's plain text. JS strings are
    /// UTF-16, so this is what CM6 / the editor consumes directly.
    pub char_start: u64,
    pub char_end: u64,
    pub heading_path: Option<String>,
}

/// One hit from a KNN query. `score` is similarity (higher = closer); we
/// convert from sqlite-vec's distance (L2) so callers can rank uniformly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHit {
    pub chunk_id: String,
    pub note_id: String,
    pub note_path: String,
    pub heading_path: Option<String>,
    pub text: String,
    pub score: f32,
}

/// Note-level hit for the related-notes panel. Aggregates a note's chunk
/// hits into a single row — score is the max similarity across the note's
/// matching chunks, snippet/heading come from that highest-scoring chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelatedHit {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub score: f32,
    pub best_heading_path: Option<String>,
    pub snippet: String,
}

/// One row in the derived `trail_waypoints` table, surfacing the link
/// between a waypoint-note and its trail/source. Populated by the indexer
/// when it ingests waypoint-notes or trail-docs.
///
/// status: trail-waypoints-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointRow {
    pub waypoint_path: String,
    pub waypoint_id: String,
    pub trail_id: String,
    /// Vault-relative path of the source note. The derived table keys on
    /// path only (path-as-identity); `trails_containing_note` matches here.
    pub source_path: String,
    /// `None` for root-level waypoints; otherwise the ULID of the
    /// parent waypoint.
    pub parent_waypoint_id: Option<String>,
    /// Materialized depth-first 1-based dotted path —
    /// `"1"`, `"1.2"`, `"1.2.1"`. Empty when the row was written via
    /// the per-waypoint ingest path before the parent trail-doc has
    /// been re-ingested (next trail-doc ingest fills the canonical
    /// value).
    pub tree_path: String,
}

/// Hit returned by `Store::trails_containing_note`. Holds enough to point
/// the UI at both the trail and the specific waypoint inside it.
///
/// status: trail-waypoints-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailContainingHit {
    pub trail_id: String,
    pub waypoint_path: String,
    pub waypoint_id: String,
    pub tree_path: String,
}

/// One row in the derived `board_cards` table — one row per card on a
/// board. Populated by the indexer when it ingests a board-doc.
///
/// status: board-cards-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardCardRow {
    pub board_id: String,
    pub board_path: String,
    /// The referenced note's vault-relative path. The derived table keys on
    /// path only (path-as-identity); `boards_containing_note` matches here.
    pub card_note_path: String,
    pub column_name: String,
    /// 0-based position within the column.
    pub ordinal: i64,
}

/// Hit returned by `Store::boards_containing_note`. Holds enough to point
/// the UI at the board and the column the card sits in.
///
/// status: board-cards-derived-table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardContainingHit {
    pub board_id: String,
    pub board_path: String,
    pub column_name: String,
}

/// Bundle of everything needed to upsert a note in one transaction. Caller
/// (the indexer task) builds this after chunking + embedding.
pub struct NoteUpsert<'a> {
    /// The note's vault-relative path — its identity (`store-path-is-identity`).
    pub path: &'a str,
    pub content_hash: &'a str,
    pub mtime: i64,
    pub size: i64,
    pub indexed_at: i64,
    pub embedder_version: &'a str,
    pub chunks: Vec<(crate::chunker::Chunk, Vec<f32>)>,
}

/// One flattened frontmatter entry, the write-side input to the
/// `note_meta` index. `key` is a dotted path (`hiker.author`); list
/// elements share a key across rows (`tags` → many entries). `num` is the
/// numeric mirror for YAML numbers / bools, `None` for strings.
///
/// status: store-note-metadata-index
#[derive(Debug, Clone, PartialEq)]
pub struct MetaEntry {
    pub key: String,
    pub value: String,
    pub num: Option<f64>,
}

/// A single predicate in a structured `query_notes` filter. All predicates
/// in a `NoteQuery` are ANDed.
///
/// status: store-note-query
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum MetaFilter {
    /// Frontmatter `key` equals `value` (string compare). Tag membership is
    /// `Equals { key: "tags", value: "<tag>" }` since list elements are
    /// stored one-per-row under the list's key.
    Equals { key: String, value: String },
    /// Frontmatter `key` is present at all (any value).
    Exists { key: String },
    /// Numeric `key` falls in `[min, max]` (inclusive). Either bound may be
    /// omitted for an open range. Matches only rows whose `num` is set.
    NumRange {
        key: String,
        min: Option<f64>,
        max: Option<f64>,
    },
}

/// Sort direction for `NoteOrder`.
///
/// status: store-note-query
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderDir {
    Asc,
    Desc,
}

/// Ordering for a `query_notes` result set.
///
/// status: store-note-query
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "by")]
pub enum NoteOrder {
    /// Filesystem mtime (the `notes.mtime` column).
    Mtime { dir: OrderDir },
    /// Vault-relative path, lexicographic.
    Path { dir: OrderDir },
    /// A frontmatter key's numeric value (`note_meta.num`). Notes lacking
    /// the key sort as NULL (last on Asc, first on Desc — sqlite default).
    MetaNum { key: String, dir: OrderDir },
    /// A frontmatter key's string value (`note_meta.value`).
    MetaText { key: String, dir: OrderDir },
}

/// A structured query over the note metadata index. All `filters` AND
/// together; `folder` restricts to a vault-relative subtree; `order` and
/// `limit` shape the result. `select` names frontmatter keys whose values
/// are returned per row in `NoteQueryRow.fields`.
///
/// status: store-note-query
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteQuery {
    #[serde(default)]
    pub filters: Vec<MetaFilter>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub order: Option<NoteOrder>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub select: Vec<String>,
}

/// One row of a `query_notes` result. `fields` carries the values of the
/// query's `select` keys (a key with multiple values — e.g. a tag list —
/// yields the values joined by `, ` for display convenience; structured
/// consumers should re-read `note_meta` if they need the list split).
///
/// status: store-note-query
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteQueryRow {
    pub note_id: String,
    pub path: String,
    pub title: String,
    pub mtime: i64,
    #[serde(default)]
    pub fields: std::collections::BTreeMap<String, String>,
}

/// Generate a fresh ulid as a string. Used by the indexer when assigning ids
/// to newly-seen notes.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Extract a display title from a vault-relative path. Filename without
/// extension; "Untitled" for an empty stem.
pub(crate) fn title_from_path(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let stem = last.strip_suffix(".md").unwrap_or(last);
    if stem.is_empty() {
        "Untitled".into()
    } else {
        stem.to_string()
    }
}

pub(crate) fn basename_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Strip a known indexable extension (`.md`, `.markdown`, `.txt`) from the
/// basename. Other extensions stay intact — the autocomplete should never
/// see them since the query filters skipped + non-indexable rows by virtue
/// of the indexer's allowlist, but if one slips through we leave the
/// filename as-is rather than mangling it.
pub(crate) fn strip_indexable_extension(basename: &str) -> &str {
    for ext in crate::indexer::INDEXABLE_EXTENSIONS {
        let dotted = format!(".{ext}");
        if basename.len() > dotted.len()
            && basename[basename.len() - dotted.len()..].eq_ignore_ascii_case(&dotted)
        {
            return &basename[..basename.len() - dotted.len()];
        }
    }
    basename
}

/// Walk `text` once and replace `char_start` / `char_end` on every entry
/// in `bounds` with the UTF-16 offset corresponding to its byte offset.
/// Single linear pass — chunks are already sorted by `chunk_index`, but
/// the byte offsets they report aren't required to be monotonic, so we
/// build a fresh sorted index of unique byte positions and binary-search
/// each into that table. For the typical ~hundreds-of-chunks-per-note
/// shape this stays cheap.
pub fn enrich_char_offsets(text: &str, bounds: &mut [ChunkBounds]) {
    if bounds.is_empty() {
        return;
    }
    // Collect distinct byte targets we need char offsets for.
    let mut targets: Vec<u64> = bounds
        .iter()
        .flat_map(|b| [b.byte_start, b.byte_end])
        .collect();
    targets.sort_unstable();
    targets.dedup();

    // Walk the string once; for each char, record the (byte_pos, utf16_pos).
    // utf16_pos uses `encode_utf16().count()` per char so surrogate pairs
    // contribute 2.
    let mut byte_to_utf16: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let total_bytes = text.len() as u64;
    let mut byte_pos: u64 = 0;
    let mut utf16_pos: u64 = 0;
    let mut targets_iter = targets.iter().peekable();
    // Map any leading targets at position 0 first.
    while let Some(&&t) = targets_iter.peek() {
        if t == 0 {
            byte_to_utf16.insert(0, 0);
            targets_iter.next();
        } else {
            break;
        }
    }
    for ch in text.chars() {
        let ch_bytes = ch.len_utf8() as u64;
        let ch_units = ch.len_utf16() as u64;
        byte_pos += ch_bytes;
        utf16_pos += ch_units;
        while let Some(&&t) = targets_iter.peek() {
            if t <= byte_pos {
                byte_to_utf16.insert(t, utf16_pos);
                targets_iter.next();
            } else {
                break;
            }
        }
        if targets_iter.peek().is_none() {
            break;
        }
    }
    // Anything past the document end clamps to the doc-end utf16 length.
    for &t in targets_iter {
        byte_to_utf16.insert(t, utf16_pos);
        let _ = total_bytes;
    }

    for b in bounds.iter_mut() {
        b.char_start = *byte_to_utf16.get(&b.byte_start).unwrap_or(&0);
        b.char_end = *byte_to_utf16.get(&b.byte_end).unwrap_or(&b.char_start);
    }
}
