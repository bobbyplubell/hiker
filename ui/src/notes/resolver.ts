// Thin wrapper over the `chat_resolve_at_note` IPC call.
//
// `@<rel-path-no-ext>` tokens in chat input (and any future at-mention
// surface — queue-detail "show source note", etc.) need the same probe:
// hand the backend a vault-relative path without an extension, get back
// the actual rel-path (with extension) plus the file body. Callers used
// to invoke the IPC directly with their own shaping; centralizing here
// keeps the call shape consistent and gives future surfaces one import.

import { Ipc } from "../ipc";

export async function resolveAtNote(
  rel: string,
): Promise<{ relPath: string; content: string }> {
  const resolved = await Ipc.chatResolveAtNote({ relNoExt: rel });
  return { relPath: resolved.relPath, content: resolved.content };
}
