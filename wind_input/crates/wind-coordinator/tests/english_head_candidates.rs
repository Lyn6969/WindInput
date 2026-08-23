//! 英文方案与临时英文的**头部候选**（输入原文 + 大小写变形）端到端测试。
//!
//! 设计见 `docs/design/schema-scoped-behavior.md` §5。
//!
//! # 这组测试要钉住的三件事
//!
//! 1. **英文方案下首候选恒是所打原文**——这是本功能的全部意义。英文引擎的「输入即内容」
//!    使输入串本身就是可上屏文本，而调频会把某个词顶到首位，届时想上屏原文就只剩回车。
//! 2. **四个键两侧独立**：`schema.english.*` 与 `input.temp_english.*` 互不影响，
//!    且 `case_variants` 两侧**默认值相反**（英文方案 false / 临英 true）。
//! 3. ★ **两个开关同时关 + 词库无命中 ⇒ 候选为空**时，空格仍必须上屏输入串。
//!    这是设计文档 §5.5 标为「实施时必验」的一条——英文方案侧走主路径的通用分支，
//!    是整个设计里唯一没有既存判据可依的地方。若它吞键，表现就是「打了一串英文按空格
//!    什么都没发生」。
//!
//! # ⚠️ 假绿源
//!
//! 词典缺失时整族**静默跳过**（判据是耗时而非通过条数），worktree 需自备 `build_dev`。
//! 见 `has_english_schema`。

use std::path::PathBuf;
use std::sync::Arc;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::{EVENT_KEY_DOWN, MOD_SHIFT};
use wind_store::Store;

const VK_SPACE: u32 = 0x20;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_english_schema() -> bool {
    let d = data_dir();
    d.join("schemas/english.schema.toml").exists() && d.join("schemas/english").is_dir()
}

fn key(key_code: u32, modifiers: u32) -> KeyEventData {
    KeyEventData {
        key_code,
        scan_code: 0,
        modifiers,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

/// 英文方案作 active。
fn english_config() -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into(), "english".into()];
    cfg.schema.active = "english".into();
    cfg.input.default.chinese_mode = true;
    cfg
}

/// 临英（主方案取**五笔**：归属必须是内置英文方案，与 active 无关）。
fn temp_english_config() -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into(), "english".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.temp_english.enabled = true;
    cfg
}

fn store_at(tag: &str) -> Arc<Store> {
    let path = std::env::temp_dir().join(format!("wind_en_head_{tag}.redb"));
    let _ = std::fs::remove_file(&path);
    Arc::new(Store::open(&path).unwrap())
}

fn coord_with(cfg: Config, tag: &str) -> Arc<Coordinator> {
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store_at(tag))
}

/// 主输入路打词（英文方案下缓冲恒小写）。
fn type_word(coord: &Coordinator, word: &str) {
    for c in word.chars() {
        coord.handle_key_event(&key((c.to_ascii_uppercase() as u32) & 0xFF, 0));
    }
}

/// Shift+首字母进入临英，再打完剩余字母。
fn enter_temp_english(coord: &Coordinator, word: &str) {
    let mut chars = word.chars();
    let first = chars.next().expect("至少一个字母");
    coord.handle_key_event(&key((first.to_ascii_uppercase() as u32) & 0xFF, MOD_SHIFT));
    for c in chars {
        coord.handle_key_event(&key((c.to_ascii_uppercase() as u32) & 0xFF, 0));
    }
}

// ───────────────────── 英文方案：原文候选 ─────────────────────

/// 英文方案下首候选恒是所打原文，其后才是词库补全。
#[test]
fn english_schema_first_candidate_is_the_raw_input() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    let coord = coord_with(english_config(), "raw_on");
    type_word(&coord, "hel");
    let page = coord.debug_page_texts();
    assert_eq!(
        page.first().map(String::as_str),
        Some("hel"),
        "首候选必须是所打原文，实际页面: {page:?}"
    );
    assert!(
        page.len() > 1,
        "原文之后应还有词库补全（hello/help/…），实际: {page:?}"
    );
}

/// 关掉 `raw_candidate`：首候选变成词库词，原文不再单独占位。
///
/// 反向对照不可省：没有它，「恒插原文」与「按开关插原文」两种实现都能让正向断言通过。
#[test]
fn english_schema_raw_candidate_can_be_turned_off() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    let mut cfg = english_config();
    cfg.schema.english.raw_candidate = false;
    let coord = coord_with(cfg, "raw_off");
    type_word(&coord, "hel");
    let page = coord.debug_page_texts();
    assert!(!page.is_empty(), "词库应有 hel 的前缀命中");
    assert_ne!(
        page.first().map(String::as_str),
        Some("hel"),
        "关掉后首条应是词库词而非原文，实际: {page:?}"
    );
}

/// 词频把某个词顶到词库段首时，原文**仍在它之前**。
///
/// 这条才是需求的原始场景：调频本身工作正常，但用户还要能一键上屏所打原文。
/// 断言落在「原文在前、被顶起来的词紧随其后」，两件事同时成立才算对。
#[test]
fn raw_candidate_outranks_frequency_promoted_word() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    let mut cfg = english_config();
    // ⚠️ 出厂是 false，不显式打开就是在测一个关着的功能。
    cfg.schema.english.frequency.enabled = true;
    cfg.schema.english.frequency.strategy = "top".into();
    let coord = coord_with(cfg, "freq_top");

    // 先选一次靠后的词，把它顶到词库段首。
    type_word(&coord, "hel");
    let page = coord.debug_page_texts();
    // page[0] 是原文，词库段从 1 开始；取第 2 个词库候选（越靠后越能证明"被顶起来了"）。
    let target = page.get(2).cloned().unwrap_or_default();
    if target.is_empty() {
        eprintln!("跳过：hel 的词库候选不足 2 条");
        return;
    }
    // 数字键 3 选中它（1 是原文）。
    coord.handle_key_event(&key(0x33, 0));

    type_word(&coord, "hel");
    let page = coord.debug_page_texts();
    assert_eq!(
        page.first().map(String::as_str),
        Some("hel"),
        "调频把词顶到词库段首后，原文仍必须在最前，实际: {page:?}"
    );
    assert_eq!(
        page.get(1),
        Some(&target),
        "被调频顶起来的词应紧随原文之后（证明调频确实生效了，不是这条测试自己没跑起来）"
    );
}

// ───────────────────── 大小写变形：两侧默认值相反 ─────────────────────

/// 英文方案默认**不**出变形候选；临英默认**出**。
///
/// 默认值本身要有测试，否则「只翻默认值」一条测试都不会红。
#[test]
fn case_variants_defaults_differ_between_the_two_scopes() {
    let cfg = Config::default();
    assert!(
        !cfg.schema.english.case_variants,
        "英文方案是长时输入场景，变形每条吃一个候选位，默认应关"
    );
    assert!(
        cfg.input.temp_english.case_variants,
        "临英是「中文里插一个英文词」，首字母大写是刚需，默认应开（既有行为）"
    );
    assert!(
        cfg.schema.english.raw_candidate && cfg.input.temp_english.raw_candidate,
        "原文候选两侧都默认开：临英是保持既有行为，英文方案是需求的核心诉求"
    );
}

/// 两侧的键互不影响：改英文方案那对，临英的产出一个字都不变。
#[test]
fn the_two_scopes_do_not_leak_into_each_other() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    // 英文方案侧全关，临英侧保持默认（原文 + 变形）。
    let mut cfg = temp_english_config();
    cfg.schema.english.raw_candidate = false;
    cfg.schema.english.case_variants = false;
    let coord = coord_with(cfg, "no_leak");
    enter_temp_english(&coord, "Hel");
    let page = coord.debug_page_texts();
    assert_eq!(
        page.first().map(String::as_str),
        Some("Hel"),
        "临英首候选仍是原文——改英文方案那对键不该影响临英，实际: {page:?}"
    );
    assert!(
        page.iter().any(|t| t == "hel") && page.iter().any(|t| t == "HEL"),
        "临英仍应出大小写变形，实际: {page:?}"
    );
}

// ───────────────────── ★ §5.5：候选为空时的上屏出口 ─────────────────────

/// ★ 英文方案：两个开关都关 + 词库无命中 ⇒ 候选为空，**空格必须上屏输入串**。
///
/// 设计文档标为「实施时必验」的一条。英文方案走主路径的通用分支，没有既存判据可依；
/// 若它吞键，表现就是「打了一串英文按空格什么都没发生」——用户会以为输入法死了。
#[test]
fn english_schema_commits_raw_input_when_no_candidates() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    let mut cfg = english_config();
    cfg.schema.english.raw_candidate = false;
    cfg.schema.english.case_variants = false;
    let coord = coord_with(cfg, "empty_commit");
    // 刻意打一个词库里不可能有的串。
    type_word(&coord, "zzqxwv");
    assert!(
        coord.debug_page_texts().is_empty(),
        "前提不成立：这串码不该有词库候选，实际: {:?}",
        coord.debug_page_texts()
    );
    let action = coord.handle_key_event(&key(VK_SPACE, 0));
    match action {
        KeyAction::InsertText { text, .. } => assert!(
            text.starts_with("zzqxwv"),
            "空候选时空格应上屏输入串本身，实际上屏: {text:?}"
        ),
        other => panic!(
            "空候选时空格必须上屏输入串，不得吞键。实际: {other:?}\n\
             （若这里是 Eaten/None，表现就是「打了一串英文按空格什么都没发生」）"
        ),
    }
}

/// ★ 临英：同样的组合下空格上屏缓冲原文。
///
/// 临英侧**判据本来就是对的**——空格臂判的是 `!candidates.is_empty()`（实际候选）
/// 而不是 `show_candidates` 配置项，所以空候选会正确落到「上屏缓冲原文」的兜底分支。
/// 这条测试是为了把「本来就对」变成「被钉住了」：那个分支的注释写的是
/// 「无候选（show_candidates 关闭）」，成因如今多了一个。
#[test]
fn temp_english_commits_buffer_when_no_candidates() {
    if !has_english_schema() {
        eprintln!("跳过：缺少英文方案或词库");
        return;
    }
    let mut cfg = temp_english_config();
    cfg.input.temp_english.raw_candidate = false;
    cfg.input.temp_english.case_variants = false;
    let coord = coord_with(cfg, "te_empty_commit");
    enter_temp_english(&coord, "Zzqxwv");
    assert!(
        coord.debug_page_texts().is_empty(),
        "前提不成立：这串不该有词库候选，实际: {:?}",
        coord.debug_page_texts()
    );
    let action = coord.handle_key_event(&key(VK_SPACE, 0));
    match action {
        KeyAction::InsertText { text, .. } => assert!(
            text.starts_with("Zzqxwv"),
            "空候选时空格应上屏缓冲原文，实际上屏: {text:?}"
        ),
        other => panic!("空候选时空格必须上屏缓冲原文，不得吞键。实际: {other:?}"),
    }
}
