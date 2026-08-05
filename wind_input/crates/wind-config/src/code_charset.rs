//! 码元字符集（`[engine.codetable].input_chars` / `.leading_chars`）的解析与查表。
//!
//! 方案声明哪些字符构成「输入码」——决定一次按键是进输入缓冲，还是作标点/选词/上屏/透传。
//! 例：五笔标准 `a-x`；某库还含 `/test` 这类词条 → 配 `a-x/`；要打 `Win10` 这类词条 → `a-z0-9`。
//!
//! 设计与优先级契约见 `docs/design/codetable-input-chars.md`。
//!
//! ## 全集与首码集
//!
//! 数字必须能作码元（`Win10`），但**不能作首码**——空缓冲下的数字键是选词/透传，
//! 若它同时是首码，用户永远打不出「第 1 个候选」，也拿不回原生数字输入。
//! 故码元集分两层：
//!
//! - `input_chars`：全集，哪些字符可以进缓冲。
//! - `leading_chars`：首码集，缓冲为空时哪些字符可以起头。缺省 = 全集。
//!
//! ★ 首码集恒为全集的子集。配了「能起头却不是码元」的字符属于自相矛盾，
//!   一律按交集处理并告警——不这样做的话，该字符会在空缓冲时进缓冲、
//!   下一键却因不是码元而被判非法，形成一个自己走不出去的状态。
//!
//! ## 两条不可让步的约束
//!
//! 1. **解析失败/为空一律回落 `a-z`，绝不产出空集**。空集意味着该方案一个字也打不出来，
//!    比「忽略了这项配置」严重得多——用户会认为输入法整个坏了，而不会想到是某个字符集写错。
//! 2. **一律小写归一**。`input_buffer` 恒存小写（见 `coordinator.rs` 字母累积臂：
//!    「缓冲恒存小写，z-fallback 探针、顶码判定、引擎查询、词频记账全部只看它」），
//!    码元集若不同域，集内的大写字符永远不会被命中。

use std::fmt;

/// 码元字符集解析错误。配置消费点一般不直接处理，走 [`CodeCharSet::new`] 回落。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeCharSetError {
    /// 含非 ASCII 字符。码元来自物理按键，非 ASCII 无从输入。
    NonAscii,
    /// 含空格或控制字符。空格是 Space 键（有独立的上屏语义），不能同时作码元。
    NonPrintable(char),
    /// 范围端点逆序，如 `z-a`。
    BadRange(char, char),
    /// 解析结果为空集。
    Empty,
}

impl fmt::Display for CodeCharSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonAscii => write!(f, "含非 ASCII 字符"),
            Self::NonPrintable(c) => write!(f, "含空格或控制字符 {:?}", c),
            Self::BadRange(lo, hi) => write!(f, "范围端点逆序：{}-{}", lo, hi),
            Self::Empty => write!(f, "解析结果为空集"),
        }
    }
}

impl std::error::Error for CodeCharSetError {}

/// ASCII 位图。下标即码位；非 ASCII 一律不是码元，故 128 足够。
type Bitmap = [bool; 128];

/// 码元字符集。按键热路径每键查一次，故用位图而非 `BTreeSet`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeCharSet {
    /// 全集：可进输入缓冲的字符。
    all: Bitmap,
    /// 首码集：缓冲为空时可起头的字符。恒为 `all` 的子集。
    leading: Bitmap,
}

impl CodeCharSet {
    /// 内置默认：26 个小写字母，全集与首码集相同。等价于历史上硬编码的 `VK_A..=VK_Z`。
    pub fn default_alpha() -> Self {
        let mut all = [false; 128];
        for c in b'a'..=b'z' {
            all[c as usize] = true;
        }
        Self { all, leading: all }
    }

    /// 从配置的两个字符串构建，**配置消费点一律走这个**。
    ///
    /// - `all_spec` 为空/非法 → 回落 `a-z`（见模块头「约束 1」）。
    /// - `leading_spec` 为空 → 首码集 = 全集。
    /// - `leading_spec` 非法 → 首码集回落为全集（保守：宁可多允许，不可让方案打不出字）。
    /// - `leading_spec` 含全集外的字符 → 取交集并告警；交集为空则回落为全集。
    ///
    /// `label` 用于日志定位是哪个方案配错了——配置错误只有日志这一条反馈通路，
    /// 不标出处等于让用户对着一个「没反应」的配置项干瞪眼。
    pub fn new(all_spec: &str, leading_spec: &str, label: &str) -> Self {
        let all = match parse_bitmap_or_warn(all_spec, label, "input_chars") {
            Some(m) => m,
            None => return Self::default_alpha(),
        };
        let leading = match parse_bitmap_or_warn(leading_spec, label, "leading_chars") {
            // 显式配了首码集：与全集取交集（★ 首码集恒为全集子集，见模块头）。
            Some(m) => {
                let mut inter = [false; 128];
                let mut dropped = Vec::new();
                for i in 0..128 {
                    if m[i] {
                        if all[i] {
                            inter[i] = true;
                        } else {
                            dropped.push(i as u8 as char);
                        }
                    }
                }
                if !dropped.is_empty() {
                    tracing::warn!(
                        "leading_chars 含 input_chars 之外的字符（{}）：{:?}；已忽略这些字符",
                        label,
                        dropped
                    );
                }
                if inter.iter().all(|&v| !v) {
                    tracing::warn!(
                        "leading_chars 与 input_chars 无交集（{}）；首码集回落为全集",
                        label
                    );
                    all
                } else {
                    inter
                }
            }
            // 未配或非法：首码集 = 全集。
            None => all,
        };
        Self { all, leading }
    }

    /// 严格解析单个字符集规格。空串按 [`CodeCharSetError::Empty`] 处理——「空=回落」的语义
    /// 由调用方（[`Self::new`]）表达，解析器本身不替调用方决定回落。
    ///
    /// 格式：范围 + 字面集，如 `"a-x"` / `"a-x/"` / `"a-z0-9"`。
    /// - 范围 `X-Y`：`X <= Y`，闭区间。
    /// - 字面：其余字符逐个收入。
    /// - `-` 作字面：位于首位或末位时（`"-a-z"` / `"a-z-"`），与正则字符类惯例一致。
    /// - 大小写：一律归一为小写（见模块头「约束 2」）。
    pub fn parse(spec: &str) -> Result<Self, CodeCharSetError> {
        let all = parse_bitmap(spec)?;
        Ok(Self { all, leading: all })
    }

    /// 内置默认集（`a-z`）是否含该字符。
    ///
    /// 供「引擎无码元集概念」的回落判定使用：那条路上每次按键都要判一次，
    /// 为一个布尔值构造整张位图不划算。与 `default_alpha().contains(ch)` 恒等
    /// （有测试锁住），改其中一个必须同时改另一个。
    #[inline]
    pub fn default_contains(ch: char) -> bool {
        ch.is_ascii_lowercase()
    }

    /// 该字符是否为码元（可进输入缓冲）。非 ASCII 恒 `false`。
    #[inline]
    pub fn contains(&self, ch: char) -> bool {
        let c = ch as u32;
        c < 128 && self.all[c as usize]
    }

    /// 该字符是否可作**首码**（缓冲为空时起头）。非 ASCII 恒 `false`。
    #[inline]
    pub fn contains_leading(&self, ch: char) -> bool {
        let c = ch as u32;
        c < 128 && self.leading[c as usize]
    }

    /// 是否恰好等于内置默认 `a-z`（全集与首码集皆是）。
    ///
    /// 消费点据此走「与历史逐键等价」的快捷路径：默认集下不引入任何新判断，
    /// 是各期「零回归」最直接的保证（也让默认路径不为这个特性付出代价）。
    pub fn is_default_alpha(&self) -> bool {
        *self == Self::default_alpha()
    }

    /// 首码集是否严格小于全集（存在「能作码元但不能起头」的字符，如数字）。
    pub fn has_non_leading(&self) -> bool {
        (0..128).any(|i| self.all[i] && !self.leading[i])
    }

    /// 全集内的字符（升序），用于日志与测试。
    pub fn chars(&self) -> Vec<char> {
        bitmap_chars(&self.all)
    }

    /// 首码集内的字符（升序），用于日志与测试。
    pub fn leading_chars(&self) -> Vec<char> {
        bitmap_chars(&self.leading)
    }
}

impl Default for CodeCharSet {
    fn default() -> Self {
        Self::default_alpha()
    }
}

fn bitmap_chars(m: &Bitmap) -> Vec<char> {
    (0u8..128)
        .filter(|&c| m[c as usize])
        .map(|c| c as char)
        .collect()
}

/// 解析并在失败时告警；空串与非法都返回 `None`（回落语义由调用方定）。
fn parse_bitmap_or_warn(spec: &str, label: &str, field: &str) -> Option<Bitmap> {
    if spec.trim().is_empty() {
        return None;
    }
    match parse_bitmap(spec) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(
                "{} 解析失败（{}）：{}；按未配置处理。原值={:?}",
                field,
                label,
                e,
                spec
            );
            None
        }
    }
}

fn parse_bitmap(spec: &str) -> Result<Bitmap, CodeCharSetError> {
    if !spec.is_ascii() {
        return Err(CodeCharSetError::NonAscii);
    }
    let b = spec.as_bytes();
    let mut m = [false; 128];
    let mut i = 0;
    while i < b.len() {
        // 范围形态 `X-Y`：要求 `-` 后还有字符，故 `-` 在末位时落不进这里，自然成为字面。
        // 同理 `-` 在首位时 b[1] 不是 `-`，也走字面分支。
        if i + 2 < b.len() && b[i + 1] == b'-' {
            let lo = b[i].to_ascii_lowercase();
            let hi = b[i + 2].to_ascii_lowercase();
            check_printable(lo)?;
            check_printable(hi)?;
            if lo > hi {
                return Err(CodeCharSetError::BadRange(lo as char, hi as char));
            }
            for c in lo..=hi {
                m[c as usize] = true;
            }
            i += 3;
        } else {
            let c = b[i].to_ascii_lowercase();
            check_printable(c)?;
            m[c as usize] = true;
            i += 1;
        }
    }
    if m.iter().all(|&v| !v) {
        return Err(CodeCharSetError::Empty);
    }
    Ok(m)
}

/// 码元必须是可打印且非空格的 ASCII。
///
/// 空格被排除是刻意的：Space 键在各输入模式下都有独立语义（上屏/选词/全角空格），
/// 把它同时当码元会与那些路径正面冲突，而这不是本特性要解决的问题。
fn check_printable(c: u8) -> Result<(), CodeCharSetError> {
    if (0x21..=0x7E).contains(&c) {
        Ok(())
    } else {
        Err(CodeCharSetError::NonPrintable(c as char))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(spec: &str) -> String {
        CodeCharSet::parse(spec)
            .unwrap()
            .chars()
            .into_iter()
            .collect()
    }

    #[test]
    fn default_alpha_is_26_letters() {
        let s = CodeCharSet::default_alpha();
        assert_eq!(s.chars().len(), 26);
        assert!(s.contains('a') && s.contains('z'));
        assert!(!s.contains('0') && !s.contains('/'));
        assert!(s.is_default_alpha());
        assert!(!s.has_non_leading(), "默认集的首码集应等于全集");
    }

    #[test]
    fn parses_range() {
        assert_eq!(set_of("a-x"), "abcdefghijklmnopqrstuvwx");
        assert_eq!(set_of("a-c"), "abc");
    }

    /// 注意输出恒按 ASCII 升序，与规格里的书写顺序无关（`/` 是 0x2F，排在字母之前）。
    #[test]
    fn parses_range_plus_literal() {
        assert_eq!(set_of("a-x/"), "/abcdefghijklmnopqrstuvwx");
        // 字面写在范围前后，结果相同。
        assert_eq!(set_of("/a-c"), "/abc");
        assert_eq!(set_of("a-c/"), "/abc");
    }

    #[test]
    fn parses_multiple_ranges() {
        assert_eq!(set_of("a-c0-2"), "012abc");
    }

    #[test]
    fn parses_pure_literals() {
        assert_eq!(set_of("abc"), "abc");
        assert_eq!(set_of(";'"), "';"); // 输出按 ASCII 升序
    }

    /// `-` 在首位或末位作字面，与正则字符类惯例一致。
    #[test]
    fn dash_as_literal_at_edges() {
        assert_eq!(set_of("-a-c"), "-abc");
        assert_eq!(set_of("a-c-"), "-abc");
        assert_eq!(set_of("-"), "-");
    }

    /// 大小写一律归一为小写——`input_buffer` 恒存小写，不同域则集内大写永不命中。
    #[test]
    fn normalizes_to_lowercase() {
        assert_eq!(set_of("A-Z"), set_of("a-z"));
        assert_eq!(set_of("ABC"), "abc");
        let s = CodeCharSet::parse("A-Z").unwrap();
        assert!(s.contains('a'));
        assert!(s.is_default_alpha(), "A-Z 归一后应等价于内置默认");
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            CodeCharSet::parse("z-a"),
            Err(CodeCharSetError::BadRange('z', 'a'))
        );
        assert_eq!(CodeCharSet::parse("中"), Err(CodeCharSetError::NonAscii));
        assert_eq!(CodeCharSet::parse(""), Err(CodeCharSetError::Empty));
        assert!(matches!(
            CodeCharSet::parse("a b"),
            Err(CodeCharSetError::NonPrintable(' '))
        ));
    }

    /// ★ 非法输入必须回落 `a-z`，**绝不能变成空集**——空集＝该方案一个字也打不出。
    #[test]
    fn invalid_falls_back_to_alpha_never_empty() {
        for bad in ["", "   ", "z-a", "中文", "a b"] {
            let s = CodeCharSet::new(bad, "", "test");
            assert!(
                s.is_default_alpha(),
                "非法输入 {:?} 应回落 a-z，实际 {:?}",
                bad,
                s.chars()
            );
            assert!(s.contains('a'), "回落后必须能打字，{:?}", bad);
        }
    }

    /// `default_contains` 是 `default_alpha().contains` 的免构造版本，两者必须恒等——
    /// 它们分别服务于「引擎无码元集」与「引擎有码元集」两条路径，漂移就会让同一个字符
    /// 在两条路上得到相反的判定。
    #[test]
    fn default_contains_matches_default_alpha() {
        let s = CodeCharSet::default_alpha();
        for c in 0u8..128 {
            let ch = c as char;
            assert_eq!(
                CodeCharSet::default_contains(ch),
                s.contains(ch),
                "字符 {:?} 的两种默认判定不一致",
                ch
            );
        }
        assert!(!CodeCharSet::default_contains('中'));
    }

    #[test]
    fn contains_rejects_non_ascii() {
        let s = CodeCharSet::parse("a-z").unwrap();
        assert!(!s.contains('中'));
        assert!(!s.contains('\u{1F600}'));
        assert!(!s.contains_leading('中'));
    }

    /// 子集与含符号集都不得被误判为默认集——`is_default_alpha` 是各消费点的快捷路径开关，
    /// 误判会让配了码元集的方案走进「与历史等价」的老路，表现为配置完全没生效。
    #[test]
    fn non_default_sets_are_not_default_alpha() {
        assert!(!CodeCharSet::parse("a-x").unwrap().is_default_alpha());
        assert!(!CodeCharSet::parse("a-z/").unwrap().is_default_alpha());
        assert!(!CodeCharSet::new("a-z0-9", "a-z", "t").is_default_alpha());
    }

    // ── 首码集 ──

    /// 主场景：数字可作码元（`Win10`）但不可作首码。
    #[test]
    fn digits_as_code_but_not_leading() {
        let s = CodeCharSet::new("a-z0-9", "a-z", "test");
        assert!(s.contains('1'), "数字应是码元");
        assert!(!s.contains_leading('1'), "数字不应可作首码");
        assert!(s.contains('w') && s.contains_leading('w'));
        assert!(s.has_non_leading());
    }

    /// 未配首码集时，首码集 = 全集。
    #[test]
    fn leading_defaults_to_all() {
        let s = CodeCharSet::new("a-z0-9", "", "test");
        assert!(s.contains('1') && s.contains_leading('1'));
        assert!(!s.has_non_leading());
        assert_eq!(s.chars(), s.leading_chars());
    }

    /// ★ 首码集恒为全集子集：配了全集外的字符要被丢掉，否则该字符会在空缓冲时进缓冲、
    /// 下一键却因不是码元而非法，形成走不出去的状态。
    #[test]
    fn leading_is_intersected_with_all() {
        let s = CodeCharSet::new("a-c", "a-z", "test");
        assert_eq!(s.leading_chars(), vec!['a', 'b', 'c']);
        assert!(!s.contains_leading('x'), "全集外的首码字符必须被丢弃");
    }

    /// 首码集与全集无交集 → 回落为全集，而不是留下一个「无法起头」的死方案。
    #[test]
    fn leading_disjoint_falls_back_to_all() {
        let s = CodeCharSet::new("a-c", "x-z", "test");
        assert_eq!(s.leading_chars(), vec!['a', 'b', 'c']);
        assert!(s.contains_leading('a'), "无交集时必须仍能起头");
    }

    /// 首码集非法 → 回落为全集（保守：宁可多允许，不可让方案打不出字）。
    #[test]
    fn invalid_leading_falls_back_to_all() {
        let s = CodeCharSet::new("a-z0-9", "z-a", "test");
        assert!(s.contains_leading('0'), "首码集非法时应回落为全集");
        assert!(!s.has_non_leading());
    }
}
