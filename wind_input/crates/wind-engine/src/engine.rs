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
    /// 英文引擎（复用码表查询，独立类型便于英文专属路由/演化）。
    English,
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
    /// （**拆分串与原始输入不同时**给出，含单音节 + 尾部残码如 `nun'l`）；码表/无拼音引擎 =
    /// 空串（恒原始码）。判据与 `preedit_display` 的「≥2 完成音节」刻意不同，见
    /// `MixedEngine::pinyin_split_of`。
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
    /// 精确匹配空码补全的**备选池**（码表 `single_code_input` + `single_code_complete`）：
    /// 从更长编码取的候选，按引擎序排好，**尚未**计入 `candidates`。
    ///
    /// 补全的语义是「一条候选都没有时的兜底」，而这个「没有」必须按**最终显示列表**判定。
    /// 引擎只看得见自己这一层，看不见协调器随后叠加的短语，就地判空会在「短语已有候选」时
    /// 多冒一条无关的后续编码；反过来，引擎抢先填非空又会把协调器的短语前缀补全误压制。
    /// 故引擎只备货，采纳与否由掌握最终列表的协调器统一收口（见 `build_candidates`）。
    ///
    /// ⚠️ 是**池**不是单条：协调器要在 shadow / 检索范围过滤**之后**才择一。只备一条的话，
    /// 用户把它隐藏掉就无货可补、屏幕全空——而词库里其实还有下一条。同「从池中择 N 条
    /// 必须发生在过滤之后」，见 `Engine::browse_display_limit` 的同款教训。
    pub completion_hints: Vec<Candidate>,
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

    /// 为词语生成**带空格的全拼音节码**（`你好` → `ni hao`；造词反推读音、多音字消歧）。
    /// 默认不支持（码表/五笔等返回 None）；拼音引擎按词典权重消歧。
    /// 用于加词页自动出码、词库导入。含无读音字符时返回 None。
    ///
    /// 空格即音节边界，与 rime 源词库同形。落库时由
    /// `wind_store::wdict::split_spaced_code` 拆成扁平 code + boundary
    /// （见 `wind_dict::binformat::DictEntry::boundary`）——造词本就逐音节拼接、边界白送，
    /// 带出来使用户自造词从诞生起即有边界，否则用户词是块边界空洞、双拼校验只能对其降级。
    fn generate_word_pinyin(&self, _word: &str) -> Option<String> {
        None
    }

    /// 反查某条已知 `(code, text)` 在词典里记录的音节边界；查不到或非拼音方案返回 0
    /// （= 无边界信息，消费方降级）。
    ///
    /// 与 [`Self::generate_word_pinyin`] 的区别是**不做推断**：那个从词反推读音、多音字
    /// 靠权重消歧，可能给出与目标条目不同的码；这里是拿现成的码去词典点查、取真值边界。
    /// 词频列表要显示音节格式，用的正是这条——词频记录只有 `(code, text)`，没有边界
    /// （词频表是唯一不带 boundary 的持久层）。
    fn syllable_boundary_of(&self, _code: &str, _text: &str) -> u64 {
        0
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

    /// 候选排序是否**忽略权重**（`[engine.codetable].base_sort = "natural"`）：码表引擎在 natural
    /// 模式下返回 true。供协调器合并短语后按**同一维度**重排——否则协调器仍以 weight 优先，会与
    /// 引擎的 `candidate::by_natural`（纯 base_order→natural_order、忽略权重）发散。其余引擎默认
    /// false（按权重排，对齐 `candidate::better`）。
    fn base_sort_ignores_weight(&self) -> bool {
        false
    }

    /// `input` 是否存在精确（code==input）匹配（码表引擎实现；其余默认 false）。
    fn has_full_input_match(&self, _input: &str) -> bool {
        false
    }

    /// 是否存在比 `input` 更长的后继编码（码表引擎实现；其余默认 false）。
    fn has_longer_code(&self, _input: &str) -> bool {
        false
    }

    /// 空码枚举：列出词典首 `limit` 条候选（按引擎内部序），供特殊模式「进入即展示」浏览。
    /// 码表引擎返回其码表首页（按 weight 降序）；拼音等无浏览语义的引擎返回空。
    ///
    /// ⚠️ **这里只取数、不施加呈现策略**。「精确匹配模式只展示一条」由
    /// [`Self::browse_display_limit`] 声明、**由调用方在过滤之后**施加——早年在此直接按
    /// `single_code_input` 截到 1 条，结果用户隐藏掉那一条后整屏空白（截断发生在候选调整
    /// 之前，池子里明明还有下一条）。取数与截断之间隔着过滤，两者不能揉在一处。
    fn enumerate(&self, _limit: usize) -> Vec<Candidate> {
        Vec::new()
    }

    /// 「进入即展示」浏览态的**呈现上限**：`Some(n)` = 过滤后最多显示 n 条；`None` = 不限。
    /// 码表引擎在精确匹配模式（`single_code_input`）下返回 `Some(1)`，语义与空码补全的
    /// 「取首位后续码」一致。调用方须在 shadow/过滤**之后**才施加它。
    fn browse_display_limit(&self) -> Option<usize> {
        None
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

    /// 前缀从起始连续解析出的完整拼音音节数（拼音引擎实现；其余默认 0）。
    /// 拼音词否决用：前缀恰 1 个完整音节（如 wang）多为「正在打拼音词的中途」→ 保护拼音；
    /// ≥2 音节（如 aipu=ai+pu）已是完整多音节单元 → 多为恰好像拼音的五笔码。
    fn completed_syllable_count(&self, _prefix: &str) -> usize {
        0
    }

    /// 满码自动上屏「显示态」复评（对齐 Go recheckAutoCommit）：给定已过滤/重排/shadow 的
    /// 显示候选，若满码上屏开、存在唯一精确全码码表候选且无更长后继 → 返回上屏文本。
    /// 引擎按未过滤候选判唯一时可能因生僻同码字被否决，智能过滤后据显示候选复评放行。
    /// 码表/混输引擎实现；其余默认 None。
    fn recheck_auto_commit(&self, _input: &str, _candidates: &[Candidate]) -> Option<String> {
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
