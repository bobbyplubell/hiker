// Pure helpers consumed by the row primitive: subtree walks for
// member-count + descendant-set computation, and the policy-chip
// label renderer.

import type { ClusterNodeRow } from "./api";

export function countMembers(nodes: ClusterNodeRow[], rootId: string): number {
  let count = 0;
  const stack = [rootId];
  const childMap = new Map<string, ClusterNodeRow[]>();
  for (const n of nodes) {
    if (!n.parent) continue;
    const arr = childMap.get(n.parent);
    if (arr) arr.push(n);
    else childMap.set(n.parent, [n]);
  }
  while (stack.length) {
    const id = stack.pop()!;
    const kids = childMap.get(id) ?? [];
    for (const k of kids) {
      if (k.kind === "leaf") count += 1;
      else stack.push(k.id);
    }
  }
  return count;
}

export function collectDescendants(
  nodes: ClusterNodeRow[],
  rootId: string,
): Set<string> {
  const out = new Set<string>();
  const stack = [rootId];
  while (stack.length) {
    const id = stack.pop()!;
    for (const n of nodes) {
      if (n.parent === id && !out.has(n.id)) {
        out.add(n.id);
        stack.push(n.id);
      }
    }
  }
  return out;
}

export function renderPolicyLabel(policyJson: string | null): string {
  if (!policyJson) return "policy…";
  try {
    const p = JSON.parse(policyJson);
    if (p.kind === "tag") return `tag: ${p.slug}${p.require_review ? " ⏸" : ""}`;
    if (p.kind === "move") return `move: ${p.folder}${p.require_review ? " ⏸" : ""}`;
    if (p.kind === "freeze") return "freeze";
  } catch {}
  return "policy…";
}
