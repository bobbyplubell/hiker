use super::*;
use super::io::{deep_merge, read_or_create};
use super::patch::eligible_key;
use std::fs;
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
    assert!(cfg.editor.live_preview);
    assert!(cfg.editor.word_wrap);
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

#[test]
fn write_back_positive_int_persists_as_integer_not_float() {
    // Regression test: a JS-side `4096` arrives as a JSON integer.
    // We need it written to TOML as `4096`, not `4096.0`, so the
    // strict-load `u32`-typed reader doesn't reject it on the next
    // launch.
    let dir = tempdir().unwrap();
    let cfg = Config::set(
        SettingsScope::Vault,
        "llm.limits.max_tokens",
        serde_json::json!(4096),
        dir.path(),
    )
    .unwrap();
    assert_eq!(cfg.llm.limits.max_tokens, 4096);
    // Confirm the on-disk shape is integer-valued.
    let raw = fs::read_to_string(dir.path().join(".hiker").join("config.toml")).unwrap();
    assert!(
        raw.contains("max_tokens = 4096") && !raw.contains("max_tokens = 4096.0"),
        "expected `max_tokens = 4096` in TOML, got:\n{raw}"
    );
}

#[test]
fn write_back_positive_int_rejects_zero_and_floats() {
    let dir = tempdir().unwrap();
    // Zero is not a positive integer.
    assert!(Config::set(
        SettingsScope::Vault,
        "llm.agent.iteration_cap",
        serde_json::json!(0),
        dir.path(),
    )
    .is_err());
    // Float is rejected even when its value is integer-equivalent;
    // serde_json carries the no-decimal vs. decimal distinction.
    assert!(Config::set(
        SettingsScope::Vault,
        "llm.agent.iteration_cap",
        serde_json::json!(10.5),
        dir.path(),
    )
    .is_err());
}

#[test]
fn write_back_api_key_refused_in_vault_scope() {
    // Spec posture: the literal API key must never live in the
    // vault TOML (which travels with Syncthing/git). The
    // eligibility list refuses the write.
    let dir = tempdir().unwrap();
    let err = Config::set(
        SettingsScope::Vault,
        "llm.provider.api_key",
        serde_json::json!("sk-secret"),
        dir.path(),
    )
    .expect_err("vault scope must refuse api_key");
    let msg = err.to_string();
    assert!(
        msg.contains("api_key") && msg.contains("not user-mutable"),
        "got: {msg}",
    );
    // User scope still lists the key even though the actual on-disk
    // write goes to the platform config dir (skipped here for test
    // isolation; see write_back_llm_keys_eligible_via_vault_scope).
    assert!(eligible_key(SettingsScope::User, "llm.provider.api_key").is_ok());
}

#[test]
fn write_back_llm_keys_eligible_via_vault_scope() {
    let dir = tempdir().unwrap();
    // The settings pane's per-section [User]/[Vault] toggle relies
    // on the LLM keys being writable from either side. We assert
    // the vault-scope path here — the user-scope write goes to the
    // platform config dir which isn't isolated per test, but
    // `eligible_key` covers both scopes uniformly via ELIGIBLE_USER
    // / ELIGIBLE_VAULT (both lists carry the LLM keys).
    let cfg = Config::set(
        SettingsScope::Vault,
        "llm.provider.model",
        serde_json::json!("claude-haiku-4-5"),
        dir.path(),
    )
    .unwrap();
    assert_eq!(cfg.llm.provider.model, "claude-haiku-4-5");
    // Spot-check the eligibility lookup directly so the both-scope
    // promise doesn't regress.
    assert!(eligible_key(SettingsScope::User, "llm.provider.api_key_env").is_ok());
    assert!(eligible_key(SettingsScope::Vault, "llm.audit.log_full_prompt").is_ok());
}
