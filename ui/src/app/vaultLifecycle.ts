/// Vault open / close / clear-on-swap orchestration. Owns the picker
/// dialog, the bootstrap-from-default flow (per
/// `settings-default-vault-autoopen`), and the on-open seeding sequence
/// — apply settings, reset per-path caches, blank tabs / preview /
/// related / search / chat, refresh tree + trash, and land on
/// vault-home. Hosted by `main.ts`, which still owns the underlying
/// state (buffer/tab stores, CM6 view, panel APIs) and threads them in
/// through deps.
///
/// Module is intentionally a thin orchestration shell: it exposes
/// `openVault()` / `bootstrapDefaultVault()` for top-level callers
/// (vault-pick button + boot path) and depends on `applyOpenedVault`
/// being supplied by the host since that path touches every panel API
/// the host owns. The host's `applyOpenedVault` does the heavy lifting;
/// this module only owns the picker+error+bootstrap sequencing.
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { Ipc } from "../ipc";
import { Logger } from "../logger";
import { showToast } from "../widgets/toast";

/// Discriminated union of vault lifecycle phases. Replaces the
/// `vaultIsOpen: boolean` flag the host previously carried; the
/// transient `opening` / `closing` phases let listeners narrow on
/// "fully open" without racing against in-flight transitions.
///
/// Today only `closed → opening → open` and `open → closing → closed`
/// transitions exist (vault swap funnels through `closed` between the
/// outgoing and incoming open). The `closing` state is currently a
/// thin pass-through — `closeVault` isn't wired to any UI gesture
/// (multi-vault open is `search-multi-vault`, deferred) — but the
/// transitions are reified so a future close gesture has a place to
/// hook drop-listener cleanup without touching every read-site.
export type VaultState =
  | { kind: "closed" }
  | { kind: "opening"; path: string }
  | { kind: "open"; path: string }
  | { kind: "closing"; path: string };

export type VaultStateListener = (state: VaultState) => void;
export type VaultUnsubscribe = () => void;

export interface VaultLifecycleDeps {
  /// Apply the opened vault's display path to every UI surface that
  /// reflects vault state (vault-path label, settings re-seed, store
  /// resets, panel refresh, vault-home land). Owned by host because it
  /// touches stores + every panel API. Called during the `opening →
  /// open` transition; the lifecycle module flips state to `open`
  /// after this resolves.
  applyOpenedVault: (path: string) => Promise<void>;
  formatError: (err: unknown) => string;
}

export interface VaultLifecycleApi {
  /// Show the OS folder picker and open the chosen path. Wired to the
  /// vault-pick button in the host.
  openVault: () => Promise<void>;
  /// Bootstrap path used at app start. Reads `vault.default`; on a
  /// configured + resolvable path, opens it; on a `not_found` error,
  /// falls through to the picker. Other open errors surface as alerts.
  bootstrapDefaultVault: () => Promise<void>;
  /// Surface an open-vault error consistently (schema-mismatch hint vs.
  /// generic alert). Exposed for host code paths that try a direct
  /// `Ipc.openVaultAt` outside this module.
  handleOpenVaultError: (err: unknown) => void;
  /// Current vault state. Cheap snapshot accessor; subscribers get a
  /// listener-driven feed via `subscribe`.
  getState: () => VaultState;
  /// Subscribe to state transitions. Listener fires after every
  /// `closed/opening/open/closing` transition with the fresh state.
  /// Does *not* fire on registration — call `getState()` for the
  /// initial paint.
  subscribe: (listener: VaultStateListener) => VaultUnsubscribe;
}

export function mountVaultLifecycle(
  deps: VaultLifecycleDeps,
): VaultLifecycleApi {
  let state: VaultState = { kind: "closed" };
  const listeners = new Set<VaultStateListener>();

  function setState(next: VaultState): void {
    state = next;
    for (const l of listeners) l(state);
  }

  /// Run `applyOpenedVault` under the `opening → open` transition. Any
  /// failure inside `applyOpenedVault` rewinds to `closed` rather than
  /// stranding the state machine in `opening` (which would silently
  /// gate every "is the vault open?" guard against a half-initialized
  /// vault).
  async function transitionToOpen(path: string): Promise<void> {
    setState({ kind: "opening", path });
    try {
      await deps.applyOpenedVault(path);
      setState({ kind: "open", path });
    } catch (err) {
      Logger.error("ui::app", "applyOpenedVault failed", { err });
      setState({ kind: "closed" });
      throw err;
    }
  }

  function handleOpenVaultError(err: unknown): void {
    const msg = deps.formatError(err);
    Logger.error("ui::app", "open vault failed", { err });
    // Surface schema-version mismatches with the canonical fix from
    // index.md's `store-version-fail-loud` policy. The error string is
    // shaped like "schema version mismatch: db is vN, binary expects vM".
    if (msg.includes("schema version mismatch")) {
      alert(
        `${msg}\n\nThis project's pre-real-use migration policy is to delete .hiker/index.db and re-index. Remove that file in your vault and try again.`,
      );
    } else {
      alert(`open vault failed: ${msg}`);
    }
  }

  /// Show the OS folder picker via the JS dialog plugin and, on a
  /// selection, open it through `open_vault_at`. The picker lives
  /// entirely in the frontend per the spec — the backend has no dialog
  /// dependency.
  async function openVault(): Promise<void> {
    let chosen: string | null;
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      chosen = typeof picked === "string" ? picked : null;
    } catch (err) {
      Logger.error("ui::app", "folder picker failed", { err });
      return;
    }
    if (!chosen) return;
    try {
      const display = await Ipc.openVaultAt({ path: chosen });
      await transitionToOpen(display);
    } catch (err) {
      handleOpenVaultError(err);
    }
  }

  // status: settings-default-vault-autoopen
  // Bootstrap: read `vault.default` from the user TOML; if non-empty,
  // try `open_vault_at`. On `HikerError::NotFound` (path no longer
  // resolves — drive unmounted, folder deleted) surface a non-fatal
  // toast and fall through to the JS dialog. The configured
  // `vault.default` is *not* auto-cleared — it represents user intent,
  // not a transient circumstance.
  async function bootstrapDefaultVault(): Promise<void> {
    let configured: string | null = null;
    try {
      configured = await Ipc.getDefaultVault();
    } catch (err) {
      Logger.error("ui::app", "get_default_vault failed", { err });
    }
    if (configured && configured.length > 0) {
      try {
        const display = await Ipc.openVaultAt({ path: configured });
        await transitionToOpen(display);
        return;
      } catch (err) {
        // HikerError is serialized as `{ kind, message }` (see
        // core::error). `not_found` is the "path no longer resolves"
        // signal that the spec says should fall through to the picker.
        // Any other error is real and surfaces as the standard alert.
        const kind = (err as { kind?: string } | null)?.kind;
        if (kind === "not_found") {
          showToast(`Default vault at ${configured} not found — pick a vault`);
        } else {
          handleOpenVaultError(err);
          return;
        }
      }
    }
    // No configured default, or fell through after a NotFound. Show
    // picker.
    await openVault();
  }

  return {
    openVault,
    bootstrapDefaultVault,
    handleOpenVaultError,
    getState: () => state,
    subscribe: (listener) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}
