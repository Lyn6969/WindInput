//! 用户词存储
//!
//! 与 Go 版本 `wind_input/internal/store/user_words.go` 对齐。

use serde::{Deserialize, Serialize};

/// 用户词记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserWordRecord {
    pub text: String,
    pub weight: i32,
    pub count: u32,
    pub created_at: String,
}
