//! Embeddable, log-structured (Bitcask-style) key-value store.
//!
//! See `docs/superpowers/specs/2026-09-29-caskdb-design.md` for the full
//! design rationale (on-disk format, recovery, compaction).

mod error;
mod index;
mod record;
mod segment;

pub use error::Error;
