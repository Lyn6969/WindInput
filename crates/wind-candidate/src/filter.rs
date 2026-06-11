//! 候选词过滤逻辑
//!
//! 与 Go 版本 `wind_input/internal/candidate/filter.go` 对齐。

use crate::candidate::Candidate;

/// 过滤模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    /// 不过滤
    Gb18030,
    /// 只保留常用词
    General,
    /// 智能过滤（同编码下有常用词则过滤非常用词）
    Smart,
}

impl FilterMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "gb18030" => Self::Gb18030,
            "general" => Self::General,
            "smart" | _ => Self::Smart,
        }
    }
}

/// 按模式过滤候选词
pub fn filter_candidates(candidates: Vec<Candidate>, mode: FilterMode) -> Vec<Candidate> {
    match mode {
        FilterMode::Gb18030 => candidates,
        FilterMode::General => filter_common_only(candidates),
        FilterMode::Smart => filter_smart(candidates),
    }
}

/// 只保留常用词、短语、命令、分组
fn filter_common_only(candidates: Vec<Candidate>) -> Vec<Candidate> {
    candidates
        .into_iter()
        .filter(|c| c.is_common || c.is_phrase || c.is_command || c.is_group)
        .collect()
}

/// 智能过滤：同编码下有常用词则过滤非常用词
fn filter_smart(candidates: Vec<Candidate>) -> Vec<Candidate> {
    use std::collections::HashMap;

    // 按 code 分组，检查每组是否有常用词
    let mut has_common: HashMap<String, bool> = HashMap::new();
    for c in &candidates {
        let entry = has_common.entry(c.code.clone()).or_insert(false);
        if c.is_common || c.is_phrase || c.is_command || c.is_group {
            *entry = true;
        }
    }

    candidates
        .into_iter()
        .filter(|c| {
            let common_exists = has_common.get(&c.code).copied().unwrap_or(false);
            // 如果该编码下存在常用词，只保留常用词；否则保留全部（孤儿编码）
            !common_exists || c.is_common || c.is_phrase || c.is_command || c.is_group
        })
        .collect()
}
