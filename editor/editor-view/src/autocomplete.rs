//! Autocomplete framework (SPEC §9.6, IMPLEMENTATION §16.5.3).
//!
//! Defines the pluggable [`CompletionSource`] trait, the [`CompletionItem`]
//! the popup displays, and the [`CompletionState`] that the view tracks while
//! the popup is open. The actual popup painting lives in the egui backend
//! (`editor-egui::completion`); input handling lives in `command.rs`.

use std::ops::Range;

use editor_core::state::Editor as EditorState;
use smol_str::SmolStr;

/// A single completion candidate produced by a [`CompletionSource`].
#[derive(Clone, Debug)]
pub struct CompletionItem {
    /// Text shown in the popup list (the "label").
    pub label: SmolStr,
    /// Optional secondary text shown next to the label (signature, type, etc.).
    pub detail: Option<SmolStr>,
    /// Text to insert when the user commits this item.
    pub insert: SmolStr,
    /// Optional explicit replace range. If `None`, the source asks the host
    /// to replace the trailing word at the cursor.
    pub replace_range: Option<Range<usize>>,
    /// Hint for icon / sort ordering.
    pub kind: CompletionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    Snippet,
    Variable,
    Function,
    Keyword,
    Wikilink,
    Text,
}

/// A pluggable source of completion candidates. Sources are stored on the
/// [`ViewState`] as `Arc<dyn CompletionSource>` and queried each time a
/// trigger character is typed (or the user requests completion explicitly).
pub trait CompletionSource: Send + Sync {
    /// Characters that should auto-open the popup when typed. The default
    /// empty slice means the source is only invoked via explicit
    /// (Ctrl-Space) triggering.
    fn triggers(&self) -> &[char] {
        &[]
    }
    /// Return all candidates this source produces for `state` with the
    /// caret at byte offset `pos`. Sources should filter against the
    /// current query themselves; the caller does not pre-filter.
    fn matches(&self, state: &EditorState, pos: usize) -> Vec<CompletionItem>;

    /// Cheap predicate: would this source produce completions at `pos`
    /// even though no trigger char was just typed? Lets the driver
    /// re-open the popup when the caret is edited back into an existing
    /// context — e.g. typing inside an already-closed `[[wikilink]]`.
    /// Must be cheap (a local scan); it runs on every non-trigger
    /// keystroke. Default `false` so most sources only open on their
    /// trigger char. [bug-wikilink-edit-reopens-popup]
    fn reopens_in_context(&self, _state: &EditorState, _pos: usize) -> bool {
        false
    }
}

/// Per-frame state of the autocomplete popup. Inactive by default.
#[derive(Clone, Debug, Default)]
pub struct CompletionState {
    pub active: bool,
    pub items: Vec<CompletionItem>,
    /// Index of the highlighted item in `items`.
    pub selected: usize,
    /// Byte offset where the popup was opened (used to anchor the popup and
    /// to compute the live query).
    pub anchor_byte: usize,
    /// Current query string (chars typed after `anchor_byte`).
    pub query: String,
}

impl CompletionState {
    pub fn close(&mut self) {
        self.active = false;
        self.items.clear();
        self.selected = 0;
        self.query.clear();
    }

    pub fn open(&mut self, anchor_byte: usize, items: Vec<CompletionItem>) {
        self.active = !items.is_empty();
        self.items = items;
        self.selected = 0;
        self.anchor_byte = anchor_byte;
        self.query.clear();
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let n = self.items.len() as isize;
        let mut s = self.selected as isize + delta;
        s = s.rem_euclid(n);
        self.selected = s as usize;
    }

    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }
}
