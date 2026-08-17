//! 快捷输入格式表的取值函数族（`system.quick.toml` 的表达式路径）。
//!
//! 取值一律经 [`EvalContext::quick_var`]——本文件**不做解析**，只做呈现加工。
//! 解析（`"2025.12.25"` → 年月日）在 `wind-quick-input` 里，故本 crate 不依赖它。
//!
//! ## 命名参数即「偏离出厂的那一点」
//!
//! 每个函数的**默认参数就是出厂行为**，与对应的 `$` 变量逐字等价：
//! `{month()}` ≡ `$M`，`{month(pad=2)}` ≡ `$MM`，`{month(cn='true')}` ≡ `$MC`。
//! 用户只需要为「与默认不同的那一处」写参数，而不是把整个格式拆成函数调用去拼。
//!
//! ## 不在这里的上下文里，一律空串
//!
//! `quick_var` 默认返回 `None`（见其文档），故这些函数写进短语只得到空串。
//! 这是刻意的：错值比空值难查。

use super::func_specs;
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "year"  : Value (0, 0) pure => fn_year,   "快捷输入: 年", "year(pad=4)"
            named(fn_year_named, "pad" = "左补零到几位, 默认不补", "cn" = "true 出中文数字 (二〇二五)");
        "month" : Value (0, 0) pure => fn_month,  "快捷输入: 月", "month(pad=2)"
            named(fn_month_named, "pad" = "左补零到几位, 默认不补", "cn" = "true 出中文数字 (十二)");
        "day"   : Value (0, 0) pure => fn_day,    "快捷输入: 日", "day(cn='true')"
            named(fn_day_named, "pad" = "左补零到几位, 默认不补", "cn" = "true 出中文数字 (二十五)");
        "lunar" : Value (0, 0) pure => fn_lunar,  "快捷输入: 农历月日 (四月廿九)", "lunar(part='ganzhi')"
            named(fn_lunar_named, "part" = "md(默认)/month/day/ganzhi/zodiac/year/festival/full");
        // 名字刻意不叫 num —— `num` 已是进制转换函数（calc.rs），同名会在本表里覆盖它，
        // 变成「同一个名字在两处含义不同」。`no_name_clash_with_builtins` 守住这条。
        "raw"   : Value (0, 0) pure => fn_raw,    "快捷输入: 所输数字原样", "raw()";
        "cn"    : Value (0, 0) pure => fn_cn,     "快捷输入: 中文数字 (一百二十三)", "cn(upper='true')"
            named(fn_cn_named, "upper" = "true 出大写 (壹佰贰拾叁)");
        "dig"   : Value (0, 0) pure => fn_dig,    "快捷输入: 逐位中文 (一二三)", "dig()";
        "thou"  : Value (0, 0) pure => fn_thou,   "快捷输入: 千分位 (1,234,567)", "thou(sep=' ')"
            named(fn_thou_named, "sep" = "分隔符, 默认 ','", "group" = "每组位数, 默认 3");
        "amt"   : Value (0, 0) pure => fn_amt,    "快捷输入: 大写金额 (壹佰贰拾叁元整)", "amt(unit='圆')"
            named(fn_amt_named, "unit" = "货币单位, 默认 '元'", "zheng" = "false 去掉末尾的 '整'");
        "expr"  : Value (0, 0) pure => fn_expr,   "快捷输入: 所输算式", "expr()";
        "result": Value (0, 0) pure => fn_result, "快捷输入: 算式结果", "result()";
    }
}

// ───────────────────────── 参数解析 ─────────────────────────

/// 具名参数取值（未给出返回 None）。
fn named<'a>(named_args: &'a [(String, String)], key: &str) -> Option<&'a str> {
    named_args
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// 布尔参数。裸 `true` 会被表达式解析成函数调用，故必须写成字符串或 0/1。
/// 无法识别的值**报错而非当成 false**——静默吞掉写错的开关，用户会以为功能坏了。
fn parse_bool(func: &str, key: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(CmdbarError::runtime(
            func,
            format!("{} 需要 'true' 或 'false'，得到 {:?}", key, v),
        )),
    }
}

fn parse_width(func: &str, key: &str, v: &str) -> Result<usize> {
    v.trim()
        .parse::<usize>()
        .map_err(|_| CmdbarError::runtime(func, format!("{} 需要非负整数，得到 {:?}", key, v)))
}

/// 左补零到 `width` 个字符（已够长则原样）。按 char 计数——中文数字模式下补零无意义，
/// 但用户真写了也不该 panic 在字节边界上。
fn pad_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width {
        return s.to_string();
    }
    let mut out = String::with_capacity(width);
    for _ in 0..(width - n) {
        out.push('0');
    }
    out.push_str(s);
    out
}

/// 取 `quick_var`，不在快捷输入上下文时给空串。
fn var(ctx: &dyn EvalContext, name: &str) -> String {
    ctx.quick_var(name).unwrap_or_default()
}

// ───────────────────────── 日期三兄弟 ─────────────────────────

/// 年/月/日共用：`cn` 与 `pad` 的取值逻辑一样，只是原子变量名不同。
/// `cn=true` 时 `pad` 被忽略（给「十二」补零没有意义）。
fn date_part(
    ctx: &dyn EvalContext,
    named_args: &[(String, String)],
    func: &str,
    plain: &str,
    cn_name: &str,
) -> Result<String> {
    if let Some(v) = named(named_args, "cn")
        && parse_bool(func, "cn", v)?
    {
        return Ok(var(ctx, cn_name));
    }
    let raw = var(ctx, plain);
    match named(named_args, "pad") {
        Some(v) => Ok(pad_left(&raw, parse_width(func, "pad", v)?)),
        None => Ok(raw),
    }
}

// ───────────────────────── 农历 ─────────────────────────

/// 取农历变量。**换算不出就报错，不给空串**——这是与本文件其它函数相反的一条，
/// 理由是日期超出 1900–2100（或不在日期上下文）时农历压根算不出来，
/// 而模板通常带字面前缀：`农历{lunar()}` 若把空串填进去，会把「农历」二字单独上屏。
/// 报错则整条候选不出现，与 `$` 变量路径的行为一致。
///
/// 注意 `part='festival'`（`$LF`）**不属于**「取不到」：非节日当天它返回空串而非
/// `None`，于是 `{lunar(part='ganzhi')}年{lunar()}{lunar(part='festival')}` 在平常
/// 日子照常出「丙午年四月廿九」。两种「没有值」的分工见 `wind_quick_input::lunar::var`。
fn lunar_var(ctx: &dyn EvalContext, name: &str) -> Result<String> {
    ctx.quick_var(name).ok_or_else(|| {
        CmdbarError::runtime(
            "lunar",
            format!("取不到 ${name}：不在日期上下文，或日期超出 1900-2100"),
        )
    })
}

fn fn_lunar(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    lunar_var(ctx, "LMD")
}

fn fn_lunar_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    let name = match named(n, "part").unwrap_or("md").trim() {
        "md" => "LMD",
        "month" => "LM",
        "day" => "LD",
        "ganzhi" => "LY",
        "zodiac" => "LZ",
        "year" => "LYN",
        "festival" => "LF",
        // 干支年 + 月日。两段分别取，任一缺失都由 `?` 整条作废，不拼出「年四月廿九」
        "full" => {
            let gz = lunar_var(ctx, "LY")?;
            let md = lunar_var(ctx, "LMD")?;
            return Ok(format!("{gz}年{md}"));
        }
        other => {
            return Err(CmdbarError::runtime(
                "lunar",
                format!(
                    "part 未知取值 {:?}，可用: md/month/day/ganzhi/zodiac/year/festival/full",
                    other
                ),
            ));
        }
    };
    lunar_var(ctx, name)
}

fn fn_year(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "Y"))
}
fn fn_year_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    date_part(ctx, n, "year", "Y", "YC")
}

fn fn_month(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "M"))
}
fn fn_month_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    date_part(ctx, n, "month", "M", "MC")
}

fn fn_day(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "D"))
}
fn fn_day_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    date_part(ctx, n, "day", "D", "DC")
}

// ───────────────────────── 数字 ─────────────────────────

fn fn_raw(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "N"))
}

fn fn_cn(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "CNL"))
}
fn fn_cn_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    let upper = match named(n, "upper") {
        Some(v) => parse_bool("cn", "upper", v)?,
        None => false,
    };
    Ok(var(ctx, if upper { "CNU" } else { "CNL" }))
}

fn fn_dig(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "DIG"))
}

fn fn_thou(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "THOU"))
}

/// 千分位。`group` 非 3 时不能在成品串上改——分组位置本身变了，必须从原数重切。
fn fn_thou_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    let sep = named(n, "sep").unwrap_or(",");
    let group = match named(n, "group") {
        Some(v) => parse_width("thou", "group", v)?.max(1),
        None => 3,
    };
    if group == 3 {
        // 出厂形态就是 3 位一组，只需换分隔符
        return Ok(var(ctx, "THOU").replace(',', sep));
    }
    let raw = var(ctx, "N");
    let (int_part, dec_part) = match raw.split_once('.') {
        Some((i, d)) => (i, Some(d)),
        None => (raw.as_str(), None),
    };
    let digits: Vec<char> = int_part.chars().collect();
    let mut grouped = String::new();
    for (i, c) in digits.iter().enumerate() {
        // 从右往左每 group 位插一个分隔符：左侧余数段之后即第一个插入点
        if i > 0 && (digits.len() - i).is_multiple_of(group) {
            grouped.push_str(sep);
        }
        grouped.push(*c);
    }
    Ok(match dec_part {
        Some(d) => format!("{}.{}", grouped, d),
        None => grouped,
    })
}

fn fn_amt(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "AMT"))
}

/// 大写金额。空串表示本次输入无金额写法（负数/超两位小数），此时任何参数都不该
/// 把它变成非空——否则会凭空造出一条无意义候选。
fn fn_amt_named(ctx: &dyn EvalContext, _a: &[String], n: &[(String, String)]) -> Result<String> {
    let mut s = var(ctx, "AMT");
    if s.is_empty() {
        return Ok(s);
    }
    if let Some(v) = named(n, "zheng")
        && !parse_bool("amt", "zheng", v)?
    {
        s = s.trim_end_matches('整').to_string();
    }
    if let Some(unit) = named(n, "unit") {
        // 「元」在大写金额里只出现一次（整数部分之后），replace 不会误伤
        s = s.replace('元', unit);
    }
    Ok(s)
}

// ───────────────────────── 计算 ─────────────────────────

fn fn_expr(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "EXPR"))
}
fn fn_result(ctx: &dyn EvalContext, _a: &[String]) -> Result<String> {
    Ok(var(ctx, "RESULT"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;
    use std::collections::HashMap;

    /// 带 quick_var 的测试上下文：直接喂一张名→值的表。
    struct QuickTestCtx {
        inner: MemoryContext,
        vars: HashMap<&'static str, &'static str>,
    }

    impl EvalContext for QuickTestCtx {
        fn input(&self) -> String {
            self.inner.input()
        }
        fn last(&self, n: i64) -> String {
            self.inner.last(n)
        }
        fn clip(&self, n: i64) -> String {
            self.inner.clip(n)
        }
        fn sel(&self) -> String {
            self.inner.sel()
        }
        fn app(&self) -> String {
            self.inner.app()
        }
        fn title(&self) -> String {
            self.inner.title()
        }
        fn env(&self, name: &str) -> String {
            self.inner.env(name)
        }
        fn reverse_lookup(&self, text: &str, format: &str) -> String {
            self.inner.reverse_lookup(text, format)
        }
        fn now(&self) -> chrono::DateTime<chrono::Local> {
            self.inner.now()
        }
        fn services(&self) -> Option<&crate::services::Services> {
            self.inner.services()
        }
        fn quick_var(&self, name: &str) -> Option<String> {
            self.vars.get(name).map(|s| s.to_string())
        }
    }

    fn ctx() -> QuickTestCtx {
        QuickTestCtx {
            inner: MemoryContext::new(),
            vars: HashMap::from([
                ("Y", "2025"),
                ("YC", "二〇二五"),
                ("M", "6"),
                ("MC", "六"),
                ("D", "5"),
                ("DC", "五"),
                ("N", "1234567.89"),
                ("CNL", "一百二十三"),
                ("CNU", "壹佰贰拾叁"),
                ("DIG", "一二三"),
                ("THOU", "1,234,567.89"),
                ("AMT", "壹佰贰拾叁元整"),
                ("EXPR", "1+2*3"),
                ("RESULT", "7"),
                // 农历（2026-06-19 端午当天的取值）
                ("LMD", "五月初五"),
                ("LM", "五月"),
                ("LD", "初五"),
                ("LY", "丙午"),
                ("LYN", "2026"),
                ("LZ", "马"),
                ("LF", "端午节"),
            ]),
        }
    }

    /// 农历取不到值的上下文（超出 1900–2100 / 非节日 / 根本不在日期上下文）。
    fn ctx_without_lunar() -> QuickTestCtx {
        let mut c = ctx();
        for k in ["LMD", "LM", "LD", "LY", "LYN", "LZ", "LF"] {
            c.vars.remove(k);
        }
        c
    }

    fn nm(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn lunar_parts() {
        let c = ctx();
        assert_eq!(fn_lunar(&c, &[]).unwrap(), "五月初五");
        let part = |p: &str| fn_lunar_named(&c, &[], &nm(&[("part", p)])).unwrap();
        assert_eq!(part("md"), "五月初五");
        assert_eq!(part("month"), "五月");
        assert_eq!(part("day"), "初五");
        assert_eq!(part("ganzhi"), "丙午");
        assert_eq!(part("zodiac"), "马");
        assert_eq!(part("year"), "2026");
        assert_eq!(part("festival"), "端午节");
        assert_eq!(part("full"), "丙午年五月初五");
        // 无参 ≡ part='md'
        assert_eq!(fn_lunar(&c, &[]).unwrap(), part("md"));
    }

    #[test]
    fn lunar_rejects_unknown_part() {
        let c = ctx();
        let e = fn_lunar_named(&c, &[], &nm(&[("part", "nope")]));
        assert!(e.is_err(), "未知 part 必须报错而不是给空串");
    }

    /// ★ 取不到农历时**报错**，不返回空串。
    ///
    /// 空串会让 `农历{lunar()}` 把「农历」二字单独上屏——表达式路径是整条插值，
    /// 字面前缀不会因为函数返回空串而消失。报错才能让整条候选不出现。
    #[test]
    fn lunar_errors_instead_of_returning_empty() {
        let c = ctx_without_lunar();
        assert!(fn_lunar(&c, &[]).is_err());
        for p in [
            "md", "month", "day", "ganzhi", "zodiac", "year", "festival", "full",
        ] {
            assert!(
                fn_lunar_named(&c, &[], &nm(&[("part", p)])).is_err(),
                "part={p} 取不到值时应报错"
            );
        }
        // 对照：公历函数在同一上下文里仍给空串（既有约定，不受本次改动影响）
        assert_eq!(fn_year(&c, &[]).unwrap(), "2025");
    }

    /// ★ 无参调用必须与对应的 `$` 变量逐字相同——两条路径同源是整个设计的前提。
    #[test]
    fn bare_calls_equal_factory_variables() {
        let c = ctx();
        assert_eq!(fn_year(&c, &[]).unwrap(), "2025");
        assert_eq!(fn_month(&c, &[]).unwrap(), "6");
        assert_eq!(fn_day(&c, &[]).unwrap(), "5");
        assert_eq!(fn_cn(&c, &[]).unwrap(), "一百二十三");
        assert_eq!(fn_dig(&c, &[]).unwrap(), "一二三");
        assert_eq!(fn_thou(&c, &[]).unwrap(), "1,234,567.89");
        assert_eq!(fn_amt(&c, &[]).unwrap(), "壹佰贰拾叁元整");
        assert_eq!(fn_result(&c, &[]).unwrap(), "7");
    }

    #[test]
    fn pad_and_cn_on_date_parts() {
        let c = ctx();
        assert_eq!(fn_month_named(&c, &[], &nm(&[("pad", "2")])).unwrap(), "06");
        assert_eq!(fn_day_named(&c, &[], &nm(&[("pad", "3")])).unwrap(), "005");
        assert_eq!(
            fn_year_named(&c, &[], &nm(&[("cn", "true")])).unwrap(),
            "二〇二五"
        );
        // cn 胜过 pad：给「六」补零没有意义
        assert_eq!(
            fn_month_named(&c, &[], &nm(&[("cn", "1"), ("pad", "2")])).unwrap(),
            "六"
        );
        // 已够宽则不动
        assert_eq!(
            fn_year_named(&c, &[], &nm(&[("pad", "2")])).unwrap(),
            "2025"
        );
    }

    #[test]
    fn thousands_separator_and_group() {
        let c = ctx();
        assert_eq!(
            fn_thou_named(&c, &[], &nm(&[("sep", " ")])).unwrap(),
            "1 234 567.89"
        );
        // group≠3 必须从原数重切，不能在成品串上替换
        assert_eq!(
            fn_thou_named(&c, &[], &nm(&[("group", "4")])).unwrap(),
            "123,4567.89"
        );
        assert_eq!(
            fn_thou_named(&c, &[], &nm(&[("group", "4"), ("sep", "'")])).unwrap(),
            "123'4567.89"
        );
    }

    #[test]
    fn amount_unit_and_zheng() {
        let c = ctx();
        assert_eq!(
            fn_amt_named(&c, &[], &nm(&[("unit", "圆")])).unwrap(),
            "壹佰贰拾叁圆整"
        );
        assert_eq!(
            fn_amt_named(&c, &[], &nm(&[("zheng", "false")])).unwrap(),
            "壹佰贰拾叁元"
        );
        assert_eq!(
            fn_amt_named(&c, &[], &nm(&[("zheng", "0"), ("unit", "圆")])).unwrap(),
            "壹佰贰拾叁圆"
        );
    }

    /// 「本次输入无金额写法」时任何参数都不得把空串变成非空。
    #[test]
    fn amount_stays_empty_when_not_applicable() {
        let mut c = ctx();
        c.vars.insert("AMT", "");
        assert_eq!(
            fn_amt_named(&c, &[], &nm(&[("unit", "圆"), ("zheng", "false")])).unwrap(),
            ""
        );
    }

    /// 写错的开关值必须报错，不能静默当 false——否则用户以为功能坏了。
    #[test]
    fn bad_bool_is_an_error_not_a_silent_false() {
        let c = ctx();
        assert!(fn_amt_named(&c, &[], &nm(&[("zheng", "nope")])).is_err());
        assert!(fn_month_named(&c, &[], &nm(&[("cn", "")])).is_err());
        assert!(fn_month_named(&c, &[], &nm(&[("pad", "x")])).is_err());
    }

    /// 不在快捷输入上下文（短语/命令栏）时一律空串，而不是拿当前编码硬解。
    #[test]
    fn outside_quick_context_yields_empty() {
        let c = MemoryContext::new().with_input("2025.12.25");
        assert_eq!(fn_year(&c, &[]).unwrap(), "");
        assert_eq!(fn_amt(&c, &[]).unwrap(), "");
        assert_eq!(fn_raw(&c, &[]).unwrap(), "");
    }

    /// ★ quick 函数名不得与既有内置函数重名。
    ///
    /// 重名不会报错——`Registry::register` 是同名覆盖，结果是同一个名字在快捷输入里
    /// 和别处含义不同（`num` 就差点这样：它已是进制转换）。将来 cmdbar 新增函数撞名时，
    /// 由这条在 CI 拦住。
    #[test]
    fn no_name_clash_with_builtins() {
        let builtins = crate::registry::Registry::full();
        for s in specs() {
            assert!(
                builtins.lookup(s.name).is_none(),
                "quick 函数 {:?} 与既有内置函数重名，请改名",
                s.name
            );
        }
    }

    /// 白名单与求值入口成对声明（registry.rs 的同名检查只遍历 `full()`，覆盖不到 quick）。
    #[test]
    fn named_params_and_eval_named_are_declared_together() {
        for s in specs() {
            assert_eq!(
                s.named_params.is_empty(),
                s.eval_named.is_none(),
                "{}: named_params 与 eval_named 必须同时声明或同时省略",
                s.name
            );
        }
    }
}
