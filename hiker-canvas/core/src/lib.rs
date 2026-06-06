//! Egui-agnostic core of the JSON Canvas (jsoncanvas.org 1.0) editor.
//!
//! This crate is the source of truth every higher layer reads, mirroring the
//! `hiker-graph` / `graph-widgets` split: an egui adapter and the app panel
//! build on top, and this crate never depends on egui, the app, or op-log.
//!
//! It provides four things:
//!
//! - [`model`] — the serde JSON Canvas 1.0 schema ([`model::Canvas`],
//!   [`model::Node`], [`model::Edge`], [`model::NodeKind`]) matching the
//!   on-the-wire camelCase format with a `type` discriminator, and tolerant of
//!   unknown fields (they round-trip untouched).
//! - [`color`] — the [`color::Color`] enum (preset slot or hex literal) with
//!   bare-string serde.
//! - [`geometry`] — egui-free [`geometry::Point`] / [`geometry::Rect`], node
//!   bounds, z-ordered hit testing, content bounds, and edge anchors.
//! - [`ops`] — pure, invertible [`ops::EditOp`] verbs for a future undo stack.
//!
//! Deterministic, tab-indented, idempotent serialization lives on the model as
//! [`model::Canvas::to_canonical_json`], so a single node edit is a localized
//! text diff rather than a whole-file rewrite.

pub mod color;
pub mod geometry;
pub mod model;
pub mod ops;

#[cfg(test)]
mod canonical_tests {
    use crate::model::Canvas;

    const SAMPLE: &str = r##"{
	"nodes": [
		{
			"id": "n1",
			"x": 0,
			"y": 0,
			"width": 200,
			"height": 120,
			"color": "4",
			"type": "text",
			"text": "# Hello\nbody",
			"customTool": "preserved"
		},
		{
			"id": "n2",
			"x": 400,
			"y": 80,
			"width": 240,
			"height": 160,
			"type": "file",
			"file": "notes/ref.md",
			"subpath": "#section"
		},
		{
			"id": "g1",
			"x": -40,
			"y": -40,
			"width": 700,
			"height": 320,
			"type": "group",
			"label": "Cluster",
			"backgroundStyle": "ratio"
		}
	],
	"edges": [
		{
			"id": "e1",
			"fromNode": "n1",
			"fromSide": "right",
			"toNode": "n2",
			"toSide": "left",
			"toEnd": "arrow",
			"label": "links to"
		}
	],
	"version": "1.0-extra"
}
"##;

    #[test]
    fn parse_serialize_parse_round_trips() {
        let canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let serialized = canvas.to_canonical_json();
        let reparsed = Canvas::from_json(&serialized).expect("reserialized parses");
        assert_eq!(canvas, reparsed, "round-trip changed the model");
    }

    #[test]
    fn unknown_fields_survive_round_trip() {
        let canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let serialized = canvas.to_canonical_json();
        assert!(serialized.contains("\"customTool\": \"preserved\""), "node-level unknown key dropped");
        assert!(serialized.contains("\"version\": \"1.0-extra\""), "top-level unknown key dropped");
    }

    #[test]
    fn canonical_json_is_idempotent() {
        let canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let once = canvas.to_canonical_json();
        let twice = Canvas::from_json(&once).expect("parses").to_canonical_json();
        assert_eq!(once, twice, "canonical serialization is not idempotent");
    }

    #[test]
    fn canonical_json_uses_tab_indentation() {
        let canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let out = canvas.to_canonical_json();
        assert!(out.contains("\n\t\"nodes\""), "expected tab-indented keys");
        assert!(!out.contains("\n  \"nodes\""), "must not use space indentation");
        assert!(out.ends_with('\n'), "expected a trailing newline");
    }

    #[test]
    fn moving_one_node_changes_only_a_localized_region() {
        let mut canvas = Canvas::from_json(SAMPLE).expect("sample parses");
        let before = canvas.to_canonical_json();
        // Move only n2.
        let n2 = canvas.nodes.iter_mut().find(|n| n.id == "n2").unwrap();
        n2.x += 25;
        n2.y += 10;
        let after = canvas.to_canonical_json();

        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = after.lines().collect();
        assert_eq!(before_lines.len(), after_lines.len(), "a move must not add/remove lines");

        let changed: Vec<usize> = before_lines
            .iter()
            .zip(&after_lines)
            .enumerate()
            .filter(|(_, (b, a))| b != a)
            .map(|(i, _)| i)
            .collect();

        // Exactly the two coordinate lines (x and y) of n2 changed, and they
        // are contiguous.
        assert_eq!(changed.len(), 2, "only x and y of one node should change, got {changed:?}");
        assert_eq!(changed[1] - changed[0], 1, "changed lines must be contiguous");
        for line in &changed {
            let text = before_lines[*line].trim_start();
            assert!(
                text.starts_with("\"x\"") || text.starts_with("\"y\""),
                "changed line was not an x/y coordinate: {text}"
            );
        }
    }
}
