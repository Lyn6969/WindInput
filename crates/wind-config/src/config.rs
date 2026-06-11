//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 完整配置
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,
    #[serde(default)]
    pub schema: SchemaConfig,
    #[serde(default)]
    pub hotkeys: HotkeysConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub compat: CompatConfig,
    #[serde(default)]
    pub debug: DebugConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeneralConfig {
    #[serde(default = "default_true")]
    pub startup_chinese: bool,
    #[serde(default)]
    pub startup_full_width: bool,
    #[serde(default = "default_true")]
    pub startup_chinese_punct: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchemaConfig {
    #[serde(default)]
    pub active: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HotkeysConfig {
    #[serde(default)]
    pub toggle_chinese: String,
    #[serde(default)]
    pub toggle_full_width: String,
    #[serde(default)]
    pub toggle_punct: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InputConfig {
    #[serde(default = "default_true")]
    pub chinese_punct: bool,
    #[serde(default)]
    pub select_keys: Vec<String>,
    #[serde(default)]
    pub page_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub font_size: f64,
    #[serde(default)]
    pub per_page: usize,
    #[serde(default)]
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeConfig {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeaturesConfig {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompatConfig {
    #[serde(default)]
    pub host_render_processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DebugConfig {
    #[serde(default)]
    pub log_level: String,
}

impl Config {
    /// 从文件加载配置（三层合并）
    pub fn load() -> anyhow::Result<Self> {
        let config = Self::default();
        // TODO: 加载系统配置 + 用户配置并合并
        Ok(config)
    }

    /// 获取用户配置目录
    pub fn user_config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("WindInput"))
    }
}
