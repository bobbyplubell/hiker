// status: cluster-editor-graph-view-color-by-policy
// status: cluster-editor-graph-view-size-by-members
// status: cluster-editor-graph-view-summary-staleness-tint
//
// Visual encoding helpers: policy color, log-scale size, churn tint.
// Pure functions — no DOM, no sigma. Easy to unit test if we ever
// want to.

import type { ClusterNodeRow } from "../../clusterEditor";

export type PolicyKind = "tag" | "move" | "freeze" | "none";

export interface ResolvedPolicy {
  kind: PolicyKind;
  /// True when the policy is set directly on this node, false when it
  /// walked up from an ancestor (inheritance).
  explicit: boolean;
  requireReview: boolean;
}

const POLICY_COLORS: Record<PolicyKind, string> = {
  // Hex chosen to match the legend in cluster-editor.md's mock. These
  // are tuned for OK contrast on both light and dark theme — the
  // adapter dim() blends toward neutral via alpha so theme-aware
  // backgrounds keep working.
  move: "#3b82f6", // blue
  tag: "#22c55e", // green
  freeze: "#94a3b8", // slate
  none: "#6b7280", // grey
};

export function policyOf(row: ClusterNodeRow): {
  kind: PolicyKind;
  requireReview: boolean;
} {
  if (!row.policy_json) return { kind: "none", requireReview: false };
  try {
    const p = JSON.parse(row.policy_json) as {
      kind?: string;
      require_review?: boolean;
    };
    const k = p.kind === "tag" || p.kind === "move" || p.kind === "freeze"
      ? (p.kind as PolicyKind)
      : "none";
    return { kind: k, requireReview: !!p.require_review };
  } catch {
    return { kind: "none", requireReview: false };
  }
}

/// Resolve a node's policy by walking up the parent chain when the
/// node has no explicit policy of its own.
export function resolvePolicy(
  row: ClusterNodeRow,
  byId: Map<string, ClusterNodeRow>,
): ResolvedPolicy {
  const direct = policyOf(row);
  if (direct.kind !== "none") {
    return { ...direct, explicit: true };
  }
  let cur = row.parent ? byId.get(row.parent) ?? null : null;
  while (cur) {
    const p = policyOf(cur);
    if (p.kind !== "none") {
      return { ...p, explicit: false };
    }
    cur = cur.parent ? byId.get(cur.parent) ?? null : null;
  }
  return { kind: "none", explicit: true, requireReview: false };
}

/// Color for a node given its resolved policy. Inherited policies
/// render in a softer (more transparent) shade so the user can see at
/// a glance which subtrees actually carry explicit rules vs ride
/// inherited ones.
export function policyColor(p: ResolvedPolicy): string {
  const base = POLICY_COLORS[p.kind];
  if (p.explicit) return base;
  // Inherited: render at 55% alpha against neutral.
  return blendToHexAlpha(base, 0.55);
}

/// Apply summary-staleness desaturation: nodes with churn > 0 get a
/// slight tint (alpha drop) on top of their policy color.
export function applyStalenessTint(color: string, churn: number): string {
  if (churn <= 0) return color;
  // 0.75 alpha — visible but not so faded the policy color is gone.
  return blendToHexAlpha(color, 0.75);
}

/// Size in pixels for a cluster node given its leaf member count.
/// Logarithmic so the root doesn't dwarf everything.
export function sizeForMembers(memberCount: number, isLeaf: boolean): number {
  if (isLeaf) return 4;
  // 6 (empty cluster) up to ~22 (very large).
  return Math.min(22, 6 + Math.log2(Math.max(1, memberCount)) * 2.5);
}

function blendToHexAlpha(color: string, alpha: number): string {
  if (color.startsWith("#") && color.length === 7) {
    const r = parseInt(color.slice(1, 3), 16);
    const g = parseInt(color.slice(3, 5), 16);
    const b = parseInt(color.slice(5, 7), 16);
    return `rgba(${r},${g},${b},${alpha})`;
  }
  return color;
}
