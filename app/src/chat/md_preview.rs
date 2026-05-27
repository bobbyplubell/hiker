//! Read-only markdown preview embedded inside chat tool-call cards.
//!
//! Rather than pull in a second markdown renderer, we reuse the real
//! editor widget in read-only mode: a tool result's note body / patch
//! text renders with the exact same heading / code / emphasis styling
//! the user sees in the buffer panel. Card content is *static* (never
//! edited), so we skip the live-preview decoration-rebuild hook the
//! buffer panel needs and build the markdown decoration layer once, at
//! construction, then only rebuild when the source text changes.
//!
//! Each rendered field owns a persistent [`Preview`] (editor + view +
//! its source hash) kept in a [`Cache`] on the `ChatRegistry`,
//! keyed by `session:turn:field`. Persistence is what lets us read back
//! the measured content height across frames so the embed sizes itself
//! to its content (capped, with the editor's own scroll past the cap)
//! instead of reserving a fixed slab.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use eframe::egui;
use editor_core::state::Editor as EditorState;
use editor_core::theme::light_default;
use editor_egui::widget::Widget as EditorWidget;
use editor_md::styling::markdown_decorations;
use editor_view::viewport::ViewState;

/// One cached read-only editor instance backing a single tool-card
/// content field. Rebuilt only when `src_hash` no longer matches the
/// field text (the agent re-running a tool, a streamed result landing).
pub struct Preview {
    editor: EditorState,
    view: ViewState,
    src_hash: u64,
}

impl Preview {
    fn build(text: &str) -> Self {
        let mut editor = EditorState::new(text);
        // Park the cursor at the very end of the document. The markdown
        // styling layer renders the line holding the cursor as raw source
        // (the editor's "reveal markers on the active line" rule); with a
        // fresh editor that line is line 0, so a leading `# Heading` would
        // show its `#`. The end of a note is typically a trailing newline /
        // plain line, so revealing it shows nothing — every heading / emphasis
        // marker stays hidden, giving a clean rendered-looking preview.
        editor.selection = editor_core::selection::Selection::single(editor.doc.len_bytes());
        let mut view = ViewState {
            // Read-only: command dispatch ignores text-modifying input,
            // and with no focus the widget paints no cursor.
            read_only: true,
            // No line numbers / fold column in a chat card — it's a
            // preview, not an editing surface.
            hide_gutter: true,
            ..ViewState::default()
        };
        // Soft-wrap long lines to the card width — without this the editor
        // lays lines out at their full length and the card content runs off
        // the right edge (it's a preview, not a horizontally-scrolled code
        // surface). The widget feeds the wrap map the live content width
        // each frame from the rect we allocate.
        view.wrap_map.set_enabled(true);
        // Build the markdown styling layer once. Card text never changes
        // under the user's cursor, so the per-keystroke rebuild hook the
        // buffer panel wires up is unnecessary here — the layer stays
        // valid for the life of this instance.
        let theme = light_default();
        view.decorations.clear();
        view.decorations.push(markdown_decorations(&editor, Some(&theme)));
        Self {
            editor,
            view,
            src_hash: hash_text(text),
        }
    }
}

/// Persistent per-field editor instances, keyed by `session:turn:field`.
/// Lives on the `ChatRegistry` so instances (and their measured heights)
/// survive across frames. Entries are small and bounded by the number of
/// content fields the user has scrolled into view.
pub type Cache = HashMap<String, Preview>;

/// Render `text` as read-only styled markdown into `ui`, reserving a
/// content-fit height capped at `max_height` (the editor scrolls
/// internally past the cap). `id` must be stable across frames and
/// unique per field so the cached instance — and its measured height —
/// is reused rather than rebuilt every frame.
pub fn render(
    ui: &mut egui::Ui,
    id: &str,
    text: &str,
    max_height: f32,
    cache: &mut Cache,
) {
    let hash = hash_text(text);
    let entry = cache
        .entry(id.to_string())
        .or_insert_with(|| Preview::build(text));
    // Source changed out from under a reused key (streamed result grew,
    // tool re-ran): rebuild against the new text.
    if entry.src_hash != hash {
        *entry = Preview::build(text);
    }

    // `total_height` is populated by the widget's measure pass, so it's
    // 0.0 until the first frame a freshly-built instance is shown. On that
    // first frame estimate from the line count (close enough that the
    // settle to the true measured height the next frame is barely visible)
    // and request a repaint so it settles immediately. A collapsed-then-
    // re-expanded card keeps its measured height, so this estimate only
    // ever runs on the very first expand — no blink on subsequent ones.
    let intrinsic = entry.view.height_map.total_height();
    let first_frame = intrinsic <= 0.0;
    let height = if first_frame {
        let lines = text.lines().count().max(1) as f32;
        (lines * entry.view.line_height + 12.0).min(max_height)
    } else {
        intrinsic.min(max_height)
    };

    let width = ui.available_width();
    // Wrap in a stable id scope so multiple embedded editors don't
    // collide on egui's source-location-derived widget ids.
    ui.push_id(id, |ui| {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        EditorWidget::new(&mut entry.editor, &mut entry.view).show(&mut child);
    });
    if first_frame {
        ui.ctx().request_repaint();
    }
}

fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}
