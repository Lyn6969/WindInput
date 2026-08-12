//! 农历（夏历）换算：公历日期 → 农历年月日 + 干支 + 生肖 + 传统节日。
//!
//! 用的是清代以来行用的**时宪历**规则，三条：
//!
//! 1. 朔日为月首（定朔，非平朔）；
//! 2. 冬至所在的朔望月恒为**十一月**；
//! 3. 两个十一月之间若有 13 个月（即含 13 个朔望月），则其中**第一个不含中气的月**为闰月。
//!
//! 天文量（朔时刻、太阳黄经）见 [`astro`]，本模块只负责把它们组织成历法。
//!
//! ## 为什么不是查表
//!
//! 见 [`astro`] 模块文档。一句话：抄来的表错了不会编译失败，只会让某一年整年错位。
//!
//! ## 供给谁
//!
//! - 快捷输入的 `$L*` 变量（绑用户打进去的日期）；
//! - 短语的 `$L*` 变量（绑当前时间）——与 `$YC`/`$MC`/`$DC` 同一套路，
//!   两处共用同一份实现，否则「今天农历几号」在两个功能里能给出不同答案。

pub mod astro;

use astro::*;
use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

/// 支持范围（公历年，闭区间）。
///
/// 下限取 1900 是惯例；上限 2100 受 ΔT 外推精度限制——再往后朔时刻的误差
/// 会开始威胁「朔落在午夜附近」的判定。超出范围一律返回 `None`，
/// 由调用方渲染成空串并丢弃该条候选，不 panic、不给错值。
pub const MIN_YEAR: i32 = 1900;
pub const MAX_YEAR: i32 = 2100;

const MONTH_NAMES: [&str; 12] = [
    "正", "二", "三", "四", "五", "六", "七", "八", "九", "十", "冬", "腊",
];

/// 日名。初十/二十/三十不写「廿」，21–29 才用「廿」。
const DAY_NAMES: [&str; 30] = [
    "初一", "初二", "初三", "初四", "初五", "初六", "初七", "初八", "初九", "初十", "十一", "十二",
    "十三", "十四", "十五", "十六", "十七", "十八", "十九", "二十", "廿一", "廿二", "廿三", "廿四",
    "廿五", "廿六", "廿七", "廿八", "廿九", "三十",
];

const GAN: [&str; 10] = ["甲", "乙", "丙", "丁", "戊", "己", "庚", "辛", "壬", "癸"];
const ZHI: [&str; 12] = [
    "子", "丑", "寅", "卯", "辰", "巳", "午", "未", "申", "酉", "戌", "亥",
];
const ANIMALS: [&str; 12] = [
    "鼠", "牛", "虎", "兔", "龙", "蛇", "马", "羊", "猴", "鸡", "狗", "猪",
];

/// 按农历月日固定的传统节日。**闰月不过节**（闰五月初五不是端午）。
///
/// 除夕不在表里：它是「腊月最后一天」，而腊月是大月还是小月逐年不同
/// （2024 年是腊月三十，2025–2028 连续四年都是廿九），只能由「次日为正月初一」判定。
const FESTIVALS: &[((u32, u32), &str)] = &[
    ((1, 1), "春节"),
    ((1, 15), "元宵节"),
    ((2, 2), "龙抬头"),
    ((5, 5), "端午节"),
    ((7, 7), "七夕"),
    ((7, 15), "中元节"),
    ((8, 15), "中秋节"),
    ((9, 9), "重阳节"),
    ((12, 8), "腊八节"),
    ((12, 23), "小年"),
];

/// 一个农历日期。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LunarDate {
    /// 农历年。**以正月初一为界**，不是公历年——公历 2026-01-01 属农历 2025 年（乙巳）。
    /// 干支与生肖都据此推，用公历年推是农历实现最常见的错。
    pub year: i32,
    /// 月，1–12（`leap` 为真时表示闰该月）。
    pub month: u32,
    /// 日，1–30。
    pub day: u32,
    /// 是否闰月。
    pub leap: bool,
    /// 传统节日；非节日为 `None`。
    pub festival: Option<&'static str>,
}

impl LunarDate {
    /// 月名，如「正」「腊」。
    pub fn month_name(&self) -> &'static str {
        MONTH_NAMES[(self.month - 1) as usize]
    }

    /// 月的完整写法，如「四月」「闰四月」。
    pub fn month_text(&self) -> String {
        format!(
            "{}{}月",
            if self.leap { "闰" } else { "" },
            self.month_name()
        )
    }

    /// 日的写法，如「初三」「廿九」。
    pub fn day_text(&self) -> &'static str {
        DAY_NAMES[(self.day - 1) as usize]
    }

    /// 月日连写，如「四月廿九」「闰四月初十」。
    pub fn date_text(&self) -> String {
        format!("{}{}", self.month_text(), self.day_text())
    }

    /// 干支年，如「丙午」。
    pub fn ganzhi(&self) -> String {
        // 1984 年为甲子年（干支纪年的现代基准）
        let idx = (self.year - 1984).rem_euclid(60) as usize;
        format!("{}{}", GAN[idx % 10], ZHI[idx % 12])
    }

    /// 生肖，如「马」。
    pub fn animal(&self) -> &'static str {
        ANIMALS[(self.year - 1984).rem_euclid(12) as usize]
    }

    /// 干支年 + 月日，如「丙午年四月廿九」。
    pub fn full_text(&self) -> String {
        format!("{}年{}", self.ganzhi(), self.date_text())
    }
}

/// 是否为农历变量名（`$LY` `$LZ` `$LM` `$LD` `$LMD` `$LF`）。
///
/// 与 [`var`] 的分支**必须一致**：这里认了而那边不认，该变量会静默变成
/// 「本次取不到值」，症状与超出范围无从区分。由 `vars_and_names_agree` 兜底。
pub fn is_var(name: &str) -> bool {
    matches!(name, "LY" | "LYN" | "LZ" | "LM" | "LD" | "LMD" | "LF")
}

/// 按变量名取值。
///
/// 快捷输入（绑用户打进去的日期）与短语（绑当前时间）共用这一份——
/// 同一个 `$LMD` 在两个配置文件里必须给出同一个答案。
///
/// `$LF` 在非节日返回 `None`，好让「今天是$LF」这类模板在平常日子整条消失，
/// 而不是产出半截文本。
pub fn var(name: &str, l: &LunarDate) -> Option<String> {
    Some(match name {
        "LY" => l.ganzhi(),
        // 农历年的数字写法。与公历年可能差 1——2026-01-01 的 $LYN 是 2025，
        // 因为农历年以正月初一为界。想要公历年用 $Y。
        "LYN" => l.year.to_string(),
        "LZ" => l.animal().to_string(),
        "LM" => l.month_text(),
        "LD" => l.day_text().to_string(),
        "LMD" => l.date_text(),
        "LF" => l.festival?.to_string(),
        _ => return None,
    })
}

/// 一个「冬至到冬至」的月序列。
#[derive(Debug, Clone)]
struct Cycle {
    /// (月首 JD, 月号 1–12, 是否闰)
    months: Vec<(i64, u32, bool)>,
    /// 序列末月的次月朔日（区间右开端点）
    end_jd: i64,
    /// 本序列中正月初一的 JD（农历年归属的分界）
    zheng_jd: i64,
}

/// 按锚年缓存。每次按键都要重算候选，而一次 `build_cycle` 要算十几次朔与太阳黄经；
/// 用户输入的日期又高度集中在少数几年，缓存命中率接近 1。
static CACHE: LazyLock<RwLock<HashMap<i32, Cycle>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// 构造锚年 `anchor` 的月序列：从 `anchor-1` 年冬至所在月（十一月）起，
/// 到 `anchor` 年冬至所在月（下一个十一月）止。
fn build_cycle(anchor: i32) -> Cycle {
    let ws_prev = winter_solstice_jd(anchor - 1);
    let ws_cur = winter_solstice_jd(anchor);

    // 定位包含某个 JD 的朔日：从估值出发向两侧收敛。
    // k_near 可能差一个月（朔恰在月初/月末时），故两个方向都要校正。
    let month_start_of = |jd: i64| -> f64 {
        let mut k = k_near(jd);
        while jde_to_china_jd(new_moon_jde(k)) > jd {
            k -= 1.0;
        }
        while jde_to_china_jd(new_moon_jde(k + 1.0)) <= jd {
            k += 1.0;
        }
        k
    };

    let k0 = month_start_of(ws_prev);
    let k1 = month_start_of(ws_cur);
    let n_months = (k1 - k0) as i64;
    // 13 个月 → 需置闰。12 个月为平年。
    let is_leap_year = n_months == 13;

    let mut months = Vec::with_capacity(n_months as usize + 1);
    let mut leap_done = false;
    let mut label: u32 = 11; // 冬至所在月恒为十一月

    for i in 0..=n_months {
        let start = jde_to_china_jd(new_moon_jde(k0 + i as f64));
        let next = jde_to_china_jd(new_moon_jde(k0 + i as f64 + 1.0));
        // 含中气 ⟺ 月首与月末的 30° 区间号不同（朔望月至多跨一个中气）
        let has_term = major_term_index(start) != major_term_index(next - 1);
        let mut leap = false;
        if is_leap_year && !leap_done && !has_term && i > 0 {
            // 闰月沿用上一个月的月号，故这一轮不递增 label
            leap = true;
            leap_done = true;
        } else if i > 0 {
            label = label % 12 + 1;
        }
        months.push((start, label, leap));
    }

    let end_jd = jde_to_china_jd(new_moon_jde(k0 + n_months as f64 + 1.0));
    let zheng_jd = months
        .iter()
        .find(|(_, l, lp)| *l == 1 && !*lp)
        .map(|(s, _, _)| *s)
        .unwrap_or(i64::MAX);

    Cycle {
        months,
        end_jd,
        zheng_jd,
    }
}

/// 取（或构造并缓存）锚年的月序列，在锁内执行 `f` 以免 clone 整个序列。
fn with_cycle<T>(anchor: i32, f: impl FnOnce(&Cycle) -> T) -> T {
    if let Ok(c) = CACHE.read()
        && let Some(cy) = c.get(&anchor)
    {
        return f(cy);
    }
    let cy = build_cycle(anchor);
    let out = f(&cy);
    if let Ok(mut c) = CACHE.write() {
        c.insert(anchor, cy);
    }
    out
}

/// 公历 → 农历。超出 [`MIN_YEAR`]–[`MAX_YEAR`] 或**公历日期本身非法**（如 2 月 31 日）
/// 返回 `None`。
///
/// 非法日期必须挡在这里：`jd_from_ymd` 对 `2026-02-31` 会照算不误（得到 3 月 3 日），
/// 于是农历候选会给出一个「看起来对」的错值。
pub fn solar_to_lunar(y: i32, m: u32, d: u32) -> Option<LunarDate> {
    if !(MIN_YEAR..=MAX_YEAR).contains(&y) {
        return None;
    }
    // 公历日期合法性（闰年、月大小）交给 chrono，本模块不重复实现一份
    chrono::NaiveDate::from_ymd_opt(y, m, d)?;

    let jd = jd_from_ymd(y, m, d);
    // 一个 jd 必落在 cycle(y) 或 cycle(y+1) 之一：cycle(y) 覆盖到 y 年冬至月，
    // 其后的腊月要到 cycle(y+1) 才出现。
    for anchor in [y, y + 1] {
        let hit = with_cycle(anchor, |cy| {
            if jd < cy.months[0].0 || jd >= cy.end_jd {
                return None;
            }
            let idx = cy.months.iter().rposition(|(s, _, _)| *s <= jd)?;
            let (start, label, leap) = cy.months[idx];
            let day = (jd - start + 1) as u32;
            // 农历年以正月初一为界：正月前的腊月/冬月属上一农历年
            let lunar_year = if jd < cy.zheng_jd { anchor - 1 } else { anchor };
            let is_eve = jd + 1 == cy.zheng_jd;
            Some((lunar_year, label, day, leap, is_eve))
        });
        if let Some((year, month, day, leap, is_eve)) = hit {
            let festival = if is_eve {
                Some("除夕")
            } else if leap {
                // 闰月不过节
                None
            } else {
                FESTIVALS
                    .iter()
                    .find(|((fm, fd), _)| *fm == month && *fd == day)
                    .map(|(_, name)| *name)
            };
            return Some(LunarDate {
                year,
                month,
                day,
                leap,
                festival,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lunar(y: i32, m: u32, d: u32) -> LunarDate {
        solar_to_lunar(y, m, d).unwrap_or_else(|| panic!("换算失败: {y}-{m}-{d}"))
    }

    /// ★ 春节锚点。跨 1900–2028，与独立来源（历表）逐年核对过。
    ///
    /// 这是整套算法的总闸门：朔时刻、ΔT、东八区日界、冬至定月序，任何一处系数写错，
    /// 这里必然大面积飘红——而不是等到用户某天发现农历差一天。
    #[test]
    fn spring_festival_anchors() {
        let cases = [
            (1900, 1, 31),
            (1910, 2, 10),
            (1920, 2, 20),
            (1930, 1, 30),
            (1940, 2, 8),
            (1950, 2, 17),
            (1960, 1, 28),
            (1970, 2, 6),
            (1980, 2, 16),
            (1990, 1, 27),
            (2000, 2, 5),
            (2010, 2, 14),
            (2011, 2, 3),
            (2012, 1, 23),
            (2013, 2, 10),
            (2014, 1, 31),
            (2015, 2, 19),
            (2016, 2, 8),
            (2017, 1, 28),
            (2018, 2, 16),
            (2019, 2, 5),
            (2020, 1, 25),
            (2021, 2, 12),
            (2022, 2, 1),
            (2023, 1, 22),
            (2024, 2, 10),
            (2025, 1, 29),
            (2026, 2, 17),
            (2027, 2, 6),
            (2028, 1, 26),
        ];
        for (y, m, d) in cases {
            let l = lunar(y, m, d);
            assert_eq!(
                (l.year, l.month, l.day, l.leap),
                (y, 1, 1, false),
                "{y}-{m}-{d} 应为{y}年正月初一，实得 {}",
                l.date_text()
            );
            assert_eq!(l.festival, Some("春节"));
        }
    }

    /// ★ 闰月分布。闰月是农历最易错的部分，且错了会连累其后整年的月号。
    #[test]
    fn leap_month_anchors() {
        // (公历年月日, 期望月号, 期望日)
        let cases = [
            (2020, 5, 23, 4, 1), // 庚子年闰四月初一
            (2020, 6, 1, 4, 10), // 闰四月初十
            (2023, 3, 22, 2, 1), // 癸卯年闰二月初一
            (2023, 3, 25, 2, 4), // 闰二月初四
            (2025, 7, 25, 6, 1), // 乙巳年闰六月初一
        ];
        for (y, m, d, lm, ld) in cases {
            let l = lunar(y, m, d);
            assert!(l.leap, "{y}-{m}-{d} 应在闰月，实得 {}", l.date_text());
            assert_eq!((l.month, l.day), (lm, ld), "{y}-{m}-{d}");
        }
    }

    /// ★ 平年不得出现闰月——只测「闰年有闰月」会漏掉「平年多算一个闰月」这一半。
    #[test]
    fn common_years_have_no_leap_month() {
        for y in [2021, 2022, 2024, 2026, 2027] {
            let mut jd = jd_from_ymd(y, 1, 1);
            let end = jd_from_ymd(y, 12, 31);
            let mut found = None;
            while jd <= end {
                let (yy, mm, dd) = ymd_from_jd(jd);
                let l = lunar(yy, mm, dd);
                // 该农历年内不应有闰月（跨年的部分属别的农历年，跳过）
                if l.leap && l.year == y {
                    found = Some(l);
                    break;
                }
                jd += 1;
            }
            assert!(found.is_none(), "{y} 农历年不应有闰月，实得 {found:?}");
        }
    }

    /// ★ 农历年归属以正月初一为界，不是公历年——干支/生肖都挂在它上面。
    #[test]
    fn lunar_year_boundary_is_spring_festival() {
        // 2026 春节为 2/17
        let before = lunar(2026, 2, 16);
        assert_eq!(before.year, 2025);
        assert_eq!(before.ganzhi(), "乙巳");
        assert_eq!(before.animal(), "蛇");

        let after = lunar(2026, 2, 17);
        assert_eq!(after.year, 2026);
        assert_eq!(after.ganzhi(), "丙午");
        assert_eq!(after.animal(), "马");

        // 元旦仍属上一农历年
        let newyear = lunar(2026, 1, 1);
        assert_eq!(newyear.year, 2025);
        assert_eq!(newyear.ganzhi(), "乙巳");
    }

    #[test]
    fn ganzhi_and_animal_cycle() {
        assert_eq!(lunar(1984, 6, 1).ganzhi(), "甲子");
        assert_eq!(lunar(1984, 6, 1).animal(), "鼠");
        assert_eq!(lunar(2024, 6, 1).ganzhi(), "甲辰");
        assert_eq!(lunar(2024, 6, 1).animal(), "龙");
        assert_eq!(lunar(2025, 6, 1).ganzhi(), "乙巳");
        assert_eq!(lunar(2044, 6, 1).ganzhi(), "甲子", "60 年一轮回");
    }

    /// ★ 除夕是腊月最后一天，而腊月大小逐年不同——写死「腊月三十」会连错四年。
    #[test]
    fn new_year_eve_tracks_month_length() {
        assert_eq!(lunar(2024, 2, 9).day, 30, "2024 除夕是腊月三十");
        assert_eq!(lunar(2024, 2, 9).festival, Some("除夕"));
        for (y, m, d) in [(2025, 1, 28), (2026, 2, 16), (2027, 2, 5), (2028, 1, 25)] {
            let l = lunar(y, m, d);
            assert_eq!(l.day, 29, "{y} 除夕应为腊月廿九");
            assert_eq!(l.festival, Some("除夕"), "{y}-{m}-{d}");
        }
    }

    #[test]
    fn festivals() {
        assert_eq!(lunar(2025, 10, 6).festival, Some("中秋节"));
        assert_eq!(lunar(2025, 10, 6).date_text(), "八月十五");
        assert_eq!(lunar(2026, 6, 19).festival, Some("端午节"));
        assert_eq!(lunar(2026, 6, 19).date_text(), "五月初五");
        assert_eq!(lunar(2026, 6, 14).festival, None);
        assert_eq!(lunar(2026, 6, 14).date_text(), "四月廿九");
    }

    /// 闰月不过节：闰五月初五不是端午。
    #[test]
    fn leap_month_has_no_festival() {
        // 2025 闰六月初一为 7/25，闰六月里不该冒出节日
        let l = lunar(2025, 7, 25);
        assert!(l.leap);
        assert_eq!(l.festival, None);
    }

    /// ★ 范围外与非法公历日期一律 None，不 panic 也不给错值。
    ///
    /// 非法日期尤其要挡：`2026-02-31` 若不挡，儒略日会照算成 3 月 3 日，
    /// 农历候选就会显示一个「看起来很对」的错值。
    #[test]
    fn out_of_range_and_invalid_dates_are_none() {
        assert!(solar_to_lunar(1899, 12, 31).is_none());
        assert!(solar_to_lunar(2101, 1, 1).is_none());
        assert!(solar_to_lunar(2026, 2, 31).is_none(), "2 月没有 31 日");
        assert!(solar_to_lunar(2025, 2, 29).is_none(), "2025 不是闰年");
        assert!(solar_to_lunar(2026, 13, 1).is_none());
        assert!(solar_to_lunar(2026, 0, 1).is_none());
        assert!(solar_to_lunar(2026, 1, 0).is_none());
        // 闰年 2/29 合法
        assert!(solar_to_lunar(2024, 2, 29).is_some());
    }

    /// 两端边界要能算，不能因为 cycle 越界而失败。
    #[test]
    fn range_endpoints_work() {
        assert!(solar_to_lunar(MIN_YEAR, 1, 1).is_some());
        assert!(solar_to_lunar(MAX_YEAR, 12, 31).is_some());
    }

    /// ★ 全范围连续性：每一天都必须能换算，且日号在 1..=30、月号在 1..=12。
    ///
    /// 抽样跑（每 7 天一次）以控制测试时长；连续性缺口（某个月定位不到）
    /// 是 cycle 边界最容易出的错。
    #[test]
    fn every_day_in_range_converts() {
        let mut jd = jd_from_ymd(MIN_YEAR, 1, 1);
        let end = jd_from_ymd(MAX_YEAR, 12, 31);
        while jd <= end {
            let (y, m, d) = ymd_from_jd(jd);
            let l = solar_to_lunar(y, m, d)
                .unwrap_or_else(|| panic!("{y}-{m}-{d} 换算失败（月序列有缺口）"));
            assert!((1..=12).contains(&l.month), "{y}-{m}-{d} 月号 {}", l.month);
            assert!((1..=30).contains(&l.day), "{y}-{m}-{d} 日号 {}", l.day);
            jd += 7;
        }
    }

    /// 相邻两天的农历日必须连续（+1 或跨月归 1）——月首定位错位会在这里暴露。
    #[test]
    fn consecutive_days_are_continuous() {
        let mut jd = jd_from_ymd(2020, 1, 1);
        let end = jd_from_ymd(2030, 12, 31);
        let (y, m, d) = ymd_from_jd(jd);
        let mut prev = solar_to_lunar(y, m, d).unwrap();
        jd += 1;
        while jd <= end {
            let (y, m, d) = ymd_from_jd(jd);
            let cur = solar_to_lunar(y, m, d).unwrap();
            if cur.day != 1 {
                assert_eq!(
                    cur.day,
                    prev.day + 1,
                    "{y}-{m}-{d} 日号不连续: {} → {}",
                    prev.date_text(),
                    cur.date_text()
                );
            } else {
                // 跨月：上一天必须是该月最后一天（29 或 30）
                assert!(
                    prev.day == 29 || prev.day == 30,
                    "{y}-{m}-{d} 跨月但上一天是 {}",
                    prev.date_text()
                );
            }
            prev = cur;
            jd += 1;
        }
    }

    /// ★ [`is_var`] 与 [`var`] 的分支必须一一对应。
    ///
    /// 只改一侧是这类「名字表 + 取值表」结构的典型错法：`is_var` 认而 `var` 不认，
    /// 该变量会退化成「本次取不到值」，与超出范围的症状完全一样，无从排查。
    #[test]
    fn vars_and_names_agree() {
        // 端午当天：六个变量（含只在节日有值的 $LF）都应取到值
        let l = lunar(2026, 6, 19);
        for name in ["LY", "LYN", "LZ", "LM", "LD", "LMD", "LF"] {
            assert!(is_var(name), "is_var 应认识 ${name}");
            assert!(var(name, &l).is_some(), "var 应能取到 ${name}");
        }
        // 公历变量不属农历表
        for name in ["Y", "M", "D", "YC", "MC", "DC", "NOPE"] {
            assert!(!is_var(name), "is_var 不该认 ${name}");
            assert!(var(name, &l).is_none(), "var 不该取到 ${name}");
        }
    }

    #[test]
    fn text_forms() {
        let l = lunar(2020, 6, 1);
        assert_eq!(l.month_text(), "闰四月");
        assert_eq!(l.day_text(), "初十");
        assert_eq!(l.date_text(), "闰四月初十");
        assert_eq!(l.full_text(), "庚子年闰四月初十");

        let n = lunar(2026, 2, 17);
        assert_eq!(n.date_text(), "正月初一");
        assert_eq!(n.full_text(), "丙午年正月初一");

        // 冬月/腊月用民间写法
        assert_eq!(lunar(2026, 1, 1).month_text(), "冬月");
        assert_eq!(lunar(2026, 2, 16).month_text(), "腊月");
    }
}
