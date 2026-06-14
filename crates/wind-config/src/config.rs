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
    pub pin_candidate: String,
    #[serde(default)]
    pub delete_candidate: String,
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
    /// 标点配对（输入左括号自动补右括号 + 输右括号智能跳过）
    #[serde(default)]
    pub auto_pair: AutoPairConfig,
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
    #[serde(default)]
    pub temp_pinyin: TempPinyinConfig,
}

/// 标点配对配置（对齐 Go AutoPairConfig）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoPairConfig {
    /// 中文标点配对开关
    #[serde(default = "default_true")]
    pub chinese: bool,
    /// 英文标点配对开关
    #[serde(default = "default_true")]
    pub english: bool,
    /// 中文配对表（每项 2 字符："（）"）
    #[serde(default = "default_chinese_pairs")]
    pub chinese_pairs: Vec<String>,
    /// 英文配对表（每项 2 字符："()"）
    #[serde(default = "default_english_pairs")]
    pub english_pairs: Vec<String>,
}

impl Default for AutoPairConfig {
    fn default() -> Self {
        Self {
            chinese: true,
            english: true,
            chinese_pairs: default_chinese_pairs(),
            english_pairs: default_english_pairs(),
        }
    }
}

fn default_chinese_pairs() -> Vec<String> {
    ["（）", "【】", "｛｝", "《》", "〈〉", "「」", "『』"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_english_pairs() -> Vec<String> {
    ["()", "[]", "{}"].iter().map(|s| s.to_string()).collect()
}

/// 临时拼音配置（码表方案下临时切到拼音反查）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempPinyinConfig {
    /// 触发键（如 "backtick" / "z" / "semicolon"），默认反引号
    #[serde(default = "default_temp_pinyin_triggers")]
    pub trigger_keys: Vec<String>,
}

fn default_temp_pinyin_triggers() -> Vec<String> {
    vec!["backtick".to_string()]
}

impl Default for TempPinyinConfig {
    fn default() -> Self {
        Self {
            trigger_keys: default_temp_pinyin_triggers(),
        }
    }
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
            auto_pair: AutoPairConfig::default(),
            temp_pinyin: TempPinyinConfig::default(),
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

fn default_per_page() -> usize {
    7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub font_size: f64,
    /// 候选每页显示数（默认 7，对齐 Go 版本）
    #[serde(default = "default_per_page")]
    pub per_page: usize,
    #[serde(default)]
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            font_size: 0.0,
            per_page: default_per_page(),
            theme: String::new(),
        }
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickInputConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 触发键（如 "semicolon"），默认分号
    #[serde(default = "default_quick_input_triggers")]
    pub trigger_keys: Vec<String>,
    /// 计算器结果小数位数，默认 6
    #[serde(default = "default_decimal_places")]
    pub decimal_places: i32,
}

fn default_quick_input_triggers() -> Vec<String> {
    vec!["semicolon".to_string()]
}

fn default_decimal_places() -> i32 {
    6
}

impl Default for QuickInputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trigger_keys: default_quick_input_triggers(),
            decimal_places: default_decimal_places(),
        }
    }
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
            // 合并所有热键字段：仅当该 key 出现在文件中才覆盖（否则保留下层值）。
            // 此前只合并了 4 个字段，导致 switch_engine 等被丢弃 → Ctrl+Shift+E 等热键失效。
            if hotkeys.get("toggle_mode_keys").is_some() {
                self.hotkeys.toggle_mode_keys = h.toggle_mode_keys;
            }
            if hotkeys.get("commit_on_switch").is_some() {
                self.hotkeys.commit_on_switch = h.commit_on_switch;
            }
            if hotkeys.get("switch_engine").is_some() {
                self.hotkeys.switch_engine = h.switch_engine;
            }
            if hotkeys.get("toggle_full_width").is_some() {
                self.hotkeys.toggle_full_width = h.toggle_full_width;
            }
            if hotkeys.get("toggle_punct").is_some() {
                self.hotkeys.toggle_punct = h.toggle_punct;
            }
            if hotkeys.get("toggle_toolbar").is_some() {
                self.hotkeys.toggle_toolbar = h.toggle_toolbar;
            }
            if hotkeys.get("open_settings").is_some() {
                self.hotkeys.open_settings = h.open_settings;
            }
            if hotkeys.get("add_word").is_some() {
                self.hotkeys.add_word = h.add_word;
            }
            if hotkeys.get("toggle_s2t").is_some() {
                self.hotkeys.toggle_s2t = h.toggle_s2t;
            }
            if hotkeys.get("activate_ime").is_some() {
                self.hotkeys.activate_ime = h.activate_ime;
            }
            if hotkeys.get("pin_candidate").is_some() {
                self.hotkeys.pin_candidate = h.pin_candidate;
            }
            if hotkeys.get("delete_candidate").is_some() {
                self.hotkeys.delete_candidate = h.delete_candidate;
            }
            if hotkeys.get("global_hotkeys").is_some() {
                self.hotkeys.global_hotkeys = h.global_hotkeys;
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
            if input.get("temp_pinyin").is_some() {
                self.input.temp_pinyin = i.temp_pinyin;
            }
        }

        if let Some(ui) = partial.get("ui") {
            if ui.get("font_size").is_some() {
                self.ui.font_size = ui.get("font_size").and_then(|v| v.as_float()).unwrap_or(14.0);
            }
            // per_page 实际位于 [ui.candidate]（对齐 Go 配置结构），回退兼容 [ui].per_page
            if let Some(pp) = ui
                .get("candidate")
                .and_then(|c| c.get("per_page"))
                .or_else(|| ui.get("per_page"))
                .and_then(|v| v.as_integer())
            {
                if pp > 0 {
                    self.ui.per_page = pp as usize;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_per_page_is_7() {
        assert_eq!(Config::default().ui.per_page, 7, "候选每页默认应为 7");
    }

    #[test]
    fn test_merge_reads_per_page_from_ui_candidate() {
        // per_page 位于 [ui.candidate]（对齐 Go 配置结构）
        let toml = "[ui.candidate]\nper_page = 9\n";
        let dir = std::env::temp_dir();
        let path = dir.join("windinput_test_per_page.toml");
        std::fs::write(&path, toml).unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(cfg.ui.per_page, 9, "应从 [ui.candidate] 读取 per_page=9");
    }

    #[test]
    fn test_merge_per_page_zero_keeps_default() {
        // per_page=0 视为无效，保留默认值，避免每页只显示 1 个
        let toml = "[ui.candidate]\nper_page = 0\n";
        let dir = std::env::temp_dir();
        let path = dir.join("windinput_test_per_page_zero.toml");
        std::fs::write(&path, toml).unwrap();

        let mut cfg = Config::default();
        cfg.merge_from_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(cfg.ui.per_page, 7, "per_page=0 应保留默认 7");
    }
}
