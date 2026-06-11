//! 频率存储（含异步批处理）
//!
//! 与 Go 版本 `wind_input/internal/store/freq.go` 对齐。
//! 运行时词频跟踪 + 异步持久化。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::RwLock;

/// 频率记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FreqRecord {
    pub count: u32,
    pub last_used: String,
    pub streak: u32,
}

/// 频率配置
#[derive(Debug, Clone)]
pub struct FreqProfile {
    pub base_scale: f64,
    pub max_recency: f64,
    pub lambda: f64,
    pub streak_scale: f64,
    pub streak_cap: f64,
    pub boost_max: f64,
}

impl Default for FreqProfile {
    fn default() -> Self {
        Self {
            base_scale: 100.0,
            max_recency: 50.0,
            lambda: 0.1,
            streak_scale: 10.0,
            streak_cap: 200.0,
            boost_max: 500.0,
        }
    }
}

impl FreqProfile {
    /// 计算频率提升值
    pub fn calc_boost(&self, count: u32, age_hours: f64, streak: u32) -> f64 {
        let base = ((count + 1) as f64).log2() * self.base_scale;
        let recency = self.max_recency * (-self.lambda * age_hours).exp();
        let streak_val = (streak as f64 * self.streak_scale).min(self.streak_cap);
        (base + recency + streak_val).min(self.boost_max)
    }
}

/// 运行时词频跟踪器
///
/// 跟踪用户选择的词频，用于实时调整候选排序。
/// 异步批量写入持久化存储。
pub struct FreqTracker {
    /// word -> 选择次数（运行时）
    freq_map: RwLock<HashMap<String, u32>>,
    /// 配置
    profile: FreqProfile,
}

impl FreqTracker {
    pub fn new() -> Self {
        Self {
            freq_map: RwLock::new(HashMap::new()),
            profile: FreqProfile::default(),
        }
    }

    /// 记录一次词选择
    pub fn record_selection(&self, word: &str) {
        let mut map = self.freq_map.write().unwrap();
        let entry = map.entry(word.to_string()).or_insert(0);
        *entry += 1;
    }

    /// 获取词的频率 boost 值
    pub fn get_boost(&self, word: &str) -> f64 {
        let map = self.freq_map.read().unwrap();
        let count = *map.get(word).unwrap_or(&0);
        if count == 0 {
            return 0.0;
        }
        // 简化计算：只用 count，不考虑时间和 streak
        ((count + 1) as f64).log2() * self.profile.base_scale * 0.1
    }

    /// 获取词的选择次数
    pub fn get_count(&self, word: &str) -> u32 {
        let map = self.freq_map.read().unwrap();
        *map.get(word).unwrap_or(&0)
    }

    /// 是否包含某词
    pub fn contains(&self, word: &str) -> bool {
        let map = self.freq_map.read().unwrap();
        map.contains_key(word)
    }

    /// 获取所有频率记录（用于持久化）
    pub fn export_records(&self) -> Vec<(String, u32)> {
        let map = self.freq_map.read().unwrap();
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// 从持久化数据加载
    pub fn import_records(&self, records: &[(String, u32)]) {
        let mut map = self.freq_map.write().unwrap();
        for (word, count) in records {
            map.insert(word.clone(), *count);
        }
    }

    /// 清空运行时频率（用于测试）
    pub fn clear(&self) {
        let mut map = self.freq_map.write().unwrap();
        map.clear();
    }
}
