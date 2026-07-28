//! 智能符号（同键连按切换中/英标点）端到端测试
//!
//! 覆盖两条新增通路：
//!   1. **反向**（数字后智能标点）：`3.` 的 press1 照旧出英文 `.`，press2 换回中文 `。`。
//!   2. **模式进入键**：`;` 被快捷输入占用，模式内二次按下出 `；` 并武装，第三次按下换 `;`。
//!
//! 这里的每条用例都**先断言 press1 的产物**再断言 press2——press1 走错分支（如反向用例里
//! 出了中文 `。`）时必须当场炸，否则 press2 的断言会在「其实是正向流程」上侥幸通过，成为假绿。

use std::path::PathBuf;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::EVENT_KEY_DOWN;

const VK_OEM_1: u32 = 0xBA; // ;
const VK_OEM_COMMA: u32 = 0xBC; // ,
const VK_OEM_PERIOD: u32 = 0xBE; // .

fn data_dir() -> PathBuf {
    // 三级：crates/wind-coordinator → crates → wind_input → 仓库根（build_dev 在仓库根）。
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

/// 标点用例不碰引擎，但模式进入（快捷输入）要求方案目录在场。
fn has_data() -> bool {
    data_dir().join("schemas").exists()
}

fn cfg_smart() -> Config {
    let mut cfg = Config::default();
    cfg.input.default.chinese_mode = true;
    cfg.input.default.chinese_punct = true;
    cfg.input.symbol.smart_mode = true;
    cfg
}

fn press(coord: &Coordinator, vk: u32, prev_char: u16) -> KeyAction {
    coord.handle_key_event(&KeyEventData {
        key_code: vk,
        scan_code: 0,
        modifiers: 0,
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char,
    })
}

fn inserted(a: &KeyAction) -> Option<&str> {
    match a {
        KeyAction::InsertText { text, .. } => Some(text),
        _ => None,
    }
}

fn replaced(a: &KeyAction) -> Option<(u32, &str)> {
    match a {
        KeyAction::ReplaceBackward { count, text } => Some((*count, text)),
        _ => None,
    }
}

/// 反向主用例：光标前是数字 → press1 出英文（数字后智能语义不变），press2 换回中文。
/// 改造前这里 press1 之后就没有下文了——`smart_symbol_arm_str` 遇数字后智能直接不武装。
#[test]
fn after_digit_press1_english_then_press2_back_to_chinese() {
    let coord = Coordinator::new_headless(cfg_smart(), Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, b'5' as u16);
    assert_eq!(
        inserted(&a1),
        Some("."),
        "数字后 press1 必须仍出英文句点（数字后智能语义不变），实际: {:?}",
        a1
    );
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(
        replaced(&a2),
        Some((1, "。")),
        "时限内同键 press2 应把英文句点换成中文句号，实际: {:?}",
        a2
    );
}

/// 正向回归锁：非数字后照旧「press1 中文 → press2 英文」，方向维度不得污染既有语义。
#[test]
fn normal_press1_chinese_then_press2_english() {
    let coord = Coordinator::new_headless(cfg_smart(), Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, 0);
    assert_eq!(inserted(&a1), Some("。"), "实际: {:?}", a1);
    let a2 = press(&coord, VK_OEM_PERIOD, '。' as u16);
    assert_eq!(replaced(&a2), Some((1, ".")), "实际: {:?}", a2);
}

/// 总开关关闭时数字后行为**完全维持改造前**：press1 出英文 `.`，第二次按下只是普通标点追加
/// （此时光标前已是 `.` 而非数字，故出中文 `。`），**不得**出现任何 `ReplaceBackward`。
/// 屏上因此是 `3.。`——与开着开关时的 `3。`（替换）恰成对照，这正是该开关的全部差别。
#[test]
fn after_digit_without_smart_mode_never_replaces() {
    let mut cfg = cfg_smart();
    cfg.input.symbol.smart_mode = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, b'5' as u16);
    assert_eq!(inserted(&a1), Some("."), "实际: {:?}", a1);
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(
        replaced(&a2),
        None,
        "关掉智能符号总开关后不得有任何删改替换，实际: {:?}",
        a2
    );
    assert_eq!(inserted(&a2), Some("。"), "实际: {:?}", a2);
}

/// 反向只认 `punct.smart_list` 里的标点：把列表收窄成 "."，同样在数字后的 `,` 应走**正向**
/// （press1 中文 `，` → press2 英文 `,`）。锁住「方向由数字后智能判定，而非由 prev_char 是数字」。
#[test]
fn digit_context_outside_smart_list_stays_forward() {
    let mut cfg = cfg_smart();
    cfg.input.punct.smart_list = ".".to_string();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_COMMA, b'5' as u16);
    assert_eq!(
        inserted(&a1),
        Some("，"),
        "逗号不在 smart_list 里，数字后也该出中文，实际: {:?}",
        a1
    );
    let a2 = press(&coord, VK_OEM_COMMA, '，' as u16);
    assert_eq!(replaced(&a2), Some((1, ",")), "实际: {:?}", a2);
}

/// 需求 2 主用例：`;` 被快捷输入占用 → 进模式 → 模式内二次按下出 `；` 并武装 →
/// 第三次按下换英文 `;`（而不是又进一次模式）。
#[test]
fn mode_trigger_third_press_replaces_with_english() {
    if !has_data() {
        eprintln!("跳过：缺少 build_dev/data/schemas");
        return;
    }
    let coord = Coordinator::new_headless(cfg_smart(), Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_1, 0);
    assert!(
        matches!(a1, KeyAction::UpdateComposition { .. }),
        "第一次按 ; 应进入快捷输入模式，实际: {:?}",
        a1
    );
    let a2 = press(&coord, VK_OEM_1, 0);
    assert_eq!(
        inserted(&a2),
        Some("；"),
        "模式内二次按下应上屏中文分号并退出，实际: {:?}",
        a2
    );
    let a3 = press(&coord, VK_OEM_1, '；' as u16);
    assert_eq!(
        replaced(&a3),
        Some((1, ";")),
        "时限内第三次按下应替换为英文分号（须抢在模式激活之前），实际: {:?}",
        a3
    );
}

/// 需求 2 的门控：符号不在 `symbol.smart_chars` 里就不武装，第三次按下回到「再进一次模式」——
/// 与改造前行为一致（用户拍板：模式进入键仍受参与集合限制）。
#[test]
fn mode_trigger_not_in_smart_chars_keeps_old_behavior() {
    if !has_data() {
        eprintln!("跳过：缺少 build_dev/data/schemas");
        return;
    }
    let mut cfg = cfg_smart();
    cfg.input.symbol.smart_chars = "。，".to_string(); // 不含 ；
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press(&coord, VK_OEM_1, 0);
    let a2 = press(&coord, VK_OEM_1, 0);
    assert_eq!(inserted(&a2), Some("；"), "实际: {:?}", a2);
    let a3 = press(&coord, VK_OEM_1, '；' as u16);
    assert!(
        matches!(a3, KeyAction::UpdateComposition { .. }),
        "未武装时第三次按下应照旧进入模式，实际: {:?}",
        a3
    );
}

// ── 英文标点状态（中文输入 + 工具栏标点切英文，`english_punct_mode`）────────────────

fn cfg_en_punct() -> Config {
    let mut cfg = Config::default();
    cfg.input.default.chinese_mode = true;
    cfg.input.default.chinese_punct = false; // 标点切英文
    cfg.input.symbol.english_punct_mode = true;
    cfg
}

/// 英文标点状态：press1 出英文 `.`，时限内再按换成中文 `。`。
#[test]
fn english_punct_press1_english_then_press2_chinese() {
    let coord = Coordinator::new_headless(cfg_en_punct(), Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, 0);
    assert_eq!(
        inserted(&a1),
        Some("."),
        "英文标点状态 press1 应出英文句点，实际: {:?}",
        a1
    );
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(
        replaced(&a2),
        Some((1, "。")),
        "时限内 press2 应换成中文句号，实际: {:?}",
        a2
    );
}

/// 中文侧总开关与英文侧**互不影响**：只开 `smart_mode`（中文侧）时英文标点状态不该有替换。
#[test]
fn english_punct_requires_its_own_switch() {
    let mut cfg = cfg_en_punct();
    cfg.input.symbol.english_punct_mode = false;
    cfg.input.symbol.smart_mode = true; // 中文侧开着也不该外溢到英文标点状态
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, 0);
    assert_eq!(inserted(&a1), Some("."), "实际: {:?}", a1);
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(
        replaced(&a2),
        None,
        "英文侧开关关闭时不得有任何替换，实际: {:?}",
        a2
    );
}

/// 参与集合按**源字符**判定：把 `english_chars` 收窄成 ","，`.` 就不再参与。
#[test]
fn english_punct_outside_english_chars_not_armed() {
    let mut cfg = cfg_en_punct();
    cfg.input.symbol.english_chars = ",".to_string();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press(&coord, VK_OEM_PERIOD, 0);
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(replaced(&a2), None, "实际: {:?}", a2);
    // 同一份配置下逗号仍参与——证明上面的 None 是集合判定所致，而非整个开关没生效。
    press(&coord, VK_OEM_COMMA, 0);
    let b2 = press(&coord, VK_OEM_COMMA, ',' as u16);
    assert_eq!(replaced(&b2), Some((1, "，")), "实际: {:?}", b2);
}

// ── 英文输入模式（整个输入法切英文，`english_mode`）──────────────────────────────────

fn cfg_en_mode() -> Config {
    let mut cfg = Config::default();
    cfg.input.default.chinese_mode = false; // 英文输入模式
    cfg.input.symbol.english_mode = true;
    cfg
}

/// 英文输入模式：press1 出英文 `.`（此前这个键是直接透传给宿主的），press2 换中文 `。`。
/// 前置条件是 core 把 `english_chars` 并入了推给 DLL 的吃键集，否则引擎根本收不到这个键。
#[test]
fn english_mode_press1_english_then_press2_chinese() {
    let coord = Coordinator::new_headless(cfg_en_mode(), Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, 0);
    assert_eq!(
        inserted(&a1),
        Some("."),
        "英文模式 press1 应由 core 出英文句点（而非 PassThrough），实际: {:?}",
        a1
    );
    let a2 = press(&coord, VK_OEM_PERIOD, '.' as u16);
    assert_eq!(
        replaced(&a2),
        Some((1, "。")),
        "时限内 press2 应换成中文句号，实际: {:?}",
        a2
    );
}

/// 关掉 `english_mode`：标点键回到**透传**（吃键集为空，DLL 压根不吃、core 也不接手）。
/// 这条同时锁住「开关关闭 = 与历史行为完全一致」，是本功能不惊扰纯英文用户的底线。
#[test]
fn english_mode_off_passes_through() {
    let mut cfg = cfg_en_mode();
    cfg.input.symbol.english_mode = false;
    cfg.input.symbol.smart_mode = true; // 中文侧开着也不该外溢
    cfg.input.symbol.english_punct_mode = true; // 英文标点状态开着同样不该外溢到英文模式
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let a1 = press(&coord, VK_OEM_PERIOD, 0);
    assert!(
        matches!(a1, KeyAction::PassThrough),
        "关掉 english_mode 后标点键应透传，实际: {:?}",
        a1
    );
}

/// 超时后模式进入键必须**交还**给模式激活链：武装是有时限的劫持，不是永久接管。
#[test]
fn mode_trigger_after_timeout_enters_mode_again() {
    if !has_data() {
        eprintln!("跳过：缺少 build_dev/data/schemas");
        return;
    }
    let mut cfg = cfg_smart();
    cfg.input.symbol.smart_timeout_ms = 1;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press(&coord, VK_OEM_1, 0);
    let a2 = press(&coord, VK_OEM_1, 0);
    assert_eq!(inserted(&a2), Some("；"), "实际: {:?}", a2);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let a3 = press(&coord, VK_OEM_1, '；' as u16);
    assert!(
        matches!(a3, KeyAction::UpdateComposition { .. }),
        "超时后第三次按下应重新进入模式，实际: {:?}",
        a3
    );
}
