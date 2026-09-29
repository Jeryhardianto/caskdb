# caskdb design spec

Date: 2026-09-29

## Purpose

Portfolio project demonstrating Rust systems-level programming: ownership over
mutable on-disk data structures, durability, and crash recovery. Complements two
existing networking-focused projects (aero-message, waxum) with a storage-engine
piece aimed at backend/infra hiring reviewers.

## Scope

Embeddable key-value store library, Bitcask-style (log-structured, append-only
segments + in-memory hash index). Single-threaded (no internal locking — callers
wrap with `Arc<Mutex<_>>` themselves if needed). fsync on every write. Compaction
is in scope, triggered manually via `compact()`, not a background thread.

Out of scope: concurrent access without external locking, range scans /
iteration order guarantees, generic serialization (keys/values are raw bytes),
automatic/background compaction, network server layer (a thin CLI binary is
included only for interactive demo, not a protocol server).

## On-disk record format

Every record in a segment file:

```
[crc32: 4B][timestamp: 8B][flags: 1B][key_len: 4B][val_len: 4B][key][value]
```

- `crc32` covers every field after itself (timestamp..value). Used to detect
  torn writes during recovery.
- `flags` bit 0 = tombstone (delete marker). A tombstone record has
  `val_len == 0` and no value bytes, but is distinguished from a real empty
  value (`val_len == 0`, flags bit 0 unset) by the flag, not the length.
- Multi-byte integers are little-endian.

Segments: one active (writable) segment at a time, identified by a
monotonically increasing `file_id` (`u64`). When the active segment exceeds a
configured max size (default 64 MiB), it is sealed (becomes immutable) and a
new active segment is opened with the next `file_id`. File naming:
`{file_id:020}.log` in the database directory.

## In-memory index

```rust
struct IndexEntry {
    file_id: u64,
    value_pos: u64,
    value_size: u32,
    timestamp: u64,
}

type Index = HashMap<Vec<u8>, IndexEntry>;
```

A tombstone removes the key from the index (not stored as an index entry) —
the index only ever reflects live keys. `value_pos` is the byte offset of the
**value payload itself** (i.e. past crc/timestamp/flags/key_len/val_len/key),
so `get` is exactly one `pread(value_pos, value_size)` with no header
re-parsing and no CRC re-check on the hot path — CRC is only verified during
the sequential replay in recovery and compaction, matching how Bitcask's
KeyDir/hint-file lookup works.

## Recovery (on `open`)

1. List all segment files in the directory, sorted by `file_id` ascending.
2. Replay each segment from offset 0: read a record, verify CRC, apply to the
   index (insert/overwrite for a normal record, remove for a tombstone).
3. If a record fails CRC or the file ends mid-record (not enough bytes left
   for the declared `key_len`/`val_len`) **and this is the last segment**
   (highest `file_id`, i.e. the one that was active when the process
   stopped): treat it as a torn write from an unclean shutdown. Truncate the
   file at the start of that broken record and stop replaying this segment.
4. If the same failure occurs in a segment that is *not* the last one:
   propagate a hard `Error::Corruption` — a sealed segment should never have
   a torn tail; a CRC failure there indicates real corruption, not a crash
   mid-write.
5. The highest `file_id` found becomes the active segment (opened for
   append); if no segments exist, create `file_id = 0`.

## Compaction

`compact()`:

1. Identify all sealed (non-active) segments.
2. Replay them in `file_id` order into a temporary `HashMap<Vec<u8>, (record
   bytes, is_tombstone)>`, keeping only the latest record per key (later
   `file_id`/offset wins).
3. Write every surviving **non-tombstone** entry into one or more new
   compacted segment files (respecting the same max-size-per-segment rule),
   with fresh `file_id`s allocated above the current active segment's id.
4. Rebuild the in-memory index entries for the relocated keys to point at the
   new segment files.
5. Delete the old sealed segment files.
6. The active segment is never touched by compaction, so keys written after
   compaction started are unaffected and remain correctly the "latest"
   version (their `file_id` is higher than any compacted segment's).

Compaction is not crash-safe across step 3/4/5 boundary for v1 (if the
process dies mid-compaction, on next `open()` both old and new segments may
be replayed — this is safe because replay is idempotent and last-writer-wins
by `file_id`/offset, it just doesn't reclaim space until compaction is
re-run). This tradeoff is called out in the README as a known limitation.

## Public API

```rust
pub struct CaskDb { /* ... */ }

impl CaskDb {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, Error>;
    pub fn open_with_options(dir: impl AsRef<Path>, opts: Options) -> Result<Self, Error>;
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error>;
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Error>;
    pub fn delete(&mut self, key: &[u8]) -> Result<(), Error>;
    pub fn compact(&mut self) -> Result<(), Error>;
    pub fn len(&self) -> usize;   // number of live keys
    pub fn is_empty(&self) -> bool;
}

pub struct Options {
    pub max_segment_size: u64, // default 64 MiB
}

pub enum Error {
    Io(std::io::Error),
    Corruption(String),
}
```

`put`/`delete` fsync before returning (per chosen durability policy: fsync on
every write). `get` never touches disk metadata beyond a `pread` at a known
offset — no scan.

A thin `src/bin/caskdb-cli.rs` binary wraps this API with a REPL (`put`,
`get`, `delete`, `compact`, `exit`) purely for demoing in a terminal
recording; it is not a network server and has no protocol.

## Error handling

- I/O failures (disk full, permission denied, etc.) propagate as
  `Error::Io`.
- Corruption detected in a non-tail record propagates as
  `Error::Corruption` and `open()` fails — the caller must not silently
  lose data by treating mid-file corruption as recoverable.
- A torn tail record in the active segment is *not* an error — it is
  expected crash-recovery behavior, handled silently by truncation (logged
  at `warn` level via `tracing` if desired, but not surfaced as an `Err`).

## Testing

Per-scenario `#[test]` functions (no new test-framework dependency):

1. **Basic correctness**: put/get/overwrite/delete/reopen — verify state
   after closing and reopening the database matches expectations.
2. **Crash recovery**: write several records to a segment file directly (or
   via the API), truncate the file mid-record to simulate a crash, reopen,
   assert records before the truncation point survive and the partial one
   is gone, and that new writes after reopening still work correctly.
3. **Compaction correctness**: perform overwrites and deletes across
   multiple sealed segments, call `compact()`, verify final key/value state
   is unchanged from before compaction, and verify segment file count/total
   size decreases.

No fuzzing/property-testing dependency added initially (YAGNI) — noted in
the README as an optional future extension.

## Non-goals / future work (README-documented)

- Concurrent access (multi-reader/writer) without external locking.
- Automatic/background compaction triggering.
- Range scans / ordered iteration.
- Crash-safety of compaction's file-swap step (currently: safe but
  non-atomic, may leave stale files needing a re-run).
