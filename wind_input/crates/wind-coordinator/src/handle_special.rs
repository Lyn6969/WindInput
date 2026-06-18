//! 特殊方案输入模式
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。


use crate::coordinator::{punct_char, Coordinator, State};
use crate::pipeline::ModeKind;
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

impl Coordinator {
    /// 引导键名 → VK（特殊模式触发；统一映射 + 额外支持单字母 a-z 引导键，见 `keymap`）。
    pub(crate) fn special_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk_with_letters(key)
    }

    /// 找出 key_code 匹配的特殊模式下标（按配置顺序先到先得；最多 256 个）。
    pub(crate) fn match_special_trigger(&self, key_code: u32) -> Option<u8> {
        for (i, m) in self.config.features.special_modes.iter().enumerate() {
            if i > u8::MAX as usize {
                break;
            }
            if m.trigger_keys
                .iter()
                .filter_map(|k| Self::special_trigger_vk(k))
                .any(|vk| vk == key_code)
            {
                return Some(i as u8);
            }
        }
        None
    }

    /// 特殊模式引用的方案 id（features.special_modes[idx].schema）。
    pub(crate) fn special_schema(&self, idx: u8) -> Option<String> {
        self.config
            .features
            .special_modes
            .get(idx as usize)
            .map(|m| m.schema.clone())
            .filter(|s| !s.is_empty())
    }

    /// 进入特殊模式（其方案须可加载，由激活点 ensure_schema 保证）。清空普通输入，初始化空编码缓冲。
    pub(crate) fn enter_special_mode(&self, state: &mut State, idx: u8) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Special(idx));
        state.special_id = idx;
        state.special_buffer.clear();
        self.update_special_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered special mode idx={}", idx);
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 退出特殊模式并清空相关状态（码表缓存保留供复用）。
    pub(crate) fn exit_special_mode(&self, state: &mut State) {
        state.active = None;
        state.special_buffer.clear();
        state.candidates.clear();
        state.preedit.clear();
    }

    /// 按当前编码缓冲刷新特殊模式候选（经其引用方案的引擎查询，复用方案 CodeTableSpec 全码策略）。
    /// 返回 Some(text) 表示该方案的全码策略请求自动上屏。
    pub(crate) fn update_special_candidates(&self, state: &mut State) -> Option<String> {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        state.preedit = state.special_buffer.clone();
        if state.special_buffer.is_empty() {
            return None;
        }
        let schema = self.overlay_engine_schema(state)?;
        let result = self
            .engine_mgr
            .convert_with(&schema, &state.special_buffer, 100);
        state.candidates = result.candidates;
        // 自动上屏由方案码表引擎的 should_auto_commit 决定（prefix_free≈全码唯一、fixed_length 等
        // 映射到该方案的 [engine.codetable] 配置）；复核上屏目标仍在候选中。
        if result.should_commit
            && !result.commit_text.is_empty()
            && state
                .candidates
                .iter()
                .any(|c| c.text == result.commit_text)
        {
            return Some(result.commit_text);
        }
        None
    }

    /// 特殊模式按键处理：编码累积 + 候选选择 + 三档自动上屏；空格选高亮、回车上屏编码原文。
    pub(crate) fn handle_special_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：放弃退出
                self.exit_special_mode(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 退格：删编码；空则退出。删除时不触发自动上屏。
                state.special_buffer.pop();
                if state.special_buffer.is_empty() {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    self.update_special_candidates(state);
                    let display = state.preedit.clone();
                    self.notify_ui_update(state);
                    KeyAction::UpdateComposition {
                        text: display.clone(),
                        caret_pos: display.chars().count() as u32,
                    }
                }
            }
            keymap::VK_SPACE => {
                // 空格：有候选选高亮上屏；无候选退出
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏编码原文
                let text = state.special_buffer.clone();
                self.exit_special_mode(state);
                self.notify_ui_hide();
                if text.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(text, true)
                }
            }
            keymap::VK_1..=keymap::VK_9 => {
                // 数字 1-9 选当前页候选
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if gi < end {
                    let text = state.candidates[gi].text.clone();
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_A..=keymap::VK_Z => {
                // 字母：小写归一累积编码
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.special_buffer.push(ch);
                if let Some(text) = self.update_special_candidates(state) {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    return Self::commit_action(text, true);
                }
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 二三候选键 → 选候选
                if !shift
                    && let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(state);
                        let gi = start + offset;
                        if gi < end {
                            let text = state.candidates[gi].text.clone();
                            self.exit_special_mode(state);
                            self.notify_ui_hide();
                            return Self::commit_action(text, true);
                        }
                    }
                // 其它可打印标点：顶屏当前高亮候选 + 转换后标点，退出
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let committed = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        state.candidates[idx].text.clone()
                    } else {
                        String::new()
                    };
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", committed, punct), true)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }
}
