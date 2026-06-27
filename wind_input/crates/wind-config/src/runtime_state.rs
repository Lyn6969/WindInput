//! 运行时状态持久化（state.toml，存于本机状态目录）。
//!
//! 与 Go 版本 `wind_input/pkg/config/runtime_state.go` 对齐。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// 运行时状态（进程退出时保存，启动时恢复）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeState {
    /// 上次中文/英文模式。
    #[serde(default)]
    pub last_chinese_mode: bool,
    /// 工具栏位置，按显示器 key（"workRight,workBottom"）独立记录。
    #[serde(default)]
    pub toolbar_positions: HashMap<String, (i32, i32)>,
    /// 候选框固定位置（pin_candidate_position 启用时）。
    /// 外层 key = 进程名（小写），内层 key = 显示器 key。
    #[serde(default)]
    pub candidate_pin_positions: HashMap<String, HashMap<String, (i32, i32)>>,
}

impl RuntimeState {
    /// 从 `state_dir/state.toml` 加载，文件不存在或解析失败时返回默认值。
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("state.toml");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// 原子写入 `state_dir/state.toml`（tmp + rename）。
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let content = toml::to_string_pretty(self)?;
        let tmp = state_dir.join("state.toml.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, state_dir.join("state.toml"))?;
        Ok(())
    }
}
