//! 标点转换器
//!
//! 与 Go 版本 `wind_input/internal/transform/punctuation.go` 对齐。
//! 英文标点 → 中文标点；引号根据左右状态切换。

/// 标点转换器（持有引号左右状态）
pub struct PunctuationConverter {
    single_quote_left: bool,
    double_quote_left: bool,
}

impl Default for PunctuationConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl PunctuationConverter {
    pub fn new() -> Self {
        Self {
            single_quote_left: true,
            double_quote_left: true,
        }
    }

    /// 重置引号状态（模式切换/清空时调用）
    pub fn reset(&mut self) {
        self.single_quote_left = true;
        self.double_quote_left = true;
    }

    /// 英文标点 → 中文标点；返回 None 表示该字符无中文标点映射。
    /// 结果可能是多字符（如 `^`→`……`），故返回 String。
    pub fn to_chinese(&mut self, c: char) -> Option<String> {
        // 引号需切换左右
        match c {
            '\'' => {
                let r = if self.single_quote_left { '\u{2018}' } else { '\u{2019}' };
                self.single_quote_left = !self.single_quote_left;
                return Some(r.to_string());
            }
            '"' => {
                let r = if self.double_quote_left { '\u{201C}' } else { '\u{201D}' };
                self.double_quote_left = !self.double_quote_left;
                return Some(r.to_string());
            }
            '^' => return Some("\u{2026}\u{2026}".to_string()), // ……
            '_' => return Some("\u{2014}\u{2014}".to_string()), // ——
            _ => {}
        }
        let mapped = match c {
            ',' => '\u{FF0C}',  // ，
            '.' => '\u{3002}',  // 。
            '?' => '\u{FF1F}',  // ？
            '!' => '\u{FF01}',  // ！
            ':' => '\u{FF1A}',  // ：
            ';' => '\u{FF1B}',  // ；
            '(' => '\u{FF08}',  // （
            ')' => '\u{FF09}',  // ）
            '[' => '\u{3010}',  // 【
            ']' => '\u{3011}',  // 】
            '{' => '\u{FF5B}',  // ｛
            '}' => '\u{FF5D}',  // ｝
            '<' => '\u{300A}',  // 《
            '>' => '\u{300B}',  // 》
            '~' => '\u{FF5E}',  // ～
            '$' => '\u{FFE5}',  // ￥
            '`' => '\u{00B7}',  // ·
            '\\' => '\u{3001}', // 、
            _ => return None,
        };
        Some(mapped.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_punct() {
        let mut p = PunctuationConverter::new();
        assert_eq!(p.to_chinese(',').as_deref(), Some("，"));
        assert_eq!(p.to_chinese('.').as_deref(), Some("。"));
        assert_eq!(p.to_chinese('\\').as_deref(), Some("、"));
        assert_eq!(p.to_chinese('^').as_deref(), Some("……"));
        assert_eq!(p.to_chinese('a'), None);
    }

    #[test]
    fn test_quote_toggle() {
        let mut p = PunctuationConverter::new();
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // 左
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201D}")); // 右
        assert_eq!(p.to_chinese('\'').as_deref(), Some("\u{2018}"));
        p.reset();
        assert_eq!(p.to_chinese('"').as_deref(), Some("\u{201C}")); // reset 后回到左
    }
}
