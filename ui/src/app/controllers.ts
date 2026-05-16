// Typed registry of controller slots. Each `mount*` / `setup*` returns
// an API; main.ts registers it into the matching slot here, and other
// modules read it directly via `controllers.X.get()`. Replaces the
// `getX()` lazy-getter fields that every extracted module used to take
// through its `Deps` bag.
//
// `tryGet()` is for slots that legitimately may be `null` at access
// time (e.g. `trailsPanel` / `clusterEditor` when their host elements
// aren't in the DOM); `get()` throws if a non-nullable slot is read
// before `set()`.

import type { EditorPaneApi } from "../editorPane";
import type { TabStripApi } from "../tabStrip";
import type { DiscoveryController } from "../discovery";
import type { TreeController } from "../tree";
import type { TrailsController } from "../trails";
import type { ClusterEditorApi } from "../clusterEditor";
import type { TrashController } from "../trash";
import type { SettingsPaneApi } from "../settings";
import type { VaultHomeController } from "../vaultHome";
import type { QueueDetailController } from "../queueDetail";
import type { PropertiesPaneApi } from "../propertiesPane";
import type { SnapshotPreviewApi } from "../snapshotPreview";
import type { AutosaveApi } from "./autosave";
import type { IndexStatusViewApi } from "./indexStatusView";
import type { NavApi } from "../navigation";
import type { TabsApi } from "./tabs";
import type { OpenFileApi } from "./openFile";
import type { SettingsManager } from "../settings/manager";

interface ChatPanelLike {
  reset(): void;
  hydrate(active: unknown): void;
  newSession(): Promise<void>;
  setEnabled(on: boolean): void;
  setHeight(h: number): void;
  setInputHeight(px: number): void;
  getActiveSessionId?: () => string | null;
}

interface TaskQueueTileLike {
  refresh(): Promise<void>;
  stopStagingPolling(): void;
}

interface ClusterPaneWiringLike {
  clusterEditorPane: import("../clusterEditorPane").ClusterEditorPaneApi | null;
  clusterReviewTab: import("../clusterReviewTab").ClusterReviewApi | null;
  clusterPaneEl: HTMLElement | null;
  clusterReviewPaneEl: HTMLElement | null;
  openClusterReviewTab(p: import("../clusterReviewTab").Purpose): void;
  openClusterTab(treeId: string, treeName?: string): Promise<void>;
  getCurrentClusterTabKey(): string | null;
}

interface PatchReviewWiringLike {
  enterPatchReviewMode(rel: string): Promise<void>;
  exitPatchReviewMode(): void;
  refreshAgentDiffBtn(): void;
  refreshWriteNotePendingBanner(): void;
  pendingEditProposalsForPath(path: string): import("../ipc").Proposal[];
  pendingWriteProposalsForPath(path: string): import("../ipc").Proposal[];
  refreshPendingProposalsCache(): Promise<void>;
  acceptPatchReviewHunk(p: import("../ipc").Proposal): Promise<void>;
  rejectPatchReviewHunk(p: import("../ipc").Proposal): Promise<void>;
  openProposalReview(p: { id: string; target_path: string }): Promise<void>;
  getPendingProposalsCache(): import("../ipc").Proposal[];
  clearWriteNoteTargetExistsCache(): void;
}

interface WriteNoteReviewLike {
  openWriteNoteReview(p: import("../ipc").Proposal): Promise<void>;
  exitWriteNoteReview(): void;
  toggleWriteNoteReviewDiff(): Promise<void>;
}

interface SidebarModeLike {
  syncToggleButtons(): void;
  setSidebarMode(mode: "files" | "clusters" | "trails", persist: boolean): void;
  getSidebarMode(): "files" | "clusters" | "trails";
  setSidebarWidthVar(px: number): void;
  setDiscoveryWidthVar(px: number): void;
}

interface NavSetupLike {
  nav: NavApi;
  checkpointNav(): void;
  paintNavButtons(): void;
  installSnapshotWrappers(): void;
  installTrashWrappers(): void;
}

interface AppPageTabsLike {
  openAppPageTab(
    kind: "home" | "home-detail" | "queue" | "settings",
    payload?: Record<string, string>,
  ): Promise<void>;
  openPropertiesTab(rel: string): void;
  openAgentTab(sessionId: string): Promise<void>;
}

interface VaultLifecycleLike {
  openVault(): Promise<void>;
  bootstrapDefaultVault(): Promise<void>;
  getState(): { kind: string };
}

interface Slot<T> {
  set(value: T): void;
  get(): T;
  tryGet(): T | null;
}

function slot<T>(name: string): Slot<T> {
  let v: T | null = null;
  return {
    set: (x: T) => { v = x; },
    get: (): T => {
      if (v === null) throw new Error(`controllers.${name} accessed before set`);
      return v;
    },
    tryGet: (): T | null => v,
  };
}

export const controllers = {
  tree: slot<TreeController>("tree"),
  tabStrip: slot<TabStripApi>("tabStrip"),
  tabs: slot<TabsApi>("tabs"),
  openFileApi: slot<OpenFileApi>("openFileApi"),
  discovery: slot<DiscoveryController>("discovery"),
  chatPanel: slot<ChatPanelLike>("chatPanel"),
  editorPane: slot<EditorPaneApi>("editorPane"),
  indexStatusView: slot<IndexStatusViewApi>("indexStatusView"),
  taskQueueTile: slot<TaskQueueTileLike>("taskQueueTile"),
  queueDetail: slot<QueueDetailController>("queueDetail"),
  trailsPanel: slot<TrailsController | null>("trailsPanel"),
  clusterEditor: slot<ClusterEditorApi | null>("clusterEditor"),
  autosave: slot<AutosaveApi>("autosave"),
  trash: slot<TrashController>("trash"),
  settingsPane: slot<SettingsPaneApi>("settingsPane"),
  vaultHome: slot<VaultHomeController>("vaultHome"),
  propertiesPane: slot<PropertiesPaneApi>("propertiesPane"),
  snapshotPreview: slot<SnapshotPreviewApi>("snapshotPreview"),
  nav: slot<NavSetupLike>("nav"),
  appPageTabs: slot<AppPageTabsLike>("appPageTabs"),
  clusterWiring: slot<ClusterPaneWiringLike>("clusterWiring"),
  patchReview: slot<PatchReviewWiringLike>("patchReview"),
  writeNoteReview: slot<WriteNoteReviewLike>("writeNoteReview"),
  sidebarMode: slot<SidebarModeLike>("sidebarMode"),
  vaultLifecycle: slot<VaultLifecycleLike>("vaultLifecycle"),
  settings: slot<SettingsManager>("settings"),
};
