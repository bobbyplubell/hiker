import { keymap } from "@codemirror/view";
import type { EditorView } from "@codemirror/view";
import type { Extension } from "@codemirror/state";

export interface Binding {
  id: string;
  keys: string;
  label: string;
  run: (view: EditorView) => boolean;
}

const bindings: Binding[] = [];

export function register(binding: Binding): void {
  bindings.push(binding);
}

export function list(): readonly Binding[] {
  return bindings;
}

export function validate(): void {
  const ids = new Set<string>();
  const keys = new Set<string>();
  const conflicts: string[] = [];
  for (const b of bindings) {
    if (ids.has(b.id)) conflicts.push(`duplicate id: ${b.id}`);
    if (keys.has(b.keys)) conflicts.push(`duplicate keys: ${b.keys}`);
    ids.add(b.id);
    keys.add(b.keys);
  }
  if (conflicts.length > 0) {
    throw new Error(`keybind registry conflicts:\n  ${conflicts.join("\n  ")}`);
  }
}

export function toCMKeymap(): Extension {
  return keymap.of(bindings.map((b) => ({ key: b.keys, run: b.run })));
}
