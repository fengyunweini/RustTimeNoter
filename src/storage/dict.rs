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
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"RTND";
const VERSION: u32 = 1;

pub struct Dict {
    path: PathBuf,
    by_string: HashMap<String, u32>,
    by_id: Vec<Option<String>>, // index = id
    next_id: u32,
}

impl Dict {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut me = Self {
            path: path.clone(),
            by_string: HashMap::new(),
            by_id: vec![None], // index 0 reserved
            next_id: 1,
        };
        if path.exists() {
            me.load()?;
        } else {
            me.write_header_if_needed()?;
        }
        Ok(me)
    }

    fn write_header_if_needed(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.path)?;
        if f.metadata()?.len() == 0 {
            f.write_all(MAGIC)?;
            f.write_all(&VERSION.to_le_bytes())?;
            f.sync_data()?;
        }
        Ok(())
    }

    fn load(&mut self) -> std::io::Result<()> {
        let f = File::open(&self.path)?;
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
        loop {
            let mut id_buf = [0u8; 4];
            match r.read_exact(&mut id_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let id = u32::from_le_bytes(id_buf);
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut bytes = vec![0u8; len];
            r.read_exact(&mut bytes)?;
            let s = String::from_utf8(bytes).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
            })?;
            // grow vec
            if (id as usize) >= self.by_id.len() {
                self.by_id.resize(id as usize + 1, None);
            }
            self.by_id[id as usize] = Some(s.clone());
            self.by_string.insert(s, id);
            if id >= self.next_id {
                self.next_id = id + 1;
            }
        }
        Ok(())
    }

    /// 返回字符串对应的 ID；不存在则分配并 append。
    pub fn intern(&mut self, s: &str) -> std::io::Result<u32> {
        if let Some(&id) = self.by_string.get(s) {
            return Ok(id);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "dict: id overflow"))?;
        self.append_record(id, s)?;
        if (id as usize) >= self.by_id.len() {
            self.by_id.resize(id as usize + 1, None);
        }
        self.by_id[id as usize] = Some(s.to_string());
        self.by_string.insert(s.to_string(), id);
        Ok(id)
    }

    fn append_record(&self, id: u32, s: &str) -> std::io::Result<()> {
        let bytes = s.as_bytes();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // ensure header on brand-new file
        if f.metadata()?.len() == 0 {
            f.write_all(MAGIC)?;
            f.write_all(&VERSION.to_le_bytes())?;
        }
        // 直接写文件即可；append 模式下不需要 BufWriter。
        f.write_all(&id.to_le_bytes())?;
        f.write_all(&(bytes.len() as u32).to_le_bytes())?;
        f.write_all(bytes)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.dict");
        {
            let mut d = Dict::open(&path).unwrap();
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
        let mut d = Dict::open(&path).unwrap();
        let id = d.intern("中文窗口标题🚀").unwrap();
        let d2 = Dict::open(&path).unwrap();
        assert_eq!(d2.get(id), Some("中文窗口标题🚀"));
    }
}
