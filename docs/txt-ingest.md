# `.txt` ingest

Strategy for ingesting and rendering plain-text files. Scope is `.txt` only — `.org`, `.rst`, `.adoc`, `.pdf`, and other unstructured formats are explicitly out of scope and will get their own docs when they land.

The headline decisions:

- `.txt` files are indexed alongside `.md` (one more extension the indexer accepts; nothing else changes about the pipeline).
- The editor renders `.txt` with markdown formatting by default, with a per-vault disable toggle. A lot of "txt" content is markdown with the wrong extension; rendering it nicely is a free win and the user can turn it off when it isn't.
- No auto-detection of "this `.txt` is really markdown." Extension is the user's responsibility; we never silently re-classify or rename.
- No source rewriting, ever. The file on disk stays exactly as the user typed or pasted it.
- The chunker uses cheap deterministic heuristics — paragraph splits, structure detection, sentence packing. No LLMs in the ingest path. More expensive techniques (TextTiling, embedder-driven boundaries, LLM rewrite) are deferred future options listed at the bottom.


## Editor rendering

`.txt` files open in the same editor as `.md` files. The CodeMirror language compartment (per `editor.md`'s extension order) is set to `markdown()` by default for `.txt` too. Live preview, syntax highlighting, list autocomplete, etc. all behave as if the file were `.md`.

Per-vault config flag in `vault/.hiker/config.toml`:

```toml
[editor]
render_txt_as_markdown = true   # default
```

When `false`, `.txt` opens with the language compartment set to `null` (plain text — no markdown parsing, no decorations, no live preview). Useful for vaults that genuinely contain plain-text content (logs, transcripts, dumps).

Per-note override is deferred. If a single `.txt` in an otherwise-markdown vault needs different rendering, the right answer is to rename the extension, not to track per-file render state. Revisit if real usage shows this is too rigid.

**No autodetection.** We do not sniff `.txt` content to decide whether it "looks like markdown" and switch modes. Two reasons: (1) heuristics here are flaky on the boundary cases (a file with one `# header` line and 200 lines of plain prose), and (2) silently re-classifying a user's file is exactly the trust violation we avoid elsewhere. The user controls the extension; we render based on the extension and the vault flag, nothing else.


## Chunking pipeline

Three layers, applied in order. Each layer's output feeds the next.

### Layer 1: Paragraph splits

Split the file on blank-line runs (one or more empty lines between paragraphs). This is the workhorse — emails, articles, READMEs, meeting notes, anything written for human reading separates ideas with blank lines, and that gives the chunker clean boundaries for free.

Empty `.txt` files produce zero chunks (matching the `.md` behavior in `index.md`).

### Layer 2: Heuristic structure detection

Within the paragraph stream, recognize lightweight structural signals and treat them as virtual markdown elements for chunking purposes only — the source file is never modified.

Recognized patterns:

- **ALL-CAPS line** (3–60 characters, more than one distinct character so we don't match `============`, fewer than ~10 words) → virtual H2 heading. Starts a new chunk; carries forward as `heading_path` like the markdown chunker.
- **Underline-style headings** — a line of all `=` underneath text → virtual H1. All `-` → virtual H2. Standard reST/setext convention; common in dumped txt.
- **Numbered list** — line starts with `1.`, `2.`, etc., followed by a space → list item, kept with the preceding heading section.
- **Bullet list** — line starts with `- ` or `* ` followed by content → list item.
- **Blockquote** — line starts with `> ` → blockquote, kept with the preceding heading section.
- **Indented blocks** (4+ leading spaces or a tab on consecutive lines) → treated as a code-shaped region. Excluded from heading-promotion checks (so `if (x):` on a Python paste isn't mistaken for a label) and kept whole rather than re-flowed by Layer 3.

### Layer 3: Sentence pack within sections

Inside each section produced by Layer 2 (a heading + the text that follows), accumulate sentences into chunks until reaching the same ~1200-char soft cap as the markdown chunker. This matches `index.md`'s chunking discipline so chunk sizes stay comparable across `.md` and `.txt` content.

Sentence segmentation rules (deterministic, no library):

- A sentence ends at `.`, `?`, or `!` *followed by whitespace and a capital letter* (or end of input). The trailing-space + capital rule rejects code operators (`foo.bar`, `obj.method()`) and abbreviations followed by lowercase tokens.
- Common abbreviations checked against a small allowlist (`Mr.`, `Dr.`, `e.g.`, `i.e.`, `etc.`, `vs.`, ...) so they don't terminate sentences. Allowlist lives in `core::txt::abbreviations`.
- If a "section" has no detectable sentence terminators (e.g. a code paste with no real sentences), pack by line until the soft cap.


## Heuristic guardrails

The cheap heuristics in Layer 2 are *very* eager by default — left unchecked they produce false-positive headings on any file that happens to contain capitalized lines or `:`-suffixed paragraphs. Three guardrails:

- **Max-promotions-per-window.** No more than one ALL-CAPS-heading promotion per rolling 5-line window. A file where every line is short and capitalized (a scream-cased poem, a list of acronyms) gets at most a few virtual headings, not one per line.
- **Period-plus-space sentence rule** (already noted in Layer 3 above). Prevents `obj.method` from being seen as a sentence break.
- **Code-region exclusion.** Any region detected as code-shaped (indented, or containing a high density of `;`, `{`, `}`, `()`, `=` punctuation per line) is excluded from heading promotion. Catches the "I pasted Python in this txt" case without needing a parser.

These guardrails are tunable per-vault but ship with sensible defaults. Slugs are reserved for the tunables in case they need to surface in `settings.md` later, but for v1 the defaults are baked in.


## Module placement

Chunking logic for `.txt` lives in `core::chunker::txt`, sibling to `core::chunker::markdown`. Both implement a `Chunker` trait with `fn chunk(&self, source: &str) -> Vec<Chunk>`. The ingest pipeline picks the chunker by extension:

```rust
let chunker: &dyn Chunker = match ext {
    "md" | "markdown" => &MARKDOWN_CHUNKER,
    "txt"             => &TXT_CHUNKER,
    _                 => return,   // not indexed in v1
};
```

Single dispatch point; no other code needs to know about extensions. Future formats slot into the same match.


## Edge cases

- **Files with no blank lines at all** (a wall of text or a log file). Layer 1 produces one giant paragraph; Layer 2 may find no headings; Layer 3 sentence-packs the whole file. Acceptable degradation — the file is still indexed and searchable, just with coarser granularity. If this proves too lossy in practice, the deferred TextTiling option (below) is the fix.
- **Files that are mostly empty lines** (something poorly exported). Layer 1 produces lots of tiny paragraphs; Layer 3 packs them up to the soft cap. Works fine.
- **Mixed-encoding or non-UTF-8 content.** The reader is UTF-8-only. Files that fail UTF-8 decode are skipped with a warning, matching the `.md` behavior. If users hit this, a `--lossy` flag on `hiker reindex` could be added later (replaces invalid sequences with U+FFFD).
- **Very large `.txt` files** (>5MB). Same sanity cap as `.md` per `index.md` — log and skip. Likely an accidentally-imported log dump.


## Deferred future options

Listed for forward-pointer / "we considered this":

- **TextTiling** (Hearst 1997) — deterministic semantic-cohesion-based chunk boundaries via sliding window of bag-of-words cosine similarity. Useful when Layer 1 + 2 produce poor boundaries on long unbroken prose.
- **Embedder-based semantic boundaries** — embed sentences with the existing embedder, place chunk boundaries where similarity to the running chunk centroid drops below a threshold. Reuses the embedder we already have but adds N embed calls per file just for chunking.
- **LLM rewrite to markdown** — most flexible, most expensive, lossy in subtle ways. Would have to be a sidecar (`.hiker/derived/<path>.md`), never a source-file mutation. Opt-in user action, not an ingest default.
- **Content-shape fingerprinting** — a per-note structural fingerprint (code-byte share, detected languages, table/list/link density, heading count, frontmatter presence) computed deterministically from the chunk pass. Tells you *what shape* a note is — mostly code, mostly prose, link-heavy, tabular — without understanding what it's about. Cheap to compute (no LLM, no embedder), unlocks UI badges, search filters, and a clustering signal, and would let the chunker pick a strategy by shape (e.g. code-heavy `.txt` skips Layer 2 and uses line-packed chunks). Not specced; speculative until there's a concrete use that justifies the surface.

These are not on the v1 roadmap. Reach for one only when the cheap layers prove inadequate on real content.


## Out of scope

- Other unstructured formats (`.org`, `.rst`, `.adoc`, `.pdf`, etc.) — separate docs when they land. The `txt-ingest` strategy may generalize but we won't pretend it does until we've worked through the format-specific concerns.
- Per-note render-mode override (the per-vault default suffices for v1).
- Auto-detection of "this `.txt` is really markdown" — explicitly rejected per the headline decisions.
- Encoding negotiation beyond UTF-8 in v1.
- A `hiker convert` command to upgrade `.txt` → `.md`. Possibly worth building later as an explicit user action; out of scope for the ingest spec itself.
