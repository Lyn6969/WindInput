//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。
//! 配置文件为 TOML 格式，三层合并：默认值 → data/config.toml → %APPDATA%/WindInput/config.toml

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::info;

/// 深合并两个 TOML 值：表递归合并（overlay 的键覆盖/新增），标量与数组由 overlay 整体覆盖。
/// 用于配置三层合并——overlay 中未出现的键保留 base 的值。
fn merge_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) => merge_value(bv, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

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
    /// 智能符号模式：同一中文标点在时限内连按两次，删前一字符替换为英文（默认 false）
    #[serde(default)]
    pub smart_symbol_mode: bool,
    /// 智能符号模式判定时限（毫秒，默认 500）
    #[serde(default = "default_smart_symbol_timeout_ms")]
    pub smart_symbol_timeout_ms: i32,
    /// 参与智能符号转换的中文标点集合（子串包含匹配，含成对/多字符标点）
    #[serde(default = "default_smart_symbol_chars")]
    pub smart_symbol_chars: String,
    /// 自定义标点映射（四状态：中半/英全/中全/英半）
    #[serde(default)]
    pub punct_custom: PunctCustomConfig,
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
    #[serde(default)]
    pub url_input: UrlInputConfig,
}

/// 自定义标点映射配置（对齐 Go PunctCustomConfig）。
/// `mappings`: key=源字符（引号用 `"1`/`"2`/`'1`/`'2` 区分左右），
/// value=`[中文半角, 英文全角, 中文全角, 英文半角]`（空串/缺列=回退默认转换）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PunctCustomConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mappings: HashMap<String, Vec<String>>,
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

/// 网址模式配置（对齐 Go UrlInputConfig）。
/// 普通输入累积时，若 `input_buffer + 当前键字符` 恰好等于某前缀，则夺取进入网址模式：
/// 后续可见 ASCII 字符原样累积，空格/回车上屏原文，退格删空退出。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlInputConfig {
    /// 总开关（默认关闭）
    #[serde(default)]
    pub enabled: bool,
    /// 触发前缀（恰好匹配；如 "www." / "http" / "https" / "ftp."）
    #[serde(default = "default_url_prefixes")]
    pub prefixes: Vec<String>,
}

fn default_url_prefixes() -> Vec<String> {
    vec![
        "www.".to_string(),
        "http".to_string(),
        "https".to_string(),
        "ftp.".to_string(),
        "bbs.".to_string(),
    ]
}

impl Default for UrlInputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefixes: default_url_prefixes(),
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
            smart_symbol_mode: false,
            smart_symbol_timeout_ms: default_smart_symbol_timeout_ms(),
            smart_symbol_chars: default_smart_symbol_chars(),
            punct_custom: PunctCustomConfig::default(),
            auto_pair: AutoPairConfig::default(),
            temp_pinyin: TempPinyinConfig::default(),
            enter_behavior: "commit".to_string(),
            space_on_empty_behavior: "commit".to_string(),
            numpad_behavior: String::new(),
            pinyin_separator: "auto".to_string(),
            shift_temp_english: ShiftTempEnglishConfig::default(),
            capslock: CapslockConfig::default(),
            url_input: UrlInputConfig::default(),
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

/// UI 配置（子表结构，对齐真实 config.toml：[ui.candidate] / [ui.font] / [ui.theme]）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub candidate: UiCandidateConfig,
    #[serde(default)]
    pub font: UiFontConfig,
    #[serde(default)]
    pub theme: UiThemeConfig,
}

/// 候选窗配置（[ui.candidate]）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCandidateConfig {
    /// 候选每页显示数（默认 7，对齐 Go 版本）
    #[serde(default = "default_per_page")]
    pub per_page: usize,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub inline_preedit: bool,
    #[serde(default)]
    pub preedit_mode: String,
    #[serde(default)]
    pub hide_window: bool,
}

impl Default for UiCandidateConfig {
    fn default() -> Self {
        Self {
            per_page: default_per_page(),
            layout: String::new(),
            inline_preedit: false,
            preedit_mode: String::new(),
            hide_window: false,
        }
    }
}

/// 字体配置（[ui.font]）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiFontConfig {
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub render_mode: String,
}

/// 主题配置（[ui.theme]）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UiThemeConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub style: String,
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

fn default_smart_symbol_timeout_ms() -> i32 {
    500
}

fn default_smart_symbol_chars() -> String {
    "。，？！：；、～￥·……——".to_string()
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
    /// 三层合并加载：默认值 → data_dir/config.toml → 用户配置。
    ///
    /// 合并方式：把三层各自的 `toml::Value`（默认值序列化得到）深合并（表递归、标量/数组后者覆盖），
    /// 最后一次性反序列化为 `Config`。相比旧的手写逐字段合并，**所有段（含 features/compat/debug）
    /// 都会被合并**，不再静默丢弃；新增配置字段无需改合并代码。
    pub fn load(data_dir: Option<&Path>) -> anyhow::Result<Self> {
        // Layer 1: 代码默认值（序列化为 Value，保证所有字段存在）
        let mut merged = toml::Value::try_from(Self::default())?;

        // Layer 2: 系统预置配置 (data/config.toml)
        if let Some(data_dir) = data_dir {
            let sys_config = data_dir.join("config.toml");
            if let Some(v) = Self::read_toml_value(&sys_config) {
                merge_value(&mut merged, v);
                info!("Loaded system config: {}", sys_config.display());
            }
        }

        // Layer 3: 用户配置 (%APPDATA%/WindInput/config.toml)
        if let Some(user_dir) = Self::user_config_dir() {
            let user_config = user_dir.join("config.toml");
            if let Some(v) = Self::read_toml_value(&user_config) {
                merge_value(&mut merged, v);
                info!("Loaded user config: {}", user_config.display());
            }
        }

        let mut config: Config = merged.try_into()?;
        config.normalize();
        Ok(config)
    }

    /// 读取 TOML 文件为 Value（不存在/解析失败返回 None 并告警，不中断加载）
    fn read_toml_value(path: &Path) -> Option<toml::Value> {
        if !path.exists() {
            return None;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<toml::Value>(&content) {
                Ok(v) => Some(v),
                Err(e) => {
                    info!("Skip invalid config {}: {}", path.display(), e);
                    None
                }
            },
            Err(e) => {
                info!("Cannot read config {}: {}", path.display(), e);
                None
            }
        }
    }

    /// 反序列化后的归一化：修正无效值（如 per_page=0 视为未设置，回退默认）。
    fn normalize(&mut self) {
        if self.ui.candidate.per_page == 0 {
            self.ui.candidate.per_page = default_per_page();
        }
    }

    /// 应用数据目录名：正式版 `WindInput`；调试变体 `WindInputDebug`
    /// （隔离调试与正式版的配置/缓存/日志，与 PIPE_SUFFIX 同源于 debug_variant 特性）。
    pub fn app_dir_name() -> &'static str {
        if cfg!(feature = "debug_variant") {
            "WindInputDebug"
        } else {
            "WindInput"
        }
    }

    /// 获取用户配置目录（漫游 %APPDATA%\<App>）：随用户同步的语言数据
    /// （config.toml / 词频 / shadow 置顶删词 / 主题选择 / 用户词库）。
    pub fn user_config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join(Self::app_dir_name()))
    }

    /// 本机状态目录（%LOCALAPPDATA%\<App>）：不随漫游同步的机器相关数据
    /// （工具栏位置等）。
    pub fn local_dir() -> Option<PathBuf> {
        dirs::data_local_dir().map(|d| d.join(Self::app_dir_name()))
    }

    /// 缓存目录（%LOCALAPPDATA%\WindInput\cache）：词库 .wdb 等可重建产物。
    pub fn cache_dir() -> Option<PathBuf> {
        Self::local_dir().map(|d| d.join("cache"))
    }

    /// 日志目录（%LOCALAPPDATA%\WindInput\logs）。
    pub fn log_dir() -> Option<PathBuf> {
        Self::local_dir().map(|d| d.join("logs"))
    }

    /// 获取 data 目录（与可执行文件同目录的 data/）
    pub fn data_dir() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("data")))
    }

    /// 获取当前激活的 schema ID
    pub fn active_schema(&self) -> &str {
        if self.schema.active.is_empty() {
            self.schema
                .available
                .first()
                .map(|s| s.as_str())
                .unwrap_or("wubi86")
        } else {
            &self.schema.active
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟 load 的合并：默认 Value ← overlay 深合并 → 反序列化 + normalize。
    fn merged_with(overlay_toml: &str) -> Config {
        let mut base = toml::Value::try_from(Config::default()).unwrap();
        let overlay: toml::Value = toml::from_str(overlay_toml).unwrap();
        merge_value(&mut base, overlay);
        let mut cfg: Config = base.try_into().unwrap();
        cfg.normalize();
        cfg
    }

    #[test]
    fn test_default_per_page_is_7() {
        assert_eq!(
            Config::default().ui.candidate.per_page,
            7,
            "候选每页默认应为 7"
        );
    }

    #[test]
    fn test_merge_reads_per_page_from_ui_candidate() {
        let cfg = merged_with("[ui.candidate]\nper_page = 9\n");
        assert_eq!(
            cfg.ui.candidate.per_page, 9,
            "应从 [ui.candidate] 读取 per_page=9"
        );
    }

    #[test]
    fn test_merge_per_page_zero_keeps_default() {
        // per_page=0 视为无效，normalize 回退默认，避免每页只显示 1 个
        let cfg = merged_with("[ui.candidate]\nper_page = 0\n");
        assert_eq!(cfg.ui.candidate.per_page, 7, "per_page=0 应保留默认 7");
    }

    #[test]
    fn test_merge_keeps_features_compat_debug() {
        // 回归：旧手写合并完全丢弃 features/compat/debug 段；deep-merge 必须保留
        let cfg = merged_with(
            "[features.s2t]\nenabled = true\nvariant = \"s2tw\"\n\
             [debug]\nlog_level = \"trace\"\n\
             [compat]\nhost_render_processes = [\"a.exe\"]\n",
        );
        assert!(cfg.features.s2t.enabled, "features.s2t.enabled 应被合并");
        assert_eq!(cfg.features.s2t.variant, "s2tw");
        assert_eq!(cfg.debug.log_level, "trace", "debug 段应被合并");
        assert_eq!(cfg.compat.host_render_processes, vec!["a.exe".to_string()]);
    }

    #[test]
    fn test_merge_partial_keeps_unspecified_default() {
        // overlay 只覆盖单个字段，同段其它字段保留默认（不被清空）
        let cfg = merged_with("[input]\nenter_behavior = \"clear\"\n");
        assert_eq!(cfg.input.enter_behavior, "clear");
        assert_eq!(cfg.input.filter_mode, "smart", "同段未指定字段应保留默认");
    }

    #[test]
    fn test_merge_input_subtable_fields() {
        // 旧合并漏掉的 input 字段（如 smart_punct/auto_pair）现应合并
        let cfg = merged_with("[input.auto_pair]\nchinese = false\nenglish = false\n");
        assert!(!cfg.input.auto_pair.chinese);
        assert!(!cfg.input.auto_pair.english);
    }
}
