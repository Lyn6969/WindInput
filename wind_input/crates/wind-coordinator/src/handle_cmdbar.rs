//! 命令栏（cmdbar）宿主集成
//!
//! 对照 Go `wind_input/internal/coordinator/cmdbar_context.go` + `cmdbar_services.go`。
//! 负责三件事：
//! 1. [`Coordinator::init_cmdbar`]：构造后装配 [`Services`] 与自身 Weak 引用；
//! 2. [`CmdbarCtx`]：把 coordinator 运行时状态适配为 [`EvalContext`]；
//! 3. 控制器（[`CoordIme`] / [`CoordDict`]）：把 cmdbar 动作映射到 coordinator 能力。
//!
//! **平台缺口**：key/clip/proc/url/search/config/setting 等服务在 Rust 平台层尚缺，
//! 对应字段留 `None`，相关动作调用返回 ServiceUnavailable（宿主侧记 WARN 降级）；
//! 现已接通 ime.toggle(cn-en/fullshape/s2t)、ime.schema、dict.add。
//!
//! **线程/锁**：动作经独立线程执行（见 `Coordinator::spawn_command`），故控制器回调
//! 自锁的 coordinator 方法是安全的（此刻按键处理已释放 state 锁）。

use crate::coordinator::Coordinator;
use chrono::{DateTime, Local};
use std::process::Command;
use std::sync::{Arc, Weak};
use tracing::warn;
use wind_cmdbar::{
    ClipboardService, DictService, EvalContext, ImeController, ProcessRunner, Services, UrlOpener,
};

impl Coordinator {
    /// 构造后装配 cmdbar：自身 Weak 引用 + Services。一次性，幂等。
    pub(crate) fn init_cmdbar(self: &Arc<Self>) {
        let _ = self.self_weak.set(Arc::downgrade(self));
        let weak = Arc::downgrade(self);
        let mut svc = Services::new();
        svc.ime = Some(Arc::new(CoordIme(weak.clone())));
        svc.dict = Some(Arc::new(CoordDict(weak)));
        // 无需 coordinator 回调的能力：进程启动、打开 URL/文件、写剪贴板（纯平台/std）。
        svc.proc = Some(Arc::new(StdProc));
        svc.open = Some(Arc::new(ShellOpener));
        svc.clip = Some(Arc::new(SysClip));
        svc.keys = Some(Arc::new(wind_keys::key_inject::SysKeys));
        // search/config/setting：经 open 默认可用 / 配置能力待补，留 None。
        let _ = self.cmdbar_services.set(svc);
    }

    /// 执行一个 `$CC` 命令源：解析 → 求值 → **按列表顺序**跑动作链。
    /// type() 文本经 push 管道上屏；其余为副作用。文本上屏后稍候再跑后续副作用，
    /// 让落字先于后续按键（如 `type("「」")` 后 `key.tap("Left")` 才能把光标落到括号中间）。
    /// **必须在独立线程、未持 state 锁时调用**（控制器会回调自锁的 coordinator 方法）。
    pub(crate) fn run_command_candidate(&self, src: &str, input: &str) {
        let Some(services) = self.cmdbar_services.get() else {
            return;
        };
        let ctx = CmdbarCtx {
            input: input.to_string(),
            now: Local::now(),
            last: self.recent_commits_snapshot(),
            services,
        };
        let reg = wind_cmdbar::default_registry();
        let actions = match wind_cmdbar::evaluate_phrase(src, &ctx, reg) {
            Ok(wind_cmdbar::PhraseEval::Single { actions, .. }) => actions,
            // $SS 数组的动作在各元素自身选中时执行，整组选中不跑动作。
            Ok(wind_cmdbar::PhraseEval::Array(_)) => return,
            Err(e) => {
                warn!("cmdbar 命令求值失败 ({:?}): {}", src, e);
                return;
            }
        };
        let mut text_pending = false;
        let mut first_text = true;
        for a in &actions {
            match a.kind {
                wind_cmdbar::ActionKind::Text => match a.run(&ctx, reg) {
                    Ok(t) if !t.is_empty() => {
                        // 首次上屏前稍候：让选词返回的 ClearComposition 先到达客户端，
                        // 避免命令线程的 push 文本与清 composition 竞争（顺序错乱）。
                        if first_text {
                            std::thread::sleep(std::time::Duration::from_millis(30));
                            first_text = false;
                        }
                        self.push_commit_text(&t);
                        text_pending = true;
                    }
                    Ok(_) => {}
                    Err(e) => warn!("cmdbar type 动作失败: {}", e),
                },
                wind_cmdbar::ActionKind::Effect => {
                    if text_pending {
                        std::thread::sleep(std::time::Duration::from_millis(30));
                        text_pending = false;
                    }
                    if let Err(e) = a.run(&ctx, reg) {
                        warn!("cmdbar 动作失败: {}", e);
                    }
                }
            }
        }
    }
}

/// 命令栏求值上下文（coordinator 适配）。提供 input/now/env + 上屏历史 last + 剪贴板 clip + services；
/// sel/app/title 待前台窗口能力补齐后接入（与 Go 早期实现一致先留空）。
struct CmdbarCtx<'a> {
    input: String,
    now: DateTime<Local>,
    /// 上屏历史快照（index 0 = 最近一次），触发命令时冻结。
    last: Vec<String>,
    services: &'a Services,
}

impl EvalContext for CmdbarCtx<'_> {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, n: i64) -> String {
        if n < 1 {
            return String::new();
        }
        self.last.get((n - 1) as usize).cloned().unwrap_or_default()
    }
    fn clip(&self, _n: i64) -> String {
        // 仅当前剪贴板（n>1 历史栈未实现）。
        #[cfg(windows)]
        {
            wind_ui::popup_menu::get_clipboard_text()
        }
        #[cfg(not(windows))]
        {
            String::new()
        }
    }
    fn sel(&self) -> String {
        String::new()
    }
    fn app(&self) -> String {
        String::new()
    }
    fn title(&self) -> String {
        String::new()
    }
    fn env(&self, name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.now
    }
    fn services(&self) -> Option<&Services> {
        Some(self.services)
    }
}

/// IME 控制器：ime.toggle / ime.schema 接通；setting.* / theme_cycle 待平台能力补齐。
struct CoordIme(Weak<Coordinator>);

impl ImeController for CoordIme {
    fn toggle(&self, target: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_ime_toggle(target);
        }
        Ok(())
    }
    fn open_setting(&self, _page: &str) -> anyhow::Result<()> {
        warn!("setting.open: Rust 端设置应用待补");
        Ok(())
    }
    fn open_setting_web(&self, _page: &str) -> anyhow::Result<()> {
        warn!("setting.web: Rust 端设置应用待补");
        Ok(())
    }
    fn set_schema(&self, id: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_set_schema(id);
        }
        Ok(())
    }
    fn theme_cycle(&self, dir: &str) -> anyhow::Result<String> {
        match self.0.upgrade() {
            Some(c) => Ok(c.cmd_theme_cycle(dir)),
            None => Ok(String::new()),
        }
    }
}

/// 词库控制器：dict.add 接通用户词层。
struct CoordDict(Weak<Coordinator>);

impl DictService for CoordDict {
    fn add_word(&self, text: &str, code: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.cmd_dict_add(text, code)?;
        }
        Ok(())
    }
}

/// 进程启动（std，跨平台，无需 coordinator）：proc.run / proc.shell。
struct StdProc;

impl ProcessRunner for StdProc {
    fn run(&self, cmd: &str, args: &[String]) -> anyhow::Result<()> {
        Command::new(cmd).args(args).spawn()?;
        Ok(())
    }
    fn shell(&self, cmdline: &str) -> anyhow::Result<()> {
        shell_spawn(cmdline)
    }
    fn shell_ex(&self, cmdline: &str, _flags: &[String]) -> anyhow::Result<()> {
        // flags(term/pwsh)暂未区分，统一走默认 shell（待平台 shell 选择补齐）。
        shell_spawn(cmdline)
    }
}

#[cfg(windows)]
fn shell_spawn(cmdline: &str) -> anyhow::Result<()> {
    Command::new("cmd").args(["/C", cmdline]).spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn shell_spawn(cmdline: &str) -> anyhow::Result<()> {
    Command::new("sh").args(["-c", cmdline]).spawn()?;
    Ok(())
}

/// 打开 URL / 程序 / 文件（系统外壳，跨平台）：open / web.search 默认通路。
struct ShellOpener;

impl UrlOpener for ShellOpener {
    fn open(&self, target: &str) -> anyhow::Result<()> {
        open::that(target)?;
        Ok(())
    }
}

/// 系统剪贴板写入（clip.copy）。读/粘贴需平台读剪贴板或按键注入。
///
/// **macOS 预留**：set_text/get_text 接 `NSPasteboard`（`general` → `setString:forType:` /
/// `stringForType:`，类型 `NSPasteboardTypeString`，可用 `objc2`/`cocoa` crate）；paste 用
/// 合成 ⌘V（见下方 cfg 分支）。
struct SysClip;

impl ClipboardService for SysClip {
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        #[cfg(windows)]
        {
            wind_ui::popup_menu::set_clipboard_text(text);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            // TODO(macos): NSPasteboard general setString。其他 Unix 暂无统一通道。
            let _ = text;
            anyhow::bail!("clip.copy: 当前平台暂未支持（macOS 待接 NSPasteboard）")
        }
    }
    fn get_text(&self) -> anyhow::Result<String> {
        // TODO(macos): NSPasteboard general stringForType。
        anyhow::bail!("clip get: 暂未支持（待平台读剪贴板）")
    }
    fn paste(&self) -> anyhow::Result<()> {
        // 经按键注入合成粘贴热键：Windows/Linux Ctrl+V，macOS ⌘V（Cmd 经 vk:0x37 LeftCmd）。
        use wind_cmdbar::KeyInjector;
        #[cfg(target_os = "macos")]
        {
            // TODO(macos): 待 key 注入接入 CGEvent 后，用 Cmd+V（此处先占位组合）。
            wind_keys::key_inject::SysKeys.tap("vk:0x37+v")
        }
        #[cfg(not(target_os = "macos"))]
        {
            wind_keys::key_inject::SysKeys.tap("Ctrl+v")
        }
    }
}
