//! 二进制词典格式 (wdb) + mmap
//!
//! 与 Go 版本 `wind_input/internal/dict/binformat/` 对齐。

use memmap2::Mmap;
use std::fs::File;
use std::path::Path;

/// wdb 文件头
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct DictFileHeader {
    pub magic: [u8; 4], // "WDIC"
    pub version: u32,
    pub key_count: u32,
    pub index_off: u64,
    pub data_off: u64,
    pub str_off: u64,
    pub abbrev_off: u64,
    pub meta_off: u64,
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

/// 条目记录 (14 bytes, V3)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct DictEntryRecord {
    pub text_off: u32,
    pub text_len: u16,
    pub weight: i32,
    pub order: i32,
}

/// 二进制词典读取器
pub struct DictReader {
    mmap: Mmap,
    // TODO: 解析后的索引
}

impl DictReader {
    /// 打开 wdb 文件
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let file = File::open(path.as_ref())?;
        let mmap = unsafe { Mmap::map(&file)? };
        // TODO: 验证 magic 和版本
        Ok(Self { mmap })
    }

    /// 获取 mmap 数据引用
    pub fn data(&self) -> &[u8] {
        &self.mmap
    }
}

/// Unigram 格式读取器
pub struct UnigramReader {
    mmap: Mmap,
}

impl UnigramReader {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let file = File::open(path.as_ref())?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
    }
}
