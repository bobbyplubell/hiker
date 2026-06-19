# QA / evaluation

How we validate retrieval quality and (later) auto-organization quality — eval verifies "the results are useful," distinct from unit tests (see "What this is *not*"). Build out only when there's actual content to evaluate against; premature eval infrastructure on a 0-note vault is wasted motion.


## Related-notes evaluation

Three layers, in build order:

### 1. Golden-set eval (build first, cheap)

`vault/.hiker/eval.yaml`:

```yaml
- source: design/hiker.md
  expected_top:
    - design/index.md
    - design/editor.md
- source: research/embeddings.md
  expected_top:
    - research/vector-databases.md
    - research/sqlite-vec.md
```

**The eval runner lives in the same external Python tool** (`tools/eval-synth/`) that generates synthetic corpora — gen + scoring + reporting in one place. The tool calls into hiker for retrieval via a small CLI primitive (`hiker query <q>` initially) or via MCP once that's real (post-v3). Hiker doesn't grow a `hiker eval` subcommand. [eval-golden-set]
status:: planned
note:: `vault/.hiker/eval.yaml` + `hiker eval`

The tool runs `related_notes(source)` for each entry and checks whether expected paths appear in top-k. Reports:

- **Recall@5** — fraction of expected paths that landed in the top 5
- **Recall@10** — same, top 10
- **MRR** — mean reciprocal rank of the first matching expected path

10–20 hand-curated entries is enough to detect regressions; not a benchmark for publication, but right-sized for a personal tool. Re-run after any tuning change (chunk size, embedder swap, scoring math) and compare.

The eval file lives in the vault by default. User decides whether to commit it via Syncthing/git/etc. — for solo use it's basically a per-vault asset.

### 2. Live thumbs feedback (add when v1 panel ships)

Each row in the related-notes panel gets a 👍 / 👎. Clicks append to `vault/.hiker/feedback.jsonl`: [eval-thumbs-feedback]
status:: planned
note:: per-row up/down → feedback.jsonl

```jsonl
{"source": "...", "hit": "...", "rank": 2, "verdict": "up", "ts": 1747000000}
```

Two uses:

- The log becomes a growing eval set with no curation effort.
- Later, can feed a per-user reranker that boosts/demotes specific note pairs.

The same data shape feeds the curated-tree reconcile flow when that lands — a rejected placement is a thumbs-down on a placement decision.

### 3. Sanity dashboards (cheap, catches "is it broken")

`hiker stats` subcommand prints: [eval-sanity-stats]
status:: planned
note:: chunk-distribution, mean top-1 sim, orphans, mutual-top-1

- Distribution of chunks-per-note (huge tail = chunker pathology)
- Mean / median top-1 similarity across the vault (collapsing toward 0 or saturating at 1 = embedder is broken)
- Notes with zero related-notes hits (almost-orphans — interesting in their own right)
- Mutual-top-1 pairs (sanity that the symmetry is reasonable)

Doesn't answer "is it good?" but catches "is it broken?" cheaply, and gives you a feel for the corpus shape.


## Auto-organization evaluation (curated tree, much later)

Lands when curated-tree placement is built (design.md `hiker reconcile` flow). Two ground-truth sources: [eval-auto-org]
status:: planned
note:: manual-placement holdout + reconcile-history regression

- **Manual placements** — every note with `hiker.placement: manual` is a ground-truth label for "where it should go." Eval flow: hide the placement, ask the placer where it would put the note, score = fraction of placements within ±1 tree-level of the manual choice.
- **Reconcile-history** — `vault/.hiker/reconcile-history.yaml` already records rejected proposals. Re-running reconcile and re-proposing something previously rejected is a regression signal.

Cluster coherence as a secondary metric: per-cluster mean intra-distance vs nearest-other-cluster mean distance. Higher gap = better-formed clusters.


## Synthetic corpora (bootstrap when real notes are thin)

A personal vault starts empty, which means both the golden-set and the auto-org eval are starved for ground truth on day one. One way around this: generate synthetic notes across a spread of topics (e.g. ask an LLM for N notes across M domains, with intentional cross-links and near-duplicates) and use them as a seed corpus. [eval-synthetic-corpus]
status:: planned
note:: LLM-generated topical notes for bootstrap benchmark; implementation lives in [[spec:eval-synth-tool]] (external Python), not hiker

**Implementation: external Python script, not a hiker feature.** Synthetic-corpus generation is a one-off batch workload that doesn't fit hiker's LLM strategy (`llm.md`'s one-action-one-prompt rule for non-fan-out features) and doesn't earn its keep being implemented in Rust. The tool lives at `tools/eval-synth/` as a plain Python script with a `requirements.txt` next to it (no packaging ceremony). Uses litellm for multi-provider LLM access, writes notes into a vault directory, exits. Hiker indexes the resulting notes like any other content; nothing in `core/` knows about the generator. [eval-synth-tool]
status:: partial
note:: v0 (`gen` subcommand) lives at `tools/eval-synth/eval-synth.py`; topic taxonomy in `topics.yaml`, prompts in `prompts/note.md` + `prompts/note-txt.md`, syntax-paste fixtures in `pastes/<kind>/`. `.md` notes stamp `hiker.provenance: synthetic-corpus` + `hiker.author: imported` per design.md authorship trichotomy. Ground-truth manifest (canonical, including `.txt`) at `<out>/.synth/manifest.jsonl`. Knobs: `--txt-rate` writes a fraction as plain `.txt` (no frontmatter, per txt-ingest.md:105 leading-`---`-is-content rule); `--paste-rate` splices fixture syntax (sql/shell/json/python/tcpdump/regex) into eligible topics — fenced in `.md`, indented/raw/inline in `.txt` to exercise txt-ingest's code-region exclusion ([[spec:txt-chunker-guardrails]]). Pathology mix (near-dup / topic-drift / very-short / very-long) at ~10%. Runner / scoring / recall@K still deferred until [[spec:cli-query]] lands

**v0 scope: corpus generation only.** The first slice of `tools/eval-synth/` is just `gen` — produce N notes across a topic taxonomy, write them as `.md` files into a target vault directory. Frontmatter stamps `hiker.provenance: synthetic-corpus` so generated notes are filterable as imported (per `design.md`'s authorship trichotomy). Runner / scoring / `recall@K` reporting wait on [[spec:cli-query]] (the small hiker CLI primitive [[spec:eval-synth-tool]] calls to run real queries against an indexed vault) — that's separate work in the hiker repo, deferred until the corpus generator is real and there's something concrete to score against. Thumbs feedback ([[spec:eval-thumbs-feedback]]) is a different feature with its own UI surface and isn't part of this script's scope.

**Inputs/outputs (sketch).** Topic spec is a YAML file: list of topics with optional inter-topic crosslink hints. Generator's CLI shape is `eval-synth.py gen --topics topics.yaml --count 200 --out /path/to/vault [--model anthropic/claude-haiku-4-5 ...]`. Provider/model/keys via env (`ANTHROPIC_API_KEY` etc.) per litellm conventions. Prompts and topic specs check into the repo so runs are reproducible.

Useful because:

- Topic labels are known by construction → related-notes ground truth comes for free (notes in the same topic *should* cluster; cross-linked pairs *should* surface each other).
- Auto-org placement has a known target tree (the topic taxonomy you generated against), so reconcile proposals can be scored without manual labels.
- Lets us stress-test pathologies cheaply: near-duplicates, topic drift within a single note, very short notes, very long notes, notes that legitimately belong in two places.

Caveats:

- Synthetic notes don't capture the user's actual writing style, vocabulary quirks, or the long-tail "weird" notes that real vaults accumulate. Good-on-synthetic ≠ good-on-real.
- Risk of over-fitting tuning choices to whatever the generator's distribution looks like. Treat synthetic scores as a *floor* (if it can't do this, it's broken) not a ceiling.

Build timing: opportunistic, but probably worth it *before* the golden-set on a real vault — it's the only way to get auto-org eval signal before the curated-tree feature has been used in anger.


## Build timing

- v1 ships with **none** of this — the panel needs to exist before there's anything to evaluate.
- **Right after v1 + first real notes**: golden-set + `hiker eval`. ~2 hours.
- Thumbs feedback: build only after you've felt the panel's quality on real notes. Might turn out obviously fine or obviously bad without needing the data pipeline.
- Sanity dashboards: opportunistic — write when you suspect something's off and want a quick check.
- Auto-org eval: with the curated-tree feature itself. Inseparable from the feature.


## What this is *not*

- A benchmark suite for comparing Hiker to other tools.
- A research-grade IR evaluation. Personal tool, personal eval.
- A substitute for unit tests. Eval validates "are results useful"; unit tests validate "does the code do what it says." Both are needed.
