use crc32fast::Hasher;

/// crc(4) + timestamp(8) + flags(1) + key_len(4) + val_len(4)
pub const HEADER_LEN: usize = 4 + 8 + 1 + 4 + 4;
const TOMBSTONE_FLAG: u8 = 0b0000_0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub timestamp: u64,
    pub flags: u8,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Not enough bytes yet to read a full header, or the header's declared
    /// key_len/val_len run past the end of the supplied slice. Expected when
    /// the caller passed the tail of a segment that a crash interrupted
    /// mid-write.
    Incomplete,
    /// A full-length record was present but its CRC did not match. Expected
    /// only as real corruption — a crash-interrupted write is caught by
    /// `Incomplete` instead, since an append() that never completed leaves
    /// a short file, not a full-length-but-wrong-checksum one.
    ChecksumMismatch,
}

impl Record {
    pub fn new(key: &[u8], value: &[u8]) -> Self {
        Record {
            timestamp: now_millis(),
            flags: 0,
            key: key.to_vec(),
            value: value.to_vec(),
        }
    }

    pub fn tombstone(key: &[u8]) -> Self {
        Record {
            timestamp: now_millis(),
            flags: TOMBSTONE_FLAG,
            key: key.to_vec(),
            value: Vec::new(),
        }
    }

    pub fn is_tombstone(&self) -> bool {
        self.flags & TOMBSTONE_FLAG != 0
    }

    /// Byte offset of the value payload relative to the start of this
    /// record (i.e. past crc/timestamp/flags/key_len/val_len/key).
    pub fn value_offset(&self) -> usize {
        HEADER_LEN + self.key.len()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(8 + 1 + 4 + 4 + self.key.len() + self.value.len());
        body.extend_from_slice(&self.timestamp.to_le_bytes());
        body.push(self.flags);
        body.extend_from_slice(&(self.key.len() as u32).to_le_bytes());
        body.extend_from_slice(&(self.value.len() as u32).to_le_bytes());
        body.extend_from_slice(&self.key);
        body.extend_from_slice(&self.value);

        let mut hasher = Hasher::new();
        hasher.update(&body);
        let crc = hasher.finalize();

        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decodes exactly one record from the start of `bytes`. Returns the
    /// record and how many bytes it consumed; does not require `bytes` to
    /// end exactly at the record boundary (trailing bytes, e.g. the next
    /// record, are ignored).
    pub fn decode(bytes: &[u8]) -> Result<(Record, usize), DecodeError> {
        if bytes.len() < HEADER_LEN {
            return Err(DecodeError::Incomplete);
        }

        let crc = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let timestamp = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
        let flags = bytes[12];
        let key_len = u32::from_le_bytes(bytes[13..17].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(bytes[17..21].try_into().unwrap()) as usize;
        let total_len = HEADER_LEN + key_len + val_len;

        if bytes.len() < total_len {
            return Err(DecodeError::Incomplete);
        }

        let mut hasher = Hasher::new();
        hasher.update(&bytes[4..total_len]);
        if hasher.finalize() != crc {
            return Err(DecodeError::ChecksumMismatch);
        }

        let key = bytes[HEADER_LEN..HEADER_LEN + key_len].to_vec();
        let value = bytes[HEADER_LEN + key_len..total_len].to_vec();

        Ok((
            Record {
                timestamp,
                flags,
                key,
                value,
            },
            total_len,
        ))
    }
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before 1970")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_normal_record() {
        let record = Record::new(b"key1", b"value1");
        let encoded = record.encode();
        let (decoded, consumed) = Record::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, record);
        assert!(!decoded.is_tombstone());
    }

    #[test]
    fn round_trips_a_tombstone() {
        let record = Record::tombstone(b"key1");
        let encoded = record.encode();
        let (decoded, _) = Record::decode(&encoded).unwrap();
        assert!(decoded.is_tombstone());
        assert_eq!(decoded.value, Vec::<u8>::new());
    }

    #[test]
    fn empty_value_is_not_a_tombstone() {
        let record = Record::new(b"key1", b"");
        let encoded = record.encode();
        let (decoded, _) = Record::decode(&encoded).unwrap();
        assert!(!decoded.is_tombstone());
        assert_eq!(decoded.value, Vec::<u8>::new());
    }

    #[test]
    fn decode_reports_incomplete_on_truncated_bytes() {
        let record = Record::new(b"key1", b"value1");
        let encoded = record.encode();
        let truncated = &encoded[..encoded.len() - 2];
        assert_eq!(Record::decode(truncated), Err(DecodeError::Incomplete));
    }

    #[test]
    fn decode_reports_checksum_mismatch_on_corrupted_body() {
        let record = Record::new(b"key1", b"value1");
        let mut encoded = record.encode();
        let last = encoded.len() - 1;
        encoded[last] ^= 0xFF; // flip a bit inside the value, length fields untouched
        assert_eq!(
            Record::decode(&encoded),
            Err(DecodeError::ChecksumMismatch)
        );
    }

    #[test]
    fn decode_ignores_trailing_bytes_after_one_record() {
        let first = Record::new(b"key1", b"value1").encode();
        let second = Record::new(b"key2", b"value2").encode();
        let mut both = first.clone();
        both.extend_from_slice(&second);

        let (decoded, consumed) = Record::decode(&both).unwrap();
        assert_eq!(consumed, first.len());
        assert_eq!(decoded.key, b"key1");
    }
}
