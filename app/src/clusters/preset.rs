//! Cluster-tree presets: reusable tree-creation params surfaced in the Clusters
//! `+` dropdown. The primary `+` starts a new tree with default params; the
//! caret lists presets that prefill the clustering review tab. A handful of
//! built-in presets are virtual (in-code); user presets are ordinary vault
//! notes carrying `hiker.kind: cluster-preset` frontmatter, discovered through
//! the store's frontmatter index (`query_notes`) — so a note the user *typed*
//! or *imported* with that frontmatter is a preset exactly like one hiker saved.
//! Nothing about presets lives under `.hiker/` (that holds only regenerable
//! data); the notes are the source of truth. status: cluster-preset

use serde::{Deserialize, Serialize};

use hiker_core::errors::HikerError;
use hiker_core::indexer::{IndexJob, IndexJobTx};
use hiker_core::store::dto::{MetaFilter, NoteQuery};
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_core::watcher::Watcher;

use crate::clusters::panel::{ReviewAlgorithm, ReviewConfig};

/// Frontmatter `hiker.kind` value that marks a note as a cluster-tree preset.
pub const KIND: &str = "cluster-preset";

/// The tree-creation params a preset stores — a `ReviewConfig` subset. The
/// per-tree `tree_name` / `purpose` aren't part of a reusable preset.
/// status: cluster-preset
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Params {
    pub name: String,
    #[serde(default)]
    pub algorithm: ReviewAlgorithm,
    #[serde(default)]
    pub source_types: String,
    #[serde(default)]
    pub name_with_llm_after_confirm: bool,
}

/// One preset entry shown in the dropdown. Built-ins and user presets render
/// the same way; `load` orders built-ins first.
#[derive(Clone, Debug)]
pub struct Entry {
    pub params: Params,
}

impl Entry {
    /// The `ReviewConfig` to open the review tab with for this preset.
    pub fn config(&self) -> ReviewConfig {
        ReviewConfig {
            algorithm: self.params.algorithm,
            source_types: self.params.source_types.clone(),
            name_with_llm_after_confirm: self.params.name_with_llm_after_confirm,
            ..ReviewConfig::default()
        }
    }
}

/// The in-code default presets, always shown first in the dropdown.
/// status: cluster-preset-defaults
pub fn builtins() -> Vec<Entry> {
    let mk = |name: &str, algorithm: ReviewAlgorithm| Entry {
        params: Params {
            name: name.to_string(),
            algorithm,
            source_types: String::new(),
            name_with_llm_after_confirm: false,
        },
    };
    vec![
        mk("Semantic \u{2014} Leiden", ReviewAlgorithm::Leiden),
        mk("Semantic \u{2014} HDBSCAN", ReviewAlgorithm::Hdbscan),
        mk("From folders", ReviewAlgorithm::FromFolders),
    ]
}

/// Built-in defaults, then every vault note carrying `hiker.kind:
/// cluster-preset` (found through the store's frontmatter index and read for its
/// params), sorted by name. status: cluster-preset
pub fn load(store: &Store, vault: &Vault) -> Vec<Entry> {
    let mut out = builtins();
    let query = NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "hiker.kind".to_string(),
            value: KIND.to_string(),
        }],
        ..Default::default()
    };
    if let Ok(rows) = store.query_notes(&query) {
        let mut user: Vec<Entry> = rows
            .iter()
            .filter_map(|row| vault.read_file(&row.path).ok())
            .filter_map(|text| parse_preset(&text))
            .map(|params| Entry { params })
            .collect();
        user.sort_by(|a, b| a.params.name.to_lowercase().cmp(&b.params.name.to_lowercase()));
        out.extend(user);
    }
    out
}

/// Save `params` as an ordinary vault note (written + indexed so the frontmatter
/// query picks it up). Default location `cluster-presets/<slug>.md`; the user
/// can move it anywhere — discovery is by frontmatter, not path. A same-slug
/// note is overwritten in place. Returns the vault-relative path.
/// status: cluster-preset-save
pub async fn save(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    params: &Params,
) -> Result<String, HikerError> {
    let rel = format!("cluster-presets/{}.md", slugify(&params.name));
    watcher.suppress(rel.clone());
    vault.write_file(&rel, &render_preset(params))?;
    // Re-suppress close to when notify surfaces the write, then index explicitly
    // (the watcher events were suppressed) so the preset is queryable at once.
    watcher.suppress(rel.clone());
    let _ = jobs
        .send(IndexJob::Upsert { rel_path: rel.clone(), force: false })
        .await;
    Ok(rel)
}

#[derive(Serialize)]
struct PresetFile<'a> {
    hiker: HikerMeta,
    cluster_preset: &'a Params,
}

#[derive(Serialize)]
struct HikerMeta {
    kind: &'static str,
}

#[derive(Deserialize)]
struct PresetFileOwned {
    #[serde(default)]
    hiker: Option<HikerMetaOwned>,
    cluster_preset: Option<Params>,
}

#[derive(Deserialize)]
struct HikerMetaOwned {
    #[serde(default)]
    kind: String,
}

/// Parse a preset note's frontmatter into `Params`; `None` if it isn't a
/// `cluster-preset` note.
fn parse_preset(text: &str) -> Option<Params> {
    let fm = hiker_core::frontmatter::split(text).frontmatter?;
    let parsed: PresetFileOwned = serde_yml::from_value(fm).ok()?;
    if parsed.hiker.map(|h| h.kind).as_deref() != Some("cluster-preset") {
        return None;
    }
    parsed.cluster_preset
}

fn render_preset(params: &Params) -> String {
    let file = PresetFile { hiker: HikerMeta { kind: "cluster-preset" }, cluster_preset: params };
    let body = format!("# {}\n\nCluster-tree preset.\n", params.name);
    serde_yml::to_value(&file)
        .ok()
        .and_then(|yaml| hiker_core::frontmatter::assemble(&yaml, &body).ok())
        .unwrap_or(body)
}

/// Lowercase, dash-separated filename slug from a preset name.
fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "preset".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trips_through_a_note() {
        let p = Params {
            name: "My Preset".to_string(),
            algorithm: ReviewAlgorithm::Leiden,
            source_types: "md,txt".to_string(),
            name_with_llm_after_confirm: true,
        };
        let text = render_preset(&p);
        assert!(text.starts_with("---\n"), "has frontmatter: {text}");
        let back = parse_preset(&text).expect("parses back");
        assert_eq!(back.name, "My Preset");
        assert_eq!(back.algorithm, ReviewAlgorithm::Leiden);
        assert_eq!(back.source_types, "md,txt");
        assert!(back.name_with_llm_after_confirm);
    }

    #[test]
    fn non_preset_note_is_ignored() {
        assert!(parse_preset("---\ntitle: hi\n---\nbody\n").is_none());
        assert!(parse_preset("just text, no frontmatter").is_none());
    }

    #[test]
    fn slugify_is_filesystem_safe() {
        assert_eq!(slugify("Semantic \u{2014} Leiden"), "semantic-leiden");
        assert_eq!(slugify("  "), "preset");
    }
}
