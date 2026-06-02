//! Unit coverage for the pure canvas-export builders. Exercises
//! [`super::trail_to_canvas`] and [`super::tree_to_canvas`] against hand-built
//! source structures so layout / mapping / determinism are tested without a
//! UI or a real vault.
//!
//! status: canvas-export-trail
//! status: canvas-export-tree
//! status: canvas-export-builder

use hiker_canvas::model::{EndCap, NodeKind};

use super::{TreeCanvasStyle, trail_to_canvas, tree_to_canvas};
use crate::trails::ops::ResolutionOutcome;
use crate::trails::{ResolvedWaypoint, TrailDetail};
use crate::trees::types::{EditableNode, NodeKind as TreeKind};

// ── helpers ─────────────────────────────────────────────────────────────

fn waypoint(rel: &str, children: Vec<ResolvedWaypoint>) -> ResolvedWaypoint {
    ResolvedWaypoint {
        waypoint_rel: rel.to_owned(),
        annotation_body: String::new(),
        source_path: String::new(),
        in_trail_path: "trail.md".to_owned(),
        resolution: ResolutionOutcome::Resolved {
            rel_path: rel.to_owned(),
        },
        children,
        tree_path: rel.to_owned(),
    }
}

fn trail(waypoints: Vec<ResolvedWaypoint>) -> TrailDetail {
    TrailDetail {
        rel_path: "trail.md".to_owned(),
        trail_id: "t1".to_owned(),
        last_activated_at: None,
        body: String::new(),
        waypoints,
        append_under: None,
    }
}

fn cluster(id: &str, parent: Option<&str>, name: &str, summary: &str) -> EditableNode {
    node(id, parent, TreeKind::Cluster, name, summary, None)
}

fn leaf(id: &str, parent: &str, name: &str, path: &str) -> EditableNode {
    node(id, Some(parent), TreeKind::Leaf, name, "", Some(path.to_owned()))
}

fn node(
    id: &str,
    parent: Option<&str>,
    kind: TreeKind,
    name: &str,
    summary: &str,
    note_path: Option<String>,
) -> EditableNode {
    EditableNode {
        id: id.to_owned(),
        parent: parent.map(str::to_owned),
        kind,
        note_path,
        name: name.to_owned(),
        summary: summary.to_owned(),
        user_edited_name: false,
        user_edited_summary: false,
        policy: None,
        centroid: None,
        confidence: 0.0,
        summary_membership_churn: 0,
    }
}

fn file_path(kind: &NodeKind) -> Option<&str> {
    match kind {
        NodeKind::File { file, .. } => Some(file),
        _ => None,
    }
}

// ── trail builder ───────────────────────────────────────────────────────

#[test]
fn trail_main_line_and_side_trail() {
    // a → b → c (main line), with b also having a side trail d.
    let detail = trail(vec![waypoint(
        "wp/a.md",
        vec![waypoint(
            "wp/b.md",
            vec![
                waypoint("wp/c.md", vec![]),
                waypoint("wp/d.md", vec![]),
            ],
        )],
    )]);
    let canvas = trail_to_canvas(&detail);

    // One File node per waypoint.
    assert_eq!(canvas.nodes.len(), 4, "one node per waypoint");
    for n in &canvas.nodes {
        assert!(matches!(n.kind, NodeKind::File { .. }), "every node is a File node");
    }
    // File paths point at the waypoint-notes.
    let paths: Vec<&str> = canvas.nodes.iter().filter_map(|n| file_path(&n.kind)).collect();
    assert_eq!(paths, vec!["wp/a.md", "wp/b.md", "wp/c.md", "wp/d.md"]);

    // Three parent→child edges, all reading-direction arrows.
    assert_eq!(canvas.edges.len(), 3, "one edge per parent->child link");
    for e in &canvas.edges {
        assert_eq!(e.from_end, Some(EndCap::None), "no source cap");
        assert_eq!(e.to_end, Some(EndCap::Arrow), "arrow at destination");
    }

    // Ids unique.
    let mut ids: Vec<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    ids.sort_unstable();
    let count = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), count, "node ids unique");

    // Edges reference real nodes and encode the parent→child shape: a→b, b→c, b→d.
    let id_of = |p: &str| -> String {
        canvas
            .nodes
            .iter()
            .find(|n| file_path(&n.kind) == Some(p))
            .unwrap()
            .id
            .clone()
    };
    let pairs: Vec<(String, String)> = canvas
        .edges
        .iter()
        .map(|e| (e.from_node.clone(), e.to_node.clone()))
        .collect();
    assert!(pairs.contains(&(id_of("wp/a.md"), id_of("wp/b.md"))));
    assert!(pairs.contains(&(id_of("wp/b.md"), id_of("wp/c.md"))));
    assert!(pairs.contains(&(id_of("wp/b.md"), id_of("wp/d.md"))));
}

#[test]
fn trail_main_line_has_distinct_coordinates() {
    let detail = trail(vec![waypoint(
        "a.md",
        vec![waypoint("b.md", vec![waypoint("c.md", vec![])])],
    )]);
    let canvas = trail_to_canvas(&detail);
    // The straight chain a→b→c sits on one row with strictly increasing x.
    let xs: Vec<i64> = canvas.nodes.iter().map(|n| n.x).collect();
    let ys: Vec<i64> = canvas.nodes.iter().map(|n| n.y).collect();
    assert!(xs[0] < xs[1] && xs[1] < xs[2], "main line marches along x: {xs:?}");
    assert!(ys.iter().all(|&y| y == ys[0]), "straight chain stays on one row: {ys:?}");
}

#[test]
fn trail_determinism() {
    let build = || {
        trail_to_canvas(&trail(vec![waypoint(
            "a.md",
            vec![
                waypoint("b.md", vec![]),
                waypoint("c.md", vec![waypoint("d.md", vec![])]),
            ],
        )]))
    };
    assert_eq!(
        build().to_canonical_json(),
        build().to_canonical_json(),
        "same trail builds byte-identical canvas"
    );
}

// ── tree builder ────────────────────────────────────────────────────────

#[test]
fn tree_clusters_leaves_and_nesting() {
    // root cluster A { summary } containing leaf l1 and child cluster B { leaf l2 }.
    let nodes = vec![
        cluster("A", None, "Alpha", "alpha summary"),
        leaf("l1", "A", "one", "notes/one.md"),
        cluster("B", Some("A"), "Beta", ""),
        leaf("l2", "B", "two", "notes/two.md"),
    ];
    let canvas = tree_to_canvas("My Tree", &nodes, TreeCanvasStyle::Grouped);

    // No edges — hierarchy is purely spatial.
    assert!(canvas.edges.is_empty(), "tree export draws no edges");

    let groups: Vec<&str> = canvas
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Group { label, .. } => label.as_deref(),
            _ => None,
        })
        .collect();
    // Synthetic root frame named after the tree + cluster A + cluster B.
    assert!(groups.contains(&"My Tree"), "root frame labeled with tree name");
    assert!(groups.contains(&"Alpha"));
    assert!(groups.contains(&"Beta"));

    // File node per leaf, pointing at the note path.
    let files: Vec<&str> = canvas.nodes.iter().filter_map(|n| file_path(&n.kind)).collect();
    assert!(files.contains(&"notes/one.md"));
    assert!(files.contains(&"notes/two.md"));

    // Summary text present for Alpha (non-empty), absent for Beta (empty).
    let texts: Vec<&str> = canvas
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["alpha summary"], "only the non-empty summary becomes a text node");

    // Ids unique.
    let mut ids: Vec<&str> = canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    ids.sort_unstable();
    let count = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), count, "node ids unique");
}

#[test]
fn tree_child_group_contained_in_parent() {
    let nodes = vec![
        cluster("A", None, "Alpha", ""),
        cluster("B", Some("A"), "Beta", ""),
        leaf("l2", "B", "two", "notes/two.md"),
    ];
    let canvas = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::Grouped);

    let rect = |label: &str| -> (i64, i64, i64, i64) {
        let n = canvas
            .nodes
            .iter()
            .find(|n| matches!(&n.kind, NodeKind::Group { label: Some(l), .. } if l == label))
            .unwrap();
        (n.x, n.y, n.x + n.width, n.y + n.height)
    };
    let (ax0, ay0, ax1, ay1) = rect("Alpha");
    let (bx0, by0, bx1, by1) = rect("Beta");
    assert!(ax0 <= bx0 && ay0 <= by0 && bx1 <= ax1 && by1 <= ay1, "Beta rect inside Alpha rect");

    // And the leaf inside Beta sits inside Beta's rect.
    let leaf = canvas
        .nodes
        .iter()
        .find(|n| file_path(&n.kind) == Some("notes/two.md"))
        .unwrap();
    assert!(
        bx0 <= leaf.x && by0 <= leaf.y && leaf.x + leaf.width <= bx1 && leaf.y + leaf.height <= by1,
        "leaf inside Beta rect"
    );
}

#[test]
fn tree_outlier_bucket_is_a_group() {
    let nodes = vec![
        node("O", None, TreeKind::OutlierBucket, "Outliers", "", None),
        leaf("l1", "O", "x", "notes/x.md"),
    ];
    let canvas = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::Grouped);
    let has_outlier_group = canvas.nodes.iter().any(
        |n| matches!(&n.kind, NodeKind::Group { label: Some(l), .. } if l == "Outliers"),
    );
    assert!(has_outlier_group, "outlier bucket exports as a group");
}

#[test]
fn tree_determinism() {
    let nodes = vec![
        cluster("A", None, "Alpha", "s"),
        leaf("l1", "A", "one", "notes/one.md"),
        cluster("B", Some("A"), "Beta", ""),
        leaf("l2", "B", "two", "notes/two.md"),
    ];
    let a = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::Grouped).to_canonical_json();
    let b = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::Grouped).to_canonical_json();
    assert_eq!(a, b, "same tree builds byte-identical canvas");
}

// ── force-directed tree builder ──────────────────────────────────────────

/// A root cluster with two child clusters and several leaves, one of them
/// path-less so the skip/fallback rule is exercised.
fn force_fixture() -> Vec<EditableNode> {
    vec![
        cluster("R", None, "Root", ""),
        cluster("A", Some("R"), "Alpha", ""),
        cluster("B", Some("R"), "Beta", ""),
        leaf("l1", "A", "one", "notes/one.md"),
        leaf("l2", "A", "two", "notes/two.md"),
        leaf("l3", "B", "three", "notes/three.md"),
        node("l4", Some("B"), TreeKind::Leaf, "pathless", "", None),
    ]
}

#[test]
fn force_node_and_edge_counts() {
    let nodes = force_fixture();
    let canvas = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected);

    // One canvas node per tree node: path-less leaves are kept as Text nodes
    // rather than skipped, so the count matches the input exactly.
    assert_eq!(canvas.nodes.len(), nodes.len(), "one node per tree node");

    // One parent->child edge per non-root node.
    let non_root = nodes.iter().filter(|n| n.parent.is_some()).count();
    assert_eq!(canvas.edges.len(), non_root, "one edge per non-root node");

    // Path-bearing leaves are File nodes; clusters + path-less leaf are Text.
    let files: Vec<&str> = canvas.nodes.iter().filter_map(|n| file_path(&n.kind)).collect();
    assert_eq!(files.len(), 3, "three path-bearing leaves became File nodes");
    assert!(files.contains(&"notes/one.md"));
    assert!(files.contains(&"notes/three.md"));
}

#[test]
fn force_edges_connect_children_to_parents() {
    let nodes = force_fixture();
    let canvas = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected);

    let ids: std::collections::HashSet<&str> =
        canvas.nodes.iter().map(|n| n.id.as_str()).collect();
    // Edges reference real node ids and carry no caps (undirected-looking).
    for e in &canvas.edges {
        assert!(ids.contains(e.from_node.as_str()), "edge from a real node");
        assert!(ids.contains(e.to_node.as_str()), "edge to a real node");
        assert_eq!(e.from_end, None, "no source cap");
        assert_eq!(e.to_end, None, "no destination cap");
    }
    // Ids are `n<index>` in stored order, so each child connects to its parent.
    // Index map: R=0, A=1, B=2, l1=3, l2=4, l3=5, l4=6.
    let pairs: Vec<(String, String)> = canvas
        .edges
        .iter()
        .map(|e| (e.from_node.clone(), e.to_node.clone()))
        .collect();
    for (parent, child) in [
        ("n0", "n1"),
        ("n0", "n2"),
        ("n1", "n3"),
        ("n1", "n4"),
        ("n2", "n5"),
        ("n2", "n6"),
    ] {
        assert!(
            pairs.contains(&(parent.to_owned(), child.to_owned())),
            "edge {parent}->{child} present"
        );
    }
}

#[test]
fn force_layout_spreads_nodes() {
    let nodes = force_fixture();
    let canvas = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected);
    let positions: std::collections::HashSet<(i64, i64)> =
        canvas.nodes.iter().map(|n| (n.x, n.y)).collect();
    assert!(positions.len() > 1, "layout actually spread the nodes apart");
}

#[test]
fn force_determinism() {
    let nodes = force_fixture();
    let a = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected).to_canonical_json();
    let b = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected).to_canonical_json();
    assert_eq!(a, b, "same tree builds byte-identical force canvas");
}

#[test]
fn force_canvas_round_trips() {
    use hiker_canvas::model::Canvas;
    let nodes = force_fixture();
    let json = tree_to_canvas("Tree", &nodes, TreeCanvasStyle::ForceDirected).to_canonical_json();
    let parsed = Canvas::from_json(&json).expect("force canvas re-parses");
    assert_eq!(parsed.nodes.len(), nodes.len());
}
