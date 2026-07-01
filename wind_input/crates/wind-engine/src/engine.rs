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
    /// 拼音音节拆分形态（供「混输高亮跟随」：高亮拼音候选时显示此拆分串，高亮码表/五笔
    /// 候选时显示原始码）。拼音引擎 = preedit_display；混输引擎 = 拼音子引擎的音节拆分
    /// （≥2 音节时，否则空）；码表/无拼音引擎 = 空串（恒原始码）。
    pub preedit_pinyin: String,
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

    /// 运行时启停某扩展词库（按 dict id），**无需重建引擎**：直接翻 composite 中对应
    /// 系统层的 enabled 标志。返回是否命中该层。默认不支持（拼音等返回 false）；
    /// 码表/混输按 `codetable-extra-<id>` 层翻标志。用于扩展词库热插拔。
    fn set_dict_enabled(&self, _dict_id: &str, _enabled: bool) -> bool {
        false
    }

    /// 最大编码长度（码表引擎返回其码长；拼音等无意义返回 0）。
    /// 供混输引擎的超长分支（pinyin_only_overflow）与顶码裁决判断输入是否溢出。
    fn max_code_length(&self) -> usize {
        0
    }

    /// `input` 是否存在精确（code==input）匹配（码表引擎实现；其余默认 false）。
    fn has_full_input_match(&self, _input: &str) -> bool {
        false
    }

    /// 是否存在比 `input` 更长的后继编码（码表引擎实现；其余默认 false）。
    fn has_longer_code(&self, _input: &str) -> bool {
        false
    }

    /// 前缀是否构成「合法拼音序列」（含残缺尾音节前缀，用于保护正在输入的拼音）。
    /// 拼音引擎实现（对齐 Go isPossiblePinyinSequence）；其余默认 false。
    fn is_possible_pinyin_sequence(&self, _prefix: &str) -> bool {
        false
    }

    /// 前缀是否「恰好由完整拼音音节构成」（切在音节边界、无残缺尾音节）。
    /// 拼音引擎实现（对齐 Go isWholeSyllablePinyin）；其余默认 false。
    fn is_whole_syllable_pinyin(&self, _prefix: &str) -> bool {
        false
    }

    /// 前缀的连续完整音节解析中是否存在「非首位单字母音节」（a/e/o，退化解析特征）。
    /// 拼音引擎实现（对齐 Go hasNonInitialSingleLetterSyllable）；其余默认 false。
    fn has_non_initial_single_letter_syllable(&self, _prefix: &str) -> bool {
        false
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
