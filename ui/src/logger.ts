// status: obs-frontend-bridge
//
// UI-side logger. Routes structured events through the
// `log_from_frontend` Tauri command so they land in
// `vault/.hiker/logs/hiker.log` alongside the rest of the `tracing` stream.
// Also dual-writes to the devtools `console.<level>` so dev workflow stays
// unchanged — devtools keep their value, the file just gains parity.
//
// The IPC client (`ui/src/ipc/index.ts`) is the canonical first caller:
// every `invoke` rejection routes through `Logger.error("ui::ipc", ...)`
// so we get one log entry per failed command rather than dozens of
// per-panel `console.error`s. Panel-side `console.error` / silent-catch
// sites also migrate here per `bug-ui-errors-not-routed-to-tracing`.
//
// Recursion: `Ipc.logFromFrontend` itself rides `invokeWithLogging`. The
// rejection path there short-circuits to `console.error` for the
// `log_from_frontend` command name so a broken bridge can't infinite-loop
// the logger. See `ipc/index.ts::invokeWithLogging`.
//
// Discipline (mirrors `obs-no-content` / `obs-no-secrets`): callers MUST
// NOT pass note body text, embeddings, or auth tokens through `fields`.
// The bridge does not strip — reviewers reject `Logger.*` calls that
// include buffer content.

import { Ipc } from "./ipc";

/// Allowed UI targets. Constrained to the `ui::` prefix per
/// `obs-frontend-bridge` so the namespace stays clean for filtering.
/// New panel? Add the slug here.
export type UiTarget =
  | "ui::ipc"
  | "ui::tree"
  | "ui::discovery"
  | "ui::vault-home"
  | "ui::queue-detail"
  | "ui::chat"
  | "ui::mutations"
  | "ui::snapshot-preview"
  | "ui::trash"
  | "ui::trails"
  | "ui::mode-controls"
  | "ui::app"
  | "ui::properties-pane";

/// Loose field map. `err` is treated specially below — `Error` instances
/// (and other non-string values) are stringified before crossing the IPC
/// boundary so the log line carries `err = "RangeError: ..."` rather than
/// `err = {}` (the default `JSON.stringify` for `Error`).
export type Fields = Record<string, unknown>;

type Level = "trace" | "debug" | "info" | "warn" | "error";

/// Stringify an error-shaped value for the logger. Mirrors the existing
/// main-side `describeErr` posture but lives here so panels can route
/// arbitrary `unknown` errors through `Logger.error("...", { err })`
/// without thinking about it.
function describeErr(err: unknown): string {
  if (err instanceof Error) {
    return err.stack ?? `${err.name}: ${err.message}`;
  }
  if (typeof err === "string") return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/// Normalize `fields` for the IPC boundary. `Error` instances are
/// `describeErr`'d; everything else round-trips as-is (Tauri serdes the
/// payload as JSON). The bridge writes the whole object as a single
/// stringified `fields=...` value on the tracing event.
function normalizeFields(fields: Fields | undefined): Record<string, unknown> {
  if (!fields) return {};
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(fields)) {
    if (v instanceof Error) {
      out[k] = describeErr(v);
    } else if (k === "err" && v !== undefined) {
      // Caller routed an `unknown` through the conventional `err` slot.
      // Stringify defensively so we never serialize `{}`.
      out[k] = typeof v === "string" ? v : describeErr(v);
    } else {
      out[k] = v;
    }
  }
  return out;
}

function emit(
  level: Level,
  target: UiTarget,
  message: string,
  fields?: Fields,
): void {
  const normalized = normalizeFields(fields);

  // Dual-write to the devtools console at the matching level. Keeping
  // parity with the on-disk log lets dev workflow stay devtools-first
  // without having to tail a file.
  const consoleArgs: unknown[] = [`[${target}] ${message}`];
  if (Object.keys(normalized).length > 0) consoleArgs.push(normalized);
  switch (level) {
    case "trace":
      // `console.trace` prints a stack trace which is wrong for our use;
      // fall through to `debug` for the dual-write.
      console.debug(...consoleArgs);
      break;
    case "debug":
      console.debug(...consoleArgs);
      break;
    case "info":
      console.info(...consoleArgs);
      break;
    case "warn":
      console.warn(...consoleArgs);
      break;
    case "error":
      console.error(...consoleArgs);
      break;
  }

  // Fire-and-forget over IPC. The bridge's own failures are handled by
  // the recursion-guarded path in `invokeWithLogging` (falls back to
  // `console.error` for the `log_from_frontend` command name) so we
  // intentionally don't `await` here and don't attach a rejection
  // handler — the seam already logged.
  void Ipc.logFromFrontend({
    level,
    target,
    message,
    fields: normalized,
  }).catch(() => {
    // Already handled inside `invokeWithLogging` for this command;
    // swallow here so an unhandled rejection doesn't escape.
  });
}

/// Public surface. Each method takes a `UiTarget`, a grep-stable message,
/// and an optional structured-fields object. The fields are the contract;
/// the message is for human grep.
export const Logger = {
  error(target: UiTarget, message: string, fields?: Fields): void {
    emit("error", target, message, fields);
  },
  warn(target: UiTarget, message: string, fields?: Fields): void {
    emit("warn", target, message, fields);
  },
  info(target: UiTarget, message: string, fields?: Fields): void {
    emit("info", target, message, fields);
  },
  debug(target: UiTarget, message: string, fields?: Fields): void {
    emit("debug", target, message, fields);
  },
  trace(target: UiTarget, message: string, fields?: Fields): void {
    emit("trace", target, message, fields);
  },
};
