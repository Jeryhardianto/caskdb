# caskdb Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `caskdb`, an embeddable Bitcask-style log-structured key-value
store crate with crash recovery and manual compaction, plus a tiny CLI demo.

**Architecture:** Append-only segment files on disk + an in-memory
`HashMap<Vec<u8>, IndexEntry>` pointing at (file_id, offset) for each live
key. Five modules: `record` (on-disk record encode/decode, pure), `segment`
(file I/O primitives), `index` (the index type + crash-recovery replay),
`db` (the public `CaskDb` API: open/put/get/delete/compact), and a
`src/bin/caskdb-cli.rs` REPL for interactive demos.

**Tech Stack:** Rust 2021 edition, `crc32fast` (checksum), `tempfile` (dev-dependency,
test-only). No async runtime, no web framework — this is a plain library crate.

**Spec:** `docs/superpowers/specs/2026-09-29-caskdb-design.md`

## Global Constraints

- Single-threaded: no internal locking. Callers wrap `CaskDb` in
  `Arc<Mutex<_>>` themselves if they need concurrent access.
- `put`/`delete` fsync before returning (durability policy: fsync on every write).
- Compaction is manual only (`compact()`), never a background thread.
- Keys and values are raw `&[u8]` — no generic serialization built in.
- Default `max_segment_size` is 64 MiB (`64 * 1024 * 1024` bytes exactly).
- Unix-only: value reads use `std::os::unix::fs::FileExt::read_exact_at`
  (positioned pread, no shared file cursor). This is a documented scope
  limitation, not a bug to fix later.
- No fuzzing/property-testing dependency in v1 (YAGNI) — plain `#[test]`
  scenario tests only.
- License: MIT, matching the sibling `aero-message` project's convention.
- Rust 2021 edition (already set by `cargo new`).

## Review Focus

- A `put` with an empty value (`b""`) must be retrievable as `Some(vec![])`,
  distinguished from a deleted key (`None`) by the tombstone flag, never by
  value length alone.
- `delete()` on a key that was never inserted (or already deleted) must
  return `Ok(())` and leave a later `get()` as `None` — it must not error
  just because there was nothing to remove.
- `get()`/`put()` must stay correct across a segment rollover boundary: a
  key written to an older sealed segment and a key written to the new
  active segment must both read back correctly through the per-file_id
  reader cache.
- `compact()` must preserve keys that live in the *active* segment (not
  only keys from sealed segments) once the active segment is renumbered
  and the reader cache is invalidated — this is the riskiest step in the
  whole design (a bug here was caught and fixed during planning).
- `compact()` must be a safe no-op (`Ok(())`, no panic, no renumbering)
  when there are no sealed segments yet, e.g. right after `open()` on a
  fresh database.

---

### Task 1: Foundations — error type + on-disk record encode/decode

**Files:**
- Modify: `Cargo.toml` (add `crc32fast = "1"` dependency)
- Create: `src/error.rs`
- Create: `src/record.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing (pure, no I/O, no earlier tasks).
- Produces: `crate::error::Error` (`Io(std::io::Error)`, `Corruption(String)`,
  implements `Display` + `std::error::Error`). `crate::record::{Record,
  DecodeError, HEADER_LEN}` — `Record::new`, `Record::tombstone`,
  `Record::is_tombstone`, `Record::value_offset`, `Record::encode`,
  `Record::decode`. All later tasks depend on these.

- [ ] **Step 1: Add the crc32fast dependency**

Edit `Cargo.toml`, in the `[dependencies]` section add:

```toml
crc32fast = "1"
```

- [ ] **Step 2: Write `src/error.rs`**

```rust
use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Corruption(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corruption(msg) => write!(f, "data corruption: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
```

- [ ] **Step 3: Write the failing tests in `src/record.rs`**

```rust
use crc32fast::Hasher;

/// crc(4) + timestamp(8) + flags(1) + key_len(4) + val_len(4)
pub const HEADER_LEN: usize = 4 + 8 + 1 + 4 + 4;
const TOMBSTONE_FLAG: u8 = 0b0000_0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub timestamp: u64,
    pub flags: u8,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Not enough bytes yet to read a full header, or the header's declared
    /// key_len/val_len run past the end of the supplied slice. Expected when
    /// the caller passed the tail of a segment that a crash interrupted
    /// mid-write.
    Incomplete,
    /// A full-length record was present but its CRC did not match. Expected
    /// only as real corruption — a crash-interrupted write is caught by
    /// `Incomplete` instead, since an append() that never completed leaves
    /// a short file, not a full-length-but-wrong-checksum one.
    ChecksumMismatch,
}

impl Record {
    pub fn new(key: &[u8], value: &[u8]) -> Self {
        Record {
            timestamp: now_millis(),
            flags: 0,
            key: key.to_vec(),
            value: value.to_vec(),
        }
    }

    pub fn tombstone(key: &[u8]) -> Self {
        Record {
            timestamp: now_millis(),
            flags: TOMBSTONE_FLAG,
            key: key.to_vec(),
            value: Vec::new(),
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.flags & TOMBSTONE_FLAG != 0
    }

    /// Byte offset of the value payload relative to the start of this
    /// record (i.e. past crc/timestamp/flags/key_len/val_len/key).
    pub fn value_offset(&self) -> usize {
        HEADER_LEN + self.key.len()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(8 + 1 + 4 + 4 + self.key.len() + self.value.len());
        body.extend_from_slice(&self.timestamp.to_le_bytes());
        body.push(self.flags);
        body.extend_from_slice(&(self.key.len() as u32).to_le_bytes());
        body.extend_from_slice(&(self.value.len() as u32).to_le_bytes());
        body.extend_from_slice(&self.key);
        body.extend_from_slice(&self.value);

        let mut hasher = Hasher::new();
        hasher.update(&body);
        let crc = hasher.finalize();

        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decodes exactly one record from the start of `bytes`. Returns the
    /// record and how many bytes it consumed; does not require `bytes` to
    /// end exactly at the record boundary (trailing bytes, e.g. the next
    /// record, are ignored).
    pub fn decode(bytes: &[u8]) -> Result<(Record, usize), DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::Incomplete);
        }

        let crc = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let timestamp = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        let flags = bytes[12];
        let key_len = u32::from_le_bytes(bytes[13..17].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(bytes[17..21].try_into().unwrap()) as usize;
        let total_len = HEADER_LEN + key_len + val_len;

        if bytes.len() < total_len {
            return Err(DecodeError::Incomplete);
        }

        let mut hasher = Hasher::new();
        hasher.update(&bytes[4..total_len]);
        if hasher.finalize() != crc {
            return Err(DecodeError::ChecksumMismatch);
        }

        let key = bytes[HEADER_LEN..HEADER_LEN + key_len].to_vec();
        let value = bytes[HEADER_LEN + key_len..total_len].to_vec();

        Ok((
            Record {
                timestamp,
                flags,
                key,
                value,
            },
            total_len,
        ))
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_normal_record() {
        let record = Record::new(b"key1", b"value1");
        let encoded = record.encode();
        let (decoded, consumed) = Record::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, record);
        assert!(!decoded.is_tombstone());
    }

    #[test]
    fn round_trips_a_tombstone() {
        let record = Record::tombstone(b"key1");
        let encoded = record.encode();
        let (decoded, _) = Record::decode(&encoded).unwrap();
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.value, Vec::<u8>::new());
    }

    #[test]
    fn empty_value_is_not_a_tombstone() {
        let record = Record::new(b"key1", b"");
        let encoded = record.encode();
        let (decoded, _) = Record::decode(&encoded).unwrap();
        assert!(!decoded.is_tombstone());
        assert_eq!(decoded.value, Vec::<u8>::new());
    }

    #[test]
    fn decode_reports_incomplete_on_truncated_bytes() {
        let record = Record::new(b"key1", b"value1");
        let encoded = record.encode();
        let truncated = &encoded[..encoded.len() - 2];
        assert_eq!(Record::decode(truncated), Err(DecodeError::Incomplete));
    }

    #[test]
    fn decode_reports_checksum_mismatch_on_corrupted_body() {
        let record = Record::new(b"key1", b"value1");
        let mut encoded = record.encode();
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF; // flip a bit inside the value, length fields untouched
        assert_eq!(
            Record::decode(&encoded),
            Err(DecodeError::ChecksumMismatch)
        );
    }

    #[test]
    fn decode_ignores_trailing_bytes_after_one_record() {
        let first = Record::new(b"key1", b"value1").encode();
        let second = Record::new(b"key2", b"value2").encode();
        let mut both = first.clone();
        both.extend_from_slice(&second);

        let (decoded, consumed) = Record::decode(&both).unwrap();
        assert_eq!(consumed, first.len());
        assert_eq!(decoded.key, b"key1");
    }
}
```

- [ ] **Step 4: Wire the modules into `src/lib.rs`**

```rust
//! Embeddable, log-structured (Bitcask-style) key-value store.
//!
//! See `docs/superpowers/specs/2026-09-29-caskdb-design.md` for the full
//! design rationale (on-disk format, recovery, compaction).

mod error;
mod record;

pub use error::Error;
```

- [ ] **Step 5: Run the tests and verify they pass**

Run: `cargo test --lib`
Expected: all 6 tests in `record::tests` PASS (this is new code with its
tests added together, so there is no separate red step here — go straight
to green).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/error.rs src/record.rs src/lib.rs
git commit -m "feat: add error type and on-disk record encode/decode"
```

---

### Task 2: Segment file I/O

**Files:**
- Modify: `Cargo.toml` (add `tempfile = "3"` to `[dev-dependencies]`)
- Create: `src/segment.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: nothing from Task 1 directly (this module only moves raw
  bytes; it does not parse records).
- Produces: `crate::segment::{segment_path, list_segment_ids,
  SegmentWriter, SegmentReader}`. `SegmentWriter { pub file_id: u64, pub
  size: u64 }` with `open_for_append(dir: &Path, file_id: u64) ->
  io::Result<Self>` and `append(&mut self, encoded: &[u8]) ->
  io::Result<u64>` (returns the offset the record started at, fsyncs
  before returning). `SegmentReader` with `open(dir: &Path, file_id: u64)
  -> io::Result<Self>` and `read_at(&self, offset: u64, len: u32) ->
  io::Result<Vec<u8>>`. Task 3 and Task 4 both depend on these exact
  signatures.

- [ ] **Step 1: Add the tempfile dev-dependency**

Edit `Cargo.toml`, add a `[dev-dependencies]` section (if not present):

```toml
[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write `src/segment.rs` with its tests**

```rust
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub fn segment_path(dir: &Path, file_id: u64) -> PathBuf {
    dir.join(format!("{file_id:020}.log"))
}

/// Returns every existing segment file's id, sorted ascending. Files that
/// don't match the `{file_id:020}.log` naming convention are ignored.
pub fn list_segment_ids(dir: &Path) -> io::Result<Vec<u64>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".log") {
            if let Ok(id) = stem.parse::<u64>() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

pub struct SegmentWriter {
    pub file_id: u64,
    file: File,
    pub size: u64,
}

impl SegmentWriter {
    /// Opens (creating if needed) the segment file for `file_id` in append
    /// mode. `size` is initialized from the file's current on-disk length,
    /// so reopening an existing segment after a crash-recovery truncation
    /// continues appending from the truncated point, not the pre-crash one.
    pub fn open_for_append(dir: &Path, file_id: u64) -> io::Result<Self> {
        let path = segment_path(dir, file_id);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata()?.len();
        Ok(SegmentWriter { file_id, file, size })
    }

    /// Appends an already-encoded record and fsyncs before returning. Returns
    /// the byte offset within this segment where the record started.
    pub fn append(&mut self, encoded: &[u8]) -> io::Result<u64> {
        use std::io::Write;
        let offset = self.size;
        self.file.write_all(encoded)?;
        self.file.sync_all()?;
        self.size += encoded.len() as u64;
        Ok(offset)
    }
}

pub struct SegmentReader {
    file: File,
}

impl SegmentReader {
    pub fn open(dir: &Path, file_id: u64) -> io::Result<Self> {
        let path = segment_path(dir, file_id);
        let file = File::open(&path)?;
        Ok(SegmentReader { file })
    }

    /// Reads exactly `len` bytes starting at `offset`, independent of any
    /// shared file cursor (positioned pread, safe to call from `&self`).
    pub fn read_at(&self, offset: u64, len: u32) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        self.file.read_exact_at(&mut buf, offset)?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn segment_path_zero_pads_the_file_id() {
        let dir = Path::new("/db");
        assert_eq!(
            segment_path(dir, 7),
            PathBuf::from("/db/00000000000000000007.log")
        );
    }

    #[test]
    fn list_segment_ids_returns_sorted_ids_and_ignores_other_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("00000000000000000002.log"), b"").unwrap();
        std::fs::write(dir.path().join("00000000000000000000.log"), b"").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();

        assert_eq!(list_segment_ids(dir.path()).unwrap(), vec![0, 2]);
    }

    #[test]
    fn append_then_read_at_round_trips_bytes() {
        let dir = tempdir().unwrap();
        let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();

        let first_offset = writer.append(b"hello").unwrap();
        let second_offset = writer.append(b"world!").unwrap();
        assert_eq!(first_offset, 0);
        assert_eq!(second_offset, 5);
        assert_eq!(writer.size, 11);

        let reader = SegmentReader::open(dir.path(), 0).unwrap();
        assert_eq!(reader.read_at(0, 5).unwrap(), b"hello");
        assert_eq!(reader.read_at(5, 6).unwrap(), b"world!");
    }

    #[test]
    fn reopening_an_existing_segment_continues_appending_at_its_current_size() {
        let dir = tempdir().unwrap();
        {
            let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();
            writer.append(b"first").unwrap();
        }
        let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();
        assert_eq!(writer.size, 5);
        let offset = writer.append(b"second").unwrap();
        assert_eq!(offset, 5);
    }
}
```

- [ ] **Step 3: Wire the module into `src/lib.rs`**

```rust
mod error;
mod record;
mod segment;

pub use error::Error;
```

- [ ] **Step 4: Run the tests and verify they pass**

Run: `cargo test --lib`
Expected: all `record::tests` and `segment::tests` PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/segment.rs src/lib.rs
git commit -m "feat: add segment file append/read primitives"
```

---

### Task 3: Index + crash-recovery replay

**Files:**
- Create: `src/index.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::record::{Record, DecodeError}` (Task 1),
  `crate::segment::{segment_path, list_segment_ids}` (Task 2),
  `crate::error::Error` (Task 1).
- Produces: `crate::index::{Index, IndexEntry, recover}`. `IndexEntry {
  pub file_id: u64, pub value_pos: u64, pub value_size: u32, pub
  timestamp: u64 }` (all fields public, `Clone + Copy`). `type Index =
  HashMap<Vec<u8>, IndexEntry>`. `pub fn recover(dir: &Path) ->
  Result<(Index, u64), Error>` returns the rebuilt index and the active
  (highest) segment's file_id — `0` with an empty index when `dir` has no
  segment files yet. Task 4 depends on this exact signature.

- [ ] **Step 1: Write `src/index.rs` with its tests**

```rust
use std::collections::HashMap;
use std::path::Path;

use crate::error::Error;
use crate::record::Record;
use crate::segment::{list_segment_ids, segment_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub file_id: u64,
    pub value_pos: u64,
    pub value_size: u32,
    pub timestamp: u64,
}

pub type Index = HashMap<Vec<u8>, IndexEntry>;

/// Rebuilds the in-memory index by replaying every segment file in `dir`,
/// ascending by file_id. Returns the rebuilt index and the id of the
/// active (highest file_id) segment — `0` with an empty index if `dir`
/// has no segment files yet.
///
/// A torn record (checksum mismatch, or not enough trailing bytes for the
/// declared key/value length) in the *last* segment is treated as an
/// unclean-shutdown artifact: the segment file is truncated at the start
/// of that record and replay stops there, silently. The same failure in
/// any earlier (already-sealed) segment is real corruption and returns
/// `Error::Corruption`.
pub fn recover(dir: &Path) -> Result<(Index, u64), Error> {
    let ids = list_segment_ids(dir).map_err(Error::Io)?;
    let mut index = Index::new();

    let Some(&last_id) = ids.last() else {
        return Ok((index, 0));
    };

    for &id in &ids {
        let path = segment_path(dir, id);
        let bytes = std::fs::read(&path).map_err(Error::Io)?;
        let is_last = id == last_id;
        let mut cursor = 0usize;

        while cursor < bytes.len() {
            match Record::decode(&bytes[cursor..]) {
                Ok((record, consumed)) => {
                    if record.is_tombstone() {
                        index.remove(&record.key);
                    } else {
                        let value_pos = cursor as u64 + record.value_offset() as u64;
                        index.insert(
                            record.key.clone(),
                            IndexEntry {
                                file_id: id,
                                value_pos,
                                value_size: record.value.len() as u32,
                                timestamp: record.timestamp,
                            },
                        );
                    }
                    cursor += consumed;
                }
                Err(_) if is_last => {
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .map_err(Error::Io)?;
                    file.set_len(cursor as u64).map_err(Error::Io)?;
                    break;
                }
                Err(e) => {
                    return Err(Error::Corruption(format!(
                        "segment {id} corrupt at offset {cursor}: {e:?}"
                    )));
                }
            }
        }
    }

    Ok((index, last_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegmentWriter;
    use tempfile::tempdir;

    #[test]
    fn recover_on_empty_directory_returns_empty_index_and_segment_zero() {
        let dir = tempdir().unwrap();
        let (index, active_id) = recover(dir.path()).unwrap();
        assert!(index.is_empty());
        assert_eq!(active_id, 0);
    }

    #[test]
    fn recover_replays_puts_overwrites_and_tombstones_in_order() {
        let dir = tempdir().unwrap();
        let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();
        writer
            .append(&Record::new(b"a", b"1").encode())
            .unwrap();
        writer
            .append(&Record::new(b"b", b"2").encode())
            .unwrap();
        writer
            .append(&Record::new(b"a", b"1-overwritten").encode())
            .unwrap();
        writer.append(&Record::tombstone(b"b").encode()).unwrap();

        let (index, active_id) = recover(dir.path()).unwrap();
        assert_eq!(active_id, 0);
        assert!(!index.contains_key(b"b".as_slice()));
        let a = index.get(b"a".as_slice()).unwrap();
        assert_eq!(a.value_size, b"1-overwritten".len() as u32);
    }

    #[test]
    fn recover_truncates_a_torn_tail_in_the_active_segment() {
        let dir = tempdir().unwrap();
        let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();
        writer.append(&Record::new(b"a", b"1").encode()).unwrap();
        let good_len = writer.size;

        // Simulate a crash mid-write: append a partial record (garbage,
        // shorter than any valid header) directly to the file.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(crate::segment::segment_path(dir.path(), 0))
            .unwrap()
            .write_all(&[0xAB; 10])
            .unwrap();

        let (index, active_id) = recover(dir.path()).unwrap();
        assert_eq!(active_id, 0);
        assert!(index.contains_key(b"a".as_slice()));

        let on_disk_len = std::fs::metadata(crate::segment::segment_path(dir.path(), 0))
            .unwrap()
            .len();
        assert_eq!(on_disk_len, good_len);
    }

    #[test]
    fn recover_errors_on_corruption_in_a_non_active_segment() {
        let dir = tempdir().unwrap();
        let mut writer = SegmentWriter::open_for_append(dir.path(), 0).unwrap();
        writer.append(&Record::new(b"a", b"1").encode()).unwrap();
        drop(writer);

        // Corrupt the sealed segment (id 0) after the fact.
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(crate::segment::segment_path(dir.path(), 0))
            .unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0xFF; 4]).unwrap(); // stomp the crc field

        // A second, later segment makes segment 0 non-active.
        let mut writer = SegmentWriter::open_for_append(dir.path(), 1).unwrap();
        writer.append(&Record::new(b"b", b"2").encode()).unwrap();

        let result = recover(dir.path());
        assert!(matches!(result, Err(Error::Corruption(_))));
    }
}
```

- [ ] **Step 2: Wire the module into `src/lib.rs`**

```rust
mod error;
mod index;
mod record;
mod segment;

pub use error::Error;
```

- [ ] **Step 3: Run the tests and verify they pass**

Run: `cargo test --lib`
Expected: all tests in `record::tests`, `segment::tests`, and
`index::tests` PASS.

- [ ] **Step 4: Commit**

```bash
git add src/index.rs src/lib.rs
git commit -m "feat: add index and crash-recovery replay"
```

---

### Task 4: Public `CaskDb` API — open, put, get, delete

**Files:**
- Create: `src/db.rs`
- Modify: `src/lib.rs`
- Test: `tests/basic_correctness.rs`

**Interfaces:**
- Consumes: `crate::record::Record` (Task 1), `crate::segment::{SegmentWriter,
  SegmentReader}` (Task 2), `crate::index::{recover, Index, IndexEntry}`
  (Task 3), `crate::error::Error` (Task 1).
- Produces: `pub struct Options { pub max_segment_size: u64 }` with
  `Default` (64 MiB). `pub struct CaskDb` with `open(dir: impl
  AsRef<Path>) -> Result<Self, Error>`, `open_with_options(dir: impl
  AsRef<Path>, options: Options) -> Result<Self, Error>`, `put(&mut self,
  key: &[u8], value: &[u8]) -> Result<(), Error>`, `get(&self, key: &[u8])
  -> Result<Option<Vec<u8>>, Error>`, `delete(&mut self, key: &[u8]) ->
  Result<(), Error>`, `len(&self) -> usize`, `is_empty(&self) -> bool`.
  Task 5 and Task 6 both build on this exact API.

- [ ] **Step 1: Write `src/db.rs`**

```rust
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::index::{recover, Index, IndexEntry};
use crate::record::Record;
use crate::segment::{SegmentReader, SegmentWriter};

/// Tunable knobs for a [`CaskDb`]. `Options::default()` matches the spec's
/// default of 64 MiB segments.
pub struct Options {
    pub max_segment_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            max_segment_size: 64 * 1024 * 1024,
        }
    }
}

pub struct CaskDb {
    dir: PathBuf,
    options: Options,
    index: Index,
    active: SegmentWriter,
    readers: RefCell<HashMap<u64, SegmentReader>>,
}

impl CaskDb {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_options(dir, Options::default())
    }

    pub fn open_with_options(dir: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(Error::Io)?;
        let (index, active_id) = recover(&dir)?;
        let active = SegmentWriter::open_for_append(&dir, active_id).map_err(Error::Io)?;
        Ok(CaskDb {
            dir,
            options,
            index,
            active,
            readers: RefCell::new(HashMap::new()),
        })
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let record = Record::new(key, value);
        let encoded = record.encode();
        let offset = self.active.append(&encoded).map_err(Error::Io)?;
        self.index.insert(
            key.to_vec(),
            IndexEntry {
                file_id: self.active.file_id,
                value_pos: offset + record.value_offset() as u64,
                value_size: value.len() as u32,
                timestamp: record.timestamp,
            },
        );
        self.maybe_roll_segment()?;
        Ok(())
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<(), Error> {
        let record = Record::tombstone(key);
        let encoded = record.encode();
        self.active.append(&encoded).map_err(Error::Io)?;
        self.index.remove(key);
        self.maybe_roll_segment()?;
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error> {
        let Some(entry) = self.index.get(key).copied() else {
            return Ok(None);
        };

        let mut readers = self.readers.borrow_mut();
        if !readers.contains_key(&entry.file_id) {
            let reader = SegmentReader::open(&self.dir, entry.file_id).map_err(Error::Io)?;
            readers.insert(entry.file_id, reader);
        }
        let reader = readers.get(&entry.file_id).unwrap();
        let bytes = reader
            .read_at(entry.value_pos, entry.value_size)
            .map_err(Error::Io)?;
        Ok(Some(bytes))
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn maybe_roll_segment(&mut self) -> Result<(), Error> {
        if self.active.size >= self.options.max_segment_size {
            let new_id = self.active.file_id + 1;
            self.active = SegmentWriter::open_for_append(&self.dir, new_id).map_err(Error::Io)?;
        }
        Ok(())
    }
}
```

- [ ] **Step 2: Wire the module into `src/lib.rs`**

```rust
mod db;
mod error;
mod index;
mod record;
mod segment;

pub use db::{CaskDb, Options};
pub use error::Error;
```

- [ ] **Step 3: Write the failing integration tests in `tests/basic_correctness.rs`**

```rust
use caskdb::{CaskDb, Options};
use tempfile::tempdir;

#[test]
fn put_get_overwrite_delete_survive_a_reopen() {
    let dir = tempdir().unwrap();

    {
        let mut db = CaskDb::open(dir.path()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.put(b"a", b"1-overwritten").unwrap();
        db.delete(b"b").unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1-overwritten".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), None);
    }

    // Reopen: state must have persisted to disk.
    let db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1-overwritten".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), None);
    assert_eq!(db.len(), 1);
}

#[test]
fn empty_value_is_distinct_from_a_deleted_key() {
    let dir = tempdir().unwrap();
    let mut db = CaskDb::open(dir.path()).unwrap();

    db.put(b"empty", b"").unwrap();
    assert_eq!(db.get(b"empty").unwrap(), Some(Vec::new()));

    db.delete(b"empty").unwrap();
    assert_eq!(db.get(b"empty").unwrap(), None);
}

#[test]
fn deleting_a_key_that_was_never_inserted_is_not_an_error() {
    let dir = tempdir().unwrap();
    let mut db = CaskDb::open(dir.path()).unwrap();

    db.delete(b"never-existed").unwrap();
    assert_eq!(db.get(b"never-existed").unwrap(), None);
    assert_eq!(db.len(), 0);
}

#[test]
fn get_and_put_stay_correct_across_a_segment_rollover() {
    let dir = tempdir().unwrap();
    // A tiny max size forces a rollover after just a couple of small records.
    let mut db = CaskDb::open_with_options(
        dir.path(),
        Options {
            max_segment_size: 40,
        },
    )
    .unwrap();

    db.put(b"key-in-old-segment", b"old").unwrap();
    db.put(b"key-in-old-segment-2", b"old2").unwrap();
    // By now a rollover should have happened; this key lands in a new segment.
    db.put(b"key-in-new-segment", b"new").unwrap();

    assert_eq!(
        db.get(b"key-in-old-segment").unwrap(),
        Some(b"old".to_vec())
    );
    assert_eq!(
        db.get(b"key-in-old-segment-2").unwrap(),
        Some(b"old2".to_vec())
    );
    assert_eq!(
        db.get(b"key-in-new-segment").unwrap(),
        Some(b"new".to_vec())
    );
}
```

- [ ] **Step 4: Run the tests and verify they pass**

Run: `cargo test`
Expected: all tests in `tests/basic_correctness.rs` PASS, alongside the
existing `--lib` unit tests.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs src/lib.rs tests/basic_correctness.rs
git commit -m "feat: add public CaskDb API (open/put/get/delete)"
```

---

### Task 5: Crash-recovery integration test through the public API

**Files:**
- Test: `tests/crash_recovery.rs`

**Interfaces:**
- Consumes: `caskdb::CaskDb` (Task 4). No production code changes — the
  recovery logic already exists from Task 3/4; this task adds a dedicated
  regression test that exercises it end-to-end through the public API
  rather than `index::recover` directly.
- Produces: nothing new for later tasks.

- [ ] **Step 1: Write `tests/crash_recovery.rs`**

```rust
use caskdb::CaskDb;
use std::io::Write;
use tempfile::tempdir;

/// Finds the single `.log` segment file a fresh, small database created,
/// without depending on caskdb's internal file-naming details.
fn find_segment_file(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().map(|ext| ext == "log").unwrap_or(false))
        .expect("expected exactly one segment file")
}

#[test]
fn a_torn_write_at_the_tail_is_recovered_from_and_writes_after_reopen_still_work() {
    let dir = tempdir().unwrap();

    {
        let mut db = CaskDb::open(dir.path()).unwrap();
        db.put(b"key1", b"value1").unwrap();
        db.put(b"key2", b"value2").unwrap();
        db.put(b"key3", b"value3").unwrap();
    }

    let segment_path = find_segment_file(dir.path());
    let good_len = std::fs::metadata(&segment_path).unwrap().len();

    // Simulate a crash mid-append: raw garbage shorter than a valid header,
    // appended directly to the file (bypassing the API entirely).
    std::fs::OpenOptions::new()
        .append(true)
        .open(&segment_path)
        .unwrap()
        .write_all(&[0xEE; 10])
        .unwrap();
    assert_eq!(
        std::fs::metadata(&segment_path).unwrap().len(),
        good_len + 10
    );

    // Reopening must recover the good records and discard the torn tail.
    let mut db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.get(b"key1").unwrap(), Some(b"value1".to_vec()));
    assert_eq!(db.get(b"key2").unwrap(), Some(b"value2".to_vec()));
    assert_eq!(db.get(b"key3").unwrap(), Some(b"value3".to_vec()));
    assert_eq!(std::fs::metadata(&segment_path).unwrap().len(), good_len);

    // The database must still be fully writable after recovery.
    db.put(b"key4", b"value4").unwrap();
    assert_eq!(db.get(b"key4").unwrap(), Some(b"value4".to_vec()));

    drop(db);
    let db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.get(b"key4").unwrap(), Some(b"value4".to_vec()));
}
```

- [ ] **Step 2: Run the test and verify it passes**

Run: `cargo test --test crash_recovery`
Expected: PASS. (No new production code in this task — this confirms the
recovery logic built in Task 3 actually holds up end-to-end through the
public API, including the write-after-recovery case that
`index::recover`'s own unit tests didn't exercise.)

- [ ] **Step 3: Commit**

```bash
git add tests/crash_recovery.rs
git commit -m "test: add end-to-end crash-recovery regression test"
```

---

### Task 6: Compaction

**Files:**
- Modify: `src/db.rs` (add `compact()`)
- Test: `tests/compaction.rs`

**Interfaces:**
- Consumes: `crate::segment::{list_segment_ids, segment_path,
  SegmentWriter}` (Task 2), `crate::record::Record` (Task 1),
  `crate::index::IndexEntry` (Task 3), and `CaskDb`'s private fields
  (Task 4) — this method lives directly in `src/db.rs`, in the same
  module as the `CaskDb` struct definition, specifically so it can reach
  those private fields without needing `pub(crate)` visibility changes.
- Produces: `CaskDb::compact(&mut self) -> Result<(), Error>`. Nothing
  later depends on this.

- [ ] **Step 1: Add `compact()` to `src/db.rs`**

Add these imports to the top of `src/db.rs`:

```rust
use crate::segment::{list_segment_ids, segment_path};
```

Add this method inside the existing `impl CaskDb { ... }` block (after
`maybe_roll_segment`):

```rust
    /// Merges all sealed (non-active) segments into fresh, smaller
    /// segments, dropping stale/overwritten entries and tombstones. See
    /// the design spec's "Compaction" section for why the active segment
    /// gets renumbered above the compacted output rather than the other
    /// way around.
    pub fn compact(&mut self) -> Result<(), Error> {
        let old_active_id = self.active.file_id;
        let mut sealed_ids: Vec<u64> = list_segment_ids(&self.dir)
            .map_err(Error::Io)?
            .into_iter()
            .filter(|id| *id != old_active_id)
            .collect();
        sealed_ids.sort_unstable();
        if sealed_ids.is_empty() {
            return Ok(());
        }

        // 1. Replay sealed segments, keeping only the latest surviving
        //    (non-tombstone) encoded record per key.
        let mut survivors: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
        for id in &sealed_ids {
            let bytes = std::fs::read(segment_path(&self.dir, *id)).map_err(Error::Io)?;
            let mut cursor = 0usize;
            while cursor < bytes.len() {
                let (record, consumed) = Record::decode(&bytes[cursor..]).map_err(|e| {
                    Error::Corruption(format!(
                        "segment {id} corrupt at offset {cursor} during compaction: {e:?}"
                    ))
                })?;
                if record.is_tombstone() {
                    survivors.remove(&record.key);
                } else {
                    survivors.insert(
                        record.key.clone(),
                        bytes[cursor..cursor + consumed].to_vec(),
                    );
                }
                cursor += consumed;
            }
        }

        // 2. Write survivors into fresh segment(s), numbered starting just
        //    above the current active id so they can never collide with an
        //    existing sealed or active segment file.
        let mut next_id = old_active_id;
        let mut writer: Option<SegmentWriter> = None;
        for (key, encoded) in &survivors {
            let needs_new = match &writer {
                None => true,
                Some(w) => {
                    w.size > 0 && w.size + encoded.len() as u64 > self.options.max_segment_size
                }
            };
            if needs_new {
                next_id += 1;
                writer = Some(SegmentWriter::open_for_append(&self.dir, next_id).map_err(Error::Io)?);
            }
            let w = writer.as_mut().expect("writer just ensured present");
            let offset = w.append(encoded).map_err(Error::Io)?;
            let (record, _) =
                Record::decode(encoded).expect("survivor record already validated above");
            self.index.insert(
                key.clone(),
                IndexEntry {
                    file_id: w.file_id,
                    value_pos: offset + record.value_offset() as u64,
                    value_size: record.value.len() as u32,
                    timestamp: record.timestamp,
                },
            );
        }

        // 3. Renumber the active segment above every id just written, so
        //    file_id ordering still reflects recency after this
        //    compaction (see spec: without this, a future `open()` would
        //    replay the compacted output as if it were newer than the
        //    active segment's real, more recent writes).
        let new_active_id = next_id + 1;
        std::fs::rename(
            segment_path(&self.dir, old_active_id),
            segment_path(&self.dir, new_active_id),
        )
        .map_err(Error::Io)?;
        self.active = SegmentWriter::open_for_append(&self.dir, new_active_id).map_err(Error::Io)?;

        for entry in self.index.values_mut() {
            if entry.file_id == old_active_id {
                entry.file_id = new_active_id;
            }
        }

        // 4. Any cached reader may point at a file_id whose backing file
        //    just moved or disappeared; drop them all and let get() reopen
        //    lazily by the (now-correct) index entries.
        self.readers.borrow_mut().clear();

        // 5. The old sealed segments are now fully superseded.
        for id in sealed_ids {
            std::fs::remove_file(segment_path(&self.dir, id)).map_err(Error::Io)?;
        }

        Ok(())
    }
```

- [ ] **Step 2: Write the failing tests in `tests/compaction.rs`**

```rust
use caskdb::{CaskDb, Options};
use tempfile::tempdir;

#[test]
fn compact_is_a_safe_no_op_when_nothing_is_sealed_yet() {
    let dir = tempdir().unwrap();
    let mut db = CaskDb::open(dir.path()).unwrap();
    db.put(b"a", b"1").unwrap();

    db.compact().unwrap();

    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn compact_drops_overwritten_and_deleted_keys_but_keeps_final_state() {
    let dir = tempdir().unwrap();
    // Tiny segments so a handful of puts force a rollover, sealing segment 0.
    let mut db = CaskDb::open_with_options(
        dir.path(),
        Options {
            max_segment_size: 40,
        },
    )
    .unwrap();

    db.put(b"a", b"1").unwrap();
    db.put(b"b", b"2").unwrap();
    db.put(b"a", b"1-final").unwrap(); // overwrite, forces rollover by here
    db.put(b"c", b"3").unwrap();
    db.delete(b"b").unwrap();

    let before_compact = (
        db.get(b"a").unwrap(),
        db.get(b"b").unwrap(),
        db.get(b"c").unwrap(),
    );

    let sealed_before = std::fs::read_dir(dir.path()).unwrap().count();
    db.compact().unwrap();
    let sealed_after = std::fs::read_dir(dir.path()).unwrap().count();

    assert_eq!(
        (db.get(b"a").unwrap(), db.get(b"b").unwrap(), db.get(b"c").unwrap()),
        before_compact
    );
    assert_eq!(before_compact.0, Some(b"1-final".to_vec()));
    assert_eq!(before_compact.1, None);
    assert!(
        sealed_after <= sealed_before,
        "compaction should not increase the number of files on disk"
    );

    // Must still be correct and writable after reopening post-compaction.
    drop(db);
    let mut db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1-final".to_vec()));
    assert_eq!(db.get(b"c").unwrap(), Some(b"3".to_vec()));
    db.put(b"d", b"4").unwrap();
    assert_eq!(db.get(b"d").unwrap(), Some(b"4".to_vec()));
}

#[test]
fn compact_preserves_keys_that_live_in_the_active_segment() {
    let dir = tempdir().unwrap();
    let mut db = CaskDb::open_with_options(
        dir.path(),
        Options {
            max_segment_size: 40,
        },
    )
    .unwrap();

    // Force at least one rollover so there is a sealed segment to compact...
    db.put(b"sealed-key", b"sealed-value-long-enough-to-roll").unwrap();
    db.put(b"another", b"to-force-rollover").unwrap();

    // ...then write a key that stays in the still-open active segment.
    db.put(b"active-key", b"active-value").unwrap();
    let active_value_before = db.get(b"active-key").unwrap();
    assert_eq!(active_value_before, Some(b"active-value".to_vec()));

    db.compact().unwrap();

    // The active-segment key must still resolve correctly after the
    // active segment was renumbered and the reader cache was cleared.
    assert_eq!(db.get(b"active-key").unwrap(), Some(b"active-value".to_vec()));

    db.put(b"after-compact", b"still-writable").unwrap();
    assert_eq!(
        db.get(b"after-compact").unwrap(),
        Some(b"still-writable".to_vec())
    );
}
```

- [ ] **Step 3: Run the tests and verify they pass**

Run: `cargo test --test compaction`
Expected: all 3 tests PASS.

- [ ] **Step 4: Run the full test suite**

Run: `cargo test`
Expected: every test across `--lib`, `basic_correctness`,
`crash_recovery`, and `compaction` PASSES.

- [ ] **Step 5: Commit**

```bash
git add src/db.rs tests/compaction.rs
git commit -m "feat: add manual compaction"
```

---

### Task 7: CLI demo binary

**Files:**
- Create: `src/bin/caskdb-cli.rs`

**Interfaces:**
- Consumes: `caskdb::{CaskDb, Options}` (public crate API from Task 4/6).
- Produces: nothing later depends on this; it's a standalone demo binary.

- [ ] **Step 1: Write `src/bin/caskdb-cli.rs` with its test**

```rust
use caskdb::CaskDb;
use std::io::{self, BufRead, Write};

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Put(Vec<u8>, Vec<u8>),
    Get(Vec<u8>),
    Delete(Vec<u8>),
    Compact,
    Exit,
    Unknown(String),
}

/// Parses one REPL line. Pure and unit-tested on its own; the REPL loop
/// itself is thin I/O glue around this.
fn parse_command(line: &str) -> Command {
    let mut parts = line.trim().splitn(3, ' ');
    match parts.next().unwrap_or("") {
        "put" => match (parts.next(), parts.next()) {
            (Some(key), Some(value)) => {
                Command::Put(key.as_bytes().to_vec(), value.as_bytes().to_vec())
            }
            _ => Command::Unknown(line.to_string()),
        },
        "get" => match parts.next() {
            Some(key) => Command::Get(key.as_bytes().to_vec()),
            None => Command::Unknown(line.to_string()),
        },
        "delete" => match parts.next() {
            Some(key) => Command::Delete(key.as_bytes().to_vec()),
            None => Command::Unknown(line.to_string()),
        },
        "compact" => Command::Compact,
        "exit" => Command::Exit,
        _ => Command::Unknown(line.to_string()),
    }
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "./caskdb-data".to_string());
    let mut db = CaskDb::open(&dir).expect("failed to open database");
    println!("caskdb REPL — data dir: {dir}");
    println!("commands: put <key> <value> | get <key> | delete <key> | compact | exit");

    let stdin = io::stdin();
    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap() == 0 {
            break; // EOF
        }

        match parse_command(&line) {
            Command::Put(key, value) => match db.put(&key, &value) {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Get(key) => match db.get(&key) {
                Ok(Some(value)) => println!("{}", String::from_utf8_lossy(&value)),
                Ok(None) => println!("(nil)"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Delete(key) => match db.delete(&key) {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Compact => match db.compact() {
                Ok(()) => println!("OK"),
                Err(e) => println!("ERR {e}"),
            },
            Command::Exit => break,
            Command::Unknown(line) => println!("unrecognized command: {line}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_put_get_delete_compact_exit() {
        assert_eq!(
            parse_command("put mykey myvalue"),
            Command::Put(b"mykey".to_vec(), b"myvalue".to_vec())
        );
        assert_eq!(parse_command("get mykey"), Command::Get(b"mykey".to_vec()));
        assert_eq!(
            parse_command("delete mykey"),
            Command::Delete(b"mykey".to_vec())
        );
        assert_eq!(parse_command("compact"), Command::Compact);
        assert_eq!(parse_command("exit"), Command::Exit);
    }

    #[test]
    fn put_value_may_contain_spaces() {
        assert_eq!(
            parse_command("put mykey hello world"),
            Command::Put(b"mykey".to_vec(), b"hello world".to_vec())
        );
    }

    #[test]
    fn missing_arguments_are_unknown() {
        assert_eq!(
            parse_command("put onlykey"),
            Command::Unknown("put onlykey".to_string())
        );
        assert_eq!(
            parse_command("banana"),
            Command::Unknown("banana".to_string())
        );
    }
}
```

- [ ] **Step 2: Run the tests and verify they pass**

Run: `cargo test --bin caskdb-cli`
Expected: all 3 tests PASS.

- [ ] **Step 3: Build the binary to confirm it compiles cleanly**

Run: `cargo build --release`
Expected: builds with no errors or warnings.

- [ ] **Step 4: Commit**

```bash
git add src/bin/caskdb-cli.rs
git commit -m "feat: add caskdb-cli demo REPL"
```

---

### Task 8: README, LICENSE, and crate metadata

**Files:**
- Create: `LICENSE`
- Modify: `Cargo.toml` (package metadata)
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing (documentation only).
- Produces: nothing later depends on this — final task.

- [ ] **Step 1: Add `LICENSE`**

```
MIT License

Copyright (c) 2026 Jery Hardianto

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

- [ ] **Step 2: Add package metadata to `Cargo.toml`**

Edit the `[package]` section to read:

```toml
[package]
name = "caskdb"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Embeddable, log-structured (Bitcask-style) key-value store with crash recovery and manual compaction"
```

- [ ] **Step 3: Write `README.md`**

```markdown
# caskdb

An embeddable key-value store in Rust. Log-structured, Bitcask-style: every
write is appended to a segment file, an in-memory hash index maps each key
to its (segment, offset), and reads are a single positioned read — no
scans, no tree traversal.

Single-threaded, fsyncs on every write, crash-safe by construction (an
unclean shutdown mid-write leaves at most one torn record, discarded on the
next open), with manual compaction to reclaim space from overwritten and
deleted keys.

Built and tested with Rust 1.98. Unix-only (uses positioned reads via
`std::os::unix::fs::FileExt`).

## Use it as a library

```rust
use caskdb::CaskDb;

let mut db = CaskDb::open("./data")?;
db.put(b"key", b"value")?;
assert_eq!(db.get(b"key")?, Some(b"value".to_vec()));
db.delete(b"key")?;
db.compact()?; // reclaim space from old/deleted entries
```

## Try the CLI demo

```sh
cargo run --release --bin caskdb-cli -- ./data
```

```
caskdb REPL — data dir: ./data
commands: put <key> <value> | get <key> | delete <key> | compact | exit
> put name budi
OK
> get name
budi
> delete name
OK
> get name
(nil)
> exit
```

## Design

See `docs/superpowers/specs/2026-09-29-caskdb-design.md` for the full
design: on-disk record format, recovery algorithm, and how compaction
renumbers the active segment to keep file_id ordering meaningful.

## Known limitations

- No concurrent access without external locking (wrap in `Arc<Mutex<_>>`
  if you need it from multiple threads).
- No automatic/background compaction — call `compact()` yourself.
- No range scans or ordered iteration.
- Compaction's file-swap isn't atomic across a crash: if the process dies
  mid-compaction, the next `open()` may see both old and new segments,
  which replay correctly (last-writer-wins by file_id) but won't have
  reclaimed space until `compact()` is run again.
```

- [ ] **Step 4: Run the full test suite one more time**

Run: `cargo test`
Expected: every test passes — this is the final verification before
calling the crate done.

- [ ] **Step 5: Commit**

```bash
git add LICENSE Cargo.toml README.md
git commit -m "docs: add README, LICENSE, and crate metadata"
```
