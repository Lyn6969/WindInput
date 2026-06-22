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
            let (mut code, text) = if first_is_code {
                // 五笔格式: code\ttext
                (parts[0].to_string(), parts[1].to_string())
            } else {
                // 拼音格式: text\tcode（去掉空格，使 "ni hao" -> "nihao"）
                let code = parts[1].replace(' ', "");
                (code, parts[0].to_string())
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

    /// 合并单个条目（用于从 CachedDict 提取数据）
    pub fn merge_single(&mut self, code: String, text: String, weight: i32, _order: i32) {
        let existing = self.entries.entry(code).or_default();
        existing.push(CodetableEntry {
            text,
            weight,
            order: existing.len() as i32,
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

/// 解析一行 rime 词条 → `(code, text, weight)`，格式自适配（五笔 `code\ttext\tweight`
/// 或拼音 `text\tcode\tweight`）。与 [`CodetableDict::load_impl`] 的行解析逻辑一致。
/// 返回 None 表示该行应跳过（空行 / `#` 注释 / 字段不足）。
fn parse_rime_line(line: &str, lowercase_code: bool) -> Option<(String, String, i32)> {
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
    let (mut code, text) = if first_is_code {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        (parts[1].replace(' ', ""), parts[0].to_string())
    };
    if lowercase_code {
        code = code.to_lowercase();
    }
    let weight: i32 = if parts.len() >= 3 {
        parts[2].parse().unwrap_or(0)
    } else {
        0
    };
    Some((code, text, weight))
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
    if content[line_start..].strip_suffix('\r').unwrap_or(&content[line_start..]) == "..." {
        Some(content.len())
    } else {
        None
    }
}

/// 并行解析 rime `.dict.yaml` 正文为 `(code, text, weight)` 列表。
///
/// 跳过 YAML 头部（到首个独占一行的 `...` 为止），正文按**行边界**切成 N 块、`thread::scope`
/// 多线程解析（行解析是纯 CPU、可完美并行——拼音大词库的主要耗时）。块边界对齐 `\n`
/// （该字节不会落在 UTF-8 多字节序列内部），故切片始终在合法 char 边界。
/// 顺序不保证与文件一致：merged 路径会按权重重排，无需稳定顺序。
pub fn parse_rime_entries_parallel(
    path: impl AsRef<Path>,
    lowercase_code: bool,
) -> anyhow::Result<Vec<(String, String, i32)>> {
    let content = fs::read_to_string(path)?;
    let Some(off) = rime_body_offset(&content) else {
        return Ok(Vec::new());
    };
    let body = &content[off..];

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // 小文件 / 单核：串行，省去切块与起线程开销。
    if threads <= 1 || body.len() < (1 << 20) {
        let mut out = Vec::new();
        for line in body.lines() {
            if let Some(e) = parse_rime_line(line, lowercase_code) {
                out.push(e);
            }
        }
        return Ok(out);
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

    let parts: Vec<Vec<(String, String, i32)>> = std::thread::scope(|s| {
        let handles: Vec<_> = chunks
            .iter()
            .map(|chunk| {
                s.spawn(move || {
                    let mut out = Vec::new();
                    for line in chunk.lines() {
                        if let Some(e) = parse_rime_line(line, lowercase_code) {
                            out.push(e);
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut out = Vec::with_capacity(parts.iter().map(|v| v.len()).sum());
    for v in parts {
        out.extend(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
    fn collect(entries: &[(String, String, i32)], text: &str) -> Vec<(String, i32)> {
        entries
            .iter()
            .filter(|(_, t, _)| t == text)
            .map(|(c, _, w)| (c.clone(), *w))
            .collect()
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
        let e = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(e.len(), 2, "应解析 2 条，跳过注释/空行: {e:?}");
        assert_eq!(collect(&e, "你好"), vec![("nihao".to_string(), 1200)]);
        assert_eq!(collect(&e, "你"), vec![("ni".to_string(), 800)]);
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
        let e = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(e.len(), n, "并行切块不应丢行/重复");
        // 抽样首/中/尾
        assert_eq!(collect(&e, "文0"), vec![("code0".to_string(), 0)]);
        assert_eq!(collect(&e, "文59999"), vec![("code59999".to_string(), 59999)]);
        // 全部 code 唯一（边界未把某行切成两半）
        let mut codes: Vec<&str> = e.iter().map(|(c, _, _)| c.as_str()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), n, "所有 code 应唯一");
    }
}
