//! Markdown formatting toolbar: Bold / Italic / Strikethrough / Highlight /
//! inline Code / Code block / bullet + numbered List / text Color buttons that
//! wrap or prefix the active selection with the matching markdown syntax.
//!
//! The text transforms are pure functions over `(doc_text, selection)` →
//! [`Rewrite`] (a single byte-range replacement plus where the selection should
//! land), so they're unit-tested without an editor. [`apply`] turns a `Rewrite`
//! into an editor [`Transaction`], applies it to the buffer, and mirrors it into
//! the op-log `working` layer via [`super::editor_binding::run`] — the same path
//! a keystroke takes — so a formatting click is just another user edit.
//!
//! Highlight uses `==text==` and text color uses an HTML
//! `<span style="color:#rrggbb">…</span>`; both render inline via the markdown
//! decoration provider (`editor-md`). Bold/italic/strikethrough/code/lists use
//! plain CommonMark/GFM the provider already styles.

use std::ops::Range;

use eframe::egui;

use editor_core::selection::{SelRange, Selection};
use editor_core::transaction::{EditType, Transaction};

use crate::state::AppState;

/// Forest / hiking themed swatches for the text-color picker. Each entry is a
/// (name, `#rrggbb`) pair; the hex is embedded verbatim in the emitted span so
/// the file stays portable and the live-preview renderer can re-parse it.
const PALETTE: &[(&str, &str)] = &[
    ("Pine", "#2e5e3a"),
    ("Fern", "#4f7942"),
    ("Moss", "#7a9a65"),
    ("Lake", "#2e7d7b"),
    ("Sky", "#4a7a96"),
    ("Bark", "#6f4e37"),
    ("Clay", "#b5651d"),
    ("Berry", "#a33b4f"),
    ("Lavender", "#7e6b9e"),
];

/// A pure description of one formatting edit: replace `replace` with `with`,
/// then place the selection at `select` (both in *new-document* byte
/// coordinates, since the replacement length differs from the original).
#[derive(Debug, PartialEq, Eq)]
struct Rewrite {
    replace: Range<usize>,
    with: String,
    select: Range<usize>,
}

/// Toolbar render context, mirroring `toolbar_menus::Menus` — bundles the egui
/// `ui`, the app, and the buffer `path` so the button builders are methods.
pub(super) struct FormatBar<'a> {
    pub(super) ui: &'a mut egui::Ui,
    pub(super) app: &'a mut AppState,
    pub(super) path: &'a str,
}

impl FormatBar<'_> {
    /// Draw the formatting button group. Inserted into the editor toolbar after
    /// a separator; only shown for editable vault buffers (the caller gates it).
    pub(super) fn render(&mut self) {
        self.ui.separator();
        self.wrap_btn(egui::RichText::new("B").strong(), "Bold (**)", "**", "**");
        self.wrap_btn(egui::RichText::new("I").italics(), "Italic (*)", "*", "*");
        self.wrap_btn(
            egui::RichText::new("S").strikethrough(),
            "Strikethrough (~~)",
            "~~",
            "~~",
        );
        self.wrap_btn(
            egui::RichText::new("H").background_color(egui::Color32::from_rgb(0xe8, 0xcf, 0x5b)),
            "Highlight (==)",
            "==",
            "==",
        );
        self.wrap_btn(egui::RichText::new("<>").monospace(), "Inline code (`)", "`", "`");
        if self
            .ui
            .add(egui::Button::new(egui::RichText::new("{}").monospace()).small())
            .on_hover_text("Code block (```)")
            .clicked()
            && let Some((text, sel)) = sel_range(self.app, self.path)
        {
            apply(self.app, self.path, code_block(&text, sel));
        }
        self.list_btn(egui::RichText::new("\u{2022}"), "Bullet list", false);
        self.list_btn(egui::RichText::new("1."), "Numbered list", true);
        self.color_btn();
    }

    /// A wrap-style button (bold / italic / strikethrough / highlight / code):
    /// toggle the `prefix`/`suffix` markers around the active selection.
    fn wrap_btn(
        &mut self,
        label: egui::RichText,
        tip: &str,
        prefix: &'static str,
        suffix: &'static str,
    ) {
        if self.ui.add(egui::Button::new(label).small()).on_hover_text(tip).clicked()
            && let Some((text, sel)) = sel_range(self.app, self.path)
        {
            apply(self.app, self.path, inline_wrap(&text, sel, prefix, suffix));
        }
    }

    /// A list button: toggle a bullet (`- `) or ordered (`1. `) marker on every
    /// line the selection spans.
    fn list_btn(&mut self, label: egui::RichText, tip: &str, ordered: bool) {
        if self.ui.add(egui::Button::new(label).small()).on_hover_text(tip).clicked()
            && let Some((text, sel)) = sel_range(self.app, self.path)
        {
            apply(self.app, self.path, toggle_list(&text, sel, ordered));
        }
    }

    /// Text-color button: opens a swatch popup; a click wraps the selection in a
    /// colored `<span>` (or recolors / removes an existing one).
    fn color_btn(&mut self) {
        let resp = self
            .ui
            .add(egui::Button::new(egui::RichText::new("A").color(egui::Color32::from_rgb(0x2e, 0x5e, 0x3a))).small())
            .on_hover_text("Text color");
        let mut chosen: Option<&'static str> = None;
        egui::Popup::menu(&resp).show(|ui| {
            ui.label(egui::RichText::new("Text color").small().strong());
            ui.horizontal_wrapped(|ui| {
                for (name, hex) in PALETTE {
                    let swatch = ui
                        .add_sized(
                            [18.0, 18.0],
                            egui::Button::new("").fill(hex_to_color32(hex)),
                        )
                        .on_hover_text(*name);
                    if swatch.clicked() {
                        chosen = Some(hex);
                        ui.close();
                    }
                }
            });
        });
        if let Some(hex) = chosen
            && let Some((text, sel)) = sel_range(self.app, self.path)
        {
            apply(self.app, self.path, color_span(&text, sel, hex));
        }
    }
}

/// The active buffer's full text + the main selection as a byte range, or
/// `None` if the buffer isn't loaded.
fn sel_range(app: &AppState, path: &str) -> Option<(String, Range<usize>)> {
    let b = app.session.buffers.get(path)?;
    let m = b.editor.selection.main();
    Some((b.editor.doc.to_string(), m.start()..m.end()))
}

/// Apply a [`Rewrite`] to the buffer as a user edit: build the transaction,
/// apply it to `editor`, then run the op-log binding with that transaction so
/// the edit lands on the `working` layer exactly like a keystroke would.
fn apply(app: &mut AppState, path: &str, rw: Rewrite) {
    let txn = {
        let Some(buffer) = app.session.buffers.get_mut(path) else { return };
        let doc_len = buffer.editor.doc.len_bytes();
        let set = editor_core::change::Set::of(doc_len, [(rw.replace, rw.with)]);
        let sel = Selection::from_range(SelRange::new(rw.select.start, rw.select.end));
        let txn = Transaction::new(set).with_selection(sel).with_edit_type(EditType::Input);
        buffer.editor = buffer.editor.apply(txn.clone());
        txn
    };
    super::editor_binding::run(app, path, std::slice::from_ref(&txn));
}

// ── pure text transforms ────────────────────────────────────────────────────

/// Toggle inline `prefix`/`suffix` markers around the selection. If the
/// selection already carries the markers (inside it, or immediately
/// surrounding it) they're stripped; otherwise the selection is wrapped. An
/// empty selection inserts the marker pair with the cursor between them.
fn inline_wrap(text: &str, sel: Range<usize>, prefix: &str, suffix: &str) -> Rewrite {
    let (s, e) = (sel.start, sel.end);
    let inner = &text[s..e];
    let (pl, sl) = (prefix.len(), suffix.len());
    // Toggle off: markers sit inside the selection (`**bold**` selected).
    if e - s >= pl + sl && inner.starts_with(prefix) && inner.ends_with(suffix) {
        let core = &inner[pl..inner.len() - sl];
        return Rewrite { replace: s..e, with: core.to_string(), select: s..s + core.len() };
    }
    // Toggle off: markers immediately surround the selection (`bold` selected
    // inside `**bold**`).
    if s >= pl
        && e + sl <= text.len()
        && text.is_char_boundary(s - pl)
        && text.is_char_boundary(e + sl)
        && &text[s - pl..s] == prefix
        && &text[e..e + sl] == suffix
    {
        let start = s - pl;
        return Rewrite { replace: start..e + sl, with: inner.to_string(), select: start..start + inner.len() };
    }
    // Wrap on.
    let inner_start = s + pl;
    Rewrite {
        replace: s..e,
        with: format!("{prefix}{inner}{suffix}"),
        select: inner_start..inner_start + inner.len(),
    }
}

/// Wrap the selection in a fenced code block on its own lines. Leading /
/// trailing newlines are added only when the selection isn't already at a line
/// boundary, so the fences never glue onto neighbouring text.
fn code_block(text: &str, sel: Range<usize>) -> Rewrite {
    let (s, e) = (sel.start, sel.end);
    let inner = &text[s..e];
    let bytes = text.as_bytes();
    let needs_lead = s > 0 && bytes[s - 1] != b'\n';
    let needs_trail = e < text.len() && bytes[e] != b'\n';
    let mut out = String::new();
    if needs_lead {
        out.push('\n');
    }
    out.push_str("```\n");
    let body_start = s + out.len();
    out.push_str(inner);
    if inner.is_empty() || !inner.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```");
    if needs_trail {
        out.push('\n');
    }
    Rewrite { replace: s..e, with: out, select: body_start..body_start + inner.len() }
}

/// Toggle a list marker on every line the selection spans. When every
/// non-blank line already carries a marker of the requested kind it's removed;
/// otherwise a marker is added (ordered lists renumber from 1).
fn toggle_list(text: &str, sel: Range<usize>, ordered: bool) -> Rewrite {
    let block_start = line_start_of(text, sel.start);
    let block_end = line_end_of(text, sel.end.max(sel.start));
    let block = &text[block_start..block_end];
    let lines: Vec<&str> = block.split('\n').collect();
    let all_marked = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| marker_len(l, ordered) > 0);

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut n = 1u32;
    for line in &lines {
        if line.trim().is_empty() {
            out.push((*line).to_string());
            continue;
        }
        let (indent, rest) = split_indent(line);
        if all_marked {
            let cut = marker_len(line, ordered);
            // `cut` counts from the indent end; preserve the indent.
            out.push(format!("{indent}{}", &rest[(cut - indent.len())..]));
        } else if ordered {
            out.push(format!("{indent}{n}. {rest}"));
            n += 1;
        } else {
            out.push(format!("{indent}- {rest}"));
        }
    }
    let with = out.join("\n");
    let len = with.len();
    Rewrite { replace: block_start..block_end, with, select: block_start..block_start + len }
}

/// Wrap the selection in a colored `<span>`. If the selection is exactly an
/// existing color span: same color toggles it off, a different color recolors.
fn color_span(text: &str, sel: Range<usize>, hex: &str) -> Rewrite {
    let (s, e) = (sel.start, sel.end);
    let inner = &text[s..e];
    if let Some((existing, content)) = parse_color_span(inner) {
        if existing.eq_ignore_ascii_case(hex) {
            return Rewrite { replace: s..e, with: content.to_string(), select: s..s + content.len() };
        }
        let prefix = color_open(hex);
        let cs = s + prefix.len();
        let with = format!("{prefix}{content}</span>");
        return Rewrite { replace: s..e, with, select: cs..cs + content.len() };
    }
    let prefix = color_open(hex);
    let cs = s + prefix.len();
    let with = format!("{prefix}{inner}</span>");
    Rewrite { replace: s..e, with, select: cs..cs + inner.len() }
}

fn color_open(hex: &str) -> String {
    format!("<span style=\"color:{hex}\">")
}

/// If `s` is exactly `<span style="color:HEX">CONTENT</span>`, return
/// `(HEX, CONTENT)`.
fn parse_color_span(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix("<span style=\"color:")?;
    let close_quote = rest.find('"')?;
    let hex = &rest[..close_quote];
    let after_tag = rest[close_quote..].strip_prefix("\">")?;
    let content = after_tag.strip_suffix("</span>")?;
    Some((hex, content))
}

/// Byte offset of the start of the line containing `byte`.
fn line_start_of(text: &str, byte: usize) -> usize {
    text[..byte.min(text.len())].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte offset of the end of the line containing `byte` (the next `\n`, or EOF).
fn line_end_of(text: &str, byte: usize) -> usize {
    let from = byte.min(text.len());
    text[from..].find('\n').map(|i| from + i).unwrap_or(text.len())
}

/// Split a line into its leading whitespace and the remainder.
fn split_indent(line: &str) -> (&str, &str) {
    let n = line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
    (&line[..n], &line[n..])
}

/// Length (in bytes, from the line start) of a leading list marker of the
/// requested kind, including its trailing space; 0 if absent. Ordered markers
/// are `<digits>. ` / `<digits>) `; bullets are `- ` / `* ` / `+ `.
fn marker_len(line: &str, ordered: bool) -> usize {
    let (indent, rest) = split_indent(line);
    let b = rest.as_bytes();
    if ordered {
        let digits = b.iter().take_while(|&&c| c.is_ascii_digit()).count();
        if digits > 0
            && b.len() > digits
            && (b[digits] == b'.' || b[digits] == b')')
            && b.get(digits + 1) == Some(&b' ')
        {
            return indent.len() + digits + 2;
        }
        0
    } else if matches!(b.first(), Some(b'-' | b'*' | b'+')) && b.get(1) == Some(&b' ') {
        indent.len() + 2
    } else {
        0
    }
}

fn hex_to_color32(hex: &str) -> egui::Color32 {
    let h = hex.trim_start_matches('#');
    let parse = |r: Range<usize>| u8::from_str_radix(h.get(r).unwrap_or("0"), 16).unwrap_or(0);
    if h.len() == 6 {
        egui::Color32::from_rgb(parse(0..2), parse(2..4), parse(4..6))
    } else {
        egui::Color32::GRAY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(replace: Range<usize>, with: &str, select: Range<usize>) -> Rewrite {
        Rewrite { replace, with: with.to_string(), select }
    }

    #[test]
    fn wrap_bold_around_selection() {
        // "hello" with "ell" (1..4) selected → "h**ell**o", inner stays selected.
        assert_eq!(inline_wrap("hello", 1..4, "**", "**"), rw(1..4, "**ell**", 3..6));
    }

    #[test]
    fn wrap_empty_selection_places_cursor_between_markers() {
        let r = inline_wrap("ab", 1..1, "**", "**");
        assert_eq!(r, rw(1..1, "****", 3..3));
    }

    #[test]
    fn unwrap_when_markers_inside_selection() {
        // "**ell**" (1..8) selected in "h**ell**o" → strip back to "ell".
        assert_eq!(inline_wrap("h**ell**o", 1..8, "**", "**"), rw(1..8, "ell", 1..4));
    }

    #[test]
    fn unwrap_when_markers_surround_selection() {
        // "ell" (3..6) selected in "h**ell**o" → markers around it are removed.
        assert_eq!(inline_wrap("h**ell**o", 3..6, "**", "**"), rw(1..8, "ell", 1..4));
    }

    #[test]
    fn highlight_uses_double_equals() {
        assert_eq!(inline_wrap("note", 0..4, "==", "=="), rw(0..4, "==note==", 2..6));
    }

    #[test]
    fn code_block_wraps_whole_lines() {
        // Whole single line selected (no surrounding newlines on a one-line doc).
        let r = code_block("let x = 1;", 0..10);
        assert_eq!(r.with, "```\nlet x = 1;\n```");
        assert_eq!(r.replace, 0..10);
        assert_eq!(&r.with[r.select.start..r.select.end], "let x = 1;");
    }

    #[test]
    fn code_block_adds_boundary_newlines_midline() {
        // Selection in the middle of a line gets leading + trailing newlines.
        let r = code_block("ab cd ef", 3..5); // "cd"
        assert_eq!(r.with, "\n```\ncd\n```\n");
    }

    #[test]
    fn bullet_list_adds_markers_per_line() {
        let r = toggle_list("one\ntwo", 0..7, false);
        assert_eq!(r.with, "- one\n- two");
    }

    #[test]
    fn bullet_list_toggles_off_when_all_marked() {
        let r = toggle_list("- one\n- two", 0..11, false);
        assert_eq!(r.with, "one\ntwo");
    }

    #[test]
    fn numbered_list_renumbers_from_one() {
        let r = toggle_list("a\nb\nc", 0..5, true);
        assert_eq!(r.with, "1. a\n2. b\n3. c");
    }

    #[test]
    fn numbered_list_toggles_off() {
        let r = toggle_list("1. a\n2. b", 0..9, true);
        assert_eq!(r.with, "a\nb");
    }

    #[test]
    fn list_preserves_indent_and_skips_blank_lines() {
        let r = toggle_list("  a\n\n  b", 0..8, false);
        assert_eq!(r.with, "  - a\n\n  - b");
    }

    #[test]
    fn color_span_wraps_selection() {
        let r = color_span("red text", 0..3, "#a33b4f");
        assert_eq!(r.with, "<span style=\"color:#a33b4f\">red</span>");
        assert_eq!(&r.with[r.select.start..r.select.end], "red");
    }

    #[test]
    fn color_span_toggles_off_same_color() {
        let span = "<span style=\"color:#a33b4f\">red</span>";
        let r = color_span(span, 0..span.len(), "#a33b4f");
        assert_eq!(r.with, "red");
    }

    #[test]
    fn color_span_recolors_existing() {
        let span = "<span style=\"color:#a33b4f\">red</span>";
        let r = color_span(span, 0..span.len(), "#2e5e3a");
        assert_eq!(r.with, "<span style=\"color:#2e5e3a\">red</span>");
    }

    #[test]
    fn parse_color_span_round_trips() {
        assert_eq!(
            parse_color_span("<span style=\"color:#2e7d7b\">lake</span>"),
            Some(("#2e7d7b", "lake"))
        );
        assert_eq!(parse_color_span("plain"), None);
    }
}
