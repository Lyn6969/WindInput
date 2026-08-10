//! 会话态按键功能表（`keys.session_actions`）的端到端测试。
//!
//! 覆盖两个用户诉求：**Tab 向下翻页**、**CapsLock 向上翻页**。
//! 设计见 `docs/design/session-key-actions.md`。
//!
//! ⚠️ 依赖词库的用例以 `if !has_schemas() { return; }` 开头——缺 `build_dev/data` 时会
//! **静默跳过且计数照绿**。判据是耗时：假绿 0.0x s，真跑约 1s+。缺数据先跑
//! `scripts/dev.ps1 gd`。

use std::path::PathBuf;
use wind_bridge::handler::{KeyEventData, MessageHandler};
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ipc::protocol::{EVENT_KEY_DOWN, EVENT_KEY_UP, MOD_CAPSLOCK, MOD_SHIFT};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_schemas() -> bool {
    data_dir().join("schemas/wubi86.schema.toml").exists()
}

const VK_TAB: u32 = 0x09;
const VK_CAPITAL: u32 = 0x14;
const VK_A: u32 = 0x41;

fn key(vk: u32, modifiers: u32, event_type: u8) -> KeyEventData {
    KeyEventData {
        key_code: vk,
        scan_code: 0,
        modifiers,
        event_type,
        toggles: 0,
        event_seq: 0,
        prev_char: 0,
    }
}

/// 配置：wubi86 + 指定的会话态绑定。
///
/// ⚠️ 显式写进 `session_actions` 的键**压过**默认 `page_keys`/`highlight_keys` 的折算
/// （见 `Config::migrate_nav_keys_into_session_actions`），所以这里写 `tab = page_next`
/// 就能覆盖出厂的 `tab = highlight_down`，不必先清空 `highlight_keys`。
fn cfg_with(binds: &[(&str, &str)]) -> Config {
    let mut c = Config::default();
    c.schema.available = vec!["wubi86".into()];
    c.schema.active = "wubi86".into();
    c.input.default.chinese_mode = true;
    for (k, v) in binds {
        c.keys.session_actions.insert(k.to_string(), v.to_string());
    }
    c
}

/// 打出一串码并确认候选**多于一页**——否则翻页测试无从证伪（翻页在只有一页时是 no-op，
/// 断言「页码没变」会恒真）。
fn type_until_multipage(coord: &Coordinator) {
    coord.handle_key_event(&key(VK_A, 0, EVENT_KEY_DOWN));
    let (_, _, total) = coord.debug_page_info();
    assert!(
        total > 1,
        "样例码 'a' 应产出多页候选（实际 {total} 页）；否则翻页断言无法证伪，请换一个码"
    );
}

/// ★★ 跨 crate 键名表一致性。
///
/// `wind-config` 不能依赖 `wind-keys`（后者经 `wind-cmdbar` 反向依赖前者，加进去成环），
/// 于是会话态键名解析**存在两份实现**：`hotkey::session_key_to_vk`（决定键进不进 TSF
/// 转发白名单）与 `keymap::session_key_name_to_vk`（决定按下时查不查得到绑定）。
///
/// 两份漂移的后果是**静默半失效**：白名单里有、查表查不到 ⇒ 键被转发过来却什么也不做；
/// 反过来则是绑定配了永不触发。两种都没有任何报错。本仓已在跨仓 schema 契约上栽过同型的坑。
///
/// 本测试是这条契约**唯一**的编译期外守门——它所在的 crate 是唯一同时看得见两份实现的地方。
#[test]
fn session_key_tables_agree_across_crates() {
    let names = [
        "tab",
        "shift+tab",
        "capslock",
        "caps",
        "pageup",
        "pgup",
        "prior",
        "pagedown",
        "pgdn",
        "next",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "minus",
        "equal",
        "lbracket",
        "rbracket",
        "comma",
        "period",
        "semicolon",
        "quote",
        "slash",
        "backtick",
        "backslash",
    ];
    for name in names {
        let a = wind_config::hotkey::session_key_to_vk(name);
        let b = wind_keys::keymap::session_key_name_to_vk(name);
        match (a, b) {
            (Some((vk, shift)), Some(k)) => {
                assert_eq!(vk, k.vk, "键名 {name} 的 VK 两份解析不一致");
                assert_eq!(shift, k.shift, "键名 {name} 的 shift 两份解析不一致");
            }
            (a, b) => panic!("键名 {name} 只被其中一份认得：hotkey={a:?} keymap={b:?}"),
        }
    }
    // 反向：两份都必须拒绝同样的东西。ctrl/alt 组合归 `keys.key_actions`，不进本表。
    for bad in ["ctrl+tab", "alt+tab", "ctrl+shift+tab", "pgeup", ""] {
        assert!(
            wind_config::hotkey::session_key_to_vk(bad).is_none(),
            "hotkey 侧不该认得 {bad:?}"
        );
        assert!(
            wind_keys::keymap::session_key_name_to_vk(bad).is_none(),
            "keymap 侧不该认得 {bad:?}"
        );
    }
}

/// 诉求一：`tab = "page_next"` 后，有候选时按 Tab 翻到下一页。
#[test]
fn tab_pages_next_when_candidates_shown() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(cfg_with(&[("tab", "page_next")]), Some(&data_dir()));
    type_until_multipage(&coord);
    let (page0, _, _) = coord.debug_page_info();

    coord.handle_key_event(&key(VK_TAB, 0, EVENT_KEY_DOWN));
    let (page1, _, _) = coord.debug_page_info();
    assert_eq!(page1, page0 + 1, "Tab 应翻到下一页");
}

/// 对照组：不配 `tab = page_next` 时，Tab 走出厂默认（高亮下移），**页码不变**。
///
/// 没有这一条，上面那个用例在「Tab 本来就翻页」时也会绿——而出厂默认恰恰是高亮下移，
/// 两者的区别只在页码有没有动。
#[test]
fn tab_defaults_to_highlight_not_paging() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(cfg_with(&[]), Some(&data_dir()));
    type_until_multipage(&coord);
    let (page0, sel0, _) = coord.debug_page_info();

    coord.handle_key_event(&key(VK_TAB, 0, EVENT_KEY_DOWN));
    let (page1, sel1, _) = coord.debug_page_info();
    assert_eq!(page1, page0, "默认配置下 Tab 不该翻页");
    assert_ne!(sel1, sel0, "默认配置下 Tab 应移动高亮");
}

/// 诉求二：`capslock = "page_prev"` 后，有候选时按 CapsLock 翻到上一页。
///
/// ★ 走的是 **keyup**：C++ 对 CapsLock 的 keydown 压根不转发给服务端，绑定挂在 keydown
/// 链上是配得上、永不触发。
#[test]
fn capslock_pages_prev_on_key_up() {
    if !has_schemas() {
        return;
    }
    let coord =
        Coordinator::new_headless(cfg_with(&[("capslock", "page_prev")]), Some(&data_dir()));
    type_until_multipage(&coord);
    // 先翻到第 2 页，否则「上一页」在首页是 no-op，断言无法证伪。
    coord.handle_key_event(&key(0x22 /* PageDown */, 0, EVENT_KEY_DOWN));
    let (page0, _, _) = coord.debug_page_info();
    assert_eq!(page0, 1, "前置条件：应已在第 2 页");

    coord.handle_key_event(&key(VK_CAPITAL, MOD_CAPSLOCK, EVENT_KEY_UP));
    let (page1, _, _) = coord.debug_page_info();
    assert_eq!(page1, 0, "CapsLock 的 keyup 应翻到上一页");
}

/// ★★ CapsLock 翻页**不得**吞掉正在打的编码。
///
/// 回归保护：keyup 分支里 CapsLock 的原有处理会调 `take_input_on_mode_switch`，把待输入
/// 上屏或丢弃。会话态绑定若排在它**之后**，用户每翻一页就毁一次输入，现象是「翻页时
/// 编码莫名没了」——极难联想到是大小写同步干的。
#[test]
fn capslock_paging_preserves_composition() {
    if !has_schemas() {
        return;
    }
    let coord =
        Coordinator::new_headless(cfg_with(&[("capslock", "page_prev")]), Some(&data_dir()));
    type_until_multipage(&coord);
    let before = coord.debug_page_texts();
    assert!(!before.is_empty(), "前置条件：应有候选");

    coord.handle_key_event(&key(VK_CAPITAL, MOD_CAPSLOCK, EVENT_KEY_UP));
    assert!(
        !coord.debug_page_texts().is_empty(),
        "翻页后候选不应消失——说明编码被 CapsLock 的状态同步分支吞了"
    );
}

/// ★ 无候选时 CapsLock 回落原语义（大小写状态同步），不被会话态绑定截走。
///
/// 「有会话归绑定、无会话归原语义」正是两张表的分野。判据落在这里而不是 C++ 侧：
/// 服务端拿到 keyup 后自己决定归谁，C++ 只负责转发。
#[test]
fn capslock_without_candidates_falls_back_to_state_sync() {
    if !has_schemas() {
        return;
    }
    let coord =
        Coordinator::new_headless(cfg_with(&[("capslock", "page_prev")]), Some(&data_dir()));
    // 不打任何码，直接按 CapsLock。
    // ⚠️ 判「无候选」要用 `debug_page_texts()` 而不是 `debug_page_info().2`——后者是
    // `total_pages`，空候选时返回 1（页数下限）而非 0。
    assert!(coord.debug_page_texts().is_empty(), "前置条件：不应有候选");

    let act = coord.handle_key_event(&key(VK_CAPITAL, MOD_CAPSLOCK, EVENT_KEY_UP));
    // 原语义会回一个状态更新；被会话态绑定截走则会是 Consumed。
    assert!(
        !matches!(act, wind_bridge::handler::KeyAction::Consumed),
        "无候选时 CapsLock 应落回状态同步，实际被绑定截走: {act:?}"
    );
}

/// ★★ 可达性：CapsLock 的绑定必须进 keyup 转发白名单。
///
/// 这是 keyup 类绑定**唯一**的可达性来源——不在白名单里，TSF 根本不发这个 keyup，
/// 绑定在配置里躺着但永远不触发。`key_actions` 四期正是漏了这一环（设计文档 §4.4 补充段）。
///
/// 断言取的是 `push_activation_status` 真正推给 C++ 的那份，不是旁路重算——用重算值
/// 断言等于没测。
#[test]
fn capslock_binding_enters_key_up_forward_set() {
    let coord = Coordinator::new_headless(cfg_with(&[("capslock", "page_prev")]), None);
    let want = (MOD_CAPSLOCK << 16) | VK_CAPITAL;
    let hashes = coord.debug_key_up_hotkeys();
    assert!(
        hashes.iter().any(|h| h & 0x0000_FFFF == VK_CAPITAL),
        "CapsLock 未进 keyup 转发集，绑定永不触发。实际: {hashes:02X?}"
    );
    // policy 位会叠加在高位，故比对时掩掉；raw 部分必须精确等于 (MOD_CAPSLOCK, VK_CAPITAL)。
    assert!(
        hashes.iter().any(|h| h & 0x01FF_FFFF == want),
        "CapsLock 的 keyup hash 应含 MOD_CAPSLOCK，否则 C++ 侧算出的 hash 对不上。实际: {hashes:02X?}"
    );
}

/// Shift+Tab 与 Tab 是两条独立绑定（`shift+` 前缀），不会互相顶掉。
#[test]
fn shift_tab_and_tab_bind_independently() {
    if !has_schemas() {
        return;
    }
    let coord = Coordinator::new_headless(
        cfg_with(&[("tab", "page_next"), ("shift+tab", "page_prev")]),
        Some(&data_dir()),
    );
    type_until_multipage(&coord);

    coord.handle_key_event(&key(VK_TAB, 0, EVENT_KEY_DOWN));
    assert_eq!(coord.debug_page_info().0, 1, "Tab 应翻到第 2 页");
    coord.handle_key_event(&key(VK_TAB, MOD_SHIFT, EVENT_KEY_DOWN));
    assert_eq!(coord.debug_page_info().0, 0, "Shift+Tab 应翻回第 1 页");
}
