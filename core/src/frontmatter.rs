//! YAML frontmatter parsing + merge. Used by the MCP write tools to stamp
//! `hiker.author: agent-authored` and merge agent-provided fields into the
//! existing frontmatter without clobbering user content.
//!
//! The chunker has its own `strip_frontmatter` (byte-offset focused, returns
//! a slice). This module is the *mutation* counterpart — parses the YAML to
//! a `serde_yml::Value`, merges, and serializes back.
//
// status: mcp-tool-set-frontmatter

use serde_yml::Value as YamlValue;

const DELIMITER: &str = "---";

/// Outcome of splitting a source file into its frontmatter (if any) and body.
pub struct Split<'a> {
    /// Parsed frontmatter as a YAML mapping, if a well-formed block was found.
    /// `None` when the file has no frontmatter.
    pub frontmatter: Option<YamlValue>,
    /// Body slice of the source — never includes the frontmatter delimiters.
    pub body: &'a str,
}

/// Split `source` into (frontmatter, body). A well-formed frontmatter block
/// is `---\n...\n---\n` at the very start of the file. Anything else returns
/// `frontmatter: None` and the entire input as body.
pub fn split(source: &str) -> Split<'_> {
    if !source.starts_with("---\n") && !source.starts_with("---\r\n") {
        return Split { frontmatter: None, body: source };
    }
    let after_open = if source.starts_with("---\r\n") { 5 } else { 4 };
    let rest = &source[after_open..];

    // Find a closing `---` line.
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find(DELIMITER) {
        let abs = search_from + idx;
        let at_line_start = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        if !at_line_start {
            search_from = abs + 1;
            continue;
        }
        let after = abs + 3;
        let valid_end = after >= rest.len()
            || rest.as_bytes()[after] == b'\n'
            || (rest.as_bytes()[after] == b'\r'
                && rest.len() > after + 1
                && rest.as_bytes()[after + 1] == b'\n');
        if valid_end {
            // Content between the opening `---\n` and `abs` is YAML.
            let yaml_text = &rest[..abs];
            let mut body_start = after;
            if body_start < rest.len() && rest.as_bytes()[body_start] == b'\r' {
                body_start += 1;
            }
            if body_start < rest.len() && rest.as_bytes()[body_start] == b'\n' {
                body_start += 1;
            }
            let body = &source[after_open + body_start..];
            let parsed: Option<YamlValue> = if yaml_text.trim().is_empty() {
                Some(YamlValue::Mapping(Default::default()))
            } else {
                serde_yml::from_str(yaml_text).ok()
            };
            return Split {
                frontmatter: parsed,
                body,
            };
        }
        search_from = abs + 1;
    }
    // Unterminated frontmatter — treat as plain body so we don't clobber it.
    Split { frontmatter: None, body: source }
}

/// Deep-merge `patch` (a JSON object) into `base` (a YAML value). Tables
/// recurse; arrays and scalars replace. Mirrors the deep-merge in
/// `core::config`.
pub fn merge_json_into_yaml(base: &mut YamlValue, patch: serde_json::Value) {
    let patch_yaml = json_to_yaml(patch);
    deep_merge_yaml(base, patch_yaml);
}

fn deep_merge_yaml(base: &mut YamlValue, over: YamlValue) {
    match (base, over) {
        (YamlValue::Mapping(b), YamlValue::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge_yaml(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => {
            *slot = other;
        }
    }
}

fn json_to_yaml(v: serde_json::Value) -> YamlValue {
    use serde_json::Value as J;
    match v {
        J::Null => YamlValue::Null,
        J::Bool(b) => YamlValue::Bool(b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                YamlValue::Number(serde_yml::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                YamlValue::Number(serde_yml::Number::from(f))
            } else {
                YamlValue::Null
            }
        }
        J::String(s) => YamlValue::String(s),
        J::Array(arr) => YamlValue::Sequence(arr.into_iter().map(json_to_yaml).collect()),
        J::Object(obj) => {
            let mut m = serde_yml::Mapping::new();
            for (k, v) in obj {
                m.insert(YamlValue::String(k), json_to_yaml(v));
            }
            YamlValue::Mapping(m)
        }
    }
}

/// Re-assemble a file from a (possibly empty) frontmatter mapping + body.
/// Empty mapping serializes to no frontmatter at all so we don't add a
/// useless `---\n---\n` block when the patch left the map empty.
pub fn assemble(frontmatter: &YamlValue, body: &str) -> Result<String, Error> {
    let is_empty = match frontmatter {
        YamlValue::Mapping(m) => m.is_empty(),
        YamlValue::Null => true,
        _ => false,
    };
    if is_empty {
        return Ok(body.to_string());
    }
    let yaml = serde_yml::to_string(frontmatter)
        .map_err(|e| Error::Serialize(e.to_string()))?;
    let yaml = yaml.trim_end_matches('\n');
    Ok(format!("---\n{yaml}\n---\n{body}"))
}

/// Apply a JSON patch to a source file's frontmatter. The patch is merged
/// over the existing frontmatter (recursing into nested maps), and the
/// `hiker.author = "agent-authored"` stamp is set unconditionally per spec.
/// Returns the new file content.
pub fn merge_agent_patch(
    source: &str,
    patch: serde_json::Value,
) -> Result<String, Error> {
    let split_view = split(source);
    let mut fm = match split_view.frontmatter {
        Some(v) => v,
        None => YamlValue::Mapping(Default::default()),
    };
    if !matches!(fm, YamlValue::Mapping(_)) {
        // Non-map frontmatter (rare but possible — e.g. just a scalar).
        // Replace with a fresh map so merge proceeds.
        fm = YamlValue::Mapping(Default::default());
    }

    if let serde_json::Value::Object(_) = patch {
        merge_json_into_yaml(&mut fm, patch);
    } else if !matches!(patch, serde_json::Value::Null) {
        return Err(Error::PatchNotMap);
    }

    // Stamp hiker.author. Adapter is responsible for any provenance fields.
    if let YamlValue::Mapping(map) = &mut fm {
        let key = YamlValue::String("hiker".into());
        let entry = map.entry(key).or_insert_with(|| YamlValue::Mapping(Default::default()));
        if let YamlValue::Mapping(hm) = entry {
            hm.insert(
                YamlValue::String("author".into()),
                YamlValue::String("agent-authored".into()),
            );
        }
    }

    assemble(&fm, split_view.body)
}

/// One flattened frontmatter field destined for the metadata index
/// (`store-note-metadata-index`). `key` is a dotted path (`hiker.author`);
/// list elements share a key across entries. `num` is the numeric mirror
/// for YAML numbers / bools, `None` for strings.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatField {
    pub key: String,
    pub value: String,
    pub num: Option<f64>,
}

/// Caps guarding the index against pathological frontmatter.
const MAX_FLAT_ENTRIES: usize = 256;
const MAX_KEY_LEN: usize = 256;
const MAX_VALUE_LEN: usize = 1024;

/// Flatten a parsed frontmatter mapping into index entries. Nested maps
/// recurse with dotted keys (`hiker.author`); sequences emit one entry per
/// scalar element under the list's key (`tags: [a, b]` → two entries);
/// null values and non-scalar list elements are skipped. Bounded by
/// `MAX_FLAT_ENTRIES` / `MAX_KEY_LEN` / `MAX_VALUE_LEN`.
///
/// status: store-note-metadata-index
pub fn flatten(frontmatter: &YamlValue) -> Vec<FlatField> {
    let mut out = Vec::new();
    if let YamlValue::Mapping(map) = frontmatter {
        for (k, v) in map {
            if let Some(key) = k.as_str() {
                flatten_into(key, v, &mut out);
            }
            if out.len() >= MAX_FLAT_ENTRIES {
                break;
            }
        }
    }
    out.truncate(MAX_FLAT_ENTRIES);
    out
}

fn flatten_into(key: &str, value: &YamlValue, out: &mut Vec<FlatField>) {
    if out.len() >= MAX_FLAT_ENTRIES || key.len() > MAX_KEY_LEN {
        return;
    }
    match value {
        YamlValue::Mapping(map) => {
            for (k, v) in map {
                if let Some(child) = k.as_str() {
                    flatten_into(&format!("{key}.{child}"), v, out);
                }
                if out.len() >= MAX_FLAT_ENTRIES {
                    break;
                }
            }
        }
        YamlValue::Sequence(items) => {
            for item in items {
                if let Some(f) = scalar_field(key, item) {
                    out.push(f);
                }
                if out.len() >= MAX_FLAT_ENTRIES {
                    break;
                }
            }
        }
        _ => {
            if let Some(f) = scalar_field(key, value) {
                out.push(f);
            }
        }
    }
}

/// A single scalar YAML value as a `FlatField`, or `None` for null / nested
/// / over-long values.
fn scalar_field(key: &str, value: &YamlValue) -> Option<FlatField> {
    let (value, num) = match value {
        YamlValue::String(s) => (s.clone(), None),
        YamlValue::Bool(b) => ((*b).to_string(), Some(if *b { 1.0 } else { 0.0 })),
        YamlValue::Number(n) => (n.to_string(), n.as_f64()),
        // Null, and nested map/seq that slipped through, are not indexed.
        _ => return None,
    };
    if value.len() > MAX_VALUE_LEN {
        return None;
    }
    Some(FlatField {
        key: key.to_string(),
        value,
        num,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("frontmatter patch must be a JSON object")]
    PatchNotMap,
    #[error("serialize frontmatter: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_no_frontmatter() {
        let s = "# Heading\n\nbody\n";
        let v = split(s);
        assert!(v.frontmatter.is_none());
        assert_eq!(v.body, s);
    }

    #[test]
    fn split_parses_frontmatter() {
        let s = "---\ntitle: hello\ntags: [a, b]\n---\n# H\n\nbody\n";
        let v = split(s);
        let fm = v.frontmatter.unwrap();
        assert_eq!(fm["title"].as_str().unwrap(), "hello");
        assert_eq!(v.body, "# H\n\nbody\n");
    }

    #[test]
    fn merge_creates_frontmatter_when_missing() {
        let src = "# Heading\n\nbody\n";
        let out = merge_agent_patch(src, serde_json::json!({"tags": ["new"]})).unwrap();
        assert!(out.starts_with("---\n"));
        assert!(out.contains("tags:"));
        assert!(out.contains("hiker:"));
        assert!(out.contains("author: agent-authored"));
        assert!(out.ends_with("# Heading\n\nbody\n"));
    }

    #[test]
    fn merge_preserves_existing_fields() {
        let src = "---\ntitle: keep\n---\nbody\n";
        let out = merge_agent_patch(src, serde_json::json!({"summary": "added"})).unwrap();
        assert!(out.contains("title: keep"));
        assert!(out.contains("summary: added"));
        assert!(out.contains("author: agent-authored"));
        assert!(out.ends_with("body\n"));
    }

    #[test]
    fn merge_deep_merges_hiker_namespace() {
        let src = "---\nhiker:\n  provenance: imported\n---\nbody\n";
        let out = merge_agent_patch(src, serde_json::json!({})).unwrap();
        // Existing hiker.provenance survives; hiker.author is stamped.
        assert!(out.contains("provenance: imported"));
        assert!(out.contains("author: agent-authored"));
    }

    #[test]
    fn empty_frontmatter_after_merge_skips_block() {
        // Patch that replaces with an empty map. Adapter never does this in
        // practice (we always stamp author), but assemble() must DTRT.
        let mut fm = YamlValue::Mapping(Default::default());
        let s = assemble(&fm, "body\n").unwrap();
        assert_eq!(s, "body\n");
        // Sanity: non-empty does include the block.
        if let YamlValue::Mapping(m) = &mut fm {
            m.insert(YamlValue::String("k".into()), YamlValue::String("v".into()));
        }
        let s = assemble(&fm, "body\n").unwrap();
        assert!(s.starts_with("---\n"));
    }

    #[test]
    fn flatten_scalars_lists_and_nested() {
        let src = "---\nstatus: active\npriority: 3\nstarred: true\ntags:\n  - project\n  - rust\nhiker:\n  author: user-authored\n---\nbody\n";
        let fm = split(src).frontmatter.unwrap();
        let fields = flatten(&fm);
        let get = |k: &str| -> Vec<FlatField> {
            fields.iter().filter(|f| f.key == k).cloned().collect()
        };
        assert_eq!(get("status")[0].value, "active");
        assert_eq!(get("status")[0].num, None);
        assert_eq!(get("priority")[0].value, "3");
        assert_eq!(get("priority")[0].num, Some(3.0));
        assert_eq!(get("starred")[0].num, Some(1.0));
        let tags: Vec<String> = get("tags").into_iter().map(|f| f.value).collect();
        assert_eq!(tags, vec!["project".to_string(), "rust".to_string()]);
        assert_eq!(get("hiker.author")[0].value, "user-authored");
    }

    #[test]
    fn flatten_skips_null_and_non_mapping() {
        assert!(flatten(&YamlValue::Null).is_empty());
        // A note key with a null value isn't indexed.
        let src = "---\nstatus:\ntags:\n  - a\n---\nbody\n";
        let fm = split(src).frontmatter.unwrap();
        let fields = flatten(&fm);
        assert!(fields.iter().all(|f| f.key != "status"));
        assert_eq!(fields.iter().filter(|f| f.key == "tags").count(), 1);
    }
}
