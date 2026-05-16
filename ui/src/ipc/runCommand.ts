// Standardized error -> toast pipeline for Tauri command failures.
//
// Tauri commands return typed `CmdError` (serialized as a string for wire
// compat). Pre-this-module, ~50 ad-hoc error-handling sites used some mix
// of `describeErr`, `formatError`, `err.message`, `String(err)`, raw
// `console.error`, raw `Logger.error`, with toasts wired in only at a few
// specific seams. A backend failure could surface as a toast, get silently
// swallowed, or only land in devtools — depending which seam you hit.
//
// `runCommand` is the one place that turns a failed Tauri call into a
// user-visible toast + structured log. Call sites adopt it by wrapping
// their `Ipc.X(args)` in `runCommand("ipc.X", () => Ipc.X(args))`. The
// `silent: true` opt-out preserves polling / background callers that
// legitimately fail during init and shouldn't toast on every poll.
//
// `describeErr` here is the canonical error stringifier. It replaces the
// per-file copies in `main.ts`, `chat/utils.ts`, `diff/index.ts`,
// `clusterReviewTab/index.ts` — all of which were near-duplicates.

import { showToast } from "../widgets/toast";
import { Logger, type UiTarget } from "../logger";

export interface RunOpts {
  /// Suppress the user-visible toast on failure. Useful for polling /
  /// background calls where errors are noise. Failure still gets logged.
  silent?: boolean;
  /// Override the user-facing message. Default: `${name} failed: ${err}`.
  /// Pass a function to compute it from the caught error.
  message?: string | ((err: unknown) => string);
  /// Optional logger target. Default `"ui::ipc"`.
  logTarget?: UiTarget;
}

export async function runCommand<T>(
  name: string,
  fn: () => Promise<T>,
  opts: RunOpts = {},
): Promise<T | null> {
  try {
    return await fn();
  } catch (err) {
    const messageText = typeof opts.message === "function"
      ? opts.message(err)
      : (opts.message ?? `${name} failed: ${describeErr(err)}`);
    Logger.error(opts.logTarget ?? "ui::ipc", messageText, { name, err });
    if (!opts.silent) {
      showToast(messageText);
    }
    return null;
  }
}

/// Canonical error-formatting helper. Replaces the per-file
/// `describeErr` / `formatError` / `formatErr` variants and the
/// inline `err.message` / `String(err)` constructions that were
/// scattered across the codebase.
export function describeErr(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  if (err && typeof err === "object") {
    const anyErr = err as { message?: unknown };
    if (typeof anyErr.message === "string") return anyErr.message;
    try { return JSON.stringify(err); } catch { /* fall through */ }
  }
  return String(err);
}
