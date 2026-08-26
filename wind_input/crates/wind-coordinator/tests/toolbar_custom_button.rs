//! 工具栏自定义按钮的端到端分派：点击 → 执行 `ui.toolbar.buttons[i].action`。
//!
//! # 为什么必须走 `inject_ui_event` 而不是直接调内部函数
//!
//! 这条链有四段——UI 事件分发（`handle_ui_event` 的 `UiEvent::Toolbar` 臂）、
//! `mouse_toolbar` 的 `Custom` 分派、按下标取配置、cmdbar 求值与动作执行——
//! 任一段断了，直接调内部函数照样通过。本仓已在直通命令那次栽过同一个坑
//! （见 `schema_direct_command.rs` 的模块注释：命令源少写一个 `$CC` marker，
//! 一个动作都没跑，而「什么都没发生」与真 bug 的症状一模一样）。
//!
//! 尤其要挡住的是 `mouse_toolbar` 里那个 `unreachable!()`：新变体若漏了第一个
//! match 的分支就会落到它上面 —— 那是 panic，不是静默，但只有真走一遍才看得见。

use std::path::PathBuf;
use std::time::{Duration, Instant};
use wind_config::{Config, ToolbarButtonSpec};
use wind_coordinator::Coordinator;
use wind_ui_types::{ToolbarAction, UiEvent};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_schemas() -> bool {
    let d = data_dir();
    d.join("schemas/wubi86.schema.toml").exists() && d.join("schemas/pinyin.schema.toml").exists()
}

fn button(id: &str, action: &str) -> ToolbarButtonSpec {
    ToolbarButtonSpec {
        id: id.to_string(),
        label: "符".to_string(),
        action: action.to_string(),
        enabled: true,
    }
}

/// 等待异步动作生效：`run_toolbar_button` 经 `spawn_command` 起独立线程
/// （`run_command_candidate` 要求未持 state 锁），故结果不是同步可见的。
///
/// 轮询而非固定 sleep：固定值要么慢、要么在忙机器上偶发失败。
fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// 点击自定义按钮 → 动作真的执行了。
///
/// 用 `ime.schema` 作探针是因为它的效果**可观测且可断言**（活跃方案变了），
/// 而 `proc.run` 这类真去启动程序的动作在测试里既不该跑也没法验。
#[test]
fn clicking_custom_button_runs_its_action() {
    if !has_schemas() {
        eprintln!("跳过：缺少方案数据");
        return;
    }
    let mut cfg = Config::default();
    cfg.schema.active = "wubi86".to_string();
    cfg.schema.available = vec!["wubi86".to_string(), "pinyin".to_string()];
    cfg.ui.toolbar.items = vec!["mode".to_string(), "custom:sw".to_string()];
    cfg.ui.toolbar.buttons = vec![button("sw", r#"$CC("切拼音", ime.schema("pinyin"))"#)];

    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86", "前置：起于五笔");

    coord.inject_ui_event(UiEvent::Toolbar(ToolbarAction::Custom(0)));

    assert!(
        wait_until(|| coord.active_schema_id() == "pinyin"),
        "点击自定义按钮后方案应切到 pinyin，实际仍是 {}",
        coord.active_schema_id()
    );
}

/// 下标越界不 panic、也不误伤别的按钮。
///
/// 可达性：UI 侧的项列表与本侧配置之间有一瞬可能错开（配置刚重载、新的
/// `SetToolbarLayout` 还没到 UI），此时旧的下标会打进来。
#[test]
fn out_of_range_index_is_ignored() {
    if !has_schemas() {
        eprintln!("跳过：缺少方案数据");
        return;
    }
    let mut cfg = Config::default();
    cfg.schema.active = "wubi86".to_string();
    cfg.schema.available = vec!["wubi86".to_string(), "pinyin".to_string()];
    cfg.ui.toolbar.buttons = vec![button("sw", r#"$CC("切拼音", ime.schema("pinyin"))"#)];

    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    // 只有下标 0 存在，打 7 进去。
    coord.inject_ui_event(UiEvent::Toolbar(ToolbarAction::Custom(7)));

    // 给足与上一条相同的时间窗，确认「什么都没发生」而不是「还没发生」。
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        coord.active_schema_id(),
        "wubi86",
        "越界下标不得执行任何按钮的动作"
    );
}

/// 按钮配了却没写 action：不执行、不 panic。
#[test]
fn empty_action_is_a_no_op() {
    if !has_schemas() {
        eprintln!("跳过：缺少方案数据");
        return;
    }
    let mut cfg = Config::default();
    cfg.schema.active = "wubi86".to_string();
    cfg.schema.available = vec!["wubi86".to_string(), "pinyin".to_string()];
    cfg.ui.toolbar.buttons = vec![button("empty", "   ")];

    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    coord.inject_ui_event(UiEvent::Toolbar(ToolbarAction::Custom(0)));

    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(coord.active_schema_id(), "wubi86");
}

/// ★ 回归：**裸表达式**（不带顶层 `$CC(...)` 标记）也必须能执行。
///
/// 这是 2026-08-26 用户真机报的 bug：按钮显示正常、日志无任何告警，点了却什么都不发生。
/// 根因是 `run_command_candidate` 走 `evaluate_phrase`，那是**短语格式**——缺 `$CC` 标记
/// 的源被当成字面文本，一个动作都不跑，且不报错。
///
/// 而 `data/config.toml` 与文档站教用户写的正是裸形式（`action = 'proc.run("…")'`），
/// 端到端测试用的却是带标记的形式 ⇒ 测试全绿、文档教错。
///
/// 判据：**按钮的 action 本来就只可能是命令**，没有「这是文本还是命令」的歧义，
/// 不该要求用户懂短语系统的标记语法。
#[test]
fn bare_expression_action_runs_without_cc_marker() {
    if !has_schemas() {
        eprintln!("跳过：缺少方案数据");
        return;
    }
    let mut cfg = Config::default();
    cfg.schema.active = "wubi86".to_string();
    cfg.schema.available = vec!["wubi86".to_string(), "pinyin".to_string()];
    // 注意这里**没有** $CC 包裹——与文档教的写法一致。
    cfg.ui.toolbar.buttons = vec![button("sw", r#"ime.schema("pinyin")"#)];

    let coord = Coordinator::new_headless(cfg, Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86", "前置：起于五笔");

    coord.inject_ui_event(UiEvent::Toolbar(ToolbarAction::Custom(0)));

    assert!(
        wait_until(|| coord.active_schema_id() == "pinyin"),
        "裸表达式必须照样执行；实际方案仍是 {}",
        coord.active_schema_id()
    );
}
