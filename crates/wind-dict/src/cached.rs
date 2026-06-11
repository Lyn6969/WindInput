//! 词典缓存层：yaml 首次加载后写入 .wdb 缓存，后续直接 mmap 读取
//!
//! 与 Go 版 mmap 共享池对齐，显著降低内存占用。

use crate::binformat::{DictReader, DictWriter};
use crate::codetable::CodetableDict;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// 缓存词典：优先使用 mmap，回退到内存模式
pub enum CachedDict {
    /// mmap 零拷贝模式（低内存）
    Mmap(DictReader),
    /// 内存模式（首次加载或缓存写入失败）
    Memory(CodetableDict),
}

impl CachedDict {
    /// 加载词典，自动使用 .wdb 缓存
    ///
    /// 流程：
    /// 1. 检查 .wdb 缓存是否存在且比 .yaml 新
    /// 2. 如果是，直接 mmap 打开
    /// 3. 如果否，加载 .yaml，写入 .wdb 缓存，然后 mmap 打开
    pub fn load(yaml_path: &Path) -> anyhow::Result<Self> {
        let wdb_path = yaml_path.with_extension("wdb");

        // 检查缓存是否有效
        if Self::cache_is_valid(yaml_path, &wdb_path) {
            match DictReader::open(&wdb_path) {
                Ok(reader) => {
                    info!("Using mmap cache: {} ({} keys)", wdb_path.display(), reader.key_count());
                    return Ok(Self::Mmap(reader));
                }
                Err(e) => {
                    warn!("Failed to open mmap cache: {}, falling back to yaml", e);
                }
            }
        }

        // 加载 yaml
        let dict = CodetableDict::load(yaml_path)?;
        info!("Loaded yaml: {} ({} entries)", yaml_path.display(), dict.len());

        // 写入 .wdb 缓存
        if let Err(e) = Self::write_cache(&dict, &wdb_path) {
            warn!("Failed to write .wdb cache: {}", e);
            return Ok(Self::Memory(dict));
        }

        // 用 mmap 重新打开缓存
        match DictReader::open(&wdb_path) {
            Ok(reader) => {
                info!("Using mmap cache: {} ({} keys)", wdb_path.display(), reader.key_count());
                Ok(Self::Mmap(reader))
            }
            Err(e) => {
                warn!("Failed to open mmap cache after write: {}", e);
                Ok(Self::Memory(dict))
            }
        }
    }

    /// 检查缓存是否有效（存在且比源文件新）
    fn cache_is_valid(yaml_path: &Path, wdb_path: &Path) -> bool {
        if !wdb_path.exists() { return false; }

        let yaml_mtime = match std::fs::metadata(yaml_path) {
            Ok(m) => m.modified().ok(),
            Err(_) => return false,
        };
        let wdb_mtime = match std::fs::metadata(wdb_path) {
            Ok(m) => m.modified().ok(),
            Err(_) => return false,
        };

        match (yaml_mtime, wdb_mtime) {
            (Some(y), Some(w)) => w >= y,
            _ => false,
        }
    }

    /// 将内存词典写入 .wdb 缓存
    fn write_cache(dict: &CodetableDict, wdb_path: &Path) -> anyhow::Result<()> {
        let mut writer = DictWriter::new();

        // 遍历所有键，导出到 writer
        dict.export_to_writer(&mut writer);

        if writer.key_count() == 0 {
            anyhow::bail!("No entries to write");
        }

        writer.write(wdb_path)?;
        info!("Wrote .wdb cache: {} ({} keys)", wdb_path.display(), writer.key_count());
        Ok(())
    }

    /// 精确查找
    pub fn search(&self, code: &str) -> Vec<(String, i32, i32)> {
        match self {
            Self::Mmap(reader) => {
                reader.search(code).into_iter()
                    .map(|e| (e.text, e.weight, e.order))
                    .collect()
            }
            Self::Memory(dict) => dict.search(code),
        }
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String, i32, i32)> {
        match self {
            Self::Mmap(reader) => {
                reader.search_prefix(prefix, limit).into_iter()
                    .map(|e| (e.code, e.text, e.weight, e.order))
                    .collect()
            }
            Self::Memory(dict) => dict.search_prefix(prefix, limit),
        }
    }

    /// 总条目数
    pub fn len(&self) -> usize {
        match self {
            Self::Mmap(reader) => reader.key_count() as usize,
            Self::Memory(dict) => dict.len(),
        }
    }

    pub fn is_empty(&self) -> bool { self.len() == 0 }
}
