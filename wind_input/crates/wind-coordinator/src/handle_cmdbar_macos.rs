//! 命令栏（cmdbar）宿主集成的 macOS 平台差异部分
//!
//! 从 [`handle_cmdbar`](crate::handle_cmdbar) 抽出，集中放置 darwin 专属实现，使主文件保持
//! 平台无关的流程清晰。全模块 `#[cfg(target_os = "macos")]`，仅在 macOS 编译。
//!
//! 核心差异：服务进程（LaunchAgent）**无 GUI 事件上下文 / 辅助功能授权**，故：
//! - `open` / `proc.run`：进程内直接经 `open` CLI / `Command::spawn`（app 侧无 shell_exec 下行分支）；
//! - `clip.paste`：不合成 ⌘V，改经 IMKit `insertText`（commit 通道）把剪贴板文本上屏（免授权、纯文本）；
//! - `key.tap/seq/hold/release/type`：推 IPC 帧给 `.app` 侧 `KeySynthesizer` 合成 CGEvent（`.app` 有授权）。

use crate::coordinator::Coordinator;
use std::process::Command;
use std::sync::{Arc, Weak};

/// 服务进程内打开 URL / 文件 / .app。经 `open` CLI（Windows ShellExecute open 语义的 macOS
/// 等价），能正确拉起并激活浏览器 / 目标 app。与 `proc.shell`（`sh -c`）、剪贴板（pbcopy/pbpaste）
/// 一致走进程内子进程——app 侧无 shell_exec 下行分支（0x020E 已被上行 candidateHover 占用），
/// 走 push_shell_exec 会被丢弃，故 macOS 不经 IPC 直接执行。
pub(crate) fn open_native(target: &str) -> anyhow::Result<()> {
    Command::new("open").arg(target).spawn()?;
    Ok(())
}

/// 服务进程内启动外部程序（带参数），直接 spawn。若需以 .app 名启动并激活，用户可改用
/// `open("...")` 或 `proc.shell("open -a ...")`。
pub(crate) fn run_native(cmd: &str, args: &[String], cwd: &str) -> anyhow::Result<()> {
    let mut c = Command::new(cmd);
    c.args(args);
    // 空串 = 继承服务进程的当前目录。服务由 launchd 拉起时那通常是 `/`，
    // 同样是不确定的，故调用方应先经 resolve_workdir 定好目录。
    if !cwd.is_empty() {
        c.current_dir(cwd);
    }
    c.spawn()?;
    Ok(())
}

/// `clip.paste` 的 macOS 实现：不合成 ⌘V，输入法直接经 IMKit `insertText`（commit 上屏通道）
/// 把剪贴板文本落到当前输入框——输入法插入文本的正道 API：免辅助功能授权、无焦点/时序竞争、
/// 更可靠。代价：仅纯文本（⌘V 才能粘富文本/图片并触发目标 app 原生粘贴），但对输入法的
/// 「粘贴」命令纯文本即所需。等价于 `type(clip())`。
pub(crate) fn paste_via_ime(weak: &Weak<Coordinator>) {
    let text = wind_ui::popup_menu::get_clipboard_text();
    if text.is_empty() {
        return;
    }
    if let Some(c) = weak.upgrade() {
        c.push_commit_text(&text);
    }
}

/// 构造 macOS 的按键注入服务（[`CoordKeys`]）。
pub(crate) fn make_keys(weak: Weak<Coordinator>) -> Arc<dyn wind_cmdbar::KeyInjector> {
    Arc::new(CoordKeys(weak))
}

/// 命令直通车按键合成经 IPC 推给 `.app`（服务进程无辅助功能授权无法 post CGEvent）。
/// 把 combo 串（"Ctrl+C" / "Cmd+v" / "Enter"）拆成 `.app` KeySynthesizer 期望的 (key, mods)。
struct CoordKeys(Weak<Coordinator>);

impl CoordKeys {
    fn push(&self, encoded: Vec<u8>) {
        if let Some(c) = self.0.upgrade() {
            c.push_cmdbar_key_frame(&encoded);
        }
    }
}

impl wind_cmdbar::KeyInjector for CoordKeys {
    fn tap(&self, combo: &str) -> anyhow::Result<()> {
        let (key, mods) = split_combo(combo);
        self.push(wind_ipc::codec::encode_key_tap(&key, &mods));
        Ok(())
    }
    fn sequence(&self, combos: &[String]) -> anyhow::Result<()> {
        let list: Vec<(String, Vec<String>)> = combos.iter().map(|c| split_combo(c)).collect();
        self.push(wind_ipc::codec::encode_key_seq(&list));
        Ok(())
    }
    fn hold(&self, combo: &str) -> anyhow::Result<()> {
        let (key, mods) = split_combo(combo);
        self.push(wind_ipc::codec::encode_key_hold(&key, &mods));
        Ok(())
    }
    fn release(&self, combo: &str) -> anyhow::Result<()> {
        let (key, mods) = split_combo(combo);
        self.push(wind_ipc::codec::encode_key_release(&key, &mods));
        Ok(())
    }
    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        self.push(wind_ipc::codec::encode_key_type(text));
        Ok(())
    }
}

/// 拆 combo 串为 `.app` KeySynthesizer 规范 (key, mods)：key 为小写 canonical 名，
/// mods ⊆ {"ctrl","shift","alt","win"}（cmd/command→win、control→ctrl、menu/option→alt）。
fn split_combo(combo: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = combo
        .split('+')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let Some((key, mods)) = parts.split_last() else {
        return (String::new(), Vec::new());
    };
    let mods = mods
        .iter()
        .map(|m| match m.to_lowercase().as_str() {
            "control" | "ctrl" => "ctrl".to_string(),
            "shift" => "shift".to_string(),
            "menu" | "alt" | "option" => "alt".to_string(),
            "win" | "cmd" | "command" | "super" | "meta" => "win".to_string(),
            other => other.to_string(),
        })
        .collect();
    (key.to_lowercase(), mods)
}
