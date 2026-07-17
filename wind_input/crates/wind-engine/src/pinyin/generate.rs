//! 词语全拼编码生成（造词反推读音）。
//!
//! 与 Go 版 `internal/engine/pinyin` 的 `GenerateWordPinyin` 对齐：
//! 为词语推断全拼编码（如"你好"→"nihao"），核心是解决**多音字在词里读哪个音**。
//!
//! 三级优先策略：
//!  1. 整词命中：枚举每字所有读音的笛卡尔积（按权重降序），第一个能让词典查回该词的组合即最优。
//!  2. 最长子词切分：DP 把词切成已知子词序列（如"长江三角洲"=长江+三角洲），继承子词整体读音。
//!  3. 逐字代表读音兜底：确保至少有结果。
//!
//! 单字读音索引 [`CharPinyinIndex`] 从**词典本身**派生（遍历标准音节查单字候选、按权重排序），
//! 不依赖 wind-reverse 的 pinyin_map.txt——代表读音 = 词典里权重最高的读音。

use std::collections::HashMap;

use wind_dict::cached::CachedDict;

use super::syllable::STANDARD_SYLLABLES;

/// 整词读音消歧时笛卡尔积组合数上限（防生僻多音字长词性能塌方）。
const MAX_READING_COMBOS: usize = 64;

/// 汉字 → 读音反向索引。
///
/// 多音字 `char_all` 按词典权重降序，`char`（代表读音）= 权重最高者。
/// 由 [`CharPinyinIndex::build`] 遍历 [`STANDARD_SYLLABLES`] 查词典单字候选构建。
#[derive(Debug, Default)]
pub struct CharPinyinIndex {
    /// 汉字 → 代表读音（权重最高）
    char: HashMap<char, String>,
    /// 汉字 → 所有读音（按权重降序），用于多音字消歧
    char_all: HashMap<char, Vec<String>>,
}

impl CharPinyinIndex {
    /// 从词典构建索引：遍历标准音节，收集单字候选及其权重，按权重降序定读音。
    pub fn build(dict: &CachedDict) -> Self {
        // 每字暂存 (读音, 权重)；同字同音节多条（异体/多源）合并取最大权重
        let mut all: HashMap<char, Vec<(String, i32)>> = HashMap::new();
        for &syl in STANDARD_SYLLABLES {
            for (text, weight, _order) in dict.search(syl) {
                let mut chars = text.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    continue; // 仅单字
                };
                let entry = all.entry(c).or_default();
                if let Some(e) = entry.iter_mut().find(|(s, _)| s == syl) {
                    if weight > e.1 {
                        e.1 = weight;
                    }
                } else {
                    entry.push((syl.to_string(), weight));
                }
            }
        }

        let mut char = HashMap::with_capacity(all.len());
        let mut char_all = HashMap::with_capacity(all.len());
        for (c, mut list) in all {
            // 按权重降序，第 0 个即代表读音
            list.sort_by_key(|(_, w)| std::cmp::Reverse(*w));
            let readings: Vec<String> = list.into_iter().map(|(s, _)| s).collect();
            char.insert(c, readings[0].clone());
            char_all.insert(c, readings);
        }
        Self { char, char_all }
    }

    fn representative(&self, c: char) -> Option<&str> {
        self.char.get(&c).map(String::as_str)
    }

    fn readings(&self, c: char) -> Option<&[String]> {
        self.char_all.get(&c).map(Vec::as_slice)
    }
}

/// 按音节/词段拼接 code，**顺带累积音节边界**。
///
/// 造词本就是「逐音节拼起来」，每次拼接前的 `code.len()` 就是该音节的起始字节位——边界是
/// 白送的，此前只是被 `String::push_str` 丢掉，逼得下游用 DAG 反猜（甚至靠 410 音节暴力反查）。
///
/// 溢出（拼接后 >64 字节，bitmask 装不下）时整体作废为 0 = 无边界信息，与
/// `wind_dict` 侧 `syllable_boundary_mask` 的降级契约一致：宁可不给，不给半截错的。
struct CodeBuilder {
    code: String,
    mask: u64,
    overflow: bool,
}

impl CodeBuilder {
    fn new(cap: usize) -> Self {
        Self {
            code: String::with_capacity(cap),
            mask: 0,
            overflow: false,
        }
    }

    /// 追加一个**已知内部边界**的词段（如整词 "nihao" + 段内 mask ni|hao）。
    /// 段内 mask 的 bit 是段内偏移，须平移 `base` 到全局位置。
    fn push_segment(&mut self, s: &str, seg_mask: u64) {
        let base = self.code.len();
        // 段内 mask 的置位不会超出 s.len()，故 base+s.len()<=64 即可安全左移。
        if base + s.len() > 64 {
            self.overflow = true;
        } else {
            self.mask |= seg_mask << base;
        }
        self.code.push_str(s);
    }

    /// 追加一个音节（段内 mask 恒为 `0b1`：段首即音节首）。
    fn push_syllable(&mut self, s: &str) {
        self.push_segment(s, 0b1);
    }

    fn finish(self) -> (String, u64) {
        (self.code, if self.overflow { 0 } else { self.mask })
    }
}

/// 为词语生成全拼编码**与音节边界**。含无读音字符时返回 `None`。
///
/// 返回的 boundary 语义同 `wind_dict::binformat::DictEntry::boundary`，供用户自造词从
/// 诞生起就带上边界（否则用户词是块「边界空洞」，双拼校验只能对其降级）。
///
/// `dict` 为拼音系统词典（提供整词验证的真值表），`index` 为单字读音索引。
pub fn generate_word_pinyin(
    dict: &CachedDict,
    index: &CharPinyinIndex,
    word: &str,
) -> Option<(String, u64)> {
    let runes: Vec<char> = word.chars().collect();
    if runes.is_empty() {
        return None;
    }
    // 1) 整词命中
    if let Some(r) = infer_whole_word_code(dict, index, &runes, word) {
        return Some(r);
    }
    // 2) 子词切分 + 整体读音继承
    if let Some(r) = infer_by_subword_segmentation(dict, index, &runes) {
        return Some(r);
    }
    // 3) 兜底：逐字代表读音（每字一音节，边界即逐字累积）
    let mut b = CodeBuilder::new(runes.len() * 4);
    for &r in &runes {
        b.push_syllable(index.representative(r)?);
    }
    Some(b.finish())
}

/// 用词典真值表为整词推断读音：枚举每字读音笛卡尔积，找到第一个能查回该词的组合。
/// 每字读音按权重降序，故按字典序枚举时首个命中天然是"各字读音权重之和"最高的合理组合。
/// 单字不进入此分支（无消歧必要）。
fn infer_whole_word_code(
    dict: &CachedDict,
    index: &CharPinyinIndex,
    runes: &[char],
    word: &str,
) -> Option<(String, u64)> {
    if runes.len() < 2 {
        return None;
    }
    // 收集每字读音列表，同时估算笛卡尔积规模
    let mut readings: Vec<&[String]> = Vec::with_capacity(runes.len());
    let mut combos = 1usize;
    for &r in runes {
        let rs = index.readings(r)?;
        if rs.is_empty() {
            return None;
        }
        combos *= rs.len();
        if combos > MAX_READING_COMBOS {
            return None;
        }
        readings.push(rs);
    }
    // 笛卡尔积枚举（按字典序，等价于按权重组合的优先级）
    let mut idxs = vec![0usize; runes.len()];
    loop {
        // 每字一音节 → 边界随拼接累积（readings[i][pos] 即第 i 字选中的读音）。
        let mut b = CodeBuilder::new(runes.len() * 4);
        for (i, &pos) in idxs.iter().enumerate() {
            b.push_syllable(&readings[i][pos]);
        }
        let (code, mask) = b.finish();
        if dict.search(&code).iter().any(|(text, _, _)| text == word) {
            return Some((code, mask));
        }
        // 递增到下一个组合（低位满则进位）
        let mut k = runes.len();
        loop {
            if k == 0 {
                return None;
            }
            k -= 1;
            idxs[k] += 1;
            if idxs[k] < readings[k].len() {
                break;
            }
            idxs[k] = 0;
        }
    }
}

/// DP 节点：拼出 `word[..i]` 字段的最优方案。
#[derive(Clone)]
struct DpState {
    prev: usize,
    /// 该段为多字子词时的整体读音 code；单字过渡为空。
    seg: String,
    /// `seg` 的**段内**音节边界（多字子词自身可含多音节，如「你好」→ ni|hao）。
    /// 回溯拼接时须平移到全局位置，否则长词的段内边界会丢。
    seg_mask: u64,
    /// 已用多字子词段数（越少越优，同总字数下）。
    multi_segs: usize,
    /// 已用多字子词的总字数（越大越优）。
    total_mul: usize,
}

/// `a` 是否优于 `b`：多字子词总字数高 > 段数少（更长子词优先）。
fn better(a: &DpState, b: &DpState) -> bool {
    if a.total_mul != b.total_mul {
        a.total_mul > b.total_mul
    } else {
        a.multi_segs < b.multi_segs
    }
}

/// 用 DP 把词切成已知子词序列，继承子词整体读音（解决长词中的多音字）。
/// 找不到任何多字子词切分时返回 `None`，让调用方走逐字兜底。
fn infer_by_subword_segmentation(
    dict: &CachedDict,
    index: &CharPinyinIndex,
    runes: &[char],
) -> Option<(String, u64)> {
    let n = runes.len();
    if n < 2 {
        return None;
    }
    let mut dp: Vec<Option<DpState>> = vec![None; n + 1];
    // dp[0].prev 永不被回溯读取（回溯条件 cur > 0），用 0 占位即可
    dp[0] = Some(DpState {
        prev: 0,
        seg: String::new(),
        seg_mask: 0,
        multi_segs: 0,
        total_mul: 0,
    });

    for i in 0..n {
        let Some(cur) = dp[i].clone() else {
            continue;
        };
        // 长度 >=2 的子段做整词查（单字走兜底过渡）
        let mut l = 2;
        while i + l <= n {
            let sub: String = runes[i..i + l].iter().collect();
            if let Some((code, mask)) = infer_whole_word_code(dict, index, &runes[i..i + l], &sub) {
                let next = DpState {
                    prev: i,
                    seg: code,
                    seg_mask: mask,
                    multi_segs: cur.multi_segs + 1,
                    total_mul: cur.total_mul + l,
                };
                if dp[i + l].as_ref().is_none_or(|d| better(&next, d)) {
                    dp[i + l] = Some(next);
                }
            }
            l += 1;
        }
        // 单字过渡（不计入 total_mul，仅承接前缀状态）
        let next = DpState {
            prev: i,
            seg: String::new(),
            seg_mask: 0,
            multi_segs: cur.multi_segs,
            total_mul: cur.total_mul,
        };
        if dp[i + 1].as_ref().is_none_or(|d| better(&next, d)) {
            dp[i + 1] = Some(next);
        }
    }

    let final_state = dp[n].as_ref()?;
    if final_state.total_mul == 0 {
        // 没有任何多字子词被命中，让上层走代表读音兜底
        return None;
    }
    // 回溯重建（从后往前收集各段，再反转）
    struct Span {
        from: usize,
        code: String,
        mask: u64,
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut cur = n;
    while cur > 0 {
        let s = dp[cur].as_ref().expect("dp 链应连续");
        spans.push(Span {
            from: s.prev,
            code: s.seg.clone(),
            mask: s.seg_mask,
        });
        cur = s.prev;
    }
    spans.reverse();

    let mut b = CodeBuilder::new(n * 4);
    for sp in &spans {
        if !sp.code.is_empty() {
            // 多字子词段：段内自带音节边界（如「你好」→ ni|hao），平移到全局位置。
            b.push_segment(&sp.code, sp.mask);
        } else {
            // 单字段：用代表读音（本身即一个音节）
            b.push_syllable(index.representative(runes[sp.from])?);
        }
    }
    Some(b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wind_dict::codetable::CodetableDict;

    /// 用 (code, text, weight) 三元组建内存拼音词典。
    fn dict_from(entries: &[(&str, &str, i32)]) -> CachedDict {
        let mut d = CodetableDict::empty();
        for (code, text, weight) in entries {
            d.merge_single(code.to_string(), text.to_string(), *weight, 0);
        }
        CachedDict::Memory(d)
    }

    fn gen_py(entries: &[(&str, &str, i32)], word: &str) -> Option<String> {
        gen_py_full(entries, word).map(|(c, _)| c)
    }

    /// 同 `gen_py`，但保留音节边界，供边界相关断言。
    fn gen_py_full(entries: &[(&str, &str, i32)], word: &str) -> Option<(String, u64)> {
        let dict = dict_from(entries);
        let idx = CharPinyinIndex::build(&dict);
        generate_word_pinyin(&dict, &idx, word)
    }

    /// 造词须同时产出音节边界——用户自造词的边界从此有来源，不再是「空洞」。
    /// 三条产码路径（整词消歧 / 子词切分 / 逐字兜底）都要带边界。
    #[test]
    fn generate_word_pinyin_carries_boundary() {
        let entries = &[
            ("ni", "你", 100),
            ("hao", "好", 100),
            ("nihao", "你好", 500),
            ("chong", "重", 50),
            ("zhong", "重", 900),
            ("qing", "庆", 100),
            ("chongqing", "重庆", 800),
        ];
        // 整词命中：ni|hao → 起始 {0,2}
        assert_eq!(gen_py_full(entries, "你好"), Some(("nihao".into(), 0b101)));
        // 整词消歧（重庆读 chongqing 而非 zhongqing）：chong|qing → 起始 {0,5}
        assert_eq!(
            gen_py_full(entries, "重庆"),
            Some(("chongqing".into(), 0b100001))
        );
        // 单字：整串一个音节 → {0}
        assert_eq!(gen_py_full(entries, "你"), Some(("ni".into(), 0b1)));
        // 逐字兜底（整词不在词典）：ni|zhong → 起始 {0,2}
        assert_eq!(
            gen_py_full(entries, "你重"),
            Some(("nizhong".into(), 0b101))
        );
    }

    /// 子词切分路径：段自身是多音节整词时，**段内边界须平移到全局**，不能按「一段一音节」记。
    #[test]
    fn subword_segmentation_preserves_inner_boundary() {
        let entries = &[
            ("ni", "你", 100),
            ("hao", "好", 100),
            ("nihao", "你好", 500),
            ("a", "啊", 100),
        ];
        // 「你好啊」整词不在词典 → 子词切分：段「你好」(nihao, 段内 ni|hao) + 单字「啊」(a)。
        // 全局边界须为 ni|hao|a = 起始 {0,2,5}，而非把 nihao 当单个音节记成 {0,5}。
        assert_eq!(
            gen_py_full(entries, "你好啊"),
            Some(("nihaoa".into(), 0b100101))
        );
    }

    /// 多音字按权重择优：费→fei(1000) 而非 bi(50)，强→qiang(1000) 而非 jiang(80)。
    #[test]
    fn multi_pron_by_weight() {
        let entries = &[
            ("fei", "费", 1000),
            ("bi", "费", 50),
            ("qiang", "强", 1000),
            ("jiang", "强", 80),
            ("xiao", "晓", 1000),
        ];
        assert_eq!(gen_py(entries, "费").as_deref(), Some("fei"));
        assert_eq!(gen_py(entries, "强").as_deref(), Some("qiang"));
        // 整词不在词典 → 逐字代表读音兜底
        assert_eq!(gen_py(entries, "费晓强").as_deref(), Some("feixiaoqiang"));
    }

    /// 整词命中覆盖逐字代表读音：重→代表音 zhong，但"重庆"整词读 chongqing。
    #[test]
    fn whole_word_overrides_per_char() {
        let entries = &[
            ("zhong", "重", 1000),
            ("chong", "重", 80),
            ("qing", "庆", 1000),
            ("chongqing", "重庆", 500),
        ];
        assert_eq!(gen_py(entries, "重庆").as_deref(), Some("chongqing"));
    }

    /// 长词子词切分继承读音：长→代表音 zhang，但经"长江"+"三角洲"得 changjiangsanjiaozhou。
    #[test]
    fn subword_segmentation() {
        let entries = &[
            ("zhang", "长", 1000),
            ("chang", "长", 500),
            ("jiang", "江", 1000),
            ("san", "三", 1000),
            ("jiao", "角", 1000),
            ("zhou", "洲", 1000),
            ("changjiang", "长江", 600),
            ("sanjiaozhou", "三角洲", 500),
        ];
        assert_eq!(
            gen_py(entries, "长江三角洲").as_deref(),
            Some("changjiangsanjiaozhou")
        );
    }

    /// 简单整词命中。
    #[test]
    fn simple_whole_word() {
        let entries = &[
            ("ni", "你", 1000),
            ("hao", "好", 1000),
            ("nihao", "你好", 800),
        ];
        assert_eq!(gen_py(entries, "你好").as_deref(), Some("nihao"));
    }

    /// 含无读音字符 → None。
    #[test]
    fn unknown_char_returns_none() {
        let entries = &[("ni", "你", 1000)];
        // "你X"：X 无任何读音
        assert_eq!(gen_py(entries, "你X"), None);
    }
}
