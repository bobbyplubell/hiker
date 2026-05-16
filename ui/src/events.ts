/// Typed event registry for the Rust → TS Tauri event boundary.
///
/// Single source of truth for the `hiker:*` event names + payload shapes
/// that the Rust backend emits. Each entry mirrors a `crate::events::name::*`
/// constant + matching `emit_*` helper on the Rust side; renaming or
/// retyping must happen in both places.
///
/// KEEP IN SYNC with `ui/src-tauri/src/events.rs`.
///
/// Why this exists: every `listen<T>("hiker:foo", cb)` call used to
/// hand-type the wire name + payload shape independently of the Rust
/// `emit("hiker:foo", &payload)` site. A renamed Rust field broke TS
/// silently. The `onHikerEvent` wrapper below keys the payload type to
/// the event name via this interface, so a mismatched callback fails
/// at compile time.

import { listen, type UnlistenFn, type Event } from "@tauri-apps/api/event";
import type { ChangeRow } from "./snapshotPreview";
import type { Settings } from "./app/settingsApply";
import type { IndexStatus, ProgressEvent } from "./app/indexStatusBus";

/// `hiker:file-changed` payload — mirrors `hiker_core::watcher::FileEvent`
/// serialized with internal `kind` tag.
export type FileChangedEvent =
  | { kind: "created" | "modified" | "deleted"; path: string }
  | { kind: "renamed"; from: string; to: string };

/// `hiker:llm-warning` payload — mirrors `crate::events::LlmWarning`.
/// `env` is always present on the wire (Rust serializes `&str`); kept
/// optional here so any future warning kind that omits it stays valid.
export interface LlmWarningPayload {
  kind: string;
  env?: string;
  message: string;
}

/// `hiker:note-mutation-applied` payload — mirrors the Rust
/// `NoteMutationAppliedEvent<'a>` private struct in `cmds/mutations.rs`.
export interface NoteMutationAppliedPayload {
  task_id: string;
  source_path: string;
  mutation_kind: string;
  content: string;
  source_hash_at_submit: string;
}

/// `hiker:note-mutation-failed` payload — mirrors the Rust
/// `NoteMutationFailedEvent<'a>` private struct in `cmds/mutations.rs`.
export interface NoteMutationFailedPayload {
  task_id: string;
  source_path: string;
  mutation: string;
  error: string;
}

/// `hiker:queue-event` payload — mirrors `hiker_core::tasks::QueueEvent`,
/// which is a discriminated union with many variants. Call sites narrow
/// further (e.g. `queueDetail/index.ts::QueueEvent`); the registry type
/// is intentionally permissive so each consumer can keep its own local
/// shape without forcing every site to import the union here.
export interface HikerQueueEvent {
  event: string;
  id?: string;
  kind?: { type?: string; source_path?: string };
  [k: string]: unknown;
}

/// `hiker:chat-event` payload — mirrors `hiker_core::agent::AgentEvent`.
/// The full discriminated union lives in `./chat`; importing it here
/// would invert the dependency, so consumers (just `./chat`) re-narrow
/// to their local `AgentEvent` type. Permissive shape sufficient for
/// the registry's wire check.
export interface HikerChatEvent {
  kind: string;
  [k: string]: unknown;
}

/// `hiker:config-reloaded` payload. The Rust side emits the full
/// `Config` struct; the frontend treats it as `Settings` (the same
/// shape, parsed by `applySettingsToUi`).
export type ConfigReloadedPayload = Settings;

/// Compile-time registry: event name → payload type. Wire names must
/// match `ui/src-tauri/src/events.rs::name`.
export interface HikerEvents {
  "hiker:changes-appended": ChangeRow;
  "hiker:chat-event": HikerChatEvent;
  "hiker:config-reloaded": ConfigReloadedPayload;
  "hiker:file-changed": FileChangedEvent;
  "hiker:index-status": IndexStatus;
  "hiker:llm-warning": LlmWarningPayload;
  "hiker:note-mutation-applied": NoteMutationAppliedPayload;
  "hiker:note-mutation-failed": NoteMutationFailedPayload;
  "hiker:queue-event": HikerQueueEvent;
  "hiker:reindex-progress": ProgressEvent;
  "hiker:staging-changed": void;
  "hiker:trash-changed": void;
  "hiker:watcher-overflow": void;
}

export type HikerEventName = keyof HikerEvents;

/// Typed listen wrapper. The payload type is inferred from the event
/// name, so a mismatched handler fails at compile time. The optional
/// `T` extends the registry payload so call sites that need a narrower
/// local type (e.g. `queueDetail/index.ts::QueueEvent`) can pass it
/// without going back to raw `listen<T>("hiker:...", ...)`.
export function onHikerEvent<N extends HikerEventName>(
  name: N,
  handler: (payload: HikerEvents[N], ev: Event<HikerEvents[N]>) => void,
): Promise<UnlistenFn> {
  return listen<HikerEvents[N]>(name, (ev) => handler(ev.payload, ev));
}

/// Escape-hatch variant for call sites that want a narrower local
/// payload type than the registry advertises (e.g. a strict discriminated
/// union for `hiker:queue-event` while the registry keeps a permissive
/// structural shape). The wire name is still type-checked against the
/// registry; the payload is the caller's responsibility.
export function onHikerEventAs<T, N extends HikerEventName = HikerEventName>(
  name: N,
  handler: (payload: T, ev: Event<T>) => void,
): Promise<UnlistenFn> {
  return listen<T>(name, (ev) => handler(ev.payload, ev));
}
