// status: vault-home-screen
// status: vault-home-button
// status: vault-home-detail-views
// status: vault-home-recent-activity-widget
// status: vault-home-recent-activity-detail
// status: vault-home-recent-activity-author-filter
// status: vault-home-recent-activity-unrollback
// status: staging-review-activity-detail-filter
// status: staging-bulk-apply-reject
//
// Vault home view: stats / recently modified / recently accessed widgets +
// recent-activity tile that expands into a filtered detail pane. Owns the
// activity row cache, recently-restored highlight, author-filter set, and
// stats-refresh debounce. Snapshot preview / note open are routed back to
// the host via callbacks so this module never touches the editor.

import type { ChangeRow, ChangeOp } from "../snapshotPreview";
import {
  Ipc,
  type VaultHomeStats,
  type RecentNote,
  type Proposal,
  type ActivityItem,
} from "../ipc";
import { Logger } from "../logger";
import { Icons } from "../icons";
import { el } from "../widgets/dom";
import {
  createPanelController,
  type PanelController,
  type PanelDeps,
} from "../panels/controller";

type DetailView = null | { kind: "recent-activity" };

type TimelineRow =
  | { kind: "change"; row: ChangeRow }
  | { kind: "proposal"; row: Proposal };

function timelineAuthorClass(t: TimelineRow): string {
  if (t.kind === "change") return t.row.author_class;
  const cls = t.row.metadata?.author_class;
  return typeof cls === "string" ? cls : "agent";
}

function timelineTimestampMs(t: TimelineRow): number {
  return t.kind === "change" ? t.row.timestamp_ms : t.row.created_at_ms;
}

export interface VaultHomeDeps extends PanelDeps {
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
  getVaultIsOpen: () => boolean;
  /// Open a snapshot read-only in the editor. Host wires this to
  /// `snapshotPreview.open`.
  onOpenSnapshot: (row: ChangeRow) => void | Promise<void>;
  /// Hook fired before the home view becomes visible. Host uses it to drop
  /// the settings pane (mutually exclusive sub-modes).
  onBeforeShow?: () => void;
  // status: tab-kinds
  /// Open a app-page tab (e.g. home-detail for activity-widget clicks).
  onOpenPage?: (kind: string, payload?: Record<string, string>) => void;
  // status: staging-review-activity-detail-filter
  /// Open a staging proposal as a read-only preview buffer (reuses the
  /// snapshot-preview diff-toggle pattern).
  onOpenStagingProposal: (proposal: Proposal) => void | Promise<void>;
  /// Accept a proposal — calls `Ipc.stagingAccept` + refreshes.
  onAcceptStaging: (proposal: Proposal) => Promise<void>;
  /// Reject a proposal — calls `Ipc.stagingReject` + refreshes.
  onRejectStaging: (proposal: Proposal) => Promise<void>;
}

export interface VaultHomeApi {
  // `isVisible` / `setVisible` live on the `PanelController` wrapper,
  // not the api — host code reads `vaultHome.isVisible()` /
  // `vaultHome.setVisible(on)` directly off the controller.
  refresh(): Promise<void>;
  showDetail(kind: "recent-activity"): void;
  /// Fired on every `hiker:changes-appended` event. No-op when home isn't
  /// visible — the next refresh on show will pick the new rows up.
  notifyChangesAppended(): void;
  /// Fired on every `hiker:staging-changed` event so the recent-activity
  /// widget's pending rows appear/disappear live. No-op when home isn't
  /// visible — the on-show refresh picks the new state up.
  notifyStagingChanged(): void;
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
  if (cls === "user") return Icons.user();
  if (cls === "agent") return Icons.robot({ size: 12, strokeWidth: 1.4 });
  return Icons.dot();
}

/// status: patch-review-per-hunk-accept
/// Activity-row Accept / Reject icon button. Rounded rectangle, no fill,
/// accent (green) or danger (red) tint on hover only — matches the
/// existing toolbar-btn type ramp.
function makeIconButton(
  kind: "accept" | "reject",
  label: string,
  title: string,
  onClick: (e: MouseEvent) => void | Promise<void>,
): HTMLButtonElement {
  return el("button", {
    class: `row-action row-action-icon row-action-${kind}`,
    title,
    attrs: { type: "button", "aria-label": label },
    html:
      kind === "accept"
        ? `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><polyline points="3,8 7,12 13,4"/></svg>`
        : `<svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round" stroke-linecap="round" aria-hidden="true"><line x1="4" y1="4" x2="12" y2="12"/><line x1="12" y1="4" x2="4" y2="12"/></svg>`,
    onClick: (e) => void onClick(e),
  });
}

function opLabel(op: ChangeOp): string {
  return op;
}

export type VaultHomeController = PanelController<VaultHomeApi>;

// Vault home owns its own visibility — `setVisible(on)` toggles the
// `home-view` class on the editor pane, hides the panel root, and syncs
// the home toolbar button. The controller's `setVisible` routes through
// the shared `onSetVisible` hook so the home button click and any
// future host-driven flip share one path. `isVisible` reads the editor
// pane's class rather than a tracked boolean — a few legacy host paths
// still toggle it directly, and reading the DOM ensures the controller
// stays in sync if that happens.
export function mountVaultHome(deps: VaultHomeDeps): VaultHomeController {
  let activeDetailView: DetailView = null;
  const activeAuthorFilters: Set<string> = new Set();
  let allFiltersOnce = false;
  let recentlyRestoredFromId: number | null = null;
  let activityRows: ChangeRow[] = [];
  let proposals: Proposal[] = [];
  let statsRefreshTimer: ReturnType<typeof setTimeout> | null = null;

  function isVisible(): boolean {
    // status: tab-kinds — visibility is driven by the `hidden` attribute
    // set/cleared by renderActiveTab() in main.ts.
    return !deps.vaultHomeEl.hidden;
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
      const stats = await Ipc.vaultHomeStats();
      renderStats(stats);
    } catch (err) {
      Logger.error("ui::vault-home", "vault_home_stats failed", { err });
      deps.statsBodyEl.replaceChildren(
        buildStatEmpty(`Failed to load stats: ${deps.formatErr(err)}`),
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
      ...cells.map(([label, num]) =>
        el("div", { class: "vault-home-stat" }, [
          el("div", { class: "num", text: String(num) }),
          el("div", { class: "label", text: label }),
        ]),
      ),
    );
  }

  function buildStatEmpty(text: string): HTMLElement {
    return el("div", { class: "vault-home-stat-empty", text });
  }

  async function refreshRecentModified(): Promise<void> {
    try {
      const rows = await Ipc.recentNotesModified({ limit: 10 });
      renderRecentList(deps.modifiedListEl, rows, "mtime", "No notes indexed yet.");
    } catch (err) {
      Logger.error("ui::vault-home", "recent_notes_modified failed", { err });
      renderRecentList(deps.modifiedListEl, [], "mtime", `Error: ${deps.formatErr(err)}`);
    }
  }

  async function refreshRecentAccessed(): Promise<void> {
    try {
      const rows = await Ipc.recentNotesAccessed({ limit: 10 });
      renderRecentList(deps.accessedListEl, rows, "accessed", "No recently opened notes.");
    } catch (err) {
      Logger.error("ui::vault-home", "recent_notes_accessed failed", { err });
      renderRecentList(deps.accessedListEl, [], "accessed", `Error: ${deps.formatErr(err)}`);
    }
  }

  // Pure builder for one recent-notes row. No event listeners attached
  // here — click handling rides container-level delegation on the host
  // `<ul>` (see `attachRecentListDelegation`). `data-path` carries the
  // hit's vault-relative path so the delegated handler can resolve back
  // to it via `closest("[data-path]")`.
  function domForRecentRow(r: RecentNote, field: "mtime" | "accessed"): HTMLElement {
    const ts = field === "mtime" ? r.mtime : (r.last_accessed_at ?? r.mtime);
    const parent = r.path.includes("/") ? r.path.slice(0, r.path.lastIndexOf("/")) : "";
    return el("li", { data: { path: r.path } }, [
      el("span", { class: "name", text: r.title }),
      el("span", { class: "rel", text: parent }),
      el("span", {
        class: "when",
        text: relativeTime(ts),
        title: new Date(ts * 1000).toLocaleString(),
      }),
    ]);
  }

  function onRecentListClick(e: MouseEvent): void {
    const target = e.target as HTMLElement | null;
    if (!target) return;
    const row = target.closest<HTMLElement>("li[data-path]");
    if (!row) return;
    const rel = row.dataset.path;
    if (!rel) return;
    const sticky = e.metaKey || e.ctrlKey;
    void deps.openNote(rel, { preview: !sticky });
  }

  // Container-level click delegation. Attached once per list element on
  // first render to avoid stacking listeners across `replaceChildren`
  // calls.
  const wiredRecentLists = new WeakSet<HTMLElement>();
  function ensureRecentListDelegation(ul: HTMLElement): void {
    if (wiredRecentLists.has(ul)) return;
    ul.addEventListener("click", onRecentListClick);
    wiredRecentLists.add(ul);
  }

  function renderRecentList(
    ul: HTMLElement,
    rows: RecentNote[],
    field: "mtime" | "accessed",
    emptyText: string,
  ): void {
    ensureRecentListDelegation(ul);
    if (rows.length === 0) {
      ul.replaceChildren(el("li", { class: "empty", text: emptyText }));
      return;
    }
    ul.replaceChildren(...rows.map((r) => domForRecentRow(r, field)));
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

  // status: vault-home-detail-views, tab-kinds
  // Drill-in entry points (header click on the recent-activity widget, row
  // click on a preview row) route through the host's `onOpenPage` callback
  // so the detail view opens as its own `home-detail` app-page tab — same
  // shape as queue / settings. That's what lets the nav stack record the
  // transition from `home` → `home-detail` so Back from the detail view
  // lands on home (rather than skipping past it to the previously-open
  // note). Falls back to a direct `showDetail` call when no host wiring is
  // present (defensive; main.ts always wires `onOpenPage` in v1).
  function requestRecentActivityDetail(): void {
    if (deps.onOpenPage) {
      deps.onOpenPage("home-detail", { view: "recent-activity" });
    } else {
      showDetail("recent-activity");
    }
  }

  // status: vault-home-recent-activity-widget
  // Consumes the unified `core::activity` feed so pending staging
  // proposals appear alongside committed change rows — same merged DTO
  // the editor status-bar version dropdown
  // (`status-bar-version-dropdown-uses-unified-feed`) and the activity
  // detail page (`activity-feed-activity-detail-consumer`) drive off.
  // Pending rows render with inline Accept/Reject affordances per
  // `staging-accept-reject-from-activity-detail`.
  async function refreshActivityWidget(): Promise<void> {
    if (!deps.getVaultIsOpen()) return;
    if (!isVisible()) return;
    let count = 0;
    try {
      count = await Ipc.activityCount({ source: "merged" });
    } catch (err) {
      Logger.error("ui::vault-home", "activity_count failed", { err });
    }
    if (count <= 0) {
      deps.activitySectionEl.hidden = true;
      return;
    }
    deps.activitySectionEl.hidden = false;
    deps.activityHeaderEl.textContent = `Recent activity (${count})`;
    let items: ActivityItem[] = [];
    try {
      items = await Ipc.activityList({ source: "merged", limit: 5 });
    } catch (err) {
      Logger.error("ui::vault-home", "activity_list failed", { err });
    }
    deps.activityListEl.replaceChildren(
      ...items.map((it) => buildActivityPreviewItem(it)),
    );
    deps.activityHeaderEl.style.cursor = "pointer";
    deps.activityHeaderEl.onclick = () => requestRecentActivityDetail();
  }

  function proposalFromStagingPayload(
    s: Extract<ActivityItem["payload"], { kind: "staging" }>,
  ): Proposal {
    const meta =
      s.metadata && typeof s.metadata === "object" && !Array.isArray(s.metadata)
        ? (s.metadata as Record<string, unknown>)
        : null;
    return {
      id: s.id,
      surface: s.surface,
      action: s.action,
      target_path: s.target_path,
      trail_id: s.trail_id,
      content_hash: s.content_hash,
      created_at_ms: s.created_at_ms,
      metadata: meta,
    };
  }

  function buildActivityPreviewItem(it: ActivityItem): HTMLElement {
    if (it.payload.kind === "staging") {
      const proposal = proposalFromStagingPayload(it.payload);
      return buildPendingPreviewRow(proposal);
    }
    const { kind: _k, ...row } = it.payload;
    return buildActivityPreviewRow(row as ChangeRow);
  }

  // Compact pending-proposal preview row for the home widget tile.
  // Shape mirrors `buildActivityPreviewRow` (change-row preview) so the
  // two row kinds align visually in the same list, with inline
  // Accept/Reject affordances per
  // `staging-accept-reject-from-activity-detail`.
  function buildPendingPreviewRow(p: Proposal): HTMLElement {
    const authorClass =
      typeof p.metadata?.author_class === "string"
        ? p.metadata.author_class
        : "agent";
    const acceptBtn = makeIconButton(
      "accept",
      "Accept",
      "Drift-check the proposed write against the current file and apply.",
      async (e) => {
        e.stopPropagation();
        try {
          await deps.onAcceptStaging(p);
        } catch (err) {
          alert(`Accept failed: ${deps.formatErr(err)}`);
          return;
        }
        await refreshActivityWidget();
      },
    );
    const rejectBtn = makeIconButton(
      "reject",
      "Reject",
      "Discard the proposal without writing. No changelog row.",
      async (e) => {
        e.stopPropagation();
        try {
          await deps.onRejectStaging(p);
        } catch (err) {
          alert(`Reject failed: ${deps.formatErr(err)}`);
          return;
        }
        await refreshActivityWidget();
      },
    );
    return el("li", {
      class: "clickable",
      onClick: (e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        void deps.onOpenStagingProposal(p);
      },
    }, [
      el("span", { class: "activity-op", text: "pending" }),
      el("span", {
        class: "name",
        text: p.target_path.split("/").pop() ?? p.target_path,
        title: p.trail_id ? `Trail: ${p.trail_id}` : undefined,
      }),
      el("span", {
        class: "rel",
        text: p.target_path.includes("/")
          ? p.target_path.slice(0, p.target_path.lastIndexOf("/"))
          : "",
      }),
      el("span", { class: "row-right" }, [
        el("span", {
          class: "when",
          text: relativeTime(Math.floor(p.created_at_ms / 1000)),
          title: new Date(p.created_at_ms).toLocaleString(),
        }),
        el("span", {
          class: "activity-author",
          html: authorPillIcon(authorClass),
          title: `Pending (${p.surface})`,
        }),
        acceptBtn,
        rejectBtn,
      ]),
    ]);
  }

  function buildActivityPreviewRow(r: ChangeRow): HTMLElement {
    const meta = r.metadata as Record<string, unknown>;
    const src = (meta?.["restored_from"] ?? meta?.["rolled_back_from"]) as
      | number
      | undefined;
    const right = el("span", { class: "row-right" }, [
      el("span", {
        class: "when",
        text: relativeTime(Math.floor(r.timestamp_ms / 1000)),
        title: new Date(r.timestamp_ms).toLocaleString(),
      }),
      el("span", {
        class: "activity-author",
        html: authorPillIcon(r.author_class),
        title: r.author,
      }),
      el("span", {
        class: "activity-id",
        text: `#${r.id}`,
        title: `Snapshot id ${r.id}`,
      }),
    ]);
    return el("li", {
      onClick: () => requestRecentActivityDetail(),
    }, [
      el("span", { class: "activity-op", text: opLabel(r.op) }),
      el("span", { class: "name", text: r.path.split("/").pop() ?? r.path }),
      src !== undefined
        ? el("span", {
            class: "rollback-badge",
            text: `↩ #${src}`,
            title: `This save was a Restore of snapshot #${src}`,
          })
        : null,
      el("span", {
        class: "rel",
        text: r.path.includes("/")
          ? r.path.slice(0, r.path.lastIndexOf("/"))
          : "",
      }),
      right,
    ]);
  }

  // status: activity-feed-activity-detail-consumer
  // status: activity-feed-merged
  // Single backend-merged feed call replaces the prior two-fetch +
  // client-side reconcile. The unified `ActivityItem[]` is split back into
  // `activityRows` / `proposals` for the existing renderers; the merge,
  // ordering, and tiebreak now live in `core::activity`.
  async function refreshActivityDetail(): Promise<void> {
    let items: ActivityItem[] = [];
    try {
      items = await Ipc.activityList({ source: "merged", limit: 200 });
    } catch (err) {
      Logger.error("ui::vault-home", "activity_list failed", { err });
      items = [];
    }
    activityRows = [];
    proposals = [];
    for (const it of items) {
      if (it.payload.kind === "change") {
        const { kind: _k, ...row } = it.payload;
        activityRows.push(row as ChangeRow);
      } else {
        const s = it.payload;
        const meta = s.metadata && typeof s.metadata === "object" && !Array.isArray(s.metadata)
          ? (s.metadata as Record<string, unknown>)
          : null;
        proposals.push({
          id: s.id,
          surface: s.surface,
          action: s.action,
          target_path: s.target_path,
          trail_id: s.trail_id,
          content_hash: s.content_hash,
          created_at_ms: s.created_at_ms,
          metadata: meta,
        });
      }
    }
    renderActivityDetail();
  }

  function renderActivityDetail(): void {
    const presentClasses = new Set<string>();
    for (const r of activityRows) presentClasses.add(r.author_class);
    for (const p of proposals) {
      const cls = typeof p.metadata?.author_class === "string" ? p.metadata.author_class : "agent";
      presentClasses.add(cls);
    }

    const ALWAYS_SHOW: readonly string[] = ["user", "agent"];
    const allClasses = new Set<string>([...ALWAYS_SHOW, ...presentClasses]);

    if (!allFiltersOnce) {
      activeAuthorFilters.clear();
      for (const c of allClasses) activeAuthorFilters.add(c);
      allFiltersOnce = true;
    }

    deps.detailFiltersEl.replaceChildren();

    // Render author-class filter pills.
    const sortedClasses = [...allClasses].sort();
    for (const cls of sortedClasses) {
      const hasRows = presentClasses.has(cls);
      const classes = ["filter-pill", "toolbar-btn", "filter-pill-icon-only"];
      if (activeAuthorFilters.has(cls)) classes.push("active");
      if (!hasRows) classes.push("empty");
      deps.detailFiltersEl.appendChild(el("button", {
        class: classes.join(" "),
        html: authorPillIcon(cls),
        title: hasRows
          ? `Show ${cls} activity`
          : `No ${cls} activity in the recent window yet`,
        attrs: { type: "button", "aria-label": `Toggle ${cls} activity` },
        onClick: () => {
          if (activeAuthorFilters.has(cls)) {
            activeAuthorFilters.delete(cls);
          } else {
            activeAuthorFilters.add(cls);
          }
          renderActivityDetail();
        },
      }));
    }

    // Build unified timeline
    const timelineRows: TimelineRow[] = [
      ...activityRows.map((r): TimelineRow => ({ kind: "change", row: r })),
      ...proposals.map((p): TimelineRow => ({ kind: "proposal", row: p })),
    ];
    timelineRows.sort((a, b) => timelineTimestampMs(b) - timelineTimestampMs(a));

    const visible = timelineRows.filter((t) => {
      if (activeAuthorFilters.size === 0) return true;
      return activeAuthorFilters.has(timelineAuthorClass(t));
    });

    if (proposals.length > 0) {
      deps.detailCountEl.textContent = `${proposals.length} pending · ${visible.length} total events`;
    } else {
      deps.detailCountEl.textContent = `${visible.length} items`;
    }

    const listChildren: HTMLElement[] = [];

    if (proposals.length > 0) {
      const acceptAllRow = el("li", {}, [
        el("button", {
          class: "row-action",
          text: `Accept all (${proposals.length})`,
          title: "Confirm then batch-apply every pending proposal.",
          onClick: async (e) => {
            e.stopPropagation();
            if (!confirm(`Accept all ${proposals.length} pending proposals?\n\nEach proposal will be drift-checked against its target file before writing.`)) return;
            try {
              await Ipc.stagingAcceptAll();
            } catch (err) {
              alert(`Accept all failed: ${deps.formatErr(err)}`);
              return;
            }
            await refreshActivityDetail();
          },
        }),
      ]);
      acceptAllRow.style.cssText = "padding:4px 8px;display:flex;gap:8px;align-items:center;border-bottom:1px solid var(--border-divider)";
      listChildren.push(acceptAllRow);
    }

    listChildren.push(...visible.map((t) => buildTimelineRow(t)));
    deps.detailListEl.replaceChildren(...listChildren);
  }

  function buildTimelineRow(t: TimelineRow): HTMLElement {
    if (t.kind === "change") {
      return buildActivityDetailRow(t.row);
    }
    return buildProposalRow(t.row);
  }

  // status: staging-review-activity-detail-filter
  function buildProposalRow(p: Proposal): HTMLElement {
    // Proposal type indicator.
    const surfaceLabel = p.surface === "mcp-tool-call" ? "Write" :
      p.surface === "trails" ? (p.action.includes("trail") ? "Trail draft" : "Waypoint") :
      p.surface === "batch-mutation" ? "Batch" : p.action;
    const acceptBtn = makeIconButton(
      "accept",
      "Accept",
      "Drift-check the proposed write against the current file and apply.",
      async (e) => {
        e.stopPropagation();
        try {
          await deps.onAcceptStaging(p);
        } catch (err) {
          alert(`Accept failed: ${deps.formatErr(err)}`);
          return;
        }
        await refreshActivityDetail();
      },
    );
    const rejectBtn = makeIconButton(
      "reject",
      "Reject",
      "Discard the proposal without writing. No changelog row.",
      async (e) => {
        e.stopPropagation();
        try {
          await deps.onRejectStaging(p);
        } catch (err) {
          alert(`Reject failed: ${deps.formatErr(err)}`);
          return;
        }
        await refreshActivityDetail();
      },
    );
    return el("li", {
      class: "clickable",
      style: { cursor: "pointer" },
      onClick: (e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        void deps.onOpenStagingProposal(p);
      },
    }, [
      el("div", { class: "row-line" }, [
        el("span", { class: "activity-op", text: surfaceLabel }),
        el("span", {
          class: "name",
          text: p.target_path,
          title: p.trail_id ? `Trail: ${p.trail_id}` : undefined,
        }),
        el("span", { class: "row-right" }, [
          el("span", {
            class: "when",
            text: relativeTime(Math.floor(p.created_at_ms / 1000)),
            title: new Date(p.created_at_ms).toLocaleString(),
          }),
          el("span", {
            class: "activity-author",
            text: p.surface,
            title: `Source: ${p.surface}`,
          }),
        ]),
      ]),
      // Accept / Reject action buttons.
      el("div", { class: "row-actions" }, [acceptBtn, rejectBtn]),
    ]);
  }

  function buildActivityDetailRow(r: ChangeRow): HTMLElement {
    const isRestoreRow =
      r.metadata && typeof r.metadata === "object" &&
      ("restored_from" in r.metadata || "rolled_back_from" in r.metadata);
    const isCurrent = r.is_current;
    const canPreview = r.op !== "deleted";

    const lineChildren: (ChildNode | null)[] = [
      el("span", { class: "activity-op", text: opLabel(r.op) }),
      el("span", {
        class: "name",
        text: r.path,
        title: r.rename_from ? `renamed from ${r.rename_from}` : undefined,
      }),
    ];
    if (isRestoreRow) {
      const meta = r.metadata as Record<string, unknown>;
      const src = (meta["restored_from"] ?? meta["rolled_back_from"]) as
        | number
        | undefined;
      lineChildren.push(el("span", {
        class: "rollback-badge",
        text: src !== undefined ? `↩ restored from #${src}` : "↩ restored",
        title: src !== undefined
          ? `This save wrote the content of snapshot #${src} back to disk`
          : "This save was a Restore",
      }));
    }
    if (isCurrent) {
      lineChildren.push(el("span", {
        class: "rollback-badge",
        text: "current",
        title: "This is the file's current state on disk",
      }));
    }
    lineChildren.push(el("span", { class: "row-right" }, [
      el("span", {
        class: "when",
        text: relativeTime(Math.floor(r.timestamp_ms / 1000)),
        title: new Date(r.timestamp_ms).toLocaleString(),
      }),
      el("span", {
        class: "activity-author",
        html: authorPillIcon(r.author_class),
        title: r.author,
      }),
      el("span", {
        class: "activity-id",
        text: `#${r.id}`,
        title: `Snapshot id ${r.id}`,
      }),
    ]));

    const liClasses: string[] = [];
    if (recentlyRestoredFromId === r.id) liClasses.push("recently-rolled-back");
    if (canPreview) liClasses.push("clickable");

    const liChildren: (ChildNode | null)[] = [
      el("div", { class: "row-line" }, lineChildren),
    ];

    if (canPreview && !isCurrent) {
      const actionsChildren: (ChildNode | null)[] = [
        el("button", {
          class: "row-action",
          text: "Restore this version",
          title:
            "Write this snapshot's contents back to the file. Append-only — the restore is itself logged as a new modified event.",
          onClick: (e) => {
            e.stopPropagation();
            void doRestoreSnapshot(r);
          },
        }),
      ];
      if (recentlyRestoredFromId === r.id) {
        actionsChildren.push(el("span", {
          class: "un-rollback-prompt",
          text: "← previous state — click Restore to undo",
        }));
      }
      liChildren.push(el("div", { class: "row-actions" }, actionsChildren));
    }

    return el("li", {
      class: liClasses.join(" ") || undefined,
      style: canPreview ? { cursor: "pointer" } : undefined,
      onClick: canPreview ? (e) => {
        if ((e.target as HTMLElement).closest("button")) return;
        void deps.onOpenSnapshot(r);
      } : undefined,
    }, liChildren);
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
      await Ipc.restoreSnapshot({ changeId: row.id });
      recentlyRestoredFromId = wasCurrentId;
      await refreshActivityDetail();
    } catch (err) {
      alert(`restore failed: ${deps.formatErr(err)}`);
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

  function notifyStagingChanged(): void {
    if (!isVisible()) return;
    void refreshActivityWidget();
    if (activeDetailView?.kind === "recent-activity") {
      void refreshActivityDetail();
    }
  }

  // Wire the home button + new-note button.
  deps.homeBtn.addEventListener("click", () => {
    controller.setVisible(!controller.isVisible());
  });
  deps.newNoteBtn.addEventListener("click", async () => {
    try {
      const created = await Ipc.createNote({ folder: "" });
      await deps.openNote(created);
    } catch (err) {
      Logger.error("ui::vault-home", "new note failed", { err });
      alert(`new note failed: ${deps.formatErr(err)}`);
    }
  });

  const api: VaultHomeApi = {
    refresh,
    showDetail,
    notifyChangesAppended,
    notifyStagingChanged,
    notifyRecentModified,
    scheduleStatsRefresh,
    doRestoreSnapshot,
    activeDetailView: () => activeDetailView,
  };

  // Visibility flips the home view on/off. Both the home-button click
  // (handled inside the module) and any host-driven flip route through
  // the same `onSetVisible` hook so the editor-pane class, panel root,
  // toolbar button state, and the on-show refresh share one path.
  const controller = createPanelController<VaultHomeApi>(api, {
    initialVisible: isVisible(),
    applyOnMount: false,
    onSetVisible: (on) => {
      // status: tab-kinds — visibility is driven by the `hidden` attribute,
      // set/cleared by renderActiveTab() in main.ts. The controller's
      // setVisible still runs the refresh on show for legacy callers.
      if (on) deps.onBeforeShow?.();
      deps.vaultHomeEl.hidden = !on;
      if (on) void refresh();
    },
  });

  return controller;
}
