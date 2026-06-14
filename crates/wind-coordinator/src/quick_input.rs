//! 快捷输入：纯逻辑提供器（日期 / 计算器）。
//!
//! 与 Go 版本 `wind_input/internal/coordinator/quick_input_{date,calc}.go` 对齐。
//! 本模块只负责把输入缓冲（如 "12.25" / "1+2*3"）转换为候选文本列表，
//! 不涉及按键流程与 UI（由协调器状态机驱动）。
//!
//! 首版覆盖：日期格式化 + 计算器（表达式=结果 / 结果）。
//! 后置：中文数字/金额读法、年月、快捷输入内拼音。

use chrono::Datelike;

/// 合并各提供器候选并去重（保留首现顺序：日期 → 计算器）。
pub fn generate_quick_input_candidates(buffer: &str, decimal_places: i32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |s: String, out: &mut Vec<String>| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    for c in generate_date_candidates(buffer) {
        push_unique(c, &mut out);
    }
    for c in generate_year_month_candidates(buffer) {
        push_unique(c, &mut out);
    }
    for c in generate_calc_candidates(buffer, decimal_places) {
        push_unique(c, &mut out);
    }
    out
}

// ───────────────────────── 日期 ─────────────────────────

/// 解析 "m.d" 或 "y.m.d"；省略年份时 year=0。
fn parse_date_parts(s: &str) -> Option<(i32, u32, u32)> {
    let parts: Vec<&str> = s.split('.').collect();
    match parts.len() {
        2 => {
            let m: u32 = parts[0].parse().ok()?;
            let d: u32 = parts[1].parse().ok()?;
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                Some((0, m, d))
            } else {
                None
            }
        }
        3 => {
            let y: i32 = parts[0].parse().ok()?;
            let m: u32 = parts[1].parse().ok()?;
            let d: u32 = parts[2].parse().ok()?;
            if (1..=12).contains(&m) && (1..=31).contains(&d) {
                Some((y, m, d))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 由日期串生成多种格式候选。
pub fn generate_date_candidates(input: &str) -> Vec<String> {
    let (mut year, month, day) = match parse_date_parts(input) {
        Some(v) => v,
        None => return Vec::new(),
    };
    if year == 0 {
        year = chrono::Local::now().year();
    }
    vec![
        format!("{:04}{:02}{:02}", year, month, day),
        format!("{}年{}月{}日", year, month, day),
        format!("{}年{:02}月{:02}日", year, month, day),
        format!("{:04}-{:02}-{:02}", year, month, day),
        format!("{:04}/{:02}/{:02}", year, month, day),
    ]
}

/// 年月表达式（首段>31，第二段 1-12）生成 "y年m月" 等。
pub fn generate_year_month_candidates(input: &str) -> Vec<String> {
    let parts: Vec<&str> = input.split('.').collect();
    if parts.len() != 2 {
        return Vec::new();
    }
    let y: i32 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let m: u32 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if y <= 31 || !(1..=12).contains(&m) {
        return Vec::new();
    }
    vec![
        format!("{}年{}月", y, m),
        format!("{}年{:02}月", y, m),
        format!("{:04}-{:02}", y, m),
        format!("{:04}/{:02}", y, m),
    ]
}

// ───────────────────────── 计算器 ─────────────────────────

/// 是否包含运算符。
fn has_operator(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '+' | '-' | '*' | '/'))
}

/// 由计算表达式生成候选（首版：表达式=结果 / 结果）。
pub fn generate_calc_candidates(expr: &str, decimal_places: i32) -> Vec<String> {
    let clean: &str = expr.trim_end_matches(['+', '-', '*', '/']);
    if clean.is_empty() || !has_operator(clean) {
        return Vec::new();
    }
    // 仅允许数字/运算符/括号/点，且以数字或左括号开头
    let first = clean.as_bytes()[0];
    if first != b'(' && !first.is_ascii_digit() {
        return Vec::new();
    }
    if !clean
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '*' | '/' | '.' | '(' | ')'))
    {
        return Vec::new();
    }
    let val = match evaluate_expression(clean) {
        Some(v) if v.is_finite() => v,
        _ => return Vec::new(),
    };
    let result = format_calc_result_prec(val, decimal_places);
    vec![format!("{}={}", clean, result), result]
}

/// 递归下降求值（支持 + - * / 与括号、优先级）。返回 None 表示解析失败。
pub fn evaluate_expression(expr: &str) -> Option<f64> {
    let bytes: Vec<u8> = expr.bytes().collect();
    let mut p = ExprParser { input: &bytes, pos: 0 };
    let v = p.parse_expr()?;
    // 必须消费完整输入
    if p.pos != p.input.len() {
        return None;
    }
    Some(v)
}

struct ExprParser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl ExprParser<'_> {
    fn parse_expr(&mut self) -> Option<f64> {
        let mut left = self.parse_term()?;
        while self.pos < self.input.len() {
            let op = self.input[self.pos];
            if op != b'+' && op != b'-' {
                break;
            }
            self.pos += 1;
            let right = self.parse_term()?;
            if op == b'+' {
                left += right;
            } else {
                left -= right;
            }
        }
        Some(left)
    }

    fn parse_term(&mut self) -> Option<f64> {
        let mut left = self.parse_primary()?;
        while self.pos < self.input.len() {
            let op = self.input[self.pos];
            if op != b'*' && op != b'/' {
                break;
            }
            self.pos += 1;
            let right = self.parse_primary()?;
            if op == b'*' {
                left *= right;
            } else {
                if right == 0.0 {
                    return None; // 除零
                }
                left /= right;
            }
        }
        Some(left)
    }

    fn parse_primary(&mut self) -> Option<f64> {
        if self.pos < self.input.len() && self.input[self.pos] == b'(' {
            self.pos += 1;
            let v = self.parse_expr()?;
            if self.pos >= self.input.len() || self.input[self.pos] != b')' {
                return None;
            }
            self.pos += 1;
            return Some(v);
        }
        self.parse_number()
    }

    fn parse_number(&mut self) -> Option<f64> {
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c.is_ascii_digit() || c == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if start == self.pos {
            return None;
        }
        std::str::from_utf8(&self.input[start..self.pos])
            .ok()?
            .parse::<f64>()
            .ok()
    }
}

/// 结果格式化：decimal_places<=0 四舍五入为整数，否则最多保留位数并去尾零。
pub fn format_calc_result_prec(val: f64, decimal_places: i32) -> String {
    if val.is_nan() || val.is_infinite() {
        return val.to_string();
    }
    if decimal_places <= 0 {
        let rounded = val.round();
        return format!("{}", rounded as i64);
    }
    // 整数结果直接输出
    if val == val.trunc() && val.abs() < i64::MAX as f64 {
        return format!("{}", val as i64);
    }
    let mut s = format!("{:.*}", decimal_places as usize, val);
    if s.contains('.') {
        s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_basic_precedence() {
        let c = generate_calc_candidates("1+2*3", 6);
        assert_eq!(c[0], "1+2*3=7");
        assert_eq!(c[1], "7");
    }

    #[test]
    fn test_calc_parentheses() {
        assert_eq!(evaluate_expression("(1+2)*3"), Some(9.0));
        assert_eq!(evaluate_expression("2*(3+4)-1"), Some(13.0));
    }

    #[test]
    fn test_calc_division_and_trailing_op() {
        // 尾部运算符应被裁剪
        let c = generate_calc_candidates("10/4+", 6);
        assert_eq!(c[0], "10/4=2.5");
    }

    #[test]
    fn test_calc_division_by_zero_no_candidates() {
        assert!(generate_calc_candidates("1/0", 6).is_empty());
    }

    #[test]
    fn test_calc_rejects_non_expression() {
        assert!(generate_calc_candidates("123", 6).is_empty()); // 无运算符
        assert!(generate_calc_candidates("abc", 6).is_empty());
    }

    #[test]
    fn test_date_full_formats() {
        let c = generate_date_candidates("2025.12.25");
        assert!(c.contains(&"20251225".to_string()));
        assert!(c.contains(&"2025年12月25日".to_string()));
        assert!(c.contains(&"2025-12-25".to_string()));
        assert!(c.contains(&"2025/12/25".to_string()));
    }

    #[test]
    fn test_date_month_day_uses_current_year() {
        let c = generate_date_candidates("12.25");
        let year = chrono::Local::now().year();
        assert!(c.iter().any(|s| s == &format!("{}年12月25日", year)));
    }

    #[test]
    fn test_date_invalid() {
        assert!(generate_date_candidates("13.40").is_empty());
        assert!(generate_date_candidates("abc").is_empty());
    }

    #[test]
    fn test_year_month() {
        let c = generate_year_month_candidates("2025.6");
        assert!(c.contains(&"2025年6月".to_string()));
        assert!(c.contains(&"2025-06".to_string()));
    }

    #[test]
    fn test_merge_dedup() {
        // 计算器表达式
        let c = generate_quick_input_candidates("3*3", 6);
        assert_eq!(c[0], "3*3=9");
        assert!(c.contains(&"9".to_string()));
    }
}
