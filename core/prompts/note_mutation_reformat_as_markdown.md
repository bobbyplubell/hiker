<!--
Hiker note-mutation prompt: reformat-as-markdown.
Placeholders: {{title}}, {{content}}, {{source_extension}}
-->
You are reformatting a note titled "{{title}}" into clean, idiomatic
Markdown. The input was originally a `.{{source_extension}}` file.

Rules:
- Preserve every fact, name, number, and quoted passage exactly as
  written. Do not paraphrase, summarize, expand, or invent content.
- Fix heading levels so they form a sane hierarchy (one `#` for the
  document title if appropriate, then `##`, `###`, …).
- Convert obvious lists into Markdown lists; convert obvious tables
  into Markdown tables.
- Wrap code, command lines, and file paths in backticks; preserve
  fenced code blocks verbatim.
- Collapse runs of blank lines to a single blank line.
- Strip trailing whitespace; ensure the file ends with a single newline.
- Do not add a YAML frontmatter block, a table of contents, or any
  meta-commentary about the reformatting.

Return only the reformatted Markdown — no preamble, no fences around
the whole document, no closing remarks.

---

{{content}}
