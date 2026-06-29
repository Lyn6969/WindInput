//! 候选反查（编码反查 / 拆字 / 拼音）
//!
//! 与 Go 版本 `wind_input/internal/tooltip/` 对齐（简化版）。
//! 为悬停候选提供"如何输入"的提示：五笔编码（拆字）+ 拼音读音。
//!
//! 数据源：
//! - 拆字/五笔码：`schemas/wubi86/wubi86_chaizi.txt`（字\t字根\t五笔编码）
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
    /// 字 → 五笔编码
    code: HashMap<char, String>,
    /// 字 → 字根分解（拆字段用）
    radicals: HashMap<char, String>,
    /// 字 → 拼音读音（多音字按常用频率排序，最常用在前）
    pinyin: HashMap<char, Vec<String>>,
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
    pub fn load(data_dir: Option<&Path>) -> Self {
        let mut rl = Self::default();
        if let Some(dir) = data_dir {
            rl.load_chaizi(&dir.join("schemas/wubi86/wubi86_chaizi.txt"));
            rl.load_pinyin(&dir.join("pinyin_map.txt"));
        }
        rl
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty() && self.pinyin.is_empty()
    }

    /// 载入五笔拆字库（字\t字根\t编码）；存编码列 + 字根列。
    fn load_chaizi(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
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
                if let Some(code) = code {
                    let code = code.trim();
                    if !code.is_empty() {
                        self.code.insert(c, code.to_string());
                    }
                }
                if let Some(radicals) = radicals {
                    let radicals = radicals.trim();
                    if !radicals.is_empty() {
                        self.radicals.insert(c, radicals.to_string());
                    }
                }
            }
        }
    }

    /// 载入拼音表（pinyin-data 格式：`U+4E00: yī  # 一`，多音字逗号分隔）。
    fn load_pinyin(&mut self, path: &Path) {
        let Ok(content) = std::fs::read_to_string(path) else {
            return;
        };
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
            let readings: Vec<String> = rest
                .trim()
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            if !readings.is_empty() {
                self.pinyin.insert(c, readings);
            }
        }
    }

    /// 生成词的拼音编码（空格分隔、去声调小写；ü→v）。无读音的字跳过。
    /// 用于设置页 dict.genPinyin / 拼音方案加词自动出码。
    pub fn gen_pinyin(&self, text: &str) -> String {
        text.chars()
            .filter_map(|c| self.pinyin.get(&c).and_then(|r| r.first()))
            .map(|py| strip_tone(py))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// 五笔词组取码（86 版首码法）：1字=全码；2字=各取前2码；3字=前2字各首码+末字前2码；
    /// ≥4字=前3字首码+末字首码。用于码表方案加词自动出码。无码的字按空串跳过。
    pub fn wubi_word_code(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let firstn = |c: char, n: usize| -> String {
            self.code
                .get(&c)
                .map(|s| s.chars().take(n).collect())
                .unwrap_or_default()
        };
        match chars.len() {
            0 => String::new(),
            1 => self.code.get(&chars[0]).cloned().unwrap_or_default(),
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
    /// 编码段：仅当拆字关闭时显示，以整词维度（不逐字拆）单行输出。
    pub fn tooltip_for(&self, text: &str, opts: &TooltipOptions) -> String {
        if self.is_empty() {
            return String::new();
        }
        let chars: Vec<char> = text.chars().filter(|c| (*c as u32) >= 0x3400).collect();
        if chars.is_empty() {
            return String::new();
        }
        let mut sections: Vec<Section> = Vec::new();

        // 拼音段（逐字，always_expand）
        if opts.pinyin {
            let mut lines = Vec::new();
            for &c in &chars {
                if let Some(readings) = self.pinyin.get(&c) {
                    let n = if !opts.heteronyms {
                        1
                    } else if opts.max_readings > 0 {
                        opts.max_readings.min(readings.len())
                    } else {
                        readings.len()
                    };
                    let shown = readings[..n.min(readings.len())].join("/");
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
                if let Some(rad) = self.radicals.get(&c) {
                    let line = match self.code.get(&c) {
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

        // 编码段：整词维度，不逐字拆（对齐用户需求）。
        // 拆字开启时编码已内嵌于拆字行（字：字根 [编码]），不再单独显示。
        if opts.code && !opts.chaizi {
            let filtered: String = chars.iter().collect();
            let code = self.wubi_word_code(&filtered);
            if !code.is_empty() {
                sections.push(Section {
                    label: "编码".into(),
                    lines: vec![code],
                    always_expand: false, // 单行 → "编码: xxxx"
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
        rl.code.insert('工', "aaaa".into());
        rl.code.insert('人', "wwww".into());
        rl.code.insert('大', "dddd".into());
        rl.code.insert('小', "ihty".into());
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
        rl.code.insert('好', "vbg".to_string());
        rl.pinyin
            .insert('好', vec!["hǎo".to_string(), "hào".to_string()]);
        rl.radicals.insert('好', "女子".to_string());
        rl.code.insert('人', "w".to_string());
        rl.pinyin.insert('人', vec!["rén".to_string()]);
        rl.radicals.insert('人', "人".to_string());
        rl
    }

    #[test]
    fn test_tooltip_default_pinyin_and_code() {
        let rl = sample_rl();
        let t = rl.tooltip_for("好人", &TooltipOptions::default());
        // [拼音] 标题行
        assert!(t.contains("[拼音]"), "应有 [拼音] 标题: {t}");
        assert!(t.contains("好：hǎo/hào"), "默认 heteronyms 显示全读音: {t}");
        // 编码为整词维度（2字取各前2码：vb+w=vbw）
        assert!(t.contains("编码: vbw"), "整词编码单行: {t}");
        assert!(!t.contains("拆字"), "默认不含拆字: {t}");
        // 纯 ASCII 无反查
        assert_eq!(rl.tooltip_for("abc", &TooltipOptions::default()), "");
    }

    #[test]
    fn test_tooltip_single_char_code() {
        let rl = sample_rl();
        let t = rl.tooltip_for("好", &TooltipOptions::default());
        // 单字编码=全码
        assert!(t.contains("编码: vbg"), "单字编码: {t}");
    }

    #[test]
    fn test_tooltip_provider_gating() {
        let rl = sample_rl();
        // 仅拼音
        let opts = TooltipOptions {
            code: false,
            pinyin: true,
            heteronyms: true,
            max_readings: 0,
            chaizi: false,
        };
        let t = rl.tooltip_for("好", &opts);
        assert!(
            t.contains("拼音") && !t.contains("编码") && !t.contains("拆字"),
            "{t}"
        );
    }

    #[test]
    fn test_tooltip_heteronyms_and_max_readings() {
        let rl = sample_rl();
        // heteronyms=false → 仅首音
        let opts = TooltipOptions {
            heteronyms: false,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts);
        assert!(t.contains("好：hǎo") && !t.contains("hào"), "仅首音: {t}");
        // max_readings=1 → 截断到 1
        let opts2 = TooltipOptions {
            max_readings: 1,
            ..Default::default()
        };
        let t2 = rl.tooltip_for("好", &opts2);
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
        let t = rl.tooltip_for("好", &opts);
        // 多行 always_expand → [拆字] 标题 + 内容行
        assert_eq!(t, "[拆字]\n好：女子 [vbg]", "拆字段含字根+编码: {t}");
    }

    #[test]
    fn test_tooltip_chaizi_embeds_code() {
        let rl = sample_rl();
        // 拆字开启时，编码段不单独显示（已内嵌于拆字行）
        let opts = TooltipOptions {
            code: true,
            pinyin: false,
            chaizi: true,
            ..Default::default()
        };
        let t = rl.tooltip_for("好", &opts);
        assert!(t.contains("[拆字]"), "拆字标题: {t}");
        assert!(t.contains("好：女子 [vbg]"), "编码内嵌于拆字行: {t}");
        // 不应有独立的"编码: "行
        assert!(!t.contains("编码: "), "拆字开时无独立编码段: {t}");
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
        let t = rl.tooltip_for("好", &opts);
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
        rl.pinyin
            .insert('重', vec!["zhòng".to_string(), "chóng".to_string()]);
        rl.pinyin.insert('要', vec!["yào".to_string()]);
        assert_eq!(rl.gen_pinyin("重要"), "zhong yao");
    }

    #[test]
    fn test_tooltip_multi_reading_joined() {
        let mut rl = ReverseLookup::default();
        rl.pinyin
            .insert('重', vec!["zhòng".to_string(), "chóng".to_string()]);
        let t = rl.tooltip_for("重", &TooltipOptions::default());
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
        assert_eq!(rl.pinyin.get(&'一').unwrap(), &vec!["yī".to_string()]);
        assert_eq!(
            rl.pinyin.get(&'重').unwrap(),
            &vec!["zhòng".to_string(), "chóng".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
