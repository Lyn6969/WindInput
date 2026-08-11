//! 直通命令 `$CC(…, ime.schema("<id>"))` 的端到端分派测试。
//!
//! # 现场
//!
//! 用户配了个直通命令切「花儿五笔」，**每次重启后的第一次必失败**，只弹「花儿五笔准备中…」。
//! 而该方案的词库缓存其实齐备（真机日志里加载只用了 450ms）。
//!
//! 根因：这条入口曾走一条自带 `is_loaded` 守卫的独立切换路径，而启动预热只覆盖
//! `schema.available` 里的方案 —— 对**未启用**方案 `is_loaded` 恒为假，守卫于是永远拦下
//! 切换，「准备中…」也永远不会等到头。更糟的是 `schema.active` 仍被无条件持久化，配置与
//! 运行时就此撕裂：用户看到「按了没反应」，下次热重载/重启却又莫名切了过去。
//!
//! # 为什么必须走命令源
//!
//! 用例走 `debug_run_command`（命令源文本）而不是直接调内部的 `cmd_set_schema`：求值、
//! 动作分派、`Services` 装配三段都在这条链上，任一段断了，直接调内部函数照样通过。
//! 这不是假想——本文件初版把命令源写成裸的 `ime.schema("pinyin")`（缺顶层 `$CC` marker），
//! 它被当成字面文本、一个动作都没跑，而「什么都没发生」与本 bug 的症状**一模一样**。
//! 故每个用例都断言没有「命令执行失败」toast：那是求值/派发出错的唯一出口，
//! 不看它就会把「命令压根没跑」误读成「切换逻辑不对」。

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use wind_config::Config;
use wind_coordinator::Coordinator;
use wind_ui::manager::UiCommand;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn has_schemas() -> bool {
    let d = data_dir();
    d.join("schemas/wubi86.schema.toml").exists() && d.join("schemas/pinyin.schema.toml").exists()
}

/// `schema.available` 里**只有** wubi86：pinyin 于是是「已安装但未启用」——正是现场里
/// 花儿五笔的处境（启动预热不覆盖它，`is_loaded` 恒为假）。
fn cfg_only_wubi() -> Config {
    let mut c = Config::default();
    c.schema.available = vec!["wubi86".into()];
    c.schema.active = "wubi86".into();
    c.input.default.chinese_mode = true;
    c
}

/// 收走通道里的两类可见反馈：状态气泡（`show_tip` 的出口，「准备中…」「加载失败」
/// 都走这里）+ toast。
///
/// 必须一次收完：`try_iter` 会排空通道，分两次收第二次必然是空的——那正是
/// 「断言看不见错误」的经典造法。
fn drain_feedback(rx: &Receiver<UiCommand>) -> (Vec<String>, Vec<String>) {
    let mut tips = Vec::new();
    let mut toasts = Vec::new();
    for c in rx.try_iter() {
        match c {
            UiCommand::ShowStatusTip { text, .. } => tips.push(text),
            UiCommand::ShowToast { text, .. } => toasts.push(text),
            _ => {}
        }
    }
    (tips, toasts)
}

/// 命令确实被求值并派发了——而不是因语法/装配问题静默跳过。
fn assert_command_ran(toasts: &[String]) {
    assert!(
        !toasts.iter().any(|t| t.contains("命令执行失败")),
        "命令未能求值/派发，本用例后续断言全部失去意义。toast: {toasts:?}"
    );
}

/// 直通命令切到一个**未启用**的方案：必须真的切过去，且不再弹「准备中…」。
#[test]
fn direct_command_switches_to_unavailable_schema() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let (coord, rx) = Coordinator::new_headless_with_ui(cfg_only_wubi(), Some(&data_dir()));
    assert_eq!(coord.active_schema_id(), "wubi86");
    // 前提断言：pinyin 确实未启用。没有这一条，本用例可能在「已启用方案」上空跑一遍
    // 而永远绿——那正好避开了要复现的那个处境。
    assert!(
        !coord
            .debug_available_schemas()
            .iter()
            .any(|s| s == "pinyin"),
        "pinyin 必须不在 available 里，本用例才复现得了「未预热方案」；实际: {:?}",
        coord.debug_available_schemas()
    );
    let _ = rx.try_iter().count(); // 清掉构造期的 UI 指令

    coord.debug_run_command(r#"$CC("拼音", ime.schema("pinyin"))"#);

    let (tips, toasts) = drain_feedback(&rx);
    assert_command_ran(&toasts);
    assert_eq!(
        coord.active_schema_id(),
        "pinyin",
        "未启用方案也应懒加载并切过去——engine_mgr.switch_schema 内部本就是 ensure_loaded，\
         协调器层那道 is_loaded 守卫只是抢在懒加载之前否决掉"
    );
    assert!(
        !tips.iter().any(|t| t.contains("准备中")),
        "不应再弹「准备中…」：方案词库齐备，那条提示是守卫拦下切换时的误导。实际: {tips:?}"
    );
}

/// 切到**已是当前**的方案：幂等，不弹任何提示。
///
/// 与直达热键同一条路后，重复执行同一条直通命令不该有副作用——旧实现在这里会无条件
/// 重写一次 `schema.active`。
#[test]
fn direct_command_to_active_schema_is_noop() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let (coord, rx) = Coordinator::new_headless_with_ui(cfg_only_wubi(), Some(&data_dir()));
    let _ = rx.try_iter().count();

    coord.debug_run_command(r#"$CC("五笔", ime.schema("wubi86"))"#);

    let (tips, toasts) = drain_feedback(&rx);
    assert_command_ran(&toasts);
    assert_eq!(coord.active_schema_id(), "wubi86");
    assert!(
        tips.is_empty(),
        "切到已是当前的方案应当完全静默，实际弹了: {tips:?}"
    );
}

/// 切到一个**不存在**的方案：active 不动，且给出可见的失败反馈。
///
/// 删掉 `is_loaded` 守卫后，「加载不出来」这一支必须仍然说得出话——否则方案文件损坏时
/// 就退化成按键毫无反应的哑失败。
#[test]
fn direct_command_to_missing_schema_keeps_active() {
    if !has_schemas() {
        eprintln!("跳过：缺少 build_dev/data 方案");
        return;
    }
    let (coord, rx) = Coordinator::new_headless_with_ui(cfg_only_wubi(), Some(&data_dir()));
    let _ = rx.try_iter().count();

    coord.debug_run_command(r#"$CC("不存在", ime.schema("no_such_schema_xyz"))"#);

    let (tips, toasts) = drain_feedback(&rx);
    assert_command_ran(&toasts);
    assert_eq!(
        coord.active_schema_id(),
        "wubi86",
        "目标方案加载不出来时必须留在原方案"
    );
    assert!(
        tips.iter().any(|t| t.contains("加载失败")),
        "加载不出来应弹「加载失败」而不是沉默。实际: {tips:?}"
    );
}
