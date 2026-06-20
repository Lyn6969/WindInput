//! 内置函数共享辅助：参数解析、rune 索引、服务取值。

use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::services::Services;

/// 把参数字符串解析为整数（容许整值浮点，如 "2" / "2.0"）。
pub fn parse_arg_int(func: &str, s: &str) -> Result<i64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CmdbarError::runtime(
            func,
            "expected integer, got empty string",
        ));
    }
    if let Ok(i) = s.parse::<i64>() {
        return Ok(i);
    }
    let f: f64 = s
        .parse()
        .map_err(|_| CmdbarError::runtime(func, format!("expected integer, got {s:?}")))?;
    Ok(f as i64)
}

/// 从第 n 个 rune（1-based）切到末尾。n<=1 返回原串；越界返回空。
pub fn rune_tail_from(s: &str, n: i64) -> String {
    if n <= 1 {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let start = (n - 1) as usize;
    if start >= chars.len() {
        return String::new();
    }
    chars[start..].iter().collect()
}

/// 把 1-based、可能为负的索引转 0-based。负数从末尾算（-1 → n）。越界返回 None。
pub fn resolve_1based(idx: i64, n: usize) -> Option<usize> {
    let n = n as i64;
    let idx = if idx < 0 { n + 1 + idx } else { idx };
    if idx < 1 || idx > n {
        return None;
    }
    Some((idx - 1) as usize)
}

/// rune 数（字符数）。
pub fn rune_len(s: &str) -> usize {
    s.chars().count()
}

/// 取注入的服务束；未注入返回 ServiceUnavailable。
pub fn services<'a>(func: &str, ctx: &'a dyn EvalContext) -> Result<&'a Services> {
    ctx.services().ok_or_else(|| CmdbarError::service(func))
}

/// 把 anyhow 错误包装为 Runtime 错误。
pub fn runtime_err(func: &str, e: anyhow::Error) -> CmdbarError {
    CmdbarError::runtime(func, e.to_string())
}
