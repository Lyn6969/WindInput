//! macOS 系统明暗变更监听：分发通知 `AppleInterfaceThemeChangedNotification`。
//!
//! 对位 Windows 的 `WM_SETTINGCHANGE`（见 `manager.rs` 消息泵里
//! `take_system_color_changed` → [`UiEvent::SystemThemeChanged`]）。macOS 此前完全没有这条
//! 通路：`system_prefers_dark()` 只在**启动时**被 `resolve_dark` 调到一次，用户之后在系统
//! 设置里切明暗，输入法就一直停在启动那刻的档位——「跟随系统」看着像坏的。
//!
//! # 为什么用 CFNotificationCenter 而不是 NSDistributedNotificationCenter
//!
//! 服务进程不链 AppKit（它不建窗口，渲染是光栅进 SHM 再推给 `.app`）。
//! `CFNotificationCenterGetDistributedCenter` 是同一套分发通知的 CoreFoundation 接口，
//! 拿得到同一个通知，不必为此把 AppKit 拖进来。
//!
//! # 线程
//!
//! 分发通知投递到**添加观察者的那个线程的 run loop**。服务进程只有主线程在跑 CFRunLoop
//! （见 `global_hotkey_macos::run_main_loop`），故 [`ensure_installed`] 必须在主线程调用。
//! 它由 `global_hotkey_macos::drain_pending` 顺带调用——那里天然已在主线程，且必定拿得到
//! `ev_tx`（协调器构造期就会 sync 一次热键表，空表也下发）。

use crate::manager::UiEvent;
use core_foundation_sys::base::kCFAllocatorDefault;
use core_foundation_sys::dictionary::CFDictionaryRef;
use core_foundation_sys::notification_center::{
    CFNotificationCenterAddObserver, CFNotificationCenterGetDistributedCenter,
    CFNotificationCenterRef, CFNotificationName,
    CFNotificationSuspensionBehaviorDeliverImmediately,
};
use core_foundation_sys::string::{CFStringCreateWithBytes, kCFStringEncodingUTF8};
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

/// 观察者回调里要用的事件通道。只装一次（观察者也只注册一次）。
static EV_TX: OnceLock<Sender<UiEvent>> = OnceLock::new();
static INSTALLED: OnceLock<()> = OnceLock::new();

const NOTIFICATION: &[u8] = b"AppleInterfaceThemeChangedNotification";

/// 注册系统明暗变更观察者（幂等）。**必须在主线程调用**（见模块头「线程」）。
pub fn ensure_installed(ev_tx: Sender<UiEvent>) {
    if INSTALLED.get().is_some() {
        return;
    }
    let _ = EV_TX.set(ev_tx);
    unsafe {
        let name = CFStringCreateWithBytes(
            kCFAllocatorDefault,
            NOTIFICATION.as_ptr(),
            NOTIFICATION.len() as isize,
            kCFStringEncodingUTF8,
            false as u8,
        );
        if name.is_null() {
            tracing::warn!("系统明暗监听: 构造通知名失败，「跟随系统」将不会随系统实时切换");
            return;
        }
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            std::ptr::null(), // observer：本模块是进程内单例，不需要句柄来反注册
            on_theme_changed,
            name,
            std::ptr::null(), // object：不限发送者
            // 分发通知默认在进程挂起时合并/丢弃；明暗是「取最新值」语义，
            // 立即投递即可，不需要排队历史。
            CFNotificationSuspensionBehaviorDeliverImmediately,
        );
        // name 由通知中心持有，此处不释放（注册是进程生命期的，无反注册路径）。
        let _ = INSTALLED.set(());
        tracing::info!(
            "系统明暗监听: 已注册 {}",
            String::from_utf8_lossy(NOTIFICATION)
        );
    }
}

/// 观察者回调（主线程）。只发事件，明暗的实际取值由协调器再走一次
/// `system_prefers_dark()` —— 通知本身不带新值。
extern "C" fn on_theme_changed(
    _center: CFNotificationCenterRef,
    _observer: *mut c_void,
    _name: CFNotificationName,
    _object: *const c_void,
    _info: CFDictionaryRef,
) {
    tracing::debug!("系统明暗设置变更 → 通知协调器");
    if let Some(tx) = EV_TX.get() {
        let _ = tx.send(UiEvent::SystemThemeChanged);
    }
}
