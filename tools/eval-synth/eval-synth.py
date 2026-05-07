#!/usr/bin/env python3
"""eval-synth: generate a synthetic markdown / plain-text corpus for hiker eval.

External tool, not a hiker feature. Hiker indexes the resulting notes like
any other content; nothing in core/ knows about this generator.

v0 scope: `gen` subcommand only — produce N notes across a topic taxonomy
and write them as `.md` (and optionally `.txt`) files into a target vault
directory. Each `.md` note is stamped with `hiker.provenance: synthetic-corpus`
+ `hiker.author: imported` per design.md authorship trichotomy. Ground-truth
for the future eval runner is mirrored to `<out>/.synth/manifest.jsonl`,
which is also the *canonical* source for `.txt` notes (txt-ingest.md treats
a leading `---` block as content, not metadata, so we don't write frontmatter
into `.txt`).

Runner / scoring / recall@K reporting wait on the hiker `cli-query` primitive.

Usage:
    pip install -r tools/eval-synth/requirements.txt
    python tools/eval-synth/eval-synth.py gen \\
        --topics tools/eval-synth/topics.yaml \\
        --count 200 \\
        --out /path/to/vault \\
        [--model anthropic/claude-haiku-4-5] \\
        [--seed 0] \\
        [--txt-rate 0.2] \\
        [--paste-rate 0.15]

Provider keys live in environment variables (`ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, etc.) per litellm conventions.
"""

# status: eval-synth-tool

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import sys
from pathlib import Path

import litellm
import yaml


ASPECTS = [
    "an introductory overview",
    "common pitfalls and failure modes",
    "a focused deep-dive into one subtopic",
    "historical context and how the field got here",
    "advanced techniques",
    "practical applications and real-world deployments",
    "open problems and ongoing debates",
    "how a beginner should approach the topic",
    "design tradeoffs",
    "a comparison of competing approaches",
]

LENGTHS = [
    ("short", "Keep it to 3-5 short paragraphs (~150-300 words)"),
    ("medium", "Aim for 6-10 paragraphs (~400-700 words), with 1-2 subheadings or sections"),
    ("long", "Aim for ~900-1300 words with 2-4 subheadings or sections"),
]

# Stress-test pathologies the generator intentionally seeds, on a fixed
# fraction of notes. These match the corpus shapes qa.md calls out:
# near-duplicates, very short / very long notes, topic drift.
PATHOLOGY_RATE = 0.10  # ~10% of notes

# Map paste-fence language hints (used in .md output) by kind.
PASTE_LANG = {
    "sql": "sql",
    "shell": "bash",
    "json": "json",
    "python": "python",
    "tcpdump": "",  # no canonical highlighter; emit a bare fence
    "regex": "",
}

# Where to splice a paste into a .txt note's body.
#   indented:   each line prefixed with 4 spaces (txt-ingest Layer 2 catches
#               this via the indent rule — should NOT be promoted to heading)
#   raw:        no indent, blank lines around — txt-ingest catches it via the
#               `;{}()=` density rule on consecutive lines
#   inline:     dropped mid-prose with no separator — worst case for the
#               heuristic; should still not produce false-positive headings
TXT_PASTE_STYLES = ["indented", "raw", "inline"]


def slugify(s: str) -> str:
    s = s.lower().strip()
    s = re.sub(r"[^a-z0-9]+", "-", s)
    s = re.sub(r"-+", "-", s).strip("-")
    return s or "untitled"


def render_prompt(
    template: str,
    topic: str,
    aspect: str,
    length_desc: str,
    crosslinks: list[str],
) -> str:
    out = template.replace("{{topic}}", topic)
    out = out.replace("{{aspect}}", aspect)
    out = out.replace("{{length}}", length_desc)
    if crosslinks:
        block = (
            "This note should naturally reference (in prose) related "
            f"concepts: {', '.join(crosslinks)}. Mention each by name "
            "where it fits; don't force every one in."
        )
    else:
        block = ""
    out = re.sub(r"\{\{#crosslinks\}\}.*?\{\{/crosslinks\}\}", block, out, flags=re.S)
    out = out.replace("{{crosslinks}}", ", ".join(crosslinks))
    return out


def parse_md_note(text: str) -> tuple[str, str]:
    """Pull the leading `# Title` line; return (title, body_without_title)."""
    text = text.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        lines = lines[1:]
        if lines and lines[-1].lstrip().startswith("```"):
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    m = re.match(r"#\s+(.+?)\n(.*)", text, flags=re.S)
    if not m:
        return ("Untitled", text)
    return (m.group(1).strip(), m.group(2).strip())


def parse_txt_note(text: str) -> tuple[str, str]:
    """First non-empty line is the title; rest is body. Strip a stray leading `# ` if the model added one."""
    text = text.strip()
    if text.startswith("```"):
        lines = text.splitlines()
        lines = lines[1:]
        if lines and lines[-1].lstrip().startswith("```"):
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    lines = text.splitlines()
    title = ""
    rest_idx = 0
    for i, ln in enumerate(lines):
        if ln.strip():
            title = ln.strip()
            rest_idx = i + 1
            break
    if title.startswith("# "):
        title = title[2:].strip()
    body = "\n".join(lines[rest_idx:]).lstrip("\n")
    return (title or "Untitled", body)


def write_md_frontmatter(
    title: str,
    topic: str,
    aspect: str,
    length_label: str,
    crosslinks: list[str],
    pathology: str | None,
    paste_kinds: list[str],
) -> str:
    fm: dict = {
        "title": title,
        "hiker": {
            "provenance": "synthetic-corpus",
            "author": "imported",
        },
        "synth": {
            "topic": topic,
            "aspect": aspect,
            "length": length_label,
            "crosslinks": crosslinks,
        },
    }
    if pathology:
        fm["synth"]["pathology"] = pathology
    if paste_kinds:
        fm["synth"]["paste_kinds"] = paste_kinds
    body = yaml.safe_dump(fm, sort_keys=False).strip()
    return f"---\n{body}\n---\n"


def allocate_counts(topics: list[dict], total: int) -> dict[str, int]:
    n = len(topics)
    base = total // n
    rem = total % n
    counts: dict[str, int] = {}
    for i, t in enumerate(topics):
        counts[t["name"]] = base + (1 if i < rem else 0)
    return counts


def llm_complete(model: str, prompt: str, max_tokens: int) -> str:
    resp = litellm.completion(
        model=model,
        messages=[{"role": "user", "content": prompt}],
        max_tokens=max_tokens,
    )
    return resp.choices[0].message.content or ""


def load_paste_library(root: Path) -> dict[str, list[tuple[str, str]]]:
    """Read pastes/<kind>/*.* into {kind: [(name, contents), ...]}."""
    paste_dir = root / "pastes"
    library: dict[str, list[tuple[str, str]]] = {}
    if not paste_dir.is_dir():
        return library
    for kind_dir in sorted(paste_dir.iterdir()):
        if not kind_dir.is_dir():
            continue
        items: list[tuple[str, str]] = []
        for f in sorted(kind_dir.iterdir()):
            if f.is_file():
                items.append((f.name, f.read_text().rstrip("\n")))
        if items:
            library[kind_dir.name] = items
    return library


def splice_paste_md(body: str, kind: str, paste_text: str) -> str:
    lang = PASTE_LANG.get(kind, "")
    fence_open = f"```{lang}" if lang else "```"
    block = f"{fence_open}\n{paste_text}\n```"
    return f"{body.rstrip()}\n\nExample ({kind}):\n\n{block}\n"


def splice_paste_txt(body: str, paste_text: str, style: str) -> str:
    if style == "indented":
        indented = "\n".join("    " + ln if ln else "" for ln in paste_text.splitlines())
        return f"{body.rstrip()}\n\n{indented}\n"
    if style == "raw":
        return f"{body.rstrip()}\n\n{paste_text}\n"
    # inline — drop mid-body if possible, else append
    paragraphs = body.split("\n\n")
    if len(paragraphs) >= 3:
        mid = len(paragraphs) // 2
        paragraphs.insert(mid, paste_text)
        return "\n\n".join(paragraphs)
    return f"{body.rstrip()}\n\n{paste_text}\n"


def write_note_file(
    out_root: Path,
    topic: str,
    title: str,
    extension: str,
    head: str,  # frontmatter for md, "" for txt
    title_line: str,  # "# Title\n\n" for md, "Title\n\n" for txt
    body: str,
    raw_for_hash: str,
) -> Path:
    topic_dir = out_root / topic
    topic_dir.mkdir(parents=True, exist_ok=True)
    slug = slugify(title)
    h = hashlib.blake2s(raw_for_hash.encode("utf-8"), digest_size=3).hexdigest()
    target = topic_dir / f"{slug}-{h}.{extension}"
    counter = 1
    while target.exists():
        target = topic_dir / f"{slug}-{h}-{counter}.{extension}"
        counter += 1
    target.write_text(head + title_line + body.rstrip() + "\n")
    return target


def cmd_gen(args: argparse.Namespace) -> int:
    out_root = Path(args.out).resolve()
    out_root.mkdir(parents=True, exist_ok=True)
    manifest_dir = out_root / ".synth"
    manifest_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = manifest_dir / "manifest.jsonl"

    spec = yaml.safe_load(Path(args.topics).read_text())
    topics = spec.get("topics") or []
    if not topics:
        print("error: topics.yaml has no `topics:` list", file=sys.stderr)
        return 2

    script_dir = Path(__file__).parent
    md_template = (Path(args.prompt) if args.prompt else script_dir / "prompts" / "note.md").read_text()
    txt_template = (script_dir / "prompts" / "note-txt.md").read_text()
    paste_library = load_paste_library(script_dir)

    rng = random.Random(args.seed)
    counts = allocate_counts(topics, args.count)

    written = 0
    skipped = 0
    near_dup_pool: list[tuple[str, str, str]] = []  # (topic, title, raw)
    manifest_f = manifest_path.open("w")

    try:
        for topic in topics:
            name = topic["name"]
            crosslinks_pool = list(topic.get("crosslinks") or [])
            topic_paste_kinds = [
                k for k in (topic.get("paste_kinds") or []) if k in paste_library
            ]

            for i in range(counts[name]):
                aspect = ASPECTS[(i + rng.randrange(len(ASPECTS))) % len(ASPECTS)]
                length_label, length_desc = LENGTHS[i % len(LENGTHS)]

                picks: list[str] = []
                if crosslinks_pool:
                    k_choices = [0, 1, 1, 2] if len(crosslinks_pool) >= 2 else [0, 1]
                    k = min(rng.choice(k_choices), len(crosslinks_pool))
                    if k:
                        picks = rng.sample(crosslinks_pool, k)

                # Format: md or txt
                fmt = "txt" if rng.random() < args.txt_rate else "md"

                # Pathology injection
                pathology: str | None = None
                extra_instruction = ""
                if rng.random() < PATHOLOGY_RATE:
                    kind = rng.choice(["near-dup", "topic-drift", "very-short", "very-long"])
                    if kind == "near-dup" and near_dup_pool:
                        _src_topic, src_title, src_raw = rng.choice(near_dup_pool)
                        pathology = "near-duplicate"
                        extra_instruction = (
                            "\n\nThis note is a NEAR-DUPLICATE rewrite of an existing note. "
                            "Cover the same ground but vary wording, ordering, and emphasis. "
                            "Original follows between <<< and >>>:\n\n<<<\n"
                            f"{src_title}\n\n{src_raw}\n>>>"
                        )
                    elif kind == "topic-drift":
                        pathology = "topic-drift"
                        extra_instruction = (
                            "\n\nDeliberately drift: start on the topic, then spend the second "
                            "half of the note on a tangentially related but distinct topic."
                        )
                    elif kind == "very-short":
                        pathology = "very-short"
                        extra_instruction = (
                            "\n\nOverride the length above: write only 1-2 sentences total."
                        )
                    elif kind == "very-long":
                        pathology = "very-long"
                        extra_instruction = (
                            "\n\nOverride the length above: write ~2500-3500 words with 5-8 "
                            "sections. Stay focused; don't repeat yourself."
                        )

                template = txt_template if fmt == "txt" else md_template
                prompt = render_prompt(template, name, aspect, length_desc, picks) + extra_instruction
                max_toks = 6000 if pathology == "very-long" else args.max_tokens

                try:
                    raw = llm_complete(args.model, prompt, max_toks)
                except Exception as e:
                    print(f"warn: {name}#{i} generation failed: {e}", file=sys.stderr)
                    skipped += 1
                    continue

                if fmt == "md":
                    title, body = parse_md_note(raw)
                else:
                    title, body = parse_txt_note(raw)

                # Paste injection
                paste_kinds_used: list[str] = []
                paste_style: str | None = None
                if topic_paste_kinds and rng.random() < args.paste_rate:
                    kind = rng.choice(topic_paste_kinds)
                    fixture_name, paste_text = rng.choice(paste_library[kind])
                    paste_kinds_used = [kind]
                    if fmt == "md":
                        body = splice_paste_md(body, kind, paste_text)
                        paste_style = "fenced"
                    else:
                        paste_style = rng.choice(TXT_PASTE_STYLES)
                        body = splice_paste_txt(body, paste_text, paste_style)

                # Build the file content
                if fmt == "md":
                    head = write_md_frontmatter(
                        title, name, aspect, length_label, picks, pathology, paste_kinds_used
                    )
                    title_line = f"\n# {title}\n\n"
                else:
                    head = ""
                    title_line = f"{title}\n\n"

                target = write_note_file(
                    out_root, name, title, fmt, head, title_line, body, raw
                )

                rel = str(target.relative_to(out_root))
                manifest_f.write(json.dumps({
                    "path": rel,
                    "format": fmt,
                    "topic": name,
                    "aspect": aspect,
                    "length": length_label,
                    "crosslinks": picks,
                    "pathology": pathology,
                    "paste_kinds": paste_kinds_used,
                    "paste_style": paste_style,
                }) + "\n")
                manifest_f.flush()

                written += 1
                tags = [length_label]
                if pathology:
                    tags.append(pathology)
                if paste_kinds_used:
                    tags.append(f"paste:{paste_kinds_used[0]}/{paste_style}")
                print(f"wrote {rel} ({', '.join(tags)})", file=sys.stderr)

                # Eligible source for future near-dup variants: clean .md notes only
                if pathology is None and length_label != "long" and fmt == "md":
                    near_dup_pool.append((name, title, raw))
                    if len(near_dup_pool) > 32:
                        near_dup_pool.pop(0)
    finally:
        manifest_f.close()

    print(json.dumps({
        "written": written,
        "skipped": skipped,
        "out": str(out_root),
        "manifest": str(manifest_path),
    }))
    return 0 if skipped == 0 else 1


def main() -> int:
    p = argparse.ArgumentParser(prog="eval-synth")
    sub = p.add_subparsers(dest="cmd", required=True)

    g = sub.add_parser("gen", help="generate a synthetic corpus")
    g.add_argument("--topics", required=True, help="path to topics.yaml")
    g.add_argument("--count", type=int, required=True, help="total notes to generate")
    g.add_argument("--out", required=True, help="vault directory to write into")
    g.add_argument(
        "--model",
        default="anthropic/claude-haiku-4-5",
        help="litellm model identifier (default: anthropic/claude-haiku-4-5)",
    )
    g.add_argument("--seed", type=int, default=0, help="RNG seed for reproducibility")
    g.add_argument(
        "--prompt",
        default=None,
        help="path to .md prompt template (default: prompts/note.md)",
    )
    g.add_argument("--max-tokens", type=int, default=2000)
    g.add_argument(
        "--txt-rate", type=float, default=0.0,
        help="fraction of notes to write as plain .txt instead of .md (0.0-1.0; default 0)",
    )
    g.add_argument(
        "--paste-rate", type=float, default=0.0,
        help="fraction of notes to splice a syntax fixture into (0.0-1.0; default 0). "
             "Only fires for topics whose paste_kinds match an available pastes/<kind>/ dir",
    )
    g.set_defaults(func=cmd_gen)

    args = p.parse_args()
    if not 0.0 <= args.txt_rate <= 1.0:
        print("error: --txt-rate must be between 0 and 1", file=sys.stderr)
        return 2
    if not 0.0 <= args.paste_rate <= 1.0:
        print("error: --paste-rate must be between 0 and 1", file=sys.stderr)
        return 2
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
