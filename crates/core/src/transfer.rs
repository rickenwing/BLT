//! Transfer primitives: verify-before-write and validation.
//!
//! The async fetch loop (retry/backoff, P2P source selection, concurrency)
//! lives in the desktop app; the *correctness-critical* pieces live here and are
//! tested against real temp files:
//!
//! - [`verify_and_write`] — the single chokepoint for HARD CONSTRAINT #1: a
//!   chunk is BLAKE3-checked against the manifest hash **before** any byte hits
//!   disk. A bad peer can never corrupt a title.
//! - [`validate_quick`] — presence + size (F5.1).
//! - [`validate_deep`] — full re-hash vs manifest (F5.2), with per-file results
//!   and a repair plan of mismatched chunks.
//! - [`completeness_check`] — the shared-pool "X of N files" check (F6.8).

use crate::hashing::{Hash, StreamHasher, verify};
use crate::manifest::Manifest;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Errors from writing or joining a destination path.
#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("chunk hash mismatch (expected {expected}) — not written")]
    HashMismatch { expected: Hash },
    #[error("unsafe relative path: {0}")]
    UnsafePath(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Join a `/`-separated relative path under `root`, refusing traversal/absolute
/// components. Manifest paths are server-derived, but we never trust a relative
/// path to stay in-bounds without checking.
///
/// `:` is rejected anywhere in a component: on Windows `C:foo` is
/// drive-relative (PathBuf::push would *replace* the whole path) and
/// `name:stream` writes an NTFS alternate data stream — both escapes. Filenames
/// containing `:` are not portable to Windows anyway (pathsafe transforms them
/// on the shared-pool write side).
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, TransferError> {
    let mut out = root.to_path_buf();
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." || comp.contains('\\') || comp.contains(':') {
            return Err(TransferError::UnsafePath(rel.to_string()));
        }
        out.push(comp);
    }
    if out == root {
        return Err(TransferError::UnsafePath(rel.to_string()));
    }
    Ok(out)
}

/// Write `data` at `offset` into `root/rel_path`, creating parent dirs and the
/// file as needed. Does **not** verify — callers must go through
/// [`verify_and_write`] for received data.
pub fn write_at(
    root: &Path,
    rel_path: &str,
    offset: u64,
    data: &[u8],
) -> Result<(), TransferError> {
    let path = safe_join(root, rel_path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(data)?;
    Ok(())
}

/// Verify a received chunk against its manifest hash, then write it. This is the
/// guard behind HARD CONSTRAINT #1 — a failed verify returns an error and
/// touches nothing on disk.
pub fn verify_and_write(
    root: &Path,
    rel_path: &str,
    offset: u64,
    data: &[u8],
    expected: &Hash,
) -> Result<(), TransferError> {
    if !verify(data, expected) {
        return Err(TransferError::HashMismatch {
            expected: *expected,
        });
    }
    write_at(root, rel_path, offset, data)
}

/// Writes verified chunks through **pooled, kept-open file handles** instead of
/// opening + closing the destination file for every chunk.
///
/// PERF-2: on Windows each `CloseHandle` of a growing multi-GB game file
/// triggers a Defender scan of the whole file, so per-chunk open/close made disk
/// writes take seconds and *grow* as the file filled — the download stalled
/// (network idle between fetches) and decayed. macOS has no such scan, so it was
/// Windows-only. Holding handles open scans once at final close instead.
///
/// Verify-before-write (HARD CONSTRAINT #1) is unchanged: a chunk is BLAKE3-
/// checked before any byte is written, and a bad chunk touches nothing.
pub struct PooledWriter {
    root: PathBuf,
    /// Open handles, least-recently-used first.
    open: Vec<(String, fs::File)>,
    max_open: usize,
}

impl PooledWriter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            open: Vec::new(),
            max_open: 8,
        }
    }

    /// BLAKE3-verify `data`, then write it at `offset` into `rel_path` via a
    /// kept-open handle. A bad chunk returns `HashMismatch` and writes nothing.
    pub fn verify_and_write(
        &mut self,
        rel_path: &str,
        offset: u64,
        data: &[u8],
        expected: &Hash,
    ) -> Result<(), TransferError> {
        if !verify(data, expected) {
            return Err(TransferError::HashMismatch {
                expected: *expected,
            });
        }
        let f = self.handle(rel_path)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(())
    }

    /// Fetch-or-open the handle for `rel_path`, promoting it to most-recently-used
    /// and evicting (closing) the LRU handle past `max_open`.
    fn handle(&mut self, rel_path: &str) -> Result<&mut fs::File, TransferError> {
        if let Some(pos) = self.open.iter().position(|(p, _)| p == rel_path) {
            let entry = self.open.remove(pos);
            self.open.push(entry);
        } else {
            let path = safe_join(&self.root, rel_path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let f = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            if self.open.len() >= self.max_open {
                self.open.remove(0); // drop = close the LRU handle
            }
            self.open.push((rel_path.to_string(), f));
        }
        Ok(&mut self.open.last_mut().expect("just inserted").1)
    }

    /// Close all open handles (flushing to the OS). Call before validating or
    /// finalising so reads see complete files.
    pub fn close_all(&mut self) {
        self.open.clear();
    }
}

/// Bring the on-disk tree in line with the manifest's **shape**: create
/// zero-byte files (which have no chunks, hence no bitmap bits — the chunk
/// loop never touches them) and truncate files that are **longer** than their
/// manifest size (in-place updates leave trailing bytes because [`write_at`]
/// never truncates; chunk repair alone can't fix that).
///
/// Call this when a download's bitmap completes and before chunk repair —
/// without it, deep verify can fail forever with an empty repair plan.
pub fn finalize_layout(manifest: &Manifest, root: &Path) -> Result<(), TransferError> {
    for f in &manifest.files {
        let path = safe_join(root, &f.rel_path)?;
        if f.size == 0 {
            // Materialise the empty file (presence is the completeness signal).
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            continue;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > f.size {
                let file = fs::OpenOptions::new().write(true).open(&path)?;
                file.set_len(f.size)?;
            }
        }
    }
    Ok(())
}

/// Why a file failed validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckDetail {
    Ok,
    Missing,
    SizeMismatch { expected: u64, got: u64 },
    HashMismatch,
}

/// Per-file validation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheck {
    pub rel_path: String,
    pub detail: CheckDetail,
}

impl FileCheck {
    pub fn ok(&self) -> bool {
        self.detail == CheckDetail::Ok
    }
}

/// A validation pass over a title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub files: Vec<FileCheck>,
}

impl ValidationReport {
    pub fn all_ok(&self) -> bool {
        self.files.iter().all(FileCheck::ok)
    }
    pub fn failures(&self) -> impl Iterator<Item = &FileCheck> {
        self.files.iter().filter(|f| !f.ok())
    }
    pub fn ok_count(&self) -> usize {
        self.files.iter().filter(|f| f.ok()).count()
    }
}

/// Quick validation (F5.1): each manifest file must exist at the expected size.
/// Cheap because every chunk was hash-verified on arrival.
pub fn validate_quick(manifest: &Manifest, root: &Path) -> ValidationReport {
    let files = manifest
        .files
        .iter()
        .map(|f| {
            let detail = match safe_join(root, &f.rel_path).and_then(|p| Ok(fs::metadata(&p)?)) {
                Ok(meta) if meta.len() == f.size => CheckDetail::Ok,
                Ok(meta) => CheckDetail::SizeMismatch {
                    expected: f.size,
                    got: meta.len(),
                },
                Err(_) => CheckDetail::Missing,
            };
            FileCheck {
                rel_path: f.rel_path.clone(),
                detail,
            }
        })
        .collect();
    ValidationReport { files }
}

/// Deep verify (F5.2): re-hash every file and compare to the manifest hash.
pub fn validate_deep(manifest: &Manifest, root: &Path) -> ValidationReport {
    let files = manifest
        .files
        .iter()
        .map(|f| {
            let detail = match safe_join(root, &f.rel_path) {
                Ok(path) => match hash_file(&path) {
                    Ok(h) if h == f.hash => CheckDetail::Ok,
                    Ok(_) => CheckDetail::HashMismatch,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => CheckDetail::Missing,
                    Err(_) => CheckDetail::HashMismatch,
                },
                Err(_) => CheckDetail::Missing,
            };
            FileCheck {
                rel_path: f.rel_path.clone(),
                detail,
            }
        })
        .collect();
    ValidationReport { files }
}

/// For a file the deep-verify flagged, recompute which chunk **global indices**
/// differ on disk so only those need refetching (F5.3 repair). Missing/short
/// files yield all of that file's chunk indices.
pub fn repair_plan(manifest: &Manifest, root: &Path) -> Vec<u64> {
    let mut bad = Vec::new();
    let mut global = 0u64;
    // One reusable read buffer sized to the manifest's chunk size, rather than a
    // fresh multi-MiB Vec per chunk across thousands of chunks.
    let mut buf = vec![0u8; manifest.chunk_size.max(1) as usize];
    for f in &manifest.files {
        let path = match safe_join(root, &f.rel_path) {
            Ok(p) => p,
            Err(_) => {
                for _ in &f.chunks {
                    bad.push(global);
                    global += 1;
                }
                continue;
            }
        };
        let mut file = fs::File::open(&path).ok();
        for c in &f.chunks {
            let mut ok = false;
            if let Some(fh) = file.as_mut() {
                let n = c.size as usize;
                if n > buf.len() {
                    buf.resize(n, 0);
                }
                let slice = &mut buf[..n];
                if fh.seek(SeekFrom::Start(c.offset)).is_ok() && fh.read_exact(slice).is_ok() {
                    ok = verify(slice, &c.hash);
                }
            }
            if !ok {
                bad.push(global);
            }
            global += 1;
        }
    }
    bad
}

fn hash_file(path: &Path) -> std::io::Result<Hash> {
    let mut f = fs::File::open(path)?;
    let mut hasher = StreamHasher::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

/// Shared-pool completeness (F6.8): given expected `(rel_path, size)` entries,
/// count how many are present at the right size under `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletenessReport {
    pub total: usize,
    pub present: usize,
    pub missing: Vec<String>,
}

impl CompletenessReport {
    pub fn complete(&self) -> bool {
        self.present == self.total
    }
}

pub fn completeness_check(entries: &[(String, u64)], root: &Path) -> CompletenessReport {
    let mut present = 0;
    let mut missing = Vec::new();
    for (rel, size) in entries {
        let ok = safe_join(root, rel)
            .ok()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len() == *size)
            .unwrap_or(false);
        if ok {
            present += 1;
        } else {
            missing.push(rel.clone());
        }
    }
    CompletenessReport {
        total: entries.len(),
        present,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::{DEFAULT_CHUNK_SIZE, plan_chunks};
    use crate::hashing::hash_bytes;
    use crate::manifest::{Manifest, build_file_entry};

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/root");
        assert!(safe_join(root, "../escape").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert!(safe_join(root, "C:").is_err());
        // Windows drive-relative + NTFS alternate-data-stream escapes
        assert!(safe_join(root, "C:foo/bar").is_err());
        assert!(safe_join(root, "file.txt:hidden").is_err());
        assert_eq!(
            safe_join(root, "a/b/c.bin").unwrap(),
            Path::new("/tmp/root/a/b/c.bin")
        );
    }

    #[test]
    fn finalize_layout_materialises_zero_byte_files() {
        // Regression: a zero-byte file has no chunks → no bitmap bit → the
        // chunk loop never creates it → validation failed forever.
        let dir = tmp();
        let fe = build_file_entry(
            1,
            "save/empty.cfg".into(),
            0,
            0,
            hash_bytes(b""),
            DEFAULT_CHUNK_SIZE,
            &[],
        );
        let mut m = Manifest {
            title_id: 1,
            manifest_ver: 1,
            total_size: 0,
            file_count: 0,
            chunk_size: DEFAULT_CHUNK_SIZE,
            files: vec![fe],
        };
        m.recompute_totals();

        assert!(!validate_quick(&m, dir.path()).all_ok());
        finalize_layout(&m, dir.path()).unwrap();
        assert!(dir.path().join("save/empty.cfg").exists());
        assert!(validate_quick(&m, dir.path()).all_ok());
        assert!(validate_deep(&m, dir.path()).all_ok());
        // idempotent, and never clobbers existing content of non-empty files
        finalize_layout(&m, dir.path()).unwrap();
        assert!(validate_quick(&m, dir.path()).all_ok());
    }

    #[test]
    fn finalize_layout_truncates_overlong_files_so_repair_converges() {
        // Regression: a file that SHRANK between manifest versions keeps its
        // trailing bytes (write_at never truncates). Deep verify failed but
        // repair_plan was empty → unrepairable. finalize_layout truncates.
        let dir = tmp();
        let content = b"exactly-the-manifest-content";
        let m = single_file_manifest(content);

        // Disk has correct prefix + stale trailing bytes.
        let mut long = content.to_vec();
        long.extend_from_slice(b"STALE-TRAILING-DATA");
        fs::create_dir_all(dir.path().join("game")).unwrap();
        fs::write(dir.path().join("game/data.bin"), &long).unwrap();

        assert!(!validate_deep(&m, dir.path()).all_ok());
        assert!(
            repair_plan(&m, dir.path()).is_empty(),
            "all chunks verify — chunk repair alone cannot fix this"
        );

        finalize_layout(&m, dir.path()).unwrap();
        assert!(validate_quick(&m, dir.path()).all_ok());
        assert!(validate_deep(&m, dir.path()).all_ok());
    }

    #[test]
    fn verify_and_write_rejects_bad_chunk_no_file_written() {
        let dir = tmp();
        let data = b"good chunk bytes";
        let wrong = hash_bytes(b"something else");
        let err = verify_and_write(dir.path(), "sub/file.bin", 0, data, &wrong);
        assert!(matches!(err, Err(TransferError::HashMismatch { .. })));
        // nothing was written (HARD CONSTRAINT #1)
        assert!(!dir.path().join("sub/file.bin").exists());
    }

    #[test]
    fn verify_and_write_accepts_good_chunk() {
        let dir = tmp();
        let data = b"good chunk bytes";
        let h = hash_bytes(data);
        verify_and_write(dir.path(), "sub/file.bin", 0, data, &h).unwrap();
        let written = fs::read(dir.path().join("sub/file.bin")).unwrap();
        assert_eq!(written, data);
    }

    #[test]
    fn write_at_offsets_assemble_a_file() {
        let dir = tmp();
        write_at(dir.path(), "f.bin", 4, b"WORLD").unwrap();
        write_at(dir.path(), "f.bin", 0, b"HELO").unwrap();
        let got = fs::read(dir.path().join("f.bin")).unwrap();
        assert_eq!(&got, b"HELOWORLD");
    }

    #[test]
    fn pooled_writer_assembles_files_and_rejects_bad_chunks() {
        let dir = tmp();
        let mut w = PooledWriter::new(dir.path().to_path_buf());
        // Interleave two files through one kept-open pool, out of offset order.
        w.verify_and_write("a.bin", 4, b"WORLD", &hash_bytes(b"WORLD"))
            .unwrap();
        w.verify_and_write("b.bin", 0, b"ONE", &hash_bytes(b"ONE"))
            .unwrap();
        w.verify_and_write("a.bin", 0, b"HELO", &hash_bytes(b"HELO"))
            .unwrap();
        // A bad chunk is rejected and writes nothing (HARD CONSTRAINT #1).
        let bad = w.verify_and_write("b.bin", 3, b"XXX", &hash_bytes(b"different"));
        assert!(matches!(bad, Err(TransferError::HashMismatch { .. })));
        w.close_all();
        assert_eq!(fs::read(dir.path().join("a.bin")).unwrap(), b"HELOWORLD");
        assert_eq!(fs::read(dir.path().join("b.bin")).unwrap(), b"ONE");
    }

    #[test]
    fn pooled_writer_evicts_past_max_open() {
        let dir = tmp();
        let mut w = PooledWriter::new(dir.path().to_path_buf());
        w.max_open = 2;
        // Three files through a 2-handle pool → the first is evicted (closed) but
        // reopening it for a later chunk still assembles correctly.
        for (i, name) in ["x.bin", "y.bin", "z.bin"].iter().enumerate() {
            let b = vec![b'a' + i as u8; 3];
            w.verify_and_write(name, 0, &b, &hash_bytes(&b)).unwrap();
        }
        w.verify_and_write("x.bin", 3, b"END", &hash_bytes(b"END"))
            .unwrap();
        w.close_all();
        assert_eq!(fs::read(dir.path().join("x.bin")).unwrap(), b"aaaEND");
        assert_eq!(fs::read(dir.path().join("z.bin")).unwrap(), b"ccc");
    }

    // Build a one-file manifest from real bytes and write the file to disk.
    fn single_file_manifest(content: &[u8]) -> Manifest {
        let cs = DEFAULT_CHUNK_SIZE;
        let plans = plan_chunks(content.len() as u64, cs);
        let chunk_hashes: Vec<Hash> = plans
            .iter()
            .map(|p| hash_bytes(&content[p.offset as usize..(p.offset + p.size) as usize]))
            .collect();
        let fe = build_file_entry(
            1,
            "game/data.bin".into(),
            content.len() as u64,
            0,
            hash_bytes(content),
            cs,
            &chunk_hashes,
        );
        let mut m = Manifest {
            title_id: 1,
            manifest_ver: 1,
            total_size: 0,
            file_count: 0,
            chunk_size: cs,
            files: vec![fe],
        };
        m.recompute_totals();
        m
    }

    #[test]
    fn quick_validation_detects_states() {
        let dir = tmp();
        let content = b"hello world this is a small game file";
        let m = single_file_manifest(content);

        // missing
        let r = validate_quick(&m, dir.path());
        assert_eq!(r.files[0].detail, CheckDetail::Missing);
        assert!(!r.all_ok());

        // present, correct size
        write_at(dir.path(), "game/data.bin", 0, content).unwrap();
        let r = validate_quick(&m, dir.path());
        assert!(r.all_ok());

        // short file
        write_at(dir.path(), "game/data.bin", 0, b"short").unwrap();
        fs::write(dir.path().join("game/data.bin"), b"short").unwrap();
        let r = validate_quick(&m, dir.path());
        assert!(matches!(
            r.files[0].detail,
            CheckDetail::SizeMismatch { .. }
        ));
    }

    #[test]
    fn deep_verify_detects_corruption_and_repair_plan() {
        let dir = tmp();
        // Force >1 chunk to exercise per-chunk repair.
        let content: Vec<u8> = (0..(DEFAULT_CHUNK_SIZE + 1024))
            .map(|i| (i % 251) as u8)
            .collect();
        let m = single_file_manifest(&content);
        fs::create_dir_all(dir.path().join("game")).unwrap();
        fs::write(dir.path().join("game/data.bin"), &content).unwrap();

        // clean
        assert!(validate_deep(&m, dir.path()).all_ok());
        assert!(repair_plan(&m, dir.path()).is_empty());

        // corrupt one byte in the second chunk
        let mut corrupt = content.clone();
        let pos = (DEFAULT_CHUNK_SIZE + 10) as usize;
        corrupt[pos] ^= 0xFF;
        fs::write(dir.path().join("game/data.bin"), &corrupt).unwrap();

        let r = validate_deep(&m, dir.path());
        assert_eq!(r.files[0].detail, CheckDetail::HashMismatch);
        // only the second chunk (global idx 1) should be in the repair plan
        assert_eq!(repair_plan(&m, dir.path()), vec![1]);
    }

    #[test]
    fn completeness_check_counts_x_of_n() {
        let dir = tmp();
        let entries = vec![
            ("a.txt".to_string(), 3u64),
            ("sub/b.txt".to_string(), 5u64),
            ("missing.txt".to_string(), 1u64),
        ];
        fs::write(dir.path().join("a.txt"), b"abc").unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.txt"), b"hello").unwrap();

        let r = completeness_check(&entries, dir.path());
        assert_eq!(r.total, 3);
        assert_eq!(r.present, 2);
        assert_eq!(r.missing, vec!["missing.txt"]);
        assert!(!r.complete());
    }
}
