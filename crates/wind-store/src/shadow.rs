//! Shadow 规则存储
//!
//! 与 Go 版本 `wind_input/internal/store/shadow.go` 对齐。

use serde::{Deserialize, Serialize};

/// Shadow 记录
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShadowRecord {
    #[serde(default)]
    pub pinned: Vec<ShadowPin>,
    #[serde(default)]
    pub deleted: Vec<ShadowDelete>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowPin {
    pub word: String,
    pub cand_id: String,
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowDelete {
    pub word: String,
    pub cand_id: String,
}
