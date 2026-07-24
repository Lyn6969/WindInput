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
    /// 从文件加载（一字一行，`#` 注释行跳过；仅收录 CJK 字符）。
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
                    if is_cjk(ch) {
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
    /// 「汉字」的作用域 = `is_cjk` ∪ `is_pua`：本码表把私用区（PUA）码位**当汉字使用**
    /// （如 `dwi` 下 U+E831 冒充生僻字、占着汉字编码排在「仄」旁边），故 PUA 必须纳入
    /// 常用性判定——否则「只对 is_cjk 查表」会把 PUA 当可忽略字符放行，无字形的垃圾
    /// 候选便混进「常用字/智能」档。反之 emoji/符号/英文有独立库与编码、语义明确，不归
    /// 本过滤管辖，保持忽略。
    pub fn is_string_common(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        for ch in text.chars() {
            // 汉字（含被本码表当汉字用的 PUA）必须在表内；其余辅助字符忽略。
            if (is_cjk(ch) || is_pua(ch)) && !self.set.contains(&ch) {
                return false;
            }
        }
        true
    }
}

/// 是否 CJK 汉字（对齐 Go `isCJKChar` 的码点范围）。
fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    (0x2E80..=0x33FF).contains(&c)       // 部首/笔画/符号区
        || (0x3400..=0x9FFF).contains(&c) // 扩展A + 基本汉字
        || (0xF900..=0xFAFF).contains(&c) // 兼容汉字
        || (0x20000..=0x323AF).contains(&c) // 扩展 B-H
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
        assert!(cc.is_string_common("我!")); // 非 CJK 忽略
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
