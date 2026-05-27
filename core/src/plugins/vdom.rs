//! The declarative VDOM a plugin returns for its UI. The plugin never draws;
//! it emits this serialized tree of bounded primitives and the host renders it
//! natively (egui in `app/`). Keeps the trust boundary clean — no markup, no
//! arbitrary styling — and stable across UI tech. See `plugins.md`.
//
// status: plugin-vdom

use serde::{Deserialize, Serialize};

/// Bounded text styling — palette/role driven, never arbitrary CSS.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextStyle {
    #[default]
    Normal,
    Heading,
    Muted,
    Strong,
    Code,
}

/// One row of a `List`, keyed by a plugin-defined stable `id` (echoed back in
/// `on_ui_event`), with one cell node per column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    #[serde(default)]
    pub cells: Vec<Node>,
}

/// A node in the plugin UI tree. The fixed, conservative primitive set from
/// `plugins.md`; extend deliberately. Tagged by `type` so the wire form reads
/// `{ "type": "text", "value": "...", "style": "heading" }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    /// Vertical stack of children.
    Vstack {
        #[serde(default)]
        children: Vec<Node>,
    },
    /// Horizontal stack of children.
    Hstack {
        #[serde(default)]
        children: Vec<Node>,
    },
    /// A run of styled text.
    Text {
        #[serde(default)]
        value: String,
        #[serde(default)]
        style: TextStyle,
    },
    /// A single-line text input. `id` is echoed in `on_ui_event` on change.
    TextInput {
        id: String,
        #[serde(default)]
        value: String,
        #[serde(default)]
        placeholder: String,
    },
    /// A clickable button; click fires `on_ui_event` with this `id`.
    Button { id: String, label: String },
    /// A clickable note reference. The host resolves `id` and opens the note —
    /// the plugin needs no permission to navigate.
    NoteLink {
        id: String,
        #[serde(default)]
        label: String,
    },
    /// A table: a header from `columns` and `rows` of cell nodes.
    List {
        #[serde(default)]
        columns: Vec<String>,
        #[serde(default)]
        rows: Vec<Row>,
    },
    /// A horizontal rule.
    Divider,
    /// Flexible empty space.
    Spacer,
}
