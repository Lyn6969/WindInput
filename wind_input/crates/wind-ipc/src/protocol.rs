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
pub const CMD_INPUT_STATE_REPORT: u16 = 0x0213;

// 上行（darwin .app / Windows host-render DLL）：鼠标候选交互。方向与下行 0x020D/0x020E 由 dispatch 上下文区分。
pub const CMD_CANDIDATE_SELECT: u16 = 0x020D; // payload: pageLocalIndex i32 LE；<0 为翻页按钮（-1 上页 / -2 下页，与 SHM 命中矩形约定一致）
pub const CMD_CANDIDATE_HOVER: u16 = 0x020E; // payload: pageLocalIndex i32 LE (-1=无；Windows 另带 anchorX/belowY/aboveY 三个 i32，当前仅取 index)
pub const CMD_CANDIDATE_CONTEXT_MENU: u16 = 0x020F; // 上行：候选右键动作 (payload: index i32 + actionLen u32 + action UTF-8)
pub const CMD_MENU_ACTION: u16 = 0x0210; // 上行：统一菜单项被选中 (payload: 菜单 id i32 LE)
// ⚠️ 0x0211 平台双语义（历史遗留，dispatch 按 cfg 分臂，两侧上行方向平台互斥）：
// - Windows DLL 上行 = CANDIDATE_SCROLL（Go/C++ BinaryProtocol.h 原始定义）；
// - macOS .app 上行 = FRONT_CONTEXT（macOS 移植期新增时误复用了该码位，Swift BinaryCodec 已固化）。
// 若未来需要在同一平台同时使用两者，须迁移 FRONT_CONTEXT 到空闲码位并同步 Swift 端。
pub const CMD_CANDIDATE_SCROLL: u16 = 0x0211; // Windows 上行：host 候选框滚轮 (payload: delta i32，WHEEL_DELTA 倍数，正=上滚)；服务端统一决策（默认不翻页，对齐 Go）
// darwin .app 上报前台上下文（命令直通车 app()/title()/sel() 取值）：
// payload = appLen u32 + app(UTF-8) + titleLen u32 + title + selLen u32 + sel，均 LE 长度前缀。
pub const CMD_FRONT_CONTEXT: u16 = 0x0211;
/// Host render: DLL 侧 Band 窗口创建失败（异步上行，payload = reason u32）。
/// 服务端收到后记日志并让 UI 回退本地窗口。与 C++ BinaryProtocol.h:36 对齐。
pub const CMD_HOST_RENDER_FAILED: u16 = 0x0212;

// 光标 & 选区
pub const CMD_CARET_UPDATE: u16 = 0x0301;
pub const CMD_SELECTION_CHANGED: u16 = 0x0302;
pub const CMD_CARET_PENDING: u16 = 0x0303;

// Host Render
pub const CMD_HOST_RENDER_REQUEST: u16 = 0x0501;

// darwin 专用 host-render push 帧（方向与上行 0x05xx 由 push 通道语义区分）。
// 字节布局须与 Swift wind_macos/.../BinaryCodec.swift decoder 及 Go binary_codec.go 一致。
pub const CMD_HOST_RENDER_FRAME: u16 = 0x0502; // SHM 新帧就绪通知 (seq+几何+flags+scale, 28B)
pub const CMD_CANDIDATE_RECTS: u16 = 0x0503; // 候选命中矩形 (panel-local)
pub const CMD_MODE_STATUS: u16 = 0x0504; // 输入模式状态 (菜单栏指示器)
pub const CMD_CANDIDATE_MENU_FLAGS: u16 = 0x0505; // 每候选右键菜单禁用位
pub const CMD_MENU_SHOW: u16 = 0x0506; // 统一菜单树 (响应 CmdShowContextMenu)
pub const CMD_OPEN_SETTINGS: u16 = 0x0507; // 请求 .app 打开设置应用 (payload: page 裸 UTF-8, 空=默认页)
pub const CMD_TOOLTIP_SHOW: u16 = 0x0508; // 候选悬停 tooltip
pub const CMD_TOOLTIP_HIDE: u16 = 0x0509;
pub const CMD_STATUS_SHOW: u16 = 0x050A; // 模式状态气泡
pub const CMD_STATUS_HIDE: u16 = 0x050B;
pub const CMD_TOAST_SHOW: u16 = 0x050C; // Toast 通知
pub const CMD_TOAST_HIDE: u16 = 0x050D;
// 命令直通车按键合成（darwin 下行）：服务进程无辅助功能授权无法 post CGEvent，
// 故把 key.tap/seq/hold/release/type 推给 .app 侧 KeySynthesizer 合成（.app 有授权）。
// combo 载荷：keyLen u32 + key + modCount u32 + modCount×(modLen u32 + mod)，key/mod 均 UTF-8。
pub const CMD_KEY_TAP: u16 = 0x050E; // 单个 combo
pub const CMD_KEY_SEQ: u16 = 0x050F; // comboCount u32 + comboCount×combo
pub const CMD_KEY_HOLD: u16 = 0x0510; // 单个 combo（按下保持）
pub const CMD_KEY_RELEASE: u16 = 0x0511; // 单个 combo（抬起）
pub const CMD_KEY_TYPE: u16 = 0x0512; // 整段 UTF-8 文本（无长度前缀），.app 走 insertText 上屏

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
/// 删除光标前 N 个字符并插入文本（智能符号替换）。
pub const CMD_REPLACE_BACKWARD: u16 = 0x0109;
/// HoldComposition 响应 (0x010A)：开启组合显示 text，在 timeout_ms 毫秒后自动提交。
/// 载荷：timeout_ms(u32 LE) + text_len(u32 LE) + UTF-8 text
pub const CMD_HOLD_COMPOSITION: u16 = 0x010A;
/// CommitAndHoldComposition 响应 (0x010B)：先提交 commit_text，再开 HoldComposition 放入 hold_text。
/// 载荷：timeout_ms(u32 LE) + commit_len(u32 LE) + hold_len(u32 LE) + commit_utf8 + hold_utf8
pub const CMD_COMMIT_AND_HOLD: u16 = 0x010B;
/// CommitThenDeferComposition 响应 (0x010C)：先真提交 commit_text，
/// 余码新组合 deferred_composition 延迟到触发键 keyup（或 timeout_ms 兜底）才开。
/// 载荷：timeout_ms(u32 LE) + commit_len(u32 LE) + defer_len(u32 LE) + commit_utf8 + defer_utf8
pub const CMD_COMMIT_THEN_DEFER: u16 = 0x010C;

// 状态
pub const CMD_STATUS_UPDATE: u16 = 0x0202;
pub const CMD_STATE_PUSH: u16 = 0x0206;
pub const CMD_SERVICE_READY: u16 = 0x0207; // push only
pub const CMD_ACTIVATION_STATUS_PUSH: u16 = 0x020C;
/// FocusGained 同步路径的轻量模式回传（仅 chineseMode+fullWidth，4 字节 flags）。
/// DLL 在 OnSetFocus 内同步等本响应，首键前写好 _bChineseMode，根治"切应用首键上屏英文"
/// 竞态；同时解除 DLL 的同步等待（否则无响应会卡到 READ_TIMEOUT_MS）。位定义同 STATUS_*。
// 注：0x020D 双用途——下行此 CMD_MODE_PUSH（service→client push，仅编码）；
// 上行 CMD_CANDIDATE_SELECT（client→service 请求，仅 dispatch）。方向区分，勿在 dispatch 加 MODE_PUSH 臂。
pub const CMD_MODE_PUSH: u16 = 0x020D;
/// TSF 侧在前台应用进程中执行 ShellExecute（打开 URL / 启动程序），解决 Service 进程无前台权限的问题。
/// 载荷：target_len(u32 LE) + target(UTF-8) + params_len(u32 LE) + params(UTF-8)
pub const CMD_SHELL_EXEC: u16 = 0x020E;
pub const CMD_SYNC_CONFIG: u16 = 0x0303;

/// 配置同步键名（对齐 C++ BinaryProtocol.h CONFIG_KEY_*）
pub const CONFIG_KEY_ENGLISH_PAIRS: &str = "en_pairs";
/// 配对跳出键（VK 码集合）同步键名。TSF 端英文模式配对跳出直接消费；
/// 中文模式仅用于「有待跳出配对时」放行转发（真正裁决在协调器）。
pub const CONFIG_KEY_JUMP_OUT_KEYS: &str = "jump_out_keys";

// 消费确认
pub const CMD_CONSUMED: u16 = 0x0401;

// Host Render
/// 仅 Windows 使用；darwin 端 SHM 名固定（endpoint::shm_name），无 setup 握手。
pub const CMD_HOST_RENDER_SETUP: u16 = 0x0501;

/// Host 窗口种类（与 C++ HostWindowKind 对齐，BinaryProtocol.h:359-365）
pub const HOST_WINDOW_CANDIDATE: u32 = 0;
pub const HOST_WINDOW_TOOLTIP: u32 = 1;
pub const HOST_WINDOW_STATUS: u32 = 2;
pub const HOST_WINDOW_KIND_COUNT: usize = 3;

/// CMD_HOST_RENDER_SETUP 响应的单条通道描述（Windows；对齐 C++ HostRenderSetupEntryHeader）
#[derive(Clone, Debug)]
pub struct HostRenderSetupEntry {
    pub window_kind: u32,
    pub max_buffer_size: u32,
    pub shm_name: String,
    pub event_name: String,
}

/// SHM 内 hit-rect 表条目（20B，对齐 C++ HostRenderHitRect；index<0 为翻页按钮）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRenderHitRect {
    pub index: i32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl HostRenderHitRect {
    pub const SIZE: usize = 20;
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.index.to_le_bytes());
        b[4..8].copy_from_slice(&self.x.to_le_bytes());
        b[8..12].copy_from_slice(&self.y.to_le_bytes());
        b[12..16].copy_from_slice(&self.w.to_le_bytes());
        b[16..20].copy_from_slice(&self.h.to_le_bytes());
        b
    }
}

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
// Focus Gained Payload (38 bytes: 旧 36 + disabled(1) + reason(1))
// ──────────────────────────────────────────────

/// 焦点获取载荷 (38 bytes: 旧 36 + disabled(1) + reason(1))
#[derive(Clone, Copy, Debug)]
pub struct FocusGainedPayload {
    pub caret: CaretPayload,
    pub client_token: u64,
    pub input_scope_mask: u64,
    pub disabled: u8,
    pub reason: u8,
}

impl FocusGainedPayload {
    pub const SIZE: usize = 38;

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        // 向后兼容：至少要有旧 36 字节；disabled/reason 缺省 0
        if buf.len() < 36 {
            return None;
        }
        let caret = CaretPayload::from_bytes(&buf[0..20])?;
        let client_token = u64::from_le_bytes([
            buf[20], buf[21], buf[22], buf[23], buf[24], buf[25], buf[26], buf[27],
        ]);
        let input_scope_mask = u64::from_le_bytes([
            buf[28], buf[29], buf[30], buf[31], buf[32], buf[33], buf[34], buf[35],
        ]);
        let disabled = if buf.len() >= 37 { buf[36] } else { 0 };
        let reason = if buf.len() >= 38 { buf[37] } else { 0 };
        Some(Self {
            caret,
            client_token,
            input_scope_mask,
            disabled,
            reason,
        })
    }
}

// ──────────────────────────────────────────────
// Input State Report Payload (14 bytes)
// ──────────────────────────────────────────────

/// compartment 变更时的最新输入态上报载荷 (14 bytes)
#[derive(Clone, Copy, Debug)]
pub struct InputStateReportPayload {
    pub pid: u32,
    pub disabled: u8,
    pub reason: u8,
    pub input_scope_mask: u64,
}

impl InputStateReportPayload {
    pub const SIZE: usize = 14;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[0..4].copy_from_slice(&self.pid.to_le_bytes());
        b[4] = self.disabled;
        b[5] = self.reason;
        b[6..14].copy_from_slice(&self.input_scope_mask.to_le_bytes());
        b
    }

    pub fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            pid: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            disabled: buf[4],
            reason: buf[5],
            input_scope_mask: u64::from_le_bytes(buf[6..14].try_into().ok()?),
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
    pub rect_count: u32,           // @40 候选 hit 矩形数
    pub rects_offset: u32,         // @44 hit 矩形表相对 SHM 基址偏移
    pub rendered_hover_index: i32, // @48 高亮候选索引（-1 无 / -2,-3 翻页）
    pub target_instance_id: u32,   // @52 darwin 忽略
    pub reserved: [u32; 2],        // @56..64
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
            rect_count: 0,
            rects_offset: 0,
            rendered_hover_index: -1,
            target_instance_id: 0,
            reserved: [0; 2],
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

#[cfg(test)]
mod input_diag_wire_tests {
    use super::*;

    #[test]
    fn focus_gained_backward_compat_36_bytes() {
        // 旧 36 字节载荷（无 disabled/reason）仍可解，新字段默认 0
        let mut buf = vec![0u8; 36];
        buf[20..28].copy_from_slice(&7u64.to_le_bytes()); // client_token
        buf[28..36].copy_from_slice(&(1u64 << 31).to_le_bytes()); // input_scope_mask
        let p = FocusGainedPayload::from_bytes(&buf).unwrap();
        assert_eq!(p.client_token, 7);
        assert_eq!(p.input_scope_mask, 1 << 31);
        assert_eq!(p.disabled, 0);
        assert_eq!(p.reason, 0);
    }

    #[test]
    fn focus_gained_reads_new_fields_38_bytes() {
        let mut buf = vec![0u8; 38];
        buf[36] = 1; // disabled
        buf[37] = 2; // reason
        let p = FocusGainedPayload::from_bytes(&buf).unwrap();
        assert_eq!(p.disabled, 1);
        assert_eq!(p.reason, 2);
    }

    #[test]
    fn input_state_report_roundtrip() {
        let r = InputStateReportPayload {
            pid: 4242,
            disabled: 1,
            reason: 1,
            input_scope_mask: 1 << 31,
        };
        let bytes = r.to_bytes();
        assert_eq!(bytes.len(), InputStateReportPayload::SIZE);
        let d = InputStateReportPayload::from_bytes(&bytes).unwrap();
        assert_eq!(d.pid, 4242);
        assert_eq!(d.disabled, 1);
        assert_eq!(d.reason, 1);
        assert_eq!(d.input_scope_mask, 1 << 31);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_render_header_field_offsets_match_go_swift() {
        // 对齐 Swift SharedMemoryReader.swift / Go binary_protocol.go 的命名字段偏移
        let mut h = SharedRenderHeader::new(
            0x11223344, 0x55667788, 0x99AABBCC, 0xDDEE0011, 0x22334455, 0x66778899,
        );
        h.sequence = 0xA1A2A3A4;
        h.rect_count = 0xB1B2B3B4;
        h.rects_offset = 0xC1C2C3C4;
        h.rendered_hover_index = -3;
        h.target_instance_id = 0xE1E2E3E4;
        let b = h.to_bytes();
        assert_eq!(b.len(), 64);
        assert_eq!(SharedRenderHeader::SIZE, 64);
        assert_eq!(&b[8..12], &0xA1A2A3A4u32.to_le_bytes()); // sequence @8
        assert_eq!(&b[40..44], &0xB1B2B3B4u32.to_le_bytes()); // rect_count @40
        assert_eq!(&b[44..48], &0xC1C2C3C4u32.to_le_bytes()); // rects_offset @44
        assert_eq!(&b[48..52], &(-3i32).to_le_bytes()); // rendered_hover_index @48
        assert_eq!(&b[52..56], &0xE1E2E3E4u32.to_le_bytes()); // target_instance_id @52
        assert_eq!(&b[56..64], &[0u8; 8]); // reserved[2] @56..64 = 0
    }
}
