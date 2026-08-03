//! 快捷输入：内置候选来源（日期 / 计算 / 数字金额）的纯逻辑提供器。
//!
//! 本模块只负责把输入缓冲（如 "12.25" / "1+2*3"）转换为候选文本列表，
//! 不涉及按键流程与 UI（由协调器状态机驱动）。
//!
//! ## 来源与开关
//!
//! 三个来源各自是一个 **mix 成员 id**（`quick_input.date` / `.calc` / `.number`），
//! 连同协调器实现的 `quick_input.repeat`（重复上屏）一起，由 `mix_modes.members`
//! 列表决定**有无与顺序**——开关即增删，优先级即排序，不再另设 bool 旁路开关
//! （旧的 `schema.quick_input.enable_english` 曾与 `members` 构成双真相源）。
//!
//! ## 格式取舍
//!
//! 候选格式按国标精简，冗余与不规范写法不再产出（见各来源函数文档）：
//! - 日期：GB/T 7408（≡ISO 8601）+ GB/T 15835（中文数字用法，月日**不补前导零**）
//! - 金额：《会计基础工作规范》第五十二条（大写金额与「整」的写法）

use chrono::Datelike;

// ───────────────────────── 成员 id ─────────────────────────

/// 旧的合并成员 id。存量配置里出现时展开为 [`LEGACY_EXPANSION`]。
pub const MEMBER_LEGACY: &str = "quick_input";
/// 日期 / 年月来源。
pub const MEMBER_DATE: &str = "quick_input.date";
/// 计算来源。
pub const MEMBER_CALC: &str = "quick_input.calc";
/// 数字 / 金额来源。
pub const MEMBER_NUMBER: &str = "quick_input.number";
/// 重复上屏来源（**由协调器实现**：候选取自上屏历史，本 crate 不产出）。
pub const MEMBER_REPEAT: &str = "quick_input.repeat";

/// 旧值 `quick_input` 的展开序，同时是内置「快捷」融合的默认来源序。
///
/// calc 在 date 之前：二者的输入形态互斥（表达式必含二元运算符，日期只有数字与点），
/// 谁在前都不会互相遮蔽，但计算结果作首选是明确诉求，故把 calc 排在最前。
pub const LEGACY_EXPANSION: &[&str] = &[MEMBER_CALC, MEMBER_DATE, MEMBER_NUMBER, MEMBER_REPEAT];

/// 是否为快捷输入家族的内置成员 id（含 `quick_input.repeat` 与旧值 `quick_input`）。
/// 用于把它们从「真实方案成员」中排除——它们没有对应的 `.schema.toml`。
pub fn is_quick_member(member: &str) -> bool {
    member == MEMBER_LEGACY || member.starts_with("quick_input.")
}

/// 本 crate 实现的候选来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickSource {
    /// 日期与年月。
    Date,
    /// 算式求值。
    Calc,
    /// 数字、中文数字与金额。
    Number,
}

impl QuickSource {
    /// 成员 id → 来源。`quick_input.repeat` 与旧值 `quick_input` 返回 `None`
    /// （前者由协调器实现，后者应先经 [`LEGACY_EXPANSION`] 展开）。
    pub fn from_member(member: &str) -> Option<Self> {
        match member {
            MEMBER_DATE => Some(Self::Date),
            MEMBER_CALC => Some(Self::Calc),
            MEMBER_NUMBER => Some(Self::Number),
            _ => None,
        }
    }
}

/// 按来源生成候选。
pub fn generate(src: QuickSource, buffer: &str, decimal_places: i32) -> Vec<String> {
    match src {
        QuickSource::Date => generate_date_candidates(buffer),
        QuickSource::Calc => generate_calc_candidates(buffer, decimal_places),
        QuickSource::Number => generate_number_candidates(buffer, decimal_places),
    }
}

/// 三个来源全开时的合并候选（按 [`LEGACY_EXPANSION`] 序去重）。
/// 便捷入口，主要供测试与不读配置的调用方使用；协调器按 `members` 逐个调 [`generate`]。
pub fn generate_quick_input_candidates(buffer: &str, decimal_places: i32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for src in [QuickSource::Calc, QuickSource::Date, QuickSource::Number] {
        for c in generate(src, buffer, decimal_places) {
            if !c.is_empty() && !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

// ───────────────────────── 输入归一 ─────────────────────────

/// 裁掉尾部「未写完」的运算符与点号，使输入过程中候选不中断：
/// `"123+"` 等同 `"123"`、`"1+2*"` 等同 `"1+2"`、`"2026.3."` 等同 `"2026.3"`。
///
/// 全部裁完则返回原串（`"+++"` 不该被当成空输入）。
fn trim_pending_tail(s: &str) -> &str {
    let t = s.trim_end_matches(['+', '-', '*', '/', '^', '.']);
    if t.is_empty() { s } else { t }
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

/// 日期来源：完整日期优先，否则试年月。输入尾部的点号被容忍
/// （`"2026.3."` 仍出「2026年3月」，此前会因第三段为空而候选全空）。
///
/// 格式集（按序）：中文 → ISO 扩展 → ISO 基本 → 斜杠。
/// **不产出**中文补零写法（`2025年03月05日`）——GB/T 15835 的中文日期不加前导零，
/// 它与不补零的那条只在月/日 <10 时不同，属纯冗余。
pub fn generate_date_candidates(input: &str) -> Vec<String> {
    let input = trim_pending_tail(input);
    let ymd = generate_full_date_candidates(input);
    if !ymd.is_empty() {
        return ymd;
    }
    generate_year_month_candidates(input)
}

/// 完整日期（y.m.d 或 m.d，后者补当前年）。
fn generate_full_date_candidates(input: &str) -> Vec<String> {
    let (mut year, month, day) = match parse_date_parts(input) {
        Some(v) => v,
        None => return Vec::new(),
    };
    if year == 0 {
        year = chrono::Local::now().year();
    }
    vec![
        format!("{}年{}月{}日", year, month, day),
        format!("{:04}-{:02}-{:02}", year, month, day),
        format!("{:04}{:02}{:02}", year, month, day),
        format!("{:04}/{:02}/{:02}", year, month, day),
    ]
}

/// 年月表达式（首段>31，第二段 1-12）。
///
/// 首段 >31 是与「月.日」的分界：`12.25` 只可能是 12 月 25 日，`2025.12` 只可能是年月。
/// 同样不产出中文补零写法（`2025年06月`）。
pub fn generate_year_month_candidates(input: &str) -> Vec<String> {
    let input = trim_pending_tail(input);
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
        format!("{:04}-{:02}", y, m),
        format!("{:04}/{:02}", y, m),
    ]
}

// ───────────────────────── 计算器 ─────────────────────────

/// 是否含**二元**运算符：开头的、以及紧跟另一运算符或左括号的 `+`/`-` 是一元号，不算。
///
/// 这道区分让 `"-5"` 不被当成算式（它只是个负数，交给数字来源），而 `"-5+3"` 是。
fn has_binary_operator(s: &str) -> bool {
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if !matches!(c, b'+' | b'-' | b'*' | b'/' | b'^') {
            continue;
        }
        if matches!(c, b'*' | b'/' | b'^') {
            return true;
        }
        // `+`/`-`：前一个非空字符是数字或右括号才是二元运算
        match b[..i].iter().rev().find(|&&p| p != b' ') {
            Some(&p) if p.is_ascii_digit() || p == b')' => return true,
            _ => {}
        }
    }
    false
}

/// 单个字符是否属于表达式字符集：数字、四则、幂、括号、点。
///
/// **公开的 char 级谓词**：协调器的自由输入透镜要判断「这个字符还能不能算表达式编码」，
/// 必须与本 crate 的求值器认同同一个字符集。抽出来是为了不让那份集合出现第二份拷贝——
/// 两处各写一遍的话，日后给求值器加个 `%` 运算符就会静默地让透镜判据落后一个字符。
pub fn is_expr_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '+' | '-' | '*' | '/' | '^' | '.' | '(' | ')')
}

/// 表达式字符集：数字、四则、幂、括号、点。
fn is_expr_charset(s: &str) -> bool {
    s.chars().all(is_expr_char)
}

/// 计算来源：**结果作首候选**，完整等式次之。
///
/// 用户打算式多半是为了拿结果，等式形态（`1+2*3=7`）留作次选，供需要留痕的场景。
/// 用户手打的 `=` 及其右侧被忽略（取首个 `=` 前求值），使「再按 =」乃至续打答案时
/// 候选不清空。
pub fn generate_calc_candidates(expr: &str, decimal_places: i32) -> Vec<String> {
    let lhs = expr.split('=').next().unwrap_or(expr);
    let clean: &str = trim_pending_tail(lhs);
    if clean.is_empty() || !has_binary_operator(clean) || !is_expr_charset(clean) {
        return Vec::new();
    }
    // 以数字、左括号或一元号开头
    let first = clean.as_bytes()[0];
    if first != b'(' && first != b'-' && first != b'+' && !first.is_ascii_digit() {
        return Vec::new();
    }
    let val = match evaluate_expression(clean) {
        Some(v) if v.is_finite() => v,
        _ => return Vec::new(),
    };
    let result = format_calc_result_prec(val, decimal_places);
    vec![result.clone(), format!("{}={}", clean, result)]
}

/// 递归下降求值。支持 `+ - * /`、幂 `^`、一元正负号与括号。返回 None 表示解析失败。
///
/// 优先级（低→高）：`+ -` < `* /` < 一元 `+ -` < `^`（右结合）。
/// 一元号低于 `^` 是数学惯例：`-2^2 = -(2^2) = -4`；指数侧仍接受一元号，故 `2^-1 = 0.5`。
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
        let mut left = self.parse_unary()?;
        while self.pos < self.input.len() {
            let op = self.input[self.pos];
            if op != b'*' && op != b'/' {
                break;
            }
            self.pos += 1;
            let right = self.parse_unary()?;
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

    /// 一元正负号（可叠加，如 `--3`）。作用于整个幂，故 `-2^2 = -4`。
    fn parse_unary(&mut self) -> Option<f64> {
        if self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'-' || c == b'+' {
                self.pos += 1;
                let v = self.parse_unary()?;
                return Some(if c == b'-' { -v } else { v });
            }
        }
        self.parse_power()
    }

    /// 幂运算，右结合：`2^3^2 = 2^(3^2) = 512`。
    /// 指数侧递归到 `parse_unary` 而非 `parse_power`，使 `2^-1` 合法。
    fn parse_power(&mut self) -> Option<f64> {
        let base = self.parse_primary()?;
        if self.pos < self.input.len() && self.input[self.pos] == b'^' {
            self.pos += 1;
            let exp = self.parse_unary()?;
            let v = base.powf(exp);
            return Some(v);
        }
        Some(base)
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
/// 超出 i64 量程的值走定点浮点格式，避免 `as i64` 饱和成 9223372036854775807。
pub fn format_calc_result_prec(val: f64, decimal_places: i32) -> String {
    if val.is_nan() || val.is_infinite() {
        return val.to_string();
    }
    let fits_i64 = val.abs() < i64::MAX as f64;
    if decimal_places <= 0 {
        let rounded = val.round();
        return if fits_i64 {
            format!("{}", rounded as i64)
        } else {
            format!("{:.0}", rounded)
        };
    }
    // 整数结果直接输出
    if val == val.trunc() {
        return if fits_i64 {
            format!("{}", val as i64)
        } else {
            format!("{:.0}", val)
        };
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

/// 大写金额（《会计基础工作规范》第五十二条）：整数到「元」写「整」。
fn number_to_amount(num: &str) -> String {
    format!(
        "{}元整",
        number_to_chinese(num, &UPPER_DIGITS, &UPPER_UNITS)
    )
}

/// 带角分的大写金额（≤2 位小数）；超 2 位返回空串。
///
/// 「整」的写法遵规范：到元、到角写「整」，到分不写。
fn decimal_to_amount(int_part: &str, dec_part: &str) -> String {
    let int_text = number_to_chinese(int_part, &UPPER_DIGITS, &UPPER_UNITS);
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
        b.push('零');
        b.push_str(UPPER_DIGITS[fen]);
        b.push('分');
    } else if fen == 0 {
        b.push_str(UPPER_DIGITS[jiao]);
        b.push_str("角整");
    } else {
        b.push_str(UPPER_DIGITS[jiao]);
        b.push('角');
        b.push_str(UPPER_DIGITS[fen]);
        b.push('分');
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
    b.push('点');
    for ch in dec_part.bytes() {
        if ch.is_ascii_digit() {
            b.push_str(digits[(ch - b'0') as usize]);
        }
    }
    b
}

/// 逐位中文（含小数点）："123" → "一二三"
fn digits_to_chinese_chars(num: &str) -> String {
    let mut b = String::new();
    for ch in num.chars() {
        if ch.is_ascii_digit() {
            b.push_str(LOWER_DIGITS[(ch as u8 - b'0') as usize]);
        } else if ch == '.' {
            b.push('点');
        }
    }
    if b.is_empty() {
        LOWER_DIGITS[0].to_string()
    } else {
        b
    }
}

/// 千分位分组："1234567" → "1,234,567"；小数部分不分组（GB/T 15835）。
fn format_thousands(int_part: &str, dec_part: &str) -> String {
    let grouped = if int_part.len() <= 3 {
        int_part.to_string()
    } else {
        let mut b = String::new();
        let remainder = int_part.len() % 3;
        if remainder > 0 {
            b.push_str(&int_part[..remainder]);
        }
        let mut i = remainder;
        while i < int_part.len() {
            if !b.is_empty() {
                b.push(',');
            }
            b.push_str(&int_part[i..i + 3]);
            i += 3;
        }
        b
    };
    if dec_part.is_empty() {
        grouped
    } else {
        format!("{}.{}", grouped, dec_part)
    }
}

/// 数字来源的取值：纯数字直接用；**算式先求值再转**，使「算完顺手要金额」一步到位
/// （`123*4` 也能出「肆佰玖拾贰元整」）。负数结果无金额读法，返回 None。
fn number_subject(buffer: &str, decimal_places: i32) -> Option<String> {
    let s = trim_pending_tail(buffer);
    if is_decimal_number(s) {
        return Some(s.to_string());
    }
    if !has_binary_operator(s) || !is_expr_charset(s) {
        return None;
    }
    let val = evaluate_expression(s).filter(|v| v.is_finite())?;
    if val < 0.0 {
        return None;
    }
    let text = format_calc_result_prec(val, decimal_places);
    is_decimal_number(&text).then_some(text)
}

/// 数字来源：金额、中文数字、千分位。
///
/// 格式集按规范精简，**不产出**：
/// - 「一百二十三元整」——财务金额只有「大写壹佰贰拾叁元整」与「小写 ¥123.00」两种合法写法，
///   中文小写加「元整」不属任何规范；
/// - 逐位大写「壹贰叁」——逐位读法用于念号码，与财务大写无关，无使用场景。
pub fn generate_number_candidates(s: &str, decimal_places: i32) -> Vec<String> {
    let Some(subject) = number_subject(s, decimal_places) else {
        return Vec::new();
    };
    let (int_part_raw, dec_part) = split_decimal(&subject);
    let int_part = if int_part_raw.is_empty() {
        "0"
    } else {
        int_part_raw
    };

    let mut out = Vec::new();
    if dec_part.is_empty() {
        out.push(number_to_amount(int_part));
    } else {
        // >2 位小数无角分写法，此条为空则跳过
        let amt = decimal_to_amount(int_part, dec_part);
        if !amt.is_empty() {
            out.push(amt);
        }
    }
    out.push(decimal_to_chinese_text(int_part, dec_part, false));
    out.push(decimal_to_chinese_text(int_part, dec_part, true));
    out.push(digits_to_chinese_chars(&subject));
    out.push(format_thousands(int_part, dec_part));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 成员 id ──

    #[test]
    fn test_member_ids() {
        assert_eq!(
            QuickSource::from_member(MEMBER_DATE),
            Some(QuickSource::Date)
        );
        assert_eq!(
            QuickSource::from_member(MEMBER_CALC),
            Some(QuickSource::Calc)
        );
        assert_eq!(
            QuickSource::from_member(MEMBER_NUMBER),
            Some(QuickSource::Number)
        );
        // repeat 由协调器实现，旧值应先展开
        assert_eq!(QuickSource::from_member(MEMBER_REPEAT), None);
        assert_eq!(QuickSource::from_member(MEMBER_LEGACY), None);
        assert_eq!(QuickSource::from_member("pinyin"), None);
        // 家族判定覆盖 repeat 与旧值，不误伤真实方案
        assert!(is_quick_member(MEMBER_LEGACY));
        assert!(is_quick_member(MEMBER_REPEAT));
        assert!(is_quick_member(MEMBER_DATE));
        assert!(!is_quick_member("pinyin"));
        assert!(!is_quick_member("english"));
    }

    // ── 计算 ──

    #[test]
    fn test_calc_result_is_first_candidate() {
        // 结果首候选，等式次之（使用算式形态的是少数）
        let c = generate_calc_candidates("1+2*3", 6);
        assert_eq!(c[0], "7");
        assert_eq!(c[1], "1+2*3=7");
    }

    #[test]
    fn test_calc_parentheses() {
        assert_eq!(evaluate_expression("(1+2)*3"), Some(9.0));
        assert_eq!(evaluate_expression("2*(3+4)-1"), Some(13.0));
    }

    #[test]
    fn test_calc_power_precedence_and_associativity() {
        // 幂高于乘除
        assert_eq!(evaluate_expression("2*3^2"), Some(18.0));
        assert_eq!(evaluate_expression("3^2+1"), Some(10.0));
        // 右结合：2^(3^2) = 512，而非 (2^3)^2 = 64
        assert_eq!(evaluate_expression("2^3^2"), Some(512.0));
        // 括号仍可改写结合
        assert_eq!(evaluate_expression("(2^3)^2"), Some(64.0));
        let c = generate_calc_candidates("5^2", 6);
        assert_eq!(c[0], "25");
        assert_eq!(c[1], "5^2=25");
    }

    #[test]
    fn test_calc_unary_sign() {
        // 一元号低于幂：-2^2 = -(2^2)
        assert_eq!(evaluate_expression("-2^2"), Some(-4.0));
        // 指数侧接受一元号
        assert_eq!(evaluate_expression("2^-1"), Some(0.5));
        assert_eq!(evaluate_expression("-5+3"), Some(-2.0));
        // 首负号的算式产出候选
        let c = generate_calc_candidates("-5+3", 6);
        assert_eq!(c[0], "-2");
        // 纯负数不是算式（无二元运算符），交给数字来源
        assert!(generate_calc_candidates("-5", 6).is_empty());
    }

    #[test]
    fn test_calc_division_and_trailing_op() {
        // 尾部运算符应被裁剪
        let c = generate_calc_candidates("10/4+", 6);
        assert_eq!(c[0], "2.5");
        assert_eq!(c[1], "10/4=2.5");
    }

    #[test]
    fn test_calc_division_by_zero_no_candidates() {
        assert!(generate_calc_candidates("1/0", 6).is_empty());
        // 0 的负幂 = inf，同样无候选
        assert!(generate_calc_candidates("0^-1", 6).is_empty());
    }

    #[test]
    fn test_calc_rejects_non_expression() {
        assert!(generate_calc_candidates("123", 6).is_empty()); // 无运算符
        assert!(generate_calc_candidates("abc", 6).is_empty());
        assert!(generate_calc_candidates("2025.12.25", 6).is_empty()); // 日期不是算式
    }

    #[test]
    fn test_calc_keeps_result_through_equals() {
        // 用户按 = 写出完整等式：候选维持不清空。
        assert_eq!(generate_calc_candidates("123+100", 6)[1], "123+100=223");
        assert_eq!(generate_calc_candidates("123+100=", 6)[1], "123+100=223");
        // 续打答案也维持（取 = 前的表达式求值）。
        assert_eq!(generate_calc_candidates("123+100=223", 6)[1], "123+100=223");
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

    // ── 日期 ──

    #[test]
    fn test_date_full_formats() {
        let c = generate_date_candidates("2025.12.25");
        assert_eq!(
            c,
            vec!["2025年12月25日", "2025-12-25", "20251225", "2025/12/25"],
            "中文优先，且不含补零的中文写法"
        );
    }

    #[test]
    fn test_date_no_padded_chinese_form() {
        // 中文日期不加前导零（GB/T 15835），补零写法不再产出
        let c = generate_date_candidates("2025.3.5");
        assert!(c.contains(&"2025年3月5日".to_string()));
        assert!(!c.contains(&"2025年03月05日".to_string()));
        // 数字格式仍补零（ISO 8601）
        assert!(c.contains(&"2025-03-05".to_string()));
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
        assert_eq!(c, vec!["2025年6月", "2025-06", "2025/06"]);
    }

    #[test]
    fn test_year_month_survives_trailing_dot() {
        // 「2026.3.」输入到一半：此前第三段为空导致候选全空，现应维持年月候选
        let c = generate_date_candidates("2026.3.");
        assert_eq!(c, vec!["2026年3月", "2026-03", "2026/03"]);
        assert_eq!(c, generate_date_candidates("2026.3"), "尾点不改变候选");
        // 完整日期同理
        assert_eq!(
            generate_date_candidates("2025.12.25."),
            generate_date_candidates("2025.12.25")
        );
    }

    // ── 数字 / 金额 ──

    #[test]
    fn test_number_integer_candidates() {
        let c = generate_number_candidates("123", 6);
        assert_eq!(
            c,
            vec![
                "壹佰贰拾叁元整",
                "一百二十三",
                "壹佰贰拾叁",
                "一二三",
                "123"
            ]
        );
        // 不规范/无场景的两条已移除
        assert!(!c.contains(&"一百二十三元整".to_string()));
        assert!(!c.contains(&"壹贰叁".to_string()));
    }

    #[test]
    fn test_number_thousands() {
        let c = generate_number_candidates("1234567", 6);
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
        let c = generate_number_candidates("123.45", 6);
        assert_eq!(
            c,
            vec![
                "壹佰贰拾叁元肆角伍分",
                "一百二十三点四五",
                "壹佰贰拾叁点肆伍",
                "一二三点四五",
                "123.45"
            ]
        );
    }

    #[test]
    fn test_number_decimal_thousands() {
        // 小数也给千分位（整数部分分组，小数部分不分组）
        let c = generate_number_candidates("1234567.89", 6);
        assert!(
            c.contains(&"1,234,567.89".to_string()),
            "小数千分位，实际: {:?}",
            c
        );
    }

    #[test]
    fn test_number_amount_zheng_rules() {
        // 到元写整、到角写整、到分不写整（《会计基础工作规范》第五十二条）
        assert_eq!(generate_number_candidates("100", 6)[0], "壹佰元整");
        assert_eq!(generate_number_candidates("100.5", 6)[0], "壹佰元伍角整");
        assert_eq!(generate_number_candidates("100.56", 6)[0], "壹佰元伍角陆分");
        assert_eq!(generate_number_candidates("100.06", 6)[0], "壹佰元零陆分");
    }

    #[test]
    fn test_number_from_calc_result() {
        // 算完顺手要金额：表达式先求值再转
        let c = generate_number_candidates("123*4", 6);
        assert_eq!(c[0], "肆佰玖拾贰元整", "实际: {:?}", c);
        assert!(c.contains(&"四百九十二".to_string()));
        // 负结果无金额读法
        assert!(generate_number_candidates("1-5", 6).is_empty());
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

    // ── 合并入口 ──

    #[test]
    fn test_merge_calc_first_then_number() {
        // 3*3：计算结果 9 首选，等式次之，随后是结果的金额读法
        let c = generate_quick_input_candidates("3*3", 6);
        assert_eq!(c[0], "9");
        assert_eq!(c[1], "3*3=9");
        assert!(c.contains(&"玖元整".to_string()), "实际: {:?}", c);
    }

    #[test]
    fn test_pure_number_via_merge() {
        // 纯整数经合并入口产出金额候选
        let c = generate_quick_input_candidates("123", 6);
        assert_eq!(c[0], "壹佰贰拾叁元整");
        assert!(c.contains(&"一百二十三".to_string()));
    }

    #[test]
    fn test_date_and_number_coexist() {
        // "12.25" 既是日期也是数字：日期在前（number 排在 LEGACY_EXPANSION 之后）
        let c = generate_quick_input_candidates("12.25", 6);
        let year = chrono::Local::now().year();
        assert_eq!(c[0], format!("{}年12月25日", year));
        assert!(c.contains(&"壹拾贰元贰角伍分".to_string()), "实际: {:?}", c);
    }
}
