//! Named Pipe 安全描述符工具
//!
//! 提供与 Go 版 go-winio 相同的 SDDL 安全描述符，
//! 允许 AppContainer/UWP 进程（如 Windows Store 版记事本）连接管道。

use tracing::error;

/// 共享的 SDDL 安全描述符字符串（与 Go 版 go-winio 的 PipeConfig.SecurityDescriptor 一致）
///
/// - `D:P` — DACL 受保护（禁用继承）
/// - `(A;;GA;;;WD)` — 所有人完全访问
/// - `(A;;GA;;;SY)` — SYSTEM 完全访问
/// - `(A;;GA;;;BA)` — Administrators 完全访问
/// - `(A;;GA;;;AC)` — AppContainer 完全访问（UWP/Store 应用必需）
/// - `S:(ML;;NW;;;LW)` — 低完整性级别（UWP 进程必需）
pub(crate) const SDDL: &str = "D:P(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AC)S:(ML;;NW;;;LW)";

/// SDDL_REVISION_1 = 1
const SDDL_REVISION_1: u32 = 1;

/// 解析后的安全描述符（RAII 封装，析构时释放 LocalAlloc 内存）
pub(crate) struct SecurityDescriptor {
    bytes: Vec<u8>,
}

impl SecurityDescriptor {
    /// 从 SDDL 字符串解析安全描述符
    pub(crate) fn from_sddl(sddl: &str) -> Option<Self> {
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorA;

        let sddl_c = match std::ffi::CString::new(sddl) {
            Ok(s) => s,
            Err(_) => return None,
        };

        let mut psd = windows::Win32::Security::PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        let mut sd_len: u32 = 0;

        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorA(
                windows::core::PCSTR(sddl_c.as_ptr() as *const u8),
                SDDL_REVISION_1,
                &mut psd,
                Some(&mut sd_len),
            )
        };

        if ok.is_err() || psd.0.is_null() {
            error!("Failed to parse SDDL security descriptor");
            return None;
        }

        // 复制到 Vec<u8>，然后释放原始内存
        let bytes = unsafe { std::slice::from_raw_parts(psd.0 as *const u8, sd_len as usize) }.to_vec();
        unsafe {
            windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(psd.0));
        }

        Some(Self { bytes })
    }

    /// 获取安全描述符指针（用于 SECURITY_ATTRIBUTES）
    pub(crate) fn as_ptr(&self) -> *const std::ffi::c_void {
        self.bytes.as_ptr() as *const std::ffi::c_void
    }
}

/// 创建管道安全属性，包含 SDDL 安全描述符
///
/// 返回 SecurityDescriptor。
/// SecurityDescriptor 必须保持存活直到 CreateNamedPipe 调用完成。
pub(crate) fn create_pipe_security_attributes() -> Option<SecurityDescriptor> {
    SecurityDescriptor::from_sddl(SDDL)
}
