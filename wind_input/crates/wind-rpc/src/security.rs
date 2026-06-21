//! Named Pipe 安全描述符工具（Windows）。
//!
//! 与 wind-bridge/security.rs 一致的 SDDL，允许 AppContainer/UWP 进程连接控制/事件管道。
//! 本地管道靠 OS ACL 授权（不再需要 token/Origin/CORS）。

#[cfg(windows)]
use tracing::error;

/// 共享 SDDL 安全描述符（与 wind-bridge / Go go-winio 一致）。
#[cfg(windows)]
pub(crate) const SDDL: &str = "D:P(A;;GA;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;AC)S:(ML;;NW;;;LW)";

#[cfg(windows)]
const SDDL_REVISION_1: u32 = 1;

/// 解析后的安全描述符（RAII：bytes 持有副本，原始 LocalAlloc 内存已释放）。
#[cfg(windows)]
pub(crate) struct SecurityDescriptor {
    bytes: Vec<u8>,
}

#[cfg(windows)]
impl SecurityDescriptor {
    pub(crate) fn from_sddl(sddl: &str) -> Option<Self> {
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorA;

        let sddl_c = std::ffi::CString::new(sddl).ok()?;
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
        let bytes =
            unsafe { std::slice::from_raw_parts(psd.0 as *const u8, sd_len as usize) }.to_vec();
        unsafe {
            windows::Win32::Foundation::LocalFree(windows::Win32::Foundation::HLOCAL(psd.0));
        }
        Some(Self { bytes })
    }

    pub(crate) fn as_ptr(&self) -> *const std::ffi::c_void {
        self.bytes.as_ptr() as *const std::ffi::c_void
    }
}

/// 创建管道安全属性（保持存活直到 CreateNamedPipe 调用完成）。
#[cfg(windows)]
pub(crate) fn create_pipe_security_attributes() -> Option<SecurityDescriptor> {
    SecurityDescriptor::from_sddl(SDDL)
}
