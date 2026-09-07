//! Append-only v1 string dictionaries. Readers never modify files. A writer
//! backs up the original bytes before repairing a plausible incomplete append.
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
const HEADER: &[u8; 8] = b"RTND\x01\0\0\0";
const MAX_ENTRY_BYTES: usize = 1024 * 1024;

pub struct Dict {
    path: PathBuf,
    by_string: HashMap<Arc<str>, u32>,
    by_id: Vec<Option<Arc<str>>>,
    next_id: u32,
    writable: bool,
    failed: bool,
    pending_file: Option<File>,
}

/// A query snapshot that can catch up with the daemon's append-only dictionary.
/// Writers synchronize dictionary entries before writing their referring logs,
/// so an ID read from a newer log must exist when the dictionary is reloaded.
pub(crate) struct DictReader {
    snapshot: Dict,
}

impl DictReader {
    pub(crate) fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            snapshot: Dict::open(path)?,
        })
    }

    pub(crate) fn resolve(&mut self, id: u32) -> io::Result<&str> {
        if self.snapshot.get(id).is_none() {
            // No borrowed entry can survive this exclusive access. Release
            // the old snapshot first instead of retaining two large tables.
            self.snapshot = Dict::empty(self.snapshot.path.clone(), false);
            self.snapshot = Dict::open(&self.snapshot.path)?;
        }
        self.snapshot.get(id).ok_or_else(|| {
            invalid_data(format!(
                "{}: log references missing dictionary id {id}",
                self.snapshot.path.display()
            ))
        })
    }
}

struct Loaded {
    valid_len: u64,
    observed_len: u64,
}
impl Dict {
    fn empty(path: PathBuf, writable: bool) -> Self {
        Self {
            path,
            by_string: HashMap::new(),
            by_id: vec![None],
            next_id: 1,
            writable,
            failed: false,
            pending_file: None,
        }
    }
    /// A missing file is empty; partial tails are ignored in memory only.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut dict = Self::empty(path.as_ref().to_path_buf(), false);
        match File::open(&dict.path) {
            Ok(file) => {
                dict.load(file)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(dict)
    }
    #[cfg(test)]
    pub(crate) fn open_writer(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::open_writer_checked(path, 0)
    }

    /// Do not repair or reuse IDs until all existing log references are present.
    /// Loading enforces contiguous IDs, so the largest reference is sufficient.
    pub(crate) fn open_writer_checked(
        path: impl AsRef<Path>,
        required_id: u32,
    ) -> io::Result<Self> {
        let mut dict = Self::empty(path.as_ref().to_path_buf(), true);
        let loaded = match File::open(&dict.path) {
            Ok(file) => dict.load(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if required_id != 0 {
                    return Err(invalid_data(format!(
                        "{}: missing dictionary contains referenced id {required_id}; refusing to reuse its IDs",
                        dict.path.display()
                    )));
                }
                if let Some(parent) = dict.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&dict.path)?;
                file.write_all(HEADER)?;
                file.sync_all()?;
                return Ok(dict);
            }
            Err(error) => return Err(error),
        };
        if required_id != 0 && dict.get(required_id).is_none() {
            return Err(invalid_data(format!(
                "{}: log references missing dictionary id {required_id}; original preserved",
                dict.path.display()
            )));
        }
        if loaded.observed_len < HEADER.len() as u64 || loaded.valid_len < loaded.observed_len {
            dict.backup_and_repair(&loaded)?;
        }
        // A previous process may have stopped after appending a complete entry
        // but before syncing it. Readability from the OS cache is not a durable
        // dictionary barrier, including when no new ID is needed this session.
        let file = OpenOptions::new().append(true).open(&dict.path)?;
        #[cfg(all(test, windows))]
        let file = startup_sync_handle(&dict.path, file)?;
        dict.pending_file = Some(file);
        dict.sync_pending()?;
        Ok(dict)
    }
    fn load(&mut self, file: File) -> io::Result<Loaded> {
        let observed_len = file.metadata()?.len();
        let mut input = BufReader::new(file.take(observed_len));
        if observed_len < HEADER.len() as u64 {
            let mut bytes = Vec::new();
            input.read_to_end(&mut bytes)?;
            if !HEADER.starts_with(&bytes) {
                return Err(invalid_data(
                    "dict: invalid partial header; original preserved",
                ));
            }
            return Ok(Loaded {
                valid_len: 0,
                observed_len,
            });
        }
        let mut header = [0u8; 8];
        input.read_exact(&mut header)?;
        if &header != HEADER {
            return Err(invalid_data("dict: invalid header; original preserved"));
        }
        let mut cursor = HEADER.len() as u64;
        while cursor < observed_len {
            let start = cursor;
            let remaining = observed_len - cursor;
            if remaining < 8 {
                let mut bytes = vec![0u8; remaining as usize];
                input.read_exact(&mut bytes)?;
                validate_partial_header(&bytes, self.next_id)?;
                return Ok(Loaded {
                    valid_len: start,
                    observed_len,
                });
            }
            let mut header = [0u8; 8];
            input.read_exact(&mut header)?;
            let id = u32::from_le_bytes(header[..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            if id != self.next_id {
                return Err(invalid_data(format!(
                    "dict: expected id {}, found {id}; original preserved",
                    self.next_id
                )));
            }
            let next = id
                .checked_add(1)
                .ok_or_else(|| invalid_data("dict: id space exhausted"))?;
            if len > MAX_ENTRY_BYTES {
                return Err(invalid_data(
                    "dict: entry exceeds 1 MiB; original preserved",
                ));
            }
            cursor += 8;
            if (len as u64) > observed_len - cursor {
                let mut tail = vec![0u8; (observed_len - cursor) as usize];
                input.read_exact(&mut tail)?;
                // A real Win32 path/title cannot contain NUL. Refuse binary
                // structure in a tail rather than searching for guessed IDs.
                if tail.contains(&0)
                    || std::str::from_utf8(&tail).is_err_and(|error| error.error_len().is_some())
                {
                    return Err(invalid_data(
                        "dict: incomplete entry is not a text prefix; original preserved",
                    ));
                }
                return Ok(Loaded {
                    valid_len: start,
                    observed_len,
                });
            }
            let mut bytes = vec![0u8; len];
            input.read_exact(&mut bytes)?;
            cursor += len as u64;
            let value = String::from_utf8(bytes)
                .map_err(|_| invalid_data("dict: invalid UTF-8; original preserved"))?;
            if self.by_string.contains_key(value.as_str()) {
                return Err(invalid_data("dict: duplicate string; original preserved"));
            }
            let value: Arc<str> = Arc::from(value);
            self.by_id.push(Some(Arc::clone(&value)));
            self.by_string.insert(value, id);
            self.next_id = next;
        }
        Ok(Loaded {
            valid_len: cursor,
            observed_len,
        })
    }
    fn backup_and_repair(&self, loaded: &Loaded) -> io::Result<()> {
        let mut source = OpenOptions::new().read(true).write(true).open(&self.path)?;
        if source.metadata()?.len() != loaded.observed_len {
            return Err(invalid_data("dict: file changed before repair"));
        }
        let name = self
            .path
            .file_name()
            .ok_or_else(|| invalid_data("dict: missing filename"))?
            .to_string_lossy();
        let mut backup = None;
        for index in 1..=999_999u32 {
            let path = self
                .path
                .with_file_name(format!("{name}.recovery-{index:06}.bak"));
            match OpenOptions::new().create_new(true).write(true).open(path) {
                Ok(file) => {
                    backup = Some(file);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let mut backup =
            backup.ok_or_else(|| invalid_data("dict: recovery backup numbers exhausted"))?;
        let copied = io::copy(&mut source, &mut backup)?;
        backup.sync_all()?;
        if copied != loaded.observed_len || source.metadata()?.len() != loaded.observed_len {
            return Err(invalid_data(
                "dict: file changed during backup; original preserved",
            ));
        }
        source.set_len(loaded.valid_len)?;
        if loaded.valid_len == 0 {
            source.seek(SeekFrom::Start(0))?;
            source.write_all(HEADER)?;
        }
        source.sync_all()
    }
    pub fn intern(&mut self, value: &str) -> io::Result<u32> {
        let id = self.intern_buffered(value)?;
        self.sync_pending()?;
        Ok(id)
    }

    /// Writer-only batching. The log writer must call sync_pending before
    /// writing any record that references one of these IDs.
    pub(crate) fn intern_buffered(&mut self, value: &str) -> io::Result<u32> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dict: intern requires open_writer",
            ));
        }
        if self.failed {
            return Err(io::Error::other(
                "dict writer failed; reopen before retrying",
            ));
        }
        if let Some(id) = self.by_string.get(value) {
            return Ok(*id);
        }
        if value.len() > MAX_ENTRY_BYTES || value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dict: value is too long or contains NUL",
            ));
        }
        let id = self.next_id;
        let next = id
            .checked_add(1)
            .ok_or_else(|| invalid_data("dict: id space exhausted"))?;
        let mut wire = Vec::with_capacity(8 + value.len());
        wire.extend_from_slice(&id.to_le_bytes());
        wire.extend_from_slice(&(value.len() as u32).to_le_bytes());
        wire.extend_from_slice(value.as_bytes());
        let result = (|| {
            if self.pending_file.is_none() {
                self.pending_file = Some(OpenOptions::new().append(true).open(&self.path)?);
            }
            self.pending_file
                .as_mut()
                .expect("append file opened")
                .write_all(&wire)
        })();
        if let Err(error) = result {
            self.failed = true;
            return Err(error);
        }
        self.next_id = next;
        let value: Arc<str> = Arc::from(value);
        self.by_id.push(Some(Arc::clone(&value)));
        self.by_string.insert(value, id);
        Ok(id)
    }

    pub(crate) fn sync_pending(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "dict writer failed; reopen before retrying",
            ));
        }
        if let Some(file) = &self.pending_file {
            if let Err(error) = file.sync_data() {
                self.failed = true;
                return Err(error);
            }
            // Release the append handle while idle. Readers still see the
            // same append-only file; no dictionary/log ordering is weakened.
            self.pending_file = None;
        }
        Ok(())
    }

    #[cfg(all(test, windows))]
    pub(crate) fn make_pending_sync_handle_read_only(&mut self) {
        assert!(self.pending_file.is_some());
        self.pending_file = Some(File::open(&self.path).unwrap());
    }
    pub fn get(&self, id: u32) -> Option<&str> {
        self.by_id
            .get(id as usize)
            .and_then(|value| value.as_deref())
    }
    pub fn len(&self) -> usize {
        self.by_string.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_string.is_empty()
    }
}
fn validate_partial_header(bytes: &[u8], id: u32) -> io::Result<()> {
    let expected = id.to_le_bytes();
    let prefix_len = bytes.len().min(4);
    if bytes[..prefix_len] != expected[..prefix_len] {
        return Err(invalid_data(
            "dict: partial tail has unexpected id; original preserved",
        ));
    }
    if bytes.len() > 4 {
        let mut length = [0u8; 4];
        length[..bytes.len() - 4].copy_from_slice(&bytes[4..]);
        if u32::from_le_bytes(length) as usize > MAX_ENTRY_BYTES {
            return Err(invalid_data(
                "dict: partial tail length exceeds limit; original preserved",
            ));
        }
    }
    Ok(())
}
fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

// A per-test-thread fault replaces an already-opened writable handle with a
// readable one. The real Windows sync then fails, rather than failing at open.
#[cfg(all(test, windows))]
thread_local! {
    static READ_ONLY_STARTUP_SYNC: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, windows))]
pub(crate) fn startup_sync_handle(path: &Path, file: File) -> io::Result<File> {
    if READ_ONLY_STARTUP_SYNC.with(|fault| fault.borrow().as_deref() == Some(path)) {
        File::open(path)
    } else {
        Ok(file)
    }
}

#[cfg(all(test, windows))]
pub(crate) fn with_read_only_startup_sync<R>(path: &Path, action: impl FnOnce() -> R) -> R {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            READ_ONLY_STARTUP_SYNC.with(|fault| *fault.borrow_mut() = self.0.take());
        }
    }
    let _restore =
        Restore(READ_ONLY_STARTUP_SYNC.with(|fault| fault.replace(Some(path.to_owned()))));
    action()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_snapshot_catches_up_after_dictionary_then_log_append() {
        use crate::storage::crypto::{Cipher, MasterKey};
        use crate::storage::log::{LogDate, LogReader, LogWriter};
        use crate::storage::model::Record;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("apps.dict");
        let mut writer = Dict::open_writer(&path).unwrap();
        writer.intern("old.exe").unwrap();
        let mut query = DictReader::open(&path).unwrap();

        // The CLI has already loaded the dictionary when the daemon appends
        // a new ID, synchronizes it, and commits a referring log block.
        let new_id = writer.intern("new.exe").unwrap();
        assert!(query.snapshot.get(new_id).is_none());
        let date = LogDate {
            year: 2026,
            month: 9,
            day: 5,
        };
        let cipher = Cipher::new(&MasterKey::new_random());
        let log_path = dir.path().join("2026-09-05.log");
        LogWriter::open(&log_path, cipher.clone(), date)
            .unwrap()
            .write_block(&[Record {
                start_offset_secs: 0,
                duration_secs: 60,
                app_id: new_id,
                title_id: 0,
                flags: 0,
            }])
            .unwrap();

        let records = LogReader::new(cipher, date).read_all(&log_path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(query.resolve(records[0].app_id).unwrap(), "new.exe");
        assert_eq!(query.resolve(1).unwrap(), "old.exe");
    }

    #[test]
    fn query_rejects_a_missing_reference_without_creating_a_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.dict");
        let mut query = DictReader::open(&path).unwrap();
        let error = query.resolve(7).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("missing dictionary id 7"));
        assert!(!path.exists());
    }

    #[test]
    fn referenced_truncated_ids_are_never_repaired_or_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        let mut writer = Dict::open_writer(&path).unwrap();
        writer.intern("old.exe").unwrap();
        let first_end = std::fs::metadata(&path).unwrap().len() as usize;
        writer.intern("referenced.exe").unwrap();
        drop(writer);
        let complete = std::fs::read(&path).unwrap();

        for len in [0, 3, first_end, first_end + 3, first_end + 10] {
            let truncated = &complete[..len];
            std::fs::write(&path, truncated).unwrap();
            let error = Dict::open_writer_checked(&path, 2).err().unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&path).unwrap(), truncated);
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn referenced_missing_dictionary_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("missing");
        let path = parent.join("a.dict");
        let error = Dict::open_writer_checked(&path, 1).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!parent.exists());
    }

    #[test]
    fn read_only_never_creates_or_repairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        assert!(Dict::open(&path).unwrap().is_empty());
        assert!(!path.exists());
        let mut writer = Dict::open_writer(&path).unwrap();
        assert_eq!(writer.intern("中文🚀").unwrap(), 1);
        drop(writer);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(b"part");
        std::fs::write(&path, &bytes).unwrap();
        let mut reader = Dict::open(&path).unwrap();
        assert_eq!(reader.get(1), Some("中文🚀"));
        assert!(reader.intern("new").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn incomplete_tail_is_backed_up_before_repair_and_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        let mut dict = Dict::open_writer(&path).unwrap();
        dict.intern("old").unwrap();
        drop(dict);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(b"part");
        std::fs::write(&path, &bytes).unwrap();
        let mut dict = Dict::open_writer_checked(&path, 1).unwrap();
        assert_eq!(dict.intern("new").unwrap(), 2);
        drop(dict);
        assert_eq!(
            std::fs::read(dir.path().join("a.dict.recovery-000001.bak")).unwrap(),
            bytes
        );
        let reader = Dict::open(&path).unwrap();
        assert_eq!(reader.get(1), Some("old"));
        assert_eq!(reader.get(2), Some("new"));
    }
    #[test]
    fn structural_error_preserves_original_without_backup_or_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        for (id, len) in [(7u32, 2u32), (1, u32::MAX)] {
            let mut bytes = HEADER.to_vec();
            bytes.extend_from_slice(&id.to_le_bytes());
            bytes.extend_from_slice(&len.to_le_bytes());
            std::fs::write(&path, &bytes).unwrap();
            assert!(Dict::open_writer(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn failed_append_cannot_be_acknowledged_or_retried_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        let mut dict = Dict::open_writer(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(dict.intern("new").is_err());
        assert!(dict.is_empty());
        assert!(!path.exists());
        assert!(dict.intern("new").is_err());
    }
}
