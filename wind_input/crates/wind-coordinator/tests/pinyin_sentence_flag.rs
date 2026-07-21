//! 整句锚定标记的跨层贯通测试
//!
//! `freq_rerank` 的整句豁免此前靠 `weight >= 20_000_000` 判定，现改为按
//! `Candidate::is_sentence` 语义标记。标记在引擎产出后要穿过
//! `finalize_candidates` → `build_candidates` → `apply_freq_rerank` 整条链路，
//! 任何一处重建 Candidate 都会把它丢掉——丢了不会编译报错，只会让整句在有词频
//! 记录时被静默挤下首位。本测试锁住这条链路。
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

/// 「恭贺」被反复使用出高词频，但 gonghe 的首选必须仍是 Viterbi 整句「共和」。
///
/// 若 `is_sentence` 在跨层传递中丢失，「共和」会退化成普通候选参与词频重排，
/// 被有使用记录的「恭贺」挤到第二——本断言即会失败。
#[test]
fn test_sentence_flag_survives_to_freq_rerank() {
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
    assert_eq!(
        all.first().map(|s| s.as_str()),
        Some("共和"),
        "整句「共和」须锚定首位（is_sentence 标记应贯通到 freq_rerank），实际候选: {:?}",
        &all[..all.len().min(5)]
    );

    let _ = std::fs::remove_file(&store_path);
}
