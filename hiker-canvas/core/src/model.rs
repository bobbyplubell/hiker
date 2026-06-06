//! The JSON Canvas 1.0 data model as serde types — the egui-agnostic source of
//! truth every layer reads. Matches the on-the-wire format exactly: a top-level
//! `{ "nodes": [...], "edges": [...] }`, camelCase keys, and a `type`
//! discriminator on each node. Unrecognized keys at the top level, on nodes,
//! and on edges are captured into [`BTreeMap`]s and round-trip untouched so a
//! canvas authored by another tool isn't lossily rewritten on the first edit.
//
// status: canvas-spec-model
// status: canvas-node-types
// status: canvas-edge-model

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::ser::{PrettyFormatter, Serializer};
use serde_json::Value;

use crate::color::Color;

/// The whole canvas document: ordered node and edge arrays plus any
/// unrecognized top-level keys.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Canvas {
    /// Nodes in document order. Array index is z-order: a later node paints on
    /// top of an earlier one.
    #[serde(default)]
    pub nodes: Vec<Node>,
    /// Edges in document order.
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Top-level keys this model does not recognize, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A single node. The common geometry fields live here; the per-kind fields
/// flatten in from [`NodeKind`] (which carries the `type` discriminator), and
/// unrecognized keys land in `extra`.
///
/// Serde is hand-written for this type (see [`node_serde`]) because serde's
/// derived `#[serde(flatten)]` cannot combine an internally-tagged enum with a
/// catch-all flatten map without double-emitting the `type` key.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Unique within the canvas.
    pub id: String,
    /// Top-left corner X in the infinite integer coordinate space.
    pub x: i64,
    /// Top-left corner Y.
    pub y: i64,
    /// Node width.
    pub width: i64,
    /// Node height.
    pub height: i64,
    /// Optional color (preset slot or hex).
    pub color: Option<Color>,
    /// The `type` tag and its per-kind extra fields.
    pub kind: NodeKind,
    /// Node keys this model does not recognize, preserved verbatim.
    pub extra: BTreeMap<String, Value>,
}

/// The node `type` discriminator and the fields specific to each kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NodeKind {
    /// A markdown text node.
    Text {
        /// Markdown body.
        text: String,
    },
    /// A node embedding a vault file.
    #[serde(rename_all = "camelCase")]
    File {
        /// Vault-relative path to the referenced file.
        file: String,
        /// Optional `#heading` or `#^block` anchor within the file.
        #[serde(skip_serializing_if = "Option::is_none")]
        subpath: Option<String>,
    },
    /// A node rendering a URL.
    Link {
        /// The target URL.
        url: String,
    },
    /// A group node that frames its geometric members.
    #[serde(rename_all = "camelCase")]
    Group {
        /// Optional group label.
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// Optional background image path.
        #[serde(skip_serializing_if = "Option::is_none")]
        background: Option<String>,
        /// Optional background rendering style.
        #[serde(skip_serializing_if = "Option::is_none")]
        background_style: Option<BackgroundStyle>,
    },
}

/// How a group node's background image is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundStyle {
    /// Fill the group, cropping to cover.
    Cover,
    /// Scale preserving aspect ratio.
    Ratio,
    /// Tile the image.
    Repeat,
}

/// A connector between two nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    /// Unique within the canvas.
    pub id: String,
    /// Source node id.
    pub from_node: String,
    /// Optional source side anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_side: Option<Side>,
    /// Optional source endpoint cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_end: Option<EndCap>,
    /// Destination node id.
    pub to_node: String,
    /// Optional destination side anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_side: Option<Side>,
    /// Optional destination endpoint cap (defaults to arrow when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_end: Option<EndCap>,
    /// Optional edge color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    /// Optional edge label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Edge keys this model does not recognize, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// A node side an edge can anchor to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Top edge.
    Top,
    /// Right edge.
    Right,
    /// Bottom edge.
    Bottom,
    /// Left edge.
    Left,
}

/// An edge endpoint cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndCap {
    /// No cap.
    None,
    /// An arrowhead.
    Arrow,
}

impl Canvas {
    /// Parse a JSON Canvas document from its on-the-wire text.
    ///
    /// # Errors
    /// Returns the underlying [`serde_json::Error`] if the text is not valid
    /// JSON Canvas.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Serialize the canvas to canonical JSON Canvas text: deterministic key
    /// order, document-order arrays, tab-indented pretty printing, with a
    /// trailing newline.
    ///
    /// This is the load-bearing inverse of [`from_json`](Self::from_json): node
    /// and edge arrays stay in document order, struct field order is fixed by
    /// serde, and the unknown-field capture maps are
    /// [`BTreeMap`]s so their keys sort deterministically. Determinism is what
    /// makes a single node move a localized text diff rather than a whole-file
    /// rewrite, which is what lets concurrent edits merge in the op-log.
    /// Idempotent — `to_canonical_json` of a parsed `to_canonical_json` output
    /// is byte-identical.
    #[must_use]
    pub fn to_canonical_json(&self) -> String {
        let mut buf = Vec::new();
        let formatter = PrettyFormatter::with_indent(b"\t");
        let mut serializer = Serializer::with_formatter(&mut buf, formatter);
        serde::Serialize::serialize(self, &mut serializer)
            .expect("Canvas serialization to an in-memory buffer cannot fail");
        let mut out = String::from_utf8(buf)
            .expect("serde_json always emits valid UTF-8");
        out.push('\n');
        out
    }

    /// Rewrite every [`NodeKind::File`] node whose `file` path equals `from`
    /// (a vault-relative path) to `to`, preserving each node's `subpath`
    /// anchor. Returns `true` when at least one node changed. Pure and
    /// egui-free so the host can run it inside a rename transaction without
    /// touching any other layer.
    ///
    /// status: canvas-file-ref-rewrite
    pub fn rewrite_file_refs(&mut self, from: &str, to: &str) -> bool {
        let mut changed = false;
        for node in &mut self.nodes {
            if let NodeKind::File { file, .. } = &mut node.kind
                && file == from
            {
                *file = to.to_owned();
                changed = true;
            }
        }
        changed
    }
}

mod node_serde;

#[cfg(test)]
mod rewrite_tests {
    use crate::model::Canvas;

    const SAMPLE: &str = r##"{
	"nodes": [
		{
			"id": "n1",
			"x": 0,
			"y": 0,
			"width": 200,
			"height": 120,
			"type": "file",
			"file": "old/path.md",
			"subpath": "#section"
		},
		{
			"id": "n2",
			"x": 400,
			"y": 0,
			"width": 200,
			"height": 120,
			"type": "file",
			"file": "other/keep.md"
		},
		{
			"id": "n3",
			"x": 0,
			"y": 300,
			"width": 200,
			"height": 120,
			"type": "text",
			"text": "old/path.md"
		}
	],
	"edges": []
}
"##;

    #[test]
    fn rewrite_file_refs_touches_only_matching_file_nodes() {
        let mut canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let before = canvas.to_canonical_json();

        let changed = canvas.rewrite_file_refs("old/path.md", "new/path.md");
        assert!(changed, "a matching file node should report a change");

        let after = canvas.to_canonical_json();
        // The matching file node's `file` rewrote; its subpath survived.
        assert!(after.contains("\"file\": \"new/path.md\""), "matched file ref must rewrite");
        assert!(after.contains("\"subpath\": \"#section\""), "subpath must be preserved");
        // Non-matching file node and the text node (whose body merely happens
        // to contain the old path) are untouched.
        assert!(after.contains("\"file\": \"other/keep.md\""), "non-matching file ref must not change");
        assert!(after.contains("\"text\": \"old/path.md\""), "text-node body must not be rewritten");
        assert!(!after.contains("\"file\": \"old/path.md\""), "old file ref must be gone");

        // The diff is localized: only the single `file` line differs.
        let diff: Vec<(usize, &str, &str)> = before
            .lines()
            .zip(after.lines())
            .enumerate()
            .filter(|(_, (b, a))| b != a)
            .map(|(i, (b, a))| (i, b, a))
            .collect();
        assert_eq!(diff.len(), 1, "exactly one line should change, got {diff:?}");
        assert!(diff[0].1.trim_start().starts_with("\"file\""), "changed line must be a file ref");
    }

    #[test]
    fn rewrite_file_refs_reports_no_change_when_nothing_matches() {
        let mut canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let before = canvas.to_canonical_json();
        let changed = canvas.rewrite_file_refs("absent/note.md", "new/note.md");
        assert!(!changed, "no matching node means no change");
        assert_eq!(before, canvas.to_canonical_json(), "canvas must be byte-identical");
    }
}
