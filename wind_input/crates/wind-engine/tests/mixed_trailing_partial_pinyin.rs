//! 混输「满码 + 尾部残码拼音」诊断用例（真机现象：五笔拼音混输打 `nunl`）。
//!
//! 现象三连：4 码本该「无匹配即清空」，却出了拼音候选「嫩」；空格上屏后缓冲残留一个 `l`；
//! 编码栏显示 `nunl` 而非 `nun'l`。三者出自同一个事实——**`nun` 是标准音节表中的合法音节**
//! （`syllable.rs` 末尾为双拼转换真值补入的稀有音节之一），故 `nunl` 被 DAG 切成
//! 「完成音节 nun + 尾部残码 l」，走的是全拼精确匹配路径，与简拼开关无关。
//!
//! 本文件用最小内存词典复刻整条链路，锁住修复前后的两侧行为：
//! - 编码栏：`preedit_pinyin`（高亮跟随用的拆分形态）在单音节 + 残码时也须给出 `nun'l`；
//! - 清空：两道拼音守护同受 `auto_commit_block_on_pinyin` 支配，开则守住、关则清空。

use std::sync::Arc;
use wind_dict::cached::CachedDict;
use wind_dict::codetable::CodetableDict;
use wind_dict::{DictManager, SystemDictLayer};
use wind_engine::codetable::{CodeTableEngine, CommitOptions};
use wind_engine::mixed::{MixConfig, MixedEngine};
use wind_engine::pinyin::Config as PinyinConfig;
use wind_engine::{Engine, PinyinEngine};

/// 五笔侧：只放与 `nunl` 无关的词条，保证该串在码表无候选、无更长后继（满码空码清空的前提）。
fn wubi_engine() -> Box<dyn Engine> {
    let mut d = CodetableDict::empty();
    d.merge_single("aaaa".into(), "工".into(), 100, 0);
    let dm = DictManager::new();
    dm.register_layer(Box::new(SystemDictLayer::new(CachedDict::Memory(d), "sys")));
    Box::new(CodeTableEngine::new(
        4,
        CommitOptions {
            clear_on_empty_max: true,
            auto_commit_at_full: true,
            ..Default::default()
        },
        Arc::new(dm),
    ))
}

/// 拼音侧：复刻 `cn_dicts/41448.dict.yaml` 的那一条 `嫩 nun 0`（大字集异读注音）。
/// **简拼显式关闭**，锁住「本现象与简拼无关」。
fn pinyin_engine() -> PinyinEngine {
    let mut d = CodetableDict::empty();
    d.merge_single("nun".into(), "嫩".into(), 1, 0);
    d.merge_single("nen".into(), "嫩".into(), 1238, 1);
    PinyinEngine::new(
        PinyinConfig {
            enable_abbrev: false,
            ..Default::default()
        },
        CachedDict::Memory(d),
    )
}

/// 事实 ①：`nun` 是合法音节，`nunl` = 完成音节 `nun` + 残码 `l`，拼音引擎单独看确实给「嫩」，
/// 且 preedit 是拆开的 `nun'l`、消费长度只有 3。
#[test]
fn pinyin_alone_splits_nunl_and_consumes_only_three() {
    let e = pinyin_engine();
    let r = e.convert("nunl", 20).unwrap();

    assert_eq!(
        r.completed_syllables,
        vec!["nun".to_string()],
        "nun 应被切为完成音节（它在 STANDARD_SYLLABLES 中）"
    );
    assert_eq!(r.partial_syllable, "l", "尾部 l 是残码");
    assert_eq!(
        r.preedit_display, "nun'l",
        "拼音引擎自身的 preedit 是拆分形态"
    );

    let nen = r
        .candidates
        .iter()
        .find(|c| c.text == "嫩")
        .expect("应出「嫩」（code=nun 精确命中）");
    assert_eq!(nen.code, "nun", "候选码只覆盖完成音节，不含残码 l");
    assert_eq!(
        nen.consumed_length, 3,
        "上屏只消费 3 个字符 → 缓冲残留 l（真机现象）"
    );
}

/// 事实 ②：混输下满 4 码**不清空**。两道守护各自独立成立，任一都足够拦住清空：
/// (a) `has_pinyin` —— 拼音此刻确有候选「嫩」；
/// (b) `pinyin_may_continue` —— `nunl` = 完整音节 nun + 合法音节前缀 l（la/le/li…）。
#[test]
fn mixed_does_not_clear_on_full_code_with_trailing_partial() {
    let e = MixedEngine::new(
        wubi_engine(),
        Some(Box::new(pinyin_engine())),
        None,
        MixConfig::default(),
    );
    let r = e.convert("nunl", 50).unwrap();

    assert!(
        r.candidates.iter().any(|c| c.text == "嫩"),
        "混输候选应含拼音「嫩」，实际: {:?}",
        r.candidates.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
    assert!(!r.should_clear, "有拼音候选 → 满码清空被否决");
}

/// 事实 ②的(b)单独成立：**即使把「嫩 nun」从词库里删掉**（拼音此刻无候选），
/// `pinyin_may_continue` 仍判「还没打完」→ 照样不清空。
/// ⇒ 只清理词库那一条异读注音**不足以**让 `nunl` 清空；也正因如此，放开清空时两道守护
/// 必须一起受 `auto_commit_block_on_pinyin` 支配（见 `guard_off_clears_nunl`）。
#[test]
fn clear_still_vetoed_even_without_the_nun_entry() {
    let mut d = CodetableDict::empty();
    d.merge_single("nen".into(), "嫩".into(), 1238, 0);
    let py = PinyinEngine::new(
        PinyinConfig {
            enable_abbrev: false,
            ..Default::default()
        },
        CachedDict::Memory(d),
    );
    assert!(
        py.convert("nunl", 20).unwrap().candidates.is_empty(),
        "前置：删掉 nun 词条后此串确无拼音候选"
    );
    assert!(
        py.is_possible_pinyin_sequence("nunl"),
        "nun(完整音节) + l(合法音节前缀) → 判为「拼音还没打完」"
    );

    let e = MixedEngine::new(
        wubi_engine(),
        Some(Box::new(py)),
        None,
        MixConfig::default(),
    );
    let r = e.convert("nunl", 50).unwrap();
    assert!(r.candidates.is_empty(), "前置：此时合并候选确为空");
    assert!(
        !r.should_clear,
        "第二道守护 pinyin_may_continue 独立拦住清空"
    );
}

/// 事实 ③（修复 A）：编码栏的两个口径分工。
///
/// `preedit_display` 是「无高亮上下文时的默认形态」，保持保守（≥2 完成音节才拆），否则
/// 纯五笔码也会被拆得莫名其妙；`preedit_pinyin` 是「拼音拆分形态」，经协调器
/// `preedit_split_body` 供高亮跟随——高亮到拼音候选「嫩」时显示 `nun'l`，高亮回五笔候选
/// 时仍是 `nunl`。修复前它与 `preedit_display` 共用「≥2 音节」判据，单音节 + 残码时为空，
/// 于是候选按 `nun|l` 算、编码栏却按整串显示，上屏残留的 `l` 显得毫无由来。
#[test]
fn mixed_exposes_split_form_for_single_syllable_with_partial() {
    let e = MixedEngine::new(
        wubi_engine(),
        Some(Box::new(pinyin_engine())),
        None,
        MixConfig::default(),
    );
    let r = e.convert("nunl", 50).unwrap();

    assert_eq!(
        r.preedit_pinyin, "nun'l",
        "高亮跟随用的拆分形态必须给出，否则协调器恒显示原始码"
    );
    assert_eq!(
        r.preedit_display, "nunl",
        "默认形态保持保守：单音节不拆（纯五笔码不该被拆）"
    );
    assert_eq!(
        pinyin_engine().convert("nunl", 20).unwrap().preedit_display,
        "nun'l",
        "溯源：拆分形态本就由拼音引擎算好，此前被混输丢弃"
    );

    // 拆分串与原始输入相同 → 无拆分形态（空 = 协调器恒用原始码，不触发高亮跟随重算）。
    let r2 = e.convert("nen", 50).unwrap();
    assert_eq!(r2.preedit_display, "nen");
    assert_eq!(r2.preedit_pinyin, "", "单音节无残码：拆分串==原串，不填");
}

/// 修复 B：关掉「有拼音候选时否决上屏」后，`nunl` 满 4 码无匹配即清空。
/// 用真 `PinyinEngine`（非 fake）走完整链路，锁住用户的实际诉求。
#[test]
fn guard_off_clears_nunl() {
    let e = MixedEngine::new(
        wubi_engine(),
        Some(Box::new(pinyin_engine())),
        None,
        MixConfig {
            auto_commit_block_on_pinyin: false,
            ..Default::default()
        },
    );
    let r = e.convert("nunl", 50).unwrap();
    assert!(
        r.should_clear,
        "① 关 + 满码码表无候选 → 清空（拼音候选「嫩」与残码守护均不得再拦）"
    );
}
