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
    /// 隐藏方案：不在设置页「方案管理」列出，也不进循环切换。
    /// 用于内部 / 被引用的词库配置方案（如 english——仅供临时英文 / 融合英文候选懒加载）。
    #[serde(default)]
    pub hidden: bool,
}

/// 引擎配置（[engine]）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineSpec {
    /// "pinyin" / "codetable" / "mixed"（用 String 容忍未知/缺省，由 Schema 方法判定）
    #[serde(rename = "type", default)]
    pub engine_type: String,
    #[serde(default)]
    pub codetable: CodeTableSpec,
    #[serde(default)]
    pub pinyin: PinyinSpec,
    #[serde(default)]
    pub mixed: MixedSpec,
    /// 拆字（字根分解）反查与字根字体（码表方案的悬停提示用）。
    #[serde(default)]
    pub chaizi: ChaiziSpec,
}

/// 拆字配置（[engine.chaizi]）。供悬停提示的"如何输入"反查与 PUA 字根字符渲染。
/// 路径相对 `data/schemas/`。三字段全空=该方案无拆字提示。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChaiziSpec {
    /// 拆字库路径（`字\t字根\t编码` 文本），相对 schemas 目录。
    #[serde(default)]
    pub db_path: String,
    /// 字根字体 TTF 文件路径，相对 schemas 目录（注册进 DirectWrite 自定义字体集）。
    #[serde(default)]
    pub font_path: String,
    /// 字根字体的 DirectWrite 家族名（取自 TTF name 表，如 "黑体字根"）；渲染时按此名引用。
    #[serde(default)]
    pub font_family: String,
}

impl ChaiziSpec {
    /// 是否配置了拆字（至少有库或字体路径）。
    pub fn is_configured(&self) -> bool {
        !self.db_path.is_empty() || !self.font_path.is_empty()
    }
}

/// 码表引擎配置（[engine.codetable]）。**仅引擎固定参数**；
/// 行为/调频/造词/临时拼音等用户可配项已上移至全局 `schema.codetable` 与 `schema_overrides`
/// （见 docs/redesign/schema-config-layering.md）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeTableSpec {
    /// 最大码长（0=未设置，构建时回退 4）
    #[serde(default)]
    pub max_code_length: usize,
    /// 基础排序："weight"（默认）/ "natural"（字根序/inner_order）。见 docs/redesign/frequency.md。
    #[serde(default)]
    pub base_sort: String,
    /// 输入码字符集，如 "a-x" / "a-x/" / "a-z"。空=回退全局/默认。
    #[serde(default)]
    pub input_chars: String,
}

/// 拼音引擎配置（[engine.pinyin]）。
/// 注：show_code_hint / use_smart_compose / candidate_order / fuzzy 已上移为全局 [pinyin]。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinyinSpec {
    /// "full"（全拼）/ "shuangpin"（双拼）
    #[serde(default)]
    pub scheme: String,
    /// 双拼布局 id（引用 data/schemas/shuangpin/<layout>.toml）
    #[serde(default)]
    pub shuangpin: ShuangpinSpec,
    /// unigram 语言模型路径（相对 schemas 目录），拼音长句 Viterbi 打分用。
    /// 属解码/引擎职责（非用户学习），故置于 [engine.pinyin] 而非 [learning]。
    #[serde(default)]
    pub unigram_path: String,
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

/// 混输引擎配置（[engine.mixed]）。**仅引擎固定参数**（方案构成 + 内部权重基线）；
/// 融合策略（show_source_hint/enable_english/min_pinyin_length 等）已上移至全局 `schema.mix`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MixedSpec {
    #[serde(default)]
    pub primary_schema: String,
    #[serde(default)]
    pub secondary_schema: String,
    /// 码表精确匹配提权基线（0=未设置，构建时回退 10_000_000）
    #[serde(default)]
    pub codetable_weight_boost: i32,
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
    /// 该词库候选的 natural_order **基偏移**：合并时 `natural_order += base_order`。等权/
    /// `base_sort=natural` 时决定库间先后。设计者显式配置（如 50000 把该扩展库整体压到基础库后）。
    /// 默认 0 = 不偏移（未配置时各库不强制分带，取代旧的按注册顺序自动偏移）。
    #[serde(default)]
    pub base_order: i32,
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

/// 方案覆盖（`schema_overrides/{id}.toml` 中的**行为覆盖段**）。
///
/// 仅码表方案有行为覆盖；拼音、混输无 override。词库启停（`[[dictionaries]] enabled`）与
/// 双拼布局（`[engine.pinyin.shuangpin] layout`）继续走 `Schema` 深合并，不在此结构。
/// 见 docs/redesign/schema-config-layering.md §3/§4。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchemeOverride {
    #[serde(default)]
    pub codetable: Option<CodetableOverride>,
}

/// 码表方案行为覆盖（`schema_overrides/{id}.toml` 的 `[codetable]` 段）。
///
/// `enabled` 为**总开关**：为 false 或缺省时整段忽略，逐字段回落全局 `schema.codetable`；
/// 为 true 时各 `Some(_)` 字段覆盖全局，`None` 仍回落全局。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodetableOverride {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub top_code_commit: Option<bool>,
    #[serde(default)]
    pub clear_on_empty_max: Option<bool>,
    #[serde(default)]
    pub auto_commit_at_full: Option<bool>,
    #[serde(default)]
    pub auto_commit_min_len: Option<usize>,
    #[serde(default)]
    pub punct_commit: Option<bool>,
    #[serde(default)]
    pub show_code_hint: Option<bool>,
    #[serde(default)]
    pub single_code_input: Option<bool>,
    #[serde(default)]
    pub single_code_complete: Option<bool>,
    #[serde(default)]
    pub z_key_repeat: Option<bool>,
}

impl SchemeOverride {
    /// 从 `schema_overrides/{id}.toml` 的 TOML 值解析行为覆盖段（容错：解析失败返回默认空覆盖）。
    pub fn from_toml(value: &toml::Value) -> Self {
        value.clone().try_into().unwrap_or_default()
    }
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

    /// 该方案当前是否受支持（全拼/双拼均支持）
    pub fn is_supported(&self) -> bool {
        if self.is_pinyin() {
            let s = self.engine.pinyin.scheme.to_lowercase();
            return s.is_empty() || s == "full" || s == "shuangpin";
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuangpin_is_supported() {
        let mut s = Schema::default();
        s.engine.engine_type = "pinyin".into();
        s.engine.pinyin.scheme = "shuangpin".into();
        assert!(s.is_supported());
    }

    #[test]
    fn pinyin_spec_ignores_removed_fields() {
        let toml_str = r#"
scheme = "shuangpin"
show_code_hint = true
fuzzy = { enabled = true, zh_z = true }
[shuangpin]
layout = "xiaohe"
"#;
        let spec: PinyinSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(spec.scheme, "shuangpin");
        assert_eq!(spec.shuangpin.layout, "xiaohe");
    }
}
