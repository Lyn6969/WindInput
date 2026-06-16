//! Named Pipe 请求-响应服务器
//!
//! 与 Go 版本 `wind_input/internal/bridge/server.go` 对齐。
//! 每个客户端连接在独立线程中处理（对应 Go 的 goroutine）。

use crate::handler::*;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

/// Bridge 服务器配置
pub struct BridgeConfig {
    /// 管道名称后缀（构建变体）
    pub suffix: String,
    /// 请求处理超时（毫秒）
    pub request_timeout_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            suffix: String::new(),
            request_timeout_ms: 1000,
        }
    }
}

/// Bridge 服务器
pub struct BridgeServer {
    config: BridgeConfig,
    handler: Arc<dyn MessageHandler>,
}

impl BridgeServer {
    pub fn new(config: BridgeConfig, handler: Arc<dyn MessageHandler>) -> Self {
        Self { config, handler }
    }

    /// 获取管道名称
    pub fn pipe_name(&self) -> String {
        format!(r"\\.\pipe\wind_input{}", self.config.suffix)
    }

    /// 启动 Named Pipe 服务器（Windows）
    #[cfg(windows)]
    pub async fn start(&self) -> anyhow::Result<()> {
        let pipe_name = self.pipe_name();
        info!("Bridge server starting on {:?}", pipe_name);

        let handler = self.handler.clone();
        let timeout_ms = self.config.request_timeout_ms;

        // 在独立线程中运行阻塞的 Named Pipe 循环
        std::thread::Builder::new()
            .name("bridge-server".into())
            .spawn(move || {
                run_pipe_server(&pipe_name, handler, timeout_ms);
            })?;

        Ok(())
    }

    /// 启动 Named Pipe 服务器（非 Windows 平台的占位实现）
    #[cfg(not(windows))]
    pub async fn start(&self) -> anyhow::Result<()> {
        warn!("Named Pipe server not supported on this platform");
        Ok(())
    }
}

/// 包装 HANDLE 使其可跨线程传递
#[cfg(windows)]
pub(crate) struct PipeHandle(pub(crate) windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for PipeHandle {}
#[cfg(windows)]
unsafe impl Sync for PipeHandle {}

/// Windows Named Pipe 服务器主循环
#[cfg(windows)]
fn run_pipe_server(pipe_name: &str, handler: Arc<dyn MessageHandler>, timeout_ms: u64) {
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
#[cfg(windows)]
fn handle_client(pipe: PipeHandle, handler: Arc<dyn MessageHandler>, _timeout_ms: u64) {
    use wind_ipc::codec::*;
    use wind_ipc::protocol::*;
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
            if last_err == windows::Win32::Foundation::ERROR_MORE_DATA && bytes_read as usize == IpcHeader::SIZE {
                // 读到了完整的 header，继续处理（payload 会在后续读取）
                info!("ReadFile returned ERROR_MORE_DATA but got full header ({} bytes), continuing", bytes_read);
            } else {
                info!("Client disconnected from bridge pipe (read failed, bytes_read={}, last_err={:?})", bytes_read, last_err);
                break;
            }
        }

        if bytes_read as usize != IpcHeader::SIZE {
            info!("Client disconnected from bridge pipe (incomplete header: {} bytes)", bytes_read);
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
        info!("Received command: 0x{:04X}, payload: {} bytes, async: {}", cmd, len, header.is_async());

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
                    info!("ReadFile payload: ERROR_MORE_DATA but got {} bytes (requested {})", bytes_read, payload_len);
                } else {
                    warn!("Failed to read payload ({} bytes, read={}, err={:?})", payload_len, bytes_read, last_err);
                    break;
                }
            }
            if (bytes_read as usize) < payload_len {
                warn!("Incomplete payload: got {} of {} bytes", bytes_read, payload_len);
                break;
            }
            &payload_buf[..payload_len]
        } else {
            &[]
        };

        // 分发命令到处理器
        let response = dispatch_command(&handler, header.command, header.is_async(), payload);

        // 写入响应（异步命令返回 None，不写入）
        if let Some(resp) = response {
            info!("Sending response: {} bytes for cmd 0x{:04X}", resp.len(), cmd);
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

/// 分发命令到处理器，返回响应字节
///
/// 与 Go 版 `processRequest` 对齐：
/// - 同步命令返回 Some(response_bytes)
/// - 异步命令返回 None（不写响应）
/// - FOCUS_GAINED：同步命令，回 CMD_MODE_PUSH（权威 chinese/full）；重型 push 延后到
///   handle_client 写出响应之后（见该处）。IME_ACTIVATED 仍异步，状态由 handler 经 push pipe 回送。
fn dispatch_command(
    handler: &Arc<dyn MessageHandler>,
    command: u16,
    is_async: bool,
    payload: &[u8],
) -> Option<Vec<u8>> {
    use wind_ipc::codec::*;
    use wind_ipc::protocol::*;

    match command {
        // ── 按键事件（同步） ──
        CMD_KEY_EVENT => {
            let key_payload = match decode_key_payload(payload) {
                Ok(p) => p,
                Err(e) => {
                    warn!("Invalid key payload: {}", e);
                    return Some(encode_pass_through());
                }
            };
            let data = KeyEventData::from(&key_payload);
            let action = handler.handle_key_event(&data);
            Some(encode_key_action(&action))
        }

        // ── 提交请求（同步，barrier 机制） ──
        CMD_COMMIT_REQUEST => {
            match decode_commit_request(payload) {
                Ok(req) => {
                    let data = CommitRequestData {
                        barrier_seq: req.barrier_seq,
                        trigger_key: req.trigger_key,
                        modifiers: req.modifiers,
                        input_buffer: req.input_buffer,
                    };
                    match handler.handle_commit_request(&data) {
                        Some(result) => Some(encode_commit_result(
                            result.barrier_seq,
                            &result.text,
                            if result.new_composition.is_empty() {
                                None
                            } else {
                                Some(&result.new_composition)
                            },
                            result.mode_changed,
                            result.chinese_mode,
                        )),
                        None => Some(encode_ack()),
                    }
                }
                Err(e) => {
                    warn!("Invalid commit request payload: {}", e);
                    Some(encode_ack())
                }
            }
        }

        // ── 焦点获取（同步命令，对齐 Go fix(focus) 0acf860b） ──
        // 两段式：本同步段只做纯内存轻量操作并**立即回 CMD_MODE_PUSH**（权威 chinese/full）：
        //   DLL 现为同步发送，在 OnSetFocus 内阻塞等本响应，首键前写好 _bChineseMode，
        //   根治"切到微信首键上屏英文"；并解除阻塞（旧实现返回 None → DLL 卡到超时，
        //   表现为切应用卡顿）。重型 handle_focus_gained（build_status + push 完整激活状态）
        //   延后到 handle_client 写出响应之后再跑，不在 DLL 阻塞路径上（见 handle_client）。
        CMD_FOCUS_GAINED => {
            if let Ok(fg) = decode_focus_gained(payload) {
                // 同步 caret（首键前必须就绪，纯字段写入，对齐 Go applyFocusGainedCaret）
                handler.handle_caret_update(&CaretData {
                    x: fg.caret.x,
                    y: fg.caret.y,
                    height: fg.caret.height,
                    composition_start_x: fg.caret.composition_start_x,
                    composition_start_y: fg.caret.composition_start_y,
                });
            }
            // 新 DLL 同步发送（is_async=false）：回传权威模式解除其阻塞并消除首键竞态。
            // 旧 DLL fire-and-forget（is_async=true）：不读响应，回了反而污染管道 → 返回 None。
            // 无论哪种，重型 handle_focus_gained 都在 handle_client 写出响应后统一触发。
            if is_async {
                None
            } else {
                let (chinese_mode, full_width) = handler.get_current_mode();
                Some(encode_mode_push(chinese_mode, full_width))
            }
        }

        // ── 焦点丢失（异步） ──
        CMD_FOCUS_LOST => {
            handler.handle_focus_lost();
            None
        }

        // ── IME 激活（异步） ──
        // Go 的两阶段模式：Phase1 更新 activeProcessID/activeToken + 回 Ack，
        // Phase2 调用 HandleIMEActivated 并推送 ActivationStatusPush。
        CMD_IME_ACTIVATED => {
            let token = if payload.len() >= 8 {
                u64::from_le_bytes(payload[..8].try_into().unwrap())
            } else {
                0
            };
            // handler 内部完成 activation + push ActivationStatusPush
            handler.handle_ime_activated(token);
            None // 异步命令不返回响应
        }

        // ── IME 停用（异步） ──
        CMD_IME_DEACTIVATED => {
            handler.handle_ime_deactivated();
            None
        }

        // ── 模式通知（异步） ──
        CMD_MODE_NOTIFY => {
            let flags = if payload.len() >= 4 {
                u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else {
                0
            };
            handler.handle_mode_notify(flags);
            None
        }

        // ── 模式切换（同步） ──
        // Go: 返回 CommitText（有待提交文本时）或 StatusUpdate（含完整状态）
        CMD_TOGGLE_MODE => {
            let (status, commit_text) = handler.handle_toggle_mode();
            if !commit_text.is_empty() {
                let chinese_mode = status.as_ref().map_or(false, |s| s.chinese_mode);
                Some(encode_commit_text(&commit_text, None, true, chinese_mode, false))
            } else if let Some(status) = status {
                Some(encode_status_update_from_data(&status))
            } else {
                Some(encode_ack())
            }
        }

        // ── 系统模式切换（同步） ──
        // Go: 解析 flags 中的 StatusChineseMode 位（0x0001）
        CMD_SYSTEM_MODE_SWITCH => {
            let flags = if payload.len() >= 4 {
                u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]])
            } else {
                0
            };
            let chinese_mode = (flags & STATUS_CHINESE_MODE) != 0;
            let (status, commit_text) = handler.handle_system_mode_switch(chinese_mode);
            if !commit_text.is_empty() {
                Some(encode_commit_text(&commit_text, None, true, chinese_mode, false))
            } else if let Some(status) = status {
                Some(encode_status_update_from_data(&status))
            } else {
                Some(encode_ack())
            }
        }

        // ── 菜单命令（同步） ──
        // Go: 返回 StatusUpdate（含完整状态）或 Ack
        CMD_MENU_COMMAND => {
            let command = std::str::from_utf8(payload).unwrap_or("");
            match handler.handle_menu_command(command) {
                Some(status) => Some(encode_status_update_from_data(&status)),
                None => Some(encode_ack()),
            }
        }

        // ── 组合终止（异步） ──
        CMD_COMPOSITION_TERMINATED => {
            handler.handle_composition_terminated();
            None
        }

        // ── Host Render 请求（同步） ──
        // TODO: 返回 EncodeHostRenderSetup（共享内存名称+事件名），当前仅 ACK
        CMD_HOST_RENDER_REQUEST => {
            handler.handle_host_render_request();
            handler.handle_host_render_ready();
            Some(encode_ack())
        }

        // ── 光标更新（异步） ──
        CMD_CARET_UPDATE => {
            if let Ok(caret) = wind_ipc::codec::decode_focus_gained(payload)
                .map(|fg| fg.caret)
                .or_else(|_| {
                    // CaretPayload 20 bytes
                    if payload.len() >= 20 {
                        Ok(wind_ipc::protocol::CaretPayload::from_bytes(payload)
                            .unwrap_or(wind_ipc::protocol::CaretPayload {
                                x: 0, y: 0, height: 0,
                                composition_start_x: 0, composition_start_y: 0,
                            }))
                    } else {
                        Err(wind_ipc::codec::CodecError::BufferTooShort { need: 20, got: payload.len() })
                    }
                })
            {
                handler.handle_caret_update(&CaretData {
                    x: caret.x,
                    y: caret.y,
                    height: caret.height,
                    composition_start_x: caret.composition_start_x,
                    composition_start_y: caret.composition_start_y,
                });
            }
            None
        }

        // ── 选区变化（异步） ──
        CMD_SELECTION_CHANGED => {
            let prev_char = if payload.len() >= 2 {
                u16::from_le_bytes([payload[0], payload[1]])
            } else {
                0
            };
            handler.handle_selection_changed(prev_char);
            None
        }

        // ── 光标待定（异步） ──
        CMD_CARET_PENDING => {
            handler.handle_caret_pending();
            None
        }

        // ── 显示功能主菜单（任务栏输入法指示右键，同步）──
        CMD_SHOW_CONTEXT_MENU => {
            // 载荷若含 8 字节则为屏幕坐标 (i32 x, i32 y)，否则用哨兵让 UI 取光标位
            let (x, y) = if payload.len() >= 8 {
                (
                    i32::from_le_bytes(payload[0..4].try_into().unwrap()),
                    i32::from_le_bytes(payload[4..8].try_into().unwrap()),
                )
            } else {
                (i32::MIN, i32::MIN)
            };
            handler.handle_show_context_menu(x, y);
            Some(encode_ack())
        }

        // ── 批处理事件 ──
        CMD_BATCH_EVENTS => handle_batch_events(handler, payload),

        // ── 输入统计（异步） ──
        CMD_INPUT_STATS => {
            None
        }

        _ => {
            warn!("Unknown command: 0x{:04X}", command);
            if is_async {
                None
            } else {
                Some(encode_pass_through())
            }
        }
    }
}

/// 处理批处理事件
fn handle_batch_events(handler: &Arc<dyn MessageHandler>, payload: &[u8]) -> Option<Vec<u8>> {
    use wind_ipc::codec::*;
    use wind_ipc::protocol::*;

    if payload.len() < 4 {
        return Some(encode_ack());
    }

    let event_count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    let mut offset = 4; // skip BatchHeader (eventCount:u16 + reserved:u16)
    let mut responses = Vec::with_capacity(event_count);

    for _ in 0..event_count {
        if offset + IpcHeader::SIZE > payload.len() {
            break;
        }
        let sub_header = match decode_header(&payload[offset..]) {
            Ok(h) => h,
            Err(_) => break,
        };
        offset += IpcHeader::SIZE;

        let sub_payload_len = sub_header.length as usize;
        if offset + sub_payload_len > payload.len() {
            break;
        }
        let sub_payload = &payload[offset..offset + sub_payload_len];
        offset += sub_payload_len;

        // Go: 只收集同步命令的响应
        if !sub_header.is_async() {
            if let Some(resp) = dispatch_command(
                handler,
                sub_header.command,
                sub_header.is_async(),
                sub_payload,
            ) {
                responses.push(resp);
            }
        } else {
            // 异步命令仍需分发（如 caret update），但不收集响应
            dispatch_command(
                handler,
                sub_header.command,
                sub_header.is_async(),
                sub_payload,
            );
        }
    }

    Some(encode_batch_response(&responses))
}

/// 将 KeyAction 编码为响应字节
///
/// 与 Go 版 handleKeyEvent 的 switch 分支对齐
fn encode_key_action(action: &KeyAction) -> Vec<u8> {
    use wind_ipc::codec::*;

    match action {
        KeyAction::InsertText {
            text,
            new_composition,
            mode_changed,
            chinese_mode,
            has_new_composition,
        } => encode_commit_text(
            text,
            new_composition.as_deref(),
            *mode_changed,
            *chinese_mode,
            *has_new_composition,
        ),
        KeyAction::UpdateComposition { text, caret_pos } => {
            encode_update_composition(text, *caret_pos)
        }
        KeyAction::ClearComposition => encode_clear_composition(),
        KeyAction::PassThrough | KeyAction::NotHandled => encode_pass_through(),
        KeyAction::StatusUpdate(status) => {
            encode_status_update_from_data(status)
        }
        KeyAction::Consumed => encode_consumed(),
        KeyAction::InsertTextWithCursor { text, cursor_offset } => {
            encode_commit_text_with_cursor(text, *cursor_offset)
        }
        KeyAction::MoveCursorRight => encode_move_cursor(1),
        KeyAction::DeletePair => encode_delete_pair(),
    }
}

/// 从 StatusUpdateData 编码 StatusUpdate 响应
fn encode_status_update_from_data(status: &StatusUpdateData) -> Vec<u8> {
    use wind_ipc::codec::*;
    encode_status_update(
        status.chinese_mode,
        status.full_width,
        status.chinese_punct,
        status.toolbar_visible,
        status.caps_lock,
        &status.key_down_hotkeys,
        &status.key_up_hotkeys,
        &status.icon_label,
    )
}
