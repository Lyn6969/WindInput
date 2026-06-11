//! 词法/语法分析器
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/parser/` 对齐。

pub mod lexer;
pub mod parser;

pub use parser::parse;
