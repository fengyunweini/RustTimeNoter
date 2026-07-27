//! 每日 `.log` 文件的读写。
//!
//! 文件布局（小端）：
//! ```text
//! HEADER (16B):
//!   magic       : [u8;4] = b"RTNL"
//!   version     : u32    = 1
//!   date_packed : u32    (year<<16 | month<<8 | day)
//!   reserved    : u32    = 0
//! BODY: 一串加密 block。
//!   每个 block: [u32 plain_len][12B nonce][ciphertext+tag]
//!   AAD = magic(4) || date_packed(4) || block_index(4)
//! 解密后的 plaintext = 连续若干 [Record;15B]。
//! ```
//!
//! 崩溃恢复：逐块认证并保留最后一个认证成功的偏移；writer 仅截断可确认的损坏尾块。
//! 中部或无法确认是尾部的认证失败会显式报错并保留原文件。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::crypto::{Cipher, CryptoError, NONCE_LEN, TAG_LEN};
use super::model::{Record, RECORD_SIZE};

const MAGIC: &[u8; 4] = b"RTNL";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 4 + 4;
const BLOCK_OVERHEAD: usize = 4 + NONCE_LEN + TAG_LEN;
pub(crate) const MAX_WRITE_BLOCK_RECORDS: usize = 4_096;
// Keep bounded compatibility for files produced by historical custom configs,
// while all new writes stay around 60 KiB of plaintext per block.
const MAX_READ_BLOCK_RECORDS: usize = 1 << 20;
const MAX_BLOCK_PLAINTEXT: usize = MAX_READ_BLOCK_RECORDS * RECORD_SIZE;

// Recovery is deliberately fail-closed.  The normal files are tiny, but a
// corrupt local file must not make startup perform unbounded resynchronisation
// work.  Exhausting either budget reports InvalidData and leaves the file
// untouched.
const MAX_RECOVERY_SCAN_BYTES: usize = 16 * 1024 * 1024;
const MAX_RECOVERY_AUTH_CANDIDATES: usize = 256;
const MAX_RECOVERY_AUTH_BYTES: usize = 16 * 1024 * 1024;
const MAX_LOG_FILE_BYTES: usize = 64 * 1024 * 1024;

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

pub struct LogWriter {
    file: File,
    cipher: Cipher,
    date: LogDate,
    block_index: u32,
    file_len: u64,
}

impl LogWriter {
    /// 打开或创建当日日志。若已存在，扫描现有 block 数确定下一个 index。
    pub fn open(path: &Path, cipher: Cipher, date: LogDate) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        let len = f.metadata()?.len();
        let (block_index, file_len) = if len < HEADER_LEN as u64 {
            let mut existing = Vec::with_capacity(len as usize);
            f.read_to_end(&mut existing)?;
            let expected = encoded_header(date);
            if !expected.starts_with(&existing) {
                return Err(invalid_data(
                    "log: partial header does not match the expected header; refusing to repair",
                ));
            }
            f.set_len(0)?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&expected)?;
            f.sync_data()?;
            (0, HEADER_LEN as u64)
        } else {
            let buf = read_limited(&mut f, MAX_LOG_FILE_BYTES)?;
            let scan = scan_blocks(&buf, &cipher, date, false)?;
            if scan.tail_damage.is_some() {
                if scan.next_block_index == 0 {
                    return Err(invalid_data(
                        "log: damage occurs before any block authenticates; refusing to truncate \
                         with an unverified key",
                    ));
                }
                f.set_len(scan.authenticated_end as u64)?;
                f.sync_data()?;
            }
            f.seek(SeekFrom::Start(scan.authenticated_end as u64))?;
            (scan.next_block_index, scan.authenticated_end as u64)
        };
        Ok(Self {
            file: f,
            cipher,
            date,
            block_index,
            file_len,
        })
    }

    /// 写入一个 block（一组 record 的明文）。
    pub fn write_block(&mut self, records: &[Record]) -> std::io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        if records.len() > MAX_WRITE_BLOCK_RECORDS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "log: block has {} records; maximum is {MAX_WRITE_BLOCK_RECORDS}",
                    records.len()
                ),
            ));
        }
        let plaintext_len = records.len().checked_mul(RECORD_SIZE).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "log: block plaintext length overflow",
            )
        })?;
        let mut plaintext = Vec::with_capacity(plaintext_len);
        for r in records {
            r.write_to(&mut plaintext);
        }
        let next_block_index = self.block_index.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "log: block index exhausted",
            )
        })?;
        let aad = make_aad(self.date, self.block_index);
        let wire = self.cipher.seal_block(&plaintext, &aad);
        let next_file_len = self
            .file_len
            .checked_add(wire.len() as u64)
            .ok_or_else(|| invalid_data("log: file length overflow"))?;
        if next_file_len > MAX_LOG_FILE_BYTES as u64 {
            return Err(invalid_data(format!(
                "log: daily file would exceed the {MAX_LOG_FILE_BYTES}-byte safety limit"
            )));
        }
        self.file.write_all(&wire)?;
        self.file.sync_data()?;
        self.block_index = next_block_index;
        self.file_len = next_file_len;
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

    /// 读取整个日志文件的全部记录。
    pub fn read_all(&self, path: &Path) -> std::io::Result<Vec<Record>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let buf = read_limited(&mut file, MAX_LOG_FILE_BYTES)?;
        if buf.is_empty() {
            return Ok(Vec::new());
        }
        if buf.len() < HEADER_LEN && encoded_header(self.date).starts_with(&buf) {
            return Ok(Vec::new());
        }
        Ok(scan_blocks(&buf, &self.cipher, self.date, true)?.records)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailDamage {
    Truncated,
    Authentication,
}

struct BlockScan {
    records: Vec<Record>,
    authenticated_end: usize,
    next_block_index: u32,
    tail_damage: Option<TailDamage>,
}

fn scan_blocks(
    buf: &[u8],
    cipher: &Cipher,
    date: LogDate,
    collect_records: bool,
) -> std::io::Result<BlockScan> {
    validate_header(buf, date)?;

    let mut records = Vec::new();
    let mut cursor = HEADER_LEN;
    let mut block_index = 0u32;

    while cursor < buf.len() {
        let remaining = &buf[cursor..];
        if remaining.len() < 4 {
            return Ok(BlockScan {
                records,
                authenticated_end: cursor,
                next_block_index: block_index,
                tail_damage: Some(TailDamage::Truncated),
            });
        }

        let plaintext_len =
            u32::from_le_bytes(remaining[0..4].try_into().expect("four bytes checked")) as usize;
        if plaintext_len == 0
            || !plaintext_len.is_multiple_of(RECORD_SIZE)
            || plaintext_len > MAX_BLOCK_PLAINTEXT
        {
            return Err(invalid_data(format!(
                "log: block {block_index} declares invalid plaintext length {plaintext_len}"
            )));
        }
        let total = plaintext_len.checked_add(BLOCK_OVERHEAD).ok_or_else(|| {
            invalid_data(format!(
                "log: block {block_index} length overflows at byte {cursor}"
            ))
        })?;
        if remaining.len() < total {
            ensure_no_authenticated_successor(buf, cursor, block_index, cipher, date)?;
            return Ok(BlockScan {
                records,
                authenticated_end: cursor,
                next_block_index: block_index,
                tail_damage: Some(TailDamage::Truncated),
            });
        }

        let frame_end = cursor + total;
        let aad = make_aad(date, block_index);
        let plaintext = match cipher.open_block(&buf[cursor..frame_end], &aad) {
            Ok((plaintext, consumed)) if consumed == total => plaintext,
            Ok(_) => {
                return Err(invalid_data(format!(
                    "log: inconsistent block length at block {block_index}, byte {cursor}"
                )))
            }
            Err(CryptoError::Auth) if frame_end == buf.len() && block_index > 0 => {
                ensure_no_authenticated_successor(buf, cursor, block_index, cipher, date)?;
                return Ok(BlockScan {
                    records,
                    authenticated_end: cursor,
                    next_block_index: block_index,
                    tail_damage: Some(TailDamage::Authentication),
                });
            }
            Err(CryptoError::Auth) => {
                return Err(invalid_data(format!(
                    "log: authentication failed at block {block_index}, byte {cursor}; \
                     refusing to recover non-tail or unverified corruption"
                )))
            }
            Err(CryptoError::Truncated) => {
                return Err(invalid_data(format!(
                    "log: inconsistent truncated block {block_index} at byte {cursor}"
                )))
            }
            Err(CryptoError::Io(error)) => return Err(error),
        };

        if plaintext.len() % RECORD_SIZE != 0 {
            return Err(invalid_data(format!(
                "log: authenticated block {block_index} has invalid plaintext length {}",
                plaintext.len()
            )));
        }
        if collect_records {
            for chunk in plaintext.chunks_exact(RECORD_SIZE) {
                let record = Record::read_from(chunk).ok_or_else(|| {
                    invalid_data(format!("log: invalid record in block {block_index}"))
                })?;
                records.push(record);
            }
        }

        cursor = frame_end;
        block_index = block_index.checked_add(1).ok_or_else(|| {
            invalid_data("log: block index exhausted while scanning existing file")
        })?;
    }

    Ok(BlockScan {
        records,
        authenticated_end: cursor,
        next_block_index: block_index,
        tail_damage: None,
    })
}

/// Proves, within fixed work budgets, that an apparently damaged tail does not
/// hide an authenticated block later in the suffix.
///
/// A corrupted length field in a middle block can otherwise make all following
/// blocks look like one truncated tail.  We linearly inspect possible frame
/// starts and only invoke AEAD for bounded, record-aligned candidates.  The
/// current index catches files produced by the historical "append after torn
/// bytes" bug; the next index catches a corrupted middle block.  If the proof
/// cannot be completed within budget, recovery is refused and the file remains
/// byte-for-byte untouched.
fn ensure_no_authenticated_successor(
    buf: &[u8],
    damaged_start: usize,
    block_index: u32,
    cipher: &Cipher,
    date: LogDate,
) -> std::io::Result<()> {
    let search_start = damaged_start.saturating_add(1);
    let search_len = buf.len().saturating_sub(search_start);
    if search_len > MAX_RECOVERY_SCAN_BYTES {
        return Err(invalid_data(format!(
            "log: recovery scan at block {block_index} exceeds {MAX_RECOVERY_SCAN_BYTES} bytes; \
             refusing to truncate"
        )));
    }
    if search_len < BLOCK_OVERHEAD {
        return Ok(());
    }

    let mut candidates = 0usize;
    let mut authenticated_bytes = 0usize;
    let last_start = buf.len() - BLOCK_OVERHEAD;
    for candidate_start in search_start..=last_start {
        let remaining = &buf[candidate_start..];
        let plaintext_len =
            u32::from_le_bytes(remaining[0..4].try_into().expect("four bytes checked")) as usize;
        if plaintext_len == 0
            || !plaintext_len.is_multiple_of(RECORD_SIZE)
            || plaintext_len > MAX_BLOCK_PLAINTEXT
        {
            continue;
        }
        let Some(total) = plaintext_len.checked_add(BLOCK_OVERHEAD) else {
            continue;
        };
        if total > remaining.len() {
            continue;
        }

        candidates = candidates.checked_add(1).ok_or_else(|| {
            invalid_data("log: recovery candidate counter overflow; refusing to truncate")
        })?;
        let authentication_attempts = 1 + usize::from(block_index.checked_add(1).is_some());
        let candidate_authentication_bytes =
            total.checked_mul(authentication_attempts).ok_or_else(|| {
                invalid_data("log: recovery authentication budget overflow; refusing to truncate")
            })?;
        authenticated_bytes = authenticated_bytes
            .checked_add(candidate_authentication_bytes)
            .ok_or_else(|| {
                invalid_data("log: recovery authentication budget overflow; refusing to truncate")
            })?;
        if candidates > MAX_RECOVERY_AUTH_CANDIDATES
            || authenticated_bytes > MAX_RECOVERY_AUTH_BYTES
        {
            return Err(invalid_data(format!(
                "log: recovery proof budget exhausted at block {block_index}; refusing to truncate"
            )));
        }

        let frame = &remaining[..total];
        let mut candidate_indices = [None, None];
        candidate_indices[0] = Some(block_index);
        candidate_indices[1] = block_index.checked_add(1);
        for candidate_index in candidate_indices.into_iter().flatten() {
            let aad = make_aad(date, candidate_index);
            if matches!(
                cipher.open_block(frame, &aad),
                Ok((plaintext, consumed))
                    if consumed == total && plaintext.len() % RECORD_SIZE == 0
            ) {
                return Err(invalid_data(format!(
                    "log: authenticated block {candidate_index} found after damage at block \
                     {block_index}; refusing to truncate non-tail data"
                )));
            }
        }
    }
    Ok(())
}

fn validate_header(buf: &[u8], expected_date: LogDate) -> std::io::Result<()> {
    if buf.len() < HEADER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "log: header truncated",
        ));
    }
    if &buf[0..4] != MAGIC {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "log: bad magic",
        ));
    }
    let ver = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if ver != VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "log: bad version",
        ));
    }
    let date_packed = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    if LogDate::unpack(date_packed) != expected_date {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "log: date mismatch",
        ));
    }
    Ok(())
}

fn encoded_header(date: LogDate) -> [u8; HEADER_LEN] {
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(MAGIC);
    header[4..8].copy_from_slice(&VERSION.to_le_bytes());
    header[8..12].copy_from_slice(&date.pack().to_le_bytes());
    header
}

fn read_limited(file: &mut File, limit: usize) -> std::io::Result<Vec<u8>> {
    let observed_len = file.metadata()?.len();
    if observed_len > limit as u64 {
        return Err(invalid_data(format!(
            "log: file is {observed_len} bytes; maximum supported size is {limit}"
        )));
    }
    let mut bytes = Vec::with_capacity(observed_len as usize);
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid_data(format!(
            "log: file grew beyond the {limit}-byte limit while it was read"
        )));
    }
    Ok(bytes)
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn make_aad(date: LogDate, block_index: u32) -> [u8; 12] {
    let mut aad = [0u8; 12];
    aad[0..4].copy_from_slice(MAGIC);
    aad[4..8].copy_from_slice(&date.pack().to_le_bytes());
    aad[8..12].copy_from_slice(&block_index.to_le_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::crypto::{Cipher, MasterKey};

    fn d() -> LogDate {
        LogDate {
            year: 2026,
            month: 4,
            day: 22,
        }
    }

    fn record(seed: u32) -> Record {
        Record {
            start_offset_secs: seed * 10,
            duration_secs: seed + 1,
            app_id: seed,
            title_id: seed + 100,
            flags: seed as u8,
        }
    }

    fn write_blocks(path: &Path, key: &MasterKey, records: &[Record]) {
        let mut writer = LogWriter::open(path, Cipher::new(key), d()).unwrap();
        for record in records {
            writer.write_block(std::slice::from_ref(record)).unwrap();
        }
    }

    fn block_ranges(bytes: &[u8]) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut cursor = HEADER_LEN;
        while cursor < bytes.len() {
            assert!(bytes.len() - cursor >= BLOCK_OVERHEAD);
            let plaintext_len =
                u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            let end = cursor + BLOCK_OVERHEAD + plaintext_len;
            assert!(end <= bytes.len());
            ranges.push((cursor, end));
            cursor = end;
        }
        ranges
    }

    fn assert_rejected_and_preserved(path: &Path, key: &MasterKey, damaged: &[u8], name: &str) {
        std::fs::write(path, damaged).unwrap();

        let read_error = LogReader::new(Cipher::new(key), d())
            .read_all(path)
            .expect_err("reader must reject non-tail corruption");
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData, "{name}");
        assert_eq!(std::fs::read(path).unwrap(), damaged, "{name}");

        let write_error = LogWriter::open(path, Cipher::new(key), d())
            .err()
            .expect("writer must reject non-tail corruption");
        assert_eq!(
            write_error.kind(),
            std::io::ErrorKind::InvalidData,
            "{name}"
        );
        assert_eq!(std::fs::read(path).unwrap(), damaged, "{name}");
    }

    #[test]
    fn write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let recs = vec![
            Record {
                start_offset_secs: 10,
                duration_secs: 5,
                app_id: 1,
                title_id: 0,
                flags: 0,
            },
            Record {
                start_offset_secs: 20,
                duration_secs: 7,
                app_id: 2,
                title_id: 3u32,
                flags: 0,
            },
        ];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&recs).unwrap();
        }
        let r = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap();
        assert_eq!(r, recs);
    }

    #[test]
    fn matching_partial_headers_are_repaired_but_never_by_readers() {
        let expected = encoded_header(d());
        let key = MasterKey::new_random();

        for retained in 0..HEADER_LEN {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("header-{retained}.log"));
            std::fs::write(&path, &expected[..retained]).unwrap();

            let reader = LogReader::new(Cipher::new(&key), d());
            assert!(reader.read_all(&path).unwrap().is_empty(), "{retained}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                &expected[..retained],
                "{retained}"
            );

            let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected, "{retained}");
            writer.write_block(&[record(1)]).unwrap();
            drop(writer);
            assert_eq!(reader.read_all(&path).unwrap(), vec![record(1)]);
        }
    }

    #[test]
    fn mismatching_partial_header_is_rejected_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-header.log");
        let key = MasterKey::new_random();
        let mut damaged = encoded_header(d())[..7].to_vec();
        damaged[2] ^= 0x40;
        std::fs::write(&path, &damaged).unwrap();

        let read_error = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap_err();
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);

        let write_error = LogWriter::open(&path, Cipher::new(&key), d())
            .err()
            .expect("mismatching partial header must not be rewritten");
        assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
    }

    #[test]
    fn append_after_reopen_preserves_old_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let r1 = vec![Record {
            start_offset_secs: 1,
            duration_secs: 2,
            app_id: 1,
            title_id: 0,
            flags: 0,
        }];
        let r2 = vec![Record {
            start_offset_secs: 3,
            duration_secs: 4,
            app_id: 2,
            title_id: 0,
            flags: 0,
        }];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&r1).unwrap();
        }
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&r2).unwrap();
        }
        let all = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap();
        assert_eq!(all, vec![r1[0], r2[0]]);
    }

    #[test]
    fn writer_rejects_oversized_blocks_without_advancing_the_aad_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized-block.log");
        let key = MasterKey::new_random();
        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        let oversized = vec![record(1); MAX_WRITE_BLOCK_RECORDS + 1];

        let error = writer.write_block(&oversized).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), HEADER_LEN as u64);

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
    fn writer_never_creates_a_file_its_reader_would_reject_for_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daily-limit.log");
        let key = MasterKey::new_random();
        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        let one_record_wire_len = (BLOCK_OVERHEAD + RECORD_SIZE) as u64;

        // Model an already large authenticated prefix without allocating or
        // writing a 64 MiB fixture. A block that lands exactly on the shared
        // reader/writer limit is valid.
        writer.file_len = MAX_LOG_FILE_BYTES as u64 - one_record_wire_len;
        writer.write_block(&[record(1)]).unwrap();
        assert_eq!(writer.file_len, MAX_LOG_FILE_BYTES as u64);

        let physical_len_before_rejection = writer.file.metadata().unwrap().len();
        let block_index_before_rejection = writer.block_index;
        let error = writer.write_block(&[record(2)]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("daily file would exceed"));
        assert_eq!(
            writer.file.metadata().unwrap().len(),
            physical_len_before_rejection
        );
        assert_eq!(writer.block_index, block_index_before_rejection);
    }

    #[test]
    fn truncation_recovers_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let recs = vec![Record {
            start_offset_secs: 1,
            duration_secs: 2,
            app_id: 1,
            title_id: 0,
            flags: 0,
        }];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&recs).unwrap();
        }
        // 把文件砍掉最后 5 字节模拟掉电
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 5);
        std::fs::write(&path, &bytes).unwrap();
        let out = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap();
        assert!(out.is_empty()); // 唯一 block 损坏 → 全丢
    }

    #[test]
    fn truncated_tail_is_removed_before_reopen_and_append() {
        let block_len = BLOCK_OVERHEAD + RECORD_SIZE;
        let cases = [
            ("whole-block-missing", 0usize),
            ("partial-length", 1),
            ("length-only", 4),
            ("partial-nonce", 4 + NONCE_LEN - 1),
            ("nonce-only", 4 + NONCE_LEN),
            ("missing-final-tag-byte", block_len - 1),
        ];

        for (name, retained) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("{name}.log"));
            let key = MasterKey::new_random();
            let first = record(1);
            let discarded = record(2);
            let replacement = record(3);
            write_blocks(&path, &key, &[first, discarded]);

            let pristine = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&pristine);
            let first_end = ranges[0].1;
            let second_start = ranges[1].0;
            assert_eq!(ranges[1].1 - second_start, block_len);
            let damaged = &pristine[..second_start + retained];
            std::fs::write(&path, damaged).unwrap();

            let prefix = LogReader::new(Cipher::new(&key), d())
                .read_all(&path)
                .unwrap();
            assert_eq!(prefix, vec![first], "{name}");

            let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                first_end as u64,
                "{name}"
            );
            writer.write_block(&[replacement]).unwrap();
            drop(writer);

            let recovered = LogReader::new(Cipher::new(&key), d())
                .read_all(&path)
                .unwrap();
            assert_eq!(recovered, vec![first, replacement], "{name}");
        }
    }

    #[test]
    fn bad_tag_on_authenticated_tail_is_replaced_with_continuous_aad_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-tail-tag.log");
        let key = MasterKey::new_random();
        let first = record(1);
        let discarded = record(2);
        let replacement = record(3);
        write_blocks(&path, &key, &[first, discarded]);

        let mut damaged = std::fs::read(&path).unwrap();
        let ranges = block_ranges(&damaged);
        let first_end = ranges[0].1;
        let second_start = ranges[1].0;
        let old_nonce = damaged[second_start + 4..second_start + 4 + NONCE_LEN].to_vec();
        *damaged.last_mut().unwrap() ^= 0x80;
        std::fs::write(&path, &damaged).unwrap();

        let prefix = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap();
        assert_eq!(prefix, vec![first]);

        let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), first_end as u64);
        writer.write_block(&[replacement]).unwrap();
        drop(writer);

        let repaired = std::fs::read(&path).unwrap();
        let repaired_ranges = block_ranges(&repaired);
        let new_second_start = repaired_ranges[1].0;
        let new_nonce = &repaired[new_second_start + 4..new_second_start + 4 + NONCE_LEN];
        assert_ne!(new_nonce, old_nonce.as_slice());

        let recovered = LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap();
        assert_eq!(recovered, vec![first, replacement]);
    }

    #[test]
    fn non_tail_corruption_is_rejected_and_preserved() {
        let cases = [
            ("ciphertext", 4 + NONCE_LEN),
            ("tag", BLOCK_OVERHEAD + RECORD_SIZE - 1),
        ];

        for (name, offset_in_block) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("{name}.log"));
            let key = MasterKey::new_random();
            write_blocks(&path, &key, &[record(1), record(2), record(3)]);

            let mut damaged = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&damaged);
            damaged[ranges[1].0 + offset_in_block] ^= 0x40;
            std::fs::write(&path, &damaged).unwrap();

            let read_error = LogReader::new(Cipher::new(&key), d())
                .read_all(&path)
                .unwrap_err();
            assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&path).unwrap(), damaged, "{name}");

            let write_error = LogWriter::open(&path, Cipher::new(&key), d())
                .err()
                .expect("non-tail corruption must reject writer reopen");
            assert_eq!(write_error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&path).unwrap(), damaged, "{name}");
        }
    }

    #[test]
    fn corrupted_middle_lengths_cannot_hide_authenticated_successors() {
        // Smaller: a two-record middle block is made to claim one record.
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("smaller-length.log");
            let key = MasterKey::new_random();
            let mut writer = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            writer.write_block(&[record(1)]).unwrap();
            writer.write_block(&[record(2), record(3)]).unwrap();
            writer.write_block(&[record(4)]).unwrap();
            drop(writer);

            let mut damaged = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&damaged);
            damaged[ranges[1].0..ranges[1].0 + 4]
                .copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
            assert_rejected_and_preserved(&path, &key, &damaged, "smaller");
        }

        // Larger: the declared frame ends inside the following block.
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("larger-length.log");
            let key = MasterKey::new_random();
            write_blocks(&path, &key, &[record(1), record(2), record(3)]);

            let mut damaged = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&damaged);
            damaged[ranges[1].0..ranges[1].0 + 4]
                .copy_from_slice(&((2 * RECORD_SIZE) as u32).to_le_bytes());
            assert_rejected_and_preserved(&path, &key, &damaged, "larger");
        }

        // Swallow-rest: without resynchronisation this looks exactly like a
        // truncated tail, although a complete authenticated block follows.
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("swallow-rest.log");
            let key = MasterKey::new_random();
            write_blocks(&path, &key, &[record(1), record(2), record(3)]);

            let mut damaged = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&damaged);
            damaged[ranges[1].0..ranges[1].0 + 4]
                .copy_from_slice(&((4 * RECORD_SIZE) as u32).to_le_bytes());
            assert_rejected_and_preserved(&path, &key, &damaged, "swallow-rest");
        }

        // Exact-to-EOF: 18 one-record frames have enough aggregate overhead
        // for a forged, record-aligned length to consume the suffix exactly.
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("swallow-exactly.log");
            let key = MasterKey::new_random();
            let records: Vec<_> = (1..=19).map(record).collect();
            write_blocks(&path, &key, &records);

            let mut damaged = std::fs::read(&path).unwrap();
            let ranges = block_ranges(&damaged);
            let forged_plaintext_len = damaged.len() - ranges[1].0 - BLOCK_OVERHEAD;
            assert_eq!(forged_plaintext_len % RECORD_SIZE, 0);
            damaged[ranges[1].0..ranges[1].0 + 4]
                .copy_from_slice(&(forged_plaintext_len as u32).to_le_bytes());
            assert_rejected_and_preserved(&path, &key, &damaged, "swallow-exactly");
        }
    }

    #[test]
    fn legacy_append_after_garbage_is_detected_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-garbage-append.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[record(1)]);

        // Reproduce the old reopen bug: a torn block remains after index 0,
        // then the reopened writer appends a valid block using index 1 after
        // those garbage bytes.
        let cipher = Cipher::new(&key);
        let mut plaintext = Vec::new();
        record(2).write_to(&mut plaintext);
        let successor = cipher.seal_block(&plaintext, &make_aad(d(), 1));
        let mut damaged = std::fs::read(&path).unwrap();
        damaged.extend_from_slice(&((10 * RECORD_SIZE) as u32).to_le_bytes());
        damaged.extend_from_slice(&[0xA5; 3]);
        damaged.extend_from_slice(&successor);

        assert_rejected_and_preserved(&path, &key, &damaged, "legacy-garbage-append");
    }

    #[test]
    fn recovery_candidate_budget_exhaustion_fails_closed() {
        let key = MasterKey::new_random();
        let cipher = Cipher::new(&key);
        let frame_len = BLOCK_OVERHEAD + RECORD_SIZE;
        let mut bytes = vec![0u8; 1 + (MAX_RECOVERY_AUTH_CANDIDATES + 1) * frame_len];
        for candidate in 0..=MAX_RECOVERY_AUTH_CANDIDATES {
            let start = 1 + candidate * frame_len;
            bytes[start..start + 4].copy_from_slice(&(RECORD_SIZE as u32).to_le_bytes());
        }

        let error = ensure_no_authenticated_successor(&bytes, 0, 7, &cipher, d()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("budget exhausted"));
    }

    #[test]
    fn bounded_reader_rejects_oversized_files_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized.log");
        let bytes = vec![0xA5; 33];
        std::fs::write(&path, &bytes).unwrap();
        let mut file = File::open(&path).unwrap();

        let error = read_limited(&mut file, 32).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn unverified_single_block_auth_failure_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrong-key.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[record(1)]);
        let original = std::fs::read(&path).unwrap();

        let wrong_key = MasterKey::new_random();
        let error = LogWriter::open(&path, Cipher::new(&wrong_key), d())
            .err()
            .expect("a first-block auth failure cannot be classified as safe tail damage");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unverified_first_block_truncation_is_never_repaired_by_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("first-block-truncated.log");
        let key = MasterKey::new_random();
        write_blocks(&path, &key, &[record(1)]);
        let mut damaged = std::fs::read(&path).unwrap();
        damaged.truncate(damaged.len() - 5);
        std::fs::write(&path, &damaged).unwrap();

        // Readers may expose the authenticated prefix (empty here), but a
        // writer cannot prove the supplied key before destructive recovery.
        assert!(LogReader::new(Cipher::new(&key), d())
            .read_all(&path)
            .unwrap()
            .is_empty());
        let wrong_key = MasterKey::new_random();
        for candidate_key in [&key, &wrong_key] {
            let error = LogWriter::open(&path, Cipher::new(candidate_key), d())
                .err()
                .expect("unverified first-block damage must remain untouched");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(std::fs::read(&path).unwrap(), damaged);
        }
    }
}
