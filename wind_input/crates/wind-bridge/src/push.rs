//! 推送管道服务器
//!
//! 与 Go 版本 `wind_input/internal/bridge/server_push.go` 对齐。
//! 服务端主动推送状态更新、配置同步等消息给 TSF DLL。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};
use wind_ipc::protocol::*;

#[cfg(windows)]
use crate::server::PipeHandle;

/// 推送管道配置
pub struct PushConfig {
    pub suffix: String,
    pub write_timeout_ms: u64,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }
    }
}

/// 客户端连接信息
struct PushClient {
    /// 客户端 token（PID << 32 | instance_counter）
    token: u64,
    /// 发送通道（writer 线程独占管道句柄）
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

/// 推送管道服务器
pub struct PushServer {
    config: PushConfig,
    clients: Arc<Mutex<Vec<PushClient>>>,
    /// 当前活动（有焦点）客户端 token；commit 仅投递给它，避免广播多发
    active_token: Arc<AtomicU64>,
}

impl PushServer {
    pub fn new(config: PushConfig) -> Self {
        Self {
            config,
            clients: Arc::new(Mutex::new(Vec::new())),
            active_token: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 记录活动客户端 token（焦点获取 / IME 激活时调用）
    pub fn set_active_token(&self, token: u64) {
        self.active_token.store(token, Ordering::Relaxed);
    }

    /// 获取推送管道名称
    ///
    /// 必须与 Go/TSF 一致：后缀插在 `wind_input` 与 `_push` 之间。
    /// Go `endpoint_windows.go`: `\\.\pipe\wind_input` + Suffix + `_push`；
    /// TSF `Globals.h` debug 变体: `\\.\pipe\wind_input_debug_push`。
    /// 此前误写成 `wind_input_push{suffix}` (= wind_input_push_debug)，
    /// 导致 TSF 永远连不上 push 管道、收不到热键白名单 → Shift/Ctrl+Shift+E 不被转发。
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\wind_input{}_push", self.config.suffix)
    }

    /// 启动推送管道服务器
    #[cfg(windows)]
    pub async fn start(&self) -> anyhow::Result<()> {
        let pipe_name = self.pipe_name();
        info!("Push server starting on {:?}", pipe_name);

        let clients = self.clients.clone();

        std::thread::Builder::new()
            .name("push-server".into())
            .spawn(move || {
                run_push_pipe_server(&pipe_name, clients);
            })?;

        Ok(())
    }

    #[cfg(not(windows))]
    pub async fn start(&self) -> anyhow::Result<()> {
        warn!("Push pipe server not supported on this platform");
        Ok(())
    }

    /// 向所有连接客户端广播消息（用于状态/激活同步，幂等无副作用）
    pub fn push_to_active(&self, data: &[u8]) {
        let clients = self.clients.lock().unwrap();
        for client in clients.iter() {
            let _ = client.tx.send(data.to_vec());
        }
    }

    /// 仅向活动客户端投递（用于 commit 等带副作用的消息，避免广播导致多次上屏）。
    /// 优先按活动 token 匹配；无匹配且仅一个客户端时兜底发它；否则跳过。
    pub fn push_commit_to_active(&self, data: &[u8]) {
        let active = self.active_token.load(Ordering::Relaxed);
        let clients = self.clients.lock().unwrap();
        if clients.is_empty() {
            return;
        }
        if active != 0 {
            if let Some(c) = clients.iter().find(|c| c.token == active) {
                let _ = c.tx.send(data.to_vec());
                return;
            }
        }
        if clients.len() == 1 {
            let _ = clients[0].tx.send(data.to_vec());
        } else {
            warn!(
                "push_commit: 无匹配活动客户端 (active=0x{:016X}, clients={})，跳过以防多发",
                active,
                clients.len()
            );
        }
    }

    /// 推送 ActivationStatus 给活跃客户端
    ///
    /// 与 Go 版本 `PushActivationStatusToActiveClient` 对齐。
    /// 激活后 TSF DLL 需要收到此消息才能正常工作。
    ///
    /// 注意：此方法使用 CMD_ACTIVATION_STATUS_PUSH 命令码（0x020C），
    /// 与 CMD_STATUS_UPDATE（0x0202）不同。C++ 端对两者有不同处理路径。
    pub fn push_activation_status(&self, chinese_mode: bool) {
        let label = if chinese_mode { "中" } else { "英" };
        let resp = wind_ipc::codec::encode_activation_status_push(
            chinese_mode,
            false, // full_width
            true,  // chinese_punct
            true,  // toolbar_visible
            false, // caps_lock
            false, // host_render_avail
            &[],   // key_down_hotkeys
            &[],   // key_up_hotkeys
            label,
        );
        self.push_to_active(&resp);
    }
}

/// 推送管道服务器主循环
#[cfg(windows)]
fn run_push_pipe_server(pipe_name: &str, clients: Arc<Mutex<Vec<PushClient>>>) {
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
#[cfg(windows)]
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁定 push 管道名格式：后缀必须插在 `wind_input` 与 `_push` 之间，
    /// 与 Go endpoint_windows.go / TSF Globals.h 一致，否则 TSF 连不上 push 管道。
    #[test]
    fn test_push_pipe_name_suffix_position() {
        let debug = PushServer::new(PushConfig {
            suffix: "_debug".into(),
            write_timeout_ms: 30_000,
        });
        assert_eq!(debug.pipe_name(), r"\\.\pipe\wind_input_debug_push");

        let release = PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        });
        assert_eq!(release.pipe_name(), r"\\.\pipe\wind_input_push");
    }
}
