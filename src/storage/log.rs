//! V1 encrypted daily logs. Damaged files remain intact; writing continues in a numbered part.
use super::crypto::{Cipher, NONCE_LEN, TAG_LEN};
use super::model::{Record, RECORD_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
const MAGIC: &[u8; 4] = b"RTNL";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const BLOCK_OVERHEAD: usize = 4 + NONCE_LEN + TAG_LEN;
pub(crate) const MAX_WRITE_BLOCK_RECORDS: usize = 4096;
const MAX_READ_BLOCK_BYTES: usize = (1 << 20) * RECORD_SIZE;
const MAX_PART: u32 = 999_999;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LogDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}
impl LogDate {
    pub fn pack(self) -> u32 {
        ((self.year as u32 & 0xFFFF) << 16) | ((self.month & 0xFF) << 8) | (self.day & 0xFF)
    }
    pub fn unpack(p: u32) -> Self {
        Self {
            year: ((p >> 16) & 0xFFFF) as i32,
            month: (p >> 8) & 0xFF,
            day: p & 0xFF,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogDamage {
    pub path: PathBuf,
    pub authenticated_records: usize,
    pub reason: String,
}
#[derive(Debug, Default)]
pub struct DayRead {
    pub records: Vec<Record>,
    pub damage: Vec<LogDamage>,
}

pub struct LogWriter {
    file: File,
    cipher: Cipher,
    date: LogDate,
    block_index: u32,
    failed: bool,
    wire: Vec<u8>,
}
impl LogWriter {
    /// Open one clean v1 file. Never repair or truncate an existing file.
    pub fn open(path: &Path, cipher: Cipher, date: LogDate) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let block_index = if file.metadata()?.len() == 0 {
            file.write_all(&encoded_header(date))?;
            file.sync_all()?;
            0
        } else {
            let scan = scan_file(&file, &cipher, date, ScanMode::Validate)?;
            if let Some(reason) = scan.damage {
                return Err(invalid_data(format!(
                    "{}: {reason}; original file preserved",
                    path.display()
                )));
            }
            scan.blocks
        };
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            cipher,
            date,
            block_index,
            failed: false,
            wire: Vec::new(),
        })
    }
    /// Check all existing parts with this key, then append to the latest clean
    /// part or create a new one without touching damaged bytes.
    pub fn open_daily(base_path: &Path, cipher: Cipher, date: LogDate) -> io::Result<Self> {
        let paths = daily_paths(base_path)?;
        let Some((last_number, _)) = paths.last() else {
            return Self::open(base_path, cipher, date);
        };
        for (number, path) in &paths {
            let latest = number == last_number;
            let (mut file, write_error) =
                match OpenOptions::new().read(true).write(latest).open(path) {
                    Ok(file) => (file, None),
                    Err(error) if latest => (File::open(path)?, Some(error)),
                    Err(error) => return Err(error),
                };
            let observed_len = file.metadata()?.len();
            let scan = scan_file(&file, &cipher, date, ScanMode::Validate)?;
            require_verified_damage(path, &scan)?;
            if latest && scan.damage.is_none() {
                if let Some(error) = write_error {
                    return Err(error);
                }
                // Keep the checked handle rather than opening and decrypting
                // the same file again. Do not append after an unseen change.
                if file.metadata()?.len() != observed_len {
                    return Err(invalid_data("log changed while opening writer"));
                }
                if observed_len == 0 {
                    file.write_all(&encoded_header(date))?;
                    file.sync_all()?;
                }
                file.seek(SeekFrom::End(0))?;
                return Ok(Self {
                    file,
                    cipher,
                    date,
                    block_index: scan.blocks,
                    failed: false,
                    wire: Vec::new(),
                });
            }
        }
        let number = last_number
            .checked_add(1)
            .filter(|n| *n <= MAX_PART)
            .ok_or_else(|| invalid_data("log: daily part numbers exhausted"))?;
        let path = part_path(base_path, number)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)?;
        file.write_all(&encoded_header(date))?;
        file.sync_all()?;
        Ok(Self {
            file,
            cipher,
            date,
            block_index: 0,
            failed: false,
            wire: Vec::new(),
        })
    }
    pub fn write_block(&mut self, records: &[Record]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "log writer failed; reopen before retrying",
            ));
        }
        if records.is_empty() {
            return Ok(());
        }
        if records.len() > MAX_WRITE_BLOCK_RECORDS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log: block exceeds 4096 records",
            ));
        }
        let next = self
            .block_index
            .checked_add(1)
            .ok_or_else(|| invalid_data("log: block index exhausted"))?;
        self.cipher.seal_block_with(
            records.len() * RECORD_SIZE,
            &make_aad(self.date, self.block_index),
            &mut self.wire,
            |out| {
                for record in records {
                    record.write_to(out);
                }
            },
        );
        if let Err(error) = self
            .file
            .write_all(&self.wire)
            .and_then(|()| self.file.sync_data())
        {
            self.failed = true;
            return Err(error);
        }
        self.block_index = next;
        Ok(())
    }
}
pub struct LogReader {
    cipher: Cipher,
    date: LogDate,
}
impl LogReader {
    pub fn new(cipher: Cipher, date: LogDate) -> Self {
        Self { cipher, date }
    }
    /// Single-file compatibility helper. Prefer read_day to expose damage.
    pub fn read_all(&self, path: &Path) -> io::Result<Vec<Record>> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let scan = scan_file(&file, &self.cipher, self.date, ScanMode::Records)?;
        require_verified_damage(path, &scan)?;
        Ok(scan.records)
    }
    /// Read base followed by numbered parts, retaining only authenticated prefixes.
    pub fn read_day(&self, base_path: &Path) -> io::Result<DayRead> {
        let mut day = DayRead::default();
        for (_, path) in daily_paths(base_path)? {
            let scan = scan_file(
                &File::open(&path)?,
                &self.cipher,
                self.date,
                ScanMode::Records,
            )?;
            require_verified_damage(&path, &scan)?;
            if let Some(reason) = scan.damage {
                day.damage.push(LogDamage {
                    path,
                    authenticated_records: scan.record_count,
                    reason,
                });
            }
            day.records.extend(scan.records);
        }
        Ok(day)
    }
}
struct Scan {
    records: Vec<Record>,
    record_count: usize,
    blocks: u32,
    damage: Option<String>,
    max_app_id: u32,
    max_title_id: u32,
}
#[derive(Clone, Copy)]
enum ScanMode {
    Validate,
    Records,
    DictionaryIds,
}
fn require_verified_damage(path: &Path, scan: &Scan) -> io::Result<()> {
    if scan.damage.is_some() && scan.blocks == 0 {
        return Err(invalid_data(format!(
            "{}: no block authenticates; wrong key or unrecoverable file, original preserved",
            path.display()
        )));
    }
    Ok(())
}
fn scan_file(file: &File, cipher: &Cipher, date: LogDate, mode: ScanMode) -> io::Result<Scan> {
    let len = file.metadata()?.len();
    let mut scan = Scan {
        records: Vec::new(),
        record_count: 0,
        blocks: 0,
        damage: None,
        max_app_id: 0,
        max_title_id: 0,
    };
    if len == 0 {
        return Ok(scan);
    }
    if len < HEADER_LEN as u64 {
        return Err(invalid_data("log: truncated header; original preserved"));
    }
    let mut input = BufReader::new(file.take(len));
    let mut header = [0u8; HEADER_LEN];
    input.read_exact(&mut header)?;
    if header[..4] != MAGIC[..]
        || u32::from_le_bytes(header[4..8].try_into().unwrap()) != VERSION
        || LogDate::unpack(u32::from_le_bytes(header[8..12].try_into().unwrap())) != date
    {
        return Err(invalid_data(
            "log: invalid header or date; original preserved",
        ));
    }
    let mut cursor = HEADER_LEN as u64;
    let mut wire = Vec::new();
    while cursor < len {
        if len - cursor < 4 {
            scan.damage = Some(format!("truncated block header at byte {cursor}"));
            break;
        }
        let mut length = [0u8; 4];
        input.read_exact(&mut length)?;
        let plaintext_len = u32::from_le_bytes(length) as usize;
        if plaintext_len == 0
            || !plaintext_len.is_multiple_of(RECORD_SIZE)
            || plaintext_len > MAX_READ_BLOCK_BYTES
        {
            scan.damage = Some(format!("invalid block length at byte {cursor}"));
            break;
        }
        let total = plaintext_len + BLOCK_OVERHEAD;
        if total as u64 > len - cursor {
            scan.damage = Some(format!("truncated block at byte {cursor}"));
            break;
        }
        wire.resize(total, 0);
        wire[..4].copy_from_slice(&length);
        input.read_exact(&mut wire[4..])?;
        match cipher.open_block_in_place(&mut wire, &make_aad(date, scan.blocks)) {
            Ok(plaintext) => {
                scan.record_count += plaintext.len() / RECORD_SIZE;
                match mode {
                    ScanMode::Validate => {}
                    ScanMode::Records => scan.records.extend(
                        plaintext
                            .chunks_exact(RECORD_SIZE)
                            .map(|bytes| Record::read_from(bytes).expect("complete record")),
                    ),
                    ScanMode::DictionaryIds => {
                        for bytes in plaintext.chunks_exact(RECORD_SIZE) {
                            let record = Record::read_from(bytes).expect("complete record");
                            scan.max_app_id = scan.max_app_id.max(record.app_id);
                            scan.max_title_id = scan.max_title_id.max(record.title_id);
                        }
                    }
                }
            }
            Err(error) => {
                scan.damage = Some(format!("{error} at byte {cursor}"));
                break;
            }
        }
        scan.blocks = scan
            .blocks
            .checked_add(1)
            .ok_or_else(|| invalid_data("log: block index exhausted"))?;
        cursor += total as u64;
    }
    Ok(scan)
}

/// Startup preflight: a restored/truncated dictionary must not reuse an ID
/// still referenced by authenticated history. Stream one block at a time.
pub(crate) fn referenced_dictionary_ids(root: &Path, cipher: &Cipher) -> io::Result<(u32, u32)> {
    let mut pending = vec![root.to_path_buf()];
    let mut max_ids = (0, 0);
    while let Some(directory) = pending.pop() {
        let entries = match super::crypto::checked_data_entries(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let kind = super::crypto::checked_data_entry_type(&entry)?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() && super::crypto::has_log_extension(&entry.path()) {
                let path = entry.path();
                let mut file = File::open(&path)?;
                if file.metadata()?.len() == 0 {
                    continue;
                }
                let mut header = [0u8; HEADER_LEN];
                file.read_exact(&mut header).map_err(|error| {
                    invalid_data(format!(
                        "{}: cannot read log header: {error}",
                        path.display()
                    ))
                })?;
                let date = LogDate::unpack(u32::from_le_bytes(header[8..12].try_into().unwrap()));
                file.seek(SeekFrom::Start(0))?;
                let scan = scan_file(&file, cipher, date, ScanMode::DictionaryIds)?;
                require_verified_damage(&path, &scan)?;
                max_ids.0 = max_ids.0.max(scan.max_app_id);
                max_ids.1 = max_ids.1.max(scan.max_title_id);
            }
        }
    }
    Ok(max_ids)
}
fn daily_paths(base_path: &Path) -> io::Result<Vec<(u32, PathBuf)>> {
    let mut paths = Vec::new();
    let parent = base_path.parent().unwrap_or_else(|| Path::new("."));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error),
    };
    let base_name = base_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_data("log: invalid base filename"))?;
    let prefix = format!("{}.part-", base_stem(base_path)?);
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Treat v1 filenames as ASCII case-insensitive on every platform,
        // matching ordinary Windows restore/backup behavior. On a filesystem
        // that permits case-only duplicates, reject ambiguity below.
        if name.eq_ignore_ascii_case(base_name) {
            paths.push((0, entry.path()));
            continue;
        }
        let Some(suffix) = name.get(prefix.len()..) else {
            continue;
        };
        if !name[..prefix.len()].eq_ignore_ascii_case(&prefix) {
            continue;
        }
        let Some(number) = suffix.get(..6) else {
            continue;
        };
        if suffix
            .get(6..)
            .is_none_or(|extension| !extension.eq_ignore_ascii_case(".log"))
            || !number.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let number: u32 = number.parse().expect("six ASCII digits");
        if number > 0 {
            paths.push((number, entry.path()));
        }
    }
    paths.sort_by_key(|(number, _)| *number);
    if paths.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(invalid_data(
            "log: ambiguous filenames for the same daily part; originals preserved",
        ));
    }
    Ok(paths)
}
fn base_stem(path: &Path) -> io::Result<&str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid_data("log: invalid base filename"))
}
fn part_path(base_path: &Path, number: u32) -> io::Result<PathBuf> {
    Ok(base_path.with_file_name(format!("{}.part-{number:06}.log", base_stem(base_path)?)))
}
fn encoded_header(date: LogDate) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&date.pack().to_le_bytes());
    header
}
fn make_aad(date: LogDate, block_index: u32) -> [u8; 12] {
    let mut aad = [0u8; 12];
    aad[..4].copy_from_slice(MAGIC);
    aad[4..8].copy_from_slice(&date.pack().to_le_bytes());
    aad[8..12].copy_from_slice(&block_index.to_le_bytes());
    aad
}
fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::crypto::MasterKey;
    fn d() -> LogDate {
        LogDate {
            year: 2026,
            month: 9,
            day: 5,
        }
    }
    fn record(id: u32) -> Record {
        Record {
            start_offset_secs: id * 10,
            duration_secs: 5,
            app_id: id,
            title_id: 0,
            flags: 0,
        }
    }
    fn write_blocks(path: &Path, key: &MasterKey, ids: &[u32]) {
        let mut writer = LogWriter::open_daily(path, Cipher::new(key), d()).unwrap();
        for id in ids {
            writer.write_block(&[record(*id)]).unwrap();
        }
    }
    #[test]
    fn clean_reopen_appends_without_a_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[1, 2]);
        write_blocks(&path, &key, &[3]);
        let day = LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .unwrap();
        assert_eq!(day.records, vec![record(1), record(2), record(3)]);
        assert!(day.damage.is_empty());
        assert_eq!(daily_paths(&path).unwrap().len(), 1);
    }

    #[test]
    fn uppercase_base_and_parts_are_verified_read_and_numbered_consistently() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("2026-09-05.log");
        let uppercase_base = base.with_extension("LOG");
        let part = dir.path().join("2026-09-05.PART-000007.LOG");
        let key = MasterKey::new_random();
        let cipher = Cipher::new(&key);
        write_blocks(&uppercase_base, &key, &[1]);
        let mut writer = LogWriter::open(&part, cipher.clone(), d()).unwrap();
        writer.write_block(&[record(2)]).unwrap();
        writer.write_block(&[record(3)]).unwrap();
        drop(writer);
        let mut damaged = std::fs::read(&part).unwrap();
        damaged.truncate(damaged.len() - 5);
        std::fs::write(&part, &damaged).unwrap();

        assert_eq!(
            referenced_dictionary_ids(dir.path(), &cipher).unwrap(),
            (2, 0)
        );
        let read = LogReader::new(cipher.clone(), d()).read_day(&base).unwrap();
        assert_eq!(read.records, vec![record(1), record(2)]);
        assert_eq!(read.damage.len(), 1);
        LogWriter::open_daily(&base, cipher.clone(), d())
            .unwrap()
            .write_block(&[record(4)])
            .unwrap();
        assert!(part_path(&base, 8).unwrap().exists());
        assert_eq!(std::fs::read(&part).unwrap(), damaged);
        let read = LogReader::new(cipher, d()).read_day(&base).unwrap();
        assert_eq!(read.records, vec![record(1), record(2), record(4)]);
        assert_eq!(read.damage.len(), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn case_only_duplicate_parts_are_rejected_instead_of_choosing_an_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("2026-09-05.log");
        std::fs::write(dir.path().join("2026-09-05.part-000001.log"), b"first").unwrap();
        std::fs::write(dir.path().join("2026-09-05.PART-000001.LOG"), b"second").unwrap();
        assert_eq!(
            daily_paths(&base).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn differently_sized_blocks_round_trip_with_reused_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        let mut expected = Vec::new();
        for count in [1, 4096, 3, 256, 1] {
            let records: Vec<_> = (1..=count).map(record).collect();
            writer.write_block(&records).unwrap();
            expected.extend(records);
        }
        drop(writer);
        let day = LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .unwrap();
        assert!(day.damage.is_empty());
        assert_eq!(day.records, expected);
    }

    #[test]
    #[cfg(windows)]
    fn read_only_damaged_log_can_continue_in_a_new_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[1, 2]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.pop();
        std::fs::write(&path, &bytes).unwrap();
        let original_permissions = std::fs::metadata(&path).unwrap().permissions();
        let mut read_only = original_permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(&path, read_only).unwrap();
        let continued = LogWriter::open_daily(&path, Cipher::new(&key), d());
        std::fs::set_permissions(&path, original_permissions).unwrap();
        continued.unwrap().write_block(&[record(3)]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let day = LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .unwrap();
        assert_eq!(day.records, vec![record(1), record(3)]);
        assert_eq!(day.damage.len(), 1);
    }
    #[test]
    fn two_missing_blocks_preserve_original_and_continue_in_part() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[1, 2, 3, 4]);
        let bytes = std::fs::read(&path).unwrap();
        let wire_len = BLOCK_OVERHEAD + RECORD_SIZE;
        let mut damaged = bytes[..HEADER_LEN + wire_len].to_vec();
        damaged.extend_from_slice(&bytes[HEADER_LEN + 3 * wire_len..]);
        std::fs::write(&path, &damaged).unwrap();
        assert!(LogWriter::open(&path, Cipher::new(&key), d()).is_err());
        write_blocks(&path, &key, &[5]);
        write_blocks(&path, &key, &[6]);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        let day = LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .unwrap();
        assert_eq!(day.records, vec![record(1), record(5), record(6)]);
        assert_eq!(day.damage.len(), 1);
        assert_eq!(daily_paths(&path).unwrap().len(), 2);
    }
    #[test]
    fn torn_parts_survive_multiple_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[1, 2]);
        let mut original = std::fs::read(&path).unwrap();
        original.truncate(original.len() - 5);
        std::fs::write(&path, &original).unwrap();
        write_blocks(&path, &key, &[3, 4]);
        let part = part_path(&path, 1).unwrap();
        let mut part_bytes = std::fs::read(&part).unwrap();
        part_bytes.truncate(part_bytes.len() - 5);
        std::fs::write(&part, &part_bytes).unwrap();
        write_blocks(&path, &key, &[5]);
        write_blocks(&path, &key, &[6]);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&part).unwrap(), part_bytes);
        let day = LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .unwrap();
        assert_eq!(
            day.records,
            vec![record(1), record(3), record(5), record(6)]
        );
        assert_eq!(day.damage.len(), 2);
        assert_eq!(daily_paths(&path).unwrap().len(), 3);
    }
    #[test]
    fn wrong_key_and_unverified_first_tail_do_not_create_parts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[1]);
        let original = std::fs::read(&path).unwrap();
        assert!(LogWriter::open_daily(&path, Cipher::new(&MasterKey::new_random()), d()).is_err());
        let truncated = &original[..original.len() - 5];
        std::fs::write(&path, truncated).unwrap();
        assert!(LogWriter::open_daily(&path, Cipher::new(&key), d()).is_err());
        assert!(LogReader::new(Cipher::new(&key), d())
            .read_day(&path)
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), truncated);
        assert_eq!(daily_paths(&path).unwrap().len(), 1);
    }
    #[test]
    fn oversized_block_is_rejected_without_poisoning_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        assert!(writer
            .write_block(&vec![record(1); MAX_WRITE_BLOCK_RECORDS + 1])
            .is_err());
        writer.write_block(&[record(2)]).unwrap();
        drop(writer);
        assert_eq!(
            LogReader::new(Cipher::new(&key), d())
                .read_all(&path)
                .unwrap(),
            vec![record(2)]
        );
    }
    #[test]
    fn failed_write_is_reported_and_poisoned() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        writer.file = File::open(&path).unwrap();
        assert!(writer.write_block(&[record(1)]).is_err());
        assert!(writer.failed);
        assert!(writer.write_block(&[record(2)]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), encoded_header(d()));
    }
    #[test]
    fn partial_header_is_invalid_data_and_never_modified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-09-05.log");
        let key = MasterKey::new_random();
        std::fs::write(&path, b"RTN").unwrap();
        let error = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(LogWriter::open_daily(&path, Cipher::new(&key), d()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"RTN");
        assert_eq!(daily_paths(&path).unwrap().len(), 1);
    }
}
