//! Two-tier prompt-file store for LLM-driven features. See `docs/llm.md`
//! §"Prompts as files".
//!
//! Every LLM-driven feature has its prompt stored as a markdown file.
//! Editing the file changes the prompt; the audit log + Prompts tab
//! (deferred) both surface what gets sent. Two tiers:
//!
//! - **User scope** at `~/.config/hiker/prompts/<feature>.md` — bundled
//!   defaults, written on first launch. The user is free to edit.
//! - **Vault scope** at `vault/.hiker/prompts/<feature>.md` — per-project
//!   overrides; vault wins on conflict.
//!
//! Mustache-style `{{var}}` substitution is provided by `render`. Each
//! shipped default starts with an HTML-style comment block naming the
//! placeholders it expects, so users editing the file can see what's
//! available.
//!
//! **Upgrade-aware staleness** (`llm-prompts-staleness-on-upgrade`) is
//! handled by stamping the bundled default's blake3 hash in a sidecar
//! `<feature>.default.sha` next to each prompt. When the bundled default
//! changes upstream, the user's edit isn't clobbered — `staleness()`
//! reports the divergence so the agent log and (eventual) Prompts tab can
//! flag it; the user decides whether to merge.
//
// status: llm-prompts-file-store
// status: llm-prompts-mustache-templating
// status: llm-prompts-staleness-on-upgrade

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::hash_string;

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("io: {0}")]
    Io(String),
    #[error("no platform config dir available")]
    NoUserConfigDir,
    #[error("unknown feature: {0}")]
    UnknownFeature(String),
}

impl From<std::io::Error> for PromptError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// One bundled default. The agent loop's chat-panel system prompt, the
/// auto-tag prompt, the summary prompt — each registers an entry here.
/// Adding a new feature is one row.
#[derive(Debug, Clone)]
pub struct PromptDefault {
    /// Stable feature name; becomes `<name>.md` on disk.
    pub name: &'static str,
    /// File contents written to user scope on first run. Convention is to
    /// start with an HTML comment block listing the placeholders.
    pub default_body: &'static str,
}

/// Bundled defaults registered for v3.5. Add a row here when a feature
/// lands; the loader writes them on first launch.
pub const fn bundled_defaults() -> &'static [PromptDefault] {
    &[
        PromptDefault {
            name: "chat_system",
            default_body: CHAT_SYSTEM_DEFAULT,
        },
        // status: note-mutation-reformat-as-markdown
        PromptDefault {
            name: "note_mutation_reformat_as_markdown",
            default_body: NOTE_MUTATION_REFORMAT_AS_MARKDOWN_DEFAULT,
        },
        // status: cluster-summarize-llm
        PromptDefault {
            name: "cluster_summarize",
            default_body: CLUSTER_SUMMARIZE_DEFAULT,
        },
    ]
}

/// Bundled prompt bodies. Each lives at `core/prompts/<feature_key>.md`
/// in the source tree per `llm.md`'s convention; `include_str!` bakes
/// the file into the binary so first-run materialization can write it
/// to the user-scope path without depending on the build output dir.
const CHAT_SYSTEM_DEFAULT: &str =
    include_str!("../prompts/chat_system.md");
const NOTE_MUTATION_REFORMAT_AS_MARKDOWN_DEFAULT: &str =
    include_str!("../prompts/note_mutation_reformat_as_markdown.md");
const CLUSTER_SUMMARIZE_DEFAULT: &str =
    include_str!("../prompts/cluster_summarize.md");

/// Resolved file paths for a single feature's prompt.
#[derive(Debug, Clone)]
pub struct PromptPaths {
    pub user: Option<PathBuf>,
    pub vault: PathBuf,
    pub user_default_hash: Option<PathBuf>,
}

impl PromptPaths {
    pub fn resolve(vault_root: &Path, feature: &str) -> Self {
        let user_dir = directories::ProjectDirs::from("", "", "hiker")
            .map(|p| p.config_dir().join("prompts"));
        Self::resolve_with_user_dir(vault_root, feature, user_dir.as_deref())
    }

    pub fn resolve_with_user_dir(
        vault_root: &Path,
        feature: &str,
        user_dir: Option<&Path>,
    ) -> Self {
        let user = user_dir.map(|d| d.join(format!("{feature}.md")));
        let user_default_hash = user_dir.map(|d| d.join(format!("{feature}.default.sha")));
        let vault = vault_root
            .join(".hiker")
            .join("prompts")
            .join(format!("{feature}.md"));
        Self {
            user,
            vault,
            user_default_hash,
        }
    }
}

/// In-memory prompt store. Loaded once at vault open per
/// `settings-load-once-at-startup`-style discipline; the user-edited file
/// is the authoritative surface, and a relaunch picks up changes.
#[derive(Debug, Clone)]
pub struct Prompts {
    /// Loaded text per feature, keyed by name. Vault scope > user scope.
    bodies: BTreeMap<String, String>,
}

impl Prompts {
    /// Load the prompt store for an open vault. Auto-creates the user
    /// scope on first run with the bundled defaults; writes a sidecar
    /// `<feature>.default.sha` so future loads can detect upstream
    /// staleness without clobbering user edits.
    pub fn load(vault_root: &Path) -> Result<Self, PromptError> {
        Self::load_with(vault_root, bundled_defaults())
    }

    /// Same as `load` but with an explicit defaults list. Used by tests.
    pub fn load_with(
        vault_root: &Path,
        defaults: &[PromptDefault],
    ) -> Result<Self, PromptError> {
        Self::load_with_user_dir(vault_root, None, defaults)
    }

    /// Same as `load_with` but with an explicit user-scope directory
    /// override. Used by tests so parallel runs don't collide on the
    /// real platform config dir.
    pub fn load_with_user_dir(
        vault_root: &Path,
        user_dir: Option<&Path>,
        defaults: &[PromptDefault],
    ) -> Result<Self, PromptError> {
        let mut bodies: BTreeMap<String, String> = BTreeMap::new();
        for d in defaults {
            let paths = match user_dir {
                Some(u) => PromptPaths::resolve_with_user_dir(vault_root, d.name, Some(u)),
                None => PromptPaths::resolve(vault_root, d.name),
            };
            // Ensure user-scope default file and hash stamp exist.
            if let (Some(user_path), Some(stamp_path)) =
                (paths.user.as_ref(), paths.user_default_hash.as_ref())
            {
                if let Some(parent) = user_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                if !user_path.exists() {
                    atomic_write(user_path, d.default_body.as_bytes())?;
                }
                // Stamp the bundled default's hash so future runs can detect
                // upstream drift. On a brand-new install that's "the hash
                // matches"; once the bundled default is bumped without
                // rewriting the user file, the staleness check fires.
                if !stamp_path.exists() {
                    atomic_write(
                        stamp_path,
                        hash_string(d.default_body).as_bytes(),
                    )?;
                }
            }
            // Vault scope wins over user scope.
            let body = if paths.vault.exists() {
                fs::read_to_string(&paths.vault)?
            } else if let Some(user) = paths.user.as_ref().filter(|p| p.exists()) {
                fs::read_to_string(user)?
            } else {
                d.default_body.to_string()
            };
            bodies.insert(d.name.to_string(), body);
        }
        Ok(Self { bodies })
    }

    /// Render `feature` with `vars` substituted. Unknown placeholders are
    /// left as-is so a partially-customized prompt doesn't silently lose
    /// data.
    pub fn render<I, K, V>(
        &self,
        feature: &str,
        vars: I,
    ) -> Result<String, PromptError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let body = self
            .bodies
            .get(feature)
            .ok_or_else(|| PromptError::UnknownFeature(feature.to_string()))?;
        let vars: Vec<(String, String)> = vars
            .into_iter()
            .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string()))
            .collect();
        // `{{var}}` substitution. Unknown placeholders are left intact — the
        // caller almost certainly has a bug if a placeholder is missing, and
        // silently dropping it would hide it.
        let mut out = String::with_capacity(body.len());
        let mut rest = body.as_str();
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after_open = &rest[start + 2..];
            match after_open.find("}}") {
                Some(end) => {
                    let key = after_open[..end].trim();
                    let replacement = vars
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.as_str());
                    match replacement {
                        Some(v) => out.push_str(v),
                        None => {
                            // Pass the placeholder through verbatim.
                            out.push_str("{{");
                            out.push_str(&after_open[..end]);
                            out.push_str("}}");
                        }
                    }
                    rest = &after_open[end + 2..];
                }
                None => {
                    // Unterminated `{{` — treat as literal and stop scanning.
                    out.push_str("{{");
                    out.push_str(after_open);
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Raw body for a feature; useful for the eventual Prompts tab and
    /// for tests. Returns `None` if the feature wasn't registered.
    pub fn body(&self, feature: &str) -> Option<&str> {
        self.bodies.get(feature).map(std::string::String::as_str)
    }

    /// Compare each loaded prompt's bundled-default hash against the
    /// current bundled default's hash. Returns the names of features
    /// whose defaults have moved upstream since the user TOML was first
    /// written — the agent log surfaces these so the user decides
    /// whether to merge.
    ///
    /// Skipped (not stale) when the user scope is unavailable (sandboxed
    /// env where `directories::ProjectDirs` resolves to nothing).
    pub fn staleness(vault_root: &Path) -> Result<Vec<String>, PromptError> {
        Self::staleness_with(vault_root, bundled_defaults())
    }

    pub fn staleness_with(
        vault_root: &Path,
        defaults: &[PromptDefault],
    ) -> Result<Vec<String>, PromptError> {
        let mut stale = Vec::new();
        for d in defaults {
            let paths = PromptPaths::resolve(vault_root, d.name);
            let stamp_path = match paths.user_default_hash.as_ref() {
                Some(p) => p,
                None => continue,
            };
            if !stamp_path.exists() {
                continue;
            }
            let stamped = fs::read_to_string(stamp_path)?;
            let current = hash_string(d.default_body);
            if stamped.trim() != current {
                stale.push(d.name.to_string());
            }
        }
        Ok(stale)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PromptError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn load_isolated(
        vault_root: &Path,
        defaults: &[PromptDefault],
    ) -> Prompts {
        // Each test gets its own user-scope tempdir so parallel runs don't
        // race over the real platform config dir.
        let user_dir = tempdir().unwrap();
        Prompts::load_with_user_dir(vault_root, Some(user_dir.path()), defaults).unwrap()
    }

    fn fixture() -> &'static [PromptDefault] {
        &[
            PromptDefault {
                name: "demo",
                default_body: "Hello {{name}}!",
            },
            PromptDefault {
                name: "untouched",
                default_body: "stable body",
            },
        ]
    }

    fn prompts_with_body(body: &'static str) -> Prompts {
        let dir = tempdir().unwrap();
        let defaults = vec![PromptDefault { name: "t", default_body: body }];
        load_isolated(dir.path(), &defaults)
    }

    #[test]
    fn substitute_replaces_known_and_preserves_unknown() {
        let prompts = prompts_with_body("Hi {{name}}, your tag is {{tag}}.");
        let out = prompts.render("t", [("name", "Alice")]).unwrap();
        assert_eq!(out, "Hi Alice, your tag is {{tag}}.");
    }

    #[test]
    fn substitute_handles_unterminated_braces() {
        let prompts = prompts_with_body("Hi {{name");
        let out = prompts.render("t", [("name", "Alice")]).unwrap();
        assert_eq!(out, "Hi {{name");
    }

    #[test]
    fn render_uses_vault_override_over_user_scope() {
        let dir = tempdir().unwrap();
        // Vault override.
        let vault_dir = dir.path().join(".hiker").join("prompts");
        fs::create_dir_all(&vault_dir).unwrap();
        fs::write(vault_dir.join("demo.md"), "Override {{name}}").unwrap();
        let prompts = load_isolated(dir.path(), fixture());
        let out = prompts
            .render("demo", [("name", "world")])
            .unwrap();
        assert_eq!(out, "Override world");
    }

    #[test]
    fn render_falls_back_to_default_body_when_neither_scope_present() {
        // When ProjectDirs resolves to nothing OR the user file just
        // hasn't been written yet, the bundled default body stands in.
        let dir = tempdir().unwrap();
        // No vault override; user scope may write itself, but we don't
        // require it to exist for this assertion to pass — render uses
        // whatever was loaded.
        let prompts = load_isolated(dir.path(), fixture());
        let out = prompts.render("demo", [("name", "v3.5")]).unwrap();
        assert_eq!(out, "Hello v3.5!");
    }

    #[test]
    fn render_unknown_feature_errors() {
        let dir = tempdir().unwrap();
        let prompts = load_isolated(dir.path(), fixture());
        match prompts.render("nope", std::iter::empty::<(&str, &str)>()) {
            Err(PromptError::UnknownFeature(name)) => assert_eq!(name, "nope"),
            other => panic!("expected UnknownFeature, got {other:?}"),
        }
    }
}
