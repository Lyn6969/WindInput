//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。
//! 配置文件为 TOML 格式，三层合并：默认值 → data/config.toml → %APPDATA%/WindInput/config.toml

use anyhow::Context;
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

/// 在 TOML 表里按 `path` 导航（缺失则创建嵌套表），把叶子设为 `value`。
/// 路径中途若遇非表值（类型冲突）则覆盖为表。供 [`Config::set_user_value`] 部分合并用。
fn set_nested(table: &mut toml::Table, path: &[&str], value: toml::Value) {
    if path.len() == 1 {
        table.insert(path[0].to_string(), value);
        return;
    }
    let entry = table
        .entry(path[0].to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    match entry {
        toml::Value::Table(t) => set_nested(t, &path[1..], value),
        other => {
            let mut t = toml::Table::new();
            set_nested(&mut t, &path[1..], value);
            *other = toml::Value::Table(t);
        }
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
    #[serde(default)]
    pub pinyin: PinyinGlobalConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeysConfig {
    #[serde(default = "default_toggle_mode_keys")]
    pub toggle_mode_keys: Vec<String>,
    #[serde(default = "default_true")]
    pub commit_on_switch: bool,
    #[serde(default = "default_switch_engine")]
    pub switch_engine: String,
    #[serde(default = "default_toggle_full_width")]
    pub toggle_full_width: String,
    #[serde(default = "default_toggle_punct")]
    pub toggle_punct: String,
    #[serde(default = "default_hotkey_none")]
    pub toggle_toolbar: String,
    #[serde(default = "default_hotkey_none")]
    pub open_settings: String,
    #[serde(default = "default_add_word")]
    pub add_word: String,
    #[serde(default = "default_toggle_s2t")]
    pub toggle_s2t: String,
    #[serde(default = "default_activate_ime")]
    pub activate_ime: String,
    #[serde(default = "default_pin_candidate")]
    pub pin_candidate: String,
    #[serde(default = "default_delete_candidate")]
    pub delete_candidate: String,
    #[serde(default)]
    pub global_hotkeys: Vec<String>,
}

// 热键默认值对齐 Go 版 DefaultConfig.Hotkeys（wind_input/pkg/config/config.go）。
// 关键：config.getDefaults 走 toml::from_str("")，[hotkeys] 整表缺失时用 Default::default()，
// 故必须手写 Default（而非 derive 的空值），否则设置页"开关后默认键丢失"(#4)。
fn default_toggle_mode_keys() -> Vec<String> {
    vec!["lshift".to_string(), "rshift".to_string()]
}
fn default_switch_engine() -> String {
    "ctrl+shift+e".to_string()
}
fn default_toggle_full_width() -> String {
    "shift+space".to_string()
}
fn default_toggle_punct() -> String {
    "ctrl+.".to_string()
}
fn default_add_word() -> String {
    "ctrl+equal".to_string()
}
fn default_toggle_s2t() -> String {
    "ctrl+shift+j".to_string()
}
fn default_activate_ime() -> String {
    "ctrl+shift+[".to_string()
}
fn default_pin_candidate() -> String {
    "ctrl+number".to_string()
}
fn default_delete_candidate() -> String {
    "ctrl+shift+number".to_string()
}
fn default_hotkey_none() -> String {
    "none".to_string()
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self {
            toggle_mode_keys: default_toggle_mode_keys(),
            commit_on_switch: true,
            switch_engine: default_switch_engine(),
            toggle_full_width: default_toggle_full_width(),
            toggle_punct: default_toggle_punct(),
            toggle_toolbar: default_hotkey_none(),
            open_settings: default_hotkey_none(),
            add_word: default_add_word(),
            toggle_s2t: default_toggle_s2t(),
            activate_ime: default_activate_ime(),
            pin_candidate: default_pin_candidate(),
            delete_candidate: default_delete_candidate(),
            global_hotkeys: Vec::new(),
        }
    }
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
    /// 全码/空码上屏策略的全局默认（方案级 [engine.codetable] 的 tri-state 字段未设时回退至此）
    #[serde(default)]
    pub code_commit: CodeCommitConfig,
    /// 短语（含命令栏 $CC/$SS/$AA）前缀列举配置
    #[serde(default)]
    pub phrase: PhraseConfig,
}

/// 短语前缀列举配置（对齐 Go `input.phrase`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhraseConfig {
    /// 触发前缀导航列举的最小输入长度（敲 `zz`/`co` 等前缀列出匹配短语；
    /// < 该长度不列举，避免单字符噪音）。默认 2。
    #[serde(default = "default_phrase_min_prefix")]
    pub min_prefix_length: usize,
    /// 短语/命令候选显示文本的最大字符数（超出截断加省略号，换行/制表统一转空格）。
    /// 防 `clip()`/`last()` 等注入超长/多行内容把候选列撑爆（如 coad 的 `{clip()}`）。
    /// 0 表示不限制。默认 30。
    #[serde(default = "default_phrase_max_display_chars")]
    pub max_display_chars: usize,
}

impl Default for PhraseConfig {
    fn default() -> Self {
        Self {
            min_prefix_length: default_phrase_min_prefix(),
            max_display_chars: default_phrase_max_display_chars(),
        }
    }
}

fn default_phrase_min_prefix() -> usize {
    2
}

fn default_phrase_max_display_chars() -> usize {
    30
}

/// 全码/空码上屏策略全局默认（对齐方案级 [engine.codetable] 同名字段）。
/// 解析顺序：方案级 Some > 本全局 > 内置默认。放在 `config.toml`（用户可合并），
/// 使非主方案/未单独配置的方案统一吃全局，且无需改只读安装目录的 schema。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodeCommitConfig {
    /// 全码自动上屏（唯一精确 + 无更长后继时直接上屏）
    #[serde(default)]
    pub auto_commit_at_full: bool,
    /// 自动上屏最短码长（0 跟随方案 max_code_length）
    #[serde(default)]
    pub auto_commit_min_len: usize,
    /// 满码无候选时清空缓冲
    #[serde(default)]
    pub clear_on_empty_max: bool,
    /// 超过满码长时取前 N 码顶字上屏
    #[serde(default)]
    pub top_code_commit: bool,
    /// 混输全码上屏时，存在拼音候选则否决（保护拼音用户）
    #[serde(default = "default_true")]
    pub auto_commit_block_on_pinyin: bool,
}

impl Default for CodeCommitConfig {
    fn default() -> Self {
        Self {
            auto_commit_at_full: false,
            auto_commit_min_len: 0,
            clear_on_empty_max: false,
            top_code_commit: false,
            auto_commit_block_on_pinyin: true,
        }
    }
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
            code_commit: CodeCommitConfig::default(),
            phrase: PhraseConfig::default(),
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
    /// 触发键（符号键进入临时英文模式，类似临时拼音触发键）。默认空（仅 Shift+字母触发）。
    /// 对齐 Go ShiftTempEnglishConfig.TriggerKeys；设置页 TriggerKeySelect 需此数组存在。
    #[serde(default)]
    pub trigger_keys: Vec<String>,
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
            trigger_keys: Vec::new(),
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
    #[serde(default)]
    pub mode_indicator: ModeIndicatorConfig,
    #[serde(default)]
    pub tooltip: TooltipConfig,
    #[serde(default)]
    pub status_indicator: StatusIndicatorConfig,
    #[serde(default)]
    pub toolbar: ToolbarConfig,
}

/// 工具栏配置（[ui.toolbar]，对齐 Go）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolbarConfig {
    /// 是否显示常驻工具栏（启动初值；运行时可经菜单切换）。
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 前台应用全屏时自动隐藏工具栏（默认 true）。
    #[serde(default = "default_true")]
    pub hide_in_fullscreen: bool,
}

impl Default for ToolbarConfig {
    fn default() -> Self {
        Self {
            visible: true,
            hide_in_fullscreen: true,
        }
    }
}

/// 状态提示气泡配置（[ui.status_indicator]，对齐 Go）：中英/标点/全半角/方案切换的瞬时气泡。
/// 样式（字号/透明度/圆角/配色）跟随主题（theme.views.status）；此处为行为与位置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusIndicatorConfig {
    /// 是否启用状态提示气泡（false=完全不显示）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 自动隐藏时长（毫秒）；display_mode="always" 时忽略（常驻不隐藏）。
    #[serde(default = "default_status_duration")]
    pub duration: i32,
    /// 显示模式："temp"（临时,duration 后隐藏,默认）| "always"（常驻:激活/获焦时显示,失焦隐藏）。
    /// 对齐 Go ui.status_indicator.display_mode。
    #[serde(default = "default_status_display_mode")]
    pub display_mode: String,
    /// 方案名显示样式："full"（全名，默认）| "short"（图标短称 icon_label，回退全名）。
    #[serde(default = "default_schema_name_style")]
    pub schema_name_style: String,
    /// 位置模式："follow_caret"（跟随光标,默认）| "fixed"（固定屏幕坐标 custom_x/custom_y）。
    #[serde(default = "default_status_position_mode")]
    pub position_mode: String,
    /// follow_caret 下相对默认位置（光标下方居中）的水平偏移（像素，正=右）。
    #[serde(default)]
    pub offset_x: i32,
    /// follow_caret 下相对默认位置的垂直偏移（像素，正=下）。
    #[serde(default)]
    pub offset_y: i32,
    /// fixed 模式的固定屏幕 X（像素）。
    #[serde(default)]
    pub custom_x: i32,
    /// fixed 模式的固定屏幕 Y（像素）。
    #[serde(default)]
    pub custom_y: i32,
}

fn default_schema_name_style() -> String {
    "full".to_string()
}
fn default_status_duration() -> i32 {
    800
}
fn default_status_display_mode() -> String {
    "temp".to_string()
}
fn default_status_position_mode() -> String {
    "follow_caret".to_string()
}

impl Default for StatusIndicatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            duration: default_status_duration(),
            display_mode: default_status_display_mode(),
            schema_name_style: default_schema_name_style(),
            position_mode: default_status_position_mode(),
            offset_x: 0,
            offset_y: 0,
            custom_x: 0,
            custom_y: 0,
        }
    }
}

/// 模式指示器配置（[ui.mode_indicator]）：进入临时拼音/双拼/快捷/英文/快符等模式时的标识。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeIndicatorConfig {
    /// 显示样式："short"（短称，默认）| "full"（全称）| "none"（不显示）。
    #[serde(default = "default_mode_indicator_style")]
    pub style: String,
}

fn default_mode_indicator_style() -> String {
    "short".to_string()
}

impl Default for ModeIndicatorConfig {
    fn default() -> Self {
        Self {
            style: default_mode_indicator_style(),
        }
    }
}

/// 模式指示样式（解析自 ui.mode_indicator.style）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeIndicatorStyle {
    /// 短称（拼/双/快/英/符）。
    Short,
    /// 全称（临时拼音 等）。
    Full,
    /// 不显示。
    None,
}

impl ModeIndicatorStyle {
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Self::Full,
            "none" => Self::None,
            _ => Self::Short,
        }
    }
}

impl ModeIndicatorConfig {
    pub fn parsed_style(&self) -> ModeIndicatorStyle {
        ModeIndicatorStyle::from_config(&self.style)
    }
}

/// 候选窗配置（[ui.candidate]）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCandidateConfig {
    /// 候选每页显示数（默认 7，对齐 Go 版本）
    #[serde(default = "default_per_page")]
    pub per_page: usize,
    /// 扩展档每页候选数（临拼/快捷/短语等 overlay 模式用，0=与 per_page 相同）。
    #[serde(default)]
    pub per_page_extended: usize,
    #[serde(default)]
    pub layout: String,
    /// 编码（组合区）显示方式。单一权威配置，取代旧的 inline_preedit + preedit_mode 组合。
    /// - "app_inline"（默认）：编码内嵌应用光标处，候选窗不显示 preedit 栏
    /// - "candidate_top"：候选窗顶部独立 preedit 栏
    /// - "candidate_inline"：编码作为候选窗首单元内联
    #[serde(default = "default_preedit_display")]
    pub preedit_display: String,
    #[serde(default)]
    pub hide_window: bool,
    /// 候选文本字号（0=跟随主题 behavior.font_size）。
    #[serde(default)]
    pub font_size: f32,
    /// 字号跟随主题：true 时忽略 font_size，用主题 behavior.font_size。
    #[serde(default)]
    pub font_size_follow_theme: bool,
    /// 翻页栏显示覆盖："" 跟随主题 / "hide" / "auto"(>1页) / "always"。
    #[serde(default)]
    pub pager_bar_display: String,
    /// 页码文字显示覆盖："" 跟随主题 / "show" / "hide"。
    #[serde(default)]
    pub page_number_display: String,
    /// 候选文本最大显示字数，超出截断（0=不限）。
    #[serde(default)]
    pub max_chars: usize,
    /// 自定义序号标签（如 "asdfg"；空=默认 1-9）。每字符一个槽位。
    #[serde(default)]
    pub index_labels: String,
    /// 候选窗在光标上方时反转候选排列顺序。
    #[serde(default)]
    pub flip_when_above: bool,
}

fn default_preedit_display() -> String {
    "app_inline".to_string()
}

/// 编码显示方式（解析自 ui.candidate.preedit_display）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreeditDisplay {
    /// 内嵌应用光标处，候选窗不显示 preedit。
    AppInline,
    /// 候选窗顶部独立 preedit 栏。
    CandidateTop,
    /// 编码作为候选窗首单元内联。
    CandidateInline,
}

impl PreeditDisplay {
    /// 解析配置字符串（空/未知 → 默认 AppInline）。
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "candidate_top" => Self::CandidateTop,
            "candidate_inline" => Self::CandidateInline,
            _ => Self::AppInline,
        }
    }

    /// 配置字符串形式（持久化用）。
    pub fn as_config(self) -> &'static str {
        match self {
            Self::AppInline => "app_inline",
            Self::CandidateTop => "candidate_top",
            Self::CandidateInline => "candidate_inline",
        }
    }

    /// 是否内嵌应用（候选窗不显示 preedit）。
    pub fn in_app(self) -> bool {
        matches!(self, Self::AppInline)
    }

    /// 是否编码内联候选首单元（对应旧 preedit_embedded）。
    pub fn embedded(self) -> bool {
        matches!(self, Self::CandidateInline)
    }

    /// 循环切换：内嵌应用 → 候选顶部 → 候选内联 → 内嵌应用。
    pub fn next(self) -> Self {
        match self {
            Self::AppInline => Self::CandidateTop,
            Self::CandidateTop => Self::CandidateInline,
            Self::CandidateInline => Self::AppInline,
        }
    }

    /// 简短中文名（状态提示用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::AppInline => "编码:内嵌应用",
            Self::CandidateTop => "编码:候选顶部",
            Self::CandidateInline => "编码:候选内联",
        }
    }
}

impl Default for UiCandidateConfig {
    fn default() -> Self {
        Self {
            per_page: default_per_page(),
            per_page_extended: 0,
            layout: String::new(),
            preedit_display: default_preedit_display(),
            hide_window: false,
            font_size: 0.0,
            font_size_follow_theme: false,
            pager_bar_display: String::new(),
            page_number_display: String::new(),
            max_chars: 0,
            index_labels: String::new(),
            flip_when_above: false,
        }
    }
}

impl UiCandidateConfig {
    /// 解析后的编码显示方式。
    pub fn preedit(&self) -> PreeditDisplay {
        PreeditDisplay::from_config(&self.preedit_display)
    }

    /// 第 `i` 个候选（0 基）的序号标签：有 index_labels 则取对应槽位，否则用 (i+1)。
    /// 槽位不足时回退数字。
    pub fn index_label(&self, i: usize) -> String {
        // index_labels 为空时 nth 直接 None，无需额外空判
        if let Some(ch) = self.index_labels.chars().nth(i) {
            return ch.to_string();
        }
        (i + 1).to_string()
    }

    /// 按 max_chars 截断候选显示文本（0=不限）。超出时截断（不加省略号，对齐 Go）。
    pub fn truncate_display(&self, text: &str) -> String {
        if self.max_chars == 0 {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= self.max_chars {
            text.to_string()
        } else {
            chars[..self.max_chars].iter().collect()
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

/// 悬停提示配置（[ui.tooltip]，对齐 Go `ui.tooltip.*`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooltipConfig {
    /// 提示延迟显示时间（毫秒）。
    #[serde(default = "default_tooltip_delay")]
    pub delay: i32,
    #[serde(default)]
    pub code: TooltipToggle,
    #[serde(default)]
    pub pinyin: TooltipPinyinConfig,
    #[serde(default = "default_tooltip_chaizi")]
    pub chaizi: TooltipToggle,
    #[serde(default = "default_tooltip_debug")]
    pub debug: TooltipToggle,
}

fn default_tooltip_delay() -> i32 {
    100
}

/// chaizi 默认关。
fn default_tooltip_chaizi() -> TooltipToggle {
    TooltipToggle { enabled: false }
}

/// debug 默认关。
fn default_tooltip_debug() -> TooltipToggle {
    TooltipToggle { enabled: false }
}

impl Default for TooltipConfig {
    fn default() -> Self {
        Self {
            delay: default_tooltip_delay(),
            code: TooltipToggle { enabled: true },
            pinyin: TooltipPinyinConfig::default(),
            chaizi: default_tooltip_chaizi(),
            debug: default_tooltip_debug(),
        }
    }
}

/// 单开关 provider（code / chaizi / debug）。默认开（chaizi/debug 由专用 default 覆盖为关）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooltipToggle {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for TooltipToggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// 拼音 provider 配置（[ui.tooltip.pinyin]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooltipPinyinConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 显示多音字所有读音（false 仅首音）。
    #[serde(default = "default_true")]
    pub heteronyms: bool,
    /// 每字最多显示读音数（0=不限）。
    #[serde(default)]
    pub max_readings: usize,
}

impl Default for TooltipPinyinConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            heteronyms: true,
            max_readings: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub stats: StatsConfig,
    #[serde(default)]
    pub s2t: S2TConfig,
    #[serde(default)]
    pub quick_input: QuickInputConfig,
    #[serde(default)]
    pub cmdbar: CmdbarConfig,
    /// 特殊模式列表（各自带码表 + 上屏策略；引导键触发）
    #[serde(default)]
    pub special_modes: Vec<SpecialModeConfig>,
    /// 临时 mix 模式列表（引导键触发，合并多个成员方案的候选）。
    /// 默认含一个 ; 触发的「快捷」融合：quick_input（日期/计算，内置类方案）+ pinyin + english。
    #[serde(default = "default_mix_modes")]
    pub mix_modes: Vec<MixModeConfig>,
}

fn default_mix_modes() -> Vec<MixModeConfig> {
    vec![MixModeConfig {
        id: "quick_mix".to_string(),
        name: "快捷".to_string(),
        short_name: "快".to_string(),
        trigger_keys: vec!["semicolon".to_string()],
        members: vec![
            "quick_input".to_string(),
            "pinyin".to_string(),
            "english".to_string(),
        ],
    }]
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            stats: StatsConfig::default(),
            s2t: S2TConfig::default(),
            quick_input: QuickInputConfig::default(),
            cmdbar: CmdbarConfig::default(),
            special_modes: Vec::new(),
            mix_modes: default_mix_modes(),
        }
    }
}

/// 临时 mix 模式配置（overlay 激活面）。触发后对每个成员方案查询并按成员序合并候选，
/// 融合临拼/快符/生僻字等。成员为真实方案 id（同特殊模式的拉平思路）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MixModeConfig {
    /// 实例唯一标识
    #[serde(default)]
    pub id: String,
    /// 显示名（UI 徽标 / 模式指示全称）
    #[serde(default)]
    pub name: String,
    /// 模式指示短称（空则取 name 首字）
    #[serde(default)]
    pub short_name: String,
    /// 引导键列表
    #[serde(default)]
    pub trigger_keys: Vec<String>,
    /// 成员方案 id 列表（按序合并候选；如 ["pinyin", "quick_symbols"]）
    #[serde(default)]
    pub members: Vec<String>,
}

/// 特殊模式配置（纯 overlay 激活面）。引擎/码表配置拉平到其引用的真实方案
/// `<schema>.schema.toml`（与 wubi86/pinyin 同级），全码策略复用方案的 [engine.codetable]。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SpecialModeConfig {
    /// 实例唯一标识
    #[serde(default)]
    pub id: String,
    /// 显示名（UI 徽标 / 模式指示全称，如 "快符"）
    #[serde(default)]
    pub name: String,
    /// 模式指示短称（如 "符"；空则取 name 首字）
    #[serde(default)]
    pub short_name: String,
    /// 引导键列表（如 "grave"/"backslash"/"z"）
    #[serde(default)]
    pub trigger_keys: Vec<String>,
    /// 引用的方案 id（其 .schema.toml 提供码表与全码策略；不进 schema.available，仅 overlay 触发懒加载）
    #[serde(default)]
    pub schema: String,
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
    // 默认不再独立抢 ;：; 由内置 mix「快捷」融合接管（quick_input 作为其成员）。
    // 仍可显式配置 trigger_keys 让 quick_input 作独立模式。
    Vec::new()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdbarConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 副作用命令候选（含 ActionEffect）在候选框渲染时的前缀标注（对齐 Go,默认 "⚡"）。
    #[serde(default = "default_candidate_prefix")]
    pub candidate_prefix: String,
}

fn default_candidate_prefix() -> String {
    "⚡".to_string()
}

impl Default for CmdbarConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            candidate_prefix: default_candidate_prefix(),
        }
    }
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

/// 全局拼音配置（[pinyin]）。所有拼音类方案（全拼/双拼/混输拼音子方案/临时拼音反查）共用。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinyinGlobalConfig {
    #[serde(default = "default_true")]
    pub show_code_hint: bool,
    #[serde(default = "default_true")]
    pub use_smart_compose: bool,
    #[serde(default = "default_candidate_order")]
    pub candidate_order: String,
    #[serde(default)]
    pub fuzzy: PinyinFuzzy,
}

fn default_candidate_order() -> String { "smart".to_string() }

impl Default for PinyinGlobalConfig {
    fn default() -> Self {
        Self {
            show_code_hint: true,
            use_smart_compose: true,
            candidate_order: "smart".to_string(),
            fuzzy: PinyinFuzzy::default(),
        }
    }
}

/// 全局模糊音（[pinyin.fuzzy]）。字段对齐引擎 FuzzyConfig。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinyinFuzzy {
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub zh_z: bool,
    #[serde(default)] pub ch_c: bool,
    #[serde(default)] pub sh_s: bool,
    #[serde(default)] pub n_l: bool,
    #[serde(default)] pub f_h: bool,
    #[serde(default)] pub r_l: bool,
    #[serde(default)] pub an_ang: bool,
    #[serde(default)] pub en_eng: bool,
    #[serde(default)] pub in_ing: bool,
    #[serde(default)] pub ian_iang: bool,
    #[serde(default)] pub uan_uang: bool,
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
            pinyin: PinyinGlobalConfig::default(),
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

    /// 把单个配置项**部分合并**写入用户层 `config.toml`（%APPDATA%/WindInput/config.toml）。
    ///
    /// 只改 `path` 指定的项、保留用户文件里其它已有项，**不写入未改动的默认/系统段**——
    /// 用户层维持最小 diff，避免覆盖系统层/默认层的后续更新（对齐 wind-setting 的"快照→diff"模型）。
    /// 原子写（tmp + rename）。`path` 如 `["ui","candidate","preedit_mode"]`。
    pub fn set_user_value(path: &[&str], value: toml::Value) -> anyhow::Result<()> {
        if path.is_empty() {
            anyhow::bail!("set_user_value: empty path");
        }
        let dir = Self::user_config_dir().context("no user config dir")?;
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("config.toml");

        // 读现有用户层（partial），不存在/解析失败则空表（不丢已有项时尽量保留）。
        let mut root = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        if !root.is_table() {
            root = toml::Value::Table(Default::default());
        }
        if let toml::Value::Table(t) = &mut root {
            set_nested(t, path, value);
        }

        let out = toml::to_string_pretty(&root)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
    }

    /// [`set_user_value`](Self::set_user_value) 的字符串便捷形式。
    pub fn set_user_string(path: &[&str], value: &str) -> anyhow::Result<()> {
        Self::set_user_value(path, toml::Value::String(value.to_string()))
    }

    /// [`set_user_value`](Self::set_user_value) 的布尔便捷形式。
    pub fn set_user_bool(path: &[&str], value: bool) -> anyhow::Result<()> {
        Self::set_user_value(path, toml::Value::Boolean(value))
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

    #[test]
    fn set_nested_creates_overwrites_and_preserves() {
        let mut t = toml::Table::new();
        t.insert("keep".into(), toml::Value::String("x".into()));
        set_nested(
            &mut t,
            &["ui", "candidate", "preedit_mode"],
            toml::Value::String("embedded".into()),
        );
        set_nested(
            &mut t,
            &["schema", "active"],
            toml::Value::String("pinyin".into()),
        );
        // 原有项保留
        assert_eq!(t.get("keep").unwrap().as_str(), Some("x"));
        // 嵌套创建
        assert_eq!(
            t.get("ui")
                .unwrap()
                .get("candidate")
                .unwrap()
                .get("preedit_mode")
                .unwrap()
                .as_str(),
            Some("embedded")
        );
        assert_eq!(
            t.get("schema").unwrap().get("active").unwrap().as_str(),
            Some("pinyin")
        );
        // 同路径覆盖
        set_nested(
            &mut t,
            &["ui", "candidate", "preedit_mode"],
            toml::Value::String("top".into()),
        );
        assert_eq!(
            t.get("ui")
                .unwrap()
                .get("candidate")
                .unwrap()
                .get("preedit_mode")
                .unwrap()
                .as_str(),
            Some("top")
        );
        // 其它兄弟键不受影响
        assert_eq!(
            t.get("schema").unwrap().get("active").unwrap().as_str(),
            Some("pinyin")
        );
    }

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

    #[test]
    fn test_tooltip_defaults_match_go() {
        let t = Config::default().ui.tooltip;
        assert_eq!(t.delay, 100);
        assert!(t.code.enabled, "code 默认开");
        assert!(
            t.pinyin.enabled && t.pinyin.heteronyms,
            "pinyin 默认开+全读音"
        );
        assert_eq!(t.pinyin.max_readings, 0);
        assert!(!t.chaizi.enabled, "chaizi 默认关");
        assert!(!t.debug.enabled, "debug 默认关");
    }

    #[test]
    fn test_tooltip_merge_override() {
        let cfg = merged_with(
            "[ui.tooltip.chaizi]\nenabled = true\n\
             [ui.tooltip.pinyin]\nheteronyms = false\nmax_readings = 2\n",
        );
        assert!(cfg.ui.tooltip.chaizi.enabled);
        assert!(!cfg.ui.tooltip.pinyin.heteronyms);
        assert_eq!(cfg.ui.tooltip.pinyin.max_readings, 2);
        // 未指定字段保留默认
        assert!(cfg.ui.tooltip.code.enabled, "code 未指定应保留默认开");
        assert_eq!(cfg.ui.tooltip.delay, 100);
    }

    #[test]
    fn test_candidate_tuning_defaults_and_methods() {
        let c = Config::default().ui.candidate;
        assert_eq!(c.font_size, 0.0, "字号默认 0=跟随主题");
        assert_eq!(c.max_chars, 0, "默认不限");
        assert!(c.index_labels.is_empty() && !c.flip_when_above);
        // index_label：默认数字
        assert_eq!(c.index_label(0), "1");
        // truncate：0=不限
        assert_eq!(
            c.truncate_display("这是一个很长的候选"),
            "这是一个很长的候选"
        );
    }

    #[test]
    fn test_candidate_index_labels_and_truncate() {
        let cfg = merged_with("[ui.candidate]\nindex_labels = \"asdf\"\nmax_chars = 4\n");
        let c = cfg.ui.candidate;
        assert_eq!(c.index_label(0), "a");
        assert_eq!(c.index_label(2), "d");
        assert_eq!(c.index_label(9), "10", "槽位不足回退数字");
        assert_eq!(
            c.truncate_display("一二三四五六"),
            "一二三四",
            "截断到 4 字"
        );
        assert_eq!(c.truncate_display("一二"), "一二", "不足不截");
    }

    #[test]
    fn pinyin_global_config_defaults() {
        let c = Config::default();
        assert!(c.pinyin.show_code_hint);
        assert!(c.pinyin.use_smart_compose);
        assert_eq!(c.pinyin.candidate_order, "smart");
        assert!(!c.pinyin.fuzzy.enabled);
        assert!(!c.pinyin.fuzzy.zh_z);
    }
}
