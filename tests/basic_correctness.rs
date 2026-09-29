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
