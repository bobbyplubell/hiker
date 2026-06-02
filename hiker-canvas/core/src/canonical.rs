//! Canonical serialization — the load-bearing feature. [`Canvas::to_canonical_json`]
//! emits deterministic, stable-key, pretty-printed JSON: node and edge arrays
//! stay in document order, struct field order is fixed by serde, and the
//! unknown-field capture maps are [`BTreeMap`](std::collections::BTreeMap)s so
//! their keys sort deterministically. Indentation is a TAB, matching the JSON
//! Canvas file convention. Determinism is what makes a single node move a
//! localized text diff rather than a whole-file rewrite, which is what lets
//! concurrent edits merge in the op-log. The output is idempotent: re-parsing
//! and re-serializing yields byte-identical text.
//
// status: canvas-canonical-json

use serde_json::ser::{PrettyFormatter, Serializer};

use crate::model::Canvas;

impl Canvas {
    /// Serialize the canvas to canonical JSON Canvas text: deterministic key
    /// order, document-order arrays, tab-indented pretty printing, with a
    /// trailing newline.
    ///
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
}
