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
            Self::Calc => matches!(name, "EXPR" | "RESULT" | "EXACT"),
        }
    }

    /// 本类可用变量的**展示清单**（名字 + 一句话，含示例值），供设置页的模板输入框提示。
    ///
    /// ★ 放在 core 而不是设置仓：变量白名单是 [`Self::supports_var`] 的知识，设置页硬编码
    /// 一份就会在加新变量时静默过时（用户照着提示写不出新变量，或提示里的变量已被删）。
    ///
    /// 与 `supports_var` 是**两份数据**（这里多了说明文字），故有两道测试钉住：
    /// 名字必须都能通过 `supports_var`，且每类的条数写死——加变量时那个数字会红，
    /// 提醒同步这里与文档站的变量表。
    pub fn var_hints(self) -> &'static [(&'static str, &'static str)] {
        // 公历年月日（date / month_day / year_month 共用前几项）
        const YEAR: &[(&str, &str)] = &[
            ("Y", "年，如 2026"),
            ("YYYY", "四位年，2026"),
            ("YY", "两位年，26"),
            ("YC", "汉字年，二〇二六"),
        ];
        const MONTH: &[(&str, &str)] =
            &[("M", "月，3"), ("MM", "两位月，03"), ("MC", "汉字月，三")];
        const DAY: &[(&str, &str)] = &[("D", "日，5"), ("DD", "两位日，05"), ("DC", "汉字日，五")];
        const LUNAR: &[(&str, &str)] = &[
            ("LY", "农历干支年，丙午"),
            ("LYN", "农历年数字，2026（以正月初一为界，可能与公历差 1）"),
            ("LZ", "生肖，马"),
            ("LM", "农历月，四月"),
            ("LD", "农历日，廿九"),
            ("LMD", "农历月日，四月廿九"),
            ("LF", "农历节日，端午节（非节日为空）"),
        ];
        // 拼接结果必须是 `'static`，故各类各写一份完整的常量（拼接要分配）。
        const DATE_ALL: &[(&str, &str)] = &[
            YEAR[0], YEAR[1], YEAR[2], YEAR[3], MONTH[0], MONTH[1], MONTH[2], DAY[0], DAY[1],
            DAY[2], LUNAR[0], LUNAR[1], LUNAR[2], LUNAR[3], LUNAR[4], LUNAR[5], LUNAR[6],
        ];
        const YEAR_MONTH_ALL: &[(&str, &str)] = &[
            YEAR[0], YEAR[1], YEAR[2], YEAR[3], MONTH[0], MONTH[1], MONTH[2],
        ];
        match self {
            // 月日与完整日期同一套变量（年份取当前年），故共用一份清单。
            Self::Date | Self::MonthDay => DATE_ALL,
            // 无 $D 系列、无农历：农历月与公历月不是一一对应，`2026.12` 推不出唯一农历月。
            Self::YearMonth => YEAR_MONTH_ALL,
            Self::Number => &[
                ("N", "原数字，1234.5"),
                ("THOU", "千分位，1,234.5"),
                ("CNL", "中文小写，一千二百三十四点五"),
                ("CNU", "中文大写，壹仟贰佰叁拾肆点伍"),
                ("DIG", "逐位读，一二三四点五"),
                ("AMT", "金额大写，壹仟贰佰叁拾肆元伍角（超两位小数为空）"),
            ],
            Self::Calc => &[
                ("EXPR", "算式原文，1+2*3"),
                ("RESULT", "计算结果，7"),
                (
                    "EXACT",
                    "结果原始精度（未按小数位截断），供 {pct()} 等函数二次换算，如 1/3 → 0.3333333333333333",
                ),
            ],
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
    // 计算：结果作首选，等式次之，百分比殿后（默认 ×100 保两位小数、去尾零）
    ("calc.result", FormatKind::Calc, "$RESULT"),
    ("calc.equation", FormatKind::Calc, "$EXPR=$RESULT"),
    ("calc.percent", FormatKind::Calc, "{pct()}"),
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
    /// 用户自己加的条目，排在基表条目**之后**（组内按 `position` 稳定排序）。
    ///
    /// 之后要挪位置走 `moved` 那套现成规则——出厂条目与用户条目共用同一个调序机制，
    /// 不给用户条目另开「它自己的顺序」。停用也一样（`disabled` 同时管两类）。
    ///
    /// ⚠️ 与 `moved`/`disabled` 不同，它是用户的**内容**而不是对出厂的**调整**：
    /// [`Self::is_empty`] 要算上它，「恢复默认」不得清它。
    pub added: Vec<FormatEntry>,
}

impl FormatAdjust {
    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.disabled.is_empty() && self.added.is_empty()
    }

    /// 这个 id 是不是用户自己加的条目。
    pub fn is_user(&self, id: &str) -> bool {
        self.added.iter().any(|e| e.id == id)
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
    /// 是不是用户自己加的条目（而非出厂表里的）。
    ///
    /// 设置页据此分流三件事，**别拿 `adjusted` 当它用**：出厂条目被调序过也是 `adjusted`，
    /// 但它不能删、能「恢复默认」；用户条目反过来——能删、能改模板，而它没有「默认」可回到。
    pub user: bool,
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
            // `{kind}.u{数字}` 是设置页里用户条目的保留形态。基表占用它就会与用户条目
            // 同 id，行为取决于遍历顺序——把约定变成加载期检查（见 `is_user_format_id`）。
            if is_user_format_id(&r.id) {
                warn!(
                    "快捷输入格式表: 跳过 id={} —— `.u数字` 结尾是设置页自定义条目的保留形态，请换个 id",
                    r.id
                );
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
    ///
    /// ## 用户自定义条目
    ///
    /// `adjust.added` 里的条目也在结果中（排在基表条目之后，再由 `moved` 决定最终位置），
    /// 故返回的引用可能指向**表**也可能指向**调整**——两个入参因此共用生命周期 `'a`。
    pub fn entries_of_adjusted<'a>(
        &'a self,
        kind: FormatKind,
        adjust: &'a FormatAdjust,
    ) -> Vec<&'a FormatEntry> {
        self.entries_with(kind, &adjust.moved, &adjust.disabled, &adjust.added)
    }

    /// [`Self::entries_of_adjusted`] 的实现，三段规则**分开收**。
    ///
    /// ★ 拆参数不是为了好看，是为了让调用方能做**部分借用**：[`Self::entries_of_view`]
    /// 需要「`moved` 与 `added` 照常生效、`disabled` 当作空」的一趟计算。若签名收整个
    /// `&'a FormatAdjust`，那就得现造一个临时的 `FormatAdjust`——而它是局部变量，
    /// 里面的 `added` 活不到 `'a`，返回的引用直接悬垂（编译不过，且没有不 clone 的绕法）。
    /// 收三个切片就能各自借自真正的 `adjust`，生命周期天然成立。
    fn entries_with<'a>(
        &'a self,
        kind: FormatKind,
        moved: &[(String, usize)],
        disabled: &[String],
        added: &'a [FormatEntry],
    ) -> Vec<&'a FormatEntry> {
        let alive = |e: &FormatEntry| !disabled.iter().any(|d| d == &e.id);
        let mut list: Vec<&FormatEntry> = self.entries_of(kind).filter(|e| alive(e)).collect();
        // 用户条目接在基表之后（组内按 position 稳定排序），于是「新加的排末尾」，
        // 而出厂日后新增的条目仍落在它的出厂位置——两类互不挤位。
        let mut mine: Vec<&FormatEntry> = added
            .iter()
            .filter(|e| e.kind == kind && alive(e))
            .collect();
        mine.sort_by_key(|e| e.position);
        list.extend(mine);
        for (id, position) in moved.iter().rev() {
            let Some(from) = list.iter().position(|e| &e.id == id) else {
                continue; // 孤儿规则：基表与用户条目里都没有这个 id
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
    /// 用户自定义条目（`adjust.added`）与出厂条目在这里**同等对待**：一样能停用、调序，
    /// 一样按上述规则保位。只有 [`FormatEntryView::user`] 标记出身，供设置页决定
    /// 「删除」还是「停用」、「恢复此条」能不能点。
    pub fn entries_of_view<'a>(
        &'a self,
        kind: FormatKind,
        adjust: &'a FormatAdjust,
    ) -> Vec<FormatEntryView<'a>> {
        let is_disabled = |id: &str| adjust.disabled.iter().any(|d| d == id);
        let mut out: Vec<FormatEntryView<'a>> = self
            .entries_of_adjusted(kind, adjust)
            .into_iter()
            .map(|entry| FormatEntryView {
                enabled: true,
                adjusted: adjust.has_rule(&entry.id),
                user: adjust.is_user(&entry.id),
                entry,
            })
            .collect();

        // 「假设全部启用」的顺序：停用项的位置从这里读，它们的 moved 规则在此生效
        // （见上方 doc）。`moved` 与 `added` 照常，只把 `disabled` 当作空——三段规则
        // 分开收正是为了这一趟（见 `entries_with`：造临时 FormatAdjust 会让 added 悬垂）。
        let as_if_all_enabled = self.entries_with(kind, &adjust.moved, &[], &adjust.added);

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
                        user: adjust.is_user(&entry.id),
                    },
                );
            }
        }
        out
    }

    /// 本类里是否已有逐字相同的模板（基表与用户条目一起查），返回撞上的那条 id。
    ///
    /// 两条完全一样的候选除了占位没有任何作用，还会让「候选里怎么有两个一样的」变成
    /// 一个查不出原因的报障（用户看不到 id，两行长得一模一样）。故新增/改写时拒绝并
    /// **指出撞的是哪一条**——只说「重复了」用户还得自己一行行找。
    ///
    /// 只比逐字相同：`$Y年` 与 `$YYYY年` 渲染结果可能一致，但那是两种不同写法，
    /// 判「等价」需要真渲染一遍，且结果随输入而变，不是稳定判据。
    pub fn duplicate_text_of(
        &self,
        kind: FormatKind,
        text: &str,
        adjust: &FormatAdjust,
        except: Option<&str>,
    ) -> Option<String> {
        let keep = |id: &str| except != Some(id);
        self.entries_of(kind)
            .chain(adjust.added.iter().filter(|e| e.kind == kind))
            .find(|e| e.text == text && keep(&e.id))
            .map(|e| e.id.clone())
    }
}

/// 用户条目 id 的保留形态：`{kind}.u{序号}`（如 `date.u1`）。
///
/// ★ 命名空间必须**有守门**，不能只靠约定：出厂表某天加一条 `date.u1`，就与用户条目
/// 静默撞车（同 id 两条内容，行为取决于遍历顺序）。故 [`FormatTable::parse`] 拒绝
/// 匹配本形态的出厂 id，把约定变成加载期检查。
pub fn is_user_format_id(id: &str) -> bool {
    match id.rsplit_once(".u") {
        Some((head, n)) => {
            !head.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// 下一个可用的用户条目 id。
///
/// ★ 取「已有最大序号 + 1」而不是「条数 + 1」：删掉中间一条后条数会回退，
/// 下一个新条目就撞上仍然存在的那个 id（`u1,u2,u3` 删 `u2` 后条数 2 ⇒ 又生成 `u3`）。
/// 症状是新加的条目「变成了」另一条的样子。
pub fn next_user_format_id(kind: FormatKind, adjust: &FormatAdjust) -> String {
    let prefix = format!("{}.u", kind.as_str());
    let max = adjust
        .added
        .iter()
        .filter(|e| e.kind == kind)
        .filter_map(|e| e.id.strip_prefix(&prefix))
        .filter_map(|n| n.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{prefix}{}", max + 1)
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
    // 用 trim 判空而不是 `is_empty`：纯空白模板会渲染出一条「看起来是空的」候选，
    // 选中它则上屏几个空格。设置页的输入框里这是最容易误提交的内容。
    if text.trim().is_empty() {
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
            added: Vec::new(),
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
                            added: Vec::new(),
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
                            added: Vec::new(),
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
            added: Vec::new(),
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
            added: Vec::new(),
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
        // 视图借用 adjust（用户条目住在里面），故不能传临时值
        let none = FormatAdjust::default();
        for &k in FormatKind::ALL {
            let view = t.entries_of_view(k, &none);
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
            added: Vec::new(),
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
            added: Vec::new(),
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
            added: Vec::new(),
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
            added: Vec::new(),
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

    // ───────── 用户自定义条目（P2）─────────

    fn user_entry(id: &str, kind: FormatKind, text: &str, position: i32) -> FormatEntry {
        FormatEntry {
            id: id.into(),
            kind,
            text: text.into(),
            position,
        }
    }

    /// 新加的条目落在本类**末尾**：出厂条目的位置不因为用户加了东西而变。
    #[test]
    fn user_entries_come_after_factory_ones() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            added: vec![
                user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0),
                user_entry("date.u2", FormatKind::Date, "$D.$M.$Y", 1),
            ],
            ..Default::default()
        };
        let ids: Vec<&str> = t
            .entries_of_adjusted(FormatKind::Date, &a)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        let base: Vec<&str> = t
            .entries_of(FormatKind::Date)
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(&ids[..base.len()], &base[..], "出厂条目原序不动");
        assert_eq!(&ids[base.len()..], &["date.u1", "date.u2"]);
    }

    /// 用户条目**只出现在它自己的类别**里——`added` 是一张跨类别的表，按 kind 过滤。
    #[test]
    fn user_entries_do_not_leak_across_kinds() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            added: vec![user_entry("number.u1", FormatKind::Number, "$N", 0)],
            ..Default::default()
        };
        for &k in FormatKind::ALL {
            let has = t
                .entries_of_adjusted(k, &a)
                .iter()
                .any(|e| e.id == "number.u1");
            assert_eq!(has, k == FormatKind::Number, "kind={}", k.as_str());
        }
    }

    /// ★ 用户条目与出厂条目共用同一套调序/停用机制，不另开一条路径。
    #[test]
    fn user_entry_obeys_move_and_disable() {
        let t = FormatTable::builtin();
        let added = vec![user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0)];

        let moved = FormatAdjust {
            moved: vec![("date.u1".into(), 0)],
            added: added.clone(),
            ..Default::default()
        };
        assert_eq!(
            t.entries_of_adjusted(FormatKind::Date, &moved)[0].id,
            "date.u1",
            "能移到首位"
        );

        let off = FormatAdjust {
            disabled: vec!["date.u1".into()],
            added: added.clone(),
            ..Default::default()
        };
        assert!(
            !t.entries_of_adjusted(FormatKind::Date, &off)
                .iter()
                .any(|e| e.id == "date.u1"),
            "停用后不出候选"
        );
    }

    /// ★★ 停用保位对用户条目同样成立：移到首位再停用，视图里仍在首位。
    ///
    /// 这条不是重复覆盖——`entries_of_view` 的「假设全部启用」那趟计算必须把 `added`
    /// 一起带上，漏了的话用户条目在那趟里根本不存在，锚点算法找不到它、只能垫到末尾。
    #[test]
    fn disabled_user_entry_keeps_its_moved_position() {
        let t = FormatTable::builtin();
        let added = vec![user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0)];
        let a = FormatAdjust {
            moved: vec![("date.u1".into(), 0)],
            disabled: vec!["date.u1".into()],
            added,
        };
        let view = t.entries_of_view(FormatKind::Date, &a);
        assert_eq!(view[0].entry.id, "date.u1", "★ 停用后仍在首位");
        assert!(!view[0].enabled);
        assert!(view[0].user);
    }

    #[test]
    fn view_marks_user_entries() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            added: vec![user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0)],
            ..Default::default()
        };
        let view = t.entries_of_view(FormatKind::Date, &a);
        for v in &view {
            assert_eq!(
                v.user,
                v.entry.id == "date.u1",
                "只有 date.u1 是用户条目，实际 id={}",
                v.entry.id
            );
        }
    }

    /// ★ id 生成取「最大序号 + 1」。反向对照：换成「条数 + 1」这条就红——
    /// 删掉中间一条后条数回退，新条目会撞上仍然存在的那个 id。
    #[test]
    fn next_user_id_survives_a_deletion_in_the_middle() {
        let mut a = FormatAdjust {
            added: vec![
                user_entry("date.u1", FormatKind::Date, "$Y", 0),
                user_entry("date.u2", FormatKind::Date, "$M", 1),
                user_entry("date.u3", FormatKind::Date, "$D", 2),
            ],
            ..Default::default()
        };
        assert_eq!(next_user_format_id(FormatKind::Date, &a), "date.u4");
        a.added.retain(|e| e.id != "date.u2"); // 删中间那条
        assert_eq!(
            next_user_format_id(FormatKind::Date, &a),
            "date.u4",
            "★ 不能回退成 date.u3——那个 id 还在用"
        );
    }

    /// 序号按类别各自计数（`added` 是跨类别的一张表）。
    #[test]
    fn next_user_id_counts_per_kind() {
        let a = FormatAdjust {
            added: vec![
                user_entry("date.u1", FormatKind::Date, "$Y", 0),
                user_entry("date.u2", FormatKind::Date, "$M", 1),
            ],
            ..Default::default()
        };
        assert_eq!(next_user_format_id(FormatKind::Date, &a), "date.u3");
        assert_eq!(next_user_format_id(FormatKind::Number, &a), "number.u1");
    }

    /// ★ 提示清单里的每个名字都必须真的被 `supports_var` 接受——否则设置页照着提示写出的
    /// 模板会在保存时被拒，而提示本身看不出错。
    #[test]
    fn var_hints_are_all_actually_supported() {
        for &k in FormatKind::ALL {
            for (name, desc) in k.var_hints() {
                assert!(
                    k.supports_var(name),
                    "kind={} 的提示里有不被支持的变量 ${name}",
                    k.as_str()
                );
                assert!(!desc.is_empty(), "${name} 缺说明");
            }
            assert!(
                !k.var_hints().is_empty(),
                "kind={} 没有任何提示",
                k.as_str()
            );
        }
    }

    /// 每类的变量条数写死。**加变量时这条会红**——那正是它的用途：提醒同步
    /// `var_hints`（设置页提示）与文档站的变量表，那两处没有编译期约束。
    #[test]
    fn var_hint_counts_are_pinned() {
        let n = |k: FormatKind| k.var_hints().len();
        assert_eq!(n(FormatKind::Date), 17, "公历 10 + 农历 7");
        assert_eq!(n(FormatKind::MonthDay), 17, "与 date 同一套");
        assert_eq!(n(FormatKind::YearMonth), 7, "无 $D 系列、无农历");
        assert_eq!(n(FormatKind::Number), 6);
        assert_eq!(n(FormatKind::Calc), 3);
    }

    /// 农历变量给 date/month_day 而**不给 year_month**：这条分工在提示清单里也要成立，
    /// 否则用户照提示给年月写 `$LMD`，保存时才被拒。
    #[test]
    fn year_month_hints_offer_no_lunar_vars() {
        let names: Vec<&str> = FormatKind::YearMonth
            .var_hints()
            .iter()
            .map(|(n, _)| *n)
            .collect();
        assert!(
            !names.iter().any(|n| crate::lunar::is_var(n)),
            "年月的提示里不该有农历变量: {names:?}"
        );
        assert!(
            FormatKind::Date
                .var_hints()
                .iter()
                .any(|(n, _)| crate::lunar::is_var(n)),
            "完整日期应当提供农历变量"
        );
    }

    #[test]
    fn user_id_shape_is_recognized() {
        assert!(is_user_format_id("date.u1"));
        assert!(is_user_format_id("month_day.u42"));
        assert!(!is_user_format_id("date.upper"), "u 后面不是数字");
        assert!(!is_user_format_id("date.cn"));
        assert!(!is_user_format_id("u1"), "没有 kind 前缀");
        assert!(!is_user_format_id("date.u"), "缺序号");
        assert!(!is_user_format_id(""));
    }

    /// ★ 加载期守门：出厂表/手写文件占用 `.u数字` 形态时跳过该条。
    /// 靠约定不够——真撞上了行为取决于遍历顺序，且完全没有报错。
    #[test]
    fn parse_rejects_reserved_user_id_shape() {
        let t = FormatTable::parse(
            r#"
[[formats]]
id = "date.u1"
kind = "date"
text = "$Y"

[[formats]]
id = "date.mine"
kind = "date"
text = "$M"
"#,
        )
        .unwrap();
        let ids: Vec<&str> = t.entries().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["date.mine"], "保留形态的那条被剔除");
    }

    #[test]
    fn duplicate_text_spots_factory_and_user_entries() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            added: vec![user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0)],
            ..Default::default()
        };
        assert_eq!(
            t.duplicate_text_of(FormatKind::Date, "$Y年$M月$D日", &a, None)
                .as_deref(),
            Some("date.cn"),
            "撞出厂条目"
        );
        assert_eq!(
            t.duplicate_text_of(FormatKind::Date, "$Y/$M/$D", &a, None)
                .as_deref(),
            Some("date.u1"),
            "撞用户条目"
        );
        assert!(
            t.duplicate_text_of(FormatKind::Date, "$YY年", &a, None)
                .is_none(),
            "没撞上"
        );
        // 同一串模板在别的类别里不算撞车（各类独立一张表）
        assert!(
            t.duplicate_text_of(FormatKind::Number, "$Y/$M/$D", &a, None)
                .is_none()
        );
    }

    /// 改写一条时要把自己排除掉，否则「只改了个错别字、模板没动」会被判成撞自己。
    #[test]
    fn duplicate_text_can_exclude_the_entry_being_edited() {
        let t = FormatTable::builtin();
        let a = FormatAdjust {
            added: vec![user_entry("date.u1", FormatKind::Date, "$Y/$M/$D", 0)],
            ..Default::default()
        };
        assert!(
            t.duplicate_text_of(FormatKind::Date, "$Y/$M/$D", &a, Some("date.u1"))
                .is_none()
        );
        assert_eq!(
            t.duplicate_text_of(FormatKind::Date, "$Y年$M月$D日", &a, Some("date.u1"))
                .as_deref(),
            Some("date.cn"),
            "排除自己不影响撞别人"
        );
    }
}
