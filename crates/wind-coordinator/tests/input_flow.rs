//! 协调器输入流程端到端测试
//!
//! 覆盖基础功能目标：五笔/拼音基本输入流程 + 方案切换 + 中英切换。
//! 使用 `Coordinator::new_headless`（不启动 Win32 UI 线程），通过模拟按键事件
//! 断言返回的 `KeyAction`，验证整条"字母累积 → 候选 → 选词上屏"链路。
//!
//! 词典缺失时自动跳过（无数据 CI 环境）。

use std::path::PathBuf;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::{EVENT_KEY_DOWN, EVENT_KEY_UP};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_debug/data")
}

fn has_schemas() -> bool {
    let d = data_dir();
    let ok = |id: &str| {
        d.join(format!("schemas/{}.schema.toml", id)).exists()
            || d.join(format!("schemas/{}.schema.yaml", id)).exists()
    };
    ok("wubi86") && ok("pinyin")
}

fn config_with(active: &str) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86".into(), "pinyin".into()];
    cfg.schema.active = active.into();
    cfg.general.default_chinese_mode = true;
    cfg.hotkeys.toggle_mode_keys = vec!["lshift".into(), "rshift".into()];
    cfg.hotkeys.switch_engine = "ctrl+shift+e".into();
    cfg
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

/// 按下一个字母键（vk = ASCII 大写）
fn press_letter(coord: &Coordinator, c: char) -> KeyAction {
    let vk = (c.to_ascii_uppercase() as u32) & 0xFF;
    coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN))
}

fn action_text(action: &KeyAction) -> Option<String> {
    match action {
        KeyAction::UpdateComposition { text, .. } => Some(text.clone()),
        KeyAction::InsertText { text, .. } => Some(text.clone()),
        _ => None,
    }
}

#[test]
fn test_wubi_basic_input_and_commit() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86");
    assert!(coord.is_chinese_mode());

    // 累积 "aaaa"
    let mut last = KeyAction::PassThrough;
    for c in ['a', 'a', 'a', 'a'] {
        last = press_letter(&coord, c);
    }
    let preedit = action_text(&last).expect("应返回 UpdateComposition");
    // 组合区只显示编码，不含候选列表（候选在候选窗口）
    assert_eq!(preedit, "aaaa", "五笔组合区应只显示编码，实际: {}", preedit);

    // 空格上屏首选
    let commit = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match commit {
        KeyAction::InsertText { text, .. } => {
            assert!(!text.is_empty(), "上屏文本应非空");
            assert_eq!(text, "恭恭敬敬", "首选应为权重最高的 恭恭敬敬");
        }
        other => panic!("空格应上屏 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_wubi_number_select() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // "a" → 组合区显示编码 "a"，候选在候选窗口
    let act = press_letter(&coord, 'a');
    let preedit = action_text(&act).unwrap();
    assert_eq!(preedit, "a", "组合区应只显示编码 a，实际: {}", preedit);

    // 数字键 2 选第二个候选
    let commit = coord.handle_key_event(&key_event(0x32, EVENT_KEY_DOWN));
    match commit {
        KeyAction::InsertText { text, .. } => assert!(!text.is_empty()),
        other => panic!("数字键应上屏，实际: {:?}", other),
    }
}

#[test]
fn test_pinyin_basic_input() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "pinyin");

    let mut last = KeyAction::PassThrough;
    for c in "nihao".chars() {
        last = press_letter(&coord, c);
    }
    let preedit = action_text(&last).expect("应返回 UpdateComposition");
    // 拼音组合区显示音节分隔的拼音串，不含候选
    assert_eq!(preedit, "ni hao", "拼音组合区应显示 'ni hao'，实际: {}", preedit);

    // 空格上屏首选，应得到 你好
    let commit = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match commit {
        KeyAction::InsertText { text, .. } => {
            assert!(text.contains("你好"), "空格上屏应含 你好，实际: {}", text);
        }
        other => panic!("空格应上屏 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_schema_switch_via_menu() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86");

    coord.handle_menu_command("switch_engine");
    assert_eq!(coord.active_schema_id(), "pinyin", "切换后应为 pinyin");

    coord.handle_menu_command("switch_engine");
    assert_eq!(coord.active_schema_id(), "wubi86", "再切回 wubi86");
}

#[test]
fn test_schema_switch_clears_input() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 输入后切换方案应清空缓冲
    press_letter(&coord, 'a');
    coord.handle_menu_command("switch_engine");
    // 切换后再输入拼音，预编辑不应残留五笔内容
    let act = press_letter(&coord, 'n');
    let preedit = action_text(&act).unwrap_or_default();
    assert!(
        preedit.starts_with('n'),
        "切换后预编辑应从新输入 'n' 开始，实际: {}",
        preedit
    );
}

#[test]
fn test_chinese_punctuation() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert!(coord.is_chinese_mode());

    // 空缓冲下按 . (VK_OEM_PERIOD=0xBE) → 中文句号 。
    let act = coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN));
    match act {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "。"),
        other => panic!("应上屏中文句号，实际: {:?}", other),
    }
    // 逗号 , (0xBC) → ，
    match coord.handle_key_event(&key_event(0xBC, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "，"),
        other => panic!("应上屏中文逗号，实际: {:?}", other),
    }
    // Shift+1 = ! → ！
    let shifted = KeyEventData {
        key_code: 0x31,
        scan_code: 0,
        modifiers: 0x0001, // MOD_SHIFT
        event_type: EVENT_KEY_DOWN,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    };
    match coord.handle_key_event(&shifted) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "！"),
        other => panic!("Shift+1 应上屏中文叹号，实际: {:?}", other),
    }
}

#[test]
fn test_punct_commits_candidate_first() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 输入 aaaa（有候选），再按句号 → 先上屏首选候选，再接中文句号
    for _ in 0..4 {
        press_letter(&coord, 'a');
    }
    match coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert!(text.ends_with("。"), "应以中文句号结尾，实际: {}", text);
            assert!(text.chars().count() >= 2, "应包含上屏候选+句号，实际: {}", text);
        }
        other => panic!("应上屏候选+句号，实际: {:?}", other),
    }
}

#[test]
fn test_arrow_down_then_space_selects_highlighted() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // "a" 在五笔下有多个候选
    press_letter(&coord, 'a');
    let texts = coord.debug_page_texts();
    if texts.len() < 2 {
        eprintln!("跳过：当前页候选不足 2 个");
        return;
    }
    let second = texts[1].clone();

    // 初始高亮在第 0 项
    let (_, sel0, _) = coord.debug_page_info();
    assert_eq!(sel0, 0, "初始高亮应为第 0 项");

    // 下方向键 → 高亮移到第 1 项
    coord.handle_key_event(&key_event(0x28, EVENT_KEY_DOWN));
    let (_, sel1, _) = coord.debug_page_info();
    assert_eq!(sel1, 1, "下方向键后高亮应为第 1 项");

    // 空格上屏高亮项（第 2 个候选）
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, second, "空格应上屏高亮的第 2 个候选");
        }
        other => panic!("空格应上屏高亮候选，实际: {:?}", other),
    }
}

#[test]
fn test_page_down_changes_page_and_renumbers() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let (_, _, total_pages) = coord.debug_page_info();
    if total_pages < 2 {
        eprintln!("跳过：候选不足两页");
        return;
    }
    let page1_first = coord.debug_page_texts()[0].clone();

    // PageDown(0x22) → 翻到第 2 页
    coord.handle_key_event(&key_event(0x22, EVENT_KEY_DOWN));
    let (page, sel, _) = coord.debug_page_info();
    assert_eq!(page, 1, "PageDown 后应在第 2 页（0-based=1）");
    assert_eq!(sel, 0, "翻页后高亮应归零");

    let page2_first = coord.debug_page_texts()[0].clone();
    assert_ne!(page1_first, page2_first, "第 2 页首项应不同于第 1 页首项");

    // 第 2 页按数字键 '1' → 上屏第 2 页的首项（编号重置）
    match coord.handle_key_event(&key_event(0x31, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, page2_first, "第 2 页数字键 1 应上屏第 2 页首项");
        }
        other => panic!("数字键应上屏，实际: {:?}", other),
    }
}

#[test]
fn test_page_up_wraps_at_first_page() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    // 第 1 页按 PageUp 应保持在第 1 页（不越界）
    coord.handle_key_event(&key_event(0x21, EVENT_KEY_DOWN));
    let (page, _, _) = coord.debug_page_info();
    assert_eq!(page, 0, "首页 PageUp 应仍在首页");
}

#[test]
fn test_minus_equal_paging_when_candidates() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let (_, _, total_pages) = coord.debug_page_info();
    if total_pages < 2 {
        return;
    }
    // '=' (0xBB) 下一页
    coord.handle_key_event(&key_event(0xBB, EVENT_KEY_DOWN));
    assert_eq!(coord.debug_page_info().0, 1, "'=' 应翻到下一页");
    // '-' (0xBD) 上一页
    coord.handle_key_event(&key_event(0xBD, EVENT_KEY_DOWN));
    assert_eq!(coord.debug_page_info().0, 0, "'-' 应翻回上一页");
}

#[test]
fn test_mode_toggle_via_shift() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert!(coord.is_chinese_mode());

    // TSF 吃掉 toggle 键的 keydown、仅在干净单击后于 keyUp 转发，故服务端收到 keyUp 即切换。
    coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    assert!(!coord.is_chinese_mode(), "左 Shift 释放应切到英文");

    // 英文模式下字母透传
    let act = press_letter(&coord, 'a');
    assert!(matches!(act, KeyAction::PassThrough), "英文模式字母应透传");

    // 再切回中文（右 Shift 也应生效）
    coord.handle_key_event(&key_event(0xA1, EVENT_KEY_UP));
    assert!(coord.is_chinese_mode(), "右 Shift 释放应切回中文");
}
