//! The **game manifest** — the structural, hot-path index of a title.
//!
//! Per the design (HARD CONSTRAINT #10) a title has **two decoupled payloads**:
//! this structural manifest (files → chunks → hashes) and a separate
//! title-info payload (cover/metadata, see [`crate::protocol::TitleInfo`]). A
//! cover/info edit must never bump the manifest version, and the `.blt/`
//! sidecar is excluded from the distributable manifest (the server is
//! responsible for that exclusion at scan time).
//!
//! Manifest versions are **immutable snapshots**: a client's in-flight download
//! is keyed by `(title_id, manifest_ver)` and always completes against the
//! version it started on (F4.9).

use crate::chunking::{plan_chunks, DEFAULT_CHUNK_SIZE};
use crate::hashing::Hash;
use serde::{Deserialize, Serialize};

/// One chunk of a file: position within the file plus its content hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    /// Chunk index within its file (0-based).
    pub idx: u32,
    /// Byte offset within the file.
    pub offset: u64,
    /// Chunk length in bytes (final chunk may be short).
    pub size: u64,
    /// BLAKE3 of the chunk bytes — checked on arrival before any write.
    pub hash: Hash,
}

/// One file in a title: a relative path (always forward-slash separated,
/// sanitised on the writing side) plus its chunk list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Server-assigned stable file id (used to address `/chunks/{file_id}/{idx}`).
    pub id: u64,
    /// Path relative to the title root, `/`-separated and canonical.
    pub rel_path: String,
    /// File length in bytes.
    pub size: u64,
    /// Last-modified time (unix seconds) — used for fast change detection.
    pub mtime: i64,
    /// BLAKE3 of the whole file (deep verify / change detection).
    pub hash: Hash,
    /// The file's chunks in order. Empty for a zero-byte file.
    pub chunks: Vec<ChunkEntry>,
}

impl FileEntry {
    pub fn chunk_count(&self) -> u64 {
        self.chunks.len() as u64
    }
}

/// The full structural manifest of a title at a specific version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub title_id: u64,
    pub manifest_ver: u32,
    pub total_size: u64,
    pub file_count: u64,
    pub chunk_size: u64,
    /// Files in stable order; this order defines the global chunk/bitmap order.
    pub files: Vec<FileEntry>,
}

/// A flattened, download-ready view of one chunk: everything the downloader
/// needs to **fetch** (`file_id`, `chunk_idx`), **verify** (`hash`), and
/// **write** (`rel_path`, `offset`) a chunk. The `global_idx` is the chunk's
/// position in the title-wide bitmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocator {
    pub global_idx: u64,
    pub file_id: u64,
    pub rel_path: String,
    pub file_size: u64,
    pub chunk_idx: u32,
    pub offset: u64,
    pub size: u64,
    pub hash: Hash,
}

/// Result of comparing two manifest versions of the same title.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestDiff {
    /// Paths present in `new` but not `old`.
    pub added: Vec<String>,
    /// Paths present in both whose content hash differs.
    pub changed: Vec<String>,
    /// Paths present in `old` but not `new`.
    pub removed: Vec<String>,
}

impl ManifestDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Internal-consistency problems detected by [`Manifest::validate_structure`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("file {path}: chunk sizes ({chunk_total}) do not sum to file size ({size})")]
    ChunkSizeMismatch {
        path: String,
        chunk_total: u64,
        size: u64,
    },
    #[error("file {path}: chunk {idx} offset {got} expected {want}")]
    ChunkOffset {
        path: String,
        idx: u32,
        got: u64,
        want: u64,
    },
    #[error("total_size ({got}) does not match the sum of file sizes ({want})")]
    TotalSizeMismatch { got: u64, want: u64 },
    #[error("file_count ({got}) does not match the number of files ({want})")]
    FileCountMismatch { got: u64, want: u64 },
}

impl Manifest {
    /// Total number of chunks across all files (== bitmap length).
    pub fn chunk_count(&self) -> u64 {
        self.files.iter().map(FileEntry::chunk_count).sum()
    }

    /// Flatten into ordered [`ChunkLocator`]s. The order is files in manifest
    /// order, chunks in index order — this is the canonical bitmap ordering and
    /// must stay stable for resume to work.
    pub fn chunk_locators(&self) -> Vec<ChunkLocator> {
        let mut out = Vec::with_capacity(self.chunk_count() as usize);
        let mut global = 0u64;
        for f in &self.files {
            for c in &f.chunks {
                out.push(ChunkLocator {
                    global_idx: global,
                    file_id: f.id,
                    rel_path: f.rel_path.clone(),
                    file_size: f.size,
                    chunk_idx: c.idx,
                    offset: c.offset,
                    size: c.size,
                    hash: c.hash,
                });
                global += 1;
            }
        }
        out
    }

    /// Look up a file by its server id.
    pub fn file_by_id(&self, id: u64) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.id == id)
    }

    /// Recompute `total_size`/`file_count` from `files` (builder convenience).
    pub fn recompute_totals(&mut self) {
        self.total_size = self.files.iter().map(|f| f.size).sum();
        self.file_count = self.files.len() as u64;
    }

    /// Check internal consistency (offsets contiguous, sizes sum, totals match).
    /// Defensive — a well-formed scan always passes; useful as a guard + in tests.
    pub fn validate_structure(&self) -> Result<(), ManifestError> {
        let mut total = 0u64;
        for f in &self.files {
            let mut expected_offset = 0u64;
            let mut chunk_total = 0u64;
            for c in &f.chunks {
                if c.offset != expected_offset {
                    return Err(ManifestError::ChunkOffset {
                        path: f.rel_path.clone(),
                        idx: c.idx,
                        got: c.offset,
                        want: expected_offset,
                    });
                }
                expected_offset += c.size;
                chunk_total += c.size;
            }
            if chunk_total != f.size {
                return Err(ManifestError::ChunkSizeMismatch {
                    path: f.rel_path.clone(),
                    chunk_total,
                    size: f.size,
                });
            }
            total += f.size;
        }
        if self.total_size != total {
            return Err(ManifestError::TotalSizeMismatch {
                got: self.total_size,
                want: total,
            });
        }
        if self.file_count != self.files.len() as u64 {
            return Err(ManifestError::FileCountMismatch {
                got: self.file_count,
                want: self.files.len() as u64,
            });
        }
        Ok(())
    }
}

/// Build a [`FileEntry`] from a known size/mtime/whole-file hash and per-chunk
/// hashes. `chunk_hashes` must already be in chunk order. Used by the server
/// scanner after hashing a file.
pub fn build_file_entry(
    id: u64,
    rel_path: String,
    size: u64,
    mtime: i64,
    file_hash: Hash,
    chunk_size: u64,
    chunk_hashes: &[Hash],
) -> FileEntry {
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    };
    let plans = plan_chunks(size, chunk_size);
    debug_assert_eq!(plans.len(), chunk_hashes.len());
    let chunks = plans
        .into_iter()
        .zip(chunk_hashes.iter().copied())
        .map(|(p, hash)| ChunkEntry {
            idx: p.idx,
            offset: p.offset,
            size: p.size,
            hash,
        })
        .collect();
    FileEntry {
        id,
        rel_path,
        size,
        mtime,
        hash: file_hash,
        chunks,
    }
}

/// Diff two manifests of the same title by relative path. A file counts as
/// `changed` when its whole-file hash differs.
pub fn diff(old: &Manifest, new: &Manifest) -> ManifestDiff {
    use std::collections::HashMap;
    let old_by: HashMap<&str, &FileEntry> =
        old.files.iter().map(|f| (f.rel_path.as_str(), f)).collect();
    let new_by: HashMap<&str, &FileEntry> =
        new.files.iter().map(|f| (f.rel_path.as_str(), f)).collect();

    let mut d = ManifestDiff::default();
    for f in &new.files {
        match old_by.get(f.rel_path.as_str()) {
            None => d.added.push(f.rel_path.clone()),
            Some(o) if o.hash != f.hash => d.changed.push(f.rel_path.clone()),
            Some(_) => {}
        }
    }
    for f in &old.files {
        if !new_by.contains_key(f.rel_path.as_str()) {
            d.removed.push(f.rel_path.clone());
        }
    }
    d.added.sort();
    d.changed.sort();
    d.removed.sort();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::DEFAULT_CHUNK_SIZE;
    use crate::hashing::hash_bytes;

    fn file(id: u64, path: &str, size: u64, tag: &[u8], nchunks: usize) -> FileEntry {
        let mut chunks = Vec::new();
        let cs = DEFAULT_CHUNK_SIZE;
        let plans = plan_chunks(size, cs);
        assert_eq!(plans.len(), nchunks);
        for p in plans {
            chunks.push(ChunkEntry {
                idx: p.idx,
                offset: p.offset,
                size: p.size,
                hash: hash_bytes(&[tag, &p.idx.to_le_bytes()].concat()),
            });
        }
        FileEntry {
            id,
            rel_path: path.into(),
            size,
            mtime: 0,
            hash: hash_bytes(tag),
            chunks,
        }
    }

    fn manifest(files: Vec<FileEntry>) -> Manifest {
        let mut m = Manifest {
            title_id: 1,
            manifest_ver: 1,
            total_size: 0,
            file_count: 0,
            chunk_size: DEFAULT_CHUNK_SIZE,
            files,
        };
        m.recompute_totals();
        m
    }

    #[test]
    fn chunk_count_and_locators_are_ordered() {
        let m = manifest(vec![
            file(1, "a.bin", DEFAULT_CHUNK_SIZE + 1, b"a", 2),
            file(2, "b.bin", 10, b"b", 1),
        ]);
        assert_eq!(m.chunk_count(), 3);
        let locs = m.chunk_locators();
        assert_eq!(locs.len(), 3);
        // global indices are 0,1,2 in file/chunk order
        assert_eq!(locs[0].global_idx, 0);
        assert_eq!(locs[0].file_id, 1);
        assert_eq!(locs[1].global_idx, 1);
        assert_eq!(locs[1].file_id, 1);
        assert_eq!(locs[2].global_idx, 2);
        assert_eq!(locs[2].file_id, 2);
        assert_eq!(locs[2].chunk_idx, 0);
    }

    #[test]
    fn validate_structure_passes_for_well_formed() {
        let m = manifest(vec![file(1, "a.bin", DEFAULT_CHUNK_SIZE + 7, b"a", 2)]);
        m.validate_structure().unwrap();
    }

    #[test]
    fn validate_structure_catches_bad_offset() {
        let mut m = manifest(vec![file(1, "a.bin", DEFAULT_CHUNK_SIZE + 7, b"a", 2)]);
        m.files[0].chunks[1].offset += 1;
        assert!(matches!(
            m.validate_structure(),
            Err(ManifestError::ChunkOffset { .. })
        ));
    }

    #[test]
    fn diff_detects_add_change_remove() {
        let old = manifest(vec![
            file(1, "keep.bin", 10, b"keep", 1),
            file(2, "change.bin", 10, b"old", 1),
            file(3, "gone.bin", 10, b"gone", 1),
        ]);
        let new = manifest(vec![
            file(1, "keep.bin", 10, b"keep", 1),
            file(2, "change.bin", 10, b"new", 1), // different tag => different hash
            file(4, "added.bin", 10, b"added", 1),
        ]);
        let d = diff(&old, &new);
        assert_eq!(d.added, vec!["added.bin"]);
        assert_eq!(d.changed, vec!["change.bin"]);
        assert_eq!(d.removed, vec!["gone.bin"]);
        assert!(!d.is_empty());
    }

    #[test]
    fn diff_empty_when_identical() {
        let m = manifest(vec![file(1, "a.bin", 10, b"a", 1)]);
        assert!(diff(&m, &m).is_empty());
    }

    #[test]
    fn build_file_entry_zips_plans_and_hashes() {
        let cs = DEFAULT_CHUNK_SIZE;
        let size = cs + 100;
        let hashes = vec![hash_bytes(b"c0"), hash_bytes(b"c1")];
        let fe = build_file_entry(9, "x".into(), size, 42, hash_bytes(b"file"), cs, &hashes);
        assert_eq!(fe.chunks.len(), 2);
        assert_eq!(fe.chunks[0].hash, hashes[0]);
        assert_eq!(fe.chunks[1].hash, hashes[1]);
        assert_eq!(fe.chunks[1].size, 100);
    }
}
