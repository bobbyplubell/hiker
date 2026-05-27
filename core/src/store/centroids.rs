//! Derived cluster-tree centroid cache (`trees-centroids-index`).
//!
//! Cluster trees live as `.md` files (`trees-md-store`); their per-cluster
//! centroids — packed little-endian f32 — are a recomputable index cache
//! kept here instead of in the synced markdown. `core::trees::Db` writes
//! them on node insert and reads them back when hydrating a tree so the
//! placement classifier (`cluster-place-beam-descent`) can score.

use std::collections::HashMap;

use rusqlite::params;

use super::error::Error;
use super::Store;

fn pack_centroid(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn unpack_centroid(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

impl Store {
    /// Insert or replace the centroid for one cluster node.
    ///
    /// status: trees-centroids-index
    pub fn put_cluster_centroid(
        &mut self,
        tree_id: &str,
        node_id: &str,
        centroid: &[f32],
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO cluster_centroids (tree_id, node_id, centroid)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(tree_id, node_id) DO UPDATE SET centroid = excluded.centroid",
            params![tree_id, node_id, pack_centroid(centroid)],
        )?;
        Ok(())
    }

    /// All centroids for a tree, keyed by node id.
    ///
    /// status: trees-centroids-index
    pub fn cluster_centroids_for_tree(
        &self,
        tree_id: &str,
    ) -> Result<HashMap<String, Vec<f32>>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT node_id, centroid FROM cluster_centroids WHERE tree_id = ?1")?;
        let rows = stmt.query_map(params![tree_id], |row| {
            let node_id: String = row.get(0)?;
            let bytes: Vec<u8> = row.get(1)?;
            Ok((node_id, unpack_centroid(&bytes)))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (id, c) = r?;
            out.insert(id, c);
        }
        Ok(out)
    }

    /// Drop every centroid for a tree (tree discarded / rebuilt).
    ///
    /// status: trees-centroids-index
    pub fn delete_cluster_centroids_for_tree(&mut self, tree_id: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM cluster_centroids WHERE tree_id = ?1", params![tree_id])?;
        Ok(())
    }

    /// Drop one node's centroid.
    ///
    /// status: trees-centroids-index
    pub fn delete_cluster_centroid(&mut self, tree_id: &str, node_id: &str) -> Result<(), Error> {
        self.conn.execute(
            "DELETE FROM cluster_centroids WHERE tree_id = ?1 AND node_id = ?2",
            params![tree_id, node_id],
        )?;
        Ok(())
    }
}
