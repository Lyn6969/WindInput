//! 辅助码表：字 → 辅助码列表的紧凑映射
//!
//! 存储布局与 `wind-reverse::PinyinTable`（字→读音列表）完全同构，三段式布局：
//!
//! ```text
//! entries: Vec<Entry{ ch, codes_end }>   按字升序
//!   ├─ 每条对应一个唯一汉字；codes_end 指向该字的所有码在 code_ends 中的
//!   │  结束下标。下标区间 = [前条 codes_end .. 本条 codes_end)
//!   │  （首条起点为 0）
//!
//! code_ends: Vec<u32>                   每码在 arena 中的结束偏移
//!   ├─ 单个辅助码的文本区间 = [code_ends[i-1] .. code_ends[i])
//!   │  （首项起点为 0）
//!   │  【码长任意】笔画/拆字等方案码长不固定，字节区间长度 = 该码的 UTF-8 字节数
//!
//! arena: String                            所有辅助码字符串按序连续拼接
//!   └─ 例：厑 有三码 u、ab、wxyz → arena 存 "uabwxyz"，code_ends = [1, 3, 7]
//! ```
//!
//! ## 单表语义
//! 同字多码**严格保留行序（文件中出现的先后），做 first-seen 稳定去重、不做排序**：
//! - 表内重复定义的相同码只保留首次出现的位置（节省 arena 空间）
//! - 匹配判断用 early-return，1~3 个码的循环差异在纳秒级，完全不影响性能
//! - 免费保留「表内行级优先级」，为未来扩展打底
//!
//! ## 多表挂载（用户同时启用拆分 + 小鹤 + …）
//! 不再单独维护「多张独立表的 Vec」结构——因为跨表合并后输出的是**已去重的
//! 混合结果**，保留"独立表外壳"只会让结构和语义脱节。
//! 正确做法：挂载时通过 [`AuxCodeTable::merge`] / [`AuxCodeTable::append`] 一次性
//! **flatten 成一张合并表**，让「表级优先级 × 行级优先级 × 跨表 first-seen 去重」
//! 全部坍缩到三段式布局的物理序中。查询阶段零额外开销（无需 flat_map / HashSet）。
//! 挂载顺序就是 merge/append 的迭代顺序。
//!
//! 文本 / 文件加载（txt 格式 → 表）见 [`crate::loader`]；过滤语义见 [`crate::filter`]。

/// 字级条目：单个汉字在 entries 数组中的登记
#[derive(Debug, Clone, Copy)]
struct CharEntry {
    /// 该条目的汉字（entries 按此字段升序排列，供二分查找定位）
    ch: char,
    /// 该字的码序列在 `code_ends` 中的结束下标（下标区间 = 前条 codes_end .. 本条 codes_end）
    codes_end: u32,
}

/// 辅助码表：字 → 辅助码列表（三段式紧凑存储；单表 / 合并多表通用）
#[derive(Default, Debug)]
pub struct AuxCodeTable {
    /// 辅助码方法名（如「笔画」）；空 = 未命名，调用方回落文件主干名。
    pub name: String,
    /// 按 `ch` 升序排列的字级条目
    entries: Vec<CharEntry>,
    /// 每一个辅助码在 arena 中的结束偏移，按「字序 + 字内行序」连续排列
    code_ends: Vec<u32>,
    /// 所有辅助码字符串的共享拼接区
    arena: String,
}

/// 某字的辅助码列表视图：按需从 arena 切片，取用不额外分配
#[derive(Clone, Copy)]
struct CodeListView<'a> {
    table: &'a AuxCodeTable,
    /// code_ends 下标区间 [start, end)
    start: usize,
    end: usize,
}

impl<'a> CodeListView<'a> {
    /// 最高优先级辅助码（第一个）；仅测试用（`first_code`）。
    #[cfg(test)]
    fn first(&self) -> Option<&'a str> {
        (self.start < self.end).then(|| self.table.code_at(self.start))
    }

    /// 按优先级遍历所有辅助码
    fn iter(self) -> impl Iterator<Item = &'a str> {
        (self.start..self.end).map(move |i| self.table.code_at(i))
    }
}

impl AuxCodeTable {
    // ------------------------------------------------------------------
    // 构造：空表 / 单表 rows / 多表合并 / 增量追加
    // ------------------------------------------------------------------

    /// 构造空码表（未挂载任何辅助码 → 过滤时原样放行，见 [`crate::filter::filter_by_aux_code`]）
    pub fn new() -> Self {
        Self::default()
    }

    /// 从 (字, 辅助码) 行构建**单张**码表。
    ///
    /// 语义：
    /// - 同字多行**按序保留 first-seen 去重**：同字的同一码只留首次出现的行（节省空间），
    ///   不同码按原序依次入列——文件中出现的先后即行级优先级
    /// - 空辅助码的行跳过（空码 = 没码，占一条记录毫无意义）
    /// - 字去重只发生在 entries 层：多个码归并到同一个字的 code_ends 区间内
    pub fn from_rows(rows: Vec<(char, &str)>) -> Self {
        // 1. 过滤空码 + 稳定排序（保留行内顺序，同字条连续出现，code_ends 自然按序入列）
        let mut rows = rows
            .into_iter()
            .filter(|(_, code)| !code.is_empty())
            .collect::<Vec<_>>();
        rows.sort_by_key(|(ch, _)| *ch);

        let mut entries = Vec::with_capacity(rows.len());
        let mut code_ends = Vec::with_capacity(rows.len());
        let mut arena = String::with_capacity(rows.iter().map(|(_, c)| c.len()).sum());

        // 2. 相邻同字的多行归并到同一个 entry，code_ends 逐条推入
        //    同字范围内对新码做 first-seen 线性比对：相同码已存在则跳过
        let mut i = 0;
        while i < rows.len() {
            let (cur_ch, _) = rows[i];
            let codes_start_for_char = code_ends.len();
            let mut j = i;
            while j < rows.len() && rows[j].0 == cur_ch {
                let (_, code) = rows[j];
                // First-seen 去重：linear scan 该字已生成的码范围（通常 1~3 个，代价可忽略）
                let mut seen = false;
                for k in codes_start_for_char..code_ends.len() {
                    if Self::code_at_from_parts(&arena, &code_ends, k) == code {
                        seen = true;
                        break;
                    }
                }
                if !seen {
                    arena.push_str(code);
                    code_ends.push(arena.len() as u32);
                }
                j += 1;
            }
            entries.push(CharEntry {
                ch: cur_ch,
                codes_end: code_ends.len() as u32,
            });
            i = j;
        }

        entries.shrink_to_fit();
        code_ends.shrink_to_fit();
        arena.shrink_to_fit();

        tracing::debug!(
            "辅助码表构建完成：{} 字，{} 码条目，arena {} 字节",
            entries.len(),
            code_ends.len(),
            arena.len()
        );

        Self {
            name: String::new(),
            entries,
            code_ends,
            arena,
        }
    }

    /// 设置名称（元数据；空表加载时由 loader 回落文件主干名）。
    pub fn with_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// **合并多张码表**：按迭代顺序 = 挂载优先级序（先出现 = 高优）。
    ///
    /// 一次调用解决所有跨表问题：
    /// - **跨表优先级序**：高优表所有 (字,码) 先入列，低优表后入列。
    ///   交由 `from_rows` 的稳定排序 + first-seen 去重，同字时保留入参先后。
    /// - **跨表同码去重**：高优表的码首次出现，低优表重复项自动丢弃。
    /// - **空表自动跳过**：不会污染合并流程（from_rows 内部对空码行也会再过滤一次）。
    ///
    /// 调用场景：用户调整码表方案（增删 / 改顺序）时，一次性 reify 所有选中项。
    pub fn merge<I>(tables: I) -> Self
    where
        I: IntoIterator<Item = AuxCodeTable>,
    {
        // Per-table codes 是 &str（指向各自表的 arena），必须先 owned 化，
        // 才能把 &'_ str 统一交给同一个 from_rows。这是低频的设置路径，String 分配可接受。
        let mut owned: Vec<(char, String)> = Vec::new();
        let mut name = String::new();
        for t in tables {
            if name.is_empty() {
                name = t.name.clone();
            }
            for (ch, code) in t.all_entries_codes() {
                owned.push((ch, code.to_string()));
            }
        }
        let borrowed: Vec<(char, &str)> = owned
            .iter()
            .map(|(ch, code)| (*ch, code.as_str()))
            .collect();
        Self::from_rows(borrowed).with_name(name)
    }

    /// 增量追加：把 `other` 表作为**最低优先级**挂到现有表的末尾。
    ///
    /// 实现简化 MVP：当前内容 self + other 一并交给 `merge` 重新构建。
    /// 实际主流用法是 `merge` 一次性合并所有表（用户在设置面板一次性点确定）；
    /// 本函数仅用于「设置面板里逐张勾选」的交互。
    pub fn append(&mut self, other: AuxCodeTable) {
        if other.is_empty() {
            tracing::debug!("跳过空的辅助码表（未收录任何字）");
            return;
        }
        let prev = std::mem::take(self);
        *self = Self::merge(vec![prev, other]);
    }

    // ------------------------------------------------------------------
    // 内部工具：区间切片取码
    // ------------------------------------------------------------------

    /// 按 code_ends 的全局下标取单个辅助码文本
    fn code_at(&self, idx: usize) -> &str {
        Self::code_at_from_parts(&self.arena, &self.code_ends, idx)
    }

    /// 静态版：在 `from_rows` 构建期间（只有 arena + code_ends 切片时）取指定码文本，
    /// 避免在去重循环里重复写区间计算逻辑
    fn code_at_from_parts<'a>(arena: &'a str, code_ends: &'a [u32], idx: usize) -> &'a str {
        let start = if idx == 0 {
            0
        } else {
            code_ends[idx - 1] as usize
        };
        &arena[start..code_ends[idx] as usize]
    }

    // ------------------------------------------------------------------
    // 查询接口：单字码列表 / 首选码 / 前缀匹配判据
    // ------------------------------------------------------------------

    /// 二分查找某字的辅助码列表视图；未收录返回 None
    fn view_codes(&self, ch: char) -> Option<CodeListView<'_>> {
        let i = self.entries.binary_search_by_key(&ch, |e| e.ch).ok()?;
        let start = if i == 0 {
            0
        } else {
            self.entries[i - 1].codes_end as usize
        };
        Some(CodeListView {
            table: self,
            start,
            end: self.entries[i].codes_end as usize,
        })
    }

    /// 某字的所有辅助码迭代器（按「行级优先级 / 合并时的全局优先级」排序）；
    /// 未收录返回空迭代。同字同码已 first-seen 去重，跨表同码也已去重。
    #[cfg(test)]
    pub fn codes_of(&self, ch: char) -> impl Iterator<Item = &str> {
        self.view_codes(ch).into_iter().flat_map(|v| v.iter())
    }

    /// 某字的**最高优先级辅助码**（合并后全局第一个）；未收录返回 None
    #[cfg(test)]
    pub fn first_code(&self, ch: char) -> Option<&str> {
        self.view_codes(ch).and_then(|v| v.first())
    }

    /// 过滤核心判据：任意辅助码以 `prefix` 为前缀即返回 true。
    ///
    /// **early-return**：找到第一个匹配的码立即停止扫描剩余码。
    pub fn any_code_starts_with(&self, ch: char, prefix: &str) -> bool {
        match self.view_codes(ch) {
            Some(v) => v.iter().any(|c| c.starts_with(prefix)),
            None => false,
        }
    }

    /// 过滤判据（词组逐字匹配用）：任意辅助码以单字符 `c` 开头。
    ///
    /// 与 [`Self::any_code_starts_with`] 的区别：后者按**字符串前缀**匹配（码可长可短，
    /// 需构造前缀串），本方法只按**单个字符**判定首码——词组「每字第一个辅助码」逐位
    /// 对齐时无需临时分配字符串，是按键热路径上的零分配版本。
    pub fn any_code_starts_with_char(&self, ch: char, c: char) -> bool {
        match self.view_codes(ch) {
            Some(v) => v.iter().any(|code| code.starts_with(c)),
            None => false,
        }
    }

    // ------------------------------------------------------------------
    // 状态查询
    // ------------------------------------------------------------------

    /// 码表是否为空（无任何字 → 过滤时全滤）
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 收录的不同汉字数
    pub fn char_count(&self) -> usize {
        self.entries.len()
    }

    /// 收录的总码条数（>= char_count，同字多码会累加；合并表已去重后的值）
    #[cfg(test)]
    pub fn code_count(&self) -> usize {
        self.code_ends.len()
    }

    // ------------------------------------------------------------------
    // 合并辅助：把整张表按「字升序 × 字内优先级」展开成 (字, 码) 对序列
    // ------------------------------------------------------------------

    /// 遍历整张表的所有 (字, 辅助码) 对，按「字升序 × 字内行级优先级」展开。
    ///
    /// 主要供 [`AuxCodeTable::merge`] 内部串联多张表使用：
    /// 按挂载顺序把各表迭代结果首尾相接 → 交给 `from_rows`，
    /// 即可一次性完成跨表优先级排序 + 跨表 first-seen 去重。
    pub fn all_entries_codes(&self) -> impl Iterator<Item = (char, &str)> {
        self.entries.iter().enumerate().flat_map(move |(i, e)| {
            let start = if i == 0 {
                0
            } else {
                self.entries[i - 1].codes_end as usize
            };
            let end = e.codes_end as usize;
            (start..end).map(move |k| (e.ch, self.code_at(k)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // 单表基础用例
    // ------------------------------------------------------------------

    /// 单字单码：基本构建 + 查询
    #[test]
    fn single_code_per_char_basic() {
        let t = AuxCodeTable::from_rows(vec![('李', "mz"), ('杨', "ms"), ('林', "mm")]);
        assert_eq!(t.char_count(), 3);
        assert_eq!(t.code_count(), 3);
        assert_eq!(t.first_code('李'), Some("mz"));
        assert_eq!(t.codes_of('李').collect::<Vec<_>>(), vec!["mz"]);
        assert_eq!(t.first_code('王'), None, "未收录字返回空");
    }

    /// 同字多码：严格按行序保留，不去重不覆盖
    #[test]
    fn multiple_codes_preserve_order() {
        // 对应 rime 例子：厑 有 ib、ii 两码
        let t = AuxCodeTable::from_rows(vec![('厑', "ib"), ('厑', "ii"), ('阿', "ek")]);
        assert_eq!(t.char_count(), 2, "厑 + 阿 = 2 字");
        assert_eq!(t.code_count(), 3, "2 + 1 = 3 码");

        let e_codes: Vec<_> = t.codes_of('厑').collect();
        assert_eq!(e_codes, vec!["ib", "ii"], "多码按行序排列，前行优先");
        assert_eq!(t.first_code('厑'), Some("ib"), "首选码 = 出现最早的那条");

        assert_eq!(t.codes_of('阿').collect::<Vec<_>>(), vec!["ek"]);
    }

    /// 同字完全重复的码：first-seen 去重，只留首次出现位置
    #[test]
    fn duplicate_codes_are_deduplicated() {
        let t = AuxCodeTable::from_rows(vec![('厑', "ib"), ('厑', "ib"), ('厑', "ii")]);
        assert_eq!(t.char_count(), 1);
        assert_eq!(t.code_count(), 2, "重复的 ib 只入列一次：ib + ii = 2");
        let codes: Vec<_> = t.codes_of('厑').collect();
        assert_eq!(codes, vec!["ib", "ii"], "重复 ib 被去重后只留首次出现的");
        // 匹配判断无感知：第一个 ib 命中就 early-return，去重不影响功能
        assert!(t.any_code_starts_with('厑', "ib"));
        // 乱序重复也能去重：ib,ii,ib → first-seen 保序去重后仍是 ib,ii
        let t2 = AuxCodeTable::from_rows(vec![('厑', "ib"), ('厑', "ii"), ('厑', "ib")]);
        let c2: Vec<_> = t2.codes_of('厑').collect();
        assert_eq!(c2, vec!["ib", "ii"]);
    }

    /// 空码行跳过
    #[test]
    fn empty_code_is_skipped() {
        let t = AuxCodeTable::from_rows(vec![('李', ""), ('杨', "ms"), ('杨', "")]);
        assert_eq!(t.char_count(), 1, "李被跳过，只留杨");
        assert_eq!(t.code_count(), 1);
        assert_eq!(t.first_code('李'), None);
        assert_eq!(t.first_code('杨'), Some("ms"));
    }

    /// 乱序输入构建后正确排序并二分
    #[test]
    fn unsorted_input_builds_sorted_entries() {
        let t = AuxCodeTable::from_rows(vec![('林', "mm"), ('李', "mz"), ('杨', "ms")]);
        assert_eq!(t.first_code('李'), Some("mz"));
        assert_eq!(t.first_code('杨'), Some("ms"));
        assert_eq!(t.first_code('林'), Some("mm"));
    }

    /// any_code_starts_with：匹配到任一码即 true，early-return
    #[test]
    fn any_code_starts_with_prefix() {
        let t = AuxCodeTable::from_rows(vec![('厑', "ib"), ('厑', "ii"), ('李', "mz")]);
        assert!(t.any_code_starts_with('厑', "i"));
        assert!(t.any_code_starts_with('厑', "ib"));
        assert!(t.any_code_starts_with('厑', "ii"));
        assert!(!t.any_code_starts_with('厑', "x"));
        assert!(!t.any_code_starts_with('王', "m"));
        assert!(t.any_code_starts_with('李', "mz"));
    }

    /// 空表行为
    #[test]
    fn empty_table() {
        let t = AuxCodeTable::new();
        assert!(t.is_empty());
        assert_eq!(t.char_count(), 0);
        assert_eq!(t.code_count(), 0);
        assert_eq!(t.first_code('李'), None);
        assert!(!t.any_code_starts_with('李', "m"));
    }

    /// codes_of 对未收录字返回空迭代（不 panic）
    #[test]
    fn codes_of_missing_char_is_empty() {
        let t = AuxCodeTable::from_rows(vec![('李', "mz")]);
        assert_eq!(t.codes_of('王').count(), 0);
    }

    /// 码长任意：笔画/拆字 1 码、2 码、4 码混合在同一张表里也能正确切片
    #[test]
    fn mixed_code_lengths_work() {
        // 对应文档示例的长短码混合：u (1B) + ab (2B) + wxyz (4B)
        let t = AuxCodeTable::from_rows(vec![
            ('厑', "u"),
            ('厑', "ab"),
            ('厑', "wxyz"),
            ('李', "abcdef"), // 6 码（笔画方案可能出现）
            ('李', "m"),      // 1 码的简码
        ]);

        // 1) 厑：三码长度各不相同，正确按序切片
        let e: Vec<_> = t.codes_of('厑').collect();
        assert_eq!(e, vec!["u", "ab", "wxyz"]);
        assert_eq!(t.first_code('厑'), Some("u"));
        assert_eq!(t.code_count(), 5, "厑 3 码 + 李 2 码 = 5 码");

        // 2) 前缀匹配正确作用在变长码上
        assert!(t.any_code_starts_with('厑', "u")); // 1 码整码命中
        assert!(t.any_code_starts_with('厑', "a")); // ab 前缀命中
        assert!(t.any_code_starts_with('厑', "ab"));
        assert!(t.any_code_starts_with('厑', "wx")); // wxyz 前 2 位命中
        assert!(t.any_code_starts_with('厑', "wxy"));
        assert!(t.any_code_starts_with('厑', "wxyz"));
        assert!(!t.any_code_starts_with('厑', "wxyza"), "超过码长不匹配");

        // 3) 李：6 码 + 1 码混排
        let li: Vec<_> = t.codes_of('李').collect();
        assert_eq!(li, vec!["abcdef", "m"]);
        assert_eq!(t.first_code('李'), Some("abcdef"), "行序优先，6 码在前");
        assert!(t.any_code_starts_with('李', "abcd"));
        assert!(t.any_code_starts_with('李', "m"));
    }

    /// load_from_file + merge 组合（协调器懒加载路径）：多文件按顺序合并、
    /// 跨文件同码去重、异码共存
    #[test]
    fn load_and_merge_multiple_files() {
        let dir =
            std::env::temp_dir().join(format!("wind-aux-code-merge-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hi = dir.join("high.txt");
        let lo = dir.join("low.txt");
        std::fs::write(&hi, "李=mz\n河=sk\n").unwrap();
        std::fs::write(&lo, "李=mz\n河=dk\n樱=mn\n").unwrap();
        let t = AuxCodeTable::merge(
            [&hi, &lo]
                .into_iter()
                .map(|p| crate::loader::load_from_file(p)),
        );
        for f in [&hi, &lo] {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_dir(&dir);
        // 高优表在前：跨文件同码去重 → 李只剩高优的 mz；河两文件码不同 → sk + dk 并存
        assert_eq!(t.codes_of('李').collect::<Vec<_>>(), vec!["mz"]);
        assert_eq!(t.codes_of('河').collect::<Vec<_>>(), vec!["sk", "dk"]);
        assert_eq!(t.codes_of('樱').collect::<Vec<_>>(), vec!["mn"]);
    }

    // ------------------------------------------------------------------
    // 合并表 / 多表挂载用例（原 AuxCodeStore 层的 6 个测试）
    // ------------------------------------------------------------------

    /// 构造合并双表：拆分（高优）+ 小鹤（低优）
    fn two_tables_merged() -> AuxCodeTable {
        let chaifen = AuxCodeTable::from_rows(vec![
            ('李', "mz"), // 木+子
            ('河', "sk"), // 氵+可
            ('樱', "my"), // 木+婴
            ('厑', "ii"), // rime 示例
        ]);
        let xiaohe = AuxCodeTable::from_rows(vec![
            ('李', "mz"), // 木子
            ('河', "dk"), // 氵口
            ('樱', "mn"), // 木女
            ('厑', "ib"), // rime 示例第二码
        ]);
        // merge(高, 低) 等同于 append_table(高).append_table(低)
        let mut s = AuxCodeTable::new();
        s.append(chaifen);
        s.append(xiaohe);
        s
    }

    /// 合并后顺序：高优先级表的所有码在前
    #[test]
    fn merge_preserves_table_priority_order() {
        let s = two_tables_merged();
        // 樱：拆分表 my（高），小鹤表 mn（低）→ 合并后顺序反映挂载顺序
        let ying: Vec<_> = s.codes_of('樱').collect();
        assert_eq!(ying, vec!["my", "mn"]);
        let yi: Vec<_> = s.codes_of('厑').collect();
        assert_eq!(yi, vec!["ii", "ib"]);
        // 李：两表都是 mz → 跨表同码 first-seen 去重，只输出一个 mz
        let li: Vec<_> = s.codes_of('李').collect();
        assert_eq!(
            li,
            vec!["mz"],
            "跨表同码去重：高优表 mz 首次出现即保留，低优表 mz 被丢弃"
        );
    }

    /// first_code：取最高优先级表的首选码
    #[test]
    fn merge_first_code_takes_highest_priority() {
        let s = two_tables_merged();
        assert_eq!(s.first_code('樱'), Some("my"));
    }

    /// merge：名称取首个非空（与「先出现 = 高优」的码优先级同语义）
    #[test]
    fn merge_name_takes_first_non_empty() {
        let a = AuxCodeTable::from_rows(vec![('李', "mz")]).with_name("拆分".to_string());
        let b = AuxCodeTable::from_rows(vec![('林', "mm")]).with_name("小鹤".to_string());
        let m = AuxCodeTable::merge(vec![a, b]);
        assert_eq!(m.name, "拆分", "高优表名称优先");
        let empty_first = AuxCodeTable::from_rows(vec![('李', "mz")]);
        let named_second =
            AuxCodeTable::from_rows(vec![('林', "mm")]).with_name("笔画".to_string());
        let m2 = AuxCodeTable::merge(vec![empty_first, named_second]);
        assert_eq!(m2.name, "笔画", "首表无名称时取后续首个非空");
    }

    /// 高优表未收录时，回落低优表
    #[test]
    fn merge_first_code_falls_back_to_lower_table() {
        let a = AuxCodeTable::from_rows(vec![('李', "mz")]);
        let b = AuxCodeTable::from_rows(vec![('林', "mm")]);
        let s = AuxCodeTable::merge(vec![a, b]);
        assert_eq!(s.first_code('林'), Some("mm"));
        assert_eq!(s.first_code('王'), None);
    }

    /// 跨表前缀匹配：高优/低优/未命中的分支都覆盖
    #[test]
    fn merge_any_code_starts_with_cross_tables() {
        let s = two_tables_merged();
        assert!(s.any_code_starts_with('李', "m"));
        assert!(s.any_code_starts_with('河', "s")); // 拆分表 sk 直接命中
        assert!(s.any_code_starts_with('河', "d")); // 拆分 sk 不匹配，小鹤 dk 命中
        assert!(!s.any_code_starts_with('樱', "y"));
        assert!(s.any_code_starts_with('樱', "my")); // 第一个码 my
        assert!(s.any_code_starts_with('樱', "mn")); // 第二个码 mn
        assert!(!s.any_code_starts_with('李', "x"));
        assert!(!s.any_code_starts_with('王', "m"));
    }

    /// 空表 append 被跳过（is_empty 仍为 true）
    #[test]
    fn merge_empty_table_append_is_skipped() {
        let mut s = AuxCodeTable::new();
        s.append(AuxCodeTable::new());
        assert!(s.is_empty());
    }

    /// merge 是整体替换：老表内容被清掉
    #[test]
    fn merge_replaces_all_previous() {
        let mut s = two_tables_merged();
        assert!(!s.is_empty());
        // 只用一张新表重新 merge，完全替换
        let new_only = AuxCodeTable::from_rows(vec![('杨', "ms")]);
        s = AuxCodeTable::merge(vec![new_only]);
        assert_eq!(s.first_code('杨'), Some("ms"));
        assert_eq!(s.first_code('李'), None, "老表的李必须被清掉");
    }
}
