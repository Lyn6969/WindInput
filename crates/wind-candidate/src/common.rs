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

    /// 单字是否常用。对齐 Go `IsCommonChar`。
    pub fn is_char_common(&self, ch: char) -> bool {
        self.set.contains(&ch)
    }

    /// 字符串是否常用：其中所有 CJK 汉字都在表内（非 CJK 字符忽略）。
    /// 空串视为非常用。对齐 Go `IsStringCommon`。
    pub fn is_string_common(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        for ch in text.chars() {
            if is_cjk(ch) && !self.set.contains(&ch) {
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
}
