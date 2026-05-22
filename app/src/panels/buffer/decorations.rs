//! Editor decoration layers for the buffer panel: chunk-boundary tints
//! and the index-diff gutter markers. These hang off `editor_core`'s
//! `Editor` (not the panel's `BufCtx`) because they're computed inside the
//! `cached!` closures in `show_editor`, which already hold a `&mut
//! buffer.decoration_cache` and so can only borrow `buffer.editor`
//! immutably. Pulled out of `buffer/mod.rs` to keep that file under the
//! workspace's per-file length cap; the panel imports the `EditorDecorations`
//! trait to call these.

/// Decoration-layer methods that hang off an `Editor` rather than
/// `BufCtx` — they're called from inside the `cached!` closures in
/// `show_editor`, which already hold a `&mut buffer.decoration_cache`
/// and so can only borrow `buffer.editor` immutably. Trait methods on
/// `&self` are exempt from `clippy::single_call_fn`.
pub(super) trait EditorDecorations {
    /// Build a decoration layer that paints a subtle line tint plus a
    /// gutter marker on every chunk-start line, matching the indexer's
    /// heading-aware chunk boundaries (`view-show-chunk-boundaries`).
    fn chunk_boundary_decorations(&self) -> editor_core::decoration::Set;
    /// Compute a `Set` that places `GutterMarker::DiffAdded`,
    /// `DiffRemoved`, or `DiffModified` on every line of the live
    /// buffer that diverges from `loaded_text` (the most recent disk
    /// read / write).
    fn index_diff_decorations(&self, loaded_text: &str) -> editor_core::decoration::Set;
}

impl EditorDecorations for editor_core::state::Editor {
    fn chunk_boundary_decorations(&self) -> editor_core::decoration::Set {
        use editor_core::decoration::Color;
        use editor_core::decoration::Decoration;
        use editor_core::decoration::GutterMarker;
        use editor_core::decoration::LineStyle;
        let editor = self;
        let text = editor.doc.to_string();
        let chunks = hiker_core::chunker::markdown::chunk(&text);
        let mut set = editor_core::decoration::Set::empty();
        // Faint stripe color (light blue) — picked to be visible against
        // both light and dark themes.
        let stripe = Color::rgba(0x66, 0x99, 0xff, 0x18);
        for (idx, chunk) in chunks.iter().enumerate() {
            if idx == 0 {
                continue; // The first chunk starts at the doc head — skip.
            }
            let byte = chunk.byte_start;
            if byte >= text.len() {
                continue;
            }
            let line = editor.doc.byte_to_line(byte);
            let line_start = editor.doc.line_to_byte(line);
            let line_end = if line + 1 < editor.doc.len_lines() {
                editor.doc.line_to_byte(line + 1)
            } else {
                editor.doc.len_bytes()
            };
            let style = LineStyle {
                bg: Some(stripe),
                gutter_marker: Some(GutterMarker::Custom(smol_str::SmolStr::new("S"))),
                ..LineStyle::default()
            };
            set = set.insert(line_start..line_end, Decoration::Line(style));
        }
        set
    }

    /// Strategy: line-level diff via `hiker_core::diff::compute`. Each
    /// Insert in the diff is a line in `after` that has no exact
    /// counterpart in `before`. We emit `DiffModified` when a Delete on
    /// the same after-line preceded the Insert (i.e. a replace),
    /// otherwise `DiffAdded`. Pure Deletes don't have a corresponding
    /// `after` line to mark, so we collapse adjacent Delete-only runs
    /// onto the nearest following surviving line as `DiffRemoved`
    /// (matches the legacy gutter behavior).
    fn index_diff_decorations(&self, loaded_text: &str) -> editor_core::decoration::Set {
        use editor_core::decoration::Decoration;
        use editor_core::decoration::GutterMarker;
        use editor_core::decoration::LineStyle;
        use editor_core::rangeset::RangeSet;
        use hiker_core::diff::Op;
        let state = self;
        let live = state.doc.to_string();
        if loaded_text == live {
            return RangeSet::empty();
        }
        let diff = hiker_core::diff::compute(loaded_text, &live);
        let mut per_after_line: std::collections::BTreeMap<u32, GutterMarker> =
            std::collections::BTreeMap::new();
        let mut pending_delete = false;
        let mut last_after_seen: u32 = 0;
        for hunk in &diff.hunks {
            for line in &hunk.lines {
                match line.op {
                    Op::Equal => {
                        if let Some(an) = line.after_line_no {
                            last_after_seen = an;
                            if pending_delete {
                                per_after_line.entry(an).or_insert(GutterMarker::DiffRemoved);
                                pending_delete = false;
                            }
                        }
                    }
                    Op::Insert => {
                        if let Some(an) = line.after_line_no {
                            let marker = if pending_delete {
                                pending_delete = false;
                                GutterMarker::DiffModified
                            } else {
                                GutterMarker::DiffAdded
                            };
                            per_after_line.insert(an, marker);
                            last_after_seen = an;
                        }
                    }
                    Op::Delete => {
                        pending_delete = true;
                    }
                }
            }
        }
        if pending_delete && last_after_seen > 0 {
            per_after_line
                .entry(last_after_seen)
                .or_insert(GutterMarker::DiffRemoved);
        }

        let doc = &state.doc;
        let total_bytes = doc.len_bytes();
        let total_lines = doc.len_lines();
        let mut entries: Vec<(std::ops::Range<usize>, Decoration)> =
            Vec::with_capacity(per_after_line.len());
        for (line1, marker) in per_after_line {
            let line0 = line1.saturating_sub(1) as usize;
            if line0 >= total_lines {
                continue;
            }
            let line_start = doc.line_to_byte(line0);
            let line_end = if line0 + 1 < total_lines {
                doc.line_to_byte(line0 + 1)
            } else {
                total_bytes
            };
            let range = if line_start == line_end {
                line_start..line_start + 1
            } else {
                line_start..line_end
            };
            entries.push((
                range,
                Decoration::Line(LineStyle {
                    gutter_marker: Some(marker),
                    ..LineStyle::default()
                }),
            ));
        }
        RangeSet::from_iter(entries)
    }
}
