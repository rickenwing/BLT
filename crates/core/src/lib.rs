//! # blt-core — Buttz LAN Tool shared core
//!
//! The compiled heart of BLT, consumed by both the server (`blt-server`) and the
//! Tauri desktop app (`blt`). Everything correctness-critical and CPU-bound lives
//! here so it runs off the UI thread and can be tested exhaustively without a
//! network (CLAUDE.md "Testing").
//!
//! Modules:
//! - [`hashing`] — BLAKE3 + the [`Hash`](hashing::Hash) newtype; the verify
//!   chokepoint behind HARD CONSTRAINT #1.
//! - [`chunking`] — 4 MiB fixed-chunk planning (TDD §3.4).
//! - [`manifest`] — the structural title manifest + diff (HARD CONSTRAINT #10).
//! - [`bitmap`] — the resume bitmap (TDD §4.2).
//! - [`transfer`] — verify-before-write + quick/deep validation + completeness.
//! - [`p2p`] — token-bucket rate cap, EWMA throughput, weighted scheduler (F13).
//! - [`jukebox`] — Fair Rotation / Vote-Ranked ordering (F8.5).
//! - [`pathsafe`] — cross-platform path sanitisation (HARD CONSTRAINT #11).
//! - [`protocol`] — HTTP + WebSocket wire types shared with both front-ends.
//! - [`discovery`] — mDNS TXT contract (`IP:port`, not `.local`; #9).

pub mod bitmap;
pub mod chunking;
pub mod discovery;
pub mod hashing;
pub mod jukebox;
pub mod manifest;
pub mod p2p;
pub mod pathsafe;
pub mod protocol;
pub mod ratemeter;
pub mod transfer;

/// Shared app plumbing (data-root + logging); enabled by both binaries.
#[cfg(feature = "runtime")]
pub mod runtime;

/// The wire/protocol version. Bump on a breaking protocol change so a client and
/// server can refuse to talk across incompatible versions.
pub const PROTOCOL_VERSION: u32 = 1;

/// Machine-facing identifier used in folders, service ids, etc.
pub const APP_ID: &str = "BLT";

/// Friendly product name.
pub const APP_NAME: &str = "Buttz LAN Tool";

pub use bitmap::Bitmap;
pub use chunking::{ChunkPlan, DEFAULT_CHUNK_SIZE, plan_chunks};
pub use hashing::{Hash, StreamHasher, hash_bytes, verify};
pub use manifest::{ChunkLocator, FileEntry, Manifest, ManifestDiff, diff};
pub use protocol::{ClientMsg, Mode, ServerMsg};
