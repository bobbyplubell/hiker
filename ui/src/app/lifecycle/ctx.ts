// Shared mutable context threaded through every lifecycle phase.
//
// Why this exists: `bootstrap()` used to be one ~900-line function whose
// many local `const`s (`editor`, `tabs`, `openFileApi`, `clusterWiring`,
// `tree`, `vaultHome`, ...) were captured by closures defined elsewhere
// in the same scope. Splitting into phase modules requires those locals
// to be reachable across files. Two existing registries already cover
// most of this (`./controllers`, `./services`); the leftover slots —
// short-lived helper functions and a couple of late-bound locals
// referenced by other phases — live here.
//
// Phases populate fields they own (e.g. `phase2_mountEditor` sets
// `ctx.editor`). Downstream phases assert non-null at read sites; an
// undefined read indicates a phase-ordering bug and surfaces as a clear
// "Cannot read properties of undefined" at the call site rather than a
// silent miswire.
//
// status: bootstrap-phase-split

import type { EditorHost } from "../editor";
import type { Buffer } from "../state";

export interface BootstrapCtx {
  // Phase 1 outputs
  formatError: ((err: unknown) => string) | null;
  isReadOnlyBuffer: ((b: Buffer | null) => boolean) | null;
  appPageTabKey: ((kind: string, view?: string) => string) | null;
  vaultIsOpen: (() => boolean) | null;
  persistSetting: ((scope: "user" | "vault", key: string, value: unknown) => Promise<void>) | null;

  // Phase 2 outputs (editor / status / patch-review)
  editor: EditorHost | null;
  setReadOnly: ((ro: boolean, mode?: "trash" | "snapshot" | "mutation" | null) => void) | null;
  save: (() => Promise<boolean>) | null;
  isDirty: (() => boolean) | null;
  refreshChunkBoundaries: (() => void) | null;
  scheduleChunkBoundariesRefresh: ((delayMs: number) => void) | null;
  updateStatus: (() => void) | null;
  renderActiveTab: (() => void) | null;
  promotePreviewIfActive: (() => void) | null;
  promotePreviewByPath: ((rel: string) => void) | null;

  // Phase 3 / 4 outputs
  openFile: ((rel: string, opts?: { preview?: boolean }) => Promise<void>) | null;
  closeTab: ((rel: string) => Promise<void>) | null;
  activateTabInner: ((rel: string) => void) | null;
  refreshTree: (() => Promise<void>) | null;
  revealInTree: ((rel: string) => Promise<void>) | null;
  refreshTrashBin: (() => Promise<void>) | null;
  openAppPageTab: ((kind: "home" | "home-detail" | "queue" | "settings", payload?: Record<string, string>) => Promise<void>) | null;

  // Live view-toggle state mirrored from settings.
  getLivePreviewEnabled: (() => boolean) | null;
  getHideFrontmatterEnabled: (() => boolean) | null;
}

export const ctx: BootstrapCtx = {
  formatError: null,
  isReadOnlyBuffer: null,
  appPageTabKey: null,
  vaultIsOpen: null,
  persistSetting: null,
  editor: null,
  setReadOnly: null,
  save: null,
  isDirty: null,
  refreshChunkBoundaries: null,
  scheduleChunkBoundariesRefresh: null,
  updateStatus: null,
  renderActiveTab: null,
  promotePreviewIfActive: null,
  promotePreviewByPath: null,
  openFile: null,
  closeTab: null,
  activateTabInner: null,
  refreshTree: null,
  revealInTree: null,
  refreshTrashBin: null,
  openAppPageTab: null,
  getLivePreviewEnabled: null,
  getHideFrontmatterEnabled: null,
};

/// Assert a phase output has been populated. Throws with a clear message
/// pointing at the missing phase if used before its producer ran.
export function need<K extends keyof BootstrapCtx>(key: K): NonNullable<BootstrapCtx[K]> {
  const v = ctx[key];
  if (v === null) {
    throw new Error(`bootstrap ctx: ${key} read before its producing phase ran`);
  }
  return v as NonNullable<BootstrapCtx[K]>;
}
