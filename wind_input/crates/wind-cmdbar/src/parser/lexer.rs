//! 词法分析器
//!
//! 对照 Go `wind_input/internal/cmdbar/parser/lexer.go`。扫描表达式级子集：
//! 字符串连同两侧引号一起扫描，产出的 [`TokenKind::String`] token 携带已解码的
//! `parts`（字面段 + `{expr}` 插值的原始子串，交给 parser 层再解析）。
//!
//! 关键不变量（与 Go 同）：ident 起始用 **ASCII 字节**判定，避免 UTF-8 多字节首字节
//! 被误判为字母而 scanIdent 不前进造成死循环。

use crate::error::{CmdbarError, Result};

/// 词法 token 类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Eof,
    Ident,
    Number,
    /// 字符串字面量；解码后的片段在 `Token::parts`。
    String,
    LParen,
    RParen,
    Comma,
    Dot,
    /// `{` —— 表达式位开启 options-bag ObjectLit。
    LBrace,
    /// `}` —— 关闭 options-bag ObjectLit。
    RBrace,
    /// `:` —— ObjectLit 内 key 与 value 的分隔。
    Colon,
}

/// 字符串字面量的一个原始片段：字面文本或待再解析的插值原文。
#[derive(Debug, Clone, PartialEq)]
pub enum RawStringPart {
    /// 已解码的字面文本。
    Lit { text: String, offset: usize },
    /// `{...}` 之间的原始源（含义由 parser 层再 lex+parse 成表达式）。
    Interp { raw: String, offset: usize },
}

/// 词法 token。
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub lexeme: String,
    /// 仅当 `kind == Number` 有效。
    pub number: f64,
    /// 仅当 `kind == String` 有效。
    pub parts: Vec<RawStringPart>,
    /// token 在源中的起始字节偏移。
    pub offset: usize,
}

impl Token {
    fn simple(kind: TokenKind, lexeme: &str, offset: usize) -> Self {
        Token {
            kind,
            lexeme: lexeme.to_string(),
            number: 0.0,
            parts: Vec::new(),
            offset,
        }
    }
}

/// 词法分析器。一次 [`Lexer::tokenize`] 消费整段输入。
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    /// 消费整段输入，返回 token 序列（末尾恒附 `Eof`）。
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                b'(' => {
                    tokens.push(Token::simple(TokenKind::LParen, "(", self.pos));
                    self.pos += 1;
                }
                b')' => {
                    tokens.push(Token::simple(TokenKind::RParen, ")", self.pos));
                    self.pos += 1;
                }
                b',' => {
                    tokens.push(Token::simple(TokenKind::Comma, ",", self.pos));
                    self.pos += 1;
                }
                b'.' => {
                    tokens.push(Token::simple(TokenKind::Dot, ".", self.pos));
                    self.pos += 1;
                }
                b'{' => {
                    // 表达式位的 '{' 开启 options-bag。字符串内的 `{...}` 由 scan_string
                    // 当作插值处理，永不走到这里。
                    tokens.push(Token::simple(TokenKind::LBrace, "{", self.pos));
                    self.pos += 1;
                }
                b'}' => {
                    tokens.push(Token::simple(TokenKind::RBrace, "}", self.pos));
                    self.pos += 1;
                }
                b':' => {
                    tokens.push(Token::simple(TokenKind::Colon, ":", self.pos));
                    self.pos += 1;
                }
                b'"' | b'\'' => {
                    let tok = self.scan_string(c)?;
                    tokens.push(tok);
                }
                b'-' | b'0'..=b'9' => {
                    let tok = self.scan_number()?;
                    tokens.push(tok);
                }
                _ if is_ascii_ident_start(c) => {
                    let tok = self.scan_ident();
                    tokens.push(tok);
                }
                _ => {
                    let ch = self.src[self.pos..].chars().next().unwrap_or('\u{fffd}');
                    return Err(CmdbarError::parse(
                        self.pos,
                        format!("unexpected character {ch:?}"),
                    ));
                }
            }
        }
        tokens.push(Token::simple(TokenKind::Eof, "", self.pos));
        Ok(tokens)
    }

    /// 扫描标识符。起始已由 ASCII 判定保证；续接允许字母/数字/下划线（含 unicode）。
    fn scan_ident(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let ch = self.src[self.pos..].chars().next().unwrap();
            if !is_ident_cont(ch) {
                break;
            }
            self.pos += ch.len_utf8();
        }
        // 防御：理论上不触达（dispatch 已用 ASCII 判定收紧）。
        if self.pos == start && start < self.bytes.len() {
            self.pos += 1;
        }
        Token::simple(TokenKind::Ident, &self.src[start..self.pos], start)
    }

    /// 扫描数字：`-?digits(.digits)?`。裸 `-` 不成数字 → 报错。
    fn scan_number(&mut self) -> Result<Token> {
        let start = self.pos;
        let mut p = self.pos;
        if p < self.bytes.len() && self.bytes[p] == b'-' {
            if p + 1 >= self.bytes.len() || !self.bytes[p + 1].is_ascii_digit() {
                return Err(CmdbarError::parse(self.pos, "unexpected '-'"));
            }
            p += 1;
        }
        while p < self.bytes.len() && self.bytes[p].is_ascii_digit() {
            p += 1;
        }
        if p < self.bytes.len() && self.bytes[p] == b'.' {
            p += 1;
            let d_start = p;
            while p < self.bytes.len() && self.bytes[p].is_ascii_digit() {
                p += 1;
            }
            if p == d_start {
                return Err(CmdbarError::parse(start, "invalid number"));
            }
        }
        let lex = &self.src[start..p];
        if lex.is_empty() || lex == "-" {
            return Err(CmdbarError::parse(start, "invalid number"));
        }
        let f: f64 = lex
            .parse()
            .map_err(|_| CmdbarError::parse(start, format!("invalid number {lex:?}")))?;
        self.pos = p;
        Ok(Token {
            kind: TokenKind::Number,
            lexeme: lex.to_string(),
            number: f,
            parts: Vec::new(),
            offset: start,
        })
    }

    /// 扫描以 `quote` 起止的字符串。内部转义 `\" \\ \{ \} \( \) \n \t \r`，
    /// `{...}` 捕获为插值（嵌套大括号计数，且尊重内层字符串边界）。
    fn scan_string(&mut self, quote: u8) -> Result<Token> {
        let start = self.pos;
        self.pos += 1; // 消费开引号
        let mut parts: Vec<RawStringPart> = Vec::new();
        let mut lit = String::new();
        let mut lit_off = self.pos;

        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c == quote {
                flush_lit(&mut parts, &mut lit, lit_off);
                self.pos += 1; // 消费闭引号
                return Ok(Token {
                    kind: TokenKind::String,
                    lexeme: self.src[start..self.pos].to_string(),
                    number: 0.0,
                    parts,
                    offset: start,
                });
            }
            if c == b'\\' && self.pos + 1 < self.bytes.len() {
                let next = self.bytes[self.pos + 1];
                decode_escape_byte(next, &mut lit);
                self.pos += 2;
                continue;
            }
            if c == b'{' {
                flush_lit(&mut parts, &mut lit, lit_off);
                let brace_off = self.pos;
                let interp_start = self.pos + 1;
                let end = self.scan_interp_body(interp_start)?;
                parts.push(RawStringPart::Interp {
                    raw: self.src[interp_start..end].to_string(),
                    offset: brace_off,
                });
                self.pos = end + 1; // 跳过 '}'
                lit_off = self.pos;
                continue;
            }
            if c == b'}' {
                return Err(CmdbarError::parse(self.pos, "unmatched '}' in string"));
            }
            // 拷贝一个完整 UTF-8 字符
            let ch = self.src[self.pos..].chars().next().unwrap();
            lit.push(ch);
            self.pos += ch.len_utf8();
        }
        Err(CmdbarError::parse(start, "unclosed string"))
    }

    /// 从 `start`（`{` 之后）扫描到匹配的 `}`，返回 `}` 的字节位置。
    /// 嵌套大括号计数，内层字符串里的大括号被忽略。
    fn scan_interp_body(&self, start: usize) -> Result<usize> {
        let mut depth = 1;
        let mut p = start;
        let mut inner = 0u8; // 当前内层字符串引号，0 表示不在字符串内
        while p < self.bytes.len() && depth > 0 {
            let ch = self.bytes[p];
            if inner != 0 {
                if ch == b'\\' && p + 1 < self.bytes.len() {
                    p += 2;
                    continue;
                }
                if ch == inner {
                    inner = 0;
                }
                p += 1;
                continue;
            }
            if ch == b'"' || ch == b'\'' {
                inner = ch;
                p += 1;
                continue;
            }
            if ch == b'{' {
                depth += 1;
            } else if ch == b'}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            p += 1;
        }
        if depth != 0 {
            return Err(CmdbarError::parse(start - 1, "unclosed '{' in string"));
        }
        Ok(p)
    }
}

fn flush_lit(parts: &mut Vec<RawStringPart>, lit: &mut String, offset: usize) {
    if !lit.is_empty() {
        parts.push(RawStringPart::Lit {
            text: std::mem::take(lit),
            offset,
        });
    }
}

/// 解码字符串内一个转义字符（白名单；未知保留 `\X`，与 Go 宽松策略一致）。
/// 公开给模板/字面量路径复用（统一转义来源）。
pub fn decode_escape_byte(next: u8, out: &mut String) {
    match next {
        b'\\' | b'"' | b'\'' | b'{' | b'}' | b'(' | b')' => out.push(next as char),
        b'n' => out.push('\n'),
        b't' => out.push('\t'),
        b'r' => out.push('\r'),
        _ => {
            out.push('\\');
            out.push(next as char);
        }
    }
}

/// 仅判定 ASCII 字节是否为 ident 起始（dispatch 阶段必须字节级判定）。
fn is_ascii_ident_start(c: u8) -> bool {
    c == b'_' || c.is_ascii_alphabetic()
}

/// ident 续接：下划线 / 字母 / 数字（含 unicode，对齐 Go isIdentCont）。
fn is_ident_cont(r: char) -> bool {
    r == '_' || r.is_alphabetic() || r.is_numeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src)
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn lex_call() {
        use TokenKind::*;
        assert_eq!(kinds("len(code)"), vec![Ident, LParen, Ident, RParen, Eof]);
    }

    #[test]
    fn lex_namespaced_call_with_string() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"clip.copy("hi")"#),
            vec![Ident, Dot, Ident, LParen, String, RParen, Eof]
        );
    }

    #[test]
    fn lex_number_negative_and_float() {
        let toks = Lexer::new("-3, 2.5").tokenize().unwrap();
        assert_eq!(toks[0].kind, TokenKind::Number);
        assert_eq!(toks[0].number, -3.0);
        assert_eq!(toks[2].number, 2.5);
    }

    #[test]
    fn lex_bare_minus_errors() {
        assert!(Lexer::new("- 3").tokenize().is_err());
    }

    #[test]
    fn lex_object_bag() {
        use TokenKind::*;
        assert_eq!(
            kinds("{prefix: true}"),
            vec![LBrace, Ident, Colon, Ident, RBrace, Eof]
        );
    }

    #[test]
    fn string_parts_literal_and_interp() {
        let toks = Lexer::new(r#""a{len(x)}b""#).tokenize().unwrap();
        assert_eq!(toks[0].kind, TokenKind::String);
        let parts = &toks[0].parts;
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], RawStringPart::Lit { text, .. } if text == "a"));
        assert!(matches!(&parts[1], RawStringPart::Interp { raw, .. } if raw == "len(x)"));
        assert!(matches!(&parts[2], RawStringPart::Lit { text, .. } if text == "b"));
    }

    #[test]
    fn string_escapes() {
        let toks = Lexer::new(r#""a\nb\{c\}\\""#).tokenize().unwrap();
        match &toks[0].parts[0] {
            RawStringPart::Lit { text, .. } => assert_eq!(text, "a\nb{c}\\"),
            _ => panic!("expected lit"),
        }
    }

    #[test]
    fn interp_with_nested_braces_and_inner_string() {
        // 内层字符串里的 `}` 不应提前闭合插值
        let toks = Lexer::new(r#""{default(x, "}")}""#).tokenize().unwrap();
        let parts = &toks[0].parts;
        assert_eq!(parts.len(), 1);
        assert!(
            matches!(&parts[0], RawStringPart::Interp { raw, .. } if raw == r#"default(x, "}")"#)
        );
    }

    #[test]
    fn unknown_escape_preserved() {
        let toks = Lexer::new(r#""a\xb""#).tokenize().unwrap();
        match &toks[0].parts[0] {
            RawStringPart::Lit { text, .. } => assert_eq!(text, "a\\xb"),
            _ => panic!(),
        }
    }

    #[test]
    fn no_infinite_loop_on_unicode() {
        // 全角空格起始：不是 ASCII ident 起始 → 立即报错而非死循环
        assert!(Lexer::new("　").tokenize().is_err());
    }
}
