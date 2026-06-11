//! 词法分析器
//!
//! 与 Go 版本 `wind_input/internal/cmdbar/parser/lexer.go` 对齐。

/// Token 类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    String,
    Number,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    Eof,
}

/// Token
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub pos: usize,
}
