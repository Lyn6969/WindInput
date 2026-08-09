//! §3.1 取值函数（对照 Go funcs/value.go）。
//!
//! 全部 `pure=true`，但依赖 [`EvalContext`] 外部状态（输入/历史/剪贴板/时间/前台），
//! 故 `deterministic=false`。

use super::func_specs;
use super::util::{parse_arg_int, rune_tail_from};
use crate::context::EvalContext;
use crate::error::{CmdbarError, Result};
use crate::registry::FuncSpec;
use chrono::{DateTime, Days, Local, Months};

pub fn specs() -> Vec<FuncSpec> {
    func_specs! {
        "code" : Value (0, 1) pure => fn_code,  "触发候选时的输入编码; code(n) 从第 n 字符 (1 起) 切到末尾", "code()";
        "tail" : Value (2, 2) det  => fn_tail,  "字符串 s 从第 n 字符 (1 起) 切到末尾", "tail(code, 2)";
        "last" : Value (0, 1) pure => fn_last,  "最近一次上屏文本; last(n) 取倒数第 n 次, n≥1", "last()";
        "clip" : Value (0, 1) pure => fn_clip,  "当前剪贴板内容; clip(n) 取历史第 n 条 (1-based)", "clip()";
        "sel"  : Value (0, 0) pure => fn_sel,   "当前前台应用中选中的文本", "sel()";
        "app"  : Value (0, 0) pure => fn_app,   "当前前台进程名 (basename)", "app()";
        "title": Value (0, 0) pure => fn_title, "当前前台窗口标题", "title()";
        "date" : Value (1, 2) pure => fn_date,  "日期; fmt 用 YYYY MM DD HH mm ss; offset 形如 '+1d' '-2w' '+3M' '-1y'", "date(\"YYYY-MM-DD\", \"+1d\")";
        "time" : Value (0, 1) pure => fn_time,  "当前时间; 默认 fmt='HH:mm:ss'", "time(\"HH:mm\")";
        "now"  : Value (0, 0) pure => fn_now,   "当前日期时间, 等价 date('YYYY-MM-DD HH:mm:ss')", "now()";
        "env"  : Value (1, 1) pure => fn_env,   "读取环境变量", "env(\"HOME\")";
        "uuid" : Value (0, 1) pure => fn_uuid,  "随机 UUID (v4); flags 含 'n' 去横杠、'u' 转大写, 可组合", "uuid(\"n\")";
    }
}

fn fn_code(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let input = ctx.input();
    if args.is_empty() {
        return Ok(input);
    }
    let n = parse_arg_int("code", &args[0])?;
    Ok(rune_tail_from(&input, n))
}

fn fn_tail(_ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let n = parse_arg_int("tail", &args[1])?;
    Ok(rune_tail_from(&args[0], n))
}

fn fn_last(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let n = if args.len() == 1 {
        parse_arg_int("last", &args[0])?
    } else {
        1
    };
    if n < 1 {
        return Ok(String::new());
    }
    Ok(ctx.last(n))
}

fn fn_clip(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let n = if args.len() == 1 {
        parse_arg_int("clip", &args[0])?
    } else {
        0
    };
    Ok(ctx.clip(n))
}

fn fn_sel(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    Ok(ctx.sel())
}
fn fn_app(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    Ok(ctx.app())
}
fn fn_title(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    Ok(ctx.title())
}

fn fn_date(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let mut t = ctx.now();
    if args.len() == 2 {
        t = apply_offset(t, &args[1])?;
    }
    Ok(format_date(&t, &args[0]))
}

fn fn_time(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    let fmt = if args.len() == 1 {
        &args[0]
    } else {
        "HH:mm:ss"
    };
    Ok(format_date(&ctx.now(), fmt))
}

fn fn_now(ctx: &dyn EvalContext, _args: &[String]) -> Result<String> {
    Ok(format_date(&ctx.now(), "YYYY-MM-DD HH:mm:ss"))
}

fn fn_env(ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    // env 透传 ctx.env（ctx 持有 env 快照，与 Go 行为一致）。
    Ok(ctx.env(&args[0]))
}

fn fn_uuid(_ctx: &dyn EvalContext, args: &[String]) -> Result<String> {
    generate_uuid(args.first().map(String::as_str).unwrap_or(""))
}

/// 生成随机 UUID（v4）。`flags` 为大小写不敏感的标志串：`n` 去掉横杠、`u` 转大写，
/// 可组合（`"nu"`）；空串 = 标准带横杠小写。未知标志报错而非静默忽略——写错标志时
/// 用户得不到任何提示的话，只会看到「格式没生效」却无从分辨是拼错还是不支持。
///
/// 与短语侧的 `$uuid` 模板变量共用本函数（`wind_phrase::expand_template`）：同一写法
/// 在 `{uuid()}` 与 `$uuid` 两处必须给出同样的结果，否则用户无从分辨是语法错还是没支持
/// （与 `${APP_DIR}` 等内部目录变量的同源处理一致）。
pub fn generate_uuid(flags: &str) -> Result<String> {
    let mut no_hyphen = false;
    let mut upper = false;
    for c in flags.chars() {
        match c.to_ascii_lowercase() {
            'n' => no_hyphen = true,
            'u' => upper = true,
            _ => {
                return Err(CmdbarError::runtime(
                    "uuid",
                    format!("unknown flag {c:?} (want 'n' = no hyphen / 'u' = uppercase)"),
                ));
            }
        }
    }
    let id = uuid::Uuid::new_v4();
    let s = if no_hyphen {
        id.simple().to_string()
    } else {
        id.hyphenated().to_string()
    };
    Ok(if upper { s.to_uppercase() } else { s })
}

/// 用户格式别名 → chrono strftime（最长前缀优先）。
const FMT_ALIASES: &[(&str, &str)] = &[
    ("YYYY", "%Y"),
    ("YY", "%y"),
    ("MM", "%m"),
    ("DD", "%d"),
    ("HH", "%H"),
    ("mm", "%M"),
    ("ss", "%S"),
    ("M", "%-m"),
    ("D", "%-d"),
    ("h", "%-I"),
    ("m", "%-M"),
    ("s", "%-S"),
];

/// 把用户格式串翻译为 chrono strftime 并格式化。`%` 不在任何别名源中，故输出不会被二次替换。
fn format_date(t: &DateTime<Local>, fmt: &str) -> String {
    let mut layout = String::with_capacity(fmt.len() + 4);
    let bytes = fmt.as_bytes();
    let mut i = 0;
    'outer: while i < bytes.len() {
        for (from, to) in FMT_ALIASES {
            if fmt[i..].starts_with(from) {
                layout.push_str(to);
                i += from.len();
                continue 'outer;
            }
        }
        // 拷贝一个完整 UTF-8 字符
        let ch = fmt[i..].chars().next().unwrap();
        layout.push(ch);
        i += ch.len_utf8();
    }
    t.format(&layout).to_string()
}

/// 按 `+Nd/-Nw/+NM/-Ny` 平移时间。空 offset 原样返回。
fn apply_offset(t: DateTime<Local>, offset: &str) -> Result<DateTime<Local>> {
    if offset.is_empty() {
        return Ok(t);
    }
    let bytes = offset.as_bytes();
    let sign = match bytes[0] {
        b'+' => 1i64,
        b'-' => -1i64,
        _ => return Err(invalid_offset(offset)),
    };
    let unit = *bytes.last().unwrap();
    let num_str = &offset[1..offset.len() - 1];
    let n: i64 = num_str.parse().map_err(|_| invalid_offset(offset))?;
    let n = sign * n;
    let shifted = match unit {
        b'd' => add_days(t, n),
        b'w' => add_days(t, n * 7),
        b'M' => add_months(t, n),
        b'y' => add_months(t, n * 12),
        _ => return Err(invalid_offset(offset)),
    };
    shifted.ok_or_else(|| CmdbarError::runtime("date", "offset out of range"))
}

fn invalid_offset(offset: &str) -> CmdbarError {
    CmdbarError::runtime(
        "date",
        format!("invalid offset {offset:?} (want e.g. +1d / -2w / +3M / -1y)"),
    )
}

fn add_days(t: DateTime<Local>, n: i64) -> Option<DateTime<Local>> {
    if n >= 0 {
        t.checked_add_days(Days::new(n as u64))
    } else {
        t.checked_sub_days(Days::new((-n) as u64))
    }
}

fn add_months(t: DateTime<Local>, n: i64) -> Option<DateTime<Local>> {
    if n >= 0 {
        t.checked_add_months(Months::new(n as u32))
    } else {
        t.checked_sub_months(Months::new((-n) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MemoryContext;
    use chrono::TimeZone;

    fn ctx_at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> MemoryContext {
        let mut c = MemoryContext::new();
        c.clock = Some(Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap());
        c
    }

    #[test]
    fn uuid_default_and_flags() {
        let d = generate_uuid("").unwrap();
        assert_eq!(d.len(), 36);
        assert_eq!(d.matches('-').count(), 4);
        assert_eq!(d, d.to_lowercase());
        // v4 版本位：第 15 个字符恒为 '4'
        assert_eq!(d.chars().nth(14), Some('4'));

        let n = generate_uuid("n").unwrap();
        assert_eq!(n.len(), 32);
        assert!(!n.contains('-'));

        let u = generate_uuid("U").unwrap(); // 标志大小写不敏感
        assert_eq!(u.len(), 36);
        assert_eq!(u, u.to_uppercase());

        let nu = generate_uuid("nu").unwrap();
        assert_eq!(nu.len(), 32);
        assert_eq!(nu, nu.to_uppercase());

        // 每次都是新值
        assert_ne!(generate_uuid("").unwrap(), generate_uuid("").unwrap());

        // 未知标志报错而非静默忽略
        assert!(generate_uuid("x").is_err());
        assert!(generate_uuid("nx").is_err());
    }

    #[test]
    fn uuid_func_takes_optional_flags() {
        let ctx = ctx_at(2026, 6, 14, 9, 5, 7);
        assert_eq!(fn_uuid(&ctx, &[]).unwrap().len(), 36);
        assert_eq!(fn_uuid(&ctx, &["n".into()]).unwrap().len(), 32);
    }

    #[test]
    fn date_format_and_offset() {
        let ctx = ctx_at(2026, 6, 14, 9, 5, 7);
        assert_eq!(fn_date(&ctx, &["YYYY-MM-DD".into()]).unwrap(), "2026-06-14");
        assert_eq!(
            fn_date(&ctx, &["YYYY-MM-DD".into(), "+1d".into()]).unwrap(),
            "2026-06-15"
        );
        assert_eq!(
            fn_date(&ctx, &["YYYY-MM".into(), "-1M".into()]).unwrap(),
            "2026-05"
        );
        assert_eq!(
            fn_date(&ctx, &["YYYY".into(), "+1y".into()]).unwrap(),
            "2027"
        );
    }

    #[test]
    fn time_and_now() {
        let ctx = ctx_at(2026, 6, 14, 9, 5, 7);
        assert_eq!(fn_time(&ctx, &["HH:mm:ss".into()]).unwrap(), "09:05:07");
        assert_eq!(fn_now(&ctx, &[]).unwrap(), "2026-06-14 09:05:07");
    }

    #[test]
    fn code_tail_clip() {
        let mut ctx = MemoryContext::new().with_input("nihao");
        ctx.clip = "板".into();
        assert_eq!(fn_code(&ctx, &[]).unwrap(), "nihao");
        assert_eq!(fn_code(&ctx, &["3".into()]).unwrap(), "hao");
        assert_eq!(
            fn_tail(&ctx, &["abcde".into(), "2".into()]).unwrap(),
            "bcde"
        );
        assert_eq!(fn_clip(&ctx, &[]).unwrap(), "板");
    }

    #[test]
    fn invalid_offset_errors() {
        let ctx = ctx_at(2026, 6, 14, 0, 0, 0);
        assert!(fn_date(&ctx, &["YYYY".into(), "1d".into()]).is_err());
    }
}
