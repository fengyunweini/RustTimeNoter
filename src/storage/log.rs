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
//! 崩溃恢复：reader 在解密失败 / 截断时停止读取（最后一个未完成 block 被丢弃）。

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use super::crypto::{Cipher, CryptoError, NONCE_LEN, TAG_LEN};
use super::model::{Record, RECORD_SIZE};

const MAGIC: &[u8; 4] = b"RTNL";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 4 + 4 + 4 + 4;

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
        let block_index = if len == 0 {
            // 写头
            f.write_all(MAGIC)?;
            f.write_all(&VERSION.to_le_bytes())?;
            f.write_all(&date.pack().to_le_bytes())?;
            f.write_all(&0u32.to_le_bytes())?;
            f.sync_data()?;
            0
        } else {
            // 验证头并扫描 block 数（仅靠长度推不准，因为 block 可变长；改为顺序扫 wire）
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?; // 单日体积小，整文件读一遍 OK
            validate_header(&buf, date)?;
            count_blocks(&buf[HEADER_LEN..])?
        };
        // 重新定位到末尾
        use std::io::Seek;
        f.seek(std::io::SeekFrom::End(0))?;
        Ok(Self { file: f, cipher, date, block_index })
    }

    /// 写入一个 block（一组 record 的明文）。
    pub fn write_block(&mut self, records: &[Record]) -> std::io::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut plaintext = Vec::with_capacity(records.len() * RECORD_SIZE);
        for r in records {
            r.write_to(&mut plaintext);
        }
        let aad = make_aad(self.date, self.block_index);
        let wire = self.cipher.seal_block(&plaintext, &aad);
        self.file.write_all(&wire)?;
        self.file.sync_data()?;
        self.block_index = self.block_index.wrapping_add(1);
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
        let buf = match std::fs::read(path) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok(Vec::new());
        }
        validate_header(&buf, self.date)?;
        let mut out = Vec::new();
        let mut idx: u32 = 0;
        let mut cursor = HEADER_LEN;
        while cursor < buf.len() {
            let aad = make_aad(self.date, idx);
            match self.cipher.open_block(&buf[cursor..], &aad) {
                Ok((pt, total)) => {
                    let mut p = 0;
                    while p + RECORD_SIZE <= pt.len() {
                        if let Some(r) = Record::read_from(&pt[p..p + RECORD_SIZE]) {
                            out.push(r);
                        }
                        p += RECORD_SIZE;
                    }
                    cursor += total;
                    idx = idx.wrapping_add(1);
                }
                Err(CryptoError::Truncated) | Err(CryptoError::Auth) => break,
                Err(CryptoError::Io(e)) => return Err(e),
            }
        }
        Ok(out)
    }
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

fn count_blocks(body: &[u8]) -> std::io::Result<u32> {
    let mut idx: u32 = 0;
    let mut cursor = 0;
    while cursor + 4 + NONCE_LEN + TAG_LEN <= body.len() {
        let plen = u32::from_le_bytes(body[cursor..cursor + 4].try_into().unwrap()) as usize;
        let total = 4 + NONCE_LEN + plen + TAG_LEN;
        if cursor + total > body.len() {
            break;
        }
        cursor += total;
        idx = idx.wrapping_add(1);
    }
    Ok(idx)
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
        LogDate { year: 2026, month: 4, day: 22 }
    }

    #[test]
    fn write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let recs = vec![
            Record { start_offset_secs: 10, duration_secs: 5, app_id: 1, title_id: 0, flags: 0 },
            Record { start_offset_secs: 20, duration_secs: 7, app_id: 2, title_id: 3u32, flags: 0 },
        ];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&recs).unwrap();
        }
        let r = LogReader::new(Cipher::new(&key), d()).read_all(&path).unwrap();
        assert_eq!(r, recs);
    }

    #[test]
    fn append_after_reopen_preserves_old_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let r1 = vec![Record { start_offset_secs: 1, duration_secs: 2, app_id: 1, title_id: 0, flags: 0 }];
        let r2 = vec![Record { start_offset_secs: 3, duration_secs: 4, app_id: 2, title_id: 0, flags: 0 }];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&r1).unwrap();
        }
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&r2).unwrap();
        }
        let all = LogReader::new(Cipher::new(&key), d()).read_all(&path).unwrap();
        assert_eq!(all, vec![r1[0], r2[0]]);
    }

    #[test]
    fn truncation_recovers_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-04-22.log");
        let key = MasterKey::new_random();
        let recs = vec![Record { start_offset_secs: 1, duration_secs: 2, app_id: 1, title_id: 0, flags: 0 }];
        {
            let mut w = LogWriter::open(&path, Cipher::new(&key), d()).unwrap();
            w.write_block(&recs).unwrap();
        }
        // 把文件砍掉最后 5 字节模拟掉电
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 5);
        std::fs::write(&path, &bytes).unwrap();
        let out = LogReader::new(Cipher::new(&key), d()).read_all(&path).unwrap();
        assert!(out.is_empty()); // 唯一 block 损坏 → 全丢
    }
}
