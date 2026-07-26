//! 跨模块共享的数据模型。

/// 聚合器输出给写入器的"原始段"。采用半开区间 [start_unix, end_unix)。
/// 跨日由 writer 自行拆分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// 进程完整路径（解析失败时退化为 basename）。
    pub app_path: String,
    /// 进程 basename（用于黑名单 / 显示）。
    pub app_basename: String,
    /// 窗口标题；`None` 表示未捕获或不允许。
    pub title: Option<String>,
    /// UTC 起点（秒）。
    pub start_unix: u64,
    /// UTC 终点（秒），>= start_unix。
    pub end_unix: u64,
}

impl Segment {
    pub fn duration(&self) -> u64 {
        self.end_unix.saturating_sub(self.start_unix)
    }
}

/// 落盘后的扁平记录（单日内偏移）。15 字节定长。
///
/// 字段编码（小端）：
/// - `start_offset_secs`: u32 (相对当日 UTC 00:00 的秒数)
/// - `duration_secs`: u32
/// - `app_id`: u32
/// - `title_id`: u32  (0 = 未记录)
/// - `flags`: u8
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub start_offset_secs: u32,
    pub duration_secs: u32,
    pub app_id: u32,
    pub title_id: u32,
    pub flags: u8,
}

pub const RECORD_SIZE: usize = 4 + 4 + 4 + 4 + 1;

impl Record {
    pub fn write_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.start_offset_secs.to_le_bytes());
        out.extend_from_slice(&self.duration_secs.to_le_bytes());
        out.extend_from_slice(&self.app_id.to_le_bytes());
        out.extend_from_slice(&self.title_id.to_le_bytes());
        out.push(self.flags);
    }

    pub fn read_from(buf: &[u8]) -> Option<Self> {
        if buf.len() < RECORD_SIZE {
            return None;
        }
        Some(Self {
            start_offset_secs: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            duration_secs: u32::from_le_bytes(buf[4..8].try_into().ok()?),
            app_id: u32::from_le_bytes(buf[8..12].try_into().ok()?),
            title_id: u32::from_le_bytes(buf[12..16].try_into().ok()?),
            flags: buf[16],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trip() {
        let r = Record {
            start_offset_secs: 12345,
            duration_secs: 678,
            app_id: 42,
            title_id: 7,
            flags: 0,
        };
        let mut buf = Vec::new();
        r.write_to(&mut buf);
        assert_eq!(buf.len(), RECORD_SIZE);
        let back = Record::read_from(&buf).unwrap();
        assert_eq!(back, r);
    }
}
