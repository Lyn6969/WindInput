//! 生命周期：配置重载、服务重启、独占模式进入/复位。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 注：IME 激活/失活、焦点变更、composition 终止是 MessageHandler trait 方法，留在
//! coordinator.rs 的 `impl MessageHandler` 块。

use crate::coordinator::{Coordinator, State, punct_char};
use crate::pipeline::ModeKind;
use tracing::{debug, info};
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_config::ZKeyAction;
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
        let dark = self.resolve_theme_dark();
        self.push_theme(&name, dark);
        // 不再弹「已重载」气泡：热重载统一由 reload_user_config 的 toast 通知，避免重复。
    }

    /// 该键是否被**任一模式**配作进入键（临拼/临英的符号触发键、特殊模式引导键、mix 触发键）。
    /// 仅用于「智能符号 press2 要不要抢在模式激活之前」的门控：只有被模式占用的符号键存在
    /// 这个冲突，其余标点照常在标点分支判 press2。
    ///
    /// z 键功能（`z_key_action`）刻意不算：它要过三重身份裁决，且字母键根本不产出标点，
    /// `punct_char` 那一关就已经把它挡在门外。
    fn is_any_mode_trigger(&self, key_code: u32) -> bool {
        self.is_temp_pinyin_trigger(key_code)
            || self.is_temp_english_trigger(key_code)
            || self.match_special_trigger(key_code).is_some()
            || self.match_mix_trigger(key_code).is_some()
    }

    /// 空缓冲模式激活的单一入口（对齐 key-pipeline.md §2.1 优先级链）。
    /// 优先级：临时英文(Shift+字母) > 快捷输入 > 临时拼音 > 特殊模式。命中返回激活 KeyAction，
    /// 都不命中返回 None（落普通输入）。URL 前缀夺取是「缓冲扩展夺取」语义，不在此链，单独处理。
    pub(crate) fn try_activate_mode(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        // 智能符号 press2 **优先于模式激活**：模式内二次按进入键时已上屏中文标点并武装
        // （见 `arm_smart_symbol_after_commit`），时限内再按同键必须替换成英文形，而不是又进
        // 一次模式——否则被模式占用的符号键（`;` / `` ` `` / `\`）永远打不出英文形，武装白武装。
        //
        // 三重收窄，确保不惊扰既有路径：① 仅空闲态（无缓冲/无已转换前缀/无候选，缓冲非空时的
        // 模式触发另有 `decideBufferedTrigger` 那条链，不归此处管）；② 仅被某模式占用的键
        // （普通标点仍按原路径在标点分支判 press2，路径与风险都不扩散）；③ 仅判 press2，不武装。
        if state.input_buffer.is_empty()
            && state.committed_text.is_empty()
            && state.candidates.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
            && self.is_any_mode_trigger(data.key_code)
            && let Some(ch) = punct_char(data.key_code, data.modifiers & MOD_SHIFT != 0)
            && let Some(act) = self.try_smart_symbol_press2_only(state, ch, data.prev_char)
        {
            return Some(act);
        }

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
            let prefix = wind_keys::keymap::vk_to_prefix_char_with_letters(data.key_code)
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

        // z 键功能（方案级 `schema.codetable.z_key_action`）的三重身份裁决
        // （对齐 Go judgeZFirstTrigger）：码表引擎 + 空缓冲 + 无修饰键 + 本方案配了 z 的功能。
        // ① z_key_repeat 开且有上屏历史 → 不进模式（z 作 repeat，落普通输入由 update_candidates
        //    注入重复候选）；② z 是活码前缀（码表/短语有以 z 开头的条目，如自定义 zhang）→ 不进
        //    模式（作正常码字母）；③ 否则（死码 + 无 repeat，如标准五笔 z）→ 执行 z_key_action。
        //
        // 只认 z、且只在码表引擎：见 `ZKeyAction` 的「为什么是方案级、且只管 z」。混输刻意排除
        // （避免 `zhang` 丢首字母，与 `try_z_fallback` 的门禁同源）。
        if state.input_buffer.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && data.key_code == keymap::VK_Z
            && matches!(
                self.engine_mgr.current_engine_type(),
                Some(wind_engine::EngineType::CodeTable)
            )
        {
            let action = self.z_key_action();
            // ①②的顺序：repeat 判据在前且更便宜，能省掉 has_code_prefix 的码表查询。
            if action.is_enabled()
                && self.z_key_repeat_text().is_none()
                && !self.has_code_prefix("z")
                && let Some(act) = self.enter_z_action(state, &action, data.key_code)
            {
                state.rewind = None; // 首键进入非夺取式，作废任何旧回退登记
                return Some(act);
            }
            // ①/②/门卫未过：返回 None，z 落普通输入路径（buffer 变 "z"，repeat 注入 / 正常码
            // 累积；后续按字母若 z… 破前缀，由 try_z_fallback 夺取——仅 temp_pinyin 支持夺取）。
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

    /// 当前方案的 z 键功能（`schema.codetable.z_key_action` 经方案折叠后的生效值）。
    ///
    /// 走 `codetable_settings()` 而非直接读全局配置：这是**方案级**配置，不同码表里 z 的
    /// 地位不同（五笔 86 是死码，别的码表未必），全局值只是没有方案覆盖时的回落基线。
    pub(crate) fn z_key_action(&self) -> ZKeyAction {
        ZKeyAction::parse(&self.engine_mgr.codetable_settings().z_key_action)
    }

    /// 执行 z 键功能：按 `action` 进对应模式（空缓冲进入语义，组合区前缀显示 `z`）。
    ///
    /// **各目标模式的可用性门卫都在这里**，与引导键进入点用的是同一套判据（临拼的
    /// `temp_pinyin_target`、mix 的成员非空、特殊模式的 `ensure_schema`）。门卫没过返回
    /// `None`，调用方让 z 落普通输入作正常码——绝不能吞键，否则配了个不可用的目标就等于
    /// 把 z 这个编码键废掉，且用户完全看不出原因。
    pub(crate) fn enter_z_action(
        &self,
        state: &mut State,
        action: &ZKeyAction,
        key_code: u32,
    ) -> Option<KeyAction> {
        match action {
            ZKeyAction::None => None,
            ZKeyAction::TempPinyin => {
                let target = self.engine_mgr.temp_pinyin_target()?;
                state.active = Some(ModeKind::TempPinyin);
                state.temp_pinyin_schema = target;
                state.temp_pinyin_buffer.clear();
                state.temp_pinyin_prefix = Self::temp_pinyin_prefix_for(key_code).to_string();
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                debug!("z_key_action: entered temp pinyin");
                Some(KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                })
            }
            ZKeyAction::TempEnglish => {
                if !self.rt().config.input.temp_english.enabled {
                    return None;
                }
                state.active = Some(ModeKind::TempEnglish);
                state.temp_english_buffer.clear();
                state.temp_english_cursor = 0;
                state.temp_english_prefix = keymap::vk_to_prefix_char_with_letters(key_code)
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                self.update_temp_english_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                debug!("z_key_action: entered temp English");
                Some(KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                })
            }
            ZKeyAction::Mix(id) => {
                let idx = self.mix_mode_idx(id)?;
                // 与引导键进入点同一门卫：含 quick_input 或至少一个可加载成员方案。
                if !self.mix_has_quick_input(idx) && self.mix_members(idx).is_empty() {
                    return None;
                }
                debug!("z_key_action: entering mix idx={}", idx);
                Some(self.enter_mix_mode(state, idx, key_code))
            }
            ZKeyAction::Special(id) => {
                let idx = self.special_mode_idx(id)?;
                let schema = self.special_schema(idx)?;
                if !self.engine_mgr.ensure_schema(&schema) {
                    return None;
                }
                debug!("z_key_action: entering special idx={}", idx);
                Some(self.enter_special_mode(state, idx, key_code))
            }
        }
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
        // 清理可能残留的组合显示（临时拼音/快捷输入会产生候选与 preedit）
        state.input_buffer.clear();
        state.input_buffer_cased.clear();
        state.input_cursor_pos = 0;
        state.candidates.clear();
        state.preedit.clear();
        // 拼音逐步转换的已转换前缀一并丢弃（焦点/模式切换不保留半成品组合）。
        state.committed_text.clear();
        state.committed_segs.clear();
        // 焦点/模式切换：解除智能符号待命，避免跨上下文误触发替换。
        self.disarm_smart_symbol();
        // 快捷加词模式遗留：焦点/模式切换时退出。
        // 布局无需在此恢复——模式标志已清，下一次候选显示会自动算回全局基线（见 layout.rs）。
        // 这正是声明式重算相对「保存/恢复」的价值：这条路径当年就是补丁式加上的第 3、第 4 个
        // 恢复出口，再加四个模式就会有十几处，漏一处即候选窗卡在竖排且无日志。
        if state.add_word_active {
            state.add_word_active = false;
            state.add_word_chars.clear();
            state.add_word_len = 0;
            state.add_word_code.clear();
        }
        if dirty {
            debug!("reset_exclusive_modes: cleared residual exclusive input mode state");
        }
    }
}
