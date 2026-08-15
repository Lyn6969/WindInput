//! 拼音整句质量批量评测 harness（Phase 0）
//!
//! 这**不是回归门禁**，是测量工具：它不做 `assert_eq!`，只产出可比对的数字。
//! 单一 ground truth 带同音词噪声（`shiyan` 可能是 实验/试验/誓言），
//! 因此**绝对命中率意义有限，有用的是改动前后的 delta**。
//!
//! 运行（默认被 `#[ignore]` 排除在 `cargo test` 之外，因为它慢且依赖 build_dev 真实数据）：
//!
//! ```text
//! cargo test -p wind-engine --test pinyin_eval -- --ignored --nocapture
//! ```
//!
//! 指定输出路径（默认 `target/pinyin_eval/latest.json`）：
//!
//! ```text
//! WIND_PINYIN_EVAL_OUT=target/pinyin_eval/baseline.json \
//!   cargo test -p wind-engine --test pinyin_eval -- --ignored --nocapture
//! ```
//!
//! 可调环境变量：
//! - `WIND_PINYIN_EVAL_OUT`   结果 JSON 路径
//! - `WIND_PINYIN_EVAL_N`     每类抽样数（默认 600）
//! - `WIND_PINYIN_EVAL_SEED`  随机种子（默认 20260721）
//! - `WIND_PINYIN_EVAL_DUMP`  每类导出的未命中明细条数（默认 40）
//!
//! ## 样本分类判据
//!
//! 设某词条为 `词 \t 拼音(空格分隔) \t 权重`：
//! - `true_syls` = 拼音按空格切分 —— **音节边界真值**（来自 rime 源数据）
//! - `input`     = 拼音去空格 —— 用户实际敲入的串
//! - `mm`        = `Dag::maximum_match(input)` —— 当前生产切分
//!
//! 入池前置条件（不满足者整条丢弃，不计入任何类别）：
//! 汉字数 == `true_syls.len()`、`mm` 完整覆盖 `input`、纯 CJK、2~8 字。
//! 于是「汉字数 > mm 音节数」等价于 `true_syls.len() > mm.len()`。
//!
//! | 类 | 判据 | 含义 |
//! |---|---|---|
//! | **A** 普通词 | `mm == true_syls` | 切分与真值一致，边界校验天然通过 |
//! | **B** 缩合音短词 | `mm != true_syls` 且 `mm.len() == 1` | 整词塌缩进**单个**音节边（李安/西安/企鹅/余额）。这正是 §2.3 描述的「N 字占 1 音节跨度」畸形节点，它在单音节边上与高频单字竞争 |
//! | **C** 多音节含缩合音 | `mm != true_syls` 且 `mm.len() >= 2` | 词跨多个 mm 音节，但内部某处边界与真值不符（西安交通大学：真值 6 音节 vs mm 5 音节） |
//!
//! B / C 判据互斥且穷尽（在 `mm != true_syls` 前提下按 `mm.len()` 二分），**不存在重叠**。
//! 选 `mm.len()` 而非「是否含零声母字」作判据的理由：前者直接刻画**缺陷的结构形态**
//! ——「占据单音节边」才是「短词被误提升」的机制本身；后者只是该形态的常见成因。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use wind_config::Config;
use wind_engine::EngineManager;
use wind_engine::pinyin::dag::Dag;
use wind_engine::pinyin::syllable::SyllableTrie;

// ---------------------------------------------------------------- 基础设施

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("build_dev")
        .join("data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

fn manager(dir: &Path) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    // 语法模型权重（标定用）。默认 0 = 不加载模型，各项指标应与「没有这功能」时逐位相同。
    // 标定：`WIND_PINYIN_EVAL_GRAM_WEIGHT=0.2 cargo test ... -- --ignored --nocapture`
    // ⚠️ 需要 build_dev/data/schemas/pinyin/grammar/ 下有 .gram，否则引擎会
    // 降级为关闭并只在日志里 warn——**看到指标没变先确认模型真的加载了**。
    if let Some(w) = std::env::var("WIND_PINYIN_EVAL_GRAM_WEIGHT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        cfg.schema.pinyin.grammar.weight = w;
    }
    // 换模型：bgc（10MB 字级）/ bgw（41MB 词级）/ wanxiang-lts（420MB）的
    // 质量与开销差别都很大，标定时要能一键切换。
    if let Ok(m) = std::env::var("WIND_GRAM_MODEL") {
        cfg.schema.pinyin.grammar.model = m;
    }
    EngineManager::new(&cfg, Some(dir))
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// SplitMix64：固定种子可复现，避免引入 rand 依赖。
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Fisher-Yates 洗牌
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

// ---------------------------------------------------------------- 词库读取

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    A,
    B,
    C,
    D,
}

impl Class {
    fn key(self) -> &'static str {
        match self {
            Class::A => "A_normal",
            Class::B => "B_contracted_short",
            Class::C => "C_multi_syllable_contracted",
            Class::D => "D_mixed_abbrev_sentence",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Class::A => "A 普通词 (mm == 真值)",
            Class::B => "B 缩合音短词 (塌缩进单音节边)",
            Class::C => "C 多音节含缩合音 (跨多音节但边界不符)",
            Class::D => "D 简拼+全拼混合整句 (bzd+haobuhao)",
        }
    }

    /// 本类的第四列（表头「切分正确」）实际度量什么。
    ///
    /// A/B/C 度量首选候选的**音节切分**是否等于真值；D 类的击键串与真值音节不同域
    /// （`bzdhaobuhao` 里的 `bzd` 压根不是音节），那个判据无从成立，故改为度量
    /// **首选是否至少给对了第一个词**（见 `Score::partial_ok`）。
    fn col4_meaning(self) -> &'static str {
        match self {
            Class::D => "首词命中",
            _ => "切分正确",
        }
    }
}

struct Sample {
    text: String,
    input: String,
    true_syls: Vec<String>,
    mm: Vec<String>,
    weight: u64,
    unigram: u64,
    class: Class,
    /// D 类的组成词（`[W1, W2]`）。其余类为空。
    ///
    /// 用于 `partial_ok`：整句没出来时，至少要能确认「首选给对了 W1」——那是当前
    /// 分步上屏的行为，改动后不应下降。
    parts: Vec<String>,
}

fn is_cjk(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// 读 rime .dict.yaml 的数据段（`...` 之后），默认列序 `[text, code, weight]`。
fn read_dict(path: &Path, out: &mut Vec<(String, String, u64)>) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let mut in_data = false;
    for line in raw.lines() {
        if !in_data {
            if line.trim() == "..." {
                in_data = true;
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        let (Some(text), Some(code)) = (it.next(), it.next()) else {
            continue;
        };
        let weight = it
            .next()
            .and_then(|w| w.trim().parse::<u64>().ok())
            .unwrap_or(0);
        out.push((text.to_string(), code.to_string(), weight));
    }
}

/// 读取引擎实际加载的全部词库：`rime_frost.dict.yaml` 的 `import_tables` 声明。
/// 不硬编码文件名，避免与 schema 漂移。
fn read_all_dicts(dir: &Path) -> Vec<(String, String, u64)> {
    let root = dir.join("schemas/pinyin");
    let mut out = Vec::new();
    let Ok(raw) = std::fs::read_to_string(root.join("rime_frost.dict.yaml")) else {
        return out;
    };
    let mut in_imports = false;
    for line in raw.lines() {
        if line.starts_with("import_tables:") {
            in_imports = true;
            continue;
        }
        if in_imports {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("- ") {
                let name = rest.split('#').next().unwrap_or("").trim();
                if !name.is_empty() {
                    let p = root.join(format!("{}.dict.yaml", name));
                    if p.exists() {
                        read_dict(&p, &mut out);
                    }
                }
            } else if !t.is_empty() && !t.starts_with('#') {
                break; // 离开 import_tables 块
            }
        }
    }
    out
}

/// 词频表：就地从已读入的词库条目汇总，同一词被多库收录时**取最高频次**
/// （与 `gen_unigram` 的口径一致，理由见该工具头部：累加等于重复计数）。
///
/// 早前读 `schemas/pinyin/unigram.txt`。该文件已不再随 `data/` 分发——引擎的词图打分
/// 改用词条自身的词典权重，unigram 只剩 `gen_dict` 一个消费者、留在 `.cache/`。
/// 就地汇总既与那份产物同源，又免了「文件缺失 → 空表 → 抽样分层静默失效、指标照出」
/// 这种最难发现的退化。
fn collect_word_freqs(raw: &[(String, String, u64)]) -> HashMap<String, u64> {
    let mut m: HashMap<String, u64> = HashMap::new();
    for (text, _code, weight) in raw {
        m.entry(text.clone())
            .and_modify(|v| *v = (*v).max(*weight))
            .or_insert(*weight);
    }
    m
}

/// 词条 → 样本；返回 None 表示不入池。同时统计丢弃原因。
fn classify(
    text: &str,
    code: &str,
    weight: u64,
    trie: &SyllableTrie,
    reject: &mut HashMap<&'static str, usize>,
) -> Option<Sample> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 2 || chars.len() > 8 {
        *reject.entry("char_len_out_of_range").or_default() += 1;
        return None;
    }
    if !chars.iter().all(|&c| is_cjk(c)) {
        *reject.entry("non_cjk_text").or_default() += 1;
        return None;
    }

    let true_syls: Vec<String> = code.split_whitespace().map(|s| s.to_string()).collect();
    if true_syls.is_empty()
        || !true_syls
            .iter()
            .all(|s| s.bytes().all(|b| b.is_ascii_lowercase()))
    {
        *reject.entry("non_plain_code").or_default() += 1;
        return None;
    }
    // 一字一音节：不满足说明词条本身格式异常（或含儿化/多音节字），排除以免污染判据
    if chars.len() != true_syls.len() {
        *reject.entry("char_syllable_count_mismatch").or_default() += 1;
        return None;
    }

    let input: String = true_syls.concat();
    let mm = Dag::build(&input, trie).maximum_match();
    if mm.concat() != input {
        *reject.entry("not_fully_segmentable").or_default() += 1;
        return None;
    }

    let class = if mm == true_syls {
        Class::A
    } else if mm.len() == 1 {
        Class::B
    } else {
        Class::C
    };

    Some(Sample {
        text: text.to_string(),
        input,
        true_syls,
        mm,
        weight,
        unigram: 0,
        class,
        parts: Vec::new(),
    })
}

/// D 类样本：把 W1 的**简拼**与 W2 的**全拼**拼成一串击键，期望整句 = W1 + W2。
///
/// 度量的是「智能组句」——微软拼音打 `bzdhaobuhao` 直接出「不知道好不好」，而本引擎的
/// 简拼段**不进 lattice/Viterbi**（简拼走独立的 AbbrevSection 点查），切点选择只靠词频
/// 启发式，故整句 top-1 的基线预期接近 0。**立这条基线正是本类的目的**：
/// 没有它，把简拼节点接进词图之后无从判断改善了多少、也无从发现 A/C 类被拖累。
///
/// 入池条件（不满足即丢弃，逐条都是为了避免假样本）：
/// - W1 取 2..=4 音节、W2 取 2..=3 音节：控制击键串长度，也保证简拼有意义（单音节词
///   的「简拼」就是一个字母，那是裸声母场景、另有路径负责）。
/// - **整串击键不能被完整切成音节序列**。否则它会走纯全拼路径（`mixed_covered` 短路），
///   测的就不是简拼了——比如 W1=「哦哦」简拼 `oo` 恰好是合法音节序列 `o|o`。
/// - 击键串 ≤ 16 字节：与 `mixed_abbrev::MAX_INPUT_LEN` 对齐，超出则混合模式压根不枚举，
///   样本落在能力范围外、只会稀释指标。
/// - 两词不同且拼接后仍是纯 CJK。
fn build_mixed_samples(
    pool: &[Sample],
    n: usize,
    seed: u64,
    trie: &SyllableTrie,
    reject: &mut HashMap<&'static str, usize>,
) -> Vec<Sample> {
    let mut rng = Rng(seed ^ 0xD_1234);

    // **只用常用词配对。** 用户打简拼正是为了少敲**熟词**；全词库随机配对产出的是
    // 「达拉特旗」+「半道儿」这种没人会打的串，拿它调阈值会把参数调偏——被度量的场景
    // 与样本分布不匹配时，指标越精确越误导。
    //
    // 判据取词频前 1/3（须 `unigram > 0`）：整句解码就是靠这份词频打分，权重为 0 的词
    // 无论如何都拼不出整句，留在池里只会把指标压成噪声。
    let mut common: Vec<&Sample> = pool.iter().filter(|s| s.unigram > 0).collect();
    common.sort_by(|a, b| b.unigram.cmp(&a.unigram).then_with(|| a.text.cmp(&b.text)));
    common.truncate((common.len() / 3).max(1));

    // 候选池：按音节数分成「可作 W1」与「可作 W2」两组
    let w1: Vec<&Sample> = common
        .iter()
        .copied()
        .filter(|s| (2..=4).contains(&s.true_syls.len()))
        .collect();
    let w2: Vec<&Sample> = common
        .iter()
        .copied()
        .filter(|s| (2..=3).contains(&s.true_syls.len()))
        .collect();
    if w1.is_empty() || w2.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 尝试次数给足（多数组合会被入池条件筛掉），但设硬上限避免词库形态异常时空转。
    let max_attempts = n.saturating_mul(60).max(2000);
    for _ in 0..max_attempts {
        if out.len() >= n {
            break;
        }
        let a = w1[(rng.next_u64() % w1.len() as u64) as usize];
        let b = w2[(rng.next_u64() % w2.len() as u64) as usize];
        if a.text == b.text {
            continue;
        }
        // W1 → 简拼（各音节首字母）；W2 → 全拼码
        let abbrev: String = a
            .true_syls
            .iter()
            .filter_map(|s| s.chars().next())
            .collect();
        let stroke = format!("{abbrev}{}", b.input);
        if stroke.len() > 16 {
            *reject.entry("D_stroke_too_long").or_default() += 1;
            continue;
        }
        // 整串可完整切分 ⇒ 走全拼路径，测不到简拼
        if Dag::build(&stroke, trie).maximum_match().concat() == stroke {
            *reject.entry("D_stroke_is_full_pinyin").or_default() += 1;
            continue;
        }
        let text = format!("{}{}", a.text, b.text);
        if !seen.insert(stroke.clone()) {
            continue;
        }
        out.push(Sample {
            text,
            input: stroke,
            // D 类不度量切分（击键串与真值音节不同域），真值音节仅供明细展示
            true_syls: a
                .true_syls
                .iter()
                .chain(b.true_syls.iter())
                .cloned()
                .collect(),
            mm: Vec::new(),
            weight: a.weight.min(b.weight),
            unigram: a.unigram.min(b.unigram),
            class: Class::D,
            parts: vec![a.text.clone(), b.text.clone()],
        });
    }
    out
}

// ---------------------------------------------------------------- 抽样

/// 按词频分三层等量抽样，保证不全是低频词。
fn stratified_sample(mut pool: Vec<Sample>, n: usize, seed: u64) -> Vec<Sample> {
    if pool.len() <= n {
        return pool;
    }
    // 频次降序 → 三等分为 高/中/低 三层
    pool.sort_by(|a, b| {
        b.unigram
            .cmp(&a.unigram)
            .then_with(|| b.weight.cmp(&a.weight))
            .then_with(|| a.text.cmp(&b.text)) // 末级定序：消除 HashMap/并列带来的不确定性
    });

    let tier_size = pool.len().div_ceil(3);
    let per_tier = n.div_ceil(3);
    let mut out = Vec::with_capacity(n);
    let mut rng = Rng(seed);

    for (t, chunk) in pool.chunks(tier_size).enumerate() {
        let mut idx: Vec<usize> = (0..chunk.len()).collect();
        let mut r = Rng(seed ^ ((t as u64 + 1).wrapping_mul(0x2545_F491_4F6C_DD1D)));
        r.shuffle(&mut idx);
        for &i in idx.iter().take(per_tier) {
            out.push(Sample {
                text: chunk[i].text.clone(),
                input: chunk[i].input.clone(),
                true_syls: chunk[i].true_syls.clone(),
                mm: chunk[i].mm.clone(),
                weight: chunk[i].weight,
                unigram: chunk[i].unigram,
                class: chunk[i].class,
                parts: chunk[i].parts.clone(),
            });
        }
    }
    out.truncate(n);
    rng.shuffle(&mut out);
    out
}

// ---------------------------------------------------------------- 打分

const TOP_N: usize = 10;

struct Miss {
    input: String,
    expect: String,
    rank: Option<usize>,
    got_top1: String,
    unigram: u64,
    true_syls: String,
    mm: String,
}

#[derive(Default)]
struct Score {
    total: usize,
    top1: usize,
    top5: usize,
    mrr_sum: f64,
    /// 首选候选的**音节切分**是否等于真值（不看词对不对，只看切分对不对）。
    ///
    /// 为什么需要它：top-1 命中率被同音词噪声主导，而同音词之争（关隘/关爱、珍爱/真爱）
    /// 与切分能力无关——`guanai` 出「关爱」是切对了 `guan|ai` 只是选错同音词，
    /// 出「挂乃」才是切分失败。边界感知词图的职责恰恰只是后者，用 top-1 度量它会被
    /// 前者淹没。本指标把两者分开。
    ///
    /// 判据：首选候选覆盖整串输入（`code == input`）且其 `boundary` == 真值 mask。
    /// 候选无边界信息（boundary==0，如单字）时不算切分正确。
    seg_ok: usize,
    /// **仅 D 类**：整句没出来时，首选是否至少给对了第一个词（W1）。
    ///
    /// 它度量的是当前「分步上屏」的行为：`bzdhaobuhao` 出「不知道」（消费 3 键）后由用户
    /// 再选一次。把简拼接进 lattice 之后 `top1` 应上升，而**这一项不应下降**——否则说明
    /// 整句路径抢走了首选却没抢对。
    partial_ok: usize,
    /// **仅 D 类**：W1 出现在候选列表里（任意位次）。
    ///
    /// 与 `partial_ok` 配对使用，用来区分两种完全不同的失败：
    /// - `partial_recall` 低 ⇒ **召回本身没做到**（前缀回退没退到 W1 那个切点），是缺陷；
    /// - `partial_recall` 高而 `partial_ok` 低 ⇒ 召回到了但排不到首位，那是简拼固有的
    ///   同简拼歧义（`bzd` 下 12 个词按词频竞争，采样到的低频 W1 本就不该在首位）。
    ///
    /// 缺了这个区分，光看 `partial_ok` 会把后者误读成前者。
    partial_recall: usize,
    misses: Vec<Miss>,
    /// 切分不正确的样本明细（含失败**原因分类**，见 `SegMiss::reason`）
    seg_misses: Vec<SegMiss>,
}

struct SegMiss {
    input: String,
    expect: String,
    true_syls: String,
    mm: String,
    top_text: String,
    top_code: String,
    /// 首选候选 boundary 还原出的切分（`-` = 无边界信息）
    top_syls: String,
    /// 失败原因：
    /// - `not_full_span` 首选候选没覆盖整串输入（是子串/单字，不是"切错"）
    /// - `no_boundary`   首选覆盖整串但无边界信息（boundary==0）
    /// - `wrong_split`   首选覆盖整串且有边界，但切分与真值不符 ← **真正的切错**
    reason: &'static str,
}

impl Score {
    fn rate(hit: usize, total: usize) -> f64 {
        if total == 0 {
            0.0
        } else {
            hit as f64 / total as f64
        }
    }
}

/// 真值切分的 bitmask（各音节起始字节位），与 `Candidate::boundary` 同域。
fn true_mask(true_syls: &[String]) -> u64 {
    let mut m = 0u64;
    let mut pos = 0usize;
    for s in true_syls {
        if pos >= 64 {
            return 0;
        }
        m |= 1u64 << pos;
        pos += s.len();
    }
    m
}

/// 把 bitmask 还原成 `a|b|c` 形式的切分串。
fn decode_mask(code: &str, mask: u64) -> String {
    if mask == 0 {
        return "-".to_string();
    }
    let mut starts: Vec<usize> = (0..code.len().min(64))
        .filter(|i| (mask >> i) & 1 == 1)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    let mut out: Vec<&str> = Vec::new();
    for (i, &s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(code.len());
        if s <= e && e <= code.len() {
            out.push(&code[s..e]);
        }
    }
    out.join("|")
}

fn json_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

// ---------------------------------------------------------------- 主流程

#[test]
#[ignore = "批量评测：慢，且依赖 build_dev 真实词库。用 --ignored 显式运行"]
fn pinyin_eval_report() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };

    // 1000 是「够用又便宜」的折中：实测 A 类 1000 条打分约 1.1 s，整轮含建集 < 5 s。
    // B 类总体只有 80 条，会被全量取用（见报告里 sampled 列）。
    let n_per_class = env_usize("WIND_PINYIN_EVAL_N", 1000);
    let seed = std::env::var("WIND_PINYIN_EVAL_SEED")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20_260_721);
    let dump = env_usize("WIND_PINYIN_EVAL_DUMP", 40);
    // 注意：测试进程的 cwd 是 crate 目录而非 workspace 根，相对路径会落到
    // `crates/wind-engine/target/...` 这种意外位置。默认路径因此显式锚定到 workspace target。
    let out = match std::env::var("WIND_PINYIN_EVAL_OUT") {
        Ok(p) => PathBuf::from(p),
        Err(_) => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/pinyin_eval")
            .join("latest.json"),
    };

    // ---- 1. 生成评测集
    let t_gen = Instant::now();
    let raw = read_all_dicts(&dir);
    let unigram = collect_word_freqs(&raw);

    let trie = SyllableTrie::new();
    let mut reject: HashMap<&'static str, usize> = HashMap::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut pools: HashMap<&'static str, Vec<Sample>> = HashMap::new();
    let mut class_totals: HashMap<&'static str, usize> = HashMap::new();

    for (text, code, weight) in &raw {
        if let Some(mut s) = classify(text, code, *weight, &trie, &mut reject) {
            if !seen.insert((s.text.clone(), s.input.clone())) {
                continue;
            }
            s.unigram = unigram.get(&s.text).copied().unwrap_or(0);
            *class_totals.entry(s.class.key()).or_default() += 1;
            pools.entry(s.class.key()).or_default().push(s);
        }
    }
    // D 类是**组合样本**（两个词拼出来的），不是从单条词库记录 classify 而来，故在
    // A/B/C 建池之后单独构造。取材于 A 池：那些词的真值切分与最大匹配一致，简拼投影
    // 不会引入额外歧义，混合整句的失败因此可归因到「简拼段没进词图」而非切分本身。
    let d_pool = {
        let a_pool = pools
            .get(Class::A.key())
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        build_mixed_samples(a_pool, n_per_class, seed, &trie, &mut reject)
    };
    *class_totals.entry(Class::D.key()).or_default() += d_pool.len();
    pools.insert(Class::D.key(), d_pool);

    let gen_ms = t_gen.elapsed().as_millis();

    println!("\n=== 评测集生成 ({} ms) ===", gen_ms);
    println!("词库原始条目: {}", raw.len());
    for c in [Class::A, Class::B, Class::C, Class::D] {
        println!(
            "  {:<44} 总体 {:>7}",
            c.label(),
            class_totals.get(c.key()).copied().unwrap_or(0)
        );
    }
    let mut rj: Vec<_> = reject.iter().collect();
    rj.sort();
    println!("丢弃（不入池）: {:?}", rj);

    // ---- 2. 抽样
    let mut samples: Vec<(Class, Vec<Sample>)> = Vec::new();
    for c in [Class::A, Class::B, Class::C, Class::D] {
        let pool = pools.remove(c.key()).unwrap_or_default();
        samples.push((c, stratified_sample(pool, n_per_class, seed)));
    }

    // ---- 3. 打分（全程共享一次 EngineManager 初始化）
    let t_load = Instant::now();
    let mgr = manager(&dir);
    let load_ms = t_load.elapsed().as_millis();
    println!("\n引擎初始化: {} ms", load_ms);

    let t_run = Instant::now();
    let mut scores: Vec<(Class, Score, usize)> = Vec::new();
    for (class, set) in &samples {
        let t = Instant::now();
        let mut sc = Score::default();
        for s in set {
            sc.total += 1;
            let cands = mgr.convert_with("pinyin", &s.input, TOP_N).candidates;
            let rank = cands.iter().position(|c| c.text == s.text);
            let tm = true_mask(&s.true_syls);
            // D 类：击键串与真值音节不同域（`bzd` 不是音节），切分判据无从成立。
            // 第四列改度量「首选至少给对了 W1」——当前分步上屏的实际行为。
            if s.class == Class::D {
                if cands
                    .first()
                    .is_some_and(|t| Some(&t.text) == s.parts.first())
                {
                    sc.partial_ok += 1;
                }
                if s.parts
                    .first()
                    .is_some_and(|w1| cands.iter().any(|c| &c.text == w1))
                {
                    sc.partial_recall += 1;
                }
            }
            match cands.first() {
                Some(top) if top.code == s.input && top.boundary != 0 && top.boundary == tm => {
                    sc.seg_ok += 1;
                }
                top => {
                    let (top_text, top_code, top_syls, reason) = match top {
                        None => (
                            String::new(),
                            String::new(),
                            "-".to_string(),
                            "no_candidate",
                        ),
                        Some(t) if t.code != s.input => (
                            t.text.clone(),
                            t.code.clone(),
                            decode_mask(&t.code, t.boundary),
                            "not_full_span",
                        ),
                        Some(t) if t.boundary == 0 => (
                            t.text.clone(),
                            t.code.clone(),
                            "-".to_string(),
                            "no_boundary",
                        ),
                        Some(t) => (
                            t.text.clone(),
                            t.code.clone(),
                            decode_mask(&t.code, t.boundary),
                            "wrong_split",
                        ),
                    };
                    sc.seg_misses.push(SegMiss {
                        input: s.input.clone(),
                        expect: s.text.clone(),
                        true_syls: s.true_syls.join("|"),
                        mm: s.mm.join("|"),
                        top_text,
                        top_code,
                        top_syls,
                        reason,
                    });
                }
            }
            if let Some(r) = rank {
                if r == 0 {
                    sc.top1 += 1;
                }
                if r < 5 {
                    sc.top5 += 1;
                }
                sc.mrr_sum += 1.0 / (r as f64 + 1.0);
            }
            if rank != Some(0) {
                sc.misses.push(Miss {
                    input: s.input.clone(),
                    expect: s.text.clone(),
                    rank,
                    got_top1: cands.first().map(|c| c.text.clone()).unwrap_or_default(),
                    unigram: s.unigram,
                    true_syls: s.true_syls.join("|"),
                    mm: s.mm.join("|"),
                });
            }
        }
        let ms = t.elapsed().as_millis() as usize;
        scores.push((*class, sc, ms));
    }
    let run_ms = t_run.elapsed().as_millis();

    // ---- 4. 报告
    println!("\n=== 基线报告 (seed={}, top_n={}) ===", seed, TOP_N);
    println!(
        "{:<46} {:>6} {:>9} {:>9} {:>9} {:>9} {:>8}",
        "类别", "样本", "top-1", "top-5", "MRR", "切分正确*", "耗时ms"
    );
    for (c, sc, ms) in &scores {
        // 第四列对 A/B/C 是切分正确率、对 D 是首词命中率（见 Class::col4_meaning）
        let col4 = match c {
            Class::D => Score::rate(sc.partial_ok, sc.total),
            _ => Score::rate(sc.seg_ok, sc.total),
        };
        println!(
            "{:<46} {:>6} {:>8.2}% {:>8.2}% {:>9.4} {:>8.2}% {:>8}",
            c.label(),
            sc.total,
            Score::rate(sc.top1, sc.total) * 100.0,
            Score::rate(sc.top5, sc.total) * 100.0,
            if sc.total == 0 {
                0.0
            } else {
                sc.mrr_sum / sc.total as f64
            },
            col4 * 100.0,
            ms
        );
    }
    println!(
        "* 第四列：A/B/C = 切分正确；{} = {}（击键串与真值音节不同域，切分判据无从成立）",
        Class::D.key(),
        Class::D.col4_meaning()
    );
    println!("  D 类 top-1 度量「智能组句」：简拼段目前不进 lattice/Viterbi，基线预期接近 0。");
    if let Some((_, sc, _)) = scores.iter().find(|(c, _, _)| *c == Class::D) {
        println!(
            "  D 类首词：命中(首选) {:.2}% / 召回(任意位次) {:.2}% —— 两者的差是「简拼同音歧义」，\n  \
             只有召回率低才说明前缀回退本身没做到。",
            Score::rate(sc.partial_ok, sc.total) * 100.0,
            Score::rate(sc.partial_recall, sc.total) * 100.0
        );
    }
    println!("\n评测总耗时: {} ms（含引擎初始化 {} ms）", run_ms, load_ms);

    // 切分不正确的明细：按原因分桶。`wrong_split` 才是「多路径选错了切分」，
    // 其余两类（首选是子串 / 首选无边界信息）不构成切分错误，只是指标判据的副产物。
    for (c, sc, _) in &scores {
        let mut by_reason: HashMap<&'static str, usize> = HashMap::new();
        for m in &sc.seg_misses {
            *by_reason.entry(m.reason).or_default() += 1;
        }
        let mut v: Vec<_> = by_reason.iter().collect();
        v.sort();
        println!(
            "\n--- {} 切分不正确 {} 条，按原因: {:?} ---",
            c.label(),
            sc.seg_misses.len(),
            v
        );
        for m in sc
            .seg_misses
            .iter()
            .filter(|m| m.reason == "wrong_split")
            .take(dump)
        {
            println!(
                "  [切错] {:<24} 期望 {:<10} 真值 {:<28} 实选 {:<10} 切分 {:<28} mm {}",
                m.input, m.expect, m.true_syls, m.top_text, m.top_syls, m.mm
            );
        }
        for m in sc
            .seg_misses
            .iter()
            .filter(|m| m.reason != "wrong_split")
            .take(dump.min(15))
        {
            println!(
                "  [{}] {:<22} 期望 {:<10} 真值 {:<28} 实选 {:<10} code {}",
                m.reason, m.input, m.expect, m.true_syls, m.top_text, m.top_code
            );
        }
    }

    // C 类 top-1 未命中的样本：核对「首选是否切分正确」——这是
    // 「剩余失败已转为同音词竞争」这一论断的可验证依据。
    for (c, sc, _) in &scores {
        if *c != Class::C {
            continue;
        }
        let seg_bad: std::collections::HashSet<&str> =
            sc.seg_misses.iter().map(|m| m.input.as_str()).collect();
        let (mut same_seg, mut diff_seg) = (0usize, 0usize);
        for m in &sc.misses {
            if seg_bad.contains(m.input.as_str()) {
                diff_seg += 1;
            } else {
                same_seg += 1;
            }
        }
        println!(
            "\n=== C 类 top-1 未命中 {} 条：首选切分正确 {} 条（同音词竞争）/ 切分不正确 {} 条 ===",
            sc.misses.len(),
            same_seg,
            diff_seg
        );
        println!("--- 其中「切分正确、仅选错同音词」的样本（前 {}）---", dump);
        for m in sc
            .misses
            .iter()
            .filter(|m| !seg_bad.contains(m.input.as_str()))
            .take(dump)
        {
            println!(
                "  {:<24} 期望 {:<10} rank={:<5} 首选 {:<10} 真值 {}",
                m.input,
                m.expect,
                m.rank
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "miss".into()),
                m.got_top1,
                m.true_syls
            );
        }
    }

    for (c, sc, _) in &scores {
        println!("\n--- {} 非首选明细（前 {}）---", c.label(), dump);
        for m in sc.misses.iter().take(dump) {
            println!(
                "  {:<24} 期望 {:<10} rank={:<5} 首选 {:<10} uni={:<8} 真值 {} / mm {}",
                m.input,
                m.expect,
                m.rank
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "miss".into()),
                m.got_top1,
                m.unigram,
                m.true_syls,
                m.mm
            );
        }
    }

    // ---- 5. 机器可读输出
    let mut j = String::new();
    j.push_str("{\n");
    let _ = writeln!(
        j,
        "  \"seed\": {}, \"top_n\": {}, \"n_per_class\": {},",
        seed, TOP_N, n_per_class
    );
    let _ = writeln!(
        j,
        "  \"timing_ms\": {{ \"generate\": {}, \"engine_load\": {}, \"score\": {} }},",
        gen_ms, load_ms, run_ms
    );
    j.push_str("  \"classes\": {\n");
    for (i, (c, sc, ms)) in scores.iter().enumerate() {
        let _ = writeln!(
            j,
            "    \"{}\": {{ \"population\": {}, \"sampled\": {}, \"top1\": {:.6}, \"top5\": {:.6}, \"mrr\": {:.6}, \"seg_ok\": {:.6}, \"top1_hits\": {}, \"top5_hits\": {}, \"score_ms\": {},",
            c.key(),
            class_totals.get(c.key()).copied().unwrap_or(0),
            sc.total,
            Score::rate(sc.top1, sc.total),
            Score::rate(sc.top5, sc.total),
            if sc.total == 0 {
                0.0
            } else {
                sc.mrr_sum / sc.total as f64
            },
            Score::rate(sc.seg_ok, sc.total),
            sc.top1,
            sc.top5,
            ms
        );
        // D 类专有：首词命中率（seg_ok 对它恒为 0，不可用于对账）
        if *c == Class::D {
            let _ = writeln!(
                j,
                "      \"first_word_top1\": {:.6}, \"first_word_hits\": {}, \"first_word_recall\": {:.6},",
                Score::rate(sc.partial_ok, sc.total),
                sc.partial_ok,
                Score::rate(sc.partial_recall, sc.total)
            );
        }
        j.push_str("      \"misses\": [\n");
        for (k, m) in sc.misses.iter().take(dump).enumerate() {
            let _ = writeln!(
                j,
                "        {{ \"input\": \"{}\", \"expect\": \"{}\", \"rank\": {}, \"top1\": \"{}\", \"unigram\": {}, \"true_syls\": \"{}\", \"mm\": \"{}\" }}{}",
                json_escape(&m.input),
                json_escape(&m.expect),
                m.rank
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "null".into()),
                json_escape(&m.got_top1),
                m.unigram,
                json_escape(&m.true_syls),
                json_escape(&m.mm),
                if k + 1 == sc.misses.len().min(dump) {
                    ""
                } else {
                    ","
                }
            );
        }
        j.push_str("      ]\n    }");
        j.push_str(if i + 1 == scores.len() { "\n" } else { ",\n" });
    }
    j.push_str("  }\n}\n");

    if let Some(p) = out.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    match std::fs::write(&out, &j) {
        Ok(()) => println!("\n结果已写入 {}", out.display()),
        Err(e) => eprintln!("\n写入 {} 失败: {}", out.display(), e),
    }
}

/// 只跑分类统计，不加载引擎（秒级）。用于核对 B 类总量是否与设计文档的 1110 条吻合。
#[test]
#[ignore = "分类统计：需要 build_dev 真实词库"]
fn pinyin_eval_class_census() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let raw = read_all_dicts(&dir);
    let unigram = collect_word_freqs(&raw);

    let trie = SyllableTrie::new();
    let mut reject = HashMap::new();
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    let mut in_unigram: HashMap<&'static str, usize> = HashMap::new();
    let mut examples: HashMap<&'static str, Vec<String>> = HashMap::new();

    // 与设计文档「1110 条畸形词条」对账用：该数字的判据是「汉字数 > mm 音节数」，
    // 与本 harness 的 B/C 判据不是同一根轴，必须分别统计才能核对。
    let mut fewer_total = 0usize; // chars > mm.len()
    let mut fewer_in_unigram = 0usize;
    let mut fewer_by_class: HashMap<&'static str, usize> = HashMap::new();
    let mut same_len_diff_split = 0usize; // mm.len() == chars 但切法不同

    for (text, code, weight) in &raw {
        if let Some(s) = classify(text, code, *weight, &trie, &mut reject) {
            *counts.entry(s.class.key()).or_default() += 1;
            if unigram.contains_key(&s.text) {
                *in_unigram.entry(s.class.key()).or_default() += 1;
            }
            if s.mm.len() < s.true_syls.len() {
                fewer_total += 1;
                *fewer_by_class.entry(s.class.key()).or_default() += 1;
                if unigram.contains_key(&s.text) {
                    fewer_in_unigram += 1;
                }
            } else if s.mm != s.true_syls {
                same_len_diff_split += 1;
            }
            let e = examples.entry(s.class.key()).or_default();
            if e.len() < 20 {
                e.push(format!(
                    "{}({} → {})",
                    s.text,
                    s.true_syls.join("|"),
                    s.mm.join("|")
                ));
            }
        }
    }

    println!("\n=== 分类普查（引擎实际加载的全部词库，去重前）===");
    println!("原始条目 {}", raw.len());
    for c in [Class::A, Class::B, Class::C] {
        println!(
            "\n{}\n  条目 {}，其中被 unigram 收录 {}",
            c.label(),
            counts.get(c.key()).copied().unwrap_or(0),
            in_unigram.get(c.key()).copied().unwrap_or(0)
        );
        for e in examples.get(c.key()).map(|v| v.as_slice()).unwrap_or(&[]) {
            println!("    {}", e);
        }
    }
    println!("\n=== 与设计文档「1110 条」对账 ===");
    println!(
        "「汉字数 > mm 音节数」共 {} 条（其中 unigram 收录 {}），按本 harness 判据分布: {:?}",
        fewer_total,
        fewer_in_unigram,
        {
            let mut v: Vec<_> = fewer_by_class.iter().collect();
            v.sort();
            v
        }
    );
    println!(
        "「音节数相同但切法不同」共 {} 条（全部落在 C）",
        same_len_diff_split
    );

    let mut rj: Vec<_> = reject.iter().collect();
    rj.sort();
    println!("\n丢弃: {:?}", rj);
}
