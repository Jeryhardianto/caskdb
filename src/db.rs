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
