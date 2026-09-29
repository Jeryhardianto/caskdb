use caskdb::{CaskDb, Options};
use tempfile::tempdir;

/// Sets (or restores) the process's RLIMIT_FSIZE. This is the only
/// `#[test]` in this binary (each `tests/*.rs` file compiles to its own
/// process), so mutating the process-wide rlimit here cannot race with any
/// other test.
fn set_fsize(limit: Option<u64>) {
    unsafe {
        let mut r: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(libc::RLIMIT_FSIZE, &mut r);
        r.rlim_cur = limit.map(|l| l as libc::rlim_t).unwrap_or(r.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &r), 0);
    }
}

fn populate(dir: &std::path::Path) {
    // Small segments so the 10 puts below span several sealed segments.
    let mut db = CaskDb::open_with_options(dir, Options { max_segment_size: 200 }).unwrap();
    for i in 0..10u8 {
        db.put(&[b'k', b'0' + i], format!("old-{i}").as_bytes())
            .unwrap();
    }
}

#[test]
fn a_failed_compact_changes_nothing_and_a_retry_after_it_is_safe() {
    unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    let dir = tempdir().unwrap();
    populate(dir.path());

    let mut db = CaskDb::open_with_options(
        dir.path(),
        Options {
            max_segment_size: 100_000,
        },
    )
    .unwrap();

    // A tight limit lets compact() create its first output segment but
    // fail while writing further into it (or renaming), simulating a
    // disk-full/EIO partway through compaction.
    set_fsize(Some(80));
    let first_attempt = db.compact();
    set_fsize(None);
    assert!(first_attempt.is_err(), "compact should report the failure");

    // A failed compact() must not have changed a single key's value: every
    // key the caller already acknowledged (the original 10 puts) must
    // still read back exactly as written, both in-process and after a
    // fresh open() from disk.
    for i in 0..10u8 {
        let expected = Some(format!("old-{i}").into_bytes());
        assert_eq!(
            db.get(&[b'k', b'0' + i]).unwrap(),
            expected,
            "key k{i} changed after a failed compact()"
        );
    }

    // Retrying compact() (now with plenty of room) must succeed and must
    // not have lost any data to the first attempt's partial output.
    db.compact().unwrap();
    for i in 0..10u8 {
        let expected = Some(format!("old-{i}").into_bytes());
        assert_eq!(
            db.get(&[b'k', b'0' + i]).unwrap(),
            expected,
            "key k{i} lost after retrying compact()"
        );
    }
    assert_eq!(db.len(), 10);

    drop(db);
    let db = CaskDb::open(dir.path()).unwrap();
    assert_eq!(db.len(), 10, "reopen after retried compact() lost keys");
    for i in 0..10u8 {
        let expected = Some(format!("old-{i}").into_bytes());
        assert_eq!(db.get(&[b'k', b'0' + i]).unwrap(), expected);
    }
}
