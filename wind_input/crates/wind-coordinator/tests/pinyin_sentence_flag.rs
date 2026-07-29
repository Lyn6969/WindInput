//! 整句排序标记的跨层贯通测试
//!
//! `freq_rerank` 的整句豁免此前靠 `weight >= 20_000_000` 判定，现改为按
//! `Candidate::is_sentence` 族语义标记。标记在引擎产出后要穿过
//! `finalize_candidates` → `build_candidates` → `apply_freq_rerank` 整条链路，
//! 任何一处重建 Candidate 都会把它丢掉——丢了不会编译报错，只会让整句排序
//! 静默走样。本测试锁住这条链路。
//!
//! ## ⚠️ 断言方向已随语义反转（原为「整句恒锚定首位」）
//!
//! 原测试断言 `gonghe` 的首选恒为整句「共和」，即便同码的「恭贺」有高词频。该语义
//! 已被推翻：「共和」自己就是一个词典精确整词，只是恰好被 Viterbi 选为最优解而继承
//! 了整句身份；锚定是**硬闸门**（`freq_rerank` 的 ① 步直接 return，衰减分连算都不算），
//! 于是同码的「恭贺」无论被选中多少次都翻不过它 —— 词频维度对整个 `gonghe` 编码失效。
//! `siyuan` 的「寺院」压住「思源」是同一现场（实测灌到 count=5000 仍纹丝不动）。
//!
//! 现由 `Candidate::is_sentence_contested` 标记这类「有同码竞争者」的整句并摘掉其锚定。
//!
//! **本测试保护的目标没有变**：`is_sentence` 与 `is_sentence_contested` 同为 Candidate 上
//! 相邻的 `serde(skip)` 字段，「跨层重建把标记丢掉」这一失效模式会同时丢掉两者 ——
//! 丢掉 contested 则锚定恢复、「共和」重回首位，本断言即会失败。判据方向反了，守的
//! 是同一条链路。
//!
//! 词典缺失时自动跳过。

use std::path::PathBuf;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

fn data_dir() -> PathBuf {
    // 三级：crates/wind-coordinator → crates → wind_input → 仓库根（build_dev 在仓库根）。
    // 曾误写成两级，解析到 wind_input/build_dev/data —— 该目录不存在，于是下面的
    // exists() 判假、整个测试族静默走「跳过」分支通过。**判据是耗时 0.00s**。
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn key_event(key_code: u32, event_type: u8) -> KeyEventData {
    KeyEventData {
        key_code,
        scan_code: 0,
        modifiers: 0,
        event_type,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

fn press_letter(coord: &Coordinator, c: char) {
    let vk = (c.to_ascii_uppercase() as u32) & 0xFF;
    coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN));
}

/// 「恭贺」被反复使用出高词频后，gonghe 的首选须让给它；整句「共和」退居第二而非消失。
///
/// 若 `is_sentence_contested` 在跨层传递中丢失，「共和」会恢复顶部锚定、把「恭贺」
/// 永久压在下面——本断言即会失败（这正是修复前的行为）。
#[test]
fn test_sentence_flags_survive_to_freq_rerank() {
    let d = data_dir();
    if !d.join("schemas/pinyin/cn_dicts/base.dict.yaml").exists() {
        eprintln!("跳过：拼音词库不存在");
        return;
    }

    let store_path = std::env::temp_dir().join("wind_sentence_flag_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    // 「恭贺」积累可观使用次数（拼音衰减分远超 PINYIN_FREQ_EPSILON）
    for _ in 0..30 {
        store
            .record_freq("pinyin", "gonghe", "恭贺")
            .expect("record_freq 失败");
    }

    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".into()];
    cfg.schema.active = "pinyin".into();
    cfg.input.default.chinese_mode = true;
    cfg.schema.pinyin.frequency.enabled = true;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&d), store);

    for c in "gonghe".chars() {
        press_letter(&coord, c);
    }

    let all = coord.debug_all_candidate_texts();
    let head: Vec<&str> = all.iter().take(5).map(|s| s.as_str()).collect();
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("恭贺"),
        "反复使用过的同码词须能反超整句（is_sentence_contested 应贯通到 freq_rerank），实际候选: {head:?}"
    );
    assert_eq!(
        all.get(1).map(|s| s.as_str()),
        Some("共和"),
        "整句只是退居第二、不得被赶出列表（本标记只摘锚定、不动 weight），实际候选: {head:?}"
    );

    let _ = std::fs::remove_file(&store_path);
}
