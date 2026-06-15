//! 语法分析器
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/parser/parser.go` 对齐。

use crate::ast::Phrase;

/// 解析源文本为 AST
pub fn parse(src: &str) -> anyhow::Result<Phrase> {
    // TODO: 实现递归下降解析器
    Ok(Phrase::Literal(src.to_string()))
}
