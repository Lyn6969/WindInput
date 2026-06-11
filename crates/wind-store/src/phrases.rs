//! 短语存储
//!
//! 与 Go 版本 `wind_input/internal/store/phrases.go` 对齐。

use serde::{Deserialize, Serialize};

/// 短语记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhraseRecord {
    pub text: String,
    pub weight: i32,
    pub position: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub is_system: bool,
}

fn default_true() -> bool {
    true
}
