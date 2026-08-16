//! 词频记录**不得改变**长词的召回位次（`cmp_by_consumed` 在两道排序里同口径）。
//!
//! ## 要守什么
//!
//! 真机报障：「冰冻三尺非一日之寒」打到 `bingdongsanchi` 首次能出（第 1 位），**上过一次
//! 屏、进了词频表之后就再也出不来**（掉到第 24 位），从词频表里删掉又恢复。
//!
//! 根因是两道排序的键不一致：协调器 `candidate_display_order` 以 `cmp_by_consumed` 开头，
//! 而最后一道整体排序 `freq_rerank::rerank_positional` 当时只有 `cmp_match_layers`；后者
//! **仅在本次输入有词频记录时才跑**（`recs.is_empty()` 直接 return）。该候选在整音节边界上
//! 拿不到残码上浮（`is_promoted_completion=false`）⇒ 落进前缀补全层 ⇒ 被 `bing` 的几十个
//! 同音单字整层压住，而它消费了整串、本该由 `cmp_by_consumed` 顶在最前。
//!
//! ⚠️ 该候选的词频记录**一格都没提升它**（`promote_prefix=single` 对 9 字候选判假），
//! 记录唯一的作用是触发那道用错键的重排 —— 所以这不是「词频排序不准」，是「词频的有无
//! 改变了与词频无关的次序」。
//!
//! ## 两条断言的分工
//!
//! 只测「位次不变」会被「把词频整个关掉」这种假修复骗过，故配一条**反向对照**：残码输入
//! `bingdongsanchif` 下该词**确实**因词频记录而前移。两条一起才说明「重排跑了，且没有越权」。

use std::path::PathBuf;
use std::sync::Arc;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;
use wind_store::Store;

/// 词库里的 9 音节长词（`base.dict.yaml`：`冰冻三尺非一日之寒 bing dong san chi fei yi ri zhi han 115`）。
const WORD: &str = "冰冻三尺非一日之寒";
const CODE: &str = "bingdongsanchifeiyirizhihan";

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_pinyin() -> bool {
    data_dir()
        .join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
}

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".into()];
    cfg.schema.active = "pinyin".into();
    cfg.input.default.chinese_mode = true;
    // ⚠️ `Config::default()` 的 `frequency.enabled` 是 serde 的 `bool::default()` = **false**，
    // 与出厂 `data/config.toml` 的 `enabled = true` 不一致。不显式打开，本文件测的那道重排
    // 压根不会被调用 —— 整个文件会静默变成恒绿。
    cfg.schema.pinyin.frequency.enabled = true;
    // 9 音节词要进得来：`started=4` 时上限 = 4 + max_extra，出厂的 3 只到 7 音节。
    // 这两个值取报障用户的实际设置。
    cfg.schema.pinyin.completion.min_syllables = 4;
    cfg.schema.pinyin.completion.max_extra_syllables = 6;
    cfg
}

/// 敲入整串，返回长词在候选中的位次（不在则 `None`）。
fn rank_of_word(store: Arc<Store>, input: &str) -> Option<usize> {
    let coord = Coordinator::new_headless_with_store(config(), Some(&data_dir()), store);
    for c in input.chars() {
        let vk = (c.to_ascii_uppercase() as u32) & 0xFF;
        coord.handle_key_event(&KeyEventData {
            key_code: vk,
            scan_code: 0,
            modifiers: 0,
            event_type: EVENT_KEY_DOWN,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        });
    }
    coord
        .debug_all_candidate_texts()
        .iter()
        .position(|t| t == WORD)
}

fn fresh_store(tag: &str) -> Arc<Store> {
    let root = std::env::temp_dir().join(format!("wind_freq_longword_{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    Arc::new(Store::open(root.join("user_data.db")).expect("打开 store"))
}

/// 整音节边界（`...chi`，无残码）：词频记录不得让长词沉底。
#[test]
fn freq_record_does_not_sink_long_word_at_syllable_boundary() {
    if !has_pinyin() {
        eprintln!("跳过：拼音词库不存在");
        return;
    }
    let store = fresh_store("boundary");
    let before = rank_of_word(store.clone(), "bingdongsanchi").expect("首次输入时长词应在候选中");

    store.record_freq("pinyin", CODE, WORD).expect("写词频");
    let after =
        rank_of_word(store.clone(), "bingdongsanchi").expect("有词频记录后长词仍应在候选中");

    assert_eq!(
        after, before,
        "词频记录不得改变长词位次（修复前：{before} → 24，被同音单字整层压住）"
    );
    // 位次本身也钉一下：只断言「相等」的话，两边一起烂掉（都在第 24 位）也能过。
    assert!(
        before <= 2,
        "长词消费了整串，应由 cmp_by_consumed 顶在最前，实际第 {before} 位"
    );
}

/// 反向对照：残码位（`...chif`）上词频记录**确实**在起作用（证明重排真的跑了）。
///
/// 缺了这条，「把词频整个关掉」也能让上面那条通过。
#[test]
fn freq_record_still_promotes_long_word_at_trailing_partial() {
    if !has_pinyin() {
        eprintln!("跳过：拼音词库不存在");
        return;
    }
    let store = fresh_store("partial");
    let before = rank_of_word(store.clone(), "bingdongsanchif").expect("首次输入时长词应在候选中");

    store.record_freq("pinyin", CODE, WORD).expect("写词频");
    let after =
        rank_of_word(store.clone(), "bingdongsanchif").expect("有词频记录后长词仍应在候选中");

    assert!(
        after < before,
        "残码位上该词有残码上浮（is_promoted_completion）⇒ 与精确候选同层，\
         词频应把它前移；实际 {before} → {after}（重排没跑？）"
    );
}
