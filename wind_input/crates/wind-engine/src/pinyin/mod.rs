//! 拼音输入引擎
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/` 对齐。
//!
//! 候选生成流程（对齐 Go convertCore）：
//! 1. 精确查找（完整音节 join 无空格）
//! 2. Viterbi 长句解码（>=2 音节）
//! 3. DAG 子短语查找
//! 4. 前缀查找
//! 5. 缩写/简拼匹配
//!
//! 注意：运行时词频 boost 由上层（协调器）应用，本引擎只产出基础权重候选。

pub mod dag;
pub mod fuzzy;
pub mod generate;
pub mod lattice;
pub mod lm;
pub mod parser;
pub mod scorer;
pub mod shuangpin;
pub mod syllable;
pub mod viterbi;

use crate::engine::{ConvertResult, Engine, EngineType};
use dag::Dag;
use fuzzy::FuzzyConfig;
use generate::CharPinyinIndex;
use lattice::LatticeBuilder;
use lm::UnigramLookup;
use scorer::AbbrevMatcher;
use shuangpin::ShuangpinConverter;
use std::sync::{Arc, OnceLock};
use syllable::SyllableTrie;
use viterbi::{ViterbiDecoder, WordNode};
use wind_candidate::{Candidate, CandidateSource};
use wind_dict::DictManager;
use wind_dict::cached::CachedDict;

/// 整句候选权重基准（高于拼音词频上限 ~19260817，确保整句置顶且不被截断）
const SENTENCE_WEIGHT_BASE: i32 = 30_000_000;

/// 拼音引擎配置
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub show_code_hint: bool,
    pub filter_mode: String,
    pub use_smart_compose: bool,
    pub candidate_order: String,
}

/// 拼音引擎
pub struct PinyinEngine {
    /// 引擎配置（show_code_hint / filter_mode 等，后续阶段接入）
    #[allow(dead_code)]
    config: Config,
    dict: CachedDict,
    trie: SyllableTrie,
    viterbi: ViterbiDecoder,
    lattice_builder: LatticeBuilder,
    fuzzy_config: FuzzyConfig,
    /// Unigram 语言模型（长句 Viterbi 打分；缺失时回退词典权重）
    unigram: Option<Arc<dyn UnigramLookup>>,
    /// 用户/临时造词层（L 造词显现）：仅含 StoreUserLayer/StoreTempLayer，无系统层。
    /// 拼音候选除主词典外，按相同的码并入这些层的用户造词（None=无持久化，如纯测试）。
    store_layers: Option<Arc<DictManager>>,
    /// 造词反推用的单字读音索引（懒构建：首次 generate_word_pinyin 时从词典派生）。
    char_pinyin_idx: OnceLock<CharPinyinIndex>,
    /// 双拼转换器（None 表示全拼模式，输入原样传递）。
    shuangpin: Option<ShuangpinConverter>,
}

impl PinyinEngine {
    pub fn new(config: Config, dict: CachedDict) -> Self {
        Self::with_unigram(config, dict, None)
    }

    pub fn with_unigram(
        config: Config,
        dict: CachedDict,
        unigram: Option<Arc<dyn UnigramLookup>>,
    ) -> Self {
        Self {
            config,
            dict,
            trie: SyllableTrie::new(),
            viterbi: ViterbiDecoder::new(),
            lattice_builder: LatticeBuilder::new(),
            fuzzy_config: FuzzyConfig::default(),
            unigram,
            store_layers: None,
            char_pinyin_idx: OnceLock::new(),
            shuangpin: None,
        }
    }

    /// 注入用户/临时造词层（L 造词显现）。链式 builder：构造后由 EngineManager 按 schema 挂上。
    pub fn with_store_layers(mut self, layers: Arc<DictManager>) -> Self {
        self.store_layers = Some(layers);
        self
    }

    /// 注入模糊音配置（取代 with_unigram 中的 FuzzyConfig::default()）。
    pub fn with_fuzzy(mut self, fuzzy: FuzzyConfig) -> Self {
        self.fuzzy_config = fuzzy;
        self
    }

    /// 注入双拼转换器（链式 builder）。注入后 convert/compute_composition 均先把输入转为全拼。
    pub fn with_shuangpin(mut self, conv: ShuangpinConverter) -> Self {
        self.shuangpin = Some(conv);
        self
    }

    /// 把外部输入（可能是双拼键序列）规整为全拼串。
    /// 无双拼方案（None）时原样返回；转换结果为空串时也回退原 input。
    fn to_full_pinyin(&self, input: &str) -> String {
        match &self.shuangpin {
            Some(conv) => {
                let s = conv.convert(input).full_pinyin;
                if s.is_empty() { input.to_string() } else { s }
            }
            None => input.to_string(),
        }
    }

    /// 仅测试用：读取 fuzzy_config.zh_z 以验证 with_fuzzy 注入是否生效。
    #[cfg(test)]
    pub fn fuzzy_zh_z(&self) -> bool {
        self.fuzzy_config.zh_z
    }

    /// 总条目数
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }

    /// 计算 preedit 显示与音节信息
    fn compute_composition(&self, input: &str) -> (String, Vec<String>, String) {
        let full = self.to_full_pinyin(input);
        let input = full.as_str();
        let dag = Dag::build(input, &self.trie);
        let syllables = dag.maximum_match();
        let consumed: usize = syllables.iter().map(|s| s.len()).sum();
        let partial = if consumed < input.len() {
            input[consumed..].to_string()
        } else {
            String::new()
        };

        let mut preedit = syllables.join(" ");
        if !partial.is_empty() {
            if !preedit.is_empty() {
                preedit.push(' ');
            }
            preedit.push_str(&partial);
        }
        if preedit.is_empty() {
            preedit = input.to_string();
        }
        (preedit, syllables, partial)
    }
}

impl Engine for PinyinEngine {
    fn convert(&self, input: &str, max_candidates: usize) -> anyhow::Result<ConvertResult> {
        if input.is_empty() {
            return Ok(ConvertResult::default());
        }

        let full = self.to_full_pinyin(input);
        let input = full.as_str();

        let dict = &self.dict;
        let trie = &self.trie;
        let mut candidates: Vec<Candidate> = Vec::new();

        let push_unique =
            |cands: &mut Vec<Candidate>, text: String, code: String, weight: i32, order: i32| {
                if text.is_empty() || cands.iter().any(|c| c.text == text) {
                    return;
                }
                cands.push(Candidate {
                    text,
                    code,
                    weight,
                    natural_order: order,
                    source: CandidateSource::Pinyin,
                    ..Default::default()
                });
            };

        // 1. 精确查找（完整匹配）
        for (text, weight, order) in dict.search(input) {
            push_unique(&mut candidates, text, input.to_string(), weight, order);
        }

        let dag = Dag::build(input, trie);
        let syllables = dag.maximum_match();

        // 完成音节覆盖的连续前缀（从起点算）。尾部不成音节的残码（如「nihaom」的「m」）
        // 不参与整句解码——否则 lattice 到不了残码末端、Viterbi 失败、整句退化成单字（bug①）。
        let completed_len: usize = syllables.iter().map(|s| s.len()).sum();
        let completed: &str = &input[..completed_len];

        // 2. Viterbi 长句解码（>=2 音节，仅在完成音节前缀上跑）
        if syllables.len() >= 2 {
            let lattice_nodes = self.lattice_builder.build(
                completed,
                trie,
                dict,
                Some(&self.fuzzy_config),
                self.unigram.as_deref(),
            );
            let input_len = completed.len();
            let mut lattice: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];
            for (end_pos, nodes_at_end) in lattice_nodes.iter().enumerate() {
                if end_pos > input_len {
                    continue;
                }
                for node in nodes_at_end {
                    lattice[end_pos].push(WordNode {
                        start: node.start,
                        end: node.end,
                        word: node.word.clone(),
                        log_prob: node.log_prob,
                    });
                }
            }
            let result = self.viterbi.decode(&lattice, input_len);
            // 仅接受有限概率的完整路径：解码失败时 log_prob 为 NEG_INFINITY，
            // 不能把这种空/错误路径强插到首选位置。
            if !result.words.is_empty() && result.log_prob.is_finite() {
                let sentence: String = result.words.join("");
                if !sentence.is_empty() {
                    // 整句优先：给予高权重置顶（log_prob 为负，原 .max(1) 会被截断淘汰）。
                    // clamp + saturating_add 防止超长低频句的 log_prob 溢出 i32 导致沉底/panic。
                    let log_offset = (result.log_prob * 1000.0)
                        .clamp(-(SENTENCE_WEIGHT_BASE as f64), 0.0)
                        as i32;
                    let weight = SENTENCE_WEIGHT_BASE.saturating_add(log_offset);
                    if let Some(existing) = candidates.iter_mut().find(|c| c.text == sentence) {
                        // 整句与已有候选（如精确匹配 你好）同文：提升其权重置顶，
                        // 否则单字（如 你）会因词频更高反超整句词。
                        existing.weight = existing.weight.max(weight);
                    } else {
                        candidates.insert(
                            0,
                            Candidate {
                                text: sentence,
                                // 码为完成音节前缀（不含残码），使 consumed_length=completed_len，
                                // 整句上屏后残码留缓冲续输（你好m → 选你好留 m）。
                                code: completed.to_string(),
                                weight,
                                natural_order: 0,
                                source: CandidateSource::Pinyin,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }

        // 3. DAG 前缀子短语查找（仅 start==0）：给出输入的各级前缀词，供从左到右分段上屏
        //    （「nihao」→「你」「你好」）。不取中段/后段子串（如「hao」→「好」）——非前缀词
        //    应在前缀提交后、剩余拼音重转时才出现，否则会污染整串候选并破坏分段语义。
        if syllables.len() >= 2 {
            for end in 1..syllables.len().min(6) {
                let code: String = syllables[..end].join("");
                if code == input {
                    continue;
                }
                for (text, weight, order) in dict.search(&code) {
                    push_unique(&mut candidates, text, code.clone(), weight, order);
                }
            }
        }

        // 4. 前缀查找
        for (code, text, weight, order) in dict.search_prefix(input, 30) {
            push_unique(&mut candidates, text, code, weight, order);
        }

        // 5. 缩写/简拼匹配
        if AbbrevMatcher::is_abbreviation(input, trie) {
            for abbrev in AbbrevMatcher::find_candidates(input, trie, dict, 10) {
                push_unique(
                    &mut candidates,
                    abbrev.text,
                    abbrev.code,
                    abbrev.weight,
                    999999,
                );
            }
        }

        // 6. 用户/临时造词层（L：让拼音造的词显现）。查询与主词典相同的码——整串精确 +
        //    各前缀子码（你好 coded「nihao」当输入「nihaoma」时部分消费）+ 前缀补全——
        //    并入候选（dedup by text，已在系统词典出现的不重复加）。weight 由 store 记录给出，
        //    随后统一按 weight 排序；词频上浮交协调器 apply_freq_rerank（衰减软置前，frequency.md §4）。
        if let Some(store_dm) = &self.store_layers {
            let limit = max_candidates.max(50);
            let mut store_cands: Vec<Candidate> = store_dm.search(input, limit);
            if syllables.len() >= 2 {
                for end in 1..syllables.len().min(6) {
                    let code: String = syllables[..end].join("");
                    if code == input {
                        continue;
                    }
                    store_cands.extend(store_dm.search(&code, limit));
                }
            }
            store_cands.extend(store_dm.search_prefix(input, limit));
            for mut c in store_cands {
                if c.text.is_empty() || candidates.iter().any(|x| x.text == c.text) {
                    continue;
                }
                c.source = CandidateSource::Pinyin;
                candidates.push(c);
            }
        }

        // 引擎内部排序（按权重降序，自然顺序升序）
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(max_candidates);

        // 分段上屏所需：标注每个候选实际消费的输入字节数。
        // code 为 input 的前缀（如 "ni" ⊂ "nihao"）→ 只消费该前缀，选中后保留剩余拼音续转；
        // 否则（整句/前缀补全/非前缀子串）消费整串。0 表示未知（由调用方按整串处理）。
        for c in candidates.iter_mut() {
            c.consumed_length = if !c.code.is_empty() && input.starts_with(&c.code) {
                c.code.len()
            } else {
                input.len()
            };
        }

        let (preedit_display, completed_syllables, partial_syllable) =
            self.compute_composition(input);
        let has_partial = !partial_syllable.is_empty();
        let is_empty = candidates.is_empty();

        Ok(ConvertResult {
            candidates,
            preedit_display,
            completed_syllables,
            partial_syllable,
            has_partial,
            should_commit: false,
            commit_text: String::new(),
            is_empty,
            should_clear: false,
        })
    }

    fn reset(&self) {}

    fn engine_type(&self) -> EngineType {
        EngineType::Pinyin
    }

    /// 为词语生成全拼编码（多音字按词典权重消歧）。
    /// 单字读音索引按词典懒构建并缓存。含无读音字符时返回 `None`。
    fn generate_word_pinyin(&self, word: &str) -> Option<String> {
        let idx = self
            .char_pinyin_idx
            .get_or_init(|| CharPinyinIndex::build(&self.dict));
        generate::generate_word_pinyin(&self.dict, idx, word)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pinyin::shuangpin::{Layout, ShuangpinConverter};
    use wind_dict::codetable::CodetableDict;
    use wind_store::Store;

    fn empty_engine() -> PinyinEngine {
        PinyinEngine::new(
            Config::default(),
            CachedDict::Memory(CodetableDict::empty()),
        )
    }

    fn tmp_store(name: &str) -> Arc<Store> {
        let p = std::env::temp_dir().join(format!("wind_pinyin_{name}.redb"));
        let _ = std::fs::remove_file(&p);
        Arc::new(Store::open(&p).unwrap())
    }

    /// L 造词显现：挂上用户/临时层后，拼音造的词应进入候选（即便主词典为空）。
    #[test]
    fn store_layer_words_appear_in_candidates() {
        let store = tmp_store("layer_show");
        store.add_user_word("pinyin", "nihao", "你好", 500).unwrap();
        store
            .learn_temp_word("pinyin", "lanshou", "蓝瘦", 800, 0)
            .unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        dm.register_layer(Box::new(wind_dict::StoreTempLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));

        // 整串精确命中用户词
        let r = engine.convert("nihao", 20).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你好"),
            "用户造词「你好」应出现在拼音候选"
        );
        // 临时词同样显现
        let r2 = engine.convert("lanshou", 20).unwrap();
        let shou = r2.candidates.iter().find(|c| c.text == "蓝瘦");
        assert!(shou.is_some(), "临时造词「蓝瘦」应出现在拼音候选");
        assert_eq!(
            shou.unwrap().source,
            CandidateSource::Pinyin,
            "来源应标为拼音"
        );
    }

    /// 无 store 层时行为不变（不 panic、空词典无候选）。
    #[test]
    fn no_store_layer_is_inert() {
        let engine = empty_engine();
        let r = engine.convert("nihao", 20).unwrap();
        assert!(r.candidates.is_empty(), "空词典无 store 层应无候选");
    }

    /// 部分消费：用户词码是输入的前缀（nihao ⊂ nihaoma）→ consumed_length 标为前缀长度，
    /// 选中后保留剩余拼音续转（分段上屏）。
    #[test]
    fn store_word_prefix_marks_partial_consumption() {
        let store = tmp_store("layer_partial");
        store.add_user_word("pinyin", "nihao", "你好", 500).unwrap();
        let dm = DictManager::new();
        dm.register_layer(Box::new(wind_dict::StoreUserLayer::new(
            store.clone(),
            "pinyin",
        )));
        let engine = empty_engine().with_store_layers(Arc::new(dm));
        let r = engine.convert("nihaoma", 20).unwrap();
        let c = r.candidates.iter().find(|c| c.text == "你好");
        assert!(c.is_some(), "前缀用户词应作为分段候选出现");
        assert_eq!(
            c.unwrap().consumed_length,
            "nihao".len(),
            "应只消费前缀 nihao"
        );
    }

    /// Task 1.4 TDD：with_fuzzy builder 注入的配置应被引擎持有（探针验证）。
    #[test]
    fn engine_applies_fuzzy_config() {
        let dict = CachedDict::Memory(CodetableDict::empty());
        let mut fz = FuzzyConfig::default();
        fz.zh_z = true;
        let eng = PinyinEngine::new(Config::default(), dict).with_fuzzy(fz);
        assert!(eng.fuzzy_zh_z(), "with_fuzzy 注入的 zh_z=true 应被引擎持有");
    }

    /// Task 4.1 TDD Step 1：双拼端到端——装配小鹤双拼 converter 后，
    /// 输入双拼键 "ni" 应返回含「你」的候选。
    #[test]
    fn pinyin_engine_shuangpin_input() {
        // 构造含 "ni"->"你" 的最小词典
        let mut raw = CodetableDict::empty();
        raw.merge_single("ni".to_string(), "你".to_string(), 100, 0);
        let dict = CachedDict::Memory(raw);

        // 小鹤双拼：ni → ni（声母 n + 韵母 i=i，即全拼 "ni"，保持不变）
        let schema_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../data/schemas/shuangpin");
        let layout = Layout::from_toml(&schema_dir.join("xiaohe.toml"))
            .expect("加载小鹤布局失败");
        let conv = ShuangpinConverter::new(layout);

        let eng = PinyinEngine::new(Config::default(), dict).with_shuangpin(conv);
        let r = eng.convert("ni", 10).unwrap();
        assert!(
            r.candidates.iter().any(|c| c.text == "你"),
            "双拼输入 \"ni\" 经转换后应返回含「你」的候选，实际候选: {:?}",
            r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }
}
