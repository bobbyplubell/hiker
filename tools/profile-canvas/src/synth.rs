//! Synthetic-large canvas generation: lay out N file nodes in a grid, each
//! pointing at a real `.md` file found by walking the vault, so the profiler
//! can benchmark scaling past the size of any real canvas.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context as _, Result};
use hiker_canvas::model::{Canvas, Edge, Node, NodeKind};

/// Card width in canvas units.
const CARD_W: i64 = 320;
/// Card height in canvas units.
const CARD_H: i64 = 240;
/// Gap between cards in canvas units.
const GAP: i64 = 60;

/// Build a canvas of `n` file nodes in a near-square grid. Each node points at a
/// vault-relative `.md` path, cycling through the notes discovered under
/// `vault`. Errors if no `.md` files are found.
pub fn grid_canvas(n: usize, vault: &Path) -> Result<Canvas> {
    let notes = find_notes(vault)?;
    anyhow::ensure!(!notes.is_empty(), "no .md files found under {}", vault.display());
    let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
    let nodes = (0..n)
        .map(|i| grid_node(i, cols, &notes[i % notes.len()]))
        .collect();
    Ok(Canvas { nodes, edges: Vec::new(), extra: BTreeMap::new() })
}

/// Build a large sweep canvas: the same `n`-node grid as [`grid_canvas`], plus an
/// edge from each card to its right and down neighbours so the zoom sweep also
/// exercises the edge LOD ladder (curves → simplified strokes → bare lines) in
/// lockstep with the cards. Errors if no `.md` files are found.
pub fn sweep_canvas(n: usize, vault: &Path) -> Result<Canvas> {
    let notes = find_notes(vault)?;
    anyhow::ensure!(!notes.is_empty(), "no .md files found under {}", vault.display());
    let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
    let nodes: Vec<Node> = (0..n)
        .map(|i| grid_node(i, cols, &notes[i % notes.len()]))
        .collect();
    let edges = grid_edges(n, cols);
    Ok(Canvas { nodes, edges, extra: BTreeMap::new() })
}

/// Edges connecting each grid slot to its right and downward neighbour, so a card
/// at `(row, col)` links to `(row, col+1)` and `(row+1, col)` when those exist.
fn grid_edges(n: usize, cols: usize) -> Vec<Edge> {
    let mut edges = Vec::new();
    for i in 0..n {
        let col = i % cols;
        if col + 1 < cols && i + 1 < n {
            edges.push(grid_edge(edges.len(), i, i + 1));
        }
        if i + cols < n {
            edges.push(grid_edge(edges.len(), i, i + cols));
        }
    }
    edges
}

/// One edge with id `e{idx}` from `synth-{from}` to `synth-{to}`.
fn grid_edge(idx: usize, from: usize, to: usize) -> Edge {
    Edge {
        id: format!("e{idx}"),
        from_node: format!("synth-{from}"),
        from_side: None,
        from_end: None,
        to_node: format!("synth-{to}"),
        to_side: None,
        to_end: None,
        color: None,
        label: None,
        extra: BTreeMap::new(),
    }
}

/// One grid node at slot `i` (row-major, `cols` per row) pointing at `file`.
fn grid_node(i: usize, cols: usize, file: &str) -> Node {
    let (row, col) = (i / cols, i % cols);
    Node {
        id: format!("synth-{i}"),
        x: col as i64 * (CARD_W + GAP),
        y: row as i64 * (CARD_H + GAP),
        width: CARD_W,
        height: CARD_H,
        color: None,
        kind: NodeKind::File { file: file.to_owned(), subpath: None },
        extra: BTreeMap::new(),
    }
}

/// Vault-relative paths of every `.md` file under `vault`, sorted for
/// determinism. A shallow hand-rolled walk (no `walkdir` dep needed here).
fn find_notes(vault: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(vault, vault, &mut out)
        .with_context(|| format!("walking vault {}", vault.display()))?;
    out.sort_unstable();
    Ok(out)
}

/// Recursively collect `.md` files under `dir`, recording each as a path
/// relative to `root`.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")) {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    Ok(())
}
