//! Unit tests for `Trail::move_waypoint`. Split out of `mod.rs` to keep
//! the parent module under the file-length budget.

use super::*;

fn wp(path: &str, children: Vec<Waypoint>) -> Waypoint {
    Waypoint { path: path.into(), at_ms: 0, children, annotation: String::new() }
}
fn tr(waypoints: Vec<Waypoint>) -> Trail {
    Trail {
        id: "t".into(),
        name: "t".into(),
        waypoints,
        created_at_ms: 0,
        last_activated_at_ms: 0,
        append_under: None,
    }
}
#[test]
fn move_to_tail_reorders_root() {
    let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
    assert!(t.move_waypoint("a", MoveOp::Tail));
    assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
}
#[test]
fn move_before_inserts_at_root() {
    let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
    assert!(t.move_waypoint("c", MoveOp::Before("a".into())));
    assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["c", "a", "b"]);
}
#[test]
fn move_as_child_nests() {
    let mut t = tr(vec![wp("a", vec![]), wp("b", vec![])]);
    assert!(t.move_waypoint("b", MoveOp::Child("a".into())));
    assert_eq!(t.waypoints.len(), 1);
    assert_eq!(t.waypoints[0].path, "a");
    assert_eq!(t.waypoints[0].children[0].path, "b");
}
#[test]
fn move_after_inserts_following_target_at_root() {
    let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
    assert!(t.move_waypoint("a", MoveOp::After("b".into())));
    assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "a", "c"]);
}
#[test]
fn move_after_last_appends_to_tail() {
    let mut t = tr(vec![wp("a", vec![]), wp("b", vec![]), wp("c", vec![])]);
    assert!(t.move_waypoint("a", MoveOp::After("c".into())));
    assert_eq!(t.waypoints.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), vec!["b", "c", "a"]);
}
#[test]
fn move_after_nested_target_stays_in_parent_list() {
    let mut t = tr(vec![wp("a", vec![wp("a1", vec![]), wp("a2", vec![])]), wp("b", vec![])]);
    assert!(t.move_waypoint("b", MoveOp::After("a1".into())));
    let children = t.waypoints[0].children.iter().map(|w| w.path.as_str()).collect::<Vec<_>>();
    assert_eq!(children, vec!["a1", "b", "a2"]);
}
#[test]
fn cycle_drop_into_own_subtree_rejected() {
    let mut t = tr(vec![wp("a", vec![wp("a1", vec![])])]);
    assert!(!t.move_waypoint("a", MoveOp::Child("a1".into())));
    // Untouched.
    assert_eq!(t.waypoints[0].path, "a");
    assert_eq!(t.waypoints[0].children[0].path, "a1");
}
