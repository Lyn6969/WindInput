//! §3.3 计算函数（对照 Go funcs/calc.go）：`calc`（算术表达式）+ `num`（进制转换）。
//! 全部 `pure=true` 且 `deterministic=true`。

use super::func_specs;
use super::util::parse_arg_int;
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "calc": Calc (1, 1) det => fn_calc, "数学表达式求值 (+ - * / % 与括号; 空输入静默返回空)", "calc(tail(code, 2))";
        "num" : Calc (2, 2) det => fn_num,  "进制转换 (2/8/10/16); num('0xff', 10) → '255'", "num(\"0xff\", 10)";
    }
}

fn fn_calc(_: &dyn EvalContext, args: &[String]) -> Result<String> {
    // 空输入静默返回，不构成错误（模板里编码尚空时不刷错误候选）。
    if args[0].trim().is_empty() {
        return Ok(String::new());
    }
    let v = eval_arith(&args[0]).map_err(|e| CmdbarError::runtime("calc", e))?;
    Ok(format_number(v))
}

fn fn_num(_: &dyn EvalContext, args: &[String]) -> Result<String> {
    let s = args[0].trim();
    let base = parse_arg_int("num", &args[1])?;
    if !matches!(base, 2 | 8 | 10 | 16) {
        return Err(CmdbarError::runtime(
            "num",
            format!("unsupported base {base}"),
        ));
    }
    let v = parse_int_auto(s)
        .ok_or_else(|| CmdbarError::runtime("num", format!("invalid number {s:?}")))?;
    Ok(to_base(v, base as u32))
}

/// 自动识别 0x/0o/0b 前缀（也支持负号）解析整数。
fn parse_int_auto(s: &str) -> Option<i64> {
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let v = if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()?
    } else if let Some(o) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
        i64::from_str_radix(o, 8).ok()?
    } else if let Some(b) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        i64::from_str_radix(b, 2).ok()?
    } else {
        body.parse::<i64>().ok()?
    };
    Some(if neg { -v } else { v })
}

/// 整数转指定进制（无前缀）。
fn to_base(mut v: i64, base: u32) -> String {
    match base {
        10 => return v.to_string(),
        2 => return format!("{v:b}"),
        8 => return format!("{v:o}"),
        16 => return format!("{v:x}"),
        _ => {}
    }
    // 理论上不触达（base 已校验）。
    if v == 0 {
        return "0".into();
    }
    let neg = v < 0;
    if neg {
        v = -v;
    }
    let digits = b"0123456789abcdef";
    let mut buf = Vec::new();
    while v > 0 {
        buf.push(digits[(v % base as i64) as usize]);
        v /= base as i64;
    }
    if neg {
        buf.push(b'-');
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// 整值无多余 `.0`，否则最短浮点。
fn format_number(f: f64) -> String {
    if f.is_nan() || f.is_infinite() {
        return format!("{f}");
    }
    if f == f.trunc() && f.abs() < 1e16 {
        return format!("{}", f as i64);
    }
    format!("{f}")
}

// ───────────────────────── 算术表达式递归下降 ─────────────────────────
//
//   expr   = term (("+" | "-") term)*
//   term   = unary (("*" | "/" | "%") unary)*
//   unary  = ("+" | "-")? primary
//   primary= NUMBER | "(" expr ")"

fn eval_arith(src: &str) -> std::result::Result<f64, String> {
    let mut p = ArithParser {
        bytes: src.as_bytes(),
        pos: 0,
        tok: ArithTok::End,
        num: 0.0,
        lex: String::new(),
    };
    p.next();
    let v = p.parse_expr()?;
    if p.tok != ArithTok::End {
        return Err(format!("unexpected character {:?}", p.lex));
    }
    Ok(v)
}

#[derive(PartialEq, Clone, Copy)]
enum ArithTok {
    End,
    Num,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    LParen,
    RParen,
}

struct ArithParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    tok: ArithTok,
    num: f64,
    lex: String,
}

impl ArithParser<'_> {
    fn next(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
        if self.pos >= self.bytes.len() {
            self.tok = ArithTok::End;
            self.lex.clear();
            return;
        }
        let c = self.bytes[self.pos];
        let single = |t: ArithTok, ch: char| (t, ch);
        let (tok, lexch) = match c {
            b'+' => single(ArithTok::Plus, '+'),
            b'-' => single(ArithTok::Minus, '-'),
            b'*' => single(ArithTok::Star, '*'),
            b'/' => single(ArithTok::Slash, '/'),
            b'%' => single(ArithTok::Percent, '%'),
            b'(' => single(ArithTok::LParen, '('),
            b')' => single(ArithTok::RParen, ')'),
            _ => {
                if c.is_ascii_digit() || c == b'.' {
                    let start = self.pos;
                    while self.pos < self.bytes.len()
                        && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'.')
                    {
                        self.pos += 1;
                    }
                    let lex = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
                    match lex.parse::<f64>() {
                        Ok(f) => {
                            self.tok = ArithTok::Num;
                            self.num = f;
                            self.lex = lex.to_string();
                        }
                        Err(_) => {
                            self.tok = ArithTok::End;
                            self.lex = lex.to_string();
                        }
                    }
                    return;
                }
                // 未知字符：留给 parser 报错。
                self.tok = ArithTok::End;
                self.lex = (c as char).to_string();
                self.pos += 1;
                return;
            }
        };
        self.tok = tok;
        self.lex = lexch.to_string();
        self.pos += 1;
    }

    fn parse_expr(&mut self) -> std::result::Result<f64, String> {
        let mut lhs = self.parse_term()?;
        while self.tok == ArithTok::Plus || self.tok == ArithTok::Minus {
            let op = self.tok;
            self.next();
            let rhs = self.parse_term()?;
            lhs = if op == ArithTok::Plus {
                lhs + rhs
            } else {
                lhs - rhs
            };
        }
        Ok(lhs)
    }

    fn parse_term(&mut self) -> std::result::Result<f64, String> {
        let mut lhs = self.parse_unary()?;
        while matches!(
            self.tok,
            ArithTok::Star | ArithTok::Slash | ArithTok::Percent
        ) {
            let op = self.tok;
            self.next();
            let rhs = self.parse_unary()?;
            match op {
                ArithTok::Star => lhs *= rhs,
                ArithTok::Slash => {
                    if rhs == 0.0 {
                        return Err("division by zero".into());
                    }
                    lhs /= rhs;
                }
                ArithTok::Percent => {
                    if rhs == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    lhs %= rhs;
                }
                _ => unreachable!(),
            }
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> std::result::Result<f64, String> {
        match self.tok {
            ArithTok::Plus => {
                self.next();
                self.parse_unary()
            }
            ArithTok::Minus => {
                self.next();
                Ok(-self.parse_unary()?)
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<f64, String> {
        match self.tok {
            ArithTok::Num => {
                let v = self.num;
                self.next();
                Ok(v)
            }
            ArithTok::LParen => {
                self.next();
                let v = self.parse_expr()?;
                if self.tok != ArithTok::RParen {
                    return Err("expected ')'".into());
                }
                self.next();
                Ok(v)
            }
            ArithTok::End => Err("unexpected end of expression".into()),
            _ => Err(format!("unexpected token {:?}", self.lex)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;

    fn calc(s: &str) -> Result<String> {
        fn_calc(&MemoryContext::new(), &[s.to_string()])
    }
    fn num(s: &str, b: &str) -> Result<String> {
        fn_num(&MemoryContext::new(), &[s.to_string(), b.to_string()])
    }

    #[test]
    fn arithmetic() {
        assert_eq!(calc("1+2*3").unwrap(), "7");
        assert_eq!(calc("(1+2)*3").unwrap(), "9");
        assert_eq!(calc("10/4").unwrap(), "2.5");
        assert_eq!(calc("10%3").unwrap(), "1");
        assert_eq!(calc("-3 + 5").unwrap(), "2");
        assert_eq!(calc("  ").unwrap(), "");
        assert!(calc("1/0").is_err());
        assert!(calc("1+").is_err());
    }

    #[test]
    fn base_convert() {
        assert_eq!(num("0xff", "10").unwrap(), "255");
        assert_eq!(num("255", "16").unwrap(), "ff");
        assert_eq!(num("0b1010", "10").unwrap(), "10");
        assert_eq!(num("8", "2").unwrap(), "1000");
        assert!(num("abc", "10").is_err());
        assert!(num("10", "7").is_err());
    }
}
