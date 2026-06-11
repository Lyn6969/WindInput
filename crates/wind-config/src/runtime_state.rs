//! 运行时状态持久化
//!
//! 与 Go 版本 `wind_input/pkg/config/runtime_state.go` 对齐。

use serde::{Deserialize, Serialize};

/// 运行时状态（在进程退出时保存，启动时恢复）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    /// 上次中文/英文模式
    #[serde(default)]
    pub last_chinese_mode: bool,
    /// 工具栏位置
    #[serde(default)]
    pub toolbar_positions: std::collections::HashMap<String, (i32, i32)>,
    /// 候选框固定位置
    #[serde(default)]
    pub candidate_pin_positions: std::collections::HashMap<String, (i32, i32)>,
}
