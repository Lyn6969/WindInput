//! Rime Codetable 词典读取器 (.dict.yaml 格式)
//!
//! 格式：YAML 头部 + TSV 正文（code\ttext\tweight）
//! 与 Go 版 `wind_input/internal/dict/codetable/` 对齐。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

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

/// 正文的列序。**文件级属性**：同一词库所有行的列序必然一致，故只判定一次、全文固定。
///
/// 曾经这是逐行猜的（`parts[0].chars().all(is_ascii)` → 认作码列），导致纯 ASCII 词条
/// （`@`、`$CC("[End]", …)`）被当成码、与编码列整个对调，静默装出一条镜像垃圾词条。
/// 更糟的是同一文件不同行可能被判成不同格式——列序是文件属性，逐行决策本身就是错的。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum ColumnLayout {
    /// `text\tcode\tweight`（拼音、符号、快符词库）。缺声明且探测无结论时的默认。
    #[default]
    TextFirst,
    /// `code\ttext\tweight`（五笔类）。
    CodeFirst,
}

impl ColumnLayout {
    /// 供日志展示的列名对（第一列, 第二列）。
    fn column_names(self) -> (&'static str, &'static str) {
        match self {
            ColumnLayout::TextFirst => ("text", "code"),
            ColumnLayout::CodeFirst => ("code", "text"),
        }
    }
}

/// 某列是否呈「编码」形态：小写字母 / 数字 / 音节分隔空格 / 隔音符。
///
/// **判据必须建在 code 列而非 text 列**——这是本模块此前出错的根源。code 的形态约束是强的
/// （码只能长成码的样子）；text 列可以是任何东西：汉字、`@`、`$CC("[End]", key.seq("End"))`、
/// 英文单词。对无约束的一侧做形态测试等于赌，对强约束的一侧做才成立。
fn is_code_shape(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' || c == '\'')
}

/// 单行投票。仅当两列中**恰有一列**像码时才给结论；两列都像（英文词库 `abandon\tabandon`）
/// 或都不像时弃权——弃权比瞎猜安全，多数票和默认值会兜住。
fn vote_layout(line: &str) -> Option<ColumnLayout> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < 2 {
        return None;
    }
    match (is_code_shape(parts[0]), is_code_shape(parts[1])) {
        (true, false) => Some(ColumnLayout::CodeFirst),
        (false, true) => Some(ColumnLayout::TextFirst),
        _ => None,
    }
}

/// 探测取样上限：最多扫这么多行正文。
const LAYOUT_SAMPLE_LINES: usize = 200;
/// 攒够这么多张有效票就提前收工，不必扫满 [`LAYOUT_SAMPLE_LINES`]。
const LAYOUT_SAMPLE_VOTES: usize = 32;

/// 按正文前若干行投票判列序，返回 `(列序, text优先票数, code优先票数)`。
/// 平票或零票 → [`ColumnLayout::TextFirst`]（默认）。
fn detect_layout(body: &str) -> (ColumnLayout, usize, usize) {
    let (mut text_first, mut code_first) = (0usize, 0usize);
    for line in body.lines().take(LAYOUT_SAMPLE_LINES) {
        match vote_layout(line) {
            Some(ColumnLayout::TextFirst) => text_first += 1,
            Some(ColumnLayout::CodeFirst) => code_first += 1,
            None => {}
        }
        if text_first + code_first >= LAYOUT_SAMPLE_VOTES {
            break;
        }
    }
    let layout = if code_first > text_first {
        ColumnLayout::CodeFirst
    } else {
        ColumnLayout::TextFirst
    };
    (layout, text_first, code_first)
}

/// 正文各列的角色分配。**文件级属性**，判定一次、全文固定。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ColumnSpec {
    text_col: usize,
    code_col: usize,
    /// 权重列下标。`None` = 该词库无权重列（`columns:` 声明里没有 `weight`）。
    ///
    /// 未声明 `columns:` 时取第 3 列（下标 2），对齐 librime 的默认列序
    /// `[text, code, weight]`（`dict_settings.cc` 的 `GetColumnIndex` 在 `columns` 为空时
    /// 硬编码 text=0/code=1/weight=2）。声明之外的列一律不读——真实反例
    /// `wubi86_jidian_extra_district.dict.yaml` 是 `text\tcode\t\t区号` 四列，
    /// 第 4 列是行政区划编号；它的第 3 列为空，故按 Rime 语义同样得权重 0。
    weight_col: Option<usize>,
}

impl ColumnSpec {
    /// 由探测/默认列序构造。权重仍取第 3 列，与 librime 无声明时的默认一致；
    /// 探测只负责 text/code 谁先谁后（librime 不做探测，恒 text 在前）。
    fn from_layout(layout: ColumnLayout) -> Self {
        match layout {
            ColumnLayout::TextFirst => Self {
                text_col: 0,
                code_col: 1,
                weight_col: Some(2),
            },
            ColumnLayout::CodeFirst => Self {
                text_col: 1,
                code_col: 0,
                weight_col: Some(2),
            },
        }
    }

    /// code 列在 text 列之前 → 五笔式码表，无音节语义（不算 boundary、不出简拼）。
    fn is_code_first(&self) -> bool {
        self.code_col < self.text_col
    }

    /// 本行至少要有这么多列才能取齐**必需**字段（text/code）。
    ///
    /// **权重不计入**：它是「有则取、无则 0」。若把 weight_col 也算进门槛，
    /// 只有两列的词库（快符 `12_kf.dict.yaml` 全部 26 行皆两列）会被整体丢弃。
    /// librime 同样是逐字段做 `num_columns > x_column` 的越界保护，而非整行门槛。
    fn required_cols(&self) -> usize {
        self.text_col.max(self.code_col) + 1
    }
}

/// 从 YAML 头部解析 `columns:` 声明，按声明顺序定位 text/code/weight 各列。
/// 无声明、或声明里缺 text/code（残缺声明不可用）→ None，由调用方降级到探测。
fn parse_columns_header(header: &str) -> Option<ColumnSpec> {
    let mut in_columns = false;
    let mut names: Vec<String> = Vec::new();
    for raw in header.lines() {
        // 剥行内注释：flypy 词库写作 `columns:    # 码表格式` / `  - text    # 文字`
        let line = raw.split('#').next().unwrap_or("");
        let trimmed = line.trim();
        if !in_columns {
            // 顶格的 `columns:` 才是块起点（缩进的同名键属于别的映射）
            if trimmed == "columns:" && !line.starts_with([' ', '\t']) {
                in_columns = true;
            }
            continue;
        }
        let Some(item) = trimmed.strip_prefix('-') else {
            if trimmed.is_empty() {
                continue; // 块内空行/纯注释行
            }
            break; // 回到非缩进键 → columns 块结束
        };
        names.push(item.trim().to_string());
    }
    if !in_columns {
        return None;
    }
    let find = |k: &str| names.iter().position(|n| n == k);
    // text/code 缺一不可；stem 等未支持的列名占位但不取用。
    Some(ColumnSpec {
        text_col: find("text")?,
        code_col: find("code")?,
        weight_col: find("weight"),
    })
}

/// 文件级列规格判定：头部 `columns:` 声明优先；缺声明则探测正文列序并 WARN 建议补声明。
fn resolve_columns(content: &str, body: &str, path: &Path) -> ColumnSpec {
    let header = &content[..content.len() - body.len()];
    if let Some(declared) = parse_columns_header(header) {
        return declared;
    }
    let (layout, text_first, code_first) = detect_layout(body);
    let (c1, c2) = layout.column_names();
    warn!(
        "词库 {} 未声明 columns:，按正文前 {} 行探测判定列序为 {}\\t{}\\tweight（{}票 text 优先 / {}票 code 优先）。\
         探测是启发式的，纯 ASCII 词条（如 @、$CC(...)）可能判错；\
         建议在 YAML 头部显式声明，例如 columns: [{}, {}, weight]",
        path.display(),
        LAYOUT_SAMPLE_LINES,
        c1,
        c2,
        text_first,
        code_first,
        c1,
        c2,
    );
    ColumnSpec::from_layout(layout)
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
        let mut order: i32 = 0;

        // 无 `...` 分隔行 → 无正文 → 零条目（与并行解析路径一致）。
        let body = rime_body_offset(&content).map_or("", |off| &content[off..]);
        // 列规格判定一次、全文固定——不再逐行猜（见 [`ColumnLayout`]）。
        let spec = resolve_columns(&content, body, path);

        for line in body.lines() {
            let Some(parsed) = parse_rime_line(line, lowercase_code, spec) else {
                continue;
            };

            let entry = CodetableEntry {
                text: parsed.text,
                weight: parsed.weight,
                order,
                boundary: parsed.boundary,
            };

            entries.entry(parsed.code).or_default().push(entry);
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

fn parse_rime_line(line: &str, lowercase_code: bool, spec: ColumnSpec) -> Option<RimeLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.split('\t').collect();
    if parts.len() < spec.required_cols() {
        return None;
    }
    // 列位置由文件级判定给定（头部 `columns:` 声明，或整文件探测），不再逐行猜。
    let (mut code, mut abbrev, text, boundary) = if spec.is_code_first() {
        // code\ttext —— 五笔类无音节概念，boundary=0、无简拼。
        (
            parts[spec.code_col].to_string(),
            None,
            parts[spec.text_col].to_string(),
            0u64,
        )
    } else {
        // 简拼：2+ 音节时取每个空格分隔音节的首字母（对齐 Go loadRimeFile）。
        let spaced = parts[spec.code_col];
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
            parts[spec.text_col].to_string(),
            syllable_boundary_mask(spaced),
        )
    };
    if lowercase_code {
        code = code.to_lowercase();
        abbrev = abbrev.map(|a| a.to_lowercase());
    }
    // 未声明 columns: 时 weight_col 为 None → 权重恒 0，不按位置猜第三列（可能是区号等）。
    let weight: i32 = spec
        .weight_col
        .and_then(|i| parts.get(i))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
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
    let path = path.as_ref();
    let content = fs::read_to_string(path)?;
    let Some(off) = rime_body_offset(&content) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let body = &content[off..];
    // 列规格判定一次、全文固定，随后传给每个并行块——保证跨块一致（逐行猜时同文件可能分裂）。
    let spec = resolve_columns(&content, body, path);

    // 解析一块 → (全拼, 简拼)。
    let parse_chunk = |chunk: &str| -> RimeEntries {
        let mut fulls = Vec::new();
        let mut abbrevs = Vec::new();
        for line in chunk.lines() {
            if let Some(r) = parse_rime_line(line, lowercase_code, spec) {
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
                        if let Some(r) = parse_rime_line(line, lowercase_code, spec) {
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
            // 声明 columns 才会取权重列（未声明时保守只认 text/code）
            writeln!(f, "---\nname: py\ncolumns:\n  - text\n  - code\n  - weight\n...").unwrap();
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
            writeln!(f, "---\nname: big\ncolumns:\n  - code\n  - text\n  - weight\n...").unwrap();
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

    /// 头部 `columns:` 声明是权威列规格，两种顺序都要认，且能穿过行内注释。
    #[test]
    fn columns_header_is_authoritative() {
        // flypy 风格：带行内注释
        let flypy = "---\nname: x\ncolumns:    # 码表格式\n  - text    # 文字\n  - code    # 输入码\n  - weight  # 权重\n...\n";
        assert_eq!(
            parse_columns_header(flypy),
            Some(ColumnSpec {
                text_col: 0,
                code_col: 1,
                weight_col: Some(2)
            })
        );
        // wubi 风格：code 在前
        let wubi = "---\nname: y\nsort: by_weight\ncolumns:\n  - code\n  - text\n  - weight\n...\n";
        assert_eq!(
            parse_columns_header(wubi),
            Some(ColumnSpec {
                text_col: 1,
                code_col: 0,
                weight_col: Some(2)
            })
        );
        // 只声明两列（用户为 12_kf 补的正是这种）→ 无权重列
        let two = "---\nname: kf\ncolumns:\n  - text\n  - code\n...\n";
        assert_eq!(
            parse_columns_header(two),
            Some(ColumnSpec {
                text_col: 0,
                code_col: 1,
                weight_col: None
            })
        );
        // 无声明 → None（交给探测）
        assert_eq!(parse_columns_header("---\nname: z\n...\n"), None);
        // 声明里出现未支持的列名：占位并顺延后续列下标，不得错位
        let stem = "---\ncolumns:\n  - text\n  - code\n  - stem\n  - weight\n...\n";
        assert_eq!(
            parse_columns_header(stem),
            Some(ColumnSpec {
                text_col: 0,
                code_col: 1,
                weight_col: Some(3)
            }),
            "stem 占一列，weight 应顺延到下标 3"
        );
        // 残缺声明（缺 code）不可用 → None，降级探测
        let broken = "---\ncolumns:\n  - text\n  - weight\n...\n";
        assert_eq!(parse_columns_header(broken), None, "缺 code 的声明不可用");
        // columns 块后回到别的键，不应越界把后续键读成列名
        let trailing = "---\ncolumns:\n  - code\n  - text\nsort: by_weight\n...\n";
        assert_eq!(
            parse_columns_header(trailing),
            Some(ColumnSpec {
                text_col: 1,
                code_col: 0,
                weight_col: None
            })
        );
    }

    /// **未声明 `columns:` 时只保守取 text/code 两列**，多余列一律忽略——不按位置猜权重。
    /// 真实反例：`wubi86_jidian_extra_district.dict.yaml` 是 `text\tcode\t\t区号` 四列，
    /// 第 4 列是行政区划编号；按位置猜会把区号读成词频。
    #[test]
    fn undeclared_columns_ignore_everything_past_code() {
        let path = std::env::temp_dir().join("wind_cols_undeclared_extra.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: district\n...").unwrap(); // 无 columns 声明
            writeln!(f, "北京市\tuyym\t\t110000").unwrap();
            writeln!(f, "东城区\tafaq\t\t110101").unwrap();
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            collect(&e, "北京市"),
            vec![("uyym".to_string(), 0)],
            "区号列不得被当作权重"
        );
        assert_eq!(collect(&e, "东城区"), vec![("afaq".to_string(), 0)]);
    }

    /// 未声明 `columns:` 时按 librime 默认 `[text, code, weight]` 取第 3 列作权重。
    #[test]
    fn undeclared_columns_follow_rime_default_weight() {
        let path = std::env::temp_dir().join("wind_cols_undeclared_w.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: x\n...").unwrap();
            writeln!(f, "你好\tni hao\t1200").unwrap();
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            collect(&e, "你好"),
            vec![("nihao".to_string(), 1200)],
            "未声明时应按 Rime 默认取第 3 列权重"
        );
    }

    /// 只有两列的词库（快符 `12_kf.dict.yaml` 全 26 行皆两列）不得因缺权重列被丢弃。
    /// 权重是「有则取、无则 0」，不能进最低列数门槛。
    #[test]
    fn two_column_rows_survive_when_weight_column_absent() {
        let path = std::env::temp_dir().join("wind_cols_two_only.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: kf\n...").unwrap();
            writeln!(f, "、\ty").unwrap();
            writeln!(f, "@\tt").unwrap();
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(e.len(), 2, "两列行不得被整体丢弃: {e:?}");
        assert_eq!(collect(&e, "@"), vec![("t".to_string(), 0)]);
    }

    /// 声明与内容形态冲突时以声明为准：英文词库两列都是 ASCII，探测必然弃权，
    /// 只有声明能救。这正是 wubi86_jidian_english（`abs\tABS\t100`）的形状。
    #[test]
    fn declared_columns_win_over_ambiguous_content() {
        let path = std::env::temp_dir().join("wind_cols_declared.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: en\ncolumns:\n  - code\n  - text\n  - weight\n...").unwrap();
            writeln!(f, "abs\tABS\t100").unwrap();
            writeln!(f, "adob\tAdobe\t20").unwrap();
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            collect(&e, "ABS"),
            vec![("abs".to_string(), 100)],
            "声明 code 在前 → text=ABS/code=abs"
        );
    }

    /// **本次修复的核心回归**：纯 ASCII 词条（快符 `@`、ASCII 参数的 `$CC(...)`）此前会被
    /// 当成码列，与编码整个对调、静默装出镜像垃圾词条。列序须由全文一次判定，不受单行影响。
    #[test]
    fn ascii_text_entries_not_mistaken_for_code_column() {
        let path = std::env::temp_dir().join("wind_kf_ascii.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            // 无 columns: 声明 —— 正是 12_kf.dict.yaml 的情况，走探测
            writeln!(f, "---\nname: kf\n...").unwrap();
            writeln!(f, "｀\tq").unwrap(); // 全角，非 ASCII → 投 TextFirst
            writeln!(f, "、\ty").unwrap();
            writeln!(f, "@\tt").unwrap(); // 纯 ASCII 词条：曾被判反
            writeln!(f, "$CC(last(), type(last()))\tf").unwrap(); // 纯 ASCII 命令：曾被判反
            writeln!(f, r#"$CC("[End]", key.seq("End"))	n"#).unwrap();
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            collect(&e, "@"),
            vec![("t".to_string(), 0)],
            "敲 t 应出 @（此前反成 code=@ / text=t）"
        );
        assert_eq!(
            collect(&e, "$CC(last(), type(last()))"),
            vec![("f".to_string(), 0)],
            "纯 ASCII 的 $CC 命令同样不得反转"
        );
        assert_eq!(
            collect(&e, r#"$CC("[End]", key.seq("End"))"#),
            vec![("n".to_string(), 0)]
        );
        // 非 ASCII 词条保持正确（回归保护）
        assert_eq!(collect(&e, "｀"), vec![("q".to_string(), 0)]);
    }

    /// 无声明的 code-first 词库仍应探测正确（码列含数字，text 为汉字）。
    #[test]
    fn detects_code_first_without_declaration() {
        let body = "a\t工\t9999\nggg\t三\t100\ncode1\t文\t5\n";
        let (layout, tf, cf) = detect_layout(body);
        assert_eq!(layout, ColumnLayout::CodeFirst, "票数 text={tf} code={cf}");
        assert_eq!((tf, cf), (0, 3));
    }

    /// 无声明的 text-first 词库（含纯 ASCII 词条）探测应判 TextFirst，
    /// 且纯 ASCII 行不干扰多数票。
    #[test]
    fn detects_text_first_without_declaration() {
        let body = "你好\tni hao\t1200\n@\tt\n、\ty\n";
        let (layout, tf, cf) = detect_layout(body);
        assert_eq!(layout, ColumnLayout::TextFirst, "票数 text={tf} code={cf}");
        assert_eq!((tf, cf), (3, 0), "`@\\tt` 也应投 TextFirst（@ 不是码形态）");
    }

    /// 两列都像码 / 都不像码 → 弃权，不瞎猜；零票时落到默认 TextFirst。
    #[test]
    fn ambiguous_lines_abstain_and_default_to_text_first() {
        assert_eq!(vote_layout("abandon\tabandon"), None, "两列都像码 → 弃权");
        assert_eq!(vote_layout("你好\t、"), None, "两列都不像码 → 弃权");
        assert_eq!(vote_layout("# 注释\tx"), None);
        assert_eq!(vote_layout("单列无tab"), None);
        let (layout, tf, cf) = detect_layout("abandon\tabandon\nABC\tABC\n");
        assert_eq!(layout, ColumnLayout::TextFirst, "零票应落默认");
        assert_eq!((tf, cf), (0, 0));
    }

    /// 列序是文件级属性：同一文件内少数派行不得把自己那行翻过来。
    /// （旧实现逐行猜，同文件可出现两种列序并存。）
    #[test]
    fn layout_is_file_level_not_per_line() {
        let path = std::env::temp_dir().join("wind_layout_filelevel.dict.yaml");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "---\nname: mixed\n...").unwrap();
            for i in 0..10 {
                writeln!(f, "字{i}\tcode{i}\t{i}").unwrap(); // 多数：TextFirst
            }
            writeln!(f, "~\tz").unwrap(); // 纯 ASCII 词条，仍须按 TextFirst 解
        }
        let (e, _) = parse_rime_entries_parallel(&path, false).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            collect(&e, "~"),
            vec![("z".to_string(), 0)],
            "少数派 ASCII 行须服从文件级列序"
        );
        assert_eq!(collect(&e, "字0"), vec![("code0".to_string(), 0)]);
    }
}
