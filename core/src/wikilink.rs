//! Wikilink syntax: parsing and path-form resolution for `[[<path>]]`
//! references.
//!
//! Under the path-based identity model (`wikilink-path-form`), a wikilink's
//! target is a vault-relative path: `[[Name]]` when the basename is unique
//! in the vault, `[[folder/sub/Name]]` (no `.md` extension) when it isn't.
//! There is no ULID form, no `|display` alias half — what the user types
//! is what's stored and what's on disk.
//!
//! This module is the single place that understands the `[[…]]` byte
//! syntax: shared by the editor decoration resolver, the backlinks scan,
//! and the rename-rewrite pass. It is pure (no vault, store, or watcher
//! access), so the same parsing rules apply across the app, CLI, and MCP
//! surfaces.

use std::ops::Range;

/// A `[[…]]` reference located in a note body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLink {
    /// Byte span of the full `[[…]]` token, including both bracket pairs.
    pub span: Range<usize>,
    /// The link body verbatim — either a bare basename (`Name`) or a
    /// vault-relative path without the `.md` extension (`folder/sub/Name`).
    pub target: String,
}

/// Scan `text` for every well-formed `[[…]]` reference. A reference is a `[[`
/// followed by a `]]` on the same line; an ordinary body may carry no nested
/// `]` — the same lenient rule the live-preview decoration uses, so what
/// parses here is exactly what renders as a link. A `code:`-namespaced body is
/// the exception: short-sym monikers qualify impl methods with bracket groups
/// and backtick spans (`impl#[`Builder<'a>`]method`), so it gets the
/// depth-aware matcher ([`code_body_close`]) instead. An empty body (`[[]]`)
/// is skipped. status: wikilink-code-nested-brackets
#[must_use]
pub fn parse_links(text: &str) -> Vec<ParsedLink> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] != b'[' || bytes[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let inner_start = i + 2;
        if text[inner_start..].starts_with("code:") {
            // Nested-bracket/backtick-aware close for `[[code:…]]` bodies.
            match code_body_close(text, inner_start) {
                Some(close_start) => {
                    out.push(ParsedLink {
                        span: i..close_start + 2,
                        target: text[inner_start..close_start].trim().to_string(),
                    });
                    i = close_start + 2;
                }
                None => i += 1,
            }
            continue;
        }
        let mut j = inner_start;
        let mut close = None;
        while j + 1 < bytes.len() {
            if bytes[j] == b'\n' {
                break;
            }
            if bytes[j] == b']' && bytes[j + 1] == b']' {
                close = Some(j);
                break;
            }
            j += 1;
        }
        let Some(close_start) = close else {
            i += 1;
            continue;
        };
        let inner = &text[inner_start..close_start];
        if inner.is_empty() || inner.contains(']') {
            i = close_start + 2;
            continue;
        }
        out.push(ParsedLink {
            span: i..close_start + 2,
            target: inner.trim().to_string(),
        });
        i = close_start + 2;
    }
    out
}

/// Byte offset of the closing `]]` of a `code:`-namespaced wikilink body starting at `from`
/// (the byte after `[[`), or `None` when the body never closes on its line. Unlike the flat
/// rule above, a code body may carry nested `[`…`]` groups and backtick spans — the canonical
/// short-sym moniker form qualifies impl methods as `impl#[`Builder<'a>`]method` — so the
/// matcher tracks bracket depth, treats backtick spans as opaque, and closes on the first `]]`
/// at depth zero outside backticks. A stray `]` at depth zero that isn't the closer is
/// malformed (no parse), matching the flat rule's strictness for everything non-nested.
/// status: wikilink-code-nested-brackets
fn code_body_close(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_backtick = false;
    let mut j = from;
    while j < bytes.len() {
        match bytes[j] {
            b'\n' => return None,
            b'`' => in_backtick = !in_backtick,
            b'[' if !in_backtick => depth += 1,
            b']' if !in_backtick => {
                if depth > 0 {
                    depth -= 1;
                } else if bytes.get(j + 1) == Some(&b']') {
                    return Some(j);
                } else {
                    return None; // stray `]` at depth zero — malformed body
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

/// Split a `code:`-namespaced wikilink target into `(repo_id, symbol)`.
///
/// Spec→code links use `[[code:<repo_id>/<symbol>]]`: the `code:` prefix marks
/// the namespace, the FIRST `/` separates the repo id from the symbol (which may
/// itself contain `/`, `#`, or `.`). Returns `None` when the target has no
/// `code:` prefix, or no `/` after the prefix (no symbol). Pure — the caller
/// resolves the symbol through the code-intelligence port. status: spec-code-link
#[must_use]
pub fn parse_code_target(target: &str) -> Option<(&str, &str)> {
    let body = target.strip_prefix("code:")?;
    let (repo_id, symbol) = body.split_once('/')?;
    if repo_id.is_empty() || symbol.is_empty() {
        return None;
    }
    Some((repo_id, symbol))
}

/// The friendly label a `[[code:…]]` pill renders, derived from the link's `<symbol>` body —
/// the canonical short-sym moniker, which stays the STORED form (it must round-trip through
/// resolve/locate/fingerprint). The label is the body's last path segment with any impl/type
/// qualifier reduced to a bare type name:
///
/// - `cluster/build/impl#[`Builder<'a>`]top_level_split` → `Builder::top_level_split`
/// - `config/sections/McpToolsConfig#get_active_note_enabled` → `McpToolsConfig::get_active_note_enabled`
/// - `trails/ops/delete_trail` → `delete_trail`
///
/// In an `impl#[Self][`Trait<…>`]method` qualifier the FIRST bracket group is the self type
/// (backticks stripped, generic arguments dropped); the trait group is display noise and
/// dropped. Lossy by design — display-only, the same way vault wikilink pills show live titles
/// rather than stored paths. status: wikilink-code-pretty-label
#[must_use]
pub fn code_link_label(symbol: &str) -> String {
    let last = symbol.rsplit('/').next().unwrap_or(symbol);
    let Some((head, qual)) = last.split_once('#') else {
        return last.to_string();
    };
    // The member name: everything after the qualifier's last bracket group; any remaining
    // `#` separators (e.g. `TabKind#Canvas#path`) read as `::`.
    let member = qual.rsplit(']').next().unwrap_or(qual).replace('#', "::");
    let type_name = if head == "impl" {
        impl_self_type(qual)
    } else {
        (!head.is_empty()).then(|| head.to_string())
    };
    match type_name {
        Some(t) => format!("{t}::{member}"),
        None => member,
    }
}

/// The self type of an `impl#[…]…` qualifier: the first bracket group's content with backticks
/// stripped and generic arguments (`<…>` and beyond) dropped — `[`Builder<'a>`]` → `Builder`.
/// `None` when the qualifier carries no non-empty bracket group.
fn impl_self_type(qual: &str) -> Option<String> {
    let start = qual.find('[')? + 1;
    let end = start + qual[start..].find(']')?;
    let raw = qual[start..end].trim_matches('`');
    let name = raw.split('<').next().unwrap_or(raw).trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Spec links use `[[spec:<slug>]]`: a reference to a spec feature by its stable kebab-case
/// slug, resolved by anchor search (the `spec_anchors` store index) — positional-free, so
/// re-homing the spec entry between docs never breaks the reference. Returns the slug, or
/// `None` when the target isn't a well-formed spec link. status: wikilink-spec-links
#[must_use]
pub fn parse_spec_target(target: &str) -> Option<&str> {
    let slug = target.strip_prefix("spec:")?;
    is_spec_slug(slug).then_some(slug)
}

/// A spec slug: lowercase/digit/dash, at least one dash — the same token rule the spec
/// engine's reconcile uses to recognize `[slug]` anchors. status: wikilink-spec-links
fn is_spec_slug(s: &str) -> bool {
    !s.is_empty()
        && s.contains('-')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Every bare `[slug]` spec-anchor token in `line`, appended to `out`. A token immediately
/// followed by `(` is a markdown-link label, not an anchor; `[[wikilink]]` bodies never match
/// (the inner token starts with `[`).
fn spec_anchors_in_line(line: &str, out: &mut Vec<String>) {
    let mut idx = 0;
    while let Some(o) = line[idx..].find('[') {
        let start = idx + o + 1;
        let Some(c) = line[start..].find(']') else { break };
        let end = start + c;
        let tok = &line[start..end];
        if is_spec_slug(tok) && line.as_bytes().get(end + 1) != Some(&b'(') {
            out.push(tok.to_string());
        }
        idx = end + 1;
    }
}

/// Every distinct spec-anchor slug defined in `text`: bare `[slug]` tokens outside fenced
/// code, in first-appearance order. The write side of the `spec_anchors` store index — the
/// same token convention the spec engine associates link lines with, so the index and
/// reconcile agree on what an anchor is. status: spec-anchor-index
#[must_use]
pub fn scan_spec_anchors(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence {
            spec_anchors_in_line(line, &mut out);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    out
}

/// Byte offset of the start of the first line defining the bare `[slug]` anchor in `text`,
/// skipping fenced code — where `[[spec:slug]]` navigation lands. `None` when the note
/// doesn't define the anchor (graceful no-op for the caller, same posture as a heading
/// miss). status: wikilink-spec-links
#[must_use]
pub fn find_slug_anchor_byte(text: &str, slug: &str) -> Option<usize> {
    if !is_spec_slug(slug) {
        return None;
    }
    let mut offset = 0usize;
    let mut in_fence = false;
    let mut toks = Vec::new();
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence {
            toks.clear();
            spec_anchors_in_line(line, &mut toks);
            if toks.iter().any(|t| t == slug) {
                return Some(offset);
            }
        }
        offset += line.len();
    }
    None
}

/// The display title hiker shows for a vault-relative path: the filename stem
/// with the `.md` extension stripped. Until frontmatter-title parsing lands
/// this mirrors the related-notes / search title convention.
#[must_use]
pub fn title_for_path(rel: &str) -> &str {
    rel.rsplit('/')
        .next()
        .unwrap_or(rel)
        .trim_end_matches(".md")
}

/// Split a link target into its page part and an optional `#section` anchor.
///
/// A wikilink / markdown-link target may carry a heading anchor after a `#`:
/// `Page#Heading`, `folder/Page#Heading`, or a same-document `#Heading` (empty
/// page). The returned page is the part before the first `#` (trimmed); the
/// section is everything after it (trimmed), or `None` when there is no `#`.
/// An empty page (a bare `#Heading`) signals "this document" to the caller.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn split_target_section(target: &str) -> (&str, Option<&str>) {
    match target.split_once('#') {
        Some((page, section)) => (page.trim(), Some(section.trim())),
        None => (target.trim(), None),
    }
}

/// Slugify a heading's text the way GitHub anchors do: lowercase, drop every
/// character that is not an ASCII alphanumeric, a space, or a hyphen, then
/// turn runs of whitespace into single hyphens. Used to match a link's
/// `#section` anchor against the headings in a note body.
///
/// This is intentionally the GitHub algorithm rather than a bespoke scheme so a
/// `#section` authored against rendered-Markdown expectations (e.g. copied from
/// a web view) resolves the same way here.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn heading_slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_hyphen = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            // Collapse any whitespace run; emit one hyphen lazily so trailing
            // whitespace never produces a trailing hyphen.
            pending_hyphen = !out.is_empty();
        } else if ch == '-' || ch.is_ascii_alphanumeric() {
            if pending_hyphen {
                out.push('-');
                pending_hyphen = false;
            }
            out.extend(ch.to_lowercase());
        }
        // All other characters (punctuation, symbols) are dropped.
    }
    out
}

/// Byte offset of the start of the first ATX heading line (`#`…`######`) in
/// `text` whose slug equals `section`'s slug, or `None` when no heading
/// matches. The match is slug-based (`heading_slug`), so the link's `#section`
/// anchor need not be byte-identical to the heading text — only slug-equal.
///
/// Fenced code blocks are skipped so a `#`-prefixed line inside a ``` fence is
/// never mistaken for a heading. The returned offset is the byte index of the
/// heading line's first character (its leading `#`), suitable for placing the
/// caret + scrolling the heading to the top of the viewport.
///
/// status: wikilink-headings-blocks
#[must_use]
pub fn find_heading_byte(text: &str, section: &str) -> Option<usize> {
    let want = heading_slug(section);
    if want.is_empty() {
        return None;
    }
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let trimmed = trimmed.trim_end();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence
            && let Some(rest) = heading_text(trimmed)
            && heading_slug(rest) == want
        {
            return Some(offset + indent);
        }
        offset += line.len();
    }
    None
}

/// If `line` is an ATX heading (`#`…`######` followed by a space or end of
/// line), return the heading text after the markers; otherwise `None`. A run of
/// more than six `#` is not a heading per CommonMark.
fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some(rest);
    }
    rest.strip_prefix(' ').map(str::trim)
}

/// Classify a `#section` anchor as a block anchor and return its raw id.
///
/// A `^`-prefixed anchor (`#^blockid`, split to a `section` of `^blockid`)
/// targets a specific block carrying a trailing `^blockid` marker, not a
/// heading. Returns `Some(id)` (without the `^`) for a block anchor whose id
/// is non-empty and a valid id (`[A-Za-z0-9-]`), or `None` when the anchor is
/// a heading anchor (no leading `^`) or a malformed block id.
///
/// status: wikilink-block-anchors
#[must_use]
pub fn block_anchor_id(section: &str) -> Option<&str> {
    let id = section.trim().strip_prefix('^')?;
    if !id.is_empty() && id.bytes().all(is_block_id_byte) {
        Some(id)
    } else {
        None
    }
}

/// True for a byte allowed in a block id (`[A-Za-z0-9-]`). The id charset is
/// intentionally narrow so the trailing ` ^id` marker is unambiguous to scan.
const fn is_block_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

/// Byte offset of the start of the first block (line) in `text` carrying a
/// trailing ` ^blockid` marker matching `blockid` exactly, or `None` when no
/// block matches.
///
/// A block marker is the last token on its line, preceded by whitespace, with
/// the id charset `[A-Za-z0-9-]`: `Some paragraph text. ^abc123`. The match is
/// exact (not slugged) — the id is an explicit handle, not derived from prose.
/// Fenced code blocks are skipped like `find_heading_byte`, so a `^id` token
/// inside a ``` fence is never read as a block marker. The returned offset is
/// the byte index of the marked line's first character, so navigation lands at
/// the top of the block.
///
/// status: wikilink-block-anchors
#[must_use]
pub fn find_block_byte(text: &str, blockid: &str) -> Option<usize> {
    if blockid.is_empty() || !blockid.bytes().all(is_block_id_byte) {
        return None;
    }
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let trimmed = trimmed.trim_end();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && line_block_id(trimmed) == Some(blockid) {
            return Some(offset + indent);
        }
        offset += line.len();
    }
    None
}

/// The trailing block id on `line` (its last whitespace-preceded ` ^id` token),
/// or `None` when the line carries no well-formed trailing marker. The marker
/// must be the final token, separated from the preceding text by whitespace, so
/// a bare `^id` line or an inline `^id` mid-sentence does not count.
fn line_block_id(line: &str) -> Option<&str> {
    let (head, last) = line.rsplit_once(char::is_whitespace)?;
    if head.is_empty() {
        return None;
    }
    block_anchor_id(last)
}

/// One block (paragraph / list-item / line) of a note, as enumerated for the
/// block-anchor picker (`wikilink-block-anchor-autoinject`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    /// Byte range of the block's (single) line in the source, including any
    /// leading indent but not the trailing newline.
    pub range: Range<usize>,
    /// A short preview of the block's text for the picker, with any existing
    /// trailing ` ^id` marker stripped so the preview reads as prose.
    pub preview: String,
    /// The block's existing trailing-marker id (without the `^`), or `None`
    /// when the block is not yet anchored. An anchored block reuses its id; an
    /// un-anchored one gets a fresh one injected on pick.
    pub existing_id: Option<String>,
}

/// Enumerate the linkable blocks of a note body for the block-anchor picker.
///
/// A block is one non-blank source line outside any fenced code block: a
/// paragraph line, a list item, or a heading. Blank lines and fence delimiters
/// (and lines inside a ``` fence) are skipped, mirroring `find_block_byte`'s
/// fence handling so the picker only offers blocks an anchor could actually
/// land on. Each entry carries the line's byte range, a marker-stripped
/// preview, and the block's existing id (if it already carries a ` ^id`
/// marker).
///
/// status: wikilink-block-anchor-autoinject
#[must_use]
pub fn scan_blocks(text: &str) -> Vec<BlockInfo> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    let mut in_fence = false;
    for raw in text.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let trimmed = line.trim();
        let line_len = line.len();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        } else if !in_fence && !trimmed.is_empty() {
            let existing_id = line_block_id(line.trim_end()).map(str::to_string);
            let preview = block_preview(line, existing_id.as_deref());
            out.push(BlockInfo {
                range: offset..offset + line_len,
                preview,
                existing_id,
            });
        }
        offset += raw.len();
    }
    out
}

/// A short, trimmed preview of a block line for the picker: the line with its
/// trailing ` ^id` marker removed (when `existing_id` is `Some`) and collapsed
/// to a single space-joined string, capped so a long paragraph doesn't blow out
/// the popup.
fn block_preview(line: &str, existing_id: Option<&str>) -> String {
    const MAX: usize = 80;
    let body = match existing_id {
        Some(id) => {
            // Strip the trailing ` ^id` token (and the whitespace before it).
            let marker = format!("^{id}");
            line.trim_end()
                .strip_suffix(&marker)
                .map_or(line, str::trim_end)
        }
        None => line,
    };
    let collapsed: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > MAX {
        let head: String = collapsed.chars().take(MAX).collect();
        format!("{head}\u{2026}")
    } else {
        collapsed
    }
}

/// Generate a fresh, collision-free block id for the block at `block_range` in
/// `text`. The id is a short base36 content hash of the block's text, so the
/// same block always derives the same id (letting an authoring helper re-locate
/// the picked block from the id alone). When that base hash already marks a
/// *different* block in the note, a `-<n>` counter suffix is appended until the
/// id is unique among the note's existing marker ids.
///
/// status: wikilink-block-anchor-autoinject
#[must_use]
pub fn fresh_block_id(text: &str, block_range: &Range<usize>) -> String {
    let block = text.get(block_range.clone()).unwrap_or("");
    let existing: std::collections::HashSet<String> = scan_blocks(text)
        .into_iter()
        .filter_map(|b| b.existing_id)
        .collect();
    let base = base36_block_hash(block.trim());
    if !existing.contains(&base) {
        return base;
    }
    for n in 2..10_000u32 {
        let candidate = format!("{base}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    base
}

/// Short base36 (`[a-z0-9]`) hash of a block's text, used as its content-
/// addressed default id. The first byte is forced into the alphabetic range so
/// the id is a valid `[A-Za-z0-9-]` token that never starts with a digit (a
/// purely cosmetic stability choice). Length is fixed at 6 base36 digits — a
/// ~2-billion space, ample for a single note's blocks.
fn base36_block_hash(block: &str) -> String {
    let digest = crate::hash_string(block);
    // `hash_string` is a hex blake3; fold its leading hex into a u64.
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    for b in digest.bytes().take(16) {
        acc = acc.rotate_left(5) ^ u64::from(b);
        acc = acc.wrapping_mul(0x0100_0000_01b3);
    }
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = Vec::with_capacity(6);
    // First digit from the letters-only range so the id reads as a word.
    out.push(ALPHABET[(acc % 26) as usize]);
    acc /= 26;
    for _ in 0..5 {
        out.push(ALPHABET[(acc % 36) as usize]);
        acc /= 36;
    }
    String::from_utf8(out).unwrap_or_else(|_| "block0".to_string())
}

/// Inject a ` ^id` marker at the end of the block whose source line is
/// `block_range` in `text`, returning the new document text. A no-op (returns
/// `text` unchanged) when the block already carries that exact id. The marker
/// is appended after the line's trailing-whitespace-trimmed end, so the block
/// reads `…existing text ^id`.
///
/// status: wikilink-block-anchor-autoinject
#[must_use]
pub fn inject_block_marker(text: &str, block_range: &Range<usize>, id: &str) -> String {
    let Some(line) = text.get(block_range.clone()) else {
        return text.to_string();
    };
    // Already carries this exact id → no-op (reuse, never duplicate).
    if line_block_id(line.trim_end()) == Some(id) {
        return text.to_string();
    }
    let trimmed_len = line.trim_end().len();
    let insert_at = block_range.start + trimmed_len;
    let mut out = String::with_capacity(text.len() + id.len() + 2);
    out.push_str(&text[..insert_at]);
    out.push(' ');
    out.push('^');
    out.push_str(id);
    out.push_str(&text[insert_at..]);
    out
}

/// Ambiguity policy for bare-name links that match more than one path.
/// Mirrors `[wikilinks] ambiguous_resolution` in user/vault config.
///
/// status: wikilink-ambiguous-resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmbiguityPolicy {
    /// Render bare-name collisions as broken; the user disambiguates via
    /// the picker. The default — never guess silently.
    #[default]
    Unresolved,
    /// Resolve to the lexicographically-first matching path.
    LexFirst,
    /// Resolve to the match with the longest shared path prefix with the
    /// referrer; ties broken lex-first.
    NearestFolder,
}

/// Outcome of resolving a `[[…]]` target against the vault's indexable paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The target resolved to exactly this path.
    Resolved(String),
    /// No path matches the target. The link renders unresolved; clicking
    /// offers to create a note at the inferred location.
    Unresolved,
    /// The bare-name target matches multiple paths and the policy is
    /// `Unresolved`; the link renders broken with a disambiguation
    /// affordance. Never produced under `LexFirst` / `NearestFolder`
    /// (those always pick a winner when there's at least one match).
    Ambiguous(Vec<String>),
}

/// Resolve a wikilink `target` against the vault's indexable `paths`. An
/// explicit-path target (`folder/sub/Name`) resolves to exactly
/// `folder/sub/Name.md`; a bare-name target (`Name`) matches every note
/// whose basename equals it. Ambiguity is handled per `policy`.
/// `referrer` is the path of the linking note — only consulted by
/// `NearestFolder`; pass `None` if irrelevant.
///
/// status: wikilink-resolve
/// status: wikilink-ambiguous-resolution
#[must_use]
pub fn resolve_path(
    paths: &[String],
    target: &str,
    policy: AmbiguityPolicy,
    referrer: Option<&str>,
) -> Resolution {
    // Drop any `#section` anchor before resolving the page: the section only
    // affects where navigation lands, not which note the link points at.
    // status: wikilink-headings-blocks
    let (page, _section) = split_target_section(target);
    let needle = page.trim_end_matches(".md");
    if needle.is_empty() {
        return Resolution::Unresolved;
    }
    if needle.contains('/') {
        // Explicit-path: must match a vault path exactly (modulo `.md`).
        let with_ext = format!("{needle}.md");
        for rel in paths {
            if rel == &with_ext {
                return Resolution::Resolved(rel.clone());
            }
        }
        return Resolution::Unresolved;
    }
    // Bare-name: collect every path whose basename (without `.md`) equals.
    let mut matches: Vec<String> = paths
        .iter()
        .filter(|rel| title_for_path(rel) == needle)
        .cloned()
        .collect();
    match matches.len() {
        0 => Resolution::Unresolved,
        1 => Resolution::Resolved(matches.remove(0)),
        _ => match policy {
            AmbiguityPolicy::Unresolved => {
                matches.sort();
                Resolution::Ambiguous(matches)
            }
            AmbiguityPolicy::LexFirst => {
                matches.sort();
                Resolution::Resolved(matches.remove(0))
            }
            AmbiguityPolicy::NearestFolder => {
                matches.sort();
                let referrer_dir = referrer
                    .and_then(|r| r.rsplit_once('/'))
                    .map_or("", |(d, _)| d);
                // Pick the match sharing the longest path-segment prefix
                // with the referrer's folder; ties already lex-sorted.
                let best = matches
                    .iter()
                    .max_by_key(|m| shared_prefix_segments(referrer_dir, m))
                    .cloned()
                    .unwrap_or_else(|| matches.remove(0));
                Resolution::Resolved(best)
            }
        },
    }
}

/// Count of leading path segments shared by two vault-relative paths.
fn shared_prefix_segments(a: &str, b: &str) -> usize {
    let a_segs = a.split('/').filter(|s| !s.is_empty());
    let b_segs = b.split('/').filter(|s| !s.is_empty());
    a_segs.zip(b_segs).take_while(|(x, y)| x == y).count()
}

/// Shortest unambiguous insert form for `target` against the vault's
/// `paths`. Returns the bare basename when it's unique; otherwise
/// extends with the minimum number of leading folder segments needed
/// to disambiguate. Always returns the form without the `.md`
/// extension (the wikilink syntax doesn't carry it).
///
/// status: wikilink-autocomplete
#[must_use]
pub fn shortest_unambiguous(paths: &[String], target: &str) -> String {
    let target_clean = target.trim_end_matches(".md");
    let basename = title_for_path(target_clean);
    let collisions: Vec<&String> = paths
        .iter()
        .filter(|p| {
            p.as_str() != target_clean
                && p.as_str() != format!("{target_clean}.md")
                && title_for_path(p) == basename
        })
        .collect();
    if collisions.is_empty() {
        return basename.to_string();
    }
    // Walk the target's path segments from the right, growing the prefix
    // until no collision shares the same suffix.
    let target_segs: Vec<&str> = target_clean.split('/').collect();
    for take in 2..=target_segs.len() {
        let suffix = target_segs[target_segs.len() - take..].join("/");
        let collides = collisions.iter().any(|c| {
            let c_clean = c.trim_end_matches(".md");
            let c_segs: Vec<&str> = c_clean.split('/').collect();
            c_segs.len() >= take
                && c_segs[c_segs.len() - take..].join("/") == suffix
        });
        if !collides {
            return suffix;
        }
    }
    target_clean.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_links() {
        let links = parse_links("see [[Alpha]] and [[work/Beta]] end");
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "Alpha");
        assert_eq!(links[1].target, "work/Beta");
        assert_eq!(&"see [[Alpha]] and [[work/Beta]] end"[links[0].span.clone()], "[[Alpha]]");
    }

    #[test]
    fn pipe_in_body_is_part_of_target_under_path_form() {
        // Pipe is no longer an alias separator: the entire body is the
        // target. Most real links won't contain `|`; if one does, it
        // simply renders unresolved (no path contains a pipe in normal use).
        let links = parse_links("[[Beta|the beta]]");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "Beta|the beta");
    }

    #[test]
    fn skips_unterminated_and_nested() {
        assert!(parse_links("[[no close").is_empty());
        assert!(parse_links("[[a]b]]").is_empty());
        assert!(parse_links("[[multi\nline]]").is_empty());
        assert!(parse_links("[[]]").is_empty());
    }

    #[test]
    fn parses_code_links_with_nested_brackets_and_backticks() {
        // The four standing impl-qualified bodies from bug-code-wikilink-impl-moniker-not-parsed
        // (clustering.md): nested `[`/`]` plus backtick spans inside `[[code:…]]`.
        let bodies = [
            "code:hiker/cluster/build/impl#[`Builder<'a>`]top_level_split",
            "code:hiker/cluster/build/impl#[`Builder<'a>`]build_top_level_nodes",
            "code:hiker/cluster/build/impl#[`Builder<'a>`]split_branch_ctx",
            "code:hiker/cluster/build/impl#[`SplitBranchCtx<'a>`]split_top_level_groups",
        ];
        for body in bodies {
            let text = format!("implements:: [[{body}]], [[code:hiker/x/y]]");
            let links = parse_links(&text);
            assert_eq!(links.len(), 2, "{body}");
            assert_eq!(links[0].target, body);
            assert_eq!(&text[links[0].span.clone()], format!("[[{body}]]"));
            assert_eq!(links[1].target, "code:hiker/x/y", "scan resumes after the close");
        }
        // Bare bracket group (no backticks) and a two-group (self type + trait) qualifier.
        let text = "[[code:hiker/tab/impl#[TabKind]git_diff_preview]] and \
                    [[code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]id]]";
        let links = parse_links(text);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target, "code:hiker/tab/impl#[TabKind]git_diff_preview");
        assert_eq!(
            links[1].target,
            "code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]id",
        );
        // The parsed body round-trips through the code-target split (storage form unchanged).
        assert_eq!(
            parse_code_target(&links[0].target),
            Some(("hiker", "tab/impl#[TabKind]git_diff_preview")),
        );
    }

    #[test]
    fn malformed_code_bodies_do_not_parse() {
        // Unterminated / multi-line / stray-`]` code bodies stay rejected.
        assert!(parse_links("[[code:hiker/impl#[`X`]m").is_empty(), "no close");
        assert!(parse_links("[[code:hiker/impl#[`X\n`]m]]").is_empty(), "newline inside");
        assert!(parse_links("[[code:hiker/a]b]]").is_empty(), "stray `]` at depth zero");
        // And the flat rule for ordinary bodies is unchanged.
        assert!(parse_links("[[a]b]]").is_empty());
    }

    #[test]
    fn code_link_label_prettifies_monikers() {
        // Impl-qualified, backticked, lifetime-generic — the bug row's headline shape.
        assert_eq!(
            code_link_label("cluster/build/impl#[`Builder<'a>`]top_level_split"),
            "Builder::top_level_split",
        );
        assert_eq!(
            code_link_label("cluster/build/impl#[`SplitBranchCtx<'a>`]split_top_level_groups"),
            "SplitBranchCtx::split_top_level_groups",
        );
        // Bare bracket group; two-group (self + trait) keeps the self type.
        assert_eq!(code_link_label("tab/impl#[TabKind]git_diff_preview"), "TabKind::git_diff_preview");
        assert_eq!(
            code_link_label("canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]id"),
            "CanvasActivity::id",
        );
        // Type#member and nested member paths read as `::`.
        assert_eq!(
            code_link_label("config/sections/McpToolsConfig#get_active_note_enabled"),
            "McpToolsConfig::get_active_note_enabled",
        );
        assert_eq!(code_link_label("tab/TabKind#Canvas#path"), "TabKind::Canvas::path");
        // Plain paths label with their last segment; crate-qualified modules likewise.
        assert_eq!(code_link_label("trails/ops/delete_trail"), "delete_trail");
        assert_eq!(code_link_label("hiker-core/trails"), "trails");
        assert_eq!(code_link_label("wikilink"), "wikilink");
    }

    #[test]
    fn parse_code_target_splits_on_first_slash() {
        // No `code:` prefix → None (an ordinary vault link).
        assert_eq!(parse_code_target("folder/Note"), None);
        // Plain repo_id + symbol.
        assert_eq!(parse_code_target("code:myrepo/EntityGraph"), Some(("myrepo", "EntityGraph")));
        // The symbol keeps every `/` after the first (the split is on the FIRST `/`).
        assert_eq!(
            parse_code_target("code:myrepo/crate::mod::Type"),
            Some(("myrepo", "crate::mod::Type")),
        );
        assert_eq!(
            parse_code_target("code:r/a/b/c"),
            Some(("r", "a/b/c")),
        );
        // Missing symbol or repo_id → None.
        assert_eq!(parse_code_target("code:myrepo"), None);
        assert_eq!(parse_code_target("code:myrepo/"), None);
        assert_eq!(parse_code_target("code:/sym"), None);
    }

    #[test]
    fn resolves_bare_name_unique() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Beta.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("notes/Alpha.md".to_string()),
        );
    }

    #[test]
    fn resolves_explicit_path() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "work/Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("work/Alpha.md".to_string()),
        );
        assert_eq!(
            resolve_path(&paths, "missing/Alpha", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn ambiguous_unresolved_policy_lists_matches() {
        let paths = vec!["work/Beta.md".to_string(), "personal/Beta.md".to_string()];
        match resolve_path(&paths, "Beta", AmbiguityPolicy::Unresolved, None) {
            Resolution::Ambiguous(ms) => {
                assert_eq!(ms, vec!["personal/Beta.md", "work/Beta.md"]);
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_lex_first_policy_picks_first() {
        let paths = vec!["work/Beta.md".to_string(), "personal/Beta.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Beta", AmbiguityPolicy::LexFirst, None),
            Resolution::Resolved("personal/Beta.md".to_string()),
        );
    }

    #[test]
    fn ambiguous_nearest_folder_uses_referrer() {
        let paths = vec![
            "work/projects/Beta.md".to_string(),
            "personal/Beta.md".to_string(),
        ];
        assert_eq!(
            resolve_path(
                &paths,
                "Beta",
                AmbiguityPolicy::NearestFolder,
                Some("work/projects/intro.md"),
            ),
            Resolution::Resolved("work/projects/Beta.md".to_string()),
        );
        assert_eq!(
            resolve_path(
                &paths,
                "Beta",
                AmbiguityPolicy::NearestFolder,
                Some("personal/notes/intro.md"),
            ),
            Resolution::Resolved("personal/Beta.md".to_string()),
        );
    }

    #[test]
    fn unresolved_when_no_match() {
        let paths = vec!["notes/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Gamma", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn shortest_unambiguous_picks_bare_when_unique() {
        let paths = vec!["notes/Alpha.md".to_string(), "work/Beta.md".to_string()];
        assert_eq!(shortest_unambiguous(&paths, "notes/Alpha.md"), "Alpha");
    }

    #[test]
    fn shortest_unambiguous_extends_to_disambiguate() {
        let paths = vec![
            "work/Beta.md".to_string(),
            "personal/Beta.md".to_string(),
        ];
        assert_eq!(shortest_unambiguous(&paths, "work/Beta.md"), "work/Beta");
        assert_eq!(shortest_unambiguous(&paths, "personal/Beta.md"), "personal/Beta");
    }

    #[test]
    fn splits_target_section() {
        assert_eq!(split_target_section("Page#Heading"), ("Page", Some("Heading")));
        assert_eq!(split_target_section("folder/Page#H"), ("folder/Page", Some("H")));
        assert_eq!(split_target_section("#Heading"), ("", Some("Heading")));
        assert_eq!(split_target_section("Page"), ("Page", None));
        assert_eq!(split_target_section(" Page # H "), ("Page", Some("H")));
    }

    #[test]
    fn heading_slug_matches_github_algorithm() {
        assert_eq!(heading_slug("Hello World"), "hello-world");
        assert_eq!(heading_slug("  Trim Me  "), "trim-me");
        assert_eq!(heading_slug("Mixed CASE 123"), "mixed-case-123");
        assert_eq!(heading_slug("Punctuation: drop! it?"), "punctuation-drop-it");
        assert_eq!(heading_slug("multiple   spaces"), "multiple-spaces");
        assert_eq!(heading_slug("already-hyphenated"), "already-hyphenated");
        assert_eq!(heading_slug("symbols *& removed"), "symbols-removed");
        assert_eq!(heading_slug(""), "");
        assert_eq!(heading_slug("!!!"), "");
    }

    #[test]
    fn finds_heading_byte_by_slug() {
        let text = "# Intro\nbody\n\n## Sub Section\nmore\n### Deep Heading!\nx\n";
        // Heading line offsets: "# Intro" at 0, "## Sub Section" after.
        assert_eq!(find_heading_byte(text, "Intro"), Some(0));
        let sub = text.find("## Sub Section").unwrap();
        assert_eq!(find_heading_byte(text, "Sub Section"), Some(sub));
        // Slug-equal but not byte-equal anchor still matches.
        assert_eq!(find_heading_byte(text, "sub-section"), Some(sub));
        let deep = text.find("### Deep Heading!").unwrap();
        // The anchor drops the `!` to match the heading's slug.
        assert_eq!(find_heading_byte(text, "Deep Heading"), Some(deep));
        // No matching heading.
        assert_eq!(find_heading_byte(text, "Missing"), None);
        // Empty section never matches.
        assert_eq!(find_heading_byte(text, ""), None);
    }

    #[test]
    fn find_heading_skips_fenced_code() {
        let text = "intro\n```\n# Not A Heading\n```\n# Real Heading\n";
        let real = text.find("# Real Heading").unwrap();
        assert_eq!(find_heading_byte(text, "Not A Heading"), None);
        assert_eq!(find_heading_byte(text, "Real Heading"), Some(real));
    }

    #[test]
    fn resolve_path_ignores_section_anchor() {
        let paths = vec!["notes/Alpha.md".to_string()];
        assert_eq!(
            resolve_path(&paths, "Alpha#Some Heading", AmbiguityPolicy::Unresolved, None),
            Resolution::Resolved("notes/Alpha.md".to_string()),
        );
        // A same-doc `#Heading` (empty page) has no note target.
        assert_eq!(
            resolve_path(&paths, "#Some Heading", AmbiguityPolicy::Unresolved, None),
            Resolution::Unresolved,
        );
    }

    #[test]
    fn block_anchor_id_classifies_caret_sections() {
        // A `#^id` target splits to a section of `^id`.
        assert_eq!(split_target_section("Note#^abc123"), ("Note", Some("^abc123")));
        assert_eq!(block_anchor_id("^abc123"), Some("abc123"));
        assert_eq!(block_anchor_id("^a-b-c"), Some("a-b-c"));
        // A heading anchor (no `^`) is not a block anchor.
        assert_eq!(block_anchor_id("Heading"), None);
        // A bare `^` or one with an out-of-charset char is malformed.
        assert_eq!(block_anchor_id("^"), None);
        assert_eq!(block_anchor_id("^bad id"), None);
        assert_eq!(block_anchor_id("^under_score"), None);
        // Surrounding whitespace is tolerated (mirrors split_target_section).
        assert_eq!(block_anchor_id(" ^abc "), Some("abc"));
    }

    #[test]
    fn finds_block_byte_by_trailing_marker() {
        let text = "intro para\n\nA tagged paragraph. ^abc123\n\ntail\n";
        let para = text.find("A tagged").unwrap();
        assert_eq!(find_block_byte(text, "abc123"), Some(para));
        // List-item blocks carry the marker at the end too; the offset is the
        // start of the (indented) line so navigation lands at the block top.
        let list = "- first item\n- second item ^item2\n";
        let line = list.find("- second").unwrap();
        assert_eq!(find_block_byte(list, "item2"), Some(line));
    }

    #[test]
    fn find_block_byte_no_match_is_none() {
        let text = "no markers here\njust prose ^present\n";
        assert_eq!(find_block_byte(text, "absent"), None);
        // An empty / malformed id never matches.
        assert_eq!(find_block_byte(text, ""), None);
        assert_eq!(find_block_byte(text, "bad id"), None);
    }

    #[test]
    fn find_block_byte_skips_fenced_code() {
        let text = "before\n```\ncode line ^infence\n```\nreal block. ^outside\n";
        assert_eq!(find_block_byte(text, "infence"), None);
        let real = text.find("real block").unwrap();
        assert_eq!(find_block_byte(text, "outside"), Some(real));
    }

    #[test]
    fn find_block_byte_requires_marker_at_end_whitespace_preceded() {
        // A marker not at the end of the line does not count.
        assert_eq!(find_block_byte("text ^abc more words\n", "abc"), None);
        // A bare `^abc` line (no preceding text token) does not count: the
        // marker tags a block, it is not a block on its own.
        assert_eq!(find_block_byte("^abc\n", "abc"), None);
        // No whitespace before the caret (glued to a word) does not count.
        assert_eq!(find_block_byte("word^abc\n", "abc"), None);
        // Exact id match only — a longer/shorter id is a different block.
        assert_eq!(find_block_byte("para ^abcdef\n", "abc"), None);
    }

    #[test]
    fn scan_blocks_enumerates_lines_with_ids() {
        let text = "# Title\n\nA paragraph. ^p1\n\n- a list item\n- tagged item ^li\n";
        let blocks = scan_blocks(text);
        // Heading, two paragraph/list lines with ids, one without.
        let previews: Vec<&str> = blocks.iter().map(|b| b.preview.as_str()).collect();
        assert_eq!(
            previews,
            vec!["# Title", "A paragraph.", "- a list item", "- tagged item"],
        );
        let ids: Vec<Option<&str>> =
            blocks.iter().map(|b| b.existing_id.as_deref()).collect();
        assert_eq!(ids, vec![None, Some("p1"), None, Some("li")]);
        // Each range slices back to the marker-bearing source line.
        let tagged = &blocks[1];
        assert_eq!(&text[tagged.range.clone()], "A paragraph. ^p1");
    }

    #[test]
    fn scan_blocks_skips_blank_and_fenced_lines() {
        let text = "intro\n```\ncode ^infence\n```\noutro ^out\n";
        let blocks = scan_blocks(text);
        let previews: Vec<&str> = blocks.iter().map(|b| b.preview.as_str()).collect();
        // The fence delimiters and the in-fence code line are not blocks.
        assert_eq!(previews, vec!["intro", "outro"]);
        assert_eq!(blocks[1].existing_id.as_deref(), Some("out"));
    }

    #[test]
    fn fresh_block_id_is_unique_and_valid() {
        let text = "first block\nsecond block\nthird block\n";
        let blocks = scan_blocks(text);
        let id_a = fresh_block_id(text, &blocks[0].range);
        let id_b = fresh_block_id(text, &blocks[1].range);
        // Distinct blocks get distinct ids; every id is a valid block-id token.
        assert_ne!(id_a, id_b);
        for id in [&id_a, &id_b] {
            assert!(!id.is_empty());
            assert!(id.bytes().all(is_block_id_byte), "{id} is a valid block id");
            // Deterministic: same block → same id.
        }
        assert_eq!(id_a, fresh_block_id(text, &blocks[0].range));
    }

    #[test]
    fn fresh_block_id_avoids_collision_with_existing_marker() {
        // Force a collision: take the id block-1 would naturally derive and
        // pretend block-0 already carries it. The generator must suffix it.
        let plain = "alpha\nbeta\n";
        let blocks = scan_blocks(plain);
        let natural = fresh_block_id(plain, &blocks[1].range);
        let text = format!("alpha ^{natural}\nbeta\n");
        let blocks2 = scan_blocks(&text);
        let regenerated = fresh_block_id(&text, &blocks2[1].range);
        assert_ne!(regenerated, natural, "must dodge the existing marker id");
        assert!(regenerated.starts_with(&natural) && regenerated.contains('-'));
    }

    #[test]
    fn inject_block_marker_appends_and_is_idempotent() {
        let text = "first block\nsecond block\n";
        let blocks = scan_blocks(text);
        let injected = inject_block_marker(text, &blocks[0].range, "x1");
        assert_eq!(injected, "first block ^x1\nsecond block\n");
        // Re-injecting the same id on the now-marked block is a no-op.
        let blocks2 = scan_blocks(&injected);
        assert_eq!(inject_block_marker(&injected, &blocks2[0].range, "x1"), injected);
        // And the injected marker is discoverable by the read side.
        assert_eq!(find_block_byte(&injected, "x1"), Some(0));
    }

    #[test]
    fn inject_then_find_round_trips_for_un_anchored_block() {
        // End-to-end: pick an un-anchored block, mint an id, inject it, and the
        // read side resolves a `#^id` anchor to the block's start byte.
        let text = "intro\n\nThe target paragraph.\n\ntail\n";
        let blocks = scan_blocks(text);
        let target = blocks.iter().find(|b| b.preview == "The target paragraph.").unwrap();
        let id = fresh_block_id(text, &target.range);
        let injected = inject_block_marker(text, &target.range, &id);
        let want = injected.find("The target").unwrap();
        assert_eq!(find_block_byte(&injected, &id), Some(want));
    }

    #[test]
    fn shortest_unambiguous_grows_to_full_when_needed() {
        let paths = vec![
            "a/sub/Note.md".to_string(),
            "b/sub/Note.md".to_string(),
        ];
        assert_eq!(shortest_unambiguous(&paths, "a/sub/Note.md"), "a/sub/Note");
    }

    #[test]
    fn parse_spec_target_accepts_slugs_only() {
        assert_eq!(parse_spec_target("spec:trail-delete-cascade"), Some("trail-delete-cascade"));
        assert_eq!(parse_spec_target("spec:a-1"), Some("a-1"));
        assert_eq!(parse_spec_target("spec:NoDash"), None, "uppercase / no dash");
        assert_eq!(parse_spec_target("spec:"), None);
        assert_eq!(parse_spec_target("spec:has space-x"), None);
        assert_eq!(parse_spec_target("code:hiker/x-y"), None, "other namespace");
        assert_eq!(parse_spec_target("plain-page"), None, "no prefix");
    }

    #[test]
    fn scan_spec_anchors_finds_bare_tokens_outside_fences() {
        let text = "Defines the thing. [my-slug]\n\
                    status:: done\n\
                    ```\nexample text [fenced-slug]\n```\n\
                    a [link-label](url) and a [[wiki-link]] and [x] checkbox\n\
                    second [other-slug] and [my-slug] again\n";
        assert_eq!(scan_spec_anchors(text), vec!["my-slug", "other-slug"], "deduped, fence/md-link/wikilink excluded");
    }

    #[test]
    fn find_slug_anchor_byte_lands_on_the_defining_line() {
        let text = "intro\n```\n[in-fence]\n```\nThe definition line. [the-slug]\nafter\n";
        let want = text.find("The definition").unwrap();
        assert_eq!(find_slug_anchor_byte(text, "the-slug"), Some(want));
        assert_eq!(find_slug_anchor_byte(text, "in-fence"), None, "fenced anchor ignored");
        assert_eq!(find_slug_anchor_byte(text, "no-such"), None);
    }
}
