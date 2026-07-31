//! 临时拼音的候选取数上限与检索范围过滤回归测试
//!
//! **取数上限**：临拼曾固定向引擎取 `ENGINE_MAX_CANDIDATES`(50) 条，而它**没有翻页扩容
//! 通路**（`expand_candidates` 的守卫比对 `input_buffer`，临拼的码在 `temp_pinyin_buffer`
//! 里），于是第 51 位之后的候选**翻多少页都取不到**。用户实测：临拼下 `ying` 打不出「瑩」
//! （该字在拼音候选的第 158 位）。修复是让拼音类目标方案取全量
//! （`TEMP_PINYIN_MAX_CANDIDATES`），翻页由对 `state.candidates` 切片天然穷尽。
//!
//! **检索范围过滤**：临拼此前完全不经过 `apply_filter`——「检索范围」设置对它从来无效，
//! 默认 smart 下临拼比主路径多出数百个生僻字（实测 `ying`：299 vs 76）。现已按主路径同序
//! 接入（`mark_common` → `apply_filter`）。

use std::path::PathBuf;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

/// 词库缺失时跳过（无数据 CI 环境）。⚠️ 判据是耗时：本测试族真跑时约 0.15s 以上，
/// 若整体 0.00s 说明走了跳过分支，不能当作通过。
fn has_schemas() -> bool {
    let d = data_dir();
    d.join("schemas/wubi86.schema.toml").exists()
        && d.join("schemas/pinyin.schema.toml").exists()
        && d.join("schemas/pinyin/cn_dicts/41448.dict.yaml").exists()
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

fn press_letter(coord: &Coordinator, c: char) -> KeyAction {
    coord.handle_key_event(&key_event((c.to_ascii_uppercase() as u32) & 0xFF))
}

/// 五笔方案（临拼只在码表/混输方案下可用）+ 指定检索范围。
fn config(filter_mode: &str) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into(), "pinyin".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.input.filter_mode = filter_mode.into();
    cfg
}

/// 进入临拼并输入给定拼音，返回全部候选文本。
fn temp_pinyin_candidates(filter_mode: &str, input: &str) -> Vec<String> {
    let coord = Coordinator::new_headless(config(filter_mode), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xC0)); // 反引号进入临拼
    assert!(coord.debug_in_temp_pinyin(), "反引号应进入临时拼音");
    for c in input.chars() {
        press_letter(&coord, c);
    }
    coord.debug_all_candidate_texts()
}

/// 生僻字必须可达：检索范围为「全部字符」时 `ying` → 「瑩」。
///
/// 断言刻意包含**位置 > 50**：这是「测试没有恰好通过」的证据——若取数上限被改回 50
/// 之类的小值，该字不再在场，测试即红。只断言「包含瑩」是不够的，那在别的实现下
/// 也可能因排序变化而偶然满足。
#[test]
fn temp_pinyin_reaches_rare_char_beyond_old_limit() {
    if !has_schemas() {
        eprintln!("跳过：词库不存在");
        return;
    }
    let all = temp_pinyin_candidates("gb18030", "ying");
    let pos = all.iter().position(|t| t == "瑩");
    assert!(
        pos.is_some(),
        "临拼 `ying` 应能取到生僻字「瑩」，实际候选数={}",
        all.len()
    );
    let pos = pos.unwrap();
    assert!(
        pos > 50,
        "「瑩」应落在旧上限(50)之外（实测约第 158 位），否则本测试证明不了扩容生效：实际位置={pos}"
    );
}

/// 临拼一次取到的候选量，不得少于主路径首屏 —— 即「翻页有内容可翻」。
///
/// ⚠️ **不能断言两者相等**。前缀补全的条数现在跟随请求量
/// （`pinyin/mod.rs` 的 `completion_limit`），而临拼一次取 `TEMP_PINYIN_MAX_CANDIDATES`、
/// 主路径首屏只取 `initial_candidate_limit`(300) 并靠翻页逐步扩容 —— 两者本就不同。
/// 早期版本曾断言相等，那建立在「补全固定 30 条、两侧都取到同一个全量」的旧假设上，
/// 放开补全后该前提即失效。
///
/// 真正要钉的是：临拼**不比主路径少**，且拿到了该输入的全部精确匹配（`ying` 的同音字
/// 约 916 条，「瑩」在其中第 158 位）。
///
/// 两侧都用 `gb18030`（不过滤）以统一口径；过滤本身由
/// [`temp_pinyin_respects_filter_mode`] 单独覆盖。
#[test]
fn temp_pinyin_candidate_count_not_less_than_main_path() {
    if !has_schemas() {
        eprintln!("跳过：词库不存在");
        return;
    }
    let temp = temp_pinyin_candidates("gb18030", "ying");

    // 主路径对照：纯拼音方案下输入同样的码。
    let mut cfg = config("gb18030");
    cfg.schema.active = "pinyin".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "ying".chars() {
        press_letter(&coord, c);
    }
    let main = coord.debug_all_candidate_texts();

    assert!(
        main.len() > 50,
        "对照组自身须超过旧上限，否则比较无意义：main={}",
        main.len()
    );
    assert!(
        temp.len() >= main.len(),
        "临拼取数不应少于主路径首屏（临拼={} 主路径={}）",
        temp.len(),
        main.len()
    );
    // 精确匹配必须完整：`ying` 的同音字约 916 条，取不全即说明取数上限又被压低。
    assert!(
        temp.len() > 900,
        "临拼应取到 `ying` 的全部同音字（约 916 条）加补全，实际只有 {}",
        temp.len()
    );
}

/// 临拼必须遵守「检索范围」设置。
///
/// **自带反向对照**：同一输入在 `smart` 下生僻字「瑩」应被滤掉、在 `gb18030` 下应在场。
/// 只测其中一侧都不足以证明过滤真的接上了——只测 smart 无法区分「过滤生效」与「取数
/// 上限又退回 50」，只测 gb18030 则根本不经过过滤分支。
#[test]
fn temp_pinyin_respects_filter_mode() {
    if !has_schemas() {
        eprintln!("跳过：词库不存在");
        return;
    }
    let all = temp_pinyin_candidates("gb18030", "ying");
    let smart = temp_pinyin_candidates("smart", "ying");

    assert!(
        all.iter().any(|t| t == "瑩"),
        "全部字符下「瑩」应在场（对照组）"
    );
    assert!(
        !smart.iter().any(|t| t == "瑩"),
        "智能过滤下生僻字「瑩」应被滤掉——临拼未接 apply_filter 时此断言必红"
    );
    assert!(
        smart.len() < all.len(),
        "智能过滤应使候选变少（smart={} 全部={}）",
        smart.len(),
        all.len()
    );
    // 常用字不受影响。
    assert!(
        smart.iter().any(|t| t == "应"),
        "常用字「应」在智能过滤下仍应在场"
    );
}

/// 翻页不改变候选集合：临拼靠对 `state.candidates` 切片翻页，不重新查询，
/// 故翻到底也不会新增或丢失候选。
#[test]
fn temp_pinyin_paging_does_not_change_candidate_set() {
    if !has_schemas() {
        eprintln!("跳过：词库不存在");
        return;
    }
    let coord = Coordinator::new_headless(config("gb18030"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xC0));
    for c in "ying".chars() {
        press_letter(&coord, c);
    }
    let before = coord.debug_all_candidate_texts();
    for _ in 0..60 {
        coord.handle_key_event(&key_event(0x22)); // PageDown
    }
    let after = coord.debug_all_candidate_texts();
    assert_eq!(before, after, "翻页不应改变候选集合");
    assert!(after.iter().any(|t| t == "瑩"), "翻页后「瑩」仍应在候选中");
}
