//! Markdown fold regions: headings (collapse content under each heading until
//! the next same-or-lower-level heading) and nested list items.
//!
//! The host owns the `FoldState` (which fold ids are collapsed). This module
//! produces the chevrons + the per-line hide marks given that state.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::FoldChevron;

use editor_core::decoration::LineStyle;

use editor_core::rangeset::RangeSet;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Default-empty: hosts treat membership as "this id is collapsed".
pub type FoldState = HashSet<u64>;

#[derive(Clone, Debug)]
pub struct FoldRegion {
    pub id: u64,
    /// Line index of the heading / list item that owns the chevron.
    pub head_line: u32,
    /// Inclusive line range of the body that gets hidden when collapsed.
    pub body_lines: std::ops::Range<u32>,
    pub kind: FoldKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldKind {
    Heading(u8),
    List,
}

/// Walk the document and return every foldable region. IDs are stable as long
/// as the heading text / list-item-first-line text doesn't change.
pub fn fold_regions(state: &EditorState) -> Vec<FoldRegion> {
    let text = state.doc.to_string();
    let doc_len = text.len();
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_GFM);
    let parser = Parser::new_ext(&text, opts).into_offset_iter();

    let line_of = |byte: usize| -> u32 { state.doc.byte_to_line(byte.min(doc_len)) as u32 };

    // Stage 1: collect heading line + level + heading text.
    let mut headings: Vec<(u32, u8, String)> = Vec::new();
    // Stage 2: collect list-item ranges that contain nested children.
    let mut list_items: Vec<(u32, u32, String)> = Vec::new();

    // pulldown-cmark Heading events span only the title text. We record by line.
    let mut in_heading: Option<(u32, u8, String)> = None;
    // List item stack: each entry tracks first line and accumulated head text.
    struct ItemFrame {
        start_line: u32,
        end_line: u32,
        head_text: String,
        has_nested_list: bool,
    }
    let mut item_stack: Vec<ItemFrame> = Vec::new();

    for (event, byte_range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let lvl = match level {
                    HeadingLevel::H1 => 1u8,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                in_heading = Some((line_of(byte_range.start), lvl, String::new()));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(h) = in_heading.take() {
                    headings.push(h);
                }
            }
            Event::Text(ref s) if in_heading.is_some() => {
                if let Some((_, _, text)) = in_heading.as_mut() {
                    text.push_str(s);
                }
            }
            Event::Start(Tag::Item) => {
                item_stack.push(ItemFrame {
                    start_line: line_of(byte_range.start),
                    end_line: line_of(byte_range.end.saturating_sub(1).max(byte_range.start)),
                    head_text: String::new(),
                    has_nested_list: false,
                });
            }
            Event::End(TagEnd::Item) => {
                if let Some(frame) = item_stack.pop() {
                    if frame.has_nested_list && frame.end_line > frame.start_line {
                        list_items.push((frame.start_line, frame.end_line, frame.head_text));
                    }
                }
            }
            Event::Start(Tag::List(_)) => {
                if let Some(parent) = item_stack.last_mut() {
                    parent.has_nested_list = true;
                }
            }
            Event::Text(s) => {
                if let Some(frame) = item_stack.last_mut() {
                    if frame.head_text.is_empty() {
                        frame.head_text.push_str(&s);
                    }
                }
            }
            _ => {}
        }
        // Track item end line as we see further events.
        if let Some(frame) = item_stack.last_mut() {
            frame.end_line = frame.end_line.max(line_of(byte_range.end.saturating_sub(1).max(byte_range.start)));
        }
    }

    // Build fold regions for headings: body is from the line AFTER the heading
    // to the line BEFORE the next heading of same or shallower level (or EOF).
    let mut regions = Vec::new();
    let total_lines = state.doc.len_lines() as u32;
    for (i, (head_line, level, text)) in headings.iter().enumerate() {
        let mut end_line = total_lines.saturating_sub(1);
        for (j, (next_line, next_level, _)) in headings.iter().enumerate().skip(i + 1) {
            let _ = j;
            if *next_level <= *level {
                end_line = next_line.saturating_sub(1);
                break;
            }
        }
        if end_line > *head_line {
            let id = fold_id(FoldKind::Heading(*level), text);
            regions.push(FoldRegion {
                id,
                head_line: *head_line,
                body_lines: (*head_line + 1)..(end_line + 1),
                kind: FoldKind::Heading(*level),
            });
        }
    }

    // Build fold regions for list items with nested children.
    for (start_line, end_line, head_text) in list_items {
        if end_line > start_line {
            let id = fold_id(FoldKind::List, &head_text);
            regions.push(FoldRegion {
                id,
                head_line: start_line,
                body_lines: (start_line + 1)..(end_line + 1),
                kind: FoldKind::List,
            });
        }
    }

    // Disambiguate IDs that collide (two headings with identical text).
    {
        let mut seen: HashMap<u64, u32> = HashMap::new();
        for r in regions.iter_mut() {
            let entry = seen.entry(r.id).or_insert(0);
            if *entry > 0 {
                // Mix in occurrence index for uniqueness.
                let mut h = std::collections::hash_map::DefaultHasher::new();
                r.id.hash(&mut h);
                (*entry as u64).hash(&mut h);
                r.id = h.finish();
            }
            *entry += 1;
        }
    }
    regions
}

/// Produce a DecorationSet that:
///   - Attaches a fold chevron to each foldable line (always visible).
///   - For each id in `state` (collapsed), hides the body lines.
pub fn fold_decorations(state: &EditorState, fold_state: &FoldState) -> DecorationSet {
    let regions = fold_regions(state);
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for region in &regions {
        let collapsed = fold_state.contains(&region.id);
        let head_byte_start = state
            .doc
            .line_to_byte(region.head_line as usize);
        let head_byte_end = if (region.head_line as usize) + 1 < state.doc.len_lines() {
            state.doc.line_to_byte((region.head_line as usize) + 1)
        } else {
            state.doc.len_bytes()
        };
        entries.push((
            head_byte_start..head_byte_end,
            Decoration::Line(LineStyle {
                fold_chevron: Some(FoldChevron { id: region.id, collapsed }),
                ..LineStyle::default()
            }),
        ));

        if collapsed {
            for line in region.body_lines.clone() {
                let line = line as usize;
                if line >= state.doc.len_lines() {
                    break;
                }
                let s = state.doc.line_to_byte(line);
                let e = if line + 1 < state.doc.len_lines() {
                    state.doc.line_to_byte(line + 1)
                } else {
                    state.doc.len_bytes()
                };
                entries.push((
                    s..e,
                    Decoration::Line(LineStyle { hide: true, ..LineStyle::default() }),
                ));
            }
        }
    }
    RangeSet::from_iter(entries)
}

fn fold_id(kind: FoldKind, text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match kind {
        FoldKind::Heading(level) => {
            "heading".hash(&mut h);
            level.hash(&mut h);
        }
        FoldKind::List => {
            "list".hash(&mut h);
        }
    }
    text.trim().hash(&mut h);
    h.finish()
}

