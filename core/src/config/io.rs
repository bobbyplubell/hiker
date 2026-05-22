//! File I/O helpers for the settings loader: deep-merge, atomic write,
//! and the `read_or_create*` family used to seed user/vault TOMLs.

use std::fs;
use std::path::Path;

use crate::errors::HikerError;

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
