//! 快捷输入：纯逻辑提供器（日期 / 计算器）。
//!
//! 与 Go 版本 `wind_input/internal/coordinator/quick_input_{date,calc}.go` 对齐。
//! 本模块只负责把输入缓冲（如 "12.25" / "1+2*3"）转换为候选文本列表，
//! 不涉及按键流程与 UI（由协调器状态机驱动）。
//!
//! 首版覆盖：日期格式化 + 计算器（表达式=结果 / 结果）。
//! 后置：中文数字/金额读法、年月、快捷输入内拼音。

use chrono::Datelike;

/// 合并各提供器候选并去重（保留首现顺序：日期 → 年月 → 计算器 → 数字/金额）。
pub fn generate_quick_input_candidates(buffer: &str, decimal_places: i32) -> Vec<String> {
    // 表达式以运算符结尾（输入未完成，如 "123+"）：候选维持为去掉尾部运算符后的样子，
    // 不中断。"123+" → 与 "123" 一致；"1+2*" → 与 "1+2" 一致。
    let trimmed = buffer.trim_end_matches(['+', '-', '*', '/']);
    let buffer = if trimmed.len() != buffer.len() && !trimmed.is_empty() {
        trimmed
    } else {
        buffer
    };
    let mut out: Vec<String> = Vec::new();
    let push_unique = |s: String, out: &mut Vec<String>| {
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
    for c in generate_number_candidates(buffer) {
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
    // 用户可手打完整等式（如 "100+200=300"）：取首个 '=' 前的表达式部分求值，
    // 使「再按 =」乃至续打答案时首候选维持为 "100+200=300"，不清空。
    let lhs = expr.split('=').next().unwrap_or(expr);
    let clean: &str = lhs.trim_end_matches(['+', '-', '*', '/']);
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
    let mut p = ExprParser {
        input: &bytes,
        pos: 0,
    };
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

// ───────────────────────── 数字 / 金额 / 中文数字 ─────────────────────────

const LOWER_DIGITS: [&str; 10] = ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const UPPER_DIGITS: [&str; 10] = ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"];
const LOWER_UNITS: [&str; 4] = ["", "十", "百", "千"];
const UPPER_UNITS: [&str; 4] = ["", "拾", "佰", "仟"];
const GROUP_UNITS: [&str; 4] = ["", "万", "亿", "万亿"];

/// 是否为纯数字（整数或小数，允许尾部点号，不允许多点/点开头）。
fn is_decimal_number(s: &str) -> bool {
    if s.is_empty() || !s.as_bytes()[0].is_ascii_digit() {
        return false;
    }
    let mut dots = 0;
    for ch in s.bytes() {
        if ch == b'.' {
            dots += 1;
            if dots > 1 {
                return false;
            }
        } else if !ch.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// "123.45" → ("123","45")，"123" → ("123","")
fn split_decimal(s: &str) -> (&str, &str) {
    match s.find('.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    }
}

fn needs_leading_zero(group: &str) -> bool {
    group.len() < 4 || group.as_bytes()[0] == b'0'
}

fn group_to_chinese(group: &str, digits: &[&str; 10], units: &[&str; 4]) -> String {
    let mut result = String::new();
    let mut all_zero = true;
    let mut prev_zero = false;
    let length = group.len();
    for (i, b) in group.bytes().enumerate() {
        let d = (b - b'0') as usize;
        let unit_idx = length - 1 - i;
        if d == 0 {
            prev_zero = true;
            continue;
        }
        all_zero = false;
        if prev_zero && !result.is_empty() {
            result.push_str(digits[0]);
        }
        prev_zero = false;
        result.push_str(digits[d]);
        if unit_idx < units.len() {
            result.push_str(units[unit_idx]);
        }
    }
    if all_zero { String::new() } else { result }
}

/// 数字串 → 中文（按每 4 位一组：个/万/亿/万亿）
fn number_to_chinese(num: &str, digits: &[&str; 10], units: &[&str; 4]) -> String {
    let num = num.trim_start_matches('0');
    if num.is_empty() {
        return digits[0].to_string();
    }
    // 从右往左切 4 位一组
    let mut groups: Vec<&str> = Vec::new();
    let mut end = num.len();
    while end > 0 {
        let start = end.saturating_sub(4);
        groups.push(&num[start..end]);
        end = start;
    }
    let mut result = String::new();
    for i in (0..groups.len()).rev() {
        let group_str = groups[i];
        let group_text = group_to_chinese(group_str, digits, units);
        if group_text.is_empty() {
            continue;
        }
        if !result.is_empty() && needs_leading_zero(group_str) {
            result.push_str(digits[0]);
        }
        result.push_str(&group_text);
        if i < GROUP_UNITS.len() {
            result.push_str(GROUP_UNITS[i]);
        }
    }
    if result.is_empty() {
        digits[0].to_string()
    } else {
        result
    }
}

fn number_to_amount(num: &str, upper: bool) -> String {
    let text = if upper {
        number_to_chinese(num, &UPPER_DIGITS, &UPPER_UNITS)
    } else {
        number_to_chinese(num, &LOWER_DIGITS, &LOWER_UNITS)
    };
    format!("{}元整", text)
}

/// 带角分金额转换（≤2 位小数）；超 2 位返回空串。
fn decimal_to_amount(int_part: &str, dec_part: &str, upper: bool) -> String {
    let int_text = if upper {
        number_to_chinese(int_part, &UPPER_DIGITS, &UPPER_UNITS)
    } else {
        number_to_chinese(int_part, &LOWER_DIGITS, &LOWER_UNITS)
    };
    let digits = if upper { &UPPER_DIGITS } else { &LOWER_DIGITS };
    if dec_part.is_empty() {
        return format!("{}元整", int_text);
    }
    if dec_part.len() > 2 {
        return String::new();
    }
    let jiao = (dec_part.as_bytes()[0] - b'0') as usize;
    let fen = if dec_part.len() == 2 {
        (dec_part.as_bytes()[1] - b'0') as usize
    } else {
        0
    };
    if jiao == 0 && fen == 0 {
        return format!("{}元整", int_text);
    }
    let mut b = format!("{}元", int_text);
    if jiao == 0 {
        b.push_str("零");
        b.push_str(digits[fen]);
        b.push_str("分");
    } else if fen == 0 {
        b.push_str(digits[jiao]);
        b.push_str("角整");
    } else {
        b.push_str(digits[jiao]);
        b.push_str("角");
        b.push_str(digits[fen]);
        b.push_str("分");
    }
    b
}

/// 中文小数读法："123","456" → "一百二十三点四五六"
fn decimal_to_chinese_text(int_part: &str, dec_part: &str, upper: bool) -> String {
    let int_text = if upper {
        number_to_chinese(int_part, &UPPER_DIGITS, &UPPER_UNITS)
    } else {
        number_to_chinese(int_part, &LOWER_DIGITS, &LOWER_UNITS)
    };
    if dec_part.is_empty() {
        return int_text;
    }
    let digits = if upper { &UPPER_DIGITS } else { &LOWER_DIGITS };
    let mut b = int_text;
    b.push_str("点");
    for ch in dec_part.bytes() {
        if ch.is_ascii_digit() {
            b.push_str(digits[(ch - b'0') as usize]);
        }
    }
    b
}

/// 逐位中文（含小数点）："123" → "一二三"
fn digits_to_chinese_chars(num: &str, upper: bool) -> String {
    let digits = if upper { &UPPER_DIGITS } else { &LOWER_DIGITS };
    let mut b = String::new();
    for ch in num.chars() {
        if ch.is_ascii_digit() {
            b.push_str(digits[(ch as u8 - b'0') as usize]);
        } else if ch == '.' {
            b.push_str("点");
        }
    }
    if b.is_empty() {
        digits[0].to_string()
    } else {
        b
    }
}

/// 千分位分组："1234567" → "1,234,567"
fn format_thousands(num: &str) -> String {
    if num.len() <= 3 {
        return num.to_string();
    }
    let mut b = String::new();
    let remainder = num.len() % 3;
    if remainder > 0 {
        b.push_str(&num[..remainder]);
    }
    let mut i = remainder;
    while i < num.len() {
        if !b.is_empty() {
            b.push(',');
        }
        b.push_str(&num[i..i + 3]);
        i += 3;
    }
    b
}

/// 由纯数字串生成候选（金额/中文数字/千分位）。非数字串返回空。
pub fn generate_number_candidates(s: &str) -> Vec<String> {
    if !is_decimal_number(s) {
        return Vec::new();
    }
    let (int_part_raw, dec_part) = split_decimal(s);
    let int_part = if int_part_raw.is_empty() {
        "0"
    } else {
        int_part_raw
    };

    if dec_part.is_empty() {
        // 整数（含 "123." 情况）
        return vec![
            number_to_amount(int_part, true),
            number_to_amount(int_part, false),
            number_to_chinese(int_part, &LOWER_DIGITS, &LOWER_UNITS),
            number_to_chinese(int_part, &UPPER_DIGITS, &UPPER_UNITS),
            digits_to_chinese_chars(int_part, false),
            digits_to_chinese_chars(int_part, true),
            format_thousands(int_part),
        ];
    }

    // 小数
    let mut out = Vec::new();
    let amt_u = decimal_to_amount(int_part, dec_part, true);
    if !amt_u.is_empty() {
        out.push(amt_u);
    }
    let amt_l = decimal_to_amount(int_part, dec_part, false);
    if !amt_l.is_empty() {
        out.push(amt_l);
    }
    out.push(decimal_to_chinese_text(int_part, dec_part, false));
    out.push(decimal_to_chinese_text(int_part, dec_part, true));
    out.push(digits_to_chinese_chars(s, false));
    out.push(digits_to_chinese_chars(s, true));
    out
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
    fn test_calc_keeps_result_through_equals() {
        // 用户按 = 写出完整等式：首候选维持为 123+100=223，不清空。
        let c0 = generate_calc_candidates("123+100", 6);
        assert_eq!(c0[0], "123+100=223");
        let c1 = generate_calc_candidates("123+100=", 6);
        assert_eq!(c1[0], "123+100=223");
        // 续打答案也维持（取 = 前的表达式求值）。
        let c2 = generate_calc_candidates("123+100=223", 6);
        assert_eq!(c2[0], "123+100=223");
    }

    #[test]
    fn test_trailing_operator_matches_prefix() {
        // "123+" 的候选与 "123" 一致（不中断）。
        assert_eq!(
            generate_quick_input_candidates("123+", 6),
            generate_quick_input_candidates("123", 6)
        );
        // "1+2*" 的候选与 "1+2" 一致。
        assert_eq!(
            generate_quick_input_candidates("1+2*", 6),
            generate_quick_input_candidates("1+2", 6)
        );
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

    #[test]
    fn test_number_integer_candidates() {
        let c = generate_number_candidates("123");
        assert!(
            c.contains(&"壹佰贰拾叁元整".to_string()),
            "大写金额，实际: {:?}",
            c
        );
        assert!(
            c.contains(&"一百二十三".to_string()),
            "中文小写，实际: {:?}",
            c
        );
        assert!(c.contains(&"壹佰贰拾叁".to_string()), "中文大写");
        assert!(c.contains(&"一二三".to_string()), "逐位");
    }

    #[test]
    fn test_number_thousands() {
        let c = generate_number_candidates("1234567");
        assert!(
            c.contains(&"1,234,567".to_string()),
            "千分位，实际: {:?}",
            c
        );
        assert!(
            c.contains(&"一百二十三万四千五百六十七".to_string()),
            "中文大数，实际: {:?}",
            c
        );
    }

    #[test]
    fn test_number_decimal_amount() {
        let c = generate_number_candidates("123.45");
        assert!(
            c.contains(&"壹佰贰拾叁元肆角伍分".to_string()),
            "大写角分金额，实际: {:?}",
            c
        );
        assert!(
            c.contains(&"一百二十三点四五".to_string()),
            "中文小数读法，实际: {:?}",
            c
        );
    }

    #[test]
    fn test_pure_number_via_merge() {
        // 纯整数经合并入口也应产出金额候选（修复"123 无候选"）
        let c = generate_quick_input_candidates("123", 6);
        assert!(!c.is_empty(), "纯数字应有候选");
        assert!(c.contains(&"一百二十三".to_string()));
    }

    #[test]
    fn test_number_with_zeros() {
        // 连续零合并
        assert_eq!(
            number_to_chinese("10001", &LOWER_DIGITS, &LOWER_UNITS),
            "一万零一"
        );
        assert_eq!(
            number_to_chinese("100", &LOWER_DIGITS, &LOWER_UNITS),
            "一百"
        );
    }
}
