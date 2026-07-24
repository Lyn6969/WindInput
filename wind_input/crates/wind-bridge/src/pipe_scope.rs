//! 命名管道的 per-user 作用域后缀。
//!
//! 命名管道的名字空间是**机器级**的（不像互斥体有 `Local\` 每会话前缀），要按用户
//! 隔离只能把用户身份编进名字里。用**当前用户 SID 字符串**：
//! - SID 恒为 ASCII（`S-1-5-21-...`），规避 `CreateNamedPipeA`(Rust 服务端) 与
//!   `CreateFileW`(C++ TSF 客户端) 的 A/W 编码分歧，也避免中文等非 ASCII 用户名撞坏管道名；
//! - 作为**扁平后缀**（`wind_input_dev_S-1-...`）拼接，不引入 `\` 路径段——AppContainer
//!   宿主对带目录前缀的管道名可能打不开（见 wind_tsf `Globals.h` 的注释）。
//!
//! C++ 侧 wind_tsf 用同一 OS API（`ConvertSidToStringSidW`）算出**同一字符串**，两端才
//! 能在同名管道上会合。任一侧取 SID 失败都对称回退到无后缀裸名（取自己进程令牌的 SID
//! 实际上不会失败）。互斥体不需要它：`Local\` 命名空间已按会话隔离。

/// per-user 后缀：`"_S-1-5-21-..."`；取不到 SID 或非 Windows 返回空串。
#[cfg(windows)]
pub fn user_scope_suffix() -> String {
    current_user_sid()
        .map(|sid| format!("_{sid}"))
        .unwrap_or_default()
}

#[cfg(not(windows))]
pub fn user_scope_suffix() -> String {
    String::new()
}

/// 当前进程令牌的用户 SID 字符串（`S-1-5-...`）。
#[cfg(windows)]
fn current_user_sid() -> Option<String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::core::PWSTR;

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return None;
        }
        struct Guard(HANDLE);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
        let _guard = Guard(token);

        // 先探长度再取 TOKEN_USER。
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut _),
            len,
            &mut len,
        )
        .ok()?;
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);

        let mut pstr = PWSTR::null();
        ConvertSidToStringSidW(tu.User.Sid, &mut pstr).ok()?;
        if pstr.is_null() {
            return None;
        }
        let s = pstr.to_string().ok();
        let _ = LocalFree(HLOCAL(pstr.0 as *mut _));
        s
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_suffix_is_sid_shaped() {
        // 真机（Windows）上取当前用户 SID：应是 "_S-1-..." 形。
        let s = user_scope_suffix();
        assert!(s.starts_with("_S-"), "per-user 后缀应为 SID 形，实得 {s:?}");
    }
}
