# CLI


## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **cli-mv** — shares [[spec:move-note-core-cmd]] with tree DnD [cli-mv]
  status:: planned
- **cli-rm** — shares [[spec:delete-note-core-cmd]]; soft delete; `--yes` bypasses confirm [cli-rm]
  status:: planned
- **cli-trash-list** — enumerate trash manifest [cli-trash-list]
  status:: planned
- **cli-trash-restore** — restore by id or original path [cli-trash-restore]
  status:: planned
- **cli-trash-empty** — permanent delete of all trash entries [cli-trash-empty]
  status:: planned
- **cli-reindex** — spec'd in index.md ingest pipeline [cli-reindex]
  status:: planned
- **cli-reindex-rebuild** — drop + recreate schema [cli-reindex-rebuild]
  status:: planned
- **cli-query** — thin CLI primitive that runs a single search/related query and prints results; consumed by the external eval tool until MCP is real [cli-query]
  status:: planned
- **cli-stats** — sanity dashboards (qa.md) [cli-stats]
  status:: planned
- **cli-trail-list** — enumerate trails; shares `core::trails::list_trails` with [[spec:mcp-tool-trails-list]] [cli-trail-list]
  status:: planned
- **cli-trail-show** — print a trail's body + ordered waypoint list; shares `core::trails::get_trail` with [[spec:mcp-tool-trail-get]] [cli-trail-show]
  status:: planned
- **cli-trail-activate** — set the active trail for the vault from a shell-script-driven workflow; same op the sidebar dropdown calls [cli-trail-activate]
  status:: planned
- **cli-trail-new** — create a new trail at the configured `[trails] new_trail_dir`; shares the `core::trails::create_trail` op [cli-trail-new]
  status:: planned
