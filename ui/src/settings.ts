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
import { runCommand } from "./ipc/runCommand";
import { confirmAccent } from "./widgets/confirm";
import { el } from "./widgets/dom";

// Generated mirror of `core::config::*`. The Rust struct definitions in
// `core/src/config/sections.rs` are the source of truth; the TS shapes
// land via `cargo test --features ts-export gen_ts_types` and are
// verified-in-sync by `scripts/check.sh`. Adding a Rust field no longer
// requires a matching TS edit — the generated file lights up the new
// shape on the next codegen run.
import type { Config as SettingsConfig } from "./generatedTypes/Config";
export type { Config as SettingsConfig } from "./generatedTypes/Config";
export type { SettingsScope } from "./generatedTypes/SettingsScope";
import type { SettingsScope } from "./generatedTypes/SettingsScope";

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
  // status: embedder-model-selectable
  // Mirror of `IndexingConfig::default()` in core/src/config.rs.
  indexing: {
    model: "bge-small-en-v1.5" as const,
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

// Mirror of the fastembed model registry in `core::embed::KNOWN_MODELS`
// (`embedder-model-selectable`). Order matches the spec table in
// `docs/index.md`. Used by the model dropdown and by the change-warning
// modal to render the optional "Dim change" bullet. Kept in sync by hand —
// the set is fixed at three entries; drift is a one-line check at PR time.
//
// status: settings-embedder-model-change-warning
const EMBEDDER_MODELS: Array<{ id: string; label: string; dim: number }> = [
  { id: "bge-small-en-v1.5",   label: "bge-small-en-v1.5 (default, 384-dim, English)", dim: 384 },
  { id: "bge-m3",              label: "bge-m3 (1024-dim, multilingual, 8k context)",   dim: 1024 },
  { id: "embedding-gemma-300m", label: "embedding-gemma-300m (768-dim)",                dim: 768 },
];

function embedderModelDim(id: string): number | null {
  const m = EMBEDDER_MODELS.find((m) => m.id === id);
  return m ? m.dim : null;
}

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
      // status: embedder-model-selectable
      // status: settings-embedder-model-change-warning
      // Live dropdown; flips are gated by a confirm modal (per spec) that
      // names the consequences and (when applicable) flags the dim change.
      { key: "indexing.model", label: "Model",
        desc: "bge-small-en-v1.5 (default), bge-m3, embedding-gemma-300m.",
        writeScope: "vault", control: { kind: "enum",
          options: EMBEDDER_MODELS.map((m) => [m.id, m.label]) } },
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
    // status: triage-review-required, triage-staging-proposals
    id: "suggestions-triage",
    title: "Triage",
    defaultScope: "vault",
    rows: [
      { key: "suggestions.triage.review_required", label: "Require review on every triage match",
        desc: "When on, every triage match stays pending in staging until you accept. When off, auto-* policies auto-accept (subject to the per-node require_review flag). Live-applied.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "suggestions.triage.scope", label: "Source-folder scope",
        desc: "Triage never produces a move_note row whose source path is outside this folder. Also drives the on-save trigger's default folder. Vault-relative; default \"inbox/\".",
        writeScope: "vault", control: { kind: "string" } },
      { key: "suggestions.triage.scheduled_rerun", label: "Scheduled rerun (cron)",
        desc: "Cron-shape; empty disables. e.g. \"0 3 * * *\" (cron-style; not yet supported) or simple duration strings like \"1h\", \"24h\", \"7d\". Per cluster-editor-triage-scheduled-rerun.",
        writeScope: "vault", control: { kind: "string" } },
      { key: "suggestions.triage.modified_rerun", label: "Re-triage on meaningful edit",
        desc: "Opt-in: re-run triage when an already-placed note's embedding shifts beyond the cosine guard. Distinct from the on-save trigger which always fires for notes inside the source-folder scope.",
        writeScope: "vault", control: { kind: "bool" } },
      { key: "suggestions.triage.modified_rerun_cosine_guard", label: "Modified-rerun cosine guard",
        desc: "Cosine-distance threshold (0.0–1.0) the embedding must shift past before a re-triage fires. Typical save noise sits below 0.15; meaningful edits clear it. Default 0.15.",
        writeScope: "vault", control: { kind: "number", min: 0, max: 1, step: 0.01 } },
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
    return el("header", { class: "settings-header" }, [
      el("h1", {
        text: `Settings · ${vaultPathEl.title || vaultPathEl.textContent || "Vault"}`,
      }),
      el("button", {
        class: "toolbar-btn",
        text: "Refresh",
        title: "Reload from disk",
        attrs: { type: "button" },
        // status: settings-pane-manual-refresh
        onClick: async () => {
          try {
            const cfg = await Ipc.reloadConfig();
            deps.onSettingApplied(cfg);
            await render();
          } catch (err) {
            console.error("reload_config failed:", err);
          }
        },
      }),
    ]);
  }

  async function buildSections(): Promise<HTMLElement[]> {
    return Promise.all(SECTIONS.map((s) => buildSectionCard(s)));
  }

  async function buildSectionCard(spec: SectionSpec): Promise<HTMLElement> {
    const collapsed = sectionCollapsed.get(spec.id) ?? false;
    const chevron = el("span", {
      class: "settings-card-chevron",
      text: collapsed ? "▸" : "▾",
    });
    const headerChildren: (ChildNode | null)[] = [
      chevron,
      el("span", { class: "settings-card-title", text: spec.title }),
    ];
    if (spec.deferred) {
      headerChildren.push(el("span", {
        class: "settings-card-deferred-note",
        text: "deferred",
      }));
    } else {
      headerChildren.push(buildScopeToggle(spec));
    }
    const card = el("section", {
      class: collapsed ? "settings-card collapsed" : "settings-card",
    });
    const header = el("header", {
      class: "settings-card-header",
      onClick: (e) => {
        // Don't collapse when clicking inside the scope toggle.
        if ((e.target as HTMLElement).closest(".settings-scope-toggle")) return;
        const next = !card.classList.contains("collapsed");
        card.classList.toggle("collapsed", next);
        sectionCollapsed.set(spec.id, next);
        chevron.textContent = next ? "▸" : "▾";
      },
    }, headerChildren);
    card.appendChild(header);

    const body = el("div", { class: "settings-card-body" });
    if (spec.deferred) {
      const lbl = el("div", {
        class: "settings-row-label txt-muted",
        text: spec.deferred,
        style: { fontStyle: "italic" },
      });
      body.appendChild(el("div", { class: "settings-row" }, [lbl]));
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
    const current = sectionScope.get(spec.id) ?? spec.defaultScope;
    const buttons = (["user", "vault"] as SettingsScope[]).map((s) =>
      el("button", {
        class: s === current ? "active" : undefined,
        text: s === "user" ? "User" : "Vault",
        attrs: { type: "button" },
        onClick: async (e) => {
          e.stopPropagation();
          if (sectionScope.get(spec.id) === s) return;
          sectionScope.set(spec.id, s);
          await render();
        },
      }),
    );
    return el("span", { class: "settings-scope-toggle" }, buttons);
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
    const labelChildren: (ChildNode | string)[] = [row.label];
    if (row.desc) {
      labelChildren.push(el("span", { class: "settings-row-desc", text: row.desc }));
    }
    const label = el("div", { class: "settings-row-label" }, labelChildren);

    const ctrl = el("div", { class: "settings-row-control" });
    const value = readKey(scoped, row.key);

    if (row.control && row.writeScope) {
      const control = buildControl(row, value, row.writeScope);
      const reset = buildResetButton(row, value, control);
      ctrl.append(control, reset);
    } else {
      ctrl.append(buildReadonlyDisplay(value));
      ctrl.append(buildInfoPopover(row, scope));
    }
    return el("div", { class: "settings-row" }, [label, ctrl]);
  }

  function buildControl(row: RowSpec, value: unknown, writeScope: SettingsScope): HTMLElement {
    const c = row.control!;
    if (c.kind === "bool") {
      const cb = el("input", {
        class: "settings-row-checkbox",
        attrs: { type: "checkbox" },
        on: { change: () => { void persist(row.key, writeScope, cb.checked); } },
      });
      cb.checked = !!value;
      return cb;
    }
    if (c.kind === "enum") {
      const prior = String(value ?? "");
      const options = c.options.map(([v, label]) => {
        const opt = el("option", { text: label });
        opt.value = v;
        if (String(value) === v) opt.selected = true;
        return opt;
      });
      const sel = el("select", {
        class: "settings-row-input",
        on: {
          change: async () => {
            const next = sel.value;
            // status: settings-embedder-model-change-warning
            // The embedder-model flip is gated by a consequential-confirm
            // modal. Cancel reverts the dropdown without writing. Same-value
            // change (defensive) is a no-op.
            if (row.key === "indexing.model" && next !== prior) {
              const ok = await confirmEmbedderModelChange(prior, next);
              if (!ok) {
                sel.value = prior;
                return;
              }
            }
            void persist(row.key, writeScope, sel.value);
          },
        },
      }, options);
      return sel;
    }
    if (c.kind === "number") {
      const inp = el("input", {
        class: "settings-row-input",
        attrs: { type: "number", min: String(c.min), max: String(c.max) },
      });
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
      const inp = el("input", {
        class: "settings-row-input",
        attrs: { type: "text" },
        value: typeof value === "string" ? value : "",
      });
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
    const def = readKey(DEFAULTS as unknown as SettingsConfig, row.key);
    const btn = el("button", {
      class: "settings-row-reset",
      text: "reset",
      title: "Reset to default",
      attrs: { type: "button" },
      disabled: sameValue(currentValue, def),
    });
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
    return el("span", {
      class: "settings-row-readonly",
      text: formatValue(value),
    });
  }

  function buildInfoPopover(row: RowSpec, scope: SettingsScope): HTMLElement {
    const btn = el("button", {
      class: "settings-row-info",
      text: "ⓘ",
      title: "Edit the TOML to change.",
      attrs: { type: "button" },
      onClick: (e) => {
        e.stopPropagation();
        openInfoPopover(btn, row, scope);
      },
    });
    return btn;
  }

  let activePopover: HTMLElement | null = null;
  function openInfoPopover(anchor: HTMLElement, row: RowSpec, scope: SettingsScope): void {
    closePopover();
    const pop = el("div", { class: "settings-popover" }, [
      el("div", { text: `${row.key} — read-only` }),
      el("div", {
        class: "source",
        text: scope === "user"
          ? "User TOML at the platform config dir"
          : "vault/.hiker/config.toml",
      }),
      el("button", {
        text: "Open in file manager",
        attrs: { type: "button" },
        onClick: () => {
          // `silent: true` preserves pre-pipeline behavior — this site logged
          // to console only, never showed a toast on failure.
          void runCommand("ipc.revealConfigFile", () => Ipc.revealConfigFile({ scope }), { silent: true });
          closePopover();
        },
      }),
    ]);
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
    const children: ChildNode[] = [
      el("span", { class: "schema", text: "Schema version: 1" }),
    ];
    for (const scope of ["user", "vault"] as SettingsScope[]) {
      children.push(el("button", {
        text: scope === "user" ? "Open user config.toml" : "Open vault config.toml",
        attrs: { type: "button" },
        onClick: () => {
          // `silent: true` preserves pre-pipeline behavior — console-only log.
          void runCommand("ipc.revealConfigFile", () => Ipc.revealConfigFile({ scope }), { silent: true });
        },
      }));
    }
    return el("footer", { class: "settings-footer" }, children);
  }

  return { isVisible, setVisible, toggle, refresh: render };
}

// status: settings-embedder-model-change-warning
// Confirm modal for the embedder-model dropdown. Names the current + new
// model verbatim, states the re-embed consequence in plain language, only
// shows the "Dim change" bullet when the two models differ in dim, and
// hands the user a qualitative time-range estimate (per spec — vault size
// and CPU vary too widely for a computed number to be honest). Cancel is
// default-focused inside `confirmAccent`.
async function confirmEmbedderModelChange(
  prior: string,
  next: string,
): Promise<boolean> {
  const priorDim = embedderModelDim(prior);
  const nextDim = embedderModelDim(next);
  const dimChange = priorDim !== null && nextDim !== null && priorDim !== nextDim;
  const lines: string[] = [
    `Switching from ${prior} to ${next} will re-embed every note in this vault.`,
    "",
    "• All chunks re-embedded (no chat / search answers from semantic until done)",
  ];
  if (dimChange) {
    lines.push(`• Dim change (${priorDim} → ${nextDim}): the vector table is dropped and recreated`);
  }
  lines.push("• Expect minutes to hours depending on vault size and CPU");
  return confirmAccent(lines.join("\n"), "Change model and re-embed");
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
