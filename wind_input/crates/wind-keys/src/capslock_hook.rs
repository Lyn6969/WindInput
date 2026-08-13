//! CapsLock 全局低级键盘钩子（`WH_KEYBOARD_LL`）。
//!
//! # 为什么非它不可
//!
//! CapsLock / NumLock / ScrollLock 的锁定态由系统在**输入线程状态机**里维护，位置在 TSF
//! key event sink **之前**——TSF 里 `pfEaten = TRUE` 只表示「这个键事件我处理了」，**不是**
//! 「这个键没发生过」，压不住锁定态翻转（2026-08-11 真机实测）。
//!
//! 先前尝试过「让它翻转，再 `SendInput` 回敲复原」，真机撞到两个无解的问题：快速连按时
//! 物理事件与注入事件在队列里的相对顺序无法保证，大写会卡住；且那次真实的状态变化会被
//! 厂商 OSD 工具（联想等）观测到并弹窗。**事后修正在竞态下没有正确解**，只能在它发生之前
//! 阻止它发生。
//!
//! `LowLevelKeyboardProc` 是用户态唯一做得到的位置，MS 文档原文：
//! > the callback function is called **before the asynchronous state of the key is updated**
//! > ...it may return a nonzero value to prevent the system from passing the message to the
//! > rest of the hook chain or the target window procedure.
//!
//! # 三条硬约束（都来自文档，违反了都是无声故障）
//!
//! 1. **回调必须极快返回**。超时后 Win7+ 会把钩子**静默移除**，且「there is no way for the
//!    application to know whether the hook is removed」。故回调里只读一个原子量 + 一次
//!    非阻塞 `send`，**不加锁、不分配、不做 IPC**。
//! 2. **安装线程必须有消息循环**（钩子是靠给该线程发消息来调用的）。故本模块自带专用线程。
//! 3. ★ **专用线程，不能搭 UI 线程的便车**。UI 线程要渲染候选窗（LayeredWindow 位图合成），
//!    某次渲染慢过 `LowLevelHooksTimeout`（Win10 1709+ 上限 1000ms）钩子就永久掉了，而且
//!    没有任何信号——本仓最难排查的故障全是这一类。
//!
//! # 安装门控
//!
//! ★ **只有用户在 `keys.session_actions` 里真的配了 `capslock` 才安装**（协调器侧判定）。
//! 没配的用户进程里根本不存在全局键盘钩子——这是本功能唯一的风险控制手段，不可省。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// 钩子此刻是否应当吃掉 CapsLock。
///
/// 由协调器随「有没有输入会话」实时更新。★ 它为 true 的时间窗必须尽量短：钩子是**全局**的，
/// 这个标志滞留就意味着用户在**别的应用**里按 CapsLock 也切不动大小写——那比功能不生效
/// 糟糕得多，属于必须优先避免的故障方向。
static SHOULD_EAT: AtomicBool = AtomicBool::new(false);

/// 按下 CapsLock 时的通知回调。在钩子线程里执行，实现方必须只做非阻塞投递。
type PressCallback = Box<dyn Fn() + Send + Sync + 'static>;
static CALLBACK: OnceLock<PressCallback> = OnceLock::new();

/// 设置「当前是否拦截 CapsLock」。协调器在会话状态变化时调用，钩子未安装时也可安全调用。
pub fn set_should_eat(eat: bool) {
    SHOULD_EAT.store(eat, Ordering::Relaxed);
}

/// 当前拦截状态（供日志/诊断）。
pub fn should_eat() -> bool {
    SHOULD_EAT.load(Ordering::Relaxed)
}

#[cfg(windows)]
mod imp {
    use super::{CALLBACK, SHOULD_EAT};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use tracing::{debug, error, info};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, PostThreadMessageW,
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
    };

    const VK_CAPITAL: u32 = 0x14;
    /// `nCode` 为该值时 wParam/lParam 才含按键信息；小于 0 时文档要求原样下传。
    const HC_ACTION: i32 = 0;

    /// 已安装的钩子。Drop 即卸载（停消息泵 → 线程内 `UnhookWindowsHookEx` → join）。
    pub struct CapsLockHook {
        thread_id: u32,
        join: Option<std::thread::JoinHandle<()>>,
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // 文档：nCode < 0 必须直接下传；这里连 HC_ACTION 之外的一律下传。
        // 未开启拦截时也立刻下传——绝大多数按键走的是这条路径，必须最短。
        if code == HC_ACTION && SHOULD_EAT.load(Ordering::Relaxed) {
            // SAFETY: 文档保证 nCode == HC_ACTION 时 lParam 指向有效的 KBDLLHOOKSTRUCT。
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if info.vkCode == VK_CAPITAL {
                // 注入事件一律放行。本模块自己不注入 CapsLock，但别的工具（AHK / 厂商热键
                // 程序）可能会——拦下它们既无意义，又会让那些工具行为异常。
                if (info.flags & LLKHF_INJECTED).0 == 0 {
                    let msg = wparam.0 as u32;
                    if (msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN)
                        && let Some(cb) = CALLBACK.get()
                    {
                        cb();
                    }
                    // ★ down 和 up **都要吃**。只吃 down 会让系统状态机收到不成对的
                    // 事件，某些宿主会据此认为该键仍处于按下状态。
                    return LRESULT(1);
                }
            }
        }
        // SAFETY: 转交钩子链的标准调用；参数原样下传。
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    impl CapsLockHook {
        /// 安装钩子。`on_press` 在钩子线程执行，**必须只做非阻塞投递**（见模块文档约束 1）。
        ///
        /// 回调只能设置一次（`OnceLock`）——钩子的装卸可以反复，回调本身是进程级常量。
        pub fn install(on_press: super::PressCallback) -> anyhow::Result<Self> {
            let _ = CALLBACK.set(on_press);

            let (tx, rx) = mpsc::channel::<Result<u32, String>>();
            let join = std::thread::Builder::new()
                .name("capslock-hook".into())
                .spawn(move || unsafe {
                    let hook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) {
                        Ok(h) => h,
                        Err(e) => {
                            let _ = tx.send(Err(format!("SetWindowsHookExW 失败: {e}")));
                            return;
                        }
                    };
                    let tid = windows::Win32::System::Threading::GetCurrentThreadId();
                    let _ = tx.send(Ok(tid));

                    // 钩子靠「给本线程发消息」来调用，故必须有消息泵。这里只等 WM_QUIT，
                    // 泵本身不做任何事——线程越空闲，钩子回调的响应越不可能超时。
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {}

                    if let Err(e) = UnhookWindowsHookEx(hook) {
                        error!("CapsLock 钩子卸载失败: {e}");
                    } else {
                        debug!("CapsLock 钩子已卸载");
                    }
                })?;

            match rx.recv() {
                Ok(Ok(thread_id)) => {
                    info!("CapsLock 全局钩子已安装 (tid={thread_id})");
                    Ok(Self {
                        thread_id,
                        join: Some(join),
                    })
                }
                Ok(Err(e)) => anyhow::bail!("{e}"),
                Err(e) => anyhow::bail!("钩子线程未回报安装结果: {e}"),
            }
        }
    }

    impl Drop for CapsLockHook {
        fn drop(&mut self) {
            // 卸载期间先停止拦截：PostThreadMessage 到线程真正退出之间仍会有回调进来。
            SHOULD_EAT.store(false, Ordering::Relaxed);
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    /// 非 Windows 平台的空壳：安装恒失败，调用方按「功能不可用」降级。
    pub struct CapsLockHook;

    impl CapsLockHook {
        pub fn install(_on_press: super::PressCallback) -> anyhow::Result<Self> {
            anyhow::bail!("低级键盘钩子仅 Windows 可用")
        }
    }
}

pub use imp::CapsLockHook;
