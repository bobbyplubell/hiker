//! Eligible-key allowlist + write-back patch helpers for `Config::set`.

use crate::error::HikerError;

use super::SettingsScope;

/// One node of the eligible-key set: dotted path + expected JSON-side type.
#[derive(Debug, Clone, Copy)]
pub(super) struct EligibleKey {
    /// Dotted path, e.g. `"editor.live_preview"`. The first component is
    /// the section, then leaf or sub-table.
    pub(super) path: &'static str,
    ty: ValueType,
}

#[derive(Debug, Clone, Copy)]
enum ValueType {
    Bool,
    String,
    StringArray,
    /// `name_asc | name_desc | mtime_desc | mtime_asc`.
    TreeSortBy,
    /// `files | clusters | trails`.
    SidebarMode,
    /// Floating-point fraction in `[0.0, 1.0]`.
    UnitFraction,
    /// Positive integer (fits in u32). Used for the LLM/agent knobs
    /// (`max_tokens`, `iteration_cap`, etc.) where 0 is meaningless.
    PositiveInt,
    /// Non-negative integer (fits in u32). 0 is meaningful — used for
    /// `vault.chat_input_height` (0 = auto-grow, >0 = user-set px).
    NonNegativeInt,
    /// `auto | internal | external` for `[tasks] worker_preference`.
    WorkerPreference,
    /// `0..=65535` — used for `[mcp] port`. Distinct from `PositiveInt`
    /// because port `0` means "ephemeral / OS-assigned" and is valid.
    Port,
    /// Floating-point in `[0.0, 0.95]` — `[search.semantic] min_similarity`.
    SemanticMinSim,
    /// `5..=100` — `[search.semantic] top_k`.
    SemanticTopK,
    /// `off | mild | strong` for `[search.semantic] recency_bias`.
    RecencyBias,
    /// `all | lazy` for `[indexing] id_stamping`.
    IdStamping,
    /// One of the supported fastembed model ids — `bge-small-en-v1.5` /
    /// `bge-m3` / `embedding-gemma-300m`. Used by `[indexing] model`.
    /// status: embedder-model-selectable
    EmbedderModel,
    /// `#RRGGBB` or `#RRGGBBAA` hex color.
    HexColor,
    /// Minimap strip width in pixels: `16..=300`.
    MinimapWidth,
    /// Per-side bar padding: `0..=24`.
    MinimapPad,
    /// Bar corner radius: `0..=6`.
    MinimapRadius,
    /// Minimum bar width: `1..=12`.
    MinimapMinBarWidth,
    /// Bar vertical gap (tenths of a pixel): `0..=20`.
    MinimapBarGap,
}

const ELIGIBLE_VAULT: &[EligibleKey] = &[
    EligibleKey { path: "editor.render_txt_as_markdown", ty: ValueType::Bool },
    EligibleKey { path: "editor.live_preview",           ty: ValueType::Bool },
    EligibleKey { path: "editor.word_wrap",              ty: ValueType::Bool },
    EligibleKey { path: "editor.show_line_numbers",      ty: ValueType::Bool },
    EligibleKey { path: "editor.show_whitespace",        ty: ValueType::Bool },
    EligibleKey { path: "editor.highlight_trailing_whitespace", ty: ValueType::Bool },
    EligibleKey { path: "editor.show_chunk_boundaries",  ty: ValueType::Bool },
    EligibleKey { path: "editor.hide_frontmatter",       ty: ValueType::Bool },
    EligibleKey { path: "editor.intraline_diff",         ty: ValueType::Bool },
    EligibleKey { path: "editor.show_minimap",           ty: ValueType::Bool },
    EligibleKey { path: "editor.minimap.width",                ty: ValueType::MinimapWidth },
    EligibleKey { path: "editor.minimap.bar_padding_left",     ty: ValueType::MinimapPad },
    EligibleKey { path: "editor.minimap.bar_padding_right",    ty: ValueType::MinimapPad },
    EligibleKey { path: "editor.minimap.bar_corner_radius",    ty: ValueType::MinimapRadius },
    EligibleKey { path: "editor.minimap.min_bar_width",        ty: ValueType::MinimapMinBarWidth },
    EligibleKey { path: "editor.minimap.bar_gap_tenths",       ty: ValueType::MinimapBarGap },
    EligibleKey { path: "editor.minimap.colored",              ty: ValueType::Bool },
    EligibleKey { path: "editor.minimap.show_section_rules",   ty: ValueType::Bool },
    EligibleKey { path: "editor.minimap.show_viewport",        ty: ValueType::Bool },
    EligibleKey { path: "editor.minimap.show_left_edge",       ty: ValueType::Bool },
    EligibleKey { path: "editor.minimap.color_heading",        ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_code",           ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_emphasis",       ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_quote",          ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_plain",          ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_background",     ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_section_rule",   ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_viewport",       ty: ValueType::HexColor },
    EligibleKey { path: "editor.minimap.color_viewport_hover", ty: ValueType::HexColor },
    EligibleKey { path: "editor.font_system",            ty: ValueType::String },
    EligibleKey { path: "editor.font_editor",            ty: ValueType::String },
    EligibleKey { path: "editor.font_code",              ty: ValueType::String },
    EligibleKey { path: "vault.sidebar_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.related_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.trash_expanded",          ty: ValueType::Bool },
    EligibleKey { path: "vault.chat_height",             ty: ValueType::UnitFraction },
    EligibleKey { path: "vault.chat_input_height",       ty: ValueType::NonNegativeInt },
    EligibleKey { path: "vault.sidebar_width",           ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.discovery_width",         ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.show_sessions_in_tree",   ty: ValueType::Bool },
    EligibleKey { path: "vault.sidebar_mode",            ty: ValueType::SidebarMode },
    // status: active-trail-state
    EligibleKey { path: "vault.active_trail",            ty: ValueType::String },
    EligibleKey { path: "vault.tree.sort_by",            ty: ValueType::TreeSortBy },
    // status: trails-default-location
    EligibleKey { path: "trails.new_trail_dir",          ty: ValueType::String },
    // status: note-id-stamping
    EligibleKey { path: "indexing.id_stamping",          ty: ValueType::IdStamping },
    // status: embedder-model-selectable
    // status: settings-embedder-model-change-warning
    // Gated in the UI by a confirm modal (`settings-embedder-model-change-warning`)
    // because a flip re-embeds every note in the vault and — when the dim
    // differs — rebuilds the vec0 table via `store-rebuild-chunk-vecs-on-dim-change`.
    EligibleKey { path: "indexing.model",                ty: ValueType::EmbedderModel },
    EligibleKey { path: "search.modes.semantic",         ty: ValueType::Bool },
    EligibleKey { path: "search.modes.lexical",          ty: ValueType::Bool },
    EligibleKey { path: "search.sections.results_expanded", ty: ValueType::Bool },
    EligibleKey { path: "search.sections.related_expanded", ty: ValueType::Bool },
    // status: search-lexical-options, search-semantic-options
    EligibleKey { path: "search.lexical.case_sensitive",     ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.diacritic_sensitive",ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.prefix_match",       ty: ValueType::Bool },
    EligibleKey { path: "search.lexical.phrase_mode",        ty: ValueType::Bool },
    EligibleKey { path: "search.semantic.min_similarity",    ty: ValueType::SemanticMinSim },
    EligibleKey { path: "search.semantic.top_k",             ty: ValueType::SemanticTopK },
    EligibleKey { path: "search.semantic.recency_bias",      ty: ValueType::RecencyBias },
    // LLM section. Per-vault override (provider key / model / cap can
    // be tuned per workspace) shares the same eligibility set as user
    // scope so the per-section [User]/[Vault] toggle in the settings
    // pane can write either side.
    EligibleKey { path: "llm.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "llm.provider.backend",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.model",               ty: ValueType::String },
    EligibleKey { path: "llm.provider.api_key_env",         ty: ValueType::String },
    // `llm.provider.api_key` deliberately omitted from the vault list:
    // the literal key must never travel with a synced vault TOML. See
    // `llm.md` §`[llm-providers-config]` and `ELIGIBLE_USER` below.
    EligibleKey { path: "llm.provider.base_url",            ty: ValueType::String },
    EligibleKey { path: "llm.limits.max_tokens",            ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.limits.timeout_secs",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.iteration_cap",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.tool_timeout_secs",      ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.audit.log_full_prompt",        ty: ValueType::Bool },
    // [tasks] section. Per-vault override: every key is eligible at
    // vault scope per `task-queue-settings-section`.
    EligibleKey { path: "tasks.worker_preference",          ty: ValueType::WorkerPreference },
    EligibleKey { path: "tasks.terminal_retention_secs",    ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.direct_worker.enabled",      ty: ValueType::Bool },
    EligibleKey { path: "tasks.direct_worker.parallelism",  ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.expose_to_chat_agent",       ty: ValueType::Bool },
    EligibleKey { path: "tasks.lease.default_secs",         ty: ValueType::PositiveInt },
    EligibleKey { path: "tasks.lease.max_secs",             ty: ValueType::PositiveInt },
    // status: mcp-settings-ui-section
    // [mcp] section. Vault-scope by default — the discovery file lives
    // in the vault, so per-vault overrides are the natural shape.
    EligibleKey { path: "mcp.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "mcp.host",                         ty: ValueType::String },
    EligibleKey { path: "mcp.port",                         ty: ValueType::Port },
    EligibleKey { path: "mcp.max_top_k",                    ty: ValueType::PositiveInt },
    // [mcp.tools] — master gates + per-tool toggles
    // (status: mcp-tool-toggles).
    EligibleKey { path: "mcp.tools.writes_enabled",         ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.allow_redacted_lookup",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.search_notes_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.get_note_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.related_notes_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.write_note_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.edit_note_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.set_frontmatter_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.apply_tag_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.remove_tag_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_checkout_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_submit_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_fail_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_heartbeat_enabled", ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_list_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.review_required",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.audit.log_full_input",         ty: ValueType::Bool },
    // status: agent-write-review-mode
    EligibleKey { path: "llm.background.review_required",   ty: ValueType::Bool },
    // ACP section. The agent can be overridden per vault.
    // Also eligible at user scope for a global default.
    EligibleKey { path: "acp.command",                      ty: ValueType::String },
    // status: staging-config-section
    EligibleKey { path: "staging.auto_reject_on_conflict",  ty: ValueType::Bool },
    EligibleKey { path: "staging.retention_days",           ty: ValueType::PositiveInt },
    // status: triage-review-required
    EligibleKey { path: "suggestions.triage.review_required", ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.scope",           ty: ValueType::String },
    EligibleKey { path: "suggestions.triage.scheduled_rerun", ty: ValueType::String },
    // status: cluster-editor-triage-modified-rerun
    EligibleKey { path: "suggestions.triage.modified_rerun",  ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.modified_rerun_cosine_guard", ty: ValueType::UnitFraction },
];

const ELIGIBLE_USER: &[EligibleKey] = &[
    EligibleKey { path: "vault.recent",  ty: ValueType::StringArray },
    EligibleKey { path: "vault.default", ty: ValueType::String },
    // status: embedder-model-selectable
    // Also eligible at user scope as a global default; per-vault override
    // wins per the standard merge rule.
    EligibleKey { path: "indexing.model", ty: ValueType::EmbedderModel },
    // LLM section. Default scope for the settings pane is `user` so
    // API-key env name + provider live in the platform config dir; the
    // vault TOML can still override per-workspace via the eligible-vault
    // duplicates above.
    EligibleKey { path: "llm.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "llm.provider.backend",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.model",               ty: ValueType::String },
    EligibleKey { path: "llm.provider.api_key_env",         ty: ValueType::String },
    // `api_key` (literal) is user-scope only — see the spec posture in
    // `llm.md`. The vault eligibility list above intentionally omits it
    // so a `set_setting(Vault, "llm.provider.api_key", ...)` call is
    // rejected with the standard "not user-mutable in v1" error.
    EligibleKey { path: "llm.provider.api_key",             ty: ValueType::String },
    EligibleKey { path: "llm.provider.base_url",            ty: ValueType::String },
    EligibleKey { path: "llm.limits.max_tokens",            ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.limits.timeout_secs",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.iteration_cap",          ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.agent.tool_timeout_secs",      ty: ValueType::PositiveInt },
    EligibleKey { path: "llm.audit.log_full_prompt",        ty: ValueType::Bool },
    // worker_preference is also valid at user scope (per `task-queue.md`'s
    // settings eligibility note); the rest of `[tasks]` is vault-only.
    EligibleKey { path: "tasks.worker_preference",          ty: ValueType::WorkerPreference },
    // [mcp] — user scope is supported as a global default (the vault
    // table above wins per `core::config` merge order).
    EligibleKey { path: "mcp.enabled",                      ty: ValueType::Bool },
    EligibleKey { path: "mcp.host",                         ty: ValueType::String },
    EligibleKey { path: "mcp.port",                         ty: ValueType::Port },
    EligibleKey { path: "mcp.max_top_k",                    ty: ValueType::PositiveInt },
    EligibleKey { path: "mcp.tools.writes_enabled",         ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.allow_redacted_lookup",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.search_notes_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.get_note_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.related_notes_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.write_note_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.edit_note_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.set_frontmatter_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.apply_tag_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.remove_tag_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_checkout_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_submit_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_fail_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_heartbeat_enabled", ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.task_list_enabled",      ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.review_required",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.audit.log_full_input",         ty: ValueType::Bool },
    // status: agent-write-review-mode
    EligibleKey { path: "llm.background.review_required",   ty: ValueType::Bool },
    // ACP section. Also eligible at user scope for a global default.
    EligibleKey { path: "acp.command",                      ty: ValueType::String },
    // status: staging-config-section
    EligibleKey { path: "staging.auto_reject_on_conflict",  ty: ValueType::Bool },
    EligibleKey { path: "staging.retention_days",           ty: ValueType::PositiveInt },
    // status: triage-review-required
    EligibleKey { path: "suggestions.triage.review_required", ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.scope",           ty: ValueType::String },
    EligibleKey { path: "suggestions.triage.scheduled_rerun", ty: ValueType::String },
    // status: cluster-editor-triage-modified-rerun
    EligibleKey { path: "suggestions.triage.modified_rerun",  ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.modified_rerun_cosine_guard", ty: ValueType::UnitFraction },
];

fn is_hex_color(s: &str) -> bool {
    let bytes = s.as_bytes();
    if !matches!(bytes.first(), Some(b'#')) {
        return false;
    }
    let hex = &bytes[1..];
    if !(hex.len() == 6 || hex.len() == 8) {
        return false;
    }
    hex.iter().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn eligible_key(scope: SettingsScope, key: &str) -> Result<EligibleKey, HikerError> {
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

pub(super) fn validate_value(key: &EligibleKey, value: &serde_json::Value) -> Result<(), HikerError> {
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
        (ValueType::SidebarMode, J::String(s)) => {
            matches!(s.as_str(), "files" | "clusters" | "trails")
        }
        (ValueType::UnitFraction, J::Number(n)) => n
            .as_f64()
            .map(|f| (0.0..=1.0).contains(&f))
            .unwrap_or(false),
        // Positive integer that fits u32. JSON.stringify on a JS
        // number-without-fraction parses back as an integer, so
        // `as_u64` returns Some only for true integer values; floats
        // are rejected, which is what we want for `max_tokens` etc.
        (ValueType::PositiveInt, J::Number(n)) => n
            .as_u64()
            .map(|u| u >= 1 && u <= u32::MAX as u64)
            .unwrap_or(false),
        (ValueType::NonNegativeInt, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= u32::MAX as u64)
            .unwrap_or(false),
        (ValueType::WorkerPreference, J::String(s)) => {
            matches!(s.as_str(), "auto" | "internal" | "external")
        }
        (ValueType::Port, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= u16::MAX as u64)
            .unwrap_or(false),
        (ValueType::SemanticMinSim, J::Number(n)) => n
            .as_f64()
            .map(|f| (0.0..=0.95).contains(&f))
            .unwrap_or(false),
        (ValueType::SemanticTopK, J::Number(n)) => n
            .as_u64()
            .map(|u| (5..=100).contains(&u))
            .unwrap_or(false),
        (ValueType::RecencyBias, J::String(s)) => {
            matches!(s.as_str(), "off" | "mild" | "strong")
        }
        (ValueType::IdStamping, J::String(s)) => {
            matches!(s.as_str(), "all" | "lazy")
        }
        (ValueType::EmbedderModel, J::String(s)) => crate::embed::is_known_model(s),
        (ValueType::HexColor, J::String(s)) => is_hex_color(s),
        (ValueType::MinimapWidth, J::Number(n)) => n
            .as_u64()
            .map(|u| (16..=300).contains(&u))
            .unwrap_or(false),
        (ValueType::MinimapPad, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= 24)
            .unwrap_or(false),
        (ValueType::MinimapRadius, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= 6)
            .unwrap_or(false),
        (ValueType::MinimapMinBarWidth, J::Number(n)) => n
            .as_u64()
            .map(|u| (1..=12).contains(&u))
            .unwrap_or(false),
        (ValueType::MinimapBarGap, J::Number(n)) => n
            .as_u64()
            .map(|u| u <= 20)
            .unwrap_or(false),
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
pub(super) fn apply_patch(doc: &mut toml_edit::DocumentMut, key: &str, value: &serde_json::Value) {
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
        J::Number(n) => {
            // Try integer before float — `serde_json::Number::as_f64`
            // succeeds for both shapes, and routing every integer
            // through the float branch would write `4096.0` to TOML
            // and then fail strict-load against `u32` fields. JSON
            // produced by JS for an integer (no decimal point) parses
            // here with `as_i64() = Some(_)`, so this branch wins for
            // PositiveInt rows; floats (e.g. `vault.chat_height = 0.3`)
            // fall through to the float branch as before.
            if let Some(i) = n.as_i64() {
                toml_edit::value(i)
            } else if let Some(f) = n.as_f64() {
                toml_edit::value(f)
            } else {
                toml_edit::Item::None
            }
        }
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
        J::Object(_) => {
            // validate_value rejects this for our eligible-key set, so this
            // branch is unreachable in practice. Falling back to None keeps
            // the function total without panicking.
            toml_edit::Item::None
        }
    }
}
