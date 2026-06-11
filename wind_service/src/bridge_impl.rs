//! 最小可输入协调器：实现基本中文拼音输入
//!
//! 与 Go 版本 coordinator 对齐的协议行为：
//! - IME_ACTIVATED / FOCUS_GAINED 返回 StatusUpdateData，由 push pipe 推送
//! - TOGGLE_MODE / SYSTEM_MODE_SWITCH 返回 (StatusUpdateData, commitText)
//! - MENU_COMMAND 返回 StatusUpdateData
//! - COMMIT_REQUEST 实现 barrier 机制

use std::sync::{Arc, Mutex};
use wind_bridge::handler::*;
use wind_bridge::push::PushServer;
use wind_ipc::protocol::{EVENT_KEY_DOWN, MOD_CTRL, MOD_ALT};
use tracing::{info, debug};

/// 候选词
#[derive(Debug, Clone)]
struct Candidate {
    text: String,
    code: String,
}

/// 协调器状态
struct State {
    /// 中文模式
    chinese_mode: bool,
    /// 全角模式
    full_width: bool,
    /// 中文标点
    chinese_punct: bool,
    /// 工具栏可见
    toolbar_visible: bool,
    /// CapsLock
    caps_lock: bool,
    /// 输入缓冲区（拼音）
    input_buffer: String,
    /// 当前候选词列表
    candidates: Vec<Candidate>,
    /// 光标位置
    caret_x: i32,
    caret_y: i32,
    caret_height: i32,
}

/// 最小可输入协调器
pub struct MinimalCoordinator {
    state: Mutex<State>,
    push_server: Arc<PushServer>,
}

impl MinimalCoordinator {
    pub fn new(push_server: Arc<PushServer>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: true,
                full_width: false,
                chinese_punct: true,
                toolbar_visible: true,
                caps_lock: false,
                input_buffer: String::new(),
                candidates: Vec::new(),
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
            }),
            push_server,
        })
    }

    /// 构建当前状态的 StatusUpdateData
    fn build_status(&self) -> StatusUpdateData {
        let state = self.state.lock().unwrap();
        StatusUpdateData {
            chinese_mode: state.chinese_mode,
            full_width: state.full_width,
            chinese_punct: state.chinese_punct,
            toolbar_visible: state.toolbar_visible,
            caps_lock: state.caps_lock,
            icon_label: if state.chinese_mode { "中".to_string() } else { "英".to_string() },
            key_down_hotkeys: vec![],
            key_up_hotkeys: vec![],
        }
    }

    /// 推送 ActivationStatusPush 到 push pipe
    ///
    /// 与 Go 版 PushActivationStatusToActiveClient 对齐：
    /// 使用 CMD_ACTIVATION_STATUS_PUSH 命令码，载荷含完整状态。
    fn push_activation_status(&self) {
        let status = self.build_status();
        info!("Pushing ActivationStatusPush: chinese_mode={}", status.chinese_mode);
        let encoded = wind_ipc::codec::encode_activation_status_push(
            status.chinese_mode,
            status.full_width,
            status.chinese_punct,
            status.toolbar_visible,
            status.caps_lock,
            false, // host_render_avail（最小实现无 host render）
            &status.key_down_hotkeys,
            &status.key_up_hotkeys,
            &status.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    /// 推送 StatePush 到 push pipe
    ///
    /// 与 Go 版 PushStateToActiveClient 对齐：
    /// 使用 CMD_STATE_PUSH 命令码，不含 hotkeys。
    fn push_state_update(&self) {
        let status = self.build_status();
        debug!("Pushing StatePush: chinese_mode={}", status.chinese_mode);
        let encoded = wind_ipc::codec::encode_state_push(
            status.chinese_mode,
            status.full_width,
            status.chinese_punct,
            status.toolbar_visible,
            status.caps_lock,
            &status.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    /// 根据输入缓冲区更新候选词
    fn update_candidates(state: &mut State) {
        state.candidates.clear();
        if state.input_buffer.is_empty() {
            return;
        }

        let input = &state.input_buffer;
        let dict = get_builtin_dict();

        // 精确匹配
        for entry in dict {
            if entry.code == *input {
                state.candidates.push(Candidate {
                    text: entry.text.to_string(),
                    code: entry.code.to_string(),
                });
            }
        }

        // 前缀匹配（如果精确匹配没有结果）
        if state.candidates.is_empty() {
            for entry in dict {
                if entry.code.starts_with(input) {
                    state.candidates.push(Candidate {
                        text: entry.text.to_string(),
                        code: entry.code.to_string(),
                    });
                }
            }
        }

        state.candidates.truncate(9);
    }
}

/// 内置词典条目
struct DictEntry {
    code: &'static str,
    text: &'static str,
}

/// 内置简单拼音词典（用于测试）
fn get_builtin_dict() -> &'static [DictEntry] {
    &[
        DictEntry { code: "ni", text: "你" },
        DictEntry { code: "ni", text: "尼" },
        DictEntry { code: "ni", text: "泥" },
        DictEntry { code: "hao", text: "好" },
        DictEntry { code: "hao", text: "号" },
        DictEntry { code: "hao", text: "毫" },
        DictEntry { code: "wo", text: "我" },
        DictEntry { code: "wo", text: "握" },
        DictEntry { code: "ta", text: "他" },
        DictEntry { code: "ta", text: "她" },
        DictEntry { code: "ta", text: "它" },
        DictEntry { code: "shi", text: "是" },
        DictEntry { code: "shi", text: "十" },
        DictEntry { code: "shi", text: "时" },
        DictEntry { code: "shi", text: "事" },
        DictEntry { code: "de", text: "的" },
        DictEntry { code: "de", text: "得" },
        DictEntry { code: "de", text: "地" },
        DictEntry { code: "le", text: "了" },
        DictEntry { code: "le", text: "乐" },
        DictEntry { code: "bu", text: "不" },
        DictEntry { code: "bu", text: "部" },
        DictEntry { code: "bu", text: "步" },
        DictEntry { code: "zai", text: "在" },
        DictEntry { code: "zai", text: "再" },
        DictEntry { code: "zai", text: "载" },
        DictEntry { code: "ren", text: "人" },
        DictEntry { code: "ren", text: "认" },
        DictEntry { code: "ren", text: "任" },
        DictEntry { code: "zhong", text: "中" },
        DictEntry { code: "zhong", text: "重" },
        DictEntry { code: "zhong", text: "种" },
        DictEntry { code: "guo", text: "国" },
        DictEntry { code: "guo", text: "过" },
        DictEntry { code: "guo", text: "果" },
        DictEntry { code: "da", text: "大" },
        DictEntry { code: "da", text: "打" },
        DictEntry { code: "da", text: "达" },
        DictEntry { code: "xue", text: "学" },
        DictEntry { code: "xue", text: "雪" },
        DictEntry { code: "sheng", text: "生" },
        DictEntry { code: "sheng", text: "声" },
        DictEntry { code: "sheng", text: "省" },
        DictEntry { code: "ri", text: "日" },
        DictEntry { code: "ri", text: "入" },
        DictEntry { code: "yi", text: "一" },
        DictEntry { code: "yi", text: "以" },
        DictEntry { code: "yi", text: "已" },
        DictEntry { code: "er", text: "二" },
        DictEntry { code: "er", text: "而" },
        DictEntry { code: "er", text: "耳" },
        DictEntry { code: "san", text: "三" },
        DictEntry { code: "san", text: "散" },
        DictEntry { code: "si", text: "四" },
        DictEntry { code: "si", text: "死" },
        DictEntry { code: "si", text: "思" },
        DictEntry { code: "wu", text: "五" },
        DictEntry { code: "wu", text: "无" },
        DictEntry { code: "wu", text: "物" },
        DictEntry { code: "liu", text: "六" },
        DictEntry { code: "liu", text: "流" },
        DictEntry { code: "liu", text: "留" },
        DictEntry { code: "qi", text: "七" },
        DictEntry { code: "qi", text: "起" },
        DictEntry { code: "qi", text: "气" },
        DictEntry { code: "ba", text: "八" },
        DictEntry { code: "ba", text: "把" },
        DictEntry { code: "ba", text: "吧" },
        DictEntry { code: "jiu", text: "九" },
        DictEntry { code: "jiu", text: "就" },
        DictEntry { code: "jiu", text: "久" },
        DictEntry { code: "ling", text: "零" },
        DictEntry { code: "ling", text: "领" },
        DictEntry { code: "ling", text: "令" },
        DictEntry { code: "nihao", text: "你好" },
        DictEntry { code: "women", text: "我们" },
        DictEntry { code: "tamen", text: "他们" },
        DictEntry { code: "shijie", text: "世界" },
        DictEntry { code: "zhongguo", text: "中国" },
        DictEntry { code: "daxue", text: "大学" },
        DictEntry { code: "xuesheng", text: "学生" },
        DictEntry { code: "laoshi", text: "老师" },
        DictEntry { code: "pengyou", text: "朋友" },
        DictEntry { code: "shijian", text: "时间" },
        DictEntry { code: "jintian", text: "今天" },
        DictEntry { code: "mingtian", text: "明天" },
        DictEntry { code: "zuotian", text: "昨天" },
        DictEntry { code: "henhao", text: "很好" },
        DictEntry { code: "bucuo", text: "不错" },
        DictEntry { code: "keyi", text: "可以" },
        DictEntry { code: "xiexie", text: "谢谢" },
        DictEntry { code: "duibuqi", text: "对不起" },
        DictEntry { code: "meiguanxi", text: "没关系" },
        DictEntry { code: "zaijian", text: "再见" },
    ]
}

impl MessageHandler for MinimalCoordinator {
    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction {
        if data.event_type != EVENT_KEY_DOWN {
            return KeyAction::PassThrough;
        }

        let mut state = self.state.lock().unwrap();

        // Shift 键切换中英文模式
        if data.key_code == 0xA0 || data.key_code == 0xA1 {
            if state.input_buffer.is_empty() {
                state.chinese_mode = !state.chinese_mode;
                drop(state);
                self.push_state_update();
                return KeyAction::StatusUpdate(self.build_status());
            }
        }

        // 英文模式：直接透传
        if !state.chinese_mode {
            return KeyAction::PassThrough;
        }

        // Ctrl/Alt 组合键透传
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            if !state.input_buffer.is_empty() {
                state.input_buffer.clear();
                state.candidates.clear();
                return KeyAction::ClearComposition;
            }
            return KeyAction::PassThrough;
        }

        match data.key_code {
            0x1B => {
                // Escape
                state.input_buffer.clear();
                state.candidates.clear();
                KeyAction::ClearComposition
            }
            0x08 => {
                // Backspace
                if !state.input_buffer.is_empty() {
                    state.input_buffer.pop();
                    Self::update_candidates(&mut state);
                    if state.input_buffer.is_empty() {
                        KeyAction::ClearComposition
                    } else {
                        KeyAction::UpdateComposition {
                            text: state.input_buffer.clone(),
                            caret_pos: state.input_buffer.len() as u32,
                        }
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x20 => {
                // Space — 提交第一个候选或原始输入
                if !state.candidates.is_empty() {
                    let text = state.candidates[0].text.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x0D => {
                // Enter — 提交原始输入
                if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x31..=0x39 => {
                // 数字键 1-9 选择候选
                let idx = (data.key_code - 0x31) as usize;
                if idx < state.candidates.len() {
                    let text = state.candidates[idx].text.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x41..=0x5A => {
                // A-Z 字母键
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.input_buffer.push(ch);
                Self::update_candidates(&mut state);
                let display = build_preedit_display(&state.input_buffer, &state.candidates);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos: state.input_buffer.len() as u32,
                }
            }
            _ => {
                if !state.input_buffer.is_empty() {
                    KeyAction::Consumed
                } else {
                    KeyAction::PassThrough
                }
            }
        }
    }

    /// 焦点获取：保存光标位置，推送 ActivationStatusPush
    fn handle_focus_gained(&self, data: &FocusData) -> Option<StatusUpdateData> {
        {
            let mut state = self.state.lock().unwrap();
            state.caret_x = data.x;
            state.caret_y = data.y;
            state.caret_height = data.height;
        }
        // 与 Go 版 HandleFocusGained 对齐：返回状态用于 ActivationStatusPush
        let status = self.build_status();
        self.push_activation_status();
        Some(status)
    }

    fn handle_focus_lost(&self) {
        // 最小实现：不做特殊处理
    }

    /// IME 激活：推送 ActivationStatusPush（含完整状态 + hotkeys）
    fn handle_ime_activated(&self, _client_token: u64) -> Option<StatusUpdateData> {
        info!("IME activated, pushing ActivationStatusPush");
        let status = self.build_status();
        self.push_activation_status();
        Some(status)
    }

    fn handle_ime_deactivated(&self) {
        // 最小实现：不做特殊处理
    }

    fn handle_mode_notify(&self, flags: u32) {
        // 解析 flags 中的模式状态
        let chinese_mode = (flags & wind_ipc::protocol::STATUS_CHINESE_MODE) != 0;
        let clear_input = (flags & wind_ipc::protocol::STATUS_MODE_CHANGED) != 0;
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = chinese_mode;
        if clear_input {
            state.input_buffer.clear();
            state.candidates.clear();
        }
    }

    /// 模式切换（同步）：返回状态和可选的待提交文本
    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = !state.chinese_mode;

        // 如果有待提交输入，切换模式时提交
        let commit_text = if !state.input_buffer.is_empty() && !state.chinese_mode {
            let text = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            text
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };

        drop(state);

        // 推送状态到 push pipe
        self.push_state_update();

        let status = self.build_status();
        (Some(status), commit_text)
    }

    /// 系统模式切换（同步）：系统已决定目标模式，Go 必须 follow
    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = chinese_mode;

        // 如果有待提交输入，切换模式时提交
        let commit_text = if !state.input_buffer.is_empty() && !chinese_mode {
            let text = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            text
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };

        drop(state);

        // 推送状态到 push pipe
        self.push_state_update();

        let status = self.build_status();
        (Some(status), commit_text)
    }

    /// 菜单命令：返回状态更新
    fn handle_menu_command(&self, command: &str) -> Option<StatusUpdateData> {
        info!("Menu command: {}", command);
        match command {
            "toggle_mode" => {
                let (status, _) = self.handle_toggle_mode();
                status
            }
            "toggle_width" => {
                let mut state = self.state.lock().unwrap();
                state.full_width = !state.full_width;
                drop(state);
                self.push_state_update();
                Some(self.build_status())
            }
            "toggle_punct" => {
                let mut state = self.state.lock().unwrap();
                state.chinese_punct = !state.chinese_punct;
                drop(state);
                self.push_state_update();
                Some(self.build_status())
            }
            _ => {
                debug!("Unknown menu command: {}", command);
                None
            }
        }
    }

    fn handle_composition_terminated(&self) {
        let mut state = self.state.lock().unwrap();
        state.input_buffer.clear();
        state.candidates.clear();
    }

    fn handle_caret_update(&self, data: &CaretData) {
        let mut state = self.state.lock().unwrap();
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
    }

    fn handle_caret_pending(&self) {
        // 最小实现：不做特殊处理
    }

    fn handle_selection_changed(&self, _prev_char: u16) {
        // 最小实现：不做特殊处理
    }

    /// 提交请求（barrier 机制）
    ///
    /// 与 Go 版 HandleCommitRequest 对齐：
    /// 根据 triggerKey 决定提交行为，返回 CommitResultData。
    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData> {
        let mut state = self.state.lock().unwrap();

        if state.input_buffer.is_empty() {
            return None;
        }

        let trigger_key = data.trigger_key;
        let text = if trigger_key == 0x20 {
            // Space — 提交第一个候选或原始输入
            if !state.candidates.is_empty() {
                state.candidates[0].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else if trigger_key == 0x0D {
            // Enter — 提交原始输入
            state.input_buffer.clone()
        } else if trigger_key >= 0x31 && trigger_key <= 0x39 {
            // 数字键 1-9 — 选择候选
            let idx = (trigger_key - 0x31) as usize;
            if idx < state.candidates.len() {
                state.candidates[idx].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else {
            state.input_buffer.clone()
        };

        state.input_buffer.clear();
        state.candidates.clear();

        Some(CommitResultData {
            barrier_seq: data.barrier_seq,
            text,
            new_composition: String::new(),
            mode_changed: false,
            chinese_mode: state.chinese_mode,
        })
    }

    fn handle_host_render_request(&self) {
        // 最小实现：不做特殊处理
    }

    fn handle_host_render_ready(&self) {
        // 最小实现：不做特殊处理
    }
}

fn build_preedit_display(input: &str, candidates: &[Candidate]) -> String {
    let mut display = String::new();
    display.push_str(input);
    if !candidates.is_empty() {
        display.push_str(" [");
        for (i, cand) in candidates.iter().enumerate() {
            if i > 0 {
                display.push(' ');
            }
            display.push_str(&format!("{}.{}", i + 1, cand.text));
        }
        display.push(']');
    }
    display
}
