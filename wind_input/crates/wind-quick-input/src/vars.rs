//! 格式表变量的取值。
//!
//! 每个函数对应一个 [`crate::FormatKind`]，返回 `None` 表示「本类不支持该变量」——
//! 与 [`FormatKind::supports_var`](crate::FormatKind::supports_var) 的白名单**必须一一对应**：
//! 白名单放行而这里取不到值，模板会在渲染期悄悄整条作废，加载期的校验就白做了。
//! 两处都改的义务由 `vars_match_whitelist` 测试兜底。
//!
//! 日期类变量名与 `system.phrases.toml` 同名同义（`$Y` `$MM` `$YC` …），差别只在数据源：
//! 短语层绑当前时间，这里绑用户打进去的数字。

use crate::{
    FormatKind, decimal_to_amount, decimal_to_chinese_text, digits_to_chinese_chars,
    format_thousands, number_to_amount, small_int_to_chinese, split_decimal, year_to_chinese,
};

/// 一次输入解析出的量。
///
/// 两条模板路径的**共同数据源**：`$` 变量经 [`Self::get`] 取值，`{表达式}` 经宿主的
/// `EvalContext::quick_var` 转发到同一个 [`Self::get`]。同源是刻意的——
/// `{month()}` 与 `$M` 若能取到不同的值，用户就没法在两种写法间迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickValues {
    Date {
        y: i32,
        m: u32,
        d: u32,
    },
    /// 只打了月日（`12.25`），`y` 是代码补的当前年。
    ///
    /// 与 [`Self::Date`] 同构且共用 [`date_var`]——差别不在取值，而在**渲染哪一组条目**
    /// （见 [`FormatKind::MonthDay`]）。合成一个变体就没法让两种输入形态各有出厂格式集。
    MonthDay {
        y: i32,
        m: u32,
        d: u32,
    },
    YearMonth {
        y: i32,
        m: u32,
    },
    Number {
        subject: String,
    },
    Calc {
        expr: String,
        result: String,
        /// 结果的原始精度（未按 `decimal_places` 截断/去尾零）。供 `{pct()}` 这类
        /// 需要自行选倍数/小数位/后缀的函数在此基础上二次换算——若只能拿到已按
        /// `decimal_places` 舍入过的 `$RESULT`，`decimal_places=2` 时 1/3 的 `$RESULT`
        /// 已经是 "0.33"，`×100` 只能得到 "33" 而非用户想要的 "33.33"。
        exact: String,
    },
}

impl QuickValues {
    /// 按变量名取值。名字即 `system.quick.toml` 的 `$` 变量名。
    ///
    /// 返回 `None` = 本类不支持该变量（与 [`FormatKind::supports_var`] 一一对应）。
    pub fn get(&self, name: &str) -> Option<String> {
        match self {
            Self::Date { y, m, d } | Self::MonthDay { y, m, d } => date_var(name, *y, *m, *d),
            Self::YearMonth { y, m } => year_month_var(name, *y, *m),
            Self::Number { subject } => number_var(name, subject),
            Self::Calc {
                expr,
                result,
                exact,
            } => calc_var(name, expr, result, exact),
        }
    }

    /// 本次解析对应的格式类别。
    pub fn kind(&self) -> FormatKind {
        match self {
            Self::Date { .. } => FormatKind::Date,
            Self::MonthDay { .. } => FormatKind::MonthDay,
            Self::YearMonth { .. } => FormatKind::YearMonth,
            Self::Number { .. } => FormatKind::Number,
            Self::Calc { .. } => FormatKind::Calc,
        }
    }
}

/// 年份三态 + 中文年。`date` / `month_day` / `year_month` 共用
/// （`month_day` 的年是代码补的当前年，取值路径与另两类无差别）。
fn year_var(name: &str, y: i32) -> Option<String> {
    Some(match name {
        // 原样：改造前 `format!("{}年", year)` 的等价物
        "Y" => y.to_string(),
        // 补零到四位：改造前 ISO/斜杠形态用的是 `{:04}`，与 `$Y` 在 y<1000 时不同
        "YYYY" => format!("{:04}", y),
        // 后两位（`25-12-25` 这类写法）
        "YY" => format!("{:02}", y.rem_euclid(100)),
        "YC" => year_to_chinese(y),
        _ => return None,
    })
}

/// `kind = "date"` 与 `kind = "month_day"`：年 + 月 + 日（含农历）。
///
/// 两类共用一份实现——它们的差别在渲染哪组条目，不在取值。
pub(crate) fn date_var(name: &str, y: i32, m: u32, d: u32) -> Option<String> {
    if let Some(v) = year_var(name, y) {
        return Some(v);
    }
    if crate::lunar::is_var(name) {
        // ★ **换算不出**（超出 1900–2100，或 2 月 31 日这种非法公历日）时返回 `None`
        // 而**不是**空串：空串只在整条模板恰好只有这一个变量时才会让候选消失，
        // 而 `农历$LMD` 会剩下「农历」二字上屏。`None` 让整条模板作废，
        // 公历那几条候选不受影响。
        //
        // 这与 `$LF` 在**非节日**返回空串不冲突，两者是不同的问题：那里日期换算成功了、
        // 只是这一天没有节日名，`$LY年$LMD$LF` 理应给出「丙午年四月廿九」而非整条消失。
        // 详见 [`crate::lunar::var`] 的「`None` 与空串的分工」。
        return crate::lunar::solar_to_lunar(y, m, d).and_then(|l| crate::lunar::var(name, &l));
    }
    Some(match name {
        "M" => m.to_string(),
        "MM" => format!("{:02}", m),
        "MC" => small_int_to_chinese(m),
        "D" => d.to_string(),
        "DD" => format!("{:02}", d),
        "DC" => small_int_to_chinese(d),
        _ => return None,
    })
}

/// `kind = "year_month"`：年 + 月（无日）。
pub(crate) fn year_month_var(name: &str, y: i32, m: u32) -> Option<String> {
    if let Some(v) = year_var(name, y) {
        return Some(v);
    }
    Some(match name {
        "M" => m.to_string(),
        "MM" => format!("{:02}", m),
        "MC" => small_int_to_chinese(m),
        _ => return None,
    })
}

/// `kind = "number"`：`subject` 是 `number_subject` 的产出（纯数字串，可含一个小数点）。
///
/// `$AMT` 在「小数超两位」时返回**空串**而非 `None`：那不是「不支持该变量」，
/// 而是这条候选在本次输入下无意义（无角分写法），空串会让渲染层丢弃该条。
pub(crate) fn number_var(name: &str, subject: &str) -> Option<String> {
    let (int_raw, dec_part) = split_decimal(subject);
    let int_part = if int_raw.is_empty() { "0" } else { int_raw };
    Some(match name {
        "N" => subject.to_string(),
        "THOU" => format_thousands(int_part, dec_part),
        "CNL" => decimal_to_chinese_text(int_part, dec_part, false),
        "CNU" => decimal_to_chinese_text(int_part, dec_part, true),
        "DIG" => digits_to_chinese_chars(subject),
        "AMT" => {
            if dec_part.is_empty() {
                number_to_amount(int_part)
            } else {
                decimal_to_amount(int_part, dec_part)
            }
        }
        _ => return None,
    })
}

/// `kind = "calc"`：`expr` 为裁剪后的算式（不含 `=` 及其右侧），`result` 为格式化后的结果，
/// `exact` 为未截断的原始精度结果。
pub(crate) fn calc_var(name: &str, expr: &str, result: &str, exact: &str) -> Option<String> {
    Some(match name {
        "EXPR" => expr.to_string(),
        "RESULT" => result.to_string(),
        "EXACT" => exact.to_string(),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FormatKind;

    /// ★ 白名单与取值实现必须一一对应。
    ///
    /// 只对一侧做增改是本设计最容易犯的错：白名单放行而取值缺失 → 模板在渲染期整条静默作废，
    /// 加载期校验照样放行，症状是「某条格式莫名不出候选」且日志干净。
    #[test]
    fn vars_match_whitelist() {
        // 覆盖全部四类可能出现的变量名（并集），逐个比对两侧口径
        let all = [
            "Y", "YYYY", "YY", "YC", "M", "MM", "MC", "D", "DD", "DC", "N", "THOU", "CNL", "CNU",
            "DIG", "AMT", "EXPR", "RESULT", "EXACT", "NOPE", "WC", "LY", "LYN", "LZ", "LM", "LD",
            "LMD", "LF",
        ];
        for name in all {
            let cases = [
                // 样本取平常日子（2026-06-14 非节日）而非节日：`$LF` 在非节日返回
                // **空串而非 None**，故「白名单放行 ⟺ 取得到值」在任意一天都成立。
                // 拿平常日子做样本才能真正压住这条——用节日当天会让「$LF 在非节日
                // 退化成 None」这类回归从这里溜过去（由
                // `festival_is_empty_on_ordinary_days` 正面覆盖）。
                (FormatKind::Date, date_var(name, 2026, 6, 14).is_some()),
                // 月日与完整日期共用 date_var，白名单也必须同口径——只给 Date 放行
                // 而漏了 MonthDay，`12.25` 的农历/年份条目会在渲染期整条静默作废。
                (FormatKind::MonthDay, date_var(name, 2026, 6, 14).is_some()),
                (
                    FormatKind::YearMonth,
                    year_month_var(name, 2026, 6).is_some(),
                ),
                (FormatKind::Number, number_var(name, "123").is_some()),
                (FormatKind::Calc, calc_var(name, "1+1", "2", "2").is_some()),
            ];
            for (kind, has_value) in cases {
                assert_eq!(
                    kind.supports_var(name),
                    has_value,
                    "变量 ${} 在 kind={} 上两侧口径不一致（白名单={}，取值={}）",
                    name,
                    kind.as_str(),
                    kind.supports_var(name),
                    has_value
                );
            }
        }
    }

    #[test]
    fn year_forms_differ_below_1000() {
        // $Y 原样、$YYYY 补零——改造前 `{}` 与 `{:04}` 的差别，不能合并成一个变量
        assert_eq!(date_var("Y", 999, 1, 1).unwrap(), "999");
        assert_eq!(date_var("YYYY", 999, 1, 1).unwrap(), "0999");
        assert_eq!(date_var("YY", 2026, 1, 1).unwrap(), "26");
        assert_eq!(date_var("YY", 2005, 1, 1).unwrap(), "05");
    }

    #[test]
    fn month_day_forms() {
        assert_eq!(date_var("M", 2026, 6, 4).unwrap(), "6");
        assert_eq!(date_var("MM", 2026, 6, 4).unwrap(), "06");
        assert_eq!(date_var("MC", 2026, 12, 4).unwrap(), "十二");
        assert_eq!(date_var("DC", 2026, 12, 25).unwrap(), "二十五");
        assert_eq!(date_var("YC", 2005, 1, 1).unwrap(), "二〇〇五");
    }

    #[test]
    fn number_forms() {
        assert_eq!(number_var("N", "123.45").unwrap(), "123.45");
        assert_eq!(number_var("THOU", "1234567").unwrap(), "1,234,567");
        assert_eq!(number_var("CNL", "123").unwrap(), "一百二十三");
        assert_eq!(number_var("CNU", "123").unwrap(), "壹佰贰拾叁");
        assert_eq!(number_var("DIG", "2026").unwrap(), "二〇二六");
        assert_eq!(number_var("AMT", "123").unwrap(), "壹佰贰拾叁元整");
        assert_eq!(number_var("AMT", "123.45").unwrap(), "壹佰贰拾叁元肆角伍分");
    }

    #[test]
    fn lunar_forms() {
        assert_eq!(date_var("LMD", 2026, 6, 14).unwrap(), "四月廿九");
        assert_eq!(date_var("LM", 2026, 6, 14).unwrap(), "四月");
        assert_eq!(date_var("LD", 2026, 6, 14).unwrap(), "廿九");
        assert_eq!(date_var("LY", 2026, 6, 14).unwrap(), "丙午");
        assert_eq!(date_var("LZ", 2026, 6, 14).unwrap(), "马");
        assert_eq!(date_var("LF", 2026, 6, 19).unwrap(), "端午节");
        // 闰月要带「闰」字
        assert_eq!(date_var("LMD", 2020, 6, 1).unwrap(), "闰四月初十");
        // 干支按农历年，不按公历年：2026-01-01 仍在乙巳年
        assert_eq!(date_var("LY", 2026, 1, 1).unwrap(), "乙巳");
        assert_eq!(date_var("LZ", 2026, 1, 1).unwrap(), "蛇");
        // ★ 农历年数字同理与公历年差 1，两者不可混用
        assert_eq!(date_var("LYN", 2026, 1, 1).unwrap(), "2025");
        assert_eq!(date_var("Y", 2026, 1, 1).unwrap(), "2026");
        assert_eq!(date_var("LYN", 2026, 6, 14).unwrap(), "2026");
    }

    /// ★ **算不出来** → `None`（整条模板作废）。
    ///
    /// 空串在这里是错的：它只在整条模板恰好只有这一个变量时才让候选消失，
    /// 而 `农历$LMD` 会剩下「农历」二字上屏。两种成因都要覆盖：范围外、公历日非法。
    #[test]
    fn unconvertible_dates_kill_the_template() {
        // 超出 1900–2100
        assert!(date_var("LMD", 1899, 12, 31).is_none());
        assert!(date_var("LMD", 2101, 1, 1).is_none());
        // 非法公历日期（儒略日会照算成 3/3，必须挡住）
        assert!(date_var("LMD", 2026, 2, 31).is_none());
        // 范围外时**所有**农历变量一起作废，不能只挡住其中几个
        for name in ["LY", "LYN", "LZ", "LM", "LD", "LMD", "LF"] {
            assert!(
                date_var(name, 2101, 1, 1).is_none(),
                "${name} 应随换算失败作废"
            );
        }
    }

    /// ★ **算得出来但这一天没有值** → 空串（整条照常出），不是 `None`。
    ///
    /// 只有 `$LF` 属于这一类。用户写 `$LY年$LMD$LF` 想要的是「节日当天追加节日名」，
    /// 平常日子给出「丙午年四月廿九」——让整条消失没人想要。
    /// 与 `$AMT`「小数超两位返回空串」同一条约定。
    #[test]
    fn festival_is_empty_on_ordinary_days() {
        // 非节日：$LF 有值且为空，同一天其它农历变量照常有值
        assert_eq!(date_var("LF", 2026, 6, 14).unwrap(), "");
        assert_eq!(date_var("LMD", 2026, 6, 14).unwrap(), "四月廿九");
        // 节日当天照常给出节日名
        assert_eq!(date_var("LF", 2026, 6, 19).unwrap(), "端午节");
    }

    /// ★ 年月类不支持农历：农历月与公历月不一一对应，`2026.12` 推不出唯一农历月。
    #[test]
    fn year_month_has_no_lunar_vars() {
        for name in ["LY", "LZ", "LM", "LD", "LMD", "LF"] {
            assert!(
                year_month_var(name, 2026, 12).is_none(),
                "year_month 不该支持 ${name}"
            );
            assert!(!FormatKind::YearMonth.supports_var(name));
            assert!(FormatKind::Date.supports_var(name), "date 应支持 ${name}");
        }
    }

    /// 「不适用」用空串表达，不是 None——渲染层据此丢弃该条候选。
    #[test]
    fn amount_is_empty_beyond_two_decimals() {
        assert_eq!(number_var("AMT", "1.234").unwrap(), "");
        assert!(
            !number_var("CNL", "1.234").unwrap().is_empty(),
            "中文小数不受两位限制"
        );
    }
}
