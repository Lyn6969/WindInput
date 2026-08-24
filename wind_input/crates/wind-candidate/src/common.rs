//! 通用规范汉字表（常用字判定）
//!
//! 与 Go 版本 `wind_input/internal/dict/common_chars.go` 对齐。
//! 用于"检索范围"过滤：判定候选是否为常用字/词。

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 通用规范汉字表（出厂 8104 字：一级 3500 + 二级 3000 + 三级其余）+ **用户覆盖**。
///
/// 两层分开存是刻意的：
/// - `base` 来自 `common_chars.txt`（含用户目录整份覆盖），跟随出厂更新；
/// - `overrides` 是用户在候选右键 / 词库管理里一个个点出来的稀疏调整，落在 redb。
///
/// 合成只发生在查询那一刻（覆盖优先），**不预先 merge 成一个集合**——merge 之后就分不清
/// 「这个字出厂就常用」还是「用户把它设成了常用」，而界面要显示的正是这个差别
/// （「出厂：生僻 → 现在：常用」），「恢复出厂」也需要知道回退到哪一边。
pub struct CommonChars {
    /// 出厂基表（判定用，O(1) 查表）。
    base: HashSet<char>,
    /// 出厂基表的**原始顺序**（列举用）。
    ///
    /// 单独留一份 Vec 而不是从 `base` 迭代：`common_chars.txt` 是按级别顺序拼接的
    /// （一级 3500 → 二级 3000 → 三级其余），而 HashSet 的迭代序是随机的。设置页要按
    /// 字表原序列出全表，从 HashSet 取会得到一份每次启动都不一样的乱序清单——分页更是
    /// 直接失效（第 2 页的内容会随机变）。8104 个 char 约 32KB，代价可以忽略。
    base_order: Vec<char>,
    /// 用户覆盖：`true` = 强制判为常用，`false` = 强制判为生僻。只含被碰过的字。
    overrides: HashMap<char, bool>,
}

impl CommonChars {
    /// 从文件加载（一字一行，`#` 注释行跳过；仅收录汉字，见 [`is_han`]）。
    /// 失败（文件缺失）返回空集；上层应在空集时退化为"不过滤"。
    pub fn load(path: &Path) -> Self {
        let mut base = HashSet::new();
        let mut base_order = Vec::new();
        if let Ok(content) = std::fs::read_to_string(path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                for ch in line.chars() {
                    // `insert` 返回 false = 重复字，不再追加进顺序表：字表里若有重复
                    // （历史数据难免），列表会出现两行一模一样的字。
                    if is_han(ch) && base.insert(ch) {
                        base_order.push(ch);
                    }
                }
            }
        }
        Self {
            base,
            base_order,
            overrides: HashMap::new(),
        }
    }

    /// 由一批字直接构造基表（测试与内存态装配用）。
    pub fn from_base(chars: impl IntoIterator<Item = char>) -> Self {
        let mut base = HashSet::new();
        let mut base_order = Vec::new();
        for ch in chars {
            if base.insert(ch) {
                base_order.push(ch);
            }
        }
        Self {
            base,
            base_order,
            overrides: HashMap::new(),
        }
    }

    /// 全表列举：出厂字（按字表原序）+ 用户加的、**不在出厂表里**的字（追加在后）。
    ///
    /// 返回 `(字, 出厂是否常用, 现在是否常用)`。设置页靠它列全表并搜索——只列「改过的」
    /// 那几条不足以回答用户最常问的那个问题：「这个字现在算不算常用」。
    ///
    /// 追加的那批（用户把某个不在规范表里的字设成了常用）出厂判定恒 false，
    /// 语义就是「出厂没有它」。
    pub fn list_all(&self) -> Vec<(char, bool, bool)> {
        let mut out: Vec<(char, bool, bool)> = self
            .base_order
            .iter()
            .map(|&c| (c, true, self.is_char_common(c)))
            .collect();
        // 覆盖里那些不在出厂表的字：按码位排序后追加，保证顺序稳定（HashMap 迭代序随机，
        // 直接追加会让这批字每次刷新都换位置）。
        let mut extra: Vec<char> = self
            .overrides
            .keys()
            .copied()
            .filter(|c| !self.base.contains(c))
            .collect();
        extra.sort_unstable();
        out.extend(
            extra
                .into_iter()
                .map(|c| (c, false, self.is_char_common(c))),
        );
        out
    }

    /// **整体替换**用户覆盖（调用方从 store 全量读出后灌进来）。
    ///
    /// 刻意不做增量合并：增量语义下「撤销某字的覆盖」这个操作没有落点——被删掉的那条
    /// 在旧集合里仍然存在，用户会看到「点了恢复出厂、重启前毫无变化」。
    pub fn set_overrides(&mut self, it: impl IntoIterator<Item = (char, bool)>) {
        self.overrides = it.into_iter().collect();
    }

    /// 是否未加载到任何**出厂**字（数据缺失）。
    ///
    /// ⚠️ 只看 `base`，不看覆盖：上层拿它决定「退化为不过滤」。若用户覆盖也算数，那么
    /// 出厂表缺失、而用户恰好设过一两个字时本函数返回 false，过滤照常进行——此刻几乎
    /// 所有字都查不到表、全被判成生僻，智能档会把候选滤得只剩那一两个字。
    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    /// 单字是否常用：**用户覆盖优先**，其次查出厂基表。
    /// 两边都没有的字符（含 PUA）一律非常用。
    pub fn is_char_common(&self, ch: char) -> bool {
        match self.overrides.get(&ch) {
            Some(&v) => v,
            None => self.base.contains(&ch),
        }
    }

    /// 出厂判定（忽略用户覆盖）。供界面显示「出厂：常用 → 现在：生僻」这类对照，
    /// 以及判断某条覆盖是否与出厂同向（同向的覆盖是冗余的，可以提示清理）。
    pub fn is_base_common(&self, ch: char) -> bool {
        self.base.contains(&ch)
    }

    /// 某字的用户覆盖方向；`None` = 未覆盖。
    pub fn override_of(&self, ch: char) -> Option<bool> {
        self.overrides.get(&ch).copied()
    }

    /// 字符串是否常用：其中所有「汉字」都在表内，非汉字辅助字符（标点/字母/数字/
    /// emoji/符号）忽略。空串视为非常用。
    ///
    /// 「汉字」的作用域 = [`is_han`] ∪ [`is_pua`]，两侧各自解决一类误判：
    /// - **纳入 PUA**：本码表把私用区码位**当汉字使用**（如 `dwi` 下 U+E831 冒充生僻字、
    ///   占着汉字编码排在「仄」旁边），不查表就会让无字形的垃圾候选混进「常用字/智能」档；
    /// - **`is_han` 排除 CJK 标点/符号**：`、。《》` 等虽紧邻汉字块，却与 `，`、emoji 同属
    ///   辅助符号，规范汉字表对其无从判断，查表必然失败 → 含中文顿号的词条被静默滤掉。
    ///
    /// 两者是同一个判据的两端：**「码表拿它当汉字用」才查表，「它只是符号」就忽略**，
    /// 与 Unicode 块的相邻关系无关。
    pub fn is_string_common(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        for ch in text.chars() {
            // 汉字（含被本码表当汉字用的 PUA）必须判为常用；其余辅助字符忽略。
            if is_common_scope(ch) && !self.is_char_common(ch) {
                return false;
            }
        }
        true
    }
}

/// 常用字表**管辖**哪些字符 = [`is_han`] ∪ [`is_pua`]。
///
/// 这是 [`CommonChars::is_string_common`] 里那道判据的公开形态，供上层做**准入**用：
/// 候选右键的「设为生僻字 / 设为常用字」只对本域内的字符成立。
///
/// ★ 两处必须同源。写端放行了本域之外的字符（比如给 `、` 或 emoji 存一条覆盖），
/// 读端会照旧忽略它 —— 表现是「设了，存进去了，毫无作用」，且完全静默：
/// 数据库里躺着一条用户以为生效的记录，没有任何一层会报错。
pub fn is_common_scope(ch: char) -> bool {
    is_han(ch) || is_pua(ch)
}

/// 是否「须按通用规范汉字表判定常用性」的汉字。
///
/// 判定域是**真汉字块**，外加无独立输入语义的类汉字符号（部首、笔画）——后者与 PUA 同理，
/// 在码表里占着汉字编码出现，不在规范字表内即应判非常用。
///
/// **刻意排除**（虽紧邻汉字块但属辅助符号，规范汉字表对其无从判断）：
/// - `U+3000..=U+303F` CJK 符号和标点：`、。《》〈〉「」〇` 等；
/// - `U+3040..=U+30FF` 假名、`U+3100..=U+318F` 注音/谚文、`U+3190..=U+319F` 汉文标注；
/// - `U+3200..=U+33FF` 带圈与兼容符号：`① ㈱ ℃ ㎡` 等。
///
/// 旧实现按整段 `0x2E80..=0x33FF` 圈定（名为 `is_cjk`，对齐 Go `isCJKChar`），把上述符号
/// 当成「必须查表的汉字」，而字表里只有 8105 个纯汉字 → 中文顿号一律判非常用，用户词库中
/// 含 `、` 的词条在「常用字/智能」档被静默滤掉。**指纹是判定不自洽**：同为中文标点，
/// `、`(U+3001) 判非常用、`，`(U+FF0C) 却判常用，差别只在落没落进那段区间。
fn is_han(ch: char) -> bool {
    let c = ch as u32;
    (0x2E80..=0x2EFF).contains(&c)        // CJK 部首补充
        || (0x2F00..=0x2FDF).contains(&c) // 康熙部首
        || (0x31C0..=0x31EF).contains(&c) // CJK 笔画
        || (0x3400..=0x4DBF).contains(&c) // 扩展 A
        || (0x4E00..=0x9FFF).contains(&c) // 基本汉字
        || (0xF900..=0xFAFF).contains(&c) // 兼容汉字
        || (0x20000..=0x323AF).contains(&c) // 扩展 B-H
}

/// 候选文本的**语义单元数**：汉字逐字计，连续的西文/数字段整体计 1。
///
/// ```text
/// 「东西」    → 2      hello        → 1
/// 「的样子」  → 3      thank you    → 2
/// 「iPhone」  → 1      「新iPhone」 → 2
/// ```
///
/// 用途：词频位置提升的准入判据（`schema.*.frequency.promote_prefix = "single"`）。
///
/// **为什么不能用 `chars().count()`**：英文候选 `hello` 有 5 个 char，按字符数判据会被
/// 「只提升单字」的规则直接挡死——而英文**所有候选都是前缀匹配**（打 `hel` 出 `hello`），
/// 那等于英文调频全灭。语义单元数让同一条规则在中英文下都成立：一个汉字、一个西文词，
/// 都是用户心智里的「一个东西」。
///
/// PUA 按汉字计——本码表把私用区码位当生僻字使用，见 [`is_pua`]。
pub fn semantic_units(text: &str) -> usize {
    let mut units = 0usize;
    let mut in_latin_word = false;
    for ch in text.chars() {
        if is_han(ch) || is_pua(ch) {
            units += 1;
            in_latin_word = false;
        } else if ch.is_whitespace() {
            in_latin_word = false;
        } else if !in_latin_word {
            units += 1;
            in_latin_word = true;
        }
    }
    units
}

/// 是否 Unicode 私用区（PUA）。本码表把 PUA 码位当汉字使用（占汉字编码、冒充生僻字），
/// 故常用性判定须把 PUA 视作「必须查表的汉字」，不在规范字表内即判非常用。
fn is_pua(ch: char) -> bool {
    let c = ch as u32;
    (0xE000..=0xF8FF).contains(&c)          // BMP 私用区
        || (0xF0000..=0xFFFFD).contains(&c) // 补充私用区 A
        || (0x100000..=0x10FFFD).contains(&c) // 补充私用区 B
}

#[cfg(test)]
mod semantic_units_tests {
    use super::semantic_units;

    /// 中英混排的计数口径——词频 `promote_prefix = "single"` 全靠它区分「单字/单词」与「词组」。
    #[test]
    fn counts_han_per_char_and_latin_per_word() {
        // 中文逐字
        assert_eq!(semantic_units("东"), 1);
        assert_eq!(semantic_units("东西"), 2);
        assert_eq!(semantic_units("的样子"), 3);
        // 西文整词计 1 —— 这是英文调频能工作的前提
        assert_eq!(semantic_units("hello"), 1);
        assert_eq!(semantic_units("a"), 1);
        assert_eq!(semantic_units("thank you"), 2);
        assert_eq!(semantic_units("e-mail"), 1, "连字符不断词");
        // 混排
        assert_eq!(semantic_units("新iPhone"), 2);
        assert_eq!(semantic_units("iPhone手机"), 3);
        // 边界
        assert_eq!(semantic_units(""), 0);
        assert_eq!(semantic_units("   "), 0);
        // 数字按一段计
        assert_eq!(semantic_units("2026"), 1);
    }

    /// 扩展区汉字与 PUA 均按汉字逐字计——码表把私用区码位当生僻字使用。
    #[test]
    fn counts_ext_han_and_pua_as_chars() {
        assert_eq!(semantic_units("\u{20000}"), 1, "扩展 B");
        assert_eq!(semantic_units("\u{E000}\u{E001}"), 2, "PUA 逐字计");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_common() {
        let cc = CommonChars::from_base(['我', '们']);
        assert!(cc.is_string_common("我们")); // 全部常用
        assert!(!cc.is_string_common("我鬱")); // 含生僻
        assert!(!cc.is_string_common("")); // 空串
        assert!(cc.is_string_common("我!")); // 非汉字忽略
    }

    #[test]
    fn test_cjk_punct_ignored() {
        // 回归：用户词库里含中文顿号的词条在「常用字/智能」档被滤掉。
        // 根因＝判定域按 `0x2E80..=0x33FF` 整段圈定，把 CJK 符号和标点区当成必须查表的汉字，
        // 而字表里只有纯汉字。判据现按语义而非 Unicode 块邻接：符号一律忽略。
        let cc = CommonChars::from_base(['我', '们']);

        assert!(cc.is_string_common("、")); // 顿号单条词条（本次上报的现象）
        assert!(cc.is_string_common("我、们")); // 混排：标点不再拖累整词判定
        for s in ["。", "《", "》", "「", "」", "〇", "；", "："] {
            assert!(cc.is_string_common(s), "CJK 标点应忽略: {s}");
        }
        // 判定自洽：同为中文标点，落不落进旧区间都该同一结果（旧实现下 、=false 而 ，=true）
        assert_eq!(cc.is_string_common("、"), cc.is_string_common("，"));
        // 带圈/兼容符号与假名同属辅助字符，规范汉字表管不着
        assert!(cc.is_string_common("①"));
        assert!(cc.is_string_common("℃"));
        assert!(cc.is_string_common("あ"));
        // 真汉字仍按表判定，未被本次放宽波及
        assert!(!cc.is_string_common("我鬱"));
        assert!(!cc.is_string_common("、鬱")); // 标点忽略，但同串里的生僻字照旧拦下
    }

    #[test]
    fn test_pua_not_common() {
        // 回归：五笔 dwi 下 U+E831（PUA）冒充生僻字混进常用字档。PUA 被本码表当汉字用，
        // 不在规范字表内即非常用；emoji/符号等真辅助字符仍忽略。
        let cc = CommonChars::from_base(['仄']);
        assert!(cc.is_string_common("仄")); // 真汉字在表内
        assert!(!cc.is_string_common("\u{E831}")); // PUA 单字：非常用（正是 dwi 豆腐候选）
        assert!(!cc.is_string_common("仄\u{E831}")); // 含 PUA 的混合串亦非常用
        assert!(cc.is_string_common("仄😀")); // emoji（U+1F600）非汉字：忽略，不影响判定
    }

    /// 用户覆盖两个方向都要生效，且要能穿透到整串判定。
    #[test]
    fn overrides_win_over_base_in_both_directions() {
        let mut cc = CommonChars::from_base(['我', '们']);
        assert!(cc.is_char_common('我'));
        assert!(!cc.is_char_common('鬱'));

        cc.set_overrides([('我', false), ('鬱', true)]);
        assert!(!cc.is_char_common('我'), "常用字降级为生僻");
        assert!(cc.is_char_common('鬱'), "生僻字升级为常用");
        // 整串判定走同一条路：降级后的字会拖累整个词。
        assert!(!cc.is_string_common("我们"));
        assert!(cc.is_string_common("鬱"));
        // 出厂判定不受覆盖影响——界面要靠它显示「出厂 → 现在」的对照。
        assert!(cc.is_base_common('我'));
        assert!(!cc.is_base_common('鬱'));
        assert_eq!(cc.override_of('我'), Some(false));
        assert_eq!(cc.override_of('们'), None);
    }

    /// `set_overrides` 是整体替换：撤销掉的那条必须真的消失。
    ///
    /// 若实现成增量合并，「恢复出厂」在下次重灌前不会生效——症状是点了没反应，
    /// 重启后才对，属于最难查的一类（[[project_runtime_mirror_state_config_sync]]）。
    #[test]
    fn set_overrides_replaces_rather_than_merges() {
        let mut cc = CommonChars::from_base(['我']);
        cc.set_overrides([('我', false), ('鬱', true)]);
        assert!(!cc.is_char_common('我'));

        // 用户撤销了「我」那条，store 全量读出来只剩「鬱」。
        cc.set_overrides([('鬱', true)]);
        assert!(cc.is_char_common('我'), "撤销后回到出厂判定");
        assert!(cc.is_char_common('鬱'));
    }

    /// ⚠️ `is_empty` 只看出厂基表：它是上层「退化为不过滤」的判据。
    ///
    /// 把覆盖也算进去的话，出厂表缺失而用户设过一个字时它会返回 false，于是过滤照常
    /// 进行——此刻几乎所有字都不在表里、全被判生僻，智能档会把候选滤到只剩那一个字。
    #[test]
    fn is_empty_ignores_overrides() {
        let mut cc = CommonChars::from_base([]);
        assert!(cc.is_empty());
        cc.set_overrides([('鬱', true)]);
        assert!(cc.is_empty(), "有覆盖也仍算数据缺失");
    }

    /// 判定域自洽：`is_common_scope` 放行的字符，正是 `is_string_common` 会去查表的那些。
    ///
    /// 上层拿 `is_common_scope` 做右键菜单的准入。两处一旦漂移，用户就能给一个读端
    /// 根本不查的字符（`、`、emoji）存下覆盖，然后发现「设了完全没用」且毫无报错。
    #[test]
    fn common_scope_matches_string_judgement() {
        let cc = CommonChars::from_base([]);
        for ch in ['我', '鬱', '\u{E831}', '\u{20000}', '氵'] {
            assert!(is_common_scope(ch), "{ch} 应在管辖域内");
            // 域内且不在表里 ⇒ 整串判非常用。
            assert!(!cc.is_string_common(&ch.to_string()), "{ch} 应判非常用");
        }
        for ch in ['、', '，', '①', '℃', 'あ', '😀', 'A', '7'] {
            assert!(!is_common_scope(ch), "{ch} 应在管辖域外");
            // 域外 ⇒ 被忽略，不拖累判定。
            assert!(cc.is_string_common(&ch.to_string()), "{ch} 应被忽略");
        }
    }

    /// 全表列举：**按字表原序**，用户加的字追加在后。
    ///
    /// 顺序是硬要求：`common_chars.txt` 按级别拼接（一级→二级→三级），设置页分页浏览
    /// 全靠它。若从 `HashSet` 迭代，每次启动的顺序都不一样，第 2 页的内容会随机变。
    #[test]
    fn list_all_keeps_table_order_and_appends_extras() {
        let mut cc = CommonChars::from_base(['一', '乙', '二']);
        assert_eq!(
            cc.list_all(),
            vec![('一', true, true), ('乙', true, true), ('二', true, true)],
            "无覆盖时＝字表原序，且默认与现在一致"
        );

        // 一个降级（在表内）+ 一个新增（不在表内）。
        cc.set_overrides([('乙', false), ('槮', true)]);
        assert_eq!(
            cc.list_all(),
            vec![
                ('一', true, true),
                // 默认仍是常用，现在被改成生僻——两个值都要保留，界面靠差异显示对照。
                ('乙', true, false),
                ('二', true, true),
                // 表外字追加在最后，默认判定恒 false（＝出厂表里没有它）。
                ('槮', false, true),
            ]
        );
    }

    /// 表外字按码位排序追加：`HashMap` 迭代序随机，直接追加会让这批字每次刷新都换位置。
    #[test]
    fn list_all_orders_extras_deterministically() {
        let mut cc = CommonChars::from_base(['一']);
        cc.set_overrides([('鬱', true), ('槮', true), ('乂', true)]);
        let extras: Vec<char> = cc
            .list_all()
            .into_iter()
            .skip(1)
            .map(|(c, _, _)| c)
            .collect();
        let mut sorted = extras.clone();
        sorted.sort_unstable();
        assert_eq!(extras, sorted, "表外字必须按码位升序，顺序才稳定");
    }

    /// 字表里的重复字只列一次——历史数据里难免有重复，列两行一模一样的字很怪。
    #[test]
    fn list_all_dedupes_repeated_chars() {
        let cc = CommonChars::from_base(['一', '乙', '一']);
        assert_eq!(cc.list_all().len(), 2);
    }

    /// 域外字符即使被强行写入覆盖也不生效——这正是准入判据必须与读端同源的原因。
    #[test]
    fn overrides_on_out_of_scope_chars_are_inert() {
        let mut cc = CommonChars::from_base(['我']);
        cc.set_overrides([('、', false), ('😀', false)]);
        // 读端对域外字符直接跳过，覆盖形同虚设。
        assert!(cc.is_string_common("、"));
        assert!(cc.is_string_common("我😀"));
    }
}
