//! 应用兼容性规则
//!
//! 与 Go 版本 `wind_input/pkg/config/app_compat.go` 对齐。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 应用兼容性配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppCompat {
    #[serde(default)]
    pub rules: HashMap<String, AppCompatRule>,
}

/// 单个应用的兼容性规则
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppCompatRule {
    #[serde(default)]
    pub host_render: bool,
    #[serde(default)]
    pub force_chinese_punct: Option<bool>,
}
