//! The resume bitmap: one bit per chunk, `have` / `have-not`.
//!
//! This is the heart of resume (TDD §4.2). On resume the downloader fetches
//! only the zero-bits; on a verified chunk it sets the bit and the bitmap is
//! persisted to the client SQLite (`download_state.chunk_bitmap`). Bit order is
//! the manifest's global chunk order ([`crate::manifest::Manifest::chunk_locators`]).

use serde::{Deserialize, Serialize};

/// A fixed-length bit set sized to a title's chunk count.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bitmap {
    /// Number of meaningful bits (== chunk count). Bytes may hold trailing
    /// padding bits which are always kept zero.
    len: u64,
    bytes: Vec<u8>,
}

impl Bitmap {
    /// A new all-zero bitmap for `len` chunks.
    pub fn new(len: u64) -> Self {
        let nbytes = len.div_ceil(8) as usize;
        Bitmap {
            len,
            bytes: vec![0u8; nbytes],
        }
    }

    /// Reconstruct from persisted bytes and a known length (from SQLite).
    /// Trailing padding bits beyond `len` are forced to zero defensively.
    pub fn from_bytes(len: u64, mut bytes: Vec<u8>) -> Self {
        let nbytes = len.div_ceil(8) as usize;
        bytes.resize(nbytes, 0);
        let mut bm = Bitmap { len, bytes };
        bm.clear_padding();
        bm
    }

    /// Raw bytes for persistence.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of chunks this bitmap tracks.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether chunk `i` is present. Out-of-range indices read as `false`.
    pub fn has(&self, i: u64) -> bool {
        if i >= self.len {
            return false;
        }
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        (self.bytes[byte] >> bit) & 1 == 1
    }

    /// Mark chunk `i` present. Out-of-range indices are ignored.
    pub fn set(&mut self, i: u64) {
        if i >= self.len {
            return;
        }
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        self.bytes[byte] |= 1 << bit;
    }

    /// Mark chunk `i` absent (e.g. a deep-verify mismatch → refetch).
    pub fn clear(&mut self, i: u64) {
        if i >= self.len {
            return;
        }
        let byte = (i / 8) as usize;
        let bit = (i % 8) as u8;
        self.bytes[byte] &= !(1 << bit);
    }

    /// Count of present chunks.
    pub fn count_set(&self) -> u64 {
        self.bytes.iter().map(|b| b.count_ones() as u64).sum()
    }

    /// All chunks present?
    pub fn is_complete(&self) -> bool {
        self.count_set() == self.len
    }

    /// Iterator over the indices of missing (zero) chunks — the resume work list.
    pub fn missing(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len).filter(move |&i| !self.has(i))
    }

    /// Fraction complete in [0.0, 1.0]; 1.0 for an empty (zero-chunk) title.
    pub fn fraction(&self) -> f64 {
        if self.len == 0 {
            return 1.0;
        }
        self.count_set() as f64 / self.len as f64
    }

    fn clear_padding(&mut self) {
        let used = (self.len % 8) as u8;
        if used != 0 {
            if let Some(last) = self.bytes.last_mut() {
                let mask = (1u16 << used) as u8;
                *last &= mask.wrapping_sub(1);
            }
        }
    }
}

impl std::fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitmap({}/{} chunks)", self.count_set(), self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_all_zero() {
        let bm = Bitmap::new(20);
        assert_eq!(bm.len(), 20);
        assert_eq!(bm.count_set(), 0);
        assert!(!bm.is_complete());
        assert!(!bm.has(0));
        assert_eq!(bm.missing().count(), 20);
    }

    #[test]
    fn set_has_clear() {
        let mut bm = Bitmap::new(10);
        bm.set(3);
        bm.set(9);
        assert!(bm.has(3));
        assert!(bm.has(9));
        assert!(!bm.has(4));
        assert_eq!(bm.count_set(), 2);
        bm.clear(3);
        assert!(!bm.has(3));
        assert_eq!(bm.count_set(), 1);
    }

    #[test]
    fn missing_lists_zero_bits() {
        let mut bm = Bitmap::new(5);
        bm.set(1);
        bm.set(3);
        let missing: Vec<u64> = bm.missing().collect();
        assert_eq!(missing, vec![0, 2, 4]);
    }

    #[test]
    fn complete_detection() {
        let mut bm = Bitmap::new(3);
        bm.set(0);
        bm.set(1);
        bm.set(2);
        assert!(bm.is_complete());
        assert_eq!(bm.fraction(), 1.0);
    }

    #[test]
    fn empty_bitmap_is_complete() {
        let bm = Bitmap::new(0);
        assert!(bm.is_complete());
        assert_eq!(bm.fraction(), 1.0);
        assert!(bm.is_empty());
    }

    #[test]
    fn persistence_roundtrip() {
        let mut bm = Bitmap::new(19); // not a byte multiple
        for i in [0u64, 5, 7, 8, 18] {
            bm.set(i);
        }
        let bytes = bm.as_bytes().to_vec();
        let restored = Bitmap::from_bytes(19, bytes);
        assert_eq!(restored, bm);
        for i in 0..19 {
            assert_eq!(restored.has(i), bm.has(i));
        }
    }

    #[test]
    fn from_bytes_clears_padding_bits() {
        // 3 bits meaningful; supply a byte with high bits set — they must be ignored.
        let bm = Bitmap::from_bytes(3, vec![0b1111_1111]);
        assert_eq!(bm.count_set(), 3);
        assert!(bm.is_complete());
        // padding cleared so it equals a freshly-built full bitmap
        let mut fresh = Bitmap::new(3);
        fresh.set(0);
        fresh.set(1);
        fresh.set(2);
        assert_eq!(bm, fresh);
    }

    #[test]
    fn out_of_range_is_safe() {
        let mut bm = Bitmap::new(4);
        bm.set(100); // ignored
        assert!(!bm.has(100));
        assert_eq!(bm.count_set(), 0);
    }

    #[test]
    fn fraction_partial() {
        let mut bm = Bitmap::new(4);
        bm.set(0);
        assert_eq!(bm.fraction(), 0.25);
    }
}
