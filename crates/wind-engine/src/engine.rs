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
#[derive(Debug, Clone)]
pub struct ConvertResult {
    pub candidates: Vec<Candidate>,
    pub preedit_display: String,
    pub completed_syllables: Vec<String>,
    pub partial_syllable: String,
    pub has_partial: bool,
}

/// 基础引擎接口
pub trait Engine: Send + Sync {
    /// 转换输入为候选词列表
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult>;

    /// 重置引擎状态
    fn reset(&self);

    /// 引擎类型
    fn engine_type(&self) -> EngineType;
}

/// 扩展引擎接口（码表引擎特有）
pub trait ExtendedEngine: Engine {
    /// 获取最大编码长度
    fn max_code_length(&self) -> usize;

    /// 判断是否应自动上屏
    fn should_auto_commit(&self, input: &str, candidates: &[Candidate]) -> Option<String>;

    /// 处理空编码
    fn handle_empty_code(&self, input: &str) -> (bool, bool, String);

    /// 处理顶码
    fn handle_top_code(&self, input: &str) -> Option<(String, String)>;
}
