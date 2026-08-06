//! macOS 全局热键：Carbon `RegisterEventHotKey`。
//!
//! 对位 Windows 侧 `manager.rs` 里 `UiCommand::RegisterGlobalHotkeys` 的 `RegisterHotKey`
//! 分支。触发后同样产出 [`UiEvent::GlobalHotkey`]，协调器的 `handle_global_hotkey` 跨平台共用。
//!
//! # 为什么是 Carbon，不是 CGEventTap / NSEvent 全局监听
//!
//! 后两者都要「辅助功能」(Accessibility) 授权——ad-hoc 签名重部署时 cdhash 一变旧授权就
//! 失效（`KeySynthesizer` 已经踩过这个坑），而全局热键的语义是「本输入法没激活时也得生效」，
//! 一旦授权掉了就是静默失灵。且那两条路要接管全局按键流，为了几个组合键去过一遍所有按键，
//! 量级不对。Carbon 热键免授权，只在命中组合时回调。
//!
//! # 线程约定（照做，不要"顺手"简化）
//!
//! 热键事件由系统投递到**主线程**的 Carbon 事件队列，只有主线程在跑 CFRunLoop 才收得到；
//! 辅助线程自己跑一个 CFRunLoop 是收不到的。因此：
//!
//! - **主线程**调 [`run_main_loop`]：装 handler + 建唤醒源 + 跑 CFRunLoop（服务的 `main`
//!   在 macOS 上最后停在这里，取代原先的 `restart_rx.recv()` 阻塞）；
//! - **forwarder 线程**调 [`apply`]：只把新热键表塞进 `PENDING` 并唤醒主线程；真正的
//!   `RegisterEventHotKey` / `UnregisterEventHotKey` 一律在主线程的 perform 回调里执行。
//!
//! 把注册直接放在 forwarder 线程上跑，表面上"能编过也不报错"，但 Carbon Event Manager
//! 是主线程亲和的，出问题的形态是热键**时灵时不灵**——比查不出来更糟。

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use core_foundation_sys::base::CFIndex;
use core_foundation_sys::date::CFTimeInterval;
use core_foundation_sys::runloop::{
    CFRunLoopAddSource, CFRunLoopGetMain, CFRunLoopRunInMode, CFRunLoopSourceContext,
    CFRunLoopSourceCreate, CFRunLoopSourceRef, CFRunLoopSourceSignal, CFRunLoopStop,
    CFRunLoopWakeUp, kCFRunLoopCommonModes, kCFRunLoopDefaultMode,
};

use crate::manager::{GlobalHotkeyEntry, UiEvent};

// ── Carbon FFI ────────────────────────────────────────────────────────────
// 只声明用得到的部分。四字符码用 u32 字面量（`'keyb'` 之类在 Rust 里没有字面量语法）。

type OSStatus = i32;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

const K_EVENT_CLASS_KEYBOARD: u32 = 0x6B65_7962; // 'keyb'
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const K_EVENT_PARAM_DIRECT_OBJECT: u32 = 0x2D2D_2D2D; // '----'
const TYPE_EVENT_HOT_KEY_ID: u32 = 0x686B_6964; // 'hkid'
/// 热键签名，任意四字符码，只用于和别家的热键区分：'wind'
const HOTKEY_SIGNATURE: u32 = 0x7769_6E64;

type EventHandlerProc = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: u32,
        type_list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(hot_key: EventHotKeyRef) -> OSStatus;
    fn GetEventParameter(
        event: EventRef,
        name: u32,
        desired_type: u32,
        actual_type: *mut u32,
        buffer_size: usize,
        actual_size: *mut usize,
        data: *mut c_void,
    ) -> OSStatus;
    fn TransformProcessType(psn: *const ProcessSerialNumber, form: u32) -> OSStatus;
}

#[repr(C)]
struct ProcessSerialNumber {
    high: u32,
    low: u32,
}

/// `kCurrentProcess`（`{0, 2}`）。
const CURRENT_PROCESS: ProcessSerialNumber = ProcessSerialNumber { high: 0, low: 2 };
/// `kProcessTransformToUIElementApplication`：提升为「UI 元素应用」——有窗口服务器连接，
/// 但**不进 Dock、不占菜单栏**。
const TRANSFORM_TO_UI_ELEMENT: u32 = 4;

// ── Carbon 修饰键掩码（Events.h）──
const CARBON_CMD: u32 = 0x0100;
const CARBON_SHIFT: u32 = 0x0200;
const CARBON_OPTION: u32 = 0x0800;
const CARBON_CONTROL: u32 = 0x1000;

// ── Win32 MOD_*（GlobalHotkeyEntry.modifiers 的口径）──
const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;

/// Win32 修饰位 → Carbon 修饰掩码。
///
/// 按物理键直译：Alt→Option、Win→Command。**不做"Ctrl 自动换 Command"的贴心翻译**——
/// 配置里写的是哪个键就注册哪个键，否则用户在设置界面看到的与实际生效的不是一回事，
/// 且与按键注入侧（`key_inject` 的 `win`→Command 映射）口径分叉。
fn win32_mods_to_carbon(m: u32) -> u32 {
    let mut out = 0;
    if m & MOD_ALT != 0 {
        out |= CARBON_OPTION;
    }
    if m & MOD_CONTROL != 0 {
        out |= CARBON_CONTROL;
    }
    if m & MOD_SHIFT != 0 {
        out |= CARBON_SHIFT;
    }
    if m & MOD_WIN != 0 {
        out |= CARBON_CMD;
    }
    out
}

// ── 跨线程状态 ────────────────────────────────────────────────────────────

/// 裸指针跨线程搬运的封装。
///
/// `Sync` 的安全性依据：`WAKE_SOURCE` 里的 `CFRunLoopSourceRef` 只被两种方式使用——
/// 主线程建它、其它线程对它调 `CFRunLoopSourceSignal`；后者是 CF 明文保证线程安全的
/// （signal 的全部用途就是从别的线程叫醒 run loop）。`REGISTERED` 里的 `EventHotKeyRef`
/// 只在主线程 perform 回调里读写，Mutex 之外无并发。
struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

#[derive(Default)]
struct Pending {
    /// 待应用的热键表；`None` = 无新请求。
    entries: Option<Vec<GlobalHotkeyEntry>>,
    /// 回协调器的事件通道（每次 apply 刷新，允许重建）。
    ev_tx: Option<Sender<UiEvent>>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// 主线程建好的唤醒源。`apply` 用它把主线程从 CFRunLoop 里叫醒。
static WAKE_SOURCE: OnceLock<SendPtr<c_void>> = OnceLock::new();
/// 已注册热键：(Carbon ref, 热键 id)。只在主线程读写。
static REGISTERED: Mutex<Vec<(SendPtr<c_void>, i32)>> = Mutex::new(Vec::new());
/// 热键 id → 动作名 + 事件通道。handler 回调（主线程）查它决定发什么事件。
static ACTIONS: Mutex<Option<(HashMap<u32, String>, Sender<UiEvent>)>> = Mutex::new(None);
/// 主循环退出标志（服务重启时置位）。
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);

/// 从任意线程提交新的全局热键表（覆盖式：主线程会先撤旧再注册新）。
///
/// 只入队 + 唤醒，不碰 Carbon。主线程尚未进入 [`run_main_loop`] 时也可调用——请求会
/// 留在 `PENDING` 里，等主循环起来后的第一次 perform 一并应用。
pub fn apply(entries: Vec<GlobalHotkeyEntry>, ev_tx: Sender<UiEvent>) {
    {
        let mut guard = match PENDING.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = Some(Pending {
            entries: Some(entries),
            ev_tx: Some(ev_tx),
        });
    }
    // 主循环还没起来：留在 PENDING 里，run_main_loop 起来后会主动 drain 一次。
    if let Some(src) = WAKE_SOURCE.get() {
        unsafe {
            CFRunLoopSourceSignal(src.0 as CFRunLoopSourceRef);
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }
}

/// 请求主循环退出（服务重启路径）。可从任意线程调用。
pub fn stop_main_loop() {
    SHOULD_EXIT.store(true, Ordering::SeqCst);
    unsafe { CFRunLoopStop(CFRunLoopGetMain()) };
}

/// 主线程入口：装 Carbon handler、建唤醒源，然后跑 CFRunLoop 直到 [`stop_main_loop`]。
///
/// **必须在进程主线程调用**（见模块头「线程约定」）。
pub fn run_main_loop() {
    unsafe {
        // 0. 把本进程提升为「UI 元素应用」。
        //
        // ⚠️ **这一步不能省，且它的缺失极难从返回码上看出来**：服务是 LaunchAgent 拉起的
        // 裸可执行文件，默认是后台进程，没有窗口服务器连接。此时 `InstallEventHandler` 与
        // `RegisterEventHotKey` **都照常返回 noErr**（日志里赫然写着「N/N 条生效」），
        // 但热键事件经由窗口服务器投递到应用事件队列，而这个进程根本没有那个队列——
        // 回调于是一次也不会触发。
        //
        // 实测判据：不做本调用时进程不出现在 `lsappinfo list` 里，做了才出现。
        // UIElement 形态不进 Dock、不占菜单栏，对一个输入法服务没有可见副作用。
        let st = TransformProcessType(&CURRENT_PROCESS, TRANSFORM_TO_UI_ELEMENT);
        if st != 0 {
            tracing::warn!("全局热键: TransformProcessType 失败 OSStatus={st}，热键可能收不到事件");
        }

        // 1. 装热键 handler（应用级 target，收 kEventHotKeyPressed）。
        let spec = EventTypeSpec {
            event_class: K_EVENT_CLASS_KEYBOARD,
            event_kind: K_EVENT_HOT_KEY_PRESSED,
        };
        let mut handler: EventHandlerRef = std::ptr::null_mut();
        let st = InstallEventHandler(
            GetApplicationEventTarget(),
            hotkey_handler,
            1,
            &spec,
            std::ptr::null_mut(),
            &mut handler,
        );
        if st != 0 {
            // 装不上就没有全局热键，但服务其余部分照常——不 panic，只把结论写进日志。
            tracing::error!("全局热键: InstallEventHandler 失败 OSStatus={st}，本次运行无全局热键");
        }

        // 2. 建唤醒源，挂到主 run loop 的 common modes。
        //
        // ⚠️ 逐字段构造，**不能**用 `mem::zeroed()`：`perform` 是非空函数指针
        // （不是 `Option<fn>`），全零对它是非法值——真机上会在启动时
        // 「attempted to zero-initialize type ... which is invalid」直接 abort，
        // 且 `cargo check`/`cargo test` 都看不出来（只在跑到这一行才炸）。
        let mut ctx = CFRunLoopSourceContext {
            version: 0 as CFIndex,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: wake_perform,
        };
        let src = CFRunLoopSourceCreate(std::ptr::null(), 0 as CFIndex, &mut ctx);
        if src.is_null() {
            tracing::error!("全局热键: CFRunLoopSourceCreate 失败，热键表将无法热更新");
        } else {
            CFRunLoopAddSource(CFRunLoopGetMain(), src, kCFRunLoopCommonModes);
            let _ = WAKE_SOURCE.set(SendPtr(src as *mut c_void));
        }

        // 3. 主循环起来之前可能已经有人提交过热键表（协调器构造期就会 sync 一次），
        //    先 drain 一次，避免那批热键要等到下一次配置变更才生效。
        drain_pending();

        tracing::info!("全局热键: 主线程 CFRunLoop 启动");
        while !SHOULD_EXIT.load(Ordering::SeqCst) {
            // 大超时 + 循环重入：CFRunLoopStop 会让本次调用返回，回到循环顶再判退出标志。
            // 用 RunInMode 而非 CFRunLoopRun，是为了「先置 SHOULD_EXIT 再 Stop」与
            // 「Stop 早于 Run 到达」两种时序都能正确退出，不会卡死在 loop 里。
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0e10 as CFTimeInterval, 0);
        }
        tracing::info!("全局热键: 主线程 CFRunLoop 退出");
    }
}

/// 唤醒源回调（主线程）。
extern "C" fn wake_perform(_info: *const c_void) {
    unsafe { drain_pending() };
}

/// 应用 `PENDING` 里的热键表：先撤全部旧的，再注册新的。**只在主线程调用**。
unsafe fn drain_pending() {
    let pending = {
        let mut guard = match PENDING.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.take()
    };
    let Some(pending) = pending else { return };
    let (Some(entries), Some(ev_tx)) = (pending.entries, pending.ev_tx) else {
        return;
    };

    // 覆盖式：配置重载可能改键/删项，旧的必须全撤。
    {
        let mut reg = match REGISTERED.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for (r, id) in reg.drain(..) {
            let st = unsafe { UnregisterEventHotKey(r.0 as EventHotKeyRef) };
            if st != 0 {
                tracing::warn!("全局热键: 撤销 id={id} 失败 OSStatus={st}");
            }
        }
    }

    let mut actions: HashMap<u32, String> = HashMap::new();
    let mut ok = 0usize;
    for e in &entries {
        let Some(key_code) = wind_keys::key_inject::vk_to_cgkeycode(e.vk) else {
            // OEM 符号键等表里没有的 VK：跳过而非静默当成 keycode 0（那会注册出一个
            // 按 'a' 就触发的热键，比不生效更坏）。
            tracing::warn!(
                "全局热键: {} 的 vk={:#04X} 无 macOS CGKeyCode 映射，跳过",
                e.action,
                e.vk
            );
            continue;
        };
        let mods = win32_mods_to_carbon(e.modifiers);
        let hk_id = EventHotKeyID {
            signature: HOTKEY_SIGNATURE,
            id: e.id as u32,
        };
        let mut r: EventHotKeyRef = std::ptr::null_mut();
        let st = unsafe {
            RegisterEventHotKey(
                key_code as u32,
                mods,
                hk_id,
                GetApplicationEventTarget(),
                0,
                &mut r,
            )
        };
        if st != 0 || r.is_null() {
            // 组合被别的程序占用等：仅告警，不影响其余热键（与 Windows 分支同策略）。
            tracing::warn!(
                "全局热键: 注册 {} 失败 OSStatus={st}（组合可能已被系统或其它程序占用）",
                e.action
            );
            continue;
        }
        actions.insert(e.id as u32, e.action.clone());
        match REGISTERED.lock() {
            Ok(mut g) => g.push((SendPtr(r as *mut c_void), e.id)),
            Err(p) => p.into_inner().push((SendPtr(r as *mut c_void), e.id)),
        }
        ok += 1;
        tracing::debug!(
            "全局热键: 已注册 {}（carbon_mods={:#06X} keycode={}）",
            e.action,
            mods,
            key_code
        );
    }

    match ACTIONS.lock() {
        Ok(mut g) => *g = Some((actions, ev_tx)),
        Err(p) => *p.into_inner() = Some((actions, ev_tx)),
    }
    tracing::info!("全局热键: 应用完成，{}/{} 条生效", ok, entries.len());
}

/// Carbon 热键回调（主线程）：取热键 id → 查动作名 → 发 [`UiEvent::GlobalHotkey`]。
extern "C" fn hotkey_handler(
    _call: EventHandlerCallRef,
    event: EventRef,
    _user: *mut c_void,
) -> OSStatus {
    let mut hk_id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    let st = unsafe {
        GetEventParameter(
            event,
            K_EVENT_PARAM_DIRECT_OBJECT,
            TYPE_EVENT_HOT_KEY_ID,
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            &mut hk_id as *mut _ as *mut c_void,
        )
    };
    if st != 0 {
        tracing::warn!("全局热键: GetEventParameter 失败 OSStatus={st}");
        return 0; // noErr：事件已消费，不再往下传
    }
    if hk_id.signature != HOTKEY_SIGNATURE {
        return 0;
    }
    let guard = match ACTIONS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some((map, tx)) = guard.as_ref()
        && let Some(action) = map.get(&hk_id.id)
    {
        tracing::debug!("全局热键触发: {action}");
        let _ = tx.send(UiEvent::GlobalHotkey(action.clone()));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_mods_map_to_carbon_by_physical_key() {
        assert_eq!(win32_mods_to_carbon(MOD_ALT), CARBON_OPTION);
        assert_eq!(win32_mods_to_carbon(MOD_CONTROL), CARBON_CONTROL);
        assert_eq!(win32_mods_to_carbon(MOD_SHIFT), CARBON_SHIFT);
        assert_eq!(win32_mods_to_carbon(MOD_WIN), CARBON_CMD);
        assert_eq!(win32_mods_to_carbon(0), 0);
    }

    #[test]
    fn win32_mods_combine() {
        assert_eq!(
            win32_mods_to_carbon(MOD_CONTROL | MOD_SHIFT),
            CARBON_CONTROL | CARBON_SHIFT
        );
    }

    /// MOD_NOREPEAT(0x4000) 是 Windows 独有的注册选项位，不该漏进 Carbon 掩码。
    #[test]
    fn unknown_win32_bits_are_ignored() {
        assert_eq!(win32_mods_to_carbon(0x4000 | MOD_ALT), CARBON_OPTION);
    }
}
