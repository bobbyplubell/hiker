use rusqlite::params;

use super::error::Error;
use super::dto::{TrailContainingHit, WaypointRow};
use super::Store;

impl Store {
    /// Insert or replace one `trail_waypoints` row.
    ///
    /// status: trail-waypoints-derived-table
    pub fn upsert_trail_waypoint(
        &mut self,
        row: &WaypointRow,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO trail_waypoints
               (waypoint_path, waypoint_id, trail_id, source_id, source_path,
                parent_waypoint_id, tree_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(waypoint_path) DO UPDATE SET
               waypoint_id        = excluded.waypoint_id,
               trail_id           = excluded.trail_id,
               source_id          = excluded.source_id,
               source_path        = excluded.source_path,
               parent_waypoint_id = excluded.parent_waypoint_id,
               tree_path          = excluded.tree_path",
            params![
                row.waypoint_path,
                row.waypoint_id,
                row.trail_id,
                row.source_id,
                row.source_path,
                row.parent_waypoint_id,
                row.tree_path,
            ],
        )?;
        Ok(())
    }

    /// Delete every row tied to `trail_id`. Used by the trail-doc walk
    /// path (clear + re-insert) so the table stays consistent with the
    /// trail-doc's `hiker.waypoints` array.
    ///
    /// status: trail-waypoints-derived-table
    pub fn delete_trail_waypoints_by_trail(
        &mut self,
        trail_id: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "DELETE FROM trail_waypoints WHERE trail_id = ?1",
            params![trail_id],
        )?;
        Ok(n)
    }

    /// Delete a single waypoint row by its waypoint path.
    ///
    /// status: trail-waypoints-derived-table
    pub fn delete_trail_waypoint_by_path(
        &mut self,
        waypoint_path: &str,
    ) -> Result<usize, Error> {
        let n = self.conn.execute(
            "DELETE FROM trail_waypoints WHERE waypoint_path = ?1",
            params![waypoint_path],
        )?;
        Ok(n)
    }

    /// Trails that contain a given source note. `note_id_or_path` is
    /// matched against both `source_id` and `source_path` so the caller
    /// doesn't have to pre-resolve.
    ///
    /// status: trail-waypoints-derived-table
    pub fn trails_containing_note(
        &self,
        note_id_or_path: &str,
    ) -> Result<Vec<TrailContainingHit>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT trail_id, waypoint_path, waypoint_id, tree_path
             FROM trail_waypoints
             WHERE source_id = ?1 OR source_path = ?1
             ORDER BY trail_id, tree_path",
        )?;
        let rows = stmt
            .query_map(params![note_id_or_path], |row| {
                Ok(TrailContainingHit {
                    trail_id: row.get(0)?,
                    waypoint_path: row.get(1)?,
                    waypoint_id: row.get(2)?,
                    tree_path: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All waypoint rows for a given trail, ordered by `tree_path` so
    /// the natural row order matches reading order.
    ///
    /// status: trail-waypoints-derived-table
    pub fn waypoints_of(&self, trail_id: &str) -> Result<Vec<WaypointRow>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT waypoint_path, waypoint_id, trail_id, source_id, source_path,
                    parent_waypoint_id, tree_path
             FROM trail_waypoints
             WHERE trail_id = ?1
             ORDER BY tree_path",
        )?;
        let rows = stmt
            .query_map(params![trail_id], |row| {
                Ok(WaypointRow {
                    waypoint_path: row.get(0)?,
                    waypoint_id: row.get(1)?,
                    trail_id: row.get(2)?,
                    source_id: row.get(3)?,
                    source_path: row.get(4)?,
                    parent_waypoint_id: row.get(5)?,
                    tree_path: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Bulk-rewrite waypoint paths whose `waypoint_path` starts with
    /// `old_prefix` to start with `new_prefix` instead. Used by the
    /// auto-update-on-move path (slice 3) when a trail's waypoint dir is
    /// renamed alongside the trail-doc; landed here so the API surface
    /// is settled.
    ///
    /// status: trail-waypoints-derived-table
    pub fn rename_trail_waypoint_paths(
        &mut self,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<usize, Error> {
        // Use SQLite substr() to splice the new prefix in. Length-of-prefix
        // is computed on the server so we don't have to re-bind it.
        let like_pattern = format!("{}%", old_prefix);
        let n = self.conn.execute(
            "UPDATE trail_waypoints
             SET waypoint_path = ?1 || substr(waypoint_path, ?2)
             WHERE waypoint_path LIKE ?3",
            params![
                new_prefix,
                (old_prefix.len() as i64) + 1,
                like_pattern,
            ],
        )?;
        Ok(n)
    }
}
