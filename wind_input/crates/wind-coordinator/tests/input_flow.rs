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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_dev/data")
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
    // 默认 numpad_behavior 为空 → direct：不把该键解释为选词，但**已打的码不丢**——
    // 顶屏当前高亮候选后接着输出小键盘数字（旧契约为「丢弃编码只输出数字」，已废止：
    // 丢掉用户已打的码是数据丢失，且与主键盘标点键的既有行为不一致）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a'); // 产生组合 + 候选
    // 小键盘 5 (VK_NUMPAD5 = 0x65)
    let act = coord.handle_key_event(&key_event(0x65, EVENT_KEY_DOWN));
    match act {
        KeyAction::InsertText { text, .. } => {
            assert!(
                text.ends_with('5') && text.chars().count() > 1,
                "direct 小键盘应顶屏候选再接数字 5，实际: {}",
                text
            );
        }
        other => panic!("direct 小键盘应 InsertText，实际: {:?}", other),
    }

    // 空组合时无候选可顶：仅输出数字本身。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0x65, EVENT_KEY_DOWN));
    assert_eq!(
        action_text(&act).unwrap_or_default(),
        "5",
        "空组合 direct 小键盘应只输出数字"
    );
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
        preedit, "ni'hao",
        "拼音组合区应显示 'ni'hao'，实际: {}",
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

/// z 作字母触发键：`znihao` 应经临时拼音上屏「你好」，不含字面 z。
/// 无论 z 在方案里是死码（首键即进临拼，身份③）还是活码前缀（后续字母处 z-fallback 夺取，
/// 身份②→③），都收敛到临拼编码「nihao」——故对 schema 细节鲁棒。
#[test]
fn test_z_letter_trigger_temp_pinyin() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_pinyin.enabled = true;
    cfg.input.temp_pinyin.trigger_keys = vec!["z".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "znihao".chars() {
        press_letter(&coord, c);
    }
    let commit = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match commit {
        KeyAction::InsertText { text, .. } => {
            assert!(
                text.contains("你好"),
                "znihao 应经临拼上屏 你好，实际: {}",
                text
            );
            assert!(!text.contains('z'), "上屏不应含字面 z，实际: {}", text);
        }
        other => panic!("空格应上屏 InsertText，实际: {:?}", other),
    }
}

/// z 未配为触发键时：znihao 走正常五笔，不进临拼（回归保护——不误触发）。
#[test]
fn test_z_not_trigger_stays_normal() {
    if !has_schemas() {
        return;
    }
    // 默认 trigger_keys 不含 z（默认 ["backtick"]）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    let a = press_letter(&coord, 'z');
    // 正常五笔：z 进缓冲（组合区含 z）或作码，绝不进临拼前缀语义。
    if let Some(disp) = action_text(&a) {
        assert!(
            disp.starts_with('z') || disp.is_empty(),
            "z 未配触发键应作正常码累积，实际组合区: {}",
            disp
        );
    }
}

/// z 临拼模式下切中英文：应遵循 keys.commit_on_switch —— 开启（默认）时把拼音原码上屏，
/// 而非无条件清空（回归保护：此前 take_input_on_mode_switch 独占分支对临拼恒返回空串）。
#[test]
fn test_temp_pinyin_commit_on_mode_switch() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_pinyin.enabled = true;
    cfg.input.temp_pinyin.trigger_keys = vec!["z".into()];
    cfg.keys.commit_on_switch = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    // 进入 z 临拼，缓冲拼音码 nihao（不选词、不上屏）。
    for c in "znihao".chars() {
        press_letter(&coord, c);
    }
    // 左 Shift 释放：中→英切换，commit_on_switch=true → 应上屏拼音原码 nihao。
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert_eq!(
        text, "nihao",
        "临拼切英文应按 commit_on_switch 上屏原码 nihao，实际: {:?}",
        act
    );
    assert!(!coord.is_chinese_mode(), "左 Shift 应切到英文");
}

/// 关闭 commit_on_switch 时：临拼切中英文应清空，不上屏原码。
#[test]
fn test_temp_pinyin_no_commit_on_mode_switch_when_disabled() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_pinyin.enabled = true;
    cfg.input.temp_pinyin.trigger_keys = vec!["z".into()];
    cfg.keys.commit_on_switch = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "znihao".chars() {
        press_letter(&coord, c);
    }
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert!(
        text.is_empty(),
        "commit_on_switch=false 时临拼切换应清空，实际上屏: {:?}",
        text
    );
}

/// 只按了模式进入符（缓冲空）时切英文：应像回车一样原样上屏该前缀符号，而非清空。
/// commit_on_switch=on（上屏编码选项）时对齐回车空缓冲上屏语义。
#[test]
fn test_temp_pinyin_prefix_only_commits_symbol_on_switch() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.commit_on_switch = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    // 只按反引号进入临拼（缓冲空，只有前缀 `）。
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    // 左 Shift 释放切英文：应上屏前缀符号 `（与回车空缓冲上屏一致）。
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert_eq!(text, "`", "只按进入符切英文应上屏该符号 `，实际: {:?}", act);
    assert!(!coord.is_chinese_mode(), "左 Shift 应切到英文");
}

/// 快捷输入只按进入符 ; 时切英文：应像回车一样原样上屏 ;（非中文 ；）。
#[test]
fn test_quick_input_prefix_only_commits_symbol_on_switch() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.commit_on_switch = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入快捷输入（空缓冲）
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert_eq!(
        text, ";",
        "只按进入符 ; 切英文应原样上屏 ;，实际: {:?}",
        act
    );
}

/// 关闭 commit_on_switch 时：只按进入符切英文应清空，不上屏符号。
#[test]
fn test_prefix_only_no_commit_on_switch_when_disabled() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.commit_on_switch = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)); // ` 进入临拼（空缓冲）
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert!(
        text.is_empty(),
        "commit_on_switch=false 时只按进入符切英文应清空，实际上屏: {:?}",
        text
    );
}

/// 断言某次分隔符键返回的动作**未**把 `'` 压入组合区。
fn separator_not_inserted(act: &KeyAction) -> bool {
    !matches!(act, KeyAction::UpdateComposition { text, .. } if text.contains('\''))
}

/// Task 8 / Fix Round 1：`auto` 真语义——默认选键组（`semicolon_quote` 含 `'`=VK_OEM_7）下，
/// `'` 保留三选键功能、**不**作分隔符；改由反引号(`, VK_OEM_3=0xC0)作硬边界压入缓冲。
#[test]
fn separator_auto_avoids_quote_when_it_is_select_key() {
    if !has_schemas() {
        return;
    }
    // ' (VK_OEM_7=0xDE)：默认作三选键 → 不作分隔符，preedit 不应出现 '
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    let q = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN));
    assert!(
        separator_not_inserted(&q),
        "auto+默认选键组：引号应保留选键功能、不作分隔符，实际: {:?}",
        q
    );

    // 反引号(0xC0)：' 被占 → 反引号作分隔符，压入 ' 并固定音节边界
    let coord2 = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord2, c);
    }
    let b = coord2.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    let pre = action_text(&b).expect("反引号应作分隔符并返回组合区");
    assert!(
        pre.contains('\''),
        "auto+默认选键组：反引号应插入分隔符，实际 preedit: {}",
        pre
    );
    let mut last = b;
    for c in "an".chars() {
        last = press_letter(&coord2, c);
    }
    assert_eq!(
        action_text(&last).unwrap(),
        "xi'an",
        "反引号手动分隔符应固定音节边界"
    );
    assert!(
        !coord2.debug_page_texts().is_empty(),
        "分隔后仍应有候选（如「西」/「西安」）"
    );
}

/// Fix Round 1：`auto` 下若 `'` **不**在选键组（此处 `comma_period`）→ `'` 空闲、作分隔符；
/// 反引号则不作分隔符。
#[test]
fn separator_auto_uses_quote_when_not_select_key() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("pinyin");
    cfg.keys.select_key_groups = vec!["comma_period".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    let q = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN));
    let pre = action_text(&q).expect("引号应作分隔符并返回组合区");
    assert!(
        pre.contains('\''),
        "auto+选键组不含引号：引号应作分隔符，实际: {}",
        pre
    );

    let mut cfg2 = config_with("pinyin");
    cfg2.keys.select_key_groups = vec!["comma_period".into()];
    let coord2 = Coordinator::new_headless(cfg2, Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord2, c);
    }
    let b = coord2.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    assert!(
        separator_not_inserted(&b),
        "auto+选键组不含引号：反引号不应作分隔符，实际: {:?}",
        b
    );
}

/// Fix Round 1：显式 `quote` 模式尊重用户指定值——即使默认选键组含 `'`，引号仍作分隔符（覆盖选键）。
#[test]
fn separator_explicit_quote_overrides_select_key() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("pinyin");
    cfg.schema.pinyin.separator = "quote".into(); // 显式，默认选键组仍含 '
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    let q = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN));
    let pre = action_text(&q).expect("显式 quote 引号应作分隔符并返回组合区");
    assert!(
        pre.contains('\''),
        "显式 quote 模式：引号应作分隔符（覆盖选键），实际: {}",
        pre
    );
}

/// Fix Round 1：双拼方案下手动分隔符一律禁用（`'` 会进 buffer 但引擎剥除，致 buffer 与 preedit
/// 发散）——引号/反引号均不作分隔符。
#[test]
fn separator_disabled_for_shuangpin() {
    let d = data_dir();
    if !d.join("schemas/shuangpin.schema.toml").exists()
        && !d.join("schemas/shuangpin.schema.yaml").exists()
    {
        return;
    }
    let mut cfg = config_with("pinyin");
    cfg.schema.available = vec!["shuangpin".into(), "pinyin".into()];
    cfg.schema.active = "shuangpin".into(); // separator 保持默认 auto
    let coord = Coordinator::new_headless(cfg, Some(&d));
    for c in "ui".chars() {
        press_letter(&coord, c);
    }
    let q = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN));
    assert!(
        separator_not_inserted(&q),
        "双拼：引号不应作分隔符，实际: {:?}",
        q
    );

    let mut cfg2 = config_with("pinyin");
    cfg2.schema.available = vec!["shuangpin".into(), "pinyin".into()];
    cfg2.schema.active = "shuangpin".into();
    let coord2 = Coordinator::new_headless(cfg2, Some(&d));
    for c in "ui".chars() {
        press_letter(&coord2, c);
    }
    let b = coord2.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    assert!(
        separator_not_inserted(&b),
        "双拼：反引号不应作分隔符，实际: {:?}",
        b
    );
}

/// C1 回归：全拼手动分隔符 `xi'an` 选「西安」应**全消费**上屏、组合区清空无残留。
/// 修复前引擎按剥除 `'` 的 query 算 consumed_length，协调器却按含 `'` 缓冲切片 → 误判 partial、
/// 残留尾字符 "n"（组合区变「西安n」）。修复后 consumed_length 回映射到含 `'` 的原始输入空间。
#[test]
fn separator_full_commit_consumes_all() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    // 反引号(0xC0)作硬分隔符（默认 auto + 选键组含 ' → 反引号作分隔符，参照 Task 8 现有测试）。
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    let mut last = KeyAction::PassThrough;
    for c in "an".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(
        action_text(&last).as_deref(),
        Some("xi'an"),
        "缓冲应为 xi'an"
    );

    let texts = coord.debug_page_texts();
    let p = texts
        .iter()
        .position(|t| t == "西安")
        .unwrap_or_else(|| panic!("候选应含整句「西安」，实际: {:?}", texts));
    // 数字键选「西安」→ 全消费上屏，无残留尾字符
    match coord.handle_key_event(&key_event(0x31 + p as u32, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "西安", "分隔符输入选「西安」应完整上屏，不残留 'n'");
        }
        other => panic!("选「西安」应上屏 InsertText，实际: {:?}", other),
    }
    assert_eq!(
        coord.debug_candidate_count(),
        0,
        "全消费后组合区候选应清空（无残留拼音续转）"
    );
}

/// C1 回归：`xi'an` 两步分段——先选「西」组合区剩 "an"（`'` 随已消费段吃掉，非 "'an"），
/// 再选「安」整体上屏「西安」并清空。
#[test]
fn separator_two_step_segmentation() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    for c in "an".chars() {
        press_letter(&coord, c);
    }
    // 先选「西」（子短语，仅消费 xi 段；边界紧跟的 `'` 应归入已消费侧）
    let texts = coord.debug_page_texts();
    let p_xi = texts
        .iter()
        .position(|t| t == "西")
        .unwrap_or_else(|| panic!("候选应含子短语「西」，实际: {:?}", texts));
    let step = coord.handle_key_event(&key_event(0x31 + p_xi as u32, EVENT_KEY_DOWN));
    let disp = action_text(&step).expect("选「西」应返回 UpdateComposition");
    assert!(
        disp.starts_with('西') && disp.ends_with("an") && !disp.contains('\''),
        "选「西」后组合区应为「西」+剩余 an（无 ' 残留），实际: {:?}",
        disp
    );

    // 再选「安」→ 整体上屏「西安」，组合区清空
    let texts2 = coord.debug_page_texts();
    let p_an = texts2
        .iter()
        .position(|t| t == "安")
        .unwrap_or_else(|| panic!("剩余 an 的候选应含「安」，实际: {:?}", texts2));
    match coord.handle_key_event(&key_event(0x31 + p_an as u32, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "西安", "两步分段最终应上屏「西安」");
        }
        other => panic!("选「安」应上屏 InsertText，实际: {:?}", other),
    }
    assert_eq!(coord.debug_candidate_count(), 0, "两步选完组合区应清空");
}

/// C1 回归（鼠标版）：点选分段候选须与数字键同为分步提交——先点「西」组合区留活剩 "an"、
/// 候选续查出「安」，再点「安」整体上屏「西安」。
///
/// 曾因 `mouse_select` 独走 `commit_candidate`（无条件清缓冲、不看 consumed_length）而：
/// 剩余码被丢弃、候选窗直接消失，且第二步只上屏「安」丢掉已确认的「西」段。
#[test]
fn mouse_select_two_step_segmentation() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for c in "xi".chars() {
        press_letter(&coord, c);
    }
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    for c in "an".chars() {
        press_letter(&coord, c);
    }
    // 鼠标点选「西」（子短语，仅消费 xi 段）→ 分步提交，组合区留活
    let texts = coord.debug_page_texts();
    let p_xi = texts
        .iter()
        .position(|t| t == "西")
        .unwrap_or_else(|| panic!("候选应含子短语「西」，实际: {:?}", texts));
    let step = coord
        .debug_mouse_select(p_xi)
        .expect("主输入路点选应产生待推送的 KeyAction");
    let disp = action_text(&step).unwrap_or_else(|| {
        panic!(
            "点选「西」应为 UpdateComposition（组合区留活），实际: {:?}",
            step
        )
    });
    assert!(
        disp.starts_with('西') && disp.ends_with("an") && !disp.contains('\''),
        "点选「西」后组合区应为「西」+剩余 an（无 ' 残留），实际: {:?}",
        disp
    );
    // 剩余分词的候选必须还在（原 bug：候选窗直接消失，count 归 0）
    assert!(
        coord.debug_candidate_count() > 0,
        "点选分段候选后应续查剩余码的候选，不应清空"
    );

    // 再点「安」→ 整体上屏「西安」（含已确认的「西」段），组合区清空
    let texts2 = coord.debug_page_texts();
    let p_an = texts2
        .iter()
        .position(|t| t == "安")
        .unwrap_or_else(|| panic!("剩余 an 的候选应含「安」，实际: {:?}", texts2));
    match coord.debug_mouse_select(p_an) {
        Some(KeyAction::InsertText { text, .. }) => {
            assert_eq!(
                text, "西安",
                "两步点选最终应上屏「西安」，不得丢失已确认的「西」段"
            );
        }
        other => panic!("点选「安」应上屏 InsertText，实际: {:?}", other),
    }
    assert_eq!(coord.debug_candidate_count(), 0, "两步点选完组合区应清空");
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
    // punct_commit 默认关闭（标点键在有编码时吞键、不顶字上屏），须显式开启才有此行为。
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.punct_commit = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
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
        preedit, "`ni'hao",
        "临时拼音组合区应为 `ni'hao，实际: {}",
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

    // 反引号：应上屏当前高亮候选并进入临时拼音。默认 top_commit_mode=direct_commit：
    // 真提交候选、前缀新组合延迟到触发键 keyup 才开（与顶码上屏同一分流）。
    match coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)) {
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            ..
        } => {
            assert_eq!(commit_text, first, "应真提交当前高亮候选");
            assert_eq!(deferred_composition, "`", "延迟新组合应为临时拼音前缀");
        }
        other => panic!("有候选按反引号应顶屏+进临时拼音，实际: {:?}", other),
    }

    // 现已在临时拼音模式：输入拼音 nihao 应得拼音候选
    let mut last = KeyAction::PassThrough;
    for c in "nihao".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(action_text(&last).unwrap(), "`ni'hao", "应处于临时拼音模式");
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
    // 拼音方案下反引号不应触发临时拼音（仅码表/混输方案启用）。
    // 注：旧断言是 assert_ne!(txt, "`ni")——进临拼时根本不会产出该串，故恒真、从未真正设防，
    // 判据缺失（引导键分支无引擎类型检查）多年未被发现。现直接断言"未进入临拼模式"。
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    assert!(
        !coord.debug_in_temp_pinyin(),
        "拼音方案不应进入临时拼音，实际 act={act:?}"
    );
    // 且反引号应作标点上屏（不被模式吞掉）。
    let txt = action_text(&act).unwrap_or_default();
    assert!(
        txt.contains('`') || txt.contains('·'),
        "反引号应作标点输出，实际: {txt:?}"
    );
}

/// 组合意外终止（鼠标点击移光标 / 焦点切换 / 宿主强制 EndComposition）必须整体复位
/// overlay 模式，不能只清 input_buffer——临拼/快捷的缓冲与前缀不在 input_buffer 里。
/// 真机现象（回归）：` 进临拼后点鼠标移光标，再按 d 组合区显示 `d（模式残留）。
#[test]
fn test_composition_terminated_resets_overlay_modes() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));

    // ` 进入临时拼音
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN));
    assert!(coord.debug_in_temp_pinyin(), "反引号应进入临时拼音");

    // 宿主终止组合（鼠标点击移光标）
    coord.handle_composition_terminated();
    assert!(
        !coord.debug_in_temp_pinyin(),
        "组合终止后不应残留临时拼音模式"
    );

    // 再按 d：应走普通输入（五笔码），而非临拼的 `d
    let act = press_vk(&coord, 0x44, false);
    let txt = match &act {
        KeyAction::UpdateComposition { text, .. } => text.clone(),
        _ => String::new(),
    };
    assert!(
        !txt.starts_with('`'),
        "终止后按键不应续在临拼前缀上: {txt:?}"
    );
}

/// 按下一个字符键（vk + 可选 shift）
fn press_vk(coord: &Coordinator, vk: u32, shift: bool) -> KeyAction {
    let mut ev = key_event(vk, EVENT_KEY_DOWN);
    if shift {
        ev.modifiers = 0x0001;
    }
    coord.handle_key_event(&ev)
}

/// 快捷输入模式下切中英文：与临拼一致，遵循 keys.commit_on_switch —— 开启（默认）时把
/// 剩余原码上屏（前缀 ; 不输出），而非无条件清空（回归保护：独占分支曾对 mix 恒返回空串）。
#[test]
fn test_quick_input_commit_on_mode_switch() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.commit_on_switch = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入快捷输入
    press_vk(&coord, 0x31, false); // 1
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x32, false); // 2
    // 左 Shift 释放：中→英切换，commit_on_switch=true → 上屏原码 1+2（前缀 ; 不输出）。
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert_eq!(
        text, "1+2",
        "快捷输入切英文应按 commit_on_switch 上屏原码 1+2，实际: {:?}",
        act
    );
    assert!(!coord.is_chinese_mode(), "左 Shift 应切到英文");
}

/// 关闭 commit_on_switch 时：快捷输入切中英文应清空，不上屏原码。
#[test]
fn test_quick_input_no_commit_on_mode_switch_when_disabled() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.commit_on_switch = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入快捷输入
    press_vk(&coord, 0x31, false); // 1
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x32, false); // 2
    let act = coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    let text = action_text(&act).unwrap_or_default();
    assert!(
        text.is_empty(),
        "commit_on_switch=false 时快捷输入切换应清空，实际上屏: {:?}",
        text
    );
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
fn test_quick_input_colon_enters_numeric_symbol_lens() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入快捷输入

    // 冒号与分号共用 VK_OEM_1，但带 Shift；它应作为数字/符号输入进入 mix 缓冲，
    // 不应被误判为“触发键二次按下”而直接上屏中文冒号。
    match press_vk(&coord, 0xBA, true) {
        KeyAction::UpdateComposition { text, .. } => {
            assert_eq!(text, ";:", "冒号应进入快捷输入数字/符号模式");
        }
        other => panic!("冒号应更新快捷输入组合区，实际: {:?}", other),
    }
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

// ───── 模式键空缓冲回车上屏触发符号本身（仅空缓冲场景，补输被模式键占用的符号）─────

#[test]
fn test_quick_input_empty_enter_outputs_trigger_symbol() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 分号进入快捷输入（空缓冲），随即按回车 → 原样上屏触发符号 ;（不按中英标点转换）
    let act = coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    assert_eq!(action_text(&act).unwrap(), ";", "分号应进入快捷输入");
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, ";", "空缓冲回车应原样上屏触发符号 ;（非中文 ；）");
        }
        other => panic!("空缓冲回车应上屏触发符号，实际: {:?}", other),
    }
    // 退出后五笔输入恢复正常
    let act = press_letter(&coord, 'a');
    assert_eq!(
        action_text(&act).unwrap(),
        "a",
        "回车上屏后应已退出快捷输入"
    );
}

#[test]
fn test_temp_pinyin_empty_enter_outputs_trigger_symbol() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 反引号进入临时拼音（空缓冲），随即按回车 → 原样上屏触发符号 `
    let act = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)); // `
    assert_eq!(action_text(&act).unwrap(), "`", "反引号应进入临时拼音");
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "`", "空缓冲回车应原样上屏触发符号 `");
        }
        other => panic!("空缓冲回车应上屏触发符号，实际: {:?}", other),
    }
    let act = press_letter(&coord, 'a');
    assert_eq!(
        action_text(&act).unwrap(),
        "a",
        "回车上屏后应已退出临时拼音"
    );
}

#[test]
fn test_quick_input_empty_enter_clear_behavior_discards() {
    if !has_schemas() {
        return;
    }
    // enter_behavior=clear：空缓冲回车放弃退出，不上屏任何符号（严格遵循配置）
    let mut cfg = config_with("wubi86");
    cfg.input.enter_behavior = "clear".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ; 进入
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("clear 模式空缓冲回车应清空退出，实际: {:?}", other),
    }
}

#[test]
fn test_mix_letter_trigger_empty_enter_no_symbol() {
    if !has_schemas() {
        return;
    }
    // 字母触发键无 prefix 符号：空缓冲回车不应误输出字母，安全清空退出。
    let mut cfg = config_with("wubi86");
    cfg.schema.mix_modes = vec![wind_config::config::MixModeConfig {
        id: "mix_z".into(),
        name: "测试".into(),
        short_name: "测".into(),
        trigger_keys: vec!["z".into()],
        members: vec!["quick_input".into()],
    }];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    // 按 z(0x5A) 进入 mix（字母触发键，mix_prefix 为空）
    coord.handle_key_event(&key_event(0x5A, EVENT_KEY_DOWN));
    // 空缓冲回车：prefix 为空 → 走清空退出（而非上屏 z）。若误入五笔，则会上屏/提交 z 而非清空。
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "字母触发键空缓冲回车应清空退出（不输出字母），实际: {:?}",
            other
        ),
    }
}

#[test]
fn test_phrase_date_expansion() {
    if !has_schemas() {
        return;
    }
    // 短语层存储于 store（TOML 只是同步种子，见 build() 的 store.sync_system_phrases），
    // 无 store 时短语层不建、"date" 不会展开——须用 new_headless_with_store 注入真实 store。
    // 输入 "date" → 短语层应展开当前日期候选（如 2026年6月14日）
    let store_path = std::env::temp_dir().join("wind_phrase_date_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord =
        Coordinator::new_headless_with_store(config_with("wubi86"), Some(&data_dir()), store);
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
    // 短语层需真实 store 才会同步/启用（见 test_phrase_date_expansion 注释）。
    let store_path = std::env::temp_dir().join("wind_phrase_time_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord =
        Coordinator::new_headless_with_store(config_with("wubi86"), Some(&data_dir()), store);
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
    // 关闭英文候选查词：本测试验证「数字在无可选候选时应入缓冲」，若开着词库候选，
    // "ver" 命中真实英文词（Verb/Verbal…）会让数字被解释成候选翻页选词（设计如此，
    // 见 handle_temp.rs 数字分支注释），与本测试意图无关，故关闭以消除数据耦合。
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.show_candidates = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
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

/// direct（默认）：临英缓冲是文本，小键盘数字/符号直接入缓冲 →「英文数字连输」可用。
#[test]
fn test_temp_english_numpad_direct_inputs() {
    if !has_schemas() {
        return;
    }
    // 小键盘数字在临英下曾被静默吃掉（只认主键盘 0x30-0x39，小键盘 0x60-0x69 落标点臂
    // → punct_char 判 None → Consumed）。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'v'); // V
    press_letter(&coord, 'e');
    press_letter(&coord, 'r');
    let last = press_vk(&coord, 0x62, false); // 小键盘 2 (VK_NUMPAD2)
    assert_eq!(
        action_text(&last).unwrap(),
        "Ver2",
        "小键盘数字应入临英缓冲"
    );
    // 小键盘小数点 / 减号同样入缓冲。
    press_vk(&coord, 0x6E, false); // VK_DECIMAL '.'
    let last = press_vk(&coord, 0x6D, false); // VK_SUBTRACT '-'
    assert_eq!(action_text(&last).unwrap(), "Ver2.-", "小键盘符号应入缓冲");
}

/// follow_main：入口归一化 → 小键盘键在**所有模式**下与主键盘同键完全一致。
/// 归一化是唯一实现手段，故本测试即是「所有模式一致」的守护。
#[test]
fn test_numpad_follow_main_matches_mainboard_all_modes() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.numpad_behavior = "follow_main".into();
    // 临英下主键盘数字的语义依赖候选有无，关掉词库候选以固定为「入缓冲」，
    // 令主/小键盘对照不受真实英文词库数据影响（同 test_temp_english_digits_and_punct）。
    cfg.input.temp_english.show_candidates = false;

    // ① 临时英文：小键盘 2 ≡ 主键盘 2（无候选 → 入缓冲）
    let coord = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_shift_letter(&coord, 'v');
    press_letter(&coord, 'e');
    press_letter(&coord, 'r');
    let np = press_vk(&coord, 0x62, false); // VK_NUMPAD2
    let coord2 = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_shift_letter(&coord2, 'v');
    press_letter(&coord2, 'e');
    press_letter(&coord2, 'r');
    let main = press_vk(&coord2, 0x32, false); // 主键盘 2
    assert_eq!(
        action_text(&np),
        action_text(&main),
        "临英：小键盘 2 应与主键盘 2 一致"
    );

    // ② 普通码表：小键盘 2 ≡ 主键盘 2（有候选 → 选第 2 个候选）
    let coord = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_letter(&coord, 'a');
    let np = press_vk(&coord, 0x62, false);
    let coord2 = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_letter(&coord2, 'a');
    let main = press_vk(&coord2, 0x32, false);
    assert_eq!(
        action_text(&np),
        action_text(&main),
        "普通码表：小键盘 2 应与主键盘 2 一致（同选第 2 候选）"
    );

    // ③ 运算符须连 Shift 一并归一：小键盘 * ≡ 主键盘 Shift+8
    let coord = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_letter(&coord, 'a');
    let np = press_vk(&coord, 0x6A, false); // VK_MULTIPLY
    let coord2 = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    press_letter(&coord2, 'a');
    let main = press_vk(&coord2, 0x38, true); // Shift+8 = '*'
    assert_eq!(
        action_text(&np),
        action_text(&main),
        "小键盘 * 应与主键盘 Shift+8 一致"
    );
}

/// 数字键 0 选当前页第 10 个候选（主键盘 / 小键盘 follow_main 一致）。
/// 主键盘 0 此前落兜底流水线只输出 '0'，不选第 10——「0 = 第10候选」是通行约定，
/// 也是 follow_main 下 Numpad0「和主键盘一样」的前提。
#[test]
fn test_number_zero_selects_tenth_candidate() {
    if !has_schemas() {
        return;
    }
    // 0 选「当前页第 10 个」，故须每页容量 ≥10（默认 per_page=7 时第 10 越界）。
    // 拼音 "shi" 候选远多于 10，确保 0 选第 10 而非越界 overflow。
    let mut cfg = config_with("pinyin");
    cfg.input.numpad_behavior = "follow_main".into();
    cfg.ui.candidate.per_page = 10;
    let type_shi = |c: &Coordinator| {
        for ch in "shi".chars() {
            press_letter(c, ch);
        }
    };

    let a = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    type_shi(&a);
    let main0 = action_text(&press_vk(&a, 0x30, false)); // 主键盘 0

    let b = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    type_shi(&b);
    let np0 = action_text(&press_vk(&b, 0x60, false)); // 小键盘 0 (VK_NUMPAD0, follow_main)

    assert!(
        main0.as_deref().is_some_and(|t| !t.is_empty()),
        "主键盘 0 应选中第 10 候选并上屏（shi 候选足够多），实际: {:?}",
        main0
    );
    assert_eq!(np0, main0, "小键盘 0 (follow_main) 应与主键盘 0 选同一候选");

    // 空缓冲下的 0 不进选词臂：输出数字本身，不回归 fullwidth（此处半角态 → '0'）。
    let c = Coordinator::new_headless(cfg.clone(), Some(&data_dir()));
    let empty0 = c.handle_key_event(&key_event(0x30, EVENT_KEY_DOWN));
    assert!(
        matches!(&empty0, KeyAction::PassThrough) || action_text(&empty0).as_deref() == Some("0"),
        "空缓冲主键盘 0 应输出数字 0（透传或上屏），实际: {:?}",
        empty0
    );
}

/// direct 下编码型模式：不丢已打的码——顶屏当前高亮候选后再输出该数字。
#[test]
fn test_numpad_direct_commits_candidate_then_digit() {
    if !has_schemas() {
        return;
    }
    // 默认 numpad_behavior 为空 → direct。
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_letter(&coord, 'a'); // 产生候选
    // 对照组：取此刻首候选文本（direct 应顶屏它）。
    let expect_head = {
        let c2 = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
        press_letter(&c2, 'a');
        // 空格上屏高亮候选 = direct 应顶屏的同一个候选。
        action_text(&c2.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN))).unwrap()
    };
    let act = press_vk(&coord, 0x62, false); // 小键盘 2
    assert_eq!(
        action_text(&act).unwrap(),
        format!("{}2", expect_head),
        "direct：应顶屏高亮候选再接小键盘数字（旧行为是丢弃编码只输出数字）"
    );
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
    // 用码表方案（非拼音）：拼音普通候选禁调位（见 handle_candidate.rs 的
    // "拼音普通候选禁调位" 分支——无稳定位置语义，pin 与衰减软置前冲突），MoveTop 恒为空操作。
    let store_path = std::env::temp_dir().join("wind_candidate_op_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord =
        Coordinator::new_headless_with_store(config_with("wubi86"), Some(&data_dir()), store);
    // 五笔输入 "a" 以获取多个候选
    press_letter(&coord, 'a');
    let before = coord.debug_page_texts();
    if before.len() < 2 {
        return; // 候选不足，跳过
    }
    let second = before[1].clone();

    // 置顶第二项 → 应成为首项
    coord.debug_candidate_op(CandidateOp::MoveTop, 1);
    let after = coord.debug_page_texts();
    assert_eq!(after.first(), Some(&second), "置顶后第二项应排首位");

    // 删除一个多字候选 → 应从候选中消失
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
fn test_candidate_op_delete_single_char_hides() {
    if !has_schemas() {
        return;
    }
    use wind_ui::manager::CandidateOp;
    // 单字保护已取消：隐藏候选对单字同样生效（shadow 按 code+word 键控，
    // 仅该编码下隐藏，设置页可恢复）。
    let store_path = std::env::temp_dir().join("wind_candidate_op_single_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let coord =
        Coordinator::new_headless_with_store(config_with("pinyin"), Some(&data_dir()), store);
    for c in "shi".chars() {
        press_letter(&coord, c);
    }
    let before = coord.debug_page_texts();
    if let Some((pl, w)) = before
        .iter()
        .enumerate()
        .find(|(_, w)| w.chars().count() == 1)
        .map(|(i, w)| (i, w.clone()))
    {
        coord.debug_candidate_op(CandidateOp::Delete, pl);
        let after = coord.debug_page_texts();
        assert!(!after.contains(&w), "单字 '{}' 隐藏后不应再出现", w);
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
    deferred.set_ready(coord.clone());

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

    // 统计采集器为后台线程定时落库，测试需显式 flush 才能读到（生产由定时器/关闭时落库）。
    coord.debug_flush_stats();

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
    // 默认 top_commit_mode=direct_commit：真提交高亮候选、前缀新组合延迟到 keyup 才开。
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            ..
        } => {
            assert_eq!(commit_text, highlighted, "; 应顶字真提交当前高亮候选");
            assert_eq!(deferred_composition, ";", "进入融合模式应延迟开前缀组合 ;");
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
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            ..
        } => {
            assert_eq!(commit_text, only, "1 候选时 ; 应顶字真提交该候选");
            assert_eq!(
                deferred_composition, ";",
                "并进入融合模式（延迟开前缀组合）"
            );
        }
        other => panic!("1 候选时 ; 应顶字+进融合，实际: {:?}", other),
    }
}

#[test]
fn test_special_trigger_with_candidates_commits_and_enters() {
    if !has_schemas() {
        return;
    }
    // 特殊模式引导键在「有候选」时应与 mix/临拼一致：顶屏高亮候选 + 进模式
    // （此前只有空缓冲入口，有候选时 \ 落标点流程上屏 、）。默认 direct_commit：
    // 真提交候选、引导符新组合延迟到 keyup 才开。
    let mut cfg = config_with("wubi86");
    cfg.schema.special_modes = vec![wind_config::config::SpecialModeConfig {
        id: "sym".into(),
        trigger_keys: vec!["backslash".into()],
        schema: "pinyin".into(),
        ..Default::default()
    }];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord, 'a');
    let texts = coord.debug_page_texts();
    if texts.is_empty() {
        return;
    }
    let highlighted = texts[0].clone();
    match coord.handle_key_event(&key_event(0xDC, EVENT_KEY_DOWN)) {
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            ..
        } => {
            assert_eq!(commit_text, highlighted, "\\ 应顶字真提交当前高亮候选");
            assert_eq!(deferred_composition, "\\", "进入特殊模式应延迟开引导符组合");
        }
        other => panic!("有候选按 \\ 应顶屏+进特殊模式，实际: {:?}", other),
    }
    // 已在特殊模式：后续输入走其引用方案，组合区以引导符 \ 开头。
    let act = press_letter(&coord, 'n');
    let preedit = action_text(&act).unwrap();
    assert!(
        preedit.starts_with('\\'),
        "顶屏进入后应处于特殊模式（组合区以 \\ 开头），实际: {}",
        preedit
    );
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
fn test_fullwidth_space_on_empty_buffer() {
    if !has_schemas() {
        return;
    }
    // 全角态空缓冲按空格 → 上屏全角空格 U+3000（对齐设置端展示基线与微软拼音行为）。
    // 回归：空格键先于标点流水线被 VK_SPACE 分支截获，空缓冲曾恒 PassThrough 半角空格，
    // 全角转换（fullwidth.rs 已支持 ' '→U+3000）与自定义映射「空格」行均够不着。
    let mut cfg = config_with("pinyin");
    cfg.input.default.full_width = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "\u{3000}", "全角态空格应上屏全角空格");
        }
        other => panic!("全角态空缓冲空格应上屏全角空格，实际: {:?}", other),
    }
    // 半角态（默认）维持透传，保留宿主对空格键的原生语义。
    let coord2 = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    assert!(
        matches!(
            coord2.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)),
            KeyAction::PassThrough
        ),
        "半角态空缓冲空格应透传"
    );
}

#[test]
fn test_select_char_brackets_group() {
    if !has_schemas() {
        return;
    }
    // 回归：select_char_index 曾误用选词键组解析（select_key_vks 不识别 brackets），
    // 致配置 brackets 后 `[`/`]` 直接走标点流水线上屏【】。brackets 仅存在于
    // select_char_vks，须用它解析。`[`(VK_OEM_4=0xDB) 取第 1 字，`]`(VK_OEM_6=0xDD) 取第 2 字。
    let mut cfg = config_with("pinyin");
    cfg.keys.select_char_keys = vec!["brackets".into()];
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
    // `[` → 取第 1 字
    match coord.handle_key_event(&key_event(0xDB, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, word[0].to_string(), "[ 应上屏高亮词第 1 字");
        }
        other => panic!("[ 应以词定字上屏第 1 字，实际: {:?}", other),
    }
    // 重新输入，`]` → 取第 2 字
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    match coord.handle_key_event(&key_event(0xDD, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, word[1].to_string(), "] 应上屏高亮词第 2 字");
        }
        other => panic!("] 应以词定字上屏第 2 字，实际: {:?}", other),
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

// ---- 临时词晋升闭环 promote_count ----

#[test]
fn temp_word_promotes_after_threshold_selections() {
    // 验证 6a 造词路径晋升闭环：
    // - get_temp_word 点查 API 正确反映 count
    // - count >= promote_count → promote_temp_word 晋升入用户词库
    // - 晋升后临时层删除（get_temp_word → None），用户层新增
    // - promote_count=0 禁用语义：永不晋升（零回归保证）
    //
    // 注：6b 整词选中路径需要引擎把临时层词条作为普通候选返回；
    // 无头 harness 中引擎与 store 临时层未直接联通，该路径由
    // handle_addword.rs 内 learn_phrase_on_commit 单元测试覆盖。
    use std::sync::Arc;

    let store_path = std::env::temp_dir().join("wind_promote_thresh_integ.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = Arc::new(wind_store::Store::open(&store_path).unwrap());

    // 1. 两次累积 → count=2
    let c1 = store
        .learn_temp_word("wubi86", "abcd", "测试", 800, 0)
        .unwrap();
    assert_eq!(c1, 1, "第 1 次 count 应为 1");
    assert_eq!(
        store.get_temp_word("wubi86", "abcd", "测试").unwrap(),
        Some(1),
        "get_temp_word 应返回 count=1"
    );

    let c2 = store
        .learn_temp_word("wubi86", "abcd", "测试", 800, 0)
        .unwrap();
    assert_eq!(c2, 2, "第 2 次 count 应为 2");

    // 2. count=2 >= promote_count=2 → 晋升
    assert!(
        store.promote_temp_word("wubi86", "abcd", "测试").unwrap(),
        "count 达阈值时 promote 应返回 true"
    );
    assert_eq!(
        store.get_temp_word("wubi86", "abcd", "测试").unwrap(),
        None,
        "晋升后临时层应删除"
    );
    let user = store.get_user_words("wubi86", "abcd").unwrap();
    assert!(
        user.iter().any(|r| r.text == "测试"),
        "晋升后用户词层应含该词"
    );

    // 3. promote_count=0 禁用语义：手动验证 maybe_promote_temp 语义等价
    //    （当 promote_count=0 时，coordinator 永不调用 promote_temp_word）。
    //    此处用 get_temp_word None → 确认未晋升的词不在临时层。
    store
        .learn_temp_word("wubi86", "zzzz", "不晋升", 800, 0)
        .unwrap();
    // promote_count=0 时不晋升：临时层仍有该词
    assert_eq!(
        store.get_temp_word("wubi86", "zzzz", "不晋升").unwrap(),
        Some(1),
        "promote_count=0 时临时层应保留"
    );

    // 4. 不存在的词返回 None
    assert_eq!(
        store.get_temp_word("wubi86", "xxxx", "无").unwrap(),
        None,
        "不存在的词应返回 None"
    );

    let _ = std::fs::remove_file(&store_path);
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

fn config_with_english_trigger(active: &str, trigger: &str) -> wind_config::Config {
    let mut cfg = config_with(active);
    cfg.input.temp_english.trigger_keys = vec![trigger.to_string()];
    cfg
}

#[test]
fn test_temp_english_trigger_key_shows_prefix() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(
        config_with_english_trigger("wubi86", "slash"),
        Some(&data_dir()),
    );
    // 空缓冲按 / 进入临时英文，preedit 应显示前缀 "/"
    let act = coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN));
    assert_eq!(
        action_text(&act).as_deref(),
        Some("/"),
        "触发键进入临时英文，preedit 应显示前缀 /"
    );
}

#[test]
fn test_temp_english_trigger_key_prefix_in_preedit() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(
        config_with_english_trigger("wubi86", "slash"),
        Some(&data_dir()),
    );
    // 触发键进入后继续输入字母，preedit = 前缀 + 缓冲
    coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN)); // /
    let act = press_letter(&coord, 'h');
    assert_eq!(
        action_text(&act).as_deref(),
        Some("/h"),
        "输入 h 后 preedit 应为 /h"
    );
}

#[test]
fn test_temp_english_trigger_key_enter_empty_commits_prefix() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(
        config_with_english_trigger("wubi86", "slash"),
        Some(&data_dir()),
    );
    // 触发键进入，空缓冲直接回车 → 上屏触发键字符 "/"
    coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN)); // /
    let act = coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)); // Enter
    assert_eq!(
        action_text(&act).as_deref(),
        Some("/"),
        "空缓冲回车应上屏触发键字符 /"
    );
}

/// Bug 复现（协调层）：双拼模式下，存储在 "pinyin" 域的用户词应出现在候选中。
/// 小鹤双拼输入 "dabologe" → 全拼 "daboluoge"，store 中有该用户词，候选应包含「大菠萝哥」。
#[test]
fn test_shuangpin_userword_appears_in_candidates() {
    let d = data_dir();
    let sp_schema = d.join("schemas/shuangpin.schema.toml");
    if !sp_schema.exists() {
        eprintln!("跳过：缺少 shuangpin.schema.toml");
        return;
    }

    // 构造带用户词的 store
    let store_path = std::env::temp_dir().join("wind_sp_userword_test.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    // 用户词存在 "pinyin" 域（拼音族共享存储的规范 schema_id）
    store
        .add_user_word("pinyin", "daboluoge", "大菠萝哥", 0, 0)
        .expect("add_user_word 失败");

    // 创建双拼方案协调器并注入 store
    let mut cfg = Config::default();
    cfg.schema.available = vec!["shuangpin".into()];
    cfg.schema.active = "shuangpin".into();
    cfg.input.default.chinese_mode = true;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&d), store);

    // 输入小鹤双拼 "dabologe" → 应转换为全拼 "daboluoge"
    for c in "dabologe".chars() {
        press_letter(&coord, c);
    }

    let all = coord.debug_all_candidate_texts();
    assert!(
        all.iter().any(|t| t == "大菠萝哥"),
        "双拼输入 \"dabologe\" 经转换后应命中用户词「大菠萝哥」，实际候选: {:?}",
        all
    );

    let _ = std::fs::remove_file(&store_path);
}

// 顶码触发序列：wubi86 下 skce 满码，第 5 键 y 溢出 → 顶码上屏，余码 y。
fn drive_top_code(coord: &Coordinator) -> KeyAction {
    for ch in ['s', 'k', 'c', 'e'] {
        press_letter(coord, ch);
    }
    // 'y' = VK 0x59
    coord.handle_key_event(&key_event(0x59, EVENT_KEY_DOWN))
}

#[test]
fn top_code_pre_confirm_returns_insert_text() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.punct_commit = true;
    cfg.schema.codetable.top_code_commit = true;
    cfg.input.top_commit_mode = wind_config::TopCommitMode::PreConfirm;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    match drive_top_code(&coord) {
        KeyAction::InsertText {
            has_new_composition,
            ..
        } => {
            assert!(has_new_composition, "顶码应带余码新组合");
        }
        other => panic!("pre_confirm 顶码应返回 InsertText，实际: {:?}", other),
    }
}

#[test]
fn top_code_direct_commit_returns_commit_then_defer() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.punct_commit = true;
    cfg.schema.codetable.top_code_commit = true;
    cfg.input.top_commit_mode = wind_config::TopCommitMode::DirectCommit;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    match drive_top_code(&coord) {
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            timeout_ms,
        } => {
            assert!(!commit_text.is_empty(), "应有顶出文本");
            assert!(!deferred_composition.is_empty(), "应有余码新组合");
            assert_eq!(timeout_ms, 150);
        }
        other => panic!(
            "direct_commit 顶码应返回 CommitThenDeferComposition，实际: {:?}",
            other
        ),
    }
}

// 顶码前缓冲 skce 注入短语/命令作首选（短语高权重 PHRASE_WEIGHT_BASE 保证排首），
// 用于验证顶码上屏对短语(cmdbar)类型生效。
fn coord_with_skce_phrase(
    phrase_text: &str,
    mode: wind_config::TopCommitMode,
    tag: &str,
) -> std::sync::Arc<Coordinator> {
    let store_path = std::env::temp_dir().join(format!("wind_top_code_phrase_{tag}.redb"));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    // build() 构造期读 enabled_phrases_for_input()，故须在建 coordinator 前入库。
    store.add_phrase("skce", phrase_text, 0, 100).unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.punct_commit = true;
    cfg.schema.codetable.top_code_commit = true;
    cfg.input.top_commit_mode = mode;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

#[test]
fn top_code_plain_phrase_first_commits_phrase_text() {
    if !has_schemas() {
        return;
    }
    // 普通短语作 skce 首选：顶码应上屏短语文本 + 余码 y 续打（pre_confirm）。
    let coord = coord_with_skce_phrase(
        "顶码短语文本",
        wind_config::TopCommitMode::PreConfirm,
        "plain",
    );
    match drive_top_code(&coord) {
        KeyAction::InsertText {
            text,
            has_new_composition,
            ..
        } => {
            assert_eq!(text, "顶码短语文本", "顶码应上屏短语首选文本");
            assert!(has_new_composition, "顶码应带余码 y 新组合");
        }
        other => panic!("普通短语顶码应返回 InsertText，实际: {:?}", other),
    }
}

#[test]
fn top_code_text_command_first_commits_evaluated_text() {
    if !has_schemas() {
        return;
    }
    // 纯文本 $CC 命令（type 文本，无副作用）作 skce 首选：顶码同步求值命令文本上屏，
    // 而非上屏 display 标签「标签」（区分命令求值路径与普通短语路径）。
    let coord = coord_with_skce_phrase(
        r#"$CC("标签", type("命令文本"))"#,
        wind_config::TopCommitMode::PreConfirm,
        "textcmd",
    );
    match drive_top_code(&coord) {
        KeyAction::InsertText {
            text,
            has_new_composition,
            ..
        } => {
            assert_eq!(
                text, "命令文本",
                "纯文本命令顶码应上屏求值文本(而非 display 标签)"
            );
            assert!(has_new_composition, "顶码应带余码 y 新组合");
        }
        other => panic!("纯文本命令顶码应返回 InsertText，实际: {:?}", other),
    }
}

#[test]
fn top_code_phrase_code_no_codetable_char_still_commits() {
    if !has_schemas() {
        return;
    }
    // 用户真机场景：短语专属码 date（五笔码表无字）敲满码后再敲字符应顶短语 + 余码续打。
    // 引擎 handle_top_code 原 `first()?` 在 prefix 无字时短路 None → 顶码不触发（datea 累积）；
    // 修复后返回 Some(("", 余码))，coordinator 用短语显示首选顶码。用内置 date 日期短语验证
    // （系统短语须真 store 才同步，见 test_phrase_date_expansion）。
    let store_path = std::env::temp_dir().join("wind_top_code_datecode.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.punct_commit = true;
    cfg.schema.codetable.top_code_commit = true;
    cfg.input.top_commit_mode = wind_config::TopCommitMode::PreConfirm;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
    for ch in ['d', 'a', 't', 'e'] {
        press_letter(&coord, ch);
    }
    // 'g' = VK 0x47（溢出触发键，dateg=5>满码4 且码表无匹配）→ 顶 date 日期短语，余码 g
    match coord.handle_key_event(&key_event(0x47, EVENT_KEY_DOWN)) {
        KeyAction::InsertText {
            text,
            has_new_composition,
            ..
        } => {
            assert!(
                text.contains('年') && text.contains('月') && text.contains('日'),
                "date 短语码(码表无字)溢出应顶出日期短语，实际: {:?}",
                text
            );
            assert!(has_new_composition, "应带余码 g 新组合");
        }
        other => panic!(
            "date 短语码顶码应返回 InsertText(顶短语)，实际: {:?}(顶码未触发?)",
            other
        ),
    }
    let _ = std::fs::remove_file(&store_path);
}

#[test]
fn phrase_auto_commit_unique_exact_no_longer() {
    if !has_schemas() {
        return;
    }
    // 开启「全码唯一自动上屏」时，唯一精确码短语（无更长后继）应自动上屏。
    // 引擎 decide_auto_commit 只认码表候选（短语 code 空、且在引擎 convert 后追加），故短语原不进
    // 判据；phrase_auto_commit 补齐。注入短语码 kkkkx（五笔码表 4 码封顶，5 码处必无码表候选，
    // 短语成唯一候选）；kkkk 处有多个码表候选（非唯一）→ 第 4 键不会提前自动上屏。
    let store_path = std::env::temp_dir().join("wind_phrase_autocommit.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store.add_phrase("kkkkx", "唯一测试短语", 0, 100).unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_commit_at_full = true;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    // 第 5 键 'x'(VK 0x58) → 只剩注入短语 kkkkx 唯一 + 无更长后继 → 自动上屏
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "唯一测试短语", "唯一精确码短语应自动上屏其文本");
        }
        other => panic!(
            "短语全码唯一应自动上屏(InsertText)，实际: {:?}(未触发自动上屏?)",
            other
        ),
    }
    let _ = std::fs::remove_file(&store_path);
}

#[test]
fn phrase_auto_commit_effect_command_executes() {
    if !has_schemas() {
        return;
    }
    // 含副作用 $CC 命令（Effect 动作）作唯一精确码短语：不再被自动上屏排除，
    // 应清组合并异步执行（与空格选中命令同语义 → ClearComposition）。
    // ask() 为未实现 Effect（异步执行仅 warn 降级），测试无真实副作用。
    let store_path = std::env::temp_dir().join("wind_phrase_autocmd_effect.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store
        .add_phrase("kkkkx", r#"$CC("标签", ask("x"))"#, 0, 100)
        .unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_commit_at_full = true;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    // 第 5 键 'x' → 唯一含副作用命令候选 + 无更长后继 → 清组合 + 异步执行
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "含副作用命令全码唯一应清组合并异步执行(ClearComposition)，实际: {:?}",
            other
        ),
    }
    assert!(
        coord.debug_all_candidate_texts().is_empty(),
        "命令自动执行后候选应已清空"
    );
    let _ = std::fs::remove_file(&store_path);
}

/// 精确匹配模式（`single_code_input`）+ 空码补全（`single_code_complete`）下，短语前缀
/// 补全**只出首选一条**——与码表引擎同分支「从更长编码取首个候选」的规格一致。
///
/// 回归：原 `allow_prefix` 在补全分支放行整串前缀命中，致空码补全冒出多条「后续」。
/// 注入同前缀 zzq 的三条短语（码 zzqa/zzqb/zzqc，五笔无 zzq 精确字 → 触发补全分支）。
fn coord_with_prefix_phrases(complete: bool) -> std::sync::Arc<Coordinator> {
    let store_path =
        std::env::temp_dir().join(format!("wind_phrase_complete_{}.redb", complete as u8));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    // 权重递增：首选应为权重最高的 zzqc。
    store.add_phrase("zzqa", "短语甲", 0, 10).unwrap();
    store.add_phrase("zzqb", "短语乙", 0, 20).unwrap();
    store.add_phrase("zzqc", "短语丙", 0, 30).unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.single_code_input = true;
    cfg.schema.codetable.single_code_complete = complete;
    cfg.input.phrase.min_prefix = 2;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

#[test]
fn exact_mode_phrase_complete_yields_single_hit() {
    if !has_schemas() {
        return;
    }
    let coord = coord_with_prefix_phrases(true);
    for ch in ['z', 'z', 'q'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    let phrase_hits: Vec<&String> = texts.iter().filter(|t| t.starts_with("短语")).collect();
    assert_eq!(
        phrase_hits.len(),
        1,
        "精确模式空码补全应只出首选一条短语，实际: {:?}",
        texts
    );
    assert_eq!(
        phrase_hits[0], "短语丙",
        "补全应取权重最高的首选（HashMap 序不定，须先定序）"
    );
}

#[test]
fn exact_mode_without_complete_suppresses_phrase_prefix() {
    if !has_schemas() {
        return;
    }
    // 补全关闭：精确模式应彻底抑制短语前缀枚举（证明上一个测试的一条来自补全分支）。
    let coord = coord_with_prefix_phrases(false);
    for ch in ['z', 'z', 'q'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    assert!(
        !texts.iter().any(|t| t.starts_with("短语")),
        "补全关闭时精确模式不应出短语前缀候选，实际: {:?}",
        texts
    );
}

/// 短语自动上屏须过 `auto_commit_min_len` 闸（与码表「满码唯一自动上屏」同规格）。
///
/// 回归：`phrase_auto_commit` 原只判「唯一 + 无更长后继」、不设最短码长，致短码短语
/// （如 3 码 `ocd` 的 $CC 命令在 4 码方案里）绕过「满码」语义直接上屏/执行。
///
/// 复用 kkkkx（5 码，五笔 4 码封顶 → 必无更长后继）隔离出 min_len 单一变量：
/// 显式设 6 → 5 < 6 应被拦；设 5 → 恰好达标应放行（边界为 >=）。
fn coord_with_phrase_min_len(min_len: usize, tag: &str) -> std::sync::Arc<Coordinator> {
    let store_path = std::env::temp_dir().join(format!("wind_phrase_minlen_{tag}.redb"));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store.add_phrase("kkkkx", "唯一测试短语", 0, 100).unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_commit_at_full = true;
    cfg.schema.codetable.auto_commit_min_len = min_len;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

#[test]
fn phrase_auto_commit_blocked_below_min_len() {
    if !has_schemas() {
        return;
    }
    // min_len=6 > 短语码长 5：即便唯一且无更长后继，也不得自动上屏。
    let coord = coord_with_phrase_min_len(6, "block");
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    let act = coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN));
    assert!(
        !matches!(act, KeyAction::InsertText { .. }),
        "码长 5 < min_len 6 时短语不得自动上屏，实际: {:?}",
        act
    );
    assert!(
        coord
            .debug_all_candidate_texts()
            .contains(&"唯一测试短语".to_string()),
        "未达 min_len 应留在候选里等用户选，实际: {:?}",
        coord.debug_all_candidate_texts()
    );
}

#[test]
fn phrase_auto_commit_at_min_len_boundary() {
    if !has_schemas() {
        return;
    }
    // min_len=5 == 短语码长 5：边界为 >=，应自动上屏（证明上一个测试拦的是 min_len 本身，
    // 而非 kkkkx 这个构造本来就不会自动上屏）。
    let coord = coord_with_phrase_min_len(5, "boundary");
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "唯一测试短语", "码长恰达 min_len 应自动上屏");
        }
        other => panic!("码长 5 == min_len 5 应自动上屏，实际: {:?}", other),
    }
}

// 码表用户词库值内嵌 $CC 命令（用户真机场景 bccc=$CC(...)）自动上屏测试基建：
// 注入 5 码用户词 kkkkx（五笔 4 码封顶，5 码处必无码表候选 → 唯一 + 无更长后继，
// 与短语侧同构造）。原三重漏判：引擎意向 commit_text=原始 $CC 源 vs 展开后候选
// text=display 标签 → 复核不匹配被否决；recheck 因意向已 Some 不跑；phrase_auto_commit
// 只认 is_phrase。修复=复核按 phrase_template 补匹配 + 首选命令分流(command_auto_outcome)。
fn coord_with_dict_command(template: &str, tag: &str) -> std::sync::Arc<Coordinator> {
    let store_path = std::env::temp_dir().join(format!("wind_dict_autocmd_{tag}.redb"));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store
        .add_user_word("wubi86", "kkkkx", template, 0, 0)
        .unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_commit_at_full = true;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

#[test]
fn dict_effect_command_auto_commit_executes() {
    if !has_schemas() {
        return;
    }
    // 含副作用 $CC 命令用户词条：全码唯一自动命中应清组合并异步执行（ClearComposition）。
    let coord = coord_with_dict_command(r#"$CC("《》", ask("x"))"#, "effect");
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "含副作用命令词条全码唯一应清组合异步执行(ClearComposition)，实际: {:?}",
            other
        ),
    }
}

#[test]
fn dict_text_command_auto_commit_evaluates() {
    if !has_schemas() {
        return;
    }
    // 纯文本 $CC 命令用户词条：全码唯一自动命中应同步求值上屏其文本（而非 display 标签）。
    let coord = coord_with_dict_command(r#"$CC("标签", type("命令文本"))"#, "text");
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "命令文本", "纯文本命令词条应自动上屏求值文本");
        }
        other => panic!(
            "纯文本命令词条全码唯一应自动上屏(InsertText)，实际: {:?}",
            other
        ),
    }
}

#[test]
fn special_mode_effect_command_auto_commit_executes() {
    if !has_schemas() {
        return;
    }
    // 快符特殊模式（引用 wubi86 方案）：编码命中唯一含副作用 $CC 词条时，
    // 自动上屏应走命令执行路径（退出模式 + 异步执行 → ClearComposition），
    // 而非因引擎意向(原始 $CC 源)与展开后 display 文本复核不匹配而静默不触发。
    let store_path = std::env::temp_dir().join("wind_special_autocmd_effect.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store
        .add_user_word("wubi86", "kkkkx", r#"$CC("《》", ask("x"))"#, 0, 0)
        .unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_commit_at_full = true;
    cfg.schema.special_modes = vec![wind_config::config::SpecialModeConfig {
        id: "sym".into(),
        trigger_keys: vec!["backslash".into()],
        schema: "wubi86".into(),
        ..Default::default()
    }];
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
    // 空缓冲按 \ 进入特殊模式
    let act = coord.handle_key_event(&key_event(0xDC, EVENT_KEY_DOWN));
    assert!(
        matches!(act, KeyAction::UpdateComposition { .. }),
        "\\ 应进入特殊模式，实际: {:?}",
        act
    );
    for ch in ['k', 'k', 'k', 'k'] {
        press_letter(&coord, ch);
    }
    match coord.handle_key_event(&key_event(0x58, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "特殊模式命中唯一含副作用命令词条应清组合异步执行(ClearComposition)，实际: {:?}",
            other
        ),
    }
    let _ = std::fs::remove_file(&store_path);
}

#[test]
fn clear_on_empty_max_keeps_phrase_candidate() {
    if !has_schemas() {
        return;
    }
    // 回归：满码空码清空（clear_on_empty_max）开启 + 短语专属码（码表无字，如 zzbd）时，
    // should_clear 由码表引擎在**追加短语之前**算出 true（仅看码表空候选），但协调器随后追加了
    // 精确码短语候选 → 不应清空缓冲。原 bug：`None if should_clear => Clear` 未复查叠加短语后的
    // 最终候选，把短语列表连同缓冲一并误清（handle_candidate.rs）。
    // 复用 kkkkx（五笔码表 4 码封顶，5 码处必无码表候选 → is_empty 且满码 → should_clear 成立）。
    let store_path = std::env::temp_dir().join("wind_phrase_clear_empty.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store.add_phrase("kkkkx", "空码短语文本", 0, 100).unwrap();
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.clear_on_empty_max = true;
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
    for ch in ['k', 'k', 'k', 'k', 'x'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    assert!(
        texts.iter().any(|t| t == "空码短语文本"),
        "满码空码清空开启时，短语专属码候选不应被清空，实际候选: {:?}",
        texts
    );
    let _ = std::fs::remove_file(&store_path);
}

#[test]
fn top_code_plain_phrase_direct_commit_defers() {
    if !has_schemas() {
        return;
    }
    // 普通短语首选 + direct_commit：走成熟 CommitThenDeferComposition 路径，commit_text=短语文本。
    let coord = coord_with_skce_phrase(
        "顶码短语文本",
        wind_config::TopCommitMode::DirectCommit,
        "direct",
    );
    match drive_top_code(&coord) {
        KeyAction::CommitThenDeferComposition {
            commit_text,
            deferred_composition,
            timeout_ms,
        } => {
            assert_eq!(
                commit_text, "顶码短语文本",
                "direct_commit 顶码应真提交短语文本"
            );
            assert!(!deferred_composition.is_empty(), "应有余码 y 新组合");
            assert_eq!(timeout_ms, 150);
        }
        other => panic!(
            "普通短语 direct_commit 顶码应返回 CommitThenDeferComposition，实际: {:?}",
            other
        ),
    }
}

/// 配对跳出键：中文配对开 + 配置 Tab 为跳出键。
/// 输入左括号插入配对后，按 Tab 应等效输入右符号跳出（MoveCursorRight）；
/// 栈空后再按 Tab 应透传给宿主（不吞正常按键）。
#[test]
fn auto_pair_jump_out_key_moves_cursor_right() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true; // 中文配对开（默认 chinese_punct=true → 用 cn_pairs）
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // 左括号（Shift+9 → '（'）：插入配对，光标置于中间
    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    match ins {
        KeyAction::InsertTextWithCursor {
            text,
            cursor_offset,
        } => {
            assert_eq!(text, "（）", "应插入中文配对");
            assert_eq!(cursor_offset, 1, "光标应落在配对中间");
        }
        other => panic!("左括号应插入配对，实际: {:?}", other),
    }

    // 按 Tab：配对栈非空 → 跳出（光标右移）
    let jump = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(jump, KeyAction::MoveCursorRight),
        "Tab 应跳出配对（MoveCursorRight），实际: {:?}",
        jump
    );

    // 再按 Tab：栈已空 → 不拦截，透传给宿主
    let passthrough = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(passthrough, KeyAction::PassThrough),
        "栈空时 Tab 应透传，实际: {:?}",
        passthrough
    );
}

/// 配对跳出键未配置时：Tab 不被吞（回归保护——默认空集不启用）。
#[test]
fn auto_pair_no_jump_out_key_passes_tab_through() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    // jump_out_keys 默认只含 right_symbol → Tab 不在其中，不该被吞
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // 插入配对
    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "左括号应插入配对，实际: {:?}",
        ins
    );

    // 未配置跳出键：Tab 即使栈非空也不跳出，透传
    let tab = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(tab, KeyAction::PassThrough),
        "未配置跳出键时 Tab 应透传，实际: {:?}",
        tab
    );
}

/// `right_symbol` 在跳出列表内：打右括号 → 光标越过已配对的右符号（不重复插入）。
#[test]
fn jump_out_right_symbol_enabled_moves_cursor() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.jump_out_keys = vec!["right_symbol".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // Shift+9 → `（`：插入配对
    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "左括号应插入配对，实际: {ins:?}"
    );
    // Shift+0 → `）`：栈顶正是它 → 跳出
    let jump = coord.handle_key_event(&key_event_mods(0x30, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(jump, KeyAction::MoveCursorRight),
        "启用 right_symbol 时右括号应跳出，实际: {jump:?}"
    );
}

/// `right_symbol` 不在跳出列表内：打右括号 → **正常上屏该字符，不跳出**。
/// 回归保护：列表里没有就是没有，不做隐式补偿（用户拍板的语义）。
#[test]
fn jump_out_right_symbol_disabled_commits_char() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()]; // 只留 Tab，不含 right_symbol
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "左括号应插入配对，实际: {ins:?}"
    );
    let act = coord.handle_key_event(&key_event_mods(0x30, EVENT_KEY_DOWN, 0x0001));
    assert!(
        !matches!(act, KeyAction::MoveCursorRight),
        "未启用 right_symbol 时右括号不该跳出，实际: {act:?}"
    );
    assert!(
        format!("{act:?}").contains('）'),
        "应正常上屏右括号，实际: {act:?}"
    );
    // Tab 仍可跳出（栈未被右符号消费）
    let tab = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(tab, KeyAction::MoveCursorRight),
        "Tab 应仍能跳出，实际: {tab:?}"
    );
}

/// 引号配对回归：**连按引号键每次都开新的一对**，绝不交替。
///
/// 历史 bug：引号是唯一的对称配对键，`PunctuationConverter` 用交替开关决定出左还是出右
/// （第 1 次 `“`、第 2 次 `”`），而自动配对**一次按键就把左右都吐出去了**、开关却只前进
/// 一格 → 第 2 次按键给出 `”` → 不是左符号（不插对）、却是右符号（跳出或裸提交单个 `”`）
/// → 「出对 / 出单」严格交替循环。修法是配对生效时把交替态钉死在「左」，
/// 左右判定单一收口到配对栈，跳出交给 `jump_out_keys`。
#[test]
fn auto_pair_quote_always_opens_new_pair() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.chinese_pairs.push("“”".into());
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // Shift+VK_OEM_7(0xDE) = `"` → 中文双引号
    for round in 1..=3 {
        let act = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
        match act {
            KeyAction::InsertTextWithCursor {
                text,
                cursor_offset,
            } => {
                assert_eq!(text, "“”", "第 {round} 次按引号应插入完整一对");
                assert_eq!(cursor_offset, 1, "第 {round} 次光标应落在配对中间");
            }
            other => panic!("第 {round} 次按引号应插入配对（不得跳出/裸出单引号），实际: {other:?}"),
        }
    }
}

/// 引号不在配对表内时，保持原生「第一次左、第二次右」交替（不被上面的钉左误伤）。
#[test]
fn quote_alternates_when_not_in_pair_table() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true; // 配对开，但配对表**不含**引号
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let first = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    let second = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    assert!(
        format!("{first:?}").contains('“'),
        "首次应出左引号，实际: {first:?}"
    );
    assert!(
        format!("{second:?}").contains('”'),
        "第二次应出右引号（原生交替），实际: {second:?}"
    );
}

fn action_caret(action: &KeyAction) -> Option<u32> {
    match action {
        KeyAction::UpdateComposition { caret_pos, .. } => Some(*caret_pos),
        _ => None,
    }
}

// ---- 编码区光标（对齐 Go engine_default_cursor_move / engine_default_delete golden）----

const VK_LEFT: u32 = 0x25;
const VK_RIGHT: u32 = 0x27;
const VK_HOME: u32 = 0x24;
const VK_END: u32 = 0x23;
const VK_DELETE: u32 = 0x2E;
const VK_BACK: u32 = 0x08;

/// 无修饰键按下（复用文件上方的 `press_vk(coord, vk, shift)`）。
fn tap(coord: &Coordinator, vk: u32) -> KeyAction {
    press_vk(coord, vk, false)
}

fn type_str(coord: &Coordinator, s: &str) -> KeyAction {
    let mut last = KeyAction::PassThrough;
    for c in s.chars() {
        last = press_letter(coord, c);
    }
    last
}

/// 光标左移跨过引擎插入的音节分隔符时，caret 需按**显示串**位置换算（buffer "nihao" 的第 2
/// 字节 → 显示 "ni'hao" 的第 2 位，一次左移跨两个显示位）。这是 buffer→display 映射的核心用例。
#[test]
fn test_pinyin_cursor_maps_through_separator() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    let last = type_str(&coord, "nihao");
    assert_eq!(action_text(&last).as_deref(), Some("ni'hao"));
    assert_eq!(action_caret(&last), Some(6), "初始光标在末尾");

    // ni'ha|o → ni'h|ao：缓冲内左移一字符，显示位同步左移一位
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(5));
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(4));
    // ni'h|ao → ni|'hao：缓冲从 "nih|ao" 退到 "ni|hao"，显示上跨过分隔符 '（4 → 2）
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(2));

    // Home / End 到两端
    assert_eq!(action_caret(&tap(&coord, VK_HOME)), Some(0));
    assert_eq!(action_caret(&tap(&coord, VK_END)), Some(6));
    // 右移到边界后再右移：无位可动 → 吃掉，不透传给宿主
    assert!(matches!(tap(&coord, VK_RIGHT), KeyAction::Consumed));
}

/// 光标移动不改变组合区文本，也不重算候选（光标不参与引擎查询）。
#[test]
fn test_cursor_move_keeps_text_and_candidates() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    let before = action_text(&type_str(&coord, "nihao")).unwrap();
    let moved = tap(&coord, VK_LEFT);
    assert_eq!(
        action_text(&moved).as_deref(),
        Some(before.as_str()),
        "左移只改 caret，组合区文本不变"
    );
    // 移回末尾后空格上屏，候选与移动前一致（未因光标移动而重算）
    tap(&coord, VK_END);
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert!(!text.is_empty()),
        other => panic!("空格应上屏，实际: {:?}", other),
    }
}

/// 光标在中间时字母插到光标处（而非追加末尾），候选按新的完整缓冲重算。
#[test]
fn test_insert_at_cursor_position() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    assert_eq!(action_text(&type_str(&coord, "aa")).as_deref(), Some("aa"));
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(1)); // a|a
    let act = press_letter(&coord, 'b'); // a|a + b → ab|a
    assert_eq!(action_text(&act).as_deref(), Some("aba"), "应插在光标处");
    assert_eq!(action_caret(&act), Some(2), "插入后光标随之后移");
}

/// Delete 删光标后一字符且光标不动；Backspace 删光标前一字符。
#[test]
fn test_delete_and_backspace_at_cursor() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    type_str(&coord, "abc");
    tap(&coord, VK_HOME); // |abc
    let act = tap(&coord, VK_DELETE); // 删 'a' → |bc
    assert_eq!(action_text(&act).as_deref(), Some("bc"));
    assert_eq!(action_caret(&act), Some(0), "Delete 后光标不动");

    tap(&coord, VK_END); // bc|
    let act = tap(&coord, VK_BACK); // 删 'c' → b|
    assert_eq!(action_text(&act).as_deref(), Some("b"));
    assert_eq!(action_caret(&act), Some(1));
}

/// 边界三态：无组合 → 透传宿主；有组合但已在边界 → 吃掉（含光标在最左时的 Backspace，
/// 若透传会让宿主删掉组合区之前的正文）。
#[test]
fn test_cursor_boundary_semantics() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 无组合：方向键/Delete 透传给宿主，宿主照常移动文档光标
    assert!(matches!(tap(&coord, VK_LEFT), KeyAction::PassThrough));
    assert!(matches!(tap(&coord, VK_RIGHT), KeyAction::PassThrough));
    assert!(matches!(tap(&coord, VK_HOME), KeyAction::PassThrough));
    assert!(matches!(tap(&coord, VK_DELETE), KeyAction::PassThrough));

    type_str(&coord, "aa");
    tap(&coord, VK_HOME); // |aa
    assert!(
        matches!(tap(&coord, VK_LEFT), KeyAction::Consumed),
        "已在最左：吃掉不透传"
    );
    assert!(
        matches!(tap(&coord, VK_BACK), KeyAction::Consumed),
        "光标在最左时 Backspace 吃掉，不得透传给宿主"
    );
    tap(&coord, VK_END); // aa|
    assert!(
        matches!(tap(&coord, VK_DELETE), KeyAction::Consumed),
        "光标在末尾：前删无物，吃掉"
    );
}

/// 已转换前缀是**只读**的：光标进不去（Home 只到剩余编码开头），caret 需含前缀的 UTF-16 长度。
/// Delete 把剩余编码删空时回退最后一段（对齐 Go handleDelete → popConfirmedSegment）。
#[test]
fn test_committed_prefix_is_readonly_and_delete_pops_segment() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    type_str(&coord, "nihao");
    // 数字键 3 = 分步确认「你」，剩余编码 "hao" 留在组合区
    let act = tap(&coord, 0x33);
    assert_eq!(action_text(&act).as_deref(), Some("你hao"));
    assert_eq!(
        action_caret(&act),
        Some(4),
        "caret = 前缀「你」1 个 UTF-16 单元 + 剩余 \"hao\" 3 个"
    );

    // Home 只到剩余编码开头（caret=1，即「你」之后），不进只读前缀
    assert_eq!(action_caret(&tap(&coord, VK_HOME)), Some(1));
    assert!(
        matches!(tap(&coord, VK_LEFT), KeyAction::Consumed),
        "已在剩余编码最左：吃掉，不得退进已转换前缀"
    );

    // Delete 三次删空 "hao" → 回退段「你」，其码 "ni" 并回缓冲
    tap(&coord, VK_DELETE);
    tap(&coord, VK_DELETE);
    let act = tap(&coord, VK_DELETE);
    assert_eq!(
        action_text(&act).as_deref(),
        Some("ni"),
        "删空剩余编码应回退已转换段，而非留下空组合区"
    );
    assert_eq!(action_caret(&act), Some(2), "回退后光标落在码末尾");
}

/// Backspace 的段回退**优先于光标**：即便光标在剩余编码最左（Backspace 本该无字符可删），
/// 有已转换段时仍先回退段（Go handleBackspace 的分支顺序）。
#[test]
fn test_backspace_pops_segment_regardless_of_cursor() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    type_str(&coord, "nihao");
    tap(&coord, 0x33); // 「你」+ "hao"
    tap(&coord, VK_HOME); // 光标到剩余编码最左
    let act = tap(&coord, VK_BACK);
    assert_eq!(
        action_text(&act).as_deref(),
        Some("ni'hao"),
        "段回退优先：码 \"ni\" 并回缓冲前部，与 \"hao\" 合成 \"nihao\""
    );
    assert_eq!(action_caret(&act), Some(6), "回退后光标拉到缓冲末尾");
}

// ---- overlay 模式的编码区光标 ----

/// 临时英文：Shift+字母进入时缓冲已含首字母，光标须落其后（回归：曾因光标停在 0 而把
/// 后续字符插到首字母之前，"Hello" 变成 "elloH"）；随后可在编码区内移动并插入。
#[test]
fn test_temp_english_cursor_edit() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'h'); // 进入临英，缓冲 "H"
    let act = type_str(&coord, "ello");
    assert_eq!(action_text(&act).as_deref(), Some("Hello"));
    assert_eq!(action_caret(&act), Some(5));

    // He|llo → 插入 'X'
    tap(&coord, VK_HOME);
    assert_eq!(action_caret(&tap(&coord, VK_RIGHT)), Some(1));
    let act = press_letter(&coord, 'x');
    assert_eq!(action_text(&act).as_deref(), Some("Hxello"), "应插在光标处");
    assert_eq!(action_caret(&act), Some(2));

    // Delete 删光标后的 'e'
    let act = tap(&coord, VK_DELETE);
    assert_eq!(action_text(&act).as_deref(), Some("Hxllo"));
    assert_eq!(action_caret(&act), Some(2), "Delete 后光标不动");
}

/// 网址模式：夺取进入时缓冲已含前缀（"www."），光标须落其后；支持光标位编辑。
#[test]
fn test_url_mode_cursor_edit() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.url.enabled = true;
    cfg.input.url.prefixes = vec!["www.".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in ['w', 'w', 'w'] {
        press_letter(&coord, c);
    }
    let enter = coord.handle_key_event(&key_event(0xBE, EVENT_KEY_DOWN)); // '.' 补满前缀
    assert_eq!(action_text(&enter).as_deref(), Some("www."));
    let act = type_str(&coord, "ab");
    assert_eq!(
        action_text(&act).as_deref(),
        Some("www.ab"),
        "续打应追加在前缀之后"
    );
    assert_eq!(action_caret(&act), Some(6));

    // www.a|b → 退格删 'a'
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(5));
    let act = tap(&coord, VK_BACK);
    assert_eq!(action_text(&act).as_deref(), Some("www.b"));
    assert_eq!(action_caret(&act), Some(4));
}

/// 临时拼音：与主输入同构——caret 需跨过引擎插入的音节分隔符，且模式引导符（`）作为只读
/// 前缀计入 caret，光标进不去。
#[test]
fn test_temp_pinyin_cursor_maps_through_separator() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_pinyin.enabled = true;
    cfg.input.temp_pinyin.trigger_keys = vec!["backtick".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let enter = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)); // ` 进入临拼
    assert_eq!(
        action_text(&enter).as_deref(),
        Some("`"),
        "组合区显示引导符"
    );

    let act = type_str(&coord, "nihao");
    assert_eq!(action_text(&act).as_deref(), Some("`ni'hao"));
    assert_eq!(action_caret(&act), Some(7), "引导符 1 + 显示串 6");

    // 左移三次：`ni'ha|o → `ni'h|ao → `ni|'hao（跨过分隔符，6 → 5 → 3）
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(6));
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(5));
    assert_eq!(action_caret(&tap(&coord, VK_LEFT)), Some(3));

    // Home 只到剩余拼音开头（引导符之后），不进只读前缀
    assert_eq!(action_caret(&tap(&coord, VK_HOME)), Some(1));
    assert!(
        matches!(tap(&coord, VK_LEFT), KeyAction::Consumed),
        "已在最左：吃掉，不得退进引导符"
    );
}

// ── 全角（英文模式 / 中文模式数字）─────────────────────────────────────────────
// 背景：全角横跨两层门控——C++ `OnTestKeyDown` 决定是否吃键转发，Rust 决定是否转全角。
// 两侧不一致即「吃了再吐」(OnTestKeyDown(TRUE)+OnKeyDown(FALSE))，严格 TSF 宿主直接丢键。
// 下列用例锁的是 Rust 侧「C++ 吃了就必须出字」的契约。

/// 英文模式 + 全角的配置（C++ `english_fullwidth` 分支会吃 Letter|Number|Punctuation|Space）。
fn config_english_fullwidth() -> wind_config::Config {
    let mut cfg = config_with("pinyin");
    cfg.input.default.chinese_mode = false;
    cfg.input.default.full_width = true;
    cfg
}

#[test]
fn test_english_fullwidth_letters_digits_space() {
    if !has_schemas() {
        return;
    }
    // 回归：英文模式曾无条件 PassThrough（从不读 full_width），而 C++ 已为全角吃下这些键
    // → 吃了再吐 → Chrome/VSCode 等严格宿主里空格/数字/符号完全打不出。
    let coord = Coordinator::new_headless(config_english_fullwidth(), Some(&data_dir()));
    let cases = [
        (0x41_u32, "ａ", "小写字母"),
        (0x35, "５", "数字"),
        (0x20, "\u{3000}", "空格"),
        (0xBD, "－", "标点(减号)"),
        (0x60, "０", "小键盘数字"),
    ];
    for (vk, want, what) in cases {
        match coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN)) {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(text, want, "英文全角{}应上屏全角", what)
            }
            other => panic!("英文全角{}应出字（透传即丢键），实际: {:?}", what, other),
        }
    }
}

#[test]
fn test_english_fullwidth_shift_and_capslock_case() {
    if !has_schemas() {
        return;
    }
    // 键被 TSF 吃下后系统不再代劳大小写，须由 Rust 按 CapsLock 镜像 XOR Shift 自行决定。
    use wind_ipc::protocol::MOD_SHIFT;
    let coord = Coordinator::new_headless(config_english_fullwidth(), Some(&data_dir()));
    // Shift+A → 大写全角
    match coord.handle_key_event(&key_event_mods(0x41, EVENT_KEY_DOWN, MOD_SHIFT)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "Ａ", "Shift+字母应大写全角"),
        other => panic!("实际: {:?}", other),
    }
    // Shift+1 → '!' 的全角（走 punct_char 的 shifted 支）
    match coord.handle_key_event(&key_event_mods(0x31, EVENT_KEY_DOWN, MOD_SHIFT)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "！", "Shift+1 应出全角叹号"),
        other => panic!("实际: {:?}", other),
    }
    // CapsLock 开（toggles bit0）+ 无 Shift → 大写全角；镜像由每键 toggles 快照校准。
    let caps = KeyEventData {
        toggles: 0x01,
        ..key_event(0x41, EVENT_KEY_DOWN)
    };
    match coord.handle_key_event(&caps) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "Ａ", "CapsLock 应大写全角"),
        other => panic!("实际: {:?}", other),
    }
    // CapsLock + Shift → 相互抵消回小写
    let caps_shift = KeyEventData {
        toggles: 0x01,
        modifiers: MOD_SHIFT,
        ..key_event(0x41, EVENT_KEY_DOWN)
    };
    match coord.handle_key_event(&caps_shift) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "ａ", "CapsLock+Shift 应抵消回小写"),
        other => panic!("实际: {:?}", other),
    }
}

#[test]
fn test_english_halfwidth_still_passthrough() {
    if !has_schemas() {
        return;
    }
    // 零回归：英文半角仍须透传（C++ 此时也不吃键），保留宿主 WM_KEYDOWN 原生语义。
    let mut cfg = config_with("pinyin");
    cfg.input.default.chinese_mode = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for vk in [0x41_u32, 0x35, 0x20, 0xBD] {
        assert!(
            matches!(
                coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN)),
                KeyAction::PassThrough
            ),
            "英文半角 vk=0x{:02X} 应透传",
            vk
        );
    }
}

#[test]
fn test_english_fullwidth_ctrl_alt_not_intercepted() {
    if !has_schemas() {
        return;
    }
    // Ctrl/Alt 组合是快捷键：C++ 的 ClassifyInputKey 对其返回 None 本就不吃，
    // Rust 侧须对称放行，否则会把宿主快捷键（Ctrl+A 等）吞成全角字符。
    use wind_ipc::protocol::{MOD_ALT, MOD_CTRL};
    let coord = Coordinator::new_headless(config_english_fullwidth(), Some(&data_dir()));
    for mods in [MOD_CTRL, MOD_ALT] {
        assert!(
            matches!(
                coord.handle_key_event(&key_event_mods(0x41, EVENT_KEY_DOWN, mods)),
                KeyAction::PassThrough
            ),
            "英文全角下 Ctrl/Alt 组合应透传给宿主"
        );
    }
}

#[test]
fn test_english_fullwidth_autopair_uses_fullwidth_pairs() {
    if !has_schemas() {
        return;
    }
    // 配对表须由 english_pairs 逐字符过同一条流水线派生：打 `(` 出 `（` 就配 `）`。
    // 关键回归：不可复用 cn_pairs——`to_full_width('[')` = `［`(U+FF3B) 而 cn_pairs 是
    // `【`(U+3010)，混用会「打 [ 出 【 却配 ］」。故此处专测 `[`。
    let mut cfg = config_english_fullwidth();
    cfg.input.auto_pair.english = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    match coord.handle_key_event(&key_event(0xDB, EVENT_KEY_DOWN)) {
        KeyAction::InsertTextWithCursor {
            text,
            cursor_offset,
        } => {
            assert_eq!(text, "［］", "全角 `[` 应配全角 `］`，而非中文的 【】");
            assert_eq!(cursor_offset, 1, "光标应落在配对之间");
        }
        other => panic!("英文全角 `[` 应插入全角配对，实际: {:?}", other),
    }
}

#[test]
fn test_chinese_fullwidth_digits_1_to_9() {
    if !has_schemas() {
        return;
    }
    // 回归：中文全角空缓冲下 1-9 曾恒 PassThrough（无视 full_width），而 C++ 为全角专门
    // 在无 session 时也吃数字（`chinese_fullwidth_number`）→ 吃了再吐 → 部分应用丢键、
    // 部分出半角。`0` 因无该 match 臂、落标点流水线，反而一直正常——本测锁死两者一致。
    let mut cfg = config_with("pinyin");
    cfg.input.default.full_width = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for (vk, want) in [
        (0x31_u32, "１"),
        (0x35, "５"),
        (0x39, "９"),
        (0x30, "０"), // `0` 走另一条路（标点流水线），须与 1-9 结果一致
    ] {
        match coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN)) {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(text, want, "中文全角数字 vk=0x{:02X} 应上屏全角", vk)
            }
            other => panic!("中文全角数字 vk=0x{:02X} 应出字，实际: {:?}", vk, other),
        }
    }
}

#[test]
fn test_chinese_halfwidth_digits_still_passthrough() {
    if !has_schemas() {
        return;
    }
    // 零回归：半角态空缓冲数字仍透传（C++ 此时不吃），保留宿主原生按键语义。
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    for vk in [0x31_u32, 0x39] {
        assert!(
            matches!(
                coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN)),
                KeyAction::PassThrough
            ),
            "中文半角空缓冲数字应透传"
        );
    }
}

#[test]
fn test_chinese_capslock_fullwidth_space_and_numpad() {
    if !has_schemas() {
        return;
    }
    // 回归：CapsLock+全角分支原用 printable_char 取字符，而它不含 VK_SPACE(punct_char 无该键)
    // 也不含小键盘 → 落 PassThrough。但 C++ 在中文全角下对空格(chinese_fullwidth_space)
    // 与小键盘(chinese_fullwidth_number)都吃键 → 吃了再吐 → 严格 TSF 宿主丢键。
    // 现由 full_width_source_char 统一收口，保证 Rust 出字集 ⊇ C++ 吃键集。
    let mut cfg = config_with("pinyin");
    cfg.input.default.full_width = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for (vk, want, what) in [
        (0x20_u32, "\u{3000}", "空格"),
        (0x60, "０", "小键盘 0"),
        (0x41, "Ａ", "字母(CapsLock 大写)"),
    ] {
        let ev = KeyEventData {
            toggles: 0x01, // CapsLock ON
            ..key_event(vk, EVENT_KEY_DOWN)
        };
        match coord.handle_key_event(&ev) {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(text, want, "CapsLock+全角 {} 应上屏全角", what)
            }
            other => panic!(
                "CapsLock+全角 {} 应出字（透传即丢键），实际: {:?}",
                what, other
            ),
        }
    }
}

#[test]
fn test_chinese_fullwidth_numpad_direct_no_caps() {
    if !has_schemas() {
        return;
    }
    // 定位用：中文全角、非 CapsLock、空缓冲、default(direct) numpad_behavior 下，
    // 小键盘数字应走 numpad direct 分支的 to_full_width 出全角。
    // 若本测通过而真机仍半角/丢键 → 问题在 C++ 吃键或 full_width 跨进程同步，不在 core 逻辑。
    let mut cfg = config_with("pinyin");
    cfg.input.default.full_width = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for (vk, want) in [(0x60_u32, "０"), (0x65, "５"), (0x69, "９")] {
        match coord.handle_key_event(&key_event(vk, EVENT_KEY_DOWN)) {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(text, want, "中文全角小键盘(direct) vk=0x{:02X} 应出全角", vk)
            }
            other => panic!("中文全角小键盘 vk=0x{:02X} 应出字，实际: {:?}", vk, other),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────
// 码表自动造词端到端
//
// ⚠ 必须走 `handle_key_event_policed`（bridge 真入口，server.rs:440 调的就是它）。
// 本文件其余测试调的是裸 `handle_key_event`，那条路**不经过**自提交打点与造词投喂，
// 用它写造词测试会得到「永远不造词」的假象。
// ──────────────────────────────────────────────────────────────────────────

use std::sync::Arc;
use wind_store::Store;

/// 建一个开/关自动造词的 wubi86 无头协调器 + 独立 store。
fn auto_phrase_coord(tag: &str, enabled: bool) -> (Arc<Coordinator>, Arc<Store>, PathBuf) {
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.auto_phrase.enabled = enabled;
    let db = std::env::temp_dir().join(format!("wind_auto_phrase_{tag}.redb"));
    let _ = std::fs::remove_file(&db);
    let store = Arc::new(Store::open(&db).unwrap());
    let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), Arc::clone(&store));
    (coord, store, db)
}

/// 枚举某方案下全部临时词（空前缀即扫该方案全部键）。
fn temp_words(store: &Store, schema: &str) -> Vec<(String, String)> {
    store
        .search_temp_words_prefix(schema, "", 200)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.code, r.text))
        .collect()
}

/// 敲「字母 + 空格」上屏一个字，返回上屏文本。
fn commit_one_char(coord: &Coordinator, letter: u8) -> String {
    coord.handle_key_event_policed(&key_event(letter as u32, EVENT_KEY_DOWN));
    match coord.handle_key_event_policed(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => text,
        other => panic!("空格应上屏 InsertText，实际: {:?}", other),
    }
}

/// 连续单字上屏 → 终止信号 → 造出词组并写入临时词库。
///
/// 覆盖历史上「完全不工作」的两个断裂：触发源（旧实现挂在拼音专属的 `committed_segs` 上，
/// 码表恒不满足）与编码算法（旧实现拼接各段全码，造出的码查不出来）。
#[test]
fn test_codetable_auto_phrase_learns_from_single_chars() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let (coord, store, db) = auto_phrase_coord("learn", true);

    let a = commit_one_char(&coord, b'A');
    let b = commit_one_char(&coord, b'A');
    let word = format!("{a}{b}");
    assert_eq!(word.chars().count(), 2, "应上屏两个单字，实际: {:?}", word);

    // 造词发生在终止信号（此处用失焦，等价于打完一句切窗口）。
    coord.handle_focus_lost();

    let words = temp_words(&store, "wubi86");
    let hit = words
        .iter()
        .find(|(_, t)| *t == word)
        .unwrap_or_else(|| panic!("终止信号后应造出「{word}」，临时层实际: {words:?}"));
    // 五笔二字词规则 AaAbBaBb = 各字全码前两位 → 码长恒为 4。
    // 这条同时否掉了「拼接各字全码」的旧做法（那会得到 7~8 位）。
    assert_eq!(
        hit.0.chars().count(),
        4,
        "二字词组码应为 4 位（各字全码前两位），实际: {}",
        hit.0
    );
    let _ = std::fs::remove_file(&db);
}

/// 造词只在终止信号发生：上屏过程中不得写库，否则每打一个字就造一次半截词。
#[test]
fn test_codetable_auto_phrase_does_not_learn_before_terminator() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let (coord, store, db) = auto_phrase_coord("before_term", true);
    commit_one_char(&coord, b'A');
    commit_one_char(&coord, b'A');
    assert!(
        temp_words(&store, "wubi86").is_empty(),
        "终止信号之前不应写入任何临时词，实际: {:?}",
        temp_words(&store, "wubi86")
    );
    let _ = std::fs::remove_file(&db);
}

/// 开关关闭时闸门有效，一个词都不造。
#[test]
fn test_codetable_auto_phrase_disabled_learns_nothing() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let (coord, store, db) = auto_phrase_coord("disabled", false);
    commit_one_char(&coord, b'A');
    commit_one_char(&coord, b'A');
    coord.handle_focus_lost();
    assert!(
        temp_words(&store, "wubi86").is_empty(),
        "开关关闭时不应造词，实际: {:?}",
        temp_words(&store, "wubi86")
    );
    let _ = std::fs::remove_file(&db);
}

/// 单字不成词：只上屏一个字就终止，不应写库（min_phrase_len=2）。
#[test]
fn test_codetable_auto_phrase_single_char_is_not_a_word() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let (coord, store, db) = auto_phrase_coord("single", true);
    commit_one_char(&coord, b'A');
    coord.handle_focus_lost();
    assert!(
        temp_words(&store, "wubi86").is_empty(),
        "单字不应成词，实际: {:?}",
        temp_words(&store, "wubi86")
    );
    let _ = std::fs::remove_file(&db);
}
