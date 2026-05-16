// status: tab-kinds
// status: note-properties-tab
// status: chat-panel-expand-to-editor
//
// Synthetic app-page tab openers. Reads singletons directly.

import {
  bumpActivationCounter,
  getOpenBuffers,
  getPreviewTabPath,
  setBufferState,
} from "./state";
import type { Buffer, TabKind } from "./state";
import { controllers } from "./controllers";
import { services } from "./services";

export interface AppPageTabsApi {
  openAppPageTab(
    kind: "home" | "home-detail" | "queue" | "settings",
    payload?: Record<string, string>,
  ): Promise<void>;
  openPropertiesTab(rel: string): void;
  openAgentTab(sessionId: string): Promise<void>;
}

interface OpenSyntheticTabOpts {
  /// Synthetic key — the lookup key inside `openBuffers`.
  key: string;
  /// Buffer kind set on the synthetic Buffer.
  kind: TabKind;
  /// `true` for preview-slotted tabs (evicts the existing preview).
  /// `false` for sticky tabs (no eviction, no preview slotting).
  preview: boolean;
  /// Optional callback fired after activation, in both the reuse
  /// and fresh-open paths. (Properties uses this to push `propertiesPane.update(rel)`.)
  postActivate?: () => void;
}

function openSyntheticTab(opts: OpenSyntheticTabOpts): void {
  const { key, kind, preview, postActivate } = opts;
  const openBuffers = getOpenBuffers();
  if (openBuffers.has(key)) {
    services.activateTabInner(key);
    postActivate?.();
    return;
  }
  if (preview) {
    // Evict the current preview tab (any kind) — at most one preview
    // exists at a time per spec. Sticky tabs (preview: false) survive.
    const previewTabPath = getPreviewTabPath();
    if (previewTabPath) {
      const oldEntry = openBuffers.get(previewTabPath);
      if (oldEntry && oldEntry.buffer.preview) {
        openBuffers.delete(previewTabPath);
        controllers.nav.get().nav.pruneTab(previewTabPath);
      }
    }
  }
  const buf: Buffer = {
    path: key,
    loadedText: "",
    token: null,
    kind,
    mode: { kind: "file" },
    pendingChangesMetadata: null,
    preview,
  };
  openBuffers.set(key, {
    buffer: buf,
    savedState: null,
    lastActivatedAt: bumpActivationCounter(),
  });
  if (preview) {
    setBufferState({ previewTabPath: key });
  }
  services.activateTabInner(key);
  postActivate?.();
}

export function setupAppPageTabs(): AppPageTabsApi {
  // status: tab-kinds
  /// Open or activate a app-page tab in the preview slot. Evicts other
  /// app-page previews. Called from Home / Queue / Settings button handlers.
  async function openAppPageTab(
    kind: "home" | "home-detail" | "queue" | "settings",
    payload?: Record<string, string>,
  ): Promise<void> {
    const key = services.appPageTabKey(kind, payload?.view);
    openSyntheticTab({ key, kind: kind as TabKind, preview: true });
  }

  // status: note-properties-tab
  /// Open a properties-kind tab for the given relative path. Reuses an
  /// existing properties tab for the same path; otherwise creates a new one.
  function openPropertiesTab(rel: string): void {
    const key = services.appPageTabKey("properties", rel);
    const propertiesPane = controllers.propertiesPane.get();
    openSyntheticTab({
      key,
      kind: "properties",
      preview: false,
      postActivate: () => void propertiesPane.update(rel),
    });
  }

  // status: chat-panel-expand-to-editor
  /// Open an agent-kind tab for the given chat session. Creates one
  /// tab per session; reopens the existing one if already open.
  async function openAgentTab(sessionId: string): Promise<void> {
    const key = services.appPageTabKey("agent", sessionId);
    openSyntheticTab({ key, kind: "agent", preview: false });
  }

  return { openAppPageTab, openPropertiesTab, openAgentTab };
}
