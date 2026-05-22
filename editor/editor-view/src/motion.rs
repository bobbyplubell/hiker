//! Cursor motion. Pure functions over state → new selection.

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::rope::Rope;

use editor_core::selection::SelRange;

use editor_core::selection::Selection;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub fn move_char(
    state: &EditorState,
    dir: Direction,
    extend: bool,
    layers: &[DecorationSet],
) -> Selection {
    map_heads(state, extend, |doc, _line, head| {
        let candidate = match dir {
            Direction::Left => doc.prev_char_boundary(head),
            Direction::Right => doc.next_char_boundary(head),
            _ => head,
        };
        skip_atomic_ranges(doc, layers, candidate, dir)
    })
}

pub fn move_vertical(
    state: &EditorState,
    dir: Direction,
    extend: bool,
    page_lines: usize,
) -> Selection {
    move_vertical_wrapped(state, dir, extend, page_lines, None)
}

/// VLine-aware vertical motion. When `wrap` is `Some`, up/down moves between
/// visual rows (within the same buffer line when wrapped) instead of always
/// jumping to the next/prev buffer line. Pass `None` to get the legacy
/// buffer-line-only motion.
pub fn move_vertical_wrapped(
    state: &EditorState,
    dir: Direction,
    extend: bool,
    page_lines: usize,
    wrap: Option<&crate::wrapping::WrapMap>,
) -> Selection {
    debug_assert!(matches!(dir, Direction::Up | Direction::Down));
    let step = page_lines.max(1) as isize;
    let delta_sign: isize = if dir == Direction::Up { -1 } else { 1 };

    let mut new_ranges = Vec::with_capacity(state.selection.ranges().len());
    for r in state.selection.ranges() {
        let head = r.head.offset();
        let line = state.doc.byte_to_line(head);
        let line_start = state.doc.line_to_byte(line);
        let col_bytes = head - line_start;

        let (cur_vline_idx, cur_vline_col) = wrap
            .and_then(|w| w.peek(line))
            .map(|w| {
                let (vi, off) = w.vline_at_byte(col_bytes);
                (vi as isize, off)
            })
            .unwrap_or((0, col_bytes));

        let goal_col = r.goal_col.unwrap_or(cur_vline_col as u32);

        // Accumulate `step` vlines in the given direction across buffer-line
        // boundaries.
        let mut remaining = step * delta_sign;
        let mut tgt_line = line as isize;
        let mut tgt_vline = cur_vline_idx + remaining;
        // Walk lines until tgt_vline is within [0, vline_count(tgt_line)).
        loop {
            if tgt_line < 0 {
                tgt_line = 0;
                tgt_vline = 0;
                break;
            }
            let line_vcount = wrap
                .and_then(|w| w.peek(tgt_line as usize).map(|wl| wl.visual_count() as isize))
                .unwrap_or(1);
            if tgt_vline < 0 {
                tgt_line -= 1;
                if tgt_line < 0 {
                    tgt_line = 0;
                    tgt_vline = 0;
                    break;
                }
                let prev_vc = wrap
                    .and_then(|w| w.peek(tgt_line as usize).map(|wl| wl.visual_count() as isize))
                    .unwrap_or(1);
                tgt_vline += prev_vc;
            } else if tgt_vline >= line_vcount {
                let next = tgt_line + 1;
                if next as usize >= state.doc.len_lines() {
                    tgt_vline = line_vcount - 1;
                    break;
                }
                tgt_vline -= line_vcount;
                tgt_line = next;
            } else {
                break;
            }
            // safety: prevent infinite loops
            remaining -= delta_sign;
            if remaining.abs() > step.abs() + 1_000_000 {
                break;
            }
        }

        let tgt_line = (tgt_line.max(0) as usize).min(state.doc.len_lines().saturating_sub(1));
        let tgt_vline = tgt_vline.max(0) as usize;
        let new_head = {
            let line_start = state.doc.line_to_byte(tgt_line);
            let line_len = state.doc.line_len_bytes(tgt_line);
            let (vstart, vend) = wrap
                .and_then(|w| w.peek(tgt_line))
                .map(|w| {
                    let i = tgt_vline.min(w.visual_count().saturating_sub(1));
                    w.vline_range(i)
                })
                .unwrap_or((0, line_len));
            let max_col = vend.saturating_sub(vstart);
            let col = (goal_col as usize).min(max_col);
            let mut nh = line_start + vstart + col;
            while nh > line_start + vstart
                && nh < state.doc.len_bytes()
                && (state.doc.byte_at(nh) & 0b1100_0000) == 0b1000_0000
            {
                nh -= 1;
            }
            nh
        };

        let new_anchor = if extend { r.anchor.offset() } else { new_head };
        let mut nr = SelRange::new(new_anchor, new_head);
        nr.goal_col = Some(goal_col);
        new_ranges.push(nr);
    }
    Selection::from_ranges(new_ranges, state.selection.main_index())
}

pub fn move_line_edge(state: &EditorState, end: bool, extend: bool) -> Selection {
    map_heads(state, extend, |doc, line, _head| {
        let start = doc.line_to_byte(line);
        if end {
            start + doc.line_len_bytes(line)
        } else {
            start
        }
    })
}

pub fn move_doc_edge(state: &EditorState, end: bool, extend: bool) -> Selection {
    map_heads(state, extend, |doc, _line, _head| {
        if end {
            doc.len_bytes()
        } else {
            0
        }
    })
}

pub fn move_word(
    state: &EditorState,
    dir: Direction,
    extend: bool,
    layers: &[DecorationSet],
) -> Selection {
    map_heads(state, extend, |doc, line, head| {
        let line_start = doc.line_to_byte(line);
        let line_text = doc.line_str(line);
        let local = head - line_start;
        let candidate = match dir {
            Direction::Left => {
                if local == 0 && line > 0 {
                    let prev = line - 1;
                    doc.line_to_byte(prev) + doc.line_len_bytes(prev)
                } else {
                    line_start + {
                        let mut iter = line_text.unicode_word_indices().rev();
                        let mut last: Option<usize> = None;
                        for (i, _) in iter.by_ref() {
                            if i < local {
                                last = Some(i);
                                break;
                            }
                        }
                        last.unwrap_or(0)
                    }
                }
            }
            Direction::Right => {
                if local == line_text.len() && line + 1 < doc.len_lines() {
                    doc.line_to_byte(line + 1)
                } else {
                    line_start + {
                        let mut end_pos = line_text.len();
                        for (i, w) in line_text.unicode_word_indices() {
                            let e = i + w.len();
                            if e > local {
                                end_pos = e;
                                break;
                            }
                        }
                        end_pos
                    }
                }
            }
            _ => head,
        };
        skip_atomic_ranges(doc, layers, candidate, dir)
    })
}

pub fn select_all(state: &EditorState) -> Selection {
    Selection::from_range(SelRange::new(0, state.doc.len_bytes()))
}

fn map_heads<F: Fn(&Rope, usize, usize) -> usize>(
    state: &EditorState,
    extend: bool,
    f: F,
) -> Selection {
    let mut new_ranges = Vec::with_capacity(state.selection.ranges().len());
    for r in state.selection.ranges() {
        let head = r.head.offset();
        let line = state.doc.byte_to_line(head);
        let new_head = f(&state.doc, line, head);
        let new_anchor = if extend { r.anchor.offset() } else { new_head };
        new_ranges.push(SelRange::new(new_anchor, new_head));
    }
    Selection::from_ranges(new_ranges, state.selection.main_index())
}

/// If `pos` falls strictly inside an atomic decoration range in any layer,
/// snap to the boundary of that range in the direction of motion. Ranges are
/// atomic when they are `Decoration::Replace` or `Decoration::Mark` with
/// `MarkStyle::atomic = true`. The snap is iterated to handle overlapping or
/// adjacent atomic ranges (snapping out of one might land inside another).
pub fn skip_atomic_ranges(
    doc: &Rope,
    layers: &[DecorationSet],
    pos: usize,
    dir: Direction,
) -> usize {
    let doc_len = doc.len_bytes();
    let mut cur = pos.min(doc_len);
    // Iterate a bounded number of times to avoid pathological cycles.
    for _ in 0..layers.len().saturating_mul(8).max(8) {
        let mut snapped: Option<usize> = None;
        for layer in layers {
            for (range, deco) in layer.iter_all() {
                let atomic = match deco {
                    Decoration::Replace { .. } => true,
                    Decoration::Mark(style) => style.atomic,
                    _ => false,
                };
                if !atomic {
                    continue;
                }
                if range.start >= range.end {
                    continue;
                }
                if cur > range.start && cur < range.end {
                    let target = match dir {
                        Direction::Left | Direction::Up => range.start,
                        Direction::Right | Direction::Down => range.end,
                    };
                    snapped = Some(match snapped {
                        None => target,
                        Some(prev) => match dir {
                            Direction::Left | Direction::Up => prev.min(target),
                            _ => prev.max(target),
                        },
                    });
                }
            }
        }
        match snapped {
            Some(new_pos) if new_pos != cur => cur = new_pos.min(doc_len),
            _ => break,
        }
    }
    cur
}

