// Hand-rolled runtime validators for the high-blast-radius IPC commands —
// `get_settings` / `get_settings_scoped` / `reload_config`, `search_vault`,
// `recent_changes`. Same `parse*` shape zod's `.parse()` exposes: each fn
// takes an unknown value, returns the typed value on success, throws a
// clear `Error` on shape mismatch.
//
// Why hand-rolled and not zod: zod isn't a dep (see `ui/package.json`)
// and bug_tracking.md / hiker-dev rules prefer not adding new deps without
// user input. The three commands picked here are the ones whose shapes
// are read from many surfaces — silent destructure of `undefined` if the
// Rust DTO drifts is the failure mode. The other commands keep the
// TypeScript-only typed wrappers in `index.ts`.
//
// Validators throw an `Error` whose message names the offending command +
// path; the throw rides the same rejection path as any other IPC error
// (callers already handle `IpcError` shape). No special error class —
// keep it boring.

import type { SearchResponse, SearchNoteHit } from "./index";
import type { ChangeRow, ChangeOp, AuthorClass } from "../snapshotPreview";
import type { SettingsConfig } from "../settings";

// ---------------- tiny primitive helpers ----------------

function fail(cmd: string, path: string, expected: string, got: unknown): never {
  const t = got === null ? "null" : Array.isArray(got) ? "array" : typeof got;
  throw new Error(
    `ipc[${cmd}]: ${path} — expected ${expected}, got ${t}`,
  );
}

function isObject(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function expectObject(
  cmd: string,
  path: string,
  v: unknown,
): Record<string, unknown> {
  if (!isObject(v)) fail(cmd, path, "object", v);
  return v;
}

function expectArray(cmd: string, path: string, v: unknown): unknown[] {
  if (!Array.isArray(v)) fail(cmd, path, "array", v);
  return v;
}

function expectString(cmd: string, path: string, v: unknown): string {
  if (typeof v !== "string") fail(cmd, path, "string", v);
  return v;
}

function expectNumber(cmd: string, path: string, v: unknown): number {
  if (typeof v !== "number") fail(cmd, path, "number", v);
  return v;
}

function expectBool(cmd: string, path: string, v: unknown): boolean {
  if (typeof v !== "boolean") fail(cmd, path, "boolean", v);
  return v;
}

function expectStringOrNull(
  cmd: string,
  path: string,
  v: unknown,
): string | null {
  if (v === null) return null;
  if (typeof v !== "string") fail(cmd, path, "string | null", v);
  return v;
}

// ---------------- search_vault ----------------

const CMD_SEARCH = "search_vault";

function parseSearchNoteHit(path: string, v: unknown): SearchNoteHit {
  const o = expectObject(CMD_SEARCH, path, v);
  return {
    note_id: expectString(CMD_SEARCH, `${path}.note_id`, o.note_id),
    path: expectString(CMD_SEARCH, `${path}.path`, o.path),
    title: expectString(CMD_SEARCH, `${path}.title`, o.title),
    score: expectNumber(CMD_SEARCH, `${path}.score`, o.score),
    chunk_id: expectString(CMD_SEARCH, `${path}.chunk_id`, o.chunk_id),
    chunk_index: expectNumber(CMD_SEARCH, `${path}.chunk_index`, o.chunk_index),
    heading_path: expectStringOrNull(
      CMD_SEARCH,
      `${path}.heading_path`,
      o.heading_path,
    ),
    snippet: expectString(CMD_SEARCH, `${path}.snippet`, o.snippet),
  };
}

function parseHitArray(path: string, v: unknown): SearchNoteHit[] {
  const arr = expectArray(CMD_SEARCH, path, v);
  return arr.map((h, i) => parseSearchNoteHit(`${path}[${i}]`, h));
}

export function parseSearchResponse(v: unknown): SearchResponse {
  const o = expectObject(CMD_SEARCH, "$", v);
  return {
    epoch: expectNumber(CMD_SEARCH, "$.epoch", o.epoch),
    lexical_hits: parseHitArray("$.lexical_hits", o.lexical_hits),
    semantic_hits: parseHitArray("$.semantic_hits", o.semantic_hits),
    fused: parseHitArray("$.fused", o.fused),
    hits: parseHitArray("$.hits", o.hits),
  };
}

// ---------------- recent_changes ----------------

const CMD_CHANGES = "recent_changes";
const CHANGE_OPS: ChangeOp[] = ["created", "modified", "deleted", "renamed"];
const AUTHOR_CLASSES: AuthorClass[] = [
  "user",
  "agent",
  "sync",
  "import",
  "other",
];

function parseChangeOp(path: string, v: unknown): ChangeOp {
  if (typeof v !== "string" || !CHANGE_OPS.includes(v as ChangeOp)) {
    fail(CMD_CHANGES, path, `one of ${CHANGE_OPS.join("|")}`, v);
  }
  return v as ChangeOp;
}

function parseAuthorClass(path: string, v: unknown): AuthorClass {
  if (typeof v !== "string" || !AUTHOR_CLASSES.includes(v as AuthorClass)) {
    fail(CMD_CHANGES, path, `one of ${AUTHOR_CLASSES.join("|")}`, v);
  }
  return v as AuthorClass;
}

function parseChangeRow(path: string, v: unknown): ChangeRow {
  const o = expectObject(CMD_CHANGES, path, v);
  // `metadata` is `serde_json::Value` on the Rust side — accept any
  // object shape; require it to be an object (not null, not array).
  const meta = o.metadata;
  if (!isObject(meta)) fail(CMD_CHANGES, `${path}.metadata`, "object", meta);
  return {
    id: expectNumber(CMD_CHANGES, `${path}.id`, o.id),
    timestamp_ms: expectNumber(
      CMD_CHANGES,
      `${path}.timestamp_ms`,
      o.timestamp_ms,
    ),
    path: expectString(CMD_CHANGES, `${path}.path`, o.path),
    op: parseChangeOp(`${path}.op`, o.op),
    author: expectString(CMD_CHANGES, `${path}.author`, o.author),
    author_class: parseAuthorClass(`${path}.author_class`, o.author_class),
    content_hash: expectStringOrNull(
      CMD_CHANGES,
      `${path}.content_hash`,
      o.content_hash,
    ),
    rename_from: expectStringOrNull(
      CMD_CHANGES,
      `${path}.rename_from`,
      o.rename_from,
    ),
    metadata: meta,
    is_current: expectBool(CMD_CHANGES, `${path}.is_current`, o.is_current),
  };
}

export function parseChangeRowArray(v: unknown): ChangeRow[] {
  const arr = expectArray(CMD_CHANGES, "$", v);
  return arr.map((r, i) => parseChangeRow(`$[${i}]`, r));
}

// ---------------- get_settings ----------------
//
// `Config` is wide and grows organically (settings-pane-deferred-sections
// keeps slots open for future tables). The pane reads a small subset by
// dotted-path lookup that tolerates missing keys; the validator's job is
// not to enforce every leaf, only to ensure the top-level shape is what
// callers destructure on. Spot-check the sections the pane reaches into
// (`editor`, `vault`, `search`, `indexing`, `llm`); leave `mcp` /
// `tasks` / future tables loose since their consumers already read them
// via `readKey` with `unknown` fallbacks.

const CMD_SETTINGS = "get_settings";

function expectObjectAt(path: string, v: unknown): Record<string, unknown> {
  return expectObject(CMD_SETTINGS, path, v);
}

export function parseSettingsConfig(v: unknown): SettingsConfig {
  const o = expectObjectAt("$", v);
  // schema_version is the load-bearing top-level invariant; the pane
  // renders the footer from it directly.
  if (typeof o.schema_version !== "number") {
    fail(CMD_SETTINGS, "$.schema_version", "number", o.schema_version);
  }

  // Each top-level section needs to be an object so the pane's dotted-path
  // walk doesn't hit `undefined` on the first hop and silently render a
  // blank pane. Sub-leaves are loosely typed in `SettingsConfig` and
  // tolerated by the pane's `readKey` walk.
  expectObjectAt("$.editor", o.editor);
  expectObjectAt("$.indexing", o.indexing);
  expectObjectAt("$.vault", o.vault);
  expectObjectAt("$.search", o.search);
  expectObjectAt("$.llm", o.llm);
  // `mcp` is `unknown` in `SettingsConfig`; just confirm presence.
  if (o.mcp === undefined) {
    fail(CMD_SETTINGS, "$.mcp", "present", undefined);
  }

  // The cast preserves the loose-shape posture of the TS interface; the
  // checks above guard the destructure paths the pane uses.
  return o as unknown as SettingsConfig;
}
