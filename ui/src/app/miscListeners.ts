// Watcher overflow toast, LLM-warning toast, config-reloaded reload, plus
// the snapshot / file / trash mode-controls registrations. Reads
// singletons directly.

import { onHikerEvent } from "../events";
import { showToast } from "../widgets/toast";
import { iconButton } from "../modeControls";
import { Icons } from "../icons";
import { getBuffer } from "./state";
import { controllers } from "./controllers";
import { services } from "./services";

export function installMiscListeners(): void {
  const editorPane = controllers.editorPane.get();

  editorPane.modeControls.register("snapshot", (host) => {
    const buffer = getBuffer();
    if (buffer?.mode.kind !== "snapshot") return;
    const row = buffer.mode.row;
    const diffActive = buffer.mode.diffActive;
    const snapshotPreview = controllers.snapshotPreview.get();
    // status: mode-controls-diff-toggle
    // Hidden for `op = "deleted"` rows — there's no `before` blob to diff
    // against, so the toggle's affordance lies. Other rows always offer it.
    if (row && row.op !== "deleted") {
      host.appendChild(
        iconButton({
          title: diffActive ? "Hide diff" : "Show diff vs current",
          pressed: diffActive,
          svg: Icons.diff(),
          onClick: () => snapshotPreview.toggleDiff(),
        }),
      );
    }
    host.appendChild(
      iconButton({
        title: "Restore this version",
        svg: Icons.restore(),
        onClick: () => snapshotPreview.restore(),
      }),
    );
    host.appendChild(
      iconButton({
        title: "Close preview",
        svg: Icons.close(),
        onClick: () => snapshotPreview.close(),
      }),
    );
  });

  // status: note-mutation-buffer-ro-while-in-flight
  // `#mode-controls` renderer for the regular `file` buffer state.
  // The "Reformatting…" pill moved to the status-bar left region; the
  // toolbar slot now holds only action buttons.
  editorPane.modeControls.register("file", (_host) => {
    // Empty — file-mode chrome (Save / Diff / View / Mutations) lives
    // outside the centered mode-controls slot.
  });

  editorPane.modeControls.register("trash", (host) => {
    host.appendChild(
      iconButton({
        title: "Close preview",
        svg: Icons.close(),
        onClick: () => controllers.trash.get().api.closePreview(),
      }),
    );
  });

  // Watcher overflow toast; trash-changed listener lives inside the trash
  // module now (it owns the cleanup of a previewed entry that vanished).
  void onHikerEvent("hiker:watcher-overflow", () => {
    showToast("Filesystem watcher fell behind — rescanning…");
  });

  // status: llm-providers-config
  // API-key preflight surface (per llm.md §Disable mode): the backend
  // emits this on vault open when [llm].enabled = true and the configured
  // api_key_env is unset, so the user sees the problem before they try to
  // chat. Longer TTL than the default toast so the message is readable.
  void onHikerEvent("hiker:llm-warning", (payload) => {
    showToast(payload.message, undefined, 8000);
  });

  // External edits to either config.toml fire this event. Reload through
  // the same applySettingsToUi path as vault open + set_setting writes so
  // every surface that reflects a setting (View menu, tree sort, panels,
  // chat) repaints from the live Config.
  void onHikerEvent("hiker:config-reloaded", (payload) => {
    services.applySettingsToUi(payload);
  });
}
