// status: settings-pane-mode
// status: settings-pane-section-list
// status: settings-pane-eligible-key-controls
// status: settings-pane-readonly-display
// status: settings-pane-scope-toggle
// status: settings-pane-reset-row
// status: settings-pane-open-toml-link
// status: settings-pane-deferred-sections-stub
// status: settings-pane-manual-refresh
//
// Settings pane sub-mode of the editor pane. Mirrors `vault-home-screen`'s
// shape: `#editor-pane` gets a `settings-view` class, the CM6 view + home
// pane hide via CSS, the settings DOM is rebuilt from the loaded `Config`
// every time it opens.
//
// Settings live on disk in two TOMLs (per-user + per-vault, see
// `core::config`). This pane is a UI on top of `set_setting` /
// `Config::load` — never a parallel storage path. Eligible keys (the closed
// `ELIGIBLE_*` sets in `core::config`) get interactive controls; every
// other key is read-only with an info popover that opens its file in the
// system file manager via `reveal_config_file`.

import { Ipc } from "./ipc";

// Mirror of `core::config::SettingsScope`. Snake-case for serde.
export type SettingsScope = "user" | "vault";

// Mirrors core::config::Config (subset the pane reads). Kept loose because
// any field-level type strictness lives on the Rust side; the pane only
// renders rows for the keys it knows about.
export interface SettingsConfig {
  schema_version: number;
  editor: {
    render_txt_as_markdown: boolean;
    live_preview: boolean;
    word_wrap: boolean;
    show_line_numbers: boolean;
    show_whitespace: boolean;
    show_chunk_boundaries: boolean;
    hide_frontmatter: boolean;
    intraline_diff: boolean;
    tab_size: number;
  };
  indexing: {
    model: string;
    batch_size: number;
    ignored_paths: string[];
  };
  vault: {
    recent: string[];
    default: string | null;
    sidebar_open: boolean;
    related_open: boolean;
    trash_expanded: boolean;
    chat_height: number;
    chat_input_height: number;
    sidebar_width: number;
    discovery_width: number;
    show_sessions_in_tree: boolean;
    sidebar_mode: "files" | "clusters" | "trails";
    /// status: active-trail-state
    /// Vault-relative path of the active trail-doc, or `null` when none
    /// is active. Persisted via `set_setting` / `trail_set_active` (the
    /// latter is the canonical writer because it also stamps
    /// `hiker.last_activated_at` on the trail-doc).
    active_trail: string | null;
    tree: { sort_by: "name_asc" | "name_desc" | "mtime_desc" | "mtime_asc" };
  };
  search: {
    modes: { semantic: boolean; lexical: boolean };
    sections: { results_expanded: boolean; related_expanded: boolean };
    lexical: {
      case_sensitive: boolean;
      diacritic_sensitive: boolean;
      prefix_match: boolean;
      phrase_mode: boolean;
    };
    semantic: {
      min_similarity: number;
      top_k: number;
      recency_bias: "off" | "mild" | "strong";
    };
  };
  // Loosely shaped — the pane only inspects keys via dotted-path lookup,
  // and the deferred sections (llm, mcp) don't have rendered rows yet.
  // Carrying the full Rust shape here would add a maintenance ratchet
  // every time core::config grows a key the pane doesn't display.
  llm: {
    enabled: boolean;
    provider: {
      backend: string;
      model: string;
      api_key_env: string;
      api_key: string;
      base_url: string;
    };
    limits: { max_tokens: number; timeout_secs: number };
    agent: { iteration_cap: number; tool_timeout_secs: number };
    audit: { log_full_prompt: boolean };
  };
  mcp: unknown;
  acp: {
    command: string;
  };
}

// Mirror of `Config::default()` from `core::config`. Used by the per-row
// reset affordance. Kept in sync by hand — there's no Rust → TS export for
// "the in-code defaults" today, and the v1 schema is small enough that
// duplication beats a Tauri command per render.
const DEFAULTS = {
  editor: {
    render_txt_as_markdown: true,
    live_preview: true,
    word_wrap: true,
    show_line_numbers: true,
    show_whitespace: false,
    show_chunk_boundaries: false,
    hide_frontmatter: false,
    intraline_diff: false,
    tab_size: 2,
  },
  vault: {
    sidebar_open: true,
    related_open: false,
    trash_expanded: false,
    chat_height: 0.30,
    chat_input_height: 0,
    sidebar_width: 280,
    discovery_width: 320,
    show_sessions_in_tree: false,
    sidebar_mode: "files" as const,
    active_trail: null as string | null,
    tree: { sort_by: "name_asc" as const },
  },
  search: {
    modes: { semantic: true, lexical: true },
    sections: { results_expanded: true, related_expanded: true },
    lexical: {
      case_sensitive: false,
      diacritic_sensitive: false,
      prefix_match: false,
      phrase_mode: false,
    },
    semantic: {
      min_similarity: 0.0,
      top_k: 25,
      recency_bias: "off" as const,
    },
  },
  // Mirror of `LlmConfig::default()` (and its sub-tables) in
  // core/src/config.rs. The settings pane's reset affordance reads
  // these directly; keep in sync when the Rust defaults change.
  llm: {
    enabled: true,
    provider: {
      backend: "anthropic",
      model: "claude-sonnet-4-7",
      api_key_env: "",
      api_key: "",
      base_url: "",
    },
    limits: { max_tokens: 4096, timeout_secs: 60 },
    agent: { iteration_cap: 10, tool_timeout_secs: 30 },
    audit: { log_full_prompt: false },
  },
  // Mirror of `TasksConfig::default()` in core/src/config.rs.
  tasks: {
    worker_preference: "auto" as const,
    terminal_retention_secs: 60,
    direct_worker: { enabled: true, parallelism: 1 },
    expose_to_chat_agent: true,
    lease: { default_secs: 60, max_secs: 600 },
  },
  // Mirror of `AcpConfig::default()` in core/src/config.rs.
  acp: {
    command: "",
  },
  // Mirror of `StagingConfig::default()` in core/src/config.rs.
  // status: staging-config-section
  staging: {
    auto_reject_on_conflict: false,
    retention_days: 14,
  },
} as const;

type RowControl =
  | { kind: "bool" }
  | { kind: "enum"; options: Array<[string, string]> }
  | { kind: "number"; min: number; max: number; step?: number }
  | { kind: "string-array" }
  | { kind: "string" };

interface RowSpec {
  /// Dotted path into the merged `Config`. Same shape `set_setting` accepts.
  key: string;
  label: string;
  desc?: string;
  /// Where `set_setting` writes this row. Read-only rows use `null`.
  writeScope: SettingsScope | null;
  control: RowControl | null; // null = read-only
  /// When `true`, the row is hidden whenever the section's scope toggle
  /// is at `[Vault]`. Used for `llm.provider.api_key`: the literal key
  /// is user-scope only per the spec posture in `llm.md` §
  /// `[llm-providers-config]`, so showing the row in vault scope would
  /// suggest a write target that the eligibility list refuses.
  userScopeOnly?: boolean;
}

interface SectionSpec {
  id: string;
  title: string;
  defaultScope: SettingsScope;
  /// One-line stub message rendered when the section is deferred.
  deferred?: string;
  rows: RowSpec[];
}

const SECTIONS: SectionSpec[] = [
  {
    id: "editor",
    title: "Editor",
    defaultScope: "vault",
    rows: [
      { key: "editor.render_txt_as_markdown", label: "Render .txt as markdown",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.live_preview", label: "Live preview",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.word_wrap", label: "Word wrap",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.show_line_numbers", label: "Show line numbers",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.show_whitespace", label: "Show whitespace",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.show_chunk_boundaries", label: "Show chunk boundaries",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.hide_frontmatter", label: "Hide frontmatter",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "editor.tab_size", label: "Tab size",
        desc: "Edit the TOML to change.",
        writeScope: null, control: null },
    ],
  },
  {
    id: "indexing",
    title: "Indexing",
    defaultScope: "vault",
    rows: [
      { key: "indexing.model", label: "Model",
        desc: "Only bge-small-en-v1.5 is supported in v1.",
        writeScope: null, control: null },
      { key: "indexing.batch_size", label: "Batch size",
        writeScope: null, control: null },
      { key: "indexing.ignored_paths", label: "Ignored paths",
        writeScope: null, control: null },
    ],
  },
  {
    id: "vault",
    title: "Vault",
    defaultScope: "vault",
    rows: [
      { key: "vault.sidebar_open", label: "Sidebar open",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "vault.related_open", label: "Discovery panel open",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "vault.trash_expanded", label: "Trash expanded",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "vault.tree.sort_by", label: "Tree sort",
        writeScope: "vault", control: { kind: "enum", options: [
          ["name_asc", "Name (A→Z)"],
          ["name_desc", "Name (Z→A)"],
          ["mtime_desc", "Modified (newest)"],
          ["mtime_asc", "Modified (oldest)"],
        ] } },
      { key: "vault.default", label: "Default vault",
        desc: "User-scope only. The vault auto-opened on startup.",
        writeScope: null, control: null },
      { key: "vault.recent", label: "Recent vaults",
        desc: "User-scope only.",
        writeScope: null, control: null },
    ],
  },
  {
    id: "search",
    title: "Search",
    defaultScope: "vault",
    rows: [
      { key: "search.modes.semantic", label: "Semantic search",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "search.modes.lexical", label: "Lexical search",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "search.sections.results_expanded", label: "Search results expanded",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "search.sections.related_expanded", label: "Related notes expanded",
        writeScope: "vault", control: { kind: "bool" } },
    ],
  },
  {
    id: "keymap",
    title: "Keymap",
    defaultScope: "vault",
    deferred: "No overrides yet — keymap loader is planned (see settings-section-keymap).",
    rows: [],
  },
  {
    id: "llm",
    title: "LLM",
    // User-scope by default — API-key env name + provider live globally;
    // per-vault override available via the [User]/[Vault] toggle.
    defaultScope: "user",
    rows: [
      { key: "llm.enabled", label: "Enable LLM features",
        writeScope: "user", control: { kind: "bool" } },
      { key: "llm.provider.backend", label: "Backend",
        writeScope: "user", control: { kind: "enum", options: [
          ["anthropic", "Anthropic"],
          ["openai", "OpenAI"],
          ["ollama", "Ollama (local)"],
          ["google", "Google"],
          ["openrouter", "OpenRouter"],
          ["groq", "Groq"],
          ["mistral", "Mistral"],
          ["deepseek", "DeepSeek"],
        ] } },
      { key: "llm.provider.model", label: "Model",
        writeScope: "user", control: { kind: "string" } },
      { key: "llm.provider.api_key_env", label: "API-key env var",
        desc: "Fallback when literal key is empty.",
        writeScope: "user", control: { kind: "string" } },
      { key: "llm.provider.api_key", label: "API key",
        desc: "Stored plain text. User-scope only; takes precedence over env var.",
        writeScope: "user", control: { kind: "string" }, userScopeOnly: true },
      { key: "llm.provider.base_url", label: "Base URL",
        desc: "Optional override.",
        writeScope: "user", control: { kind: "string" } },
      { key: "llm.limits.max_tokens", label: "Max tokens",
        writeScope: "user", control: { kind: "number", min: 1, max: 1_000_000, step: 1 } },
      { key: "llm.limits.timeout_secs", label: "Request timeout (s)",
        writeScope: "user", control: { kind: "number", min: 1, max: 86_400, step: 1 } },
      { key: "llm.agent.iteration_cap", label: "Iteration cap",
        writeScope: "user", control: { kind: "number", min: 1, max: 100, step: 1 } },
      { key: "llm.agent.tool_timeout_secs", label: "Tool timeout (s)",
        writeScope: "user", control: { kind: "number", min: 1, max: 3600, step: 1 } },
      { key: "llm.audit.log_full_prompt", label: "Log full prompt/response",
        writeScope: "user", control: { kind: "bool" } },
      { key: "llm.background.review_required", label: "Review required (background)",
        desc: "When on, background LLM features write to staging instead of mutating notes directly.",
        writeScope: "vault", control: { kind: "bool" } },
    ],
  },
  {
    // status: task-queue-settings-ui-section
    id: "tasks",
    title: "Task queue",
    defaultScope: "vault",
    rows: [
      { key: "tasks.worker_preference", label: "Worker preference",
        desc: "Who drains the queue first when multiple workers are eligible.",
        writeScope: "vault", control: { kind: "enum", options: [
          ["auto", "Auto (1s grace)"],
          ["internal", "Internal (direct worker first)"],
          ["external", "External (5s grace)"],
        ] } },
      { key: "tasks.direct_worker.enabled", label: "Run direct LLM worker",
        desc: "In-process tokio worker draining Direct-shape tasks via core::llm.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "tasks.direct_worker.parallelism", label: "Direct worker parallelism",
        writeScope: "vault", control: { kind: "number", min: 1, max: 16, step: 1 } },
      { key: "tasks.expose_to_chat_agent", label: "Expose task_* tools to chat agent",
        desc: "When on, the basic chat agent can pull queue work during turns.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "tasks.terminal_retention_secs", label: "Terminal-row retention (s)",
        writeScope: "vault", control: { kind: "number", min: 1, max: 3600, step: 1 } },
      { key: "tasks.lease.default_secs", label: "Default lease (s)",
        writeScope: "vault", control: { kind: "number", min: 1, max: 600, step: 1 } },
      { key: "tasks.lease.max_secs", label: "Lease cap (s)",
        writeScope: "vault", control: { kind: "number", min: 1, max: 3600, step: 1 } },
    ],
  },
  {
    // status: agent-write-review-settings-toggles
    // status: mcp-settings-ui-section
    id: "mcp",
    title: "MCP server",
    defaultScope: "vault",
    rows: [
      { key: "mcp.enabled", label: "Enable MCP server",
        desc: "Master switch. Off → server doesn't bind, no MCP tools advertised.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.host", label: "Bind host",
        desc:
          "Default 127.0.0.1 (localhost only). Anything else exposes vault " +
          "contents to whoever can reach the listening port — auth is " +
          "localhost-trust, so non-loopback effectively means trust all on " +
          "the LAN. Server restarts in place when this changes.",
        writeScope: "vault", control: { kind: "string" } },
      { key: "mcp.port", label: "Port",
        desc: "0 = ephemeral (OS-assigned). Server restarts in place on change.",
        writeScope: "vault", control: { kind: "number", min: 0, max: 65_535, step: 1 } },
      { key: "mcp.max_top_k", label: "Max top-k",
        desc: "Cap on agent-requested top_k for search_notes / related_notes.",
        writeScope: "vault", control: { kind: "number", min: 1, max: 1000, step: 1 } },
      { key: "mcp.tools.writes_enabled", label: "Allow write tools (master gate)",
        desc:
          "When off, every write tool is refused with `1004 disabled` " +
          "regardless of the per-tool flags below. Live-applied.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.allow_redacted_lookup", label: "Allow redacted-body lookup",
        desc: "When on, agents passing scope can fetch redacted bodies. Default off.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.review_required", label: "Review required",
        desc: "When on, MCP write tools route through staging (manual review) instead of writing directly.",
        writeScope: "vault", control: { kind: "bool" } },
      // Per-tool toggles (status: mcp-tool-toggles). Live-applied.
      { key: "mcp.tools.search_notes_enabled",   label: "Tool: search_notes",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.get_note_enabled",       label: "Tool: get_note",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.related_notes_enabled",  label: "Tool: related_notes",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.write_note_enabled",     label: "Tool: write_note",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.edit_note_enabled",      label: "Tool: edit_note",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.set_frontmatter_enabled",label: "Tool: set_frontmatter",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.apply_tag_enabled",      label: "Tool: apply_tag",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.remove_tag_enabled",     label: "Tool: remove_tag",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.task_checkout_enabled",  label: "Tool: task_checkout",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.task_submit_enabled",    label: "Tool: task_submit",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.task_fail_enabled",      label: "Tool: task_fail",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.task_heartbeat_enabled", label: "Tool: task_heartbeat",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.tools.task_list_enabled",      label: "Tool: task_list",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.audit.log_full_input", label: "Log full tool inputs",
        desc: "Mirror of [llm.audit] log_full_prompt.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "mcp.discovery_file", label: "Discovery file",
        desc: "Vault-relative path. Edit the TOML to change.",
        writeScope: null, control: null },
    ],
  },
  {
    id: "embedder",
    title: "Embedder",
    defaultScope: "vault",
    deferred: "Per-provider config lands when the cloud / Ollama embedder option ships (see embedder-config-section).",
    rows: [],
  },
  {
    // status: staging-config-section
    id: "staging",
    title: "Staging",
    defaultScope: "vault",
    rows: [
      { key: "staging.auto_reject_on_conflict", label: "Auto-reject on conflict",
        desc: "When on, proposals that transition from applyable to conflicted are immediately rejected and disappear from every surface. Live-applied.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "staging.retention_days", label: "Retention (days)",
        desc: "GC age threshold applied at vault open. Older pending proposals are discarded.",
        writeScope: "vault", control: { kind: "number", min: 1, max: 365, step: 1 } },
    ],
  },
  {
    // status: llm-acp-client-optional
    id: "acp",
    title: "ACP",
    defaultScope: "vault",
    rows: [
      { key: "acp.command", label: "Agent command",
        desc: "Full command line to launch the ACP agent. Empty = ACP disabled (uses built-in agent loop). The agent must be installed separately. Examples: \"auggie --acp\", \"gemini --acp\", \"cursor --acp\".",
        writeScope: "vault", control: { kind: "string" } },
    ],
  },
];

export interface SettingsPaneDeps {
  paneEl: HTMLElement;
  settingsBtn: HTMLButtonElement;
  vaultPathEl: HTMLElement;
  // Returns true if it's safe to swap the buffer (clean, or user picked
  // Save / Discard); false on Cancel.
  guardDirtyBuffer: () => Promise<boolean>;
  // Called when entering settings to ensure home view is hidden, and on
  // exit so the caller can decide what to swap back to. The pane itself
  // doesn't own buffer state.
  onEnter: () => void;
  // Refresh hook: settings-pane-eligible-key-controls flips persist via
  // `set_setting` which already updates the in-memory Config in
  // `VaultSession`. The host needs to know so any UI seeded from the
  // setting (View menu state, sidebar collapsed, etc.) stays in sync.
  onSettingApplied: (cfg: SettingsConfig) => void;
}

export interface SettingsPaneApi {
  isVisible(): boolean;
  /// Open or close the settings pane. Returns true if the visibility
  /// changed; false if the dirty-buffer guard cancelled the entry.
  setVisible(on: boolean): Promise<boolean>;
  /// Toggle. Same return shape as setVisible.
  toggle(): Promise<boolean>;
  /// Refresh the pane content in place (e.g. after an external config
  /// edit). // status: tab-kinds
  refresh(): Promise<void>;
}

export function mountSettingsPane(deps: SettingsPaneDeps): SettingsPaneApi {
  const { paneEl, vaultPathEl } = deps;

  // Per-section scope state (in-memory, per-session, not persisted —
  // settings.md "the choice is per-section and per-session, not persisted").
  const sectionScope = new Map<string, SettingsScope>();
  for (const s of SECTIONS) sectionScope.set(s.id, s.defaultScope);

  // Per-section collapsed state (in-memory only — same rationale).
  const sectionCollapsed = new Map<string, boolean>();
  for (const s of SECTIONS) sectionCollapsed.set(s.id, !!s.deferred);

  function isVisible(): boolean {
    // status: tab-kinds — visibility driven by the `hidden` attribute
    // set/cleared by renderActiveTab() in main.ts.
    return !paneEl.hidden;
  }

  async function setVisible(on: boolean): Promise<boolean> {
    if (on === isVisible()) return false;
    if (on) {
      const ok = await deps.guardDirtyBuffer();
      if (!ok) return false;
      deps.onEnter();
      paneEl.hidden = false;
      await render();
    } else {
      paneEl.hidden = true;
    }
    return true;
  }

  async function toggle(): Promise<boolean> {
    return setVisible(!isVisible());
  }

  async function render(): Promise<void> {
    // Build a fresh DOM each render — same idempotency rule as
    // `renderModeControls` and the home page widgets. Cheap; the pane is
    // small and only renders on entry / refresh.
    paneEl.replaceChildren(buildHeader(), ...await buildSections(), buildFooter());
  }

  function buildHeader(): HTMLElement {
    const h = document.createElement("header");
    h.className = "settings-header";
    const title = document.createElement("h1");
    title.textContent = `Settings · ${vaultPathEl.title || vaultPathEl.textContent || "Vault"}`;
    const refresh = document.createElement("button");
    refresh.type = "button";
    refresh.className = "toolbar-btn";
    refresh.textContent = "Refresh";
    refresh.title = "Reload from disk";
    // status: settings-pane-manual-refresh
    refresh.addEventListener("click", async () => {
      try {
        const cfg = await Ipc.reloadConfig();
        deps.onSettingApplied(cfg);
        await render();
      } catch (err) {
        console.error("reload_config failed:", err);
      }
    });
    h.append(title, refresh);
    return h;
  }

  async function buildSections(): Promise<HTMLElement[]> {
    return Promise.all(SECTIONS.map((s) => buildSectionCard(s)));
  }

  async function buildSectionCard(spec: SectionSpec): Promise<HTMLElement> {
    const card = document.createElement("section");
    card.className = "settings-card";
    const collapsed = sectionCollapsed.get(spec.id) ?? false;
    if (collapsed) card.classList.add("collapsed");

    const header = document.createElement("header");
    header.className = "settings-card-header";
    const chevron = document.createElement("span");
    chevron.className = "settings-card-chevron";
    chevron.textContent = collapsed ? "▸" : "▾";
    const title = document.createElement("span");
    title.className = "settings-card-title";
    title.textContent = spec.title;
    header.append(chevron, title);
    if (spec.deferred) {
      const note = document.createElement("span");
      note.className = "settings-card-deferred-note";
      note.textContent = "deferred";
      header.append(note);
    } else {
      header.append(buildScopeToggle(spec));
    }
    header.addEventListener("click", (e) => {
      // Don't collapse when clicking inside the scope toggle.
      if ((e.target as HTMLElement).closest(".settings-scope-toggle")) return;
      const next = !card.classList.contains("collapsed");
      card.classList.toggle("collapsed", next);
      sectionCollapsed.set(spec.id, next);
      chevron.textContent = next ? "▸" : "▾";
    });
    card.appendChild(header);

    const body = document.createElement("div");
    body.className = "settings-card-body";
    if (spec.deferred) {
      const stub = document.createElement("div");
      stub.className = "settings-row";
      const lbl = document.createElement("div");
      lbl.className = "settings-row-label";
      lbl.style.fontStyle = "italic";
      lbl.classList.add("txt-muted");
      lbl.textContent = spec.deferred;
      stub.appendChild(lbl);
      body.appendChild(stub);
    } else {
      const scope = sectionScope.get(spec.id) ?? spec.defaultScope;
      const cfg = await loadScopedConfig(scope);
      for (const row of spec.rows) {
        // Hide user-scope-only rows when the toggle is at [Vault] —
        // the eligibility list refuses vault writes for these keys, so
        // rendering them in vault scope would suggest a write target
        // that doesn't exist.
        if (row.userScopeOnly && scope === "vault") continue;
        body.appendChild(buildRow(spec, row, scope, cfg));
      }
    }
    card.appendChild(body);
    return card;
  }

  function buildScopeToggle(spec: SectionSpec): HTMLElement {
    const wrap = document.createElement("span");
    wrap.className = "settings-scope-toggle";
    const current = sectionScope.get(spec.id) ?? spec.defaultScope;
    for (const s of ["user", "vault"] as SettingsScope[]) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = s === "user" ? "User" : "Vault";
      if (s === current) btn.classList.add("active");
      btn.addEventListener("click", async (e) => {
        e.stopPropagation();
        if (sectionScope.get(spec.id) === s) return;
        sectionScope.set(spec.id, s);
        await render();
      });
      wrap.appendChild(btn);
    }
    return wrap;
  }

  async function loadScopedConfig(scope: SettingsScope): Promise<SettingsConfig> {
    try {
      return await Ipc.getSettingsScoped({ scope });
    } catch (err) {
      console.error("get_settings_scoped failed:", err);
      // Fall back to the merged in-memory copy so the pane still renders.
      return await Ipc.getSettings();
    }
  }

  function buildRow(
    _section: SectionSpec,
    row: RowSpec,
    scope: SettingsScope,
    scoped: SettingsConfig,
  ): HTMLElement {
    const el = document.createElement("div");
    el.className = "settings-row";

    const label = document.createElement("div");
    label.className = "settings-row-label";
    label.textContent = row.label;
    if (row.desc) {
      const d = document.createElement("span");
      d.className = "settings-row-desc";
      d.textContent = row.desc;
      label.append(d);
    }
    el.append(label);

    const ctrl = document.createElement("div");
    ctrl.className = "settings-row-control";
    const value = readKey(scoped, row.key);

    if (row.control && row.writeScope) {
      const control = buildControl(row, value, row.writeScope);
      const reset = buildResetButton(row, value, control);
      ctrl.append(control, reset);
    } else {
      ctrl.append(buildReadonlyDisplay(value));
      ctrl.append(buildInfoPopover(row, scope));
    }
    el.append(ctrl);
    return el;
  }

  function buildControl(row: RowSpec, value: unknown, writeScope: SettingsScope): HTMLElement {
    const c = row.control!;
    if (c.kind === "bool") {
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.className = "settings-row-checkbox";
      cb.checked = !!value;
      cb.addEventListener("change", () => {
        void persist(row.key, writeScope, cb.checked);
      });
      return cb;
    }
    if (c.kind === "enum") {
      const sel = document.createElement("select");
      sel.className = "settings-row-input";
      for (const [v, label] of c.options) {
        const opt = document.createElement("option");
        opt.value = v;
        opt.textContent = label;
        if (String(value) === v) opt.selected = true;
        sel.appendChild(opt);
      }
      sel.addEventListener("change", () => {
        void persist(row.key, writeScope, sel.value);
      });
      return sel;
    }
    if (c.kind === "number") {
      const inp = document.createElement("input");
      inp.type = "number";
      inp.className = "settings-row-input";
      inp.min = String(c.min);
      inp.max = String(c.max);
      if (c.step) inp.step = String(c.step);
      inp.value = String(value);
      let timer: number | null = null;
      inp.addEventListener("input", () => {
        if (timer !== null) window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          timer = null;
          const n = Number(inp.value);
          if (Number.isFinite(n)) void persist(row.key, writeScope, n);
        }, 300);
      });
      return inp;
    }
    if (c.kind === "string") {
      const inp = document.createElement("input");
      inp.type = "text";
      inp.className = "settings-row-input";
      inp.value = typeof value === "string" ? value : "";
      let timer: number | null = null;
      inp.addEventListener("input", () => {
        if (timer !== null) window.clearTimeout(timer);
        timer = window.setTimeout(() => {
          timer = null;
          void persist(row.key, writeScope, inp.value);
        }, 300);
      });
      return inp;
    }
    // string-array: degraded read-only view in v1 — the only consumer is
    // indexing.ignored_paths which is currently read-only anyway. Pill-list
    // editor lands when an interactive consumer needs it.
    return buildReadonlyDisplay(value);
  }

  function buildResetButton(
    row: RowSpec,
    currentValue: unknown,
    control: HTMLElement,
  ): HTMLElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "settings-row-reset";
    btn.textContent = "reset";
    btn.title = "Reset to default";
    const def = readKey(DEFAULTS as unknown as SettingsConfig, row.key);
    btn.disabled = sameValue(currentValue, def);
    btn.addEventListener("click", () => {
      if (row.writeScope === null || def === undefined) return;
      // Update the control's displayed value in place — `persist` no
      // longer re-renders (would yank focus from text inputs mid-typing),
      // so the control needs to reflect the reset value directly.
      applyValueToControl(control, def);
      btn.disabled = true;
      void persist(row.key, row.writeScope, def);
    });
    return btn;
  }

  /// Push `value` into a freshly-built control element. Mirrors the
  /// initialization paths in `buildControl` for each kind.
  function applyValueToControl(control: HTMLElement, value: unknown): void {
    if (control instanceof HTMLInputElement) {
      if (control.type === "checkbox") control.checked = !!value;
      else control.value = String(value);
    } else if (control instanceof HTMLSelectElement) {
      control.value = String(value);
    }
  }

  function buildReadonlyDisplay(value: unknown): HTMLElement {
    const el = document.createElement("span");
    el.className = "settings-row-readonly";
    el.textContent = formatValue(value);
    return el;
  }

  function buildInfoPopover(row: RowSpec, scope: SettingsScope): HTMLElement {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "settings-row-info";
    btn.textContent = "ⓘ";
    btn.title = "Edit the TOML to change.";
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      openInfoPopover(btn, row, scope);
    });
    return btn;
  }

  let activePopover: HTMLElement | null = null;
  function openInfoPopover(anchor: HTMLElement, row: RowSpec, scope: SettingsScope): void {
    closePopover();
    const pop = document.createElement("div");
    pop.className = "settings-popover";
    const head = document.createElement("div");
    head.textContent = `${row.key} — read-only`;
    pop.appendChild(head);
    const src = document.createElement("div");
    src.className = "source";
    src.textContent = scope === "user"
      ? "User TOML at the platform config dir"
      : "vault/.hiker/config.toml";
    pop.appendChild(src);
    const open = document.createElement("button");
    open.type = "button";
    open.textContent = "Open in file manager";
    open.addEventListener("click", () => {
      void Ipc.revealConfigFile({ scope }).catch((err) => {
        console.error("reveal_config_file failed:", err);
      });
      closePopover();
    });
    pop.appendChild(open);
    document.body.appendChild(pop);
    const rect = anchor.getBoundingClientRect();
    pop.style.top = `${Math.min(rect.bottom + 4, window.innerHeight - pop.offsetHeight - 4)}px`;
    pop.style.left = `${Math.min(rect.left, window.innerWidth - pop.offsetWidth - 4)}px`;
    activePopover = pop;

    const onDocDown = (ev: MouseEvent) => {
      if (!pop.contains(ev.target as Node)) closePopover();
    };
    setTimeout(() => document.addEventListener("mousedown", onDocDown, true));
    pop.addEventListener("remove", () => {
      document.removeEventListener("mousedown", onDocDown, true);
    });
  }

  function closePopover(): void {
    if (activePopover && activePopover.parentNode) {
      activePopover.parentNode.removeChild(activePopover);
      activePopover.dispatchEvent(new Event("remove"));
    }
    activePopover = null;
  }

  async function persist(key: string, writeScope: SettingsScope, value: unknown): Promise<void> {
    // Deliberately *don't* re-render the pane on every flip — debounced
    // text/number inputs would lose focus mid-typing every time the
    // commit fired. The control already reflects the user's value; the
    // on-disk write is in flight; `applySettingsToUi` covers every other
    // surface that reads the setting. The reset button's disabled state
    // and read-only mirrors stay slightly stale until the next render
    // (scope toggle, section collapse, manual Refresh, or pane reopen),
    // which is acceptable in v1. Persist failures fall through silently
    // (logged) — re-rendering would also yank focus.
    try {
      const cfg = await Ipc.setSetting({
        scope: writeScope,
        key,
        value,
      });
      deps.onSettingApplied(cfg);
    } catch (err) {
      console.error(`set_setting ${writeScope}.${key} failed:`, err);
    }
  }

  function buildFooter(): HTMLElement {
    const f = document.createElement("footer");
    f.className = "settings-footer";
    const schema = document.createElement("span");
    schema.className = "schema";
    schema.textContent = "Schema version: 1";
    f.append(schema);
    for (const scope of ["user", "vault"] as SettingsScope[]) {
      const btn = document.createElement("button");
      btn.type = "button";
      btn.textContent = scope === "user" ? "Open user config.toml" : "Open vault config.toml";
      btn.addEventListener("click", () => {
        void Ipc.revealConfigFile({ scope }).catch((err) => {
          console.error("reveal_config_file failed:", err);
        });
      });
      f.append(btn);
    }
    return f;
  }

  return { isVisible, setVisible, toggle, refresh: render };
}

// Walk a dotted path through a record and return the leaf value, or
// `undefined` if any segment is missing. Used both for reading the merged
// `Config` shape and the in-code `DEFAULTS` table.
function readKey(obj: unknown, key: string): unknown {
  let cur: unknown = obj;
  for (const part of key.split(".")) {
    if (cur === null || cur === undefined) return undefined;
    if (typeof cur !== "object") return undefined;
    cur = (cur as Record<string, unknown>)[part];
  }
  return cur;
}

function sameValue(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (Array.isArray(a) && Array.isArray(b)) {
    return a.length === b.length && a.every((v, i) => v === b[i]);
  }
  return false;
}

function formatValue(v: unknown): string {
  if (v === null || v === undefined) return "—";
  if (Array.isArray(v)) {
    return v.length === 0 ? "[empty]" : `[${v.length} ${v.length === 1 ? "entry" : "entries"}]`;
  }
  if (typeof v === "boolean") return v ? "true" : "false";
  if (typeof v === "object") return "—";
  return String(v);
}
