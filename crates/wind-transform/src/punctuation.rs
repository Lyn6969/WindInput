//! 标点转换器
//!
//! 与 Go 版本 `wind_input/internal/transform/punctuation.go` 对齐。

use std::collections::HashMap;

/// 标点转换器
pub struct PunctuationConverter {
    single_quote_left: bool,
    double_quote_left: bool,
    custom_mappings: HashMap<char, Vec<String>>,
}

impl PunctuationConverter {
    pub fn new() -> Self {
        Self {
            single_quote_left: true,
            double_quote_left: true,
            custom_mappings: HashMap::new(),
        }
    }

    /// 英文标点转中文标点（单字符）
    pub fn to_chinese_punct(&mut self, c: char) -> Option<char> {
        match c {
            ',' => Some('，'),
            '.' => Some('。'),
            '?' => Some('？'),
            '!' => Some('！'),
            '(' => Some('（'),
            ')' => Some('）'),
            '[' => Some('【'),
            ']' => Some('】'),
            '<' => Some('《'),
            '>' => Some('》'),
            '\'' => {
                self.single_quote_left = !self.single_quote_left;
                if self.single_quote_left {
                    Some('‘')
                } else {
                    Some('’')
                }
            }
            '"' => {
                self.double_quote_left = !self.double_quote_left;
                if self.double_quote_left {
                    Some('"')
                } else {
                    Some('"')
                }
            }
            _ => None,
        }
    }

    /// 英文标点转中文标点（多字符结果）
    pub fn to_chinese_punct_str(&self, c: char) -> Option<&'static str> {
        match c {
            '^' => Some("……"),
            '_' => Some("------"),
            _ => None,
        }
    }
}
