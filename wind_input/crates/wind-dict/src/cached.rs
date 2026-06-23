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

    /// 构建反查索引:汉字/词 → 实际编码。同一词多码时取「权重降序→码长降序→码字典序升序」
    /// 首位(对齐 Go `CodeTable.BuildReverseIndex`)。供拼音方案显示「该词在主码表里实际怎么打」,
    /// 避免按字生成码却在码表中打不出的错配。
    pub fn build_reverse_index(&self) -> std::collections::HashMap<String, String> {
        use std::collections::HashMap;
        let mut best: HashMap<String, (String, i32)> = HashMap::new();
        self.for_each_entry(&mut |code, text, weight| {
            let replace = match best.get(text) {
                Some((bc, bw)) => {
                    weight > *bw
                        || (weight == *bw && code.len() > bc.len())
                        || (weight == *bw && code.len() == bc.len() && code < bc.as_str())
                }
                None => true,
            };
            if replace {
                best.insert(text.to_string(), (code.to_string(), weight));
            }
        });
        best.into_iter().map(|(t, (c, _))| (t, c)).collect()
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

    /// 反查索引选码规则:权重降序 → 码长降序 → 码字典序升序(对齐 Go BuildReverseIndex)。
    #[test]
    fn reverse_index_picks_by_weight_then_len_then_lex() {
        let mut d = CodetableDict::empty();
        // 「工」两码同权重 → 取较长码 aaaa。
        d.merge_single("a".into(), "工".into(), 100, 0);
        d.merge_single("aaaa".into(), "工".into(), 100, 1);
        // 「中」唯一码。
        d.merge_single("k".into(), "中".into(), 50, 2);
        // 「大」两码不同权重 → 取高权重 ddd(无视码长)。
        d.merge_single("dd".into(), "大".into(), 10, 3);
        d.merge_single("ddd".into(), "大".into(), 99, 4);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_reverse_index();
        assert_eq!(idx.get("工").map(String::as_str), Some("aaaa"));
        assert_eq!(idx.get("中").map(String::as_str), Some("k"));
        assert_eq!(idx.get("大").map(String::as_str), Some("ddd"));
        assert_eq!(idx.get("无"), None);
    }
}
