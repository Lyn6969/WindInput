//! Windows Named Pipe 请求-响应服务器
//!
//! 从 `server.rs` 剪切迁移，保持原有逻辑不变。
//! 与 Go 版本 `wind_input/internal/bridge/server.go` 对齐。

use crate::handler::*;
use crate::server::dispatch_command;
use std::sync::Arc;
use tracing::{debug, error, info, warn};
use wind_ipc::codec::*;
use wind_ipc::protocol::*;

/// 包装 HANDLE 使其可跨线程传递
pub(crate) struct PipeHandle(pub(crate) windows::Win32::Foundation::HANDLE);

unsafe impl Send for PipeHandle {}
unsafe impl Sync for PipeHandle {}

/// Windows Named Pipe 服务器主循环
pub fn run_pipe_server(pipe_name: &str, handler: Arc<dyn MessageHandler>, timeout_ms: u64) {
    use std::ffi::CString;
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Pipes::*;

    let pipe_name_c = match CString::new(pipe_name) {
        Ok(s) => s,
        Err(e) => {
            error!("Invalid pipe name: {}", e);
            return;
        }
    };

    // 解析 SDDL 安全描述符，允许 AppContainer/UWP 进程连接
    let sd = crate::security::create_pipe_security_attributes();

    // 构建 SECURITY_ATTRIBUTES（sd 保持存活直到函数结束）
    let sa = sd.as_ref().map(|s| {
        use windows::Win32::Security::SECURITY_ATTRIBUTES;
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: s.as_ptr() as *mut _,
            bInheritHandle: false.into(),
        }
    });

    loop {
        // 创建 Named Pipe 实例
        let pipe_handle = unsafe {
            CreateNamedPipeA(
                windows::core::PCSTR(pipe_name_c.as_ptr() as *const u8),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536, // out buffer
                65536, // in buffer
                0,     // default timeout
                sa.as_ref().map(|s| s as *const _),
            )
        };

        let pipe_handle = match pipe_handle {
            Ok(h) => h,
            Err(e) => {
                error!("CreateNamedPipe failed: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        // 等待客户端连接
        let connected = unsafe { ConnectNamedPipe(pipe_handle, None) };
        if connected.is_err() {
            // ERROR_PIPE_CONNECTED = 客户端已连接
            let err = windows::core::Error::from_win32();
            if err.code() != ERROR_PIPE_CONNECTED.into() {
                warn!("ConnectNamedPipe failed: {}", err);
                unsafe {
                    let _ = CloseHandle(pipe_handle);
                }
                continue;
            }
        }

        info!("Client connected to bridge pipe");

        // 为每个连接启动独立线程
        let handler = handler.clone();
        let pipe = PipeHandle(pipe_handle);
        std::thread::Builder::new()
            .name("bridge-client".into())
            .spawn(move || {
                handle_client(pipe, handler, timeout_ms);
            })
            .ok();
    }
}

/// 处理单个客户端连接
fn handle_client(pipe: PipeHandle, handler: Arc<dyn MessageHandler>, _timeout_ms: u64) {
    use windows::Win32::Foundation::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Pipes::*;

    let pipe = pipe.0;
    let mut header_buf = [0u8; IpcHeader::SIZE];
    let mut payload_buf = vec![0u8; 65536];

    loop {
        // 读取 8 字节 header
        let mut bytes_read: u32 = 0;
        let read_ok = unsafe { ReadFile(pipe, Some(&mut header_buf), Some(&mut bytes_read), None) };

        if read_ok.is_err() {
            // 检查是否是 ERROR_MORE_DATA（消息模式下消息比缓冲区大时的正常情况）
            let last_err = unsafe { windows::Win32::Foundation::GetLastError() };
            if last_err == windows::Win32::Foundation::ERROR_MORE_DATA
                && bytes_read as usize == IpcHeader::SIZE
            {
                // 读到了完整的 header，继续处理（payload 会在后续读取）
                info!(
                    "ReadFile returned ERROR_MORE_DATA but got full header ({} bytes), continuing",
                    bytes_read
                );
            } else {
                info!(
                    "Client disconnected from bridge pipe (read failed, bytes_read={}, last_err={:?})",
                    bytes_read, last_err
                );
                break;
            }
        }

        if bytes_read as usize != IpcHeader::SIZE {
            info!(
                "Client disconnected from bridge pipe (incomplete header: {} bytes)",
                bytes_read
            );
            break;
        }

        let header = match decode_header(&header_buf) {
            Ok(h) => h,
            Err(e) => {
                warn!("Invalid header: {}", e);
                break;
            }
        };

        let cmd = header.command;
        let len = header.length;
        info!(
            "Received command: 0x{:04X}, payload: {} bytes, async: {}",
            cmd,
            len,
            header.is_async()
        );

        // 读取 payload（如果有）
        let payload_len = header.length as usize;
        let payload = if payload_len > 0 {
            if payload_len > payload_buf.len() {
                payload_buf.resize(payload_len, 0);
            }
            let mut bytes_read: u32 = 0;
            let read_ok = unsafe {
                ReadFile(
                    pipe,
                    Some(&mut payload_buf[..payload_len]),
                    Some(&mut bytes_read),
                    None,
                )
            };
            if read_ok.is_err() {
                let last_err = unsafe { windows::Win32::Foundation::GetLastError() };
                if last_err == windows::Win32::Foundation::ERROR_MORE_DATA {
                    // ERROR_MORE_DATA 表示消息比请求的字节数多，但已读到请求的字节数
                    info!(
                        "ReadFile payload: ERROR_MORE_DATA but got {} bytes (requested {})",
                        bytes_read, payload_len
                    );
                } else {
                    warn!(
                        "Failed to read payload ({} bytes, read={}, err={:?})",
                        payload_len, bytes_read, last_err
                    );
                    break;
                }
            }
            if (bytes_read as usize) < payload_len {
                warn!(
                    "Incomplete payload: got {} of {} bytes",
                    bytes_read, payload_len
                );
                break;
            }
            &payload_buf[..payload_len]
        } else {
            &[]
        };

        // 分发命令到处理器
        let response = crate::server::dispatch_command(&handler, header.command, header.is_async(), payload);

        // 写入响应（异步命令返回 None，不写入）
        if let Some(resp) = response {
            info!(
                "Sending response: {} bytes for cmd 0x{:04X}",
                resp.len(),
                cmd
            );
            let mut bytes_written: u32 = 0;
            let write_ok = unsafe { WriteFile(pipe, Some(&resp), Some(&mut bytes_written), None) };
            if write_ok.is_err() {
                warn!("Failed to write response for cmd 0x{:04X}", cmd);
                break;
            }
            info!("Response sent: {} bytes written", bytes_written);
        } else {
            info!("No response for cmd 0x{:04X} (async)", cmd);
        }

        // FOCUS_GAINED 重型段延后到响应写出之后（对齐 Go runActivationHandlerAndPush）：
        // 同步段已回 ModePush 解除 DLL 阻塞，此处再 build_status + push 完整激活状态
        // （工具栏/热键/图标/active token），不占用 DLL 的同步等待窗口。
        if cmd == CMD_FOCUS_GAINED
            && let Ok(fg) = decode_focus_gained(payload)
        {
            let data = FocusData {
                x: fg.caret.x,
                y: fg.caret.y,
                height: fg.caret.height,
                composition_start_x: fg.caret.composition_start_x,
                composition_start_y: fg.caret.composition_start_y,
                client_token: fg.client_token,
                input_scope_mask: fg.input_scope_mask,
            };
            handler.handle_focus_gained(&data);
        }
    }

    unsafe {
        windows::Win32::System::Pipes::DisconnectNamedPipe(pipe);
        let _ = CloseHandle(pipe);
    }
    debug!("Client disconnected from bridge pipe");
}
