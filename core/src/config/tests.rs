use super::*;
use super::io::deep_merge;
use super::sections::TreeSortBy;
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
fn write_back_canvas_scroll_mode_validates_allowed_values() {
    // status: canvas-scroll-mode
    // `[ui] canvas_scroll_mode` is an enum-as-string (`auto`/`pan`/`zoom`): a
    // member of the allowed set lands on the typed field, anything else is
    // rejected by the eligibility path — the `sync.mode` precedent.
    let dir = tempdir().unwrap();
    let cfg = Config::set(
        SettingsScope::Vault,
        "ui.canvas_scroll_mode",
        &serde_json::Value::String("zoom".into()),
        dir.path(),
    )
    .unwrap();
    assert_eq!(cfg.ui.canvas_scroll_mode, CanvasScrollMode::Zoom);

    let err = Config::set(
        SettingsScope::Vault,
        "ui.canvas_scroll_mode",
        &serde_json::Value::String("spin".into()),
        dir.path(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("invalid value"));
}

#[test]
fn current_valid_config_round_trips_on_load() {
    // A current-schema vault config.toml with valid sections loads cleanly
    // and the values land in the merged Config. No compat shims: the loader
    // is strict (`deny_unknown_fields`) over the current schema only.
    let dir = tempdir().unwrap();
    let cfg_toml = r#"schema_version = 1
[editing]
review_required = false
[editor]
word_wrap = false
"#;
    let vault_path = dir.path().join(".hiker").join("config.toml");
    fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
    fs::write(&vault_path, cfg_toml).unwrap();
    let cfg = Config::load(dir.path()).expect("current-valid config must load");
    assert!(!cfg.editing.review_required);
    assert!(!cfg.editor.word_wrap);
}

#[test]
fn unknown_section_hard_fails() {
    // Strict load: an unknown top-level table (a typo, or a stale removed
    // section like `[sync]` / `[op-log]`) hard-fails. Pre-1.0, no compat
    // shims — a stale config.toml is intended to abort vault open loudly.
    let dir = tempdir().unwrap();
    let vault_path = dir.path().join(".hiker").join("config.toml");
    fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
    fs::write(&vault_path, "schema_version = 1\n[totally_bogus]\nx = 1\n").unwrap();
    assert!(Config::load(dir.path()).is_err());
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
fn editing_section_round_trips() {
    // status: op-log-config-section
    // The whole `[editing]` section must survive a serialize → parse round
    // trip with its renamed (`editing`) table key. Mirrors `defaults_round_trip`.
    let cfg = Config::default();
    let s = toml::to_string_pretty(&cfg).unwrap();
    assert!(s.contains("[editing]"), "section key not renamed: {s}");
    let back: Config = toml::from_str(&s).unwrap();
    assert_eq!(back.editing.metadata_retention_days, 365);
    assert_eq!(back.editing.rejected_retention_days, 14);
    assert!(!back.editing.auto_reject_on_drift);
    assert!(back.editing.review_required);
}

#[test]
fn editing_keys_persist_and_reload() {
    // status: op-log-config-section
    // Every write-back-eligible `[editing]` key must survive a
    // `Config::set` → `Config::load` round-trip, flipped away from its
    // in-code default. Mirrors `editor_view_toggles_persist_and_reload`.
    let dir = tempdir().unwrap();
    Config::set(
        SettingsScope::Vault,
        "editing.metadata_retention_days",
        &serde_json::json!(30),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "editing.rejected_retention_days",
        &serde_json::json!(7),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "editing.auto_reject_on_drift",
        &serde_json::Value::Bool(true),
        dir.path(),
    )
    .unwrap();
    Config::set(
        SettingsScope::Vault,
        "editing.review_required",
        &serde_json::Value::Bool(false),
        dir.path(),
    )
    .unwrap();
    let cfg = Config::load(dir.path()).unwrap();
    assert_eq!(cfg.editing.metadata_retention_days, 30);
    assert_eq!(cfg.editing.rejected_retention_days, 7);
    assert!(cfg.editing.auto_reject_on_drift);
    assert!(!cfg.editing.review_required);
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
        "editing.metadata_retention_days",
        &serde_json::json!(0),
        dir.path(),
    )
    .is_err());
    // Float is rejected even when its value is integer-equivalent;
    // serde_json carries the no-decimal vs. decimal distinction.
    assert!(Config::set(
        SettingsScope::Vault,
        "editing.metadata_retention_days",
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

#[test]
fn kinds_builtins_merge_under_user_and_vault() {
    // status: kind-builtin-pm-set
    // Built-ins are the lowest deep-merge layer (built-ins <- user <-
    // vault). A vault that redefines `kinds.story.fields` replaces the
    // list wholesale (arrays replace) while untouched entries keep their
    // built-in values; `enabled = false` disables an entry.
    let mut merged = crate::kinds::builtin_kinds_value();
    let vault: toml::Value = toml::from_str(
        r#"
[kinds.story]
shape = "leaf"
fields = [ { name = "urgency", type = "number" } ]

[kinds.plan]
enabled = false
"#,
    )
    .unwrap();
    deep_merge(&mut merged, vault);
    let cfg: Config = merged.try_into().unwrap();
    let reg = crate::kinds::Registry::compile(&cfg.kinds).unwrap();

    // Override replaced story's field list wholesale.
    let story = reg.get("story").unwrap();
    assert_eq!(story.fields.len(), 1);
    assert_eq!(story.fields[0].name, "urgency");
    // Untouched built-ins survive; the disabled one is gone.
    assert!(reg.get("task").is_some());
    assert!(reg.get("sprint").is_some());
    assert!(reg.get("epic").is_some());
    assert!(reg.get("plan").is_none());
}

#[test]
fn kinds_invalid_entry_fails_cross_field_naming_offender() {
    // status: kind-registry
    // The registry is strict-load: an invalid entry aborts the load with an
    // error naming the offending `[kinds.<name>]` table (the inbox-rules
    // posture). The TOML itself parses — the failure is the cross-field
    // validation hook.
    let cfg: Config = toml::from_str(
        "schema_version = 1\n[kinds.sprint]\nshape = \"board-like\"\nstates = [ { name = \"Todo\" } ]\n",
    )
    .unwrap();
    let err = validate_cross_field(&cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[kinds.sprint]"), "{msg}");
    assert!(msg.contains("category"), "{msg}");
}

#[test]
fn kinds_default_config_carries_no_entries() {
    // The built-ins are a merge layer, not defaults — so auto-created
    // config files never freeze them, and `Config::default()` stays empty.
    assert!(Config::default().kinds.is_empty());
    let s = toml::to_string_pretty(&Config::default()).unwrap();
    assert!(!s.contains("[kinds"), "defaults must not serialize kinds: {s}");
}

#[test]
fn rules_invalid_entry_fails_cross_field_naming_offender() {
    // status: rule-shape
    // The `[rules.<name>]` table is strict-load beside `[kinds.<name>]`:
    // the reserved `script` verb (and any other invalid entry) aborts the
    // load naming the offending rule.
    let cfg: Config = toml::from_str(
        "schema_version = 1\n[rules.bad]\non = \"note-created\"\ndo = [ { script = { src = \"x\" } } ]\n",
    )
    .unwrap();
    let err = validate_cross_field(&cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("[rules.bad]"), "{msg}");
    assert!(msg.contains("reserved"), "{msg}");
}

#[test]
fn rules_valid_entry_passes_cross_field() {
    // status: rule-shape
    let cfg: Config = toml::from_str(
        "schema_version = 1\n[rules.ok]\non = \"card-moved\"\ndo = [ { add_to_board = { board = \"boards/b.md\" } } ]\n",
    )
    .unwrap();
    assert!(validate_cross_field(&cfg).is_ok());
    assert_eq!(cfg.rules.len(), 1);
    // Defaults stay empty and never serialize a [rules] table.
    assert!(Config::default().rules.is_empty());
    let s = toml::to_string_pretty(&Config::default()).unwrap();
    assert!(!s.contains("[rules"), "defaults must not serialize rules: {s}");
}
