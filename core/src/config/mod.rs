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

use crate::errors::HikerError;

mod io;
mod patch;
pub mod recovery;
pub mod sections;
pub mod vcs;

use recovery::HistoryConfig;
use vcs::GitSection;
use sections::{
    BoardsConfig, ClusteringConfig, EditorConfig, InboxConfig,
    IndexingConfig, LlmConfig, McpConfig, EditingConfig, RenderConfig, SearchConfig, SuggestionsConfig,
    TasksConfig, TrailsConfig, VaultConfig, WikilinksConfig,
};

use io::{atomic_write, deep_merge, display_path, write_defaults};
use patch::EligibleKey;

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
    pub boards: BoardsConfig,
    /// status: wikilink-ambiguous-resolution
    #[serde(default)]
    pub wikilinks: WikilinksConfig,
    /// status: op-log-config-section
    #[serde(default, rename = "editing")]
    pub editing: EditingConfig,
    /// status: plain-file-snapshots
    #[serde(default)]
    pub history: HistoryConfig,
    /// status: git-config-section
    #[serde(default)]
    pub git: GitSection,
    #[serde(default)]
    pub suggestions: SuggestionsConfig,
    /// status: trail-draft-from-clustering
    #[serde(default)]
    pub clustering: ClusteringConfig,
    /// status: inbox-rules
    #[serde(default)]
    pub inbox: InboxConfig,
    /// status: render-cache-diagrams-toggle
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub ui: Ui,
    /// The `[kinds.<name>]` registry entries, kept as raw TOML values so
    /// `kinds::Registry::compile` can produce strict-load errors naming the
    /// offending entry (a typed serde field would lose the entry context
    /// once the merged document deserializes). Validated in
    /// `validate_cross_field`; the built-in PM set is merged in as the
    /// lowest config layer by `Config::load`, so defaults stay empty here
    /// (and out of auto-created files). See `docs/kinds.md`.
    ///
    /// status: kind-registry
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub kinds: std::collections::BTreeMap<String, toml::Value>,
    /// The `[rules.<name>]` vault-rule entries (`docs/rules.md`), kept as
    /// raw TOML values exactly like `kinds` so `rules::RuleSet::compile`
    /// can produce strict-load errors naming the offending entry.
    /// Validated in `validate_cross_field` against the compiled kind
    /// registry; the live engine recompiles at vault open.
    ///
    /// status: rule-shape
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub rules: std::collections::BTreeMap<String, toml::Value>,
}

/// How a plain (no-modifier) scroll behaves in the canvas view. `auto` picks per
/// device (mouse wheel → zoom to cursor, touchpad → pan); `pan` / `zoom` force one
/// behavior. Serialized lowercase in `[ui] canvas_scroll_mode`. [canvas-scroll-mode]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CanvasScrollMode {
    #[default]
    Auto,
    Pan,
    Zoom,
}

impl CanvasScrollMode {
    /// The lowercase wire string (`auto` / `pan` / `zoom`), for `Config::set`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pan => "pan",
            Self::Zoom => "zoom",
        }
    }
}

/// UI-layer preferences. Currently just the custom-titlebar toggle;
/// future entries will join (theme, sidebar widths, etc.). Living on
/// `Config` means changes persist via the standard `Config::set` path.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    /// When true (the default), the app draws its own titlebar (window
    /// controls + merged top bar) and asks eframe to hide native chrome.
    #[serde(default = "default_true")]
    pub custom_titlebar: bool,
    /// When true, reader / focus mode also hides the global top bar (the
    /// custom titlebar or the native top toolbar), leaving only the editor
    /// and — in frameless mode — the window resize grips. Default false.
    #[serde(default)]
    pub reader_hide_top_bar: bool,
    /// When true, reader / focus mode also hides the tab strip (the row of tab
    /// handles above the focused tab). Reader mode shows tabs by default; this
    /// opts into hiding them. Default false.
    #[serde(default)]
    pub reader_hide_tabs: bool,
    /// When true, reader / focus mode also hides each view's in-tab toolbar (the
    /// canvas create toolbar, the editor toolbar, the board/graph action rows,
    /// …). Reader mode shows these by default; this opts into hiding them.
    /// Default false.
    #[serde(default)]
    pub reader_hide_toolbar: bool,
    /// How a plain (no-modifier) two-finger / wheel scroll behaves in the canvas
    /// view. `auto` (default) — **detect the device**: a mouse wheel **zooms** to
    /// the cursor, a touchpad two-finger scroll **pans** the camera. `pan` / `zoom`
    /// force one behavior (e.g. a high-res wheel that reports pixel deltas and is
    /// misread as a touchpad). Ctrl/Cmd+scroll and pinch always zoom regardless;
    /// scrolling over a note card always scrolls that card. [canvas-scroll-mode]
    #[serde(default)]
    pub canvas_scroll_mode: CanvasScrollMode,
    /// Whether a two-finger horizontal trackpad swipe navigates Back/Forward
    /// (browser-style). Default `true`. Turn off if it misfires during ordinary
    /// horizontal scrolling. [navigation-swipe-disable]
    #[serde(default = "default_true")]
    pub swipe_nav_enabled: bool,
    /// Whether hovering a sidebar row shows a rich hover preview (the canvas /
    /// cluster-tree thumbnail expand and the note markdown+diagram popup). Default
    /// `true`; toggled from any preview-showing view's eye menu. Disabling leaves
    /// any always-on inline thumbnails but suppresses the hover-expand popup.
    /// [preview-toggle]
    #[serde(default = "default_true")]
    pub hover_previews_enabled: bool,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            custom_titlebar: true,
            reader_hide_top_bar: false,
            reader_hide_tabs: false,
            reader_hide_toolbar: false,
            canvas_scroll_mode: CanvasScrollMode::Auto,
            swipe_nav_enabled: true,
            hover_previews_enabled: true,
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_schema_version() -> u32 {
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
            boards: BoardsConfig::default(),
            wikilinks: WikilinksConfig::default(),
            editing: EditingConfig::default(),
            history: HistoryConfig::default(),
            git: GitSection::default(),
            suggestions: SuggestionsConfig::default(),
            clustering: ClusteringConfig::default(),
            inbox: InboxConfig::default(),
            render: RenderConfig::default(),
            ui: Ui::default(),
            kinds: std::collections::BTreeMap::new(),
            rules: std::collections::BTreeMap::new(),
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
pub struct Paths {
    pub user: Option<PathBuf>,
    pub vault: PathBuf,
}

impl Paths {
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
            .map(std::string::ToString::to_string))
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
        let paths = Paths::resolve(vault_root);
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
        let paths = Paths::resolve(vault_root);
        let user_doc = load_user_doc(paths.user.as_deref())?;
        let vault_doc = load_vault_doc(&paths.vault)?;

        // Deep-merge built-ins under user under vault (vault wins per-key).
        // Tables recurse; arrays and scalars replace. The built-in PM kinds
        // are the lowest layer — registry entries in the same TOML format
        // users write, so a vault that redefines `kinds.story.fields`
        // replaces the list wholesale while untouched keys keep their
        // built-in values, and `[kinds.<name>] enabled = false` disables an
        // entry (`kind-builtin-pm-set`).
        let mut merged: toml::Value = crate::kinds::builtin_kinds_value();
        if let Some(user) = user_doc {
            deep_merge(&mut merged, user);
        }
        deep_merge(&mut merged, vault_doc);

        check_schema_version(&merged, &paths)?;
        let cfg = deserialize_strict(merged, &paths)?;
        validate_cross_field(&cfg)?;
        // Wire the (previously dead) `[indexing] ignored_paths` into the
        // per-vault composed ignore matcher, alongside the vault-root
        // `.gitignore` / `.hikerignore` read from disk. Consulted by the
        // indexer walk, the watcher route, and `vault::list_dir`. See
        // `core::ignore` (Phase B of code-as-reference-content).
        crate::ignore::register(vault_root, &cfg.indexing.ignored_paths);
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
        value: &serde_json::Value,
        vault_root: &Path,
    ) -> Result<Self, HikerError> {
        use patch::{ELIGIBLE_USER, ELIGIBLE_VAULT, Patcher};
        let table = match scope {
            SettingsScope::User => ELIGIBLE_USER,
            SettingsScope::Vault => ELIGIBLE_VAULT,
        };
        let allowed: EligibleKey = table
            .iter()
            .copied()
            .find(|k| k.path == key)
            .ok_or_else(|| {
                HikerError::Config(format!(
                    "setting `{key}` is not user-mutable in v1 (scope: {scope:?})"
                ))
            })?;
        if !allowed.validate(value) {
            return Err(HikerError::Config(format!(
                "setting `{}` got invalid value `{value}`",
                allowed.path
            )));
        }

        let paths = Paths::resolve(vault_root);
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
            SettingsScope::User => {
                if !target.exists() {
                    write_defaults(&target, &Self::default())?;
                }
                let raw = fs::read_to_string(&target).map_err(|e| {
                    HikerError::Config(format!("read {}: {e}", target.display()))
                })?;
                raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
                    HikerError::Config(format!("parse {}: {e}", target.display()))
                })?
            }
            SettingsScope::Vault => {
                if !target.exists() {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).map_err(|e| {
                            HikerError::Config(format!("mkdir {}: {e}", parent.display()))
                        })?;
                    }
                    let header = format!(
                        "# Hiker vault settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n\
                         # This file was auto-generated. Add per-vault overrides here;\n\
                         # user-scope settings (LLM provider, API keys, etc.) live in your user config.toml.\n\n"
                    );
                    let body = format!("schema_version = {SCHEMA_VERSION}\n");
                    atomic_write(&target, format!("{header}{body}").as_bytes())?;
                }
                let raw = fs::read_to_string(&target).map_err(|e| {
                    HikerError::Config(format!("read {}: {e}", target.display()))
                })?;
                raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
                    HikerError::Config(format!("parse {}: {e}", target.display()))
                })?
            }
        };
        Patcher { doc: &mut doc }.set(key, value);
        atomic_write(&target, doc.to_string().as_bytes())?;

        // Reload through the normal path so the returned Config reflects
        // the merged state across both files.
        Self::load(vault_root)
    }
}

// ---------- `Config::load` helpers ----------
//
// Split out of `load` so the loader stays under the cognitive-complexity
// budget. The split is along natural seams: parse the user file, parse
// the vault file, verify the schema version against the binary, deserialize
// with full per-file error context, and run cross-field invariants.

/// Read + parse the per-user TOML, auto-creating it with defaults if
/// missing. `None` when the platform config dir couldn't be resolved at
/// all (treated as "no user file"); the loader still works off the vault
/// file alone in that case.
fn load_user_doc(user_path: Option<&Path>) -> Result<Option<toml::Value>, HikerError> {
    let Some(p) = user_path else { return Ok(None) };
    if p.exists() {
        let raw = fs::read_to_string(p).map_err(|e| {
            tracing::error!(file = %p.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", p.display()))
        })?;
        let parsed = toml::from_str::<toml::Value>(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(file = %p.display(), error = %e, "settings parse failed");
            HikerError::Config(format!("parse {}: {e}", p.display()))
        })?;
        Ok(Some(parsed))
    } else {
        write_defaults(p, &Config::default())?;
        Ok(Some(
            toml::Value::try_from(Config::default()).expect("Config serializes cleanly"),
        ))
    }
}

/// Read + parse the per-vault TOML, auto-creating a minimal stub
/// (header + `schema_version`) if missing.
fn load_vault_doc(path: &Path) -> Result<toml::Value, HikerError> {
    if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| {
            tracing::error!(file = %path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str::<toml::Value>(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(file = %path.display(), error = %e, "settings parse failed");
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| HikerError::Config(format!("mkdir {}: {e}", parent.display())))?;
        }
        let header = format!(
            "# Hiker vault settings (schema_version = {SCHEMA_VERSION}). See docs/settings.md.\n\
             # This file was auto-generated. Add per-vault overrides here;\n\
             # user-scope settings (LLM provider, API keys, etc.) live in your user config.toml.\n\n"
        );
        let body = format!("schema_version = {SCHEMA_VERSION}\n");
        let bytes = format!("{header}{body}");
        atomic_write(path, bytes.as_bytes())?;
        let mut map = toml::map::Map::new();
        map.insert(
            "schema_version".into(),
            toml::Value::Integer(SCHEMA_VERSION.into()),
        );
        Ok(toml::Value::Table(map))
    }
}

/// Fail fast on a `schema_version` mismatch so users get a "schema N,
/// expected M" instead of an unknown-field error from a future binary.
fn check_schema_version(merged: &toml::Value, paths: &Paths) -> Result<(), HikerError> {
    let Some(toml::Value::Integer(v)) = merged.get("schema_version") else {
        return Ok(());
    };
    if *v as u32 == SCHEMA_VERSION {
        return Ok(());
    }
    let user_disp = display_path(paths.user.as_deref());
    let vault_disp = paths.vault.display().to_string();
    tracing::error!(
        user_file = %user_disp,
        vault_file = %vault_disp,
        found = *v,
        expected = SCHEMA_VERSION,
        "settings schema_version mismatch",
    );
    Err(HikerError::Config(format!(
        "settings schema_version {v}, this binary expects {SCHEMA_VERSION} (user={user_disp}, vault={vault_disp})"
    )))
}

/// Deserialize the merged TOML into `Config` with `deny_unknown_fields`
/// active. On failure, surface both source paths so the user can grep —
/// we can't single out which file contributed the offending key from the
/// merged view.
fn deserialize_strict(merged: toml::Value, paths: &Paths) -> Result<Config, HikerError> {
    merged.try_into().map_err(|e: toml::de::Error| {
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
    })
}

/// Cross-field invariants checked after deserialization succeeds:
/// embedder model is a supported id, batch size is non-zero, inbox rules
/// compile cleanly.
fn validate_cross_field(cfg: &Config) -> Result<(), HikerError> {
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
    // status: inbox-rules
    // Compile happens here (vs. at first use) so a malformed rule aborts at
    // vault open with a clear "rule N: <reason>" message.
    if let Err(e) = crate::inbox::Rules::validate(&cfg.inbox.rules) {
        tracing::error!(error = %e, "invalid [inbox] rules");
        return Err(HikerError::Config(format!("[inbox] {e}")));
    }
    validate_registries(cfg)
}

/// The registry half of the cross-field hook: the `[kinds.<name>]` table
/// compiles first (the inbox-rules posture — an invalid entry aborts
/// startup naming the offender while notes validated *against* it stay
/// lenient, `kind-lenient-validation`), then the `[rules.<name>]` table
/// compiles beside the kinds it references — an unknown trigger, a
/// condition outside the queries grammar, an unknown verb, the reserved
/// `script` verb, or a malformed board / kind reference aborts startup
/// naming the rule (`docs/rules.md`).
///
/// status: kind-registry
/// status: rule-shape
fn validate_registries(cfg: &Config) -> Result<(), HikerError> {
    let kinds = match crate::kinds::Registry::compile(&cfg.kinds) {
        Ok(registry) => registry,
        Err(e) => {
            tracing::error!(error = %e, "invalid [kinds] registry");
            return Err(HikerError::Config(e.to_string()));
        }
    };
    if let Err(e) = crate::rules::RuleSet::compile(&cfg.rules, &kinds) {
        tracing::error!(error = %e, "invalid [rules] entry");
        return Err(HikerError::Config(e.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
