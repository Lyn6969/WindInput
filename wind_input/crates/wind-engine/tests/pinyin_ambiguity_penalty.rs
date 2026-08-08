//! 歧义音节罚的定点守卫（Phase 4 / 方案 C）
//!
//! 多路径切分（Phase 3）移除了 `Dag::maximum_match` 的**隐式**歧义惩罚——它的
//! tie-break（长音节优先 + 严格大于）永远偏好更少更长的音节。移除后 `score_node`
//! 的真实性质暴露：对「多加一个词」和「多加一个音节」都不收费，于是把低频词打碎成
//! 两个高频片段在原始 log_prob 上就是赢的（`guotian` 过天→过提案 `guo|ti|an`）。
//!
//! `WORD_PENALTY` 与 `AMBIGUOUS_PENALTY` 把这个偏置用显式形式重建回来。
//!
//! **本文件存在的理由**：`AMBIGUOUS_PENALTY = 0.35` 是一个**刀刃值**，且聚合指标
//! 在 0.30~0.35 之间完全不变——`pinyin_eval` 的 top-1 / 切分正确率**看不出**这条边
//! 被越过。只有下面这些定点能。改系数前请先跑本文件。
//!
//! **必须用真实词库**：内联夹具的 `boundary` 恒为 0，边界校验一律降级放行，
//! 测不出任何东西。词典缺失时自动跳过。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

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

fn manager(dir: &std::path::Path) -> EngineManager {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::new(&cfg, Some(dir))
}

fn top1(mgr: &EngineManager, input: &str) -> String {
    mgr.convert_with("pinyin", input, 10)
        .candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// 本次改造的**原始缺陷**：`lianzhengtixing`（廉政提醒）首选是「李安整体性」。
///
/// 「李安」真值 `li|an`，在多路径下合法入图，且其 unigram 分（-12.09）本就高于
/// 「廉政」（-15.42），故 A/B 两阶段落地后缺陷会原样复发——真正压住它的是歧义罚。
///
/// **`AMBIGUOUS_PENALTY ≤ 0.30` 时本测试必红。**
#[test]
fn test_original_defect_lianzhengtixing() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let got = top1(&mgr, "lianzhengtixing");
    assert_ne!(got, "李安整体性", "原始缺陷复发：歧义罚过小");
}

/// 多路径**新引入**的 A 类切分破坏必须被压住。
///
/// 这些词的真值切分与 `maximum_match` 一致（属 A 类），Phase 3 之前从不出错；
/// 多路径放开零声母拆分后，`score_node` 因不对音节数收费而偏好碎片路径。
/// 实测 N=4000 时 A 类 `wrong_split` 由 1 涨到 13，**13 条全部是这一形态、零例外**。
#[test]
fn test_zero_initial_split_suppressed() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    for (input, expect) in [
        ("guotian", "过天"),     // 曾被切成 guo|ti|an → 过提案
        ("hualong", "化龙"),     // 曾被切成 hu|a|long  → 和阿龙
        ("lianfenxi", "链分析"), // 曾被切成 li|an|…   → 李安分析
    ] {
        assert_eq!(top1(&mgr, input), expect, "{input} 首选应为 {expect}");
    }
}

/// **已知取舍，非缺陷**：`liandaoyan`（李安导演）首选为「连导演」。
///
/// 「李安导演」与「李安整体性」是**同一个词、同一条 `li|an` 拆分、同一个歧义接缝**，
/// 切分层没有任何可区分二者的信息。实测扫描：`AMBIGUOUS_PENALTY ≤ 0.30` 时
/// `lianzhengtixing` 退回「李安整体性」，`≥ 0.35` 时本例劣化——**中间不存在兼顾值**。
///
/// 用户已在两者间选择优先修复原始缺陷（见
/// `docs/design/pinyin-boundary-aware-lattice.md`）。真正的区分需要 bigram 上下文
/// （「李安‖导演」vs「李安‖整体性」），尚无 bigram（缺磁盘语料）。
///
/// 本测试断言的是**当前的取舍结果**而非理想行为：若将来补上 bigram 使「李安导演」
/// 重新夺魁，本测试会红——那时应当删掉它，并把该例移回
/// `pinyin_multipath.rs::test_contracted_syllable_words_win_top1`。
#[test]
fn test_known_tradeoff_liandaoyan() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    let cands = mgr.convert_with("pinyin", "liandaoyan", 10).candidates;
    let got = cands.first().map(|c| c.text.as_str()).unwrap_or_default();
    assert_eq!(got, "连导演", "取舍点已漂移，请重新评估 AMBIGUOUS_PENALTY");

    // 「李安导演」不是词典词条，只能作为 Viterbi 整句出现；整句既已判给「连导演」，
    // 它就**完全不在候选列表里**，而非仅被降权。代价比「排名下降」更重。
    assert!(
        !cands.iter().any(|c| c.text == "李安导演"),
        "行为已变：若它重新出现在候选中，说明取舍点漂移，请重新评估"
    );

    // 逃生出口：隔音符号是用户主动消歧的手段（`'` 使长音节边 lian 不复存在）。
    // **这是本取舍可被接受的前提** —— 用户仍有确定的方式打出它。
    let with_sep = top1(&mgr, "li'andaoyan");
    assert_eq!(with_sep, "李安导演", "隔音符号必须仍能打出「李安导演」");
}

/// 长句不得因每词固定罚而被压成碎片或整体劣化。
///
/// `WORD_PENALTY` 对路径上每个词各扣一次，词数越多扣得越狠；这些多词长句是
/// 「罚过头」的敏感面——若系数过大，它们会退化成更少更长但更差的组合。
#[test]
fn test_long_sentences_unaffected() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：build_dev 拼音词库不存在");
        return;
    };
    let mgr = manager(&dir);
    for (input, expect) in [
        ("woshizhongguoren", "我是中国人"),
        ("jintiantianqizhenhao", "今天天气真好"),
        ("zhonghuarenmingongheguo", "中华人民共和国"),
        ("nihao", "你好"),
    ] {
        assert_eq!(top1(&mgr, input), expect, "{input} 首选应为 {expect}");
    }
}
