//! BLAKE3 hashing helpers and the [`Hash`] newtype used throughout BLT.
//!
//! A [`Hash`] is a 32-byte BLAKE3 digest that serialises to/from a lowercase
//! hex string, so manifests, the wire protocol, and SQLite all agree on one
//! representation. **No chunk is ever written to disk unverified** (HARD
//! CONSTRAINT #1); every chunk hash on the wire is checked against the manifest
//! using these helpers before acceptance.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Length of a BLAKE3 digest in bytes.
pub const HASH_LEN: usize = 32;

/// A 32-byte BLAKE3 content hash.
///
/// Serialises as a 64-char lowercase hex string (JSON/TOML friendly) and stores
/// in SQLite as either the 32 raw bytes (`BLOB`) or the hex `TEXT` — both round
/// trips are provided.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; HASH_LEN]);

impl Hash {
    /// Construct from raw bytes.
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Hash(bytes)
    }

    /// The raw 32 bytes (e.g. for a SQLite `BLOB`).
    pub fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// Lowercase hex encoding (64 chars).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from a hex string (case-insensitive, must be exactly 64 chars).
    pub fn from_hex(s: &str) -> Result<Self, HashParseError> {
        let bytes = hex::decode(s).map_err(|_| HashParseError::NotHex)?;
        let arr: [u8; HASH_LEN] = bytes.try_into().map_err(|_| HashParseError::WrongLength)?;
        Ok(Hash(arr))
    }

    /// Parse from a raw byte slice (must be exactly 32 bytes), e.g. a SQLite BLOB.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, HashParseError> {
        let arr: [u8; HASH_LEN] = bytes.try_into().map_err(|_| HashParseError::WrongLength)?;
        Ok(Hash(arr))
    }
}

/// Error parsing a [`Hash`] from text or bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HashParseError {
    #[error("hash string is not valid hex")]
    NotHex,
    #[error("hash has the wrong length (expected 32 bytes / 64 hex chars)")]
    WrongLength,
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl From<blake3::Hash> for Hash {
    fn from(h: blake3::Hash) -> Self {
        Hash(*h.as_bytes())
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Hash a byte slice (used for chunk verification on arrival).
pub fn hash_bytes(data: &[u8]) -> Hash {
    blake3::hash(data).into()
}

/// Streaming hasher for whole files (deep verify / scan). Wraps [`blake3::Hasher`].
#[derive(Default)]
pub struct StreamHasher(blake3::Hasher);

impl StreamHasher {
    pub fn new() -> Self {
        StreamHasher(blake3::Hasher::new())
    }

    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finalize(&self) -> Hash {
        self.0.finalize().into()
    }
}

/// Verify that `data` hashes to `expected`. The single chokepoint behind
/// HARD CONSTRAINT #1 — call this before writing any received chunk.
#[must_use]
pub fn verify(data: &[u8], expected: &Hash) -> bool {
    &hash_bytes(data) == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_blake3_empty_vector() {
        // BLAKE3 of the empty input (official test vector).
        let h = hash_bytes(b"");
        assert_eq!(
            h.to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn hex_roundtrip() {
        let h = hash_bytes(b"buttz lan tool");
        let s = h.to_hex();
        assert_eq!(s.len(), 64);
        assert_eq!(Hash::from_hex(&s).unwrap(), h);
        // case-insensitive parse
        assert_eq!(Hash::from_hex(&s.to_uppercase()).unwrap(), h);
    }

    #[test]
    fn bytes_roundtrip() {
        let h = hash_bytes(b"abc");
        assert_eq!(Hash::from_slice(h.as_bytes()).unwrap(), h);
    }

    #[test]
    fn serde_is_hex_string() {
        let h = hash_bytes(b"abc");
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{}\"", h.to_hex()));
        let back: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Hash::from_hex("zz"), Err(HashParseError::NotHex));
        assert_eq!(Hash::from_hex("abcd"), Err(HashParseError::WrongLength));
        assert_eq!(
            Hash::from_slice(&[0u8; 31]),
            Err(HashParseError::WrongLength)
        );
    }

    #[test]
    fn verify_accepts_good_rejects_bad() {
        let data = b"a chunk of game data";
        let good = hash_bytes(data);
        assert!(verify(data, &good));
        let bad = hash_bytes(b"different data");
        assert!(!verify(data, &bad));
    }

    #[test]
    fn stream_matches_oneshot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut s = StreamHasher::new();
        s.update(&data[..10]);
        s.update(&data[10..]);
        assert_eq!(s.finalize(), hash_bytes(data));
    }
}
