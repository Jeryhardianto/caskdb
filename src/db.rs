use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::index::{recover, Index, IndexEntry};
use crate::record::Record;
use crate::segment::{list_segment_ids, segment_path, SegmentReader, SegmentWriter};

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

    /// Merges all sealed (non-active) segments into fresh, smaller
    /// segments, dropping stale/overwritten/deleted entries. See the
    /// design spec's "Compaction" section for why this relocates keys by
    /// consulting the live index rather than replaying the sealed
    /// segments' raw bytes in isolation, and why the active segment gets
    /// renumbered above the compacted output rather than the other way
    /// around.
    ///
    /// On failure (disk full, EIO, ...) this leaves `self.index` and
    /// `self.active` completely untouched — every fallible step happens
    /// before anything observable is mutated — and deletes whatever
    /// compacted segment files it had managed to create, so a retry starts
    /// from a clean slate instead of compounding the first attempt's
    /// partial output.
    pub fn compact(&mut self) -> Result<(), Error> {
        let old_active_id = self.active.file_id;
        let sealed_ids: HashSet<u64> = list_segment_ids(&self.dir)
            .map_err(Error::Io)?
            .into_iter()
            .filter(|id| *id != old_active_id)
            .collect();
        if sealed_ids.is_empty() {
            return Ok(());
        }

        // 1. Find every key whose *live* index entry points at a sealed
        //    segment, and read its current value the same way get() does.
        let mut to_relocate: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut readers = self.readers.borrow_mut();
            for (key, entry) in self.index.iter() {
                if !sealed_ids.contains(&entry.file_id) {
                    continue;
                }
                if !readers.contains_key(&entry.file_id) {
                    let reader =
                        SegmentReader::open(&self.dir, entry.file_id).map_err(Error::Io)?;
                    readers.insert(entry.file_id, reader);
                }
                let reader = readers.get(&entry.file_id).unwrap();
                let value = reader
                    .read_at(entry.value_pos, entry.value_size)
                    .map_err(Error::Io)?;
                to_relocate.push((key.clone(), value));
            }
        }

        // 2. Write the relocated keys into fresh segment(s) and rename the
        //    active segment above them, staging every index change in
        //    local variables so a failure anywhere in this step touches
        //    neither `self.index` nor `self.active`.
        let mut written_ids: Vec<u64> = Vec::new();
        let commit = self.write_and_commit_compacted_segments(old_active_id, &to_relocate, &mut written_ids);

        if let Err(e) = commit {
            // Nothing in `self` changed; only delete the orphaned output
            // files this attempt managed to create before it failed.
            for id in &written_ids {
                let _ = std::fs::remove_file(segment_path(&self.dir, *id));
            }
            return Err(e);
        }

        // 3. Any cached reader may point at a file_id whose backing file
        //    just moved or disappeared; drop them all and let get() reopen
        //    lazily by the (now-correct) index entries.
        self.readers.borrow_mut().clear();

        // 4. The old sealed segments are now fully superseded.
        for id in sealed_ids {
            std::fs::remove_file(segment_path(&self.dir, id)).map_err(Error::Io)?;
        }

        Ok(())
    }

    /// Does the fallible part of compaction: writes `to_relocate` into
    /// fresh segments (recording their ids in `written_ids` as they're
    /// created, so the caller can clean up on error) and renames the
    /// active segment above them. Everything here either succeeds
    /// completely or returns `Err` without having touched `self.index` or
    /// `self.active` — the index/active updates below the rename are all
    /// infallible, so once the rename succeeds this cannot fail partway.
    fn write_and_commit_compacted_segments(
        &mut self,
        old_active_id: u64,
        to_relocate: &[(Vec<u8>, Vec<u8>)],
        written_ids: &mut Vec<u64>,
    ) -> Result<(), Error> {
        let mut next_id = old_active_id;
        let mut writer: Option<SegmentWriter> = None;
        let mut staged_index: Vec<(Vec<u8>, IndexEntry)> = Vec::new();

        for (key, value) in to_relocate {
            let record = Record::new(key, value);
            let encoded = record.encode();
            let needs_new = match &writer {
                None => true,
                Some(w) => {
                    w.size > 0 && w.size + encoded.len() as u64 > self.options.max_segment_size
                }
            };
            if needs_new {
                next_id += 1;
                writer = Some(SegmentWriter::create_new(&self.dir, next_id).map_err(Error::Io)?);
                written_ids.push(next_id);
            }
            let w = writer.as_mut().expect("writer just ensured present");
            let offset = w.append(&encoded).map_err(Error::Io)?;
            staged_index.push((
                key.clone(),
                IndexEntry {
                    file_id: w.file_id,
                    value_pos: offset + record.value_offset() as u64,
                    value_size: value.len() as u32,
                    timestamp: record.timestamp,
                },
            ));
        }

        // Renumber the active segment above every id just written, so
        // file_id ordering still reflects recency after this compaction
        // (see spec: without this, a future `open()` would replay the
        // compacted output as if it were newer than the active segment's
        // real, more recent writes). The already-open file handle stays
        // valid across the rename on Unix, so no reopen is needed — which
        // also removes a failure window between "renamed" and "reopened".
        let new_active_id = next_id + 1;
        std::fs::rename(
            segment_path(&self.dir, old_active_id),
            segment_path(&self.dir, new_active_id),
        )
        .map_err(Error::Io)?;

        for (key, entry) in staged_index {
            self.index.insert(key, entry);
        }
        for entry in self.index.values_mut() {
            if entry.file_id == old_active_id {
                entry.file_id = new_active_id;
            }
        }
        self.active.file_id = new_active_id;

        Ok(())
    }
}
