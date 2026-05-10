// CSS class names that are referenced from multiple TS modules. Keeping
// the strings here (rather than scattered across `classList.add(...)` /
// `querySelectorAll(".foo")` calls) means renaming a class is a one-file
// edit on the TS side, and TypeScript catches a typo at the call site.
//
// Scope: only classes used in 2+ TS files, or named explicitly by
// `bug-css-class-strings-scattered`. Single-file class names stay inline
// — the rename-drift footgun this module exists to prevent doesn't apply
// when there's only one callsite. The CSS itself (`style.css`) is left
// untouched; this is pure indirection on the TS side.

export const Classes = {
  // tree row index-state markers — used in tree/index.ts and main.ts
  IX_UNSUPPORTED: "ix-unsupported",
  IX_SKIPPED: "ix-skipped",
  IX_QUEUED: "ix-queued",
  IX_INDEXED: "ix-indexed",

  // queue-detail filter pills — referenced by class+selector in queueDetail
  QUEUE_PILL: "queue-pill",

  // tree drag-and-drop highlight
  DROP_TARGET: "drop-target",

  // discovery panel result row (search + related-notes)
  RELATED_ITEM: "related-item",
} as const;

export const Selectors = {
  QUEUE_PILLS: ".queue-pill",
  RELATED_ITEM: ".related-item",
} as const;

// Convenience tuple for the ix-* classes since they're always swapped as
// a group (remove all four, then add the matching one).
export const IX_STATE_CLASSES: readonly string[] = [
  Classes.IX_UNSUPPORTED,
  Classes.IX_SKIPPED,
  Classes.IX_QUEUED,
  Classes.IX_INDEXED,
];
