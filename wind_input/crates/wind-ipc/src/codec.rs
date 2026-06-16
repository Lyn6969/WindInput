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
            let buf_len = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]) as usize;
            if payload.len() >= 12 + buf_len {
                String::from_utf8(payload[12..12 + buf_len].to_vec())
                    .unwrap_or_default()
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
