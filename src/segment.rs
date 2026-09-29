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
    /// Set when a failed append's rollback (truncating the file back to
    /// `size`) itself failed. Once poisoned, every further append is
    /// refused rather than risk writing at an offset that no longer
    /// matches the file's real, unknown-good length.
    poisoned: bool,
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
        Ok(SegmentWriter {
            file_id,
            file,
            size,
            poisoned: false,
        })
    }

    /// Like `open_for_append`, but fails if `file_id`'s segment file
    /// already exists instead of silently appending to it. Compaction uses
    /// this for the fresh segments it writes, so a leftover file from a
    /// previous failed/retried compaction becomes a hard error instead of
    /// silent (and possibly wrong) reuse.
    pub fn create_new(dir: &Path, file_id: u64) -> io::Result<Self> {
        let path = segment_path(dir, file_id);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(SegmentWriter {
            file_id,
            file,
            size: 0,
            poisoned: false,
        })
    }

    /// Appends an already-encoded record and fsyncs before returning. Returns
    /// the byte offset within this segment where the record started.
    ///
    /// If the write or fsync fails partway (disk full, EIO, ...), the file
    /// may now hold trailing bytes beyond `size` even though `size` itself
    /// was never advanced — leaving the two disagreeing would corrupt the
    /// *next* append (it would compute its offset from the too-small
    /// `size`, while append-mode writes actually land past the leftover
    /// garbage). So on any failure this rolls the file back to `size`
    /// before returning the error; if even that fails, the writer is
    /// poisoned so no further append can compound the damage.
    pub fn append(&mut self, encoded: &[u8]) -> io::Result<u64> {
        use std::io::Write;
        if self.poisoned {
            return Err(io::Error::other(
                "segment writer is poisoned: a previous failed append could not be rolled back",
            ));
        }

        let offset = self.size;
        match self.file.write_all(encoded).and_then(|()| self.file.sync_all()) {
            Ok(()) => {
                self.size += encoded.len() as u64;
                Ok(offset)
            }
            Err(write_err) => {
                if let Err(rollback_err) = self.file.set_len(self.size) {
                    self.poisoned = true;
                    return Err(rollback_err);
                }
                Err(write_err)
            }
        }
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
