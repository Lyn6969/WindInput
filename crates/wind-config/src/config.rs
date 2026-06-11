//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。
//! 配置文件为 TOML 格式，三层合并：默认值 → data/config.toml → %APPDATA%/WindInput/config.toml

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// 完整配置
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_true")]
    pub remember_last_state: bool,
    #[serde(default = "default_true")]
    pub default_chinese_mode: bool,
    #[serde(default)]
    pub default_full_width: bool,
    #[serde(default = "default_true")]
    pub default_chinese_punct: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            remember_last_state: false,
            default_chinese_mode: true,
            default_full_width: false,
            default_chinese_punct: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SchemaConfig {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub available: Vec<String>,
    #[serde(default)]
    pub primary_codetable: String,
    #[serde(default)]
    pub primary_pinyin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HotkeysConfig {
    #[serde(default)]
    pub toggle_mode_keys: Vec<String>,
    #[serde(default = "default_true")]
    pub commit_on_switch: bool,
    #[serde(default)]
    pub switch_engine: String,
    #[serde(default)]
    pub toggle_full_width: String,
    #[serde(default)]
    pub toggle_punct: String,
    #[serde(default)]
    pub toggle_toolbar: String,
    #[serde(default)]
    pub open_settings: String,
    #[serde(default)]
    pub add_word: String,
    #[serde(default)]
    pub toggle_s2t: String,
    #[serde(default)]
    pub activate_ime: String,
    #[serde(default)]
    pub global_hotkeys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    #[serde(default)]
    pub punct_follow_mode: bool,
    #[serde(default = "default_filter_mode")]
    pub filter_mode: String,
    #[serde(default)]
    pub select_key_groups: Vec<String>,
    #[serde(default)]
    pub page_keys: Vec<String>,
    #[serde(default)]
    pub highlight_keys: Vec<String>,
    #[serde(default)]
    pub select_char_keys: Vec<String>,
    #[serde(default = "default_true")]
    pub smart_punct_after_digit: bool,
    #[serde(default = "default_smart_punct_list")]
    pub smart_punct_list: String,
    #[serde(default = "default_enter_behavior")]
    pub enter_behavior: String,
    #[serde(default = "default_space_behavior")]
    pub space_on_empty_behavior: String,
    #[serde(default)]
    pub numpad_behavior: String,
    #[serde(default = "default_pinyin_separator")]
    pub pinyin_separator: String,
    #[serde(default)]
    pub shift_temp_english: ShiftTempEnglishConfig,
    #[serde(default)]
    pub capslock: CapslockConfig,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            punct_follow_mode: false,
            filter_mode: "smart".to_string(),
            select_key_groups: vec!["semicolon_quote".to_string()],
            page_keys: vec!["pageupdown".to_string(), "minus_equal".to_string()],
            highlight_keys: vec!["arrows".to_string(), "tab".to_string()],
            select_char_keys: vec![],
            smart_punct_after_digit: true,
            smart_punct_list: ".,:".to_string(),
            enter_behavior: "commit".to_string(),
            space_on_empty_behavior: "commit".to_string(),
            numpad_behavior: String::new(),
            pinyin_separator: "auto".to_string(),
            shift_temp_english: ShiftTempEnglishConfig::default(),
            capslock: CapslockConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShiftTempEnglishConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub show_english_candidates: bool,
    #[serde(default = "default_shift_behavior")]
    pub shift_behavior: String,
    #[serde(default)]
    pub allow_symbols: bool,
    #[serde(default)]
    pub space_as_input: bool,
}

impl Default for ShiftTempEnglishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_english_candidates: true,
            shift_behavior: "temp_english".to_string(),
            allow_symbols: false,
            space_as_input: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CapslockConfig {
    #[serde(default)]
    pub cancel_on_mode_switch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub font_size: f64,
    #[serde(default)]
    pub per_page: usize,
    #[serde(default)]
    pub theme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub stats: StatsConfig,
    #[serde(default)]
    pub s2t: S2TConfig,
    #[serde(default)]
    pub quick_input: QuickInputConfig,
    #[serde(default)]
    pub cmdbar: CmdbarConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub track_english: bool,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            track_english: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S2TConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub variant: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuickInputConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CmdbarConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompatConfig {
    #[serde(default)]
    pub host_render_processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DebugConfig {
    #[serde(default)]
    pub log_level: String,
    #[serde(default)]
    pub perf_sampling: bool,
}

fn default_true() -> bool {
    true
}

fn default_filter_mode() -> String {
    "smart".to_string()
}

fn default_smart_punct_list() -> String {
    ".,:".to_string()
}

fn default_enter_behavior() -> String {
    "commit".to_string()
}

fn default_space_behavior() -> String {
    "commit".to_string()
}

fn default_pinyin_separator() -> String {
    "auto".to_string()
}

fn default_shift_behavior() -> String {
    "temp_english".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            schema: SchemaConfig::default(),
            hotkeys: HotkeysConfig::default(),
            input: InputConfig::default(),
            ui: UiConfig::default(),
            features: FeaturesConfig::default(),
            compat: CompatConfig::default(),
            debug: DebugConfig::default(),
        }
    }
}

impl Config {
    /// 三层合并加载：默认值 → data_dir/config.toml → 用户配置
    pub fn load(data_dir: Option<&Path>) -> anyhow::Result<Self> {
        let mut config = Self::default();

        // Layer 2: 系统预置配置 (data/config.toml)
        if let Some(data_dir) = data_dir {
            let sys_config = data_dir.join("config.toml");
            if sys_config.exists() {
                config.merge_from_file(&sys_config)?;
                info!("Loaded system config: {}", sys_config.display());
            }
        }

        // Layer 3: 用户配置 (%APPDATA%/WindInput/config.toml)
        if let Some(user_dir) = Self::user_config_dir() {
            let user_config = user_dir.join("config.toml");
            if user_config.exists() {
                config.merge_from_file(&user_config)?;
                info!("Loaded user config: {}", user_config.display());
            }
        }

        Ok(config)
    }

    /// 从 TOML 文件合并配置（部分覆盖）
    fn merge_from_file(&mut self, path: &Path) -> anyhow::Result<()> {
        let content = std::fs::read_to_string(path)?;
        let partial: toml::Value = toml::from_str(&content)?;

        // 逐段合并
        if let Some(general) = partial.get("general") {
            let g: GeneralConfig = general.clone().try_into().unwrap_or_default();
            if general.get("remember_last_state").is_some() {
                self.general.remember_last_state = g.remember_last_state;
            }
            if general.get("default_chinese_mode").is_some() {
                self.general.default_chinese_mode = g.default_chinese_mode;
            }
            if general.get("default_full_width").is_some() {
                self.general.default_full_width = g.default_full_width;
            }
            if general.get("default_chinese_punct").is_some() {
                self.general.default_chinese_punct = g.default_chinese_punct;
            }
        }

        if let Some(schema) = partial.get("schema") {
            let s: SchemaConfig = schema.clone().try_into().unwrap_or_default();
            if schema.get("active").is_some() {
                self.schema.active = s.active;
            }
            if schema.get("available").is_some() {
                self.schema.available = s.available;
            }
        }

        if let Some(hotkeys) = partial.get("hotkeys") {
            let h: HotkeysConfig = hotkeys.clone().try_into().unwrap_or_default();
            if hotkeys.get("toggle_mode_keys").is_some() {
                self.hotkeys.toggle_mode_keys = h.toggle_mode_keys;
            }
            if hotkeys.get("commit_on_switch").is_some() {
                self.hotkeys.commit_on_switch = h.commit_on_switch;
            }
            if hotkeys.get("toggle_full_width").is_some() {
                self.hotkeys.toggle_full_width = h.toggle_full_width;
            }
            if hotkeys.get("toggle_punct").is_some() {
                self.hotkeys.toggle_punct = h.toggle_punct;
            }
        }

        if let Some(input) = partial.get("input") {
            let i: InputConfig = input.clone().try_into().unwrap_or_default();
            if input.get("filter_mode").is_some() {
                self.input.filter_mode = i.filter_mode;
            }
            if input.get("select_key_groups").is_some() {
                self.input.select_key_groups = i.select_key_groups;
            }
            if input.get("page_keys").is_some() {
                self.input.page_keys = i.page_keys;
            }
            if input.get("enter_behavior").is_some() {
                self.input.enter_behavior = i.enter_behavior;
            }
            if input.get("pinyin_separator").is_some() {
                self.input.pinyin_separator = i.pinyin_separator;
            }
        }

        if let Some(ui) = partial.get("ui") {
            if ui.get("font_size").is_some() {
                self.ui.font_size = ui.get("font_size").and_then(|v| v.as_float()).unwrap_or(14.0);
            }
            if ui.get("per_page").is_some() {
                self.ui.per_page = ui.get("per_page").and_then(|v| v.as_integer()).unwrap_or(5) as usize;
            }
            if ui.get("theme").is_some() {
                self.ui.theme = ui.get("theme").and_then(|v| v.as_str()).unwrap_or("").to_string();
            }
        }

        debug!("Config merged from {}", path.display());
        Ok(())
    }

    /// 获取用户配置目录
    pub fn user_config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("WindInput"))
    }

    /// 获取 data 目录（与可执行文件同目录的 data/）
    pub fn data_dir() -> Option<PathBuf> {
        std::env::current_exe().ok().and_then(|p| {
            p.parent().map(|d| d.join("data"))
        })
    }

    /// 获取当前激活的 schema ID
    pub fn active_schema(&self) -> &str {
        if self.schema.active.is_empty() {
            self.schema.available.first().map(|s| s.as_str()).unwrap_or("wubi86")
        } else {
            &self.schema.active
        }
    }
}
