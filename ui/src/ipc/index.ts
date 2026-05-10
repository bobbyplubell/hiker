// Single-seam IPC client. Every Tauri command the UI uses funnels through
// `invokeWithLogging<T>(cmd, args)`, which:
//
//   - calls `invoke(cmd, args)` from `@tauri-apps/api/core`
//   - on rejection, routes a structured event through `Logger.error` so the
//     failure lands in `vault/.hiker/logs/hiker.log` alongside the rest of
//     the tracing stream (`obs-frontend-bridge`).
//
// `log_from_frontend` itself is a Tauri command and thus rides
// `invokeWithLogging`. The recursion guard below short-circuits to
// `console.error` on its own failure so a broken bridge can't infinite-loop
// the logger.
//
// Response-type discipline: the typed surface mirrors each command's
// return shape based on the call sites that consumed it before the
// migration. The high-blast-radius commands (`get_settings` /
// `get_settings_scoped` / `reload_config`, `search_vault`,
// `recent_changes`) get hand-rolled runtime validators in `./validators`
// — same `parse*` shape zod's `.parse()` exposes, throwing a clear
// `Error` when the wire shape drifts from the Rust DTO. The remaining
// commands keep their TypeScript-only typed wrappers (`invoke<T>`-style
// generics at the seam, no runtime check) — the cost of a mismatch on
// those is bounded to a single panel. Resolves
// `bug-ipc-responses-untyped`.
//
// Why hand-rolled instead of zod: zod isn't a dep of `ui/package.json`
// and the bug row called for not adding one without user input. The
// validators in `./validators.ts` are short and obvious; revisit if a
// fourth command needs runtime checks or schema composition.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import {
  parseSearchResponse,
  parseChangeRowArray,
  parseSettingsConfig,
} from "./validators";
import { Logger } from "../logger";

// ---------------- shared response types ----------------
// Hand-mirrored from the Rust DTOs. Where a panel module already exports
// a richer shape (e.g. `ChangeRow`, `ChunkBounds`, `IndexState`,
// `TrashListItem`, `DirEntry`, `SettingsConfig`), we re-import that as the
// canonical type so the IPC surface and the consumer agree by structural
// equality, not by separate copies.

import type { ChangeRow } from "../snapshotPreview";
import type { ChunkBounds } from "../editor/chunkBoundaries";
import type { IndexState, DirEntry } from "../tree";
import type { TrashListItem } from "../trash";
import type { SettingsConfig, SettingsScope } from "../settings";

/// Opaque buffer-identity token issued by `open_for_edit` and rotated by
/// `commit_buffer` / `resolve_drift`. The UI must NEVER inspect or
/// reconstruct the inner fields — round-trip the value verbatim.
export type BufferToken = unknown;

export interface OpenForEditOutcome {
  contents: string;
  token: BufferToken;
}

export type DriftChoice = "keep_mine" | "take_theirs" | "cancel";

export type CommitOutcome =
  | { kind: "written"; new_hash: string; token: BufferToken }
  | { kind: "drift_detected"; current_disk_text: string; current_hash: string };

export type DriftResolution =
  | { kind: "written"; new_hash: string; token: BufferToken }
  | { kind: "took_theirs"; contents: string; token: BufferToken }
  | { kind: "cancelled" };

export interface TrashEntry {
  id: string;
  original_path: string;
  trashed_name: string;
  original_mtime: number;
  deleted_at: number;
  kind: "file" | "folder";
  members?: string[] | null;
}

export interface RelatedHit {
  note_id: string;
  path: string;
  title: string;
  score: number;
  best_heading_path: string | null;
  snippet: string;
}

export interface SearchNoteHit {
  note_id: string;
  path: string;
  title: string;
  score: number;
  chunk_id: string;
  chunk_index: number;
  heading_path: string | null;
  snippet: string;
}

export interface SearchResponse {
  epoch: number;
  lexical_hits: SearchNoteHit[];
  semantic_hits: SearchNoteHit[];
  fused: SearchNoteHit[];
  hits: SearchNoteHit[];
}

export type DiffOp = "equal" | "insert" | "delete";
export interface DiffLine {
  op: DiffOp;
  line: string;
  before_line_no: number | null;
  after_line_no: number | null;
}
export interface DiffHunk {
  lines: DiffLine[];
}
export interface DiffResult {
  hunks: DiffHunk[];
}

export interface VaultHomeStats {
  total_notes: number;
  total_chunks: number;
  indexed: number;
  skipped: number;
  queued: number;
}

export interface RecentNote {
  path: string;
  title: string;
  mtime: number;
  last_accessed_at: number | null;
}

export interface RollbackOutcome {
  prior_change_id: number;
  path: string;
  new_hash: string;
}

export interface AtSuggestion {
  relPath: string;
  basename: string;
  parentDir: string;
  lastAccessedAt: number | null;
}

export interface ResolvedAtNote {
  relPath: string;
  content: string;
}

export interface ActiveSessionDto {
  sessionId: string;
  relPath: string;
  turns: { user: string; agent: string }[];
}

export interface SessionListItem {
  sessionId: string;
  relPath: string;
  mtimeUnix: number;
  firstUserPreview: string;
  turnCount: number;
  isActive: boolean;
}

export interface ChatContextBlock {
  kind: "activeNote" | "selection" | "note";
  relPath: string;
  content: string;
  lineRange?: string | null;
}

export type IndexScope =
  | { kind: "all" }
  | { kind: "path"; rel: string };

export interface SubmitMutationOutcome {
  task_id: string;
}

export interface AutosaveTabState {
  open_paths: string[];
  active_path: string | null;
  preview_path: string | null;
  saved_at_ms: number;
}

export interface AutosaveRecoveredEntry {
  path: string;
  autosave_id: string;
  autosaved_content: string;
  autosaved_hash: string;
  on_disk_hash: string | null;
  saved_at_ms: number;
}

// Loose DTO for `task_details` — full shape lives in queueDetail/.
export interface TaskDetailsDto {
  id: string;
  prompt: string;
  inputs: unknown;
  metadata: unknown;
  output_schema?: unknown;
  result?: unknown;
  error?: string;
  state: string;
  finished_at_ms?: number;
  worker?: unknown;
}

// Loose DTO for `tasks_snapshot` rows — only the fields current call
// sites read. queueDetail/'s richer `TaskRecord` is structurally
// compatible but we don't import it to avoid a circular type dep.
export interface TaskSnapshotRow {
  id: string;
  state: string;
  [key: string]: unknown;
}

// ---------------- the wrapper ----------------

/// Single seam every IPC call funnels through. On rejection, route a
/// structured event through `Logger.error("ui::ipc", ...)` so the failure
/// lands in `vault/.hiker/logs/hiker.log` (`obs-frontend-bridge`) and the
/// devtools console keeps parity for dev workflow.
///
/// Recursion guard: `log_from_frontend` is itself a Tauri command and
/// rides this same wrapper. If *its* IPC call rejects (the bridge is dead,
/// the vault is closed, etc.) we MUST NOT call `Logger.error` again — that
/// would re-enter `log_from_frontend` and infinite-loop. Special-cased by
/// command name so the guard is explicit and survives any future refactor
/// of the Logger module.
export async function invokeWithLogging<T>(
  cmd: string,
  args?: Record<string, unknown>,
  validate?: (raw: unknown) => T,
): Promise<T> {
  try {
    const raw = await tauriInvoke<unknown>(cmd, args);
    // Validators (used for the three high-blast-radius commands per
    // `bug-ipc-responses-untyped`) throw on shape mismatch; the throw
    // rides the same rejection path as any other IPC error so callers
    // see the same `IpcError`-shaped failure they already handle.
    return validate ? validate(raw) : (raw as T);
  } catch (err) {
    if (cmd === "log_from_frontend") {
      // Bridge itself failed. Fall back to console only — calling
      // `Logger.error` here would re-enter this same wrapper.
      console.error("ipc command failed", { command: cmd, error: err });
    } else {
      Logger.error("ui::ipc", "ipc command failed", {
        command: cmd,
        err,
      });
    }
    throw err;
  }
}

// ---------------- typed surface ----------------

export const Ipc = {
  // ----- settings / config -----
  // `get_settings` / `set_setting` / `reload_config` /
  // `get_settings_scoped` round-trip the whole `core::config::Config`
  // shape. Different consumers need different slices (the settings pane
  // wants `SettingsConfig`; main.ts has its own `Settings` interface;
  // queueDetail just probes `tasks.*`). Each method takes a type
  // parameter that defaults to the loose `SettingsConfig` so most
  // callers don't have to think about it.
  // The `T` type parameter is preserved for callers that want to
  // narrow the return type (none today). Runtime validation always
  // runs against `SettingsConfig` shape — the wire shape is the same
  // regardless of how the caller types it. If the caller's `T` is
  // structurally different from `SettingsConfig` the cast at the end
  // is the seam they accept.
  setSetting<T = SettingsConfig>(args: {
    scope: SettingsScope;
    key: string;
    value: unknown;
  }): Promise<T> {
    return invokeWithLogging<SettingsConfig>(
      "set_setting",
      args,
      parseSettingsConfig,
    ) as Promise<T>;
  },
  getSettings<T = SettingsConfig>(): Promise<T> {
    return invokeWithLogging<SettingsConfig>(
      "get_settings",
      undefined,
      parseSettingsConfig,
    ) as Promise<T>;
  },
  getSettingsScoped<T = SettingsConfig>(args: {
    scope: SettingsScope;
  }): Promise<T> {
    return invokeWithLogging<SettingsConfig>(
      "get_settings_scoped",
      args,
      parseSettingsConfig,
    ) as Promise<T>;
  },
  reloadConfig<T = SettingsConfig>(): Promise<T> {
    return invokeWithLogging<SettingsConfig>(
      "reload_config",
      undefined,
      parseSettingsConfig,
    ) as Promise<T>;
  },
  revealConfigFile(args: { scope: SettingsScope }): Promise<void> {
    return invokeWithLogging<void>("reveal_config_file", args);
  },
  getDefaultVault(): Promise<string | null> {
    return invokeWithLogging<string | null>("get_default_vault");
  },

  // ----- vault lifecycle -----
  openVaultAt(args: { path: string }): Promise<string> {
    return invokeWithLogging<string>("open_vault_at", args);
  },
  revealInFileManager(args: { rel: string }): Promise<void> {
    return invokeWithLogging<void>("reveal_in_file_manager", args);
  },

  // ----- file I/O -----
  /// Plain file read — used by read-only preview surfaces (trash,
  /// snapshot diff target) that don't need a `BufferToken` because they
  /// never commit. Editable buffers go through `openForEdit` instead.
  readFile(args: { rel: string }): Promise<string> {
    return invokeWithLogging<string>("read_file", args);
  },
  openForEdit(args: { rel: string }): Promise<OpenForEditOutcome> {
    return invokeWithLogging<OpenForEditOutcome>("open_for_edit", args);
  },
  writeFile(args: {
    rel: string;
    contents: string;
    extraMetadata: Record<string, unknown> | null;
  }): Promise<void> {
    return invokeWithLogging<void>("write_file", args);
  },
  commitBuffer(args: {
    token: BufferToken;
    newText: string;
    extraMetadata: Record<string, unknown> | null;
  }): Promise<CommitOutcome> {
    return invokeWithLogging<CommitOutcome>("commit_buffer", {
      token: args.token,
      newText: args.newText,
      extraMetadata: args.extraMetadata,
    });
  },
  resolveDrift(args: {
    rel: string;
    choice: DriftChoice;
    newText: string;
    extraMetadata: Record<string, unknown> | null;
  }): Promise<DriftResolution> {
    return invokeWithLogging<DriftResolution>("resolve_drift", {
      rel: args.rel,
      choice: args.choice,
      newText: args.newText,
      extraMetadata: args.extraMetadata,
    });
  },
  listDir(args: { rel: string; sort: string }): Promise<DirEntry[]> {
    return invokeWithLogging<DirEntry[]>("list_dir", args);
  },
  createNote(args: { folder: string }): Promise<string> {
    return invokeWithLogging<string>("create_note", args);
  },
  moveNote(args: { from: string; to: string }): Promise<void> {
    return invokeWithLogging<void>("move_note", args);
  },
  moveFolder(args: { from: string; to: string }): Promise<void> {
    return invokeWithLogging<void>("move_folder", args);
  },
  countNotesIn(args: { rel: string }): Promise<number> {
    return invokeWithLogging<number>("count_notes_in", args);
  },
  noteAccessed(args: { rel: string }): Promise<void> {
    return invokeWithLogging<void>("note_accessed", args);
  },

  // ----- trash -----
  listTrash(): Promise<TrashListItem[]> {
    return invokeWithLogging<TrashListItem[]>("list_trash");
  },
  deleteNote(args: { rel: string }): Promise<TrashEntry> {
    return invokeWithLogging<TrashEntry>("delete_note", args);
  },
  restoreTrashEntry(args: { id: string | null }): Promise<TrashEntry> {
    return invokeWithLogging<TrashEntry>("restore_trash_entry", args);
  },
  permanentDeleteTrashEntry(args: { trashedName: string }): Promise<void> {
    return invokeWithLogging<void>("permanent_delete_trash_entry", args);
  },
  emptyTrash(): Promise<void> {
    return invokeWithLogging<void>("empty_trash");
  },

  // ----- index / chunks -----
  indexStateFor(args: { rel: string }): Promise<IndexState> {
    return invokeWithLogging<IndexState>("index_state_for", args);
  },
  chunksFor(args: { rel: string }): Promise<ChunkBounds[]> {
    return invokeWithLogging<ChunkBounds[]>("chunks_for", args);
  },
  index(args: { scope: IndexScope }): Promise<void> {
    return invokeWithLogging<void>("index", args);
  },

  // ----- search / related -----
  searchNotes(args: {
    query: string;
    modes: { semantic: boolean; lexical: boolean };
    epoch: number;
  }): Promise<SearchResponse> {
    return invokeWithLogging<SearchResponse>(
      "search_vault",
      args,
      parseSearchResponse,
    );
  },
  relatedNotes(args: { rel: string; topK: number }): Promise<RelatedHit[]> {
    return invokeWithLogging<RelatedHit[]>("related_notes", args);
  },

  // ----- diff -----
  computeDiff(args: { before: string; after: string }): Promise<DiffResult> {
    return invokeWithLogging<DiffResult>("compute_diff", args);
  },

  // ----- changes / snapshots -----
  changeContent(args: { changeId: number }): Promise<string | null> {
    return invokeWithLogging<string | null>("change_content", args);
  },
  recentChanges(args: { limit: number }): Promise<ChangeRow[]> {
    return invokeWithLogging<ChangeRow[]>(
      "recent_changes",
      args,
      parseChangeRowArray,
    );
  },
  changesCount(): Promise<number> {
    return invokeWithLogging<number>("changes_count");
  },
  restoreSnapshot(args: { changeId: number }): Promise<RollbackOutcome> {
    return invokeWithLogging<RollbackOutcome>("restore_snapshot", args);
  },

  // ----- vault-home -----
  vaultHomeStats(): Promise<VaultHomeStats> {
    return invokeWithLogging<VaultHomeStats>("vault_home_stats");
  },
  recentNotesModified(args: { limit: number }): Promise<RecentNote[]> {
    return invokeWithLogging<RecentNote[]>("recent_notes_modified", args);
  },
  recentNotesAccessed(args: { limit: number }): Promise<RecentNote[]> {
    return invokeWithLogging<RecentNote[]>("recent_notes_accessed", args);
  },

  // ----- chat -----
  chatSend(args: {
    sessionId: string | null;
    turnId: string | null;
    message: string;
    contextBlocks: ChatContextBlock[];
  }): Promise<string> {
    return invokeWithLogging<string>("chat_send", args);
  },
  chatContinue(args: {
    sessionId: string | null;
    turnId: string;
  }): Promise<void> {
    return invokeWithLogging<void>("chat_continue", args);
  },
  chatStop(args: {
    sessionId: string | null;
    turnId: string;
  }): Promise<void> {
    return invokeWithLogging<void>("chat_stop", args);
  },
  chatSessionNew(): Promise<string> {
    return invokeWithLogging<string>("chat_session_new");
  },
  chatSessionDelete(args: { sessionId: string }): Promise<void> {
    return invokeWithLogging<void>("chat_session_delete", args);
  },
  chatSessionOpen(args: {
    sessionId: string;
  }): Promise<ActiveSessionDto | null> {
    return invokeWithLogging<ActiveSessionDto | null>(
      "chat_session_open",
      args,
    );
  },
  chatSessionActive(): Promise<ActiveSessionDto | null> {
    return invokeWithLogging<ActiveSessionDto | null>("chat_session_active");
  },
  chatSessionList(): Promise<SessionListItem[]> {
    return invokeWithLogging<SessionListItem[]>("chat_session_list");
  },
  chatAtAutocomplete(args: {
    prefix: string;
    limit: number;
  }): Promise<AtSuggestion[]> {
    return invokeWithLogging<AtSuggestion[]>("chat_at_autocomplete", args);
  },
  chatResolveAtNote(args: { relNoExt: string }): Promise<ResolvedAtNote> {
    return invokeWithLogging<ResolvedAtNote>("chat_resolve_at_note", args);
  },

  // ----- mutations -----
  submitNoteMutation(args: {
    rel: string;
    mutation: string;
    sourceExtension: string;
    content: string;
  }): Promise<SubmitMutationOutcome> {
    return invokeWithLogging<SubmitMutationOutcome>(
      "submit_note_mutation",
      args,
    );
  },

  // ----- task queue -----
  // `tasks_snapshot` and `task_details` round-trip richer DTOs that
  // queueDetail/ owns. The IPC surface defaults to the loose shapes
  // declared above (so most callers don't have to think about it) and
  // accepts a type parameter so the high-detail consumer (queueDetail)
  // can specialize without `as`-casting the result. The follow-up slug
  // `bug-ipc-responses-untyped` collapses the type parameter once the
  // canonical shape lives in one place.
  tasksSnapshot<T = TaskSnapshotRow>(): Promise<T[]> {
    return invokeWithLogging<T[]>("tasks_snapshot");
  },
  tasksCancel(args: { id: string }): Promise<void> {
    return invokeWithLogging<void>("tasks_cancel", args);
  },
  taskDetails<T = TaskDetailsDto>(args: { id: string }): Promise<T | null> {
    return invokeWithLogging<T | null>("task_details", args);
  },

  // ----- autosave (autosave.md) -----
  autosaveWrite(args: { path: string; contents: string }): Promise<void> {
    return invokeWithLogging<void>("autosave_write", args);
  },
  autosaveClear(args: { path: string }): Promise<void> {
    return invokeWithLogging<void>("autosave_clear", args);
  },
  autosaveSaveTabState(args: { statePayload: AutosaveTabState }): Promise<void> {
    return invokeWithLogging<void>("autosave_save_tab_state", args);
  },
  autosaveLoadTabState(): Promise<AutosaveTabState | null> {
    return invokeWithLogging<AutosaveTabState | null>("autosave_load_tab_state");
  },
  autosaveRecover(): Promise<AutosaveRecoveredEntry[]> {
    return invokeWithLogging<AutosaveRecoveredEntry[]>("autosave_recover");
  },
  autosaveDiscard(args: { path: string }): Promise<void> {
    return invokeWithLogging<void>("autosave_discard", args);
  },

  // ----- frontend logging bridge (obs-frontend-bridge) -----
  // Fire-and-forget: callers (the `Logger` wrapper) don't await; the
  // returned promise's rejection is handled by the recursion-guarded
  // path inside `invokeWithLogging`.
  logFromFrontend(args: {
    level: "trace" | "debug" | "info" | "warn" | "error";
    target: string;
    message: string;
    fields: Record<string, unknown>;
  }): Promise<void> {
    return invokeWithLogging<void>("log_from_frontend", args);
  },
};
