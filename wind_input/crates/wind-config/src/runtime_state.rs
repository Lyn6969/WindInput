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
    /// 语言栏图标：标点角标的编码方式（`BadgeShape` 的稳定 id，如 `"corner_triangle"`；
    /// `"none"` = 不显示角标）。
    ///
    /// ⚠ **三个 langbar_icon_* 字段一律用 `Option`，`None` 表示「用代码默认」。**
    /// 本 crate 不重复声明这些默认值：它们的唯一出处是 `wind_ui::langbar_icon` 里的
    /// 构造函数与 `#[default]`，在这里再写一份，改默认时必然漏掉一处，
    /// 而症状是「新装的机器和用过的机器表现不一样」——极难联想到是两份默认值对不上。
    ///
    /// 存 id 而不是下标：下标是位置身份，会被枚举的声明顺序绑架，见 `BadgeShape::as_id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langbar_icon_shape: Option<String>,
    /// 语言栏图标：角标是否彩色（关 = 与主字同色、跟随明暗主题）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langbar_icon_colored: Option<bool>,
    /// 语言栏图标：是否在各尺寸档位图上烧尺寸标记（调试用，见设计文档「验证设计」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub langbar_icon_size_marks: Option<bool>,
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
            langbar_icon_shape: None,
            langbar_icon_colored: None,
            langbar_icon_size_marks: None,
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

    /// 语言栏图标三项 roundtrip，且**未设置时不得出现在文件里**。
    ///
    /// 后半条是要害：`None` 的语义是「用代码默认」，一旦被序列化成某个具体值写进
    /// state.toml，这台机器就此被钉死在写入当天的默认上——之后改代码默认对它无效，
    /// 表现为「新机器和老机器不一样」。toml 也确实不能表达 None，漏掉
    /// skip_serializing_if 会直接让整个 save 失败（连工具栏位置一起丢）。
    #[test]
    fn langbar_icon_prefs_roundtrip_and_omit_when_unset() {
        let empty = toml::to_string_pretty(&RuntimeState::default()).unwrap();
        assert!(
            !empty.contains("langbar_icon"),
            "未设置的语言栏图标偏好被写进了文件:\n{empty}"
        );

        let rs = RuntimeState {
            langbar_icon_shape: Some("outer_ring".into()),
            langbar_icon_colored: Some(false),
            langbar_icon_size_marks: Some(true),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&rs).unwrap();
        let back: RuntimeState = toml::from_str(&s).unwrap();
        assert_eq!(back.langbar_icon_shape.as_deref(), Some("outer_ring"));
        assert_eq!(back.langbar_icon_colored, Some(false));
        assert_eq!(back.langbar_icon_size_marks, Some(true));
    }

    /// 三字段 roundtrip。
    #[test]
    fn last_state_roundtrip() {
        let rs = RuntimeState {
            last_chinese_mode: false,
            last_full_width: true,
            last_chinese_punct: false,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&rs).unwrap();
        let back: RuntimeState = toml::from_str(&s).unwrap();
        assert!(!back.last_chinese_mode);
        assert!(back.last_full_width);
        assert!(!back.last_chinese_punct);
    }
}
