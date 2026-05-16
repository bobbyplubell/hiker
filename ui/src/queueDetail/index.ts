// status: task-queue-home-widget
// status: task-queue-home-detail-view
// status: task-queue-row-pulsing-leased
// status: task-queue-row-cancel-action
// status: queue-detail-shared-page
// status: queue-detail-filter-tasks
// status: queue-detail-filter-index
// status: queue-detail-shared-row-primitive
//
// Shared queue detail page: tasks (`core::tasks`) + embedding queue
// (`core::indexer`) rendered together with filter pills. The two queues
// stay strictly separate at the data layer — this module is only the
// rendering primitive + a small in-memory mirror of each event channel.

import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import {
  createSettingsManager,
  type SettingsManager,
} from "../settings/manager";
import {
  createPanelController,
  type PanelController,
  type PanelDeps,
} from "../panels/controller";
import { Classes, Selectors } from "../style/classes";
import { Icons } from "../icons";
import { on } from "../events/bus";

type Priority = "low" | "normal" | "high";
type TaskShape = "direct" | "agent";
type TaskState = "queued" | "leased" | "completed" | "failed" | "cancelled";

interface TaskKind {
  type: string;
  [key: string]: unknown;
}

interface WorkerKind {
  // `indexer` covers self-managed queue rows produced by the indexer
  // task (currently: `embedder_model_load`).
  // status: embedder-model-load-as-task
  kind: "direct_llm" | "mcp_client" | "indexer";
  client_id?: string;
  via?: "external" | "in_process_chat_agent";
}

interface TaskRecord {
  id: string;
  kind: TaskKind;
  kind_summary: string;
  priority: Priority;
  shape: TaskShape;
  state: TaskState;
  submitted_at_ms: number;
  lease_expires_at_ms?: number;
  worker?: WorkerKind;
  finished_at_ms?: number;
}

type QueueEvent =
  | {
      event: "task_queued";
      id: string;
      kind: TaskKind;
      priority: Priority;
      shape: TaskShape;
      submitted_at_ms: number;
    }
  | {
      event: "task_leased";
      id: string;
      worker: WorkerKind;
      lease_expires_at_ms: number;
    }
  | { event: "task_heartbeat"; id: string; lease_expires_at_ms: number }
  | { event: "task_completed"; id: string; worker: WorkerKind; duration_ms: number }
  | {
      event: "task_failed";
      id: string;
      worker?: WorkerKind;
      error_summary: string;
      duration_ms: number;
    }
  | { event: "task_cancelled"; id: string; reason: string };

// status: queue-detail-filter-tasks, queue-detail-filter-index
// Two multi-select pills: LLM tasks (robot icon, matching the chat
// panel's agent glyph) and Embedding (brain icon, matching the search
// bar's semantic-search glyph). Both default to active = the previous
// "All" view; toggle either off to narrow. Worker toggles surface only
// when the LLM-tasks pill is on.
type FilterPill = "tasks" | "embedding";

const ROBOT_ICON_SVG = Icons.robot();
const BRAIN_ICON_SVG = Icons.brain();

// status: queue-detail-embedding-row-shape
// Mirror of `hiker_core::indexer::ProgressEvent` (snake_case discriminant).
type ProgressEvent =
  | { kind: "model_loaded" }
  | { kind: "started"; path: string }
  | { kind: "finished"; path: string }
  | { kind: "skipped"; path: string; reason: string }
  | { kind: "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string }
  | { kind: "scan_complete"; scanned: number; queued: number }
  | { kind: "error"; path: string | null; message: string };

type IndexState = "started" | "finished" | "skipped" | "errored";

interface IndexRow {
  path: string;
  state: IndexState;
  reason?: string;
  finished_at_ms?: number;
  // Monotonic counter — reused for sort order so newer rows surface first
  // within the "Recently finished" section, mirroring the rest of the page.
  seq: number;
}

export interface QueueDetailDeps extends PanelDeps {
  containerEl: HTMLElement;
}

export interface QueueDetailApi {
  // `isVisible` / `setVisible` live on the `PanelController` wrapper.
  setFilter(f: FilterPill): void;
  /// Re-fetch settings and re-seed the worker toggles. Called by the
  /// host after a vault opens and by the bus `vault-opened` subscriber,
  /// because `mountQueueDetail` runs at module-load before any vault is
  /// open so the initial `getSettings` can resolve against a
  /// not-yet-vault-bound config.
  refreshFromSettings(): Promise<void>;
  /// Tear down event subscriptions. Currently never called (the page
  /// stays mounted for the lifetime of the app).
  destroy(): Promise<void>;
}

export type QueueDetailController = PanelController<QueueDetailApi>;

// Queue detail is a full-pane content surface (mounted into
// `vault-home-queue-detail`). Visibility maps to the container's
// `hidden` attribute; flipping it on seeds a fresh tasks snapshot so a
// long-paused page picks up rows that arrived while it was hidden.
// Both the host's "show queue detail" verb and any future internal
// trigger route through the controller's single `onSetVisible` hook.
export function mountQueueDetail(deps: QueueDetailDeps): QueueDetailController {
  const taskRows = new Map<string, TaskRecord>();
  // Cap the embedding mirror so a long-running index pass doesn't grow
  // the local map without bound. Active rows are unbounded (one per
  // in-flight path); finished rows trim past this.
  const INDEX_FINISHED_CAP = 50;
  const indexRows = new Map<string, IndexRow>();
  let indexSeq = 0;
  // Multi-select filter set. Default: both pills active (= the previous
  // "All" view). External `setFilter(pill)` activates only the named pill
  // (matches the prior pre-select behavior on entry from a tile).
  const activeFilters = new Set<FilterPill>(["tasks", "embedding"]);
  let unlistenQueue: UnlistenFn | null = null;
  let unlistenIndex: UnlistenFn | null = null;
  let visible = false;

  const root = deps.containerEl;
  root.classList.add("queue-detail");
  root.innerHTML = `
    <header class="vault-home-header">
      <h1 id="queue-detail-title">Background work</h1>
    </header>
    <div class="queue-detail-pills vault-home-filters" role="group" aria-label="Background work filters">
      <button class="toolbar-btn ${Classes.QUEUE_PILL} filter-pill-icon-only" data-filter="tasks" type="button" title="LLM tasks" aria-label="Toggle LLM tasks">${ROBOT_ICON_SVG}</button>
      <button class="toolbar-btn ${Classes.QUEUE_PILL} filter-pill-icon-only" data-filter="embedding" type="button" title="Embedding" aria-label="Toggle embedding tasks">${BRAIN_ICON_SVG}</button>
    </div>
    <div class="queue-detail-toggles" data-section="toggles" hidden>
      <label class="queue-toggle-label">
        <input type="checkbox" data-toggle="direct_worker"> Direct worker
      </label>
      <label class="queue-toggle-label">
        <input type="checkbox" data-toggle="expose_chat"> Expose to chat agent
      </label>
      <label class="queue-toggle-label">
        Worker preference
        <select class="toolbar-btn" data-toggle="worker_preference">
          <option value="auto">Auto</option>
          <option value="internal">Internal</option>
          <option value="external">External</option>
        </select>
      </label>
      <span class="queue-toggle-status" data-toggle-status></span>
    </div>
    <section class="vault-home-section queue-detail-section" data-section="active">
      <h2>Active</h2>
      <ul class="queue-list" data-list="active"></ul>
    </section>
    <section class="vault-home-section queue-detail-section" data-section="queued">
      <h2>Queued</h2>
      <ul class="queue-list" data-list="queued"></ul>
    </section>
    <section class="vault-home-section queue-detail-section" data-section="finished">
      <h2>Recently finished</h2>
      <ul class="queue-list" data-list="finished"></ul>
    </section>
  `;

  for (const btn of Array.from(root.querySelectorAll<HTMLButtonElement>(Selectors.QUEUE_PILLS))) {
    btn.addEventListener("click", () => {
      const f = btn.getAttribute("data-filter") as FilterPill;
      togglePill(f);
    });
  }

  // status: task-queue-settings-ui-section
  // Bind the inline toggles to `set_setting` so flips persist + the
  // running queue picks up the change on next vault open. Seeded from
  // `get_settings` on mount.
  const directToggle = root.querySelector<HTMLInputElement>(
    'input[data-toggle="direct_worker"]',
  );
  const exposeToggle = root.querySelector<HTMLInputElement>(
    'input[data-toggle="expose_chat"]',
  );
  const prefSelect = root.querySelector<HTMLSelectElement>(
    'select[data-toggle="worker_preference"]',
  );
  const statusEl = root.querySelector<HTMLElement>("[data-toggle-status]");

  // Local `SettingsManager` that flashes the toggles-tray status string on
  // success/error. The shared try/catch + Logger.error + flash pattern
  // lives once in `createSettingsManager`; this surface just supplies the
  // flash callback so the prior "Saved" / "Error: ..." UX is preserved.
  const settings: SettingsManager = createSettingsManager({
    logTarget: "ui::queue-detail",
    flash: (msg, isError) => {
      if (!statusEl) return;
      statusEl.textContent = msg;
      statusEl.classList.toggle("error", isError);
      setTimeout(() => {
        if (statusEl.textContent === msg) statusEl.textContent = "";
      }, 2400);
    },
  });

  if (directToggle) {
    directToggle.addEventListener("change", () => {
      void settings.setVaultSetting(
        "tasks.direct_worker.enabled",
        directToggle.checked,
      );
    });
  }
  if (exposeToggle) {
    exposeToggle.addEventListener("change", () => {
      void settings.setVaultSetting(
        "tasks.expose_to_chat_agent",
        exposeToggle.checked,
      );
    });
  }
  if (prefSelect) {
    prefSelect.addEventListener("change", () => {
      void settings.setVaultSetting(
        "tasks.worker_preference",
        prefSelect.value,
      );
    });
  }

  interface TasksConfigShape {
    worker_preference: string;
    direct_worker: { enabled: boolean };
    expose_to_chat_agent: boolean;
  }

  async function refreshFromSettings(): Promise<void> {
    try {
      const cfg = await Ipc.getSettings<{ tasks: TasksConfigShape }>();
      if (directToggle) directToggle.checked = cfg.tasks.direct_worker.enabled;
      if (exposeToggle) exposeToggle.checked = cfg.tasks.expose_to_chat_agent;
      if (prefSelect) prefSelect.value = cfg.tasks.worker_preference;
    } catch (err) {
      Logger.error("ui::queue-detail", "get_settings (tasks pane) failed", {
        err,
      });
    }
  }

  // Initial seed — may resolve against a not-yet-vault-bound config;
  // `refreshFromSettings` is called again from `applyOpenedVault` and
  // the `vault-opened` bus event to fix that.
  void refreshFromSettings();

  // Re-seed whenever a vault opens (belt-and-suspenders with the host's
  // `applyOpenedVault` call — covers future open paths).
  on("vault-opened", () => {
    void refreshFromSettings();
  });

  function paintPills(): void {
    for (const btn of Array.from(root.querySelectorAll<HTMLButtonElement>(Selectors.QUEUE_PILLS))) {
      const f = btn.getAttribute("data-filter") as FilterPill;
      btn.classList.toggle("active", activeFilters.has(f));
    }
    // Worker toggles ride with the LLM-tasks filter — hide when that
    // pill is off (no LLM rows visible → toggles aren't relevant).
    // Re-seed toggles from settings when they become visible so a user
    // who opens the page mid-session doesn't see stale state (the
    // initial seed runs before any vault is open).
    const togglesEl = root.querySelector<HTMLElement>('[data-section="toggles"]');
    if (togglesEl) {
      const wasHidden = togglesEl.hidden;
      togglesEl.hidden = !activeFilters.has("tasks");
      if (wasHidden && !togglesEl.hidden) void refreshFromSettings();
    }
  }

  function togglePill(f: FilterPill): void {
    if (activeFilters.has(f)) {
      // Don't allow turning off the last active pill — empty filter
      // would render an empty page with no recovery affordance.
      if (activeFilters.size === 1) return;
      activeFilters.delete(f);
    } else {
      activeFilters.add(f);
    }
    paintPills();
    render();
  }

  function setFilter(f: FilterPill): void {
    // Pre-select hook used by the home tile entry points: activate only
    // the named pill (matches the prior "drill in pre-selects this
    // queue" behavior). User can re-enable the other pill with one click.
    activeFilters.clear();
    activeFilters.add(f);
    paintPills();
    render();
  }
  paintPills();

  // Container-level click delegation for queue rows. Cancel buttons
  // carry `data-action="cancel"`; the row root carries `data-task-id`
  // (only `core::tasks` rows — embedding rows are inert at the click
  // layer per `queue-detail-embedding-row-shape`). The delegated
  // handler resolves both via `closest()` so per-row listeners aren't
  // re-bound on every `replaceChildren` cycle.
  root.addEventListener("click", (e) => {
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const li = target.closest<HTMLElement>("li.queue-row[data-task-id]");
    if (!li || !root.contains(li)) return;
    const taskId = li.dataset.taskId;
    if (!taskId) return;
    const cancelBtn = target.closest<HTMLElement>('[data-action="cancel"]');
    if (cancelBtn && li.contains(cancelBtn)) {
      e.stopPropagation();
      void Ipc.tasksCancel({ id: taskId }).catch((err) => {
        Logger.error("ui::queue-detail", "tasks_cancel failed", { err });
      });
      return;
    }
    // Only the row's head toggles the details panel — clicks inside an
    // expanded `.queue-row-details` (e.g. on a `<pre>`) must not collapse
    // the panel out from under the user.
    const head = target.closest<HTMLElement>(".queue-row-head");
    if (!head || !li.contains(head)) return;
    toggleDetails(li, taskId);
  });

  function applyEvent(ev: QueueEvent): void {
    switch (ev.event) {
      case "task_queued": {
        taskRows.set(ev.id, {
          id: ev.id,
          kind: ev.kind,
          kind_summary: kindSummary(ev.kind),
          priority: ev.priority,
          shape: ev.shape,
          state: "queued",
          submitted_at_ms: ev.submitted_at_ms,
        });
        break;
      }
      case "task_leased": {
        const r = taskRows.get(ev.id);
        if (r) {
          r.state = "leased";
          r.worker = ev.worker;
          r.lease_expires_at_ms = ev.lease_expires_at_ms;
        }
        break;
      }
      case "task_heartbeat": {
        const r = taskRows.get(ev.id);
        if (r) r.lease_expires_at_ms = ev.lease_expires_at_ms;
        break;
      }
      case "task_completed": {
        const r = taskRows.get(ev.id);
        if (r) {
          r.state = "completed";
          r.worker = ev.worker;
          r.finished_at_ms = Date.now();
        }
        break;
      }
      case "task_failed": {
        const r = taskRows.get(ev.id);
        if (r) {
          r.state = "failed";
          r.worker = ev.worker;
          r.finished_at_ms = Date.now();
        }
        break;
      }
      case "task_cancelled": {
        const r = taskRows.get(ev.id);
        if (r) {
          r.state = "cancelled";
          r.finished_at_ms = Date.now();
        }
        break;
      }
    }
    if (visible) render();
  }

  async function seedSnapshot(): Promise<void> {
    try {
      const rows = await Ipc.tasksSnapshot<TaskRecord>();
      taskRows.clear();
      for (const r of rows) taskRows.set(r.id, r);
      render();
    } catch (e) {
      Logger.error("ui::queue-detail", "tasks_snapshot failed", { err: e });
    }
  }

  function applyIndexEvent(ev: ProgressEvent): void {
    indexSeq += 1;
    switch (ev.kind) {
      case "started": {
        indexRows.set(ev.path, { path: ev.path, state: "started", seq: indexSeq });
        break;
      }
      case "finished": {
        indexRows.set(ev.path, {
          path: ev.path,
          state: "finished",
          finished_at_ms: Date.now(),
          seq: indexSeq,
        });
        break;
      }
      case "skipped": {
        indexRows.set(ev.path, {
          path: ev.path,
          state: "skipped",
          reason: ev.reason,
          finished_at_ms: Date.now(),
          seq: indexSeq,
        });
        break;
      }
      case "deleted":
      case "scan_complete":
      case "model_loaded": {
        // No row state to update for these — they're informational.
        return;
      }
      case "renamed": {
        const prior = indexRows.get(ev.from);
        if (prior) {
          indexRows.delete(ev.from);
          indexRows.set(ev.to, { ...prior, path: ev.to, seq: indexSeq });
        }
        break;
      }
      case "error": {
        if (ev.path) {
          indexRows.set(ev.path, {
            path: ev.path,
            state: "errored",
            reason: ev.message,
            finished_at_ms: Date.now(),
            seq: indexSeq,
          });
        }
        break;
      }
    }
    // GC finished/skipped/errored past the cap (keep newest by seq).
    const terminals: IndexRow[] = [];
    for (const r of indexRows.values()) {
      if (r.state !== "started") terminals.push(r);
    }
    if (terminals.length > INDEX_FINISHED_CAP) {
      terminals.sort((a, b) => a.seq - b.seq);
      const drop = terminals.slice(0, terminals.length - INDEX_FINISHED_CAP);
      for (const r of drop) indexRows.delete(r.path);
    }
    if (visible) render();
  }

  async function start(): Promise<void> {
    unlistenQueue = await listen<QueueEvent>("hiker:queue-event", (ev) => {
      applyEvent(ev.payload);
    });
    unlistenIndex = await listen<ProgressEvent>(
      "hiker:reindex-progress",
      (ev) => applyIndexEvent(ev.payload),
    );
    await seedSnapshot();
  }
  void start();

  function priorityPill(p: Priority): string {
    const label = p === "high" ? "High" : p === "low" ? "Low" : "Norm";
    return `<span class="queue-pri queue-pri-${p}">${label}</span>`;
  }

  function statePill(state: TaskState): string {
    if (state === "leased") {
      return `<span class="queue-state queue-state-leased">…<span class="queue-pulse-dot"></span></span>`;
    }
    if (state === "completed") return `<span class="queue-state queue-state-ok">✓</span>`;
    if (state === "failed") return `<span class="queue-state queue-state-err">✗</span>`;
    if (state === "cancelled") return `<span class="queue-state queue-state-cancelled">∅</span>`;
    return `<span class="queue-state queue-state-queued">…</span>`;
  }

  function workerLabel(w?: WorkerKind): string {
    if (!w) return "";
    if (w.kind === "direct_llm") return "Direct LLM";
    if (w.kind === "indexer") return "Indexer";
    if (w.via === "in_process_chat_agent") return "Chat agent";
    return `External: ${w.client_id ?? "unknown"}`;
  }

  function kindSummary(k: TaskKind): string {
    // status: embedder-model-load-as-task
    // Show "Loading embedder model: <id>" so the user can see the
    // first-run download / hot-swap is actually doing something.
    if (k.type === "embedder_model_load" && typeof k.model_id === "string") {
      return `Loading embedder model: ${k.model_id}`;
    }
    if (typeof k.source_path === "string") return k.source_path;
    if (typeof k.cluster_id === "string") {
      return `cluster ${k.cluster_id}`;
    }
    if (k.type === "cluster_build_tree" && typeof k.name === "string") {
      return `build ${k.name}`;
    }
    if (k.type === "cluster_rebuild_tree" && typeof k.tree_id === "string") {
      return `rebuild ${k.tree_id}`;
    }
    if (
      k.type === "cluster_recluster_subtree"
      && typeof k.tree_id === "string"
      && typeof k.node_id === "string"
    ) {
      return `recluster ${k.tree_id}/${k.node_id}`;
    }
    return k.type;
  }

  // status: task-queue-row-details
  // Track which row's details panel is expanded (one at a time, like a
  // master/detail). Click the row to expand; click again or click
  // another row to swap.
  let expandedTaskId: string | null = null;

  // Pure builder for one task row. No event listeners attached — the
  // container-level click handler on `root` reads `data-task-id` /
  // `data-action="cancel"` to dispatch toggle-expand vs. cancel. Cancel
  // button carries the `data-action="cancel"` hook so the delegated
  // handler can short-circuit before toggling the row.
  function domForTaskRow(r: TaskRecord, terminal: boolean): HTMLElement {
    const li = document.createElement("li");
    li.className = "queue-row queue-row-clickable";
    li.dataset.taskId = r.id;
    const kindLabel = r.kind.type;
    const head = document.createElement("div");
    head.className = "queue-row-head";
    head.innerHTML = `
      ${priorityPill(r.priority)}
      <span class="queue-kind">${escapeHtml(kindLabel)}</span>
      <span class="queue-summary">${escapeHtml(r.kind_summary)}</span>
      ${statePill(r.state)}
      ${
        r.worker
          ? `<span class="queue-worker">worker: ${escapeHtml(workerLabel(r.worker))}</span>`
          : ""
      }
      ${
        terminal
          ? ""
          : `<button class="queue-cancel" data-action="cancel" title="Cancel" type="button">✕</button>`
      }
    `;
    li.appendChild(head);
    if (expandedTaskId === r.id) {
      // Re-render kept the row expanded — refetch + repaint the panel
      // so result/error update when the row terminalizes.
      void renderDetailsPanel(li, r.id);
    }
    return li;
  }

  function toggleDetails(rowEl: HTMLElement, taskId: string): void {
    if (expandedTaskId === taskId) {
      expandedTaskId = null;
      const panel = rowEl.querySelector(".queue-row-details");
      panel?.remove();
      rowEl.classList.remove("expanded");
      return;
    }
    // Collapse any other expanded row first.
    if (expandedTaskId) {
      const prev = root.querySelector(
        `li.queue-row[data-task-id="${cssEscapeAttr(expandedTaskId)}"]`,
      );
      prev?.querySelector(".queue-row-details")?.remove();
      prev?.classList.remove("expanded");
    }
    expandedTaskId = taskId;
    rowEl.classList.add("expanded");
    void renderDetailsPanel(rowEl, taskId);
  }

  function cssEscapeAttr(s: string): string {
    return s.replaceAll('"', '\\"');
  }

  interface TaskDetailsDto {
    id: string;
    prompt: string;
    inputs: unknown;
    metadata: unknown;
    output_schema?: unknown;
    result?: unknown;
    error?: string;
    state: string;
    finished_at_ms?: number;
    worker?: WorkerKind;
  }

  async function renderDetailsPanel(rowEl: HTMLElement, taskId: string): Promise<void> {
    let panel = rowEl.querySelector<HTMLDivElement>(".queue-row-details");
    if (!panel) {
      panel = document.createElement("div");
      panel.className = "queue-row-details";
      rowEl.appendChild(panel);
    }
    panel.innerHTML = `<div class="queue-detail-section queue-detail-loading">Loading…</div>`;
    let details: TaskDetailsDto | null;
    try {
      details = await Ipc.taskDetails<TaskDetailsDto>({ id: taskId });
    } catch (err) {
      panel.innerHTML = `<div class="queue-detail-section queue-detail-error">${escapeHtml(
        String(err),
      )}</div>`;
      return;
    }
    // Race protection: user collapsed or swapped while the IPC was in flight.
    if (expandedTaskId !== taskId) return;
    if (!details) {
      panel.innerHTML = `<div class="queue-detail-section">Task no longer in queue (retention window expired).</div>`;
      return;
    }
    const sections: string[] = [];
    sections.push(detailSection("Prompt", details.prompt, "queue-detail-pre"));
    if (details.error) {
      sections.push(
        detailSection("Error", details.error, "queue-detail-pre queue-detail-pre-err"),
      );
    }
    if (details.result !== undefined && details.result !== null) {
      const text = typeof details.result === "string"
        ? (details.result as string)
        : JSON.stringify(details.result, null, 2);
      sections.push(detailSection("Response", text, "queue-detail-pre"));
    }
    if (
      details.metadata
      && typeof details.metadata === "object"
      && Object.keys(details.metadata as object).length > 0
    ) {
      sections.push(
        detailSection(
          "Metadata",
          JSON.stringify(details.metadata, null, 2),
          "queue-detail-pre queue-detail-pre-meta",
        ),
      );
    }
    panel.innerHTML = sections.join("");
  }

  function detailSection(label: string, body: string, preClass: string): string {
    return `<div class="queue-detail-section">
      <div class="queue-detail-label">${escapeHtml(label)}</div>
      <pre class="${preClass}">${escapeHtml(body)}</pre>
    </div>`;
  }

  function escapeHtml(s: string): string {
    return s
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;");
  }

  // Pure builder for one embedding-queue row. Stateless, no listeners —
  // `core::indexer` rows have no per-row cancel (per `queue-detail-
  // embedding-row-shape`) and no expandable details, so the delegated
  // click handler on `root` simply ignores them (no `data-task-id`).
  function domForIndexRow(r: IndexRow): HTMLElement {
    const li = document.createElement("li");
    li.className = "queue-row queue-row-index";
    // Empty priority slot — the indexer doesn't have priorities; the
    // source badge identifies the row's lane.
    const sourceBadge = `<span class="queue-source-badge queue-source-index">index</span>`;
    const state = r.state;
    let pill: string;
    if (state === "started") {
      pill = `<span class="queue-state queue-state-leased">…<span class="queue-pulse-dot"></span></span>`;
    } else if (state === "finished") {
      pill = `<span class="queue-state queue-state-ok">✓</span>`;
    } else if (state === "skipped") {
      pill = `<span class="queue-state queue-state-cancelled">skip</span>`;
    } else {
      pill = `<span class="queue-state queue-state-err">✗</span>`;
    }
    li.innerHTML = `
      ${sourceBadge}
      <span class="queue-kind">indexing</span>
      <span class="queue-summary">${escapeHtml(r.path)}</span>
      ${pill}
      ${r.reason ? `<span class="queue-worker">${escapeHtml(r.reason)}</span>` : ""}
    `;
    // No per-row cancel button — `queue-detail-embedding-row-shape`:
    // cancelling embedding jobs individually isn't supported.
    return li;
  }

  function domForTaskRowWithBadge(r: TaskRecord, terminal: boolean): HTMLElement {
    const row = domForTaskRow(r, terminal);
    const badge = document.createElement("span");
    badge.className = "queue-source-badge queue-source-task";
    badge.textContent = "task";
    row.classList.add("queue-row-task");
    row.insertBefore(badge, row.firstChild);
    return row;
  }

  function render(): void {
    const showTasks = activeFilters.has("tasks");
    const showEmbedding = activeFilters.has("embedding");
    type ActiveItem =
      | { kind: "task"; row: TaskRecord }
      | { kind: "index"; row: IndexRow };
    type QueuedItem = { kind: "task"; row: TaskRecord };
    type FinishedItem =
      | { kind: "task"; row: TaskRecord }
      | { kind: "index"; row: IndexRow };

    const active: ActiveItem[] = [];
    const queued: QueuedItem[] = [];
    const finished: FinishedItem[] = [];

    if (showTasks) {
      for (const r of taskRows.values()) {
        if (r.state === "leased") active.push({ kind: "task", row: r });
        else if (r.state === "queued") queued.push({ kind: "task", row: r });
        else finished.push({ kind: "task", row: r });
      }
    }
    if (showEmbedding) {
      for (const r of indexRows.values()) {
        if (r.state === "started") active.push({ kind: "index", row: r });
        else finished.push({ kind: "index", row: r });
      }
    }

    const taskRank = (p: Priority) => (p === "high" ? 2 : p === "normal" ? 1 : 0);
    const sortDrainMixed = (a: ActiveItem, b: ActiveItem) => {
      // Tasks rank by priority then submitted_at; index rows fall to the
      // bottom of the bucket (they don't carry a priority).
      const ar = a.kind === "task" ? taskRank(a.row.priority) : -1;
      const br = b.kind === "task" ? taskRank(b.row.priority) : -1;
      if (ar !== br) return br - ar;
      const at =
        a.kind === "task" ? a.row.submitted_at_ms : a.row.seq;
      const bt =
        b.kind === "task" ? b.row.submitted_at_ms : b.row.seq;
      return at - bt;
    };
    active.sort(sortDrainMixed);
    queued.sort(
      (a, b) =>
        taskRank(b.row.priority) - taskRank(a.row.priority)
        || a.row.submitted_at_ms - b.row.submitted_at_ms,
    );
    finished.sort((a, b) => {
      const at = a.kind === "task" ? a.row.finished_at_ms ?? 0 : a.row.finished_at_ms ?? 0;
      const bt = b.kind === "task" ? b.row.finished_at_ms ?? 0 : b.row.finished_at_ms ?? 0;
      return bt - at;
    });

    const renderItem = (item: ActiveItem | FinishedItem | QueuedItem, terminal: boolean) => {
      if (item.kind === "task") return domForTaskRowWithBadge(item.row, terminal);
      return domForIndexRow(item.row);
    };

    const activeEl = root.querySelector<HTMLElement>('[data-list="active"]')!;
    const queuedEl = root.querySelector<HTMLElement>('[data-list="queued"]')!;
    const finishedEl = root.querySelector<HTMLElement>('[data-list="finished"]')!;
    activeEl.replaceChildren(...active.map((it) => renderItem(it, false)));
    queuedEl.replaceChildren(...queued.map((it) => renderItem(it, false)));
    finishedEl.replaceChildren(...finished.map((it) => renderItem(it, true)));
  }

  // Initial state: hidden. The DOM is arranged below before the
  // controller's `onSetVisible` is wired so the helper's `applyOnMount`
  // doesn't double-fire the seed-snapshot side effect.
  root.hidden = true;
  visible = false;

  const api: QueueDetailApi = {
    setFilter,
    refreshFromSettings,
    destroy: async () => {
      if (unlistenQueue) {
        unlistenQueue();
        unlistenQueue = null;
      }
      if (unlistenIndex) {
        unlistenIndex();
        unlistenIndex = null;
      }
    },
  };

  return createPanelController<QueueDetailApi>(api, {
    initialVisible: false,
    applyOnMount: false,
    onSetVisible: (on) => {
      visible = on;
      root.hidden = !on;
      if (on) void seedSnapshot();
    },
  });
}
