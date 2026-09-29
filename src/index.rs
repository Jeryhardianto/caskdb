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
