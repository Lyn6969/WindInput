//! 输入方案定义（统一富 Schema）
//!
//! 与 Go 版本 `wind_input/internal/schema/` 对齐，但合理精简：只保留实际有意义的字段，
//! tri-state 用 `Option<bool>`（区分"未设置/false"），剔除仅为临时兼容的遗留。
//!
//! 本类型是**唯一**的方案表示——取代 wind-engine 早期私有的 `SchemaFile`。
//! 字段对齐真实 `data/schemas/*.schema.toml`（码表/拼音/混输/双拼）。

use serde::{Deserialize, Serialize};

/// 完整方案定义
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schema {
    #[serde(default)]
    pub schema: SchemaInfo,
    #[serde(default)]
    pub engine: EngineSpec,
    #[serde(default)]
    pub dictionaries: Vec<DictSpec>,
    #[serde(default)]
    pub learning: LearningSpec,
    #[serde(default)]
    pub encoder: Option<EncoderSpec>,
}

/// 方案元信息（[schema]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemaInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub icon_label: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub description: String,
}

/// 引擎配置（[engine]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineSpec {
    /// "pinyin" / "codetable" / "mixed"（用 String 容忍未知/缺省，由 Schema 方法判定）
    #[serde(rename = "type", default)]
    pub engine_type: String,
    #[serde(default)]
    pub filter_mode: String,
    #[serde(default)]
    pub codetable: CodeTableSpec,
    #[serde(default)]
    pub pinyin: PinyinSpec,
    #[serde(default)]
    pub mixed: MixedSpec,
}

/// 码表引擎配置（[engine.codetable]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeTableSpec {
    /// 最大码长（0=未设置，构建时回退 4）
    #[serde(default)]
    pub max_code_length: usize,
    /// 满码唯一精确时自动上屏（tri-state；未设置时回退 legacy auto_commit_unique）
    #[serde(default)]
    pub auto_commit_at_full: Option<bool>,
    /// 自动上屏最短码长（0=跟随 max_code_length）
    #[serde(default)]
    pub auto_commit_min_len: usize,
    /// 混输时有拼音候选则否决自动上屏
    #[serde(default)]
    pub auto_commit_block_on_pinyin: Option<bool>,
    /// 满码无候选时清空
    #[serde(default)]
    pub clear_on_empty_max: bool,
    /// 五码顶字上屏
    #[serde(default)]
    pub top_code_commit: bool,
    /// 标点触发上屏
    #[serde(default)]
    pub punct_commit: bool,
    /// 显示编码提示
    #[serde(default)]
    pub show_code_hint: bool,
    /// 精确匹配模式（关闭前缀）
    #[serde(default)]
    pub single_code_input: bool,
    /// 精确模式空码补全
    #[serde(default)]
    pub single_code_complete: bool,
    /// Z 键重复上屏
    #[serde(default)]
    pub z_key_repeat: bool,
    /// 临时拼音（码表方案下触发键临时切拼音反查）
    #[serde(default)]
    pub temp_pinyin: TempPinyinSpec,

    // ── 排序（两层，见 docs/redesign/frequency.md）──
    /// 基础排序："weight"（默认）/ "natural"（字根序/inner_order）
    #[serde(default)]
    pub base_sort: String,
    /// 用户词频开关（叠加在 base_sort 之上）
    #[serde(default)]
    pub user_frequency: bool,
    /// 词频应用策略："top"（置前，默认）/ "step"（逐次前进，预留未实现）
    #[serde(default)]
    pub freq_strategy: String,

    // ── 码元字符集（见 docs/redesign/config-schema.md §3b）──
    /// 输入码字符集，如 "a-x" / "a-x/" / "a-z"。空=回退全局/默认。
    #[serde(default)]
    pub input_chars: String,

    // ── 前缀/权重（未来阶段 B 接入，对齐 engine.md §2）──
    #[serde(default)]
    pub weight_mode: String,
    #[serde(default)]
    pub prefix_mode: String,
    #[serde(default)]
    pub charset_preference: String,
    #[serde(default)]
    pub short_code_first: Option<bool>,

    /// legacy：旧版满码唯一上屏开关（被 auto_commit_at_full 取代，仅作回退读取）
    #[serde(default)]
    pub auto_commit_unique: bool,
    /// legacy：旧版单一排序模式（被 base_sort+user_frequency 取代，仅作回退读取）
    #[serde(default)]
    pub candidate_sort_mode: String,
}

/// 临时拼音（[engine.codetable.temp_pinyin]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TempPinyinSpec {
    #[serde(default)]
    pub enabled: bool,
    /// 目标拼音方案 id（空=回退 "pinyin"）
    #[serde(default)]
    pub schema: String,
}

/// 拼音引擎配置（[engine.pinyin]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinyinSpec {
    /// "full"（全拼）/ "shuangpin"（双拼）
    #[serde(default)]
    pub scheme: String,
    #[serde(default)]
    pub show_code_hint: bool,
    #[serde(default)]
    pub use_smart_compose: bool,
    #[serde(default)]
    pub candidate_order: String,
    /// 双拼布局（自定义映射，见 config-schema.md §3b）
    #[serde(default)]
    pub shuangpin: ShuangpinSpec,
    #[serde(default)]
    pub fuzzy: FuzzySpec,
}

/// 双拼布局（[engine.pinyin.shuangpin]）
///
/// `layout` 引用一个布局 id（内置预置或用户自定义映射文件），引擎据此加载键位→声母/韵母
/// 映射与所用符号；**不在代码内硬编码具体方案**。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShuangpinSpec {
    #[serde(default)]
    pub layout: String,
}

/// 模糊音（[engine.pinyin.fuzzy]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FuzzySpec {
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

/// 混输引擎配置（[engine.mixed]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MixedSpec {
    #[serde(default)]
    pub primary_schema: String,
    #[serde(default)]
    pub secondary_schema: String,
    /// 拼音生效最小输入长度（0=未设置，构建时回退 2）
    #[serde(default)]
    pub min_pinyin_length: usize,
    /// 码表精确匹配提权基线（0=未设置，构建时回退 10_000_000）
    #[serde(default)]
    pub codetable_weight_boost: i32,
    #[serde(default)]
    pub show_source_hint: bool,
    #[serde(default)]
    pub z_key_repeat: bool,
    #[serde(default)]
    pub enable_english: Option<bool>,
    #[serde(default)]
    pub pinyin_only_overflow: Option<bool>,
    #[serde(default)]
    pub top_code_override_pinyin: Option<bool>,
}

/// 词库规格（[[dictionaries]]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DictSpec {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub path: String,
    /// "rime_codetable" / "rime_pinyin" / "codetable"（空=回退 rime_codetable）
    #[serde(rename = "type", default)]
    pub dict_type: String,
    /// 主词库
    #[serde(default)]
    pub default: bool,
    /// 非默认但默认启用的附加库（tri-state，nil=true）
    #[serde(default)]
    pub default_enabled: Option<bool>,
    /// 用户覆盖启用（tri-state，nil=继承 default_enabled）
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 权重仅表示同码内排序序号
    #[serde(default)]
    pub weight_as_order: bool,
    /// 权重归一化参数（[dictionaries.weight_spec]）
    #[serde(default)]
    pub weight_spec: Option<WeightSpec>,
}

/// 权重归一化（[dictionaries.weight_spec]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WeightSpec {
    #[serde(default)]
    pub median: i64,
    #[serde(default)]
    pub max: i64,
    #[serde(default)]
    pub min: i64,
    /// "linear" / "log"
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub target: i64,
}

/// 学习配置（[learning]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSpec {
    /// unigram 语言模型路径（相对 schemas 目录），拼音长句打分用
    #[serde(default)]
    pub unigram_path: String,
    #[serde(default)]
    pub temp_max_entries: usize,
    #[serde(default)]
    pub temp_promote_count: usize,
    #[serde(default)]
    pub auto_learn: AutoLearnSpec,
    #[serde(default)]
    pub auto_phrase: AutoPhraseSpec,
    #[serde(default)]
    pub freq: FreqSpec,
}

/// 自动造词（拼音，[learning.auto_learn]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutoLearnSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub count_threshold: u32,
    #[serde(default)]
    pub min_word_length: usize,
    #[serde(default)]
    pub weight_delta: i32,
    #[serde(default)]
    pub add_weight: i32,
}

/// 自动造词（码表连续单字，[learning.auto_phrase]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutoPhraseSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub min_phrase_len: usize,
    #[serde(default)]
    pub max_phrase_len: usize,
    #[serde(default)]
    pub add_weight: i32,
    #[serde(default)]
    pub weight_delta: i32,
    #[serde(default)]
    pub count_threshold: u32,
    #[serde(default)]
    pub idle_timeout_ms: u64,
}

/// 用户词频（[learning.freq]，见 docs/redesign/frequency.md——衰减参数，拼音用）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FreqSpec {
    #[serde(default)]
    pub enabled: bool,
    /// 锁定码表原始前 N 位（仅纯码表生效）
    #[serde(default)]
    pub protect_top_n: usize,
    /// 半衰期（小时，拼音衰减；0=用 store 默认）
    #[serde(default)]
    pub half_life: f64,
    /// base 系数（0=用 store 默认）
    #[serde(default)]
    pub base_scale: f64,
    /// 最近使用峰值（0=用 store 默认）
    #[serde(default)]
    pub recency_peak: f64,
}

/// 造词编码规则（[encoder]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncoderSpec {
    #[serde(default)]
    pub max_word_length: usize,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub rules: Vec<EncoderRule>,
}

/// 单条编码规则（[[encoder.rules]]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncoderRule {
    /// 精确匹配字数
    #[serde(default)]
    pub length_equal: usize,
    /// 字数范围 [min, max]
    #[serde(default)]
    pub length_in_range: Vec<usize>,
    /// 拆字公式，如 "AaAbBaBb"
    #[serde(default)]
    pub formula: String,
}

impl Schema {
    /// 是否为拼音类型引擎（engine.type 缺省时依据默认词典类型判定）
    pub fn is_pinyin(&self) -> bool {
        match self.engine.engine_type.to_lowercase().as_str() {
            "pinyin" => true,
            "codetable" | "mixed" => false,
            _ => {
                let default = self
                    .dictionaries
                    .iter()
                    .find(|d| d.default)
                    .or_else(|| self.dictionaries.first());
                matches!(default, Some(d) if d.dict_type == "rime_pinyin")
            }
        }
    }

    /// 是否为混输方案
    pub fn is_mixed(&self) -> bool {
        self.engine.engine_type.eq_ignore_ascii_case("mixed")
    }

    /// 该方案当前是否受支持（双拼 scheme≠full 暂未实现，先排除）
    pub fn is_supported(&self) -> bool {
        if self.is_pinyin() {
            let s = self.engine.pinyin.scheme.to_lowercase();
            return s.is_empty() || s == "full";
        }
        true
    }
}

impl DictSpec {
    /// 是否应加载（主词库，或 default_enabled 默认启用的附加库；enabled 用户覆盖优先）
    pub fn is_enabled(&self) -> bool {
        if let Some(e) = self.enabled {
            return e || self.default;
        }
        self.default || self.default_enabled.unwrap_or(false)
    }

    /// 词典类型（空时回退 rime_codetable）
    pub fn effective_type(&self) -> &str {
        if self.dict_type.is_empty() {
            "rime_codetable"
        } else {
            &self.dict_type
        }
    }
}
