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
    /// 全码/空码上屏策略的全局默认（方案级 [engine.codetable] 的 tri-state 字段未设时回退至此）
    #[serde(default)]
    pub code_commit: CodeCommitConfig,
}

/// 全码/空码上屏策略全局默认（对齐方案级 [engine.codetable] 同名字段）。
/// 解析顺序：方案级 Some > 本全局 > 内置默认。放在 `config.toml`（用户可合并），
/// 使非主方案/未单独配置的方案统一吃全局，且无需改只读安装目录的 schema。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 显示名（UI 徽标）
    #[serde(default)]
    pub name: String,
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
    /// 显示名（UI 徽标）
    #[serde(default)]
    pub name: String,
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
            t.get("ui").unwrap().get("candidate").unwrap().get("preedit_mode").unwrap().as_str(),
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
            t.get("ui").unwrap().get("candidate").unwrap().get("preedit_mode").unwrap().as_str(),
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
}
