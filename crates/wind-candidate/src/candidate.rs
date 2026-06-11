//! 候选词数据类型
//!
//! 与 Go 版本 `wind_input/internal/candidate/candidate.go` 对齐。

use serde::{Deserialize, Serialize};

/// 候选词来源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CandidateSource {
    #[serde(rename = "")]
    None,
    #[serde(rename = "codetable")]
    CodeTable,
    #[serde(rename = "pinyin")]
    Pinyin,
    #[serde(rename = "english")]
    English,
    #[serde(rename = "phrase")]
    Phrase,
}

impl Default for CandidateSource {
    fn default() -> Self {
        Self::None
    }
}

/// 候选词元数据
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CandidateMeta {
    pub lexicon_name: String,
    pub is_user_dict: bool,
    pub is_temp_dict: bool,
    pub raw_weight: i32,
    pub freq_boost: i32,
}

/// 命令栏动作
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    /// 文本插入
    Text,
    /// 副作用（不插入文本）
    Effect,
}

/// 候选词
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub text: String,
    pub pinyin: String,
    pub code: String,
    pub weight: i32,
    pub natural_order: i32,
    pub comment: String,
    pub is_common: bool,
    pub is_phrase: bool,
    pub is_command: bool,
    pub consumed_length: usize,
    pub source: CandidateSource,
    pub phrase_template: String,
    pub is_group: bool,
    pub is_group_member: bool,
    pub group_code: String,
    pub group_name: String,
    pub group_template: String,
    pub index: usize,
    pub has_shadow: bool,
    pub index_label: String,
    pub meta: CandidateMeta,
    pub id: String,
    pub display_text: String,
    pub actions: Vec<Action>,
}

impl Default for Candidate {
    fn default() -> Self {
        Self {
            text: String::new(),
            pinyin: String::new(),
            code: String::new(),
            weight: 0,
            natural_order: 0,
            comment: String::new(),
            is_common: false,
            is_phrase: false,
            is_command: false,
            consumed_length: 0,
            source: CandidateSource::None,
            phrase_template: String::new(),
            is_group: false,
            is_group_member: false,
            group_code: String::new(),
            group_name: String::new(),
            group_template: String::new(),
            index: 0,
            has_shadow: false,
            index_label: String::new(),
            meta: CandidateMeta::default(),
            id: String::new(),
            display_text: String::new(),
            actions: Vec::new(),
        }
    }
}

/// 比较两个候选词的排序优先级（权重降序）
///
/// 与 Go 版本 `candidate.Better` 对齐。
pub fn better(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    a.weight
        .cmp(&b.weight)
        .reverse()
        .then(a.natural_order.cmp(&b.natural_order))
        .then(a.code.cmp(&b.code))
        .then(a.consumed_length.cmp(&b.consumed_length).reverse())
        .then(a.text.cmp(&b.text))
}

/// 比较两个候选词的自然排序优先级（精确匹配优先）
///
/// 与 Go 版本 `candidate.BetterNatural` 对齐。
pub fn better_natural(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    let a_exact = a.weight >= 0;
    let b_exact = b.weight >= 0;
    match (a_exact, b_exact) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a
            .natural_order
            .cmp(&b.natural_order)
            .then_with(|| better(a, b)),
    }
}

/// 排序候选词列表（权重降序）
pub fn sort_candidates(candidates: &mut [Candidate]) {
    candidates.sort_by(better);
}

/// 排序候选词列表（自然顺序，精确匹配优先）
pub fn sort_candidates_natural(candidates: &mut [Candidate]) {
    candidates.sort_by(better_natural);
}
