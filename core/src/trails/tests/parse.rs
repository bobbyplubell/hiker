//! Pure parse / write / walk-tree tests for trail-doc and waypoint-note
//! frontmatter under path-as-identity (`trail-path-references`). The
//! ULID-stamping tests retired with `note-id-stamping`; the path-only
//! shape exercised here is the canonical form going forward.

use super::*;

#[test]
fn waypoints_dir_for_uses_forward_slashes() {
    assert_eq!(
        waypoints_dir_for("01HRX"),
        ".hiker/trails/01HRX/waypoints"
    );
}

#[test]
fn waypoint_filename_uses_rand6_suffix() {
    let actual = waypoint_filename("raptor-paper");
    // status: trail-storage-layout — `<basename>--<rand6>.md`.
    assert!(actual.starts_with("raptor-paper--"));
    assert!(actual.ends_with(".md"));
    // The rand6 token sits between `--` and `.md` — 6 chars.
    let suffix = actual.trim_start_matches("raptor-paper--").trim_end_matches(".md");
    assert_eq!(suffix.len(), 6);
}

#[test]
fn parse_trail_doc_round_trip() {
    let src = "---\nhiker:\n  kind: trail\n  last_activated_at: 2026-05-10T12:00:00Z\n  waypoints:\n    - path: .hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md\n    - path: .hiker/trails/01HTRAIL/waypoints/b--BBBBBB.md\n---\nbody prose\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.last_activated_at.as_deref(), Some("2026-05-10T12:00:00Z"));
    assert_eq!(parsed.waypoints.len(), 2);
    assert_eq!(
        parsed.waypoints[0],
        we(".hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md")
    );

    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(written.ends_with("body prose\n"));
}

// status: trail-side-trail-shape
#[test]
fn parse_trail_doc_round_trips_nested_tree() {
    let src = "---\nhiker:\n  kind: trail\n  waypoints:\n    - path: .hiker/trails/01HTRAIL/waypoints/r1--AAAAAA.md\n      waypoints:\n        - path: .hiker/trails/01HTRAIL/waypoints/c1--BBBBBB.md\n          waypoints:\n            - path: .hiker/trails/01HTRAIL/waypoints/g1--CCCCCC.md\n    - path: .hiker/trails/01HTRAIL/waypoints/r2--DDDDDD.md\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.waypoints.len(), 2);
    assert_eq!(parsed.waypoints[0].waypoints.len(), 1);
    assert_eq!(parsed.waypoints[0].waypoints[0].waypoints.len(), 1);
    assert!(parsed.waypoints[1].waypoints.is_empty());

    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
}

// status: trail-side-trail-shape
#[test]
fn parse_trail_doc_round_trips_empty_tree() {
    let src = "---\nhiker:\n  kind: trail\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert!(parsed.waypoints.is_empty());
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_trail_doc(&written).unwrap();
    assert_eq!(parsed, reparsed);
}

// status: trail-path-references
#[test]
fn parse_trail_doc_drops_legacy_id_halves_on_parse() {
    // Legacy yaml with `id:` siblings on entries (pre-path-as-identity);
    // the parser drops the id half but keeps the path.
    let src = "---\nhiker:\n  kind: trail\n  id: 01HLEGACY\n  waypoints:\n    - id: A\n      path: a.md\n    - id: B\n      path: b.md\n---\n";
    let parsed = parse_trail_doc(src).unwrap();
    assert_eq!(parsed.waypoints.len(), 2);
    assert_eq!(parsed.waypoints[0].path, "a.md");
    assert_eq!(parsed.waypoints[1].path, "b.md");
}

// status: trail-side-trail-shape / trail-path-references
#[test]
fn walk_waypoints_yields_depth_first_with_tree_paths() {
    let tree = vec![
        WaypointEntry {
            path: "r1.md".into(),
            waypoints: vec![WaypointEntry {
                path: "c1.md".into(),
                waypoints: vec![we("g1.md")],
            }],
        },
        we("r2.md"),
    ];
    let mut visits: Vec<(Option<String>, String, String)> = Vec::new();
    walk_waypoints_depth_first(&tree, &mut |parent, e, path| {
        visits.push((
            parent.map(str::to_string),
            e.path.clone(),
            path.to_string(),
        ));
    });
    assert_eq!(
        visits,
        vec![
            (None, "r1.md".into(), "1".into()),
            (Some("r1.md".into()), "c1.md".into(), "1.1".into()),
            (Some("c1.md".into()), "g1.md".into(), "1.1.1".into()),
            (None, "r2.md".into(), "2".into()),
        ]
    );
}

#[test]
fn parse_waypoint_round_trip() {
    let src = "---\nhiker:\n  kind: waypoint\n  references:\n    path: research/raptor-paper.md\n  in_trail:\n    path: trails/my-trail.md\n---\nuser annotation\n";
    let parsed = parse_waypoint(src).unwrap();
    assert_eq!(parsed.references, "research/raptor-paper.md");
    assert_eq!(parsed.in_trail, "trails/my-trail.md");

    let written = write_waypoint_frontmatter(src, &parsed).unwrap();
    let reparsed = parse_waypoint(&written).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(written.ends_with("user annotation\n"));
}

#[test]
fn parse_trail_doc_for_rejects_non_markdown() {
    let src = "---\nhiker:\n  kind: trail\n---\n";
    let err = parse_trail_doc_for("trails/my-trail.txt", src).unwrap_err();
    assert!(matches!(err, Error::NotMarkdown(_)));
    assert!(parse_trail_doc_for("trails/my-trail.md", src).is_ok());
}

#[test]
fn parse_trail_doc_rejects_wrong_kind() {
    let src = "---\nhiker:\n  kind: waypoint\n---\n";
    let err = parse_trail_doc(src).unwrap_err();
    assert!(matches!(err, Error::KindMismatch { expected: "trail", .. }));
}

#[test]
fn write_trail_doc_preserves_unknown_hiker_siblings() {
    // hiker.author and hiker.provenance must round-trip; only the
    // managed trail-doc fields get rewritten.
    let src = "---\nhiker:\n  kind: trail\n  author: user-authored\n  provenance: user\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("author: user-authored"));
    assert!(written.contains("provenance: user"));
}

// status: trail-doc-shape — rewriting a legacy trail-doc that carried
// `hiker.id` drops the stale field.
#[test]
fn write_trail_doc_strips_legacy_hiker_id() {
    let src = "---\nhiker:\n  kind: trail\n  id: 01HLEGACY\n  waypoints: []\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(!written.contains("01HLEGACY"));
    assert!(!written.contains("\n  id:"));
}

// status: trail-empty-waypoint-body
#[test]
fn empty_waypoint_note_has_zero_bytes_after_closing_fm() {
    let src = {
        let fm = crate::trails::WaypointFrontmatter {
            references: "research/raptor.md".to_string(),
            in_trail: "trails/my-trail.md".to_string(),
        };
        crate::trails::write_waypoint_frontmatter("", &fm).unwrap()
    };
    // The body must end at the closing `---\n` with no further bytes.
    // (This is the "clean canvas" invariant from the spec.)
    let body_start = src.find("---\n").unwrap();
    let after_first = body_start + "---\n".len();
    let close_rel = src[after_first..].find("---\n").unwrap();
    let close_abs = after_first + close_rel + "---\n".len();
    assert_eq!(
        close_abs,
        src.len(),
        "expected zero bytes after closing fm; got: {:?}",
        &src[close_abs..]
    );
}

#[test]
fn write_trail_doc_preserves_top_level_non_hiker_fields() {
    let src = "---\ntitle: My Trail\nhiker:\n  kind: trail\n  waypoints: []\ntags: [research]\n---\nbody\n";
    let parsed = parse_trail_doc(src).unwrap();
    let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
    assert!(written.contains("title: My Trail"));
    assert!(written.contains("tags:"));
}
