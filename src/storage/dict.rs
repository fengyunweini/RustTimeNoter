//! 字符串池字典：路径 / 标题 → u32 ID。
//!
//! 文件格式（little-endian）：
//! ```text
//! magic   : [u8; 4] = b"RTND"
//! version : u32     = 1
//! repeat:
//!   id     : u32
//!   len    : u32   (字节数，UTF-8)
//!   bytes  : [u8; len]
//! ```
//! ID 从 1 开始；0 保留为"无"语义。
//! Append-only：每次新增项 fsync 写尾部一条记录。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"RTND";
const VERSION: u32 = 1;
const HEADER_LEN: u64 = 4 + 4;
const RECORD_HEADER_LEN: u64 = 4 + 4;
const MAX_ENTRY_BYTES: usize = 1024 * 1024;
const MAX_RECOVERY_CHAIN_CANDIDATES: usize = 256;
const MAX_RECOVERY_CHAIN_BYTES: usize = 4 * MAX_ENTRY_BYTES;

pub struct Dict {
    path: PathBuf,
    by_string: HashMap<String, u32>,
    by_id: Vec<Option<String>>, // index = id
    next_id: u32,
    writable: bool,
}

impl Dict {
    /// Opens a dictionary for readers.
    ///
    /// A torn tail is ignored in memory but is never truncated here because a
    /// CLI reader may have observed the daemon in the middle of an append.
    /// A missing dictionary is treated as empty without creating it.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut me = Self::empty(path.clone(), false);
        let observed_len = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(me),
            Err(error) => return Err(error),
        };
        if observed_len == 0 {
            return Ok(me);
        }
        if observed_len < HEADER_LEN {
            let file = File::open(&path)?;
            let mut existing = Vec::with_capacity(HEADER_LEN as usize + 1);
            file.take(HEADER_LEN + 1).read_to_end(&mut existing)?;
            if existing.len() >= HEADER_LEN as usize {
                me.load()?;
                return Ok(me);
            }
            if encoded_header().starts_with(&existing) {
                return Ok(me);
            }
            return Err(invalid_data(
                "dict: partial header does not match the expected header",
            ));
        }
        me.load()?;
        Ok(me)
    }

    /// Opens a dictionary for the daemon writer and repairs a torn tail before
    /// allowing any new record to be appended.
    pub(crate) fn open_writer(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut me = Self::empty(path.clone(), true);
        let observed_len = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        if observed_len < HEADER_LEN {
            me.write_header_if_needed()?;
            return Ok(me);
        }

        let loaded = me.load()?;
        if loaded.valid_len < loaded.observed_len {
            me.truncate_to(loaded.valid_len)?;
        }
        Ok(me)
    }

    fn empty(path: PathBuf, writable: bool) -> Self {
        Self {
            path,
            by_string: HashMap::new(),
            by_id: vec![None], // index 0 reserved
            next_id: 1,
            writable,
        }
    }

    fn write_header_if_needed(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        let mut existing = Vec::with_capacity(HEADER_LEN as usize + 1);
        (&mut f).take(HEADER_LEN + 1).read_to_end(&mut existing)?;
        if existing.len() >= HEADER_LEN as usize {
            return Err(invalid_data(
                "dict: file changed while repairing its partial header",
            ));
        }
        let expected = encoded_header();
        if !expected.starts_with(&existing) {
            return Err(invalid_data(
                "dict: partial header does not match the expected header; refusing to repair",
            ));
        }
        f.set_len(0)?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&expected)?;
        f.sync_data()?;
        Ok(())
    }

    fn load(&mut self) -> std::io::Result<LoadOutcome> {
        let f = File::open(&self.path)?;
        let observed_len = f.metadata()?.len();
        let mut r = BufReader::new(f);
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "dict: bad magic",
            ));
        }
        let mut ver = [0u8; 4];
        r.read_exact(&mut ver)?;
        if u32::from_le_bytes(ver) != VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "dict: bad version",
            ));
        }
        let mut cursor = HEADER_LEN;
        while cursor < observed_len {
            let record_start = cursor;
            if observed_len - cursor < RECORD_HEADER_LEN {
                let remaining = usize::try_from(observed_len - cursor)
                    .expect("partial record header is shorter than eight bytes");
                let mut partial_header = vec![0u8; remaining];
                r.read_exact(&mut partial_header)?;
                validate_partial_record_header(&partial_header, self.next_id)?;
                return Ok(LoadOutcome {
                    valid_len: record_start,
                    observed_len,
                });
            }

            let mut id_buf = [0u8; 4];
            if let Err(error) = r.read_exact(&mut id_buf) {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(LoadOutcome {
                        valid_len: record_start,
                        observed_len,
                    });
                }
                return Err(error);
            }
            let id = u32::from_le_bytes(id_buf);
            let mut len_buf = [0u8; 4];
            if let Err(error) = r.read_exact(&mut len_buf) {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(LoadOutcome {
                        valid_len: record_start,
                        observed_len,
                    });
                }
                return Err(error);
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            cursor += RECORD_HEADER_LEN;
            if id != self.next_id {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("dict: expected id {}, found {id}", self.next_id),
                ));
            }
            let next_id = id.checked_add(1).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "dict: id overflow")
            })?;
            if len > MAX_ENTRY_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("dict: entry {id} declares {len} bytes; maximum is {MAX_ENTRY_BYTES}"),
                ));
            }
            if observed_len - cursor < len as u64 {
                let remaining = usize::try_from(observed_len - cursor)
                    .expect("incomplete dictionary payload is bounded by MAX_ENTRY_BYTES");
                let mut tail = Vec::with_capacity(remaining);
                (&mut r).take(remaining as u64).read_to_end(&mut tail)?;
                if tail.len() != remaining {
                    return Err(invalid_data(
                        "dict: file changed while checking an incomplete tail",
                    ));
                }
                if contains_plausible_successor_chain(&tail, next_id)? {
                    return Err(invalid_data(format!(
                        "dict: incomplete entry {id} contains a plausible complete successor \
                         chain; refusing to truncate possible non-tail data"
                    )));
                }
                return Ok(LoadOutcome {
                    valid_len: record_start,
                    observed_len,
                });
            }

            let mut bytes = vec![0u8; len];
            if let Err(error) = r.read_exact(&mut bytes) {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Ok(LoadOutcome {
                        valid_len: record_start,
                        observed_len,
                    });
                }
                return Err(error);
            }
            cursor += len as u64;
            let s = String::from_utf8(bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            if self.by_string.contains_key(&s) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("dict: duplicate string at id {id}"),
                ));
            }
            self.by_id.push(Some(s.clone()));
            self.by_string.insert(s, id);
            self.next_id = next_id;
        }
        Ok(LoadOutcome {
            valid_len: cursor,
            observed_len,
        })
    }

    fn truncate_to(&self, valid_len: u64) -> std::io::Result<()> {
        let f = OpenOptions::new().write(true).open(&self.path)?;
        f.set_len(valid_len)?;
        f.sync_data()
    }

    /// 返回字符串对应的 ID；不存在则分配并 append。
    pub fn intern(&mut self, s: &str) -> std::io::Result<u32> {
        if !self.writable {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "dict: intern requires a writer-opened dictionary",
            ));
        }
        if let Some(&id) = self.by_string.get(s) {
            return Ok(id);
        }
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("dict: id overflow"))?;
        self.append_record(id, s)?;
        self.next_id = next_id;
        self.by_id.push(Some(s.to_string()));
        self.by_string.insert(s.to_string(), id);
        Ok(id)
    }

    fn append_record(&self, id: u32, s: &str) -> std::io::Result<()> {
        let bytes = s.as_bytes();
        if bytes.len() > MAX_ENTRY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "dict: entry is {} bytes; maximum is {MAX_ENTRY_BYTES}",
                    bytes.len()
                ),
            ));
        }
        let len = u32::try_from(bytes.len()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "dict: entry is too large")
        })?;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        let mut wire = Vec::with_capacity(RECORD_HEADER_LEN as usize + bytes.len());
        wire.extend_from_slice(&id.to_le_bytes());
        wire.extend_from_slice(&len.to_le_bytes());
        wire.extend_from_slice(bytes);
        f.write_all(&wire)?;
        f.sync_data()?;
        Ok(())
    }

    pub fn get(&self, id: u32) -> Option<&str> {
        self.by_id.get(id as usize).and_then(|o| o.as_deref())
    }

    pub fn len(&self) -> usize {
        self.by_string.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_string.is_empty()
    }
}

struct LoadOutcome {
    valid_len: u64,
    observed_len: u64,
}

fn encoded_header() -> [u8; HEADER_LEN as usize] {
    let mut header = [0u8; HEADER_LEN as usize];
    header[0..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn validate_partial_record_header(bytes: &[u8], expected_id: u32) -> std::io::Result<()> {
    debug_assert!(bytes.len() < RECORD_HEADER_LEN as usize);
    if bytes.is_empty() {
        return Ok(());
    }
    if expected_id == u32::MAX {
        return Err(invalid_data(
            "dict: trailing bytes cannot be a valid append because the id space is exhausted",
        ));
    }

    let expected_id_bytes = expected_id.to_le_bytes();
    let id_prefix_len = bytes.len().min(expected_id_bytes.len());
    if bytes[..id_prefix_len] != expected_id_bytes[..id_prefix_len] {
        return Err(invalid_data(format!(
            "dict: partial tail does not match expected id {expected_id}; refusing to truncate"
        )));
    }

    if bytes.len() > expected_id_bytes.len() {
        let length_prefix = &bytes[expected_id_bytes.len()..];
        let mut minimum_length_bytes = [0u8; 4];
        minimum_length_bytes[..length_prefix.len()].copy_from_slice(length_prefix);
        let minimum_length = u32::from_le_bytes(minimum_length_bytes) as usize;
        if minimum_length > MAX_ENTRY_BYTES {
            return Err(invalid_data(format!(
                "dict: partial tail already declares at least {minimum_length} bytes; maximum is \
                 {MAX_ENTRY_BYTES}"
            )));
        }
    }
    Ok(())
}

/// Look for a structurally complete successor record chain inside the bytes
/// that an incomplete length claims as payload. Such a chain is evidence that
/// a corrupted middle length may be swallowing records, so automatic
/// truncation is unsafe.
///
/// This is intentionally conservative and bounded. Candidate or byte-budget
/// exhaustion returns an error rather than silently approving truncation.
fn contains_plausible_successor_chain(bytes: &[u8], successor_id: u32) -> std::io::Result<bool> {
    if bytes.len() < RECORD_HEADER_LEN as usize {
        return Ok(false);
    }

    let successor_bytes = successor_id.to_le_bytes();
    let mut candidates = 0usize;
    let mut inspected_bytes = 0usize;
    let last_start = bytes.len() - RECORD_HEADER_LEN as usize;
    for start in 0..=last_start {
        if bytes[start..start + 4] != successor_bytes {
            continue;
        }
        candidates = candidates.checked_add(1).ok_or_else(|| {
            invalid_data("dict: recovery candidate counter overflow; refusing to truncate")
        })?;
        if candidates > MAX_RECOVERY_CHAIN_CANDIDATES {
            return Err(invalid_data(format!(
                "dict: recovery proof exceeds {MAX_RECOVERY_CHAIN_CANDIDATES} candidates; \
                 refusing to truncate"
            )));
        }
        if plausible_record_chain(&bytes[start..], successor_id, &mut inspected_bytes)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn plausible_record_chain(
    bytes: &[u8],
    mut expected_id: u32,
    inspected_bytes: &mut usize,
) -> std::io::Result<bool> {
    let mut cursor = 0usize;
    let mut complete_records = 0usize;

    loop {
        let remaining = &bytes[cursor..];
        if remaining.is_empty() {
            return Ok(complete_records > 0);
        }
        if remaining.len() < RECORD_HEADER_LEN as usize {
            return if complete_records > 0 {
                Ok(validate_partial_record_header(remaining, expected_id).is_ok())
            } else {
                Ok(false)
            };
        }

        let id = u32::from_le_bytes(remaining[0..4].try_into().expect("four bytes checked"));
        if id != expected_id {
            return Ok(false);
        }
        let len =
            u32::from_le_bytes(remaining[4..8].try_into().expect("four bytes checked")) as usize;
        if len > MAX_ENTRY_BYTES {
            return Ok(false);
        }

        let record_len = (RECORD_HEADER_LEN as usize)
            .checked_add(len)
            .ok_or_else(|| invalid_data("dict: recovery record length overflow"))?;
        *inspected_bytes = inspected_bytes.checked_add(record_len).ok_or_else(|| {
            invalid_data("dict: recovery byte counter overflow; refusing to truncate")
        })?;
        if *inspected_bytes > MAX_RECOVERY_CHAIN_BYTES {
            return Err(invalid_data(format!(
                "dict: recovery proof exceeds {MAX_RECOVERY_CHAIN_BYTES} inspected bytes; \
                 refusing to truncate"
            )));
        }

        if remaining.len() < record_len {
            return Ok(complete_records > 0);
        }
        if std::str::from_utf8(&remaining[RECORD_HEADER_LEN as usize..record_len]).is_err() {
            return Ok(false);
        }

        complete_records += 1;
        cursor += record_len;
        expected_id = match expected_id.checked_add(1) {
            Some(next) => next,
            None => return Ok(cursor == bytes.len()),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        {
            let mut d = Dict::open_writer(&path).unwrap();
            assert_eq!(d.intern("foo").unwrap(), 1);
            assert_eq!(d.intern("bar").unwrap(), 2);
            assert_eq!(d.intern("foo").unwrap(), 1);
            assert_eq!(d.len(), 2);
        }
        let d2 = Dict::open(&path).unwrap();
        assert_eq!(d2.get(1), Some("foo"));
        assert_eq!(d2.get(2), Some("bar"));
        assert_eq!(d2.len(), 2);
    }

    #[test]
    fn unicode_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("u.dict");
        let mut d = Dict::open_writer(&path).unwrap();
        let id = d.intern("中文窗口标题🚀").unwrap();
        let d2 = Dict::open(&path).unwrap();
        assert_eq!(d2.get(id), Some("中文窗口标题🚀"));
    }

    fn encoded_entry(id: u32, value: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(RECORD_HEADER_LEN as usize + value.len());
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
        bytes
    }

    fn write_prefix(path: &Path) -> u64 {
        let mut dict = Dict::open_writer(path).unwrap();
        assert_eq!(dict.intern("foo").unwrap(), 1);
        assert_eq!(dict.intern("bar").unwrap(), 2);
        std::fs::metadata(path).unwrap().len()
    }

    fn assert_rejected_and_preserved(path: &Path, damaged: &[u8], expected_message: &str) {
        std::fs::write(path, damaged).unwrap();

        let read_error = Dict::open(path)
            .err()
            .expect("reader must reject an unsafe recovery");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            read_error.to_string().contains(expected_message),
            "{read_error}"
        );
        assert_eq!(std::fs::read(path).unwrap(), damaged);

        let write_error = Dict::open_writer(path)
            .err()
            .expect("writer must reject an unsafe recovery");
        assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            write_error.to_string().contains(expected_message),
            "{write_error}"
        );
        assert_eq!(std::fs::read(path).unwrap(), damaged);
    }

    #[test]
    fn read_only_open_never_creates_or_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.dict");

        let mut dict = Dict::open(&path).unwrap();
        assert!(dict.is_empty());
        assert!(!path.exists());
        let error = dict.intern("foo").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!path.exists());

        let writer = Dict::open_writer(&path).unwrap();
        assert!(writer.is_empty());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEADER_LEN);
    }

    #[test]
    fn matching_partial_headers_are_repaired_only_by_writers() {
        let expected = encoded_header();
        for retained in 0..HEADER_LEN as usize {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("header-{retained}.dict"));
            std::fs::write(&path, &expected[..retained]).unwrap();

            let reader = Dict::open(&path).unwrap();
            assert!(reader.is_empty(), "{retained}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                &expected[..retained],
                "{retained}"
            );

            let mut writer = Dict::open_writer(&path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected, "{retained}");
            assert_eq!(writer.intern("after-repair").unwrap(), 1, "{retained}");
            drop(writer);

            let reopened = Dict::open(&path).unwrap();
            assert_eq!(reopened.get(1), Some("after-repair"), "{retained}");
        }
    }

    #[test]
    fn mismatching_partial_header_is_rejected_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-header.dict");
        let mut damaged = encoded_header()[..6].to_vec();
        damaged[1] ^= 0x20;
        std::fs::write(&path, &damaged).unwrap();

        let read_error = Dict::open(&path)
            .err()
            .expect("reader must reject a mismatching partial header");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);

        let write_error = Dict::open_writer(&path)
            .err()
            .expect("writer must not replace a mismatching partial header");
        assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }

    #[test]
    fn read_only_open_does_not_truncate_an_in_flight_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in-flight.dict");
        let valid_len = write_prefix(&path);
        let partial = &encoded_entry(3, "in-flight")[..6];

        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(partial);
        std::fs::write(&path, &bytes).unwrap();

        let reader = Dict::open(&path).unwrap();
        assert_eq!(reader.get(1), Some("foo"));
        assert_eq!(reader.get(2), Some("bar"));
        assert_eq!(reader.len(), 2);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            valid_len + partial.len() as u64
        );
    }

    #[test]
    fn writer_recovers_torn_tail_then_appends_and_reopens() {
        let tail = encoded_entry(3, "baz");
        let cases = [
            ("partial-id", 1usize),
            ("id-only", 4),
            ("partial-len", 7),
            ("header-only", RECORD_HEADER_LEN as usize),
            ("partial-payload", tail.len() - 1),
        ];

        for (name, retained) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("{name}.dict"));
            let valid_len = write_prefix(&path);
            let mut damaged = std::fs::read(&path).unwrap();
            damaged.extend_from_slice(&tail[..retained]);
            std::fs::write(&path, &damaged).unwrap();

            let reader = Dict::open(&path).unwrap();
            assert_eq!(reader.get(1), Some("foo"), "{name}");
            assert_eq!(reader.get(2), Some("bar"), "{name}");
            assert_eq!(reader.len(), 2, "{name}");
            assert_eq!(std::fs::read(&path).unwrap(), damaged, "{name}");

            let mut writer = Dict::open_writer(&path).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().len(), valid_len, "{name}");
            assert_eq!(writer.intern("baz").unwrap(), 3, "{name}");
            drop(writer);

            let reopened = Dict::open(&path).unwrap();
            assert_eq!(reopened.get(1), Some("foo"), "{name}");
            assert_eq!(reopened.get(2), Some("bar"), "{name}");
            assert_eq!(reopened.get(3), Some("baz"), "{name}");
            assert_eq!(reopened.len(), 3, "{name}");
        }
    }

    #[test]
    fn mismatching_partial_next_id_is_rejected_and_preserved() {
        for retained in 1..RECORD_HEADER_LEN as usize {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("bad-next-id-{retained}.dict"));
            write_prefix(&path);
            let mut damaged = std::fs::read(&path).unwrap();
            damaged.extend_from_slice(&encoded_entry(7, "not-next")[..retained]);

            assert_rejected_and_preserved(&path, &damaged, "expected id 3");
        }
    }

    #[test]
    fn impossible_partial_length_prefix_is_rejected_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-partial-length.dict");
        write_prefix(&path);
        let mut damaged = std::fs::read(&path).unwrap();
        damaged.extend_from_slice(&3u32.to_le_bytes());
        // With three little-endian length bytes already present, the minimum
        // possible completed value is 0x20_0000 (> 1 MiB).
        damaged.extend_from_slice(&[0x00, 0x00, 0x20]);

        assert_rejected_and_preserved(&path, &damaged, "already declares at least");
    }

    #[test]
    fn corrupted_length_cannot_swallow_a_complete_successor_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("swallowed-successors.dict");
        write_prefix(&path);
        let mut damaged = std::fs::read(&path).unwrap();

        damaged.extend_from_slice(&3u32.to_le_bytes());
        damaged.extend_from_slice(&1_000u32.to_le_bytes());
        damaged.extend_from_slice(b"x");
        damaged.extend_from_slice(&encoded_entry(4, "survivor"));
        damaged.extend_from_slice(&encoded_entry(5, "also-survives"));

        assert_rejected_and_preserved(&path, &damaged, "successor chain");
    }

    #[test]
    fn recovery_candidate_budget_exhaustion_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate-budget.dict");
        write_prefix(&path);

        let mut tail = Vec::new();
        for _ in 0..=MAX_RECOVERY_CHAIN_CANDIDATES {
            tail.extend_from_slice(&4u32.to_le_bytes());
            tail.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        let declared_len = u32::try_from(tail.len() + 1).unwrap();
        let mut damaged = std::fs::read(&path).unwrap();
        damaged.extend_from_slice(&3u32.to_le_bytes());
        damaged.extend_from_slice(&declared_len.to_le_bytes());
        damaged.extend_from_slice(&tail);

        assert_rejected_and_preserved(&path, &damaged, "exceeds 256 candidates");
    }

    #[test]
    fn malformed_huge_length_is_rejected_without_allocation_or_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge-len.dict");
        write_prefix(&path);
        let mut damaged = std::fs::read(&path).unwrap();
        damaged.extend_from_slice(&3u32.to_le_bytes());
        damaged.extend_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, &damaged).unwrap();

        let read_error = Dict::open(&path)
            .err()
            .expect("reader must reject huge len");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);

        let write_error = Dict::open_writer(&path)
            .err()
            .expect("writer must reject huge len");
        assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }
}
