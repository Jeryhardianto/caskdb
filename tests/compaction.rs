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
    // Encoded record size = 21 (header) + key.len() + value.len(). These
    // two fillers are 49 bytes each (8-byte key + 20-byte value), so after
    // both are written the running size is 98 — still under 100, no roll
    // yet. A third, smaller filler (30 bytes) then pushes the running size
    // to 128, past the 100-byte threshold, sealing segment 0 with all
    // three fillers in it and rolling to a fresh, empty segment 1.
    let mut db = CaskDb::open_with_options(
        dir.path(),
        Options {
            max_segment_size: 100,
        },
    )
    .unwrap();

    db.put(b"filler-1", b"aaaaaaaaaaaaaaaaaaaa").unwrap(); // 49 bytes, running 49
    db.put(b"filler-2", b"bbbbbbbbbbbbbbbbbbbb").unwrap(); // 49 bytes, running 98
    db.put(b"filler-3", b"c").unwrap(); // 30 bytes, running 128 -> rolls to segment 1

    // This key's encoded size (43 bytes) stays under 100 on the fresh
    // segment 1, so it remains genuinely in the *active* segment below.
    db.put(b"active-key", b"active-value").unwrap();
    assert_eq!(db.get(b"active-key").unwrap(), Some(b"active-value".to_vec()));

    db.compact().unwrap();

    // The active-segment key must still resolve correctly after the
    // active segment was renumbered and the reader cache was cleared —
    // compaction must never touch a key that was never in a sealed
    // segment to begin with.
    assert_eq!(db.get(b"active-key").unwrap(), Some(b"active-value".to_vec()));
    assert_eq!(db.get(b"filler-1").unwrap(), Some(b"aaaaaaaaaaaaaaaaaaaa".to_vec()));

    db.put(b"after-compact", b"still-writable").unwrap();
    assert_eq!(
        db.get(b"after-compact").unwrap(),
        Some(b"still-writable".to_vec())
    );
}
