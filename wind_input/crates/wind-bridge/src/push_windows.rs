//! Windows 命名管道推送服务器
//!
//! 从 `push.rs` 剪切，仅 Windows 平台编译。
//! 与 Go 版本 `internal/bridge/server_push.go` 对齐。

use super::push::PushClient;
use crate::server::PipeHandle;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use wind_ipc::protocol::*;

/// 推送管道服务器主循环
pub(crate) fn run_push_pipe_server(pipe_name: &str, clients: Arc<Mutex<Vec<PushClient>>>) {
    use std::ffi::CString;
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Pipes::*;

    let pipe_name_c = match CString::new(pipe_name) {
        Ok(s) => s,
        Err(e) => {
            error!("Invalid push pipe name: {}", e);
            return;
        }
    };

    // 解析 SDDL 安全描述符，允许 AppContainer/UWP 进程连接
    let sd = crate::security::create_pipe_security_attributes();
    let sa = sd.as_ref().map(|s| {
        use windows::Win32::Security::SECURITY_ATTRIBUTES;
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: s.as_ptr() as *mut _,
            bInheritHandle: false.into(),
        }
    });

    loop {
        let pipe_handle = unsafe {
            CreateNamedPipeA(
                windows::core::PCSTR(pipe_name_c.as_ptr() as *const u8),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                sa.as_ref().map(|s| s as *const _),
            )
        };

        let pipe_handle = match pipe_handle {
            Ok(h) => h,
            Err(e) => {
                error!("CreateNamedPipe (push) failed: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        let connected = unsafe { ConnectNamedPipe(pipe_handle, None) };
        if connected.is_err() {
            let err = windows::core::Error::from_win32();
            if err.code() != ERROR_PIPE_CONNECTED.into() {
                warn!("ConnectNamedPipe (push) failed: {}", err);
                unsafe {
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }
        }

        info!("Push client connected to push pipe");

        // 与 Go 版对齐：先发送 CMD_SERVICE_READY，再读取 token。
        // Go 的 push pipe 在 ConnectNamedPipe 后立即写 SERVICE_READY，
        // C++ 端 AsyncReader 收到后触发 _DoFullStateSync(WM_SERVICE_READY)。
        let ready_msg = IpcHeader::new(CMD_SERVICE_READY, 0).to_bytes().to_vec();
        {
            let mut bytes_written: u32 = 0;
            let write_ok = unsafe {
                WriteFile(
                    pipe_handle,
                    Some(&ready_msg),
                    Some(&mut bytes_written),
                    None,
                )
            };
            if write_ok.is_err() {
                warn!("Failed to send SERVICE_READY to push client");
                unsafe {
                    DisconnectNamedPipe(pipe_handle);
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }
        }
        debug!("Sent SERVICE_READY to push client");

        // 读取客户端 token（8 字节）
        let mut token_buf = [0u8; 8];
        let mut bytes_read: u32 = 0;
        let read_ok = unsafe {
            ReadFile(
                pipe_handle,
                Some(&mut token_buf),
                Some(&mut bytes_read),
                None,
            )
        };

        if read_ok.is_err() || bytes_read != 8 {
            warn!("Failed to read push client token");
            unsafe {
                DisconnectNamedPipe(pipe_handle);
                let _ = CloseHandle(pipe_handle);
            }
            continue;
        }

        let token = u64::from_le_bytes(token_buf);
        info!("Push client token: 0x{:016X}", token);

        // 创建发送通道
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();

        // 注册客户端（不持有 pipe handle，writer 线程独占）
        let client = PushClient { token, tx };

        {
            let mut clients = clients.lock().unwrap();
            // 清理同 token 的旧连接
            clients.retain(|c| c.token != token);
            clients.push(client);
        }

        // 将 pipe handle 移交给 writer 线程（包装为 PipeHandle 以满足 Send）
        let clients_clone = clients.clone();
        let pipe = PipeHandle(pipe_handle);
        std::thread::Builder::new()
            .name("push-writer".into())
            .spawn(move || {
                push_writer_loop(pipe, rx, token, clients_clone);
            })
            .ok();
    }
}

/// 推送写入循环
fn push_writer_loop(
    pipe: PipeHandle,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    token: u64,
    clients: Arc<Mutex<Vec<PushClient>>>,
) {
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;

    let pipe = pipe.0;
    loop {
        match rx.recv() {
            Ok(data) => {
                let mut bytes_written: u32 = 0;
                let write_ok =
                    unsafe { WriteFile(pipe, Some(&data), Some(&mut bytes_written), None) };
                if write_ok.is_err() {
                    debug!("Push client 0x{:016X} write failed, removing", token);
                    break;
                }
            }
            Err(_) => {
                break;
            }
        }
    }

    unsafe {
        windows::Win32::System::Pipes::DisconnectNamedPipe(pipe);
        let _ = CloseHandle(pipe);
    }

    let mut clients = clients.lock().unwrap();
    clients.retain(|c| c.token != token);
    debug!("Push client 0x{:016X} disconnected", token);
}
