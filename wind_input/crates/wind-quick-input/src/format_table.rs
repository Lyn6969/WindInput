//! 快捷输入格式表：候选「渲染成什么样、按什么顺序」的外置配置。
//!
//! 设计与约束见 `docs/design/quick-input-format-table.md`。三条要点：
//!
//! - **解析归代码、渲染归配置**：`kind` 是白名单（对应五个已有解析器），配置不能新增；
//! - **组内顺序归 `position`，跨来源顺序仍归 `mix_modes.members`**（不设第二真相源）；
//! - **坏掉也得能打字**：文件缺失/整份解析失败一律回落 [`FormatTable::builtin`]，
//!   单条非法只剔除该条。

use std::path::Path;
use tracing::warn;

/// 解析器类别。恰好对应五个已有解析器，**不可由配置新增**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    /// 完整日期 `2025.12.25`（用户明确给了年）
    Date,
    /// 月日 `12.25`（年由代码补当年）
    ///
    /// 与 [`Self::Date`] 分开而不是共用一套模板：用户只打两段时想要的多半是
    /// 「12月25日」这种不带年的短写法，而替他补上的年份在三段输入里才是他自己打的。
    /// 变量集与 `Date` 相同（年可用，取当前年），差别只在**出厂条目**与用户调整各自记账。
    MonthDay,
    /// 年月 `2025.12`
    YearMonth,
    /// 数字 / 金额（纯数字，或算式求值结果）
    Number,
    /// 算式求值
    Calc,
}

impl FormatKind {
    /// 全部类别，**按 [`Self::group_order`] 升序**。
    ///
    /// 设置页要列出全部类别（含一条候选都没有的），穷尽性测试也要它。此前枚举列表在三处
    /// 各手写一遍，加 `MonthDay` 时漏掉了其中一处（`kind_parse_and_as_str_roundtrip`
    /// 因此一直没覆盖新类别，且测试照绿）——单一真相源就是防这个。
    pub const ALL: &'static [Self] = &[
        Self::Date,
        Self::MonthDay,
        Self::YearMonth,
        Self::Number,
        Self::Calc,
    ];

    /// 配置文件里的写法 → 类别。也用于候选 id（`quick:{kind}:{格式 id}`）的反解析，
    /// 故与 [`Self::as_str`] 必须严格互逆。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "date" => Some(Self::Date),
            "month_day" => Some(Self::MonthDay),
            "year_month" => Some(Self::YearMonth),
            "number" => Some(Self::Number),
            "calc" => Some(Self::Calc),
            _ => None,
        }
    }

    /// 表内分组顺序。**不是**候选的呈现顺序（那由 `mix_modes.members` 决定），
    /// 只用来让 `entries()` 有个稳定的规范表示，好让「出厂文件 == 内置表」可比。
    fn group_order(self) -> u8 {
        match self {
            Self::Date => 0,
            Self::MonthDay => 1,
            Self::YearMonth => 2,
            Self::Number => 3,
            Self::Calc => 4,
        }
    }

    /// 配置文件里的写法（错误信息与测试用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::MonthDay => "month_day",
            Self::YearMonth => "year_month",
            Self::Number => "number",
            Self::Calc => "calc",
        }
    }

    /// 本类支持的变量名（校验用；取值实现见 `crate::vars`）。
    ///
    /// ⚠️ 农历变量给 `Date` 与 `MonthDay`（两者都有确定的年月日），**不给 `YearMonth`**：
    /// 农历月与公历月不是一一对应（闰月、且月首不在公历月初），`2026.12` 根本推不出唯一的
    /// 农历月。放行了只会让用户拿到一个看似合理的错值。
    pub fn supports_var(self, name: &str) -> bool {
        let common_year = matches!(name, "Y" | "YYYY" | "YY" | "YC");
        match self {
            // 月日与完整日期同一套变量：年份取当前年，故 `$Y`/农历照样可用
            // （出厂表借此把「2026年12月25日」留作月日组的次选）。
            Self::Date | Self::MonthDay => {
                common_year
                    || matches!(name, "M" | "MM" | "MC" | "D" | "DD" | "DC")
                    || crate::lunar::is_var(name)
            }
            Self::YearMonth => common_year || matches!(name, "M" | "MM" | "MC"),
            Self::Number => matches!(name, "N" | "THOU" | "CNL" | "CNU" | "DIG" | "AMT"),
            Self::Calc => matches!(name, "EXPR" | "RESULT"),
        }
    }
}

/// 一条格式：解析出的量渲染成候选文本的模板。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatEntry {
    /// 稳定标识（同 kind 内唯一）。日志与将来的候选身份按它认人。
    pub id: String,
    pub kind: FormatKind,
    /// 模板。`$` 变量路径见 [`crate::template`]；含裸 `{` 时走表达式路径。
    pub text: String,
    /// 组内顺序（升序）。缺省时按文件出现序。
    pub position: i32,
}

impl FormatEntry {
    /// 是否走表达式路径（`{amt(unit='圆')}`）而非 `$` 变量替换。
    ///
    /// 分流与 `wind-phrase` 的短语 text 同构，判据收窄为「含裸 `{`」：格式表一条只产
    /// 一条候选，故 `$SS`/`$AA`（多候选）与 `$CC`（副作用动作）都不适用，
    /// 不能直接套用 `is_cmdbar_grammar`。
    pub fn is_expression(&self) -> bool {
        crate::template::has_bare_brace(&self.text)
    }
}

/// 内置默认表。**必须与出厂 `data/system.quick.toml` 逐条一致**（有测试钉住），
/// 它是文件缺失/损坏时的兜底，也是改造前硬编码行为的等价物。
///
/// 顺序即改造前 `generate_*_candidates` 里 `vec![]` 的字面顺序，不要随手调整——
/// 首选变了就是用户可感的行为变更。
const BUILTIN: &[(&str, FormatKind, &str)] = &[
    // 日期：中文 → 全汉字 → ISO 扩展 → ISO 基本 → 斜杠
    ("date.cn", FormatKind::Date, "$Y年$M月$D日"),
    ("date.cn_hans", FormatKind::Date, "$YC年$MC月$DC日"),
    ("date.iso", FormatKind::Date, "$YYYY-$MM-$DD"),
    ("date.basic", FormatKind::Date, "$YYYY$MM$DD"),
    ("date.slash", FormatKind::Date, "$YYYY/$MM/$DD"),
    // 农历排在公历五条之后：首选不变，日期候选 5→7 条仍在一页内。
    // 超出 1900–2100 时这两条自动消失（变量取不到值 → 整条模板作废）。
    ("date.lunar", FormatKind::Date, "农历$LMD"),
    ("date.lunar_ganzhi", FormatKind::Date, "$LY年$LMD"),
    // 月日（只打两段）：不带年的短写法在前——用户没打年份，首选就不该替他补一个。
    // 农历紧随其后（它也不带公历年份），补年的两条垫底：「打 12.25 想要 2026年12月25日」
    // 是真实用法，翻一下仍取得到。**改这里必须同步出厂 `data/system.quick.toml`**
    // （`factory_file_matches_builtin_table` 逐条比对）。
    ("month_day.cn", FormatKind::MonthDay, "$M月$D日"),
    ("month_day.cn_hans", FormatKind::MonthDay, "$MC月$DC日"),
    ("month_day.iso", FormatKind::MonthDay, "$MM-$DD"),
    ("month_day.slash", FormatKind::MonthDay, "$MM/$DD"),
    ("month_day.lunar", FormatKind::MonthDay, "农历$LMD"),
    (
        "month_day.with_year_cn",
        FormatKind::MonthDay,
        "$Y年$M月$D日",
    ),
    (
        "month_day.with_year_iso",
        FormatKind::MonthDay,
        "$YYYY-$MM-$DD",
    ),
    // 年月：与完整日期同构
    ("year_month.cn", FormatKind::YearMonth, "$Y年$M月"),
    ("year_month.cn_hans", FormatKind::YearMonth, "$YC年$MC月"),
    ("year_month.iso", FormatKind::YearMonth, "$YYYY-$MM"),
    ("year_month.slash", FormatKind::YearMonth, "$YYYY/$MM"),
    // 数字：金额 → 中文小写 → 中文大写 → 逐位 → 千分位
    ("number.amount", FormatKind::Number, "$AMT"),
    ("number.cn_lower", FormatKind::Number, "$CNL"),
    ("number.cn_upper", FormatKind::Number, "$CNU"),
    ("number.digits", FormatKind::Number, "$DIG"),
    ("number.thousands", FormatKind::Number, "$THOU"),
    // 计算：结果作首选，等式次之
    ("calc.result", FormatKind::Calc, "$RESULT"),
    ("calc.equation", FormatKind::Calc, "$EXPR=$RESULT"),
];

/// 用户对某一类格式的调整（右键菜单产生，存在 userdata.redb，**不写回格式表文件**）。
///
/// 本类型是「中立数据」：`wind-store` 有自己的可序列化记录，协调器负责转换。
/// 两处不合并是刻意的——store 不依赖业务类型，与 shadow 同一条纪律。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatAdjust {
    /// 被移动过的条目 `(格式 id, 组内目标下标)`。
    /// **LIFO，index 0 = 最新**，应用时逆序遍历。
    pub moved: Vec<(String, usize)>,
    /// 被停用的格式 id。
    pub disabled: Vec<String>,
}

impl FormatAdjust {
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.disabled.is_empty()
    }

    /// 这条格式是否被用户调整过（移动或停用）。
    ///
    /// 语义是「有没有用户规则」，不是「位置是否与出厂不同」——把某条移回原位也算调整过，
    /// 因为那条规则确实在库里（将来出厂顺序变了它就会生效）。设置页据此决定
    /// 「恢复此条」能不能点，与 `wind_store::QuickFormatRecord::has_rule` 同一口径。
    pub fn has_rule(&self, id: &str) -> bool {
        self.moved.iter().any(|(i, _)| i == id) || self.disabled.iter().any(|d| d == id)
    }
}

/// 设置页列表里的一行：条目 + 它当前的状态。
///
/// 与候选生成的 [`FormatTable::entries_of_adjusted`] 是**两种口径**，刻意分开：
/// 候选要的是「用户现在能看到什么」（停用项必须消失），设置页要的是「用户能管理什么」
/// （停用项必须还在，否则再也开不回来——正是这个缺口让右键菜单只能提供「整类重置」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatEntryView<'a> {
    pub entry: &'a FormatEntry,
    /// 是否启用（停用项照样出现在本列表里，只是这里为 false）。
    pub enabled: bool,
    /// 用户是否调整过它（见 [`FormatAdjust::has_rule`]）。
    pub adjusted: bool,
}

/// 格式表。条目已按 kind 分组、组内按 position 稳定排序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatTable {
    entries: Vec<FormatEntry>,
}

impl Default for FormatTable {
    fn default() -> Self {
        Self::builtin()
    }
}

impl FormatTable {
    /// 代码内置的出厂表（兜底路径）。
    ///
    /// `position` 按**组内** 1-based 计数，与出厂 TOML 的写法一致——两者要能直接比相等
    /// （见 `tests/factory_table.rs`），全局序号的话每次给某一类加条目都会让另一类的
    /// position 整体位移。
    pub fn builtin() -> Self {
        let mut entries: Vec<FormatEntry> = Vec::with_capacity(BUILTIN.len());
        for (id, kind, text) in BUILTIN {
            let position = entries.iter().filter(|e| e.kind == *kind).count() as i32 + 1;
            entries.push(FormatEntry {
                id: (*id).to_string(),
                kind: *kind,
                text: (*text).to_string(),
                position,
            });
        }
        entries.sort_by_key(|e| (e.kind.group_order(), e.position));
        Self { entries }
    }

    /// 解析 TOML 文本。整份语法错误返回 `Err`（调用方回落 [`Self::builtin`]）；
    /// **单条非法只剔除该条**并告警，其余照常——一条写错不该让整个快捷输入退回出厂。
    pub fn parse(toml_text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawFile = toml::from_str(toml_text)?;
        let mut entries: Vec<FormatEntry> = Vec::with_capacity(raw.formats.len());
        for (i, r) in raw.formats.into_iter().enumerate() {
            let Some(kind) = FormatKind::parse(&r.kind) else {
                warn!(
                    "快捷输入格式表: 跳过 id={} —— 未知 kind {:?}（可用: date/month_day/year_month/number/calc）",
                    r.id, r.kind
                );
                continue;
            };
            if let Err(e) = validate_format_text(kind, &r.text) {
                warn!("快捷输入格式表: 跳过 id={} —— {}", r.id, e);
                continue;
            }
            if entries.iter().any(|e| e.kind == kind && e.id == r.id) {
                warn!(
                    "快捷输入格式表: 跳过重复 id={}（kind={}）",
                    r.id,
                    kind.as_str()
                );
                continue;
            }
            entries.push(FormatEntry {
                id: r.id,
                kind,
                text: r.text,
                // 缺省时用文件出现序。它与显式 position 混排时可能重叠，但同值保持出现序，
                // 结果仍与「作者看到的文件顺序」一致。
                position: r.position.unwrap_or(i as i32),
            });
        }
        // 先按 kind 分组、组内按 position。**稳定**排序，故同 position 保持文件出现序。
        // position 是组内序号（出厂文件里每类都从 1 起），跨类比较没有意义，
        // 分组是为了让 `entries()` 有个规范表示。
        entries.sort_by_key(|e| (e.kind.group_order(), e.position));
        Ok(Self { entries })
    }

    /// 从解析好的路径加载；`None`（两处都没有）或读取/解析失败一律回落
    /// [`Self::builtin`] 并告警——配置坏掉不能导致「打不出字」。
    ///
    /// 路径解析本身是调用方的事（`Config::resolve_data_file`，含用户覆盖日志）。
    pub fn load(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            warn!("快捷输入格式表: system.quick.toml 两处均不存在，回落内置默认表（部署可能损坏）");
            return Self::builtin();
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "快捷输入格式表: 读取失败，回落内置默认表 {}: {}",
                    path.display(),
                    e
                );
                return Self::builtin();
            }
        };
        match Self::parse(&text) {
            Ok(t) if t.is_empty() => {
                warn!(
                    "快捷输入格式表: {} 未产出任何有效条目，回落内置默认表",
                    path.display()
                );
                Self::builtin()
            }
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "快捷输入格式表: 解析失败，回落内置默认表 {}: {}",
                    path.display(),
                    e
                );
                Self::builtin()
            }
        }
    }

    /// 某类的条目（已按 position 排序）。
    pub fn entries_of(&self, kind: FormatKind) -> impl Iterator<Item = &FormatEntry> {
        self.entries.iter().filter(move |e| e.kind == kind)
    }

    /// 某类的条目，**已应用用户调整**（停用剔除 + 移动重排）。
    ///
    /// 算法与 `shadow` 的规则应用同构：
    ///
    /// 1. 取基表该类条目（已按 `position` 排序）；
    /// 2. 剔除 `disabled`；
    /// 3. **逆序**遍历 `moved`，每条：取出该 id，插入到目标下标。
    ///
    /// 逆序是 LIFO 语义——最新的操作最后应用，故优先级最高。
    ///
    /// **没被碰过的条目保持基表顺序**，所以将来出厂新增一条格式，它不在 `moved`/`disabled`
    /// 里，会自然落在它的出厂位置；若改存完整 id 序列，就得再定一条「新增格式排哪」的
    /// 规则，怎么定都会让人意外。
    ///
    /// 找不到的 id（高级用户改文件时删掉了那条）静默忽略，不清理——他可能过会儿又加回来。
    pub fn entries_of_adjusted<'a>(
        &'a self,
        kind: FormatKind,
        adjust: &FormatAdjust,
    ) -> Vec<&'a FormatEntry> {
        let mut list: Vec<&FormatEntry> = self
            .entries_of(kind)
            .filter(|e| !adjust.disabled.iter().any(|d| d == &e.id))
            .collect();
        for (id, position) in adjust.moved.iter().rev() {
            let Some(from) = list.iter().position(|e| &e.id == id) else {
                continue; // 孤儿规则：基表里没有这个 id
            };
            let entry = list.remove(from);
            // 下标可能越界：条目被停用、或基表条目数变少（用户改了文件）
            let to = (*position).min(list.len());
            list.insert(to, entry);
        }
        list
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 全部条目（设置页/调试用）。
    pub fn entries(&self) -> &[FormatEntry] {
        &self.entries
    }

    /// 某类的条目，**设置页口径**：启用项按候选实际顺序，停用项**留在原位**。
    ///
    /// ★ 启用项的顺序直接取 [`Self::entries_of_adjusted`] 的结果，**不重算**。同一个
    /// 「第几位」由两段代码各算一遍，迟早漂移，而症状是「设置页显示的顺序与实际候选不符」
    /// ——用户点了上移，列表动了、候选没动，且两边都没有报错。
    ///
    /// ## 停用项为什么必须留在原位
    ///
    /// 停用项不在候选里，看似「没有正确位置」，最初因此把它们沉到该类末尾。**实测被否**：
    /// 停用是**可逆、反复试**的动作，沉底会让同一行在停用/启用之间来回跳——管理界面里
    /// 「行不乱动」比「位置反映候选顺序」重要得多。
    ///
    /// 故给每个停用项找一个锚点（它前面最近的那个启用条目），插在锚点之后；前面没有启用
    /// 条目时排到最前。于是**停用与启用的位置一致**，来回切一行都不动。
    ///
    /// ## 锚点的参照系是「假设全部启用」的顺序，不是基表
    ///
    /// 这一点决定了「先调序、再停用同一条」会不会跳。位置信息其实一直都在——**停用不清
    /// `moved` 规则**（两者是独立字段），丢失只发生在计算环节：[`Self::entries_of_adjusted`]
    /// 先剔除停用项，那条 `moved` 就找不到目标、成了孤儿规则被跳过。
    ///
    /// 所以锚点不查基表，而是查「把 `disabled` 清空后重算一遍」的顺序——那里每条的 `moved`
    /// 都生效。于是被移到首位又停用的条目仍显示在首位。零新增状态，代价是每类多算一次
    /// 排序（至多七条，可忽略）。
    ///
    /// ```text
    /// 基表 [A,B,C]、moved=[(C,0)]、C 停用
    ///   假设全启用  [C,A,B]   ← C 的 moved 生效
    ///   真实候选    [A,B]     ← C 的 moved 成了孤儿
    ///   C 的前驱: 无 → 插最前 ⇒ 视图 [C,A,B]，与停用前一致
    /// ```
    ///
    /// ⚠️ 不能图省事写成「不剔除停用项、直接对全表应用 `moved`」：`moved` 的下标是
    /// 「剔除停用项之后」的位置，把停用项留在列表里再套同一个下标，启用项之间的相对顺序
    /// 就会与候选不一致。举例——基表 `[A,B,C,D]`、A 停用、`moved=[(D,1)]`：候选是
    /// `[B,D,C]`，而直接套下标得到 `[A,D,B,C]`（启用项成了 `[D,B,C]`）。
    ///
    /// 已知限制：**被调序过**的条目停用后会回到它的出厂位置（它的 `moved` 规则仍在库里，
    /// 启用后即恢复）。这个跳动是一次性的，且反映了「它当前不参与排序」这一事实。
    pub fn entries_of_view<'a>(
        &'a self,
        kind: FormatKind,
        adjust: &FormatAdjust,
    ) -> Vec<FormatEntryView<'a>> {
        let is_disabled = |id: &str| adjust.disabled.iter().any(|d| d == id);
        let mut out: Vec<FormatEntryView<'a>> = self
            .entries_of_adjusted(kind, adjust)
            .into_iter()
            .map(|entry| FormatEntryView {
                enabled: true,
                adjusted: adjust.has_rule(&entry.id),
                entry,
            })
            .collect();

        // 「假设全部启用」的顺序：停用项的位置从这里读，它们的 moved 规则在此生效
        // （见上方 doc）。只借 `moved`，`disabled` 清空。
        let as_if_all_enabled = self.entries_of_adjusted(
            kind,
            &FormatAdjust {
                moved: adjust.moved.clone(),
                disabled: Vec::new(),
            },
        );

        // 停用项按该顺序归组，每组挂在同一个锚点后面（连续几条停用的保持彼此相对顺序）。
        let mut groups: Vec<(Option<&str>, Vec<&'a FormatEntry>)> = Vec::new();
        let mut anchor: Option<&str> = None;
        for e in as_if_all_enabled {
            if is_disabled(&e.id) {
                match groups.last_mut() {
                    Some((a, v)) if *a == anchor => v.push(e),
                    _ => groups.push((anchor, vec![e])),
                }
            } else {
                anchor = Some(&e.id);
            }
        }
        // 从后往前插入：插入点每次按锚点 id 重新定位，故靠后的插入不会挪动靠前锚点的下标。
        for (anchor, items) in groups.iter().rev() {
            let at = match anchor {
                None => 0,
                Some(id) => out
                    .iter()
                    .position(|v| v.entry.id == *id)
                    .map(|i| i + 1)
                    .unwrap_or(out.len()),
            };
            for (k, entry) in items.iter().enumerate() {
                out.insert(
                    at + k,
                    FormatEntryView {
                        entry,
                        enabled: false,
                        // 在 disabled 里即已被调整过，无需再查。
                        adjusted: true,
                    },
                );
            }
        }
        out
    }
}

/// 模板静态校验：变量名必须在本 kind 的白名单内。
///
/// 在**加载期**做而不是渲染期：渲染是每次按键都走的热路径，把告警放那里会刷屏，
/// 且用户改错一个变量名应当在启动日志里一次性看到。
///
/// 对外公开是给设置页用的：那里的容错策略与文件加载**相反**——加载遇到坏条目
/// 「剔除该条 + 一条 warn」（坏文件也得能打字），而设置页必须在保存前**拒绝**并把
/// 原因给用户看。同一份判据两处共用，否则设置页放行的模板会在下次启动时被静默剔除，
/// 用户只看到「我加的格式没了」。
pub fn validate_format_text(kind: FormatKind, text: &str) -> Result<(), String> {
    if text.is_empty() {
        return Err("模板为空".into());
    }
    if crate::template::has_bare_brace(text) {
        // 表达式路径：函数名/参数由 cmdbar 在求值期校验（本 crate 不依赖它）。
        // 这里只拒绝混用——两套语法在同一条模板里会让人以为 `${Y}年{month()}月` 能work，
        // 实际上变量路径根本不认识 `{month()}`，会把它原样上屏。
        if crate::template::has_variable(text) {
            return Err("不能在同一条模板里混用 $变量 与 {表达式}，二选一".into());
        }
        return Ok(());
    }
    // RefCell 而非 &mut：`expand` 收的是 `Fn`（渲染期要能重复调用），闭包不能可变捕获。
    let bad: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let ok = crate::template::expand(text, |name| {
        if kind.supports_var(name) {
            Some(String::new())
        } else {
            let mut b = bad.borrow_mut();
            if b.is_none() {
                *b = Some(name.to_string());
            }
            None
        }
    });
    match (ok, bad.into_inner()) {
        (Some(_), _) => Ok(()),
        (None, Some(name)) => Err(format!("kind={} 不支持变量 ${}", kind.as_str(), name)),
        (None, None) => Err("模板语法错误（`${` 未闭合）".into()),
    }
}

#[derive(serde::Deserialize)]
struct RawFile {
    #[serde(default)]
    formats: Vec<RawEntry>,
}

#[derive(serde::Deserialize)]
struct RawEntry {
    id: String,
    kind: String,
    text: String,
    #[serde(default)]
    position: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ `parse` 与 `as_str` 必须严格互逆——候选 id 靠这对函数往返，
    /// 一侧改了名字另一侧没改，右键就会认不出这条候选（且没有任何报错）。
    #[test]
    fn kind_parse_and_as_str_roundtrip() {
        for &k in FormatKind::ALL {
            assert_eq!(FormatKind::parse(k.as_str()), Some(k));
        }
        assert!(FormatKind::parse("weather").is_none());
    }

    /// `ALL` 是「全部类别」的单一真相源，它自己漏一项就会让所有依赖它的穷尽性测试
    /// 一起失效（而且全都照绿）。故这里独立钉住两件事：条数，以及与 `group_order` 同序。
    #[test]
    fn all_kinds_is_exhaustive_and_ordered() {
        assert_eq!(FormatKind::ALL.len(), 5, "新增类别必须同步 ALL");
        let orders: Vec<u8> = FormatKind::ALL.iter().map(|k| k.group_order()).collect();
        let mut sorted = orders.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(orders, sorted, "ALL 必须按 group_order 升序且无重复");
    }

    #[test]
    fn builtin_covers_every_kind() {
        let t = FormatTable::builtin();
        for &k in FormatKind::ALL {
            assert!(
                t.entries_of(k).next().is_some(),
                "内置表缺 kind={}",
                k.as_str()
            );
        }
    }

    #[test]
    fn builtin_templates_all_validate() {
        // 内置表自身必须过校验——写错变量名会静默丢条目
        for e in FormatTable::builtin().entries() {
            assert!(
                validate_format_text(e.kind, &e.text).is_ok(),
                "内置条目 {} 校验失败: {:?}",
                e.id,
                validate_format_text(e.kind, &e.text)
            );
        }
    }

    #[test]
    fn parse_orders_by_position_not_file_order() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "b"
kind = "date"
text = "$Y"
position = 2

[[formats]]
id = "a"
kind = "date"
text = "$M"
position = 1
"#,
        )
        .unwrap();
        let ids: Vec<&str> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn missing_position_falls_back_to_file_order() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "first"
kind = "date"
text = "$Y"

[[formats]]
id = "second"
kind = "date"
text = "$M"
"#,
        )
        .unwrap();
        let ids: Vec<&str> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, vec!["first", "second"]);
    }

    /// ★ 单条非法只剔除该条，其余存活——否则用户改错一个变量名，整个快捷输入退回出厂。
    #[test]
    fn one_bad_entry_does_not_kill_the_rest() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "good"
kind = "date"
text = "$Y年"

[[formats]]
id = "unknown_var"
kind = "date"
text = "$NOPE"

[[formats]]
id = "unknown_kind"
kind = "weather"
text = "$Y"

[[formats]]
id = "wrong_kind_var"
kind = "calc"
text = "$Y"

[[formats]]
id = "expr"
kind = "date"
text = "{month(cn='true')}"

[[formats]]
id = "mixed_syntax"
kind = "date"
text = "${Y}年{month()}月"
"#,
        )
        .unwrap();
        let ids: Vec<&str> = t.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["good", "expr"],
            "合法的变量模板与表达式模板都留下，其余剔除"
        );
    }

    /// 表达式模板的函数名/参数由 cmdbar 在求值期校验，加载期不认识它们——
    /// 但**混用两套语法**必须在加载期就拒绝：变量路径不认识 `{month()}`，
    /// 放行的话会把它当字面量上屏。
    #[test]
    fn expression_entries_are_accepted_but_mixing_is_not() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "expr.unknown_func"
kind = "date"
text = "{no_such_func()}"
"#,
        )
        .unwrap();
        assert_eq!(t.entries().len(), 1, "未知函数留到求值期报，加载期放行");
        assert!(t.entries()[0].is_expression());

        assert!(validate_format_text(FormatKind::Date, "${Y}年{month()}月").is_err());
        assert!(validate_format_text(FormatKind::Date, "$Y年{month()}月").is_err());
    }

    #[test]
    fn duplicate_id_within_kind_is_dropped() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "dup"
kind = "date"
text = "$Y"

[[formats]]
id = "dup"
kind = "date"
text = "$M"
"#,
        )
        .unwrap();
        assert_eq!(t.entries().len(), 1);
        assert_eq!(t.entries()[0].text, "$Y", "保留先出现的那条");
    }

    #[test]
    fn same_id_in_different_kinds_is_allowed() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "cn"
kind = "date"
text = "$Y"

[[formats]]
id = "cn"
kind = "calc"
text = "$RESULT"
"#,
        )
        .unwrap();
        assert_eq!(t.entries().len(), 2);
    }

    #[test]
    fn syntax_error_is_reported_to_caller() {
        assert!(FormatTable::parse("[[formats]]\nid = ").is_err());
    }

    // ───────── 用户调整（FormatAdjust）─────────

    fn date_ids(t: &FormatTable, a: &FormatAdjust) -> Vec<String> {
        t.entries_of_adjusted(FormatKind::Date, a)
            .iter()
            .map(|e| e.id.clone())
            .collect()
    }

    // ───────── 设置页口径（entries_of_view）─────────

    /// ★★ 设置页里**启用项的顺序必须与候选完全一致**。
    ///
    /// 这是本函数存在的全部风险所在：两处各算一遍「第几位」，漂移后的症状是
    /// 「用户在设置页点上移，列表动了、实际候选没动」，两边都不报错。故这里直接拿
    /// `entries_of_adjusted`（候选那条路径）的输出做基准，而不是另写一份期望顺序——
    /// 后者只能证明「视图等于我以为的顺序」，证明不了「视图等于候选」。
    #[test]
    fn view_enabled_order_equals_candidate_order() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.iso".into(), 0), ("date.lunar".into(), 2)],
            disabled: vec!["date.basic".into()],
        };
        let candidate: Vec<&str> = t
            .entries_of_adjusted(FormatKind::Date, &a)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let view_enabled: Vec<&str> = t
            .entries_of_view(FormatKind::Date, &a)
            .iter()
            .filter(|v| v.enabled)
            .map(|v| v.entry.id.as_str())
            .collect();
        assert_eq!(view_enabled, candidate);
    }

    /// 停用项**留在列表里**（这正是设置页存在的理由），且条数一条不少。
    #[test]
    fn view_keeps_disabled_entries_listed() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            disabled: vec!["date.cn".into(), "date.basic".into()],
            ..Default::default()
        };
        let view = t.entries_of_view(FormatKind::Date, &a);
        assert_eq!(
            view.len(),
            t.entries_of(FormatKind::Date).count(),
            "候选里少了两条，管理列表里一条不少"
        );
        assert_eq!(view.iter().filter(|v| !v.enabled).count(), 2);
    }

    /// ★★ **停用一行不该让它换位置**。
    ///
    /// 停用是可逆、反复试的动作；让停用项沉到末尾（最初的做法）会使同一行在停用/启用之间
    /// 来回跳。这里对**每一条**都验一遍「停用前的下标 == 停用后的下标」，而不是只挑一条
    /// ——首条、末条、中间条走的是锚点算法的不同分支（锚点为 None / 锚点在末尾 / 一般情形）。
    #[test]
    fn disabling_a_row_does_not_move_it() {
        let t = FormatTable::builtin();
        for &kind in FormatKind::ALL {
            let base: Vec<String> = t
                .entries_of_view(kind, &FormatAdjust::default())
                .iter()
                .map(|v| v.entry.id.clone())
                .collect();
            for (i, id) in base.iter().enumerate() {
                let a = FormatAdjust {
                    disabled: vec![id.clone()],
                    ..Default::default()
                };
                let after: Vec<String> = t
                    .entries_of_view(kind, &a)
                    .iter()
                    .map(|v| v.entry.id.clone())
                    .collect();
                assert_eq!(
                    after,
                    base,
                    "停用 {id}（kind={}, 第 {i} 行）后整列顺序不该有任何变化",
                    kind.as_str()
                );
            }
        }
    }

    /// ★★ **先调序、再停用同一条，位置也不该变**。
    ///
    /// 这条比 `disabling_a_row_does_not_move_it` 更严：那条测的是从未调序过的条目（锚点
    /// 落在基表位置即可），这条要求锚点参照「假设全部启用」的顺序，否则被移到首位的条目
    /// 一停用就跳回出厂位置。位置信息本就没丢（停用不清 `moved`），全靠参照系选对。
    #[test]
    fn disabling_a_reordered_row_keeps_its_new_position() {
        let t = FormatTable::builtin();
        for &kind in FormatKind::ALL {
            let ids: Vec<String> = t.entries_of(kind).map(|e| e.id.clone()).collect();
            if ids.len() < 3 {
                continue; // calc 只有两条，移动空间不足以体现差别
            }
            // 把末条移到每一个可能的位置，各验一遍「停用前后视图不变」。
            for target in 0..ids.len() {
                let moved = vec![(ids[ids.len() - 1].clone(), target)];
                let before: Vec<String> = t
                    .entries_of_view(
                        kind,
                        &FormatAdjust {
                            moved: moved.clone(),
                            disabled: Vec::new(),
                        },
                    )
                    .iter()
                    .map(|v| v.entry.id.clone())
                    .collect();
                let after: Vec<String> = t
                    .entries_of_view(
                        kind,
                        &FormatAdjust {
                            moved: moved.clone(),
                            disabled: vec![ids[ids.len() - 1].clone()],
                        },
                    )
                    .iter()
                    .map(|v| v.entry.id.clone())
                    .collect();
                assert_eq!(
                    after,
                    before,
                    "kind={} 把 {} 移到第 {target} 位后再停用，顺序不该变",
                    kind.as_str(),
                    ids[ids.len() - 1]
                );
            }
        }
    }

    /// 连续多条停用也各自留在原位（含首条——它的锚点是 `None`）。
    #[test]
    fn several_disabled_rows_each_stay_put() {
        let t = FormatTable::builtin();
        let base: Vec<String> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.clone())
            .collect();
        // 首条 + 相邻两条 + 末条：覆盖锚点为 None、同锚点多条、锚点在末尾三种情形。
        let a = FormatAdjust {
            disabled: vec![
                base[0].clone(),
                base[2].clone(),
                base[3].clone(),
                base[base.len() - 1].clone(),
            ],
            ..Default::default()
        };
        let after: Vec<String> = t
            .entries_of_view(FormatKind::Date, &a)
            .iter()
            .map(|v| v.entry.id.clone())
            .collect();
        assert_eq!(after, base, "四条同时停用，顺序仍与出厂一致");
    }

    /// 停用项夹在中间之后，**启用项的相对顺序仍须与候选完全一致**。
    ///
    /// 这条与 `view_enabled_order_equals_candidate_order` 的区别在于：那条测的是纯调序，
    /// 这条测「调序 + 停用」混合——正是「不剔除就套 `moved` 下标」会算错的场景
    /// （见 `entries_of_view` 的 ⚠️ 段）。
    #[test]
    fn enabled_order_still_matches_candidates_with_disabled_in_between() {
        let t = FormatTable::builtin();
        let base: Vec<String> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.clone())
            .collect();
        // 停用首条 + 把末条移到下标 1：直接套下标会得到与候选不同的启用序。
        let a = FormatAdjust {
            moved: vec![(base[base.len() - 1].clone(), 1)],
            disabled: vec![base[0].clone()],
        };
        let candidate: Vec<&str> = t
            .entries_of_adjusted(FormatKind::Date, &a)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let view_enabled: Vec<&str> = t
            .entries_of_view(FormatKind::Date, &a)
            .iter()
            .filter(|v| v.enabled)
            .map(|v| v.entry.id.as_str())
            .collect();
        assert_eq!(view_enabled, candidate);
    }

    /// `adjusted` 标记决定设置页「恢复此条」能不能点：移动过、停用过都算，
    /// 没碰过的条目为 false。
    #[test]
    fn view_marks_adjusted_entries() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.lunar".into(), 0)],
            disabled: vec!["date.basic".into()],
        };
        let view = t.entries_of_view(FormatKind::Date, &a);
        let flag = |id: &str| {
            view.iter()
                .find(|v| v.entry.id == id)
                .map(|v| v.adjusted)
                .unwrap()
        };
        assert!(flag("date.lunar"), "移动过");
        assert!(flag("date.basic"), "停用过");
        assert!(!flag("date.cn"), "没碰过的不该显示成已调整");
    }

    /// 空调整下，视图 == 基表原序、全部启用、全部未调整。
    #[test]
    fn view_without_adjust_is_base_table() {
        let t = FormatTable::builtin();
        for &k in FormatKind::ALL {
            let view = t.entries_of_view(k, &FormatAdjust::default());
            let base: Vec<&str> = t.entries_of(k).map(|e| e.id.as_str()).collect();
            let got: Vec<&str> = view.iter().map(|v| v.entry.id.as_str()).collect();
            assert_eq!(got, base, "kind={}", k.as_str());
            assert!(view.iter().all(|v| v.enabled && !v.adjusted));
        }
    }

    /// 空调整 == 基表原序。
    #[test]
    fn empty_adjust_is_identity() {
        let t = FormatTable::builtin();
        let base: Vec<String> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(date_ids(&t, &FormatAdjust::default()), base);
    }

    #[test]
    fn disabled_entry_is_dropped() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            disabled: vec!["date.basic".into()],
            ..Default::default()
        };
        let ids = date_ids(&t, &a);
        assert!(!ids.contains(&"date.basic".to_string()));
        assert!(ids.contains(&"date.cn".to_string()), "其余不受影响");
    }

    #[test]
    fn move_to_front() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.lunar".into(), 0)],
            ..Default::default()
        };
        assert_eq!(date_ids(&t, &a)[0], "date.lunar");
    }

    /// ★ 逆序遍历 = LIFO：后写入的规则（index 0）优先级最高。
    ///
    /// 两条规则都想占 0 号位时，最新的那条赢。顺序搞反的话，用户会发现
    /// 「我刚调的那条被上一次的调整顶掉了」。
    #[test]
    fn newest_move_wins() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            // index 0 = 最新
            moved: vec![("date.iso".into(), 0), ("date.lunar".into(), 0)],
            ..Default::default()
        };
        assert_eq!(date_ids(&t, &a)[0], "date.iso", "最新的规则赢");
    }

    /// 移到中间位置（上移/下移用的就是这条路径）。
    #[test]
    fn move_to_middle() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.lunar".into(), 2)],
            ..Default::default()
        };
        assert_eq!(date_ids(&t, &a)[2], "date.lunar");
    }

    /// ★ 越界下标不 panic，钳到末尾——条目可能因停用或用户改文件而变少。
    #[test]
    fn out_of_range_position_clamps() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.cn".into(), 999)],
            ..Default::default()
        };
        let ids = date_ids(&t, &a);
        assert_eq!(ids.last().unwrap(), "date.cn");
        assert_eq!(ids.len(), t.entries_of(FormatKind::Date).count());
    }

    /// ★ 孤儿规则（基表里没有该 id）静默忽略，不影响其余条目。
    ///
    /// 高级用户整份覆盖格式表、删掉了某条时就会这样。
    #[test]
    fn orphan_rule_is_ignored() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.nonexistent".into(), 0), ("date.lunar".into(), 0)],
            disabled: vec!["date.alsogone".into()],
        };
        let ids = date_ids(&t, &a);
        assert_eq!(ids[0], "date.lunar");
        assert_eq!(ids.len(), t.entries_of(FormatKind::Date).count());
    }

    /// 停用 + 移动叠加：先剔除再重排，下标按剔除后的列表算。
    #[test]
    fn disable_and_move_compose() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.slash".into(), 0)],
            disabled: vec!["date.cn".into()],
        };
        let ids = date_ids(&t, &a);
        assert_eq!(ids[0], "date.slash");
        assert!(!ids.contains(&"date.cn".to_string()));
        assert_eq!(ids.len(), t.entries_of(FormatKind::Date).count() - 1);
    }

    /// 被停用的条目即使有移动规则也不复活。
    #[test]
    fn disabled_entry_stays_out_even_if_moved() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("date.basic".into(), 0)],
            disabled: vec!["date.basic".into()],
        };
        assert!(!date_ids(&t, &a).contains(&"date.basic".to_string()));
    }

    /// 调整只作用于本类，不串类。
    #[test]
    fn adjust_does_not_leak_across_kinds() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            moved: vec![("number.amount".into(), 0)],
            disabled: vec!["calc.result".into()],
        };
        // date 类不含这些 id，故完全不受影响
        let base: Vec<String> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(date_ids(&t, &a), base);
    }

    #[test]
    fn load_without_path_falls_back_to_builtin() {
        assert_eq!(FormatTable::load(None), FormatTable::builtin());
    }

    #[test]
    fn load_of_empty_file_falls_back_to_builtin() {
        // 空表（合法 TOML 但零条目）必须兜底，否则快捷输入整个哑掉
        let dir = std::env::temp_dir().join("wind_quick_fmt_empty_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("system.quick.toml");
        std::fs::write(&p, "# 空表\n").unwrap();
        assert_eq!(FormatTable::load(Some(&p)), FormatTable::builtin());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_of_broken_file_falls_back_to_builtin() {
        let dir = std::env::temp_dir().join("wind_quick_fmt_broken_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("system.quick.toml");
        std::fs::write(&p, "[[formats]]\nid = ").unwrap();
        assert_eq!(FormatTable::load(Some(&p)), FormatTable::builtin());
        let _ = std::fs::remove_file(&p);
    }
}
