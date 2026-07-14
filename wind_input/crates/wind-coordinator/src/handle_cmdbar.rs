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
        svc.dict = Some(Arc::new(CoordDict(weak.clone())));
        // 无需 coordinator 回调的能力：进程启动、打开 URL/文件、写剪贴板（纯平台/std）。
        svc.proc = Some(Arc::new(CoordProc(weak.clone())));
        svc.open = Some(Arc::new(CoordOpener(weak.clone())));
        svc.clip = Some(Arc::new(SysClip(weak.clone())));
        // 按键合成：macOS 服务进程（LaunchAgent）无辅助功能授权无法 post CGEvent，改推 IPC 帧
        // 给 .app 侧 KeySynthesizer 合成（见 handle_cmdbar_macos）；其它平台进程内 SendInput/CGEvent。
        #[cfg(target_os = "macos")]
        {
            svc.keys = Some(crate::handle_cmdbar_macos::make_keys(weak.clone()));
        }
        #[cfg(not(target_os = "macos"))]
        {
            svc.keys = Some(Arc::new(wind_keys::key_inject::SysKeys));
        }
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
        let (front_app, front_title, front_sel) = self.front_ctx_snapshot();
        let ctx = CmdbarCtx {
            input: input.to_string(),
            now: Local::now(),
            last: self.recent_commits_snapshot(),
            front_app,
            front_title,
            front_sel,
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

    /// 顶码等同步场景：求值命令源，动作链**全为 Text**（无副作用）时返回拼接文本 `Some(text)`；
    /// 含任一 Effect（shell/key/clip 等需异步回调 coordinator 锁）返回 `None`，交异步 spawn 执行。
    ///
    /// **不跑任何 Effect**——纯文本求值只碰 `CmdbarCtx` 读快照（input/last/clip/now/env），无锁、
    /// 无副作用，可在持 state 锁的按键线程内安全调用。`$SS` 组 / 求值失败 / services 未装配亦返回 None。
    pub(crate) fn eval_command_text_only(&self, src: &str, input: &str) -> Option<String> {
        let services = self.cmdbar_services.get()?;
        let (front_app, front_title, front_sel) = self.front_ctx_snapshot();
        let ctx = CmdbarCtx {
            input: input.to_string(),
            now: Local::now(),
            last: self.recent_commits_snapshot(),
            front_app,
            front_title,
            front_sel,
            services,
        };
        let reg = wind_cmdbar::default_registry();
        let actions = match wind_cmdbar::evaluate_phrase(src, &ctx, reg) {
            Ok(wind_cmdbar::PhraseEval::Single { actions, .. }) => actions,
            _ => return None,
        };
        // 含副作用 → None（交异步 spawn 执行，见 top_commit_command_with_remainder）。
        if actions
            .iter()
            .any(|a| a.kind != wind_cmdbar::ActionKind::Text)
        {
            return None;
        }
        // 纯文本：按序拼接（此刻 act.run 只求值文本表达式，不回调 coordinator 锁）。
        let mut text = String::new();
        for a in &actions {
            text.push_str(&a.run(&ctx, reg).ok()?);
        }
        Some(text)
    }
}

/// 命令栏求值上下文（coordinator 适配）。提供 input/now/env + 上屏历史 last + 剪贴板 clip + services；
/// sel/app/title 待前台窗口能力补齐后接入（与 Go 早期实现一致先留空）。
struct CmdbarCtx<'a> {
    input: String,
    now: DateTime<Local>,
    /// 上屏历史快照（index 0 = 最近一次），触发命令时冻结。
    last: Vec<String>,
    /// 前台上下文快照（app/title/sel），darwin 经 CMD_FRONT_CONTEXT 于聚焦时上报；
    /// 其它平台为空。触发命令时冻结。
    front_app: String,
    front_title: String,
    front_sel: String,
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
        // 仅当前剪贴板（n>1 历史栈未实现）。macOS 走 pbpaste（与 SysClip::get_text 一致），
        // 让 clip() 取值与 clip.copy 写入对称——此前 macOS 硬编码返回空是 bug。
        #[cfg(any(windows, target_os = "macos"))]
        {
            wind_ui::popup_menu::get_clipboard_text()
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            String::new()
        }
    }
    fn sel(&self) -> String {
        self.front_sel.clone()
    }
    fn app(&self) -> String {
        self.front_app.clone()
    }
    fn title(&self) -> String {
        self.front_title.clone()
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
    fn open_setting(&self, page: &str) -> anyhow::Result<()> {
        if let Some(c) = self.0.upgrade() {
            c.open_settings(if page.is_empty() { None } else { Some(page) });
        }
        Ok(())
    }
    fn open_setting_web(&self, page: &str) -> anyhow::Result<()> {
        // web 配置已废弃，降级到 native 设置
        if let Some(c) = self.0.upgrade() {
            c.open_settings(if page.is_empty() { None } else { Some(page) });
        }
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

/// 进程启动：proc.run 经 TSF 侧执行（前台权限）；proc.shell 仍走本地 shell。
struct CoordProc(Weak<Coordinator>);

impl ProcessRunner for CoordProc {
    fn run(&self, cmd: &str, args: &[String]) -> anyhow::Result<()> {
        // macOS：进程内直接 spawn（无需 IPC 转 TSF）；其它平台经 push_shell_exec 借前台权限。
        #[cfg(target_os = "macos")]
        {
            let _ = &self.0;
            crate::handle_cmdbar_macos::run_native(cmd, args)
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self.0.upgrade() {
                Some(c) => c.push_shell_exec(cmd, &shell_quote_args(args)),
                None => warn!("proc.run: coordinator 已释放，跳过执行 {cmd:?}"),
            }
            Ok(())
        }
    }
    fn shell(&self, cmdline: &str) -> anyhow::Result<()> {
        shell_spawn(cmdline)
    }
    fn shell_ex(&self, cmdline: &str, _flags: &[String]) -> anyhow::Result<()> {
        // flags(term/pwsh)暂未区分，统一走默认 shell（待平台 shell 选择补齐）。
        shell_spawn(cmdline)
    }
}

/// 将 argv 列表拼成 ShellExecuteW lpParameters 字符串，含空格/引号的参数加双引号。
/// 仅非 macOS（经 push_shell_exec 转 TSF 侧 ShellExecuteW）路径使用。
#[cfg(not(target_os = "macos"))]
fn shell_quote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.contains(' ') || a.contains('"') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

/// 打开 URL / 程序 / 文件：经 TSF 侧 ShellExecuteW 在前台应用进程中执行。
struct CoordOpener(Weak<Coordinator>);

impl UrlOpener for CoordOpener {
    fn open(&self, target: &str) -> anyhow::Result<()> {
        // macOS：进程内经 `open` CLI 拉起（无需 IPC 转 TSF）；其它平台经 push_shell_exec。
        #[cfg(target_os = "macos")]
        {
            let _ = &self.0;
            crate::handle_cmdbar_macos::open_native(target)
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self.0.upgrade() {
                Some(c) => c.push_shell_exec(target, ""),
                None => warn!("open: coordinator 已释放，跳过执行 {target:?}"),
            }
            Ok(())
        }
    }
}

/// 系统剪贴板服务（clip.copy / clip.get / clip.paste）。
///
/// set/get 复用 `wind_ui::popup_menu`：Windows 走 CF_UNICODETEXT，macOS 走 `pbcopy`/`pbpaste`
/// 子进程（无需 AppKit/主线程，服务进程即可用）；其它 Unix 暂无统一通道。
/// paste 经按键注入合成粘贴热键（macOS 推 CmdKeyTap 给 .app，见 [`SysClip::paste`]）。
struct SysClip(Weak<Coordinator>);

impl ClipboardService for SysClip {
    fn set_text(&self, text: &str) -> anyhow::Result<()> {
        // try 版传播失败（OpenClipboard 被占用重试后仍失败等），run_actions 记 warn；
        // 菜单"复制"等 best-effort 路径仍用无返回值的 set_clipboard_text。
        wind_ui::popup_menu::try_set_clipboard_text(text)
    }
    fn get_text(&self) -> anyhow::Result<String> {
        #[cfg(any(windows, target_os = "macos"))]
        {
            Ok(wind_ui::popup_menu::get_clipboard_text())
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            anyhow::bail!("clip.get: 当前平台暂未支持")
        }
    }
    fn paste(&self) -> anyhow::Result<()> {
        // macOS：不合成 ⌘V，经 IMKit insertText 上屏剪贴板文本（见 handle_cmdbar_macos::paste_via_ime）。
        #[cfg(target_os = "macos")]
        {
            crate::handle_cmdbar_macos::paste_via_ime(&self.0);
            Ok(())
        }
        // Windows/Linux：沿用进程内合成 Ctrl+V（有 HID 层修饰键状态，直接生效；且保留富文本粘贴）。
        #[cfg(not(target_os = "macos"))]
        {
            let _ = &self.0;
            use wind_cmdbar::KeyInjector;
            wind_keys::key_inject::SysKeys.tap("Ctrl+v")
        }
    }
}
