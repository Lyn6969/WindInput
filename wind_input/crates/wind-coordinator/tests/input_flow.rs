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
    // 三级：crates/wind-coordinator → crates → wind_input → 仓库根（build_dev 在仓库根）。
    // 曾误写成两级，解析到 wind_input/build_dev/data —— 该目录不存在，于是下面的
    // exists() 判假、整个测试族静默走「跳过」分支通过。**判据是耗时 0.00s**。
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
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

    // 首选是**结果**（用算式形态的是少数），等式次之，随后是结果的金额读法
    let texts = coord.debug_page_texts();
    assert_eq!(texts[0], "7", "计算首选应为结果，实际: {:?}", texts);
    assert_eq!(texts[1], "1+2*3=7", "等式形态应为次选，实际: {:?}", texts);
    assert!(
        texts.contains(&"柒元整".to_string()),
        "计算结果应同时给出金额读法，实际: {:?}",
        texts
    );

    // 字母 a 选第 1 个候选上屏
    match press_vk(&coord, 0x41, false) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "7"),
        other => panic!("字母 a 应上屏首选，实际: {:?}", other),
    }
}

/// 幂运算 `^`（Shift+6）：优先级高于乘除，结果作首选。
#[test]
fn test_quick_input_power_operator() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    press_vk(&coord, 0x32, false); // 2
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x33, false); // 3
    press_vk(&coord, 0x36, true); // ^ (Shift+6)
    press_vk(&coord, 0x32, false); // 2
    let texts = coord.debug_page_texts();
    assert_eq!(texts[0], "11", "2+3^2 应先算幂（=2+9），实际: {:?}", texts);
    assert_eq!(texts[1], "2+3^2=11", "实际: {:?}", texts);
}

/// 日期打到一半的尾点（`2026.3.`）不应清空候选——年月候选须维持。
#[test]
fn test_quick_input_date_trailing_dot_keeps_year_month() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    for vk in [0x32, 0x30, 0x32, 0x36] {
        press_vk(&coord, vk, false); // 2026
    }
    press_vk(&coord, 0xBE, false); // .
    press_vk(&coord, 0x33, false); // 3
    let before = coord.debug_page_texts();
    assert!(
        before.contains(&"2026年3月".to_string()),
        "2026.3 应有年月候选，实际: {:?}",
        before
    );
    press_vk(&coord, 0xBE, false); // 第二个 . —— 此前候选在此清空
    let after = coord.debug_page_texts();
    assert!(
        after.contains(&"2026年3月".to_string()),
        "2026.3. 应维持年月候选，实际: {:?}",
        after
    );
}

/// 重复上屏（成员 `quick_input.repeat`）：空缓冲时把上次上屏内容作唯一候选，空格再上屏一次。
#[test]
fn test_quick_input_repeat_last_commit() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    // 先用快捷输入上屏一次计算结果
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    press_vk(&coord, 0x31, false); // 1
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x32, false); // 2
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "3", "空格应上屏计算结果"),
        other => panic!("空格应上屏首选，实际: {:?}", other),
    }
    // 再进快捷输入：空缓冲应出重复候选
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    let texts = coord.debug_page_texts();
    assert_eq!(
        texts,
        vec!["3"],
        "空缓冲应显示上次上屏内容，实际: {:?}",
        texts
    );
    // 空格重复上屏
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "3", "空格应重复上屏"),
        other => panic!("空格应重复上屏，实际: {:?}", other),
    }
}

/// 移除成员即关闭该来源：members 去掉 `quick_input.calc` 后，算式不再产出计算候选
/// （金额来源仍会对结果求值，故这里连 number 一起移除，验证「开关=增删」）。
#[test]
fn test_quick_input_member_removal_disables_source() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.schema.mix_modes[0]
        .members
        .retain(|m| m != wind_quick_input::MEMBER_CALC && m != wind_quick_input::MEMBER_NUMBER);
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    press_vk(&coord, 0x31, false); // 1
    press_vk(&coord, 0xBB, true); // +
    press_vk(&coord, 0x32, false); // 2
    let texts = coord.debug_page_texts();
    assert!(
        texts.is_empty(),
        "移除 calc/number 成员后算式不应有候选，实际: {:?}",
        texts
    );
    // 日期成员仍在：日期照常出候选（证明关的是单个来源而非整个快捷输入）
    let coord2 = {
        let mut c = config_with("wubi86");
        c.schema.mix_modes[0]
            .members
            .retain(|m| m != wind_quick_input::MEMBER_CALC);
        Coordinator::new_headless(c, Some(&data_dir()))
    };
    coord2.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN));
    for vk in [0x31, 0x32] {
        press_vk(&coord2, vk, false); // 12
    }
    press_vk(&coord2, 0xBE, false); // .
    for vk in [0x32, 0x35] {
        press_vk(&coord2, vk, false); // 25
    }
    let texts2 = coord2.debug_page_texts();
    assert!(
        texts2.iter().any(|t| t.ends_with("月25日")),
        "date 成员仍在，日期候选应照常产出，实际: {:?}",
        texts2
    );
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
    // 中文日期是首选（中文输入法场景下最常用），且不产出补零的中文写法
    assert_eq!(texts[0], "2025年12月25日", "实际: {:?}", texts);
    assert_eq!(
        texts.iter().filter(|t| t.contains('年')).count(),
        1,
        "中文日期只应有不补零的一条（补零写法不合 GB/T 15835），实际: {:?}",
        texts
    );
    // 空格上屏高亮（首选）
    match coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "2025年12月25日"),
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

// ───── enter_behavior=clear 在各模式的「非空缓冲」路径同样生效 ─────
//
// 回归保护：此前四个模式 handler 的回车分支都把 enter_behavior 判断写在
// `if buffer.is_empty()` **内部**，于是「打了码再按回车」走非空缓冲路径无条件上屏原码，
// 配置只对「什么都没打就回车」生效。指纹＝空缓冲时配置生效、打了码就失效。
//
// 每个测试都必须先断言「确实进了模式」：触发键若没生效，按键会落到主输入路径，
// 而主输入路径的 clear 同样返回 ClearComposition —— 不验进入就是假绿。

/// 临时拼音打了码再回车：clear 应整段放弃，不上屏拼音原码。
#[test]
fn test_temp_pinyin_nonempty_enter_clear_discards() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.enter_behavior = "clear".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)); // ` 进入临拼
    assert_eq!(action_text(&act).unwrap(), "`", "反引号应进入临时拼音");
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    let disp = action_text(&press_letter(&coord, 'o')).unwrap_or_default();
    assert!(
        disp.starts_with('`'),
        "字母应进临拼缓冲（组合区以 ` 开头），实际: {:?}",
        disp
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("clear 模式临拼非空缓冲回车应清空不上屏，实际: {:?}", other),
    }
    let act = press_letter(&coord, 'a');
    assert_eq!(action_text(&act).unwrap(), "a", "回车清空后应已退出临拼");
}

/// 对照组：commit 模式（默认）下同样操作仍应上屏原码。
/// 没有它，上面的测试无法区分「配置生效」与「临拼回车本来就不上屏」。
#[test]
fn test_temp_pinyin_nonempty_enter_commit_still_outputs_code() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xC0, EVENT_KEY_DOWN)); // `
    for c in "nihao".chars() {
        press_letter(&coord, c);
    }
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "nihao", "commit 模式临拼回车应上屏拼音原码");
        }
        other => panic!("commit 模式临拼回车应上屏原码，实际: {:?}", other),
    }
}

/// 快捷输入（混合模式）打了码再回车：clear 应整段放弃。
#[test]
fn test_quick_input_nonempty_enter_clear_discards() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.enter_behavior = "clear".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    assert_eq!(action_text(&act).unwrap(), ";", "分号应进入快捷输入");
    let mut disp = String::new();
    for c in "abc".chars() {
        disp = action_text(&press_letter(&coord, c)).unwrap_or_default();
    }
    assert!(
        disp.starts_with(';'),
        "字母应进快捷输入缓冲，实际组合区: {:?}",
        disp
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "clear 模式快捷输入非空缓冲回车应清空不上屏，实际: {:?}",
            other
        ),
    }
}

/// 对照组：commit 模式下快捷输入回车仍上屏缓冲原文。
#[test]
fn test_quick_input_nonempty_enter_commit_still_outputs_code() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)); // ;
    for c in "abc".chars() {
        press_letter(&coord, c);
    }
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "abc", "commit 模式快捷输入回车应上屏缓冲原文");
        }
        other => panic!("commit 模式快捷输入回车应上屏原文，实际: {:?}", other),
    }
}

/// 特殊模式打了码再回车：clear 应整段放弃，不上屏编码原文。
#[test]
fn test_special_mode_nonempty_enter_clear_discards() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.enter_behavior = "clear".into();
    cfg.schema.special_modes = vec![wind_config::config::SpecialModeConfig {
        id: "sym".into(),
        trigger_keys: vec!["backslash".into()],
        schema: "pinyin".into(),
        ..Default::default()
    }];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xDC, EVENT_KEY_DOWN)); // \
    assert!(
        matches!(act, KeyAction::UpdateComposition { .. }),
        "反斜杠应进入特殊模式，实际: {:?}",
        act
    );
    let mut disp = String::new();
    for c in "ni".chars() {
        disp = action_text(&press_letter(&coord, c)).unwrap_or_default();
    }
    assert!(
        disp.contains("ni"),
        "字母应进特殊模式编码缓冲，实际组合区: {:?}",
        disp
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!(
            "clear 模式特殊模式非空缓冲回车应清空不上屏，实际: {:?}",
            other
        ),
    }
}

/// 临时英文**豁免** clear：打了内容再回车仍须上屏。
///
/// 临英缓冲装的是英文原文而非「编码」，且 `space_as_input` 开启后空格被占作输入字符、
/// 上屏职责整个压在回车上 —— clear 若管辖非空缓冲，本模式一个上屏通路都不剩。
/// 故临英的 clear 只管空缓冲（见下一个测试），非空缓冲无条件上屏。
#[test]
fn test_temp_english_nonempty_enter_clear_still_commits() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with_english_trigger("wubi86", "slash");
    cfg.input.enter_behavior = "clear".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN)); // /
    assert_eq!(action_text(&act).unwrap(), "/", "斜杠应进入临时英文");
    let mut disp = String::new();
    for c in "abc".chars() {
        disp = action_text(&press_letter(&coord, c)).unwrap_or_default();
    }
    assert!(
        disp.starts_with('/'),
        "字母应进临英缓冲，实际组合区: {:?}",
        disp
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "abc", "临英非空缓冲回车应豁免 clear、照常上屏原文");
        }
        other => panic!("临英非空缓冲回车应上屏原文，实际: {:?}", other),
    }
}

/// 临英 clear 的**保留边界**：空缓冲（只按了触发键）回车仍按 clear 放弃，不回显触发键字符。
/// 没有它，「豁免」会被误实现成「临英完全不读 enter_behavior」而无人察觉。
#[test]
fn test_temp_english_empty_enter_clear_discards_prefix() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with_english_trigger("wubi86", "slash");
    cfg.input.enter_behavior = "clear".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN)); // /
    assert_eq!(action_text(&act).unwrap(), "/", "斜杠应进入临时英文");
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::ClearComposition => {}
        other => panic!("clear 模式临英空缓冲回车应清空不上屏，实际: {:?}", other),
    }
}

/// 用户实报场景：`space_as_input` + `enter_behavior=clear` 叠加曾使临英**没有任何上屏通路**
/// —— 空格让位给输入字符，回车又被 clear 拿走，打进去的英文只能靠 Esc 丢弃。
#[test]
fn test_temp_english_space_as_input_enter_clear_still_commits() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with_english_trigger("wubi86", "slash");
    cfg.input.enter_behavior = "clear".into();
    cfg.input.temp_english.space_as_input = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    let act = coord.handle_key_event(&key_event(0xBF, EVENT_KEY_DOWN)); // /
    assert_eq!(action_text(&act).unwrap(), "/", "斜杠应进入临时英文");
    for c in "hi".chars() {
        press_letter(&coord, c);
    }
    coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)); // 空格入缓冲
    let mut last = KeyAction::Consumed;
    for c in "there".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(
        action_text(&last).unwrap(),
        "/hi there",
        "前置条件：空格应入缓冲而非上屏"
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(
                text, "hi there",
                "space_as_input + clear 下回车仍须上屏整句"
            );
        }
        other => panic!("回车应上屏整句，实际: {:?}", other),
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
        ..Default::default()
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
fn test_s2t_one_to_many_variant_expansion() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    if !coord.debug_set_s2t(true) {
        eprintln!("跳过：缺少 opencc 数据");
        return;
    }
    // 拼音输入 chu → 单字候选「出」（STCharacters 多值行 出→出 齣）应紧跟变体「齣」。
    for c in "chu".chars() {
        press_letter(&coord, c);
    }
    let internal = coord.debug_page_texts();
    let display = coord.debug_page_display_texts();
    let Some(p) = display.iter().position(|t| t == "出") else {
        panic!("输入 chu 候选应含「出」，实际: {:?}", display);
    };
    assert!(
        p + 1 < display.len() && display[p + 1] == "齣",
        "「出」之后应紧跟 1对多变体「齣」，实际: {:?}",
        display
    );
    // 变体候选**内部 text 保持简体**（词频/匹配域不被繁体污染）。
    assert_eq!(internal[p], "出");
    assert_eq!(internal[p + 1], "出", "变体候选内部 text 应仍是简体「出」");
    // 选中变体（页内第 p+2 项，数字键 1-based）→ 上屏「齣」而非默认转换的「出」。
    match coord.handle_key_event(&key_event(0x31 + (p + 1) as u32, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "齣", "选中变体候选应上屏「齣」");
        }
        other => panic!("应上屏 InsertText，实际: {:?}", other),
    }
}

#[test]
fn test_s2t_variant_absent_when_disabled() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("pinyin"), Some(&data_dir()));
    // 简繁关闭：不展开变体，「出」之后不应出现内部 text 重复的变体候选。
    for c in "chu".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    let dup_adjacent = texts.windows(2).any(|w| w[0] == "出" && w[1] == "出");
    assert!(
        !dup_adjacent,
        "简繁关闭时不应出现展开产生的相邻重复候选: {:?}",
        texts
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

/// 临英候选排布：`原文 → 大小写变形 → 词库原文`，且词库候选**不再被套上输入的大小写形态**。
/// 回归点：临英由 Shift+字母进入，缓冲首字母恒大写，旧实现据此把整列词库候选适配成
/// `Help`/`Held`/`Hell`，于是「候选全是大写首字母」。
#[test]
fn test_temp_english_case_variants_and_dict_keeps_original_case() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'h'); // H
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // 缓冲 "Hel"

    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts.iter().take(3).collect::<Vec<_>>(),
        vec!["Hel", "hel", "HEL"],
        "前三候选应为 原文 → 全小写 → 全大写（原文已是首字母大写，Title 变形被去重），实际: {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "help"),
        "词库候选应保持原文小写，实际: {:?}",
        texts
    );
    assert!(
        !texts.iter().any(|t| t == "Help"),
        "词库候选不应被适配成输入的首字母大写形态，实际: {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "Helen"),
        "词库中本就大写的专有名词应原样保留，实际: {:?}",
        texts
    );
}

/// 变形候选对全小写 / 全大写输入同样自洽：缺哪种形态就补哪种，原文永远排首位。
#[test]
fn test_temp_english_case_variants_from_lowercase_entry() {
    if !has_schemas() {
        return;
    }
    // 触发键进入 → 缓冲首字母不受 Shift 影响，可打出全小写原文。
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.trigger_keys = vec!["/".to_string()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_vk(&coord, 0xBF, false); // "/" 进入临英
    for c in "hel".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts.iter().take(3).collect::<Vec<_>>(),
        vec!["hel", "Hel", "HEL"],
        "全小写输入应补出首字母大写与全大写两个变形，实际: {:?}",
        texts
    );
}

/// `case_variants = false`：不再生成大小写变形候选，原文之后直接是词库候选。
///
/// 与上面两个测试互为正反面——它们钉住「开着时变形恒在前三位」，这条钉住「关掉即消失」。
/// 只有开态测试的话，开关没接上（读了配置但没用）照样全绿。
#[test]
fn test_temp_english_case_variants_disabled() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.case_variants = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h'); // H
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // 缓冲 "Hel"

    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts.first().map(|s| s.as_str()),
        Some("Hel"),
        "原文仍是首候选"
    );
    assert!(
        !texts.iter().any(|t| t == "hel" || t == "HEL"),
        "关掉后不得再有大小写变形候选，实际: {:?}",
        texts
    );
    assert!(
        texts.iter().any(|t| t == "help"),
        "词库候选不受影响（本开关只管变形项），实际: {:?}",
        texts
    );
}

/// allow_symbols 开：数字键 1-9 一律入缓冲（英文原文优先于选词），即使此刻有词库候选。
#[test]
fn test_temp_english_allow_symbols_digits_go_to_buffer() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.allow_symbols = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // "Hel" —— 此刻词库候选非空
    assert!(
        coord.debug_all_candidate_texts().len() > 1,
        "前置条件：此刻应有候选，否则测不出「有候选时数字仍入缓冲」"
    );
    let act = press_vk(&coord, 0x32, false); // 2
    assert_eq!(
        action_text(&act).unwrap(),
        "Hel2",
        "allow_symbols 开启时数字应入缓冲而非选第 2 个候选"
    );
    // 符号同样入缓冲（既有行为），并可继续与数字混排。
    let act = press_vk(&coord, 0xBD, false); // "-"
    assert_eq!(action_text(&act).unwrap(), "Hel2-", "符号应入缓冲");
}

/// 对照组：allow_symbols 关（默认）时数字键仍是选词键——守住既有行为不被上面的改动误伤。
#[test]
fn test_temp_english_digits_still_select_when_symbols_disallowed() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // 候选 [Hel, hel, HEL, held, ...]
    match coord.handle_key_event(&key_event(0x32, EVENT_KEY_DOWN)) {
        // 2 → 第 2 个候选 = 全小写变形
        KeyAction::InsertText { text, .. } => assert_eq!(text, "hel"),
        other => panic!("数字键应选第 2 个候选并上屏，实际: {:?}", other),
    }
}

/// 二三候选键（默认 `;` `'`）在临英下应选中对应候选。
/// 回归点：临英曾是唯一没接 `select_key_offset` 的模式处理器，`;` 一路落到标点臂被判成
/// 「上屏高亮候选 + 标点」，用户按次选键实得**首候选被直接上屏**。
#[test]
fn test_temp_english_select_keys_pick_candidates() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    // 显式声明键组，使本测试不随默认值漂移（默认亦为 semicolon_quote）。
    cfg.keys.select_key_groups = vec!["semicolon_quote".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // 候选 [Hel, hel, HEL, held, ...]
    // 前置条件：页内至少 3 项，否则 `gi < end` 不成立，选词分支根本执行不到（假绿）。
    assert!(
        coord.debug_all_candidate_texts().len() >= 3,
        "前置条件：应有 ≥3 个候选，否则测不到二/三选键的选词分支"
    );
    match coord.handle_key_event(&key_event(0xBA, EVENT_KEY_DOWN)) {
        // `;` → 次选 = 第 2 候选（全小写变形）
        KeyAction::InsertText { text, .. } => assert_eq!(text, "hel"),
        other => panic!("`;` 应选第 2 候选并上屏，实际: {:?}", other),
    }

    // 三选键 `'` → 第 3 候选（全大写变形）。复用同一 Coordinator 重新进临英——
    // 上屏后临英已退出，重打即可。刻意不新建实例：引擎 reader / LRU 跨实例共享且带
    // 配额语义（见 mmap 共享 reader 的设计），一个测试建多个实例会与并行跑的其他
    // 测试争用，实测会让无关测试偶发失败。
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    match coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "HEL"),
        other => panic!("`'` 应选第 3 候选并上屏，实际: {:?}", other),
    }
}

/// 对照组一：allow_symbols 开时二三候选键让位于字符输入——该开关的声明语义是
/// 符号「入缓冲而非上屏退出**或选词**」，与数字臂同构，不能被上面的接线改动破坏。
#[test]
fn test_temp_english_select_keys_yield_to_input_when_symbols_allowed() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.select_key_groups = vec!["semicolon_quote".into()];
    cfg.input.temp_english.allow_symbols = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    assert!(
        coord.debug_all_candidate_texts().len() >= 3,
        "前置条件：应有 ≥3 个候选，否则「有候选仍不选词」无从谈起"
    );
    let act = press_vk(&coord, 0xBA, false); // `;`
    assert_eq!(
        action_text(&act).unwrap(),
        "Hel;",
        "allow_symbols 开启时 `;` 应入缓冲而非选第 2 候选"
    );
}

/// 对照组二：页内候选不足时 `;` 仍走标点臂（上屏高亮候选 + 标点并退出），
/// 守住越界语义不被选词接线误伤。`show_candidates` 关 → 候选只剩原文一项。
#[test]
fn test_temp_english_select_key_overflow_falls_back_to_punct() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.select_key_groups = vec!["semicolon_quote".into()];
    cfg.input.temp_english.show_candidates = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    assert_eq!(
        coord.debug_all_candidate_texts().len(),
        1,
        "前置条件：show_candidates 关时应只剩原文候选，次选键才会越界"
    );
    let act = press_vk(&coord, 0xBA, false); // `;`
    let text = action_text(&act).expect("越界时应按标点臂上屏");
    assert!(
        text.starts_with("Hel") && text.chars().count() == 4,
        "越界时应上屏「原文 + 转换后标点」，实际: {:?}",
        text
    );
}

/// space_as_input 开：空格被占作输入字符，回车接过「上屏高亮候选」的职责。
/// 回归点：该配置下空格不再选词，`allow_symbols` 再开则数字键也让位，若回车仍固定上屏原文，
/// 就一个选词键都不剩、候选窗形同虚设。
#[test]
fn test_temp_english_space_as_input_enter_commits_highlighted() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.space_as_input = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l'); // 候选 [Hel, hel, HEL, hell, ...]
    coord.handle_key_event(&key_event(0x28, EVENT_KEY_DOWN)); // ↓ 高亮第 1 项
    let (_, sel, _) = coord.debug_page_info();
    assert_eq!(sel, 1, "前置条件：下方向键应把高亮移到第 1 项");
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "hel", "space_as_input 下回车应上屏高亮候选")
        }
        other => panic!("回车应上屏高亮候选，实际: {:?}", other),
    }
}

/// 同上配置但**未导航**：高亮停在首候选（=用户原文），故回车仍上屏原文——
/// 对「回车上屏原文」的既有直觉向下兼容，只有主动导航过才会上屏别的候选。
#[test]
fn test_temp_english_space_as_input_enter_without_nav_commits_original() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.space_as_input = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => assert_eq!(text, "Hel", "未导航时回车应上屏原文"),
        other => panic!("回车应上屏原文，实际: {:?}", other),
    }
}

/// space_as_input 开的端到端：空格入缓冲打出带空格的短句，回车上屏整句。
#[test]
fn test_temp_english_space_as_input_multiword_enter() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.temp_english.space_as_input = true;
    cfg.input.temp_english.trigger_keys = vec!["/".to_string()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_vk(&coord, 0xBF, false); // "/" 进入（首字母不受 Shift 影响）
    for c in "hi".chars() {
        press_letter(&coord, c);
    }
    coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN)); // 空格入缓冲
    let mut last = KeyAction::Consumed;
    for c in "there".chars() {
        last = press_letter(&coord, c);
    }
    assert_eq!(
        action_text(&last).unwrap(),
        "/hi there",
        "空格应入缓冲（组合区含触发键前缀）"
    );
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "hi there", "回车应上屏整句（高亮在首候选=原文）")
        }
        other => panic!("回车应上屏整句，实际: {:?}", other),
    }
}

/// 对照组：space_as_input 关（默认）时回车仍固定上屏原文，**即使已导航到别的候选**——
/// 此时空格才是选词键，回车的「放弃候选、要我打的原文」语义必须保住。
#[test]
fn test_temp_english_enter_commits_original_when_space_selects() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_with("wubi86"), Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    coord.handle_key_event(&key_event(0x28, EVENT_KEY_DOWN)); // ↓ 高亮第 1 项 (hel)
    match coord.handle_key_event(&key_event(0x0D, EVENT_KEY_DOWN)) {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "Hel", "默认配置下回车应上屏原文而非高亮候选")
        }
        other => panic!("回车应上屏原文，实际: {:?}", other),
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

/// 混输打 `xu`：拼音精确候选「需」必须进首页，不得被码表 `xu*` 的前缀补全整体压后。
///
/// **真机现场**（本测试即其回归）：首选是码表精确全码「弱」（`xu` 是二简码，权重 9950+1e7），
/// 而拼音的「需」（`code==xu` 精确匹配、该音节最高频字 6999）被 `xu*` 的码表前缀补全整体压住。
/// 短路本改动实测「需」落在**第 98 位**（报告者 `per_page=5` ⇒ 正是其所报的第 20 页）；候选前 12 条
/// 全是五笔：`["弱","缮","绊","弹","缯","缔","绞","缣","缢","弱点","弹幕","弹性"]`。
/// 词库侧规模：主库 130 条加 extra 4 条，按 `text` 去重后 124 条 `xu*` 前缀补全。
/// 根因是混输的档位系统只承认码表那一半「精确 vs 前缀」：码表精确 `+1e7`、码表前缀补全
/// `+PARTIAL_MATCH_BOOST`(500K)，而拼音**不分精确与补全**统一 `÷PINYIN_TIER_SCALE`(100)。
///
/// ⚠️ `new_headless` 的 `store` 为 `None` ⇒ `freq_rerank` 不参与（其触发前提要求有词频记录），
/// 故本测试测的是纯 `candidate_display_order` 的效果 —— 正是文档「验证匹配层类改动必须关自动
/// 调频」要求的隔离条件。`freq_tier` 侧的同款档位另由 wind-engine 的单测覆盖。
#[test]
fn mixed_xu_pinyin_exact_reaches_first_page() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    // 贴合真机配置：filter_mode=general（只保留常用字）——提档判据 is_common 与该模式同口径。
    let mut cfg = config_mixed();
    cfg.input.filter_mode = "general".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord, 'x');
    press_letter(&coord, 'u');
    let texts = coord.debug_all_candidate_texts();

    // 前置一：码表精确全码仍稳居首位（本改动**不得**动摇「码表精确 > 拼音」这条硬约束）。
    assert_eq!(
        texts.first().map(String::as_str),
        Some("弱"),
        "码表精确全码「弱」必须仍是首选，实际: {:?}",
        &texts[..texts.len().min(10)]
    );
    // 前置二：确认码表前缀补全候选**确实在场** —— 否则本测试退化成「没有竞争者的假绿」。
    assert!(
        texts
            .iter()
            .any(|t| t == "弹幕" || t == "弹性" || t == "弱点"),
        "前置：xu 的码表前缀补全候选应在候选列表内，实际: {:?}",
        &texts[..texts.len().min(20)]
    );

    let pos = texts.iter().position(|t| t == "需").unwrap_or_else(|| {
        panic!(
            "「需」应在候选中，实际: {:?}",
            &texts[..texts.len().min(20)]
        )
    });
    assert!(
        pos < 7,
        "「需」应进首页（per_page=7），实际第 {} 位；前 12 条: {:?}",
        pos + 1,
        &texts[..texts.len().min(12)]
    );
}

/// 混输打 `aaw`（本意是 `aawt`→「工作」）：拼音的**部分匹配整句**不得抢走首位。
///
/// 真机现象：`aaw` 时首选变成拼音「啊啊」，把 `a`+`a` 拆成两个音节。
///
/// ★ 这是「拼音精确档」判据的边界：五笔 `aaw` **无精确全码**（候选全是 `aawt` 工作 / `aawf`
/// 工会 一类前缀补全），所以没有 `is_exact_code=true` 的候选占着首位 —— 一旦拼音被误判进精确
/// 档，它就直接是首选。而「啊啊」正是那个误判：
/// - 它是 Viterbi 整句（词条 `啊啊 a a`），`code` 取 `completed`="aa"、`consumed_length=2`，
///   只解释了 3 键中的 2 键，`w` 是残码；
/// - 但 `is_partial` **是 false** —— 整句走 `insert(0)` 不经 `push_hit` 闭包，且同文合并时
///   `mod.rs` 还会主动 `existing.is_partial = false`（其语义是「这不是子短语」，不是「消费了整串」）。
///
/// 故判据不能拿 `!is_partial` 代替「消费整串」，必须直接问 `consumed_length`。
#[test]
fn mixed_aaw_partial_sentence_does_not_preempt_codetable() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let mut cfg = config_mixed();
    cfg.input.filter_mode = "general".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "aaw".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_all_candidate_texts();
    // 前置：确认码表前缀补全确实在场（否则本用例退化成「没有竞争者」的假绿）。
    assert!(
        texts.iter().any(|t| t == "工作"),
        "前置：aawt→「工作」应在候选内，实际: {:?}",
        &texts[..texts.len().min(15)]
    );
    assert_eq!(
        texts.first().map(String::as_str),
        Some("工作"),
        "首选应是码表前缀补全 aawt→「工作」(w=2268)，而非只消费 2/3 键的拼音整句「啊啊」；         前 10 条: {:?}",
        &texts[..texts.len().min(10)]
    );
}

/// 反向锁：**纯拼音**方案不受本改动影响（拼音精确档只在混输生效）。
/// 纯拼音下全体候选同为 `Pinyin` 来源，若那个层级键误在此生效，会退化成「is_common 优先」，
/// 把含生僻字的多字词硬降到全部常用单字之后。此处以「打 `xu` 首选仍是最高频字」把住基线。
#[test]
fn pure_pinyin_xu_order_is_unaffected() {
    if !has_schemas() {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let mut cfg = config_with("pinyin");
    cfg.input.filter_mode = "general".into();
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_letter(&coord, 'x');
    press_letter(&coord, 'u');
    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts.first().map(String::as_str),
        Some("需"),
        "纯拼音下 xu 首选应是最高频字「需」，实际: {:?}",
        &texts[..texts.len().min(10)]
    );
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

/// ★ 混输**超码长**回捞的码表前缀候选只解释得了前 N 码，选中时必须只消费那 N 码。
///
/// `yijg` 是五笔全码「就是」，再打一个 `a` 即超码长（五笔 4 码封顶）。引擎的
/// `codetable_owns_overflow` 把「就是」回捞到首位（拼音的 `jg` 不成音节，主张不了这串），
/// 此时它的 `code` 只覆盖 `yijg` —— 选中后 `a` 必须留在缓冲里继续参与输入。
///
/// 修复前码表候选 `consumed_length` 恒 0 ⇒ 协调器 `commit_selected` 的
/// `partial = consumed > 0 && consumed < total` 恒为 false ⇒ 走「消费整串」分支整体上屏，
/// 尾码 `a` 凭空消失。同一条链路上 `github` 打出的是更刺眼的版本（首选「不算」+ 吃掉 `ub`）。
#[test]
fn test_mixed_overflow_prefix_candidate_consumes_only_prefix() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(config_mixed(), Some(&data_dir()));
    for c in "yijga".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    assert_eq!(
        texts.first().map(String::as_str),
        Some("就是"),
        "前置：回捞的码表候选应在首位，否则下面选中的不是被测候选。实际: {:?}",
        &texts[..texts.len().min(5)]
    );

    // 空格选首选：应留在组合区（分段），而非整体上屏。
    let act = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match &act {
        KeyAction::UpdateComposition { text, .. } => assert_eq!(
            text, "就是a",
            "「就是」应进组合区前缀、尾码 a 留在缓冲继续输入"
        ),
        other => panic!("不应整串上屏（那会吃掉尾码 a），实际: {other:?}"),
    }
}

/// ★ 真机回归（用户报告）：混输 + 英文词库下打 `github`，首候选变成五笔词「不算」，
/// 空格上屏还把尾码 `ub` 一并吃掉。
///
/// 成因是超码长归属判据只问了「拼音主张不主张」：`github` 前 4 码 `gith` 在五笔主库确是精确
/// 全码「不算」，而 `gi` 不成音节 ⇒ 拼音交不出候选，于是归属判给码表，码表精确 `+1e7` 把英文
/// 精确档 `+500K` 整层压掉。判据补上「英文主张不主张」后归属回到英文。
///
/// 配置取用户的真实场景：`enable_english` + `auto_commit_block_on_english` 都开（后者不开的话
/// 第 5 键 `githu` 就被顶码顶走了，那是另一条通路，见 `mixed_overflow_codetable_claim.rs` 的
/// `topcode_on_english_word_is_still_governed_by_the_english_guard`）。
#[test]
fn test_mixed_english_word_keeps_overflow_ownership() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_mixed();
    cfg.schema.mix.enable_english = true;
    cfg.schema.mix.auto_commit_block_on_english = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    for c in "github".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    assert!(
        !texts.iter().any(|t| t == "不算"),
        "码表前缀候选（只解释得了 gith）不得夺走 github 的归属，实际: {:?}",
        &texts[..texts.len().min(6)]
    );
    assert!(
        texts.iter().any(|t| t.eq_ignore_ascii_case("github")),
        "英文候选 GitHub 应在列，实际: {:?}",
        &texts[..texts.len().min(6)]
    );
}

/// ★ 真机回归（用户报告的原始配置：**英文词库关着**，即出厂默认）：打 `github` 首候选是五笔词
/// 「不算」，空格上屏还把整个缓冲吃掉。
///
/// 此时英文引擎不在场，前三条归属判据全部放行（`gith` 是精确全码、`gi` 不成音节 ⇒ 拼音主张
/// 不了、英文缺席），全靠第四条「拼音须交得出候选」兜住：`github` 拼音一条候选都出不来，
/// 说明它连开头都不在中文语境里，码表没有依据主张它。候选保持为空，空格直接上屏原码。
///
/// 对照 `test_mixed_overflow_prefix_candidate_consumes_only_prefix`：`yijga` 的拼音出得来「以」，
/// 码表照常主张 —— 两条用例只差「拼音交不交得出候选」这一个变量。
#[test]
fn test_mixed_non_chinese_overflow_falls_back_to_raw_code() {
    if !has_schemas() {
        return;
    }
    // 出厂默认即 enable_english=false，此处不额外开启，就是用户的配置。
    let coord = Coordinator::new_headless(config_mixed(), Some(&data_dir()));
    for c in "github".chars() {
        press_letter(&coord, c);
    }
    let texts = coord.debug_page_texts();
    assert!(
        texts.is_empty(),
        "github 不该被五笔前 4 码强行解释，候选应为空，实际: {:?}",
        &texts[..texts.len().min(6)]
    );

    let act = coord.handle_key_event(&key_event(0x20, EVENT_KEY_DOWN));
    match &act {
        KeyAction::InsertText { text, .. } => {
            assert_eq!(text, "github", "空码空格应上屏原码全串")
        }
        other => panic!("应上屏原码 github，实际: {other:?}"),
    }
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
    //
    // 契约是**带空格的音节码**（`ni hao`），让设置页用户看清拼音词库的音节格式。
    // 原断言只有 `is_string()`，契约从扁平码改成空格码时它照样绿——弱断言等于没有断言。
    let code = coord
        .web_data_rpc(
            "dict.encode",
            &serde_json::json!({ "schemaId": "pinyin", "text": "你好" }),
        )
        .unwrap();
    assert_eq!(
        code.as_str(),
        Some("ni hao"),
        "dict.encode 应回带空格的音节码"
    );
    let gen_code = coord
        .web_data_rpc("dict.genPinyin", &serde_json::json!({ "text": "你好" }))
        .unwrap();
    assert_eq!(gen_code.as_str(), Some("ni hao"), "dict.genPinyin 同源同形");
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

/// 精确匹配空码补全的判据须落在**最终显示列表**上，而非某一层的局部视野。
///
/// 回归：码表引擎在协调器注入短语**之前**按自己那半边判空，于是 `aab`（五笔全库无精确字、
/// 主库有 aabx 后继）无条件被补上一条更长编码候选；随后精确码短语 aab 再进来 → 屏幕上短语
/// 旁边多出一条与输入无关的「后续」。反向同源：引擎抢先把列表填非空，又会让短语侧的补全
/// 枚举误判「已有候选」而放弃，该补的短语反倒不补。
///
/// `aab` 的选取依据：六个 wubi86 词库均无 code=="aab" 的精确项，主库有 4 条 aab? 后继——
/// 即「码表侧必然想补、且补得出来」，是这个 bug 的最小复现条件。
fn coord_exact_completion(with_phrase: bool) -> std::sync::Arc<Coordinator> {
    let store_path =
        std::env::temp_dir().join(format!("wind_exact_completion_{}.redb", with_phrase as u8));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    if with_phrase {
        store.add_phrase("aab", "短语占位", 0, 10).unwrap();
    }
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.single_code_input = true;
    cfg.schema.codetable.single_code_complete = true;
    cfg.input.phrase.min_prefix = 2;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

#[test]
fn exact_mode_completion_yields_to_phrase() {
    if !has_schemas() {
        return;
    }
    let coord = coord_exact_completion(true);
    for ch in ['a', 'a', 'b'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts,
        vec!["短语占位".to_string()],
        "短语已占位时不应再补码表后续——补全以最终屏幕候选数为准，实际: {:?}",
        texts
    );
}

#[test]
fn exact_mode_completion_fires_without_phrase() {
    if !has_schemas() {
        return;
    }
    // 对照组：同一编码在无短语时仍应补上一条码表后续。证明上一个测试里「没有多余候选」
    // 来自补全**让位**，而不是补全整体被改坏了。
    let coord = coord_exact_completion(false);
    for ch in ['a', 'a', 'b'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    assert_eq!(
        texts.len(),
        1,
        "无短语时精确模式空码应补且仅补一条码表后续，实际: {:?}",
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
fn special_mode_exact_completion_shows_longer_code() {
    if !has_schemas() {
        return;
    }
    // 需求回归：特殊模式（引用 wubi86）在精确匹配模式 + 空码补全下，输入 `aab` 无精确候选，
    // 但主库有 `aab?` 更长后继 → 引擎备下 completion_hint（备货不 push）。此前特殊模式只消费
    // result.candidates、丢弃 completion_hint → 屏幕全空；修复后应采纳这条更长编码首选，与主码表
    // 方案一致。single_code_input/single_code_complete 配在全局 schema.codetable、方案未覆盖 →
    // tri-state 回落全局（manager.rs resolved），故特殊模式独立引擎也拿到这两个开关。
    // `aab` 复用 project_phrase_candidate_commit §三 的回归码（六库均无精确 aab、主库有 4 条 aab? 后继）。
    let store_path = std::env::temp_dir().join("wind_special_exact_completion.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    let mut cfg = config_with("wubi86");
    cfg.schema.codetable.single_code_input = true;
    cfg.schema.codetable.single_code_complete = true;
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
    for ch in ['a', 'a', 'b'] {
        press_letter(&coord, ch);
    }
    let texts = coord.debug_all_candidate_texts();
    assert!(
        !texts.is_empty(),
        "精确匹配+空码补全下，特殊模式 aab 无精确候选时应补一条更长编码候选（completion_hint），实际候选: {:?}",
        texts
    );
    let _ = std::fs::remove_file(&store_path);
}

#[test]
fn special_mode_show_all_on_enter_lists_candidates() {
    if !has_schemas() {
        return;
    }
    // 需求：show_all_on_enter 开启时，进入模式（空编码、尚未敲码）即枚举方案码表首页候选；
    // 关闭时（默认）进入模式候选为空、敲码才出。用同一份配置的开/关两态对照。
    let make = |show_all: bool| {
        let store_path =
            std::env::temp_dir().join(format!("wind_special_showall_{}.redb", show_all));
        let _ = std::fs::remove_file(&store_path);
        let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
        let mut cfg = config_with("wubi86");
        cfg.schema.special_modes = vec![wind_config::config::SpecialModeConfig {
            id: "sym".into(),
            trigger_keys: vec!["backslash".into()],
            schema: "wubi86".into(),
            show_all_on_enter: show_all,
            ..Default::default()
        }];
        let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
        // 空缓冲按 \ 进入特殊模式（尚未敲任何编码）
        let act = coord.handle_key_event(&key_event(0xDC, EVENT_KEY_DOWN));
        assert!(
            matches!(act, KeyAction::UpdateComposition { .. }),
            "\\ 应进入特殊模式，实际: {:?}",
            act
        );
        let texts = coord.debug_all_candidate_texts();
        let _ = std::fs::remove_file(&store_path);
        texts
    };
    assert!(
        !make(true).is_empty(),
        "show_all_on_enter 开启时，进入模式（空编码）应立即枚举出码表候选"
    );
    assert!(
        make(false).is_empty(),
        "show_all_on_enter 关闭（默认）时，进入模式（空编码）候选应为空"
    );
}

#[test]
fn special_mode_show_all_respects_single_code_input() {
    if !has_schemas() {
        return;
    }
    // show_all_on_enter 遵循方案 single_code_input：精确匹配模式下进入即展示最多补 1 条
    // （与空码补全「取首位后续码」同语义）；非精确模式枚举整页（多条）。
    let make = |single_code: bool| {
        let store_path =
            std::env::temp_dir().join(format!("wind_special_showall_single_{}.redb", single_code));
        let _ = std::fs::remove_file(&store_path);
        let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
        let mut cfg = config_with("wubi86");
        // 全局基线设 single_code_input；wubi86 方案未覆盖 → tri-state 回落此值。
        cfg.schema.codetable.single_code_input = single_code;
        cfg.schema.special_modes = vec![wind_config::config::SpecialModeConfig {
            id: "sym".into(),
            trigger_keys: vec!["backslash".into()],
            schema: "wubi86".into(),
            show_all_on_enter: true,
            ..Default::default()
        }];
        let coord = Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store);
        coord.handle_key_event(&key_event(0xDC, EVENT_KEY_DOWN));
        let texts = coord.debug_all_candidate_texts();
        let _ = std::fs::remove_file(&store_path);
        texts
    };
    assert_eq!(
        make(true).len(),
        1,
        "精确匹配模式下 show_all_on_enter 应最多补 1 条"
    );
    assert!(
        make(false).len() > 1,
        "非精确模式下 show_all_on_enter 应枚举整页（多条）"
    );
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

/// 中英文切换**不清**配对栈：切走再切回后 Tab 仍能跳出。
/// 切模式既不移动光标也不消除已插入的右符号，「光标紧贴右符号」的前提仍成立。
/// （对照组见 `auto_pair_focus_lost_clears_stack`：失焦才该清。）
#[test]
fn auto_pair_stack_survives_mode_switch() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // 前置：左括号插入配对，栈里确有一层（不断言就无从区分「保住了」与「压根没进栈」）。
    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "前置：左括号应插入配对，实际: {ins:?}"
    );

    // 左 Shift 释放切英文 → 再切回中文。两次都断言模式确实翻转，否则本测试会退化成
    // 「压根没切过模式」的假绿。
    coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    assert!(!coord.is_chinese_mode(), "前置：应已切到英文");
    coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    assert!(coord.is_chinese_mode(), "前置：应已切回中文");

    // 核心断言：配对栈跨模式切换存活 → Tab 仍跳出。
    let jump = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(jump, KeyAction::MoveCursorRight),
        "中英切换不应清配对栈，切回后 Tab 应仍能跳出，实际: {jump:?}"
    );
}

/// 跨模式跳出（本次改造的核心目标）：中文里打的配对，切到英文后 Tab 应能跳出。
///
/// 旧实现下这条必失败——协调器的跳出判定写在中文 composition 路径里，而英文模式在更早处
/// 就 `PassThrough` 了，那段判定是死代码。
#[test]
fn auto_pair_jump_out_works_in_english_mode() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "前置：中文模式下左括号应插入配对，实际: {ins:?}"
    );
    coord.handle_key_event(&key_event(0xA0, EVENT_KEY_UP));
    assert!(!coord.is_chinese_mode(), "前置：应已切到英文");

    let jump = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(jump, KeyAction::MoveCursorRight),
        "英文模式应能跳出中文模式建立的配对，实际: {jump:?}"
    );
}

/// 英文半角普通配对键由协调器接手（此前由 DLL 的 `_englishPairEngine` 本地插入）。
/// 这是「四条建立路径全部入同一个栈」的关键一步。
#[test]
fn english_halfwidth_pair_handled_by_coordinator() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.default.chinese_mode = false; // 英文模式
    cfg.input.auto_pair.english = true;
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    assert!(!coord.is_chinese_mode(), "前置：应处于英文模式");

    // Shift+9 → `(`：协调器出字并补右括号
    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    match ins {
        KeyAction::InsertTextWithCursor {
            ref text,
            cursor_offset,
        } => {
            assert_eq!(text, "()", "英文半角应插入 ASCII 配对");
            assert_eq!(cursor_offset, 1, "光标应落在配对中间");
        }
        other => panic!("英文半角左括号应由协调器插入配对，实际: {other:?}"),
    }

    let jump = coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN));
    assert!(
        matches!(jump, KeyAction::MoveCursorRight),
        "英文模式应能跳出自己建立的配对，实际: {jump:?}"
    );
}

/// 吃键面未扩大（硬性约束的回归保护）：配对开关关闭时，协调器不得接手配对键。
/// 接手即意味着 DLL 也吃了它，而 DLL 的判据是 `IsEnabled() && 在配对表内`——
/// 两侧一旦不同源就是「吃了再吐」丢键。
#[test]
fn english_pair_not_handled_when_disabled() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.default.chinese_mode = false;
    cfg.input.auto_pair.english = false; // 配对关
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let act = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(act, KeyAction::PassThrough),
        "配对关闭时英文括号必须透传（吃键面不得扩大），实际: {act:?}"
    );
}

/// 失焦后配对状态的存废。**跨焦点保留已放弃**（2026-07-29 真机后决定）：
/// 配对状态在 core 全局单栈与每个宿主进程各自一份的 DLL 计数两处，作用域模型对不齐，
/// 加上焦点离开期间用户做了什么输入法无法感知，保留本质上是猜测——实测大部分情况失效。
/// 故凡是会清输入缓冲的 reason，一律连配对状态一起清；`CtxLost` 是 DocMgr 噪声层，
/// 它本来就不清任何输入态，配对状态也跟着不清。
fn pair_state_after_focus_lost(reason: wind_bridge::handler::FocusLostReason) -> KeyAction {
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true;
    cfg.input.auto_pair.jump_out_keys = vec!["tab".into()];
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let ins = coord.handle_key_event(&key_event_mods(0x39, EVENT_KEY_DOWN, 0x0001));
    assert!(
        matches!(ins, KeyAction::InsertTextWithCursor { .. }),
        "前置：左括号应插入配对，实际: {ins:?}"
    );
    coord.handle_focus_lost(0, reason);
    coord.handle_key_event(&key_event(0x09, EVENT_KEY_DOWN))
}

#[test]
fn auto_pair_cleared_on_real_focus_loss() {
    if !has_schemas() {
        return;
    }
    use wind_bridge::handler::FocusLostReason;
    for reason in [
        FocusLostReason::Thread,
        FocusLostReason::DocChanged,
        FocusLostReason::NoEditCtx,
    ] {
        let act = pair_state_after_focus_lost(reason);
        assert!(
            matches!(act, KeyAction::PassThrough),
            "{reason:?} 属真实失焦，配对状态须清空、Tab 应透传，实际: {act:?}"
        );
    }
}

/// `CtxLost` 是 DocMgr 噪声层（Excel 实测同一 DocMgr 6ms 内掉了又回），它**不清任何输入态**，
/// 配对状态也跟着不清——在这里清就是把 Excel 那类抖动变成「配对忽然跳不出去」。
#[test]
fn auto_pair_survives_ctx_lost_noise() {
    if !has_schemas() {
        return;
    }
    let act = pair_state_after_focus_lost(wind_bridge::handler::FocusLostReason::CtxLost);
    assert!(
        matches!(act, KeyAction::MoveCursorRight),
        "CtxLost 是噪声层，不该清配对状态，实际: {act:?}"
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
            other => {
                panic!("第 {round} 次按引号应插入配对（不得跳出/裸出单引号），实际: {other:?}")
            }
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

/// 英文模式（半角）下「英文半角」列生效：DLL 按 core 推送的字符集合吃下这些标点键转发，
/// 此处必须出字。
///
/// 历史：英文非全角时 DLL 直接透传标点键（真机日志 `decision=passthrough_not_handled`），
/// 引擎收不到 → 四列里的「英半」是打不到的死格（英全列有 `english_fullwidth` 分支才生效）。
#[test]
fn english_mode_uses_english_half_width_column() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.default.chinese_mode = false; // 英文输入模式
    cfg.input.punct.custom_enabled = true;
    cfg.input.punct.custom_mappings.insert(
        "\"1".into(),
        vec!["E".into(), "＂".into(), "R".into(), "#".into()],
    );
    cfg.input.punct.custom_mappings.insert(
        "\"2".into(),
        vec!["￥".into(), "＂".into(), "%".into(), "$".into()],
    );
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    assert!(!coord.is_chinese_mode(), "前置：应处于英文模式");

    // Shift+VK_OEM_7 两次 → 英半列的左形 / 右形（`#` → `$`）
    let first = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    let second = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    assert_eq!(
        action_text(&first).as_deref(),
        Some("#"),
        "英文模式首次应出英半列的左形，实际: {first:?}"
    );
    assert_eq!(
        action_text(&second).as_deref(),
        Some("$"),
        "英文模式第二次应出英半列的右形，实际: {second:?}"
    );
}

/// 吃键集 ⊆ 出字集的**反向**保证：没配英半列的标点键在英文模式下仍须透传。
///
/// DLL 只吃 core 推送的字符集合内的键，core 也只接手同一集合——两侧同源。若此处误接手
/// （返回 Consumed 之类）就会吞掉 DLL 根本没吃的键；反之若 DLL 吃了而这里不出字，
/// 就是「吃了再吐」，Chrome/Electron 不回退合成 WM_CHAR，键直接丢失。
#[test]
fn english_mode_uncovered_punct_still_passes_through() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.default.chinese_mode = false;
    cfg.input.punct.custom_enabled = true;
    // 只给双引号配英半列，逗号不配
    cfg.input.punct.custom_mappings.insert(
        "\"1".into(),
        vec!["".into(), "".into(), "".into(), "#".into()],
    );
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let comma = coord.handle_key_event(&key_event(0xBC, EVENT_KEY_DOWN)); // VK_OEM_COMMA
    assert!(
        matches!(comma, KeyAction::PassThrough),
        "未配英半列的标点键在英文模式下必须透传（DLL 也没吃它），实际: {comma:?}"
    );
    // 单引号（同一物理键、无 Shift）也没配 → 同样透传
    let quote = coord.handle_key_event(&key_event(0xDE, EVENT_KEY_DOWN));
    assert!(
        matches!(quote, KeyAction::PassThrough),
        "同键无 Shift 的 `'` 未配英半列，应透传，实际: {quote:?}"
    );
}

/// 自定义映射 × 引号配对：`"1`/`"2` 两行 = **左形/右形**，配对时一次按键两行都用上。
///
/// 语义定名（用户拍板）：界面上的「第一次 / 第二次」实质是左形 / 右形，「第几次」只是没有
/// 自动配对时按次序推导角色的说法。此前配对判定用硬编码的内置 `“”`，而上屏走自定义映射：
/// 把引号自定义成 `「」` 后判定不命中 → 不钉左 → 交替态照旧前进 → 第 2 次按键出 `」`（右符号）
/// → 「出对 / 出单」交替循环复发；反过来若判定命中却钉左，`"2` 那行就永远取不到。
#[test]
fn custom_quote_mapping_pairs_by_left_right_rows() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = true; // 默认中文配对表已含「」
    cfg.input.punct.custom_enabled = true;
    cfg.input
        .punct
        .custom_mappings
        .insert("\"1".into(), vec!["「".into()]);
    cfg.input
        .punct
        .custom_mappings
        .insert("\"2".into(), vec!["」".into()]);
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // 连按两次：每次都插入由「左形 + 右形」组成的完整一对，第二次不退化成裸右符号。
    for round in 1..=2 {
        let act = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
        match act {
            KeyAction::InsertTextWithCursor {
                text,
                cursor_offset,
            } => {
                assert_eq!(
                    text, "「」",
                    "第 {round} 次按引号应插入自定义左右形组成的一对"
                );
                assert_eq!(cursor_offset, 1, "第 {round} 次光标应落在配对中间");
            }
            other => panic!("第 {round} 次按引号应插入自定义配对，实际: {other:?}"),
        }
    }
}

/// 自定义映射 + 引号**不**参与配对时，两行仍按「第一次左 / 第二次右」交替取用。
#[test]
fn custom_quote_mapping_alternates_without_pairing() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.input.auto_pair.chinese = false; // 配对关 → 回到按次序取行
    cfg.input.punct.custom_enabled = true;
    cfg.input
        .punct
        .custom_mappings
        .insert("\"1".into(), vec!["@".into()]);
    cfg.input
        .punct
        .custom_mappings
        .insert("\"2".into(), vec!["￥".into()]);
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let first = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    let second = coord.handle_key_event(&key_event_mods(0xDE, EVENT_KEY_DOWN, 0x0001));
    assert_eq!(action_text(&first).as_deref(), Some("@"), "首次应取 \"1 行");
    assert_eq!(
        action_text(&second).as_deref(),
        Some("￥"),
        "第二次应取 \"2 行"
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

/// 回归：**双拼**分步上屏后退格，回退的必须是原始击键码而非全拼码。
///
/// 引擎只把 `consumed_length` 回映射到双拼击键空间，候选的 `code` 刻意保持全拼语义。
/// 曾因 `committed_segs` 只记全拼码，退格把 `hao` 并回击键缓冲 `ma` → `haoma` 被当双拼
/// 重解析成 `ha|o|ma`，preedit 变 `ha'oma`，此后整串错乱。
///
/// 用 `hcma`（小鹤：hao=hc、ma=ma）而非 `nihc`：**必须选一个双拼码 ≠ 全拼码的首音节**，
/// 否则 bug 隐身——`ni` 两种码恰好相同，正是它让这个缺陷表现为「有时正常」。
/// 末尾的 `nihc` 是对照组，锁住等长场景不被改动波及。
#[test]
fn test_shuangpin_backspace_restores_raw_keys() {
    let d = data_dir();
    if !d.join("schemas/shuangpin.schema.toml").exists() {
        return;
    }
    let sp_cfg = || {
        let mut cfg = Config::default();
        cfg.schema.available = vec!["shuangpin".into()];
        cfg.schema.active = "shuangpin".into();
        cfg.input.default.chinese_mode = true;
        cfg
    };

    // hcma → 「好吗」。选第 6 候选「好」（分步上屏，消费 hc 两键），再退格。
    let coord = Coordinator::new_headless(sp_cfg(), Some(&d));
    type_str(&coord, "hcma");
    let page = coord.debug_page_texts();
    let i = page
        .iter()
        .position(|t| t == "好")
        .expect("首页应有单字候选「好」");
    let act = tap(&coord, 0x31 + i as u32);
    assert_eq!(
        action_text(&act).as_deref(),
        Some("好ma"),
        "分步上屏：「好」入前缀，剩余击键 ma"
    );
    let act = tap(&coord, VK_BACK);
    assert_eq!(
        action_text(&act).as_deref(),
        Some("hc'ma"),
        "退格须还原**击键** hc（全拼 hao 会被重解析成 ha|o → \"ha'oma\"）"
    );

    // 对照：ni 的双拼码与全拼码相同，行为不得改变。
    let coord = Coordinator::new_headless(sp_cfg(), Some(&d));
    type_str(&coord, "nihc");
    let page = coord.debug_page_texts();
    let i = page.iter().position(|t| t == "你").expect("首页应有「你」");
    tap(&coord, 0x31 + i as u32);
    assert_eq!(
        action_text(&tap(&coord, VK_BACK)).as_deref(),
        Some("ni'hc"),
        "等长场景行为不变"
    );
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
                assert_eq!(
                    text, want,
                    "中文全角小键盘(direct) vk=0x{:02X} 应出全角",
                    vk
                )
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
    coord.handle_focus_lost(0, wind_bridge::handler::FocusLostReason::Thread);

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
    coord.handle_focus_lost(0, wind_bridge::handler::FocusLostReason::Thread);
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
    coord.handle_focus_lost(0, wind_bridge::handler::FocusLostReason::Thread);
    assert!(
        temp_words(&store, "wubi86").is_empty(),
        "单字不应成词，实际: {:?}",
        temp_words(&store, "wubi86")
    );
    let _ = std::fs::remove_file(&db);
}

/// 前缀匹配的全局短语（此处以 `$AA` 组 marker 为例）按**来源**统一处理：来源=短语库、全局、
/// 不与方案挂钩，故前缀命中一律避让、不占首位——码表下与更长编码补全按权重同档、拼音/混输下
/// 降到拼音精确候选之下。**不按语法类型区分**（`$CC`/`$SS`/静态同规则），也不再靠 40M 类别硬顶。
///
/// 回归：marker 来自 `lookup_prefix`（前缀枚举、码严格更长＝非完全匹配），曾被标 `is_exact_code=true`
/// + `PHRASE_WEIGHT_BASE`(40M) 抬进精确档并整体上浮，压过普通候选（用户报「系统/用户短语前缀
/// 匹配时优先级偏高、压普通编码/候选」）。现改为 `is_exact_code=false` + `is_prefix=!codetable` +
/// `weight=hit.weight`。低权重（1）确保 marker 可靠沉到码表候选之下，隔离出「避让」这一单一断言。
/// 构造组短语码 `nia`（严格长于输入 `ni` → 前缀枚举命中）。
fn coord_with_group_phrase(schema: &str, tag: &str) -> std::sync::Arc<Coordinator> {
    let store_path = std::env::temp_dir().join(format!("wind_group_marker_{tag}.redb"));
    let _ = std::fs::remove_file(&store_path);
    let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
    store
        .add_phrase("nia", r#"$AA("测试组", "①②③")"#, 0, 1)
        .unwrap();
    let mut cfg = config_with(schema);
    cfg.input.phrase.min_prefix = 2;
    Coordinator::new_headless_with_store(cfg, Some(&data_dir()), store)
}

/// 断言：前缀 marker 仍在候选列表里，但**不占首位**（避让首选普通候选）。
fn assert_group_marker_defers(coord: &Coordinator, mode: &str) {
    let texts = coord.debug_all_candidate_texts();
    let group_pos = texts.iter().position(|t| t == "测试组");
    assert!(
        group_pos.is_some(),
        "[{mode}] 前缀枚举应仍列出组 marker，实际: {:?}",
        texts
    );
    assert_ne!(
        group_pos,
        Some(0),
        "[{mode}] 前缀匹配的组 marker 不应占首位（须避让普通候选），实际: {:?}",
        texts
    );
}

#[test]
fn prefix_group_marker_defers_below_pinyin_candidates() {
    if !has_schemas() {
        return;
    }
    let coord = coord_with_group_phrase("pinyin", "pinyin");
    for ch in ['n', 'i'] {
        press_letter(&coord, ch);
    }
    // 拼音：is_prefix 使 marker 落到拼音精确候选（is_prefix=false）之下。
    assert_group_marker_defers(&coord, "pinyin");
}

#[test]
fn prefix_group_marker_defers_in_codetable_too() {
    if !has_schemas() {
        return;
    }
    // 码表：marker 不再靠 is_exact_code+40M 置顶（旧行为），改按权重——低权重沉到码表候选之下。
    // 与拼音测试同断言，印证「按来源统一避让」而非按引擎模式分档。
    let coord = coord_with_group_phrase("wubi86", "wubi");
    for ch in ['n', 'i'] {
        press_letter(&coord, ch);
    }
    assert_group_marker_defers(&coord, "wubi86");
}

/// 真机回归（`nunl`）：混输下满 4 码，五笔无候选，拼音只有**部分匹配**「嫩」——
/// `nun` 是标准音节表中的稀有音节（为双拼转换真值补入），故 `nunl` 被切成
/// 「完成音节 nun + 残码 l」，候选只消费 3 码。用户诉求：这不算匹配，满码应清空。
///
/// **这是三道门串联的唯一端到端验证**，缺任何一道都不会清空：
/// ① 码表 `clear_on_empty_max`（满码 + 无候选 + 无更长后继）
/// ② 混输 `should_clear`（两道拼音守护，受 `auto_commit_block_on_pinyin` 支配）
/// ③ 协调器 `clear_blocked_by_candidates`（拼音部分匹配不算有效候选）
#[test]
fn test_mixed_full_code_clears_when_only_partial_pinyin() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_mixed();
    cfg.schema.codetable.clear_on_empty_max = true;
    cfg.schema.mix.auto_commit_block_on_pinyin = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    // 前置：打到 nun（3 键）时拼音候选「嫩」确实存在——否则后面测的根本不是本场景
    // （多道闸门串联时，「无候选」会让测试静默退化成从不执行被测分支的假绿）。
    // 必须查**全部**候选而非当前页：nun 的首页被五笔前缀词（习惯/憧憬…）占满，
    // 「嫩」排在第 8 位、落到第二页去了。
    for c in "nun".chars() {
        press_letter(&coord, c);
    }
    let all = coord.debug_all_candidate_texts();
    assert!(
        all.iter().any(|t| t == "嫩"),
        "前置：nun 应出拼音候选「嫩」（41448 大字表的异读注音），实际: {:?}",
        &all[..all.len().min(10)]
    );

    // 第 4 键 l：满码，五笔无候选，拼音只剩部分匹配（嫩/嫰/黁，code 均为 nun、消费 3 码）→ 清空。
    // 注意此刻候选列表**非空**（3 条），旧判据 `state.candidates.is_empty()` 正是在这里拦下清空的。
    match press_letter(&coord, 'l') {
        KeyAction::ClearComposition => {}
        other => panic!(
            "满 4 码仅剩拼音部分匹配时应清空缓冲，实际: {:?}，候选: {:?}",
            other,
            coord.debug_all_candidate_texts()
        ),
    }
}

/// 反向锁，与上一个测试构成**单一变量对照**：同样满 4 码、同样关掉守护开关、同样是
/// 「完整音节 + 单个声母字母」的结构（`wanl` vs `nunl`），唯一差别是拼音候选的类型——
///
/// | 输入 | 候选 | code | consumed | 判定 |
/// |---|---|---|---|---|
/// | `nunl` | 嫩 | `nun`（比输入**短**） | 3 < 4 | 部分匹配 → 清空 |
/// | `wanl` | 完了/晚了 | `wanle`（比输入**长**） | 4 = 4 | 前缀补全 → 拦住 |
///
/// 这一条锁住的正是「拼音还没打完」的中途态保护：前缀补全候选消费整串，天然拦下清空，
/// 用户接着打 `wanle` 不会被吞。**关掉守护开关并不会牺牲这类中途态**——真正被清空的只有
/// 「候选全是部分匹配」的串，也就是确实打岔了的那些。
#[test]
fn test_mixed_full_code_keeps_prefix_completion_candidates() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_mixed();
    cfg.schema.codetable.clear_on_empty_max = true;
    cfg.schema.mix.auto_commit_block_on_pinyin = false;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));

    let mut last = None;
    for c in "wanl".chars() {
        last = Some(press_letter(&coord, c));
    }
    assert!(
        !matches!(last, Some(KeyAction::ClearComposition)),
        "wanl 有前缀补全候选（wanle→完了），不得清空"
    );
    let all = coord.debug_all_candidate_texts();
    assert!(
        all.iter().any(|t| t == "完了" || t == "晚了"),
        "应保留消费整串的前缀补全候选，实际: {:?}",
        &all[..all.len().min(10)]
    );
}

/// 翻页键（默认 `-`/`=`）在临英下应翻页。
/// 回归点：`handle_candidate_nav` 曾按 `ModeKind` 把临英整类排除出可打印导航键
/// （`include_printable` 恒 false），于是 `=` 落到 `_ =>` 标点臂被判成「上屏高亮候选 +
/// 标点」——用户按 `=` 想翻页，实得首候选连同 `=` 被直接上屏并退出临英（`Hel=`）。
/// 与二三候选键 `;`/`'` 是同一条兜底臂的两个出口，但成因不同（那次是漏调选词偏移）。
#[test]
fn test_temp_english_page_keys_flip_pages_when_symbols_disallowed() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    // 显式声明键组与页大小，使本测试不随默认值漂移（默认亦含 minus_equal）。
    cfg.keys.page_keys = vec!["pageupdown".into(), "minus_equal".into()];
    cfg.ui.candidate.per_page = 3;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    // 前置条件：多于一页，否则 page_next 返回 false，翻页分支测不出差异（假绿）。
    let (_, _, total_pages) = coord.debug_page_info();
    assert!(
        total_pages >= 2,
        "前置条件：应有 ≥2 页候选，否则测不到翻页，实际 {total_pages} 页"
    );
    let page0_first = coord.debug_page_texts()[0].clone();

    let act = press_vk(&coord, 0xBB, false); // `=` 下一页
    assert!(
        matches!(act, KeyAction::Consumed),
        "`=` 应作翻页被消费，而非上屏退出临英，实际: {act:?}"
    );
    assert_eq!(coord.debug_page_info().0, 1, "`=` 应翻到第 2 页");
    assert_ne!(
        coord.debug_page_texts()[0],
        page0_first,
        "第 2 页首候选应与第 1 页不同"
    );

    let act = press_vk(&coord, 0xBD, false); // `-` 上一页
    assert!(
        matches!(act, KeyAction::Consumed),
        "`-` 应作翻页被消费，实际: {act:?}"
    );
    assert_eq!(coord.debug_page_info().0, 0, "`-` 应翻回第 1 页");
}

/// 对照组：allow_symbols 开时翻页键让位于字符输入——该开关的声明语义是符号「入缓冲，
/// 而非上屏退出、选词或导航」，与二三候选键 / 数字臂同构，不能被上面的接线改动破坏。
#[test]
fn test_temp_english_page_keys_yield_to_input_when_symbols_allowed() {
    if !has_schemas() {
        return;
    }
    let mut cfg = config_with("wubi86");
    cfg.keys.page_keys = vec!["pageupdown".into(), "minus_equal".into()];
    cfg.ui.candidate.per_page = 3;
    cfg.input.temp_english.allow_symbols = true;
    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    press_shift_letter(&coord, 'h');
    press_letter(&coord, 'e');
    press_letter(&coord, 'l');
    assert!(
        coord.debug_page_info().2 >= 2,
        "前置条件：应有 ≥2 页候选，否则「有得翻却不翻」无从谈起"
    );
    let act = press_vk(&coord, 0xBB, false); // `=`
    assert_eq!(
        action_text(&act).unwrap(),
        "Hel=",
        "allow_symbols 开启时 `=` 应入缓冲而非翻页"
    );
    assert_eq!(coord.debug_page_info().0, 0, "让位输入时不应翻页");
}
