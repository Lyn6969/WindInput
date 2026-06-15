//! 音节解析器
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/parser.go` 对齐。

/// 解析后的音节
#[derive(Debug, Clone)]
pub struct ParsedSyllable {
    pub text: String,
    pub start: usize,
    pub end: usize,
    pub is_exact: bool,
    pub possible: Vec<String>,
}

/// 解析结果
#[derive(Debug, Clone)]
pub struct ParseResult {
    pub syllables: Vec<ParsedSyllable>,
    pub remainder: String,
}
