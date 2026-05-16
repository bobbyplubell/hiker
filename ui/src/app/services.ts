// Late-bound function registry for cross-cutting verbs that the host
// (`main.ts`) defines but other modules want to call. Each entry is a
// callable that throws if invoked before `.set()` wires it. Replaces
// the closure-captured callback fields (`openFile`, `formatError`,
// `updateStatus`, …) that every extracted module used to take through
// its `Deps` bag.
//
// Type-stub the signatures here rather than importing the full module
// types — these are usage-shape contracts, not the host's implementation.

import type { Proposal } from "../ipc";
import type { Buffer } from "./state";
import type { Settings } from "./settingsApply";
import type { Purpose as ClusterReviewPurpose } from "../clusterReviewTab";

type AnyFn = (...args: never[]) => unknown;

interface ServiceFn<T extends AnyFn> {
  (...args: Parameters<T>): ReturnType<T>;
  set(impl: T): void;
}

function fn<T extends AnyFn>(name: string): ServiceFn<T> {
  let impl: T | null = null;
  const wrapper = ((...args: Parameters<T>): ReturnType<T> => {
    if (impl === null) throw new Error(`services.${name} called before set`);
    return impl(...args) as ReturnType<T>;
  }) as ServiceFn<T>;
  wrapper.set = (f: T): void => { impl = f; };
  return wrapper;
}

export const services = {
  openFile: fn<(rel: string, opts?: { preview?: boolean }) => Promise<void>>("openFile"),
  openAppPageTab: fn<(
    kind: "home" | "home-detail" | "queue" | "settings",
    payload?: Record<string, string>,
  ) => Promise<void>>("openAppPageTab"),
  openPropertiesTab: fn<(rel: string) => void>("openPropertiesTab"),
  openAgentTab: fn<(sessionId: string) => Promise<void>>("openAgentTab"),
  openClusterTab: fn<(treeId: string, treeName?: string) => Promise<void>>("openClusterTab"),
  openClusterReviewTab: fn<(p: ClusterReviewPurpose) => void>("openClusterReviewTab"),
  openProposalReview: fn<(p: { id: string; target_path: string }) => Promise<void>>("openProposalReview"),
  openWriteNoteReview: fn<(proposal: Proposal) => Promise<void>>("openWriteNoteReview"),

  refreshTree: fn<() => Promise<void>>("refreshTree"),
  refreshTrashBin: fn<() => Promise<void>>("refreshTrashBin"),
  refreshPendingProposalsCache: fn<() => Promise<void>>("refreshPendingProposalsCache"),
  refreshAgentDiffBtn: fn<() => void>("refreshAgentDiffBtn"),
  refreshWriteNotePendingBanner: fn<() => void>("refreshWriteNotePendingBanner"),
  clearWriteNoteTargetExistsCache: fn<() => void>("clearWriteNoteTargetExistsCache"),

  pendingEditProposalsForPath: fn<(path: string) => Proposal[]>("pendingEditProposalsForPath"),
  pendingWriteProposalsForPath: fn<(path: string) => Proposal[]>("pendingWriteProposalsForPath"),

  applySettingsToUi: fn<(s: Settings) => void>("applySettingsToUi"),
  updateStatus: fn<() => void>("updateStatus"),
  formatError: fn<(err: unknown) => string>("formatError"),
  activateTabInner: fn<(rel: string) => void>("activateTabInner"),
  startBackgroundIntervals: fn<() => void>("startBackgroundIntervals"),
  isReadOnlyBuffer: fn<(b: Buffer | null) => boolean>("isReadOnlyBuffer"),
  isDirty: fn<() => boolean>("isDirty"),
  closeTab: fn<(rel: string) => Promise<void>>("closeTab"),
  closeActiveTab: fn<() => void>("closeActiveTab"),
  cycleTab: fn<(delta: 1 | -1) => void>("cycleTab"),
  jumpToTab: fn<(n: number) => void>("jumpToTab"),
  save: fn<() => Promise<boolean>>("save"),
  chatNewSession: fn<() => Promise<void>>("chatNewSession"),
  navBack: fn<() => Promise<void>>("navBack"),
  navForward: fn<() => Promise<void>>("navForward"),
  openSettingsPage: fn<() => void>("openSettingsPage"),
  focusSearchInput: fn<() => void>("focusSearchInput"),
  scheduleTreeRefreshFromWatcher: fn<() => void>("scheduleTreeRefreshFromWatcher"),
  scheduleChunkBoundariesRefresh: fn<(delayMs: number) => void>("scheduleChunkBoundariesRefresh"),
  handleWatcherConflictDirty: fn<(rel: string) => Promise<void>>("handleWatcherConflictDirty"),
  checkpointNav: fn<() => void>("checkpointNav"),
  appPageTabKey: fn<(kind: string, view?: string) => string>("appPageTabKey"),
  persistSetting: fn<(scope: "user" | "vault", key: string, value: unknown) => Promise<void>>("persistSetting"),
  vaultIsOpen: fn<() => boolean>("vaultIsOpen"),
  getHideFrontmatterEnabled: fn<() => boolean>("getHideFrontmatterEnabled"),
  panelToast: fn<(msg: string, opts?: { actionLabel?: string; onAction?: () => void }) => void>("panelToast"),
};
