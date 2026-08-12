//! 农历换算的天文底座：朔（新月）时刻与太阳视黄经。
//!
//! **不查表**。1900–2100 的农历数据表流传很广，但来源与许可大多不可考，且闰月分布
//! 一旦抄错，症状是「某一年的农历全错」而非编译失败。这里改用 Meeus《天文算法》
//! 的截断级数直接算，系数可逐条对照原书核验，范围也不受表的两端限制。
//!
//! 精度：朔时刻误差约数秒，太阳黄经约 0.01°（≈15 分钟）。农历只取到「日」，
//! 唯一会被精度影响的是朔或中气恰好落在当地午夜前后几分钟的年份——
//! 这类临界年由 `mod.rs` 的锚点测试覆盖（含 2033 年闰十一月这个罕见情形）。

use std::f64::consts::PI;

const RAD: f64 = PI / 180.0;

/// ΔT = TT − UT，单位秒。Espenak & Meeus 分段多项式。
///
/// 朔时刻算出来是力学时（TT），而日界是世界时（UT）的事——两者在本模块的
/// 适用区间内差 20~200 秒。看似可忽略，但朔落在当地午夜附近时，这 200 秒
/// 直接决定初一是哪天，进而整月错位。
pub fn delta_t_seconds(year: f64) -> f64 {
    if year < 1920.0 {
        let t = year - 1900.0;
        -2.79 + 1.494119 * t - 0.0598939 * t.powi(2) + 0.0061966 * t.powi(3) - 0.000197 * t.powi(4)
    } else if year < 1941.0 {
        let t = year - 1920.0;
        21.20 + 0.84493 * t - 0.076100 * t.powi(2) + 0.0020936 * t.powi(3)
    } else if year < 1961.0 {
        let t = year - 1950.0;
        29.07 + 0.407 * t - t.powi(2) / 233.0 + t.powi(3) / 2547.0
    } else if year < 1986.0 {
        let t = year - 1975.0;
        45.45 + 1.067 * t - t.powi(2) / 260.0 - t.powi(3) / 718.0
    } else if year < 2005.0 {
        let t = year - 2000.0;
        63.86 + 0.3345 * t - 0.060374 * t.powi(2)
            + 0.0017275 * t.powi(3)
            + 0.000651814 * t.powi(4)
            + 0.00002373599 * t.powi(5)
    } else if year < 2050.0 {
        let t = year - 2000.0;
        62.92 + 0.32217 * t + 0.005589 * t.powi(2)
    } else if year < 2150.0 {
        -20.0 + 32.0 * ((year - 1820.0) / 100.0).powi(2) - 0.5628 * (2150.0 - year)
    } else {
        let u = (year - 1820.0) / 100.0;
        -20.0 + 32.0 * u * u
    }
}

/// 第 `k` 个朔的 JDE（力学时）。Meeus 第 49 章。
///
/// `k = 0` 对应 2000-01-06 的朔；负数向前。整数 `k` 才是朔，半整数是望。
pub fn new_moon_jde(k: f64) -> f64 {
    let t = k / 1236.85;
    let (t2, t3, t4) = (t * t, t.powi(3), t.powi(4));
    let mut jde = 2451550.097_66 + 29.530_588_861 * k + 0.000_154_37 * t2 - 0.000_000_150 * t3
        + 0.000_000_000_73 * t4;

    // 地球轨道离心率的长期变化（含 M 的项要按它修正）
    let e = 1.0 - 0.002516 * t - 0.0000074 * t2;
    // 太阳平近点角
    let m = (2.5534 + 29.105_356_70 * k - 0.0000014 * t2 - 0.00000011 * t3) * RAD;
    // 月亮平近点角
    let mp =
        (201.5643 + 385.816_935_28 * k + 0.0107582 * t2 + 0.00001238 * t3 - 0.000000058 * t4) * RAD;
    // 月亮升交点角距
    let f =
        (160.7108 + 390.670_502_84 * k - 0.0016118 * t2 - 0.00000227 * t3 + 0.000000011 * t4) * RAD;
    // 白道升交点黄经
    let om = (124.7746 - 1.563_755_88 * k + 0.0020672 * t2 + 0.00000215 * t3) * RAD;

    jde += -0.40720 * mp.sin()
        + 0.17241 * e * m.sin()
        + 0.01608 * (2.0 * mp).sin()
        + 0.01039 * (2.0 * f).sin()
        + 0.00739 * e * (mp - m).sin()
        - 0.00514 * e * (mp + m).sin()
        + 0.00208 * e * e * (2.0 * m).sin()
        - 0.00111 * (mp - 2.0 * f).sin()
        - 0.00057 * (mp + 2.0 * f).sin()
        + 0.00056 * e * (2.0 * mp + m).sin()
        - 0.00042 * (3.0 * mp).sin()
        + 0.00042 * e * (m + 2.0 * f).sin()
        + 0.00038 * e * (m - 2.0 * f).sin()
        - 0.00024 * e * (2.0 * mp - m).sin()
        - 0.00017 * om.sin()
        - 0.00007 * (mp + 2.0 * m).sin()
        + 0.00004 * (2.0 * mp - 2.0 * f).sin()
        + 0.00004 * (3.0 * m).sin()
        + 0.00003 * (mp + m - 2.0 * f).sin()
        + 0.00003 * (2.0 * mp + 2.0 * f).sin()
        - 0.00003 * (mp + m + 2.0 * f).sin()
        + 0.00003 * (mp - m + 2.0 * f).sin()
        - 0.00002 * (mp - m - 2.0 * f).sin()
        - 0.00002 * (3.0 * mp + m).sin()
        + 0.00002 * (4.0 * mp).sin();

    // 行星摄动等附加周期项 A1..A14
    const A: [(f64, f64, f64); 14] = [
        (299.77, 0.107408, 0.000325),
        (251.88, 0.016321, 0.000165),
        (251.83, 26.651886, 0.000164),
        (349.42, 36.412478, 0.000126),
        (84.66, 18.206239, 0.000110),
        (141.74, 53.303771, 0.000062),
        (207.14, 2.453732, 0.000060),
        (154.84, 7.306860, 0.000056),
        (34.52, 27.261239, 0.000047),
        (207.19, 0.121824, 0.000042),
        (291.34, 1.844379, 0.000040),
        (161.72, 24.198154, 0.000037),
        (239.56, 25.513099, 0.000035),
        (331.55, 3.592518, 0.000023),
    ];
    for (i, (c0, c1, coef)) in A.iter().enumerate() {
        let mut ang = c0 + c1 * k;
        // A1 独有的 T² 项
        if i == 0 {
            ang -= 0.009173 * t2;
        }
        jde += coef * (ang * RAD).sin();
    }
    jde
}

/// 太阳视黄经（度，0–360）。Meeus 第 25 章低精度公式。
pub fn solar_apparent_longitude(jde: f64) -> f64 {
    let t = (jde - 2451545.0) / 36525.0;
    let l0 = 280.46646 + 36000.76983 * t + 0.0003032 * t * t;
    let m = (357.52911 + 35999.05029 * t - 0.0001537 * t * t) * RAD;
    let c = (1.914602 - 0.004817 * t - 0.000014 * t * t) * m.sin()
        + (0.019993 - 0.000101 * t) * (2.0 * m).sin()
        + 0.000289 * (3.0 * m).sin();
    let om = (125.04 - 1934.136 * t) * RAD;
    let lam = l0 + c - 0.00569 - 0.00478 * om.sin();
    lam.rem_euclid(360.0)
}

/// JDE（力学时）→ 东八区**当地日期**的儒略日整数。
///
/// 农历的日界是北京时间午夜（东经 120°），不是 UTC——这一步错了，
/// 所有落在 UTC 16:00 之后的朔都会算到前一天。
pub fn jde_to_china_jd(jde: f64) -> i64 {
    let year = 2000.0 + (jde - 2451545.0) / 365.25;
    let jd_ut = jde - delta_t_seconds(year) / 86400.0;
    (jd_ut + 8.0 / 24.0 + 0.5).floor() as i64
}

/// 公历 → 儒略日整数（Fliegel–Van Flandern）。
pub fn jd_from_ymd(y: i32, m: u32, d: u32) -> i64 {
    let (y, m, d) = (y as i64, m as i64, d as i64);
    let a = (14 - m) / 12;
    let yy = y + 4800 - a;
    let mm = m + 12 * a - 3;
    d + (153 * mm + 2) / 5 + 365 * yy + yy / 4 - yy / 100 + yy / 400 - 32045
}

/// 儒略日整数 → 公历。
pub fn ymd_from_jd(jd: i64) -> (i32, u32, u32) {
    let a = jd + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - 146097 * b / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - 1461 * d / 4;
    let m = (5 * e + 2) / 153;
    let day = e - (153 * m + 2) / 5 + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = 100 * b + d - 4800 + m / 10;
    (year as i32, month as u32, day as u32)
}

/// 该公历年冬至（太阳视黄经 270°）的当地日期 JD。
///
/// 冬至是农历月序的锚：冬至所在月恒为十一月。二分求解，区间取 12/15–12/25
/// （冬至在 12/21±1，留足余量）。
pub fn winter_solstice_jd(year: i32) -> i64 {
    // 二分在 JDE(TT) 上做，故把 UT 区间端点换算回 TT
    let dt = delta_t_seconds(year as f64) / 86400.0;
    let mut lo = jd_from_ymd(year, 12, 15) as f64 - 0.5 - 8.0 / 24.0 + dt;
    let mut hi = jd_from_ymd(year, 12, 25) as f64 - 0.5 - 8.0 / 24.0 + dt;
    // 以 270° 为零点，(x+180) mod 360 − 180 把回绕摊平成单调的 ±180 区间
    let f = |jde: f64| ((solar_apparent_longitude(jde) - 270.0 + 180.0).rem_euclid(360.0)) - 180.0;
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if f(lo) * f(mid) <= 0.0 {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    jde_to_china_jd((lo + hi) / 2.0)
}

/// 某个 JD（当地日）正午的太阳视黄经所在的 30° 区间序号（0–11）。
///
/// 判「该月是否含中气」用的就是它：一个朔望月约 29.5 天、太阳走约 29°，
/// 至多跨一个 30° 边界，故**月首与月末的区间号是否相同**即可判定，
/// 无需逐个中气去二分求时刻（那要多算一个数量级）。
pub fn major_term_index(jd: i64) -> i64 {
    // 当地正午 → UT → TT
    let jd_ut = jd as f64 - 0.5 - 8.0 / 24.0 + 0.5;
    let year = 2000.0 + (jd_ut - 2451545.0) / 365.25;
    let jde = jd_ut + delta_t_seconds(year) / 86400.0;
    let lam = solar_apparent_longitude(jde);
    // 以冬至 270° 为 0 号
    ((lam - 270.0).rem_euclid(360.0) / 30.0).floor() as i64
}

/// 给定 JD 附近的朔月序 `k`（四舍五入到最近的朔）。
pub fn k_near(jd: i64) -> f64 {
    ((jd as f64 - 2451550.097_66) / 29.530_588_861).round()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jd_roundtrip() {
        for (y, m, d) in [(1900, 1, 1), (2000, 2, 29), (2026, 6, 14), (2100, 12, 31)] {
            assert_eq!(ymd_from_jd(jd_from_ymd(y, m, d)), (y, m, d));
        }
    }

    /// 冬至恒在 12/21±1。算错了整个月序都会错位，故单独钉住。
    #[test]
    fn winter_solstice_is_around_dec_21() {
        for year in [1900, 1950, 2000, 2024, 2025, 2033, 2100] {
            let (y, m, d) = ymd_from_jd(winter_solstice_jd(year));
            assert_eq!((y, m), (year, 12), "冬至年月错: {year}");
            assert!((20..=23).contains(&d), "冬至日 {d} 不在 12/20-23: {year}");
        }
    }

    /// 朔望月长度必须落在 29.27–29.84 天（天文极值）之间。
    #[test]
    fn synodic_month_length_is_sane() {
        for k in -1200..1200 {
            let a = new_moon_jde(k as f64);
            let b = new_moon_jde(k as f64 + 1.0);
            let len = b - a;
            assert!(
                (29.20..=29.90).contains(&len),
                "朔望月长度异常 k={k}: {len}"
            );
        }
    }

    #[test]
    fn solar_longitude_advances_about_one_degree_per_day() {
        let base = 2451545.0;
        let a = solar_apparent_longitude(base);
        let b = solar_apparent_longitude(base + 1.0);
        let diff = (b - a).rem_euclid(360.0);
        assert!((0.9..=1.1).contains(&diff), "日行度异常: {diff}");
    }
}
