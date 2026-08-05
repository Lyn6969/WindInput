//! 码元字符集 `input_chars` / `leading_chars` 的端到端验证
//! （设计见 docs/design/codetable-input-chars.md）。
//!
//! 五笔 86 的码元是 a-y，故用 `input_chars = "a-x"` 把 `y` 排除出去——`y` 在默认配置下
//! 是**完全正常**的码元（`ay` 有候选），对照因此鲜明：同一个键、同一套操作，仅因字符集
//! 不同而走向两条路。
//!
//! ## ⚠️ 反向对照不可省
//!
//! 每条「非码元字符被拒」的用例都配一条「默认字符集下同一操作照常进缓冲」的对照。
//! 只测正向的话，哪怕 `input_chars` 整个没接线、或 `y` 因别的原因本就打不出，用例
//! 一样会绿——对照用例把「是本特性起了作用」与「碰巧看起来对」区分开。
//!
//! 词典缺失时自动跳过 —— ⚠️ `build_dev/data` 不存在时**整族静默跳过而计数照绿**，
//! 判据是耗时（正常 1s 量级 vs 跳过 0.0x s）。见 project_build_dev_data_missing。

use std::path::PathBuf;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
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

fn press(coord: &Coordinator, code: &str) {
    for c in code.chars() {
        coord.handle_key_event(&key_event((c.to_ascii_uppercase() as u32) & 0xFF));
    }
}

fn press_one(coord: &Coordinator, c: char) -> KeyAction {
    coord.handle_key_event(&key_event((c.to_ascii_uppercase() as u32) & 0xFF))
}

/// 上屏动作的文本；非上屏动作返回 `None`。
fn committed(action: &KeyAction) -> Option<&str> {
    match action {
        KeyAction::InsertText { text, .. } => Some(text.as_str()),
        _ => None,
    }
}

/// `input_chars` / `leading_chars` 皆空 = 内置默认 `a-z`（历史行为）。
fn wubi_config(input_chars: &str, leading_chars: &str) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into()];
    cfg.schema.active = "wubi86".into();
    cfg.input.default.chinese_mode = true;
    cfg.schema.codetable.input_chars = input_chars.into();
    cfg.schema.codetable.leading_chars = leading_chars.into();
    cfg
}

// ────────────────────────── 非码元字母 ──────────────────────────

/// 主用例：`a-x` 下组码中按 `y` → 终结组合，顶屏当前高亮候选后接 `y`。
#[test]
fn non_code_letter_terminates_composition() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let coord = Coordinator::new_headless(wubi_config("a-x", ""), Some(&d));

    press(&coord, "a");
    let first = coord
        .debug_all_candidate_texts()
        .first()
        .cloned()
        .expect("打 a 应有候选");

    let act = press_one(&coord, 'y');
    assert_eq!(
        committed(&act),
        Some(format!("{first}y").as_str()),
        "非码元字母应顶屏高亮候选再输出自身（不丢已打的码），实际: {act:?}"
    );
}

/// ★ **反向对照**：默认字符集下同一操作，`y` 照常进缓冲、不上屏。
///
/// 这一条证明主用例的行为来自 `input_chars = "a-x"`，而不是 `y` 本来就打不出、
/// 或整条判定压根没接线。
#[test]
fn default_charset_buffers_the_same_letter() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let coord = Coordinator::new_headless(wubi_config("", ""), Some(&d));

    press(&coord, "a");
    let act = press_one(&coord, 'y');
    assert_eq!(
        committed(&act),
        None,
        "默认码元集 a-z 下 y 是合法码元，应继续组码而非上屏，实际: {act:?}"
    );
}

/// ★ 空缓冲下的非码元字母**也必须出字，不能透传**。
///
/// C++ 在中文模式下对字母键是无条件吃的（`chinese_letter` 分支），返回 PassThrough
/// 就构成「吃了再吐」——不补发 WM_KEYDOWN 的宿主直接丢字符。铁律见
/// project_fullwidth_eat_flip：C++ 吃键集 ⊆ Rust 出字集。
#[test]
fn non_code_letter_on_empty_buffer_still_outputs() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let coord = Coordinator::new_headless(wubi_config("a-x", ""), Some(&d));

    let act = press_one(&coord, 'y');
    assert_eq!(
        committed(&act),
        Some("y"),
        "空缓冲的非码元字母须由本侧出字（透传会被宿主丢键），实际: {act:?}"
    );
}

/// 子集内的字母照常进缓冲——证明 `a-x` 只排除了 `y`，没把整套字母判死。
#[test]
fn code_letter_still_buffers_under_subset() {
    let d = data_dir();
    if !dict_ready(&d) {
        eprintln!("跳过：五笔词库不存在");
        return;
    }
    let coord = Coordinator::new_headless(wubi_config("a-x", ""), Some(&d));

    let act = press_one(&coord, 'a');
    assert_eq!(
        committed(&act),
        None,
        "a 在 a-x 内，应继续组码，实际: {act:?}"
    );
    assert!(
        !coord.debug_all_candidate_texts().is_empty(),
        "码元字母进缓冲后应有候选"
    );
}
