//! User and per-vault TOML settings. See `docs/settings.md`.
//!
//! Two TOML files (per-user at the platform config dir, per-vault at
//! `vault/.hiker/config.toml`) are deep-merged with vault winning, then
//! deserialized into a frozen `Config`. Missing files are auto-created with
//! the current defaults serialized in full so users have a self-documenting
//! file to edit. `set_setting` uses `toml_edit` to patch in place so users'
//! comments and key ordering survive in-app writes.
//
// status: settings-user-config-toml
// status: settings-vault-config-toml
// status: settings-load-once-at-startup
// status: settings-strict-load
// status: settings-defaults-in-code
// status: settings-auto-create-defaults
// status: settings-write-back
// status: settings-section-editor
// status: settings-section-indexing
// status: settings-section-vault
// status: settings-schema-version
// status: search-mode-state-persisted

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HikerError;

pub const SCHEMA_VERSION: u32 = 1;

/// Cap on the `vault.recent` list. Older entries past this point fall off
/// when a new vault open pushes onto the front.
pub const RECENT_VAULTS_CAP: usize = 10;

/// Push `root` to the front of `current`, dedupe by string equality, cap at
/// `RECENT_VAULTS_CAP` entries. Returns the new list. Pure policy — caller
/// is responsible for persisting it via `Config::set("vault.recent", ...)`
/// if needed.
///
/// Lives in `core::config` rather than at adapter level so any future
/// adapter (CLI / MCP) that opens a vault gets the same recent-list shape
/// without re-implementing the dedupe + cap.
pub fn push_recent_vault(current: &[String], root: &Path) -> Vec<String> {
    let display = root.to_string_lossy().into_owned();
    let mut out = Vec::with_capacity(current.len() + 1);
    out.push(display.clone());
    for entry in current {
        if entry != &display {
            out.push(entry.clone());
        }
        if out.len() >= RECENT_VAULTS_CAP {
            break;
        }
    }
    out
}

/// Top-level config struct loaded from the merged user+vault TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub editor: EditorConfig,
    #[serde(default)]
    pub indexing: IndexingConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub search: SearchConfig,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            editor: EditorConfig::default(),
            indexing: IndexingConfig::default(),
            vault: VaultConfig::default(),
            search: SearchConfig::default(),
        }
    }
}

/// `[search]` section. Holds discovery-panel state: which backends run by
/// default (mode toggles), and the per-section collapsed/expanded state
/// inside the panel. Vault-scoped via `settings-write-back`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchConfig {
    #[serde(default)]
    pub modes: SearchModesConfig,
    #[serde(default)]
    pub sections: SearchSectionsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchModesConfig {
    #[serde(default = "yes")]
    pub semantic: bool,
    #[serde(default = "yes")]
    pub lexical: bool,
}

impl Default for SearchModesConfig {
    fn default() -> Self {
        Self { semantic: true, lexical: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchSectionsConfig {
    #[serde(default = "yes")]
    pub results_expanded: bool,
    #[serde(default = "yes")]
    pub related_expanded: bool,
}

impl Default for SearchSectionsConfig {
    fn default() -> Self {
        Self { results_expanded: true, related_expanded: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorConfig {
    #[serde(default = "yes")]
    pub render_txt_as_markdown: bool,
    #[serde(default = "yes")]
    pub live_preview: bool,
    #[serde(default = "yes")]
    pub word_wrap: bool,
    #[serde(default = "yes")]
    pub show_line_numbers: bool,
    #[serde(default = "no")]
    pub show_whitespace: bool,
    #[serde(default = "no")]
    pub show_chunk_boundaries: bool,
    #[serde(default = "default_tab_size")]
    pub tab_size: u8,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            render_txt_as_markdown: true,
            live_preview: true,
            word_wrap: true,
            show_line_numbers: true,
            show_whitespace: false,
            show_chunk_boundaries: false,
            tab_size: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: u16,
    #[serde(default)]
    pub ignored_paths: Vec<String>,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            batch_size: default_batch_size(),
            ignored_paths: Vec::new(),
        }
    }
}

fn default_model() -> String {
    "bge-small-en-v1.5".to_string()
}

fn default_batch_size() -> u16 {
    64
}

fn default_tab_size() -> u8 {
    2
}

fn yes() -> bool {
    true
}

fn no() -> bool {
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    #[serde(default)]
    pub recent: Vec<String>,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default = "yes")]
    pub sidebar_open: bool,
    #[serde(default = "no")]
    pub related_open: bool,
    #[serde(default = "no")]
    pub trash_expanded: bool,
    #[serde(default)]
    pub tree: TreeConfig,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            recent: Vec::new(),
            default: None,
            sidebar_open: true,
            related_open: false,
            trash_expanded: false,
            tree: TreeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeConfig {
    #[serde(default)]
    pub sort_by: TreeSortBy,
}

impl Default for TreeConfig {
    fn default() -> Self {
        Self {
            sort_by: TreeSortBy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TreeSortBy {
    NameAsc,
    NameDesc,
    MtimeDesc,
    MtimeAsc,
}

impl Default for TreeSortBy {
    fn default() -> Self {
        TreeSortBy::NameAsc
    }
}

/// Whether a write-back targets the per-user TOML or the per-vault TOML.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsScope {
    User,
    Vault,
}

/// Resolved file paths for the two TOMLs. `user` is `None` when the
/// platform config dir can't be resolved (rare — sandboxed test envs).
#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub vault: PathBuf,
}

impl ConfigPaths {
    pub fn resolve(vault_root: &Path) -> Self {
        let user = directories::ProjectDirs::from("", "", "hiker")
            .map(|p| p.config_dir().join("config.toml"));
        let vault = vault_root.join(".hiker").join("config.toml");
        Self { user, vault }
    }
}

impl Config {
    /// Read only the per-user TOML and return its `vault.default` field if
    /// set. Used at app bootstrap (before any vault is open) to decide
    /// whether to auto-open a default vault. Returns `Ok(None)` if the
    /// platform config dir can't be resolved, the user TOML doesn't exist
    /// yet, or the field is unset. Errors only on real I/O / parse
    /// failures so a malformed TOML still aborts loudly.
    pub fn user_default_vault() -> Result<Option<String>, HikerError> {
        let user_path = match directories::ProjectDirs::from("", "", "hiker") {
            Some(p) => p.config_dir().join("config.toml"),
            None => return Ok(None),
        };
        if !user_path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&user_path).map_err(|e| {
            tracing::error!(file = %user_path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", user_path.display()))
        })?;
        let doc: toml::Value = toml::from_str(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(file = %user_path.display(), error = %e, "settings parse failed");
            HikerError::Config(format!("parse {}: {e}", user_path.display()))
        })?;
        Ok(doc
            .get("vault")
            .and_then(|v| v.get("default"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()))
    }

    /// Load and merge the user + vault TOMLs. Auto-creates either file with
    /// the current defaults if missing. Strict: any unknown key, type
    /// mismatch, or schema-version mismatch aborts with a clear error.
    pub fn load(vault_root: &Path) -> Result<Self, HikerError> {
        let paths = ConfigPaths::resolve(vault_root);

        // User file: best-effort. If we couldn't resolve the platform config
        // dir, treat it as empty rather than failing — vault TOML can still
        // carry everything the user needs.
        let user_doc = match paths.user.as_ref() {
            Some(p) => Some(read_or_create(p, &Self::default())?),
            None => None,
        };

        let vault_doc = read_or_create(&paths.vault, &Self::default())?;

        // Deep-merge user under vault (vault wins per-key). Tables recurse;
        // arrays and scalars replace.
        let mut merged: toml::Value = match user_doc {
            Some(u) => u,
            None => toml::Value::Table(toml::map::Map::new()),
        };
        deep_merge(&mut merged, vault_doc);

        // Schema-version check fires before deserialization so users get a
        // helpful "schema N, expected M" instead of an unknown-field error
        // from a future binary's keys.
        if let Some(toml::Value::Integer(v)) = merged.get("schema_version") {
            if *v as u32 != SCHEMA_VERSION {
                let user_disp = display_path(paths.user.as_deref());
                let vault_disp = paths.vault.display().to_string();
                tracing::error!(
                    user_file = %user_disp,
                    vault_file = %vault_disp,
                    found = *v,
                    expected = SCHEMA_VERSION,
                    "settings schema_version mismatch",
                );
                return Err(HikerError::Config(format!(
                    "settings schema_version {v}, this binary expects {SCHEMA_VERSION} (user={user_disp}, vault={vault_disp})"
                )));
            }
        }

        // Both files have already parsed cleanly via `read_or_create`; if
        // try_into fails here it's an unknown key or type mismatch from the
        // *merged* view, so we can't single out which file contributed it
        // without a per-file trial-deserialize. Surface both paths so the
        // user can grep.
        let cfg: Config = merged.try_into().map_err(|e: toml::de::Error| {
            let user_disp = display_path(paths.user.as_deref());
            let vault_disp = paths.vault.display().to_string();
            tracing::error!(
                user_file = %user_disp,
                vault_file = %vault_disp,
                error = %e,
                "settings strict-load rejected merged config",
            );
            HikerError::Config(format!(
                "invalid settings (user={user_disp}, vault={vault_disp}): {e}"
            ))
        })?;

        // Cross-field validation: model is the only supported value in v1
        // (the field exists for forward compatibility). batch_size must be
        // non-zero.
        if cfg.indexing.model != default_model() {
            tracing::error!(
                key = "indexing.model",
                value = %cfg.indexing.model,
                "unsupported settings value",
            );
            return Err(HikerError::Config(format!(
                "indexing.model = \"{}\" — only \"{}\" is supported in v1",
                cfg.indexing.model,
                default_model(),
            )));
        }
        if cfg.indexing.batch_size == 0 {
            tracing::error!(key = "indexing.batch_size", "value must be > 0");
            return Err(HikerError::Config(
                "indexing.batch_size must be > 0".to_string(),
            ));
        }

        Ok(cfg)
    }

    /// Write the new value through to the appropriate TOML on disk and
    /// return the freshly-loaded merged Config so the caller can swap its
    /// in-memory copy. The eligible-key set is closed: only the keys with a
    /// real-time UI control accept writes. Anything else returns
    /// `HikerError::Config`.
    pub fn set(
        scope: SettingsScope,
        key: &str,
        value: serde_json::Value,
        vault_root: &Path,
    ) -> Result<Self, HikerError> {
        let allowed = eligible_key(scope, key)?;
        validate_value(&allowed, &value)?;

        let paths = ConfigPaths::resolve(vault_root);
        let target = match scope {
            SettingsScope::User => paths
                .user
                .clone()
                .ok_or_else(|| HikerError::Config("no platform config dir available".into()))?,
            SettingsScope::Vault => paths.vault.clone(),
        };

        // Read-or-create-defaults so write-back works on a vault that's
        // never had its TOML touched. Use toml_edit so user comments and
        // key ordering survive the patch.
        let mut doc = read_or_create_doc(&target, &Self::default())?;
        apply_patch(&mut doc, key, &value);
        atomic_write(&target, doc.to_string().as_bytes())?;

        // Reload through the normal path so the returned Config reflects
        // the merged state across both files.
        Self::load(vault_root)
    }
}

/// Deep-merge `override_v` onto `base` in place. Tables recurse; arrays
/// and scalars replace.
fn deep_merge(base: &mut toml::Value, override_v: toml::Value) {
    use toml::Value;
    match (base, override_v) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
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

fn display_path(p: Option<&Path>) -> String {
    match p {
        Some(p) => p.display().to_string(),
        None => "<unset>".to_string(),
    }
}

/// Read the file as a `toml::Value`. If missing, write the defaults
/// serialized in full and return that.
fn read_or_create(path: &Path, defaults: &Config) -> Result<toml::Value, HikerError> {
    if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| {
            tracing::error!(file = %path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            // toml::de::Error::span() is private, but the Display impl
            // already includes line/col when known. Fields kept structured
            // per `obs-error-context`; the stringified message preserves
            // the parser's positional info.
            tracing::error!(
                file = %path.display(),
                error = %e,
                "settings parse failed",
            );
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    } else {
        write_defaults(path, defaults)?;
        Ok(toml_value_from_serde(defaults))
    }
}

/// Same as `read_or_create` but returns a `toml_edit::DocumentMut` for
/// in-place patching.
fn read_or_create_doc(
    path: &Path,
    defaults: &Config,
) -> Result<toml_edit::DocumentMut, HikerError> {
    if !path.exists() {
        write_defaults(path, defaults)?;
    }
    let raw = fs::read_to_string(path).map_err(|e| {
        HikerError::Config(format!("read {}: {e}", path.display()))
    })?;
    raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        HikerError::Config(format!("parse {}: {e}", path.display()))
    })
}

fn toml_value_from_serde(cfg: &Config) -> toml::Value {
    toml::Value::try_from(cfg).expect("Config serializes cleanly")
}

fn write_defaults(path: &Path, defaults: &Config) -> Result<(), HikerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            HikerError::Config(format!("mkdir {}: {e}", parent.display()))
        })?;
    }
    let header = format!(
        "# Hiker settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n# This file was auto-generated with the current defaults; edit freely.\n\n"
    );
    let body = toml::to_string_pretty(defaults).map_err(|e| {
        HikerError::Config(format!("serialize defaults: {e}"))
    })?;
    let bytes = format!("{header}{body}");
    atomic_write(path, bytes.as_bytes())
}

/// Atomic write: write to `<path>.tmp`, then rename. Avoids leaving a
/// half-written file if the process dies mid-write.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), HikerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            HikerError::Config(format!("mkdir {}: {e}", parent.display()))
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, bytes).map_err(|e| {
        HikerError::Config(format!("write {}: {e}", tmp.display()))
    })?;
    fs::rename(&tmp, path).map_err(|e| {
        HikerError::Config(format!("rename {} → {}: {e}", tmp.display(), path.display()))
    })?;
    Ok(())
}

/// One node of the eligible-key set: dotted path + expected JSON-side type.
#[derive(Debug, Clone, Copy)]
struct EligibleKey {
    /// Dotted path, e.g. `"editor.live_preview"`. The first component is
    /// the section, then leaf or sub-table.
    path: &'static str,
    ty: ValueType,
}

#[derive(Debug, Clone, Copy)]
enum ValueType {
    Bool,
    String,
    StringArray,
    /// `name_asc | name_desc | mtime_desc | mtime_asc`.
    TreeSortBy,
}

const ELIGIBLE_VAULT: &[EligibleKey] = &[
    EligibleKey { path: "editor.render_txt_as_markdown", ty: ValueType::Bool },
    EligibleKey { path: "editor.live_preview",           ty: ValueType::Bool },
    EligibleKey { path: "editor.word_wrap",              ty: ValueType::Bool },
    EligibleKey { path: "editor.show_line_numbers",      ty: ValueType::Bool },
    EligibleKey { path: "editor.show_whitespace",        ty: ValueType::Bool },
    EligibleKey { path: "editor.show_chunk_boundaries",  ty: ValueType::Bool },
    EligibleKey { path: "vault.sidebar_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.related_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.trash_expanded",          ty: ValueType::Bool },
    EligibleKey { path: "vault.tree.sort_by",            ty: ValueType::TreeSortBy },
    EligibleKey { path: "search.modes.semantic",         ty: ValueType::Bool },
    EligibleKey { path: "search.modes.lexical",          ty: ValueType::Bool },
    EligibleKey { path: "search.sections.results_expanded", ty: ValueType::Bool },
    EligibleKey { path: "search.sections.related_expanded", ty: ValueType::Bool },
];

const ELIGIBLE_USER: &[EligibleKey] = &[
    EligibleKey { path: "vault.recent",  ty: ValueType::StringArray },
    EligibleKey { path: "vault.default", ty: ValueType::String },
];

fn eligible_key(scope: SettingsScope, key: &str) -> Result<EligibleKey, HikerError> {
    let table = match scope {
        SettingsScope::User => ELIGIBLE_USER,
        SettingsScope::Vault => ELIGIBLE_VAULT,
    };
    table
        .iter()
        .copied()
        .find(|k| k.path == key)
        .ok_or_else(|| {
            HikerError::Config(format!(
                "setting `{key}` is not user-mutable in v1 (scope: {scope:?})"
            ))
        })
}

fn validate_value(key: &EligibleKey, value: &serde_json::Value) -> Result<(), HikerError> {
    use serde_json::Value as J;
    let ok = match (key.ty, value) {
        (ValueType::Bool, J::Bool(_)) => true,
        (ValueType::String, J::String(_)) => true,
        (ValueType::String, J::Null) => true,
        (ValueType::StringArray, J::Array(arr)) => arr.iter().all(|v| v.is_string()),
        (ValueType::TreeSortBy, J::String(s)) => matches!(
            s.as_str(),
            "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc"
        ),
        _ => false,
    };
    if !ok {
        return Err(HikerError::Config(format!(
            "setting `{}` got invalid value `{value}`",
            key.path
        )));
    }
    Ok(())
}

/// Patch `doc` so the dotted-path key resolves to `value`. Creates any
/// intermediate tables that don't exist.
fn apply_patch(doc: &mut toml_edit::DocumentMut, key: &str, value: &serde_json::Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let item = json_to_toml_item(value);

    // Walk to the parent table, creating intermediate tables as we go.
    let mut cursor: &mut toml_edit::Item = doc.as_item_mut();
    for part in &parts[..parts.len() - 1] {
        // If the slot is missing or not a table, replace with an empty table.
        let needs_replace = !matches!(cursor.get(part), Some(toml_edit::Item::Table(_)));
        if needs_replace {
            // `cursor` here may be the root document or a sub-table.
            match cursor {
                toml_edit::Item::Table(t) => {
                    t.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                _ => {
                    // The parent isn't a table — replace it wholesale.
                    *cursor = toml_edit::Item::Table(toml_edit::Table::new());
                    if let toml_edit::Item::Table(t) = cursor {
                        t.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
                    }
                }
            }
        }
        cursor = cursor
            .get_mut(part)
            .expect("intermediate slot was just ensured to be a Table");
    }

    let leaf = parts[parts.len() - 1];
    match cursor {
        toml_edit::Item::Table(t) => {
            t.insert(leaf, item);
        }
        _ => {
            // Same fallback as above: ensure the parent is a table.
            *cursor = toml_edit::Item::Table(toml_edit::Table::new());
            if let toml_edit::Item::Table(t) = cursor {
                t.insert(leaf, item);
            }
        }
    }
}

fn json_to_toml_item(value: &serde_json::Value) -> toml_edit::Item {
    use serde_json::Value as J;
    match value {
        J::Bool(b) => toml_edit::value(*b),
        J::String(s) => toml_edit::value(s.as_str()),
        J::Null => toml_edit::Item::None,
        J::Array(arr) => {
            let mut a = toml_edit::Array::new();
            for v in arr {
                if let J::String(s) = v {
                    a.push(s.as_str());
                }
            }
            toml_edit::value(a)
        }
        J::Number(_) | J::Object(_) => {
            // validate_value rejects these for our eligible-key set, so this
            // branch is unreachable in practice. Falling back to None keeps
            // the function total without panicking.
            toml_edit::Item::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_round_trip() {
        let cfg = Config::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg.editor.live_preview, back.editor.live_preview);
        assert_eq!(cfg.indexing.batch_size, back.indexing.batch_size);
    }

    #[test]
    fn unknown_key_rejected() {
        let bad = "schema_version = 1\nmystery_key = true\n";
        let err = toml::from_str::<Config>(bad).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mystery_key"), "got: {msg}");
    }

    #[test]
    fn unknown_section_key_rejected() {
        let bad = "[editor]\nrandom = true\n";
        let err = toml::from_str::<Config>(bad).unwrap_err();
        assert!(err.to_string().contains("random"));
    }

    #[test]
    fn auto_create_writes_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".hiker").join("config.toml");
        assert!(!path.exists());
        let _ = read_or_create(&path, &Config::default()).unwrap();
        assert!(path.exists());
        let raw = fs::read_to_string(&path).unwrap();
        // Header comment + serialized defaults.
        assert!(raw.contains("# Hiker settings"));
        assert!(raw.contains("[editor]"));
        assert!(raw.contains("render_txt_as_markdown = true"));
    }

    #[test]
    fn deep_merge_vault_wins() {
        let mut base: toml::Value = toml::from_str(
            r#"schema_version = 1
[editor]
live_preview = true
word_wrap = false
"#,
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            r#"[editor]
word_wrap = true
"#,
        )
        .unwrap();
        deep_merge(&mut base, over);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.editor.live_preview, true);
        assert_eq!(cfg.editor.word_wrap, true);
    }

    #[test]
    fn deep_merge_arrays_replace() {
        let mut base: toml::Value = toml::from_str(
            r#"[indexing]
model = "bge-small-en-v1.5"
ignored_paths = ["foo/"]
"#,
        )
        .unwrap();
        let over: toml::Value = toml::from_str(
            r#"[indexing]
ignored_paths = ["bar/"]
"#,
        )
        .unwrap();
        deep_merge(&mut base, over);
        let cfg: Config = base.try_into().unwrap();
        assert_eq!(cfg.indexing.ignored_paths, vec!["bar/".to_string()]);
    }

    #[test]
    fn schema_version_mismatch_errors() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join(".hiker").join("config.toml");
        fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        fs::write(&vault_path, "schema_version = 999\n").unwrap();
        // Force the user-side path empty by using a separate vault dir; the
        // user TOML will auto-create defaults at the platform config dir
        // which is fine — vault wins on schema_version.
        let err = Config::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("schema_version 999"));
    }

    #[test]
    fn write_back_patches_in_place_preserving_comments() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().join(".hiker").join("config.toml");
        fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        // Hand-written TOML with a comment that must survive.
        fs::write(
            &vault_path,
            "schema_version = 1\n\n# my preferred toggles\n[editor]\nlive_preview = true\n",
        )
        .unwrap();
        Config::set(
            SettingsScope::Vault,
            "editor.live_preview",
            serde_json::Value::Bool(false),
            dir.path(),
        )
        .unwrap();
        let raw = fs::read_to_string(&vault_path).unwrap();
        assert!(raw.contains("# my preferred toggles"), "comment lost: {raw}");
        assert!(raw.contains("live_preview = false"), "value not patched: {raw}");
    }

    #[test]
    fn write_back_rejects_non_eligible_key() {
        let dir = tempdir().unwrap();
        let err = Config::set(
            SettingsScope::Vault,
            "editor.tab_size",
            serde_json::Value::Number(4.into()),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not user-mutable"));
    }

    #[test]
    fn write_back_rejects_wrong_type() {
        let dir = tempdir().unwrap();
        let err = Config::set(
            SettingsScope::Vault,
            "editor.live_preview",
            serde_json::Value::String("yes please".into()),
            dir.path(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid value"));
    }

    #[test]
    fn write_back_creates_new_table_path() {
        let dir = tempdir().unwrap();
        // Brand-new vault, no TOML yet. Set a nested key and confirm it
        // landed at the correct path.
        let cfg = Config::set(
            SettingsScope::Vault,
            "vault.tree.sort_by",
            serde_json::Value::String("mtime_desc".into()),
            dir.path(),
        )
        .unwrap();
        assert_eq!(cfg.vault.tree.sort_by, TreeSortBy::MtimeDesc);
    }
}
