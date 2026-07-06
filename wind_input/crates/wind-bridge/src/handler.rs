//! MessageHandler trait：处理上游命令的接口
//!
//! 与 Go 版本 `bridge.MessageHandler` 对齐。

use wind_ipc::protocol::KeyPayload;

/// 按键事件数据
#[derive(Debug, Clone)]
pub struct KeyEventData {
    pub key_code: u32,
    pub scan_code: u32,
    pub modifiers: u32,
    pub event_type: u8,
    pub toggles: u8,
    pub event_seq: u16,
    pub prev_char: u16,
}

impl From<&KeyPayload> for KeyEventData {
    fn from(p: &KeyPayload) -> Self {
        Self {
            key_code: p.key_code,
            scan_code: p.scan_code,
            modifiers: p.modifiers,
            event_type: p.event_type,
            toggles: p.toggles,
            event_seq: p.event_seq,
            prev_char: p.prev_char,
        }
    }
}

/// 状态更新数据（与 Go StatusUpdateData 对齐）
#[derive(Debug, Clone, Default)]
pub struct StatusUpdateData {
    pub chinese_mode: bool,
    pub full_width: bool,
    pub chinese_punct: bool,
    pub toolbar_visible: bool,
    pub caps_lock: bool,
    pub icon_label: String,
    pub key_down_hotkeys: Vec<u32>,
    pub key_up_hotkeys: Vec<u32>,
}

/// 提交请求数据（barrier 机制）
#[derive(Debug, Clone)]
pub struct CommitRequestData {
    pub barrier_seq: u16,
    pub trigger_key: u16,
    pub modifiers: u32,
    pub input_buffer: String,
}

/// 提交结果数据（barrier 机制）
#[derive(Debug, Clone)]
pub struct CommitResultData {
    pub barrier_seq: u16,
    pub text: String,
    pub new_composition: String,
    pub mode_changed: bool,
    pub chinese_mode: bool,
}

/// 按键事件结果类型
#[derive(Debug, Clone)]
pub enum KeyAction {
    /// 插入文本
    InsertText {
        text: String,
        new_composition: Option<String>,
        mode_changed: bool,
        chinese_mode: bool,
        has_new_composition: bool,
    },
    /// 更新组合
    UpdateComposition { text: String, caret_pos: u32 },
    /// 清除组合
    ClearComposition,
    /// 透传给系统
    PassThrough,
    /// 状态更新（携带完整状态含 iconLabel）
    StatusUpdate(StatusUpdateData),
    /// 消费但不处理
    Consumed,
    /// 按键不处理（未匹配）
    NotHandled,
    /// 插入文本并定位光标
    InsertTextWithCursor { text: String, cursor_offset: u32 },
    /// 光标右移（智能跳过）
    MoveCursorRight,
    /// 删除配对（智能删除）
    DeletePair,
    /// 删除光标前 count 个字符并插入文本（智能符号替换）
    ReplaceBackward { count: u32, text: String },
    /// 持有组合态（智能符号 HoldComposition 方案）：
    /// C++ 端开启组合显示 text，在 timeout_ms 毫秒后自动提交中文；
    /// press2 到来时直接用英文文本替换组合（通过普通 InsertText / CommitText 提交）。
    HoldComposition { text: String, timeout_ms: u32 },
    /// 顶屏后开 HoldComposition（has_input + 智能符号 HoldComposition 组合路径）：
    /// 先提交 commit_text（候选/前缀），再将 hold_text（中文标点）放入 TSF 组合态，
    /// timeout_ms 后自动提交中文；press2 与普通 HoldComposition press2 路径一致。
    CommitAndHoldComposition {
        commit_text: String,
        hold_text: String,
        timeout_ms: u32,
    },
}

impl KeyAction {
    /// 非 app_inline（候选窗自行显示 preedit）时，应用侧组合串替换为单个占位空格、光标置前。
    /// 目的：保留一段组合串供应用上报 caret 坐标（候选窗定位），但不在应用内显示真实编码
    /// （避免与候选窗 preedit 重复）。对齐 Go 版"模拟空格 + 光标移前"。
    pub fn with_composition_placeholder(self) -> KeyAction {
        match self {
            KeyAction::UpdateComposition { text, .. } if !text.is_empty() => {
                KeyAction::UpdateComposition {
                    text: " ".to_string(),
                    caret_pos: 0,
                }
            }
            KeyAction::InsertText {
                text,
                new_composition: Some(c),
                mode_changed,
                chinese_mode,
                has_new_composition,
            } if !c.is_empty() => KeyAction::InsertText {
                text,
                new_composition: Some(" ".to_string()),
                mode_changed,
                chinese_mode,
                has_new_composition,
            },
            other => other,
        }
    }
}

/// 焦点数据
#[derive(Debug, Clone)]
pub struct FocusData {
    pub x: i32,
    pub y: i32,
    pub height: i32,
    pub composition_start_x: i32,
    pub composition_start_y: i32,
    pub client_token: u64,
    pub input_scope_mask: u64,
}

/// 光标位置数据
#[derive(Debug, Clone, Copy)]
pub struct CaretData {
    pub x: i32,
    pub y: i32,
    pub height: i32,
    pub composition_start_x: i32,
    pub composition_start_y: i32,
}

/// MessageHandler trait：协调器实现此接口处理各种事件
pub trait MessageHandler: Send + Sync {
    /// 处理按键事件
    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction;

    /// 应用侧组合串是否使用占位空格（候选窗显示 preedit 的非 app_inline 模式）。默认否（app_inline）。
    fn preedit_uses_placeholder(&self) -> bool {
        false
    }

    /// 处理按键并按 preedit 显示策略后处理组合串（bridge 入口应调用此方法）。
    fn handle_key_event_policed(&self, data: &KeyEventData) -> KeyAction {
        let action = self.handle_key_event(data);
        if self.preedit_uses_placeholder() {
            action.with_composition_placeholder()
        } else {
            action
        }
    }

    /// 处理焦点获取（返回状态用于 ActivationStatusPush）
    fn handle_focus_gained(&self, data: &FocusData) -> Option<StatusUpdateData>;

    /// 处理焦点丢失
    fn handle_focus_lost(&self);

    /// 处理 IME 激活（返回状态用于 ActivationStatusPush）
    fn handle_ime_activated(&self, client_token: u64) -> Option<StatusUpdateData>;

    /// 处理 IME 停用
    fn handle_ime_deactivated(&self);

    /// 处理模式通知
    fn handle_mode_notify(&self, flags: u32);

    /// 处理模式切换（返回状态和可选的待提交文本）
    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String);

    /// 处理系统模式切换（返回状态和可选的待提交文本）
    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String);

    /// 处理菜单命令（返回状态更新）
    fn handle_menu_command(&self, command: &str) -> Option<StatusUpdateData>;

    /// 处理组合终止
    fn handle_composition_terminated(&self);

    /// 处理光标位置更新
    fn handle_caret_update(&self, data: &CaretData);

    /// 处理光标待定（composition 刚启动，真正 caret 在 reflow 后到达）
    fn handle_caret_pending(&self);

    /// 处理选区变化
    fn handle_selection_changed(&self, prev_char: u16);

    /// 处理提交请求（barrier 机制）
    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData>;

    /// 处理 Host Render 失败上报（DLL 侧初始化/SHM 映射失败等，reason 为失败码）。
    /// setup 应答与断线清理由 bridge 服务器直接经 HostRenderManager 完成，不经此 trait；
    /// 此回调仅用于让协调器记录/告警。默认空实现。
    fn handle_host_render_failed(&self, _reason: u32) {}

    /// 显示功能主菜单（任务栏输入法指示右键）。x/y 为屏幕坐标；
    /// i32::MIN 表示坐标缺失（由 UI 取光标位置）。默认空实现。
    fn handle_show_context_menu(&self, _x: i32, _y: i32) {}

    /// 处理 TSF 侧上报的英文模式输入统计（CMD_INPUT_STATS，异步，无响应）。
    /// chars = a-z/A-Z 字符数; digits = 数字键(0-9/numpad); puncts = 符号键; spaces = 空格键。
    /// 对齐 Go `RecordTSFEnglish`。默认空实现（无统计的 handler 静默忽略）。
    fn handle_english_stats(&self, _chars: u32, _digits: u32, _puncts: u32, _spaces: u32) {}

    /// 鼠标点选候选（darwin .app / Windows host-render DLL，页内下标）。
    /// 负值为翻页按钮：-1 上页 / -2 下页（与 SHM 命中矩形及 C++ _OnMouseClick 约定一致）。默认空。
    fn handle_candidate_select(&self, _page_local_index: i32) {}

    /// host 候选框鼠标滚轮（delta 为 WHEEL_DELTA 倍数，正=上滚）。
    /// 对齐 Go HandleCandidateScroll：默认不做任何动作（本地候选窗无滚轮翻页），
    /// 统一接入点便于后续按配置实现滚轮行为。默认空。
    fn handle_candidate_scroll(&self, _delta: i32) {}

    /// darwin: .app 鼠标 hover 候选（页内下标，-1=无）。默认空。
    fn handle_candidate_hover(&self, _page_local_index: i32) {}

    /// darwin: 查询功能菜单，返回已编码的 `CmdMenuShow` 帧字节（响应 `CmdShowContextMenu`）。
    /// `simplified=true` 为 IMK 输入源菜单用的精简树(无子菜单)，false 为候选框右键/菜单栏
    /// 指示器用的完整树(带子菜单)。`.app` 端解码为原生 NSMenu。默认返回空 `Vec`。
    fn query_menu_encoded(&self, _simplified: bool) -> Vec<u8> {
        Vec::new()
    }

    /// darwin: .app 回传统一菜单项选择（菜单 id）。默认空。
    fn handle_menu_action_id(&self, _id: i32) {}

    /// darwin: .app 候选右键上下文菜单动作（页内下标 + 动作串）。
    /// action ∈ {move_top, move_up, move_down, delete, reset_default, copy}。默认空。
    fn handle_candidate_context_menu(&self, _page_local_index: i32, _action: &str) {}

    /// darwin: .app 上报前台上下文（app bundle id / 窗口标题 / 选中文本），供命令直通车
    /// app()/title()/sel() 取值。聚焦时快照，缺 AX/IMKit 支持的字段为空串。默认空实现。
    fn handle_front_context(&self, _app: &str, _title: &str, _sel: &str) {}

    /// 返回当前权威模式 (chinese_mode, full_width)，供 FocusGained 同步路径回传 ModePush。
    /// `client_token`（PID<<32|instance）标识焦点进程：state_scope="app" 时按进程切换记忆状态。
    /// 必须极轻量（仅锁+内存查询），不得有任何阻塞/跨进程调用——DLL 正同步阻塞等本值。
    /// 与 Go `MessageHandler.GetCurrentMode` 对齐。默认返回中文模式（安全默认）。
    fn get_current_mode(&self, _client_token: u64) -> (bool, bool) {
        (true, false)
    }
}
