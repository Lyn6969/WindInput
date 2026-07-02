//! 二进制协议编解码器
//!
//! 与 Go 版本 `wind_input/internal/ipc/binary_codec.go` 对齐。

use crate::protocol::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("buffer too short: need {need}, got {got}")]
    BufferTooShort { need: usize, got: usize },
    #[error("unsupported protocol version: 0x{version:04X}")]
    UnsupportedVersion { version: u16 },
    #[error("payload too large: {size} > {max}")]
    PayloadTooLarge { size: usize, max: usize },
    #[error("invalid UTF-8 in payload: {0}")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
}

/// 最大载荷大小 (16MB，与 RPC 一致)
pub const MAX_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;

/// 从字节流解码 IPC Header
pub fn decode_header(buf: &[u8]) -> Result<IpcHeader, CodecError> {
    if buf.len() < IpcHeader::SIZE {
        return Err(CodecError::BufferTooShort {
            need: IpcHeader::SIZE,
            got: buf.len(),
        });
    }
    let header = IpcHeader::from_bytes(&buf[..8].try_into().unwrap());

    // 版本兼容性检查：只检查主版本号
    let major = header.major_version();
    let expected_major = PROTOCOL_VERSION & VERSION_MASK;
    if major != expected_major {
        return Err(CodecError::UnsupportedVersion {
            version: header.version,
        });
    }

    // 载荷大小检查
    if header.length as usize > MAX_PAYLOAD_SIZE {
        return Err(CodecError::PayloadTooLarge {
            size: header.length as usize,
            max: MAX_PAYLOAD_SIZE,
        });
    }

    Ok(header)
}

/// 编码 IPC Header 到字节数组
pub fn encode_header(header: &IpcHeader) -> [u8; 8] {
    header.to_bytes()
}

/// 从载荷字节解码 KeyPayload
pub fn decode_key_payload(payload: &[u8]) -> Result<KeyPayload, CodecError> {
    KeyPayload::from_bytes(payload).ok_or(CodecError::BufferTooShort {
        need: KeyPayload::SIZE,
        got: payload.len(),
    })
}

/// 从载荷字节解码 FocusGainedPayload
pub fn decode_focus_gained(payload: &[u8]) -> Result<FocusGainedPayload, CodecError> {
    FocusGainedPayload::from_bytes(payload).ok_or(CodecError::BufferTooShort {
        need: FocusGainedPayload::SIZE,
        got: payload.len(),
    })
}

/// 编码 CommitText 响应 (CMD_COMMIT_TEXT 0x0101)
///
/// 格式: CommitTextHeader(12) + UTF-8 text + optional newComposition
///
/// flags: bit0=modeChanged(0x01), bit1=hasNewComposition(0x02), bit2=chineseMode(0x04)
pub fn encode_commit_text(
    text: &str,
    new_composition: Option<&str>,
    mode_changed: bool,
    chinese_mode: bool,
    has_new_composition: bool,
) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let comp_bytes = new_composition.map(|s| s.as_bytes());
    let comp_len = comp_bytes.map_or(0, |b| b.len());

    let mut flags: u32 = 0;
    if mode_changed {
        flags |= 0x01;
    }
    if comp_bytes.is_some() || has_new_composition {
        flags |= 0x02;
    }
    if chinese_mode {
        flags |= 0x04;
    }

    let total = 12 + text_bytes.len() + comp_len;
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + total);

    // IpcHeader
    let ipc = IpcHeader::new(CMD_COMMIT_TEXT, total as u32);
    buf.extend_from_slice(&ipc.to_bytes());

    // CommitTextHeader
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(comp_len as u32).to_le_bytes());

    // Text
    buf.extend_from_slice(text_bytes);

    // Optional new composition
    if let Some(comp) = comp_bytes {
        buf.extend_from_slice(comp);
    }

    buf
}

/// 编码 CommitResult 响应 (CMD_COMMIT_RESULT 0x0105)
///
/// 格式: CommitResultHeader(12) + UTF-8 text + optional UTF-8 newComposition
///
/// 用于 barrier 机制的提交响应（Space/Enter/数字选词）。
pub fn encode_commit_result(
    barrier_seq: u16,
    text: &str,
    new_composition: Option<&str>,
    mode_changed: bool,
    chinese_mode: bool,
) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let comp_bytes = new_composition.map(|s| s.as_bytes());
    let comp_len = comp_bytes.map_or(0, |b| b.len());

    let mut flags: u16 = 0;
    if mode_changed {
        flags |= COMMIT_FLAG_MODE_CHANGED;
    }
    if comp_bytes.is_some() {
        flags |= COMMIT_FLAG_HAS_NEW_COMPOSITION;
    }
    if chinese_mode {
        flags |= COMMIT_FLAG_CHINESE_MODE;
    }

    let total = 12 + text_bytes.len() + comp_len;
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + total);

    // IpcHeader
    let ipc = IpcHeader::new(CMD_COMMIT_RESULT, total as u32);
    buf.extend_from_slice(&ipc.to_bytes());

    // CommitResultHeader: barrierSeq(u16) + flags(u16) + textLength(u32) + compositionLength(u32)
    buf.extend_from_slice(&barrier_seq.to_le_bytes());
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(comp_len as u32).to_le_bytes());

    // Text
    buf.extend_from_slice(text_bytes);

    // Optional new composition
    if let Some(comp) = comp_bytes {
        buf.extend_from_slice(comp);
    }

    buf
}

/// 从载荷字节解码 CommitRequestPayload
///
/// 格式: barrierSeq(u16) + triggerKey(u16) + modifiers(u32) + inputBufferLen(u32) + inputBuffer(UTF-8)
pub fn decode_commit_request(payload: &[u8]) -> Result<CommitRequestPayload, CodecError> {
    if payload.len() < 8 {
        return Err(CodecError::BufferTooShort {
            need: 8,
            got: payload.len(),
        });
    }
    let barrier_seq = u16::from_le_bytes([payload[0], payload[1]]);
    let trigger_key = u16::from_le_bytes([payload[2], payload[3]]);
    let modifiers = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);

    let input_buffer = if payload.len() > 8 {
        // 剩余字节为 inputBuffer（可能有长度前缀，也可能直接是 UTF-8）
        // Go 版 DecodeCommitRequestPayload 读取：barrierSeq(2) + triggerKey(2) + modifiers(4) + inputBufferLen(4) + inputBuffer
        if payload.len() >= 12 {
            let buf_len =
                u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
            if payload.len() >= 12 + buf_len {
                String::from_utf8(payload[12..12 + buf_len].to_vec()).unwrap_or_default()
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    Ok(CommitRequestPayload {
        barrier_seq,
        trigger_key,
        modifiers,
        input_buffer,
    })
}

/// 编码 UpdateComposition 响应
pub fn encode_update_composition(text: &str, caret_pos: u32) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let payload_len = 4 + text_bytes.len(); // caretPos(u32) + text

    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_UPDATE_COMPOSITION, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&caret_pos.to_le_bytes());
    buf.extend_from_slice(text_bytes);

    buf
}

/// 编码 ACK 响应
pub fn encode_ack() -> Vec<u8> {
    let ipc = IpcHeader::new(CMD_ACK, 0);
    ipc.to_bytes().to_vec()
}

/// 编码 ModePush 响应（FocusGained 同步路径）：4 字节 LE flags，仅携带中英/全半角。
/// DLL 收到后在首键前写好 _bChineseMode/_bFullWidth。与 Go `EncodeModePush` 字节对齐。
pub fn encode_mode_push(chinese_mode: bool, full_width: bool) -> Vec<u8> {
    let mut flags: u32 = 0;
    if chinese_mode {
        flags |= STATUS_CHINESE_MODE;
    }
    if full_width {
        flags |= STATUS_FULL_WIDTH;
    }
    let ipc = IpcHeader::new(CMD_MODE_PUSH, 4);
    let mut out = ipc.to_bytes().to_vec();
    out.extend_from_slice(&flags.to_le_bytes());
    out
}

/// 编码 ShellExec 推送（CMD_SHELL_EXEC 0x020E）：让 TSF DLL 在前台应用进程中执行 ShellExecuteW。
///
/// 格式: target_len(u32 LE) + target(UTF-8) + params_len(u32 LE) + params(UTF-8)
/// - open(url/file): target = url, params = ""
/// - proc.run(cmd, args): target = cmd, params = args joined with space
pub fn encode_shell_exec(target: &str, params: &str) -> Vec<u8> {
    let t = target.as_bytes();
    let p = params.as_bytes();
    let payload_len = 4 + t.len() + 4 + p.len();
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);
    let ipc = IpcHeader::new(CMD_SHELL_EXEC, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&(t.len() as u32).to_le_bytes());
    buf.extend_from_slice(t);
    buf.extend_from_slice(&(p.len() as u32).to_le_bytes());
    buf.extend_from_slice(p);
    buf
}

/// 编码 PassThrough 响应
pub fn encode_pass_through() -> Vec<u8> {
    let ipc = IpcHeader::new(CMD_PASS_THROUGH, 0);
    ipc.to_bytes().to_vec()
}

/// 编码 Consumed 响应
pub fn encode_consumed() -> Vec<u8> {
    let ipc = IpcHeader::new(CMD_CONSUMED, 0);
    ipc.to_bytes().to_vec()
}

/// 编码 ClearComposition 响应
pub fn encode_clear_composition() -> Vec<u8> {
    let ipc = IpcHeader::new(CMD_CLEAR_COMPOSITION, 0);
    ipc.to_bytes().to_vec()
}

/// 编码 StatusUpdate 响应 (CMD_STATUS_UPDATE 0x0202)
///
/// 格式: StatusHeader(12) + keyHashes(u32*N) + iconLabel(UTF-8)
///
/// 用于 bridge pipe 上的同步状态响应（如 ToggleMode、MenuCommand 等）。
/// 与 EncodeActivationStatusPush 载荷格式一致，但 command 不同。
pub fn encode_status_update(
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    key_down_hashes: &[u32],
    key_up_hashes: &[u32],
    icon_label: &str,
) -> Vec<u8> {
    encode_status_update_ex(
        CMD_STATUS_UPDATE,
        chinese_mode,
        full_width,
        chinese_punct,
        toolbar_visible,
        caps_lock,
        false, // host_render_avail
        key_down_hashes,
        key_up_hashes,
        icon_label,
    )
}

/// 编码 ActivationStatusPush (CMD_ACTIVATION_STATUS_PUSH 0x020C)
///
/// 格式与 StatusUpdate 完全一致，仅 command 字段不同。
/// 用于 IMEActivated/FocusGained 异步化后通过 push pipe 推送状态回包。
/// C++ 端 AsyncReader 收到后 Post 到 TSF 线程做 _SyncStateFromResponse + _EnsureHostRenderSetup。
///
/// 与 StatePush 的区别：本命令是 activation 握手回包，必须携带完整 hotkeys + hostRenderAvail。
pub fn encode_activation_status_push(
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    host_render_avail: bool,
    key_down_hashes: &[u32],
    key_up_hashes: &[u32],
    icon_label: &str,
) -> Vec<u8> {
    encode_status_update_ex(
        CMD_ACTIVATION_STATUS_PUSH,
        chinese_mode,
        full_width,
        chinese_punct,
        toolbar_visible,
        caps_lock,
        host_render_avail,
        key_down_hashes,
        key_up_hashes,
        icon_label,
    )
}

/// 编码 StatePush (CMD_STATE_PUSH 0x0206)
///
/// 格式与 StatusUpdate 一致但使用 CmdStatePush 命令码，且不含 hotkeys。
/// 用于焦点不变时的状态变化广播（如点击工具栏切换中英模式）。
pub fn encode_state_push(
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    icon_label: &str,
) -> Vec<u8> {
    encode_status_update_ex(
        CMD_STATE_PUSH,
        chinese_mode,
        full_width,
        chinese_punct,
        toolbar_visible,
        caps_lock,
        false, // host_render_avail
        &[],   // no hotkeys
        &[],
        icon_label,
    )
}

/// 状态编码公共逻辑（StatusUpdate / StatePush / ActivationStatusPush 共用）
fn encode_status_update_ex(
    command: u16,
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    host_render_avail: bool,
    key_down_hashes: &[u32],
    key_up_hashes: &[u32],
    icon_label: &str,
) -> Vec<u8> {
    let mut flags: u32 = 0;
    if chinese_mode {
        flags |= STATUS_CHINESE_MODE;
    }
    if full_width {
        flags |= STATUS_FULL_WIDTH;
    }
    if chinese_punct {
        flags |= STATUS_CHINESE_PUNCT;
    }
    if toolbar_visible {
        flags |= STATUS_TOOLBAR_VISIBLE;
    }
    if caps_lock {
        flags |= STATUS_CAPS_LOCK;
    }
    if host_render_avail {
        flags |= STATUS_HOST_RENDER_AVAIL;
    }

    let key_down_count = key_down_hashes.len() as u32;
    let key_up_count = key_up_hashes.len() as u32;
    let label_bytes = icon_label.as_bytes();

    let hash_total = (key_down_count + key_up_count) as usize * 4;
    let payload_len = 12 + hash_total + label_bytes.len();

    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    // IpcHeader
    let ipc = IpcHeader::new(command, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());

    // StatusHeader
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&key_down_count.to_le_bytes());
    buf.extend_from_slice(&key_up_count.to_le_bytes());

    // Key hashes
    for h in key_down_hashes {
        buf.extend_from_slice(&h.to_le_bytes());
    }
    for h in key_up_hashes {
        buf.extend_from_slice(&h.to_le_bytes());
    }

    // Icon label
    buf.extend_from_slice(label_bytes);

    buf
}

/// 编码批处理响应
pub fn encode_batch_response(sub_messages: &[Vec<u8>]) -> Vec<u8> {
    // BatchHeader: eventCount(u16) + reserved(u16)
    let sub_total: usize = sub_messages.iter().map(|m| m.len()).sum();
    let payload_len = 4 + sub_total;

    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_BATCH_RESPONSE, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&(sub_messages.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved

    for msg in sub_messages {
        buf.extend_from_slice(msg);
    }

    buf
}

/// 编码 CommitTextWithCursor 响应 (CMD_COMMIT_TEXT_WITH_CURSOR 0x0106)
///
/// 格式: textLength(4) + cursorOffset(4) + UTF-8 text
pub fn encode_commit_text_with_cursor(text: &str, cursor_offset: u32) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let payload_len = 8 + text_bytes.len();

    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_COMMIT_TEXT_WITH_CURSOR, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&cursor_offset.to_le_bytes());
    buf.extend_from_slice(text_bytes);

    buf
}

/// 编码 MoveCursor 响应 (CMD_MOVE_CURSOR 0x0107)
///
/// 格式: direction(4) — 1=right
pub fn encode_move_cursor(direction: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + 4);

    let ipc = IpcHeader::new(CMD_MOVE_CURSOR, 4);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&direction.to_le_bytes());

    buf
}

/// 编码 DeletePair 响应 (CMD_DELETE_PAIR 0x0108)
///
/// 无载荷：删除 1 个左侧字符 + 1 个右侧字符
pub fn encode_delete_pair() -> Vec<u8> {
    let ipc = IpcHeader::new(CMD_DELETE_PAIR, 0);
    ipc.to_bytes().to_vec()
}

/// 编码 ReplaceBackward 响应 (CMD_REPLACE_BACKWARD 0x0109)
///
/// 格式: count(4) + text_len(4) + UTF-8 text —— 删光标前 count 个字符后插入 text（智能符号替换）
pub fn encode_replace_backward(count: u32, text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let payload_len = 8 + text_bytes.len();
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_REPLACE_BACKWARD, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(text_bytes);

    buf
}

/// 编码 CommitAndHoldComposition 响应 (CMD_COMMIT_AND_HOLD 0x010B)
///
/// 格式：timeout_ms(4) + commit_len(4) + hold_len(4) + commit_utf8 + hold_utf8
/// C++ 端先提交 commit_text（候选），再开 HoldComposition 放入 hold_text（中文标点）。
pub fn encode_commit_and_hold(timeout_ms: u32, commit_text: &str, hold_text: &str) -> Vec<u8> {
    let commit_bytes = commit_text.as_bytes();
    let hold_bytes = hold_text.as_bytes();
    let payload_len = 12 + commit_bytes.len() + hold_bytes.len();
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_COMMIT_AND_HOLD, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&timeout_ms.to_le_bytes());
    buf.extend_from_slice(&(commit_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(hold_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(commit_bytes);
    buf.extend_from_slice(hold_bytes);
    buf
}

/// 编码 HoldComposition 响应 (CMD_HOLD_COMPOSITION 0x010A)
///
/// 格式：timeout_ms(4) + text_len(4) + UTF-8 text
/// C++ 端开启组合显示 text，timeout_ms 毫秒后自动提交（智能符号 HoldComposition 方案）。
pub fn encode_hold_composition(timeout_ms: u32, text: &str) -> Vec<u8> {
    let text_bytes = text.as_bytes();
    let payload_len = 8 + text_bytes.len();
    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload_len);

    let ipc = IpcHeader::new(CMD_HOLD_COMPOSITION, payload_len as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&timeout_ms.to_le_bytes());
    buf.extend_from_slice(&(text_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(text_bytes);
    buf
}

/// 编码 HostRenderSetup 响应 (CMD_HOST_RENDER_SETUP 0x0501)
///
/// 格式: entryCount(u32) + entries...
/// 每个 entry: kind(u32) + shmNameLen(u32) + shmName(UTF-8) + eventNameLen(u32) + eventName(UTF-8)
pub fn encode_host_render_setup(entries: &[(u32, String, String)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (kind, shm_name, event_name) in entries {
        let shm_bytes = shm_name.as_bytes();
        let evt_bytes = event_name.as_bytes();
        payload.extend_from_slice(&kind.to_le_bytes());
        payload.extend_from_slice(&(shm_bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(shm_bytes);
        payload.extend_from_slice(&(evt_bytes.len() as u32).to_le_bytes());
        payload.extend_from_slice(evt_bytes);
    }

    let mut buf = Vec::with_capacity(IpcHeader::SIZE + payload.len());
    let ipc = IpcHeader::new(CMD_HOST_RENDER_SETUP, payload.len() as u32);
    buf.extend_from_slice(&ipc.to_bytes());
    buf.extend_from_slice(&payload);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_hold_composition_layout() {
        let buf = encode_hold_composition(500, "，");
        // IpcHeader: 8 bytes (cmd u16 LE + version u16 LE + payload_len u32 LE)
        // payload: timeout_ms(4) + text_len(4) + "，"(3 UTF-8 bytes) = 11 bytes
        assert_eq!(buf.len(), 8 + 11);
        // cmd = 0x010A LE (at offset 2-4)
        assert_eq!(buf[2], 0x0A);
        assert_eq!(buf[3], 0x01);
        // payload_len = 11 LE (at offset 4-8)
        assert_eq!(u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]), 11);
        // timeout_ms = 500 LE (at offset 8-12)
        assert_eq!(u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]), 500);
        // text_len = 3 LE (at offset 12-16)
        assert_eq!(u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]), 3);
        // UTF-8 bytes of "，" = [0xEF, 0xBC, 0x8C] (at offset 16+)
        assert_eq!(&buf[16..], "，".as_bytes());
    }
}

// ── darwin host-render push 帧编码器 (W4) ──
// 字节布局对照 Swift wind_macos/.../BinaryCodec.swift decoder。均小端，返回完整帧。

/// 追加一个长度前缀(u32 LE)的 UTF-8 字符串
fn push_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// 组帧：IpcHeader(cmd,len) + payload
fn frame(cmd: u16, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(IpcHeader::SIZE + payload.len());
    out.extend_from_slice(&IpcHeader::new(cmd, payload.len() as u32).to_bytes());
    out.extend_from_slice(&payload);
    out
}

/// CmdHostRenderFrame (0x0502): seq:u32 + x:i32 + y:i32 + w:u32 + h:u32 + flags:u32 + scale:u32 (28B)
pub fn encode_host_render_frame(
    seq: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    flags: u32,
    scale: u32,
) -> Vec<u8> {
    let mut p = Vec::with_capacity(28);
    p.extend_from_slice(&seq.to_le_bytes());
    p.extend_from_slice(&x.to_le_bytes());
    p.extend_from_slice(&y.to_le_bytes());
    p.extend_from_slice(&w.to_le_bytes());
    p.extend_from_slice(&h.to_le_bytes());
    p.extend_from_slice(&flags.to_le_bytes());
    p.extend_from_slice(&scale.to_le_bytes());
    frame(CMD_HOST_RENDER_FRAME, p)
}

/// CmdCandidateRects (0x0503): count:u32 + count×(index,x,y,w,h 各 i32 LE)。
/// index<0 为翻页按钮 (-1=上页 -2=下页)。坐标为 panel-local。
pub fn encode_candidate_rects(rects: &[(i32, i32, i32, i32, i32)]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + rects.len() * 20);
    p.extend_from_slice(&(rects.len() as u32).to_le_bytes());
    for (idx, x, y, w, h) in rects {
        for v in [idx, x, y, w, h] {
            p.extend_from_slice(&v.to_le_bytes());
        }
    }
    frame(CMD_CANDIDATE_RECTS, p)
}

/// CmdModeStatus (0x0504): flags:u32 + effective_mode:u32 + labelLen:u32 + label(UTF-8)
pub fn encode_mode_status(flags: u32, effective_mode: u32, label: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&flags.to_le_bytes());
    p.extend_from_slice(&effective_mode.to_le_bytes());
    push_string(&mut p, label);
    frame(CMD_MODE_STATUS, p)
}

/// CmdCandidateMenuFlags (0x0505): count:u32 + count×(1 字节禁用位)
pub fn encode_candidate_menu_flags(per_cand: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + per_cand.len());
    p.extend_from_slice(&(per_cand.len() as u32).to_le_bytes());
    p.extend_from_slice(per_cand);
    frame(CMD_CANDIDATE_MENU_FLAGS, p)
}

/// 统一菜单树的线格式节点（wind-ipc 本地类型，避免反向依赖 wind-ui）。
/// 上游（coordinator）把 `MenuItemSpec` 映射为此结构后编码。
#[derive(Debug, Clone, Default)]
pub struct MenuNode {
    /// 菜单 id（macOS .app 经 NSMenuItem.tag 回传；分隔线/子菜单父项为 0）。
    pub id: i32,
    pub separator: bool,
    pub checked: bool,
    pub disabled: bool,
    pub label: String,
    pub children: Vec<MenuNode>,
}

/// CmdMenuShow (0x0506): 统一菜单树（响应 CmdShowContextMenu）。
/// 递归布局，与 Swift `BinaryCodec.decodeMenuItems` 对齐：
///   count:u32 + count×item；item = id:i32 + flags:u8 + labelLen:u32 + label(UTF-8) + children(递归)
///   flags 位：bit0=separator bit1=checked bit2=disabled
pub fn encode_menu_show(items: &[MenuNode]) -> Vec<u8> {
    let mut p = Vec::new();
    push_menu_items(&mut p, items);
    frame(CMD_MENU_SHOW, p)
}

fn push_menu_items(out: &mut Vec<u8>, items: &[MenuNode]) {
    out.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for it in items {
        out.extend_from_slice(&it.id.to_le_bytes());
        let flags = (it.separator as u8) | ((it.checked as u8) << 1) | ((it.disabled as u8) << 2);
        out.push(flags);
        push_string(out, &it.label);
        push_menu_items(out, &it.children);
    }
}

/// CmdOpenSettings (0x0507): 请求 .app 打开设置应用。payload = page 裸 UTF-8（无长度前缀），
/// 空串=默认页。与 Swift `CandidatePanelHost` 的 `String(data:encoding:.utf8)` 解码对齐。
pub fn encode_open_settings(page: &str) -> Vec<u8> {
    frame(CMD_OPEN_SETTINGS, page.as_bytes().to_vec())
}

/// 写单个按键 combo：keyLen u32 + key + modCount u32 + modCount×(modLen u32 + mod)。
/// 与 Swift `BinaryCodec.decodeCombo` 对齐。
fn push_key_combo(out: &mut Vec<u8>, key: &str, mods: &[String]) {
    push_string(out, key);
    out.extend_from_slice(&(mods.len() as u32).to_le_bytes());
    for m in mods {
        push_string(out, m);
    }
}

/// CmdKeyTap (0x050E): 单个 combo。key 为 canonical 键名（如 "v"/"enter"/"left"），
/// mods 为 {"ctrl","shift","alt","win"} 子集（win 在 .app 侧映射 Command）。
pub fn encode_key_tap(key: &str, mods: &[String]) -> Vec<u8> {
    let mut p = Vec::new();
    push_key_combo(&mut p, key, mods);
    frame(CMD_KEY_TAP, p)
}

/// CmdKeyHold (0x0510): 单个 combo（按下保持）。
pub fn encode_key_hold(key: &str, mods: &[String]) -> Vec<u8> {
    let mut p = Vec::new();
    push_key_combo(&mut p, key, mods);
    frame(CMD_KEY_HOLD, p)
}

/// CmdKeyRelease (0x0511): 单个 combo（抬起）。
pub fn encode_key_release(key: &str, mods: &[String]) -> Vec<u8> {
    let mut p = Vec::new();
    push_key_combo(&mut p, key, mods);
    frame(CMD_KEY_RELEASE, p)
}

/// CmdKeySeq (0x050F): comboCount u32 + comboCount×combo。
pub fn encode_key_seq(combos: &[(String, Vec<String>)]) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&(combos.len() as u32).to_le_bytes());
    for (key, mods) in combos {
        push_key_combo(&mut p, key, mods);
    }
    frame(CMD_KEY_SEQ, p)
}

/// CmdKeyType (0x0512): 整段 UTF-8 文本（无长度前缀），.app 走 insertText 上屏。
pub fn encode_key_type(text: &str) -> Vec<u8> {
    frame(CMD_KEY_TYPE, text.as_bytes().to_vec())
}

/// CmdTooltipShow (0x0508): textLen+text + bgLen+bg + fgLen+fg + fontPathLen+fontPath
pub fn encode_tooltip_show(text: &str, bg: &str, fg: &str, font_path: &str) -> Vec<u8> {
    let mut p = Vec::new();
    for s in [text, bg, fg, font_path] {
        push_string(&mut p, s);
    }
    frame(CMD_TOOLTIP_SHOW, p)
}

/// CmdTooltipHide (0x0509): 空 payload
pub fn encode_tooltip_hide() -> Vec<u8> {
    frame(CMD_TOOLTIP_HIDE, Vec::new())
}

/// CmdStatusShow (0x050A): textLen+text + bgLen+bg + fgLen+fg + x:i32 + y:i32 + duration_ms:i32
pub fn encode_status_show(
    text: &str,
    bg: &str,
    fg: &str,
    x: i32,
    y: i32,
    duration_ms: i32,
) -> Vec<u8> {
    let mut p = Vec::new();
    for s in [text, bg, fg] {
        push_string(&mut p, s);
    }
    p.extend_from_slice(&x.to_le_bytes());
    p.extend_from_slice(&y.to_le_bytes());
    p.extend_from_slice(&duration_ms.to_le_bytes());
    frame(CMD_STATUS_SHOW, p)
}

/// CmdStatusHide (0x050B): 空 payload
pub fn encode_status_hide() -> Vec<u8> {
    frame(CMD_STATUS_HIDE, Vec::new())
}

/// CmdToastShow (0x050C): 六段长度前缀串 (title+message+bg+fg+accent+position) + duration_ms:i32 + max_width:i32
#[allow(clippy::too_many_arguments)]
pub fn encode_toast_show(
    title: &str,
    message: &str,
    bg: &str,
    fg: &str,
    accent: &str,
    position: &str,
    duration_ms: i32,
    max_width: i32,
) -> Vec<u8> {
    let mut p = Vec::new();
    for s in [title, message, bg, fg, accent, position] {
        push_string(&mut p, s);
    }
    p.extend_from_slice(&duration_ms.to_le_bytes());
    p.extend_from_slice(&max_width.to_le_bytes());
    frame(CMD_TOAST_SHOW, p)
}

/// CmdToastHide (0x050D): 空 payload
pub fn encode_toast_hide() -> Vec<u8> {
    frame(CMD_TOAST_HIDE, Vec::new())
}

#[cfg(test)]
mod darwin_push_tests {
    use super::*;

    fn cmd_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[2], frame[3]])
    }

    #[test]
    fn host_render_frame_layout_is_28_bytes_le() {
        let f = encode_host_render_frame(7, -3, 20, 100, 40, 0x3, 2);
        assert_eq!(f.len(), 8 + 28);
        assert_eq!(cmd_of(&f), CMD_HOST_RENDER_FRAME);
        let p = &f[8..];
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 7);
        assert_eq!(i32::from_le_bytes(p[4..8].try_into().unwrap()), -3);
        assert_eq!(i32::from_le_bytes(p[8..12].try_into().unwrap()), 20);
        assert_eq!(u32::from_le_bytes(p[12..16].try_into().unwrap()), 100);
        assert_eq!(u32::from_le_bytes(p[16..20].try_into().unwrap()), 40);
        assert_eq!(u32::from_le_bytes(p[20..24].try_into().unwrap()), 0x3);
        assert_eq!(u32::from_le_bytes(p[24..28].try_into().unwrap()), 2);
    }

    #[test]
    fn candidate_rects_layout_count_then_5xi32() {
        let f = encode_candidate_rects(&[(0, 1, 2, 30, 24), (-1, 5, 6, 12, 12)]);
        assert_eq!(cmd_of(&f), CMD_CANDIDATE_RECTS);
        let p = &f[8..];
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(p[4..8].try_into().unwrap()), 0);
        assert_eq!(i32::from_le_bytes(p[8..12].try_into().unwrap()), 1);
        assert_eq!(i32::from_le_bytes(p[24..28].try_into().unwrap()), -1);
    }

    #[test]
    fn mode_status_label_utf8_length_prefixed() {
        let f = encode_mode_status(0x5, 1, "五笔");
        assert_eq!(cmd_of(&f), CMD_MODE_STATUS);
        let p = &f[8..];
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 0x5);
        assert_eq!(u32::from_le_bytes(p[4..8].try_into().unwrap()), 1);
        let n = u32::from_le_bytes(p[8..12].try_into().unwrap()) as usize;
        assert_eq!(n, "五笔".len());
        assert_eq!(&p[12..12 + n], "五笔".as_bytes());
    }

    // 递归解码器：镜像 Swift BinaryCodec.decodeMenuItems，作为 encode_menu_show 的规范验证。
    struct DecodedItem {
        id: i32,
        flags: u8,
        label: String,
        children: Vec<DecodedItem>,
    }
    fn decode_menu_items(p: &[u8], off: &mut usize) -> Vec<DecodedItem> {
        let n = u32::from_le_bytes(p[*off..*off + 4].try_into().unwrap()) as usize;
        *off += 4;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let id = i32::from_le_bytes(p[*off..*off + 4].try_into().unwrap());
            *off += 4;
            let flags = p[*off];
            *off += 1;
            let ln = u32::from_le_bytes(p[*off..*off + 4].try_into().unwrap()) as usize;
            *off += 4;
            let label = String::from_utf8(p[*off..*off + ln].to_vec()).unwrap();
            *off += ln;
            let children = decode_menu_items(p, off);
            out.push(DecodedItem {
                id,
                flags,
                label,
                children,
            });
        }
        out
    }

    #[test]
    fn menu_show_roundtrips_nested_tree_le() {
        let tree = vec![
            MenuNode {
                id: 100,
                label: "英文".into(),
                checked: true,
                ..Default::default()
            },
            MenuNode {
                id: 0,
                label: "主题".into(),
                children: vec![
                    MenuNode {
                        id: 2000,
                        label: "默认".into(),
                        checked: true,
                        ..Default::default()
                    },
                    MenuNode {
                        separator: true,
                        ..Default::default()
                    },
                    MenuNode {
                        id: 4001,
                        label: "亮色".into(),
                        disabled: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
        ];
        let f = encode_menu_show(&tree);
        assert_eq!(cmd_of(&f), CMD_MENU_SHOW);
        let p = &f[8..];
        let mut off = 0usize;
        let top = decode_menu_items(p, &mut off);
        assert_eq!(off, p.len(), "整帧应被消费完（无游离字节）");
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].id, 100);
        assert_eq!(top[0].flags & 0x02, 0x02); // checked
        assert_eq!(top[0].label, "英文");
        assert!(top[0].children.is_empty());
        // 子菜单父项 id=0、无勾选，含 3 子项
        assert_eq!(top[1].label, "主题");
        assert_eq!(top[1].id, 0);
        let sub = &top[1].children;
        assert_eq!(sub.len(), 3);
        assert_eq!(sub[0].id, 2000);
        assert_eq!(sub[0].flags & 0x02, 0x02); // checked
        assert_eq!(sub[1].flags & 0x01, 0x01); // separator
        assert_eq!(sub[2].id, 4001);
        assert_eq!(sub[2].flags & 0x04, 0x04); // disabled
    }

    #[test]
    fn tooltip_show_four_length_prefixed_strings() {
        let f = encode_tooltip_show("abc", "#fff", "#000", "/p.ttf");
        assert_eq!(cmd_of(&f), CMD_TOOLTIP_SHOW);
        let p = &f[8..];
        let mut off = 0usize;
        for s in ["abc", "#fff", "#000", "/p.ttf"] {
            let n = u32::from_le_bytes(p[off..off + 4].try_into().unwrap()) as usize;
            assert_eq!(n, s.len());
            assert_eq!(&p[off + 4..off + 4 + n], s.as_bytes());
            off += 4 + n;
        }
        assert_eq!(off, p.len());
    }

    #[test]
    fn status_show_three_strings_then_three_i32() {
        let f = encode_status_show("中 ，", "#111", "#eee", 50, 80, 1000);
        assert_eq!(cmd_of(&f), CMD_STATUS_SHOW);
        let p = &f[8..];
        let mut off = 0usize;
        for s in ["中 ，", "#111", "#eee"] {
            let n = u32::from_le_bytes(p[off..off + 4].try_into().unwrap()) as usize;
            assert_eq!(&p[off + 4..off + 4 + n], s.as_bytes());
            off += 4 + n;
        }
        assert_eq!(i32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 50);
        assert_eq!(
            i32::from_le_bytes(p[off + 4..off + 8].try_into().unwrap()),
            80
        );
        assert_eq!(
            i32::from_le_bytes(p[off + 8..off + 12].try_into().unwrap()),
            1000
        );
    }

    #[test]
    fn empty_payload_frames_are_header_only() {
        assert_eq!(encode_tooltip_hide().len(), 8);
        assert_eq!(encode_status_hide().len(), 8);
        assert_eq!(encode_toast_hide().len(), 8);
        assert_eq!(cmd_of(&encode_tooltip_hide()), CMD_TOOLTIP_HIDE);
        assert_eq!(cmd_of(&encode_status_hide()), CMD_STATUS_HIDE);
        assert_eq!(cmd_of(&encode_toast_hide()), CMD_TOAST_HIDE);
    }

    #[test]
    fn candidate_menu_flags_count_then_bytes() {
        let f = encode_candidate_menu_flags(&[0x01, 0x10, 0x00]);
        assert_eq!(cmd_of(&f), CMD_CANDIDATE_MENU_FLAGS);
        let p = &f[8..];
        assert_eq!(u32::from_le_bytes(p[0..4].try_into().unwrap()), 3);
        assert_eq!(&p[4..7], &[0x01, 0x10, 0x00]);
    }

    #[test]
    fn toast_show_six_strings_then_two_i32() {
        let f = encode_toast_show("标题", "正文", "#1", "#2", "#3", "bottom_right", 5000, 320);
        assert_eq!(cmd_of(&f), CMD_TOAST_SHOW);
        let p = &f[8..];
        let mut off = 0usize;
        for s in ["标题", "正文", "#1", "#2", "#3", "bottom_right"] {
            let n = u32::from_le_bytes(p[off..off + 4].try_into().unwrap()) as usize;
            assert_eq!(&p[off + 4..off + 4 + n], s.as_bytes());
            off += 4 + n;
        }
        assert_eq!(
            i32::from_le_bytes(p[off..off + 4].try_into().unwrap()),
            5000
        );
        assert_eq!(
            i32::from_le_bytes(p[off + 4..off + 8].try_into().unwrap()),
            320
        );
    }
}
