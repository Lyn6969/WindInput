//! 频率存储（含异步批处理）
//!
//! 与 Go 版本 `wind_input/internal/store/freq.go` 对齐。

use serde::{Deserialize, Serialize};

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
