//! 输入统计存储（redb，按日期键）
//!
//! 与 Go 版本 `wind_input/internal/store/stats.go` **完整对齐**：每日聚合 `DailyStats`
//! （字符分类 / 24 小时分布 / 码长 / 选重 / 活跃时间 / 按方案 / 按来源）+ 全局 `StatsMeta`
//! （累计 / 首日 / 连续天数 / 最快速度）。采集器见 `stat_collector.rs`。
//!
//! - 每日：key = "YYYY-MM-DD"（STATS_DAILY 表），value = `DailyStats` 的 JSON。
//!   旧库只含 `{chinese, english}`，新增字段全部 `#[serde(default)]`，向后兼容无需迁移脚本。
//! - 元数据：存入现有 `META` 表的 `stats_meta` 键（不新增表定义）。
//! - `date` 是 redb 的 key，不进 `DailyStats` 结构（对齐 Go 用 bucket key）。

use crate::store::{META, STATS_DAILY, Store};
use chrono::{Duration, NaiveDate};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// META 表中存放统计全局元数据的键。
const STATS_META_KEY: &str = "stats_meta";

/// 上屏来源分类（对齐 Go `CommitSource`，末尾追加 Rust 特有的 Url/Mix）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CommitSource {
    #[default]
    Candidate = 0,    // 候选词选择
    RawInput = 1,     // 原始编码上屏（回车/无候选/顶码）
    Punctuation = 2,  // 标点符号
    TempEnglish = 3,  // 临时英文
    TempPinyin = 4,   // 临时拼音
    QuickInput = 5,   // 快捷输入
    FullWidth = 6,    // 全角转换（保留枚举值，Rust 暂不单独产出）
    ModeSwitch = 7,   // 模式切换上屏
    TsfDirect = 8,    // TSF 直接输入（保留枚举值，Rust 暂无该路径）
    SpecialMode = 9,  // 引导键特殊模式
    Url = 10,         // 网址模式（Rust 特有）
    Mix = 11,         // 混合模式（Rust 特有）
}

impl CommitSource {
    /// 来源总数（by_source 数组长度）。
    pub const COUNT: usize = 12;
    /// 数组索引。
    pub fn index(self) -> usize {
        self as usize
    }
}

/// 每个方案的独立统计（对齐 Go `SchemaStats`）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SchemaStats {
    #[serde(default)]
    pub total_chars: u32,
    #[serde(default)]
    pub commit_count: u32,
    #[serde(default)]
    pub code_len_sum: u32,
    #[serde(default)]
    pub code_len_count: u32,
    #[serde(default)]
    pub cand_pos_dist: [u32; 5],
}

/// 单日聚合统计（对齐 Go `DailyStat`；`date` 为 redb key，不入结构）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DailyStats {
    // 字符分类
    #[serde(default)]
    pub chinese: u32,
    #[serde(default)]
    pub english: u32,
    #[serde(default)]
    pub punct: u32,
    #[serde(default)]
    pub other: u32,
    // 时段分布（按小时）
    #[serde(default)]
    pub hours: [u32; 24],
    // 上屏次数
    #[serde(default)]
    pub commit_count: u32,
    // 码长统计（仅候选词上屏）
    #[serde(default)]
    pub code_len_sum: u32,
    #[serde(default)]
    pub code_len_count: u32,
    #[serde(default)]
    pub code_len_dist: [u32; 6], // [1码,2码,3码,4码,5码,6码+]
    // 选重统计（仅候选词上屏）：[首选,2选,3选,4选,5选+]
    #[serde(default)]
    pub cand_pos_dist: [u32; 5],
    // 活跃输入时间（秒），连续输入间隔 < 阈值视为活跃
    #[serde(default)]
    pub active_seconds: u32,
    // 按方案分类
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub by_schema: HashMap<String, SchemaStats>,
    // 按来源分类（索引 = CommitSource::index）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_source: Vec<u32>,
}

impl DailyStats {
    /// 总字符数 = 各分类之和（与 Go `TotalChars` 增量恒等，故不单独存储）。
    pub fn total(&self) -> u32 {
        self.chinese
            .saturating_add(self.english)
            .saturating_add(self.punct)
            .saturating_add(self.other)
    }
}

/// 统计全局元数据（对齐 Go `StatsMeta`）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StatsMeta {
    #[serde(default)]
    pub total_chars: u64,
    #[serde(default)]
    pub first_day: String,
    #[serde(default)]
    pub streak_current: u32,
    #[serde(default)]
    pub streak_max: u32,
    #[serde(default)]
    pub streak_last_day: String,
    #[serde(default)]
    pub max_speed: u32, // 历史最快速度（字/分钟，按天计算）
}

/// 统计摘要，对齐前端 StatsSummary（Stage 4 将扩展富字段）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StatsSummary {
    pub today: u32,
    pub week: u32,
    pub month: u32,
    pub total: u64,
    pub streak: u32,
}

/// 每分钟字数：活跃时间设 5 秒下限（对齐 Go `SpeedPerMinute`）。
pub fn speed_per_minute(chars: u32, active_seconds: u32) -> u32 {
    if chars == 0 || active_seconds == 0 {
        return 0;
    }
    let secs = active_seconds.max(5);
    chars.saturating_mul(60) / secs
}

impl Store {
    /// 累加某日的中文/英文上屏字符数（读改写）。Stage 1 保留，供现有采集单点使用。
    pub fn record_stat(&self, date: &str, chinese: u32, english: u32) -> anyhow::Result<()> {
        if chinese == 0 && english == 0 {
            return Ok(());
        }
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(STATS_DAILY)?;
                let mut rec: DailyStats = t
                    .get(date)?
                    .and_then(|g| serde_json::from_slice(g.value()).ok())
                    .unwrap_or_default();
                rec.chinese = rec.chinese.saturating_add(chinese);
                rec.english = rec.english.saturating_add(english);
                let bytes = serde_json::to_vec(&rec)?;
                t.insert(date, bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 覆盖写入某日完整统计（采集器 flush 用）。
    pub fn put_daily_stat(&self, date: &str, stat: &DailyStats) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(STATS_DAILY)?;
                let bytes = serde_json::to_vec(stat)?;
                t.insert(date, bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 取某日统计（无则零值）。
    pub fn get_daily_stat(&self, date: &str) -> anyhow::Result<DailyStats> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(STATS_DAILY)?;
            Ok(t.get(date)?
                .and_then(|g| serde_json::from_slice(g.value()).ok())
                .unwrap_or_default())
        })
    }

    /// 取日期区间 [from, to]（含端点；YYYY-MM-DD 字典序即时间序）。
    pub fn daily_stats(&self, from: &str, to: &str) -> anyhow::Result<Vec<(String, DailyStats)>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(STATS_DAILY)?;
            let mut out = Vec::new();
            for item in t.range(from..)? {
                let (k, v) = item?;
                let date = k.value();
                if date > to {
                    break;
                }
                if let Ok(rec) = serde_json::from_slice::<DailyStats>(v.value()) {
                    out.push((date.to_string(), rec));
                }
            }
            Ok(out)
        })
    }

    /// 全部每日统计（升序）。
    fn all_daily_stats(&self) -> anyhow::Result<Vec<(String, DailyStats)>> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(STATS_DAILY)?;
            let mut out = Vec::new();
            for item in t.range::<&str>(..)? {
                let (k, v) = item?;
                if let Ok(rec) = serde_json::from_slice::<DailyStats>(v.value()) {
                    out.push((k.value().to_string(), rec));
                }
            }
            Ok(out)
        })
    }

    /// 读取统计全局元数据（无则零值）。
    pub fn get_stats_meta(&self) -> anyhow::Result<StatsMeta> {
        self.with_db(|db| {
            let txn = db.begin_read()?;
            let t = txn.open_table(META)?;
            Ok(t.get(STATS_META_KEY)?
                .and_then(|g| serde_json::from_slice(g.value()).ok())
                .unwrap_or_default())
        })
    }

    /// 写入统计全局元数据。
    pub fn put_stats_meta(&self, meta: &StatsMeta) -> anyhow::Result<()> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            {
                let mut t = txn.open_table(META)?;
                let bytes = serde_json::to_vec(meta)?;
                t.insert(STATS_META_KEY, bytes.as_slice())?;
            }
            txn.commit()?;
            Ok(())
        })
    }

    /// 从现存每日数据重建全局元数据（prune 后/meta 缺失时调用，对齐 Go `RecalculateStatsMeta`）。
    pub fn recalculate_stats_meta(&self) -> anyhow::Result<StatsMeta> {
        let all = self.all_daily_stats()?;
        let mut meta = StatsMeta::default();
        let mut dates = Vec::with_capacity(all.len());
        for (date, stat) in &all {
            if meta.first_day.is_empty() {
                meta.first_day = date.clone();
            }
            meta.total_chars += stat.total() as u64;
            let sp = speed_per_minute(stat.total(), stat.active_seconds);
            if sp > meta.max_speed {
                meta.max_speed = sp;
            }
            dates.push(date.clone());
        }
        let (cur, mx, last) = calculate_streaks(&dates);
        meta.streak_current = cur;
        meta.streak_max = mx;
        meta.streak_last_day = last;
        self.put_stats_meta(&meta)?;
        Ok(meta)
    }

    /// 统计摘要：today/week(近7日)/month(近30日)/total/streak（连续天数）。
    pub fn stats_summary(&self, today: &str) -> anyhow::Result<StatsSummary> {
        let all = self.all_daily_stats()?;
        let today_date = NaiveDate::parse_from_str(today, "%Y-%m-%d").ok();

        let mut s = StatsSummary::default();
        let mut have: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (d, rec) in &all {
            let t = rec.total();
            s.total += t as u64;
            if t > 0 {
                have.insert(d.clone());
            }
            if d == today {
                s.today = t;
            }
            if let Some(td) = today_date {
                if let Ok(dd) = NaiveDate::parse_from_str(d, "%Y-%m-%d") {
                    let days = (td - dd).num_days();
                    if (0..7).contains(&days) {
                        s.week = s.week.saturating_add(t);
                    }
                    if (0..30).contains(&days) {
                        s.month = s.month.saturating_add(t);
                    }
                }
            }
        }

        if let Some(td) = today_date {
            let mut cur = if have.contains(today) {
                td
            } else {
                td - Duration::days(1)
            };
            while have.contains(&cur.format("%Y-%m-%d").to_string()) {
                s.streak += 1;
                cur -= Duration::days(1);
            }
        }
        Ok(s)
    }

    /// 清空所有统计（每日 + 元数据），返回删除的天数。
    pub fn clear_stats(&self) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(STATS_DAILY)?;
                let keys: Vec<String> = {
                    let mut ks = Vec::new();
                    for item in t.range::<&str>(..)? {
                        ks.push(item?.0.value().to_string());
                    }
                    ks
                };
                n = keys.len();
                for k in keys {
                    t.remove(k.as_str())?;
                }
            }
            {
                // 同事务清除元数据，保证 daily 与 meta 一致。
                let mut m = txn.open_table(META)?;
                m.remove(STATS_META_KEY)?;
            }
            txn.commit()?;
            Ok(n)
        })
    }

    /// 删除 `before`（不含）之前的统计，返回删除条数。
    pub fn prune_stats_before(&self, before: &str) -> anyhow::Result<usize> {
        self.with_db(|db| {
            let txn = db.begin_write()?;
            let n;
            {
                let mut t = txn.open_table(STATS_DAILY)?;
                let keys: Vec<String> = {
                    let mut ks = Vec::new();
                    for item in t.range::<&str>(..before)? {
                        ks.push(item?.0.value().to_string());
                    }
                    ks
                };
                n = keys.len();
                for k in keys {
                    t.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            Ok(n)
        })
    }
}

/// 连续天数计算（对齐 Go `calculateStreaks`）：dates 须按升序。
fn calculate_streaks(dates: &[String]) -> (u32, u32, String) {
    let mut current = 0u32;
    let mut max = 0u32;
    let mut last = String::new();
    let mut prev: Option<NaiveDate> = None;
    for date in dates {
        let day = match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        if last.is_empty() {
            current = 1;
            max = 1;
        } else if let Some(p) = prev {
            if (day - p).num_days() <= 1 {
                current += 1;
            } else {
                current = 1;
            }
        }
        if current > max {
            max = current;
        }
        prev = Some(day);
        last = date.clone();
    }
    (current, max, last)
}

/// 上屏文本字符分类：返回 (中文, 英文)。Stage 1 保留 2 分类，供现有采集单点使用。
pub fn classify_chars(text: &str) -> (u32, u32) {
    let mut chinese = 0u32;
    let mut english = 0u32;
    for ch in text.chars() {
        if is_cjk(ch) {
            chinese += 1;
        } else if ch.is_ascii_alphabetic() {
            english += 1;
        }
    }
    (chinese, english)
}

/// 上屏文本 4 分类：返回 (中文, 英文, 标点, 其他)。对齐 Go `ClassifyChars`。
pub fn classify_chars_full(text: &str) -> (u32, u32, u32, u32) {
    let (mut chinese, mut english, mut punct, mut other) = (0u32, 0u32, 0u32, 0u32);
    for ch in text.chars() {
        if is_cjk(ch) {
            chinese += 1;
        } else if ch.is_ascii_alphabetic() {
            english += 1;
        } else if is_punct_or_symbol(ch) {
            punct += 1;
        } else {
            other += 1;
        }
    }
    (chinese, english, punct, other)
}

/// 标点/符号判定（ASCII 标点 + 常见 CJK/全角标点区段），近似 Go `unicode.IsPunct||IsSymbol`。
fn is_punct_or_symbol(ch: char) -> bool {
    if ch.is_ascii_punctuation() {
        return true;
    }
    matches!(ch as u32,
        0x2000..=0x206F   // 通用标点
        | 0x3000..=0x303F // CJK 符号和标点
        | 0xFE30..=0xFE4F // CJK 兼容形式
        | 0xFF00..=0xFFEF // 全角 ASCII / 半宽形式
    )
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x4E00..=0x9FFF   // CJK 统一表意文字
        | 0x3400..=0x4DBF // 扩展 A
        | 0xF900..=0xFAFF // 兼容表意文字
        | 0x20000..=0x2A6DF // 扩展 B
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_record_and_daily() {
        let path = tmp("wind_stats_daily.redb");
        let s = Store::open(&path).unwrap();
        s.record_stat("2026-06-18", 10, 2).unwrap();
        s.record_stat("2026-06-18", 5, 0).unwrap(); // 累加
        s.record_stat("2026-06-20", 3, 7).unwrap();

        assert_eq!(s.get_daily_stat("2026-06-18").unwrap().total(), 17);
        let range = s.daily_stats("2026-06-18", "2026-06-20").unwrap();
        assert_eq!(range.len(), 2);
        assert_eq!(range[0].0, "2026-06-18");
        assert_eq!(range[1].1.total(), 10);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_summary_and_streak() {
        let path = tmp("wind_stats_summary.redb");
        let s = Store::open(&path).unwrap();
        s.record_stat("2026-06-20", 100, 0).unwrap(); // today
        s.record_stat("2026-06-19", 50, 0).unwrap();
        s.record_stat("2026-06-18", 30, 0).unwrap();
        s.record_stat("2026-06-01", 5, 0).unwrap();

        let sum = s.stats_summary("2026-06-20").unwrap();
        assert_eq!(sum.today, 100);
        assert_eq!(sum.week, 180, "近7日=100+50+30");
        assert_eq!(sum.month, 185, "近30日含06-01");
        assert_eq!(sum.total, 185);
        assert_eq!(sum.streak, 3, "06-18~06-20 连续");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_classify_chars() {
        let (zh, en) = classify_chars("你好abc，123");
        assert_eq!(zh, 2);
        assert_eq!(en, 3);
    }

    // ── Stage 1 新增 ──

    #[test]
    fn test_speed_per_minute() {
        assert_eq!(speed_per_minute(252, 6), 2520);
        assert_eq!(speed_per_minute(252, 120), 126);
        assert_eq!(speed_per_minute(60, 3), 720, "极短时间触发 5s 下限");
        assert_eq!(speed_per_minute(0, 60), 0);
        assert_eq!(speed_per_minute(100, 0), 0);
    }

    #[test]
    fn test_classify_chars_full() {
        let (zh, en, pu, ot) = classify_chars_full("你好abc，123");
        assert_eq!(zh, 2);
        assert_eq!(en, 3);
        assert_eq!(pu, 1, "，为全角标点");
        assert_eq!(ot, 3, "123 计入其他");
    }

    #[test]
    fn test_daily_stats_backward_compat() {
        // 旧库只含 {chinese, english}，新字段应回落默认值。
        let rec: DailyStats = serde_json::from_str(r#"{"chinese":10,"english":2}"#).unwrap();
        assert_eq!(rec.chinese, 10);
        assert_eq!(rec.english, 2);
        assert_eq!(rec.total(), 12);
        assert_eq!(rec.hours, [0u32; 24]);
        assert_eq!(rec.active_seconds, 0);
        assert!(rec.by_schema.is_empty());
    }

    #[test]
    fn test_put_get_daily_full() {
        let path = tmp("wind_stats_put_full.redb");
        let s = Store::open(&path).unwrap();
        let mut stat = DailyStats {
            chinese: 50,
            english: 10,
            punct: 5,
            other: 3,
            commit_count: 20,
            code_len_sum: 80,
            code_len_count: 20,
            active_seconds: 300,
            ..Default::default()
        };
        stat.hours[9] = 30;
        stat.code_len_dist[3] = 12;
        stat.cand_pos_dist[0] = 18;
        stat.by_source = vec![0; CommitSource::COUNT];
        stat.by_source[CommitSource::Candidate.index()] = 60;
        stat.by_schema.insert(
            "wubi86".into(),
            SchemaStats {
                total_chars: 60,
                commit_count: 20,
                ..Default::default()
            },
        );

        s.put_daily_stat("2026-06-20", &stat).unwrap();
        let got = s.get_daily_stat("2026-06-20").unwrap();
        assert_eq!(got, stat, "完整字段往返一致");
        assert_eq!(got.total(), 68);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_meta_put_get() {
        let path = tmp("wind_stats_meta.redb");
        let s = Store::open(&path).unwrap();
        // 默认空
        assert_eq!(s.get_stats_meta().unwrap(), StatsMeta::default());
        let meta = StatsMeta {
            total_chars: 12345,
            first_day: "2026-01-01".into(),
            streak_current: 7,
            streak_max: 30,
            streak_last_day: "2026-06-20".into(),
            max_speed: 420,
        };
        s.put_stats_meta(&meta).unwrap();
        assert_eq!(s.get_stats_meta().unwrap(), meta);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_clear_resets_meta() {
        let path = tmp("wind_stats_clear_meta.redb");
        let s = Store::open(&path).unwrap();
        s.record_stat("2026-06-10", 1, 0).unwrap();
        s.record_stat("2026-06-20", 1, 0).unwrap();
        s.put_stats_meta(&StatsMeta {
            total_chars: 2,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(s.clear_stats().unwrap(), 2);
        assert_eq!(s.daily_stats("2026-01-01", "2026-12-31").unwrap().len(), 0);
        assert_eq!(
            s.get_stats_meta().unwrap(),
            StatsMeta::default(),
            "clear 应同时重置元数据"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_recalculate_stats_meta_after_prune() {
        let path = tmp("wind_stats_recalc.redb");
        let s = Store::open(&path).unwrap();
        for (date, total, active) in [
            ("2026-04-20", 100u32, 60u32),
            ("2026-04-21", 200, 120),
            ("2026-04-23", 300, 60),
        ] {
            let stat = DailyStats {
                chinese: total,
                active_seconds: active,
                ..Default::default()
            };
            s.put_daily_stat(date, &stat).unwrap();
        }

        assert_eq!(s.prune_stats_before("2026-04-21").unwrap(), 1);
        let meta = s.recalculate_stats_meta().unwrap();
        assert_eq!(meta.total_chars, 500);
        assert_eq!(meta.first_day, "2026-04-21");
        assert_eq!(meta.streak_current, 1);
        assert_eq!(meta.streak_max, 1);
        assert_eq!(meta.streak_last_day, "2026-04-23");
        assert_eq!(meta.max_speed, 300, "300字/60s = 300字/分");
        let _ = std::fs::remove_file(&path);
    }
}
