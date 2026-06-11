//! 统计存储
//!
//! 与 Go 版本 `wind_input/internal/store/stats.go` 对齐。

use serde::{Deserialize, Serialize};

/// 每日统计
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DailyStats {
    pub date: String,
    pub total_keys: u32,
    pub chinese_chars: u32,
    pub english_chars: u32,
}

/// 统计收集器
pub struct StatCollector {
    // TODO
}
