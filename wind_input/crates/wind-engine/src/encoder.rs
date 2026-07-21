//! 码表词组编码器：按方案 `[[encoder.rules]]` 的公式，从各字**全码**组装词组编码。
//!
//! 五笔 86 的规则形如 `AaAbBaBb`（二字取各字前两码）、`AaBaCaCb`（三字取前两字首码+末字前
//! 两码）、`AaBaCaZa`（四字及以上取前三字首码+末字首码）。公式每两个字符一组：**大写=字序**
//! （`A`=第 1 字，`Z`=末字），**小写=码序**（`a`=第 1 码）。
//!
//! # 为什么不能拼接各字全码
//!
//! 造词的唯一目的是「造出来的词以后能打出来」。五笔「你好」的词组码是 `wqvb`（各取前两码），
//! 而拼接全码得到 `wqiyvbg` 之类——词库里查不到，等于没造。旧 `learn_phrase_on_commit` 正是
//! 拼接各段码（`code.push_str`），这是自动造词「完全不工作」的两个根因之一。
//!
//! # 与旧 `wind_reverse::wubi_word_code` 的关系
//!
//! 后者把五笔 86 规则**硬编码**成 `match chars.len()` 三个分支，且码源是**拆字表**而非码表
//! 词库。两者对五笔 86 结果等价（公式与硬编码规则一一对应），但硬编码版换任何非五笔码表方案
//! 就静默出错。本模块取代它，手动造词与自动造词统一走这里。

use wind_config::schema::{EncoderRule, EncoderSpec};

/// 公式的一步：取第 `char_index` 个字的第 `code_index` 位码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FormulaStep {
    /// 字序，0-based；`-1` 表示末字（公式中的 `Z`）。
    char_index: i32,
    /// 码序，0-based（`a`=0）。
    code_index: usize,
}

/// 取码失败的原因。**携带具体是哪个字卡住**——这是排查「自动造词不生效」最关键的线索
/// （对齐 Go `CalcWordCode returned empty` 那条注释的意图，但 Go 只返回空串丢失了原因）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// 词太短（< 2 字），没有词组编码的概念。
    TooShort,
    /// 方案没有配 `[[encoder.rules]]`。
    NoRules,
    /// 该词长没有匹配的规则。
    NoMatchingRule { word_len: usize },
    /// 公式本身非法（长度为奇数、含非字母等）。
    BadFormula { formula: String },
    /// 公式引用的字序超出词长（规则与词长不匹配，属方案配置错误）。
    CharIndexOutOfRange { char_index: i32, word_len: usize },
    /// 该字在码表词库中查不到任何码。**整词作废**，不做「跳过该字」的降级。
    MissingCode { ch: char },
    /// 该字的全码位数不够公式要求（如公式要第 2 码但该字只有 1 位码）。
    CodeTooShort { ch: char, code: String, need: usize },
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "词少于 2 字，无词组编码"),
            Self::NoRules => write!(f, "方案未配置 [[encoder.rules]]"),
            Self::NoMatchingRule { word_len } => {
                write!(f, "词长 {word_len} 无匹配的编码规则")
            }
            Self::BadFormula { formula } => write!(f, "编码公式非法: {formula:?}"),
            Self::CharIndexOutOfRange {
                char_index,
                word_len,
            } => write!(f, "公式引用第 {char_index} 字，超出词长 {word_len}"),
            Self::MissingCode { ch } => write!(f, "码表中查不到「{ch}」的编码"),
            Self::CodeTooShort { ch, code, need } => write!(
                f,
                "「{ch}」的全码 {code:?} 只有 {} 位，公式需要第 {} 位",
                code.chars().count(),
                need + 1
            ),
        }
    }
}

/// 解析编码公式为步骤列表。每两个字符一组：大写=字序（`A`=0…`Y`=24，`Z`=-1 表末字），
/// 小写=码序（`a`=0）。长度为奇数或含非法字符返回 `None`。
fn parse_formula(formula: &str) -> Option<Vec<FormulaStep>> {
    let bytes = formula.as_bytes();
    // 公式恒为 ASCII 字母；非 ASCII 直接判非法，避免按字节切进多字节字符中间。
    if !formula.is_ascii() || bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut steps = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let (upper, lower) = (pair[0], pair[1]);
        if !upper.is_ascii_uppercase() || !lower.is_ascii_lowercase() {
            return None;
        }
        steps.push(FormulaStep {
            // 'Z' 是末字的专用记号，不参与 A=0 的顺序编号。
            char_index: if upper == b'Z' {
                -1
            } else {
                (upper - b'A') as i32
            },
            code_index: (lower - b'a') as usize,
        });
    }
    Some(steps)
}

/// 为词长挑规则：先找 `length_equal` 精确匹配，再找 `length_in_range` 区间匹配
/// （对齐 Go `MatchRule` 的两轮顺序——精确规则优先于区间规则，与书写顺序无关）。
fn match_rule(rules: &[EncoderRule], word_len: usize) -> Option<&EncoderRule> {
    rules
        .iter()
        .find(|r| r.length_equal != 0 && r.length_equal == word_len)
        .or_else(|| {
            rules.iter().find(|r| {
                matches!(r.length_in_range.as_slice(), [min, max]
                    if word_len >= *min && word_len <= *max)
            })
        })
}

/// 按方案编码规则计算词组编码。
///
/// `code_of` 提供单字**全码**（见 `EngineManager::single_char_full_codes` 的全码判据：
/// 码长上限闸 → 最长码长 → 权重降序 → 首次出现）。任一字取不到码即**整词作废**，
/// 不做「跳过该字」的降级——那会把「你X好」算成「你好」的码，静默产出错词。
pub fn calc_word_code<F>(word: &str, spec: &EncoderSpec, code_of: F) -> Result<String, EncodeError>
where
    F: Fn(char) -> Option<String>,
{
    let chars: Vec<char> = word.chars().collect();
    if chars.len() < 2 {
        return Err(EncodeError::TooShort);
    }
    if spec.rules.is_empty() {
        return Err(EncodeError::NoRules);
    }
    let rule = match_rule(&spec.rules, chars.len()).ok_or(EncodeError::NoMatchingRule {
        word_len: chars.len(),
    })?;
    let steps = parse_formula(&rule.formula).ok_or_else(|| EncodeError::BadFormula {
        formula: rule.formula.clone(),
    })?;

    // 先按字缓存全码：同一个字在公式里常被取多次（如 `AaAb` 取两次首字），避免重复查表。
    let mut cache: Vec<(char, String)> = Vec::with_capacity(chars.len());
    let mut out = String::with_capacity(steps.len());
    for step in &steps {
        let ch = if step.char_index < 0 {
            chars[chars.len() - 1]
        } else {
            let i = step.char_index as usize;
            if i >= chars.len() {
                return Err(EncodeError::CharIndexOutOfRange {
                    char_index: step.char_index,
                    word_len: chars.len(),
                });
            }
            chars[i]
        };
        let code = match cache.iter().find(|(c, _)| *c == ch) {
            Some((_, code)) => code.clone(),
            None => {
                let code = code_of(ch).ok_or(EncodeError::MissingCode { ch })?;
                cache.push((ch, code.clone()));
                code
            }
        };
        // 码是 ASCII 键位串，但仍按 char 取以防方案用了非 ASCII 码位。
        let piece = code
            .chars()
            .nth(step.code_index)
            .ok_or_else(|| EncodeError::CodeTooShort {
                ch,
                code: code.clone(),
                need: step.code_index,
            })?;
        out.push(piece);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// 五笔 86 标准三条规则（与 `data/schemas/wubi86.schema.toml` 一致）。
    fn wubi_spec() -> EncoderSpec {
        EncoderSpec {
            max_word_length: 10,
            exclude_patterns: Vec::new(),
            rules: vec![
                EncoderRule {
                    length_equal: 2,
                    length_in_range: Vec::new(),
                    formula: "AaAbBaBb".into(),
                },
                EncoderRule {
                    length_equal: 3,
                    length_in_range: Vec::new(),
                    formula: "AaBaCaCb".into(),
                },
                EncoderRule {
                    length_equal: 0,
                    length_in_range: vec![4, 10],
                    formula: "AaBaCaZa".into(),
                },
            ],
        }
    }

    fn codes() -> HashMap<char, String> {
        // 取真实五笔全码，便于人工核对。
        [
            ('你', "wqiy"),
            ('好', "vbg"),
            ('我', "trnt"),
            ('中', "khk"),
            ('国', "lgyi"),
            ('人', "wwww"),
            ('民', "nav"),
            ('共', "awu"),
            ('和', "tkg"),
        ]
        .into_iter()
        .map(|(c, s)| (c, s.to_string()))
        .collect()
    }

    fn lookup(m: &HashMap<char, String>) -> impl Fn(char) -> Option<String> + '_ {
        move |c| m.get(&c).cloned()
    }

    #[test]
    fn parses_formula_pairs() {
        let steps = parse_formula("AaAbBaBb").unwrap();
        assert_eq!(
            steps,
            vec![
                FormulaStep {
                    char_index: 0,
                    code_index: 0
                },
                FormulaStep {
                    char_index: 0,
                    code_index: 1
                },
                FormulaStep {
                    char_index: 1,
                    code_index: 0
                },
                FormulaStep {
                    char_index: 1,
                    code_index: 1
                },
            ]
        );
    }

    /// `Z` 是末字记号，不能被当成 A=0 序列里的第 26 个字。
    #[test]
    fn z_means_last_char() {
        let steps = parse_formula("Za").unwrap();
        assert_eq!(steps[0].char_index, -1);
    }

    #[test]
    fn rejects_malformed_formula() {
        assert!(parse_formula("Aa B").is_none(), "含空格应判非法");
        assert!(parse_formula("AaB").is_none(), "奇数长度应判非法");
        assert!(parse_formula("aA").is_none(), "大小写颠倒应判非法");
        assert!(parse_formula("").is_none(), "空公式应判非法");
        assert!(
            parse_formula("A字").is_none(),
            "非 ASCII 应判非法且不 panic"
        );
    }

    /// 二字词：各取前两码。你(wqiy)+好(vbg) → wq+vb = wqvb。
    #[test]
    fn two_char_word_takes_first_two_codes_each() {
        let m = codes();
        let code = calc_word_code("你好", &wubi_spec(), lookup(&m)).unwrap();
        assert_eq!(code, "wqvb");
    }

    /// 三字词：前两字首码 + 末字前两码。中(k)+国(l)+人(ww) → klww。
    #[test]
    fn three_char_word_uses_third_rule() {
        let m = codes();
        let code = calc_word_code("中国人", &wubi_spec(), lookup(&m)).unwrap();
        assert_eq!(code, "klww");
    }

    /// 四字：前三字首码 + 末字首码。中(k)+国(l)+人(w)+民(n) → klwn。
    #[test]
    fn four_char_word_uses_range_rule() {
        let m = codes();
        let code = calc_word_code("中国人民", &wubi_spec(), lookup(&m)).unwrap();
        assert_eq!(code, "klwn");
    }

    /// 五字及以上：`Za` 取的是**末字**，中间字全部跳过——中(k)+国(l)+人(w)+末字和(t)。
    /// 这条专门盯住 `Z` 的语义：若把 `Z` 当成 A=0 序列的第 26 个字，此处会越界报错而非取到「和」。
    #[test]
    fn five_char_word_skips_middle_and_takes_last() {
        let m = codes();
        let code = calc_word_code("中国人民和", &wubi_spec(), lookup(&m)).unwrap();
        assert_eq!(code, "klwt");
    }

    /// 缺码整词作废，且错误携带具体是哪个字——这是排查造词不生效的关键线索。
    #[test]
    fn missing_code_fails_whole_word_with_char() {
        let m = codes();
        let err = calc_word_code("你囧好", &wubi_spec(), lookup(&m)).unwrap_err();
        assert_eq!(err, EncodeError::MissingCode { ch: '囧' });
    }

    /// 公式要第 2 码但该字只有 1 位码（简码被误当全码时的典型症状）→ 明确报错，不静默截断。
    #[test]
    fn code_shorter_than_formula_needs_reports_which_char() {
        let mut m = codes();
        m.insert('好', "v".into()); // 模拟错取了简码
        let err = calc_word_code("你好", &wubi_spec(), lookup(&m)).unwrap_err();
        assert_eq!(
            err,
            EncodeError::CodeTooShort {
                ch: '好',
                code: "v".into(),
                need: 1,
            }
        );
    }

    #[test]
    fn rejects_single_char_and_missing_rules() {
        let m = codes();
        assert_eq!(
            calc_word_code("你", &wubi_spec(), lookup(&m)).unwrap_err(),
            EncodeError::TooShort
        );
        let empty = EncoderSpec::default();
        assert_eq!(
            calc_word_code("你好", &empty, lookup(&m)).unwrap_err(),
            EncodeError::NoRules
        );
    }

    /// 精确规则优先于区间规则，与书写顺序无关（区间写在前也不应抢走 length_equal 的匹配）。
    #[test]
    fn exact_rule_wins_over_range_regardless_of_order() {
        let spec = EncoderSpec {
            max_word_length: 10,
            exclude_patterns: Vec::new(),
            rules: vec![
                EncoderRule {
                    length_equal: 0,
                    length_in_range: vec![2, 10],
                    formula: "AaBa".into(),
                },
                EncoderRule {
                    length_equal: 2,
                    length_in_range: Vec::new(),
                    formula: "AaAbBaBb".into(),
                },
            ],
        };
        let m = codes();
        assert_eq!(
            calc_word_code("你好", &spec, lookup(&m)).unwrap(),
            "wqvb",
            "词长 2 应命中 length_equal 规则而非区间规则"
        );
    }

    /// 词长超出所有规则覆盖范围 → 明确报 NoMatchingRule，不静默返回空串。
    #[test]
    fn word_longer_than_all_rules_reports_no_matching_rule() {
        let m = codes();
        let spec = EncoderSpec {
            max_word_length: 10,
            exclude_patterns: Vec::new(),
            rules: vec![EncoderRule {
                length_equal: 2,
                length_in_range: Vec::new(),
                formula: "AaAbBaBb".into(),
            }],
        };
        assert_eq!(
            calc_word_code("中国人", &spec, lookup(&m)).unwrap_err(),
            EncodeError::NoMatchingRule { word_len: 3 }
        );
    }
}
