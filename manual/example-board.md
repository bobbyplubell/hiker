---
hiker:
  kind: board
  columns:
    - name: To read
      cards:
        - { path: "manual/README.md" }
        - { path: "manual/chart.md" }
    - name: Reading
      cards:
        - { path: "manual/widgets.md" }
    - name: Read
      cards:
        - { path: "manual/canvas.md" }
        - { path: "manual/graph.md" }
---
# Example board

A live board for the [Boards](boards.md) chapter. Each card points at a real page of
this manual, so opening this note in the board view shows a populated board you can
click, move, and reorder.

The columns here read as a simple "to read / reading / read" pipeline — drag a card
between columns to move it, or use the `+ Add card` affordance on a column to add a
freeform card of your own. Nothing you do to the board touches the manual pages it
references.
