//! 音节解析器
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/parser.go` 对齐。
//!
//! 注：Rust 侧的音节切分主体由 [`super::dag::Dag`] + [`super::syllable::SyllableTrie`]
//! 承担（见 `mod.rs` 的 `convert` / `compute_composition`）。手动音节分隔符 `'`
//! 作为**硬边界**的处理也在 `mod.rs`：按 `'` 分段、各段独立 DAG 最大匹配，`'` 不入 trie
//! 故任何音节都不会跨越它（`segment_with_separators` / `compose_segment`）。本文件仅保留
//! 解析结果的数据结构定义。

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
