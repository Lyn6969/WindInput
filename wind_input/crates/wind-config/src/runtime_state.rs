//! 运行时状态持久化（state.toml，存于本机状态目录）。
//!
//! 与 Go 版本 `wind_input/pkg/config/runtime_state.go` 对齐。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// 运行时状态（进程退出时保存，启动时恢复）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    /// 上次中文/英文模式。缺字段（旧 state.toml 从未写过）默认 true（中文，与配置默认一致）。
    #[serde(default = "default_true")]
    pub last_chinese_mode: bool,
    /// 上次全角/半角。
    #[serde(default)]
    pub last_full_width: bool,
    /// 上次中/英标点。缺字段（旧 state.toml）默认 true（中文标点，与配置默认一致）。
    #[serde(default = "default_true")]
    pub last_chinese_punct: bool,
    /// 工具栏位置，按显示器 key（"workRight,workBottom"）独立记录。
    #[serde(default)]
    pub toolbar_positions: HashMap<String, (i32, i32)>,
    /// 候选框固定位置（pin_candidate_position 启用时）。
    /// 外层 key = 进程名（小写），内层 key = 显示器 key。
    #[serde(default)]
    pub candidate_pin_positions: HashMap<String, HashMap<String, (i32, i32)>>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            last_chinese_mode: true,
            last_full_width: false,
            last_chinese_punct: true,
            toolbar_positions: HashMap::new(),
            candidate_pin_positions: HashMap::new(),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧 state.toml（无 last_* 三字段）反序列化应落到语义默认：中文/半角/中文标点。
    #[test]
    fn old_state_toml_defaults_to_chinese() {
        let rs: RuntimeState = toml::from_str("[toolbar_positions]\n").unwrap();
        assert!(rs.last_chinese_mode);
        assert!(!rs.last_full_width);
        assert!(rs.last_chinese_punct);
    }

    /// Default 与 serde 缺字段默认一致（load 失败回退 unwrap_or_default 的语义相同）。
    #[test]
    fn default_matches_serde_defaults() {
        let d = RuntimeState::default();
        assert!(d.last_chinese_mode);
        assert!(!d.last_full_width);
        assert!(d.last_chinese_punct);
    }

    /// 三字段 roundtrip。
    #[test]
    fn last_state_roundtrip() {
        let mut rs = RuntimeState::default();
        rs.last_chinese_mode = false;
        rs.last_full_width = true;
        rs.last_chinese_punct = false;
        let s = toml::to_string_pretty(&rs).unwrap();
        let back: RuntimeState = toml::from_str(&s).unwrap();
        assert!(!back.last_chinese_mode);
        assert!(back.last_full_width);
        assert!(!back.last_chinese_punct);
    }
}
