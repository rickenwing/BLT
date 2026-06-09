//! Fixed-size chunk planning.
//!
//! Per TDD §3.4: **4 MiB default chunk size**. Files at or below the chunk size
//! are a single chunk (no padding); large files split into fixed-size chunks
//! with a final short chunk. This gives good resume granularity and a manifest
//! that stays compact even for a 500 GB library (~128k chunks at 4 MiB) and for
//! many-tiny-file games (one chunk entry per small file).

/// Default chunk size: 4 MiB.
pub const DEFAULT_CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// The planned position of one chunk within a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkPlan {
    /// Chunk index within its file (0-based).
    pub idx: u32,
    /// Byte offset of the chunk within the file.
    pub offset: u64,
    /// Chunk length in bytes (the last chunk may be shorter).
    pub size: u64,
}

/// Plan the chunks for a file of `file_size` bytes given `chunk_size`.
///
/// - A zero-byte file yields **no chunks** (its presence + size of 0 is the
///   completeness signal; the file hash still covers it).
/// - A file `<= chunk_size` yields exactly one chunk of its real size.
/// - Otherwise, `ceil(file_size / chunk_size)` chunks, the last one short.
///
/// `chunk_size` of 0 is treated as the default to avoid a divide-by-zero from a
/// misconfigured value.
pub fn plan_chunks(file_size: u64, chunk_size: u64) -> Vec<ChunkPlan> {
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    };
    if file_size == 0 {
        return Vec::new();
    }
    let count = file_size.div_ceil(chunk_size);
    let mut plans = Vec::with_capacity(count as usize);
    let mut offset = 0u64;
    let mut idx = 0u32;
    while offset < file_size {
        let size = chunk_size.min(file_size - offset);
        plans.push(ChunkPlan { idx, offset, size });
        offset += size;
        idx += 1;
    }
    plans
}

/// Number of chunks a file of `file_size` will produce under `chunk_size`.
pub fn chunk_count(file_size: u64, chunk_size: u64) -> u64 {
    let chunk_size = if chunk_size == 0 {
        DEFAULT_CHUNK_SIZE
    } else {
        chunk_size
    };
    if file_size == 0 {
        0
    } else {
        file_size.div_ceil(chunk_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_byte_file_has_no_chunks() {
        assert!(plan_chunks(0, DEFAULT_CHUNK_SIZE).is_empty());
        assert_eq!(chunk_count(0, DEFAULT_CHUNK_SIZE), 0);
    }

    #[test]
    fn small_file_is_single_chunk_of_real_size() {
        let plans = plan_chunks(10, DEFAULT_CHUNK_SIZE);
        assert_eq!(
            plans,
            vec![ChunkPlan {
                idx: 0,
                offset: 0,
                size: 10
            }]
        );
    }

    #[test]
    fn exactly_one_chunk_boundary() {
        let plans = plan_chunks(DEFAULT_CHUNK_SIZE, DEFAULT_CHUNK_SIZE);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].size, DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn just_over_one_chunk_makes_two() {
        let plans = plan_chunks(DEFAULT_CHUNK_SIZE + 1, DEFAULT_CHUNK_SIZE);
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].size, DEFAULT_CHUNK_SIZE);
        assert_eq!(
            plans[1],
            ChunkPlan {
                idx: 1,
                offset: DEFAULT_CHUNK_SIZE,
                size: 1
            }
        );
    }

    #[test]
    fn contiguous_offsets_and_total_size() {
        let size = 10 * 1024 * 1024 + 123; // 10 MiB + 123, with 4 MiB chunks => 3 chunks
        let cs = DEFAULT_CHUNK_SIZE;
        let plans = plan_chunks(size, cs);
        assert_eq!(plans.len(), 3);
        // offsets are contiguous and cover exactly the file
        let mut expected_offset = 0;
        let mut total = 0;
        for (i, p) in plans.iter().enumerate() {
            assert_eq!(p.idx as usize, i);
            assert_eq!(p.offset, expected_offset);
            expected_offset += p.size;
            total += p.size;
        }
        assert_eq!(total, size);
        // 10 MiB mod 4 MiB = 2 MiB, so the final chunk is 2 MiB + 123.
        assert_eq!(plans.last().unwrap().size, 2 * 1024 * 1024 + 123);
        assert_eq!(chunk_count(size, cs), 3);
    }

    #[test]
    fn custom_chunk_size() {
        let plans = plan_chunks(2500, 1000);
        assert_eq!(plans.len(), 3);
        assert_eq!(plans[2].size, 500);
    }

    #[test]
    fn zero_chunk_size_falls_back_to_default() {
        // Must not panic with a divide-by-zero.
        let plans = plan_chunks(DEFAULT_CHUNK_SIZE + 1, 0);
        assert_eq!(plans.len(), 2);
    }
}
