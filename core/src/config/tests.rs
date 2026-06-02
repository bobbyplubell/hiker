use super::*;
use super::io::deep_merge;
use super::sections::{SyncMode, TreeSortBy};
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
    // The vault TOML is auto-created with only `schema_version` to
    // avoid clobbering user-scope settings, so exercise auto-create
    // via the user-scope file path that Config::load seeds with full
    // defaults. We do this by writing a full-default file via
    // Config::set on a recognized user key and re-reading. Simpler:
    // call Config::load, then assert the user-side or vault-side
    // file came into being. We assert the vault file gets created
    // with at minimum its schema_version row.
    let dir = tempdir().unwrap();
    let path = dir.path().join(".hiker").join("config.toml");
    assert!(!path.exists());
    let _ = Config::load(dir.path()).unwrap();
    assert!(path.exists());
    let raw = fs::read_to_string(&path).unwrap();
    assert!(raw.contains("schema_version"));
}

#[test]
fn extract_section_loads() {
    // status: settings-section-extract
    // A well-formed [extract] table must load under strict-load (it was
    // deferred before; it lands now). Defaults fill in omitted keys.
    let toml = r#"schema_version = 1
[extract]
auto_globs = ["inbox/", "**/*.pdf"]
clip_folder = "captures/"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.extract.auto_globs, vec!["inbox/".to_string(), "**/*.pdf".to_string()]);
    assert_eq!(cfg.extract.clip_folder, "captures/");
    // Omitted keys take their in-code defaults.
    assert_eq!(cfg.extract.artifact_retention, "latest");
    assert_eq!(cfg.extract.feed_default_poll, "6h");
    assert_eq!(cfg.extract.feed_item_retention, "keep:200");
}

#[test]
fn extract_section_rejects_unknown_key() {
    let bad = "[extract]\nmystery = true\n";
    let err = toml::from_str::<Config>(bad).unwrap_err();
    assert!(err.to_string().contains("mystery"));
}

#[test]
fn chat_section_round_trips() {
    // status: settings-section-chat
    // A well-formed [chat] table loads under strict-load; `chats_dir`
    // overrides the default and an omitted table takes the default.
    let toml = "schema_version = 1\n[chat]\nchats_dir = \"conversations/\"\n";
    let cfg: Config = toml::from_str(toml).unwrap();
    assert_eq!(cfg.chat.chats_dir, "conversations/");
    // Default when absent.
    let cfg2: Config = toml::from_str("schema_version = 1\n").unwrap();
    assert_eq!(cfg2.chat.chats_dir, "chats/");
    // Default round-trips through serialize → deserialize.
    let s = toml::to_string_pretty(&Config::default()).unwrap();
    let back: Config = toml::from_str(&s).unwrap();
    assert_eq!(back.chat.chats_dir, "chats/");
}

#[test]
fn chat_section_rejects_unknown_key() {
    let bad = "[chat]\nmystery = true\n";
    let err = toml::from_str::<Config>(bad).unwrap_err();
    assert!(err.to_string().contains("mystery"));
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
        &serde_json::Value::Bool(false),
        dir.path(),
    )
    .unwrap();
    let raw = fs::read_to_string(&vault_path).unwrap();
    assert!(raw.contains("# my preferred toggles"), "comment lost: {raw}");
    assert!(raw.contains("live_preview = false"), "value not patched: {raw}");
}

#[test]
fn editor_view_toggles_persist_and_reload() {
    // The full set of write-back-eligible `[editor]` view toggles must
    // survive a `Config::set` → `Config::load` round-trip, so a View-menu
    // flip is still in effect after a relaunch. Each key is flipped away
    // from its in-code default and then read back through the normal load
    // path (strict-load, deep-merge, validation).
    let dir = tempdir().unwrap();
    let defaults = EditorConfig::default();
    // (key, value-to-write, default-it-must-differ-from) for every bool
    // toggle the View menu persists via `persist_view_setting`.
    let flips: &[(&str, bool, bool)] = &[
        ("editor.render_txt_as_markdown", false, defaults.render_txt_as_markdown),
        ("editor.live_preview", false, defaults.live_preview),
        ("editor.word_wrap", false, defaults.word_wrap),
        ("editor.show_line_numbers", false, defaults.show_line_numbers),
        ("editor.show_whitespace", true, defaults.show_whitespace),
        ("editor.highlight_trailing_whitespace", true, defaults.highlight_trailing_whitespace),
        ("editor.show_chunk_boundaries", true, defaults.show_chunk_boundaries),
        ("editor.hide_frontmatter", true, defaults.hide_frontmatter),
        ("editor.intraline_diff", true, defaults.intraline_diff),
        ("editor.show_minimap", false, defaults.show_minimap),
        ("editor.hide_scrollbar", true, defaults.hide_scrollbar),
    ];
    for (key, value, default) in flips {
        assert_ne!(value, default, "flip for {key} must differ from its default");
        Config::set(SettingsScope::Vault, key, &serde_json::Value::Bool(*value), dir.path())
            .unwrap_or_else(|e| panic!("set {key} failed: {e}"));
    }
    // Fresh load from disk — the relaunch path. None of the flips may
    // revert to their default.
    let cfg = Config::load(dir.path()).unwrap();
    let e = &cfg.editor;
    assert!(!e.render_txt_as_markdown);
    assert!(!e.live_preview);
    assert!(!e.word_wrap);
    assert!(!e.show_line_numbers);
    assert!(e.show_whitespace);
    assert!(e.highlight_trailing_whitespace);
    assert!(e.show_chunk_boundaries);
    assert!(e.hide_frontmatter);
    assert!(e.intraline_diff);
    assert!(!e.show_minimap);
    assert!(e.hide_scrollbar);
}

#[test]
fn op_log_section_round_trips() {
    // status: op-log-config-section
    // The whole `[op-log]` section must survive a serialize → parse round
    // trip with its renamed (`op-log`) table key. Mirrors `defaults_round_trip`.
    let cfg = Config::default();
    let s = toml::to_string_pretty(&cfg).unwrap();
    assert!(s.contains("[op-log]"), "section key not renamed: {s}");
    let back: Config = toml::from_str(&s).unwrap();
    assert_eq!(back.op_log.metadata_retention_days, 365);
    assert_eq!(back.op_log.rejected_retention_days, 14);
    assert!(!back.op_log.auto_reject_on_drift);
    assert!(back.op_log.review_required);
    assert!((back.op_log.compact_threshold - 4.0).abs() < f32::EPSILON);
}

#[test]
fn op_log_keys_persist_and_reload() {
    // status: op-log-config-section
    // Every write-back-eligible `[op-log]` key must survive a
    // `Config::set` → `Config::load` round-trip, flipped away from its
    // in-code default. Mirrors `editor_view_toggles_persist_and_reload`.
    let dir = tempdir().unwrap();
    Config::set(
        SettingsScope::Vault,
        "op-log.metadata_retention_days",
        &serde_json::json!(30),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "op-log.rejected_retention_days",
        &serde_json::json!(7),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "op-log.auto_reject_on_drift",
        &serde_json::Value::Bool(true),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "op-log.review_required",
        &serde_json::Value::Bool(false),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "op-log.compact_threshold",
        &serde_json::json!(8.0),
        dir.path(),
    )
    .unwrap();
    let cfg = Config::load(dir.path()).unwrap();
    assert_eq!(cfg.op_log.metadata_retention_days, 30);
    assert_eq!(cfg.op_log.rejected_retention_days, 7);
    assert!(cfg.op_log.auto_reject_on_drift);
    assert!(!cfg.op_log.review_required);
    assert!((cfg.op_log.compact_threshold - 8.0).abs() < f32::EPSILON);
}

#[test]
fn sync_section_round_trips_and_overrides() {
    // status: sync-config-section
    // Defaults apply when `[sync]` is absent; a full override block
    // (mode = "server", an enrolled device, enabled = true) parses into
    // the expected struct. Mirrors `op_log_section_round_trips`.

    // Absent section → documented defaults.
    let cfg: Config = toml::from_str("schema_version = 1\n").unwrap();
    assert!(!cfg.sync.enabled);
    assert_eq!(cfg.sync.mode, SyncMode::Peer);
    assert!(cfg.sync.server_url.is_empty());
    assert!(cfg.sync.discovery);
    assert!(cfg.sync.devices.is_empty());

    // Defaults survive a serialize → parse round trip.
    let s = toml::to_string_pretty(&Config::default()).unwrap();
    assert!(s.contains("[sync]"), "section missing: {s}");
    let back: Config = toml::from_str(&s).unwrap();
    assert!(!back.sync.enabled);
    assert_eq!(back.sync.mode, SyncMode::Peer);

    // Override block parses with snake_case mode + a device fingerprint.
    let over: Config = toml::from_str(
        r#"schema_version = 1
[sync]
enabled = true
mode = "server"
server_url = "/dns4/hub.example/tcp/4001"
discovery = false
devices = ["ABCDEFG-HIJKLMN"]
"#,
    )
    .unwrap();
    assert!(over.sync.enabled);
    assert_eq!(over.sync.mode, SyncMode::Server);
    assert_eq!(over.sync.server_url, "/dns4/hub.example/tcp/4001");
    assert!(!over.sync.discovery);
    assert_eq!(over.sync.devices, vec!["ABCDEFG-HIJKLMN".to_string()]);
}

#[test]
fn op_log_compact_threshold_rejects_below_one() {
    // status: op-log-config-section
    let dir = tempdir().unwrap();
    assert!(Config::set(
        SettingsScope::Vault,
        "op-log.compact_threshold",
        &serde_json::json!(0.5),
        dir.path(),
    )
    .is_err());
}

#[test]
fn write_back_sync_mode_validates_allowed_values() {
    // status: sync-config-section
    // Mirrors the `vault.tree.sort_by` enum-as-string precedent: a member
    // of the allowed set (`peer`/`server`/`both`) is accepted and lands on
    // the typed field; anything else is rejected by the eligibility path.
    let dir = tempdir().unwrap();
    let cfg = Config::set(
        SettingsScope::Vault,
        "sync.mode",
        &serde_json::Value::String("server".into()),
        dir.path(),
    )
    .unwrap();
    assert_eq!(cfg.sync.mode, SyncMode::Server);

    let err = Config::set(
        SettingsScope::Vault,
        "sync.mode",
        &serde_json::Value::String("bogus".into()),
        dir.path(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("invalid value"));
}

#[test]
fn write_back_sync_keys_eligible_at_vault_scope() {
    // status: sync-config-section
    // The non-secret `[sync]` keys persist through the write-back path.
    let dir = tempdir().unwrap();
    Config::set(SettingsScope::Vault, "sync.enabled", &serde_json::Value::Bool(true), dir.path()).unwrap();
    Config::set(SettingsScope::Vault, "sync.discovery", &serde_json::Value::Bool(false), dir.path()).unwrap();
    Config::set(
        SettingsScope::Vault,
        "sync.server_url",
        &serde_json::Value::String("/dns4/hub.example/tcp/4001".into()),
        dir.path(),
    )
    .unwrap();
    let cfg = Config::set(
        SettingsScope::Vault,
        "sync.devices",
        &serde_json::json!(["ABCDEFG-HIJKLMN"]),
        dir.path(),
    )
    .unwrap();
    assert!(cfg.sync.enabled);
    assert!(!cfg.sync.discovery);
    assert_eq!(cfg.sync.server_url, "/dns4/hub.example/tcp/4001");
    assert_eq!(cfg.sync.devices, vec!["ABCDEFG-HIJKLMN".to_string()]);
}

#[test]
fn write_back_rejects_non_eligible_key() {
    let dir = tempdir().unwrap();
    let err = Config::set(
        SettingsScope::Vault,
        "editor.tab_size",
        &serde_json::Value::Number(4.into()),
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
        &serde_json::Value::String("yes please".into()),
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
        &serde_json::Value::String("mtime_desc".into()),
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
        &serde_json::json!(4096),
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
fn write_back_whole_number_float_persists_as_float() {
    // Regression: a settings UI commit of `scroll_speed = 3.0` arrives
    // here as a Number stored as f64. `as_i64()` would happily coerce
    // it to `3` because it's whole-valued, but writing `editor.
    // scroll_speed = 3` to TOML then fails strict-load against the
    // `f32` field on the next reload and the setting reverts to its
    // default. The TOML must keep the `.0` so the round-trip is stable.
    let dir = tempdir().unwrap();
    let cfg = Config::set(
        SettingsScope::Vault,
        "editor.scroll_speed",
        &serde_json::json!(3.0),
        dir.path(),
    )
    .unwrap();
    assert!((cfg.editor.scroll_speed - 3.0).abs() < f32::EPSILON);
    let raw = fs::read_to_string(dir.path().join(".hiker").join("config.toml")).unwrap();
    assert!(
        raw.contains("scroll_speed = 3.0"),
        "expected `scroll_speed = 3.0` in TOML, got:\n{raw}"
    );
}

#[test]
fn write_back_positive_int_rejects_zero_and_floats() {
    let dir = tempdir().unwrap();
    // Zero is not a positive integer.
    assert!(Config::set(
        SettingsScope::Vault,
        "llm.agent.iteration_cap",
        &serde_json::json!(0),
        dir.path(),
    )
    .is_err());
    // Float is rejected even when its value is integer-equivalent;
    // serde_json carries the no-decimal vs. decimal distinction.
    assert!(Config::set(
        SettingsScope::Vault,
        "llm.agent.iteration_cap",
        &serde_json::json!(10.5),
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
        &serde_json::json!("sk-secret"),
        dir.path(),
    )
    .expect_err("vault scope must refuse api_key");
    let msg = err.to_string();
    assert!(
        msg.contains("api_key") && msg.contains("not user-mutable"),
        "got: {msg}",
    );
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
        &serde_json::json!("claude-haiku-4-5"),
        dir.path(),
    )
    .unwrap();
    assert_eq!(cfg.llm.provider.model, "claude-haiku-4-5");
}
