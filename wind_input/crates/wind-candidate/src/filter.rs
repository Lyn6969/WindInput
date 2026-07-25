//! 候选词过滤逻辑
//!
//! 与 Go 版本 `wind_input/internal/candidate/filter.go` 对齐。

use crate::candidate::{Candidate, CandidateSource};

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

    /// 配置值（`input.filter_mode`）。与 [`from_str`](Self::from_str) 成对：菜单切换要把
    /// 新模式写回配置（config 为单一源，见 `set_filter_mode`），故必须能反向取到配置字符串。
    pub fn as_config(&self) -> &'static str {
        match self {
            Self::Gb18030 => "gb18030",
            Self::General => "general",
            Self::Smart => "smart",
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

/// 智能过滤：同一来源+编码下有常用词则过滤非常用词
fn filter_smart(candidates: Vec<Candidate>) -> Vec<Candidate> {
    use std::collections::HashMap;

    // 按 (来源, code) 分组检查是否有常用词。**必须带来源**：混输下码表（五笔码）与拼音候选常
    // 共用同一 code 字符串（原始输入，如 "wang"），但属不同编码体系；若仅按 code 分组，常用的
    // 拼音候选会误使同 code 的生僻码表字（如 佢）被过滤，导致混输码表主方案与纯五笔表现不一致。
    // 按来源隔离后，码表候选只受同来源候选影响，混输码表与纯五笔一致。
    let mut has_common: HashMap<(CandidateSource, String), bool> = HashMap::new();
    for c in &candidates {
        let entry = has_common
            .entry((c.source, c.code.clone()))
            .or_insert(false);
        if c.is_common || c.is_phrase || c.is_command || c.is_group {
            *entry = true;
        }
    }

    candidates
        .into_iter()
        .filter(|c| {
            let common_exists = has_common
                .get(&(c.source, c.code.clone()))
                .copied()
                .unwrap_or(false);
            // 同来源同编码下存在常用词则只保留常用词；否则保留全部（孤儿编码）
            !common_exists || c.is_common || c.is_phrase || c.is_command || c.is_group
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(text: &str, code: &str, source: CandidateSource, is_common: bool) -> Candidate {
        Candidate {
            text: text.into(),
            code: code.into(),
            source,
            is_common,
            ..Default::default()
        }
    }

    #[test]
    fn smart_keeps_uncommon_codetable_when_common_pinyin_shares_code() {
        // 混输：码表生僻字 佢 与常用拼音 往/王 共用 code "wang"（不同来源）。
        // 智能过滤按来源隔离 → 佢 不被拼音的常用性挤掉（与纯五笔一致）。
        let out = filter_candidates(
            vec![
                cand("佢", "wang", CandidateSource::CodeTable, false),
                cand("往", "wang", CandidateSource::Pinyin, true),
                cand("王", "wang", CandidateSource::Pinyin, true),
            ],
            FilterMode::Smart,
        );
        assert!(
            out.iter().any(|c| c.text == "佢"),
            "混输下生僻码表字应保留（与纯五笔一致）"
        );
        assert!(out.iter().any(|c| c.text == "往"));
    }

    #[test]
    fn smart_filters_uncommon_within_same_source_and_code() {
        // 同来源同 code：有常用则滤非常用（原语义不变）。
        let out = filter_candidates(
            vec![
                cand("王", "wang", CandidateSource::Pinyin, true),
                cand("尪", "wang", CandidateSource::Pinyin, false),
            ],
            FilterMode::Smart,
        );
        assert!(out.iter().any(|c| c.text == "王"));
        assert!(
            !out.iter().any(|c| c.text == "尪"),
            "同来源同码有常用则滤非常用"
        );
    }

    #[test]
    fn filter_mode_config_round_trip() {
        // 菜单切换靠 as_config 写回配置、启动/热重载靠 from_str 读回；两者必须互逆，
        // 否则会出现「菜单选了全部字符、重启回到智能」这类静默回退。
        for m in [FilterMode::Smart, FilterMode::General, FilterMode::Gb18030] {
            assert_eq!(FilterMode::from_str(m.as_config()), m);
        }
        // 配置值集须与 wind-setting 的 select options 一致（smart/general/gb18030）。
        assert_eq!(FilterMode::Smart.as_config(), "smart");
        assert_eq!(FilterMode::General.as_config(), "general");
        assert_eq!(FilterMode::Gb18030.as_config(), "gb18030");
    }

    #[test]
    fn smart_keeps_orphan_uncommon_code() {
        // 孤儿编码（无常用同码）：保留全部（纯五笔生僻字场景）。
        let out = filter_candidates(
            vec![cand("佢", "wtn", CandidateSource::CodeTable, false)],
            FilterMode::Smart,
        );
        assert!(out.iter().any(|c| c.text == "佢"));
    }
}
