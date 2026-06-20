//! 生命周期：配置重载、服务重启、独占模式进入/复位。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 注：IME 激活/失活、焦点变更、composition 终止是 MessageHandler trait 方法，留在
//! coordinator.rs 的 `impl MessageHandler` 块。

use crate::coordinator::{Coordinator, State};
use crate::pipeline::ModeKind;
use tracing::{debug, info};
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_ipc::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT};
use wind_keys::keymap;
use wind_ui::manager::UiCommand;

impl Coordinator {
    /// 重启服务进程：隐藏 UI 后向 main 发重启信号（main 释放单例并重拉自身）。
    pub(crate) fn restart_service(&self) {
        info!("Restart service requested from menu");
        self.notify_ui_hide();
        let _ = self.ui_tx.send(UiCommand::HideToolbar);
        crate::request_restart();
    }

    /// 重载配置（best-effort：重新下发当前主题）。
    pub(crate) fn reload_config(&self) {
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        self.push_theme(&name, dark);
        self.show_tip("已重载");
    }

    /// 空缓冲模式激活的单一入口（对齐 key-pipeline.md §2.1 优先级链）。
    /// 优先级：临时英文(Shift+字母) > 快捷输入 > 临时拼音 > 特殊模式。命中返回激活 KeyAction，
    /// 都不命中返回 None（落普通输入）。URL 前缀夺取是「缓冲扩展夺取」语义，不在此链，单独处理。
    pub(crate) fn try_activate_mode(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        // 临时英文：Shift+字母（空缓冲 + 无候选 + 已启用）
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && self.rt().config.input.shift_temp_english.enabled
            && data.modifiers & MOD_SHIFT != 0
            && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
            && (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code)
        {
            let ch = (b'A' + (data.key_code - 0x41) as u8) as char; // 首字母大写
            state.active = Some(ModeKind::TempEnglish);
            state.temp_english_buffer = ch.to_string();
            self.update_temp_english_candidates(state);
            let disp = state.preedit.clone();
            self.notify_ui_update(state);
            debug!("Entered temp English mode (buffer={})", disp);
            return Some(KeyAction::UpdateComposition {
                text: disp.clone(),
                caret_pos: disp.chars().count() as u32,
            });
        }

        // 快捷输入已退役为内置类方案 mix 成员（quick_input），不再独立激活：
        // 想要纯快捷输入，配一个 members=["quick_input"] 的 mix 即可。; 默认走「快捷」融合 mix。

        // 临时拼音：码表方案 + 空缓冲 + 匹配触发键 + 无修饰键（不要求候选空）
        if state.input_buffer.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && self.is_temp_pinyin_trigger(data.key_code)
            && let Some(target) = self.engine_mgr.temp_pinyin_target()
        {
            state.active = Some(ModeKind::TempPinyin);
            state.temp_pinyin_schema = target;
            state.temp_pinyin_buffer.clear();
            state.temp_pinyin_prefix = Self::temp_pinyin_prefix_for(data.key_code).to_string();
            self.update_temp_pinyin_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            debug!(
                "Entered temp pinyin mode (prefix={})",
                state.temp_pinyin_prefix
            );
            return Some(KeyAction::UpdateComposition {
                text: display.clone(),
                caret_pos: display.chars().count() as u32,
            });
        }

        // 特殊模式：空缓冲 + 无候选 + 无修饰键 + 引导键匹配（优先级最低）。
        // 码表不可用时不拦截该键，返回 None 继续普通流程。
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
        {
            if let Some(idx) = self.match_special_trigger(data.key_code) {
                // 方案可加载才进入（否则不拦截该键，落普通流程）。
                if let Some(schema) = self.special_schema(idx)
                    && self.engine_mgr.ensure_schema(&schema)
                {
                    return Some(self.enter_special_mode(state, idx, data.key_code));
                }
            }
            // 临时 mix：含 quick_input 或至少一个可加载成员方案才进入（优先级最低）。
            if let Some(idx) = self.match_mix_trigger(data.key_code)
                && (self.mix_has_quick_input(idx) || !self.mix_members(idx).is_empty())
            {
                return Some(self.enter_mix_mode(state, idx, data.key_code));
            }
        }

        None
    }

    /// 复位三种独占输入模式（临时英文/临时拼音/快捷输入）的状态。仅清空，不负责上屏；
    /// 调用方需在调用前取出待上屏文本（如模式切换时的临时英文缓冲）。
    pub(crate) fn reset_exclusive_modes(&self, state: &mut State) {
        let dirty = state.active.is_some();
        state.active = None;
        state.temp_english_buffer.clear();
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.url_buffer.clear();
        state.rewind = None;
        state.special_buffer.clear();
        state.mix_buffer.clear();
        state.mix_numeric = false;
        // 清理可能残留的组合显示（临时拼音/快捷输入会产生候选与 preedit）
        state.input_buffer.clear();
        state.candidates.clear();
        state.preedit.clear();
        // 拼音逐步转换的已转换前缀一并丢弃（焦点/模式切换不保留半成品组合）。
        state.committed_text.clear();
        state.committed_segs.clear();
        // 焦点/模式切换：解除智能符号待命，避免跨上下文误触发替换。
        self.disarm_smart_symbol();
        if dirty {
            debug!("reset_exclusive_modes: cleared residual exclusive input mode state");
        }
    }
}
