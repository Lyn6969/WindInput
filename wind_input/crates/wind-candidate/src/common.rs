//! 通用规范汉字表（常用字判定）
//!
//! 与 Go 版本 `wind_input/internal/dict/common_chars.go` 对齐。
//! 用于"检索范围"过滤：判定候选是否为常用字/词。

use std::collections::HashSet;
use std::path::Path;

/// 通用规范汉字表（8105 字：一级 3500 + 二级 3000 + 三级 1605）。
pub struct CommonChars {
    set: HashSet<char>,
}

impl CommonChars {
    /// 从文件加载（一字一行，`#` 注释行跳过；仅收录汉字，见 [`is_han`]）。
    /// 失败（文件缺失）返回空集；上层应在空集时退化为"不过滤"。
    pub fn load(path: &Path) -> Self {
        let mut set = HashSet::new();
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                for ch in line.chars() {
                    if is_han(ch) {
                        set.insert(ch);
                    }
                }
            }
        }
        Self { set }
    }

    /// 是否未加载到任何字（数据缺失）。
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// 单字是否常用。PUA 等不在表内的字符一律非常用（直接查表即可，无忽略逻辑）。
    pub fn is_char_common(&self, ch: char) -> bool {
        self.set.contains(&ch)
    }

    /// 字符串是否常用：其中所有「汉字」都在表内，非汉字辅助字符（标点/字母/数字/
    /// emoji/符号）忽略。空串视为非常用。
    ///
    /// 「汉字」的作用域 = [`is_han`] ∪ [`is_pua`]，两侧各自解决一类误判：
    /// - **纳入 PUA**：本码表把私用区码位**当汉字使用**（如 `dwi` 下 U+E831 冒充生僻字、
    ///   占着汉字编码排在「仄」旁边），不查表就会让无字形的垃圾候选混进「常用字/智能」档；
    /// - **`is_han` 排除 CJK 标点/符号**：`、。《》` 等虽紧邻汉字块，却与 `，`、emoji 同属
    ///   辅助符号，规范汉字表对其无从判断，查表必然失败 → 含中文顿号的词条被静默滤掉。
    ///
    /// 两者是同一个判据的两端：**「码表拿它当汉字用」才查表，「它只是符号」就忽略**，
    /// 与 Unicode 块的相邻关系无关。
    pub fn is_string_common(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        for ch in text.chars() {
            // 汉字（含被本码表当汉字用的 PUA）必须在表内；其余辅助字符忽略。
            if (is_han(ch) || is_pua(ch)) && !self.set.contains(&ch) {
                return false;
            }
        }
        true
    }
}

/// 是否「须按通用规范汉字表判定常用性」的汉字。
///
/// 判定域是**真汉字块**，外加无独立输入语义的类汉字符号（部首、笔画）——后者与 PUA 同理，
/// 在码表里占着汉字编码出现，不在规范字表内即应判非常用。
///
/// **刻意排除**（虽紧邻汉字块但属辅助符号，规范汉字表对其无从判断）：
/// - `U+3000..=U+303F` CJK 符号和标点：`、。《》〈〉「」〇` 等；
/// - `U+3040..=U+30FF` 假名、`U+3100..=U+318F` 注音/谚文、`U+3190..=U+319F` 汉文标注；
/// - `U+3200..=U+33FF` 带圈与兼容符号：`① ㈱ ℃ ㎡` 等。
///
/// 旧实现按整段 `0x2E80..=0x33FF` 圈定（名为 `is_cjk`，对齐 Go `isCJKChar`），把上述符号
/// 当成「必须查表的汉字」，而字表里只有 8105 个纯汉字 → 中文顿号一律判非常用，用户词库中
/// 含 `、` 的词条在「常用字/智能」档被静默滤掉。**指纹是判定不自洽**：同为中文标点，
/// `、`(U+3001) 判非常用、`，`(U+FF0C) 却判常用，差别只在落没落进那段区间。
fn is_han(ch: char) -> bool {
    let c = ch as u32;
    (0x2E80..=0x2EFF).contains(&c)        // CJK 部首补充
        || (0x2F00..=0x2FDF).contains(&c) // 康熙部首
        || (0x31C0..=0x31EF).contains(&c) // CJK 笔画
        || (0x3400..=0x4DBF).contains(&c) // 扩展 A
        || (0x4E00..=0x9FFF).contains(&c) // 基本汉字
        || (0xF900..=0xFAFF).contains(&c) // 兼容汉字
        || (0x20000..=0x323AF).contains(&c) // 扩展 B-H
}

/// 候选文本的**语义单元数**：汉字逐字计，连续的西文/数字段整体计 1。
///
/// ```text
/// 「东西」    → 2      hello        → 1
/// 「的样子」  → 3      thank you    → 2
/// 「iPhone」  → 1      「新iPhone」 → 2
/// ```
///
/// 用途：词频位置提升的准入判据（`schema.*.frequency.promote_prefix = "single"`）。
///
/// **为什么不能用 `chars().count()`**：英文候选 `hello` 有 5 个 char，按字符数判据会被
/// 「只提升单字」的规则直接挡死——而英文**所有候选都是前缀匹配**（打 `hel` 出 `hello`），
/// 那等于英文调频全灭。语义单元数让同一条规则在中英文下都成立：一个汉字、一个西文词，
/// 都是用户心智里的「一个东西」。
///
/// PUA 按汉字计——本码表把私用区码位当生僻字使用，见 [`is_pua`]。
pub fn semantic_units(text: &str) -> usize {
    let mut units = 0usize;
    let mut in_latin_word = false;
    for ch in text.chars() {
        if is_han(ch) || is_pua(ch) {
            units += 1;
            in_latin_word = false;
        } else if ch.is_whitespace() {
            in_latin_word = false;
        } else if !in_latin_word {
            units += 1;
            in_latin_word = true;
        }
    }
    units
}

/// 是否 Unicode 私用区（PUA）。本码表把 PUA 码位当汉字使用（占汉字编码、冒充生僻字），
/// 故常用性判定须把 PUA 视作「必须查表的汉字」，不在规范字表内即判非常用。
fn is_pua(ch: char) -> bool {
    let c = ch as u32;
    (0xE000..=0xF8FF).contains(&c)          // BMP 私用区
        || (0xF0000..=0xFFFFD).contains(&c) // 补充私用区 A
        || (0x100000..=0x10FFFD).contains(&c) // 补充私用区 B
}

#[cfg(test)]
mod semantic_units_tests {
    use super::semantic_units;

    /// 中英混排的计数口径——词频 `promote_prefix = "single"` 全靠它区分「单字/单词」与「词组」。
    #[test]
    fn counts_han_per_char_and_latin_per_word() {
        // 中文逐字
        assert_eq!(semantic_units("东"), 1);
        assert_eq!(semantic_units("东西"), 2);
        assert_eq!(semantic_units("的样子"), 3);
        // 西文整词计 1 —— 这是英文调频能工作的前提
        assert_eq!(semantic_units("hello"), 1);
        assert_eq!(semantic_units("a"), 1);
        assert_eq!(semantic_units("thank you"), 2);
        assert_eq!(semantic_units("e-mail"), 1, "连字符不断词");
        // 混排
        assert_eq!(semantic_units("新iPhone"), 2);
        assert_eq!(semantic_units("iPhone手机"), 3);
        // 边界
        assert_eq!(semantic_units(""), 0);
        assert_eq!(semantic_units("   "), 0);
        // 数字按一段计
        assert_eq!(semantic_units("2026"), 1);
    }

    /// 扩展区汉字与 PUA 均按汉字逐字计——码表把私用区码位当生僻字使用。
    #[test]
    fn counts_ext_han_and_pua_as_chars() {
        assert_eq!(semantic_units("\u{20000}"), 1, "扩展 B");
        assert_eq!(semantic_units("\u{E000}\u{E001}"), 2, "PUA 逐字计");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_common() {
        let mut set = HashSet::new();
        set.insert('我');
        set.insert('们');
        let cc = CommonChars { set };
        assert!(cc.is_string_common("我们")); // 全部常用
        assert!(!cc.is_string_common("我鬱")); // 含生僻
        assert!(!cc.is_string_common("")); // 空串
        assert!(cc.is_string_common("我!")); // 非汉字忽略
    }

    #[test]
    fn test_cjk_punct_ignored() {
        // 回归：用户词库里含中文顿号的词条在「常用字/智能」档被滤掉。
        // 根因＝判定域按 `0x2E80..=0x33FF` 整段圈定，把 CJK 符号和标点区当成必须查表的汉字，
        // 而字表里只有纯汉字。判据现按语义而非 Unicode 块邻接：符号一律忽略。
        let mut set = HashSet::new();
        set.insert('我');
        set.insert('们');
        let cc = CommonChars { set };

        assert!(cc.is_string_common("、")); // 顿号单条词条（本次上报的现象）
        assert!(cc.is_string_common("我、们")); // 混排：标点不再拖累整词判定
        for s in ["。", "《", "》", "「", "」", "〇", "；", "："] {
            assert!(cc.is_string_common(s), "CJK 标点应忽略: {s}");
        }
        // 判定自洽：同为中文标点，落不落进旧区间都该同一结果（旧实现下 、=false 而 ，=true）
        assert_eq!(cc.is_string_common("、"), cc.is_string_common("，"));
        // 带圈/兼容符号与假名同属辅助字符，规范汉字表管不着
        assert!(cc.is_string_common("①"));
        assert!(cc.is_string_common("℃"));
        assert!(cc.is_string_common("あ"));
        // 真汉字仍按表判定，未被本次放宽波及
        assert!(!cc.is_string_common("我鬱"));
        assert!(!cc.is_string_common("、鬱")); // 标点忽略，但同串里的生僻字照旧拦下
    }

    #[test]
    fn test_pua_not_common() {
        // 回归：五笔 dwi 下 U+E831（PUA）冒充生僻字混进常用字档。PUA 被本码表当汉字用，
        // 不在规范字表内即非常用；emoji/符号等真辅助字符仍忽略。
        let mut set = HashSet::new();
        set.insert('仄');
        let cc = CommonChars { set };
        assert!(cc.is_string_common("仄")); // 真汉字在表内
        assert!(!cc.is_string_common("\u{E831}")); // PUA 单字：非常用（正是 dwi 豆腐候选）
        assert!(!cc.is_string_common("仄\u{E831}")); // 含 PUA 的混合串亦非常用
        assert!(cc.is_string_common("仄😀")); // emoji（U+1F600）非汉字：忽略，不影响判定
    }
}
