//! 词典缓存层：yaml 首次加载后写入 .wdb 缓存，后续直接 mmap 读取
//!
//! 与 Go 版 mmap 共享池对齐，显著降低内存占用。

use crate::codetable::CodetableDict;
use crate::datformat::{WdatReader, WdatWriter};
use std::path::Path;
use tracing::{info, warn};

/// 缓存词典：优先使用 mmap，回退到内存模式
pub enum CachedDict {
    /// mmap 零拷贝模式（低内存，wdat DAT 格式）
    Mmap(WdatReader),
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
        let wdb_path = yaml_path.with_extension("wdat");
        Self::load_at(yaml_path, &wdb_path)
    }

    /// 加载词典，使用指定的 .wdb 缓存路径（缓存可与源分离，如放
    /// `%LOCALAPPDATA%\WindInput\cache`，避免写入只读的安装目录）。
    pub fn load_at(yaml_path: &Path, wdb_path: &Path) -> anyhow::Result<Self> {
        Self::load_at_with(yaml_path, wdb_path, false)
    }

    /// 同 [`load_at`]，`lowercase_code=true` 时把 code 列小写化（英文词库）。
    /// 缓存命中时直接 mmap（缓存内已是小写码）；缓存重建时用 `load_lowercased`。
    pub fn load_at_with(
        yaml_path: &Path,
        wdb_path: &Path,
        lowercase_code: bool,
    ) -> anyhow::Result<Self> {
        // 检查缓存是否有效
        if Self::cache_is_valid(yaml_path, wdb_path) {
            match WdatReader::open(wdb_path) {
                Ok(reader) => {
                    info!(
                        "Using mmap cache: {} ({} keys)",
                        wdb_path.display(),
                        reader.key_count()
                    );
                    return Ok(Self::Mmap(reader));
                }
                Err(e) => {
                    warn!("Failed to open mmap cache: {}, falling back to yaml", e);
                }
            }
        }

        // 加载 yaml
        let dict = if lowercase_code {
            CodetableDict::load_lowercased(yaml_path)?
        } else {
            CodetableDict::load(yaml_path)?
        };
        info!(
            "Loaded yaml: {} ({} entries)",
            yaml_path.display(),
            dict.len()
        );

        // 确保缓存目录存在后写入 .wdb 缓存
        if let Some(parent) = wdb_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = Self::write_cache(&dict, wdb_path) {
            warn!("Failed to write .wdb cache: {}", e);
            return Ok(Self::Memory(dict));
        }
        // 写内容指纹 sidecar，供下次按内容(而非 mtime)校验复用
        crate::cache_fp::write_cache_fp(wdb_path, &[yaml_path]);

        // 用 mmap 重新打开缓存
        match WdatReader::open(wdb_path) {
            Ok(reader) => {
                info!(
                    "Using mmap cache: {} ({} keys)",
                    wdb_path.display(),
                    reader.key_count()
                );
                Ok(Self::Mmap(reader))
            }
            Err(e) => {
                warn!("Failed to open mmap cache after write: {}", e);
                Ok(Self::Memory(dict))
            }
        }
    }

    /// 检查缓存是否有效（按源文件**内容指纹**，不受 scp/部署刷新 mtime 影响）。
    fn cache_is_valid(yaml_path: &Path, wdb_path: &Path) -> bool {
        crate::cache_fp::cache_is_fresh(wdb_path, &[yaml_path])
    }

    /// 将内存词典写入 .wdb 缓存
    fn write_cache(dict: &CodetableDict, wdb_path: &Path) -> anyhow::Result<()> {
        let mut writer = WdatWriter::new();

        // 遍历所有键，导出到 writer
        dict.export_to_wdat(&mut writer);

        if writer.key_count() == 0 {
            anyhow::bail!("No entries to write");
        }

        writer.write(wdb_path)?;
        info!(
            "Wrote .wdb cache: {} ({} keys)",
            wdb_path.display(),
            writer.key_count()
        );
        Ok(())
    }

    /// 精确查找
    pub fn search(&self, code: &str) -> Vec<(String, i32, i32)> {
        match self {
            Self::Mmap(reader) => reader
                .search(code)
                .into_iter()
                .map(|e| (e.text, e.weight, e.order))
                .collect(),
            Self::Memory(dict) => dict.search(code),
        }
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String, i32, i32)> {
        match self {
            Self::Mmap(reader) => reader
                .search_prefix(prefix, limit)
                .into_iter()
                .map(|e| (e.code, e.text, e.weight, e.order))
                .collect(),
            Self::Memory(dict) => dict.search_prefix(prefix, limit),
        }
    }

    /// 简拼查找（声母缩写，如 nh→你好）：仅 wdat(Mmap) 的独立 AbbrevSection 支持；
    /// 内存回退(yaml 未建简拼) 返回空。结果 (text, weight, order)，已按权重降序、截断。
    pub fn search_abbrev(&self, code: &str, limit: usize) -> Vec<(String, i32, i32)> {
        match self {
            Self::Mmap(reader) => reader
                .search_abbrev(code, limit)
                .into_iter()
                .map(|e| (e.text, e.weight, e.order))
                .collect(),
            Self::Memory(_) => Vec::new(),
        }
    }

    /// 遍历全部条目(供反查索引构建)。
    pub fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        match self {
            Self::Mmap(reader) => reader.for_each_entry(f),
            Self::Memory(dict) => dict.for_each_entry(f),
        }
    }

    /// 构建反查索引:汉字/词 → 词库中的**全部**编码,按「码长升序→字典序升序」排列并去重。
    /// 供悬停 [编码] 段显示完整打法列表(如 `a/ab/abc`)与拼音编码提示(取末位=最长码,
    /// 全码最稳——简码可能被一级简码等占用)。取词库实际码,避免按字生成码却打不出的错配。
    pub fn build_reverse_index(&self) -> std::collections::HashMap<String, Vec<String>> {
        use std::collections::HashMap;
        let mut idx: HashMap<String, Vec<String>> = HashMap::new();
        self.for_each_entry(&mut |code, text, _weight| {
            idx.entry(text.to_string())
                .or_default()
                .push(code.to_string());
        });
        for codes in idx.values_mut() {
            codes.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
            codes.dedup();
            codes.shrink_to_fit();
        }
        idx
    }

    /// 总条目数
    pub fn len(&self) -> usize {
        match self {
            Self::Mmap(reader) => reader.key_count() as usize,
            Self::Memory(dict) => dict.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codetable::CodetableDict;

    /// 反查索引:同词收集全部码,码长升序 → 字典序升序,去重。
    #[test]
    fn reverse_index_collects_all_codes_sorted_by_len() {
        let mut d = CodetableDict::empty();
        // 「工」简码+全码 → 短码在前。
        d.merge_single("aaaa".into(), "工".into(), 100, 0);
        d.merge_single("a".into(), "工".into(), 100, 1);
        // 「中」唯一码 + 重复条目去重。
        d.merge_single("k".into(), "中".into(), 50, 2);
        d.merge_single("k".into(), "中".into(), 40, 3);
        // 「大」同长两码 → 字典序。
        d.merge_single("de".into(), "大".into(), 10, 4);
        d.merge_single("dd".into(), "大".into(), 99, 5);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_reverse_index();
        assert_eq!(
            idx.get("工"),
            Some(&vec!["a".to_string(), "aaaa".to_string()])
        );
        assert_eq!(idx.get("中"), Some(&vec!["k".to_string()]));
        assert_eq!(
            idx.get("大"),
            Some(&vec!["dd".to_string(), "de".to_string()])
        );
        assert_eq!(idx.get("无"), None);
    }
}
