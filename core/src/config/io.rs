//! File I/O helpers for the settings loader: deep-merge, atomic write,
//! and the `read_or_create*` family used to seed user/vault TOMLs.

use std::fs;
use std::path::Path;

use crate::error::HikerError;

use super::{Config, SCHEMA_VERSION};

/// Deep-merge `override_v` onto `base` in place. Tables recurse; arrays
/// and scalars replace.
pub(super) fn deep_merge(base: &mut toml::Value, override_v: toml::Value) {
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

pub(super) fn display_path(p: Option<&Path>) -> String {
    match p {
        Some(p) => p.display().to_string(),
        None => "<unset>".to_string(),
    }
}

/// Read the file as a `toml::Value`. If missing, write the defaults
/// serialized in full and return that.
pub(super) fn read_or_create(path: &Path, defaults: &Config) -> Result<toml::Value, HikerError> {
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

/// Like `read_or_create` but seeds only `schema_version` in the
/// auto-created file. Used for the vault TOML so auto-created vault
/// defaults don't silently override user-scope settings (e.g. LLM
/// provider backend). If the file already exists, reads it normally.
pub(super) fn read_or_create_minimal(path: &Path) -> Result<toml::Value, HikerError> {
    if path.exists() {
        let raw = fs::read_to_string(path).map_err(|e| {
            tracing::error!(file = %path.display(), error = %e, "settings read failed");
            HikerError::Config(format!("read {}: {e}", path.display()))
        })?;
        toml::from_str(&raw).map_err(|e: toml::de::Error| {
            tracing::error!(
                file = %path.display(),
                error = %e,
                "settings parse failed",
            );
            HikerError::Config(format!("parse {}: {e}", path.display()))
        })
    } else {
        if let Some(parent) = path.parent() {
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
        let bytes = format!("{header}{body}");
        atomic_write(path, bytes.as_bytes())?;
        let mut map = toml::map::Map::new();
        map.insert("schema_version".into(), toml::Value::Integer(SCHEMA_VERSION as i64));
        Ok(toml::Value::Table(map))
    }
}

/// Same as `read_or_create` but returns a `toml_edit::DocumentMut` for
/// in-place patching.
pub(super) fn read_or_create_doc(
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

/// Same as `read_or_create_doc` but seeds only `schema_version`.
/// Used for vault-scope write-back to avoid auto-created defaults
/// overriding user settings.
pub(super) fn read_or_create_minimal_doc(path: &Path) -> Result<toml_edit::DocumentMut, HikerError> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
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
        atomic_write(path, format!("{header}{body}").as_bytes())?;
    }
    let raw = fs::read_to_string(path).map_err(|e| {
        HikerError::Config(format!("read {}: {e}", path.display()))
    })?;
    raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        HikerError::Config(format!("parse {}: {e}", path.display()))
    })
}

pub(super) fn toml_value_from_serde(cfg: &Config) -> toml::Value {
    toml::Value::try_from(cfg).expect("Config serializes cleanly")
}

pub(super) fn write_defaults(path: &Path, defaults: &Config) -> Result<(), HikerError> {
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
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), HikerError> {
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
