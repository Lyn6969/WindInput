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
        // 若有活跃 composition（拼音输入中/独占模式），先清空内部状态并通知 TSF 清除 composition，
        // 避免服务退出后 TSF 持有孤儿 composition 导致残留。
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let has_composition = !state.input_buffer.is_empty()
            || !state.preedit.is_empty()
            || !state.committed_text.is_empty()
            || state.active.is_some();
        if has_composition {
            self.reset_exclusive_modes(&mut state);
        }
        drop(state);
        if has_composition {
            let encoded = wind_ipc::codec::encode_clear_composition();
            self.push_server.push_to_active(&encoded);
        }
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
        let dark = *self.theme_style.lock().unwrap_or_else(|e| e.into_inner()) == 2;
        self.push_theme(&name, dark);
        // 不再弹「已重载」气泡：热重载统一由 reload_user_config 的 toast 通知，避免重复。
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
            && self.rt().config.input.temp_english.enabled
            && data.modifiers & MOD_SHIFT != 0
            && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
            && (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code)
        {
            let ch = (b'A' + (data.key_code - 0x41) as u8) as char; // 首字母大写
            // shift_behavior == "direct_commit"：不进临时英文，直接上屏大写字母（对齐 Go）。
            if self.rt().config.input.temp_english.shift_behavior == "direct_commit" {
                let out = if state.full_width {
                    wind_transform::fullwidth::to_full_width(&ch.to_string())
                } else {
                    ch.to_string()
                };
                return Some(Self::commit_action(out, true));
            }
            state.active = Some(ModeKind::TempEnglish);
            state.temp_english_buffer = ch.to_string();
            // Shift+字母进入时缓冲已含首字母：光标必须落到其后，否则续打会插到首字母之前。
            state.temp_english_cursor = state.temp_english_buffer.len();
            state.temp_english_prefix = String::new();
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

        // 临时英文触发键：符号键进入（空缓冲 + 无候选 + 已启用 + 无修饰键 + 匹配 trigger_keys）
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && self.rt().config.input.temp_english.enabled
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && self.is_temp_english_trigger(data.key_code)
        {
            let prefix = wind_keys::keymap::vk_to_prefix_char(data.key_code)
                .map(|c| c.to_string())
                .unwrap_or_default();
            state.active = Some(ModeKind::TempEnglish);
            state.temp_english_buffer.clear();
            state.temp_english_prefix = prefix;
            self.update_temp_english_candidates(state);
            let disp = state.preedit.clone();
            self.notify_ui_update(state);
            debug!(
                "Entered temp English mode via trigger key (prefix={})",
                state.temp_english_prefix
            );
            return Some(KeyAction::UpdateComposition {
                text: disp.clone(),
                caret_pos: disp.chars().count() as u32,
            });
        }

        // 临时拼音：空缓冲 + 匹配触发键 + 无修饰键（不要求候选空）。
        // 方案适用范围（仅码表/混输）由 temp_pinyin_target() 统一把关——它是所有进入点的
        // 公共门卫（引导键/字母触发/热键/顶屏进模式/z-fallback），判据放这里才不会漏网。
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

        // 临时拼音「字母触发键」（z）三重身份裁决（对齐 Go judgeZFirstTrigger；符号触发键在上方
        // is_temp_pinyin_trigger 分支处理，此处专司字母键）：码表引擎 + 空缓冲 + 无修饰键 +
        // 配置了该字母触发键。
        // ① z_key_repeat 开且有上屏历史 → 不进临拼（z 作 repeat，落普通输入由 update_candidates 注入
        //    重复候选）；② 该字母是活码前缀（码表/短语有以其开头的条目，如自定义 zhang）→ 不进临拼
        //    （作正常码字母）；③ 否则（死前缀 + 无 repeat，如标准五笔 z）→ 进临时拼音。
        if state.input_buffer.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && matches!(
                self.engine_mgr.current_engine_type(),
                Some(wind_engine::EngineType::CodeTable)
            )
            && let Some(letter) = self.matched_letter_temp_trigger(data.key_code)
            && let Some(target) = self.engine_mgr.temp_pinyin_target()
        {
            let repeat_active = self.z_key_repeat_text().is_some();
            if !repeat_active && !self.has_code_prefix(&letter.to_string()) {
                state.active = Some(ModeKind::TempPinyin);
                state.temp_pinyin_schema = target;
                state.temp_pinyin_buffer.clear();
                state.temp_pinyin_prefix = letter.to_string();
                state.rewind = None; // 首键进入非夺取式，作废任何旧回退登记
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                debug!("Entered temp pinyin via letter trigger '{}'", letter);
                return Some(KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                });
            }
            // ①/②：返回 None，z 落普通输入路径（buffer 变 "z"，repeat 注入 / 正常码累积；
            // 后续按字母若 z… 破前缀，由 try_z_fallback 夺取进临拼）。
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
        state.temp_english_cursor = 0;
        state.temp_english_prefix.clear();
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_cursor = 0;
        state.temp_pinyin_prefix.clear();
        state.url_buffer.clear();
        state.url_cursor = 0;
        state.rewind = None;
        state.special_buffer.clear();
        state.special_cursor = 0;
        state.mix_buffer.clear();
        state.mix_cursor = 0;
        state.mix_numeric = false;
        // 清理可能残留的组合显示（临时拼音/快捷输入会产生候选与 preedit）
        state.input_buffer.clear();
        state.input_cursor_pos = 0;
        state.candidates.clear();
        state.preedit.clear();
        // 拼音逐步转换的已转换前缀一并丢弃（焦点/模式切换不保留半成品组合）。
        state.committed_text.clear();
        state.committed_segs.clear();
        // 焦点/模式切换：解除智能符号待命，避免跨上下文误触发替换。
        self.disarm_smart_symbol();
        // 快捷输入「强制竖排」遗留：离开模式时恢复进入前布局。
        if let Some(prev) = state.quick_saved_vertical.take() {
            let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(prev));
        }
        // 快捷加词模式遗留：焦点/模式切换时退出并恢复布局。
        if state.add_word_active {
            state.add_word_active = false;
            state.add_word_chars.clear();
            state.add_word_len = 0;
            state.add_word_code.clear();
            if let Some(prev) = state.add_word_saved_vertical.take() {
                let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(prev));
            }
        }
        if dirty {
            debug!("reset_exclusive_modes: cleared residual exclusive input mode state");
        }
    }
}
