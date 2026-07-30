//! 输入统计采集器（内存聚合 + 后台定时 flush）
//!
//! 对齐 Go `wind_input/internal/store/stat_collector.go`：
//! - `record()` 把单次上屏事件聚合进当日内存 `DailyStats`，跨天自动 flush 旧日并开新日；
//! - 后台线程每 30s flush 一次；`Drop`（对齐 Go `Close`）停线程并最终 flush；
//! - 活跃时间：两次上屏间隔 < 15s 视为持续输入，累加秒数（用于速度统计）。

use crate::stats::{
    CommitSource, DailyStats, StatsMeta, qualifies_for_max_speed, speed_per_minute,
};
use crate::store::Store;
use chrono::{DateTime, Local, NaiveDate, Timelike};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::warn;

/// 活跃输入判定阈值（秒）：两次上屏间隔小于此值视为持续输入。
const ACTIVE_THRESHOLD_SECS: i64 = 15;
/// 后台 flush 周期（秒）。
const FLUSH_INTERVAL_SECS: u64 = 30;

/// 单次上屏的统计事件（仅元数据，不含原文）。
#[derive(Debug, Clone)]
pub struct StatEvent {
    pub timestamp: DateTime<Local>,
    pub chinese: u32,
    pub english: u32,
    pub punct: u32,
    pub other: u32,
    pub code_len: u32,      // 编码长度（0 = 标点/直接输入）
    pub candidate_pos: i32, // 候选位置：0=首选 … -1=非候选
    pub schema_id: String,
    pub source: CommitSource,
}

impl StatEvent {
    /// 本次事件的总字符数（各分类之和）。
    pub fn rune_count(&self) -> u32 {
        self.chinese
            .saturating_add(self.english)
            .saturating_add(self.punct)
            .saturating_add(self.other)
    }
}

impl Default for StatEvent {
    fn default() -> Self {
        Self {
            timestamp: Local::now(),
            chinese: 0,
            english: 0,
            punct: 0,
            other: 0,
            code_len: 0,
            candidate_pos: -1,
            schema_id: String::new(),
            source: CommitSource::default(),
        }
    }
}

/// 内存聚合状态（受 `Shared.inner` 互斥锁保护）。
struct Inner {
    today: DailyStats,
    today_date: String,
    meta: StatsMeta,
    dirty: bool,
    last_commit: Option<DateTime<Local>>,
}

/// 采集器与后台线程共享的状态。
struct Shared {
    store: Arc<Store>,
    inner: Mutex<Inner>,
}

/// 输入统计采集器。
pub struct StatCollector {
    shared: Arc<Shared>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    handle: Option<JoinHandle<()>>,
}

fn today_string() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

impl StatCollector {
    /// 创建采集器：加载当日数据与元数据，启动后台定时 flush 线程。
    pub fn new(store: Arc<Store>) -> Self {
        let today = today_string();
        let mut inner = Inner {
            today: store.get_daily_stat(&today).unwrap_or_default(),
            today_date: today.clone(),
            meta: store.get_stats_meta().unwrap_or_default(),
            dirty: false,
            last_commit: None,
        };
        if inner.meta.first_day.is_empty() {
            inner.meta.first_day = today;
        }
        let shared = Arc::new(Shared {
            store,
            inner: Mutex::new(inner),
        });
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let handle = {
            let sh = shared.clone();
            let st = stop.clone();
            thread::spawn(move || background_loop(sh, st))
        };
        Self {
            shared,
            stop,
            handle: Some(handle),
        }
    }

    /// 记录一次上屏事件。
    pub fn record(&self, event: StatEvent) {
        self.shared.record(event);
    }

    /// 将内存数据持久化（线程安全）。
    pub fn flush(&self) {
        self.shared.flush();
    }

    /// 返回当日统计快照（跨天则先 flush 旧日）。
    pub fn get_today_stat(&self) -> DailyStats {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_string();
        if inner.today_date != today {
            self.shared.flush_locked(&mut inner);
            inner.today = DailyStats::default();
            inner.today_date = today;
        }
        inner.today.clone()
    }

    /// 返回元数据快照。
    pub fn get_meta(&self) -> StatsMeta {
        self.shared
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .meta
            .clone()
    }

    /// 清空内存统计（配合 Store::clear_stats）。
    pub fn reset(&self) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_string();
        inner.today = DailyStats::default();
        inner.today_date = today.clone();
        inner.meta = StatsMeta {
            first_day: today,
            ..Default::default()
        };
        inner.dirty = false;
        inner.last_commit = None;
    }

    /// 暂停：flush 后由调用方释放 store（Windows 热替换）。
    pub fn pause(&self) {
        self.flush();
    }

    /// 恢复：重新从 store 加载当日与元数据。
    pub fn resume(&self) {
        let mut inner = self.shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_string();
        inner.today = self.shared.store.get_daily_stat(&today).unwrap_or_default();
        inner.today_date = today.clone();
        inner.meta = self.shared.store.get_stats_meta().unwrap_or_default();
        if inner.meta.first_day.is_empty() {
            inner.meta.first_day = today;
        }
        inner.dirty = false;
        inner.last_commit = None;
    }
}

impl Drop for StatCollector {
    fn drop(&mut self) {
        {
            let (lock, cv) = &*self.stop;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // 最终 flush（对齐 Go Close），确保退出时未落库数据持久化。
        self.shared.flush();
    }
}

impl Shared {
    fn record(&self, event: StatEvent) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let today = today_string();
        if inner.today_date != today {
            self.flush_locked(&mut inner);
            inner.today = DailyStats::default();
            inner.today_date = today;
        }

        let rune = event.rune_count();
        {
            let d = &mut inner.today;
            d.chinese = d.chinese.saturating_add(event.chinese);
            d.english = d.english.saturating_add(event.english);
            d.punct = d.punct.saturating_add(event.punct);
            d.other = d.other.saturating_add(event.other);
            d.commit_count = d.commit_count.saturating_add(1);

            let hour = event.timestamp.hour() as usize;
            if hour < 24 {
                d.hours[hour] = d.hours[hour].saturating_add(rune);
            }

            // 码长统计（仅候选词上屏且 code_len > 0）
            if event.code_len > 0 {
                d.code_len_sum = d.code_len_sum.saturating_add(event.code_len);
                d.code_len_count = d.code_len_count.saturating_add(1);
                let idx = (event.code_len - 1).min(5) as usize;
                d.code_len_dist[idx] = d.code_len_dist[idx].saturating_add(1);
            }
            // 选重统计（仅候选词上屏）
            if event.candidate_pos >= 0 {
                let idx = event.candidate_pos.min(4) as usize;
                d.cand_pos_dist[idx] = d.cand_pos_dist[idx].saturating_add(1);
            }
            // 按方案统计
            if !event.schema_id.is_empty() {
                let ss = d.by_schema.entry(event.schema_id.clone()).or_default();
                ss.total_chars = ss.total_chars.saturating_add(rune);
                ss.commit_count = ss.commit_count.saturating_add(1);
                if event.code_len > 0 {
                    ss.code_len_sum = ss.code_len_sum.saturating_add(event.code_len);
                    ss.code_len_count = ss.code_len_count.saturating_add(1);
                }
                if event.candidate_pos >= 0 {
                    let idx = event.candidate_pos.min(4) as usize;
                    ss.cand_pos_dist[idx] = ss.cand_pos_dist[idx].saturating_add(1);
                }
            }
            // 按来源统计
            if d.by_source.len() < CommitSource::COUNT {
                d.by_source.resize(CommitSource::COUNT, 0);
            }
            let si = event.source.index();
            d.by_source[si] = d.by_source[si].saturating_add(rune);
        }

        // 活跃时间：与上次上屏间隔 < 阈值则累加
        if let Some(last) = inner.last_commit {
            let dt = (event.timestamp - last).num_seconds();
            if (0..ACTIVE_THRESHOLD_SECS).contains(&dt) {
                inner.today.active_seconds = inner.today.active_seconds.saturating_add(dt as u32);
            }
        }
        inner.last_commit = Some(event.timestamp);
        inner.meta.total_chars = inner.meta.total_chars.saturating_add(rune as u64);
        inner.dirty = true;
    }

    fn flush(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.flush_locked(&mut inner);
    }

    fn flush_locked(&self, inner: &mut Inner) {
        if !inner.dirty || self.store.is_paused() {
            return;
        }
        if let Err(e) = self.store.put_daily_stat(&inner.today_date, &inner.today) {
            warn!("flush DailyStats failed: {}", e);
            return;
        }
        update_streak(&mut inner.meta, &inner.today_date);
        // flush 每 30 秒一次，当天开头几次的活跃秒数还是个位数——不设门槛就会把
        // 外推出来的上千字/分永久写进历史最快（见 qualifies_for_max_speed）。
        let (total, active) = (inner.today.total(), inner.today.active_seconds);
        if qualifies_for_max_speed(total, active) {
            let sp = speed_per_minute(total, active);
            if sp > inner.meta.max_speed {
                inner.meta.max_speed = sp;
            }
        }
        if let Err(e) = self.store.put_stats_meta(&inner.meta) {
            warn!("flush StatsMeta failed: {}", e);
            return;
        }
        inner.dirty = false;
    }
}

/// 连续天数更新（对齐 Go `updateStreak`）。
fn update_streak(meta: &mut StatsMeta, today: &str) {
    if meta.streak_last_day.is_empty() {
        meta.streak_current = 1;
        meta.streak_last_day = today.to_string();
        if meta.streak_max < 1 {
            meta.streak_max = 1;
        }
        return;
    }
    if meta.streak_last_day == today {
        return;
    }
    let last = NaiveDate::parse_from_str(&meta.streak_last_day, "%Y-%m-%d");
    let td = NaiveDate::parse_from_str(today, "%Y-%m-%d");
    match (last, td) {
        (Ok(last), Ok(td)) => {
            if (td - last).num_days() <= 1 {
                meta.streak_current += 1;
            } else {
                meta.streak_current = 1;
            }
            meta.streak_last_day = today.to_string();
            if meta.streak_current > meta.streak_max {
                meta.streak_max = meta.streak_current;
            }
        }
        _ => {
            meta.streak_current = 1;
            meta.streak_last_day = today.to_string();
        }
    }
}

fn background_loop(shared: Arc<Shared>, stop: Arc<(Mutex<bool>, Condvar)>) {
    let (lock, cv) = &*stop;
    loop {
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (stopped, _) = cv
            .wait_timeout_while(guard, Duration::from_secs(FLUSH_INTERVAL_SECS), |s| !*s)
            .unwrap_or_else(|e| e.into_inner());
        if *stopped {
            break;
        }
        drop(stopped);
        shared.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> Arc<Store> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static CNT: AtomicU32 = AtomicU32::new(0);
        let n = CNT.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("wind_collector_{}_{}.redb", std::process::id(), n));
        let _ = std::fs::remove_file(&p);
        Arc::new(Store::open(&p).unwrap())
    }

    #[test]
    fn test_record_aggregates_all_dimensions() {
        let sc = StatCollector::new(mem_store());
        sc.record(StatEvent {
            chinese: 5,
            code_len: 2,
            candidate_pos: 0,
            source: CommitSource::Candidate,
            schema_id: "wubi86".into(),
            ..Default::default()
        });

        let today = sc.get_today_stat();
        assert_eq!(today.total(), 5);
        assert_eq!(today.commit_count, 1);
        assert_eq!(today.code_len_sum, 2);
        assert_eq!(today.code_len_count, 1);
        assert_eq!(today.cand_pos_dist[0], 1);
        assert_eq!(today.by_source[CommitSource::Candidate.index()], 5);
        let ss = today.by_schema.get("wubi86").expect("schema stats");
        assert_eq!(ss.total_chars, 5);
        assert_eq!(ss.commit_count, 1);
        assert_eq!(sc.get_meta().total_chars, 5);
    }

    #[test]
    fn test_cross_day_flushes_old_and_starts_new() {
        let store = mem_store();
        let sc = StatCollector::new(store.clone());
        {
            let mut inner = sc.shared.inner.lock().unwrap();
            inner.today_date = "2026-01-01".into();
            inner.today.chinese = 100;
            inner.dirty = true;
        }
        sc.record(StatEvent {
            chinese: 3,
            source: CommitSource::Candidate,
            ..Default::default()
        });

        // 旧日落库
        let old = store.get_daily_stat("2026-01-01").unwrap();
        assert_eq!(old.total(), 100);
        // 新日只含新记录
        let today = sc.get_today_stat();
        assert_ne!(sc.shared.inner.lock().unwrap().today_date, "2026-01-01");
        assert_eq!(today.total(), 3);
        // 旧日 flush 时更新了 streak
        assert_eq!(sc.get_meta().streak_last_day, "2026-01-01");
    }

    #[test]
    fn test_active_time_within_threshold() {
        let sc = StatCollector::new(mem_store());
        let base = Local::now();
        sc.record(StatEvent {
            timestamp: base,
            chinese: 1,
            source: CommitSource::Candidate,
            ..Default::default()
        });
        sc.record(StatEvent {
            timestamp: base + chrono::Duration::seconds(10),
            chinese: 1,
            source: CommitSource::Candidate,
            ..Default::default()
        });
        sc.record(StatEvent {
            timestamp: base + chrono::Duration::seconds(100),
            chinese: 1,
            source: CommitSource::Candidate,
            ..Default::default()
        });
        assert_eq!(
            sc.get_today_stat().active_seconds,
            10,
            "仅 10s 间隔计入活跃"
        );
    }

    #[test]
    fn test_streak_increment_and_reset() {
        let sc = StatCollector::new(mem_store());

        let flush_day = |date: &str| {
            let mut inner = sc.shared.inner.lock().unwrap();
            inner.today_date = date.into();
            inner.today.chinese = 10;
            inner.dirty = true;
            sc.shared.flush_locked(&mut inner);
        };

        flush_day("2026-04-01");
        let m = sc.get_meta();
        assert_eq!(m.streak_current, 1);
        assert_eq!(m.streak_last_day, "2026-04-01");

        flush_day("2026-04-02");
        let m = sc.get_meta();
        assert_eq!(m.streak_current, 2);
        assert_eq!(m.streak_max, 2);

        flush_day("2026-04-04");
        let m = sc.get_meta();
        assert_eq!(m.streak_current, 1, "间隔后重置");
        assert_eq!(m.streak_max, 2, "max 保留");
    }

    #[test]
    fn test_drop_flushes_pending() {
        let store = mem_store();
        {
            let sc = StatCollector::new(store.clone());
            sc.record(StatEvent {
                chinese: 7,
                source: CommitSource::Candidate,
                ..Default::default()
            });
        } // Drop 应 flush
        let today = store.get_daily_stat(&today_string()).unwrap();
        assert_eq!(today.total(), 7, "Drop 时应持久化未落库数据");
    }
}
