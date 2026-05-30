//! Eligible-key allowlist + write-back patch helpers for `Config::set`.

/// One node of the eligible-key set: dotted path + expected JSON-side type.
#[derive(Debug, Clone, Copy)]
pub(super) struct EligibleKey {
    /// Dotted path, e.g. `"editor.live_preview"`. The first component is
    /// the section, then leaf or sub-table.
    pub(super) path: &'static str,
    pub(super) ty: ValueType,
}

impl EligibleKey {
    pub(super) const fn ty(&self) -> ValueType {
        self.ty
    }

    /// Type-check a candidate JSON value against this key's `ValueType`,
    /// applying the same range/enum constraints the UI controls enforce.
    /// Returns `true` when the value is a legal write for this key.
    pub(super) fn validate(&self, value: &serde_json::Value) -> bool {
        use serde_json::Value as J;
        match (self.ty(), value) {
            (ValueType::Bool, J::Bool(_)) => true,
            (ValueType::String, J::String(_)) => true,
            (ValueType::String, J::Null) => true,
            (ValueType::StringArray, J::Array(arr)) => arr.iter().all(serde_json::Value::is_string),
            (ValueType::TreeSortBy, J::String(s)) => matches!(
                s.as_str(),
                "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc"
            ),
            (ValueType::MinimapStyle, J::String(s)) => {
                matches!(s.as_str(), "glyphs" | "bars")
            }
            (ValueType::UnitFraction, J::Number(n)) => n
                .as_f64()
                .map(|f| (0.0..=1.0).contains(&f))
                .unwrap_or(false),
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
            (ValueType::EmbedderModel, J::String(s)) => crate::embed::is_known_model(s),
            (ValueType::HexColor, J::String(s)) => {
                let bytes = s.as_bytes();
                matches!(bytes.first(), Some(b'#'))
                    && {
                        let hex = &bytes[1..];
                        (hex.len() == 6 || hex.len() == 8)
                            && hex.iter().all(u8::is_ascii_hexdigit)
                    }
            }
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
            (ValueType::ScrollSpeed, J::Number(n)) => n
                .as_f64()
                .map(|f| (0.25..=10.0).contains(&f))
                .unwrap_or(false),
            (ValueType::CompactThreshold, J::Number(n)) => n
                .as_f64()
                .map(|f| (1.0..=64.0).contains(&f))
                .unwrap_or(false),
            (ValueType::SyncMode, J::String(s)) => {
                matches!(s.as_str(), "peer" | "server" | "both")
            }
            _ => false,
        }
    }
}

/// In-place writer for a single dotted-key leaf in a parsed TOML document.
/// Owns the borrowed `DocumentMut` so `Config::set` can hand off the
/// JSON→TOML conversion and table-walking as method calls (which keeps
/// `set` small and the helpers cohesive in this module).
pub(super) struct Patcher<'a> {
    pub(super) doc: &'a mut toml_edit::DocumentMut,
}

impl Patcher<'_> {
    /// Ensure `slot` is a `Table` then insert `(name, item)` into it,
    /// replacing a non-table slot with a fresh table first.
    fn insert_into_table(slot: &mut toml_edit::Item, name: &str, item: toml_edit::Item) {
        match slot {
            toml_edit::Item::Table(t) => {
                t.insert(name, item);
            }
            _ => {
                *slot = toml_edit::Item::Table(toml_edit::Table::new());
                if let toml_edit::Item::Table(t) = slot {
                    t.insert(name, item);
                }
            }
        }
    }

    /// Write `value` at dotted `key`, creating intermediate tables as
    /// needed so the leaf lands in the right `[section.sub]` table.
    pub(super) fn set(&mut self, key: &str, value: &serde_json::Value) {
        use serde_json::Value as J;
        let item = match value {
            J::Bool(b) => toml_edit::value(*b),
            J::String(s) => toml_edit::value(s.as_str()),
            J::Number(n) => {
                if n.is_f64() {
                    if let Some(f) = n.as_f64() {
                        toml_edit::value(f)
                    } else {
                        toml_edit::Item::None
                    }
                } else if let Some(i) = n.as_i64() {
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
            J::Object(_) => toml_edit::Item::None,
        };
        let parts: Vec<&str> = key.split('.').collect();
        let mut cursor: &mut toml_edit::Item = self.doc.as_item_mut();
        for part in &parts[..parts.len() - 1] {
            let needs_replace = !matches!(cursor.get(part), Some(toml_edit::Item::Table(_)));
            if needs_replace {
                Self::insert_into_table(
                    cursor,
                    part,
                    toml_edit::Item::Table(toml_edit::Table::new()),
                );
            }
            cursor = cursor
                .get_mut(part)
                .expect("intermediate slot was just ensured to be a Table");
        }
        let leaf = parts[parts.len() - 1];
        Self::insert_into_table(cursor, leaf, item);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ValueType {
    Bool,
    String,
    StringArray,
    /// `name_asc | name_desc | mtime_desc | mtime_asc`.
    TreeSortBy,
    /// `glyphs | bars` for `[editor.minimap] style`.
    MinimapStyle,
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
    /// Editor scroll-wheel multiplier: `0.25..=10.0`.
    ScrollSpeed,
    /// `[op-log] compact_threshold` — Yrs-snapshot size multiple over the
    /// materialized content size that triggers compaction. `1.0..=64.0`
    /// (a multiple below 1.0 would compact constantly).
    CompactThreshold,
    /// `peer | server | both` for `[sync] mode`. status: sync-config-section.
    SyncMode,
}

pub(super) const ELIGIBLE_VAULT: &[EligibleKey] = &[
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
    EligibleKey { path: "editor.hide_scrollbar",         ty: ValueType::Bool },
    EligibleKey { path: "editor.scroll_speed",           ty: ValueType::ScrollSpeed },
    EligibleKey { path: "editor.minimap.style",                ty: ValueType::MinimapStyle },
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
    EligibleKey { path: "editor.double_click_pattern",   ty: ValueType::String },
    EligibleKey { path: "editor.triple_click_pattern",   ty: ValueType::String },
    EligibleKey { path: "ui.custom_titlebar",            ty: ValueType::Bool },
    EligibleKey { path: "vault.sidebar_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.related_open",            ty: ValueType::Bool },
    EligibleKey { path: "vault.trash_expanded",          ty: ValueType::Bool },
    EligibleKey { path: "vault.chat_height",             ty: ValueType::UnitFraction },
    EligibleKey { path: "vault.chat_input_height",       ty: ValueType::NonNegativeInt },
    EligibleKey { path: "vault.sidebar_width",           ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.discovery_width",         ty: ValueType::PositiveInt },
    EligibleKey { path: "vault.show_sessions_in_tree",   ty: ValueType::Bool },
    // status: active-trail-state
    EligibleKey { path: "vault.active_trail",            ty: ValueType::String },
    EligibleKey { path: "vault.tree.sort_by",            ty: ValueType::TreeSortBy },
    // status: trails-default-location
    EligibleKey { path: "trails.new_trail_dir",          ty: ValueType::String },
    // status: board-default-location
    EligibleKey { path: "boards.new_board_dir",          ty: ValueType::String },
    // status: trail-draft-from-clustering
    EligibleKey { path: "clustering.propose_trails",     ty: ValueType::Bool },
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
    // status: board-mcp-tools
    EligibleKey { path: "mcp.tools.boards_list_enabled",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_get_enabled",          ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_card_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_create_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_text_card_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_move_card_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_set_card_text_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_remove_card_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_column_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_rename_column_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_reorder_column_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_delete_column_enabled",ty: ValueType::Bool },
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
    // status: op-log-config-section
    EligibleKey { path: "op-log.metadata_retention_days",   ty: ValueType::PositiveInt },
    EligibleKey { path: "op-log.rejected_retention_days",   ty: ValueType::PositiveInt },
    EligibleKey { path: "op-log.auto_reject_on_drift",      ty: ValueType::Bool },
    EligibleKey { path: "op-log.review_required",           ty: ValueType::Bool },
    EligibleKey { path: "op-log.compact_threshold",         ty: ValueType::CompactThreshold },
    // status: sync-config-section
    // [sync] is per-vault — the config lives in the vault TOML per
    // docs/sync.md §`[sync]` config section. Secrets (the per-vault
    // content key + per-device private key) are user-scope and never
    // appear here — the same posture as `[llm].api_key`.
    EligibleKey { path: "sync.enabled",                     ty: ValueType::Bool },
    EligibleKey { path: "sync.mode",                        ty: ValueType::SyncMode },
    EligibleKey { path: "sync.server_url",                  ty: ValueType::String },
    EligibleKey { path: "sync.discovery",                   ty: ValueType::Bool },
    EligibleKey { path: "sync.devices",                     ty: ValueType::StringArray },
    // status: sync-device-name
    // THIS device's self-set human name (vault scope, carried on the sync
    // handshake). The learned `device_names` peer map is populated at runtime
    // from handshakes (seeded from config), not user-set, so it is not an
    // eligible write key.
    EligibleKey { path: "sync.device_name",                 ty: ValueType::String },
    // status: triage-review-required
    EligibleKey { path: "suggestions.triage.review_required", ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.scope",           ty: ValueType::String },
    EligibleKey { path: "suggestions.triage.scheduled_rerun", ty: ValueType::String },
    // status: cluster-editor-triage-modified-rerun
    EligibleKey { path: "suggestions.triage.modified_rerun",  ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.modified_rerun_cosine_guard", ty: ValueType::UnitFraction },
];

pub(super) const ELIGIBLE_USER: &[EligibleKey] = &[
    EligibleKey { path: "vault.recent",  ty: ValueType::StringArray },
    EligibleKey { path: "vault.default", ty: ValueType::String },
    // [ui] chrome toggle — global app preference, eligible at user scope.
    EligibleKey { path: "ui.custom_titlebar",               ty: ValueType::Bool },
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
    // status: board-mcp-tools
    EligibleKey { path: "mcp.tools.boards_list_enabled",        ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_get_enabled",          ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_card_enabled",     ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_create_enabled",       ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_text_card_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_move_card_enabled",    ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_set_card_text_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_remove_card_enabled",  ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_add_column_enabled",   ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_rename_column_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_reorder_column_enabled",ty: ValueType::Bool },
    EligibleKey { path: "mcp.tools.board_delete_column_enabled",ty: ValueType::Bool },
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
    // status: op-log-config-section
    EligibleKey { path: "op-log.metadata_retention_days",   ty: ValueType::PositiveInt },
    EligibleKey { path: "op-log.rejected_retention_days",   ty: ValueType::PositiveInt },
    EligibleKey { path: "op-log.auto_reject_on_drift",      ty: ValueType::Bool },
    EligibleKey { path: "op-log.review_required",           ty: ValueType::Bool },
    EligibleKey { path: "op-log.compact_threshold",         ty: ValueType::CompactThreshold },
    // status: triage-review-required
    EligibleKey { path: "suggestions.triage.review_required", ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.scope",           ty: ValueType::String },
    EligibleKey { path: "suggestions.triage.scheduled_rerun", ty: ValueType::String },
    // status: cluster-editor-triage-modified-rerun
    EligibleKey { path: "suggestions.triage.modified_rerun",  ty: ValueType::Bool },
    EligibleKey { path: "suggestions.triage.modified_rerun_cosine_guard", ty: ValueType::UnitFraction },
];

#[cfg(test)]
mod patch_tests {
    use super::*;
    use serde_json::json;

    /// `editor.minimap.style` is validated against the `MinimapStyle`
    /// allow-set (`glyphs` / `bars`), not as a free `String` — so a bogus
    /// value is rejected at `set` time instead of being persisted and then
    /// aborting the next strict-load. (bug-config-documented-sections-abort-strict-load)
    #[test]
    fn minimap_style_only_accepts_known_variants() {
        let key = ELIGIBLE_VAULT
            .iter()
            .find(|k| k.path == "editor.minimap.style")
            .expect("editor.minimap.style is eligible");
        assert!(matches!(key.ty(), ValueType::MinimapStyle));
        assert!(key.validate(&json!("glyphs")));
        assert!(key.validate(&json!("bars")));
        assert!(!key.validate(&json!("banana")));
        assert!(!key.validate(&json!("Glyphs")));
        assert!(!key.validate(&json!(true)));
    }

    /// `ui.custom_titlebar` is an eligible bool key at both scopes so the
    /// documented `[ui]` toggle is user-settable (and round-trips through
    /// strict-load). (bug-config-documented-sections-abort-strict-load)
    #[test]
    fn ui_custom_titlebar_is_eligible_bool_both_scopes() {
        for table in [ELIGIBLE_VAULT, ELIGIBLE_USER] {
            let key = table
                .iter()
                .find(|k| k.path == "ui.custom_titlebar")
                .expect("ui.custom_titlebar is eligible");
            assert!(matches!(key.ty(), ValueType::Bool));
            assert!(key.validate(&json!(false)));
            assert!(!key.validate(&json!("nope")));
        }
    }
}
