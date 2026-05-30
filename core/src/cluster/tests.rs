use std::collections::HashMap;

use crate::cluster::algo::{cosine_similarity, l2_normalize, partition, partition_leiden};
use crate::cluster::build::stream::build_tree_structural_streaming;
use crate::cluster::build::{persist, tree};
use crate::cluster::tree::place_beam_descent;
use crate::cluster::{
    detect_reading_order_chain, plan_sample_merge, Algorithm, Assignment, BuildError, BuildEvent,
    BuildMethod, BuildScope, ChainCandidate, Error, FolderDeriveParams, InMemoryTree, LeidenParams,
    Node, NoteInput, Params, Phase, SampleMergePlan, SummarizeInput, SummarizeMode, Summarizer,
    SummaryOutput, MAX_CHAIN_LEN, MIN_CHAIN_LEN, OUTLIER_LABEL, SAMPLE_MERGE_BATCH_SIZE,
    SAMPLE_MERGE_BATCH_THRESHOLD, SAMPLE_MERGE_MEMBER_CAP,
};

/// Test-only stand-in for the production `LlmSummarizer`. Returns a
/// deterministic name derived from member titles so build tests can
/// assert on tree shape without spinning up an LLM client.
struct MockSummarizer;

impl Summarizer for MockSummarizer {
    fn summarize(&self, input: SummarizeInput<'_>) -> Result<SummaryOutput, BuildError> {
        let name = if let Some(first) = input.members.first() {
            format!("cluster: {}", first.title)
        } else {
            "empty cluster".to_string()
        };
        Ok(SummaryOutput {
            name,
            summary: format!("{} members", input.members.len()),
            confidence: 0.7,
        })
    }
}

fn vec_at(n: usize, dim: usize, base: f32) -> Vec<f32> {
    // Deterministic synthetic embeddings: each "cluster" lives near a
    // distinct corner of the unit hypercube so HDBSCAN separates them.
    let mut v = vec![0.0; dim];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = base + (n as f32) * 0.01 + (i as f32) * 0.001;
    }
    v
}

#[test]
fn partition_separates_two_obvious_clusters() {
    // 10 points around `base=0.0` and 10 around `base=10.0` — well
    // beyond HDBSCAN's density threshold.
    let mut pts: Vec<Vec<f32>> = (0..10).map(|i| vec_at(i, 8, 0.0)).collect();
    pts.extend((0..10).map(|i| vec_at(i, 8, 10.0)));

    let labels = partition(&pts, 3, None).unwrap();
    assert_eq!(labels.len(), 20);
    // First 10 should share a label; last 10 should share one;
    // the two labels differ. Outliers are tolerated (HDBSCAN can
    // flag edge points) — we only assert majority cohesion.
    let head_label = labels[0..10]
        .iter()
        .map(|a| a.cluster_label)
        .find(|&l| l != OUTLIER_LABEL)
        .expect("first half not all outliers");
    let tail_label = labels[10..20]
        .iter()
        .map(|a| a.cluster_label)
        .find(|&l| l != OUTLIER_LABEL)
        .expect("second half not all outliers");
    assert_ne!(head_label, tail_label, "two halves got the same cluster");
}

#[test]
fn partition_rejects_empty() {
    let r = partition(&Vec::<Vec<f32>>::new(), 5, None);
    assert!(matches!(r, Err(Error::Empty)));
}

#[test]
fn partition_rejects_dim_mismatch() {
    let pts = vec![vec![1.0, 2.0], vec![3.0]];
    let r = partition(&pts, 2, None);
    assert!(matches!(r, Err(Error::DimMismatch { row: 1, .. })));
}

#[test]
fn l2_normalize_unit_length() {
    let n = l2_normalize(&[3.0, 4.0]);
    let norm: f32 = n.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-5);
}

#[test]
fn cosine_similarity_basics() {
    assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-5);
    assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-5);
    // Zero norm → 0.0, not NaN.
    assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
}

fn mk_tree() -> InMemoryTree {
    // Root → two clusters; each cluster has two leaves. Centroids
    // are normalized in 2D for easy reasoning.
    let mut nodes = HashMap::new();
    nodes.insert(
        "root".to_string(),
        Node {
            id: "root".into(),
            centroid: l2_normalize(&[1.0, 1.0]),
            children: vec!["A".into(), "B".into()],
        },
    );
    nodes.insert(
        "A".into(),
        Node {
            id: "A".into(),
            centroid: l2_normalize(&[1.0, 0.0]),
            children: vec!["A1".into(), "A2".into()],
        },
    );
    nodes.insert(
        "B".into(),
        Node {
            id: "B".into(),
            centroid: l2_normalize(&[0.0, 1.0]),
            children: vec!["B1".into(), "B2".into()],
        },
    );
    nodes.insert(
        "A1".into(),
        Node {
            id: "A1".into(),
            centroid: l2_normalize(&[1.0, 0.1]),
            children: vec![],
        },
    );
    nodes.insert(
        "A2".into(),
        Node {
            id: "A2".into(),
            centroid: l2_normalize(&[1.0, -0.1]),
            children: vec![],
        },
    );
    nodes.insert(
        "B1".into(),
        Node {
            id: "B1".into(),
            centroid: l2_normalize(&[0.1, 1.0]),
            children: vec![],
        },
    );
    nodes.insert(
        "B2".into(),
        Node {
            id: "B2".into(),
            centroid: l2_normalize(&[-0.1, 1.0]),
            children: vec![],
        },
    );
    InMemoryTree {
        root: "root".into(),
        nodes,
    }
}

#[test]
fn beam_descent_picks_nearest_leaf() {
    let tree = mk_tree();
    let query = [1.0, 0.05]; // closest to A1
    let m = place_beam_descent(&query, &tree, 2).expect("non-empty tree");
    assert_eq!(m.leaf_node_id, "A1");
    assert!(m.confidence > 0.9, "confidence={}", m.confidence);
    assert!(m.margin >= 0.0);
}

#[test]
fn beam_descent_finds_other_subtree() {
    let tree = mk_tree();
    let query = [-0.05, 1.0]; // B2 territory
    let m = place_beam_descent(&query, &tree, 2).expect("non-empty tree");
    assert_eq!(m.leaf_node_id, "B2");
}

#[test]
fn sample_merge_plan_single_under_threshold() {
    let members: Vec<String> = (0..15).map(|i| i.to_string()).collect();
    let plan = plan_sample_merge(
        &members,
        SAMPLE_MERGE_BATCH_THRESHOLD,
        SAMPLE_MERGE_BATCH_SIZE,
        SAMPLE_MERGE_MEMBER_CAP,
    );
    assert!(matches!(plan, SampleMergePlan::Single { .. }));
}

#[test]
fn sample_merge_plan_fans_out_above_threshold() {
    let members: Vec<String> = (0..75).map(|i| i.to_string()).collect();
    let plan = plan_sample_merge(&members, 30, 30, 300);
    let SampleMergePlan::SampleAndMerge { batches } = plan else {
        panic!("expected SampleAndMerge, got {plan:?}");
    };
    // 75 / 30 = 3 batches (last partial).
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].len(), 30);
    assert_eq!(batches[2].len(), 15);
    let flat_count: usize = batches.iter().map(std::vec::Vec::len).sum();
    assert_eq!(flat_count, 75);
}

#[test]
fn sample_merge_plan_marks_too_large_above_cap() {
    let members: Vec<String> = (0..400).map(|i| i.to_string()).collect();
    let plan = plan_sample_merge(&members, 30, 30, 300);
    assert!(matches!(
        plan,
        SampleMergePlan::TooLarge { member_count: 400 }
    ));
}

fn mk_note(id: &str, folder: &str, base: f32) -> NoteInput {
    // Embedding lives near a corner of the unit hypercube so HDBSCAN
    // can separate them; title/summary feed the summarizer mock.
    NoteInput {
        id: id.into(),
        title: format!("Note {id}"),
        summary: format!("notes about {folder}"),
        folder: folder.into(),
        embedding: vec![base, base + 0.01, base - 0.01, base + 0.005],
    }
}

#[test]
fn build_tree_cluster_method_produces_levels_and_leaves() {
    // Two well-separated clusters of 6 notes each, plus 2 stragglers.
    let mut notes: Vec<NoteInput> = Vec::new();
    for i in 0..6 {
        notes.push(mk_note(&format!("a{i}"), "research", 0.0 + (i as f32) * 0.001));
    }
    for i in 0..6 {
        notes.push(mk_note(&format!("b{i}"), "cooking", 10.0 + (i as f32) * 0.001));
    }
    let params = Params {
        min_cluster_size: 3,
        // Top-down divisive Split would otherwise re-split each
        // top-level community further; this test asserts on the
        // single-level shape, so disable recursion explicitly.
        disable_recursion: true,
        ..Default::default()
    };
    let summarizer = MockSummarizer;
    let result = tree(
        BuildScope::Vault { source_types: Vec::new() },
        BuildMethod::Cluster {
            params: params.clone(),
        },
        &notes,
        &summarizer,
    )
    .unwrap();
    // Top-down build: when there's >1 top-level cluster we add a
    // synthetic vault root at level 1, so the tree has 2 levels:
    // level 0 (leaf clusters) and level 1 (the synthetic root).
    assert!(!result.tree.levels.is_empty());
    assert!(result.tree.levels[0].len() >= 2);
}

#[test]
fn build_tree_from_folders_one_cluster_per_folder() {
    let notes = vec![
        mk_note("a", "research", 0.0),
        mk_note("b", "research", 0.01),
        mk_note("c", "cooking", 10.0),
        mk_note("d", "cooking", 10.01),
    ];
    let result = tree(
        BuildScope::Vault { source_types: Vec::new() },
        BuildMethod::FromFolders {
            params: FolderDeriveParams {
                summarize: SummarizeMode::None,
                ..Default::default()
            },
        },
        &notes,
        &MockSummarizer,
    )
    .unwrap();
    // Exactly two folders → two leaf clusters, no outliers.
    assert_eq!(result.tree.levels.len(), 1);
    assert_eq!(result.tree.levels[0].len(), 2);
    assert!(result.tree.outliers.is_empty());
    for n in &result.tree.levels[0] {
        assert!(n.confidence >= 1.0, "FromFolders nodes carry confidence 1.0");
    }
}

#[test]
fn build_and_persist_writes_rows() {
    let dir = tempfile::TempDir::new().unwrap();
    let trees = crate::trees::types::Db::new(
        std::sync::Arc::new(crate::oplog::OpLog::open(dir.path()).unwrap()),
        std::sync::Arc::new(crate::vault::Vault::open(dir.path()).unwrap()),
    )
    .unwrap();
    let mut notes: Vec<NoteInput> = Vec::new();
    for i in 0..6 {
        notes.push(mk_note(&format!("a{i}"), "research", 0.0 + (i as f32) * 0.001));
    }
    for i in 0..6 {
        notes.push(mk_note(&format!("b{i}"), "cooking", 10.0 + (i as f32) * 0.001));
    }
    let params = Params {
        min_cluster_size: 3,
        disable_recursion: true,
        ..Default::default()
    };
    let summarizer = MockSummarizer;
    let tree_id = persist(
        &trees,
        "test build",
        "one-shot",
        &BuildScope::Vault { source_types: Vec::new() },
        &BuildMethod::Cluster { params },
        &notes,
        &summarizer,
    )
    .unwrap();
    let row = trees.get_tree(&tree_id).unwrap().unwrap();
    assert_eq!(row.state, "draft");
    let nodes = trees.list_nodes(&tree_id).unwrap();
    assert!(nodes.iter().any(|n| matches!(n.kind, crate::trees::types::NodeKind::Cluster)));
    assert!(nodes.iter().any(|n| matches!(n.kind, crate::trees::types::NodeKind::Leaf)));
}

#[test]
fn partition_leiden_separates_two_obvious_clusters() {
    // Two cosine-orthogonal groups of 10 points each. Leiden runs
    // over L2-normalized embeddings, so cluster separation has to
    // be expressed in direction rather than magnitude — the
    // HDBSCAN fixture's `vec_at` puts every point in the same
    // direction after normalization, which doesn't exercise Leiden
    // meaningfully.
    let mut pts: Vec<Vec<f32>> = Vec::new();
    for i in 0..10 {
        // Cluster A — direction (1, 0, ...) with small noise.
        let mut v = vec![0.0_f32; 8];
        v[0] = 1.0;
        v[1] = (i as f32) * 0.005;
        pts.push(v);
    }
    for i in 0..10 {
        // Cluster B — direction (0, 1, ...) with small noise.
        let mut v = vec![0.0_f32; 8];
        v[1] = 1.0;
        v[0] = (i as f32) * 0.005;
        pts.push(v);
    }

    let leiden = LeidenParams {
        k_nearest: 5,
        edge_weight_floor: 0.0,
        iterations: 100,
        min_cluster_size: 2,
        resolution: 1.0,
        top_level_resolution: 0.3,
    };
    let labels = partition_leiden(&pts, &leiden).unwrap();
    assert_eq!(labels.len(), 20);
    // Majority cohesion: the most common non-outlier label in the
    // first half should differ from the most common in the second
    // half. The grouping itself comes out of modularity
    // optimization, not density estimates, so every point should
    // be placed.
    fn majority_label(slice: &[Assignment]) -> i32 {
        use std::collections::HashMap;
        let mut counts: HashMap<i32, usize> = HashMap::new();
        for a in slice {
            if a.cluster_label == OUTLIER_LABEL {
                continue;
            }
            *counts.entry(a.cluster_label).or_default() += 1;
        }
        counts
            .into_iter()
            .max_by_key(|(_, c)| *c)
            .map(|(l, _)| l)
            .expect("at least one non-outlier in slice")
    }
    let head_label = majority_label(&labels[0..10]);
    let tail_label = majority_label(&labels[10..20]);
    assert_ne!(
        head_label, tail_label,
        "leiden put both halves into the same community"
    );
}

#[test]
fn partition_leiden_rejects_empty() {
    let r = partition_leiden(&Vec::<Vec<f32>>::new(), &LeidenParams::default());
    assert!(matches!(r, Err(Error::Empty)));
}

// ── Async streaming structural pass ──────────────────────────────────
//
// status: cluster-build-async-pass
// status: cluster-build-progress-stream

fn structural_streaming_fixture_notes() -> Vec<NoteInput> {
    // Two well-separated direction-cluster groups so Leiden separates
    // them. 6 each — comfortably above the default `leaf_min_size = 5`,
    // so the build produces a non-trivial level-0 cluster set.
    let mut notes: Vec<NoteInput> = Vec::new();
    for i in 0..6 {
        notes.push(NoteInput {
            id: format!("a{i}"),
            title: format!("a{i}"),
            summary: String::new(),
            folder: "research".into(),
            embedding: {
                let mut v = vec![0.0_f32; 8];
                v[0] = 1.0;
                v[1] = (i as f32) * 0.005;
                v
            },
        });
    }
    for i in 0..6 {
        notes.push(NoteInput {
            id: format!("b{i}"),
            title: format!("b{i}"),
            summary: String::new(),
            folder: "cooking".into(),
            embedding: {
                let mut v = vec![0.0_f32; 8];
                v[1] = 1.0;
                v[0] = (i as f32) * 0.005;
                v
            },
        });
    }
    notes
}

#[tokio::test]
async fn streaming_build_emits_phase_counters_clusters_and_done() {
    let notes = structural_streaming_fixture_notes();
    let params = Params {
        algorithm: Algorithm::Leiden,
        min_cluster_size: 2,
        disable_recursion: true,
        ..Default::default()
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (handle, mut rx) = build_tree_structural_streaming(
        BuildMethod::Cluster { params },
        notes,
        cancel,
    );
    let mut phases: Vec<Phase> = Vec::new();
    let mut clusters_found: u32 = 0;
    let mut got_done = false;
    let mut got_cluster_discovered = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            BuildEvent::Phase { phase } => phases.push(phase),
            BuildEvent::Counters { clusters_found: c, .. } => {
                clusters_found = clusters_found.max(c);
            }
            BuildEvent::ClusterDiscovered { .. } => {
                got_cluster_discovered = true;
            }
            BuildEvent::Done { tree } => {
                got_done = true;
                assert!(!tree.levels.is_empty(), "Done carries a non-empty tree");
                break;
            }
            BuildEvent::Cancelled => panic!("unexpected Cancelled"),
            BuildEvent::Failed { error } => panic!("unexpected Failed: {error}"),
        }
    }
    handle.await.unwrap();
    assert!(got_done, "stream terminated without Done");
    assert!(got_cluster_discovered, "no ClusterDiscovered emitted");
    assert!(clusters_found >= 2, "expected >= 2 clusters, got {clusters_found}");
    // Both `LoadingEmbeddings` and at least one `PartitioningLevel` and
    // `Finalizing` should appear.
    assert!(phases.iter().any(|p| matches!(p, Phase::LoadingEmbeddings)));
    assert!(phases.iter().any(|p| matches!(p, Phase::PartitioningLevel(_))));
    assert!(phases.iter().any(|p| matches!(p, Phase::Finalizing)));
}

#[tokio::test]
async fn streaming_build_respects_cancel_before_start() {
    // Pre-flipped cancel atomic: the build should emit Cancelled on
    // the first level-boundary check and never produce Done.
    let notes = structural_streaming_fixture_notes();
    let params = Params {
        algorithm: Algorithm::Leiden,
        min_cluster_size: 2,
        disable_recursion: true,
        ..Default::default()
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (handle, mut rx) = build_tree_structural_streaming(
        BuildMethod::Cluster { params },
        notes,
        cancel,
    );
    let mut got_cancelled = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            BuildEvent::Cancelled => {
                got_cancelled = true;
                break;
            }
            BuildEvent::Done { .. } => panic!("Done despite pre-set cancel"),
            BuildEvent::Failed { error } => panic!("Failed despite cancel: {error}"),
            _ => {}
        }
    }
    handle.await.unwrap();
    assert!(got_cancelled, "stream never emitted Cancelled");
}

#[tokio::test]
async fn streaming_build_cluster_discovered_parent_link_is_consistent() {
    // With recursion enabled the build emits ClusterDiscovered for
    // every leaf and every branch. A child's `parent` should point at
    // a Id we eventually see in a subsequent ClusterDiscovered
    // (or at the synthesized virtual root represented as None for
    // top-level branches).
    let notes = structural_streaming_fixture_notes();
    let params = Params {
        algorithm: Algorithm::Leiden,
        min_cluster_size: 2,
        // Recurse so we get a branch + leaf mix.
        disable_recursion: false,
        leaf_min_size: 3,
        leaf_cohesion_threshold: 0.0,
        ..Default::default()
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (handle, mut rx) = build_tree_structural_streaming(
        BuildMethod::Cluster { params },
        notes,
        cancel,
    );
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut parents_seen: Vec<Option<String>> = Vec::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            BuildEvent::ClusterDiscovered { node, parent } => {
                // Child-first ordering: when a non-None parent is
                // emitted on this event, we haven't necessarily seen
                // it yet, but we WILL see it later. Record both.
                seen_ids.insert(node.id.clone());
                parents_seen.push(parent.clone());
            }
            BuildEvent::Done { .. } => break,
            BuildEvent::Cancelled => panic!("unexpected Cancelled"),
            BuildEvent::Failed { error } => panic!("unexpected Failed: {error}"),
            _ => {}
        }
    }
    handle.await.unwrap();
    // Every non-None parent id must resolve to a cluster we saw on
    // the stream — that's the contract consumers reconstruct on.
    for parent in parents_seen.into_iter().flatten() {
        assert!(
            seen_ids.contains(&parent),
            "ClusterDiscovered.parent {parent:?} never appeared as a node.id on the stream"
        );
    }
}

#[test]
fn beam_descent_k1_is_greedy() {
    // With beam_width=1 the descent locks into the top child at
    // each level. For our tree the query is unambiguous so it lands
    // at the same leaf as K=2; the test really just guards against
    // panics when K=1 forces a single-element beam.
    let tree = mk_tree();
    let m = place_beam_descent(&[1.0, 0.0], &tree, 1).unwrap();
    assert!(m.leaf_node_id == "A1" || m.leaf_node_id == "A2");
}

// ── Reading-order-chain detection (trail-draft-from-clustering) ────────

fn cc(note_id: &str, order_key: i64) -> ChainCandidate {
    ChainCandidate { note_id: note_id.to_string(), order_key }
}

#[test]
fn chain_detected_and_sorted_by_order_key() {
    // Out-of-input-order candidates with distinct, dense keys form a chain
    // ordered ascending by key.
    let cands = vec![cc("c", 30), cc("a", 10), cc("b", 20)];
    let chain = detect_reading_order_chain(&cands).expect("dense distinct keys = chain");
    assert_eq!(chain.ordered_note_ids, vec!["a", "b", "c"]);
}

#[test]
fn chain_declines_below_min_len() {
    let cands = vec![cc("a", 10), cc("b", 20)];
    assert!(detect_reading_order_chain(&cands).is_none(), "2 < MIN_CHAIN_LEN");
    assert_eq!(MIN_CHAIN_LEN, 3);
}

#[test]
fn chain_declines_above_max_len() {
    let cands: Vec<ChainCandidate> =
        (0..(MAX_CHAIN_LEN as i64 + 1)).map(|k| cc(&format!("n{k}"), k)).collect();
    assert!(detect_reading_order_chain(&cands).is_none(), "oversized cluster declines");
}

#[test]
fn chain_declines_on_duplicate_key() {
    // Two notes claim the same slot → ambiguous → decline (conservative).
    let cands = vec![cc("a", 10), cc("b", 10), cc("c", 30)];
    assert!(detect_reading_order_chain(&cands).is_none(), "duplicate key = ambiguous");
}
