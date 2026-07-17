//! Rime Codetable 词典读取器 (.dict.yaml 格式)
//!
//! 格式：YAML 头部 + TSV 正文（code\ttext\tweight）
//! 与 Go 版 `wind_input/internal/dict/codetable/` 对齐。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tracing::info;

/// 词典条目
#[derive(Debug, Clone)]
pub struct CodetableEntry {
    pub text: String,
    pub weight: i32,
    pub order: i32,
    /// 音节边界 bitmask，见 [`crate::binformat::DictEntry::boundary`]。
    /// 拼音词库取自源数据空格（`ni hao` → {0,2}）；五笔等无空格码为 0（无边界信息）。
    pub boundary: u64,
}

/// 由 rime 的空格分隔码算音节起始位 bitmask：`"ni hao"` → 音节 ni|hao → 起始 {0,2} → `0b101`。
///
/// **这个空格就是音节边界的真值来源**——词库作者写下的、无需推断的事实。丢掉它就只能靠 DAG
/// 反猜切分，而 DAG 只按「覆盖字符数」最大化，`xian` 是 xi'an 还是 xian 它无从分辨。
///
/// 返回 0 仅表示「无边界信息」，消费方须降级回 DAG：空码，或拼接后 ≥64 字节的超长码
/// （bitmask 装不下，宁可整体降级也不给半截错误边界；拼音词长上限远小于此，实际不触发）。
/// 单音节返回 `0b1` 而非 0——「整串是一个音节」是真实信息，不是「不知道」。
/// 五笔等非拼音码不走本函数（其 boundary 恒 0），故无「把无空格码误标成单音节」之虞。
fn syllable_boundary_mask(spaced_code: &str) -> u64 {
    let mut mask = 0u64;
    let mut pos = 0usize;
    for syl in spaced_code.split(' ').filter(|s| !s.is_empty()) {
        if pos >= 64 {
            return 0; // 超出 bitmask 表达范围 → 整体降级，不给出半截错误边界
        }
        mask |= 1u64 << pos;
        pos += syl.len();
    }
    mask
}

/// Rime Codetable 词典（内存模式，按 code 分组的 BTreeMap）
pub struct CodetableDict {
    /// code -> entries（按 weight 降序排列）
    entries: BTreeMap<String, Vec<CodetableEntry>>,
    /// 总条目数
    total_entries: usize,
}

impl CodetableDict {
    /// 从 .dict.yaml 文件加载（code 保持原样）
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::load_impl(path, false)
    }

    /// 从 .dict.yaml 加载并把 code 列小写化（英文词库用：大小写不敏感前缀匹配，text 保留原样大小写）
    pub fn load_lowercased(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Self::load_impl(path, true)
    }

    fn load_impl(path: impl AsRef<Path>, lowercase_code: bool) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)?;

        let mut entries: BTreeMap<String, Vec<CodetableEntry>> = BTreeMap::new();
        let mut in_body = false;
        let mut order: i32 = 0;

        for line in content.lines() {
            if !in_body {
                if line == "..." {
                    in_body = true;
                }
                continue;
            }

            // 跳过空行和注释
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // 格式检测：
            // - 五笔: code\ttext\tweight (如: a	工	9999)
            // - 拼音: text\tcode\tweight (如: 啊	a	241987)
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }

            // 检测格式：第一列是否为 ASCII（五笔码）或中文（拼音文本）
            let first_is_code = parts[0].chars().all(|c| c.is_ascii());
            let (mut code, text, boundary) = if first_is_code {
                // 五笔格式: code\ttext —— 无音节概念，boundary=0。
                (parts[0].to_string(), parts[1].to_string(), 0u64)
            } else {
                // 拼音格式: text\tcode。code 去空格拼平（"ni hao" -> "nihao"）供 key/前缀查询，
                // 但空格承载的音节边界先留存到 boundary，不再丢弃。
                (
                    parts[1].replace(' ', ""),
                    parts[0].to_string(),
                    syllable_boundary_mask(parts[1]),
                )
            };
            if lowercase_code {
                code = code.to_lowercase();
            }

            let weight: i32 = if parts.len() >= 3 {
                parts[2].parse().unwrap_or(0)
            } else {
                0
            };

            let entry = CodetableEntry {
                text,
                weight,
                order,
                boundary,
            };

            entries.entry(code).or_default().push(entry);
            order += 1;
        }

        // 每个 code 下按 weight 降序排列
        for code_entries in entries.values_mut() {
            code_entries.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        }

        let total: usize = entries.values().map(|v| v.len()).sum();
        info!(
            "Loaded codetable: {} ({} keys, {} entries)",
            path.display(),
            entries.len(),
            total
        );

        Ok(Self {
            entries,
            total_entries: total,
        })
    }

    /// 精确查找
    pub fn search(&self, code: &str) -> Vec<(String, i32, i32)> {
        self.entries
            .get(code)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| (e.text.clone(), e.weight, e.order))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 精确查找，并带出音节边界（内存路径对应 [`crate::cached::CachedDict::search_with_boundary`]）。
    pub fn search_with_boundary(&self, code: &str) -> Vec<crate::cached::DictHit> {
        self.entries
            .get(code)
            .map(|entries| {
                entries
                    .iter()
                    .map(|e| crate::cached::DictHit {
                        code: code.to_string(),
                        text: e.text.clone(),
                        weight: e.weight,
                        order: e.order,
                        boundary: e.boundary,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 前缀查找，并带出音节边界（内存路径对应
    /// [`crate::cached::CachedDict::search_prefix_with_boundary`]）。排序/截断语义同
    /// [`Self::search_prefix`]。
    pub fn search_prefix_with_boundary(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Vec<crate::cached::DictHit> {
        let mut results: Vec<crate::cached::DictHit> = Vec::new();
        for (code, entries) in self.entries.range(prefix.to_string()..) {
            if !code.starts_with(prefix) {
                break;
            }
            for e in entries {
                results.push(crate::cached::DictHit {
                    code: code.clone(),
                    text: e.text.clone(),
                    weight: e.weight,
                    order: e.order,
                    boundary: e.boundary,
                });
            }
            if results.len() >= limit * 2 {
                break; // 收集足够多后排序截断（同 search_prefix）
            }
        }
        results.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        results.truncate(limit);
        results
    }

    /// 前缀查找
    pub fn search_prefix(&self, prefix: &str, limit: usize) -> Vec<(String, String, i32, i32)> {
        let mut results = Vec::new();

        // BTreeMap 范围查询：找到所有以 prefix 开头的 key
        let range = self.entries.range(prefix.to_string()..);
        for (code, entries) in range {
            if !code.starts_with(prefix) {
                break;
            }
            for e in entries {
                results.push((code.clone(), e.text.clone(), e.weight, e.order));
            }
            if results.len() >= limit * 2 {
                break; // 收集足够多后排序截断
            }
        }

        // 按 weight 降序排序
        results.sort_by(|a, b| b.2.cmp(&a.2).then(a.3.cmp(&b.3)));
        results.truncate(limit);
        results
    }

    /// 遍历全部条目(供反查索引构建):对每个 (code, text, weight) 调用 `f`。
    pub fn for_each_entry(&self, f: &mut dyn FnMut(&str, &str, i32)) {
        for (code, entries) in &self.entries {
            for e in entries {
                f(code, &e.text, e.weight);
            }
        }
    }

    /// 总条目数
    pub fn len(&self) -> usize {
        self.total_entries
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.total_entries == 0
    }

    /// 创建空词典
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
            total_entries: 0,
        }
    }

    /// 合并另一个词典（用于 rime_pinyin 的 import_tables）
    pub fn merge(&mut self, other: CodetableDict) {
        for (code, entries) in other.entries {
            let existing = self.entries.entry(code).or_default();
            let base_order = existing.len() as i32;
            for mut entry in entries {
                entry.order += base_order;
                existing.push(entry);
            }
        }
        self.total_entries = self.entries.values().map(|v| v.len()).sum();
    }

    /// 导出到 DictWriter（用于写入 .wdb 缓存）
    pub fn export_to_writer(&self, writer: &mut crate::binformat::DictWriter) {
        for (code, entries) in &self.entries {
            let entries_data: Vec<(String, i32)> =
                entries.iter().map(|e| (e.text.clone(), e.weight)).collect();
            writer.add(code.clone(), entries_data);
        }
    }

    /// 同 [`export_to_writer`]，导出到 wdat（DAT）写入器。
    /// 携带每条的全局 `order`（词库文件出现序）：使无权重候选跨编码按出现顺序排序，
    /// 而非退化为编码字母序（对应 wdat v3 的 order 字段，见 datformat.rs）。
    /// 一并携带 `boundary`（wdat v4 音节边界；非拼音词库为 0）。
    pub fn export_to_wdat(&self, writer: &mut crate::datformat::WdatWriter) {
        for (code, entries) in &self.entries {
            let entries_data: Vec<(String, i32, u32, u64)> = entries
                .iter()
                .map(|e| (e.text.clone(), e.weight, e.order.max(0) as u32, e.boundary))
                .collect();
            writer.add_with_boundary(code.clone(), entries_data);
        }
    }

    /// 合并单个条目（用于从 CachedDict 提取数据）。
    /// 入参只有扁平 code，无音节信息 → boundary=0（消费方降级回 DAG）。
    pub fn merge_single(&mut self, code: String, text: String, weight: i32, _order: i32) {
        let existing = self.entries.entry(code).or_default();
        existing.push(CodetableEntry {
            text,
            weight,
            order: existing.len() as i32,
            boundary: 0,
        });
        self.total_entries += 1;
    }

    /// 写入 .wdb 缓存文件
    pub fn write_to_wdb(&self, path: &std::path::Path) -> anyhow::Result<()> {
        use crate::binformat::DictWriter;
        let mut writer = DictWriter::new();
        self.export_to_writer(&mut writer);
        writer.write(path)
    }
}

/// 解析一行 rime 词条 → `(code, abbrev, text, weight)`，格式自适配（五笔 `code\ttext\tweight`
/// 或拼音 `text\tcode\tweight`）。`abbrev`=简拼（声母缩写）：仅拼音多音节词有，取每个空格
/// 分隔音节的首字母（如 `ni hao`→`nh`）；五笔/单音节为 None。返回 None 表示跳过该行。
pub(crate) struct RimeLine {
    pub code: String,
    pub abbrev: Option<String>,
    pub text: String,
    pub weight: i32,
    /// 音节边界 bitmask（见 [`syllable_boundary_mask`]）；五笔码为 0。
    pub boundary: u64,
}

fn parse_rime_line(line: &str, lowercase_code: bool) -> Option<RimeLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 2 {
        return None;
    }
    // 第一列全 ASCII → 五笔(code 在前)；否则拼音(text 在前，code 去空格)。
    let first_is_code = parts[0].chars().all(|c| c.is_ascii());
    let (mut code, mut abbrev, text, boundary) = if first_is_code {
        (parts[0].to_string(), None, parts[1].to_string(), 0u64)
    } else {
        // 简拼：2+ 音节时取每个空格分隔音节的首字母（对齐 Go loadRimeFile）。
        let spaced = parts[1];
        let syllables: Vec<&str> = spaced.split(' ').filter(|s| !s.is_empty()).collect();
        let abbrev = if syllables.len() >= 2 {
            Some(
                syllables
                    .iter()
                    .filter_map(|s| s.chars().next())
                    .collect::<String>(),
            )
        } else {
            None
        };
        // 同一批空格既供简拼取首字母，也供 boundary 记边界——此前只用了前者，
        // 转手就 replace(' ',"") 把边界扔了，逼得查询侧用 DAG 猜、造词侧暴力反推。
        (
            spaced.replace(' ', ""),
            abbrev,
            parts[0].to_string(),
            syllable_boundary_mask(spaced),
        )
    };
    if lowercase_code {
        code = code.to_lowercase();
        abbrev = abbrev.map(|a| a.to_lowercase());
    }
    let weight: i32 = if parts.len() >= 3 {
        parts[2].parse().unwrap_or(0)
    } else {
        0
    };
    Some(RimeLine {
        code,
        abbrev,
        text,
        weight,
        boundary,
    })
}

/// 正文起点：首个（按 `str::lines()` 语义，即剥除 `\r` 后）等于 `...` 的行之后的字节偏移。
/// 无该分隔行 → None（与 load_impl 一致：无正文标记则零条目）。
fn rime_body_offset(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut line_start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            let raw = &content[line_start..i];
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            if line == "..." {
                return Some(i + 1);
            }
            line_start = i + 1;
        }
    }
    if content[line_start..]
        .strip_suffix('\r')
        .unwrap_or(&content[line_start..])
        == "..."
    {
        Some(content.len())
    } else {
        None
    }
}

/// 并行解析 rime `.dict.yaml` 正文为 `(全拼条目, 简拼条目)` 两组，各元素 `(code,text,weight)`。
/// 简拼条目即声母缩写表（如 `nh`→你好），供 wdat 独立 AbbrevSection。
///
/// 跳过 YAML 头部（到首个独占一行的 `...` 为止），正文按**行边界**切成 N 块、`thread::scope`
/// 多线程解析（行解析是纯 CPU、可完美并行——拼音大词库的主要耗时）。块边界对齐 `\n`
/// （该字节不会落在 UTF-8 多字节序列内部），故切片始终在合法 char 边界。
/// 顺序不保证与文件一致：merged 路径会按权重重排，无需稳定顺序。
/// `(fulls, abbrevs)`；fulls 每条 `(code, text, weight, boundary)`，abbrevs 每条 `(abbrev, text, weight)`。
/// 简拼码（`nh`）不带 boundary——它是各音节首字母的拼接，本身不构成音节序列，无边界语义。
type RimeEntries = (Vec<(String, String, i32, u64)>, Vec<(String, String, i32)>);

pub fn parse_rime_entries_parallel(
    path: impl AsRef<Path>,
    lowercase_code: bool,
) -> anyhow::Result<RimeEntries> {
    let content = fs::read_to_string(path)?;
    let Some(off) = rime_body_offset(&content) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let body = &content[off..];

    // 解析一块 → (全拼, 简拼)。
    let parse_chunk = |chunk: &str| -> RimeEntries {
        let mut fulls = Vec::new();
        let mut abbrevs = Vec::new();
        for line in chunk.lines() {
            if let Some(r) = parse_rime_line(line, lowercase_code) {
                if let Some(ab) = r.abbrev {
                    abbrevs.push((ab, r.text.clone(), r.weight));
                }
                fulls.push((r.code, r.text, r.weight, r.boundary));
            }
        }
        (fulls, abbrevs)
    };

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // 小文件 / 单核：串行，省去切块与起线程开销。
    if threads <= 1 || body.len() < (1 << 20) {
        return Ok(parse_chunk(body));
    }

    // 按字节均分，再各自前推到下一个换行后，得到不跨行的块边界。
    let bytes = body.as_bytes();
    let mut bounds = vec![0usize];
    for k in 1..threads {
        let mut p = (body.len() as u64 * k as u64 / threads as u64) as usize;
        while p < body.len() && bytes[p] != b'\n' {
            p += 1;
        }
        if p < body.len() {
            p += 1; // 跨过换行，块从下一行起
        }
        if p > *bounds.last().unwrap() {
            bounds.push(p);
        }
    }
    bounds.push(body.len());
    bounds.dedup();

    let chunks: Vec<&str> = bounds.windows(2).map(|w| &body[w[0]..w[1]]).collect();

    let parts: Vec<RimeEntries> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                s.spawn(move || -> RimeEntries {
                    let mut fulls = Vec::new();
                    let mut abbrevs = Vec::new();
                    for line in chunk.lines() {
                        if let Some(r) = parse_rime_line(line, lowercase_code) {
                            if let Some(ab) = r.abbrev {
                                abbrevs.push((ab, r.text.clone(), r.weight));
                            }
                            fulls.push((r.code, r.text, r.weight, r.boundary));
                        }
                    }
                    (fulls, abbrevs)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut fulls = Vec::new();
    let mut abbrevs = Vec::new();
    for (f, a) in parts {
        fulls.extend(f);
        abbrevs.extend(a);
    }
    Ok((fulls, abbrevs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn syllable_boundary_mask_basics() {
        // 多音节：起始字节位 {0,2}（ni 占 0..2，hao 占 2..5）。
        assert_eq!(syllable_boundary_mask("ni hao"), 0b101);
        // 变长音节：zhuang(6B) 起始 0，ni 起始 6。
        assert_eq!(syllable_boundary_mask("zhuang ni"), 0b1000001);
        // 单音节：整串一个音节，起始 {0}。是真实信息，不是「未知」。
        assert_eq!(syllable_boundary_mask("ni"), 0b1);
        // 空码 → 无信息。
        assert_eq!(syllable_boundary_mask(""), 0);
        // 超长码（拼接 ≥64B）：bitmask 装不下 → 整体降级为 0，不给半截错误边界。
        let long = vec!["zhuang"; 12].join(" "); // 12*6 = 72B
        assert_eq!(syllable_boundary_mask(&long), 0);
    }

    /// 端到端：rime 源 → 解析 → wdat 落盘 → mmap 读回，边界必须原样穿过整条链路。
    /// 这是 v4 的核心契约——此前边界在解析期就被 replace(' ',"") 丢弃，根本到不了磁盘。
    #[test]
    fn boundary_survives_wdat_roundtrip() {
        let dir = std::env::temp_dir().join("wind_boundary_roundtrip");
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("py.dict.yaml");
        {
            let mut f = std::fs::File::create(&src).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "你好\tni hao\t1200").unwrap();
            writeln!(f, "你\tni\t800").unwrap();
            // 同 code 不同切分：xi'an（西安，2 音节）vs xian（先，1 音节）。
            // 正是 DAG 无从分辨、必须靠词典真值的场景（两者覆盖字符数相同）。
            writeln!(f, "西安\txi an\t500").unwrap();
            writeln!(f, "先\txian\t900").unwrap();
        }
        let dict = CodetableDict::load(&src).unwrap();

        let mut w = crate::datformat::WdatWriter::new();
        dict.export_to_wdat(&mut w);
        let wdat = dir.join("py.wdat");
        w.write(&wdat).unwrap();

        let reader = crate::datformat::WdatReader::open(&wdat).unwrap();
        let find = |code: &str, text: &str| -> Option<u64> {
            reader
                .search(code)
                .into_iter()
                .find(|e| e.text == text)
                .map(|e| e.boundary)
        };

        assert_eq!(find("nihao", "你好"), Some(0b101), "ni|hao 边界应读回");
        assert_eq!(find("ni", "你"), Some(0b1));
        // 关键：同一 key "xian" 下两条候选各自带边界，据此可区分 xi|an 与 xian。
        assert_eq!(find("xian", "西安"), Some(0b101), "xi|an → 起始 {{0,2}}");
        assert_eq!(find("xian", "先"), Some(0b1), "xian → 单音节，起始 {{0}}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 英文词库格式 `word<TAB>word`（混合大小写）：load_lowercased 应小写化 code、
    /// 保留 text 原样，使大小写不敏感前缀匹配生效。
    #[test]
    fn load_lowercased_english() {
        let path = std::env::temp_dir().join("wind_en_lowercase_test.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: en\n...").unwrap();
            writeln!(f, "# ab\tab").unwrap(); // 注释行跳过
            writeln!(f, "Aaron\tAaron").unwrap();
            writeln!(f, "abandon\tabandon").unwrap();
            writeln!(f, "ABC\tABC").unwrap();
        }
        let d = CodetableDict::load_lowercased(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        // 小写前缀 "aa" 命中 Aaron（原样大小写 text）
        let r = d.search_prefix("aa", 10);
        assert!(
            r.iter()
                .any(|(code, text, _, _)| code == "aaron" && text == "Aaron"),
            "应小写码命中、保留原样 text: {:?}",
            r
        );
        // 精确小写 "abc" 命中 ABC
        assert!(d.search("abc").iter().any(|(t, _, _)| t == "ABC"));
    }

    /// 并行解析：拼音格式（text\tcode\tweight，code 去空格）+ 注释/空行跳过，
    /// 小文件走串行分支，结果应完整正确。
    /// 取 fulls（(code, text, weight, boundary)）中某 text 的 (code, weight)。
    fn collect(entries: &[(String, String, i32, u64)], text: &str) -> Vec<(String, i32)> {
        entries
            .iter()
            .filter(|(_, t, _, _)| t == text)
            .map(|(c, _, w, _)| (c.clone(), *w))
            .collect()
    }

    /// 取 abbrevs（(abbrev, text, weight)，无 boundary）中某 text 的 (abbrev, weight)。
    fn collect_ab(entries: &[(String, String, i32)], text: &str) -> Vec<(String, i32)> {
        entries
            .iter()
            .filter(|(_, t, _)| t == text)
            .map(|(c, _, w)| (c.clone(), *w))
            .collect()
    }

    /// 取 fulls 中某 text 的 boundary。
    fn boundary_of(entries: &[(String, String, i32, u64)], text: &str) -> Option<u64> {
        entries
            .iter()
            .find(|(_, t, _, _)| t == text)
            .map(|(_, _, _, b)| *b)
    }

    #[test]
    fn parallel_parse_pinyin_format_small() {
        let path = std::env::temp_dir().join("wind_parrime_small.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: py\n...").unwrap();
            writeln!(f, "# 注释跳过").unwrap();
            writeln!(f).unwrap(); // 空行跳过
            writeln!(f, "你好\tni hao\t1200").unwrap(); // code 去空格 -> nihao
            writeln!(f, "你\tni\t800").unwrap();
        }
        let (e, ab) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(e.len(), 2, "应解析 2 条，跳过注释/空行: {e:?}");
        assert_eq!(collect(&e, "你好"), vec![("nihao".to_string(), 1200)]);
        assert_eq!(collect(&e, "你"), vec![("ni".to_string(), 800)]);
        // 简拼：多音节 "ni hao"→"nh"；单音节 "ni" 无简拼。
        assert_eq!(collect_ab(&ab, "你好"), vec![("nh".to_string(), 1200)]);
        assert!(collect_ab(&ab, "你").is_empty(), "单音节不产简拼");
        // 音节边界（v4）：源数据 "ni hao" 的空格是真值边界，不得随 code 拼平而丢弃。
        // "nihao" 音节 ni|hao → 起始字节 {0,2} → 0b101。
        assert_eq!(
            boundary_of(&e, "你好"),
            Some(0b101),
            "「你好」应记住 ni|hao 的边界"
        );
        // 单音节：整串一个音节 → 起始 {0} → 0b1（是真实信息，非「未知」）。
        assert_eq!(boundary_of(&e, "你"), Some(0b1));
    }

    /// 跨 1MB 阈值触发并行切块：构造大量行，断言总数与抽样正确、块边界不丢/不重行。
    #[test]
    fn parallel_parse_large_chunked_no_loss() {
        let path = std::env::temp_dir().join("wind_parrime_large.dict.yaml");
        let n = 60_000; // 每行约 20+ 字节 → 正文 > 1MB，触发并行分支
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            writeln!(f, "---\nname: big\n...").unwrap();
            for i in 0..n {
                // 五笔格式 code\ttext\tweight，code 全 ASCII
                writeln!(f, "code{i}\t文{i}\t{i}").unwrap();
            }
        }
        let (e, _ab) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(e.len(), n, "并行切块不应丢行/重复");
        // 抽样首/中/尾
        assert_eq!(collect(&e, "文0"), vec![("code0".to_string(), 0)]);
        assert_eq!(
            collect(&e, "文59999"),
            vec![("code59999".to_string(), 59999)]
        );
        // 五笔码无音节概念 → boundary 恒 0（消费方据此降级，不会误当拼音边界）。
        assert_eq!(boundary_of(&e, "文0"), Some(0), "五笔码不应有音节边界");
        // 全部 code 唯一（边界未把某行切成两半）
        let mut codes: Vec<&str> = e.iter().map(|(c, _, _, _)| c.as_str()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n, "所有 code 应唯一");
    }
}
