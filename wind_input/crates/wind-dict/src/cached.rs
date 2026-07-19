//! 词典缓存层：yaml 首次加载后写入 .wdb 缓存，后续直接 mmap 读取
//!
//! 与 Go 版 mmap 共享池对齐，显著降低内存占用。

use crate::codetable::CodetableDict;
use crate::datformat::{WdatReader, WdatWriter};
use std::path::Path;
use tracing::{info, warn};

/// 一条查询命中，含音节边界。由 [`CachedDict::search_with_boundary`] /
/// [`CachedDict::search_prefix_with_boundary`] 返回（拼音专用）。
#[derive(Debug, Clone)]
pub struct DictHit {
    /// 命中的词典 code：精确查询即查询串本身，前缀查询为该条目的完整 code。
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub order: i32,
    /// 音节边界 bitmask，见 [`crate::binformat::DictEntry::boundary`]。0=无边界信息，降级回 DAG。
    pub boundary: u64,
}

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
        if Self::cache_is_valid(yaml_path, wdb_path, lowercase_code) {
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
        // 写内容指纹 sidecar，供下次按内容(而非 mtime)校验复用。
        // tag 带上 lowercase_code：同一份 yaml 在 english / 非 english 两种 dict_type 下
        // 解析结果不同，不区分就会在切换后复用大小写错误的缓存。
        crate::cache_fp::write_cache_fp(
            wdb_path,
            &[yaml_path],
            crate::cache_fp::dict_tag(lowercase_code),
        );

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

    /// 检查缓存是否有效（按源文件**内容指纹** + 解析方式 tag，不受 scp/部署刷新 mtime 影响）。
    fn cache_is_valid(yaml_path: &Path, wdb_path: &Path, lowercase_code: bool) -> bool {
        crate::cache_fp::cache_is_fresh(
            wdb_path,
            &[yaml_path],
            crate::cache_fp::dict_tag(lowercase_code),
        )
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

    /// 精确查找，**并返回音节边界**（[`DictHit::boundary`]）。
    ///
    /// 与 [`Self::search`] 并存而非替换它：边界只对拼音有意义，而 `search` 的消费方遍布
    /// 码表/英文/cmdbar/composite 等无音节概念的场景（全仓 60+ 调用点），不应被拼音的
    /// 需求污染接口。仅拼音引擎改用本方法。
    pub fn search_with_boundary(&self, code: &str) -> Vec<DictHit> {
        match self {
            Self::Mmap(reader) => reader
                .search(code)
                .into_iter()
                .map(|e| DictHit {
                    code: e.code,
                    text: e.text,
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                })
                .collect(),
            // 内存回退（yaml 直载，未走 wdat）：CodetableDict 保有 boundary，一并带出。
            Self::Memory(dict) => dict.search_with_boundary(code),
        }
    }

    /// 前缀查找，**并返回音节边界**。与 [`Self::search_prefix`] 并存，理由同
    /// [`Self::search_with_boundary`]。前缀补全候选（输入 ni → 补出「你好」nihao）同样需要
    /// 边界供双拼校验。
    pub fn search_prefix_with_boundary(&self, prefix: &str, limit: usize) -> Vec<DictHit> {
        match self {
            Self::Mmap(reader) => reader
                .search_prefix(prefix, limit)
                .into_iter()
                .map(|e| DictHit {
                    code: e.code,
                    text: e.text,
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                })
                .collect(),
            Self::Memory(dict) => dict.search_prefix_with_boundary(prefix, limit),
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

    /// 构建**单字全码表**：汉字 → 该字在本词库中的全码。供码表造词按 `[[encoder.rules]]`
    /// 公式组装词组编码（`wind_engine::encoder`）。
    ///
    /// # 全码判据（按序）
    ///
    /// 1. **上限闸**：滤掉码长 > `max_code_length` 的编码（`0` = 不设闸）。扩展词库塞进来的
    ///    5/6 码怪码在此被排除——否则它就是「最长码」，后续判据根本没机会参与。
    /// 2. **最长码长**：简码（如「工」=`a`）位数不够公式取第 2 码，必须取全码。
    /// 3. **权重降序**：同码长多码时取权重高者。
    /// 4. **码字典序升序**：最终确定性兜底。
    ///
    /// # 为什么第 4 条不是「首次出现」
    ///
    /// `CodetableDict::for_each_entry` 遍历的是 `HashMap`，**顺序不确定**；mmap 路径则为码
    /// 字典序。取「首次出现」会让同权同长的字在两条路径下、甚至两次构建间得到不同的码。
    /// 字典序是确定性代偿，且与 mmap 路径的天然顺序一致。
    pub fn build_single_char_full_codes(
        &self,
        max_code_length: usize,
    ) -> std::collections::HashMap<char, String> {
        use std::collections::HashMap;
        // 值存 (code, weight)；weight 仅用于比较，不外传。
        let mut best: HashMap<char, (String, i32)> = HashMap::new();
        self.for_each_entry(&mut |code, text, weight| {
            let mut it = text.chars();
            let (Some(ch), None) = (it.next(), it.next()) else {
                return; // 只收单字条目
            };
            let len = code.chars().count();
            if len == 0 || (max_code_length > 0 && len > max_code_length) {
                return;
            }
            match best.get(&ch) {
                Some((cur, cur_w)) => {
                    let cur_len = cur.chars().count();
                    let better = len > cur_len
                        || (len == cur_len
                            && (weight > *cur_w || (weight == *cur_w && code < cur.as_str())));
                    if better {
                        best.insert(ch, (code.to_string(), weight));
                    }
                }
                None => {
                    best.insert(ch, (code.to_string(), weight));
                }
            }
        });
        best.into_iter().map(|(k, (code, _))| (k, code)).collect()
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

    /// 全码判据①②：简码不能胜出（否则公式取第 2 码越界），且超过 max_code_length 的
    /// 怪码被上限闸排除——这两条各自都足以让造词静默失效。
    #[test]
    fn single_char_full_code_prefers_longest_within_cap() {
        let mut d = CodetableDict::empty();
        // 「工」：一级简码 a + 全码 aaaa → 应取 aaaa（简码只有 1 位，取不到第 2 码）。
        d.merge_single("a".into(), "工".into(), 9999, 0);
        d.merge_single("aaaa".into(), "工".into(), 100, 1);
        // 「中」：全码 khk + 扩展库塞进来的 6 码怪码 → 上限闸(4) 排除怪码，取 khk。
        d.merge_single("khk".into(), "中".into(), 500, 2);
        d.merge_single("khkkhk".into(), "中".into(), 9999, 3);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(
            idx.get(&'工').map(String::as_str),
            Some("aaaa"),
            "简码权重再高也不能当全码"
        );
        assert_eq!(
            idx.get(&'中').map(String::as_str),
            Some("khk"),
            "超 max_code_length 的怪码应被上限闸排除，即使它更长、权重更高"
        );
    }

    /// 上限闸关闭（0）时退化为纯「最长优先」——非定长码方案不应被误滤。
    #[test]
    fn single_char_full_code_cap_zero_disables_gate() {
        let mut d = CodetableDict::empty();
        d.merge_single("khk".into(), "中".into(), 500, 0);
        d.merge_single("khkkhk".into(), "中".into(), 100, 1);
        let cd = CachedDict::Memory(d);
        assert_eq!(
            cd.build_single_char_full_codes(0).get(&'中').map(String::as_str),
            Some("khkkhk"),
            "cap=0 应不设闸，取最长"
        );
    }

    /// 全码判据③④：同码长先比权重，权重相同再比码字典序（确定性兜底）。
    #[test]
    fn single_char_full_code_breaks_ties_by_weight_then_code() {
        let mut d = CodetableDict::empty();
        // 同为 2 码：权重高的 de 胜出。
        d.merge_single("dd".into(), "大".into(), 10, 0);
        d.merge_single("de".into(), "大".into(), 99, 1);
        // 同为 2 码且同权重：字典序小的 aa 胜出。
        d.merge_single("ab".into(), "式".into(), 50, 2);
        d.merge_single("aa".into(), "式".into(), 50, 3);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(idx.get(&'大').map(String::as_str), Some("de"));
        assert_eq!(idx.get(&'式').map(String::as_str), Some("aa"));
    }

    /// 只收单字：多字词条不得进入全码表（否则「你好」会被当成一个"字"）。
    #[test]
    fn single_char_full_code_skips_multi_char_entries() {
        let mut d = CodetableDict::empty();
        d.merge_single("wqvb".into(), "你好".into(), 100, 0);
        d.merge_single("wqiy".into(), "你".into(), 100, 1);
        let cd = CachedDict::Memory(d);
        let idx = cd.build_single_char_full_codes(4);
        assert_eq!(idx.len(), 1, "多字词条应被跳过");
        assert!(idx.contains_key(&'你'));
    }

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
