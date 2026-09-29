use caskdb::CaskDb;
use tempfile::tempdir;

/// Sets (or restores) the process's RLIMIT_FSIZE, so the next write that
/// crosses `limit` bytes fails with EFBIG partway through — the same shape
/// of failure a real disk-full/EIO would produce mid-`write_all`.
///
/// This test is the only `#[test]` in this binary (each `tests/*.rs` file
/// compiles to its own process), so mutating the process-wide rlimit here
/// cannot race with any other test.
fn set_fsize(limit: Option<u64>) {
    unsafe {
        let mut r: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(libc::RLIMIT_FSIZE, &mut r);
        r.rlim_cur = limit.map(|l| l as libc::rlim_t).unwrap_or(r.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &r), 0);
    }
}

#[test]
fn a_failed_partial_write_is_rolled_back_so_later_writes_stay_correct() {
    // A write that fails partway (disk full, EFBIG, EIO, ...) must not
    // leave the segment file longer than the writer's own bookkeeping
    // thinks it is — otherwise the next successful write computes its
    // offset from the stale (too-small) size, while the OS actually
    // appends past the leftover garbage, and every index entry from that
    // point on points at the wrong bytes.
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    let dir = tempdir().unwrap();
    let mut db = CaskDb::open(dir.path()).unwrap();
    db.put(b"a", &[b'A'; 100]).unwrap(); // one full, valid record on disk

    // Allow only a little more room: enough for a partial write of the next
    // (200-byte-value) record, not enough for all of it.
    set_fsize(Some(200));
    let result = db.put(b"big", &[b'B'; 300]);
    set_fsize(None);
    assert!(result.is_err(), "put should report the write failure");

    // The database must still behave correctly afterward: a fresh put must
    // land at the *true* end of file and be readable back exactly.
    db.put(b"c", b"hello").unwrap();
    assert_eq!(db.get(b"c").unwrap(), Some(b"hello".to_vec()));
    assert_eq!(db.get(b"a").unwrap(), Some(vec![b'A'; 100]));

    drop(db);
    let db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.get(b"c").unwrap(), Some(b"hello".to_vec()));
    assert_eq!(db.get(b"a").unwrap(), Some(vec![b'A'; 100]));
}
