//! DAT 格式 (wdat) — Double Array Trie
//!
//! 与 Go 版本 `wind_input/internal/dict/datformat/` 对齐。

use memmap2::Mmap;
use std::path::Path;

/// wdat 文件头 (48 bytes)
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct WdatFileHeader {
    pub magic: [u8; 4], // "WDAT"
    pub version: u32,
    pub dat_size: u32,
    pub leaf_count: u32,
    pub dat_off: u64,
    pub leaf_off: u64,
    pub entry_off: u64,
    pub str_off: u64,
    pub abbrev_off: u64,
    pub meta_off: u64,
    pub entry_count: u32,
    pub char_map_off: u32,
}

/// DAT 读取器
pub struct WdatReader {
    mmap: Mmap,
}

impl WdatReader {
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let file = std::fs::File::open(path.as_ref())?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap })
    }

    pub fn data(&self) -> &[u8] {
        &self.mmap
    }
}
