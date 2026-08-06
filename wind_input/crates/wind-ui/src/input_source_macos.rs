//! macOS 输入源切换：`TISSelectInputSource`（对应 Windows 的 `activate_ime` 热键）。
//!
//! # 为什么必须在服务进程里做
//!
//! `activate_ime` 的语义是「本输入法**没被激活**时也能一键切过来」。此刻 `.app` 通常
//! 根本没在跑（IMKit 按需拉起），推 IPC 给它是推给空气。所以选中动作只能由常驻的服务
//! 进程发起——热键注册（`global_hotkey_macos`）本来也在这个进程，两者天然同处。
//!
//! # 与 Windows 的语义差异（不可消除）
//!
//! Windows 走 `HKCU\...\CTF\DirectSwitchHotkeys`，由 ctfmon 原生处理，效果是**per-app**
//! 切换（只改当前前台应用的输入法）。macOS 的 `TISSelectInputSource` 是**全局**切换：
//! 系统层面并无「只改这个 app 的输入源」的公开 API，per-app 记忆归系统偏好
//! 「自动切换到文稿的输入源」管，第三方插不进去。
//!
//! 因此两平台在这一项上行为不同，且无法对齐——设置界面的说明文案需按平台区分。
//!
//! # 线程
//!
//! 与 Carbon 热键不同，TIS 系列没有主线程亲和要求（squirrel / rime 亦从任意线程调用）。
//! 本模块被热键回调（主线程）与协调器线程共同调用，故不持有任何状态。

use core_foundation_sys::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation_sys::base::{CFRelease, CFTypeRef, kCFAllocatorDefault};
use core_foundation_sys::dictionary::{
    CFDictionaryCreate, CFDictionaryRef, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks,
};
use core_foundation_sys::string::{CFStringCreateWithBytes, CFStringRef, kCFStringEncodingUTF8};
use std::ffi::c_void;
use tracing::{info, warn};

type OSStatus = i32;
type TISInputSourceRef = *mut c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// 按属性过滤枚举输入源。`include_all_installed=false` 只返回**已启用**的源
    /// （未启用的源选不中，拿回来也没用）。返回值 +1，调用方负责 `CFRelease`。
    fn TISCreateInputSourceList(
        properties: CFDictionaryRef,
        include_all_installed: u8,
    ) -> CFArrayRef;
    /// 选中输入源（全局生效）。源未启用 / 不可选时返回非 0。
    fn TISSelectInputSource(source: TISInputSourceRef) -> OSStatus;
    static kTISPropertyInputSourceID: CFStringRef;
}

/// 本变体的 TIS input mode id。
///
/// `.app` 的 `CFBundleIdentifier` 是 `to.feng.inputmethod.WindInput{Dev}`，其
/// `tsInputModeListKey` 下的 mode 名固定为 bundleID + `.mode`（见
/// `wind_macos/.../Info.plist`；dev 变体由 `scripts/mac/dev.sh` 整体改写 bundleID 串）。
fn mode_id() -> String {
    if wind_config::variant::is_dev() {
        "to.feng.inputmethod.WindInputDev.mode".to_string()
    } else {
        "to.feng.inputmethod.WindInput.mode".to_string()
    }
}

/// 造一个 CFString（+1，调用方负责释放）。空指针表示创建失败。
unsafe fn cfstr(s: &str) -> CFStringRef {
    unsafe {
        CFStringCreateWithBytes(
            kCFAllocatorDefault,
            s.as_ptr(),
            s.len() as isize,
            kCFStringEncodingUTF8,
            false as u8,
        )
    }
}

/// 把系统当前输入源切换到本输入法。返回是否切换成功。
///
/// 失败只记日志不 panic：热键随时可能在输入源尚未启用 / 尚未注册的状态下被按到，
/// 那属于「用户还没装好」而不是程序错误。
pub fn select_self() -> bool {
    let id = mode_id();
    unsafe {
        let key = kTISPropertyInputSourceID;
        let value = cfstr(&id);
        if value.is_null() {
            warn!("activate_ime: 构造 input source id 字符串失败");
            return false;
        }
        // 过滤字典 {kTISPropertyInputSourceID: <mode id>}：比拉全表再逐个比字符串少一大截
        // FFI，且把「怎么算匹配」交给系统自己判断。
        let keys = [key as *const c_void];
        let values = [value as *const c_void];
        let filter = CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        CFRelease(value as CFTypeRef); // 字典已 retain
        if filter.is_null() {
            warn!("activate_ime: 构造输入源过滤字典失败");
            return false;
        }

        let list = TISCreateInputSourceList(filter, false as u8);
        CFRelease(filter as CFTypeRef);
        if list.is_null() || CFArrayGetCount(list) == 0 {
            if !list.is_null() {
                CFRelease(list as CFTypeRef);
            }
            // 最常见的成因：输入法装了但没在「键盘 › 输入法」里勾选启用。
            warn!("activate_ime: 未找到已启用的输入源 {id}（是否尚未在系统设置中添加？）");
            return false;
        }
        let src = CFArrayGetValueAtIndex(list, 0) as TISInputSourceRef;
        let st = TISSelectInputSource(src);
        CFRelease(list as CFTypeRef);
        if st == 0 {
            info!("activate_ime: 已切换到 {id}");
            true
        } else {
            warn!("activate_ime: TISSelectInputSource({id}) 失败 OSStatus={st}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_id_matches_bundle_convention() {
        // 变体后缀由 WIND_VARIANT / 可执行名决定，测试进程两者都不是 dev。
        assert_eq!(mode_id(), "to.feng.inputmethod.WindInput.mode");
    }

    // ⚠ 刻意**不**给 select_self() 写单测：它会真的切换执行者当前的系统输入源。
    // 在开发机上跑一次 `cargo test` 就把人家正在用的输入法换掉，这个副作用不可接受。
    // 验证走真机手动按热键。
}
