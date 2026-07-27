//! 延迟处理器：启动时返回安全默认值，就绪后切换到真实处理器
//!
//! 与 Go 版本 `wind_input/internal/bridge/deferred_handler.go` 对齐。

use crate::handler::*;
use std::sync::{Arc, RwLock};
use tracing::info;

/// 延迟处理器：在服务初始化完成前返回安全默认值
pub struct DeferredHandler {
    inner: RwLock<Option<Arc<dyn MessageHandler>>>,
}

impl DeferredHandler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(None),
        })
    }

    /// 设置真实处理器（初始化完成后调用）
    pub fn set_ready(&self, handler: Arc<dyn MessageHandler>) {
        info!("DeferredHandler: switching to real handler");
        *self.inner.write().unwrap() = Some(handler);
    }

    /// 检查是否已就绪
    pub fn is_ready(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    /// 获取内部真实处理器（如果已就绪）
    fn with_handler<F, R>(&self, default: R, f: F) -> R
    where
        F: FnOnce(&dyn MessageHandler) -> R,
    {
        let guard = self.inner.read().unwrap();
        match guard.as_ref() {
            Some(handler) => f(handler.as_ref()),
            None => default,
        }
    }
}

impl MessageHandler for DeferredHandler {
    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction {
        self.with_handler(KeyAction::PassThrough, |h| h.handle_key_event(data))
    }

    /// bridge 真正入口（server.rs 调 policed）。必须转发到内层处理器的 policed，
    /// 否则内层 Coordinator 重写的 policed（含统计埋点 + preedit 占位后处理）被跳过——
    /// 只走 trait 默认实现（仅调内层 handle_key_event），导致上屏统计在生产中恒为 0。
    fn handle_key_event_policed(&self, data: &KeyEventData) -> KeyAction {
        self.with_handler(KeyAction::PassThrough, |h| h.handle_key_event_policed(data))
    }

    fn preedit_uses_placeholder(&self) -> bool {
        self.with_handler(false, |h| h.preedit_uses_placeholder())
    }

    fn handle_focus_gained(&self, data: &FocusData) -> Option<StatusUpdateData> {
        self.with_handler(None, |h| h.handle_focus_gained(data))
    }

    fn handle_focus_lost(&self, client_token: u64, reason: wind_ipc::protocol::FocusLostReason) {
        self.with_handler((), |h| h.handle_focus_lost(client_token, reason))
    }

    fn handle_show_context_menu(&self, x: i32, y: i32) {
        self.with_handler((), |h| h.handle_show_context_menu(x, y))
    }

    fn query_menu_encoded(&self, simplified: bool) -> Vec<u8> {
        self.with_handler(Vec::new(), |h| h.query_menu_encoded(simplified))
    }

    fn handle_menu_action_id(&self, id: i32) {
        self.with_handler((), |h| h.handle_menu_action_id(id))
    }

    fn handle_candidate_select(&self, page_local_index: i32) {
        self.with_handler((), |h| h.handle_candidate_select(page_local_index))
    }

    fn handle_candidate_scroll(&self, delta: i32) {
        self.with_handler((), |h| h.handle_candidate_scroll(delta))
    }

    fn handle_candidate_hover(&self, page_local_index: i32) {
        self.with_handler((), |h| h.handle_candidate_hover(page_local_index))
    }

    fn handle_candidate_context_menu(&self, page_local_index: i32, action: &str) {
        self.with_handler((), |h| {
            h.handle_candidate_context_menu(page_local_index, action)
        })
    }

    fn handle_front_context(&self, app: &str, title: &str, sel: &str) {
        self.with_handler((), |h| h.handle_front_context(app, title, sel))
    }

    fn handle_ime_activated(&self, client_token: u64) -> Option<StatusUpdateData> {
        self.with_handler(None, |h| h.handle_ime_activated(client_token))
    }

    fn handle_ime_deactivated(&self, client_token: u64) {
        self.with_handler((), |h| h.handle_ime_deactivated(client_token))
    }

    fn handle_mode_notify(&self, flags: u32) {
        self.with_handler((), |h| h.handle_mode_notify(flags))
    }

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        self.with_handler((None, String::new()), |h| h.handle_toggle_mode())
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        self.with_handler((None, String::new()), |h| {
            h.handle_system_mode_switch(chinese_mode)
        })
    }

    fn handle_menu_command(&self, command: &str) -> Option<StatusUpdateData> {
        self.with_handler(None, |h| h.handle_menu_command(command))
    }

    fn handle_composition_terminated(&self) {
        self.with_handler((), |h| h.handle_composition_terminated())
    }

    fn handle_caret_update(&self, data: &CaretData) {
        self.with_handler((), |h| h.handle_caret_update(data))
    }

    // ⚠ 必须显式转发，不能吃 trait 默认实现：默认实现调的是 `self.handle_caret_update`，
    // 而这里的 `self` 是本包装器 → 又转发到内层的 handle_caret_update，内层的
    // handle_focus_gained_caret 永远不会被调用，副作用（消费首显等待→立即显示候选）
    // 原封不动地回来。整条链每一步都合法，编译器不会报错，只能靠真机日志发现。
    // 本 trait 今后新增带默认实现的方法时，这里都要跟着补一条转发。
    fn handle_focus_gained_caret(&self, data: &CaretData) {
        self.with_handler((), |h| h.handle_focus_gained_caret(data))
    }

    fn handle_caret_probe(&self, data: &CaretData) {
        self.with_handler((), |h| h.handle_caret_probe(data))
    }

    fn handle_caret_pending(&self) {
        self.with_handler((), |h| h.handle_caret_pending())
    }

    fn handle_selection_changed(&self, prev_char: u16) {
        self.with_handler((), |h| h.handle_selection_changed(prev_char))
    }

    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData> {
        self.with_handler(None, |h| h.handle_commit_request(data))
    }

    fn handle_host_render_failed(&self, reason: u32) {
        self.with_handler((), |h| h.handle_host_render_failed(reason))
    }

    fn get_current_mode(&self, client_token: u64) -> (bool, bool) {
        // 未就绪时回中文模式（安全默认）；就绪后委派真实处理器读权威模式。
        self.with_handler((true, false), |h| h.get_current_mode(client_token))
    }
}
