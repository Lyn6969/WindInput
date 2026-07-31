//! 候选反查（编码反查 / 拆字 / 拼音）
//!
//! 与 Go 版本 `wind_input/internal/tooltip/` 对齐（简化版）。
//! 为悬停候选提供"如何输入"的提示：五笔编码（拆字）+ 拼音读音。
//!
//! 数据源：
//! - 拆字/编码：主码表方案 `[engine.chaizi].db_path` 指向的拆字库（字\t字根\t编码），
//!   路径由调用方解析（用户方案目录优先，回落系统 data/schemas/）
//! - 拼音：`pinyin_map.txt`（pinyin-data 格式：`U+4E00: yī  # 一`，多音字逗号分隔）
//!   由 wind-tools `gen_pinyin` 从 mozillazg/pinyin-data 合并生成。

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 悬停提示生成选项（对齐 Go `ui.tooltip.*` provider 开关）。
#[derive(Debug, Clone)]
pub struct TooltipOptions {
    /// 编码段（五笔编码）
    pub code: bool,
    /// 拼音段
    pub pinyin: bool,
    /// 拼音显示多音字所有读音（false 仅首音）
    pub heteronyms: bool,
    /// 每字最多显示读音数（0=不限）
    pub max_readings: usize,
    /// 拆字段（字根分解 [编码]）
    pub chaizi: bool,
}

impl Default for TooltipOptions {
    /// 与 Go 默认一致：编码+拼音(全读音)开，拆字关。
    fn default() -> Self {
        Self {
            code: true,
            pinyin: true,
            heteronyms: true,
            max_readings: 0,
            chaizi: false,
        }
    }
}

/// 反查表
#[derive(Default)]
pub struct ReverseLookup {
    /// 拆字表（字 → 字根/编码），可随主码表方案热重载（`reload_chaizi`）
    chaizi: ChaiziTable,
    /// 字 → 拼音读音（多音字按常用频率排序，最常用在前）
    pinyin: PinyinTable,
}

/// 拆字表：字 → (字根, 编码)。紧凑存储——按字升序的定长条目数组 + 共享文本 arena，
/// 相比每字两个 `HashMap<char, String>`（桶空位 + 逐串堆分配），十万字级词库省数倍内存。
/// 查询走二分：拆字仅用于悬停提示与加词出码，均为低频路径，无需 O(1)。
#[derive(Default)]
struct ChaiziTable {
    /// 按 `ch` 升序。条目文本区间：字根=[start, rad_end)、编码=[rad_end, code_end)，
    /// start = 前一条目的 code_end（首条为 0）。
    entries: Vec<ChaiziEntry>,
    /// 所有字根/编码文本按条目序连续拼接。
    arena: String,
}

struct ChaiziEntry {
    ch: char,
    rad_end: u32,
    code_end: u32,
}

impl ChaiziTable {
    /// 从 (字, 字根, 编码) 行构建；同字多行取靠后者（与旧 HashMap 覆盖语义一致）。
    fn build(mut rows: Vec<(char, &str, &str)>) -> Self {
        rows.sort_by_key(|r| r.0); // 稳定排序保文件序，取每组末条即"后行覆盖"
        let mut entries = Vec::with_capacity(rows.len());
        let mut arena = String::with_capacity(rows.iter().map(|r| r.1.len() + r.2.len()).sum());
        let mut i = 0;
        while i < rows.len() {
            let mut j = i;
            while j + 1 < rows.len() && rows[j + 1].0 == rows[i].0 {
                j += 1;
            }
            let (ch, rad, code) = rows[j];
            arena.push_str(rad);
            let rad_end = arena.len() as u32;
            arena.push_str(code);
            entries.push(ChaiziEntry {
                ch,
                rad_end,
                code_end: arena.len() as u32,
            });
            i = j + 1;
        }
        arena.shrink_to_fit();
        Self { entries, arena }
    }

    /// 二分查字，返回 (字根, 编码)（可为空串）。
    fn lookup(&self, c: char) -> Option<(&str, &str)> {
        let i = self.entries.binary_search_by_key(&c, |e| e.ch).ok()?;
        let start = if i == 0 {
            0
        } else {
            self.entries[i - 1].code_end as usize
        };
        let e = &self.entries[i];
        Some((
            &self.arena[start..e.rad_end as usize],
            &self.arena[e.rad_end as usize..e.code_end as usize],
        ))
    }

    fn radicals(&self, c: char) -> Option<&str> {
        self.lookup(c).map(|(r, _)| r).filter(|s| !s.is_empty())
    }

    fn code(&self, c: char) -> Option<&str> {
        self.lookup(c).map(|(_, c)| c).filter(|s| !s.is_empty())
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// 拼音表：字 → 读音列表（多音字按常用频率排序，最常用在前）。与 `ChaiziTable` 同构的紧凑
/// 存储——按字升序的定长条目数组 + 共享文本 arena，相比 `HashMap<char, Vec<String>>`
/// （桶空位 + 每字一个 Vec + 每读音一次堆分配）省数倍内存。读音是变长列表，故比拆字表多一级
/// `reading_ends` 下标。查询走二分：拼音仅用于悬停提示与加词出码，均为低频路径，无需 O(1)。
#[derive(Default)]
struct PinyinTable {
    /// 按 `ch` 升序。本条读音在 `reading_ends` 中的下标区间
    /// = [前一条目的 `reading_end`, 本条 `reading_end`)，首条起点为 0。
    entries: Vec<PinyinEntry>,
    /// 每个读音在 `arena` 中的结束偏移，按条目序连续。
    /// 单条读音的文本区间 = [前一项, 本项)，首项起点为 0。
    reading_ends: Vec<u32>,
    /// 所有读音文本按序连续拼接。
    arena: String,
}

struct PinyinEntry {
    ch: char,
    reading_end: u32,
}

/// 某字的读音列表视图：按需从 `arena` 切片，取用不分配。
#[derive(Clone, Copy)]
struct PinyinReadings<'a> {
    table: &'a PinyinTable,
    /// `reading_ends` 下标区间 [start, end)
    start: usize,
    end: usize,
}

impl<'a> PinyinReadings<'a> {
    fn len(&self) -> usize {
        self.end - self.start
    }

    /// 最常用读音（首项）。
    fn first(&self) -> Option<&'a str> {
        (self.start < self.end).then(|| self.table.reading_at(self.start))
    }

    /// 按序遍历读音（最常用在前）。
    fn iter(self) -> impl Iterator<Item = &'a str> {
        (self.start..self.end).map(move |i| self.table.reading_at(i))
    }
}

impl PinyinTable {
    /// 从 (字, 读音列表) 行构建；同字多行取靠后者（与旧 HashMap 覆盖语义一致）。
    fn build(mut rows: Vec<(char, Vec<&str>)>) -> Self {
        rows.sort_by_key(|r| r.0); // 稳定排序保文件序，取每组末条即"后行覆盖"
        let mut entries = Vec::with_capacity(rows.len());
        let mut reading_ends = Vec::with_capacity(rows.iter().map(|r| r.1.len()).sum());
        let mut arena =
            String::with_capacity(rows.iter().flat_map(|r| &r.1).map(|s| s.len()).sum());
        let mut i = 0;
        while i < rows.len() {
            let mut j = i;
            while j + 1 < rows.len() && rows[j + 1].0 == rows[i].0 {
                j += 1;
            }
            for r in &rows[j].1 {
                arena.push_str(r);
                reading_ends.push(arena.len() as u32);
            }
            entries.push(PinyinEntry {
                ch: rows[j].0,
                reading_end: reading_ends.len() as u32,
            });
            i = j + 1;
        }
        entries.shrink_to_fit();
        reading_ends.shrink_to_fit();
        arena.shrink_to_fit();
        Self {
            entries,
            reading_ends,
            arena,
        }
    }

    /// 二分查字，返回读音列表视图。
    fn readings(&self, c: char) -> Option<PinyinReadings<'_>> {
        let i = self.entries.binary_search_by_key(&c, |e| e.ch).ok()?;
        let start = if i == 0 {
            0
        } else {
            self.entries[i - 1].reading_end as usize
        };
        Some(PinyinReadings {
            table: self,
            start,
            end: self.entries[i].reading_end as usize,
        })
    }

    /// 按 `reading_ends` 全局下标取单条读音文本。
    fn reading_at(&self, i: usize) -> &str {
        let start = if i == 0 {
            0
        } else {
            self.reading_ends[i - 1] as usize
        };
        &self.arena[start..self.reading_ends[i] as usize]
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ── 内部 Section 结构（对齐 Go tooltip.Section）──────────────────────────────

struct Section {
    label: String,
    lines: Vec<String>,
    /// 强制多行展开格式（即使只有 1 行内容）
    always_expand: bool,
}

/// 格式化 sections → 最终文本（对齐 Go `FormatContent`）。
/// 单行 section：`标签: 内容`；多行或 always_expand：`[标签]` + 逐行。
fn format_sections(sections: Vec<Section>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for sec in sections {
        if sec.lines.is_empty() {
            continue;
        }
        if sec.lines.len() == 1 && !sec.always_expand {
            let line = sec.lines.into_iter().next().unwrap();
            if sec.label.is_empty() {
                parts.push(line);
            } else {
                parts.push(format!("{}: {}", sec.label, line));
            }
        } else {
            if !sec.label.is_empty() {
                parts.push(format!("[{}]", sec.label));
            }
            parts.extend(sec.lines);
        }
    }
    parts.join("\n")
}

/// 当 sections 中同时包含"拆字"和"拼音"时，按字合并为"拆字 / 拼音" section。
/// 合并行格式：`<拆字行>\t<拼音读音>`（渲染层可按 \t 做列对齐）。
/// 对齐 Go `tooltip.MergeChaiziPinyin`。
fn merge_chaizi_pinyin(sections: Vec<Section>) -> Vec<Section> {
    let ci = sections.iter().position(|s| s.label == "拆字");
    let pi = sections.iter().position(|s| s.label == "拼音");
    let (Some(ci), Some(pi)) = (ci, pi) else {
        return sections;
    };

    // 建拼音 map：rune → 读音（剥离"字："前缀，避免合并行重复出现汉字）
    const FULL_COLON: char = '：';
    let flen = FULL_COLON.len_utf8();
    let mut pin_map: HashMap<char, String> = HashMap::new();
    let mut pin_full: HashMap<char, String> = HashMap::new();
    let mut pin_order: Vec<char> = Vec::new();
    for line in &sections[pi].lines {
        if let Some(head) = line.chars().next() {
            pin_full.insert(head, line.clone());
            let reading = line
                .find(FULL_COLON)
                .map(|i| line[i + flen..].to_string())
                .unwrap_or_else(|| line.clone());
            pin_map.insert(head, reading);
            pin_order.push(head);
        }
    }

    // 合并拆字行 + 拼音读音（\t 分隔）
    let mut used: HashSet<char> = HashSet::new();
    let mut merged: Vec<String> = Vec::new();
    for cz in &sections[ci].lines {
        match cz.chars().next() {
            Some(h) if pin_map.contains_key(&h) => {
                used.insert(h);
                merged.push(format!("{}\t{}", cz, pin_map[&h]));
            }
            _ => merged.push(cz.clone()),
        }
    }
    // 拼音独有字（拆字库未收录）补在末尾，保留"字：读音"完整格式
    for &r in &pin_order {
        if !used.contains(&r) {
            merged.push(pin_full[&r].clone());
        }
    }

    let combined = Section {
        label: "拆字 / 拼音".to_string(),
        lines: merged,
        always_expand: true,
    };
    // 用合并 section 替换拆字位置，删除拼音 section
    let mut combined_opt = Some(combined);
    let mut out: Vec<Section> = Vec::with_capacity(sections.len() - 1);
    for (i, s) in sections.into_iter().enumerate() {
        if i == pi {
            // skip 拼音 section
        } else if i == ci {
            out.push(combined_opt.take().unwrap());
        } else {
            out.push(s);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────

impl ReverseLookup {
    /// 加载反查表：两份资源的路径**都**由调用方解析后传入（无则跳过）——拆字库来自方案
    /// `[engine.chaizi].db_path`，拼音读音表 `pinyin_map.txt` 来自数据根。
    ///
    /// 之所以不在这里拼 `data_dir/pinyin_map.txt`：那样就绕过了「用户目录同名文件优先」
    /// 的解析，用户放的覆盖版永远不生效。本 crate 不依赖 wind-config，解析职责一律上提。
    pub fn load(pinyin_map: Option<&Path>, chaizi_path: Option<&Path>) -> Self {
        let mut rl = Self::default();
        if let Some(p) = chaizi_path {
            rl.load_chaizi(p);
        }
        if let Some(p) = pinyin_map {
            rl.load_pinyin(p);
        }
        rl
    }

    pub fn is_empty(&self) -> bool {
        self.chaizi.is_empty() && self.pinyin.is_empty()
    }

    /// 重载拆字表（主码表方案变更时热切换）；`path=None` 清空并释放内存。
    pub fn reload_chaizi(&mut self, path: Option<&Path>) {
        self.chaizi = ChaiziTable::default();
        if let Some(p) = path {
            self.load_chaizi(p);
        }
    }

    /// 载入拆字库（字\t字根\t编码）；存编码列 + 字根列。
    fn load_chaizi(&mut self, path: &Path) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("读取拆字库失败 {}: {}", path.display(), e);
                return;
            }
        };
        let mut rows: Vec<(char, &str, &str)> = Vec::new();
        for line in content.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut it = line.split('\t');
            let (Some(ch), radicals, code) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let mut chars = ch.chars();
            if let (Some(c), None) = (chars.next(), chars.next()) {
                let rad = radicals.map(str::trim).unwrap_or("");
                let code = code.map(str::trim).unwrap_or("");
                if !rad.is_empty() || !code.is_empty() {
                    rows.push((c, rad, code));
                }
            }
        }
        self.chaizi = ChaiziTable::build(rows);
        tracing::info!(
            "拆字库加载完成 {}: {} 字",
            path.display(),
            self.chaizi.len()
        );
    }

    /// 载入拼音表（pinyin-data 格式：`U+4E00: yī  # 一`，多音字逗号分隔）。
    fn load_pinyin(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
        let mut rows: Vec<(char, Vec<&str>)> = Vec::new();
        for line in content.lines() {
            let mut line = line.trim();
            if !line.starts_with("U+") {
                continue;
            }
            // 去掉行内 `# 汉字` 注释
            if let Some(idx) = line.find('#') {
                line = line[..idx].trim();
            }
            let Some((hexpart, rest)) = line.split_once(':') else {
                continue;
            };
            let hex = hexpart.trim_start_matches("U+").trim();
            let Ok(cp) = u32::from_str_radix(hex, 16) else {
                continue;
            };
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            // 逗号分隔多音字读音，首项为最常用读音
            let readings: Vec<&str> = rest
                .trim()
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if !readings.is_empty() {
                rows.push((c, readings));
            }
        }
        self.pinyin = PinyinTable::build(rows);
    }

    /// 生成词的拼音编码（空格分隔、去声调小写；ü→v）。无读音的字跳过。
    /// 用于设置页 dict.genPinyin / 拼音方案加词自动出码。
    pub fn gen_pinyin(&self, text: &str) -> String {
        text.chars()
            .filter_map(|c| self.pinyin.readings(c).and_then(|r| r.first()))
            .map(strip_tone)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 五笔词组取码（86 版首码法）：1字=全码；2字=各取前2码；3字=前2字各首码+末字前2码；
    /// ≥4字=前3字首码+末字首码。
    ///
    /// # ⚠ 已无生产消费点，勿在新代码中使用
    ///
    /// 造词/加词的取码已统一到 `wind_engine::EngineManager::encode_word`
    /// （码源=码表词库自身的单字全码，规则=方案 `[[encoder.rules]]` 声明的公式）。
    /// 本函数保留仅作五笔 86 规则的参考实现，有三个不可用于生产的缺陷：
    ///
    /// 1. **码源是拆字表**，与实际词库解耦 —— 用户换词库/加扩展库后可能算出**打不出来**的码；
    ///    且拆字表是可选资源（全仓 5 个方案只有 `wubi86` 配了），未配的方案取码恒空。
    /// 2. **规则硬编码为五笔 86**，不读方案声明，换任何非五笔码表方案静默出错。
    /// 3. **缺码静默跳过**（`firstn` 返回空串）：「你X好」中 X 无码时会算成「你好」的码，
    ///    产出一个错码却照常入库。
    ///
    /// 见 `docs/design/codetable-auto-phrase.md` §2「码源统一」。
    pub fn wubi_word_code(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let firstn = |c: char, n: usize| -> String {
            self.chaizi
                .code(c)
                .map(|s| s.chars().take(n).collect())
                .unwrap_or_default()
        };
        match chars.len() {
            0 => String::new(),
            1 => self
                .chaizi
                .code(chars[0])
                .map(str::to_string)
                .unwrap_or_default(),
            2 => format!("{}{}", firstn(chars[0], 2), firstn(chars[1], 2)),
            3 => format!(
                "{}{}{}",
                firstn(chars[0], 1),
                firstn(chars[1], 1),
                firstn(chars[2], 2)
            ),
            _ => {
                let last = *chars.last().unwrap();
                format!(
                    "{}{}{}{}",
                    firstn(chars[0], 1),
                    firstn(chars[1], 1),
                    firstn(chars[2], 1),
                    firstn(last, 1)
                )
            }
        }
    }

    /// 为候选文本生成反查提示，按 `opts` 门控各 provider，分段输出（对齐 Go tooltip）。
    ///
    /// 格式规则（对齐 Go `FormatContent`）：
    /// - 单行 section → `标签: 内容`（行内标签，无 []）
    /// - 多行或 always_expand → `[标签]` 标题行 + 逐行内容
    ///
    /// 拆字+拼音同时开启时融合为 `[拆字 / 拼音]`，每行用 `\t` 分隔两列。
    /// 编码段（`word_code`）由调用方按方案词库反查后传入：码表方案=自身全部编码
    /// （码长升序 `/` 连接，如 `a/ab/abc`）、拼音/混输=主码表编码；词不在词库时传
    /// None 不显示——本层不按取码规则生成，生成码常与词库实际码不一致（会提示出打不出的码）。
    /// `code_source`：编码来源方案名——候选并非用该编码方案直接输入时（拼音/临时拼音/
    /// 混输反查主码表）传入，标题显示为 `[编码(五笔)]`；None 显示 `[编码]`。
    pub fn tooltip_for(
        &self,
        text: &str,
        opts: &TooltipOptions,
        word_code: Option<&str>,
        code_source: Option<&str>,
    ) -> String {
        if self.is_empty() && word_code.is_none() {
            return String::new();
        }
        let chars: Vec<char> = text.chars().filter(|c| (*c as u32) >= 0x3400).collect();
        if chars.is_empty() {
            return String::new();
        }
        let mut sections: Vec<Section> = Vec::new();

        // 编码段（整词维度，不逐字拆）：置于最前——编码是核心「如何输入」信息。
        // 以 `[编码]` 标题格式独立成段（always_expand）。只要开启编码就显示整词码，
        // 与拆字互不影响（拆字段另按字给出「字根 [逐字编码]」，二者粒度不同、可并存）。
        if opts.code
            && let Some(code) = word_code.filter(|c| !c.is_empty())
        {
            let label = match code_source.filter(|s| !s.is_empty()) {
                Some(src) => format!("编码({src})"),
                None => "编码".to_string(),
            };
            sections.push(Section {
                label,
                lines: vec![code.to_string()],
                always_expand: true, // 强制 [编码] 标题行
            });
        }

        // 拼音段（逐字，always_expand）
        if opts.pinyin {
            let mut lines = Vec::new();
            for &c in &chars {
                if let Some(readings) = self.pinyin.readings(c) {
                    let n = if !opts.heteronyms {
                        1
                    } else if opts.max_readings > 0 {
                        opts.max_readings.min(readings.len())
                    } else {
                        readings.len()
                    };
                    let shown = readings.iter().take(n).collect::<Vec<_>>().join("/");
                    if !shown.is_empty() {
                        lines.push(format!("{c}：{shown}"));
                    }
                }
            }
            if !lines.is_empty() {
                sections.push(Section {
                    label: "拼音".into(),
                    lines,
                    always_expand: true,
                });
            }
        }

        // 拆字段（字根 [编码]，always_expand；对齐 Go ChaiziProvider）
        if opts.chaizi {
            let mut lines = Vec::new();
            for &c in &chars {
                if let Some(rad) = self.chaizi.radicals(c) {
                    let line = match self.chaizi.code(c) {
                        Some(code) => format!("{c}：{rad} [{code}]"),
                        None => format!("{c}：{rad}"),
                    };
                    lines.push(line);
                }
            }
            if !lines.is_empty() {
                sections.push(Section {
                    label: "拆字".into(),
                    lines,
                    always_expand: true,
                });
            }
        }

        // 拆字+拼音融合（对齐 Go MergeChaiziPinyin）
        let sections = merge_chaizi_pinyin(sections);

        format_sections(sections)
    }
}

/// 去声调：带调号韵母 → 基本字母（ü→v，符合拼音输入习惯）。
fn strip_tone(py: &str) -> String {
    py.chars()
        .map(|c| match c {
            'ā' | 'á' | 'ǎ' | 'à' => 'a',
            'ō' | 'ó' | 'ǒ' | 'ò' => 'o',
            'ē' | 'é' | 'ě' | 'è' => 'e',
            'ī' | 'í' | 'ǐ' | 'ì' => 'i',
            'ū' | 'ú' | 'ǔ' | 'ù' => 'u',
            'ǖ' | 'ǘ' | 'ǚ' | 'ǜ' | 'ü' => 'v',
            other => other,
        })
        .collect::<String>()
        .to_lowercase()
}

#[cfg(test)]
impl ReverseLookup {
    /// 测试助手：直接构建拆字表（字, 字根, 编码）。
    fn set_chaizi(&mut self, rows: Vec<(char, &str, &str)>) {
        self.chaizi = ChaiziTable::build(rows);
    }

    /// 测试助手：直接构建拼音表（字, 读音列表）。
    fn set_pinyin(&mut self, rows: Vec<(char, Vec<&str>)>) {
        self.pinyin = PinyinTable::build(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_tone() {
        assert_eq!(strip_tone("nǐ"), "ni");
        assert_eq!(strip_tone("hǎo"), "hao");
        assert_eq!(strip_tone("lǜ"), "lv");
    }

    #[test]
    fn test_wubi_word_code_rules() {
        let mut rl = ReverseLookup::default();
        rl.set_chaizi(vec![
            ('工', "", "aaaa"),
            ('人', "", "wwww"),
            ('大', "", "dddd"),
            ('小', "", "ihty"),
        ]);
        // 1字=全码
        assert_eq!(rl.wubi_word_code("工"), "aaaa");
        // 2字=各前2码
        assert_eq!(rl.wubi_word_code("工人"), "aaww");
        // 3字=前2字首码+末字前2码
        assert_eq!(rl.wubi_word_code("工人大"), "awdd");
        // ≥4字=前3字首码+末字首码
        assert_eq!(rl.wubi_word_code("工人大小"), "awdi");
    }

    fn sample_rl() -> ReverseLookup {
        let mut rl = ReverseLookup::default();
        rl.set_chaizi(vec![('好', "女子", "vbg"), ('人', "人", "w")]);
        rl.set_pinyin(vec![('好', vec!["hǎo", "hào"]), ('人', vec!["rén"])]);
        rl
    }

    #[test]
    fn test_chaizi_table_lookup_and_dup_last_wins() {
        // 乱序输入 + 同字重复：查询按二分命中，重复取文件靠后者（对齐旧 HashMap 覆盖语义）。
        let t = ChaiziTable::build(vec![
            ('乙', "乙", "nnll"),
            ('甲', "田", "old"),
            ('甲', "田", "lhnh"),
            ('丙', "", "gmw"),
        ]);
        assert_eq!(t.len(), 3, "重复字应合并");
        assert_eq!(t.lookup('甲'), Some(("田", "lhnh")), "同字取靠后行");
        assert_eq!(t.code('乙'), Some("nnll"));
        assert_eq!(t.radicals('丙'), None, "空字根应视为无");
        assert_eq!(t.code('丙'), Some("gmw"));
        assert_eq!(t.lookup('丁'), None);
    }

    #[test]
    fn test_reload_chaizi_none_clears() {
        let mut rl = sample_rl();
        assert!(!rl.chaizi.is_empty());
        rl.reload_chaizi(None);
        assert!(rl.chaizi.is_empty(), "None 应清空拆字表");
        assert!(!rl.pinyin.is_empty(), "拼音表不受影响");
        assert_eq!(rl.wubi_word_code("好"), "");
    }

    #[test]
    fn test_tooltip_default_pinyin_and_code() {
        let rl = sample_rl();
        // 编码由调用方按方案词库反查传入（词级）
        let t = rl.tooltip_for("好人", &TooltipOptions::default(), Some("vbww"), None);
        // [拼音] 标题行
        assert!(t.contains("[拼音]"), "应有 [拼音] 标题: {t}");
        assert!(t.contains("好：hǎo/hào"), "默认 heteronyms 显示全读音: {t}");
        // 编码为调用方传入的词库实际码，以 [编码] 标题格式独立成段
        assert!(
            t.contains("[编码]") && t.contains("vbww"),
            "整词编码带标题: {t}"
        );
        assert!(!t.contains("拆字"), "默认不含拆字: {t}");
        // 纯 ASCII 无反查（即使传了编码）
        assert_eq!(
            rl.tooltip_for("abc", &TooltipOptions::default(), Some("x"), None),
            ""
        );
    }

    #[test]
    fn test_tooltip_single_char_code() {
        let rl = sample_rl();
        let t = rl.tooltip_for("好", &TooltipOptions::default(), Some("vbg"), None);
        // 单字编码=词库实际全码，以 [编码] 标题格式独立成段
        assert!(
            t.contains("[编码]") && t.contains("vbg"),
            "单字编码带标题: {t}"
        );
        // 调用方未传编码（词不在方案词库）→ 无编码段，不臆测生成
        let t2 = rl.tooltip_for("好", &TooltipOptions::default(), None, None);
        assert!(!t2.contains("[编码]"), "无词库码不显示编码段: {t2}");
    }

    #[test]
    fn test_tooltip_provider_gating() {
        let rl = sample_rl();
        // 仅拼音：code 开关关闭时传入的编码也不显示
        let opts = TooltipOptions {
            code: false,
            pinyin: true,
            heteronyms: true,
            max_readings: 0,
            chaizi: false,
        };
        let t = rl.tooltip_for("好", &opts, Some("vbg"), None);
        assert!(
            t.contains("拼音") && !t.contains("编码") && !t.contains("拆字"),
            "{t}"
        );
    }

    #[test]
    fn test_tooltip_code_only_with_empty_tables() {
        // 反查表全空（无拆字库/拼音表）但调用方传入词库码 → 仍显示编码段
        let rl = ReverseLookup::default();
        let t = rl.tooltip_for("好", &TooltipOptions::default(), Some("vbg"), None);
        assert_eq!(t, "[编码]\nvbg", "空表仅编码段: {t}");
    }

    #[test]
    fn test_tooltip_code_source_label() {
        // 编码来源方案名标注：拼音/临时拼音下编码来自主码表 → 标题带方案名
        let rl = ReverseLookup::default();
        let t = rl.tooltip_for("好", &TooltipOptions::default(), Some("vbg"), Some("五笔"));
        assert_eq!(t, "[编码(五笔)]\nvbg", "标题应带来源方案名: {t}");
        // 多码按长度排列原样显示
        let t2 = rl.tooltip_for("好", &TooltipOptions::default(), Some("v/vb/vbg"), None);
        assert_eq!(t2, "[编码]\nv/vb/vbg", "多码列表原样显示: {t2}");
        // 空来源名等同无标注
        let t3 = rl.tooltip_for("好", &TooltipOptions::default(), Some("vbg"), Some(""));
        assert_eq!(t3, "[编码]\nvbg", "空来源名不加括注: {t3}");
    }

    #[test]
    fn test_tooltip_heteronyms_and_max_readings() {
        let rl = sample_rl();
        // heteronyms=false → 仅首音
        let opts = TooltipOptions {
            heteronyms: false,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts, None, None);
        assert!(t.contains("好：hǎo") && !t.contains("hào"), "仅首音: {t}");
        // max_readings=1 → 截断到 1
        let opts2 = TooltipOptions {
            max_readings: 1,
            ..Default::default()
        };
        let t2 = rl.tooltip_for("好", &opts2, None, None);
        assert!(t2.contains("好：hǎo") && !t2.contains("hào"), "截断: {t2}");
    }

    #[test]
    fn test_tooltip_chaizi() {
        let rl = sample_rl();
        let opts = TooltipOptions {
            code: false,
            pinyin: false,
            chaizi: true,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts, None, None);
        // 多行 always_expand → [拆字] 标题 + 内容行
        assert_eq!(t, "[拆字]\n好：女子 [vbg]", "拆字段含字根+编码: {t}");
    }

    #[test]
    fn test_tooltip_code_and_chaizi_coexist() {
        let rl = sample_rl();
        // 编码 + 拆字同开：整词 [编码] 段显示（置顶），拆字行另含逐字编码，二者并存。
        let opts = TooltipOptions {
            code: true,
            pinyin: false,
            chaizi: true,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts, Some("vbg"), None);
        assert!(t.contains("[编码]"), "整词编码段应显示: {t}");
        assert!(t.contains("[拆字]"), "拆字标题: {t}");
        assert!(t.contains("好：女子 [vbg]"), "逐字编码内嵌于拆字行: {t}");
        // [编码] 段置于拆字之前（编码是核心「如何输入」信息）
        assert!(
            t.find("[编码]") < t.find("[拆字]"),
            "编码段应在拆字段之前: {t}"
        );
    }

    #[test]
    fn test_tooltip_merge_chaizi_pinyin() {
        let rl = sample_rl();
        let opts = TooltipOptions {
            code: false,
            pinyin: true,
            chaizi: true,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts, None, None);
        // 拆字+拼音融合为 [拆字 / 拼音]
        assert!(t.contains("[拆字 / 拼音]"), "融合标题: {t}");
        // 拆字行 + \t + 拼音读音（剥离"字："前缀）
        assert!(
            t.contains("好：女子 [vbg]\thǎo/hào"),
            "融合行含拆字+拼音: {t}"
        );
        // 不应有独立的拼音或拆字标题
        assert!(!t.contains("[拼音]"), "无独立拼音段: {t}");
        assert!(!t.contains("[拆字]"), "无独立拆字段: {t}");
    }

    #[test]
    fn test_gen_pinyin_uses_first_reading() {
        let mut rl = ReverseLookup::default();
        // 多音字"重"：首音 zhòng（最常用），次音 chóng
        rl.set_pinyin(vec![('重', vec!["zhòng", "chóng"]), ('要', vec!["yào"])]);
        assert_eq!(rl.gen_pinyin("重要"), "zhong yao");
    }

    #[test]
    fn test_tooltip_multi_reading_joined() {
        let mut rl = ReverseLookup::default();
        rl.set_pinyin(vec![('重', vec!["zhòng", "chóng"])]);
        let t = rl.tooltip_for("重", &TooltipOptions::default(), None, None);
        assert!(t.contains("zhòng/chóng"), "多音字读音应以 / 连接: {t}");
    }

    #[test]
    fn test_load_pinyin_parses_multi_reading() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("wind-reverse-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pinyin_map.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "# 头部注释").unwrap();
        writeln!(f, "U+4E00: yī  # 一").unwrap();
        writeln!(f, "U+91CD: zhòng,chóng  # 重").unwrap();
        drop(f);

        let mut rl = ReverseLookup::default();
        rl.load_pinyin(&path);
        assert_eq!(
            rl.pinyin.readings('一').unwrap().iter().collect::<Vec<_>>(),
            vec!["yī"]
        );
        assert_eq!(
            rl.pinyin.readings('重').unwrap().iter().collect::<Vec<_>>(),
            vec!["zhòng", "chóng"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_pinyin_table_lookup_and_dup_last_wins() {
        // 与拆字表对称：乱序输入 + 同字重复，查询按二分命中，重复取靠后者。
        let t = PinyinTable::build(vec![
            ('乙', vec!["yǐ"]),
            ('甲', vec!["jiǎ"]),
            ('甲', vec!["jiá", "jiǎ"]),
            ('丙', vec!["bǐng"]),
        ]);
        assert_eq!(t.entries.len(), 3, "重复字应合并");
        assert_eq!(
            t.readings('甲').unwrap().iter().collect::<Vec<_>>(),
            vec!["jiá", "jiǎ"],
            "同字取靠后行"
        );
        // 相邻条目的读音区间不串味（甲有 2 个读音，其后的乙仍应只取自己那条）
        assert_eq!(t.readings('乙').unwrap().first(), Some("yǐ"));
        assert_eq!(t.readings('乙').unwrap().len(), 1);
        assert_eq!(t.readings('丙').unwrap().first(), Some("bǐng"));
        assert!(t.readings('丁').is_none());
    }
}
