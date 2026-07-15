//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。
//! 配置文件为 TOML 格式，三层合并：默认值 → data/config.toml → %APPDATA%/WindInput/config.toml
//!
//! 顶级域（"正交大类"准则，详见 SETTINGS_REVAMP_PLAN.md / docs/config-key-migration.md）：
//! schema(方案+pinyin+模式) / input(输入行为，含 default 启动默认 / phrase 短语) /
//! keys(全部按键) / ui(外观) / stats(统计) / compat / debug。

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
    pub schema: SchemaConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub keys: KeysConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub stats: StatsConfig,
    #[serde(default)]
    pub compat: CompatConfig,
    #[serde(default)]
    pub debug: DebugConfig,
}

// ──────────────── input.default（启动默认状态，原 general 域）────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputDefaultConfig {
    /// 记忆前次状态：true=启动/激活时恢复上次的中英/全半角/标点；false=每次激活重置为下方默认值。
    #[serde(default)]
    pub remember_last_state: bool,
    #[serde(default = "default_true")]
    pub chinese_mode: bool,
    #[serde(default)]
    pub full_width: bool,
    #[serde(default = "default_true")]
    pub chinese_punct: bool,
    /// 中英状态作用域："global"（全局统一，默认）| "app"（按应用独立记忆，会话级）。
    #[serde(default = "default_state_scope")]
    pub state_scope: String,
}

fn default_state_scope() -> String {
    "global".to_string()
}

impl InputDefaultConfig {
    /// 中英状态是否按应用独立记忆（state_scope == "app"）。
    pub fn per_app_scope(&self) -> bool {
        self.state_scope.eq_ignore_ascii_case("app")
    }
}

impl Default for InputDefaultConfig {
    fn default() -> Self {
        Self {
            remember_last_state: false,
            chinese_mode: true,
            full_width: false,
            chinese_punct: true,
            state_scope: default_state_scope(),
        }
    }
}

// ───────────────────────── schema（方案 + 拼音 + 模式）─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SchemaConfig {
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub available: Vec<String>,
    #[serde(default)]
    pub primary_codetable: String,
    #[serde(default)]
    pub primary_pinyin: String,
    /// 全局码表配置（所有码表方案公共基线；方案经 schema_overrides 覆盖）。
    #[serde(default)]
    pub codetable: CodetableGlobal,
    /// 全局拼音配置（所有拼音类方案共用：全拼/双拼/混输拼音子方案/临时拼音反查）。
    #[serde(default)]
    pub pinyin: PinyinGlobalConfig,
    /// 全局混输配置（融合策略；全局唯一）。
    #[serde(default)]
    pub mix: MixGlobal,
    /// 快捷输入（日期/计算等内置类方案）配置。将随"英文/快捷做成方案"一并重构。
    #[serde(default)]
    pub quick_input: QuickInputConfig,
    /// 特殊模式列表（各自带码表 + 上屏策略；引导键触发）。
    #[serde(default)]
    pub special_modes: Vec<SpecialModeConfig>,
    /// 临时 mix 模式列表（引导键触发，合并多个成员方案的候选）。
    #[serde(default = "default_mix_modes")]
    pub mix_modes: Vec<MixModeConfig>,
}

impl Default for SchemaConfig {
    fn default() -> Self {
        Self {
            active: String::new(),
            available: Vec::new(),
            primary_codetable: String::new(),
            primary_pinyin: String::new(),
            codetable: CodetableGlobal::default(),
            pinyin: PinyinGlobalConfig::default(),
            mix: MixGlobal::default(),
            quick_input: QuickInputConfig::default(),
            special_modes: Vec::new(),
            mix_modes: default_mix_modes(),
        }
    }
}

/// 全局拼音配置（[schema.pinyin]）。所有拼音类方案共用，无方案级 override。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PinyinGlobalConfig {
    #[serde(default = "default_true")]
    pub show_code_hint: bool,
    #[serde(default = "default_true")]
    pub use_smart_compose: bool,
    /// 拼音分隔策略（"auto" 等）。原 input.pinyin_separator 收拢至此。
    #[serde(default = "default_pinyin_separator")]
    pub separator: String,
    #[serde(default)]
    pub fuzzy: PinyinFuzzy,
    /// 拼音调频（衰减参数；全局唯一，按引擎分——见 docs/redesign/schema-config-layering.md §3.4）。
    #[serde(default)]
    pub frequency: PinyinFrequency,
    /// 拼音自动造词（全局唯一）。
    #[serde(default)]
    pub auto_learn: AutoLearnConfig,
}

impl Default for PinyinGlobalConfig {
    fn default() -> Self {
        Self {
            show_code_hint: true,
            use_smart_compose: true,
            separator: default_pinyin_separator(),
            fuzzy: PinyinFuzzy::default(),
            frequency: PinyinFrequency::default(),
            auto_learn: AutoLearnConfig::default(),
        }
    }
}

/// 全局码表配置（[schema.codetable]）。所有码表方案的公共基线，方案可经
/// `schema_overrides/{id}.toml` 的 `[codetable]` 段（带 enabled 总开关）逐字段覆盖。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodetableGlobal {
    /// 顶码上屏（超满码长取前 N 码首选上屏）。
    #[serde(default)]
    pub top_code_commit: bool,
    /// 满码无候选时清空缓冲。
    #[serde(default)]
    pub clear_on_empty_max: bool,
    /// 满码唯一精确时自动上屏。
    #[serde(default)]
    pub auto_commit_at_full: bool,
    /// 自动上屏最短码长（隐藏参数；0=等于全码长，不在设置 UI 暴露）。
    #[serde(default)]
    pub auto_commit_min_len: usize,
    /// 标点触发上屏。
    #[serde(default)]
    pub punct_commit: bool,
    /// 显示编码提示。
    #[serde(default = "default_true")]
    pub show_code_hint: bool,
    /// 精确匹配模式（关闭前缀匹配）。
    #[serde(default)]
    pub single_code_input: bool,
    /// 精确匹配空码补全（无候选时从更长编码取首选）。
    #[serde(default)]
    pub single_code_complete: bool,
    /// z 键重复输入。
    #[serde(default)]
    pub z_key_repeat: bool,
    /// 码表调频（统一开关，取代旧 user_frequency）。
    #[serde(default)]
    pub frequency: CodetableFrequency,
    /// 码表自动造词（连续单字）。
    #[serde(default)]
    pub auto_phrase: AutoPhraseConfig,
}

impl Default for CodetableGlobal {
    fn default() -> Self {
        Self {
            top_code_commit: false,
            clear_on_empty_max: false,
            auto_commit_at_full: false,
            auto_commit_min_len: 0,
            punct_commit: false,
            show_code_hint: true,
            single_code_input: false,
            single_code_complete: false,
            z_key_repeat: false,
            frequency: CodetableFrequency::default(),
            auto_phrase: AutoPhraseConfig::default(),
        }
    }
}

impl CodetableGlobal {
    /// 折叠方案 `[engine.codetable]` 的内联/override 行为到全局基线：各 `Some(_)` 字段覆盖，
    /// `None` 回落全局。schema 内联与 `schema_overrides` 已在 `read_schema` 经 `merge_toml`
    /// 合并成单个 `CodeTableSpec`，此处只做「方案 → 全局」一次折叠。见 schema-config-layering.md §4。
    pub fn resolved(&self, ov: Option<&crate::schema::CodeTableSpec>) -> CodetableGlobal {
        let mut out = self.clone();
        let Some(o) = ov else {
            return out;
        };
        if let Some(v) = o.top_code_commit {
            out.top_code_commit = v;
        }
        if let Some(v) = o.clear_on_empty_max {
            out.clear_on_empty_max = v;
        }
        if let Some(v) = o.auto_commit_at_full {
            out.auto_commit_at_full = v;
        }
        if let Some(v) = o.auto_commit_min_len {
            out.auto_commit_min_len = v;
        }
        if let Some(v) = o.punct_commit {
            out.punct_commit = v;
        }
        if let Some(v) = o.show_code_hint {
            out.show_code_hint = v;
        }
        if let Some(v) = o.single_code_input {
            out.single_code_input = v;
        }
        if let Some(v) = o.single_code_complete {
            out.single_code_complete = v;
        }
        if let Some(v) = o.z_key_repeat {
            out.z_key_repeat = v;
        }
        out
    }
}

/// 码表调频（[schema.codetable.frequency]）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CodetableFrequency {
    #[serde(default)]
    pub enabled: bool,
    /// 锁定码表原始前 N 位（仅纯码表生效）。
    #[serde(default)]
    pub protect_top_n: usize,
    /// 词频应用策略："top"（一次到顶 MRU）/ "step"（逐次提升）。原 freq_strategy 迁入。
    #[serde(default = "default_freq_strategy")]
    pub strategy: String,
}

fn default_freq_strategy() -> String {
    "step".to_string()
}

impl Default for CodetableFrequency {
    fn default() -> Self {
        Self {
            enabled: false,
            protect_top_n: 0,
            strategy: default_freq_strategy(),
        }
    }
}

/// 拼音调频（[schema.pinyin.frequency]）。衰减参数（0=用 store 默认）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PinyinFrequency {
    #[serde(default)]
    pub enabled: bool,
    /// 半衰期（小时）。
    #[serde(default)]
    pub half_life: f64,
    /// base 系数。
    #[serde(default)]
    pub base_scale: f64,
    /// 最近使用峰值。
    #[serde(default)]
    pub recency_peak: f64,
}

/// 码表自动造词（[schema.codetable.auto_phrase]）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoPhraseConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 造词最小字数（默认 2；设置页隐藏）。
    #[serde(default = "default_phrase_min_len")]
    pub min_phrase_len: usize,
    /// 造词最大字数（默认 10；设置页隐藏）。
    #[serde(default = "default_phrase_max_len")]
    pub max_phrase_len: usize,
    /// 临时词晋升所需使用次数（原 learning.temp_promote_count）。
    #[serde(default)]
    pub promote_count: usize,
}

fn default_phrase_min_len() -> usize {
    2
}

fn default_phrase_max_len() -> usize {
    10
}

impl Default for AutoPhraseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_phrase_len: default_phrase_min_len(),
            max_phrase_len: default_phrase_max_len(),
            promote_count: 0,
        }
    }
}

/// 拼音自动造词（[schema.pinyin.auto_learn]）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoLearnConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 造词最小字数（默认 0=回退 2）。
    #[serde(default)]
    pub min_word_length: usize,
    /// 临时词晋升所需使用次数（原 learning.temp_promote_count）。
    #[serde(default)]
    pub promote_count: usize,
}

/// 全局混输配置（[schema.mix]）。融合策略；全局唯一，无方案级 override。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MixGlobal {
    /// 显示候选来源标记。
    #[serde(default)]
    pub show_source_hint: bool,
    /// 启用英文候选。
    #[serde(default)]
    pub enable_english: bool,
    /// 超码长时仅查拼音。
    #[serde(default)]
    pub pinyin_only_overflow: bool,
    /// 顶码偏好（顶码覆盖拼音）。
    #[serde(default)]
    pub top_code_override_pinyin: bool,
    /// 满码上屏遇拼音候选则否决（保护拼音用户）。默认关：粗粒度一票否决太激进，
    /// 细粒度拦截由 `block_commit_on_pinyin_word`（默认开）承担。
    #[serde(default)]
    pub auto_commit_block_on_pinyin: bool,
    /// 满码上屏遇英文候选则否决（保护正在输入英文词的用户；仅 enable_english 开时有意义）。
    #[serde(default)]
    pub auto_commit_block_on_english: bool,
    /// 拼音最小触发长度（0=回退 2）。
    #[serde(default)]
    pub min_pinyin_length: usize,
    /// 英文最小触发长度（0=回退 3，即 2 字符以内不查英文；预留可配）。
    #[serde(default)]
    pub min_english_length: usize,
    /// 拼音歧义拦截（词强度启发式）：整串是强拼音词时否决五笔自动/顶码上屏，让拼音赢
    /// （如 wangba→网吧；aipu 无强词则放行落实）。默认开；独立于 auto_commit_block_on_pinyin。
    #[serde(default = "default_true")]
    pub block_commit_on_pinyin_word: bool,
    /// 拼音歧义拦截的词强度权重阈值（0=仅结构判据：≥2 汉字且消费整串；预留真机调）。
    #[serde(default)]
    pub pinyin_word_min_weight: i32,
}

impl Default for MixGlobal {
    fn default() -> Self {
        Self {
            show_source_hint: false,
            enable_english: false,
            pinyin_only_overflow: false,
            top_code_override_pinyin: false,
            auto_commit_block_on_pinyin: false,
            auto_commit_block_on_english: false,
            min_pinyin_length: 0,
            min_english_length: 0,
            block_commit_on_pinyin_word: true,
            pinyin_word_min_weight: 0,
        }
    }
}

/// 全局模糊音（[schema.pinyin.fuzzy]）。字段对齐引擎 FuzzyConfig。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinyinFuzzy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub zh_z: bool,
    #[serde(default)]
    pub ch_c: bool,
    #[serde(default)]
    pub sh_s: bool,
    #[serde(default)]
    pub n_l: bool,
    #[serde(default)]
    pub f_h: bool,
    #[serde(default)]
    pub r_l: bool,
    #[serde(default)]
    pub an_ang: bool,
    #[serde(default)]
    pub en_eng: bool,
    #[serde(default)]
    pub in_ing: bool,
    #[serde(default)]
    pub ian_iang: bool,
    #[serde(default)]
    pub uan_uang: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickInputConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 计算器结果小数位数，默认 6
    #[serde(default = "default_decimal_places")]
    pub decimal_places: i32,
    /// 强制竖排显示：进入快捷输入时切竖排候选，退出恢复原布局。
    #[serde(default)]
    pub force_vertical: bool,
    /// 快捷（融合）模式是否混入英文候选（english 成员）。默认开启（低优先级排在拼音后）。
    /// 独立于混输的 schema.mix.enable_english。
    #[serde(default = "default_true")]
    pub enable_english: bool,
}

fn default_decimal_places() -> i32 {
    6
}

impl Default for QuickInputConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            decimal_places: default_decimal_places(),
            force_vertical: false,
            enable_english: true,
        }
    }
}

/// 临时 mix 模式配置（overlay 激活面）。触发后对每个成员方案查询并按成员序合并候选。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
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

/// 特殊模式配置（纯 overlay 激活面）。引擎/码表配置拉平到其引用的真实方案
/// `<schema>.schema.toml`，全码策略复用方案的 [engine.codetable]。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
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
    /// 专用直达热键（如 "ctrl+shift+u"，空串=不注册）。与 `trigger_keys` 引导键共存；
    /// 热键进入时组合区不写引导符（见 docs/design/special-mode-entry-hotkey.md）。
    #[serde(default)]
    pub hotkey: String,
}

impl SpecialModeConfig {
    /// 有效 id：`id` 非空则用之，否则回退 `schema`（瘦身条目只写 `schema` + `trigger_keys`，
    /// 身份从被引用方案文件派生）。供直达热键 `enter_special:<id>` 定位。
    pub fn effective_id(&self) -> &str {
        if self.id.is_empty() {
            &self.schema
        } else {
            &self.id
        }
    }
}

// ───────────────────────── input（输入行为）─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    #[serde(default = "default_filter_mode")]
    pub filter_mode: String,
    #[serde(default = "default_enter_behavior")]
    pub enter_behavior: String,
    #[serde(default = "default_space_behavior")]
    pub space_on_empty_behavior: String,
    #[serde(default = "default_numpad_behavior")]
    pub numpad_behavior: String,
    /// 启动默认状态（记住上次状态 / 默认中文 / 全角 / 中文标点；原 general 域）。
    #[serde(default)]
    pub default: InputDefaultConfig,
    /// 标点相关（随中英、智能标点、自定义映射）。
    #[serde(default)]
    pub punct: PunctConfig,
    /// 智能符号模式。
    #[serde(default)]
    pub symbol: SymbolConfig,
    /// 标点配对（输入左括号自动补右括号 + 输右括号智能跳过）。
    #[serde(default)]
    pub auto_pair: AutoPairConfig,
    /// 临时英文（Shift+字母 / 触发键进入临英缓冲）。
    #[serde(default)]
    pub temp_english: TempEnglishConfig,
    #[serde(default)]
    pub capslock: CapslockConfig,
    /// 临时拼音（码表方案下临时切到拼音反查）。
    #[serde(default)]
    pub temp_pinyin: TempPinyinConfig,
    /// 网址输入模式。
    #[serde(default)]
    pub url: UrlConfig,
    /// 简繁转换（上屏文字变换）。原 features.s2t。
    #[serde(default)]
    pub s2t: S2TConfig,
    /// 命令栏（$CC/$SS/$AA 等命令候选）。原 features.cmdbar。
    #[serde(default)]
    pub cmdbar: CmdbarConfig,
    /// 短语前缀列举（含命令栏 $CC/$SS/$AA）。原 dict.phrase / Go input.phrase。
    #[serde(default)]
    pub phrase: PhraseConfig,
    /// 顶码上屏策略（内部/实验，默认 direct_commit 真提交时序，躲开 diff 合并与整段下划线）。
    #[serde(default)]
    pub top_commit_mode: TopCommitMode,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            filter_mode: "smart".to_string(),
            enter_behavior: "commit".to_string(),
            space_on_empty_behavior: "commit".to_string(),
            numpad_behavior: default_numpad_behavior(),
            default: InputDefaultConfig::default(),
            punct: PunctConfig::default(),
            symbol: SymbolConfig::default(),
            auto_pair: AutoPairConfig::default(),
            temp_english: TempEnglishConfig::default(),
            capslock: CapslockConfig::default(),
            temp_pinyin: TempPinyinConfig::default(),
            url: UrlConfig::default(),
            s2t: S2TConfig::default(),
            cmdbar: CmdbarConfig::default(),
            phrase: PhraseConfig::default(),
            top_commit_mode: TopCommitMode::default(),
        }
    }
}

/// 标点配置（[input.punct]）：随中英、智能标点、自定义映射。
/// `custom_mappings`: key=源字符（引号用 `"1`/`"2`/`'1`/`'2` 区分左右），
/// value=`[中文半角, 英文全角, 中文全角, 英文半角]`（空串/缺列=回退默认转换）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PunctConfig {
    /// 标点随中英模式切换。
    #[serde(default)]
    pub follow_mode: bool,
    /// 数字后的标点智能直出英文。
    #[serde(default = "default_true")]
    pub smart_after_digit: bool,
    /// 参与"数字后智能英文标点"的标点集合。
    #[serde(default = "default_smart_punct_list")]
    pub smart_list: String,
    /// 自定义标点映射开关。
    #[serde(default)]
    pub custom_enabled: bool,
    /// 自定义标点映射表（四状态：中半/英全/中全/英半）。
    #[serde(default)]
    pub custom_mappings: HashMap<String, Vec<String>>,
}

impl Default for PunctConfig {
    fn default() -> Self {
        Self {
            follow_mode: false,
            smart_after_digit: true,
            smart_list: default_smart_punct_list(),
            custom_enabled: false,
            custom_mappings: HashMap::new(),
        }
    }
}

/// 智能符号替换方案。
/// - `DeleteReplace`：press1 直接提交中文符号，press2 删改（当前方案，部分 Chromium 应用光标偏移）。
/// - `HoldComposition`：press1 开启 TSF 组合态展示中文符号，press2 替换组合提交英文；
///   超时（smart_timeout_ms）后自动提交中文，无删改操作（推荐）。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SmartMethod {
    DeleteReplace,
    #[default]
    HoldComposition,
}

/// 顶码/顶屏的宿主上屏策略。影响顶码上屏时「已确认文字」如何落到宿主：
/// - PreConfirm：留在 TSF 组合态（_pendingCommitPrefix 聚合），延迟到最终 CommitText 才真提交。
///   diff 式宿主（终端/Chromium）不双写，但部分宿主整段画下划线、WPS 智能标点顶屏会清空。
/// - DirectCommit：顶码时真提交，余码新组合延迟到触发键 keyup 才开（照抄真实输入法时序），
///   靠隔一拍消息泵躲开 diff 合并；真提交无下划线歧义、WPS 不清空。
/// TODO(per-app)：后续可按宿主进程名 override（当前仅全局默认）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TopCommitMode {
    PreConfirm,
    #[default]
    DirectCommit,
}

/// 智能符号配置（[input.symbol]）：同一中文标点在时限内连按两次，删前一字符替换为英文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolConfig {
    /// 智能符号模式总开关（默认 false）。
    #[serde(default)]
    pub smart_mode: bool,
    /// 判定时限（毫秒，默认 500）。
    #[serde(default = "default_smart_symbol_timeout_ms")]
    pub smart_timeout_ms: i32,
    /// 参与智能符号转换的中文标点集合（子串包含匹配，含成对/多字符标点）。
    #[serde(default = "default_smart_symbol_chars")]
    pub smart_chars: String,
    /// 替换方案：`delete_replace`（删改）或 `hold_composition`（保持组合态，默认）。
    #[serde(default)]
    pub smart_method: SmartMethod,
}

impl Default for SymbolConfig {
    fn default() -> Self {
        Self {
            smart_mode: false,
            smart_timeout_ms: default_smart_symbol_timeout_ms(),
            smart_chars: default_smart_symbol_chars(),
            smart_method: SmartMethod::default(),
        }
    }
}

/// 标点配对配置（[input.auto_pair]，对齐 Go AutoPairConfig）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoPairConfig {
    /// 中文标点配对开关
    #[serde(default)]
    pub chinese: bool,
    /// 英文标点配对开关
    #[serde(default)]
    pub english: bool,
    /// 中文配对表（每项 2 字符："（）"）
    #[serde(default = "default_chinese_pairs")]
    pub chinese_pairs: Vec<String>,
    /// 英文配对表（每项 2 字符："()"）
    #[serde(default = "default_english_pairs")]
    pub english_pairs: Vec<String>,
    /// 跳出配对的按键（键名如 "tab"/"enter"/"space"，可多选）。命中即等效输入右符号跳出：
    /// 光标越过右符号、弹出配对栈。默认空 → 不启用。仅对协调器跟踪的中文输入态配对生效
    /// （英文模式配对由 TSF/DLL 侧处理）。
    #[serde(default)]
    pub jump_out_keys: Vec<String>,
}

impl Default for AutoPairConfig {
    fn default() -> Self {
        Self {
            chinese: false,
            english: false,
            chinese_pairs: default_chinese_pairs(),
            english_pairs: default_english_pairs(),
            jump_out_keys: Vec::new(),
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

/// 临时英文配置（[input.temp_english]，原 input.shift_temp_english）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempEnglishConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 显示英文候选（原 show_english_candidates）。
    #[serde(default = "default_true")]
    pub show_candidates: bool,
    #[serde(default = "default_shift_behavior")]
    pub shift_behavior: String,
    /// 触发键（符号键进入临时英文模式，类似临时拼音触发键）。默认空（仅 Shift+字母触发）。
    #[serde(default)]
    pub trigger_keys: Vec<String>,
    #[serde(default)]
    pub allow_symbols: bool,
    #[serde(default)]
    pub space_as_input: bool,
}

impl Default for TempEnglishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_candidates: true,
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

/// 临时拼音配置（[input.temp_pinyin]）。码表方案下临时切到拼音反查。全局唯一。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TempPinyinConfig {
    /// 总开关（原方案级 [engine.codetable.temp_pinyin].enabled 上移至此）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 目标拼音方案 id（空=回退 "pinyin"）。原方案级 schema 字段上移。
    #[serde(default)]
    pub schema: String,
    /// 触发键（如 "backtick" / "z" / "semicolon"），默认反引号
    #[serde(default = "default_temp_pinyin_triggers")]
    pub trigger_keys: Vec<String>,
    /// 专用直达热键（如 "ctrl+shift+p"，空串=不注册）。与 `trigger_keys` 引导键共存；
    /// 热键进入时组合区不写引导符（见 docs/design/special-mode-entry-hotkey.md）。
    #[serde(default)]
    pub hotkey: String,
}

fn default_temp_pinyin_triggers() -> Vec<String> {
    vec!["backtick".to_string()]
}

impl Default for TempPinyinConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schema: String::new(),
            trigger_keys: default_temp_pinyin_triggers(),
            hotkey: String::new(),
        }
    }
}

/// 网址模式配置（[input.url]，原 input.url_input）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UrlConfig {
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

impl Default for UrlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prefixes: default_url_prefixes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S2TConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_s2t_variant")]
    pub variant: String,
}

impl Default for S2TConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            variant: default_s2t_variant(),
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

// ───────────────────────── keys（全部按键）─────────────────────────

/// 全部按键绑定（[keys]，扁平）：原 hotkeys.* + 散在 input 的选择/导航键 + overflow。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeysConfig {
    // ── 热键（原 hotkeys.*）──
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
    #[serde(default = "default_toggle_toolbar")]
    pub toggle_toolbar: String,
    #[serde(default = "default_open_settings")]
    pub open_settings: String,
    #[serde(default = "default_add_word")]
    pub add_word: String,
    #[serde(default = "default_open_add_word_dialog")]
    pub open_add_word_dialog: String,
    #[serde(default = "default_toggle_s2t")]
    pub toggle_s2t: String,
    #[serde(default = "default_activate_ime")]
    pub activate_ime: String,
    #[serde(default = "default_pin_candidate")]
    pub pin_candidate: String,
    #[serde(default = "default_delete_candidate")]
    pub delete_candidate: String,
    #[serde(default = "default_take_screenshot")]
    pub take_screenshot: String,
    #[serde(default)]
    pub global_hotkeys: Vec<String>,
    // ── 选择/导航键（原 input.*）──
    #[serde(default = "default_select_key_groups")]
    pub select_key_groups: Vec<String>,
    #[serde(default = "default_page_keys")]
    pub page_keys: Vec<String>,
    #[serde(default = "default_highlight_keys")]
    pub highlight_keys: Vec<String>,
    #[serde(default)]
    pub select_char_keys: Vec<String>,
    /// 候选无效按键策略（数字键/次选三选键/以词定字键超出候选范围时的处理）。
    #[serde(default)]
    pub overflow: OverflowConfig,
}

// 热键默认值对齐 Go 版 DefaultConfig.Hotkeys（wind_input/pkg/config/config.go）。
// 关键：config.getDefaults 以 Config::default() 为 L1 基线（再叠 data/config.toml），
// [keys] 整表在 L2 缺失时用 Default::default()，故必须手写 Default（而非 derive 的
// 空值），否则设置页"开关后默认键丢失"。
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
fn default_open_add_word_dialog() -> String {
    "ctrl+shift+equal".to_string()
}
fn default_toggle_s2t() -> String {
    "ctrl+shift+j".to_string()
}
fn default_take_screenshot() -> String {
    "ctrl+shift+f11".to_string()
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
fn default_select_key_groups() -> Vec<String> {
    vec!["semicolon_quote".to_string()]
}
fn default_page_keys() -> Vec<String> {
    vec!["pageupdown".to_string(), "minus_equal".to_string()]
}
fn default_highlight_keys() -> Vec<String> {
    vec!["arrows".to_string(), "tab".to_string()]
}

impl Default for KeysConfig {
    fn default() -> Self {
        Self {
            toggle_mode_keys: default_toggle_mode_keys(),
            commit_on_switch: true,
            switch_engine: default_switch_engine(),
            toggle_full_width: default_toggle_full_width(),
            toggle_punct: default_toggle_punct(),
            toggle_toolbar: default_toggle_toolbar(),
            open_settings: default_open_settings(),
            add_word: default_add_word(),
            open_add_word_dialog: default_open_add_word_dialog(),
            toggle_s2t: default_toggle_s2t(),
            activate_ime: default_activate_ime(),
            pin_candidate: default_pin_candidate(),
            delete_candidate: default_delete_candidate(),
            take_screenshot: default_take_screenshot(),
            global_hotkeys: Vec::new(),
            select_key_groups: default_select_key_groups(),
            page_keys: default_page_keys(),
            highlight_keys: default_highlight_keys(),
            select_char_keys: Vec::new(),
            overflow: OverflowConfig::default(),
        }
    }
}

/// 候选无效按键策略（[keys.overflow]，对齐 Go OverflowConfig）。
/// 每项取值："ignore"（吞键无效）/ "commit"（上屏当前高亮候选）/
/// "commit_and_input"（上屏高亮候选 + 追加按键字符）。默认全 ignore。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverflowConfig {
    /// 数字键超出当前页候选数量时
    #[serde(default = "default_overflow_behavior")]
    pub number_key: String,
    /// 次选/三选键候选不足时
    #[serde(default = "default_overflow_behavior")]
    pub select_key: String,
    /// 以词定字键候选词长度不足时
    #[serde(default = "default_overflow_behavior")]
    pub select_char_key: String,
}

fn default_overflow_behavior() -> String {
    "ignore".to_string()
}

impl Default for OverflowConfig {
    fn default() -> Self {
        Self {
            number_key: default_overflow_behavior(),
            select_key: default_overflow_behavior(),
            select_char_key: default_overflow_behavior(),
        }
    }
}

// ───────────────────────── ui（外观）─────────────────────────

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
    pub status: StatusIndicatorConfig,
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
    /// 自动隐藏：显示后超时无交互则淡出（默认关）。
    #[serde(default)]
    pub auto_hide: bool,
    /// 自动隐藏超时（秒，默认 5；下限 1 由协调器钳制）。
    #[serde(default = "default_toolbar_auto_hide_delay")]
    pub auto_hide_delay: u32,
}

impl Default for ToolbarConfig {
    fn default() -> Self {
        Self {
            visible: true,
            hide_in_fullscreen: true,
            auto_hide: false,
            auto_hide_delay: 5,
        }
    }
}

fn default_toolbar_auto_hide_delay() -> u32 {
    5
}

/// 状态提示气泡配置（[ui.status]，对齐 Go）：中英/标点/全半角/方案切换的瞬时气泡。
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
    /// 候选文本字号（默认 18；0 亦表示跟随主题 behavior.font_size）。
    #[serde(default)]
    pub font_size: f32,
    /// 字号跟随主题（默认开）：true 时忽略 font_size，用主题 behavior.font_size。
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
    /// 候选窗在光标上方时交换编码栏与候选栏位置（编码区沉底贴光标）。与 flip_when_above 正交，可叠加。
    #[serde(default)]
    pub swap_preedit_when_above: bool,
    /// 翻页栏并入编码栏行、右对齐显示（竖排省一行）。仅"非嵌入编码"（有独立编码栏）时生效。
    #[serde(default)]
    pub pager_in_preedit: bool,
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
            layout: "horizontal".to_string(),
            preedit_display: default_preedit_display(),
            hide_window: false,
            font_size: 18.0,
            font_size_follow_theme: true,
            pager_bar_display: String::new(),
            page_number_display: String::new(),
            max_chars: 16,
            index_labels: String::new(),
            flip_when_above: false,
            swap_preedit_when_above: false,
            pager_in_preedit: false,
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

    /// 用户配置的第 `i` 个序号槽位（0 基）：仅当用户显式设置了 index_labels 且槽位存在
    /// 时返回该字符，否则 None。供协调器裁决「用户 > 主题 > 默认」优先级（主题层在 None 时接手）。
    pub fn user_index_label(&self, i: usize) -> Option<String> {
        self.index_labels.chars().nth(i).map(|c| c.to_string())
    }

    /// 按 max_chars 截断候选显示文本（0=不限）。超出时截断并加省略号 `…`
    /// 提示"过长"（仅影响显示；上屏用完整原文，见 coordinator 候选下发）。
    pub fn truncate_display(&self, text: &str) -> String {
        if self.max_chars == 0 {
            return text.to_string();
        }
        let chars: Vec<char> = text.chars().collect();
        if chars.len() <= self.max_chars {
            text.to_string()
        } else {
            let head: String = chars[..self.max_chars].iter().collect();
            format!("{head}…")
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiThemeConfig {
    // 字段级缺省保持空：加载用户配置缺字段时回退 theme.txt（旧版迁移），不被强制成 default。
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub style: String,
}

// 手写 Default：仅供 Config::default()（getDefaults/恢复本页）给出有效初值，
// 与字段级 serde 缺省（空）解耦，避免影响加载期 theme.txt 迁移回退。
impl Default for UiThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            style: "system".to_string(),
        }
    }
}

/// 悬停提示配置（[ui.tooltip]）。原 ui.tooltip.{code,pinyin,chaizi,debug}.* 子表拍平为平铺字段
/// （三级上限：ui.tooltip.<字段>）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TooltipConfig {
    /// 提示延迟显示时间（毫秒）。
    #[serde(default = "default_tooltip_delay")]
    pub delay: i32,
    /// 编码提示（原 code.enabled）。默认开。
    #[serde(default = "default_true")]
    pub code_enabled: bool,
    /// 拼音提示（原 pinyin.enabled）。默认开。
    #[serde(default = "default_true")]
    pub pinyin_enabled: bool,
    /// 显示多音字所有读音（原 pinyin.heteronyms）。默认开。
    #[serde(default = "default_true")]
    pub pinyin_heteronyms: bool,
    /// 每字最多显示读音数（原 pinyin.max_readings，0=不限）。
    #[serde(default)]
    pub pinyin_max_readings: usize,
    /// 拆字提示（原 chaizi.enabled）。默认关。
    #[serde(default)]
    pub chaizi_enabled: bool,
    /// 调试提示（原 debug.enabled）。默认关。
    #[serde(default)]
    pub debug_enabled: bool,
}

fn default_tooltip_delay() -> i32 {
    200
}

impl Default for TooltipConfig {
    fn default() -> Self {
        Self {
            delay: default_tooltip_delay(),
            code_enabled: true,
            pinyin_enabled: true,
            pinyin_heteronyms: true,
            pinyin_max_readings: 0,
            chaizi_enabled: false,
            debug_enabled: false,
        }
    }
}

// ───────────────────────── input.phrase（短语前缀列举）─────────────────────────

/// 短语前缀列举配置（[input.phrase]，对齐 Go）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhraseConfig {
    /// 触发前缀导航列举的最小输入长度（原 min_prefix_length）。默认 2。
    #[serde(default = "default_phrase_min_prefix")]
    pub min_prefix: usize,
}

impl Default for PhraseConfig {
    fn default() -> Self {
        Self {
            min_prefix: default_phrase_min_prefix(),
        }
    }
}

fn default_phrase_min_prefix() -> usize {
    2
}

// ───────────────────────── stats（统计）─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub track_english: bool,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            track_english: true,
        }
    }
}

// ───────────────────────── compat / debug ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompatConfig {
    #[serde(default = "default_host_render_processes")]
    pub host_render_processes: Vec<String>,
}

fn default_host_render_processes() -> Vec<String> {
    vec!["SearchHost.exe".to_string()]
}

impl Default for CompatConfig {
    fn default() -> Self {
        Self {
            host_render_processes: default_host_render_processes(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugConfig {
    /// 日志级别。空字符串等同 `info`（生产默认）。
    /// 注意：`info` 级别日志不得包含用户输入内容、词库词条等隐私数据。
    #[serde(default)]
    pub log_level: String,
    /// 单个日志文件的大小上限（MB），超出后滚动。默认 10。
    #[serde(default = "default_log_max_size_mb")]
    pub log_max_size_mb: u64,
    /// 保留的旧日志文件数量上限。默认 5。
    #[serde(default = "default_log_max_files")]
    pub log_max_files: usize,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            log_level: String::new(),
            log_max_size_mb: default_log_max_size_mb(),
            log_max_files: default_log_max_files(),
        }
    }
}

fn default_log_max_size_mb() -> u64 {
    10
}

fn default_log_max_files() -> usize {
    5
}

// ───────────────────────── 共享 default 助手 ─────────────────────────

fn default_true() -> bool {
    true
}

fn default_numpad_behavior() -> String {
    "direct".to_string()
}

fn default_s2t_variant() -> String {
    "s2t".to_string()
}

fn default_toggle_toolbar() -> String {
    "ctrl+shift+\\".to_string()
}

fn default_open_settings() -> String {
    "ctrl+shift+]".to_string()
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
            schema: SchemaConfig::default(),
            input: InputConfig::default(),
            keys: KeysConfig::default(),
            ui: UiConfig::default(),
            stats: StatsConfig::default(),
            compat: CompatConfig::default(),
            debug: DebugConfig::default(),
        }
    }
}

impl Config {
    /// 三层合并加载：默认值 → data_dir/config.toml → 用户配置。
    ///
    /// 合并方式：把三层各自的 `toml::Value`（默认值序列化得到）深合并（表递归、标量/数组后者覆盖），
    /// 最后一次性反序列化为 `Config`。所有段都会被合并，不再静默丢弃；新增配置字段无需改合并代码。
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

    /// 系统预置配置的 TOML 值：代码默认(L1) ⊕ `data/config.toml`(L2)，**不含用户层(L3)**。
    ///
    /// 供 capability 的 `default` 来源——出厂默认 = L1⊕L2。config.toml 作为系统预置
    /// 可合法覆盖 L1（如 schema.active、compat.host_render_processes）。
    pub fn system_preset_value(data_dir: Option<&Path>) -> anyhow::Result<toml::Value> {
        let mut merged = toml::Value::try_from(Self::default())?;
        if let Some(data_dir) = data_dir {
            let sys_config = data_dir.join("config.toml");
            if let Some(v) = Self::read_toml_value(&sys_config) {
                merge_value(&mut merged, v);
            }
        }
        Ok(merged)
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

    /// 应用数据目录名：正式版 `WindInput`；dev 变体 `WindInputDev`
    /// （隔离调试与正式版的配置/缓存/日志，与管道后缀同源于运行时变体探测）。
    pub fn app_dir_name() -> &'static str {
        crate::variant::app_dir_name()
    }

    /// 用户配置目录（config.toml / userdata.redb / 词频 / shadow 置顶删词 / 用户词库）。
    /// - 便携模式：`<exe目录>/userdata/`
    /// - 正常模式：漫游 `%APPDATA%\WindInput[Dev]`（随用户在多设备间同步）
    pub fn user_config_dir() -> Option<PathBuf> {
        if crate::variant::is_portable() {
            crate::variant::portable_userdata_dir()
        } else {
            dirs::config_dir().map(|d| d.join(Self::app_dir_name()))
        }
    }

    /// 把单个配置项**部分合并**写入用户层 `config.toml`（%APPDATA%/WindInput/config.toml）。
    ///
    /// 只改 `path` 指定的项、保留用户文件里其它已有项，**不写入未改动的默认/系统段**——
    /// 用户层维持最小 diff，避免覆盖系统层/默认层的后续更新。
    /// 原子写（tmp + rename）。`path` 如 `["ui","candidate","preedit_display"]`。
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

    /// 运行时状态目录（state.toml：工具栏位置等本机状态）。
    /// 与 `local_dir()` 相同路径，独立命名便于语义区分。
    pub fn state_dir() -> Option<PathBuf> {
        Self::local_dir()
    }

    /// 本机状态目录（工具栏位置、日志、缓存等机器相关数据）。
    /// - 便携模式：`<exe目录>/userdata/`
    /// - 正常模式：`%LOCALAPPDATA%\WindInput[Dev]`（不随漫游同步）
    pub fn local_dir() -> Option<PathBuf> {
        if crate::variant::is_portable() {
            crate::variant::portable_userdata_dir()
        } else {
            dirs::data_local_dir().map(|d| d.join(Self::app_dir_name()))
        }
    }

    /// 缓存目录（%LOCALAPPDATA%\WindInput\cache）：词库 .wdb 等可重建产物。
    pub fn cache_dir() -> Option<PathBuf> {
        Self::local_dir().map(|d| d.join("cache"))
    }

    /// 日志目录。
    /// - 便携模式：`<exe目录>/userdata/logs`
    /// - 正常模式：`%LOCALAPPDATA%\WindInput[Dev]\logs`
    pub fn log_dir() -> Option<PathBuf> {
        if crate::variant::is_portable() {
            crate::variant::portable_userdata_dir().map(|d| d.join("logs"))
        } else {
            Self::local_dir().map(|d| d.join("logs"))
        }
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

    /// input.default 新增项与既有语义：state_scope 默认 global、remember 默认 false。
    #[test]
    fn input_default_state_scope_defaults() {
        let d = InputDefaultConfig::default();
        assert_eq!(d.state_scope, "global");
        assert!(!d.per_app_scope());
        assert!(!d.remember_last_state);
        // 缺字段的旧 config.toml 反序列化与 Default 一致。
        let parsed: InputDefaultConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.state_scope, "global");
        assert!(!parsed.remember_last_state);
        assert!(parsed.chinese_mode);
        // scope 解析大小写不敏感。
        let parsed: InputDefaultConfig = toml::from_str("state_scope = \"App\"").unwrap();
        assert!(parsed.per_app_scope());
    }

    #[test]
    fn set_nested_creates_overwrites_and_preserves() {
        let mut t = toml::Table::new();
        t.insert("keep".into(), toml::Value::String("x".into()));
        set_nested(
            &mut t,
            &["ui", "candidate", "preedit_display"],
            toml::Value::String("candidate_inline".into()),
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
                .get("preedit_display")
                .unwrap()
                .as_str(),
            Some("candidate_inline")
        );
        assert_eq!(
            t.get("schema").unwrap().get("active").unwrap().as_str(),
            Some("pinyin")
        );
        // 同路径覆盖
        set_nested(
            &mut t,
            &["ui", "candidate", "preedit_display"],
            toml::Value::String("candidate_top".into()),
        );
        assert_eq!(
            t.get("ui")
                .unwrap()
                .get("candidate")
                .unwrap()
                .get("preedit_display")
                .unwrap()
                .as_str(),
            Some("candidate_top")
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
    fn test_merge_keeps_input_s2t_compat_debug() {
        // 回归：deep-merge 必须保留各段（features 拆解后 s2t 归 input）
        let cfg = merged_with(
            "[input.s2t]\nenabled = true\nvariant = \"s2tw\"\n\
             [debug]\nlog_level = \"trace\"\n\
             [compat]\nhost_render_processes = [\"a.exe\"]\n",
        );
        assert!(cfg.input.s2t.enabled, "input.s2t.enabled 应被合并");
        assert_eq!(cfg.input.s2t.variant, "s2tw");
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
        // 旧合并漏掉的 input 子表字段（如 auto_pair）现应合并
        let cfg = merged_with("[input.auto_pair]\nchinese = false\nenglish = false\n");
        assert!(!cfg.input.auto_pair.chinese);
        assert!(!cfg.input.auto_pair.english);
    }

    #[test]
    fn test_tooltip_defaults_match_go() {
        let t = Config::default().ui.tooltip;
        assert_eq!(
            t.delay, 200,
            "delay 按 data/config.toml 预置默认 200（偏离 Go 的 100）"
        );
        assert!(t.code_enabled, "code 默认开");
        assert!(
            t.pinyin_enabled && t.pinyin_heteronyms,
            "pinyin 默认开+全读音"
        );
        assert_eq!(t.pinyin_max_readings, 0);
        assert!(!t.chaizi_enabled, "chaizi 默认关");
        assert!(!t.debug_enabled, "debug 默认关");
    }

    #[test]
    fn test_tooltip_merge_override() {
        let cfg = merged_with(
            "[ui.tooltip]\nchaizi_enabled = true\npinyin_heteronyms = false\npinyin_max_readings = 2\n",
        );
        assert!(cfg.ui.tooltip.chaizi_enabled);
        assert!(!cfg.ui.tooltip.pinyin_heteronyms);
        assert_eq!(cfg.ui.tooltip.pinyin_max_readings, 2);
        // 未指定字段保留默认
        assert!(cfg.ui.tooltip.code_enabled, "code 未指定应保留默认开");
        assert_eq!(cfg.ui.tooltip.delay, 200);
    }

    #[test]
    fn test_candidate_tuning_defaults_and_methods() {
        let c = Config::default().ui.candidate;
        assert_eq!(c.font_size, 18.0, "字号默认 18");
        assert!(c.font_size_follow_theme, "默认跟随主题");
        assert_eq!(c.max_chars, 16, "默认最大 16 字");
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
            "一二三四…",
            "截断到 4 字并加省略号"
        );
        assert_eq!(c.truncate_display("一二"), "一二", "不足不截");
    }

    #[test]
    fn test_user_index_label_optional() {
        // 用户显式设置：已配槽位返回 Some，越界返回 None（主题层可接手）。
        let cfg = merged_with("[ui.candidate]\nindex_labels = \"asdf\"\n");
        let c = cfg.ui.candidate;
        assert_eq!(c.user_index_label(0), Some("a".to_string()));
        assert_eq!(c.user_index_label(3), Some("f".to_string()));
        assert_eq!(c.user_index_label(4), None, "越界→None（让位主题/默认）");
        // 未配置：全 None，优先级完全交给主题/默认。
        let d = merged_with("").ui.candidate;
        assert_eq!(d.user_index_label(0), None);
    }

    #[test]
    fn pinyin_global_config_defaults() {
        let c = Config::default();
        assert!(c.schema.pinyin.show_code_hint);
        assert!(c.schema.pinyin.use_smart_compose);
        assert_eq!(c.schema.pinyin.separator, "auto");
        assert!(!c.schema.pinyin.fuzzy.enabled);
        assert!(!c.schema.pinyin.fuzzy.zh_z);
    }

    #[test]
    fn pinyin_global_merge_partial() {
        // 仅覆盖 [schema.pinyin.fuzzy] 的 enabled 和 zh_z，其余字段应保留默认值（深合并验证）
        let c = merged_with("[schema.pinyin.fuzzy]\nenabled = true\nzh_z = true\n");
        // 被覆盖字段：变为 true
        assert!(c.schema.pinyin.fuzzy.enabled, "enabled 应被覆盖为 true");
        assert!(c.schema.pinyin.fuzzy.zh_z, "zh_z 应被覆盖为 true");
        // 未覆盖的 fuzzy 字段：保留默认 false
        assert!(!c.schema.pinyin.fuzzy.ch_c, "ch_c 未覆盖，应保留默认 false");
        assert!(!c.schema.pinyin.fuzzy.sh_s, "sh_s 未覆盖，应保留默认 false");
        // 未覆盖的 pinyin 顶层字段：保留默认值
        assert!(
            c.schema.pinyin.show_code_hint,
            "show_code_hint 未覆盖，应保留默认 true"
        );
        assert!(
            c.schema.pinyin.use_smart_compose,
            "use_smart_compose 未覆盖，应保留默认 true"
        );
        assert_eq!(
            c.schema.pinyin.separator, "auto",
            "separator 未覆盖，应保留默认 auto"
        );
    }

    #[test]
    fn test_keys_defaults() {
        // keys 合并 hotkeys + 选择键，默认值需保留（[keys] 整表缺失走 Default）
        let k = Config::default().keys;
        assert_eq!(k.toggle_mode_keys, vec!["lshift", "rshift"]);
        assert_eq!(k.switch_engine, "ctrl+shift+e");
        assert_eq!(k.select_key_groups, vec!["semicolon_quote"]);
        assert_eq!(k.page_keys, vec!["pageupdown", "minus_equal"]);
        assert_eq!(k.overflow.number_key, "ignore");
    }

    #[test]
    fn test_schema_modes_and_input_groups() {
        let c = Config::default();
        // 模式三件套归 schema
        assert_eq!(c.schema.mix_modes.len(), 1, "默认一个快捷 mix");
        assert_eq!(c.schema.mix_modes[0].trigger_keys, vec!["semicolon"]);
        assert!(c.schema.quick_input.enabled);
        assert_eq!(c.schema.quick_input.decimal_places, 6);
        // input 子组
        assert!(
            c.input.punct.smart_after_digit,
            "punct.smart_after_digit 默认开"
        );
        assert_eq!(c.input.symbol.smart_timeout_ms, 500);
        assert!(c.input.temp_english.enabled && c.input.temp_english.show_candidates);
        assert_eq!(c.input.url.prefixes.len(), 5);
        // input.phrase / stats
        assert_eq!(c.input.phrase.min_prefix, 2);
        assert!(c.stats.enabled && c.stats.track_english);
    }

    #[test]
    fn system_preset_without_data_dir_equals_default() {
        let preset = Config::system_preset_value(None).unwrap();
        assert_eq!(preset, toml::Value::try_from(Config::default()).unwrap());
    }

    #[test]
    fn system_preset_applies_config_toml_overrides() {
        let data_dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../data"));
        let preset = Config::system_preset_value(Some(data_dir)).unwrap();
        let cfg: Config = preset.try_into().unwrap();
        // config.toml 作为 L2 预置覆盖了空的 code default
        assert_eq!(
            cfg.compat.host_render_processes,
            vec!["SearchHost.exe".to_string()]
        );
        assert_eq!(cfg.schema.active, "wubi86");
        // L1 code default 也含 SearchHost.exe；config.toml 的 L2 与之相同，合并后值不变。
        assert_eq!(
            Config::default().compat.host_render_processes,
            vec!["SearchHost.exe".to_string()]
        );
    }

    #[test]
    fn test_smart_method_default() {
        let cfg = SymbolConfig::default();
        assert_eq!(cfg.smart_method, SmartMethod::HoldComposition);
    }

    #[test]
    fn test_smart_method_serde_round_trip() {
        let toml = r#"
smart_mode = true
smart_method = "delete_replace"
"#;
        let cfg: SymbolConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.smart_method, SmartMethod::DeleteReplace);
        assert!(cfg.smart_mode);
    }

    #[test]
    fn test_smart_method_default_when_absent() {
        let toml = r#"smart_mode = true"#;
        let cfg: SymbolConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.smart_method, SmartMethod::HoldComposition);
    }

    #[test]
    fn top_commit_mode_default_is_direct_commit() {
        let c = InputConfig::default();
        assert_eq!(c.top_commit_mode, TopCommitMode::DirectCommit);
    }

    #[test]
    fn top_commit_mode_serde_round_trip() {
        let toml = r#"top_commit_mode = "direct_commit""#;
        let cfg: InputConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.top_commit_mode, TopCommitMode::DirectCommit);
    }

    #[test]
    fn top_commit_mode_absent_defaults_direct_commit() {
        let cfg: InputConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.top_commit_mode, TopCommitMode::DirectCommit);
    }

    /// 工具栏自动隐藏：默认关、超时 5 秒；空表反序列化与 Default 一致。
    #[test]
    fn toolbar_auto_hide_defaults() {
        let tb: ToolbarConfig = toml::from_str("").unwrap();
        assert!(!tb.auto_hide);
        assert_eq!(tb.auto_hide_delay, 5);
        let d = ToolbarConfig::default();
        assert!(!d.auto_hide);
        assert_eq!(d.auto_hide_delay, 5);
    }

    #[test]
    fn test_compat_host_render_processes_serde_default() {
        // 验证反序列化时缺少 host_render_processes 字段时，使用具名默认函数
        let cfg: CompatConfig = toml::from_str("").unwrap();
        assert_eq!(
            cfg.host_render_processes,
            vec!["SearchHost.exe".to_string()],
            "缺少 host_render_processes 时应返回默认值 [\"SearchHost.exe\"]"
        );
    }

    #[test]
    fn test_compat_host_render_processes_missing_in_merged_config() {
        // 验证在三层合并中，缺少 [compat] 段时反序列化得到正确的默认值
        let cfg = merged_with("");
        assert_eq!(
            cfg.compat.host_render_processes,
            vec!["SearchHost.exe".to_string()],
            "合并时缺少 [compat] 段时应保留默认值"
        );
    }

    #[test]
    fn test_compat_host_render_processes_explicit_in_merged_config() {
        // 验证显式指定 host_render_processes 时的覆盖
        let cfg = merged_with("[compat]\nhost_render_processes = [\"test.exe\"]\n");
        assert_eq!(
            cfg.compat.host_render_processes,
            vec!["test.exe".to_string()],
            "显式指定时应覆盖默认值"
        );
    }
}
