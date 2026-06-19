# Source import & viewers

Hiker is a vault you read, organize, and search. Content that originates outside
the vault — a web page, a PDF, a crawled site — is **produced by external tools and
imported**; hiker itself does not fetch or scrape the web. This keeps the trust
boundary clean: the tools that touch untrusted web content and run transformations
are vault-blind and secret-blind, and hiker only ever ingests the result — a
compromised producer can at worst hand over a bad import the user reviews.

The headline decisions:

- **Acquisition is external; hiker imports and displays.** An external producer (a
  crawler, a converter, a script) acquires content and hands hiker a directory to
  ingest. [import-external-producer-boundary]
status:: planned
note:: acquisition lives in external, vault-blind/secret-blind producers; hiker only imports the result
- **One tool-agnostic import manifest.** A `manifest.json` describes the import: for
  each item, its file(s), what each file *is*, and which viewer renders it —
  `{ files: [{ path, kind, viewer }], … }`. Any producer targets the same format;
  hiker does only the vault integration. [import-manifest]
status:: planned
note:: tool-agnostic `manifest.json` (`{files:[{path,kind,viewer}],…}`); any producer targets it
- **A finite, built-in viewer registry.** Each content kind maps to a built-in
  viewer. Today: markdown, and HTML/CSS through the no-JS `hiker-htmlview` renderer
  (shared with the ZIM viewer). PDF, image, and audio viewers come later. The set is
  finite and built-in — adding a kind is a code change, not a plugin. [import-viewer-registry]
status:: planned
note:: finite built-in viewer set keyed by content kind; markdown + `hiker-htmlview` (html/warc) now, pdf/image later; adding a kind is a code change, not a plugin
- **Display and index are separate layers.** The original file is *displayed* through
  its viewer; a markdown *shadow* is what gets indexed and searched. Rich rendering
  and clean retrieval without compromising either. [import-display-vs-index]
status:: planned
note:: original displayed via its viewer; the markdown shadow is the indexed/searched layer
- **Two import shapes.** (1) an original file plus a `.md` sidecar shadow beside it —
  hiker displays the original, indexes the sidecar; (2) a bare `.md` note — already
  markdown, nothing special. The shadow is always the search layer. [import-shapes]
status:: planned
note:: two shapes: original + `.md` sidecar shadow, or a bare `.md` note
- **The display hint is just a viewer id.** A manifest item names `viewer: <id>`,
  nothing more — declarative data, not a layout language. [import-viewer-hint]
status:: planned
note:: per-item `viewer:<id>` only; no params/layout language
- **WARC archives view through the ZIM pattern.** A `.warc` is a container; the viewer
  reads it, renders the main resource through `hiker-htmlview`, and serves
  subresources offline from a resource provider — the same shape as the ZIM viewer. [import-warc-viewer]
status:: planned
note:: `.warc` viewed via the ZIM pattern (container reader + `hiker-htmlview` + offline resource provider)
- **Versioning rides the ordinary save path.** Hiker performs **no in-process
  extraction** — re-importing a changed source is the producer re-emitting the
  manifest, and the importer lands the new shadow through the normal note-write
  path, so history / diff / restore come from the existing surfaces. There is no
  in-process `extractor` re-extraction seam (removed under manifest-only ingest).
  Binary artifacts (the original, an archive) are retained per a user-set cascade
  and stay device-local. [import-versioning-oplog]
status:: planned
note:: re-import = the producer re-emitting the manifest; the importer lands the new shadow via the normal write path (no in-process re-extraction); artifacts retained per cascade, device-local
- **Imported content is searchable by source type.** Every imported note stamps its
  provenance (web, pdf, …) so search can include or exclude by source type — heavy
  imported content never crowds out hand-written notes. Spec in `search.md`. [import-source-type-facet]
status:: planned
note:: imported notes stamp provenance so search can include/exclude by source type (pairs with [[spec:search-source-type-filter]])
- **Producer output is untrusted.** The importer validates the manifest and every
  path (no writes outside the import target, no traversal). [import-untrusted-input]
status:: planned
note:: importer validates manifest + paths (no traversal/escape); producer is outside the trust boundary


## Importer

Core owns the importer: read the manifest, place the notes (and any archives under
`.hiker/refs/`), rewrite cross-item links into wikilinks, and version on re-import.
The `.md` shadows then flow through the ordinary markdown indexing path — the
importer is a front door, not a parallel index. [import-core-importer]
status:: planned
note:: core owns the importer: place notes + archives, wikilink-rewrite cross-item links, version on re-import; shadows ingest via the normal markdown path


## Future

- **Self-sufficient indexing of raw files at rest.** Indexing an `.html` / `.warc` /
  `.pdf` dropped straight into the vault with no sidecar — hiker derives its own text
  shadow. Held until content-type-aware chunking (HTML structure, PDF pages) and the
  source-type search facet land. [import-raw-self-sufficient]
status:: future
note:: core derives its own text shadow from a raw `.html`/`.warc`/`.pdf` at rest (no sidecar); needs content-type chunking + the source-type facet first
- **PDF text indexing.** A PDF's text layer indexed like `.txt` — one more extension
  the indexer accepts, cheap deterministic chunking (the `txt-ingest.md` pattern) —
  with the PDF itself opened in its viewer. [import-pdf-as-txt]
status:: future
note:: PDF text indexed like `.txt` (cheap deterministic chunker, `txt-ingest.md` pattern); the PDF opens in its viewer
- **More viewers.** PDF, image, and audio viewers as built-in registry additions. [import-viewer-types-future]
status:: future
note:: pdf/image/audio viewers as built-in registry additions
