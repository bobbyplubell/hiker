// status: vault-home-screen
// status: vault-home-button
// status: vault-home-detail-views
// status: vault-home-recent-activity-widget
// status: vault-home-recent-activity-detail
// status: vault-home-recent-activity-author-filter
// status: vault-home-recent-activity-unrollback
//
// Vault home view: stats / recently modified / recently accessed widgets +
// recent-activity tile that expands into a filtered detail pane. Owns the
// activity row cache, recently-restored highlight, author-filter set, and
// stats-refresh debounce. Snapshot preview / note open are routed back to
// the host via callbacks so this module never touches the editor.

import { invoke } from "@tauri-apps/api/core";
import type { ChangeRow, ChangeOp } from "../snapshotPreview";

interface VaultHomeStats {
  total_notes: number;
  total_chunks: number;
  indexed: number;
  skipped: number;
  queued: number;
}
interface RecentNote {
  path: string;
  title: string;
  mtime: number;
  last_accessed_at: number | null;
}
interface RollbackOutcome {
  prior_change_id: number;
  path: string;
  new_hash: string;
}

type DetailView = null | { kind: "recent-activity" };

export interface VaultHomeDeps {
  editorPaneEl: HTMLElement;
  vaultHomeEl: HTMLElement;
  homeBtn: HTMLButtonElement;
  vaultPathEl: HTMLElement;
  titleEl: HTMLElement;
  statsBodyEl: HTMLElement;
  modifiedListEl: HTMLElement;
  accessedListEl: HTMLElement;
  newNoteBtn: HTMLButtonElement;
  overviewEl: HTMLElement;
  detailEl: HTMLElement;
  detailTitleEl: HTMLElement;
  detailCountEl: HTMLElement;
  detailListEl: HTMLElement;
  detailFiltersEl: HTMLElement;
  activitySectionEl: HTMLElement;
  activityHeaderEl: HTMLElement;
  activityListEl: HTMLElement;
  formatError: (err: unknown) => string;
  getVaultIsOpen: () => boolean;
  /// Opening a note exits home view per spec ("clicking any tree row,
  /// recents entry, or search result restores the editor"). Host owns the
  /// editor state.
  onOpenNote: (
    rel: string,
    opts?: { preview?: boolean },
  ) => void | Promise<void>;
  /// Open a snapshot read-only in the editor. Host wires this to
  /// `snapshotPreview.open`.
  onOpenSnapshot: (row: ChangeRow) => void | Promise<void>;
  /// Hook fired before the home view becomes visible. Host uses it to drop
  /// the settings pane (mutually exclusive sub-modes).
  onBeforeShow?: () => void;
}

export interface VaultHomeApi {
  isVisible(): boolean;
  setVisible(on: boolean): void;
  refresh(): Promise<void>;
  showDetail(kind: "recent-activity"): void;
  /// Fired on every `hiker:changes-appended` event. No-op when home isn't
  /// visible — the next refresh on show will pick the new rows up.
  notifyChangesAppended(): void;
  /// Fired by the watcher when an external mtime change might shift the
  /// recently-modified list.
  notifyRecentModified(): void;
  /// Fired on every `hiker:reindex-progress` / `hiker:index-status` event so
  /// the stats tile reflects the new model_ready / total_notes / queued.
  scheduleStatsRefresh(): void;
  /// Restore a snapshot to disk; host shouldn't usually call this — the
  /// detail view's rows do — but the snapshot-preview's `onRestore` hook
  /// re-routes here.
  doRestoreSnapshot(row: ChangeRow): Promise<void>;
  activeDetailView(): DetailView;
}

function relativeTime(unixSecs: number): string {
  const now = Math.floor(Date.now() / 1000);
  const d = now - unixSecs;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)}m ago`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ago`;
  if (d < 86400 * 2) return "yesterday";
  if (d < 86400 * 7) return `${Math.floor(d / 86400)}d ago`;
  const date = new Date(unixSecs * 1000);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

function authorPillIcon(cls: string): string {
  // status: recent-activity-human-icon, recent-activity-agent-icon
  if (cls === "user") {
    return `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><circle cx="8" cy="5.5" r="2.4"/><path d="M3.5 13.5c0-2.4 2-4 4.5-4s4.5 1.6 4.5 4"/></svg>`;
  }
  if (cls === "agent") {
    return `<svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><rect x="3" y="6" width="10" height="7" rx="1.5"/><line x1="8" y1="3.5" x2="8" y2="6"/><circle cx="8" cy="3" r="0.6" fill="currentColor"/><circle cx="6" cy="9.2" r="0.7" fill="currentColor"/><circle cx="10" cy="9.2" r="0.7" fill="currentColor"/><line x1="6" y1="11.5" x2="10" y2="11.5"/></svg>`;
  }
  return `<svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true"><circle cx="8" cy="8" r="3" fill="currentColor"/></svg>`;
}

function opLabel(op: ChangeOp): string {
  return op;
}

export function mountVaultHome(deps: VaultHomeDeps): VaultHomeApi {
  let activeDetailView: DetailView = null;
  const activeAuthorFilters: Set<string> = new Set();
  let allFiltersOnce = false;
  let recentlyRestoredFromId: number | null = null;
  let activityRows: ChangeRow[] = [];
  let statsRefreshTimer: ReturnType<typeof setTimeout> | null = null;

  function isVisible(): boolean {
    return deps.editorPaneEl.classList.contains("home-view");
  }

  function setVisible(on: boolean): void {
    if (on) deps.onBeforeShow?.();
    deps.editorPaneEl.classList.toggle("home-view", on);
    deps.vaultHomeEl.hidden = !on;
    deps.homeBtn.classList.toggle("active", on);
    if (on) void refresh();
  }

  async function refresh(): Promise<void> {
    if (!deps.getVaultIsOpen()) return;
    deps.titleEl.textContent = deps.vaultPathEl.textContent || "Vault";
    showHomeOverview();
    await Promise.all([
      refreshStats(),
      refreshRecentModified(),
      refreshRecentAccessed(),
      refreshActivityWidget(),
    ]);
  }

  async function refreshStats(): Promise<void> {
    try {
      const stats = await invoke<VaultHomeStats>("vault_home_stats");
      renderStats(stats);
    } catch (err) {
      console.error("vault_home_stats failed:", err);
      deps.statsBodyEl.replaceChildren(
        buildStatEmpty(`Failed to load stats: ${deps.formatError(err)}`),
      );
    }
  }

  function renderStats(stats: VaultHomeStats): void {
    const cells: Array<[string, number]> = [
      ["Notes", stats.total_notes],
      ["Indexed", stats.indexed],
      ["Chunks", stats.total_chunks],
      ["Queued", stats.queued],
      ["Skipped", stats.skipped],
    ];
    deps.statsBodyEl.replaceChildren(
      ...cells.map(([label, num]) => {
        const cell = document.createElement("div");
        cell.className = "vault-home-stat";
        const numEl = document.createElement("div");
        numEl.className = "num";
        numEl.textContent = String(num);
        const lbl = document.createElement("div");
        lbl.className = "label";
        lbl.textContent = label;
        cell.append(numEl, lbl);
        return cell;
      }),
    );
  }

  function buildStatEmpty(text: string): HTMLElement {
    const el = document.createElement("div");
    el.className = "vault-home-stat-empty";
    el.textContent = text;
    return el;
  }

  async function refreshRecentModified(): Promise<void> {
    try {
      const rows = await invoke<RecentNote[]>("recent_notes_modified", { limit: 10 });
      renderRecentList(deps.modifiedListEl, rows, "mtime", "No notes indexed yet.");
    } catch (err) {
      console.error("recent_notes_modified failed:", err);
      renderRecentList(deps.modifiedListEl, [], "mtime", `Error: ${deps.formatError(err)}`);
    }
  }

  async function refreshRecentAccessed(): Promise<void> {
    try {
      const rows = await invoke<RecentNote[]>("recent_notes_accessed", { limit: 10 });
      renderRecentList(deps.accessedListEl, rows, "accessed", "No recently opened notes.");
    } catch (err) {
      console.error("recent_notes_accessed failed:", err);
      renderRecentList(deps.accessedListEl, [], "accessed", `Error: ${deps.formatError(err)}`);
    }
  }

  function renderRecentList(
    ul: HTMLElement,
    rows: RecentNote[],
    field: "mtime" | "accessed",
    emptyText: string,
  ): void {
    if (rows.length === 0) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = emptyText;
      ul.replaceChildren(li);
      return;
    }
    ul.replaceChildren(
      ...rows.map((r) => {
        const li = document.createElement("li");
        li.dataset.path = r.path;
        const ts = field === "mtime" ? r.mtime : (r.last_accessed_at ?? r.mtime);
        const when = relativeTime(ts);
        const nameEl = document.createElement("span");
        nameEl.className = "name";
        nameEl.textContent = r.title;
        const relEl = document.createElement("span");
        relEl.className = "rel";
        const parent = r.path.includes("/") ? r.path.slice(0, r.path.lastIndexOf("/")) : "";
        relEl.textContent = parent;
        const whenEl = document.createElement("span");
        whenEl.className = "when";
        whenEl.textContent = when;
        whenEl.title = new Date(ts * 1000).toLocaleString();
        li.append(nameEl, relEl, whenEl);
        // status: editor-preview-tab-from-open-callsites
        // status: editor-preview-tab-mod-click-sticky
        li.addEventListener("click", (e) => {
          const sticky = e.metaKey || e.ctrlKey;
          void deps.onOpenNote(r.path, { preview: !sticky });
        });
        ul.appendChild(li);
        return li;
      }),
    );
  }

  function scheduleStatsRefresh(): void {
    if (!isVisible()) return;
    if (statsRefreshTimer !== null) clearTimeout(statsRefreshTimer);
    statsRefreshTimer = setTimeout(() => {
      statsRefreshTimer = null;
      void refreshStats();
    }, 250);
  }

  function showHomeOverview(): void {
    activeDetailView = null;
    deps.overviewEl.hidden = false;
    deps.detailEl.hidden = true;
  }

  function showDetail(kind: "recent-activity"): void {
    activeDetailView = { kind };
    deps.overviewEl.hidden = true;
    deps.detailEl.hidden = false;
    if (kind === "recent-activity") {
      deps.detailTitleEl.textContent = "Recent activity";
      void refreshActivityDetail();
    }
  }

  async function refreshActivityWidget(): Promise<void> {
    if (!deps.getVaultIsOpen()) return;
    if (!isVisible()) return;
    let count = 0;
    try {
      count = await invoke<number>("changes_count");
    } catch (err) {
      console.error("changes_count failed:", err);
    }
    if (count <= 0) {
      deps.activitySectionEl.hidden = true;
      return;
    }
    deps.activitySectionEl.hidden = false;
    deps.activityHeaderEl.textContent = `Recent activity (${count})`;
    let rows: ChangeRow[] = [];
    try {
      rows = await invoke<ChangeRow[]>("recent_changes", { limit: 5 });
    } catch (err) {
      console.error("recent_changes failed:", err);
    }
    deps.activityListEl.replaceChildren(
      ...rows.map((r) => buildActivityPreviewRow(r)),
    );
    deps.activityHeaderEl.style.cursor = "pointer";
    deps.activityHeaderEl.onclick = () => showDetail("recent-activity");
  }

  function buildActivityPreviewRow(r: ChangeRow): HTMLElement {
    const li = document.createElement("li");
    const op = document.createElement("span");
    op.className = "activity-op";
    op.textContent = opLabel(r.op);
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = r.path.split("/").pop() ?? r.path;
    const rel = document.createElement("span");
    rel.className = "rel";
    rel.textContent = r.path.includes("/")
      ? r.path.slice(0, r.path.lastIndexOf("/"))
      : "";
    const right = document.createElement("span");
    right.className = "row-right";
    const when = document.createElement("span");
    when.className = "when";
    when.textContent = relativeTime(Math.floor(r.timestamp_ms / 1000));
    when.title = new Date(r.timestamp_ms).toLocaleString();
    const cls = r.author_class;
    const author = document.createElement("span");
    author.className = "activity-author";
    author.innerHTML = authorPillIcon(cls);
    author.title = r.author;
    const idEl = document.createElement("span");
    idEl.className = "activity-id";
    idEl.textContent = `#${r.id}`;
    idEl.title = `Snapshot id ${r.id}`;
    right.append(when, author, idEl);

    li.append(op, name);
    const meta = r.metadata as Record<string, unknown>;
    const src = (meta?.["restored_from"] ?? meta?.["rolled_back_from"]) as
      | number
      | undefined;
    if (src !== undefined) {
      const badge = document.createElement("span");
      badge.className = "rollback-badge";
      badge.textContent = `↩ #${src}`;
      badge.title = `This save was a Restore of snapshot #${src}`;
      li.appendChild(badge);
    }
    li.append(rel, right);
    li.addEventListener("click", () => showDetail("recent-activity"));
    return li;
  }

  async function refreshActivityDetail(): Promise<void> {
    try {
      activityRows = await invoke<ChangeRow[]>("recent_changes", { limit: 200 });
    } catch (err) {
      console.error("recent_changes failed:", err);
      activityRows = [];
    }
    renderActivityDetail();
  }

  function renderActivityDetail(): void {
    const presentClasses = new Set<string>();
    for (const r of activityRows) presentClasses.add(r.author_class);

    const ALWAYS_SHOW: readonly string[] = ["user", "agent"];
    const allClasses = new Set<string>([...ALWAYS_SHOW, ...presentClasses]);

    if (!allFiltersOnce) {
      activeAuthorFilters.clear();
      for (const c of allClasses) activeAuthorFilters.add(c);
      allFiltersOnce = true;
    }

    deps.detailFiltersEl.replaceChildren();
    const sortedClasses = [...allClasses].sort();
    for (const cls of sortedClasses) {
      const pill = document.createElement("button");
      pill.className = "filter-pill toolbar-btn";
      pill.type = "button";
      if (activeAuthorFilters.has(cls)) pill.classList.add("active");
      const hasRows = presentClasses.has(cls);
      if (!hasRows) pill.classList.add("empty");
      pill.innerHTML = authorPillIcon(cls);
      pill.classList.add("filter-pill-icon-only");
      pill.setAttribute("aria-label", `Toggle ${cls} activity`);
      pill.title = hasRows
        ? `Show ${cls} activity`
        : `No ${cls} activity in the recent window yet`;
      pill.addEventListener("click", () => {
        if (activeAuthorFilters.has(cls)) {
          activeAuthorFilters.delete(cls);
        } else {
          activeAuthorFilters.add(cls);
        }
        renderActivityDetail();
      });
      deps.detailFiltersEl.appendChild(pill);
    }

    const visible = activityRows.filter((r) =>
      activeAuthorFilters.has(r.author_class),
    );
    deps.detailCountEl.textContent = `${visible.length} of ${activityRows.length}`;

    deps.detailListEl.replaceChildren(
      ...visible.map((r) => buildActivityDetailRow(r)),
    );
  }

  function buildActivityDetailRow(r: ChangeRow): HTMLElement {
    const li = document.createElement("li");
    const isRestoreRow =
      r.metadata && typeof r.metadata === "object" &&
      ("restored_from" in r.metadata || "rolled_back_from" in r.metadata);
    const isCurrent = r.is_current;
    if (recentlyRestoredFromId === r.id) {
      li.classList.add("recently-rolled-back");
    }

    const canPreview = r.op !== "deleted";
    if (canPreview) {
      li.classList.add("clickable");
      li.style.cursor = "pointer";
      li.addEventListener("click", (e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        void deps.onOpenSnapshot(r);
      });
    }

    const line = document.createElement("div");
    line.className = "row-line";

    const op = document.createElement("span");
    op.className = "activity-op";
    op.textContent = opLabel(r.op);

    const name = document.createElement("span");
    name.className = "name";
    name.textContent = r.path;
    if (r.rename_from) name.title = `renamed from ${r.rename_from}`;

    const right = document.createElement("span");
    right.className = "row-right";

    const when = document.createElement("span");
    when.className = "when";
    when.textContent = relativeTime(Math.floor(r.timestamp_ms / 1000));
    when.title = new Date(r.timestamp_ms).toLocaleString();

    const cls = r.author_class;
    const author = document.createElement("span");
    author.className = "activity-author";
    author.innerHTML = authorPillIcon(cls);
    author.title = r.author;

    const idEl = document.createElement("span");
    idEl.className = "activity-id";
    idEl.textContent = `#${r.id}`;
    idEl.title = `Snapshot id ${r.id}`;

    line.append(op, name);
    if (isRestoreRow) {
      const meta = r.metadata as Record<string, unknown>;
      const src = (meta["restored_from"] ?? meta["rolled_back_from"]) as
        | number
        | undefined;
      const badge = document.createElement("span");
      badge.className = "rollback-badge";
      badge.textContent =
        src !== undefined ? `↩ restored from #${src}` : "↩ restored";
      badge.title =
        src !== undefined
          ? `This save wrote the content of snapshot #${src} back to disk`
          : "This save was a Restore";
      line.appendChild(badge);
    }
    if (isCurrent) {
      const cur = document.createElement("span");
      cur.className = "rollback-badge";
      cur.textContent = "current";
      cur.title = "This is the file's current state on disk";
      line.appendChild(cur);
    }
    right.append(when, author, idEl);
    line.append(right);
    li.appendChild(line);

    if (canPreview && !isCurrent) {
      const actions = document.createElement("div");
      actions.className = "row-actions";

      const restoreBtn = document.createElement("button");
      restoreBtn.className = "row-action";
      restoreBtn.textContent = "Restore this version";
      restoreBtn.title =
        "Write this snapshot's contents back to the file. Append-only — the restore is itself logged as a new modified event.";
      restoreBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        void doRestoreSnapshot(r);
      });
      actions.appendChild(restoreBtn);

      if (recentlyRestoredFromId === r.id) {
        const prompt = document.createElement("span");
        prompt.className = "un-rollback-prompt";
        prompt.textContent = "← previous state — click Restore to undo";
        actions.appendChild(prompt);
      }

      li.appendChild(actions);
    }
    return li;
  }

  async function doRestoreSnapshot(row: ChangeRow): Promise<void> {
    if (
      !confirm(
        `Restore ${row.path} to the version saved at ${new Date(
          row.timestamp_ms,
        ).toLocaleString()}?\n\nThe current state stays in the log; this Restore is itself a new logged event.`,
      )
    ) {
      return;
    }
    const wasCurrentRow = activityRows.find(
      (r) => r.path === row.path && r.is_current,
    );
    const wasCurrentId = wasCurrentRow?.id ?? null;
    try {
      await invoke<RollbackOutcome>("restore_snapshot", { changeId: row.id });
      recentlyRestoredFromId = wasCurrentId;
      await refreshActivityDetail();
    } catch (err) {
      alert(`restore failed: ${deps.formatError(err)}`);
    }
  }

  function notifyChangesAppended(): void {
    if (!isVisible()) return;
    void refreshActivityWidget();
    void refreshRecentModified();
    if (activeDetailView?.kind === "recent-activity") {
      void refreshActivityDetail();
    }
  }

  function notifyRecentModified(): void {
    if (!isVisible()) return;
    void refreshRecentModified();
  }

  // Wire the home button + new-note button.
  deps.homeBtn.addEventListener("click", () => {
    setVisible(!isVisible());
  });
  deps.newNoteBtn.addEventListener("click", async () => {
    try {
      const created = await invoke<string>("create_note", { folder: "" });
      await deps.onOpenNote(created);
    } catch (err) {
      console.error("vault-home new note failed:", err);
      alert(`new note failed: ${deps.formatError(err)}`);
    }
  });

  return {
    isVisible,
    setVisible,
    refresh,
    showDetail,
    notifyChangesAppended,
    notifyRecentModified,
    scheduleStatsRefresh,
    doRestoreSnapshot,
    activeDetailView: () => activeDetailView,
  };
}
