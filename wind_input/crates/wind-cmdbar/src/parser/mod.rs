//! 词法/语法分析器
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/parser/` 对齐。

pub mod lexer;
#[allow(clippy::module_inception)] // 与 Go parser/parser.go 同布局
pub mod parser;

pub use lexer::{Lexer, RawStringPart, Token, TokenKind};
pub use parser::parse;
