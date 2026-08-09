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
    /// **方案级按键功能表**（`[key_actions]`）：按键名 → 动词。
    ///
    /// 与 `[engine]` 平级而非放在 `[engine.codetable]` 下：按键功能与引擎类型无关，
    /// 拼音方案同样需要它。值域与语义见 [`crate::BoundAction`] 与
    /// `docs/design/schema-key-actions.md`。
    ///
    /// 空表 = 不覆盖任何键，各键照常走全局引导键链。**逐键合并**，不是整段替换：
    /// 方案文件内联段与 `schema_overrides/{id}.toml` 在 toml 层由 `merge_toml` 合并，
    /// 那里只能新增/覆盖、无法删除键——故「本方案禁用某个全局绑定」必须写显式
    /// `"none"`，不能靠从 override 里删掉那一行。
    ///
    /// 用 `BTreeMap`：顺序无语义（优先级由分派插入点决定），键唯一由类型保证。
    #[serde(default)]
    pub key_actions: std::collections::BTreeMap<String, String>,
    /// **overlay 激活面**（`[overlay]`）：本方案可被引导键/直达热键叠加激活时的呈现配置。
    ///
    /// **段存在即声明「我是 overlay 方案」**——这同时是实例集合的枚举依据
    /// （`EngineManager::overlay_modes`）。`None` = 普通方案，只能作 base 常驻使用。
    ///
    /// 不能复用 `[schema] hidden` 作这个判据：两者回答的是不同问题。`hidden` 是**展示**
    /// 属性（列不列进方案切换列表），本段是**行为**属性（有没有叠加进入/退出的生命周期）。
    /// 一个 overlay 方案完全可以不 hidden（作者想让它同时能常驻切换），一个 hidden 的
    /// 码表方案也可能只是 mix 成员、没有 overlay 生命周期。
    ///
    /// 见 `docs/redesign/overlay-mode-config.md`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlaySpec>,
}

/// overlay 激活面配置（`[overlay]`）。
///
/// ★ 这一段装的**不是**「这张码表是什么」（那是 `[engine.codetable]`），而是
/// 「这张码表**被叠加使用时**怎么表现」——三个字段的语义都依赖 overlay 生命周期：
/// `show_all_on_enter` 只在存在「进入这一刻」时才有意义；`candidate_layout` 的语义是
/// 「本模式期间覆盖全局、退出自动恢复」。段名 `overlay` 由此而来。
///
/// ⛔ **刻意不含 `trigger_keys` / `hotkey`**：引导键与直达热键统一住在 `keys.key_actions`
/// （全局）与方案文件 `[key_actions]`（按源方案分流）两张表里。在此再开一个入口字段
/// 就是第三个真相源，正是本轮重构要消除的东西。见设计文档 §2.2。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OverlaySpec {
    /// overlay 类别。当前只有 `"special"`（引导键特殊模式）；空 = 按 `special` 处理。
    ///
    /// 留这个字段是为消歧：`overlay` 在本仓有两个粒度——运行时状态（临拼/临英/mix/URL
    /// 也都是 overlay，但它们无宿主方案、配置只能待在 `input.*`）与方案文件的这一段
    /// （仅有宿主方案者）。段说「我可以被当 overlay 用」，本字段说「哪一类」。
    #[serde(default)]
    pub kind: String,
    /// 进入模式即展示候选：空编码（刚进入、尚未敲码）时枚举本方案码表首页候选
    /// （按 weight 降序），UI 按 per_page 分页浏览。默认 false（进入空白，敲码才出候选）。
    ///
    /// 面向快符/生僻字等**小符号表**的「进入即浏览」；大表会遍历全表取首 N 条、有开销，慎用。
    #[serde(default)]
    pub show_all_on_enter: bool,
    /// 进入本模式期间的候选布局（默认跟随全局）。每个 overlay 方案独立——快符表可竖排、
    /// 生僻字表可横排，互不影响。
    #[serde(default)]
    pub candidate_layout: crate::config::LayoutIntent,
    /// 本模式期间的注释模板覆盖（竖排）。三态见 [`crate::config::CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: crate::config::CommentTemplateOverride,
    /// 本模式期间的注释模板覆盖（横排）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: crate::config::CommentTemplateOverride,
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

/// 码表引擎配置（[engine.codetable]）：引擎固定参数 + **方案内联行为覆盖**。
///
/// 行为字段为 tri-state `Option`：`None`=回落全局 `schema.codetable`，`Some`=覆盖该字段。
/// schema 文件与 `schema_overrides/{id}.toml` 用**完全相同的段名/字段**表达行为——前者是作者
/// 内联基线，后者（设置页写入）经 `read_schema` 的 `merge_toml` 深合并到前者之上，最终由
/// `CodetableGlobal::resolved` 折叠到全局基线。故不再有独立的 `SchemeOverride` 平行路径
/// （见 docs/redesign/schema-config-layering.md）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeTableSpec {
    // ── 引擎固定参数 ──
    /// 最大码长（0=未设置，构建时回退 4）
    #[serde(default)]
    pub max_code_length: usize,
    /// 基础排序："weight"（默认）/ "natural"（字根序/inner_order）。见 docs/redesign/frequency.md。
    #[serde(default)]
    pub base_sort: String,
    /// 输入码字符集，如 "a-x" / "a-x/" / "a-z0-9"。空=回退全局/默认（`a-z`）。
    #[serde(default)]
    pub input_chars: String,
    /// 可作**首码**的字符集（`input_chars` 的子集）。空=与 `input_chars` 相同。
    ///
    /// 典型用途：数字要能作码元（打得出 `Win10`），但不能起头——空缓冲下的数字键是
    /// 选词/透传，若它同时是首码，用户就永远选不了「第 1 个候选」也拿不回原生数字输入。
    #[serde(default)]
    pub leading_chars: String,

    // ── 方案内联行为覆盖（None=回落全局 schema.codetable；Some=覆盖）──
    /// 顶码上屏（超满码长取前 N 码首选上屏）。
    #[serde(default)]
    pub top_code_commit: Option<bool>,
    /// 满码无候选时清空缓冲。
    #[serde(default)]
    pub clear_on_empty_max: Option<bool>,
    /// 满码唯一精确时自动上屏。
    #[serde(default)]
    pub auto_commit_at_full: Option<bool>,
    /// 自动上屏最短码长（隐藏参数；0=等于全码长）。
    #[serde(default)]
    pub auto_commit_min_len: Option<usize>,
    /// 标点触发上屏。
    #[serde(default)]
    pub punct_commit: Option<bool>,
    /// 显示编码提示。
    #[serde(default)]
    pub show_code_hint: Option<bool>,
    /// 精确匹配模式（关闭前缀匹配）。
    #[serde(default)]
    pub single_code_input: Option<bool>,
    /// 精确匹配空码补全。
    #[serde(default)]
    pub single_code_complete: Option<bool>,
    /// z 键重复输入。
    #[serde(default)]
    pub z_key_repeat: Option<bool>,
    /// z 键功能（`""`/`none` / `temp_pinyin` / `temp_english` / `mix:<id>` / `special:<id>`）。
    ///
    /// 方案级才有意义：z 能否借作引导键取决于这张码表里它是不是死码。
    /// 值域与语义见 `wind_config::config::BoundAction`。
    #[serde(default)]
    pub z_key_action: Option<String>,
    /// 方案级调频覆盖（`[engine.codetable.frequency]`）。
    ///
    /// 缺省 = 整段跟随基线。特殊方案的基线是内置默认（不继承全局 `schema.codetable`，
    /// 见 `EngineManager::codetable_baseline`），普通方案的基线是全局段。
    #[serde(default)]
    pub frequency: Option<CodeTableFrequencySpec>,
}

/// 方案级调频覆盖（`[engine.codetable.frequency]`），**逐字段稀疏**。
///
/// 每个字段都是 `Option`：给了就覆盖基线，没给就跟随。整段缺省 = 全部跟随。
///
/// 存在的理由是「同一台机器上不同码表的调频诉求本就不同」——快符表要的是稳定顺序
/// （作者精心排过），生僻字表要的是学习，五笔要的是简码位保护。此前这些只有一份全局值。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CodeTableFrequencySpec {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `"top"` / `"step"` / `"position"`。
    #[serde(default)]
    pub strategy: Option<String>,
    /// `"none"` / `"single"` / `"all"`；仅 `position` 生效。
    #[serde(default)]
    pub promote_prefix: Option<String>,
    /// 衰减半衰期（小时），`0` = 内置默认；仅 `position` 生效。
    #[serde(default)]
    pub half_life: Option<f64>,
    /// 全码位（码长 ≥ 4）首选保护。
    #[serde(default)]
    pub protect_top_n: Option<usize>,
    #[serde(default)]
    pub protect_top_n_len1: Option<usize>,
    #[serde(default)]
    pub protect_top_n_len2: Option<usize>,
    #[serde(default)]
    pub protect_top_n_len3: Option<usize>,
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
    /// 权重归一化参数（[dictionaries.weight_spec]）。
    ///
    /// **当前未接线**：仅作为词库权重分布的事实记录供设计者查阅（如 pinyin 方案记 median=200，
    /// 是 `pinyin/mod.rs` COMPLETION_FAR_WEIGHT_FLOOR 取值的依据）。跨库权重归一化尚未实现——
    /// 现阶段用 `default_weight`（整库定档）+ `base_order`（硬分档）手工校准。
    #[serde(default)]
    pub weight_spec: Option<WeightSpec>,
    /// 该词库的**层级基序档位**（小整数）：排序时作为独立层级（weight 之后、natural_order 之前）。
    /// 等权/`base_sort=natural` 时决定库间先后——设计者配 0/1/2…（如给扩展库配 1 排到主库 0 之后），
    /// 与词库条目数无关。默认 0。系统词库建议取 `>=0`（负值会与用户/临时词层的默认档交错）。
    #[serde(default)]
    pub base_order: i32,
    /// 默认权重（可选）：设置后**覆盖本库所有条目的权重**。用于**无权重的附加库**——与带权重
    /// 主库合并、按权重排序时让其条目落在设计者选定的权重档，而非 weight=0 全部沉底。默认 None=用自身权重。
    #[serde(default)]
    pub default_weight: Option<i32>,
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
