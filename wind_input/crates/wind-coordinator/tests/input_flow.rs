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
    cfg.input.default.chinese_mode = true;
    cfg.keys.toggle_mode_keys = vec!["lshift".into(), "rshift".into()];
    cfg.keys.switch_engine = "ctrl+shift+e".into();
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

fn key_event_mods(key_code: u32, event_type: u8, modifiers: u32) -> KeyEventData {
    KeyEventData {
        modifiers,
        ..key_event(key_code, event_type)
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
fn test_url_mode_enter_and_commit() {
    if !has_schemas() {
        return;
    }
    // #11 网址输入：打满前缀 "www." 夺取进入网址模式，续打累积，空格上屏原文。
    let mut cfg = config_with("wubi86");
    cfg.input.url.enabled = true;
    cfg.input.url.prefixes = vec!["www.".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // w w w . → 进入网址模式（最后一键补满前缀）
    press_letter(&coord, 'w');
    press_letter(&coord, 'w');
    press_letter(&coord, 'w');
    let enter = coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)); // VK_OEM_PERIOD '.'
    match &enter {
        KeyAction::UpdateComposition { text, .. } => {
            assert_eq!(text, "www.", "进入网址模式组合区应为 www.，实际: {}", text);
        }
        other => panic!(
            "打满 www. 应进入网址模式(UpdateComposition)，实际: {:?}",
            other
        ),
    }

    // 续打 g o → 缓冲累积（网址字符不上屏）
    press_letter(&coord, 'g');
    let acc = press_letter(&coord, 'o');
    match &acc {
        KeyAction::UpdateComposition { text, .. } => {
            assert_eq!(text, "www.go", "网址续打应累积，实际: {}", text);
        }
        other => panic!("网址续打应 UpdateComposition，实际: {:?}", other),
    }

    // 空格上屏原文
    let commit = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match commit {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "www.go", "网址空格应上屏原文，实际: {}", text);
        }
        other => panic!("网址空格应上屏 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_pin_candidate_hotkey_consumed_and_gated() {
    if !has_schemas() {
        return;
    }
    use wind_ipc::protocol::MOD_CTRL;
    // #12 候选热键：默认 pin=ctrl+number。有候选+有输入码时 Ctrl+2 消费按键（置顶第2候选）；
    // 无组合时 Ctrl+2 不应被当作候选热键吞掉（透传给应用）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));

    // 有组合：输入 "aaaa" 产生候选，Ctrl+2 → Consumed
    for c in ['a', 'a', 'a', 'a'] {
        press_letter(&coord, c);
    }
    let pin = coord.handle_key_event(&key_event_mods(0x32, EVENT_KEY_DOWN, MOD_CTRL));
    assert!(
        matches!(pin, KeyAction::Consumed),
        "有候选时 Ctrl+2 应被候选热键消费，实际: {:?}",
        pin
    );

    // Ctrl+0 → 第 10 候选（候选窗最大 10 项），同样应被消费（范围校验在 candidate_op 内）。
    let pin0 = coord.handle_key_event(&key_event_mods(0x30, EVENT_KEY_DOWN, MOD_CTRL));
    assert!(
        matches!(pin0, KeyAction::Consumed),
        "Ctrl+0 应作为第 10 候选热键被消费，实际: {:?}",
        pin0
    );

    // 无组合：另起 coordinator，未输入任何码，Ctrl+2 → 不消费（PassThrough）
    let coord2 = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    let no_comp = coord2.handle_key_event(&key_event_mods(0x32, EVENT_KEY_DOWN, MOD_CTRL));
    assert!(
        matches!(no_comp, KeyAction::PassThrough),
        "无组合时 Ctrl+2 不应被候选热键吞掉，实际: {:?}",
        no_comp
    );
}

#[test]
fn test_delete_candidate_hotkey_shift_gating() {
    if !has_schemas() {
        return;
    }
    use wind_ipc::protocol::{MOD_CTRL, MOD_SHIFT};
    // 默认 delete=ctrl+shift+number。有候选时 Ctrl+Shift+3 消费（删除第3候选）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    for c in ['a', 'a', 'a', 'a'] {
        press_letter(&coord, c);
    }
    let del = coord.handle_key_event(&key_event_mods(0x33, EVENT_KEY_DOWN, MOD_CTRL | MOD_SHIFT));
    assert!(
        matches!(del, KeyAction::Consumed),
        "有候选时 Ctrl+Shift+3 应被删除热键消费，实际: {:?}",
        del
    );
}

#[test]
fn test_overflow_number_key_ignore_default() {
    if !has_schemas() {
        return;
    }
    // 默认 overflow.number_key = "ignore"：数字键越界当前页候选时吞键无效（保留组合）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let count = coord.debug_candidate_count();
    if count == 0 || count >= 9 {
        return; // 保证数字 9 必然越界
    }
    let act = coord.handle_key_event(&key_event(0x39, EVENT_KEY_DOWN)); // 主键盘 9
    assert!(
        matches!(act, KeyAction::Consumed),
        "默认 ignore 下越界数字键应吞键(Consumed)，实际: {:?}",
        act
    );
}

#[test]
fn test_overflow_number_key_commit_and_input() {
    if !has_schemas() {
        return;
    }
    // overflow.number_key = "commit_and_input"：越界时顶字上屏高亮候选 + 追加数字字符。
    let mut cfg = config_with("wubi86");
    cfg.keys.overflow.number_key = "commit_and_input".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord, 'a');
    let count = coord.debug_candidate_count();
    if count == 0 || count >= 9 {
        return;
    }
    let act = coord.handle_key_event(&key_event(0x39, EVENT_KEY_DOWN)); // 越界数字 9
    match act {
        KeyAction::InsertText { text, .. } => {
            assert!(
                text.ends_with('9'),
                "commit_and_input 应以越界数字 9 结尾，实际: {}",
                text
            );
            assert!(
                text.chars().count() >= 2,
                "应为高亮候选 + 数字，实际: {}",
                text
            );
        }
        other => panic!("commit_and_input 应 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_numpad_direct_outputs_digit() {
    if !has_schemas() {
        return;
    }
    // 默认 numpad_behavior 为空 → direct：丢弃编码直接输出小键盘数字。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a'); // 产生组合 + 候选
    // 小键盘 5 (VK_NUMPAD5 = 0x65)
    let act = coord.handle_key_event(&key_event(0x65, EVENT_KEY_DOWN));
    match act {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(
                text, "5",
                "direct 模式小键盘应直接输出数字 5，实际: {}",
                text
            );
        }
        other => panic!("direct 小键盘应 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_numpad_follow_main_selects_like_main() {
    if !has_schemas() {
        return;
    }
    // follow_main：小键盘数字键应与主键盘数字键选同一候选。
    let mut cfg = config_with("wubi86");
    cfg.input.numpad_behavior = "follow_main".into();
    let coord_np = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord_np, 'a');
    // 小键盘 2 (VK_NUMPAD2 = 0x62)
    let np = coord_np.handle_key_event(&key_event(0x62, EVENT_KEY_DOWN));

    // 对照：主键盘 2 (0x32) 选第二候选
    let coord_main = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord_main, 'a');
    let main = coord_main.handle_key_event(&key_event(0x32, EVENT_KEY_DOWN));

    let np_text = action_text(&np).unwrap_or_default();
    let main_text = action_text(&main).unwrap_or_default();
    assert!(!np_text.is_empty(), "follow_main 小键盘 2 应上屏候选");
    assert_eq!(
        np_text, main_text,
        "follow_main 小键盘 2 应与主键盘 2 选同一候选（np={}, main={}）",
        np_text, main_text
    );
}

#[test]
fn test_numpad_follow_main_empty_passthrough() {
    if !has_schemas() {
        return;
    }
    // follow_main + 空缓冲：小键盘数字应透传（由应用原样输出数字），不被 IME 吞。
    let mut cfg = config_with("wubi86");
    cfg.input.numpad_behavior = "follow_main".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0x67, EVENT_KEY_DOWN)); // VK_NUMPAD7
    assert!(
        matches!(act, KeyAction::PassThrough),
        "follow_main 空缓冲小键盘数字应 PassThrough，实际: {:?}",
        act
    );
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
    assert_eq!(
        preedit, "ni hao",
        "拼音组合区应显示 'ni hao'，实际: {}",
        preedit
    );

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
            assert!(
                text.chars().count() >= 2,
                "应包含上屏候选+句号，实际: {}",
                text
            );
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
fn test_second_third_candidate_keys() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let texts = coord.debug_page_texts();
    if texts.len() < 3 {
        eprintln!("跳过：当前页候选不足 3 个");
        return;
    }
    let second = texts[1].clone();

    // 分号(;, VK_OEM_1=0xBA) → 上屏第 2 个候选
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, second, "分号应上屏第 2 个候选");
        }
        other => panic!("分号应上屏次选候选，实际: {:?}", other),
    }

    // 重新输入，引号(', VK_OEM_7=0xDE) → 上屏第 3 个候选
    press_letter(&coord, 'a');
    let texts2 = coord.debug_page_texts();
    let third = texts2[2].clone();
    match coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, third, "引号应上屏第 3 个候选");
        }
        other => panic!("引号应上屏三选候选，实际: {:?}", other),
    }
}

#[test]
fn test_empty_buffer_semicolon_enters_quick_input() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    // 空缓冲下按分号 → 进入快捷输入（分号是默认快捷输入触发键），组合区前缀 ";"
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::UpdateComposition { text, .. } => {
            assert_eq!(text, ";", "空缓冲分号应进入快捷输入显示前缀");
        }
        other => panic!("空缓冲分号应进入快捷输入，实际: {:?}", other),
    }
}

#[test]
fn test_temp_pinyin_backtick_trigger_and_commit() {
    if !has_schemas() {
        return;
    }
    // 五笔方案下，反引号(`, VK_OEM_3=0xC0)触发临时拼音
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86");

    // 按反引号进入临时拼音，组合区应显示前缀 "`"
    let act = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    let preedit = action_text(&act).expect("反引号应进入临时拼音并返回组合区");
    assert_eq!(preedit, "`", "进入临时拼音组合区应为前缀 `");

    // 输入拼音 nihao
    let mut last = act;
    for c in "nihao".chars() {
        last = press_letter(&coord, c);
    }
    let preedit = action_text(&last).unwrap();
    assert_eq!(
        preedit, "`ni hao",
        "临时拼音组合区应为 `ni hao，实际: {}",
        preedit
    );

    // 候选应来自拼音引擎（含 你好）
    let texts = coord.debug_page_texts();
    assert!(
        texts.iter().any(|t| t.contains("你好")),
        "临时拼音候选应含 你好，实际: {:?}",
        texts
    );

    // 空格上屏首选并退出临时拼音
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert!(text.contains("你好"), "应上屏 你好，实际: {}", text);
        }
        other => panic!("空格应上屏候选，实际: {:?}", other),
    }

    // 退出后五笔输入应恢复正常（输入 a 显示编码 a）
    let act = press_letter(&coord, 'a');
    assert_eq!(action_text(&act).unwrap(), "a", "退出临时拼音后五笔应正常");
}

#[test]
fn test_temp_pinyin_commit_and_enter_with_candidates() {
    if !has_schemas() {
        return;
    }
    // 五笔下已有候选时按反引号 → 顶屏高亮候选 + 进入临时拼音
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let first = coord.debug_page_texts()[0].clone();

    // 反引号：应上屏当前高亮候选并原子开启临时拼音组合
    match coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)) {
        KeyAction::InsertText {
            text,
            new_composition,
            has_new_composition,
            ..
        } => {
            assert_eq!(text, first, "应顶屏当前高亮候选");
            assert!(has_new_composition, "应原子开启新组合");
            assert_eq!(
                new_composition.as_deref(),
                Some("`"),
                "新组合应为临时拼音前缀"
            );
        }
        other => panic!("有候选按反引号应顶屏+进临时拼音，实际: {:?}", other),
    }

    // 现已在临时拼音模式：输入拼音 nihao 应得拼音候选
    let mut last = KeyAction::PassThrough;
    for c in "nihao".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(action_text(&last).unwrap(), "`ni hao", "应处于临时拼音模式");
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert!(text.contains("你好")),
        other => panic!("空格应上屏拼音候选，实际: {:?}", other),
    }
}

#[test]
fn test_temp_pinyin_esc_exits() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    press_letter(&coord, 'n');
    // Esc 退出
    match coord.handle_key_event(&key_event(0x1B, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("Esc 应清空组合区退出，实际: {:?}", other),
    }
    // 退出后五笔正常
    let act = press_letter(&coord, 'a');
    assert_eq!(action_text(&act).unwrap(), "a");
}

#[test]
fn test_temp_pinyin_not_triggered_in_pinyin_mode() {
    if !has_schemas() {
        return;
    }
    // 拼音方案下反引号不应触发临时拼音（仅码表方案启用）
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    // 应作为标点处理（反引号→ 不在中文标点表则原样/全角），不应进入临时拼音前缀
    let txt = action_text(&act).unwrap_or_default();
    assert_ne!(txt, "`ni", "拼音方案不应进入临时拼音");
}

/// 按下一个字符键（vk + 可选 shift）
fn press_vk(coord: &Coordinator, vk: u32, shift: bool) -> KeyAction {
    let mut ev = key_event(vk, EVENT_KEY_DOWN);
    if shift {
        ev.modifiers = 0x0001;
    }
    coord.handle_key_event(&ev)
}

#[test]
fn test_quick_input_calc() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 分号(;, VK_OEM_1=0xBA)进入快捷输入，组合区前缀 ";"
    let act = coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN));
    assert_eq!(action_text(&act).unwrap(), ";", "分号应进入快捷输入");

    // 输入 1+2*3：1(0x31) +(Shift+=,0xBB) 2(0x32) *(Shift+8,0x38) 3(0x33)
    press_vk(&coord, 0x31, false);
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x32, false);
    press_vk(&coord, 0x38, true); // *
    let last = press_vk(&coord, 0x33, false);
    assert_eq!(action_text(&last).unwrap(), ";1+2*3", "组合区应为 ;1+2*3");

    // 首选候选应为 "1+2*3=7"
    let texts = coord.debug_page_texts();
    assert_eq!(
        texts[0], "1+2*3=7",
        "计算器首选应为表达式=结果，实际: {:?}",
        texts
    );

    // 字母 a 选第 1 个候选上屏
    match press_vk(&coord, 0x41, false) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "1+2*3=7"),
        other => panic!("字母 a 应上屏首选，实际: {:?}", other),
    }
}

#[test]
fn test_quick_input_date_space_commits() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    // 输入 2025.12.25
    for vk in [0x32, 0x30, 0x32, 0x35] {
        press_vk(&coord, vk, false);
    }
    press_vk(&coord, 0xBE, false); // .
    for vk in [0x31, 0x32] {
        press_vk(&coord, vk, false);
    }
    press_vk(&coord, 0xBE, false); // .
    for vk in [0x32, 0x35] {
        press_vk(&coord, vk, false);
    }
    let texts = coord.debug_page_texts();
    assert!(
        texts.iter().any(|t| t == "2025年12月25日"),
        "日期候选应含 2025年12月25日，实际: {:?}",
        texts
    );
    // 空格上屏高亮（首选 20251225）
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "20251225"),
        other => panic!("空格应上屏日期首选，实际: {:?}", other),
    }
}

#[test]
fn test_quick_input_double_semicolon_outputs_literal() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入
    // 再按 ; → 按标点配置上屏（默认中文标点 → ；）并退出
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "；", "双分号应按中文标点上屏 ；"),
        other => panic!("双分号应上屏标点，实际: {:?}", other),
    }
    // 退出后五笔正常
    let act = press_letter(&coord, 'a');
    assert_eq!(action_text(&act).unwrap(), "a");
}

#[test]
fn test_quick_input_esc_exits() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN));
    press_vk(&coord, 0x31, false);
    match coord.handle_key_event(&key_event(0x1B, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("Esc 应退出快捷输入，实际: {:?}", other),
    }
}

#[test]
fn test_semicolon_still_selects_second_candidate_with_candidates() {
    if !has_schemas() {
        return;
    }
    // 有候选时分号仍应作二三候选（不进入快捷输入）
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let texts = coord.debug_page_texts();
    if texts.len() < 2 {
        return;
    }
    let second = texts[1].clone();
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, second, "有候选时分号应选第 2 个候选");
        }
        other => panic!("有候选时分号应作二三候选，实际: {:?}", other),
    }
}

#[test]
fn test_phrase_date_expansion() {
    if !has_schemas() {
        return;
    }
    // 输入 "date" → 短语层应展开当前日期候选（如 2026年6月14日）
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    for c in "date".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    // 短语高权重 → 应在候选中且靠前；校验存在「年…月…日」格式
    let has_date_phrase = texts
        .iter()
        .any(|t| t.contains('年') && t.contains('月') && t.contains('日'));
    assert!(
        has_date_phrase,
        "输入 date 应出现日期短语候选，实际: {:?}",
        texts
    );
}

#[test]
fn test_phrase_time_expansion() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    for c in "time".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    // 时间短语 $HH:$mm:$ss → 含冒号的时间串
    let has_time = texts
        .iter()
        .any(|t| t.matches(':').count() >= 1 && t.chars().any(|c| c.is_ascii_digit()));
    assert!(has_time, "输入 time 应出现时间短语候选，实际: {:?}", texts);
}

#[test]
fn test_s2t_converts_committed_candidate() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    if !coord.debug_set_s2t(true) {
        eprintln!("跳过：缺少 opencc 数据");
        return;
    }
    // 拼音输入 hanzi → 候选含 汉字；开启简繁后上屏应为 漢字
    for c in "hanzi".chars() {
        press_letter(&coord, c);
    }
    // 找到"汉字"所在候选位置并用数字键选择；若首选即是则空格
    let texts = coord.debug_page_texts();
    let pos = texts.iter().position(|t| t == "汉字");
    let commit = if let Some(p) = pos {
        // 数字键 (p+1)
        coord.handle_key_event(&key_event(0x31 + p as u32, EVENT_KEY_DOWN))
    } else {
        // 退化：直接空格上屏首选，仅校验为繁体（不强等于）
        coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN))
    };
    match commit {
        KeyAction::InsertText { text, .. } => {
            if pos.is_some() {
                assert_eq!(text, "漢字", "开启简繁后 汉字 应上屏为 漢字");
            } else {
                // 至少不应是简体"汉字"
                assert_ne!(text, "汉字");
            }
        }
        other => panic!("应上屏 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_s2t_converts_candidate_display() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    if !coord.debug_set_s2t(true) {
        eprintln!("跳过：缺少 opencc 数据");
        return;
    }
    for c in "hanzi".chars() {
        press_letter(&coord, c);
    }
    // 内部候选仍是简体（供词频/匹配）
    let internal = coord.debug_page_texts();
    // 显示文本应为繁体
    let display = coord.debug_page_display_texts();
    if let Some(p) = internal.iter().position(|t| t == "汉字") {
        assert_eq!(display[p], "漢字", "候选显示应为繁体 漢字");
    } else {
        eprintln!("跳过：候选未含 汉字");
    }
    // 简体与显示长度一致、且至少有一项被转换
    assert_eq!(internal.len(), display.len());
    assert!(
        internal.iter().zip(&display).any(|(a, b)| a != b),
        "开启简繁后显示应有候选被转换"
    );
}

#[test]
fn test_s2t_disabled_keeps_simplified() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    // 默认关闭简繁：上屏保持简体
    for c in "hanzi".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if let Some(p) = texts.iter().position(|t| t == "汉字") {
        match coord.handle_key_event(&key_event(0x31 + p as u32, EVENT_KEY_DOWN)) {
            KeyAction::InsertText { text, .. } => assert_eq!(text, "汉字", "默认应保持简体"),
            other => panic!("应上屏，实际: {:?}", other),
        }
    }
}

#[test]
fn test_smart_punct_after_digit() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 句号(0xBE)，光标前字符为数字 '5'(0x35) → 应输出英文 '.'
    let mut ev = key_event(0xBE, EVENT_KEY_DOWN);
    ev.prev_char = '5' as u16;
    match coord.handle_key_event(&ev) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, ".", "数字后句号应为英文 ."),
        other => panic!("应上屏英文句号，实际: {:?}", other),
    }
    // 光标前为非数字（'a'）→ 应为中文句号 。
    let mut ev2 = key_event(0xBE, EVENT_KEY_DOWN);
    ev2.prev_char = 'a' as u16;
    match coord.handle_key_event(&ev2) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "。", "字母后句号应为中文 。"),
        other => panic!("应上屏中文句号，实际: {:?}", other),
    }
    // 逗号(0xBC)数字后 → 英文 ','
    let mut ev3 = key_event(0xBC, EVENT_KEY_DOWN);
    ev3.prev_char = '9' as u16;
    match coord.handle_key_event(&ev3) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, ",", "数字后逗号应为英文 ,"),
        other => panic!("应上屏英文逗号，实际: {:?}", other),
    }
}

#[test]
fn test_dynamic_paging_expands_candidates() {
    if !has_schemas() {
        return;
    }
    // 单字母前缀通常有大量候选：旧实现固定封顶 50，新实现按前缀加载全部（≥初始上限再分级扩展）
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a');
    let initial = coord.debug_candidate_count();
    // 核心修复：不再固定截断到 50（'a' 前缀候选应远超旧上限）
    assert!(
        initial > 50,
        "应加载超过旧固定上限(50)的全部前缀候选，实际: {}",
        initial
    );

    // 若仍达到初始分级上限，翻页到边界应动态扩展加载更多
    if coord.debug_has_more() {
        for _ in 0..15 {
            coord.handle_key_event(&key_event(0x22, EVENT_KEY_DOWN)); // PageDown
        }
        let expanded = coord.debug_candidate_count();
        assert!(
            expanded > initial,
            "翻页到边界应动态加载更多候选: {} -> {}",
            initial,
            expanded
        );
    }
}

/// 按下 Shift+字母
fn press_shift_letter(coord: &Coordinator, c: char) -> KeyAction {
    let vk = (c.to_ascii_uppercase() as u32) & 0xFF;
    let mut ev = key_event(vk, EVENT_KEY_DOWN);
    ev.modifiers = 0x0001; // MOD_SHIFT
    coord.handle_key_event(&ev)
}

#[test]
fn test_temp_english_shift_letter_commit() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // Shift+H 进入临时英文，首字母大写
    let act = press_shift_letter(&coord, 'h');
    assert_eq!(
        action_text(&act).unwrap(),
        "H",
        "Shift+H 应进入临时英文显示 H"
    );

    // 续输 ello（无 Shift → 小写）
    let mut last = act;
    for c in "ello".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(action_text(&last).unwrap(), "Hello", "组合区应为 Hello");

    // 空格上屏
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "Hello"),
        other => panic!("空格应上屏 Hello，实际: {:?}", other),
    }
    // 退出后五笔恢复正常
    let act = press_letter(&coord, 'a');
    assert_eq!(action_text(&act).unwrap(), "a");
}

#[test]
fn test_temp_english_digits_and_punct() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'v'); // V
    press_letter(&coord, 'e');
    press_letter(&coord, 'r');
    // 数字入缓冲
    press_vk(&coord, 0x32, false); // 2
    let last = press_letter(&coord, 'b');
    assert_eq!(action_text(&last).unwrap(), "Ver2b", "数字应入缓冲");
    // 句号(0xBE)：上屏缓冲 + 中文句号（默认中文标点）
    match coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "Ver2b。", "应上屏缓冲+中文句号"),
        other => panic!("标点应上屏缓冲+标点，实际: {:?}", other),
    }
}

#[test]
fn test_temp_english_esc_exits() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'a');
    match coord.handle_key_event(&key_event(0x1B, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("Esc 应退出临时英文，实际: {:?}", other),
    }
}

fn config_mixed() -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["wubi86_pinyin".into(), "wubi86".into(), "pinyin".into()];
    cfg.schema.active = "wubi86_pinyin".into();
    cfg.input.default.chinese_mode = true;
    cfg
}

#[test]
fn test_mixed_wubi_exact_priority() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_mixed(), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86_pinyin");
    // 五笔精确码 aaaa（+10M）应压过拼音候选排首位
    for _ in 0..4 {
        press_letter(&coord, 'a');
    }
    let texts = coord.debug_page_texts();
    assert!(!texts.is_empty(), "混输应有候选");
    assert_eq!(
        texts[0],
        "恭恭敬敬",
        "五笔精确匹配应排首位，实际: {:?}",
        &texts[..texts.len().min(3)]
    );
}

#[test]
fn test_mixed_pinyin_supplement() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_mixed(), Some(&data_dir()));
    // 输入 nihao（拼音）→ 次引擎应补充拼音候选 你好
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    assert!(
        texts.iter().any(|t| t.contains("你好")),
        "混输应含拼音补充候选 你好，实际: {:?}",
        &texts[..texts.len().min(8)]
    );
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

#[test]
fn test_candidate_op_move_top_and_delete() {
    if !has_schemas() {
        return;
    }
    use wind_ui::manager::CandidateOp;
    // candidate_op 的置顶/删除经 self.store 持久化 Shadow 规则，故需注入真实 store
    // （new_headless 的 store=None 会让 pin/delete 变空操作）。
    let store_path = std::env::temp_dir().join("wind_candidate_op_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord =
        Coordinator::new_headless_with_store(config_with("pinyin"), Some(&data_dir()), store);
    // 拼音输入若干字母以获取多个候选
    for c in "shi".chars() {
        press_letter(&coord, c);
    }
    let before = coord.debug_page_texts();
    if before.len() < 2 {
        return; // 候选不足，跳过
    }
    let second = before[1].clone();

    // 置顶第二项 → 应成为首项
    coord.debug_candidate_op(CandidateOp::MoveTop, 1);
    let after = coord.debug_page_texts();
    assert_eq!(after.first(), Some(&second), "置顶后第二项应排首位");

    // 删除一个多字候选（绕过单字保护）→ 应从候选中消失
    if let Some((pl, w)) = after
        .iter()
        .enumerate()
        .find(|(_, w)| w.chars().count() >= 2)
        .map(|(i, w)| (i, w.clone()))
    {
        coord.debug_candidate_op(CandidateOp::Delete, pl);
        let after2 = coord.debug_page_texts();
        assert!(!after2.contains(&w), "删除后 '{}' 不应再出现", w);
    }
}

#[test]
fn test_candidate_op_delete_single_char_protected() {
    if !has_schemas() {
        return;
    }
    use wind_ui::manager::CandidateOp;
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "shi".chars() {
        press_letter(&coord, c);
    }
    let before = coord.debug_page_texts();
    // 找一个单字候选，删除应被拒绝（仍在列表）
    if let Some((pl, w)) = before
        .iter()
        .enumerate()
        .find(|(_, w)| w.chars().count() == 1)
        .map(|(i, w)| (i, w.clone()))
    {
        coord.debug_candidate_op(CandidateOp::Delete, pl);
        let after = coord.debug_page_texts();
        assert!(after.contains(&w), "单字 '{}' 删除应被保护", w);
    }
}

#[test]
fn test_web_schema_get_config_and_encode_real() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    // schema.getConfig：三层合并视图，应含 schema/engine 段（无 override 时即基础方案）。
    let cfg = coord
        .web_data_rpc("schema.getConfig", &serde_json::json!({ "id": "pinyin" }))
        .unwrap();
    assert!(cfg.is_object(), "getConfig 应返回对象");
    assert!(cfg.get("schema").is_some(), "应含 schema 段");
    assert!(cfg.get("engine").is_some(), "应含 engine 段");
    // dict.encode：拼音方案出拼音码；dict.genPinyin 同源。
    let code = coord
        .web_data_rpc(
            "dict.encode",
            &serde_json::json!({ "schemaId": "pinyin", "text": "你好" }),
        )
        .unwrap();
    assert!(code.is_string(), "encode 应返回字符串");
}

#[test]
fn test_web_theme_preview_real() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    // theme.preview：内置 default 主题合并 base 链后的配置（只读）。
    let prev = coord
        .web_data_rpc("theme.preview", &serde_json::json!({ "name": "default" }))
        .unwrap();
    assert!(prev.is_object(), "preview 应返回对象");
    // theme.list 至少含若干内置主题
    let list = coord
        .web_data_rpc("theme.list", &serde_json::json!({}))
        .unwrap();
    assert!(
        list.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "应列出内置主题"
    );
}

#[test]
fn test_stats_recorded_through_deferred_policed() {
    // 回归：生产链路是 bridge → DeferredHandler → Coordinator，bridge 调 handle_key_event_policed。
    // 若 DeferredHandler 不转发 policed，则 Coordinator 的统计埋点被跳过、上屏计数恒为 0。
    // 本测试经 DeferredHandler 走完整 policed 链路，断言 store 真实记录了上屏中文字数。
    if !has_schemas() {
        return;
    }
    use std::sync::Arc;
    use wind_bridge::deferred::DeferredHandler;

    let store_path = std::env::temp_dir().join("wind_stats_deferred_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord = Coordinator::new_headless_with_store(
        config_with("pinyin"),
        Some(&data_dir()),
        store.clone(),
    );
    let deferred = DeferredHandler::new();
    deferred.set_ready(coord);

    // 经 policed 输入 "nihao" + 空格 → 上屏 你好
    for c in "nihao".chars() {
        let vk = (c.to_ascii_uppercase() as u32) & 0xFF;
        deferred.handle_key_event_policed(&key_event(vk, EVENT_KEY_DOWN));
    }
    let commit = deferred.handle_key_event_policed(&key_event(0x20, EVENT_KEY_DOWN));
    assert!(
        matches!(commit, KeyAction::InsertText { .. }),
        "空格应上屏 InsertText，实际: {:?}",
        commit
    );

    // 统计应经 policed 链路真实落库（features.stats.enabled 默认 true）。
    let all = store.daily_stats("2000-01-01", "2099-12-31").unwrap();
    let chinese: u32 = all.iter().map(|(_, r)| r.chinese).sum();
    assert!(
        chinese >= 2,
        "上屏'你好'应记 ≥2 个中文字，实际 chinese={}（policed 埋点未触达？）",
        chinese
    );
    let _ = std::fs::remove_file(&store_path);
}

// ---- select_key overflow（次/三选键越界，对齐 Go handleOverflowSelectKey）----
// 触发场景：五笔 "qqqq" 仅 2 个候选 ["金","狗狗"]，按三选键 '（VK_OEM_7）→ idx=2 越界。

#[test]
fn test_overflow_select_key_ignore_default() {
    if !has_schemas() {
        return;
    }
    // 默认 overflow.select_key = "ignore"：三选键越界（页内候选 < 3）时吞键无效。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let count = coord.debug_candidate_count();
    if count == 0 || count >= 3 {
        return; // 需 < 3 才能让 '（三选）越界
    }
    let act = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN)); // ' VK_OEM_7
    assert!(
        matches!(act, KeyAction::Consumed),
        "默认 ignore 下三选键越界应吞键(Consumed)，实际: {:?}",
        act
    );
}

#[test]
fn test_overflow_select_key_commit() {
    if !has_schemas() {
        return;
    }
    // overflow.select_key = "commit"：越界时只上屏当前高亮候选，不追加触发键字符。
    let mut cfg = config_with("wubi86");
    cfg.keys.overflow.select_key = "commit".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() || texts.len() >= 3 {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, highlighted, "commit 应只上屏高亮候选，无追加字符");
        }
        other => panic!("commit 应 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_overflow_select_key_commit_and_input() {
    if !has_schemas() {
        return;
    }
    // overflow.select_key = "commit_and_input"：越界时上屏高亮候选 + 追加（转换后的）触发键字符。
    let mut cfg = config_with("wubi86");
    cfg.keys.overflow.select_key = "commit_and_input".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() || texts.len() >= 3 {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert!(
                text.starts_with(&highlighted),
                "commit_and_input 应以高亮候选开头，实际: {}",
                text
            );
            assert!(
                text.chars().count() > highlighted.chars().count(),
                "commit_and_input 应在候选后追加触发键字符，实际: {}",
                text
            );
        }
        other => panic!("commit_and_input 应 InsertText，实际: {:?}", other),
    }
}

// ---- 有候选时按融合「快捷」触发键：顶字 + 进融合模式（现唯一的快捷输入形态，支持拼音）----

#[test]
fn test_semicolon_with_candidates_enters_mix_and_accepts_pinyin() {
    if !has_schemas() {
        return;
    }
    // 隔离选词职责（select_key_groups 置空），专测「有候选 → 按 ; 顶字 + 进融合 → 可打拼音」。
    let mut cfg = config_with("wubi86");
    cfg.keys.select_key_groups = vec![];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord, 'a'); // 产生候选
    let texts = coord.debug_page_texts();
    if texts.is_empty() {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::InsertText {
            text,
            new_composition,
            ..
        } => {
            assert_eq!(text, highlighted, "; 应顶字上屏当前高亮候选");
            assert_eq!(
                new_composition.as_deref(),
                Some(";"),
                "进入融合模式应显示前缀 ;"
            );
        }
        other => panic!("有候选按 ; 应顶字+进融合模式，实际: {:?}", other),
    }
    // 融合模式输入拼音 nihao → 候选应含「你好」（拼音成员生效，证明能打拼音）
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    assert!(
        texts.iter().any(|t| t.contains("你好")),
        "融合模式应能输入拼音（nihao→你好），实际: {:?}",
        texts
    );
}

#[test]
fn test_semicolon_overflow_falls_to_mix_not_overflow() {
    if !has_schemas() {
        return;
    }
    // ; 同时是选词键(默认 semicolon_quote)与融合触发键；恰好 1 个候选时次选越界
    // → 不走 overflow，而是顶字 + 进融合（对齐 Go 优先级：选词 < 进模式 < overflow）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    for c in "yyyg".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.len() != 1 {
        return; // 需恰好 1 个候选让 ; 次选越界
    }
    let only = texts[0].clone();
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::InsertText {
            text,
            new_composition,
            ..
        } => {
            assert_eq!(text, only, "1 候选时 ; 应顶字上屏该候选");
            assert_eq!(new_composition.as_deref(), Some(";"), "并进入融合模式");
        }
        other => panic!("1 候选时 ; 应顶字+进融合，实际: {:?}", other),
    }
}

// ---- 以词定字（select_char_keys，对齐 Go handleSelectChar/handleSelectCharWithOverflow）----
// comma_period 组：`,`(VK_OEM_COMMA=0xBC) 取第 1 字，`.`(VK_OEM_PERIOD=0xBE) 取第 2 字。

#[test]
fn test_select_char_first_and_second() {
    if !has_schemas() {
        return;
    }
    // 启用以词定字 comma_period：从当前高亮候选词逐字上屏。
    let mut cfg = config_with("pinyin");
    cfg.keys.select_char_keys = vec!["comma_period".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() {
        return;
    }
    let word: Vec<char> = texts[0].chars().collect();
    if word.len() < 2 {
        return; // 需高亮词 ≥ 2 字方能测第 1/第 2 字
    }
    // `,` → 取第 1 字
    match coord.handle_key_event(&key_event(0xBC, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, word[0].to_string(), ", 应上屏高亮词第 1 字");
        }
        other => panic!(", 应以词定字上屏第 1 字，实际: {:?}", other),
    }
    // 重新输入，`.` → 取第 2 字
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    match coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, word[1].to_string(), ". 应上屏高亮词第 2 字");
        }
        other => panic!(". 应以词定字上屏第 2 字，实际: {:?}", other),
    }
}

#[test]
fn test_select_char_disabled_by_default() {
    if !has_schemas() {
        return;
    }
    // 默认 select_char_keys 为空 → `,` 不作以词定字，走正常标点流水线（零回归）。
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() {
        return;
    }
    let first_char = texts[0].chars().next().unwrap().to_string();
    let act = coord.handle_key_event(&key_event(0xBC, EVENT_KEY_DOWN));
    if let KeyAction::InsertText { text, .. } = &act {
        assert_ne!(
            *text, first_char,
            "默认禁用时 , 不应只上屏首字（应走标点：顶词+逗号）"
        );
    }
}

#[test]
fn test_select_char_overflow_ignore_default() {
    if !has_schemas() {
        return;
    }
    // 高亮词仅 1 字时按 `.`（取第 2 字）越界，默认 overflow.select_char_key = ignore → 吞键。
    let mut cfg = config_with("wubi86");
    cfg.keys.select_char_keys = vec!["comma_period".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() || texts[0].chars().count() != 1 {
        return; // 需高亮为单字词方能让 . 越界
    }
    let act = coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)); // . VK_OEM_PERIOD
    assert!(
        matches!(act, KeyAction::Consumed),
        "默认 ignore 下以词定字越界应吞键(Consumed)，实际: {:?}",
        act
    );
}

#[test]
fn test_select_char_overflow_commit() {
    if !has_schemas() {
        return;
    }
    // overflow.select_char_key = commit：越界时上屏当前高亮候选，不追加触发键字符。
    let mut cfg = config_with("wubi86");
    cfg.keys.select_char_keys = vec!["comma_period".into()];
    cfg.keys.overflow.select_char_key = "commit".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() || texts[0].chars().count() != 1 {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, highlighted, "commit 应只上屏高亮候选，无追加字符");
        }
        other => panic!("commit 应 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_select_char_overflow_commit_and_input() {
    if !has_schemas() {
        return;
    }
    // overflow.select_char_key = commit_and_input：越界时上屏高亮候选 + 追加转换后的触发键字符。
    let mut cfg = config_with("wubi86");
    cfg.keys.select_char_keys = vec!["comma_period".into()];
    cfg.keys.overflow.select_char_key = "commit_and_input".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "qqqq".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    if texts.is_empty() || texts[0].chars().count() != 1 {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert!(
                text.starts_with(&highlighted),
                "commit_and_input 应以高亮候选开头，实际: {}",
                text
            );
            assert!(
                text.chars().count() > highlighted.chars().count(),
                "commit_and_input 应在候选后追加触发键字符，实际: {}",
                text
            );
        }
        other => panic!("commit_and_input 应 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_english_stats_callable_without_store() {
    // handle_english_stats 无 store 时应静默跳过，不崩溃。
    // 验证 MessageHandler trait 接口存在且协调器已实现。
    let coord = Coordinator::new_headless(config_with("wubi86"), None);
    coord.handle_english_stats(5, 3, 2, 1);
}
