//! Windows 命名 Event（auto-reset）写端
//!
//! 供服务侧在 SHM 写帧后 SetEvent 通知 TSF DLL 侧刷新。
//! DLL 侧用 OpenEventW 以 SYNCHRONIZE 权限打开，WaitForSingleObject 等待。

#![cfg(windows)]

use std::{io, mem};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// 将 UTF-8 字符串转为以 NUL 结尾的 UTF-16 向量
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows 命名 Event（auto-reset, initial=false）
///
/// - 创建时附加与 bridge pipe 相同的 SDDL（AppContainer 可 OpenEventW）。
/// - `signal()` = `SetEvent`，auto-reset 语义：第一个等待者消费后自动复位。
pub struct NamedEvent {
    handle: HANDLE,
    name: String,
}

// SAFETY: HANDLE 线程间传递安全（SetEvent/CloseHandle 自身是线程安全的）
unsafe impl Send for NamedEvent {}

impl NamedEvent {
    /// 创建命名 Event（auto-reset, initial=false，带 AppContainer SDDL）。
    ///
    /// 名称格式应为 `"Local\\WindXxx"` 或 `"Global\\WindXxx"`。
    pub fn create(name: &str) -> io::Result<Self> {
        let sd = crate::security::create_pipe_security_attributes();
        let sa = sd.as_ref().map(|s| SECURITY_ATTRIBUTES {
            nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: s.as_ptr() as *mut _,
            bInheritHandle: false.into(),
        });

        let name_w = to_wide(name);

        let handle = unsafe {
            CreateEventW(
                sa.as_ref().map(|s| s as *const _),
                false, // bManualReset=false → auto-reset
                false, // bInitialState=false → 初始未置信号
                windows::core::PCWSTR(name_w.as_ptr()),
            )
        }
        .map_err(|_| io::Error::last_os_error())?;

        Ok(Self { handle, name: name.to_owned() })
    }

    /// 置信号（SetEvent）
    pub fn signal(&self) {
        unsafe {
            let _ = SetEvent(self.handle);
        }
    }

    /// 返回创建时的名称
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for NamedEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
