# Hiker manual

Human-facing documentation for using Hiker — written to be read, not to specify
the implementation. (For the precise, feature-by-feature specifications, see
[`../docs/`](../docs/index.md).)

These pages are themselves Hiker notes: open them in the app and the examples
render live.

## Contents

- **[Widgets & diagrams](widgets.md)** — the math, Mermaid, and WaveDrom blocks
  you can drop into any note, with one live example of every diagram type, plus
  rich tables (markdown, math, diagrams, and images inside cells).
- **[Charts](chart.md)** — ` ```chart ` blocks that plot data (bar, line, area,
  scatter, histogram, pie/donut, table) from inline CSV or a referenced `.csv`
  file, with a live example of every mark.
- **[Canvas](canvas.md)** — the infinite spatial board: note/text/link/group
  cards connected by edges, inline editing, groups, auto-arrange, and the
  fisheye / Poincaré projection modes for navigating a big board.
- **[Graph view](graph.md)** — the node-link map of how your notes connect:
  layouts, display controls, and the focus+context projection modes for
  navigating a large vault without getting lost.
- **[Boards](boards.md)** — kanban-style boards of columns and cards: moving
  cards between columns, freeform cards, managing columns, and the board /
  markdown toggle.
- **[Architecture](architecture.md)** — a bird's-eye Mermaid map of how Hiker
  is put together: the front ends, the UI building blocks, the engine crates,
  and where your data actually lives.

More chapters to come.
