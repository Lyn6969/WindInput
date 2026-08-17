//! 出简让全的端到端验证（设计见 docs/design/codetable-short-code-yields-full.md）。
//!
//! 单元测试（`short_code_yield` 模块内）证明的是判定函数本身；本文件证明的是**用户入口
//! 上真的打得出来**——记录沿途累积、判据在真实候选链上成立、让位作用在显示序上。
//! 本仓的教训是这两层必须分开测：引擎/纯函数全绿而用户打不出，是反复出现过的形态。
//!
//! ## ⚠️ 三条用例必须合看，缺一即可能假绿
//!
//! - `full_code_yields_to_word`：主用例，`wqiy` 首选从「你」变「仰泳」；
//! - `disabled_keeps_dictionary_order`：**反向对照**，同一份词库、只把档位关掉 → 首选回到
//!   「你」。它证明了词库原序确实是「你」在前，主用例不是因为词库本来就那样而假绿；
//! - `second_level_shortcode_yields_at_level_two`：档位边界，`wq` 是二简，故档位 2 也该让。
//!   若把档位判据写反（`>=` 写成 `>` 之类），这条会红而主用例照绿。
//!
//! 用例选 `wqiy`（你 / 仰泳）而不是 `khtk`（路 / 路程）：后者在**发行词库里已经被
//! `gen_dict` 的 `[demotion]` 让过位**了，首选本就是词，测不出算法层有没有干活。
//! `[demotion]` 退役后 `khtk` 也会成为可用现场。
//!
//! 词典缺失时自动跳过 —— ⚠️ `build_dev/data` 不存在时**整族静默跳过而计数照绿**，
//! 判据是耗时（正常 1s 量级 vs 跳过 0.0x s）。

use std::path::PathBuf;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn dict_ready(d: &std::path::Path) -> bool {
    d.join("schemas/wubi86/wubi86_jidian.dict.yaml").exists()
}

fn key_event(key_code: u32) -> KeyEventData {
    KeyEventData {
        key_code,
        scan_code: 0,
        modifiers: 0,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

/// 逐键按下——**必须逐键**：让位的判据来自沿途各级简码位的首选记录，
/// 直接把缓冲设成全码的写法会让记录全空，于是恒不让位。
fn press(coord: &Coordinator, code: &str) {
    for c in code.chars() {
        coord.handle_key_event(&key_event((c.to_ascii_uppercase() as u32) & 0xFF));
    }
}

fn wubi_config(level: usize) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.schema.codetable.short_code_yield_level = level;
    cfg
}

fn candidates_for(level: usize, code: &str) -> Option<Vec<String>> {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：五笔词库不存在");
        return None;
    }
    let coord = Coordinator::new_headless(wubi_config(level), Some(&d));
    press(&coord, code);
    Some(coord.debug_all_candidate_texts())
}

/// 主用例：「你」的二简是 `wq`，故全码 `wqiy` 的首选让给词。
#[test]
fn full_code_yields_to_word() {
    let Some(all) = candidates_for(3, "wqiy") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(6).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("仰泳"),
        "有简码的字应把全码首选让给词，实际候选: {head:?}"
    );
    assert!(
        all.iter().any(|t| t == "你"),
        "让的只是位次，字不得被赶出列表，实际候选: {head:?}"
    );
}

/// 让位的字沉到**本码所有候选之后**，不是降一位。
///
/// `dddd` 是现成的多候选现场：大 / 大厦 / 硕大 / 磕磕碰碰。若实现写成「与第一个词交换」，
/// 「大」会停在第 2 位而本用例会红。
#[test]
fn the_yielding_char_sinks_to_the_bottom() {
    let Some(all) = candidates_for(3, "dddd") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(8).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("大厦"),
        "首选让给词，实际候选: {head:?}"
    );
    assert_eq!(
        all.iter().position(|t| t == "大"),
        Some(all.len() - 1),
        "有简码的字须沉到本码所有候选之后，实际候选: {head:?}"
    );
}

/// 沉底前的对照：档位 0 时「大」是首选、且列表里排在其余候选之前。
#[test]
fn disabled_keeps_the_char_on_top_for_a_multi_candidate_code() {
    let Some(all) = candidates_for(0, "dddd") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(8).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("大"),
        "档位 0 须完全按词库原序，实际候选: {head:?}"
    );
}

/// **反向对照**：同一份词库，只把档位关到 0 → 首选回到词库原序的「你」。
///
/// 这一条证明主用例的「仰泳」是让位的结果，而不是词库本来就把它排在前面。
#[test]
fn disabled_keeps_dictionary_order() {
    let Some(all) = candidates_for(0, "wqiy") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(6).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("你"),
        "档位 0 须完全按词库原序（否则主用例是假绿），实际候选: {head:?}"
    );
}

/// 档位边界：`wq` 是二级简码，故档位 2 就该让位。
#[test]
fn second_level_shortcode_yields_at_level_two() {
    let Some(all) = candidates_for(2, "wqiy") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(6).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("仰泳"),
        "二简字在档位 2 下应当让位，实际候选: {head:?}"
    );
}

/// 简码位自身不让位：`wq` 是二简位，打到这里首选必须还是「你」。
///
/// 判据是「当前码长 > 档位」，若写成 `>=` 则简码位自己也会让位——用户连二简都打不出字了。
#[test]
fn shortcode_position_itself_keeps_the_char() {
    let Some(all) = candidates_for(3, "wq") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(6).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("你"),
        "简码位是让位的**来源**而不是对象，实际候选: {head:?}"
    );
}

/// 缺记录不让位：不逐键走到全码（这里直接打全码之外的路径无法构造，故用
/// 「首级记录被改码淘汰」等价场景）——`wqiy` 与 `wqiy` 之外的码不共享记录。
///
/// 与主用例的差别只有输入路径，用于锁住「判据来自沿途记录」这个设计本身：
/// 若哪天改成查询式实现，本用例仍绿而主用例也绿，但 `disabled_keeps_dictionary_order`
/// 与本用例的组合能暴露记录没被消费的情形。
#[test]
fn char_without_shortcode_top_does_not_yield() {
    // 「匹」在 aq* 各级都不是首选（aq→区、aqt→获），故 aqtd 不因它而让位；
    // 该码首选本就是词，用于确认不会把非让位场景误判成让位。
    let Some(all) = candidates_for(3, "aqtd") else {
        return;
    };
    let head: Vec<&str> = all.iter().take(6).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("匹敌"),
        "首选本就是词时不应有任何改动，实际候选: {head:?}"
    );
}
