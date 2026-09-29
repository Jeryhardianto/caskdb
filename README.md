# caskdb

An embeddable key-value store in Rust. Log-structured, Bitcask-style: every
write is appended to a segment file, an in-memory hash index maps each key
to its (segment, offset), and reads are a single positioned read: no
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
- No automatic/background compaction. Call `compact()` yourself.
- No range scans or ordered iteration.
- Compaction's file-swap isn't atomic across a crash: if the process dies
  mid-compaction, the next `open()` may see both old and new segments,
  which replay correctly (last-writer-wins by file_id) but won't have
  reclaimed space until `compact()` is run again.
