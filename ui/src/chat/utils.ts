// Small chat-panel utility helpers. Extracted from `ui/src/chat.ts` so
// that file stays under the TS file-length cap; behavior is unchanged.

export function shortLabel(s: string, max: number): string {
  const one = s.split(/\r?\n/, 1)[0] ?? "";
  if (one.length <= max) return one;
  return one.slice(0, max - 1) + "…";
}

export function formatShortDate(unix: number): string {
  if (!unix) return "";
  const d = new Date(unix * 1000);
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${d.getFullYear()}-${m}-${day}`;
}

export function clamp(x: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, x));
}

// Re-exported from the canonical pipeline so chat callers keep their
// existing import path. See `ipc/runCommand.ts`.
export { describeErr } from "../ipc/runCommand";
