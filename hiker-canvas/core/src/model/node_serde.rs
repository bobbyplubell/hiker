//! Hand-written serde for [`Node`](super::Node). Serde's derived
//! `#[serde(flatten)]` cannot combine an internally-tagged enum (the `type`
//! discriminator on [`NodeKind`](super::NodeKind)) with a catch-all flatten
//! map without emitting the `type` key twice on serialize and rejecting it as a
//! duplicate on deserialize. This module routes both through a single ordered
//! key/value pass so the wire format is exact and unknown keys round-trip.

use std::collections::BTreeMap;

use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::{Node, NodeKind};

/// Common keys the model owns directly; anything else is unknown and captured
/// into `extra`. `type` plus the per-kind keys are owned by [`NodeKind`].
const COMMON_KEYS: [&str; 6] = ["id", "x", "y", "width", "height", "color"];

impl Serialize for Node {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let kind_obj = kind_to_object::<S>(&self.kind)?;
        let len = COMMON_KEYS.len() + kind_obj.len() + self.extra.len();
        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("x", &self.x)?;
        map.serialize_entry("y", &self.y)?;
        map.serialize_entry("width", &self.width)?;
        map.serialize_entry("height", &self.height)?;
        if let Some(color) = &self.color {
            map.serialize_entry("color", color)?;
        }
        for (key, value) in &kind_obj {
            map.serialize_entry(key, value)?;
        }
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// Render a [`NodeKind`] to its `{ "type": ..., <fields> }` object so its
/// entries can be spliced into the node's flat key stream in order.
fn kind_to_object<S: Serializer>(kind: &NodeKind) -> Result<serde_json::Map<String, Value>, S::Error> {
    match serde_json::to_value(kind).map_err(serde::ser::Error::custom)? {
        Value::Object(obj) => Ok(obj),
        _ => Err(serde::ser::Error::custom("node kind did not serialize to an object")),
    }
}

impl<'de> Deserialize<'de> for Node {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(NodeVisitor)
    }
}

struct NodeVisitor;

impl<'de> Visitor<'de> for NodeVisitor {
    type Value = Node;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON Canvas node object")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut access: M) -> Result<Self::Value, M::Error> {
        let mut raw: BTreeMap<String, Value> = BTreeMap::new();
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            raw.insert(key, value);
        }
        node_from_map(raw)
    }
}

/// Split a flat key/value map into the typed [`Node`] fields, the reconstructed
/// [`NodeKind`], and the leftover unknown keys.
fn node_from_map<E: DeError>(mut raw: BTreeMap<String, Value>) -> Result<Node, E> {
    let id = take_string(&mut raw, "id")?;
    let x = take_i64(&mut raw, "x")?;
    let y = take_i64(&mut raw, "y")?;
    let width = take_i64(&mut raw, "width")?;
    let height = take_i64(&mut raw, "height")?;
    let color = match raw.remove("color") {
        Some(value) => Some(serde_json::from_value(value).map_err(E::custom)?),
        None => None,
    };
    let kind = take_kind(&mut raw)?;
    Ok(Node { id, x, y, width, height, color, kind, extra: raw })
}

/// Reconstruct the [`NodeKind`] from the `type` tag plus the kind-specific keys
/// still in the map, removing every key it consumes so they don't leak into
/// `extra`.
fn take_kind<E: DeError>(raw: &mut BTreeMap<String, Value>) -> Result<NodeKind, E> {
    let type_value = raw.get("type").cloned().ok_or_else(|| E::missing_field("type"))?;
    let kind_keys = kind_field_keys(&type_value)?;
    let mut kind_obj = serde_json::Map::new();
    kind_obj.insert("type".to_owned(), type_value);
    for &key in kind_keys {
        if let Some(value) = raw.remove(key) {
            kind_obj.insert(key.to_owned(), value);
        }
    }
    raw.remove("type");
    serde_json::from_value(Value::Object(kind_obj)).map_err(E::custom)
}

/// The per-kind field names owned by each node `type`.
fn kind_field_keys<E: DeError>(type_value: &Value) -> Result<&'static [&'static str], E> {
    match type_value.as_str() {
        Some("text") => Ok(&["text"]),
        Some("file") => Ok(&["file", "subpath"]),
        Some("link") => Ok(&["url"]),
        Some("group") => Ok(&["label", "background", "backgroundStyle"]),
        other => Err(E::custom(format!("unknown node type {other:?}"))),
    }
}

fn take_string<E: DeError>(raw: &mut BTreeMap<String, Value>, key: &'static str) -> Result<String, E> {
    match raw.remove(key) {
        Some(Value::String(s)) => Ok(s),
        Some(_) => Err(E::custom(format!("field {key:?} must be a string"))),
        None => Err(E::missing_field(key)),
    }
}

fn take_i64<E: DeError>(raw: &mut BTreeMap<String, Value>, key: &'static str) -> Result<i64, E> {
    match raw.remove(key) {
        Some(value) => value.as_i64().ok_or_else(|| E::custom(format!("field {key:?} must be an integer"))),
        None => Err(E::missing_field(key)),
    }
}
