//! 配置系统：三层合并（代码默认值、系统配置、用户配置）
//!
//! 与 Go 版本 `wind_input/pkg/config/config.go` 对齐。
//! 配置文件为 TOML 格式，三层合并：默认值 → data/config.toml → %APPDATA%/WindInput/config.toml
//!
//! 顶级域（"正交大类"准则，详见 SETTINGS_REVAMP_PLAN.md / docs/config-key-migration.md）：
//! schema(方案+pinyin+模式) / input(输入行为，含 default 启动默认 / phrase 短语) /
//! keys(全部按键) / ui(外观) / stats(统计) / debug。
//!
//! 按进程名的兼容性规则（HostRender 白名单、caret 定位等）不在这里，见
//! `app_compat.rs`（独立的 `compat.toml` 文件，字段级合并，键名不受本文件三层合并约束）。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

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

/// 在 TOML 值里按 `path` 逐级取值（任一级缺失或非表则 `None`）。
/// 供 [`Config::set_user_value`] 与出厂默认（L1⊕L2）比对用。
fn get_nested<'a>(root: &'a toml::Value, path: &[&str]) -> Option<&'a toml::Value> {
    let mut cur = root;
    for k in path {
        cur = cur.as_table()?.get(*k)?;
    }
    Some(cur)
}

/// 在 TOML 表里按 `path` 删除叶子，并回收因此变空的中间表（避免留下 `[schema.mix]` 这类空段）。
/// 返回是否真的删掉了东西。供用户层「与默认相同即不落盘」的收口用。
fn remove_nested(table: &mut toml::Table, path: &[&str]) -> bool {
    if path.is_empty() {
        return false;
    }
    if path.len() == 1 {
        return table.remove(path[0]).is_some();
    }
    let Some(toml::Value::Table(t)) = table.get_mut(path[0]) else {
        return false;
    };
    let removed = remove_nested(t, &path[1..]);
    if removed && t.is_empty() {
        table.remove(path[0]);
    }
    removed
}

/// 从 `root`（用户层）删除所有与 `preset`（出厂默认 L1⊕L2）取值相同的叶子键，返回删除数。
///
/// 纯函数、不碰文件系统：[`Config::prune_user_config`] 负责 IO，本函数负责判定，
/// 单测得以在不触碰真实 `%APPDATA%` 的前提下验证「清理前后三层合并结果不变」这条不变量。
///
/// **两道保险，缺一不可**：
/// 1. `is_known_key` —— 只碰注册表登记过的键。这排除掉两类绝不能删的东西：**废弃键**（清理它们
///    是另一件事，必须走显式名单，绝不能靠「preset 里没有」来推断）、以及 `Map`/`StructList`
///    类型键的**下钻子路径**（`input.punct.custom_mappings` 整体才是一个配置项，
///    `collect_leaf_paths` 却会切出 `...custom_mappings."'1"` 这种伪键——删单条是错的语义）。
/// 2. 值必须与 preset 逐一相等（`get_nested` 两侧都取到才比）。
fn prune_redundant(root: &mut toml::Value, preset: &toml::Value) -> usize {
    let mut leaves = Vec::new();
    collect_leaf_paths(root, &mut Vec::new(), &mut leaves);
    let redundant: Vec<Vec<String>> = leaves
        .into_iter()
        .filter(|p| crate::config_schema::is_known_key(&p.join(".")))
        .filter(|p| {
            let refs: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
            get_nested(root, &refs)
                .zip(get_nested(preset, &refs))
                .is_some_and(|(user, default)| user == default)
        })
        .collect();
    let toml::Value::Table(t) = root else {
        return 0;
    };
    let mut removed = 0usize;
    for p in &redundant {
        let refs: Vec<&str> = p.iter().map(|s| s.as_str()).collect();
        if remove_nested(t, &refs) {
            removed += 1;
        }
    }
    removed
}

/// **已退役的配置键**：结构体里已经没有它们，`load()` 时被 serde 静默丢弃。
///
/// 但它们会在用户层 `config.toml` 里**永久留存**——写回走的是原始 `toml::Value`
/// （见 [`Config::set_user_value`]），从不经过类型化结构体，未知键因此既不会被读取、
/// 也不会被删除。留着只有坏处：用户翻开配置看见 `enable_english = true`，会以为它还在
/// 起作用，而实际早已无人读取。
///
/// **只能用显式名单**，不能改成「凡未登记键一律删」——`input.punct.custom_mappings.<字符>`
/// 这类 `Map` 子路径同样不在注册表里，一刀切会把用户的自定义标点映射删光。这也正是
/// [`prune_redundant`] 用 `is_known_key` 把未登记键整体排除在外的原因。
const RETIRED_KEYS: &[&[&str]] = &[
    // 与 `mix_modes.members` 构成双真相源，已废弃：英文候选的开关只看 members 里有没有
    // `english`。⚠️ **不是** `schema.mix.enable_english` —— 那个还活着，是混输引擎
    // （`wubi86_pinyin` 这类方案）混入英文词库候选的开关，两者只是名字像。
    &["schema", "quick_input", "enable_english"],
    // 从未被任何逻辑读取过，关掉不产生任何效果（曾被误当作快捷输入的总开关）。
    // 真正的「禁用快捷输入」＝把 quick_mix 的 trigger_keys 清空。
    &["schema", "quick_input", "enabled"],
    // 随英文段独立迁至 `schema.english.frequency.code_scope`。**不做值迁移**：该键
    // 从未随任何版本发布到用户手里（接进设置页的改动与本次迁移在同一个未发布版本内），
    // 且新旧默认值都是 "candidate"，能读到它的只有开发期配置。
    &["schema", "codetable", "frequency", "english_code_scope"],
];

/// 从用户层删除 [`RETIRED_KEYS`] 里的退役键，返回删除数。
///
/// 与 [`prune_redundant`] 同一条不变量：**清理前后 `load()` 的结果逐键完全相同**——
/// 这些键本来就已经被 serde 丢弃，删掉不改变任何生效值。
/// 幂等；`remove_nested` 会顺带回收变空的父表（`[schema.quick_input]` 只剩这两个键时整段消失）。
fn prune_retired(root: &mut toml::Value) -> usize {
    let toml::Value::Table(t) = root else {
        return 0;
    };
    RETIRED_KEYS
        .iter()
        .filter(|path| remove_nested(t, path))
        .count()
}

/// 收集 TOML 值里所有叶子路径（表递归；数组/标量视为叶子，不下钻）。
///
/// 数组**必须**当叶子：`schema.mix_modes` / `keys.page_keys` 这类整体就是一个配置项，
/// 下钻进数组元素会切出无法用 `path` 表达、也无法与出厂默认逐项比对的伪键。
fn collect_leaf_paths(v: &toml::Value, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        toml::Value::Table(t) if !t.is_empty() => {
            for (k, sub) in t {
                prefix.push(k.clone());
                collect_leaf_paths(sub, prefix, out);
                prefix.pop();
            }
        }
        _ => out.push(prefix.clone()),
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
    /// 全局英文配置（英文方案自身的行为与调频；不再共用码表那套）。
    #[serde(default)]
    pub english: EnglishGlobal,
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
            english: EnglishGlobal::default(),
            quick_input: QuickInputConfig::default(),
            special_modes: Vec::new(),
            mix_modes: default_mix_modes(),
        }
    }
}

/// 全局英文配置（[schema.english]）。
///
/// 英文自 0.114 起是可切换方案，行为不再挂靠码表段——那是历史包袱：英文引擎复用了
/// 码表的重排路径，配置就顺手挂在了 `schema.codetable` 下，于是纯码表用户的「上屏行为」
/// 里混着只对英文生效的项，而英文用户改调频策略又会连带改掉五笔的。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct EnglishGlobal {
    #[serde(default)]
    pub frequency: EnglishFrequency,
    /// 英文方案下上屏一个词后**再补一个空格**。
    ///
    /// 英文是词间带空格的语言，连续打词时每次上屏都要多按一次空格。开启后由输入法补上。
    ///
    /// 生效范围（消费点见 `english_appends_space` / `english_space_enabled`）：
    /// - **所有选中方式**——空格 / 数字键 / 次三选键 / 修饰键选词 / 鼠标点选；
    /// - **空格上屏原码**（打了词库里没有的词）；
    /// - **不含**回车上屏原码（终结性动作）、标点键顶屏（会得到 `hello ,`）、顶码。
    #[serde(default)]
    pub commit_space: bool,
}

/// 英文调频（[schema.english.frequency]）。
///
/// 不 derive `Eq`：`half_life` 是 f64。与 `CodetableFrequency` / `PinyinFrequency` 一致。
///
/// **没有 `protect_top_n*`**：那组是「简码位首选保护」，判据是本次输入的码长——英文
/// 没有简码位这回事，一个 `a` 后面跟的是几万个词而不是钦定首选，照搬过来只会锁死
/// 前几位不让调频。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnglishFrequency {
    #[serde(default)]
    pub enabled: bool,
    /// `"top"` / `"step"` / `"position"`，默认 `"position"`。
    ///
    /// **与码表默认不同**：英文候选几乎全是前缀匹配，`top`/`step` 那种「用过一次即整体
    /// 跳到没用过的那批之前」在这里过于激进——误选一次就把词顶到很显眼的位置且不衰减。
    /// `position` 每次只前移一半、久不用会回落，更适合前缀为主的场景。
    #[serde(default = "default_english_freq_strategy")]
    pub strategy: String,
    /// 前缀补全候选参与位置提升的范围；**仅 `strategy = "position"` 时生效**。
    ///
    /// 英文默认 `"all"`：它的候选**本来就几乎全是前缀补全**（打 `hel` 出 `hello`），
    /// 收窄到 `single` 等于把调频关掉大半。
    #[serde(default = "default_codetable_promote_prefix")]
    pub promote_prefix: String,
    /// 衰减半衰期（小时），`0` = 内置默认 72 小时；仅 `position` 策略生效。
    #[serde(default)]
    pub half_life: f64,
    /// **词频记账码口径**（`"candidate"` / `"input"`，默认 `"candidate"`）。
    ///
    /// 原 `schema.codetable.frequency.english_code_scope`，随英文段独立迁到这里。
    ///
    /// | 取值 | 打 `hel` 选 `hello` 记成 | 之后打 `he` |
    /// |---|---|---|
    /// | `"candidate"`（默认） | `(hello, hello)` | **也受益**（跨码位共享） |
    /// | `"input"` | `(hel, hello)` | 不受益（码位独立） |
    ///
    /// ⚠️ 本项**按候选来源生效，不按当前方案**——混输方案里混进来的英文候选同样读它。
    /// 故 `EngineManager::freq_settings` 的**每个分支**都要从这里取值，不能只在
    /// 「当前是英文方案」时读。
    #[serde(default = "default_english_code_scope")]
    pub code_scope: String,
}

fn default_english_freq_strategy() -> String {
    "position".to_string()
}

impl Default for EnglishFrequency {
    fn default() -> Self {
        Self {
            enabled: false,
            strategy: default_english_freq_strategy(),
            promote_prefix: default_codetable_promote_prefix(),
            half_life: 0.0,
            code_scope: default_english_code_scope(),
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
    /// 词组补全的音节数约束（全局唯一）。
    #[serde(default)]
    pub completion: PinyinCompletion,
}

/// 词组补全（前缀补全）的音节数约束（`[schema.pinyin.completion]`）。
///
/// 约束的是「码比输入长」的补全词——即引擎在**预测用户尚未输入的音节**。精确匹配、
/// 子短语、整句、简拼都不受这两项影响（它们不预测任何东西）。
///
/// 判据的尺子是 `started` = 输入的完整音节数 + (有尾部残码 ? 1 : 0)，是**输入自身的
/// 属性**。允许的候选音节数上限 = `started < min_syllables ? started : started + max_extra_syllables`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinyinCompletion {
    /// 至少输入几个音节才给出词组候选。
    ///
    /// 补全词的音节数恒 ≥ 输入音节数，故 `started < min_syllables` 时上限收紧到
    /// `started` 本身，效果就是「只出同音节数的候选」——单字母 `d` 与单音节 `dian`
    /// 只出单字，不再混进「但是」「电话」。取 1 = 不设限（回到历史行为）。
    ///
    /// 尾部残码算作起头的一个音节：`dianh`(dian + h) 已经算 2 个，故它照常出「电话」。
    #[serde(default = "default_completion_min_syllables")]
    pub min_syllables: u32,
    /// 词组最多比输入多几个音节。
    ///
    /// 0 = 只给音节数与输入相等的词；1 = 只补下一个音节（`nih` → 你好/你会，
    /// 不给 4 音节的「你会发现」）；4 以上才够 `zhonghuar` → 「中华人民共和国」
    /// （输入 3 音节、词 7 音节）。数值越大，引擎越敢预测你还没打的内容。
    #[serde(default = "default_completion_max_extra_syllables")]
    pub max_extra_syllables: u32,
}

fn default_completion_min_syllables() -> u32 {
    2
}

fn default_completion_max_extra_syllables() -> u32 {
    3
}

impl Default for PinyinCompletion {
    /// ⚠️ 与 [`CodetableGlobal`] 那种「结构体零值」不同，这里给的是**真实默认值**，
    /// 与 `data/config.toml` 的出厂值一致。零值在此没有意义：`min_syllables = 0`
    /// 等于不设限，而 `max_extra_syllables = 0` 是一个合法且很严格的取值，
    /// 没法拿来当「未配置」的哨兵。
    fn default() -> Self {
        Self {
            min_syllables: default_completion_min_syllables(),
            max_extra_syllables: default_completion_max_extra_syllables(),
        }
    }
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
            completion: PinyinCompletion::default(),
        }
    }
}

/// 全局码表配置（[schema.codetable]）。所有码表方案的公共基线，方案可经
/// `schema_overrides/{id}.toml` 的 `[codetable]` 段（带 enabled 总开关）逐字段覆盖。
/// z 键功能（`schema.codetable.z_key_action` 的解析形态）。
///
/// # 为什么是方案级、且只管 z
///
/// 字母天然是编码键，能否借作引导键取决于**这张码表里它是不是死码**（五笔 86 的 z 是，
/// 别的码表未必）。这是方案的属性，全局 `trigger_keys` 无从表达——那里配了字母就是无条件
/// 抢键，该字母在所有方案里都打不出编码。故字母引导键已从 `trigger_keys` 移除
/// （见 `Coordinator::special_trigger_vk`），能力收归本项。
///
/// 只管 z 而不做「任意字母可配」：本项与 `z_key_repeat` 是同一个键的两个身份，裁决链要在
/// 二者之间选。若本项可配成别的字母，`z_key_repeat` 的状态就会去挡一个与它无关的键
/// （旧实现正是如此：配 `u` 作触发键时，按 u 会被 z 的 repeat 历史挡住不进模式）。
/// 严格同域才自洽。将来真有换字母的需求，改这一处即可。
///
/// # 值域
///
/// - `""` / `"none"`：z 是普通编码字母（默认）
/// - `"temp_pinyin"`：进临时拼音
/// - `"temp_english"`：进临时英文
/// - `"mix:<id>"`：进指定融合模式（`mix:quick_mix` = 内置「快捷」）
/// - `"special:<id>"`：进指定特殊模式
///
/// 未知值一律解析成 [`BoundAction::None`]（不静默变成别的功能）；指向不存在的 id 由消费端
/// 的门卫拦下（`mix_members` / `ensure_schema`），并在加载期 `warn`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundAction {
    /// 不启用：z 作正常编码字母。
    None,
    /// 进临时拼音。
    TempPinyin,
    /// 进临时英文。
    TempEnglish,
    /// 进指定融合模式（携带实例 id）。
    Mix(String),
    /// 进指定特殊模式（携带实例 id）。
    Special(String),
}

impl BoundAction {
    /// 解析配置字符串。大小写与首尾空白不敏感；未知值 → [`Self::None`]。
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if let Some(id) = s.strip_prefix("mix:") {
            let id = id.trim();
            return if id.is_empty() {
                Self::None
            } else {
                Self::Mix(id.to_string())
            };
        }
        if let Some(id) = s.strip_prefix("special:") {
            let id = id.trim();
            return if id.is_empty() {
                Self::None
            } else {
                Self::Special(id.to_string())
            };
        }
        match s.to_lowercase().as_str() {
            "temp_pinyin" => Self::TempPinyin,
            "temp_english" => Self::TempEnglish,
            _ => Self::None,
        }
    }

    /// 是否启用（非 `None`）。
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::None)
    }
}

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
    /// z 键功能：空缓冲按 z 且 z 在本方案是死码时，进哪个模式。见 [`BoundAction`]。
    ///
    /// 与 [`Self::z_key_repeat`] **正交**（可同时开）：repeat 先手，继续打字母才轮到本项，
    /// 详见 `Coordinator::try_activate_mode` 的三重身份裁决。
    #[serde(default)]
    pub z_key_action: String,
    /// 码元字符集（哪些字符可进输入缓冲）。空=内置默认 `a-z`。
    /// 解析与回落见 [`crate::code_charset::CodeCharSet`]。
    #[serde(default)]
    pub input_chars: String,
    /// 可作**首码**的字符集（`input_chars` 的子集）。空=与 `input_chars` 相同。
    #[serde(default)]
    pub leading_chars: String,
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
            // ⚠️ 这是**结构体零值**，不是「出厂默认」——出厂值在 `data/config.toml`（L2 层，
            // 恒覆盖本处）。大量集成测试以 `Config::default()` 构造，把这些拨成 true 会连带
            // 改变它们的输入行为（顶码/标点上屏都会生效）。
            //
            // 特殊方案的折叠基线**另有定义**，见 `EngineManager::SPECIAL_SCHEMA_BASELINE`——
            // 那是「特殊方案该长什么样」，与本处的「结构体零值」不是同一件事，别合并。
            top_code_commit: false,
            clear_on_empty_max: false,
            auto_commit_at_full: false,
            auto_commit_min_len: 0,
            punct_commit: false,
            show_code_hint: true,
            single_code_input: false,
            single_code_complete: false,
            z_key_repeat: false,
            z_key_action: String::new(),
            // 空串 = 未设置 → `CodeCharSet::new` 回落内置默认 `a-z`，与历史硬编码
            // `VK_A..=VK_Z` 逐键等价。这里刻意不写 "a-z" 字面量：让「未配置」在
            // 结构体零值与 TOML 缺省两处是同一个值，避免两套默认源不一致。
            input_chars: String::new(),
            leading_chars: String::new(),
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
        // 码元字符集是 `String` 而非 `Option`，故「未设置」由**空串**表达（与上面那些
        // tri-state 字段不同）。非空才覆盖——否则方案没写这项时会把全局基线抹成空串，
        // 落到 `CodeCharSet::new` 又被回落成 `a-z`，全局配的字符集就被静默丢弃了。
        if !o.input_chars.is_empty() {
            out.input_chars = o.input_chars.clone();
        }
        if !o.leading_chars.is_empty() {
            out.leading_chars = o.leading_chars.clone();
        }
        if let Some(v) = &o.z_key_action {
            out.z_key_action = v.clone();
        }
        // 调频段逐字段折叠。整段缺省 = 全部跟随基线。
        //
        // ⚠️ 这一段的消费方是 `EngineManager::freq_settings`，与上面那些上屏行为字段的
        // 消费方（`build_engine` 的 CommitOptions）不是同一条路径。加字段时两条都要看：
        // 光在这里折叠、`freq_settings` 仍读全局镜像的话，方案文件里写了也没人读。
        if let Some(f) = &o.frequency {
            if let Some(v) = f.enabled {
                out.frequency.enabled = v;
            }
            if let Some(v) = &f.strategy {
                out.frequency.strategy = v.clone();
            }
            if let Some(v) = &f.promote_prefix {
                out.frequency.promote_prefix = v.clone();
            }
            if let Some(v) = f.half_life {
                out.frequency.half_life = v;
            }
            if let Some(v) = f.protect_top_n {
                out.frequency.protect_top_n = v;
            }
            if let Some(v) = f.protect_top_n_len1 {
                out.frequency.protect_top_n_len1 = v;
            }
            if let Some(v) = f.protect_top_n_len2 {
                out.frequency.protect_top_n_len2 = v;
            }
            if let Some(v) = f.protect_top_n_len3 {
                out.frequency.protect_top_n_len3 = v;
            }
        }
        out
    }
}

/// 码表调频（[schema.codetable.frequency]）。
///
/// 不 derive `Eq`：`half_life` 是 f64。与 `PinyinFrequency` 一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodetableFrequency {
    #[serde(default)]
    pub enabled: bool,
    /// 锁定码表原始前 N 位——**兜底档**：码长 ≥ 4 的深码位。
    /// 简码位（码长 1/2/3）另有分级值，见下面三个字段。
    ///
    /// ⚠️ 作用域是「码表配置组」而非「纯码表方案」：混输走的也是这套值
    /// （`EngineManager::freq_settings` 按"非拼音即码表"分流）。
    #[serde(default)]
    pub protect_top_n: usize,
    /// 一简位（码长 1）保护前 N 位。五笔一简 25 个码每个都是二选一，默认保护首选。
    #[serde(default = "default_protect_len1")]
    pub protect_top_n_len1: usize,
    /// 二简位（码长 2）保护前 N 位。
    #[serde(default = "default_protect_len2")]
    pub protect_top_n_len2: usize,
    /// 三简位（码长 3）保护前 N 位。默认不保护——三简的钦定性弱于一二简。
    #[serde(default)]
    pub protect_top_n_len3: usize,
    /// 词频应用策略：`"top"`（一次到顶 MRU）/ `"step"`（逐次提升，默认）/
    /// `"position"`（位次减半）。原 freq_strategy 迁入。
    ///
    /// `top`/`step` 是**布尔 used-first**——用过一次即整体跳到档内最前，策略只决定「已用过
    /// 的那批内部怎么排」。`position` 让位次连续表达强弱，没有「用过 / 没用过」这道台阶，
    /// 适合**前缀匹配为主**的方案（英文尤甚，其候选几乎全是前缀匹配）。
    #[serde(default = "default_freq_strategy")]
    pub strategy: String,
    /// 前缀补全候选参与词频位置提升的范围（`"none"` / `"single"` / `"all"`）。
    ///
    /// **仅 `strategy = "position"` 时生效**；`top`/`step` 走布尔 used-first，不读本项。
    ///
    /// 码表默认 `"all"`（与拼音的 `"single"` 不同）：码表的前缀补全已由来源档位隔离、
    /// 跨不到精确档之前，无需再按语义单元收窄；且这与 `top`/`step` 的历史行为一致
    /// （那两者对前缀补全从无限制），避免升级后存量用户的调频范围突然变窄。
    #[serde(default = "default_codetable_promote_prefix")]
    pub promote_prefix: String,
    /// **衰减半衰期（小时）**；`0` = 用内置默认 72 小时。**与拼音段完全独立，不回落到它。**
    ///
    /// **仅 `strategy = "position"` 时生效**；`top`/`step` 直接比 `count`/`last_used`，不读衰减。
    ///
    /// 曾做成「`0` 回落到 `schema.pinyin.frequency.half_life`」，已否决：设置页上这是两个
    /// 独立控件，回落链会让用户「把码表的留在 0、改了拼音的、发现码表跟着变」。**回落链只在
    /// 配置层不可见时才是便利，一旦两端都有 GUI 就变成了陷阱。**
    #[serde(default)]
    pub half_life: f64,
}

fn default_english_code_scope() -> String {
    "candidate".to_string()
}

fn default_codetable_promote_prefix() -> String {
    "all".to_string()
}

fn default_protect_len1() -> usize {
    1
}

fn default_protect_len2() -> usize {
    1
}

fn default_freq_strategy() -> String {
    "step".to_string()
}

impl Default for CodetableFrequency {
    fn default() -> Self {
        Self {
            enabled: false,
            protect_top_n: 0,
            protect_top_n_len1: default_protect_len1(),
            protect_top_n_len2: default_protect_len2(),
            protect_top_n_len3: 0,
            strategy: default_freq_strategy(),
            promote_prefix: default_codetable_promote_prefix(),
            half_life: 0.0, // 0 = 用内置默认 72 小时
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
    /// **前缀补全候选参与词频位置提升的范围**（默认 `"single"`）。
    ///
    /// | 取值 | 打 `d` 选「得」 | 打 `d` 选「东西」 | 打 `hel` 选 `hello` |
    /// |---|---|---|---|
    /// | `"none"` | 不提升 | 不提升 | 不提升 |
    /// | `"single"`（默认） | **提升** | 不提升 | **提升** |
    /// | `"all"` | 提升 | 提升 | 提升 |
    ///
    /// 判据是[语义单元数][wind_candidate::semantic_units]（汉字逐字计、西文词整体计 1），
    /// **不是字符数**——英文候选 `hello` 有 5 个 char，按字符数会被「只提升单个」挡死，
    /// 而英文所有候选都是前缀匹配，那等于英文调频全灭。
    ///
    /// 默认 `"single"` 的理由：短输入下用户给出的信息量撑不起一个词组，把长词组靠词频顶到
    /// 高频单字前面与直觉相悖（微软拼音实测「只对全码生效」是同一取舍的更强版本）。而单字
    /// 之间的调整（「的」/「得」）是合理的，`"none"` 会连它一起挡掉。
    ///
    /// 只作用于**有效前缀层**（`is_prefix && !is_promoted_completion`），与 `cmp_match_layers`
    /// 同口径：被引擎主动提升进完整匹配层的候选是结构决策，不该被本项误伤。
    #[serde(default = "default_promote_prefix")]
    pub promote_prefix: String,
}

fn default_promote_prefix() -> String {
    "single".to_string()
}

/// 码表自动造词（[schema.codetable.auto_phrase]）。
///
/// 语义：连续单字上屏累积成序列，遇终止符（标点/回车/空格/焦点切换/光标移动/多字词上屏）
/// 或超过 `idle_timeout_ms` 未继续时，按方案 `[[encoder.rules]]` 为整个序列算词组编码并
/// 写入**临时词库**（立即可作为候选）；累计使用达 `promote_count` 次才晋升进用户词库。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoPhraseConfig {
    #[serde(default)]
    pub enabled: bool,
    /// 造词最小字数（默认 2；内部字段，设置页不开放）。
    #[serde(default = "default_phrase_min_len")]
    pub min_phrase_len: usize,
    /// 造词最大字数（默认 5；内部字段，设置页不开放）。**超长序列整体放弃**，不截取末尾
    /// N 字——在连续多字中间切一刀，切出来的多半不是词，是杂词的主要来源。
    #[serde(default = "default_phrase_max_len")]
    pub max_phrase_len: usize,
    /// 临时词晋升进用户词库所需使用次数。**0 = 不晋升**，一直留在临时词库（默认）。
    #[serde(default)]
    pub promote_count: usize,
    /// 连续单字之间的最大间隔（毫秒，0=默认 5000）。超过则把已累积序列视作终止。
    /// 兜底用：终止信号全漏时防止跨句拼出「加好加好」这类杂词。内部字段，设置页不开放。
    #[serde(default)]
    pub idle_timeout_ms: u32,
    /// 临时词库条目上限（0=不限）。超出后淘汰权重最低者。内部字段，设置页不开放。
    #[serde(default = "default_temp_max_entries")]
    pub temp_max_entries: usize,
}

fn default_phrase_min_len() -> usize {
    2
}

/// 默认 5（原为 10）。五笔场景下 10 字连续序列几乎必是跨句杂词；Go 版默认亦为 5。
fn default_phrase_max_len() -> usize {
    5
}

fn default_temp_max_entries() -> usize {
    5000
}

impl Default for AutoPhraseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_phrase_len: default_phrase_min_len(),
            max_phrase_len: default_phrase_max_len(),
            promote_count: 0,
            idle_timeout_ms: 0,
            temp_max_entries: default_temp_max_entries(),
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
    /// 满码自动上屏 **与顶码上屏**遇拼音候选则否决（保护拼音用户）。默认开。
    ///
    /// 这是**粗粒度**一票否决：整串只要能查出任何拼音候选就让路拼音，不看拼音成不成词。
    /// 与细粒度的 `block_commit_on_pinyin_word`（按词强度判，默认开）叠加生效，
    /// 二者任一命中即否决（见 `pinyin_vetoes_commit`）。
    /// 注意作用面覆盖顶码上屏，而 `schema.codetable.top_code_commit` 出厂即开——
    /// 混输方案下改动本项会直接改变顶码行为；`top_code_override_pinyin` 可无视本否决。
    ///
    /// 它还兼管**满码空码清空**（`schema.codetable.clear_on_empty_max`）的两道拼音守护：
    /// 「已有拼音候选」与「拼音还没打完」（`is_possible_pinyin_sequence`，如 zhon→zhong）。
    /// 关闭本项 = 拼音一律不干预码表处置，满码无匹配即清空/上屏。
    #[serde(default = "default_true")]
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
    /// 混输时拼音是否产出简拼候选（声母缩写，nh→你好）。默认开=历史行为（此前恒开无开关）。
    /// 关闭后混输里的拼音只认全拼，适合「只把拼音当临时输入补位、不用简拼」的用户；
    /// 简拼会让几乎任何字母串都可能是拼音，关掉可让候选更干净。仅影响混输的拼音子引擎，
    /// 纯拼音方案不受影响。
    #[serde(default = "default_true")]
    pub enable_pinyin_abbrev: bool,
}

impl Default for MixGlobal {
    fn default() -> Self {
        Self {
            show_source_hint: false,
            enable_english: false,
            // ⚠️ 三处同源：本处 / `MixConfig::default()`（wind-engine mixed/engine.rs）/
            // `data/config.toml [schema.mix]` 必须一致，改默认须同步全部三处。
            pinyin_only_overflow: true,
            top_code_override_pinyin: false,
            auto_commit_block_on_pinyin: true,
            auto_commit_block_on_english: false,
            min_pinyin_length: 0,
            min_english_length: 0,
            block_commit_on_pinyin_word: true,
            pinyin_word_min_weight: 0,
            enable_pinyin_abbrev: true,
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

/// 快捷输入的**全局**行为配置。
///
/// 各候选来源的开关与优先级**不在这里**——它们是 `mix_modes.members` 的成员
/// （`quick_input.calc` / `.date` / `.number` / `.repeat` 与 `$primary_pinyin` / `english`），
/// 开关即增删、优先级即排序。本结构只留与来源无关的全局项。
///
/// 曾有 `enable_english` 与 `members` 并存，构成双真相源（协调器两处各过滤一遍
/// english 成员）。已废弃并在加载期迁移：旧值 false → 从 quick_mix 的 members 移除 english。
/// 快捷输入的**全局**行为配置。
///
/// 没有总开关：想禁用就把 `quick_mix` 的 `trigger_keys` 清空——没有触发键自然进不去，
/// 一件事只有一种表达。（曾有 `enabled` 字段，但它从未被任何逻辑读取，关掉不产生任何效果。）
///
/// 曾有 `force_vertical`（强制竖排），但它的判定条件是「**这个 mix 实例**含 quick 成员」，
/// 属于实例的显示属性却被存在与实例无关的全局段里。已迁移到
/// [`MixModeConfig::candidate_layout`]（见 docs/design/mode-candidate-layout.md）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuickInputConfig {
    /// 计算器结果小数位数，默认 6
    #[serde(default = "default_decimal_places")]
    pub decimal_places: i32,
}

fn default_decimal_places() -> i32 {
    6
}

impl Default for QuickInputConfig {
    fn default() -> Self {
        Self {
            decimal_places: default_decimal_places(),
        }
    }
}

// ───────────────────────── 模式级显示属性（多模式共用）─────────────────────────

/// 模式级候选布局意图（设计见 docs/design/mode-candidate-layout.md）。
///
/// - `Follow`：跟随全局 `ui.candidate.layout`——用户改全局，本模式跟着改。
/// - `Vertical` / `Horizontal`：进入该模式期间覆盖全局方向，退出自动回到全局。
///
/// 刻意与 `ui.candidate.layout` 共用取值词汇（"vertical"/"horizontal"），让「模式级设置」
/// 与「全局设置」在用户眼里是同一件事的两个层级，而不是两套发明出来的开关名。
///
/// **为什么不是布尔**：`Follow` 与 `Vertical` 只在全局本身是竖排时才有区别——前者跟着
/// 全局变、后者恒定竖排。布尔（旧 `quick_input.force_vertical`）把这两种意图压成同一个
/// `true`，且表达不了「全局竖排但本模式横排」（临英候选一行放得下，竖排反而占屏）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LayoutIntent {
    #[default]
    Follow,
    Vertical,
    Horizontal,
}

/// 模式级注释模板覆盖（三态）。见 `crate::comment::template_for` 的决策点说明。
///
/// - **键缺失**（`None`）= 跟随全局同方向的模板（默认，零回归）
/// - **非空** = 本模式期间改用该模板
/// - **空串** = 本模式期间不显示注释
///
/// # 为什么是 `Option<String>` 而不是 `String`
///
/// 用空串表达「跟随全局」的话，「本模式不要注释」就没法表达了——而这恰恰是本功能最主要的
/// 用途（反查类模式信息太多、干扰正常输入）。三态里「缺失」与「空」必须是两件事。
///
/// # 横竖各一份，与全局同构
///
/// 字段名与 `ui.candidate.comment_template_vertical` / `_horizontal` 刻意保持一致，
/// 且**两个方向各自独立三态**——只覆盖竖排、横排仍跟随全局是合法且常见的配置。
/// 与 [`LayoutIntent`] 的取值词汇复用同一个理由：让「模式级」与「全局」在用户眼里
/// 是同一件事的两个层级，而不是两套发明出来的键名。
pub type CommentTemplateOverride = Option<String>;

/// 自由输入（字面输入）模式：让 mix 能打出 `GetTestData()` / `test_data` / `<TAB>`
/// 这类**任何 member 都无法接受**的内容。
///
/// - `Off`：完全维持既有行为（越界字符仍走「顶屏候选 + 上屏标点 + 退出」）。
/// - `Auto`（**默认**）：由缓冲内容自动推导，见 `MixLens`。
/// - `Always`：本实例恒为自由输入——用于新建一个专做字面输入的融合模式。
///
/// # 为什么没有切换键
///
/// mix 里几乎每个键都已双重占用（文本透镜：数字选词、标点顶屏；数字透镜：字母选词），
/// 挑不出一个真正空闲的可打印键；而非可打印键（Tab / 方向键 / PgUp）又都是可配置的
/// 导航键组。于是判据落在**输入内容本身**：一个字符若不在当前透镜的合法字符集内，
/// 它就不可能是编码，只能是字面内容。
///
/// # 为什么是缓冲的纯函数而非粘滞状态位
///
/// 没有进入键就没有退出键，粘滞态找不到可解释的清除时机。纯函数下退格删掉最后一个
/// 越界字符就自然回到原透镜，所见即所得。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FreeInputMode {
    Off,
    #[default]
    Auto,
    Always,
}

/// 临时 mix 模式配置（overlay 激活面）。触发后对每个成员方案查询并按成员序合并候选。
///
/// ⚠️ **`Default` 手写而非 derive**：`free_input_takes_select_keys` 的 serde 缺省是 `true`，
/// 而 derive 出来的 `bool::default()` 是 `false`——两条路径会给出相反的默认值，测试夹具
/// （`..Default::default()`）与真实配置的行为就此分叉。新增带非零默认值的字段时，
/// **必须同时改这里**（与 `TempEnglishConfig` 手写 `Default` 同因）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// 成员列表：**候选来源的单一真相源**——有无即开关，顺序即优先级。
    ///
    /// 三类取值：
    /// - 真实方案 id（`"pinyin"` / `"english"` / 码表方案…），经其 `.schema.toml` 加载；
    /// - 占位符 [`MIX_MEMBER_PRIMARY_PINYIN`]（解析为 `schema.primary_pinyin`）；
    ///   字面 id 一律精确解释（`"pinyin"` 恒为全拼，永不被替换）；
    /// - 快捷输入内置来源（`wind_quick_input::MEMBER_*`：`quick_input.calc` / `.date` /
    ///   `.number` / `.repeat`），无对应方案文件，由协调器直接产出候选。
    ///   旧的合并值 `"quick_input"` 在加载期展开为这四项。
    #[serde(default)]
    pub members: Vec<String>,
    /// 进入本 mix 期间的候选布局（默认跟随全局）。每实例独立——两个融合模式可以
    /// 一个竖排一个横排。旧的 `schema.quick_input.force_vertical` 已迁移到这里
    /// （它本就是 quick_mix 这个实例的属性，却被存在了与实例无关的全局段里）。
    #[serde(default)]
    pub candidate_layout: LayoutIntent,
    /// 本 mix 期间的注释模板覆盖（竖排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: CommentTemplateOverride,
    /// 本 mix 期间的注释模板覆盖（横排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: CommentTemplateOverride,
    /// 自由（字面）输入，见 [`FreeInputMode`]。默认 `Auto`。
    ///
    /// 每实例独立：一个只做日期/计算的 mix 可以关掉它，保住「打完拼音按逗号顶屏出
    /// 中文标点」；专做字面输入的实例则设 `Always`。
    #[serde(default)]
    pub free_input: FreeInputMode,
    /// 自由输入是否**夺取二三候选键**（`keys.select_key_groups` 的键，默认 `;` `'`）
    /// 作字面输入。默认开；`free_input = "off"` 时本项无意义。
    ///
    /// # 为什么需要它
    ///
    /// 文本透镜下「选词键」与「字面字符」是同一批物理键的两种解释，无法两全。而
    /// `rock'n'roll` / `don't` / `for(;;)` 这类内容里的 `'` `;` 恰好就是默认选词键：
    /// 不夺取的话它们在第④步就被 `select_key_offset` 吃掉，根本走不到第⑤步的字面输入
    /// （实测 `;rock` 按 `'` 会选走第 3 候选「日欧」并触发分步确认，输入被打散）。
    ///
    /// 夺取的代价是**零能力损失**：`;`/`'` 选第 2/3 候选只是数字键 `2`/`3` 的冗余别名，
    /// 数字键仍在。**数字键 1-9 刻意不在夺取范围内**——它们是文本透镜唯一的选词通路，
    /// 让位就没有选词键了。代价是 `utf8` / `mp3` / `x64` 这类「纯小写字母 + 数字」仍需
    /// 先打一个大写字母或符号切进自由输入。
    ///
    /// # 为什么是独立开关而非跟随 `free_input`
    ///
    /// 翻页键（`-`/`=`）的让位是跟着 `free_input` 走的，本项刻意不对称：翻页有
    /// PageUp/PageDown 作等价替代，让位是纯收益；而选词键的取舍因人而异——习惯用
    /// `;`/`'` 选词的用户可以单独关掉本项，保住手感，同时仍享有大写字母与其它符号
    /// 触发的自由输入。
    #[serde(default = "default_true")]
    pub free_input_takes_select_keys: bool,
}

impl Default for MixModeConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            short_name: String::new(),
            trigger_keys: Vec::new(),
            members: Vec::new(),
            candidate_layout: LayoutIntent::default(),
            comment_template_vertical: None,
            comment_template_horizontal: None,
            free_input: FreeInputMode::default(),
            // 与 `#[serde(default = "default_true")]` 对齐，见结构体文档的 ⚠️。
            free_input_takes_select_keys: default_true(),
        }
    }
}

/// 内置「快捷」融合 mix 的实例 id（`;` 触发，成员含日期/计算/拼音/英文）。
/// 设置页只暴露其 trigger_keys；其余字段（尤其 members）为内置默认值。
pub const QUICK_MIX_ID: &str = "quick_mix";

/// mix 成员占位符：解析期替换为 `schema.primary_pinyin`（空=全拼 "pinyin"）。
/// 内置「快捷」默认成员用它，使快捷输入的拼音跟随主拼音方案（双拼用户得双拼）。
/// 与字面 `"pinyin"` 严格区分——后者表示"就要全拼"，永不被替换。
pub const MIX_MEMBER_PRIMARY_PINYIN: &str = "$primary_pinyin";

/// 主拼音方案缺省回退（`schema.primary_pinyin` 为空时的目标方案）。
/// 固定全拼，不扫描 available——避免方案列表顺序静默改变拼音行为。
pub const DEFAULT_PINYIN_SCHEMA: &str = "pinyin";

fn default_mix_modes() -> Vec<MixModeConfig> {
    let mut members: Vec<String> = wind_quick_input::LEGACY_EXPANSION
        .iter()
        .map(|s| s.to_string())
        .collect();
    members.push(MIX_MEMBER_PRIMARY_PINYIN.to_string());
    members.push("english".to_string());
    vec![MixModeConfig {
        id: QUICK_MIX_ID.to_string(),
        name: "快捷".to_string(),
        short_name: "快".to_string(),
        trigger_keys: vec!["semicolon".to_string()],
        members,
        // 出厂强制竖排：快捷输入的候选是日期/算式结果等长文本，横排放不下。
        // 与旧 data/config.toml 的 `quick_input.force_vertical = true` 行为一致
        // （mix_modes 不能写进预置文件，故默认值只能落在这里，见 §迁移）。
        candidate_layout: LayoutIntent::Vertical,
        // None = 跟随全局注释模板（内置 quick_mix 不预设覆盖）
        comment_template_vertical: None,
        comment_template_horizontal: None,
        // 出厂开自动自由输入：`;` 是为特殊内容而进的模式，字面输入是它的常见用途。
        free_input: FreeInputMode::Auto,
        // 夺取 `;`/`'` 作字面：它们选第 2/3 候选只是数字键的冗余别名，而 `rock'n'roll`
        // 这类内容里的 `'` 没有别的输入通路。
        free_input_takes_select_keys: true,
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
    /// 引导键列表（如 "grave"/"backslash"）。**只认符号键**——字母不作引导键，
    /// 见 `Coordinator::special_trigger_vk`；要让 z 进本模式请配 `schema.codetable.z_key_action`。
    #[serde(default)]
    pub trigger_keys: Vec<String>,
    /// 引用的方案 id（其 .schema.toml 提供码表与全码策略；不进 schema.available，仅 overlay 触发懒加载）
    #[serde(default)]
    pub schema: String,
    /// 专用直达热键（如 "ctrl+shift+u"，空串=不注册）。与 `trigger_keys` 引导键共存；
    /// 热键进入时组合区不写引导符（见 docs/design/special-mode-entry-hotkey.md）。
    #[serde(default)]
    pub hotkey: String,
    /// 进入模式即展示候选：空编码（刚进入、尚未敲码）时枚举该方案码表首页候选（按 weight 降序），
    /// UI 按 per_page 分页浏览。默认 false（进入空白，敲码才出候选）。
    /// 面向快符/生僻字等**小符号表**的「进入即浏览」；大表会遍历全表取首 N 条、有开销，慎用。
    #[serde(default)]
    pub show_all_on_enter: bool,
    /// 进入本特殊模式期间的候选布局（默认跟随全局）。每实例独立——快符表可竖排、
    /// 生僻字表可横排，互不影响。
    #[serde(default)]
    pub candidate_layout: LayoutIntent,
    /// 本特殊模式期间的注释模板覆盖（竖排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: CommentTemplateOverride,
    /// 本特殊模式期间的注释模板覆盖（横排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: CommentTemplateOverride,
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

/// 「检索范围」智能档的放宽增强（设计见 `docs/design/smart-filter-scope-relax.md`）。
///
/// 智能档会滤掉同码位有常用字的生僻字，代价是**唯一编码被占的生僻字彻底打不出**（如五笔
/// 「桜」sivg 与常用「档」同码），用户只能整体切「全部字符」并常忘记切回。
///
/// 出路只有**一条**：候选窗内按向后翻页键翻到底，再按一次即临时放宽。刻意不做任何自动
/// 行为——曾实现过「候选不足一页自动补充」，实测平白改变了智能档的既有观感，已删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeRelaxConfig {
    /// 末页再按向后翻页键即临时放宽为「全部字符」，本次组合结束后自动恢复。
    ///
    /// 三类引擎通用且唯一的入口——用户找生僻字本就会一路翻页，翻到底即是明确的放宽意图。
    /// 候选**不足一页**时同样适用：那时只有一页，按翻页键一样翻不动，落到同一条路径。
    #[serde(default = "default_true")]
    pub page_end_key: bool,
    /// 放宽放出来的候选的前缀标注（空=不标注），用于与正常候选区分。
    #[serde(default = "default_scope_relax_prefix")]
    pub prefix: String,
}

impl Default for ScopeRelaxConfig {
    fn default() -> Self {
        Self {
            page_end_key: true,
            prefix: default_scope_relax_prefix(),
        }
    }
}

fn default_scope_relax_prefix() -> String {
    "·".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    #[serde(default = "default_filter_mode")]
    pub filter_mode: String,
    /// 检索范围放宽（智能档增强）。
    #[serde(default)]
    pub scope_relax: ScopeRelaxConfig,
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
    /// 快捷加词面板（目前只有候选布局一项；进入方式是 keys.add_word 热键）。
    #[serde(default)]
    pub add_word: AddWordConfig,
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
            scope_relax: ScopeRelaxConfig::default(),
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
            add_word: AddWordConfig::default(),
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

/// 智能符号替换方案。两者是「体感」与「兼容性」的取舍，故都保留：
/// - `DeleteReplace`（**默认**）：press1 直接提交中文符号，press2 删掉重打成英文。
///   所见即所得、无预览态中间状态，实际体感更好；代价是依赖对宿主做删改
///   （早期的 Office 500ms 重复、SendInput 自重入、prevChar 读不到致完全不触发
///   三处已修）。
/// - `HoldComposition`：press1 开启 TSF 组合态展示中文符号，press2 替换组合提交英文；
///   超时（smart_timeout_ms）后自动提交中文。全程不做删改，**兼容性更好**，
///   适合对删改敏感、DeleteReplace 下表现异常的宿主。
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SmartMethod {
    #[default]
    DeleteReplace,
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

/// 智能符号配置（[input.symbol]）：同一标点在时限内连按两次，删前一字符替换为另一形态。
///
/// 三个总开关**互相独立**，各管一种上下文，都默认关：
///   - `smart_mode`：中文标点状态 —— press1 中文 → press2 英文（数字后智能标点方向相反）。
///   - `english_punct_mode`：中文输入 + 英文标点状态 —— press1 英文 → press2 中文。
///   - `english_mode`：英文输入模式 —— 同上，但发生在整个输入法切英文时。
///
/// 后两者拆成两个开关而非一个，是因为它们是**不同场景**：前者是「用英文标点写中文、偶尔要个
/// 中文句号」，后者是「正在打英文」。很多人只想要前者，英文态保持纯净。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolConfig {
    /// 智能符号模式总开关（默认 false）。
    #[serde(default)]
    pub smart_mode: bool,
    /// 判定时限（毫秒，默认 500）。三种上下文共用。
    #[serde(default = "default_smart_symbol_timeout_ms")]
    pub smart_timeout_ms: i32,
    /// 参与智能符号转换的中文标点集合（子串包含匹配，含成对/多字符标点）。
    #[serde(default = "default_smart_symbol_chars")]
    pub smart_chars: String,
    /// 替换方案：`delete_replace`（删改，默认）或 `hold_composition`（保持组合态，兼容性更好）。
    /// 三种上下文共用。
    #[serde(default)]
    pub smart_method: SmartMethod,
    /// 英文标点状态（中文输入模式 + 工具栏标点切英文）下的智能符号（默认 false）。
    #[serde(default)]
    pub english_punct_mode: bool,
    /// 英文输入模式（整个输入法切英文）下的智能符号（默认 false）。
    ///
    /// 开启会让 core 把 `english_chars` 里的键推给 DLL 吃下转发——英文半角下这些键本来是直接
    /// 透传给宿主的，不吃就永远到不了引擎。故此开关的影响面比另外两个大，默认关。
    #[serde(default)]
    pub english_mode: bool,
    /// 参与英文智能符号的**源字符**集合（`english_punct_mode` 与 `english_mode` 共用）。
    ///
    /// 与 `smart_chars` 存中文产物不同，这里存的是**键本身的 ASCII 标点**（`.` 而非 `。`）：
    /// 英文侧的产物通常就等于源字符，而推给 DLL 的吃键集必须是源字符——按源字符判定，
    /// 两边同源、无需从产物反推。
    ///
    /// **不建议放配对符**（`([{"'` 等）：英文模式下这些键被吃走后，配对改由 core 处理，而
    /// DLL 的跳出栈是空的，Tab 跳出会失效（见 `handle_english_custom_punct` 的已知限制）。
    #[serde(default = "default_english_smart_chars")]
    pub english_chars: String,
}

impl Default for SymbolConfig {
    fn default() -> Self {
        Self {
            smart_mode: false,
            smart_timeout_ms: default_smart_symbol_timeout_ms(),
            smart_chars: default_smart_symbol_chars(),
            smart_method: SmartMethod::default(),
            english_punct_mode: false,
            english_mode: false,
            english_chars: default_english_smart_chars(),
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
    /// 跳出配对的按键：命中即光标越过右符号、弹出配对栈。可多选。
    ///
    /// 取值为键名（`"tab"`/`"enter"`/`"space"`/`"escape"`），外加一个特殊值
    /// **`"right_symbol"` = 右符号键本身**（打 `）` 跳出已插入的 `（）`）。右符号跳出曾是
    /// 无条件行为，现收敛为本列表的一项——**列表里没有它就是没有，不做隐式补偿**，故旧配置
    /// 若只写了 `["tab"]`，右符号跳出即关闭（用户可在设置界面重新勾选）。
    ///
    /// 对称配对（引号）**永不参与右符号跳出**，与本项无关：按键不携带「开/闭」这一位，
    /// 无从判断跳出还是嵌套，故一律开新的一对（见 `pin_quote_left_if_paired`）。
    #[serde(default = "default_jump_out_keys")]
    pub jump_out_keys: Vec<String>,
    /// 配对状态时效，单位秒（内部项，设置界面不暴露）。`0` = 不过期。
    ///
    /// 管的是**同一个输入框内**的状态陈旧：用户中途用鼠标点过别处、滚过页、把括号退格删掉
    /// ——这些输入法都感知不到，没有时效的话陈旧状态会一直存活到吃掉用户的 Tab。
    /// 距**最后一次按键**超过本值即视为陈旧，跳出键不再生效；从最后一次按键算起而非从插入
    /// 配对算起，因此持续输入会不断刷新，在括号里打多久都不会误过期。
    ///
    /// 跨焦点的陈旧不归它管——失焦一律清空配对状态（见 `handle_focus_lost`）。
    #[serde(default = "default_pair_state_ttl_secs")]
    pub state_ttl_secs: u32,
}

impl Default for AutoPairConfig {
    fn default() -> Self {
        Self {
            chinese: false,
            english: false,
            chinese_pairs: default_chinese_pairs(),
            english_pairs: default_english_pairs(),
            jump_out_keys: default_jump_out_keys(),
            state_ttl_secs: default_pair_state_ttl_secs(),
        }
    }
}

/// 默认 120 秒。够覆盖「在括号里停下来想一会儿」，又不会让状态在用户去干别的事之后
/// 仍然存活到吃掉 Tab。
fn default_pair_state_ttl_secs() -> u32 {
    120
}

/// 默认只启用右符号跳出（保持「打 `）` 跳出」这一长期行为），Tab/Enter 需用户显式勾选。
fn default_jump_out_keys() -> Vec<String> {
    vec![JUMP_OUT_RIGHT_SYMBOL.to_string()]
}

/// `jump_out_keys` 里代表「右符号键本身」的特殊值（非键名，不参与 VK 解析）。
pub const JUMP_OUT_RIGHT_SYMBOL: &str = "right_symbol";

fn default_chinese_pairs() -> Vec<String> {
    ["（）", "【】", "｛｝", "《》", "〈〉", "「」", "『』"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_english_pairs() -> Vec<String> {
    ["()", "[]", "{}"].iter().map(|s| s.to_string()).collect()
}

/// 临英符号白名单出厂值：数字 + 标识符/代码里最常用的符号。
///
/// 含 `.` 与 `-` 是刻意的（`obj.prop` / `e-mail` / `snake_case` 都要打得出），代价是这
/// 两个键在临英下交出各自的翻页职责——`comma_period` / `minus_equal` 两个键组各被劈掉
/// 一半，「上一页」只剩 ↑ 与 PgUp；「打完英文顺手按句号上屏」这条通路也随之失效。
/// 不需要代码场景的人把这两个字符从列表里删掉即可拿回。
fn default_temp_english_symbol_chars() -> String {
    "0123456789+-_.@#/".to_string()
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
    /// 允许符号与数字直接入缓冲（`C++` / `hello2` / `x64`）而非触发上屏或选词。
    /// 总开关，放行哪些字符由 [`Self::symbol_chars`] 精确决定。
    #[serde(default)]
    pub allow_symbols: bool,
    /// 允许入缓冲的符号/数字白名单（**纯字面字符集**，逐字符匹配）。仅 `allow_symbols`
    /// 开启时生效；**列表外的字符一律维持关闭时的语义**——符号仍「上屏高亮候选 +
    /// 转换后标点 → 退出临英」，`;`/`'` 仍选第 2/3 候选，`-=[],.` 仍翻页，数字键 1-9
    /// 仍选词。即：白名单只把选中的字符从这套语义里摘出来改成入缓冲。
    ///
    /// 留空 = 一个字符都不放行（等价于关掉 `allow_symbols`）。
    ///
    /// ★ 刻意**不**复用码元集 `input_chars` 的 `a-z` 范围语法：`-` 在符号集里是高频
    /// 字符（`e-mail`），那套语法下 `+-_` 会被解析成 `0x2B..=0x5F` 一整片（含全部大写
    /// 字母与 `:;<=>?@[\]^`），用户在设置页填一个减号就静默放行几十个字符。与同类的
    /// `symbol.english_chars` / `symbol.smart_chars` 一致，字面即全部真相。
    #[serde(default = "default_temp_english_symbol_chars")]
    pub symbol_chars: String,
    /// 空格作为输入字符入缓冲（可打出带空格的英文短句）。上屏职责随之转给回车，
    /// 且回车此时上屏**高亮候选**而非原文——否则该配置下没有任何选词键可用。
    #[serde(default)]
    pub space_as_input: bool,
    /// 进入临时英文期间的候选布局（默认跟随全局）。
    /// 典型用法是设 `horizontal`——英文候选一行放得下，全局竖排时反而占屏。
    #[serde(default)]
    pub candidate_layout: LayoutIntent,
    /// 临英期间的注释模板覆盖（竖排），见 [`CommentTemplateOverride`]。
    /// 典型用法 `"${dict}"`——只在打英文时显示挂载的英汉释义，中文输入不受影响。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: CommentTemplateOverride,
    /// 临英期间的注释模板覆盖（横排），见 [`CommentTemplateOverride`]。
    /// 临英常设 `candidate_layout = "horizontal"`，此时生效的是本项而非竖排那份。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: CommentTemplateOverride,
    /// 生成大小写变形候选（全小写 / 首字母大写 / 全大写）。
    ///
    /// 关掉后候选只剩输入原文 + 词库匹配。变形候选的代价是**每条都占一个候选位**：
    /// 每页 5 条时它们能吃掉一半，把真正的词库候选挤到下一页；且它们与词库候选交错，
    /// 注释、词频这些附加信息在变形项上都是空的，列表看起来参差。
    /// 需要大小写变换的人默认开着，只想要词库补全的人可以关掉。
    #[serde(default = "default_true")]
    pub case_variants: bool,
}

impl Default for TempEnglishConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_candidates: true,
            shift_behavior: "temp_english".to_string(),
            trigger_keys: Vec::new(),
            allow_symbols: false,
            symbol_chars: default_temp_english_symbol_chars(),
            space_as_input: false,
            candidate_layout: LayoutIntent::default(),
            case_variants: true,
            comment_template_vertical: None,
            comment_template_horizontal: None,
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
    /// 触发键（如 "backtick" / "semicolon"），默认反引号。**只认符号键**——字母触发键
    /// 已迁往方案级 `schema.codetable.z_key_action`（见 `migrate_letter_trigger_keys`）。
    #[serde(default = "default_temp_pinyin_triggers")]
    pub trigger_keys: Vec<String>,
    /// 专用直达热键（如 "ctrl+shift+p"，空串=不注册）。与 `trigger_keys` 引导键共存；
    /// 热键进入时组合区不写引导符（见 docs/design/special-mode-entry-hotkey.md）。
    #[serde(default)]
    pub hotkey: String,
    /// 进入临时拼音期间的候选布局（默认跟随全局）。
    #[serde(default)]
    pub candidate_layout: LayoutIntent,
    /// 临拼期间的注释模板覆盖（竖排），见 [`CommentTemplateOverride`]。
    /// 反查场景的典型用法是设 `"${code}"` 只留编码，或设 `""` 什么都不显示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: CommentTemplateOverride,
    /// 临拼期间的注释模板覆盖（横排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: CommentTemplateOverride,
}

fn default_temp_pinyin_triggers() -> Vec<String> {
    vec!["backtick".to_string()]
}

impl Default for TempPinyinConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger_keys: default_temp_pinyin_triggers(),
            hotkey: String::new(),
            candidate_layout: LayoutIntent::default(),
            comment_template_vertical: None,
            comment_template_horizontal: None,
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
    /// 进入网址模式期间的候选布局（默认跟随全局）。
    #[serde(default)]
    pub candidate_layout: LayoutIntent,
    /// 网址模式期间的注释模板覆盖（竖排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_vertical: CommentTemplateOverride,
    /// 网址模式期间的注释模板覆盖（横排），见 [`CommentTemplateOverride`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_template_horizontal: CommentTemplateOverride,
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
            candidate_layout: LayoutIntent::default(),
            comment_template_vertical: None,
            comment_template_horizontal: None,
        }
    }
}

/// 快捷加词配置（[input.add_word]）。加词面板是覆盖在任意输入态之上的临时面板，
/// 故其布局意图优先于底层模式（见 `Coordinator::layout_intent`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddWordConfig {
    /// 加词面板期间的候选布局。默认竖排——逐字确认的面板竖排更易读。
    /// 此前是**无条件硬编码**强制竖排、连开关都没有；本项只是给它一个出口，
    /// 默认值保持原行为不变。
    #[serde(default = "default_add_word_layout")]
    pub candidate_layout: LayoutIntent,
}

fn default_add_word_layout() -> LayoutIntent {
    LayoutIntent::Vertical
}

impl Default for AddWordConfig {
    fn default() -> Self {
        Self {
            candidate_layout: default_add_word_layout(),
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
    /// **方案直达热键**：`schema_id` → 热键串（如 `{ english = "ctrl+shift+n" }`）。
    ///
    /// 按下即把该方案切成当前方案（等价于用 `switch_engine` 循环键一路切到它），
    /// 与循环切换共用同一条 `switch_schema` 路径，因而也共用「切换时是否上屏」
    /// （`commit_on_switch`）与持久化 `schema.active` 的行为。
    ///
    /// 与特殊模式的 `hotkey` 是**两种不同的进入**，别混：那个是 overlay，打完一段就退回原
    /// 方案；这个是换方案，不按第二次不会回来。英文方案属于后者——「切过去打一段英文，
    /// 再切回中文」正是它存在的理由。
    ///
    /// 空串 = 不注册。指向不存在 / 未启用方案的条目在切换时安全失败（`switch_schema`
    /// 加载不到引擎即原样返回），不做启动期校验——方案可被删除或停用，校验只会在
    /// 那种时刻制造无从修复的启动告警。
    #[serde(default)]
    pub schema_hotkeys: HashMap<String, String>,
    /// **按键功能表**：热键串 → 动词（如 `{ "ctrl+shift+n" = "toggle_schema:english" }`）。
    ///
    /// 与上面那批「一个功能一个字段」的热键不同，这是「键 → 干什么」的通用表——同一套
    /// 动词值域将来也用于方案级 `[key_actions]`（见 docs/design/schema-key-actions.md）。
    /// 当前只接 `toggle_schema:<id>`，其余动词随后续阶段接入。
    ///
    /// 用 `BTreeMap` 而非 `HashMap`：编译成热键条目时遍历顺序即冲突时的胜者顺序，
    /// `HashMap` 会让同一份配置在不同进程里表现不同（`schema_hotkeys` 为此要显式排序）。
    #[serde(default)]
    pub key_actions: BTreeMap<String, String>,
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
            schema_hotkeys: HashMap::new(),
            key_actions: BTreeMap::new(),
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
fn default_first_show_settle_ratio() -> f32 {
    0.8
}

fn default_fast_typing_window_ms() -> u64 {
    100
}

fn default_fast_first_show_fallback_ms() -> u64 {
    25
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
    /// 注释词库挂载列表（`[[ui.comment_dicts]]`），供候选注释模板的 `${dict}` 变量查询。
    ///
    /// **数组顺序即优先级**：同一个词在多个库里都有注释时，取靠前那个库的。
    #[serde(default)]
    pub comment_dicts: Vec<CommentDictSpec>,
}

/// 一个注释词库（`[[ui.comment_dicts]]`）。
///
/// # 为什么是独立配置表，而不是塞进 `[[dictionaries]]`
///
/// 🔴 **注释库不参与召回**。若复用词库表加个 `type = "comment"` 区分，那么词库开关、
/// `base_order`、`composite::merge_search`、造词、加词、词频学习 —— 每一条路径都得**记得**
/// 跳过它，漏一处的表现是注释库里的词变成候选。独立成表意味着它从来就不在召回的数据结构里，
/// 不需要任何一处记得跳过。
///
/// 这也是本仓反复出现的教训（见候选调整按来源分流、密码框抑制的分层）：**「加个标志位区分」
/// 要求所有消费点同步，而消费点的数量只增不减。**
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommentDictSpec {
    /// 稳定标识（供设置页与日志定位；不参与查询）。
    #[serde(default)]
    pub id: String,
    /// 显示名。
    #[serde(default)]
    pub label: String,
    /// 词库路径，相对数据目录（用户目录优先，回落安装目录）。
    #[serde(default)]
    pub path: String,
    /// 是否启用。缺省视为启用 —— 用户手写一条却忘了 `enabled = true` 时，
    /// 「配了没反应」比「多加载一份」难查得多。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 限定生效的方案 id；**留空 = 全部方案**。
    ///
    /// 注释库常常是方案专属的：一份大英汉词典只在英文方案下有意义，挂在五笔方案上
    /// 每次输入都要多走一次二分且注定查不到。留空之所以是「全部」而非「无」——
    /// 用户手写一条却没写 `schemas` 时，「到处都显示」比「哪都不显示」好查得多，
    /// 与 `enabled` 缺省即启用同一取舍。
    #[serde(default)]
    pub schemas: Vec<String>,
}

impl CommentDictSpec {
    /// 本库是否适用于给定方案。
    pub fn applies_to(&self, schema_id: &str) -> bool {
        self.schemas.is_empty() || self.schemas.iter().any(|s| s == schema_id)
    }
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
    /// 纵向排列（默认 false=横条）。纵向是横向的转置：条宽取主题 `[toolbar] height`，
    /// 每格高取 `button_width`，故同一套主题几何在两个朝向下都成立、无需另配。
    /// 属用户偏好而非视觉设计，所以落在此处而非主题。
    #[serde(default)]
    pub vertical: bool,
}

impl Default for ToolbarConfig {
    fn default() -> Self {
        Self {
            visible: true,
            hide_in_fullscreen: true,
            auto_hide: false,
            auto_hide_delay: 5,
            vertical: false,
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
    /// 焦点切换到新的输入框时，是否也强制显示一次状态气泡（默认关）。
    ///
    /// 与 `display_mode` 正交：`always` 本就在获焦时显示，本项对它无额外效果；真正改变行为的是
    /// `temp`——原本只有用户主动切换中英/标点/全半角时才弹，开启后**换个输入框也弹一次**，
    /// 用来提示「你现在切到的这个框，输入法是什么状态」。
    ///
    /// ⚠ 显示时会绕过 `show_status` 的文本去重：焦点切换恰恰是「状态文本没变但仍要重弹」的场景，
    /// 走去重路径会让它在同状态下**完全不显示**。
    #[serde(default)]
    pub show_on_focus: bool,
    /// 方案名显示样式："full"（全名，默认）| "short"（图标短称 icon_label，回退全名）。
    #[serde(default = "default_schema_name_style")]
    pub schema_name_style: String,
    /// 位置模式："follow_caret"（跟随光标,默认）| "fixed"（固定屏幕坐标 custom_x/custom_y）。
    #[serde(default = "default_status_position_mode")]
    pub position_mode: String,
    /// follow_caret 下相对默认位置（光标下方、左边缘对齐光标）的水平偏移（像素，正=右）。
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
    /// 气泡显示哪些内容段（按此处顺序无关，渲染顺序固定）。合法项：
    /// `schema`（输入方案 / 中英）、`punct`（标点状态）、`full_width`（全半角）、
    /// `s2t`（简繁）、`caps`（大写锁定）。
    ///
    /// **留空 = 全部显示**：既是"未配置"的合理默认，也让旧配置文件（无此键）行为不变。
    /// 用列表而非逐项 bool，是为了后续增加状态项时不必再动配置结构。
    #[serde(default = "default_status_items")]
    pub items: Vec<String>,
}

/// 状态气泡内容段的全集，同时也是默认值（全部显示）。
pub const STATUS_ITEM_KEYS: [&str; 5] = ["schema", "punct", "full_width", "s2t", "caps"];

fn default_status_items() -> Vec<String> {
    STATUS_ITEM_KEYS.iter().map(|s| s.to_string()).collect()
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
            show_on_focus: false,
            schema_name_style: default_schema_name_style(),
            position_mode: default_status_position_mode(),
            offset_x: 0,
            offset_y: 0,
            custom_x: 0,
            custom_y: 0,
            items: default_status_items(),
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
    /// 首显容差系数（**内部选项**，不进设置页）：首帧用了非权威坐标时，随后到达的权威
    /// 坐标与它相差在 `行高 × 本系数` 以内就**不再校正**——校正本身才是抖动的观感来源，
    /// 十几像素的偏差不动比"跳一下修正"更稳（多数输入法也是这么做的）。
    /// 换行/重排的偏差通常 ≥2 个行高，远超此阈值，仍会正常校正。
    /// 0 表示禁用该容差（任何偏差都校正，即旧行为）。默认 0.8。
    #[serde(default = "default_first_show_settle_ratio")]
    pub first_show_settle_ratio: f32,
    /// 连续输入判定窗口（**内部选项**，毫秒）：两次按键间隔小于此值即视为"连续快速输入"，
    /// 此时 fast 档直接采信首条试探坐标、不再比对上一轮权威坐标。
    /// 依据是连打时光标顺序前移、不发生重排，且用户对"跟手"的敏感度远高于十几像素的偏差。
    /// 0 表示禁用该快路径。默认 100。
    #[serde(default = "default_fast_typing_window_ms")]
    pub fast_typing_window_ms: u64,
    /// fast 档的首显兜底超时（**内部选项**，毫秒）：等不到试探/权威坐标就用现有坐标先显示。
    ///
    /// 为什么必须远小于 wait 档的 150ms：实测 Word 从不发 `OnLayoutChange`（试探坐标无从产生），
    /// 其组合坐标要 60~190ms 才到，而连打时组合只活 27~57ms——上屏即 `reset_first_show()` 作废
    /// timer，150ms 兜底**永远等不到自己到期**，fast 档就此退化成 wait 档，候选窗 57/70 轮不显示。
    /// 取小值让 fast 在这类宿主上退化成 instant（用旧坐标 + 放宽容差）而非干等。
    /// 发 `OnLayoutChange` 的宿主（EverEdit/WPS）试探坐标 3~10ms 就到，不受本值影响。
    /// 默认 25。
    #[serde(default = "default_fast_first_show_fallback_ms")]
    pub fast_first_show_fallback_ms: u64,
    /// 候选文本最大显示字数，超出截断（0=不限）。
    #[serde(default)]
    pub max_chars: usize,
    /// **竖排**候选的注释段（候选右侧灰字）模板。语法见 `wind_coordinator::comment`。
    ///
    /// 横竖各持一份模板、互不影响：两种排布的可用横向空间差一个数量级（竖排每行独占，
    /// 横排全部候选共享一行宽度），能放什么本就不是同一个答案。共用一份的结果是
    /// 「为竖排配的拼音把横排候选窗撑爆」或「为横排收着配的注释让竖排一片空白」。
    #[serde(default = "default_comment_template")]
    pub comment_template_vertical: String,
    /// **横排**候选的注释段模板。见 [`Self::comment_template_vertical`]。
    #[serde(default = "default_comment_template")]
    pub comment_template_horizontal: String,
    /// 注释段的最大字数（0=不限），超出截断并加 `…`。
    ///
    /// 默认 0：本项引入前注释段从无长度限制，非 0 的默认值会让存量用户的注释突然变短。
    #[serde(default)]
    pub comment_max_chars: usize,
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
    /// 候选窗定位方式："follow_caret"（默认，跟随光标）/ "fixed"（固定屏幕坐标）。
    /// fixed 下窗口不再随光标移动，也不再上翻（flip/swap_when_above 随之失去意义）。
    #[serde(default = "default_candidate_position_mode")]
    pub position_mode: String,
    /// 固定模式下的**内容左上**屏幕坐标（不含阴影扩边），仅 position_mode="fixed" 生效。
    /// 由用户拖动候选窗落盘，设置页刻意不暴露：手填绝对坐标既不直观又会与拖动互相覆盖
    /// （与 ui.status.custom_x/y 同一决策）。(0,0) 视作"尚未设定"，首次显示落到屏幕默认锚点。
    #[serde(default)]
    pub custom_x: i32,
    #[serde(default)]
    pub custom_y: i32,
}

fn default_preedit_display() -> String {
    "app_inline".to_string()
}

fn default_candidate_position_mode() -> String {
    "follow_caret".to_string()
}

/// 注释段默认模板：`${code_hint|code}` **精确等价于本功能引入前的硬编码行为**
/// （引擎产的剩余编码优先，为空则回退到拼音候选的主码表反查码）。
///
/// 出厂默认能用模板原样表达，是 `${a|b}` 回退语法存在的主要理由 —— 没有它，出厂行为
/// 就得留在代码里作特例，模板便不再是注释内容的唯一真相源。
fn default_comment_template() -> String {
    "${code_hint|code}".to_string()
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
            first_show_settle_ratio: default_first_show_settle_ratio(),
            fast_typing_window_ms: default_fast_typing_window_ms(),
            fast_first_show_fallback_ms: default_fast_first_show_fallback_ms(),
            per_page_extended: 0,
            layout: "horizontal".to_string(),
            preedit_display: default_preedit_display(),
            hide_window: false,
            font_size: 18.0,
            font_size_follow_theme: true,
            pager_bar_display: String::new(),
            page_number_display: String::new(),
            max_chars: 16,
            comment_template_vertical: default_comment_template(),
            comment_template_horizontal: default_comment_template(),
            comment_max_chars: 0,
            index_labels: String::new(),
            flip_when_above: false,
            swap_preedit_when_above: false,
            pager_in_preedit: false,
            position_mode: default_candidate_position_mode(),
            custom_x: 0,
            custom_y: 0,
        }
    }
}

impl UiCandidateConfig {
    /// 是否为固定位置模式（position_mode="fixed"）。
    pub fn is_fixed_position(&self) -> bool {
        self.position_mode.eq_ignore_ascii_case("fixed")
    }

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

    /// 当前排布对应的注释模板（`vertical` 为 true 取竖排那份）。
    pub fn comment_template(&self, vertical: bool) -> &str {
        if vertical {
            &self.comment_template_vertical
        } else {
            &self.comment_template_horizontal
        }
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

// ───────────────────────── debug ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugConfig {
    /// 日志级别。空字符串等同 `info`（生产默认）。
    /// 注意：`info` 级别日志不得包含用户输入内容、词库词条等隐私数据。
    #[serde(default)]
    pub log_level: String,
    /// 单个日志文件的大小上限（MB），超出后滚动。默认 10。
    #[serde(default = "default_log_max_size_mb")]
    pub log_max_size_mb: u64,
    /// 保留的旧日志文件数量上限（不含主文件）。默认 10。
    ///
    /// 服务每次启动都会滚动一次，故该值约等于「能回溯最近几次运行」。
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
    10
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

/// 参与英文智能符号的源字符默认集：`smart_chars` 那批中文标点对应的 ASCII 键，去掉配对符
/// （配对符在英文模式下被吃走会让 DLL 的 Tab 跳出失效，见 `SymbolConfig::english_chars`）。
fn default_english_smart_chars() -> String {
    ".,?!:;".to_string()
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
            debug: DebugConfig::default(),
        }
    }
}

/// 用户配置目录的就绪探测结果。
///
/// 存在的意义是把「系统尚未就绪」与「用户确实没有配置」分开——两者此前都表现为
/// [`Config::load`] 静默跳过用户层，然后 [`Config::active_schema`] 回退到系统预置方案，
/// 用户看到的就是「设置好的方案重启后变回出厂方案」。
#[derive(Debug)]
pub enum UserConfigProbe {
    /// 便携模式：路径来自 exe 同目录，不依赖 known folder，恒就绪。
    Portable(PathBuf),
    /// 用户自定义数据目录（安装向导选定，见 `variant::custom_userdata_dir`）：
    /// 是本机固定盘上的普通目录，不经漫游 known folder，故与便携同属恒就绪一类。
    CustomDir(PathBuf),
    /// `dirs::config_dir()` 解析失败——漫游 known folder 尚不可用。
    RoamingUnavailable,
    /// 漫游根解析出来了但尚不存在（用户配置文件未挂载完成）。
    RoamingMissing(PathBuf),
    /// 漫游根已就绪、但本用户的 `config.toml` 此刻还看不到，**而本地标记表明它本该存在**
    /// （该用户此前确有用户配置）。这是开机早期漫游 profile 尚未挂载完的竞态，不是
    /// 「用户没配置」——**必须继续等**，别把「没看到」当成「没有」而退回系统五笔。
    ConfigPending { dir: PathBuf },
    /// 漫游根已就绪。此时 `dir_exists`/`file_exists` 是**确定性事实**，
    /// 再等下去也不会变，故不属于需要重试的状态。
    Ready {
        dir: PathBuf,
        dir_exists: bool,
        file_exists: bool,
    },
}

impl UserConfigProbe {
    /// 是否已到达「再等也不会变」的状态。`ConfigPending` **刻意排除**：它正是
    /// 「本该有、暂时没看到」的可变态，要继续轮询等漫游挂载。
    pub fn is_settled(&self) -> bool {
        matches!(
            self,
            Self::Portable(_) | Self::CustomDir(_) | Self::Ready { .. }
        )
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
        match Self::user_config_dir() {
            Some(user_dir) => {
                let user_config = user_dir.join("config.toml");
                if let Some(v) = Self::read_toml_value(&user_config) {
                    merge_value(&mut merged, v);
                    info!("Loaded user config: {}", user_config.display());
                }
            }
            // 漫游 known folder 解析失败。此前这里静默跳过整个用户层，配置退化为
            // 「默认 ⊕ 系统层」，用户的 schema.active 等设置全部失效且无任何痕迹。
            None => warn!("User config dir unavailable, user layer skipped"),
        }

        Self::migrate_enable_english_value(&mut merged);
        Self::migrate_force_vertical_value(&mut merged);
        let mut config: Config = merged.try_into()?;
        config.normalize();
        Ok(config)
    }

    /// 存量迁移（**须在反序列化前**跑，字段已从 [`QuickInputConfig`] 移除、结构体上读不到）：
    /// 废弃键 `schema.quick_input.enable_english = false` → 从内置 quick_mix 的 members 移除
    /// `english`。
    ///
    /// 该键与 members 曾是双真相源；语义合并到 members 后，关掉过英文候选的存量用户
    /// 必须在这里落成成员删除，否则升级后英文候选会自己冒回来。只认 false——true 是默认值，
    /// 无需动作。
    fn migrate_enable_english_value(merged: &mut toml::Value) {
        let disabled = merged
            .get("schema")
            .and_then(|s| s.get("quick_input"))
            .and_then(|q| q.get("enable_english"))
            .and_then(|v| v.as_bool())
            .is_some_and(|v| !v);
        if !disabled {
            return;
        }
        let Some(modes) = merged
            .get_mut("schema")
            .and_then(|s| s.get_mut("mix_modes"))
            .and_then(|m| m.as_array_mut())
        else {
            return;
        };
        for mode in modes.iter_mut() {
            if mode.get("id").and_then(|v| v.as_str()) != Some(QUICK_MIX_ID) {
                continue;
            }
            if let Some(members) = mode.get_mut("members").and_then(|m| m.as_array_mut()) {
                members.retain(|v| v.as_str() != Some("english"));
            }
        }
        info!("Migrated quick_input.enable_english=false into quick_mix members");
    }

    /// 存量迁移（**须在反序列化前**跑，字段已从 [`QuickInputConfig`] 移除）：
    /// 废弃键 `schema.quick_input.force_vertical` → 内置 quick_mix 的
    /// [`MixModeConfig::candidate_layout`]。
    ///
    /// 映射刻意**不对称**：
    /// - `true`  → `"vertical"`（强制竖排）
    /// - `false` → `"follow"`（**不是** `"horizontal"`）——旧布尔的 false 语义是「不强制」，
    ///   即跟随全局；写成 horizontal 会把「没开过这个开关」的用户强行钉在横排上。
    ///
    /// 键不存在则不动，让 [`default_mix_modes`] 的出厂值（Vertical）生效。老版预置文件
    /// 写的是 `force_vertical = true`，与出厂值同义，故未改过配置的用户升级后行为不变。
    fn migrate_force_vertical_value(merged: &mut toml::Value) {
        let Some(forced) = merged
            .get("schema")
            .and_then(|s| s.get("quick_input"))
            .and_then(|q| q.get("force_vertical"))
            .and_then(|v| v.as_bool())
        else {
            return;
        };
        let layout = if forced { "vertical" } else { "follow" };
        let Some(modes) = merged
            .get_mut("schema")
            .and_then(|s| s.get_mut("mix_modes"))
            .and_then(|m| m.as_array_mut())
        else {
            return;
        };
        for mode in modes.iter_mut() {
            if mode.get("id").and_then(|v| v.as_str()) != Some(QUICK_MIX_ID) {
                continue;
            }
            if let Some(t) = mode.as_table_mut() {
                t.insert(
                    "candidate_layout".to_string(),
                    toml::Value::String(layout.to_string()),
                );
            }
        }
        info!(
            "Migrated quick_input.force_vertical={forced} into quick_mix candidate_layout={layout}"
        );
    }

    /// 系统预置配置的 TOML 值：代码默认(L1) ⊕ `data/config.toml`(L2)，**不含用户层(L3)**。
    ///
    /// 供 capability 的 `default` 来源——出厂默认 = L1⊕L2。config.toml 作为系统预置
    /// 可合法覆盖 L1（如 schema.active）。
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

    /// 「与默认相同即不落盘」判定所用的出厂默认（L1⊕L2）。取不到则 `None`。
    ///
    /// ⚠️ **必须确认 `data/config.toml` 在场才返回 `Some`**：[`system_preset_value`] 传 `None`
    /// 会退回纯 L1，而 L2 本就允许合法覆盖 L1（`schema.active` 等出厂值只写在 L2）。
    /// 拿纯 L1 当"默认"去比对，会把用户显式设的值误判成默认而删掉，
    /// `load()` 时再从 L2 回落成**另一个**值 —— 用户的设置被静默改写，比不清理坏得多。
    ///
    /// 这不是假想：`schema.mix.pinyin_only_overflow` 与 `auto_commit_block_on_pinyin` 就曾长期
    /// L1/L2 不一致（已随本次修复对齐），此类漂移只要发生一次，纯 L1 比对就会开始吃用户配置。
    ///
    /// 返回 `None` 时调用方一律退化为「照常写入 / 不清理」，即旧行为。
    ///
    /// [`system_preset_value`]: Self::system_preset_value
    fn preset_for_pruning() -> Option<toml::Value> {
        let dir = Self::data_dir()?;
        if !dir.join("config.toml").is_file() {
            return None;
        }
        Self::system_preset_value(Some(&dir)).ok()
    }

    /// 清理用户层里与出厂默认（L1⊕L2）相同的冗余键，返回删除的键数。
    ///
    /// **不变量：清理前后 `load()` 的结果逐键完全相同** —— 删掉的每个键，三层合并时都会从
    /// L1⊕L2 回落到同一个值。故本操作对当前行为零影响，只影响**将来**默认值变更能否到达该用户。
    /// `set_user_value` 的同款收口负责不再产生新的冗余键，本函数负责清掉存量（该收口上线前
    /// 积累的量很可观：真机一份配置 105 键中 62 键冗余）。
    ///
    /// 幂等——跑第二次删 0 个。`data/config.toml` 或用户层缺失时直接返回 0。
    pub fn prune_user_config() -> anyhow::Result<usize> {
        let Some(dir) = Self::user_config_dir() else {
            return Ok(0);
        };
        let file = dir.join("config.toml");
        let Some(mut root) = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        else {
            return Ok(0);
        };

        // 退役键（[`RETIRED_KEYS`]）先清：它们与出厂默认无关，**不能**被 preset 取不到时的
        // 提前返回挡住——否则没装 data/config.toml 的环境永远清不掉。
        let mut removed = prune_retired(&mut root);
        // 冗余键需要出厂默认做逐键比对，取不到时跳过（安全降级为「不清理」的旧行为）。
        if let Some(preset) = Self::preset_for_pruning() {
            removed += prune_redundant(&mut root, &preset);
        }
        if removed == 0 {
            return Ok(0);
        }

        let out = toml::to_string_pretty(&root)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &file)?;
        info!("Pruned {} stale key(s) from user config", removed);
        Ok(removed)
    }

    /// 读取 TOML 文件为 Value（不存在/解析失败返回 None 并告警，不中断加载）
    fn read_toml_value(path: &Path) -> Option<toml::Value> {
        if !path.exists() {
            // 「文件不存在」曾是唯一无日志的失败路径：它让「用户没有配置」与
            // 「开机早期读不到配置」在日志上完全同形，只能靠有无 `Loaded user config`
            // 反推。DEBUG 级——`load()` 在热重载/RPC 上高频调用，不能进 INFO 刷屏。
            debug!("Config file absent: {}", path.display());
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
        self.migrate_quick_mix_pinyin_member();
        self.migrate_quick_input_legacy_member();
        self.migrate_letter_trigger_keys();
    }

    /// 存量迁移：`trigger_keys` 里的单字母 → 方案级 [`BoundAction`]。
    ///
    /// 引导键曾接受任意 a-z（`key_name_to_vk_with_letters`），字母的特殊能力现已收归
    /// `schema.codetable.z_key_action`。**必须显式迁移**：解析端改成只认符号后，
    /// `filter_map` 会把留在配置里的字母**静默丢弃**——用户的功能无声消失，且配置文件里
    /// 那行还在，从现象完全看不出原因。
    ///
    /// `z` 折算成对应 action，其余字母只能丢弃（本项只管 z）——但要 `warn` 出来，
    /// 让日志里留下痕迹。
    ///
    /// 归属优先级与老的模式激活链一致（临拼 > 特殊模式 > mix）：z 同时配在多处时，
    /// 老实现里也是临拼先匹配。已显式配过 `z_key_action` 的不覆盖——用户的新配置优先。
    fn migrate_letter_trigger_keys(&mut self) {
        /// 摘掉 `keys` 里的所有单字母项，返回其中是否含 `z`。
        fn take_letters(keys: &mut Vec<String>, owner: &str) -> bool {
            let mut has_z = false;
            keys.retain(|k| {
                let k = k.trim().to_lowercase();
                let is_letter = k.len() == 1 && k.as_bytes()[0].is_ascii_lowercase();
                if !is_letter {
                    return true;
                }
                if k == "z" {
                    has_z = true;
                } else {
                    warn!(
                        "配置迁移：{} 的引导键 \"{}\" 已失效（字母不再作引导键），已移除。\
                         若需让某个字母进模式，请配 schema.codetable.z_key_action（仅支持 z）",
                        owner, k
                    );
                }
                false
            });
            has_z
        }

        let mut migrated: Option<String> = None;
        let mut claim = |action: String| {
            if migrated.is_none() {
                migrated = Some(action);
            }
        };

        if take_letters(
            &mut self.input.temp_pinyin.trigger_keys,
            "input.temp_pinyin",
        ) {
            claim("temp_pinyin".to_string());
        }
        for m in self.schema.special_modes.iter_mut() {
            let owner = format!("schema.special_modes[{}]", m.effective_id());
            if take_letters(&mut m.trigger_keys, &owner) {
                claim(format!("special:{}", m.effective_id()));
            }
        }
        for m in self.schema.mix_modes.iter_mut() {
            let owner = format!("schema.mix_modes[{}]", m.id);
            if take_letters(&mut m.trigger_keys, &owner) {
                claim(format!("mix:{}", m.id));
            }
        }

        // 已显式配过则不覆盖：用户的新配置优先于存量迁移。
        if let Some(action) = migrated
            && self.schema.codetable.z_key_action.trim().is_empty()
        {
            info!(
                "配置迁移：z 引导键 → schema.codetable.z_key_action = \"{}\"",
                action
            );
            self.schema.codetable.z_key_action = action;
        }
    }

    /// 存量迁移：合并成员 `"quick_input"` → 细分来源 [`wind_quick_input::LEGACY_EXPANSION`]。
    ///
    /// 快捷输入的四个来源（计算/日期/数字/重复）曾是一个不可分的成员，无法单独开关。
    /// 拆分后旧值在原位展开，顺序与展开序一致——存量用户的候选序不变，只是从此可增删。
    /// 对**所有** mix 生效（不限内置 quick_mix）：`"quick_input"` 是保留 id，任何 mix 里
    /// 出现都只可能是这个含义。
    fn migrate_quick_input_legacy_member(&mut self) {
        for m in self.schema.mix_modes.iter_mut() {
            let Some(at) = m
                .members
                .iter()
                .position(|s| s == wind_quick_input::MEMBER_LEGACY)
            else {
                continue;
            };
            // 展开时跳过已单独写在别处的细分来源，避免重复成员。
            let expansion: Vec<String> = wind_quick_input::LEGACY_EXPANSION
                .iter()
                .filter(|e| !m.members.iter().any(|s| s == *e))
                .map(|e| e.to_string())
                .collect();
            m.members.splice(at..=at, expansion);
        }
    }

    /// 存量迁移：内置 `quick_mix` 的字面 `"pinyin"` 成员 → [`MIX_MEMBER_PRIMARY_PINYIN`] 占位符。
    ///
    /// 背景：`members` 从未开放给用户（无 UI、data/config.toml 无 mix_modes 段），但设置页改
    /// 「快捷输入激活键」时会把整个 mix_modes 数组连同 members 写回用户配置。故存量用户配置里的
    /// 字面 `"pinyin"` 必是旧默认值残留、而非「就要全拼」的用户意图，替换为占位符是安全的。
    /// 只认内置 quick_mix：用户自定义 mix 的字面 id 一律精确解释，不动。
    fn migrate_quick_mix_pinyin_member(&mut self) {
        for m in self
            .schema
            .mix_modes
            .iter_mut()
            .filter(|m| m.id == QUICK_MIX_ID)
        {
            for s in m.members.iter_mut() {
                if s == DEFAULT_PINYIN_SCHEMA {
                    *s = MIX_MEMBER_PRIMARY_PINYIN.to_string();
                }
            }
        }
    }

    /// 应用数据目录名：正式版 `WindInput`；dev 变体 `WindInputDev`
    /// （隔离调试与正式版的配置/缓存/日志，与管道后缀同源于运行时变体探测）。
    pub fn app_dir_name() -> &'static str {
        crate::variant::app_dir_name()
    }

    /// 用户配置目录（config.toml / userdata.redb / 词频 / shadow 置顶删词 / 用户词库）。
    /// - 便携模式：`<exe目录>/userdata/`
    /// - 自定义数据目录（安装向导选定，落 `datadir.conf`）：该目录本身
    /// - 正常模式：漫游 `%APPDATA%\WindInput[Dev]`（随用户在多设备间同步）
    ///
    /// 三者优先级即上述顺序。注意自定义目录**只影响本函数**——`local_dir()` 系
    /// （cache / logs / state.toml）不跟随，详见 `variant::custom_userdata_dir`。
    pub fn user_config_dir() -> Option<PathBuf> {
        if crate::variant::is_portable() {
            return crate::variant::portable_userdata_dir();
        }
        if let Some(d) = crate::variant::custom_userdata_dir() {
            return Some(d);
        }
        dirs::config_dir().map(|d| d.join(Self::app_dir_name()))
    }

    /// 探测用户配置目录当前是否可用。纯查询，无副作用、不重试。
    ///
    /// 判据刻意建在**漫游根目录**而非 `config.toml` 上：漫游根一旦可用，
    /// 「我们的目录/文件在不在」就是确定性事实（全新安装本就没有 config.toml，
    /// 它只在用户首次改设置时由 `set_user_value` 创建）。把判据建在文件上会让
    /// 每个全新用户白等一个完整超时。
    pub fn probe_user_config() -> UserConfigProbe {
        if crate::variant::is_portable() {
            return match crate::variant::portable_userdata_dir() {
                Some(d) => UserConfigProbe::Portable(d),
                None => UserConfigProbe::RoamingUnavailable,
            };
        }
        // 自定义目录同样绕开漫游 known folder，恒就绪——若漏了这一支，配置已指向
        // 自定义目录、探测却仍盯着漫游根，就会出现「等一个根本不用的目录」的错配。
        if let Some(d) = crate::variant::custom_userdata_dir() {
            return UserConfigProbe::CustomDir(d);
        }
        let Some(root) = dirs::config_dir() else {
            return UserConfigProbe::RoamingUnavailable;
        };
        if !root.is_dir() {
            return UserConfigProbe::RoamingMissing(root);
        }
        let dir = root.join(Self::app_dir_name());
        let file_exists = dir.join("config.toml").is_file();
        if !file_exists && Self::user_config_seen() {
            // 看不到 config.toml，但本地标记说这用户此前确有配置：开机早期漫游
            // profile 还没挂载完的竞态。继续等，别退回系统预置。
            return UserConfigProbe::ConfigPending { dir };
        }
        UserConfigProbe::Ready {
            dir_exists: dir.is_dir(),
            file_exists,
            dir,
        }
    }

    /// 本地「用户配置曾存在」标记文件路径
    /// （`%LOCALAPPDATA%\WindInput[Dev]\user_config.seen`）。
    ///
    /// 放 `%LOCALAPPDATA%`（非漫游）是关键：它登录即挂载、不受漫游延迟影响
    /// （日志能写出就是证据），故能可靠仲裁那个「可能迟到」的漫游 `config.toml`。
    fn user_config_marker_path() -> Option<PathBuf> {
        Self::local_dir().map(|d| d.join("user_config.seen"))
    }

    /// 查询本地「用户配置曾存在」标记。纯查询、无副作用、不重试——供 [`probe_user_config`]
    /// 区分「默认用户（永不等）」与「定制用户但漫游未挂载（要等）」。
    pub fn user_config_seen() -> bool {
        Self::user_config_marker_path()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    /// 若本用户当前确有 `config.toml`（即定制过设置），落下本地标记（幂等）。
    ///
    /// **只应由服务启动路径调用一次**，绝不放进 [`load`](Self::load)：`load()` 在
    /// 热重载/RPC 上高频调用、且被单元测试直接执行，从中写盘会污染真实
    /// `%LOCALAPPDATA%`。写标记是「观察到真实用户配置」后的一次性副作用，
    /// 收敛在服务二进制里。
    pub fn mark_user_config_seen_if_present() {
        // 只有确实看得到用户 config.toml 时才记；看不到就不记，避免把
        // 「漫游没挂载」误记成「用户有配置」而污染下次判断。
        let Some(user_dir) = Self::user_config_dir() else {
            return;
        };
        if !user_dir.join("config.toml").is_file() {
            return;
        }
        let Some(marker) = Self::user_config_marker_path() else {
            return;
        };
        if marker.exists() {
            return;
        }
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&marker, b"1") {
            Ok(()) => info!("Marked user-config-seen: {}", marker.display()),
            Err(e) => warn!("Failed to write user-config-seen marker: {e}"),
        }
    }

    /// 阻塞等待用户配置目录就绪，最多 `timeout`。返回是否就绪。
    ///
    /// 只应在服务启动早期调用一次，且必须在 logger 初始化之后——否则探测日志全部丢失。
    /// **不要**放进 `load()`：热重载与 RPC 也走 `load()`，在那些线程上阻塞会卡住输入。
    ///
    /// 超时后仍继续启动（降级为系统预置配置），不死等：输入法晚几秒可用尚可接受，
    /// 完全起不来不可接受。
    pub fn wait_user_config_ready(timeout: std::time::Duration) -> bool {
        Self::wait_until_settled(
            Self::probe_user_config,
            timeout,
            std::time::Duration::from_millis(250),
        )
    }

    /// [`Self::wait_user_config_ready`] 的可注入内核：探测源与轮询间隔都是参数。
    ///
    /// 抽出来是为了能测重试路径——真机上漫游根几乎总是就绪，重试分支在开发机
    /// 永远走不到，而它恰恰是这个修复的目的所在，不能靠「上真机重启一次」来验证。
    fn wait_until_settled(
        mut probe_fn: impl FnMut() -> UserConfigProbe,
        timeout: std::time::Duration,
        interval: std::time::Duration,
    ) -> bool {
        let start = std::time::Instant::now();
        let mut attempts = 0u32;

        loop {
            let probe = probe_fn();
            if probe.is_settled() {
                // 就绪状态也记录：dir_exists/file_exists 能直接回答
                // 「是路径没解析出来，还是配置真的不在」，无需再猜。
                info!(
                    "User config ready after {} attempt(s), {}ms: {:?}",
                    attempts,
                    start.elapsed().as_millis(),
                    probe
                );
                return true;
            }
            if start.elapsed() >= timeout {
                warn!(
                    "User config NOT ready after {}ms ({} attempts), last={:?}; \
                         falling back to system preset — user settings will be ignored",
                    start.elapsed().as_millis(),
                    attempts,
                    probe
                );
                return false;
            }
            if attempts == 0 {
                warn!("User config dir not ready, waiting: {:?}", probe);
            } else {
                debug!(
                    "User config dir still not ready (attempt {}): {:?}",
                    attempts, probe
                );
            }
            attempts += 1;
            std::thread::sleep(interval);
        }
    }

    /// 用户覆盖命中时的统一日志打点。
    ///
    /// 「用户目录同名文件整体替代安装目录自带文件」这条能力散落在多个解析函数里
    /// （方案文件 / 词库 / 方案附属资源 / 双拼布局 / 主题 / 数据根文件），各函数的回退
    /// 级数还不一样。排查「同一版程序、这台机器行为和出厂不一致」时，唯一可靠的线索就是
    /// 「当时到底加载了哪个文件」——故所有解析点一律经此打点、共用同一措辞，便于按
    /// `用户覆盖生效` 一次 grep 出全部生效的覆盖。
    ///
    /// `kind` 是资源类别（`schema` / `dict` / `resource` / `shuangpin` / `theme` / `data`），
    /// `rel` 是方案/数据根下的相对路径。**只在命中用户层时调用**：未覆盖的默认安装
    /// 不产生任何日志，故日志里出现即异常排查线索。
    ///
    /// `shadowed` 区分命中用户层的两种情形，**不可省**：安装目录也有同名文件时才是真的
    /// 「覆盖自带数据」（记 info，排查目标）；安装目录没有时只是第三方方案自带资源走用户
    /// 目录（记 debug）。二者都打 info 的话，一个第三方方案的几十个词库会把真正的覆盖淹掉。
    pub fn log_user_override(kind: &str, rel: &str, path: &Path, shadowed: bool) {
        if shadowed {
            info!("用户覆盖生效[{}]: {} → {}", kind, rel, path.display());
        } else {
            debug!("用户目录资源[{}]: {} → {}", kind, rel, path.display());
        }
    }

    /// 「用户目录优先、回落安装目录」的解析内核。`sub` 为两侧共同的子目录
    /// （方案类资源传 `Some("schemas")`，数据根文件传 `None`）。
    fn resolve_overridable(
        data_dir: Option<&Path>,
        sub: Option<&str>,
        rel: &str,
        kind: &str,
    ) -> Option<PathBuf> {
        if rel.is_empty() {
            return None;
        }
        let under = |base: &Path| -> PathBuf {
            match sub {
                Some(s) => base.join(s).join(rel),
                None => base.join(rel),
            }
        };
        // 借用而非 move：`under` 在下面的用户分支里还要再用一次。
        let sys = data_dir.map(&under);
        if let Some(user) = Self::user_config_dir() {
            let p = under(&user);
            if p.is_file() {
                let shadowed = sys.as_ref().is_some_and(|s| s.is_file());
                Self::log_user_override(kind, rel, &p, shadowed);
                return Some(p);
            }
        }
        sys.filter(|p| p.is_file())
    }

    /// 解析方案附属资源（拆字库/字根字体等 `[engine.chaizi]` 相对路径）：与方案文件同规则，
    /// 用户方案目录（`user_config_dir()/schemas/`）优先，回落系统数据目录 `data_dir/schemas/`。
    /// 第三方方案装在用户目录，其资源只在用户目录下——只拼 data_dir 会永远找不到。
    /// 两处均不存在返回 None（调用方自行告警）。
    pub fn resolve_schema_resource(data_dir: Option<&Path>, rel: &str) -> Option<PathBuf> {
        Self::resolve_overridable(data_dir, Some("schemas"), rel, "resource")
    }

    /// 解析**数据根**下的程序自带文件（`system.phrases.toml` / `pinyin_map.txt` 等）：
    /// 用户配置目录同名文件整体替代，回落安装目录 `data_dir/`。两处均无返回 None。
    ///
    /// 与 [`Self::resolve_schema_resource`] 的差别只在根少一层 `schemas/`。这类文件是
    /// **整体替换**语义（不做键级合并）——合并语义只有 `config.toml`（三层）与
    /// `compat.toml`（字段级）两处，它们各有专用加载器，不走本函数。
    pub fn resolve_data_file(data_dir: Option<&Path>, rel: &str) -> Option<PathBuf> {
        Self::resolve_overridable(data_dir, None, rel, "data")
    }

    /// 把单个配置项**部分合并**写入用户层 `config.toml`（%APPDATA%/WindInput/config.toml）。
    ///
    /// 只改 `path` 指定的项、保留用户文件里其它已有项，**不写入未改动的默认/系统段**——
    /// 用户层维持最小 diff，避免覆盖系统层/默认层的后续更新。
    /// 原子写（tmp + rename）。`path` 如 `["ui","candidate","preedit_display"]`。
    ///
    /// ★ **值等于出厂默认（L1⊕L2）时删除该键，而不是写入**（见
    /// [`preset_for_pruning`](Self::preset_for_pruning)）。这条收口是上面那句「避免覆盖后续更新」
    /// 唯一的兑现方式：此前无论值是什么都照写，用户把开关点回默认位就在用户层留下一个显式值，
    /// 从此**永久钉死、不再跟随 L1/L2 的后续变更**。真机实测一份用户配置 105 个键里 62 个是这种
    /// 冗余键，其中 `schema.mix.auto_commit_block_on_pinyin` 已经引爆：它在默认值还是 `false` 的
    /// 版本被写入，之后默认改回 `true`，该用户却一直停在 `false`，顶码的拼音保护被静默卸掉。
    ///
    /// 语义取舍：加了这条之后，「用户显式选了与默认相同的值」无法与「跟随默认」区分。对配置系统
    /// 而言后者才是正确语义；若将来要支持「锁定某值不随升级变化」（pin），需要另设表达方式，
    /// **不要**靠退回「照原样写入」来实现——那等于把这 62 颗雷再埋回去。
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
        // 供落盘后通知钩子用：下方 set_nested 会 move 掉 value。
        let value_for_hook = value.clone();
        // 出厂默认取不到时 `is_default` 恒 false → 退化为「照常写入」的旧行为（安全降级）。
        // `is_known_key` 与 `prune_redundant` 同一道保险：未登记键（废弃键 / Map 子路径）不收口。
        let is_default = crate::config_schema::is_known_key(&path.join("."))
            && Self::preset_for_pruning()
                .as_ref()
                .and_then(|p| get_nested(p, path))
                .is_some_and(|d| *d == value);
        if let toml::Value::Table(t) = &mut root {
            if is_default {
                remove_nested(t, path);
            } else {
                set_nested(t, path, value);
            }
        }

        let out = toml::to_string_pretty(&root)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, &file)?;
        // 落盘成功后通知订阅方（设置界面等）。放在 rename 之后：失败路径提前 `?`
        // 返回，不会误报。传出的是入参值——键被剪枝删除时用户层虽无此键，生效值
        // 仍是它（见上方剪枝说明），对订阅方语义一致。
        crate::change_hook::notify_changed(path, &value_for_hook);
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

    /// 缓存目录（%LOCALAPPDATA%\WindInput\cache）：词库 .wdat 等可重建产物。
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

    /// 注释库的方案限定：`schemas` 留空适用于全部方案，非空则只在列出的方案下加载。
    ///
    /// 「留空=全部」这个方向不能反：反过来的话，用户手写一条却没写 `schemas` 就是
    /// 「配了完全没反应」，而那是本仓反复出现的最难自查的一类故障。
    #[test]
    fn comment_dict_schema_scoping() {
        let global = CommentDictSpec::default();
        assert!(global.applies_to("wubi86"), "留空适用于任意方案");
        assert!(global.applies_to(""), "空方案 id 也算适用");

        let scoped = CommentDictSpec {
            schemas: vec!["english".into(), "pinyin".into()],
            ..Default::default()
        };
        assert!(scoped.applies_to("english"));
        assert!(scoped.applies_to("pinyin"));
        assert!(
            !scoped.applies_to("wubi86"),
            "未列出的方案不得加载——挂大英汉词典在五笔下查是纯浪费，这正是本字段的目的"
        );
        assert!(
            !scoped.applies_to("English"),
            "方案 id 区分大小写，按精确匹配"
        );
    }

    #[test]
    fn z_key_action_parses_value_domain() {
        assert_eq!(BoundAction::parse(""), BoundAction::None);
        assert_eq!(BoundAction::parse("none"), BoundAction::None);
        assert_eq!(BoundAction::parse(" TEMP_PINYIN "), BoundAction::TempPinyin);
        assert_eq!(BoundAction::parse("temp_english"), BoundAction::TempEnglish);
        assert_eq!(
            BoundAction::parse("mix:quick_mix"),
            BoundAction::Mix("quick_mix".into())
        );
        assert_eq!(
            BoundAction::parse("special:rare"),
            BoundAction::Special("rare".into())
        );
        // 未知值绝不静默变成别的功能。
        assert_eq!(BoundAction::parse("enter_temp_pinyin"), BoundAction::None);
        assert_eq!(BoundAction::parse("quick_input"), BoundAction::None);
        // 空 id 无从定位目标，等同不启用（消费端也会被门卫挡下，此处提前收敛）。
        assert_eq!(BoundAction::parse("mix:"), BoundAction::None);
        assert_eq!(BoundAction::parse("special:  "), BoundAction::None);
        // id 大小写敏感（与 special_mode_idx / mix_mode_idx 的精确匹配同口径）。
        assert_eq!(
            BoundAction::parse("mix:Quick_Mix"),
            BoundAction::Mix("Quick_Mix".into())
        );
    }

    /// 存量迁移：`trigger_keys` 里的 z → `z_key_action`，其余字母丢弃。
    ///
    /// 不迁移的后果是**静默失效**：解析端只认符号后 `filter_map` 会把字母无声吃掉，
    /// 配置文件里那行还在，用户完全看不出功能为什么没了。
    #[test]
    fn migrate_letter_trigger_keys_moves_z_to_action() {
        let mut c = Config::default();
        c.input.temp_pinyin.trigger_keys = vec!["backtick".into(), "z".into(), "q".into()];
        c.normalize();

        assert_eq!(
            c.schema.codetable.z_key_action, "temp_pinyin",
            "z 应折算成 z_key_action"
        );
        assert_eq!(
            c.input.temp_pinyin.trigger_keys,
            vec!["backtick".to_string()],
            "字母项应从 trigger_keys 摘除，符号项保留"
        );
    }

    /// 归属优先级与老的模式激活链一致：临拼 > 特殊模式 > mix。
    #[test]
    fn migrate_letter_trigger_keys_follows_activation_priority() {
        let mut c = Config::default();
        c.input.temp_pinyin.trigger_keys = vec!["z".into()];
        c.schema.special_modes = vec![SpecialModeConfig {
            id: "rare".into(),
            trigger_keys: vec!["z".into()],
            ..Default::default()
        }];
        c.normalize();

        assert_eq!(
            c.schema.codetable.z_key_action, "temp_pinyin",
            "同时配在多处时按激活链取临拼（老实现也是临拼先匹配）"
        );
        assert!(
            c.schema.special_modes[0].trigger_keys.is_empty(),
            "未中选的字母项同样要摘除，否则留在配置里也是静默失效"
        );
    }

    /// 特殊模式独有的 z：折算成 `special:<id>`。
    #[test]
    fn migrate_letter_trigger_keys_maps_special_mode() {
        let mut c = Config::default();
        c.schema.special_modes = vec![SpecialModeConfig {
            id: "rare".into(),
            trigger_keys: vec!["backslash".into(), "z".into()],
            ..Default::default()
        }];
        c.normalize();

        assert_eq!(c.schema.codetable.z_key_action, "special:rare");
        assert_eq!(
            c.schema.special_modes[0].trigger_keys,
            vec!["backslash".to_string()],
            "符号引导键不受影响，仍可与 z_key_action 并存"
        );
    }

    /// 已显式配过 `z_key_action` 时，存量迁移不得覆盖用户的新配置。
    #[test]
    fn migrate_letter_trigger_keys_does_not_override_explicit() {
        let mut c = Config::default();
        c.schema.codetable.z_key_action = "temp_english".into();
        c.input.temp_pinyin.trigger_keys = vec!["z".into()];
        c.normalize();

        assert_eq!(
            c.schema.codetable.z_key_action, "temp_english",
            "显式配置优先于存量迁移"
        );
        assert!(
            c.input.temp_pinyin.trigger_keys.is_empty(),
            "旧字母项无论是否中选都要摘除"
        );
    }

    /// 内置「快捷」默认成员用占位符，使快捷输入的拼音跟随主拼音方案（而非恒为全拼）。
    #[test]
    fn quick_mix_default_members_use_primary_pinyin_placeholder() {
        let modes = default_mix_modes();
        let quick = modes
            .iter()
            .find(|m| m.id == QUICK_MIX_ID)
            .expect("应有内置 quick_mix");
        assert!(
            quick
                .members
                .contains(&MIX_MEMBER_PRIMARY_PINYIN.to_string()),
            "默认成员应为占位符，实际 {:?}",
            quick.members
        );
        assert!(
            !quick.members.contains(&DEFAULT_PINYIN_SCHEMA.to_string()),
            "不应再硬编码字面 pinyin，实际 {:?}",
            quick.members
        );
    }

    /// 存量迁移：改过「快捷输入激活键」的用户配置里，members 被整体写回为字面 pinyin，
    /// 加载期须迁成占位符，否则这些用户的快捷输入永远是全拼。
    #[test]
    fn normalize_migrates_quick_mix_literal_pinyin() {
        let mut cfg = Config::default();
        cfg.schema.mix_modes = vec![
            MixModeConfig {
                id: QUICK_MIX_ID.to_string(),
                members: vec![
                    "quick_input".to_string(),
                    "pinyin".to_string(),
                    "english".to_string(),
                ],
                ..Default::default()
            },
            // 用户自定义 mix：字面 id 精确解释，不迁移。
            MixModeConfig {
                id: "my_mix".to_string(),
                members: vec!["pinyin".to_string()],
                ..Default::default()
            },
        ];
        cfg.normalize();
        assert!(
            cfg.schema.mix_modes[0]
                .members
                .contains(&MIX_MEMBER_PRIMARY_PINYIN.to_string()),
            "内置 quick_mix 的字面 pinyin 应迁为占位符，实际 {:?}",
            cfg.schema.mix_modes[0].members
        );
        assert_eq!(
            cfg.schema.mix_modes[1].members,
            vec!["pinyin"],
            "自定义 mix 的字面 pinyin 应原样保留"
        );
    }

    /// 在合并值里塞一个存量用户配置残留的 `schema.quick_input.force_vertical`。
    fn merged_with_force_vertical(v: Option<bool>) -> toml::Value {
        let mut merged = toml::Value::try_from(Config::default()).expect("默认配置应可序列化");
        if let Some(v) = v {
            merged
                .get_mut("schema")
                .and_then(|s| s.get_mut("quick_input"))
                .and_then(|q| q.as_table_mut())
                .expect("schema.quick_input 应存在")
                .insert("force_vertical".to_string(), toml::Value::Boolean(v));
        }
        merged
    }

    fn quick_mix_layout(merged: toml::Value) -> LayoutIntent {
        let cfg: Config = merged.try_into().expect("迁移后应可反序列化");
        cfg.schema
            .mix_modes
            .iter()
            .find(|m| m.id == QUICK_MIX_ID)
            .expect("内置 quick_mix 应存在")
            .candidate_layout
    }

    /// 废弃键 `force_vertical` → `mix_modes[quick_mix].candidate_layout`。
    ///
    /// ★ 映射刻意不对称：`false` 迁成 **Follow 而非 Horizontal**。旧布尔的 false 语义是
    /// 「不强制」（跟随全局），迁成 Horizontal 会把从没开过这个开关、又把全局设成竖排的
    /// 用户强行钉在横排上。
    #[test]
    fn force_vertical_migrates_into_quick_mix_candidate_layout() {
        for (old, want) in [
            (true, LayoutIntent::Vertical),
            (false, LayoutIntent::Follow),
        ] {
            let mut merged = merged_with_force_vertical(Some(old));
            Config::migrate_force_vertical_value(&mut merged);
            assert_eq!(
                quick_mix_layout(merged),
                want,
                "force_vertical={old} 应迁为 {want:?}"
            );
        }
    }

    /// 旧键缺席（全新安装 / 新版预置文件已删该行）时不动，保留出厂竖排。
    /// 守的是「未改过配置的用户升级后行为不变」。
    #[test]
    fn absent_force_vertical_keeps_factory_vertical() {
        let mut merged = merged_with_force_vertical(None);
        Config::migrate_force_vertical_value(&mut merged);
        assert_eq!(
            quick_mix_layout(merged),
            LayoutIntent::Vertical,
            "无旧键时应保留 default_mix_modes() 的出厂竖排"
        );
    }

    /// 存量迁移：合并成员 `quick_input` 就地展开为四个细分来源，其余成员的相对序不变。
    #[test]
    fn normalize_expands_legacy_quick_input_member() {
        let mut cfg = Config::default();
        cfg.schema.mix_modes = vec![MixModeConfig {
            id: QUICK_MIX_ID.to_string(),
            members: vec![
                "quick_input".to_string(),
                MIX_MEMBER_PRIMARY_PINYIN.to_string(),
                "english".to_string(),
            ],
            ..Default::default()
        }];
        cfg.normalize();
        let mut expected: Vec<String> = wind_quick_input::LEGACY_EXPANSION
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.push(MIX_MEMBER_PRIMARY_PINYIN.to_string());
        expected.push("english".to_string());
        assert_eq!(
            cfg.schema.mix_modes[0].members, expected,
            "旧值应在原位展开，展开序 = 默认成员序"
        );
        // 幂等：再跑一次不重复展开
        let once = cfg.schema.mix_modes[0].members.clone();
        cfg.normalize();
        assert_eq!(cfg.schema.mix_modes[0].members, once, "迁移应幂等");
    }

    /// 展开时跳过用户已单独写出的细分来源，不产生重复成员。
    #[test]
    fn legacy_expansion_skips_already_present_sources() {
        let mut cfg = Config::default();
        cfg.schema.mix_modes = vec![MixModeConfig {
            id: QUICK_MIX_ID.to_string(),
            members: vec![
                wind_quick_input::MEMBER_NUMBER.to_string(),
                "quick_input".to_string(),
            ],
            ..Default::default()
        }];
        cfg.normalize();
        let m = &cfg.schema.mix_modes[0].members;
        assert_eq!(
            m.iter()
                .filter(|s| *s == wind_quick_input::MEMBER_NUMBER)
                .count(),
            1,
            "细分来源不应重复，实际 {:?}",
            m
        );
        assert_eq!(
            m,
            &vec![
                wind_quick_input::MEMBER_NUMBER.to_string(),
                wind_quick_input::MEMBER_CALC.to_string(),
                wind_quick_input::MEMBER_DATE.to_string(),
                wind_quick_input::MEMBER_REPEAT.to_string(),
            ],
            "用户显式写出的来源保持其位置"
        );
    }

    /// 存量迁移：废弃键 `enable_english = false` 落成 members 里的 english 删除。
    /// 该迁移在反序列化前作用于 TOML 值，故直接验证 `migrate_enable_english_value`。
    #[test]
    fn migrates_disabled_enable_english_into_members() {
        let mut v = toml::Value::try_from(Config::default()).unwrap();
        // 模拟存量用户配置：关掉了英文候选
        v.get_mut("schema")
            .unwrap()
            .get_mut("quick_input")
            .unwrap()
            .as_table_mut()
            .unwrap()
            .insert("enable_english".to_string(), toml::Value::Boolean(false));
        Config::migrate_enable_english_value(&mut v);
        let cfg: Config = v.try_into().unwrap();
        assert!(
            !cfg.schema.mix_modes[0]
                .members
                .contains(&"english".to_string()),
            "关过英文候选的存量用户，升级后英文不应冒回来：{:?}",
            cfg.schema.mix_modes[0].members
        );
        assert!(
            cfg.schema.mix_modes[0]
                .members
                .contains(&MIX_MEMBER_PRIMARY_PINYIN.to_string()),
            "只应移除 english，其余成员不动"
        );
    }

    /// 默认值（enable_english 缺省或为 true）不触发迁移。
    #[test]
    fn default_keeps_english_member() {
        let mut v = toml::Value::try_from(Config::default()).unwrap();
        Config::migrate_enable_english_value(&mut v);
        let cfg: Config = v.try_into().unwrap();
        assert!(
            cfg.schema.mix_modes[0]
                .members
                .contains(&"english".to_string()),
            "无废弃键时 english 成员应保留"
        );
    }

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

    /// `is_settled` 的语义边界：只有「再等也不会变」的两态算就绪。
    /// 尤其 `Ready { file_exists: false }` **必须**算就绪——全新安装本就没有
    /// config.toml（只在用户首次改设置时创建），把它当未就绪会让每个新用户白等整个超时。
    #[test]
    fn probe_settled_semantics() {
        let dir = PathBuf::from("x");
        assert!(UserConfigProbe::Portable(dir.clone()).is_settled());
        // 自定义数据目录是本机固定盘上的普通目录，不存在「等漫游挂载」一说；
        // 漏判会让每次启动都白等一个完整超时，然后退回系统预置方案。
        assert!(UserConfigProbe::CustomDir(dir.clone()).is_settled());
        assert!(
            UserConfigProbe::Ready {
                dir: dir.clone(),
                dir_exists: true,
                file_exists: true,
            }
            .is_settled()
        );
        assert!(
            UserConfigProbe::Ready {
                dir: dir.clone(),
                dir_exists: false,
                file_exists: false,
            }
            .is_settled(),
            "漫游根就绪后，配置在不在是确定性事实，不该继续等待"
        );
        assert!(
            !UserConfigProbe::ConfigPending { dir: dir.clone() }.is_settled(),
            "本地标记说该用户本有 config.toml，但此刻看不到 → 竞态，须继续等而非就绪"
        );
        // 这两态才是「系统尚未就绪」，等待有意义。
        assert!(!UserConfigProbe::RoamingUnavailable.is_settled());
        assert!(!UserConfigProbe::RoamingMissing(dir).is_settled());
    }

    /// 等待的返回值必须与探测结论一致，且未就绪时不得超出 timeout 太多
    /// （防止把服务启动无限期卡住——超时后要降级继续启动，不是死等）。
    #[test]
    fn wait_respects_probe_and_timeout() {
        let settled = Config::probe_user_config().is_settled();
        let start = std::time::Instant::now();
        let ready = Config::wait_user_config_ready(std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();

        assert_eq!(ready, settled, "返回值应与探测结论一致");
        if settled {
            // 开发机/CI 上漫游根通常存在：必须立即返回，一次 sleep 都不能有。
            assert!(
                elapsed < std::time::Duration::from_millis(250),
                "已就绪却等待了 {elapsed:?}"
            );
        } else {
            assert!(
                elapsed < std::time::Duration::from_secs(2),
                "超时后应降级返回而非死等，实际 {elapsed:?}"
            );
        }
    }

    /// 重试路径：前几次未就绪，之后转就绪 → 必须等到就绪再返回 true。
    /// 这是本修复的核心分支，开发机上探测恒就绪走不到，只能靠注入。
    #[test]
    fn wait_retries_until_ready() {
        let mut calls = 0u32;
        let ready = Config::wait_until_settled(
            || {
                calls += 1;
                if calls < 3 {
                    UserConfigProbe::RoamingUnavailable
                } else {
                    UserConfigProbe::Ready {
                        dir: PathBuf::from("x"),
                        dir_exists: true,
                        file_exists: true,
                    }
                }
            },
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(1),
        );
        assert!(ready, "转为就绪后应返回 true");
        assert_eq!(calls, 3, "应恰好重试到就绪那次为止");
    }

    /// 始终未就绪 → 必须在 timeout 后降级返回 false，而不是死等把服务卡住。
    #[test]
    fn wait_gives_up_after_timeout() {
        let mut calls = 0u32;
        let start = std::time::Instant::now();
        let ready = Config::wait_until_settled(
            || {
                calls += 1;
                UserConfigProbe::RoamingMissing(PathBuf::from("x"))
            },
            std::time::Duration::from_millis(60),
            std::time::Duration::from_millis(10),
        );
        assert!(!ready, "始终未就绪应返回 false");
        assert!(calls > 1, "应至少重试过，实际 {calls} 次");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "应及时放弃，实际 {:?}",
            start.elapsed()
        );
    }

    fn tv(s: &str) -> toml::Value {
        toml::from_str::<toml::Value>(s).expect("测试 TOML 应可解析")
    }

    /// 出厂默认（L1⊕L2）样本：覆盖标量 / 数组 / 多级嵌套。
    fn preset_sample() -> toml::Value {
        tv(r#"
[schema]
active = "wubi86"

[schema.mix]
auto_commit_block_on_pinyin = true
pinyin_only_overflow = true
show_source_hint = false

[schema.codetable]
top_code_commit = true

[keys]
page_keys = ["pageupdown", "minus_equal"]
"#)
    }

    #[test]
    fn prune_removes_redundant_keeps_overrides() {
        let preset = preset_sample();
        let mut user = tv(r#"
[schema]
active = "wubi86_pinyin"

[schema.mix]
auto_commit_block_on_pinyin = false
pinyin_only_overflow = true
show_source_hint = false

[keys]
page_keys = ["pageupdown", "minus_equal"]
"#);
        let removed = prune_redundant(&mut user, &preset);
        assert_eq!(
            removed, 3,
            "pinyin_only_overflow / show_source_hint / page_keys 三个与默认相同"
        );
        assert_eq!(
            get_nested(&user, &["schema", "mix", "auto_commit_block_on_pinyin"]),
            Some(&toml::Value::Boolean(false)),
            "真实覆盖（与默认相反）必须保留"
        );
        assert_eq!(
            get_nested(&user, &["schema", "active"]).and_then(|v| v.as_str()),
            Some("wubi86_pinyin"),
            "真实覆盖必须保留"
        );
        assert!(
            get_nested(&user, &["keys"]).is_none(),
            "唯一子键被删后，空的 [keys] 段应一并回收"
        );
    }

    #[test]
    fn prune_preserves_merged_result() {
        // ★ 本轮修复的核心保证：被删的键都会从 L1⊕L2 回落到同一个值，故清理对**当前**行为
        // 零影响，只影响将来默认值变更能否到达该用户。这条不变量成立，清理才是安全的。
        let preset = preset_sample();
        let user = tv(r#"
[schema]
active = "wubi86_pinyin"

[schema.mix]
auto_commit_block_on_pinyin = false
pinyin_only_overflow = true
show_source_hint = false

[schema.codetable]
top_code_commit = true

[keys]
page_keys = ["pageupdown", "minus_equal"]

[input.punct.custom_mappings]
"'1" = ["1", "＇"]
"#);
        let mut before = preset.clone();
        merge_value(&mut before, user.clone());

        let mut pruned = user.clone();
        let removed = prune_redundant(&mut pruned, &preset);
        assert!(removed > 0, "样本应含冗余键，否则本测试证明不了任何事");
        let mut after = preset.clone();
        merge_value(&mut after, pruned);

        assert_eq!(before, after, "清理前后三层合并结果必须逐键相同");
    }

    #[test]
    fn prune_is_idempotent() {
        let preset = preset_sample();
        let mut user = tv(r#"
[schema.mix]
pinyin_only_overflow = true
show_source_hint = false
"#);
        assert_eq!(prune_redundant(&mut user, &preset), 2);
        assert_eq!(prune_redundant(&mut user, &preset), 0, "二次清理应无事可做");
    }

    /// ★ 模式级注释模板（三态 `Option`，**刻意不进注册表**）不得被写回清理掉。
    ///
    /// 这类键的出厂值是「键不存在」＝跟随全局，故 preset 里没有它们、注册表也不登记
    /// （见 `config_schema::REGISTRY` 的说明）。若哪天有人为了「让设置页能看见」把它们
    /// 补进注册表，`prune_redundant` 的第一道保险就失效——用户手写的模板会在某次保存后
    /// 被静默删掉，表现为「配了几天突然没了」。本测试是那个改动的拦截点。
    #[test]
    fn prune_keeps_mode_comment_templates() {
        let preset = preset_sample();
        let mut user = tv(r#"
[input.temp_english]
comment_template_vertical = "${dict}"
comment_template_horizontal = ""
"#);
        assert_eq!(prune_redundant(&mut user, &preset), 0, "未登记键一律不碰");
        assert!(
            get_nested(
                &user,
                &["input", "temp_english", "comment_template_vertical"]
            )
            .is_some(),
            "用户手写的模式级模板必须原样保留"
        );
        assert!(
            get_nested(
                &user,
                &["input", "temp_english", "comment_template_horizontal"]
            )
            .is_some(),
            "空串（= 本模式不显示注释）同样是有效配置，不得被当成冗余删除"
        );
    }

    #[test]
    fn prune_keeps_keys_absent_from_preset() {
        // 出厂默认里没有的键一律保留：用户自定义标点映射这类**动态键**（键名由用户输入决定，
        // 不可能出现在 preset 里）若被当成冗余删掉就是丢用户数据。废弃键的清理是另一件事，
        // 必须走显式名单，绝不能靠「preset 里没有」来推断。
        let preset = preset_sample();
        let mut user = tv(r#"
[input.punct.custom_mappings]
"'1" = ["1", "＇"]
"#);
        assert_eq!(prune_redundant(&mut user, &preset), 0);
        assert!(get_nested(&user, &["input", "punct", "custom_mappings", "'1"]).is_some());
    }

    #[test]
    fn prune_keeps_unregistered_keys_even_when_matching_preset() {
        // 注册表未登记的键即使与 preset 完全相同也不得删——此处用真实存在过的废弃键
        // `input.code_commit.*`（已迁到 schema.codetable.*，注册表里查不到）。
        // 废弃键清理是另一件事，必须走显式名单：靠「等于 preset」去推断会把语义搞反。
        assert!(
            !crate::config_schema::is_known_key("input.code_commit.auto_commit_at_full"),
            "前提：该键确未登记，否则本测试证明不了 registry 这道保险"
        );
        let preset = tv(r#"
[input.code_commit]
auto_commit_at_full = false
"#);
        let mut user = preset.clone();
        assert_eq!(prune_redundant(&mut user, &preset), 0);
        assert!(
            get_nested(&user, &["input", "code_commit", "auto_commit_at_full"]).is_some(),
            "未登记键必须原样保留"
        );
    }

    #[test]
    fn prune_keeps_map_subpaths() {
        // `input.punct.custom_mappings` 在注册表里是 Map 类型——**整体**才是一个配置项。
        // collect_leaf_paths 会把它下钻成 `...custom_mappings."'1"` 这种伪键，删单条是错的语义
        // （等于悄悄改写用户的标点映射表）。registry 保险必须拦住。
        assert!(
            crate::config_schema::is_known_key("input.punct.custom_mappings"),
            "前提：Map 整体是登记键"
        );
        assert!(
            !crate::config_schema::is_known_key("input.punct.custom_mappings.'1"),
            "前提：其子路径不是登记键"
        );
        let preset = tv(r#"
[input.punct.custom_mappings]
"'1" = ["1", "＇"]
"#);
        let mut user = preset.clone();
        assert_eq!(
            prune_redundant(&mut user, &preset),
            0,
            "Map 子路径不得被当叶子删除"
        );
        assert!(get_nested(&user, &["input", "punct", "custom_mappings", "'1"]).is_some());
    }

    #[test]
    fn remove_nested_reclaims_empty_parents_only() {
        let mut root = tv(r#"
[a.b]
x = 1
y = 2
"#);
        let toml::Value::Table(t) = &mut root else {
            unreachable!()
        };
        assert!(remove_nested(t, &["a", "b", "x"]));
        assert!(
            get_nested(&root, &["a", "b", "y"]).is_some(),
            "兄弟键还在时不得回收父表"
        );
        let toml::Value::Table(t) = &mut root else {
            unreachable!()
        };
        assert!(remove_nested(t, &["a", "b", "y"]));
        assert!(get_nested(&root, &["a"]).is_none(), "父表变空应逐级回收");
    }

    /// `MixModeConfig` 的两条默认值路径必须一致：serde 缺省（读一份没写该键的配置）与
    /// `Default::default()`（测试夹具 / 代码构造）。
    ///
    /// `free_input_takes_select_keys` 的 serde 缺省是 `true`，而 derive 出来的
    /// `bool::default()` 是 `false`——所以 `Default` 是手写的。本测试就是那条约束的守门：
    /// 日后再加带非零默认值的字段而忘了改手写 `Default`，这里会红。
    #[test]
    fn mix_mode_config_serde_default_matches_default_impl() {
        let from_serde: MixModeConfig =
            toml::from_str("").expect("空表应能反序列化出全默认的 MixModeConfig");
        assert_eq!(
            from_serde,
            MixModeConfig::default(),
            "serde 缺省与 Default::default() 必须逐字段一致"
        );
        assert!(
            MixModeConfig::default().free_input_takes_select_keys,
            "夺取二三候选键默认应为开"
        );
    }

    /// 退役键走显式名单清除，且**三类不得误伤**：同段里还活着的键、名字相似但仍在使用的
    /// 另一个键、以及未登记的 Map 子路径。
    #[test]
    fn prune_retired_removes_dead_keys_only() {
        let mut root = tv(r#"
[schema.quick_input]
enable_english = true
enabled = true
decimal_places = 6

[schema.mix]
enable_english = true

[input.punct.custom_mappings]
"/" = ["、", "／", "、", "/"]
"#);
        assert_eq!(prune_retired(&mut root), 2, "两个退役键都应删除");
        assert!(get_nested(&root, &["schema", "quick_input", "enable_english"]).is_none());
        assert!(get_nested(&root, &["schema", "quick_input", "enabled"]).is_none());
        assert!(
            get_nested(&root, &["schema", "quick_input", "decimal_places"]).is_some(),
            "同段里还活着的键不得误删"
        );
        // ★ `schema.mix.enable_english` 是**另一个仍在使用的键**（混输引擎混入英文词库
        //   候选的开关，manager.rs 实读）。它与退役的 `schema.quick_input.enable_english`
        //   只是叶子名相同，按整条路径匹配才不会误伤。
        assert!(
            get_nested(&root, &["schema", "mix", "enable_english"]).is_some(),
            "schema.mix.enable_english 仍在使用，不得误删"
        );
        // Map 子路径同样不在注册表里，靠「未登记就删」会把用户的自定义标点映射删光。
        assert!(
            get_nested(&root, &["input", "punct", "custom_mappings", "/"]).is_some(),
            "Map 子路径不得误删"
        );
        assert_eq!(prune_retired(&mut root), 0, "幂等：再跑一次删 0 个");
    }

    /// 父表被清空时应整段回收——用户配置里 `[schema.quick_input]` 常常只有这两个退役键。
    #[test]
    fn prune_retired_reclaims_emptied_parent_table() {
        let mut root = tv(r#"
[schema.quick_input]
enable_english = true
enabled = true
"#);
        assert_eq!(prune_retired(&mut root), 2);
        assert!(
            get_nested(&root, &["schema", "quick_input"]).is_none(),
            "两个键都删完后空的 [schema.quick_input] 段应一并回收"
        );
    }

    #[test]
    fn collect_leaf_paths_treats_arrays_as_leaves() {
        // 数组整体是一个配置项：下钻进元素会切出无法用 path 表达、也无法与 preset 比对的伪键。
        let v = tv(r#"
[keys]
page_keys = ["a", "b"]

[schema]
active = "x"
"#);
        let mut out = Vec::new();
        collect_leaf_paths(&v, &mut Vec::new(), &mut out);
        out.sort();
        assert_eq!(
            out,
            vec![
                vec!["keys".to_string(), "page_keys".to_string()],
                vec!["schema".to_string(), "active".to_string()],
            ]
        );
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
    fn test_merge_keeps_input_s2t_debug() {
        // 回归：deep-merge 必须保留各段（features 拆解后 s2t 归 input）
        let cfg = merged_with(
            "[input.s2t]\nenabled = true\nvariant = \"s2tw\"\n\
             [debug]\nlog_level = \"trace\"\n",
        );
        assert!(cfg.input.s2t.enabled, "input.s2t.enabled 应被合并");
        assert_eq!(cfg.input.s2t.variant, "s2tw");
        assert_eq!(cfg.debug.log_level, "trace", "debug 段应被合并");
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
        assert_eq!(cfg.schema.active, "wubi86");
    }

    #[test]
    fn test_smart_method_default() {
        let cfg = SymbolConfig::default();
        assert_eq!(cfg.smart_method, SmartMethod::DeleteReplace);
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
        assert_eq!(cfg.smart_method, SmartMethod::DeleteReplace);
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
}
