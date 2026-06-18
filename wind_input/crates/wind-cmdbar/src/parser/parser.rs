//! 语法分析器（递归下降）
//!
//! 对照 Go `wind_input/internal/cmdbar/parser/parser.go`。顶层分派：
//! 1. 源在 depth-0（非字符串内）含 marker（`$CC(` / `$CC1(` / `$SS(`）→ 命令/数组短语；
//! 2. 否则含顶层未转义 `{` → 模板短语（隐式字符串包裹）；
//! 3. 否则 → 字面短语。

use super::lexer::{decode_escape_byte, Lexer, RawStringPart, Token, TokenKind};
use crate::ast::{ArrayPhrase, CommandPhrase, Expr, ModValue, Modifiers, Phrase, StringPart};
use crate::error::{CmdbarError, Result};

/// 解析一条短语源为 [`Phrase`]。
pub fn parse(src: &str) -> Result<Phrase> {
    if let Some((marker, idx, open_off)) = find_top_level_marker(src) {
        // marker 之前只允许空白（短语不在顶层拼接）。
        if !src[..idx].trim().is_empty() {
            return Err(CmdbarError::parse(0, format!("unexpected text before {marker}")));
        }
        return match marker {
            "$CC" | "$CC1" => parse_command_phrase(src, idx, open_off).map(Phrase::Command),
            "$SS" => parse_array_phrase(src, idx, open_off),
            _ => unreachable!(),
        };
    }
    if has_top_level_brace(src) {
        return parse_template_phrase(src);
    }
    Ok(Phrase::Literal(src.to_string()))
}

/// 源是否使用命令栏语法（含顶层 marker 或顶层未转义 `{` 插值）。
/// 宿主据此分流：true 走命令栏求值，false 走旧的简单模板/字面量路径。
pub fn is_cmdbar_grammar(src: &str) -> bool {
    find_top_level_marker(src).is_some() || has_top_level_brace(src)
}

/// 顶层 marker 表（最长前缀优先；`$CC1` 必须排在 `$CC` 前）。
const MARKER_TABLE: &[&str] = &["$CC1", "$CC", "$SS"];

/// 扫描首个不在字符串内的 marker，返回 (marker, '$' 偏移, '(' 偏移)。
fn find_top_level_marker(src: &str) -> Option<(&'static str, usize, usize)> {
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            i += 2;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                    continue;
                }
                if b[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'$' {
            for &m in MARKER_TABLE {
                let end = i + m.len();
                if end < b.len() && b[end] == b'(' && &src[i..end] == m {
                    return Some((m, i, end));
                }
            }
        }
        i += 1;
    }
    None
}

/// 源是否含不在字符串内的未转义 `{`。
fn has_top_level_brace(src: &str) -> bool {
    let b = src.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            i += 2;
            continue;
        }
        if c == b'{' {
            return true;
        }
        i += 1;
    }
    false
}

/// 找到 `open_idx`（`(` 位置）的匹配 `)`，忽略字符串内容。返回 `)` 字节位置。
fn find_matching_paren(src: &str, open_idx: usize) -> Result<usize> {
    let b = src.as_bytes();
    let mut depth = 1;
    let mut i = open_idx + 1;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            i += 2;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 2;
                    continue;
                }
                if b[i] == q {
                    i += 1;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return Ok(i);
            }
        }
        i += 1;
    }
    Err(CmdbarError::parse(open_idx, "unclosed '('"))
}

fn parse_command_phrase(src: &str, idx: usize, open: usize) -> Result<CommandPhrase> {
    let marker = &src[idx..open];
    if open >= src.len() || src.as_bytes()[open] != b'(' {
        return Err(CmdbarError::parse(idx, format!("expected '(' after {marker}")));
    }
    let end = find_matching_paren(src, open)?;
    let inner = &src[open + 1..end];
    if !src[end + 1..].trim().is_empty() {
        return Err(CmdbarError::parse(
            end + 1,
            format!("unexpected text after {marker}(...)"),
        ));
    }
    let toks = Lexer::new(inner).tokenize()?;
    let mut p = Parser::new(toks, open + 1);
    let mut args = p.parse_expr_list()?;
    if p.peek().kind != TokenKind::Eof {
        return Err(p.errf(
            p.peek().offset,
            format!("unexpected token {:?}", p.peek().lexeme),
        ));
    }

    // 末参若为 ObjectLit → 提取为 Modifiers；中段 ObjectLit 报错。
    let mut explicit = Modifiers::new();
    if matches!(args.last(), Some(Expr::Object(_)))
        && let Some(Expr::Object(pairs)) = args.pop()
    {
        explicit = Modifiers(pairs);
    }
    for (i, a) in args.iter().enumerate() {
        if matches!(a, Expr::Object(_)) {
            return Err(CmdbarError::parse(
                open,
                format!(
                    "{marker}: options bag must be the last argument (found at arg {})",
                    i + 1
                ),
            ));
        }
    }
    if args.is_empty() {
        return Err(CmdbarError::parse(
            idx,
            format!("{marker} requires a display expression"),
        ));
    }

    let modifiers = Modifiers::merge(marker_defaults(marker), explicit);
    let display = args.remove(0);
    Ok(CommandPhrase {
        display,
        actions: args,
        modifiers,
    })
}

/// marker 名隐含的默认修饰符（对齐 Go markerDefaults）。
fn marker_defaults(marker: &str) -> Modifiers {
    let mut m = Modifiers::new();
    match marker {
        "$CC1" => m.push("prefix", ModValue::Bool(true)),
        "$SS" => {
            m.push("prefix", ModValue::Bool(true));
            m.push("expand", ModValue::Sym("exact".into()));
            m.push("nav", ModValue::Bool(true));
        }
        _ => {}
    }
    m
}

fn parse_array_phrase(src: &str, idx: usize, open: usize) -> Result<Phrase> {
    let marker = &src[idx..open];
    if open >= src.len() || src.as_bytes()[open] != b'(' {
        return Err(CmdbarError::parse(idx, format!("expected '(' after {marker}")));
    }
    let end = find_matching_paren(src, open)?;
    let inner = &src[open + 1..end];
    if !src[end + 1..].trim().is_empty() {
        return Err(CmdbarError::parse(
            end + 1,
            format!("unexpected text after {marker}(...)"),
        ));
    }
    let spans = split_array_args(inner, open + 1)?;
    let mut parsed: Vec<Expr> = Vec::with_capacity(spans.len());
    for sp in &spans {
        parsed.push(parse_array_element(sp.text, sp.offset)?);
    }

    // 末参 ObjectLit → Modifiers（同 parse_command_phrase）。
    let mut explicit = Modifiers::new();
    if matches!(parsed.last(), Some(Expr::Object(_)))
        && let Some(Expr::Object(pairs)) = parsed.pop()
    {
        explicit = Modifiers(pairs);
    }
    for (i, a) in parsed.iter().enumerate() {
        if matches!(a, Expr::Object(_)) {
            return Err(CmdbarError::parse(
                open,
                format!(
                    "{marker}: options bag must be the last argument (found at arg {})",
                    i + 1
                ),
            ));
        }
    }
    if parsed.is_empty() {
        return Err(CmdbarError::parse(
            idx,
            format!("{marker} requires a group name (first argument)"),
        ));
    }
    let name = match &parsed[0] {
        Expr::StringLit(parts) => string_lit_to_plain(parts)?,
        other => {
            return Err(CmdbarError::parse(
                open,
                format!("{marker}: first argument must be a string literal (group name), got {other:?}"),
            ))
        }
    };

    let elements: Vec<Expr> = parsed.split_off(1);
    for (i, e) in elements.iter().enumerate() {
        match e {
            Expr::StringLit(_) => {}
            Expr::Command(cp) => {
                if cp.modifiers.contains("prefix") {
                    return Err(CmdbarError::parse(
                        open,
                        format!(
                            "{marker} element {}: nested $CC must not set 'prefix' modifier (group prefix is controlled by {marker})",
                            i + 1
                        ),
                    ));
                }
            }
            other => {
                return Err(CmdbarError::parse(
                    open,
                    format!(
                        "{marker} element {}: must be string literal or $CC(...), got {other:?}",
                        i + 1
                    ),
                ))
            }
        }
    }

    let modifiers = Modifiers::merge(marker_defaults(marker), explicit);
    Ok(Phrase::Array(ArrayPhrase {
        name,
        elements,
        modifiers,
    }))
}

/// 一个顶层 `$SS` 参数切片：原始子串 + 在原源中的字节偏移。
struct ArgSpan<'a> {
    text: &'a str,
    offset: usize,
}

/// 按顶层逗号切割 inner，跳过字符串字面量与括号/花括号嵌套。
fn split_array_args(inner: &str, base_off: usize) -> Result<Vec<ArgSpan<'_>>> {
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let b = inner.as_bytes();
    let mut out = Vec::new();
    let mut depth: i32 = 0;
    let mut in_string = 0u8;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if in_string != 0 {
            if c == b'\\' && i + 1 < b.len() {
                i += 2;
                continue;
            }
            if c == in_string {
                in_string = 0;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' => in_string = c,
            b'\\' if i + 1 < b.len() => {
                i += 2;
                continue;
            }
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push(ArgSpan {
                    text: &inner[start..i],
                    offset: base_off + start,
                });
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if in_string != 0 {
        return Err(CmdbarError::parse(base_off, "unclosed string in array args"));
    }
    if depth != 0 {
        return Err(CmdbarError::parse(base_off, "unbalanced brackets in array args"));
    }
    out.push(ArgSpan {
        text: &inner[start..],
        offset: base_off + start,
    });
    Ok(out)
}

/// 解析一个 `$SS` 元素 span：以 `$CC(`/`$CC1(` 开头走命令短语，否则单表达式。
fn parse_array_element(text: &str, offset: usize) -> Result<Expr> {
    let leading = text.len() - text.trim_start().len();
    let rest = &text[leading..];
    if rest.starts_with("$CC1(") || rest.starts_with("$CC(") {
        let (marker, midx, mopen) = find_top_level_marker(text)
            .ok_or_else(|| CmdbarError::parse(offset, "element $CC parse failed"))?;
        if midx != leading || (marker != "$CC" && marker != "$CC1") {
            return Err(CmdbarError::parse(
                offset,
                "element starts with $CC marker but parse failed",
            ));
        }
        let cp = parse_command_phrase(text, midx, mopen)?;
        return Ok(Expr::Command(Box::new(cp)));
    }
    let toks = Lexer::new(text).tokenize()?;
    let mut p = Parser::new(toks, offset);
    let expr = p.parse_expr()?;
    if p.peek().kind != TokenKind::Eof {
        return Err(p.errf(
            p.peek().offset,
            format!("unexpected token {:?} in array element", p.peek().lexeme),
        ));
    }
    Ok(expr)
}

/// 取 StringLit 的纯静态文本，拒绝任何 `{expr}` 插值。
fn string_lit_to_plain(parts: &[StringPart]) -> Result<String> {
    let mut s = String::new();
    for p in parts {
        match p {
            StringPart::Text(t) => s.push_str(t),
            StringPart::Interp(_) => {
                return Err(CmdbarError::parse(0, "interpolation not allowed in group name"))
            }
        }
    }
    Ok(s)
}

fn parse_template_phrase(src: &str) -> Result<Phrase> {
    let parts = scan_template_parts(src)?;
    let expr = build_string_lit(&parts)?;
    Ok(Phrase::Template(expr))
}

/// 把整段 src 当作隐式无引号字符串体扫描成原始片段（字面 + 插值原文）。
/// 与 lexer scan_string 共享转义白名单与大括号匹配语义（对齐 Go parseTemplatePhrase）。
fn scan_template_parts(src: &str) -> Result<Vec<RawStringPart>> {
    let b = src.as_bytes();
    let mut parts: Vec<RawStringPart> = Vec::new();
    let mut lit = String::new();
    let mut lit_off = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && i + 1 < b.len() {
            decode_escape_byte(b[i + 1], &mut lit);
            i += 2;
            continue;
        }
        if c == b'{' {
            if !lit.is_empty() {
                parts.push(RawStringPart::Lit {
                    text: std::mem::take(&mut lit),
                    offset: lit_off,
                });
            }
            let brace_off = i;
            let interp_start = i + 1;
            let end = scan_interp_end(src, interp_start)?;
            parts.push(RawStringPart::Interp {
                raw: src[interp_start..end].to_string(),
                offset: brace_off,
            });
            i = end + 1;
            lit_off = i;
            continue;
        }
        if c == b'}' {
            return Err(CmdbarError::parse(i, "unmatched '}'"));
        }
        let ch = src[i..].chars().next().unwrap();
        lit.push(ch);
        i += ch.len_utf8();
    }
    if !lit.is_empty() {
        parts.push(RawStringPart::Lit {
            text: lit,
            offset: lit_off,
        });
    }
    Ok(parts)
}

/// 从 `start`（`{` 之后）扫描到匹配的 `}`，忽略内层字符串里的大括号。返回 `}` 位置。
fn scan_interp_end(src: &str, start: usize) -> Result<usize> {
    let b = src.as_bytes();
    let mut depth = 1;
    let mut p = start;
    let mut inner = 0u8;
    while p < b.len() && depth > 0 {
        let ch = b[p];
        if inner != 0 {
            if ch == b'\\' && p + 1 < b.len() {
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
        return Err(CmdbarError::parse(start - 1, "unclosed '{'"));
    }
    Ok(p)
}

/// 把原始片段列表构建为 [`Expr::StringLit`]，逐个把插值原文再 lex+parse 成表达式。
fn build_string_lit(parts: &[RawStringPart]) -> Result<Expr> {
    let mut out: Vec<StringPart> = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            RawStringPart::Lit { text, .. } => out.push(StringPart::Text(text.clone())),
            RawStringPart::Interp { raw, offset } => {
                let toks = Lexer::new(raw).tokenize()?;
                let mut pr = Parser::new(toks, offset + 1);
                let expr = pr.parse_expr()?;
                if pr.peek().kind != TokenKind::Eof {
                    return Err(pr.errf(
                        pr.peek().offset,
                        format!("unexpected token {:?} in interpolation", pr.peek().lexeme),
                    ));
                }
                out.push(StringPart::Interp(Box::new(expr)));
            }
        }
    }
    Ok(Expr::StringLit(out))
}

/// token 流上的递归下降解析器。`base_off` 是 token 偏移 0 在原源中的字节位置。
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    base_off: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, base_off: usize) -> Self {
        Parser {
            tokens,
            pos: 0,
            base_off,
        }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        self.pos += 1;
        t
    }

    fn errf(&self, off: usize, msg: impl Into<String>) -> CmdbarError {
        CmdbarError::parse(self.base_off + off, msg.into())
    }

    /// 零或多个逗号分隔的表达式。
    fn parse_expr_list(&mut self) -> Result<Vec<Expr>> {
        if matches!(self.peek().kind, TokenKind::Eof | TokenKind::RParen) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        loop {
            out.push(self.parse_expr()?);
            if self.peek().kind != TokenKind::Comma {
                break;
            }
            self.bump(); // ,
            if matches!(self.peek().kind, TokenKind::Eof | TokenKind::RParen) {
                return Err(self.errf(self.peek().offset, "trailing comma"));
            }
        }
        Ok(out)
    }

    fn parse_expr(&mut self) -> Result<Expr> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::Number => {
                self.bump();
                Ok(Expr::Number {
                    value: t.number,
                    raw: t.lexeme,
                })
            }
            TokenKind::String => {
                self.bump();
                build_string_lit(&t.parts)
            }
            TokenKind::Ident => {
                self.bump();
                let mut name = t.lexeme.clone();
                // 可选 namespace：ident "." ident（至多一个点）。
                if self.peek().kind == TokenKind::Dot {
                    self.bump();
                    if self.peek().kind != TokenKind::Ident {
                        return Err(self.errf(self.peek().offset, "expected identifier after '.'"));
                    }
                    let second = self.bump();
                    name = format!("{name}.{}", second.lexeme);
                    if self.peek().kind == TokenKind::Dot {
                        return Err(
                            self.errf(self.peek().offset, "function name may have at most one '.'")
                        );
                    }
                }
                // 调用形式？
                if self.peek().kind == TokenKind::LParen {
                    self.bump();
                    let args = if self.peek().kind != TokenKind::RParen {
                        self.parse_expr_list()?
                    } else {
                        Vec::new()
                    };
                    if self.peek().kind != TokenKind::RParen {
                        return Err(self.errf(
                            self.peek().offset,
                            format!("expected ')' to close call to {name}"),
                        ));
                    }
                    self.bump();
                    return Ok(Expr::Call { name, args });
                }
                // 裸标识符：带 namespace 的必须用 ()。
                if name.contains('.') {
                    return Err(self.errf(
                        t.offset,
                        format!("namespaced function {name:?} must be called with ()"),
                    ));
                }
                Ok(Expr::Ident(name))
            }
            TokenKind::LParen => {
                self.bump();
                let e = self.parse_expr()?;
                if self.peek().kind != TokenKind::RParen {
                    return Err(self.errf(self.peek().offset, "expected ')'"));
                }
                self.bump();
                Ok(e)
            }
            TokenKind::LBrace => self.parse_object_lit(),
            TokenKind::Eof => Err(self.errf(t.offset, "unexpected end of input")),
            _ => Err(self.errf(t.offset, format!("unexpected token {:?}", t.lexeme))),
        }
    }

    /// 解析 `{key: value, ...}` options bag；value 限字面量。允许尾逗号与空 `{}`。
    fn parse_object_lit(&mut self) -> Result<Expr> {
        let open = self.bump(); // '{'
        let mut pairs: Vec<(String, ModValue)> = Vec::new();
        if self.peek().kind == TokenKind::RBrace {
            self.bump();
            return Ok(Expr::Object(pairs));
        }
        loop {
            if self.peek().kind != TokenKind::Ident {
                return Err(self.errf(
                    self.peek().offset,
                    format!(
                        "expected key identifier in options bag (got {:?})",
                        self.peek().lexeme
                    ),
                ));
            }
            let key = self.bump().lexeme;
            if self.peek().kind != TokenKind::Colon {
                return Err(self.errf(self.peek().offset, format!("expected ':' after key {key:?}")));
            }
            self.bump(); // ':'
            let val = self.parse_modifier_value()?;
            pairs.push((key, val));
            if self.peek().kind != TokenKind::Comma {
                break;
            }
            self.bump(); // ','
            if self.peek().kind == TokenKind::RBrace {
                break;
            }
        }
        if self.peek().kind != TokenKind::RBrace {
            return Err(self.errf(
                self.peek().offset,
                format!(
                    "expected '}}' to close options bag (opened at offset {})",
                    self.base_off + open.offset
                ),
            ));
        }
        self.bump(); // '}'
        Ok(Expr::Object(pairs))
    }

    /// options bag 内 value 限字面量：string（无插值）/ number / ident（true/false/符号）。
    fn parse_modifier_value(&mut self) -> Result<ModValue> {
        let t = self.peek().clone();
        match t.kind {
            TokenKind::String => {
                self.bump();
                // 必须是纯字面（无插值）。
                let expr = build_string_lit(&t.parts)?;
                let Expr::StringLit(parts) = expr else {
                    unreachable!()
                };
                let mut s = String::new();
                for p in parts {
                    match p {
                        StringPart::Text(txt) => s.push_str(&txt),
                        StringPart::Interp(_) => {
                            return Err(
                                self.errf(t.offset, "interpolation not allowed in modifier value")
                            )
                        }
                    }
                }
                Ok(ModValue::Str(s))
            }
            TokenKind::Number => {
                self.bump();
                Ok(ModValue::Num(t.number))
            }
            TokenKind::Ident => {
                self.bump();
                if self.peek().kind == TokenKind::Dot {
                    return Err(
                        self.errf(self.peek().offset, "namespaced ident not allowed as modifier value")
                    );
                }
                if self.peek().kind == TokenKind::LParen {
                    return Err(self.errf(self.peek().offset, "call not allowed as modifier value"));
                }
                Ok(match t.lexeme.as_str() {
                    "true" => ModValue::Bool(true),
                    "false" => ModValue::Bool(false),
                    _ => ModValue::Sym(t.lexeme),
                })
            }
            _ => Err(self.errf(
                t.offset,
                format!(
                    "expected literal value (string/number/true/false/ident) in options bag, got {:?}",
                    t.lexeme
                ),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(src: &str) -> Phrase {
        parse(src).unwrap_or_else(|e| panic!("parse {src:?} failed: {e}"))
    }

    #[test]
    fn literal_phrase() {
        assert_eq!(pp("hello"), Phrase::Literal("hello".into()));
    }

    #[test]
    fn template_phrase_roundtrip() {
        let p = pp("你好 {code} 长度 {len(code)}");
        match &p {
            Phrase::Template(Expr::StringLit(parts)) => {
                assert_eq!(parts.len(), 4);
                assert!(matches!(&parts[0], StringPart::Text(t) if t == "你好 "));
                assert!(matches!(&parts[1], StringPart::Interp(_)));
            }
            _ => panic!("expected template, got {p:?}"),
        }
    }

    #[test]
    fn command_phrase_basic() {
        let p = pp(r#"$CC("《》", type("《》"), key.tap("Left"))"#);
        match p {
            Phrase::Command(c) => {
                assert_eq!(c.actions.len(), 2);
                assert!(c.modifiers.is_empty());
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn cc1_injects_prefix_modifier() {
        let p = pp(r#"$CC1("x", type("x"))"#);
        match p {
            Phrase::Command(c) => assert_eq!(c.modifiers.get_bool("prefix"), Some(true)),
            _ => panic!(),
        }
    }

    #[test]
    fn explicit_options_override_default() {
        let p = pp(r#"$CC1("x", type("x"), {prefix: false, async: true})"#);
        match p {
            Phrase::Command(c) => {
                assert_eq!(c.modifiers.get_bool("prefix"), Some(false));
                assert_eq!(c.modifiers.get_bool("async"), Some(true));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn options_bag_must_be_last() {
        assert!(parse(r#"$CC("x", {a: 1}, type("y"))"#).is_err());
    }

    #[test]
    fn array_phrase_with_embedded_cc() {
        let p = pp(r#"$SS("操作", "纯文本", $CC("动作", open("https://x")))"#);
        match p {
            Phrase::Array(a) => {
                assert_eq!(a.name, "操作");
                assert_eq!(a.elements.len(), 2);
                assert!(matches!(&a.elements[0], Expr::StringLit(_)));
                assert!(matches!(&a.elements[1], Expr::Command(_)));
                // $SS 默认注入 prefix/expand/nav
                assert_eq!(a.modifiers.get_bool("prefix"), Some(true));
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn array_nested_cc_with_prefix_rejected() {
        assert!(parse(r#"$SS("g", $CC1("x", type("x")))"#).is_err());
    }

    #[test]
    fn namespaced_bare_ident_requires_parens() {
        assert!(parse("{clip.copy}").is_err()); // template 内裸 namespaced
    }

    #[test]
    fn text_before_marker_rejected() {
        assert!(parse(r#"abc$CC("x")"#).is_err());
    }

    #[test]
    fn display_roundtrips_via_display_impl() {
        let p = pp(r#"$CC("hi", open("u"))"#);
        let s = format!("{p}");
        let reparsed = pp(&s);
        assert_eq!(p, reparsed);
    }
}
