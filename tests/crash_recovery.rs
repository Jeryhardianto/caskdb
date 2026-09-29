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
