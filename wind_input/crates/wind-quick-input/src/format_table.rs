//! 快捷输入格式表：候选「渲染成什么样、按什么顺序」的外置配置。
//!
//! 设计与约束见 `docs/design/quick-input-format-table.md`。三条要点：
//!
//! - **解析归代码、渲染归配置**：`kind` 是白名单（对应四个已有解析器），配置不能新增；
//! - **组内顺序归 `position`，跨来源顺序仍归 `mix_modes.members`**（不设第二真相源）；
//! - **坏掉也得能打字**：文件缺失/整份解析失败一律回落 [`FormatTable::builtin`]，
//!   单条非法只剔除该条。

use std::path::Path;
use tracing::warn;

/// 解析器类别。恰好对应四个已有解析器，**不可由配置新增**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatKind {
    /// 完整日期 `12.25` / `2025.12.25`
    Date,
    /// 年月 `2025.12`
    YearMonth,
    /// 数字 / 金额（纯数字，或算式求值结果）
    Number,
    /// 算式求值
    Calc,
}

impl FormatKind {
    /// 配置文件里的写法 → 类别。也用于候选 id（`quick:{kind}:{格式 id}`）的反解析，
    /// 故与 [`Self::as_str`] 必须严格互逆。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "date" => Some(Self::Date),
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
            Self::YearMonth => 1,
            Self::Number => 2,
            Self::Calc => 3,
        }
    }

    /// 配置文件里的写法（错误信息与测试用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::YearMonth => "year_month",
            Self::Number => "number",
            Self::Calc => "calc",
        }
    }

    /// 本类支持的变量名（校验用；取值实现见 `crate::vars`）。
    ///
    /// ⚠️ 农历变量**只给 `Date`，不给 `YearMonth`**：农历月与公历月不是一一对应
    /// （闰月、且月首不在公历月初），`2026.12` 根本推不出唯一的农历月。
    /// 放行了只会让用户拿到一个看似合理的错值。
    pub fn supports_var(self, name: &str) -> bool {
        let common_year = matches!(name, "Y" | "YYYY" | "YY" | "YC");
        match self {
            Self::Date => {
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
                    "快捷输入格式表: 跳过 id={} —— 未知 kind {:?}（可用: date/year_month/number/calc）",
                    r.id, r.kind
                );
                continue;
            };
            if let Err(e) = validate_text(kind, &r.text) {
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
}

/// 模板静态校验：变量名必须在本 kind 的白名单内。
///
/// 在**加载期**做而不是渲染期：渲染是每次按键都走的热路径，把告警放那里会刷屏，
/// 且用户改错一个变量名应当在启动日志里一次性看到。
fn validate_text(kind: FormatKind, text: &str) -> Result<(), String> {
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
        for k in [
            FormatKind::Date,
            FormatKind::YearMonth,
            FormatKind::Number,
            FormatKind::Calc,
        ] {
            assert_eq!(FormatKind::parse(k.as_str()), Some(k));
        }
        assert!(FormatKind::parse("weather").is_none());
    }

    #[test]
    fn builtin_covers_all_four_kinds() {
        let t = FormatTable::builtin();
        for k in [
            FormatKind::Date,
            FormatKind::YearMonth,
            FormatKind::Number,
            FormatKind::Calc,
        ] {
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
                validate_text(e.kind, &e.text).is_ok(),
                "内置条目 {} 校验失败: {:?}",
                e.id,
                validate_text(e.kind, &e.text)
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

        assert!(validate_text(FormatKind::Date, "${Y}年{month()}月").is_err());
        assert!(validate_text(FormatKind::Date, "$Y年{month()}月").is_err());
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
