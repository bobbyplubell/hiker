//! The `chunk_vecs` vector concern: KNN retrieval plus the embedding
//! (de)serialization and persisted-dimension plumbing it rides on. The
//! `chunk_vecs` virtual table stores raw little-endian f32 blobs; these
//! helpers convert between `Vec<f32>` and that blob form, mean-pool a
//! note's chunk embeddings, read back the dim sqlite-vec refuses to report
//! via PRAGMA, and run the similarity query that backs semantic search.

use rusqlite::{params, Connection, OptionalExtension};

use super::error::Error;
use super::dto::ChunkHit;
use super::DEFAULT_EMBED_DIM;

/// Standalone KNN against any connection (so read connections from
/// `Store::open_reader` can reuse the implementation). Returns up to
/// `top_k` chunk hits ranked by similarity, optionally excluding chunks
/// from `exclude_note_id`.
pub fn knn_chunks_on(
    conn: &Connection,
    query: &[f32],
    top_k: usize,
    exclude_note_id: Option<&str>,
) -> Result<Vec<ChunkHit>, Error> {
    let expected = read_chunk_vecs_dim(conn)?.unwrap_or(DEFAULT_EMBED_DIM);
    if query.len() != expected {
        return Err(Error::EmbedDim {
            got: query.len(),
            expected,
        });
    }
    // Pull a wider candidate set than top_k since we filter post-hoc; vec0
    // doesn't support arbitrary WHERE filters in the MATCH clause.
    let candidate_k = top_k.saturating_mul(4).max(top_k + 16);
    let mut stmt = conn.prepare(
        "SELECT v.chunk_id, c.note_id, n.path, c.heading_path, c.text, v.distance
         FROM chunk_vecs v
         JOIN chunks c ON c.id = v.chunk_id
         JOIN notes n ON n.id = c.note_id
         WHERE v.embedding MATCH ?1 AND k = ?2
         ORDER BY v.distance",
    )?;
    let blob = embedding_to_blob(query);
    let rows = stmt
        .query_map(params![blob, candidate_k as i64], |row| {
            let distance: f32 = row.get(5)?;
            Ok(ChunkHit {
                chunk_id: row.get(0)?,
                note_id: row.get(1)?,
                note_path: row.get(2)?,
                heading_path: row.get(3)?,
                text: row.get(4)?,
                // sqlite-vec returns L2 distance; convert to a similarity-ish
                // score so higher = closer. 1 / (1 + d) is bounded (0, 1].
                score: 1.0 / (1.0 + distance),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let filtered: Vec<ChunkHit> = match exclude_note_id {
        Some(id) => rows.into_iter().filter(|h| h.note_id != id).collect(),
        None => rows,
    };
    Ok(filtered.into_iter().take(top_k).collect())
}

pub(super) fn embedding_to_blob(emb: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(emb.len() * 4);
    for f in emb {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Inverse of `embedding_to_blob`. Trailing partial-f32 bytes (impossible
/// on a well-formed blob) are dropped.
pub(super) fn blob_to_embedding(blob: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(blob.len() / 4);
    for chunk in blob.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().expect("chunks_exact yields 4 bytes");
        out.push(f32::from_le_bytes(arr));
    }
    out
}

/// Byte-length-weighted mean-pool over a note's chunk embeddings. Empty
/// input is rejected; zero total weight (every chunk reported zero-length,
/// which shouldn't happen but is cheap to guard against) falls back to an
/// unweighted mean. See `cluster-note-embeddings` in `clustering.md`.
pub(super) fn byte_weighted_mean_pool(
    weighted: &[(Vec<f32>, u64)],
    expected_dim: usize,
) -> Result<Vec<f32>, Error> {
    debug_assert!(!weighted.is_empty());
    let dim = weighted[0].0.len();
    if dim != expected_dim {
        return Err(Error::EmbedDim {
            got: dim,
            expected: expected_dim,
        });
    }
    let mut acc = vec![0.0f32; dim];
    let mut total: u64 = 0;
    for (vec, w) in weighted {
        if vec.len() != dim {
            return Err(Error::EmbedDim {
                got: vec.len(),
                expected: dim,
            });
        }
        // Treat a zero-length chunk as weight 1 so it still contributes —
        // dropping it would silently make empty-headed chunks invisible to
        // the pool.
        let effective = (*w).max(1);
        total = total.saturating_add(effective);
        let wf = effective as f32;
        for (a, &v) in acc.iter_mut().zip(vec.iter()) {
            *a += v * wf;
        }
    }
    let denom = (total.max(1)) as f32;
    for a in acc.iter_mut() {
        *a /= denom;
    }
    Ok(acc)
}

/// Read the on-disk `chunk_vecs` embedding column dim. Returns `None` when
/// the dim hasn't been recorded yet (the meta row is missing — fresh db
/// state before the first `ensure_schema` run).
///
/// Implementation note: `PRAGMA table_info(chunk_vecs)` does *not* report
/// the bracketed `float[N]` type the spec describes — sqlite-vec
/// `declare_vtab`s the embedding column with an empty type slot (see
/// sqlite-vec 0.1.x `vec0_init`, which builds `CREATE TABLE x(...
/// "embedding", distance hidden, k hidden)`). So we persist the dim
/// ourselves in a tiny `meta` key/value table at `ensure_schema` /
/// rebuild time, and read it back here. The PRAGMA-based approach in the
/// original spec would only work against a vec0 release that surfaces
/// the dim in `table_info`; this is the equivalent guarantee until then.
///
/// status: store-rebuild-chunk-vecs-on-dim-change
pub(super) fn read_chunk_vecs_dim(conn: &Connection) -> Result<Option<usize>, Error> {
    let row: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'chunk_vecs_dim'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.and_then(|s| s.parse().ok()))
}
