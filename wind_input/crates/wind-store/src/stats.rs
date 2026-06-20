//! 输入统计存储（redb，按日期键）
//!
//! 与 Go 版本 `wind_input/internal/store/stats.go` 对齐，但**精简到设置页所需**：
//! 前端契约（WindInputSetting models.ts）只消费 `StatsSummary{today,week,month,total,streak}`
//! 与 `DailyStat{date,count}`，故此处只持久化每日 {中文字数, 英文字数}，不保留 Go 的
//! 24 小时分布 / 码长分布 / 按方案分类（引擎本体不再需要它们）。
//!
//! key = "YYYY-MM-DD"（STATS_DAILY 表），value = DailyStats 的 JSON（每日一条、写入低频）。

use crate::store::{Store, STATS_DAILY};
use chrono::{Duration, NaiveDate};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};

/// 单日统计：中文/英文上屏字符数（count = 两者之和）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DailyStats {
    #[serde(default)]
    pub chinese: u32,
    #[serde(default)]
    pub english: u32,
}

impl DailyStats {
    pub fn total(&self) -> u32 {
        self.chinese.saturating_add(self.english)
    }
}

/// 统计摘要，对齐前端 StatsSummary。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StatsSummary {
    pub today: u32,
    pub week: u32,
    pub month: u32,
    pub total: u64,
    pub streak: u32,
}

impl Store {
    /// 累加某日的中文/英文上屏字符数（读改写）。
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

    /// 统计摘要：today/week(近7日)/month(近30日)/total/streak（连续天数）。
    /// `today` 由调用方传入（YYYY-MM-DD），保持 store 纯净可测。
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

        // 连续天数：从今天（若今天无数据则从昨天）往前数有记录的天数。
        if let Some(td) = today_date {
            let mut cur = if have.contains(today) { td } else { td - Duration::days(1) };
            while have.contains(&cur.format("%Y-%m-%d").to_string()) {
                s.streak += 1;
                cur -= Duration::days(1);
            }
        }
        Ok(s)
    }

    /// 清空所有统计，返回删除的天数。
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

/// 上屏文本字符分类：返回 (中文, 英文)。标点/数字/其他不计入字数统计。
/// 与 Go ClassifyChars 简化对齐：CJK 统一表意文字记中文，ASCII 字母记英文。
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
        // 连续 3 天 + 1 个更早的孤立天
        s.record_stat("2026-06-20", 100, 0).unwrap(); // today
        s.record_stat("2026-06-19", 50, 0).unwrap();
        s.record_stat("2026-06-18", 30, 0).unwrap();
        s.record_stat("2026-06-01", 5, 0).unwrap(); // 早于一周/一月内但中断 streak

        let sum = s.stats_summary("2026-06-20").unwrap();
        assert_eq!(sum.today, 100);
        assert_eq!(sum.week, 180, "近7日=100+50+30");
        assert_eq!(sum.month, 185, "近30日含06-01");
        assert_eq!(sum.total, 185);
        assert_eq!(sum.streak, 3, "06-18~06-20 连续");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_clear_and_prune() {
        let path = tmp("wind_stats_prune.redb");
        let s = Store::open(&path).unwrap();
        s.record_stat("2026-06-10", 1, 0).unwrap();
        s.record_stat("2026-06-15", 1, 0).unwrap();
        s.record_stat("2026-06-20", 1, 0).unwrap();
        // prune 06-15 之前 → 删 06-10
        assert_eq!(s.prune_stats_before("2026-06-15").unwrap(), 1);
        assert_eq!(s.daily_stats("2026-01-01", "2026-12-31").unwrap().len(), 2);
        // clear 全删
        assert_eq!(s.clear_stats().unwrap(), 2);
        assert_eq!(s.daily_stats("2026-01-01", "2026-12-31").unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_classify_chars() {
        let (zh, en) = classify_chars("你好abc，123");
        assert_eq!(zh, 2);
        assert_eq!(en, 3);
    }
}
