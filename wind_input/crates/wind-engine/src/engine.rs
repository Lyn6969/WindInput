//! 引擎接口定义
//!
//! 与 Go 版本 `wind_input/internal/engine/engine.go` 对齐。

use wind_candidate::Candidate;

/// 引擎类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineType {
    Pinyin,
    CodeTable,
    Mixed,
}

/// 引擎转换结果
#[derive(Debug, Clone, Default)]
pub struct ConvertResult {
    /// 候选列表（已按引擎内部权重排序，未应用运行时词频 boost）
    pub candidates: Vec<Candidate>,
    /// 预编辑显示文本（拼音：含音节分隔；码表：原始编码）
    pub preedit_display: String,
    /// 已完成音节（拼音 UI 高亮用）
    pub completed_syllables: Vec<String>,
    /// 末尾未完成音节（拼音）
    pub partial_syllable: String,
    /// 是否存在未完成音节
    pub has_partial: bool,
    /// 是否应自动上屏（码表满码等）
    pub should_commit: bool,
    /// 自动上屏的文本
    pub commit_text: String,
    /// 是否为空码（有输入但无候选）
    pub is_empty: bool,
    /// 满码空码时是否应清空缓冲（码表 clear_on_empty_max）
    pub should_clear: bool,
}

/// 基础引擎接口
pub trait Engine: Send + Sync {
    /// 转换输入为候选词列表
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult>;

    /// 重置引擎状态
    fn reset(&self);

    /// 引擎类型
    fn engine_type(&self) -> EngineType;

    /// 顶码上屏：超过满码长时取前 N 码首选上屏，返回 (上屏文本, 剩余编码)。
    /// 默认不支持（拼音等）；码表/混输引擎按 schema 的 top_code_commit 实现。
    fn handle_top_code(&self, _input: &str) -> Option<(String, String)> {
        None
    }

    /// 为词语生成全拼编码（造词反推读音、多音字消歧）。
    /// 默认不支持（码表/五笔等返回 None）；拼音引擎按词典权重消歧。
    /// 用于加词页自动出码、词库导入。含无读音字符时返回 None。
    fn generate_word_pinyin(&self, _word: &str) -> Option<String> {
        None
    }
}

/// 扩展引擎接口（码表引擎特有）
pub trait ExtendedEngine: Engine {
    /// 获取最大编码长度
    fn max_code_length(&self) -> usize;

    /// 判断是否应自动上屏
    fn should_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String>;

    /// 处理空编码
    fn handle_empty_code(&self, input: &str) -> (bool, bool, String);
}
