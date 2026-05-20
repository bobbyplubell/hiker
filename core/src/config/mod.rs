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

mod io;
mod patch;
mod sections;

pub use sections::*;

use io::{
    atomic_write, deep_merge, display_path, read_or_create, read_or_create_doc,
    read_or_create_minimal, read_or_create_minimal_doc,
};
use patch::{apply_patch, eligible_key, validate_value};

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
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub tasks: TasksConfig,
    #[serde(default)]
    pub trails: TrailsConfig,
    #[serde(default)]
    pub acp: AcpConfig,
    #[serde(default)]
    pub staging: StagingConfig,
    #[serde(default)]
    pub suggestions: SuggestionsConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

/// UI-layer preferences. Currently just the custom-titlebar toggle;
/// future entries will join (theme, sidebar widths, etc.). Living on
/// `Config` means changes persist via the standard `Config::set` path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    /// When true, the app draws its own titlebar (drag region + window
    /// controls) and asks eframe to hide native chrome.
    #[serde(default)]
    pub custom_titlebar: bool,
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
            mcp: McpConfig::default(),
            llm: LlmConfig::default(),
            tasks: TasksConfig::default(),
            trails: TrailsConfig::default(),
            acp: AcpConfig::default(),
            staging: StagingConfig::default(),
            suggestions: SuggestionsConfig::default(),
            ui: UiConfig::default(),
        }
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

    /// Read a single file (user or vault TOML) without merging or
    /// triggering auto-create. Missing files return `Config::default()` so
    /// the settings UI's per-section scope toggle can show "what this file
    /// alone contributes" against the current schema's defaults. Parse
    /// errors and unknown-field errors bubble up — the same strict-load
    /// posture as `Config::load`.
    ///
    /// status: settings-pane-scope-toggle
    pub fn read_file_only(scope: SettingsScope, vault_root: &Path) -> Result<Self, HikerError> {
        let paths = ConfigPaths::resolve(vault_root);
        let path = match scope {
            SettingsScope::User => match paths.user.as_ref() {
                Some(p) => p.clone(),
                None => return Ok(Self::default()),
            },
            SettingsScope::Vault => paths.vault.clone(),
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path).map_err(|e| {
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
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

        let vault_doc = read_or_create_minimal(&paths.vault)?;

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
        if let Some(toml::Value::Integer(v)) = merged.get("schema_version")
            && *v as u32 != SCHEMA_VERSION
        {
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

        // Cross-field validation: model must be one of the supported
        // fastembed ids (per `embedder-model-selectable`). batch_size must
        // be non-zero.
        if !crate::embed::is_known_model(&cfg.indexing.model) {
            tracing::error!(
                key = "indexing.model",
                value = %cfg.indexing.model,
                "unsupported settings value",
            );
            return Err(HikerError::Config(format!(
                "indexing.model = \"{}\" — supported: {}",
                cfg.indexing.model,
                crate::embed::supported_model_ids().join(", "),
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

        // Read-or-create the target file. For user-scope writes we seed
        // full defaults so the user can see available keys; for vault-scope
        // writes we seed only schema_version to avoid auto-created defaults
        // silently overriding user settings (e.g. LLM provider backend).
        let mut doc = match scope {
            SettingsScope::User => read_or_create_doc(&target, &Self::default())?,
            SettingsScope::Vault => read_or_create_minimal_doc(&target)?,
        };
        apply_patch(&mut doc, key, &value);
        atomic_write(&target, doc.to_string().as_bytes())?;

        // Reload through the normal path so the returned Config reflects
        // the merged state across both files.
        Self::load(vault_root)
    }
}

#[cfg(test)]
mod tests;
