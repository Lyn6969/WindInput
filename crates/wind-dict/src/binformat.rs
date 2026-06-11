//! 二进制词典格式 (wdb) + mmap 零拷贝查询
//!
//! 与 Go 版本 `wind_input/internal/dict/binformat/` 对齐。
//! 支持 V2（10字节条目）和 V3（14字节条目，含 Order 字段）。

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use tracing::{debug, info, warn};

/// wdb 魔数 "WDIC"
const MAGIC: [u8; 4] = [b'W', b'D', b'I', b'C'];

/// wdb 文件头 (32 bytes)
#[derive(Debug, Clone)]
#[repr(C, packed)]
pub struct DictFileHeader {
    pub magic: [u8; 4],
    pub version: u32,
    pub key_count: u32,
    pub index_off: u32,
    pub data_off: u32,
    pub str_off: u32,
    pub abbrev_off: u32,
    pub meta_off: u32,
}

impl DictFileHeader {
    pub const SIZE: usize = 32;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            magic: [buf[0], buf[1], buf[2], buf[3]],
            version: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            key_count: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            index_off: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            data_off: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
            str_off: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
            abbrev_off: u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]),
            meta_off: u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]),
        })
    }
}

/// 键索引条目 (12 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct DictKeyIndex {
    pub code_off: u32,
    pub code_len: u16,
    pub entry_off: u32,
    pub entry_len: u16,
}

impl DictKeyIndex {
    pub const SIZE: usize = 12;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            code_off: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            code_len: u16::from_le_bytes([buf[4], buf[5]]),
            entry_off: u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]),
            entry_len: u16::from_le_bytes([buf[10], buf[11]]),
        })
    }
}

/// 条目记录 V3 (14 bytes，含 Order)
#[derive(Debug, Clone)]
pub struct EntryRecord {
    pub text: String,
    pub weight: i32,
    pub order: i32,
}

/// 词典查询结果
#[derive(Debug, Clone)]
pub struct DictEntry {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub order: i32,
}

/// 二进制词典读取器（mmap 零拷贝）
pub struct DictReader {
    mmap: Mmap,
    header: DictFileHeader,
    /// 条目记录大小（V2=10, V3=14）
    entry_size: usize,
}

impl DictReader {
    /// 打开 wdb 文件
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };

        let header = DictFileHeader::from_bytes(&mmap)
            .ok_or_else(|| anyhow::anyhow!("invalid wdb file: too short"))?;

        if header.magic != MAGIC {
            anyhow::bail!(
                "invalid wdb magic: expected WDIC, got {:?}",
                header.magic
            );
        }

        let version = header.version;
        let key_count = header.key_count;
        let index_off = header.index_off;
        let data_off = header.data_off;
        let str_off = header.str_off;

        let entry_size = match version {
            3 => 14,
            2 => 10,
            1 => 10,
            _ => anyhow::bail!("unsupported wdb version: {}", version),
        };

        info!(
            "Opened wdb: {} ({} keys, v{}, index_off={}, data_off={}, str_off={})",
            path.display(),
            key_count,
            version,
            index_off,
            data_off,
            str_off,
        );

        Ok(Self {
            mmap,
            header,
            entry_size,
        })
    }

    /// 获取 mmap 数据
    fn data(&self) -> &[u8] {
        &self.mmap
    }

    /// 读取字符串池中的字符串
    fn read_string(&self, off: u32, len: u16) -> &str {
        let start = self.header.str_off as usize + off as usize;
        let end = start + len as usize;
        if end > self.data().len() {
            return "";
        }
        std::str::from_utf8(&self.data()[start..end]).unwrap_or("")
    }

    /// 读取第 i 个键索引
    fn read_key_index(&self, i: u32) -> Option<DictKeyIndex> {
        let offset = self.header.index_off as usize + (i as usize) * DictKeyIndex::SIZE;
        DictKeyIndex::from_bytes(&self.data()[offset..])
    }

    /// 读取条目记录
    fn read_entry(&self, entry_off: u32, entry_idx: u16) -> Option<EntryRecord> {
        let base = self.header.data_off as usize + entry_off as usize;
        let offset = base + (entry_idx as usize) * self.entry_size;
        let buf = &self.data()[offset..];

        if buf.len() < self.entry_size {
            return None;
        }

        let text_off = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let text_len = u16::from_le_bytes([buf[4], buf[5]]);
        let weight = i32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]);

        let order = if self.entry_size >= 14 {
            i32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]])
        } else {
            entry_idx as i32
        };

        let text = self.read_string(text_off, text_len).to_string();

        Some(EntryRecord {
            text,
            weight,
            order,
        })
    }

    /// 精确查找：二分搜索键索引
    pub fn search(&self, code: &str) -> Vec<DictEntry> {
        let key_count = self.header.key_count;
        if key_count == 0 {
            return Vec::new();
        }

        // 二分搜索
        let mut lo = 0u32;
        let mut hi = key_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if let Some(idx) = self.read_key_index(mid) {
                let mid_code = self.read_string(idx.code_off, idx.code_len);
                if mid_code < code {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            } else {
                break;
            }
        }

        // 检查是否精确匹配
        if lo < key_count {
            if let Some(idx) = self.read_key_index(lo) {
                let found_code = self.read_string(idx.code_off, idx.code_len);
                if found_code == code {
                    return self.collect_entries(&idx, code);
                }
            }
        }

        Vec::new()
    }

    /// 前缀查找：找到第一个 >= prefix 的键，然后向后扫描
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<DictEntry> {
        let key_count = self.header.key_count;
        if key_count == 0 {
            return Vec::new();
        }

        // 二分找到第一个 >= prefix 的位置
        let mut lo = 0u32;
        let mut hi = key_count;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if let Some(idx) = self.read_key_index(mid) {
                let mid_code = self.read_string(idx.code_off, idx.code_len);
                if mid_code < prefix {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            } else {
                break;
            }
        }

        // 从 lo 开始向后扫描，收集前缀匹配的结果
        let mut results = Vec::new();
        let mut i = lo;

        while i < key_count && results.len() < limit {
            if let Some(idx) = self.read_key_index(i) {
                let code = self.read_string(idx.code_off, idx.code_len);
                if !code.starts_with(prefix) {
                    break;
                }
                let entries = self.collect_entries(&idx, code);
                results.extend(entries);
            }
            i += 1;
        }

        // 按 weight 降序排序
        results.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        results.truncate(limit);
        results
    }

    /// 收集某个键下的所有条目
    fn collect_entries(&self, idx: &DictKeyIndex, code: &str) -> Vec<DictEntry> {
        let mut entries = Vec::with_capacity(idx.entry_len as usize);
        for j in 0..idx.entry_len {
            if let Some(rec) = self.read_entry(idx.entry_off, j) {
                entries.push(DictEntry {
                    code: code.to_string(),
                    text: rec.text,
                    weight: rec.weight,
                    order: rec.order,
                });
            }
        }
        entries
    }

    /// 获取键总数
    pub fn key_count(&self) -> u32 {
        self.header.key_count
    }

    /// 遍历所有键（用于调试/统计）
    pub fn for_each_key<F: FnMut(&str)>(&self, mut f: F) {
        for i in 0..self.header.key_count {
            if let Some(idx) = self.read_key_index(i) {
                let code = self.read_string(idx.code_off, idx.code_len);
                f(code);
            }
        }
    }
}
