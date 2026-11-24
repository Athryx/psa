//! Storage encoding for private codemode snapshots.
//!
//! Monty dumps are uncompressed CBOR that repeats every live REPL value, so
//! they are zstd-compressed at rest.

use super::{DbError, events::MAX_CODEMODE_SNAPSHOT_BYTES};

const ZSTD_LEVEL: i32 = 3;

/// Compress a snapshot for storage, off the async runtime.
pub(super) async fn encode_snapshot(snapshot: &[u8]) -> Result<Vec<u8>, DbError> {
    let snapshot = snapshot.to_vec();
    tokio::task::spawn_blocking(move || zstd::bulk::compress(&snapshot, ZSTD_LEVEL))
        .await
        .map_err(|e| DbError::InvalidInput(format!("snapshot compression task failed: {e}")))?
        .map_err(|e| DbError::InvalidInput(format!("cannot compress codemode snapshot: {e}")))
}

/// Restore stored snapshot bytes, refusing output beyond the snapshot limit.
pub(super) async fn decode_snapshot(stored: Vec<u8>) -> Result<Vec<u8>, DbError> {
    tokio::task::spawn_blocking(move || {
        zstd::bulk::decompress(&stored, MAX_CODEMODE_SNAPSHOT_BYTES)
    })
    .await
    .map_err(|e| DbError::Corrupt(format!("snapshot decompression task failed: {e}")))?
    .map_err(|e| DbError::Corrupt(format!("cannot decompress codemode snapshot: {e}")))
}
