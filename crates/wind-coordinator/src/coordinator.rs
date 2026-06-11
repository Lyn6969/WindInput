//! 中央协调器
//!
//! 与 Go 版本 `wind_input/internal/coordinator/coordinator.go` 对齐。

use std::sync::{Arc, Mutex};
use wind_bridge::handler::*;

/// 中央协调器
pub struct Coordinator {
    mu: Mutex<CoordinatorState>,
    // TODO: engine, ui, config, bridge, etc.
}

struct CoordinatorState {
    chinese_mode: bool,
    caps_lock_on: bool,
    sensitive_field_active: bool,
    full_width: bool,
    chinese_punct: bool,
    input_buffer: String,
    input_cursor_pos: usize,
}

impl Coordinator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mu: Mutex::new(CoordinatorState {
                chinese_mode: true,
                caps_lock_on: false,
                sensitive_field_active: false,
                full_width: false,
                chinese_punct: true,
                input_buffer: String::new(),
                input_cursor_pos: 0,
            }),
        })
    }
}

impl MessageHandler for Coordinator {
    fn handle_key_event(&self, _data: &KeyEventData) -> KeyAction {
        // TODO: 完整的按键路由逻辑
        KeyAction::PassThrough
    }

    fn handle_focus_gained(&self, _data: &FocusData) -> Option<StatusUpdateData> {
        None
    }

    fn handle_focus_lost(&self) {}

    fn handle_ime_activated(&self, _client_token: u64) -> Option<StatusUpdateData> {
        None
    }

    fn handle_ime_deactivated(&self) {}

    fn handle_mode_notify(&self, _flags: u32) {}

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        let mut state = self.mu.lock().unwrap();
        state.chinese_mode = !state.chinese_mode;
        (None, String::new())
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.mu.lock().unwrap();
        state.chinese_mode = chinese_mode;
        (None, String::new())
    }

    fn handle_menu_command(&self, _command: &str) -> Option<StatusUpdateData> {
        None
    }

    fn handle_composition_terminated(&self) {
        let mut state = self.mu.lock().unwrap();
        state.input_buffer.clear();
        state.input_cursor_pos = 0;
    }

    fn handle_caret_update(&self, _data: &CaretData) {}

    fn handle_caret_pending(&self) {}

    fn handle_selection_changed(&self, _prev_char: u16) {}

    fn handle_commit_request(&self, _data: &CommitRequestData) -> Option<CommitResultData> {
        None
    }

    fn handle_host_render_request(&self) {}

    fn handle_host_render_ready(&self) {}
}
