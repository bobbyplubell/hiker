# CLI

A thin command-line surface over the vault's core operations — shell-scriptable verbs that share the same `core::` ops the UI calls, so the CLI and the app can never drift on semantics. Each command is a primitive: it runs one operation against an indexed vault and prints a result, leaving orchestration to the caller's shell.

## Note operations

- **Move a note** — `hiker mv <from> <to>` shares the [[spec:move-note-core-cmd]] op with the file-tree drag-and-drop, so a CLI move rewrites referrers, updates the index, and op-logs the rename identically to a UI move. [cli-mv]
  status:: planned
- **Remove a note** — `hiker rm <path>` shares the [[spec:delete-note-core-cmd]] op: a soft delete into the trash (recoverable), not a permanent unlink. `--yes` bypasses the interactive confirmation for scripted use. [cli-rm]
  status:: planned

## Trash

- **List the trash** — `hiker trash list` enumerates the trash manifest: each soft-deleted entry's id, original path, and deletion time. [cli-trash-list]
  status:: planned
- **Restore from trash** — `hiker trash restore <id|path>` restores a soft-deleted note by its manifest id or by its original path. [cli-trash-restore]
  status:: planned
- **Empty the trash** — `hiker trash empty` permanently deletes every trash entry — the one CLI verb that unlinks for real (the trash is the soft-delete buffer that `cli-rm` writes into). [cli-trash-empty]
  status:: planned

## Reindex

- **Reindex** — `hiker reindex` runs the `index.md` ingest pipeline over the vault (the operational counterpart to the in-app Reindex verbs). [cli-reindex]
  status:: planned
- **Reindex (rebuild)** — `hiker reindex --rebuild` drops and recreates the schema before reindexing, covering the destructive-rebuild case that the in-app verb defers to a future settings UI (`settings.md`, `index.md`). [cli-reindex-rebuild]
  status:: planned

## Query and stats

- **Query** — `hiker query` is a thin primitive that runs a single search / related query against the indexed vault and prints the results. It exists so the external eval tool ([[spec:eval-synth-tool]]) has something concrete to score against until MCP is real. [cli-query]
  status:: planned
- **Stats** — `hiker stats` prints sanity dashboards over the index (the corpus / health numbers `qa.md` describes). [cli-stats]
  status:: planned

## Trails

- **List trails** — `hiker trail list` enumerates the vault's trails, sharing `core::trails::list_trails` with [[spec:mcp-tool-trails-list]]. [cli-trail-list]
  status:: planned
- **Show a trail** — `hiker trail show <id>` prints a trail's body plus its ordered waypoint list, sharing `core::trails::get_trail` with [[spec:mcp-tool-trail-get]]. [cli-trail-show]
  status:: planned
- **Activate a trail** — `hiker trail activate <id>` sets the active trail for the vault from a shell-script-driven workflow — the same op the sidebar's trail dropdown calls. [cli-trail-activate]
  status:: planned
- **New trail** — `hiker trail new <name>` creates a new trail at the configured `[trails] new_trail_dir`, sharing the `core::trails::create_trail` op. [cli-trail-new]
  status:: planned
