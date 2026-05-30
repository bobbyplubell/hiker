//! Trails test suite. Under path-as-identity (`trail-path-references`)
//! the previous ULID-stamping + double-link integration tests retired;
//! the parse / write / walk tests in `parse.rs` cover the path-only
//! shape going forward.

use super::*;

mod parse;

pub(super) fn we(path: &str) -> WaypointEntry {
    WaypointEntry {
        path: path.to_string(),
        waypoints: Vec::new(),
    }
}
