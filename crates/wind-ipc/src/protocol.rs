//! 二进制协议定义：命令码、Header、Payload 结构体
//!
//! 与 Go 版本 `wind_input/internal/ipc/binary_protocol.go` 和
//! C++ 版本 `wind_tsf/include/BinaryProtocol.h` 字节级对齐。

use std::fmt;

// ──────────────────────────────────────────────
// Protocol constants
// ──────────────────────────────────────────────

/// 协议版本号 (v1.1)
pub const PROTOCOL_VERSION: u16 = 0x1001;
/// 异步标志位（version 字段高位）
pub const ASYNC_FLAG: u16 = 0x8000;
/// 版本掩码（取主版本号，排除 ASYNC_FLAG 位 0x8000）
pub const VERSION_MASK: u16 = 0x7000;

// ──────────────────────────────────────────────
// Command codes — 上游 (C++ → Go/Rust)
// ──────────────────────────────────────────────

// 按键事件
pub const CMD_KEY_EVENT: u16 = 0x0101;
pub const CMD_COMMIT_REQUEST: u16 = 0x0104;

// 焦点 & 激活
pub const CMD_FOCUS_GAINED: u16 = 0x0201;
pub const CMD_FOCUS_LOST: u16 = 0x0202;
pub const CMD_IME_ACTIVATED: u16 = 0x0203;
pub const CMD_IME_DEACTIVATED: u16 = 0x0204;
pub const CMD_MODE_NOTIFY: u16 = 0x0205;
pub const CMD_TOGGLE_MODE: u16 = 0x0207;
pub const CMD_MENU_COMMAND: u16 = 0x0208;
pub const CMD_COMPOSITION_TERMINATED: u16 = 0x0209;
pub const CMD_SHOW_CONTEXT_MENU: u16 = 0x020A;
pub const CMD_SYSTEM_MODE_SWITCH: u16 = 0x020B;

// 光标 & 选区
pub const CMD_CARET_UPDATE: u16 = 0x0301;
pub const CMD_SELECTION_CHANGED: u16 = 0x0302;
pub const CMD_CARET_PENDING: u16 = 0x0303;

// Host Render
pub const CMD_HOST_RENDER_REQUEST: u16 = 0x0501;

// 批处理
pub const CMD_BATCH_EVENTS: u16 = 0x0F01;
pub const CMD_BATCH_RESPONSE: u16 = 0x0F02;

// 输入统计
pub const CMD_INPUT_STATS: u16 = 0x0F03;

// ──────────────────────────────────────────────
// Command codes — 下游 (Go/Rust → C++ 响应)
// ──────────────────────────────────────────────

pub const CMD_ACK: u16 = 0x0001;
pub const CMD_PASS_THROUGH: u16 = 0x0002;

// 文本操作
pub const CMD_COMMIT_TEXT: u16 = 0x0101;
pub const CMD_UPDATE_COMPOSITION: u16 = 0x0102;
pub const CMD_CLEAR_COMPOSITION: u16 = 0x0103;
pub const CMD_COMMIT_RESULT: u16 = 0x0105;
pub const CMD_COMMIT_TEXT_WITH_CURSOR: u16 = 0x0106;
pub const CMD_MOVE_CURSOR: u16 = 0x0107;
pub const CMD_DELETE_PAIR: u16 = 0x0108;

// 状态
pub const CMD_STATUS_UPDATE: u16 = 0x0202;
pub const CMD_STATE_PUSH: u16 = 0x0206;
pub const CMD_SERVICE_READY: u16 = 0x0207; // push only
pub const CMD_ACTIVATION_STATUS_PUSH: u16 = 0x020C;
pub const CMD_SYNC_CONFIG: u16 = 0x0303;

// 消费确认
pub const CMD_CONSUMED: u16 = 0x0401;

// Host Render
pub const CMD_HOST_RENDER_SETUP: u16 = 0x0501;

// ──────────────────────────────────────────────
// IPC Header (8 bytes, little-endian)
// ──────────────────────────────────────────────

/// 8 字节 IPC 消息头
///
/// ```text
/// Offset  Size  Field
/// 0       2     version  (含 ASYNC_FLAG)
/// 2       2     command
/// 4       4     payload_length
/// ```
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C, packed)]
pub struct IpcHeader {
    pub version: u16,
    pub command: u16,
    pub length: u32,
}

impl IpcHeader {
    pub const SIZE: usize = 8;

    pub fn new(command: u16, payload_len: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command,
            length: payload_len,
        }
    }

    pub fn new_async(command: u16, payload_len: u32) -> Self {
        Self {
            version: PROTOCOL_VERSION | ASYNC_FLAG,
            command,
            length: payload_len,
        }
    }

    pub fn is_async(&self) -> bool {
        self.version & ASYNC_FLAG != 0
    }

    pub fn major_version(&self) -> u16 {
        self.version & VERSION_MASK
    }

    pub fn to_bytes(&self) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0..2].copy_from_slice(&self.version.to_le_bytes());
        buf[2..4].copy_from_slice(&self.command.to_le_bytes());
        buf[4..8].copy_from_slice(&self.length.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8; 8]) -> Self {
        Self {
            version: u16::from_le_bytes([buf[0], buf[1]]),
            command: u16::from_le_bytes([buf[2], buf[3]]),
            length: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        }
    }
}

impl fmt::Debug for IpcHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let version = self.version;
        let command = self.command;
        let length = self.length;
        f.debug_struct("IpcHeader")
            .field("version", &format_args!("0x{:04X}", version))
            .field("command", &format_args!("0x{:04X}", command))
            .field("length", &length)
            .field("async", &self.is_async())
            .finish()
    }
}

// ──────────────────────────────────────────────
// Key Payload (18 bytes)
// ──────────────────────────────────────────────

/// 按键事件载荷 (18 bytes)
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct KeyPayload {
    pub key_code: u32,
    pub scan_code: u32,
    pub modifiers: u32,
    pub event_type: u8, // 0=keydown, 1=keyup
    pub toggles: u8,    // CapsLock/NumLock/ScrollLock
    pub event_seq: u16,
    pub prev_char: u16,
}

impl KeyPayload {
    pub const SIZE: usize = 18;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.key_code.to_le_bytes());
        buf[4..8].copy_from_slice(&self.scan_code.to_le_bytes());
        buf[8..12].copy_from_slice(&self.modifiers.to_le_bytes());
        buf[12] = self.event_type;
        buf[13] = self.toggles;
        buf[14..16].copy_from_slice(&self.event_seq.to_le_bytes());
        buf[16..18].copy_from_slice(&self.prev_char.to_le_bytes());
        buf
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            key_code: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            scan_code: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            modifiers: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            event_type: buf[12],
            toggles: buf[13],
            event_seq: u16::from_le_bytes([buf[14], buf[15]]),
            prev_char: u16::from_le_bytes([buf[16], buf[17]]),
        })
    }
}

// ──────────────────────────────────────────────
// Caret Payload (20 bytes)
// ──────────────────────────────────────────────

/// 光标位置载荷 (20 bytes)
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct CaretPayload {
    pub x: i32,
    pub y: i32,
    pub height: i32,
    pub composition_start_x: i32,
    pub composition_start_y: i32,
}

impl CaretPayload {
    pub const SIZE: usize = 20;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            x: i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            y: i32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            height: i32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            composition_start_x: i32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            composition_start_y: i32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        })
    }
}

// ──────────────────────────────────────────────
// Focus Gained Payload (36 bytes)
// ──────────────────────────────────────────────

/// 焦点获取载荷 (36 bytes)
#[derive(Clone, Copy, Debug)]
pub struct FocusGainedPayload {
    pub caret: CaretPayload,
    pub client_token: u64,
    pub input_scope_mask: u64,
}

impl FocusGainedPayload {
    pub const SIZE: usize = 36;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        let caret = CaretPayload::from_bytes(&buf[0..20])?;
        let client_token = u64::from_le_bytes([
            buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
        ]);
        let input_scope_mask = u64::from_le_bytes([
            buf[28], buf[29], buf[30], buf[31], buf[32], buf[33], buf[34], buf[35],
        ]);
        Some(Self {
            caret,
            client_token,
            input_scope_mask,
        })
    }
}

// ──────────────────────────────────────────────
// Commit Request Payload (12 + variable)
// ──────────────────────────────────────────────

/// 提交请求载荷 (12 + variable)
#[derive(Clone, Debug)]
pub struct CommitRequestPayload {
    pub barrier_seq: u16,
    pub trigger_key: u16,
    pub modifiers: u32,
    pub input_buffer: String,
}

// ──────────────────────────────────────────────
// Status Header (12 bytes + variable)
// ──────────────────────────────────────────────

/// 状态更新头 (12 bytes)
#[derive(Clone, Debug)]
pub struct StatusHeader {
    pub flags: u32,
    pub key_down_count: u32,
    pub key_up_count: u32,
    pub key_hashes: Vec<u32>,
    pub icon_label: String,
}

// ──────────────────────────────────────────────
// Commit Text Header (12 bytes + variable)
// ──────────────────────────────────────────────

/// Commit 文本头 (12 bytes)
#[derive(Clone, Debug)]
pub struct CommitTextHeader {
    pub flags: u32,
    pub text_length: u32,
    pub composition_length: u32,
}

impl CommitTextHeader {
    pub const SIZE: usize = 12;

    pub fn has_new_composition(&self) -> bool {
        self.flags & 0x02 != 0
    }

    pub fn chinese_mode(&self) -> bool {
        self.flags & 0x04 != 0
    }

    pub fn mode_changed(&self) -> bool {
        self.flags & 0x01 != 0
    }
}

// ──────────────────────────────────────────────
// Shared Render Header (64 bytes)
// ──────────────────────────────────────────────

/// 共享渲染头 (64 bytes)
pub const SHARED_RENDER_MAGIC: u32 = 0x57494E44; // 'WIND'
pub const SHARED_RENDER_VERSION: u32 = 1;
pub const MAX_SHARED_RENDER_SIZE: usize = 4 * 1024 * 1024; // 4MB

#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct SharedRenderHeader {
    pub magic: u32,
    pub version: u32,
    pub sequence: u32,
    pub flags: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data_size: u32,
    pub reserved: [u32; 6],
}

impl SharedRenderHeader {
    pub const SIZE: usize = 64;

    pub const FLAG_VISIBLE: u32 = 0x0001;
    pub const FLAG_CONTENT_READY: u32 = 0x0002;
    pub const FLAG_SOFTWARE_SHADOW: u32 = 0x0004;

    pub fn new(x: i32, y: i32, width: u32, height: u32, stride: u32, data_size: u32) -> Self {
        Self {
            magic: SHARED_RENDER_MAGIC,
            version: SHARED_RENDER_VERSION,
            sequence: 0,
            flags: Self::FLAG_VISIBLE | Self::FLAG_CONTENT_READY,
            x,
            y,
            width,
            height,
            stride,
            data_size,
            reserved: [0; 6],
        }
    }

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        // SAFETY: SharedRenderHeader is repr(C, packed) with no padding
        unsafe { std::mem::transmute_copy(self) }
    }
}

// ──────────────────────────────────────────────
// Modifier flags
// ──────────────────────────────────────────────

pub const MOD_SHIFT: u32 = 0x0001;
pub const MOD_CTRL: u32 = 0x0002;
pub const MOD_ALT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
pub const MOD_LSHIFT: u32 = 0x0010;
pub const MOD_RSHIFT: u32 = 0x0020;
pub const MOD_LCTRL: u32 = 0x0040;
pub const MOD_RCTRL: u32 = 0x0080;
pub const MOD_CAPSLOCK: u32 = 0x0100;

/// 计算热键哈希值：(modifiers << 16) | (keyCode & 0xFFFF)
pub fn calc_key_hash(modifiers: u32, key_code: u32) -> u32 {
    (modifiers << 16) | (key_code & 0xFFFF)
}

/// 热键策略位
pub const HOTKEY_POLICY_CHINESE_ONLY: u32 = 0x40000000;
pub const HOTKEY_POLICY_SESSION: u32 = 0x80000000;

// ──────────────────────────────────────────────
// Status flags (与 Go StatusChineseMode 等对齐)
// ──────────────────────────────────────────────

pub const STATUS_CHINESE_MODE: u32 = 0x0001;
pub const STATUS_FULL_WIDTH: u32 = 0x0002;
pub const STATUS_CHINESE_PUNCT: u32 = 0x0004;
pub const STATUS_TOOLBAR_VISIBLE: u32 = 0x0008;
pub const STATUS_MODE_CHANGED: u32 = 0x0010;
pub const STATUS_CAPS_LOCK: u32 = 0x0020;
pub const STATUS_HOST_RENDER_AVAIL: u32 = 0x0040;

// ──────────────────────────────────────────────
// Commit result flags
// ──────────────────────────────────────────────

pub const COMMIT_FLAG_MODE_CHANGED: u16 = 0x0001;
pub const COMMIT_FLAG_HAS_NEW_COMPOSITION: u16 = 0x0002;
pub const COMMIT_FLAG_CHINESE_MODE: u16 = 0x0004;

// ──────────────────────────────────────────────
// Event type
// ──────────────────────────────────────────────

pub const EVENT_KEY_DOWN: u8 = 0;
pub const EVENT_KEY_UP: u8 = 1;
