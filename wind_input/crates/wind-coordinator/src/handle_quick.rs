//! 快捷输入模式（日期/计算器）
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。


use crate::coordinator::{quick_input_char, Coordinator, State};
use crate::pipeline::ModeKind;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_ipc::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT};
use wind_keys::keymap;

impl Coordinator {
    /// 触发键名 → VK
    pub(crate) fn quick_input_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk(key)
    }

    /// VK → 组合区前缀字符（统一映射，见 `keymap`；缺省回退分号）
    pub(crate) fn quick_input_prefix_for(key_code: u32) -> char {
        keymap::vk_to_prefix_char(key_code).unwrap_or(';')
    }

    /// 当前按键是否匹配配置的快捷输入触发键
    pub(crate) fn is_quick_input_trigger(&self, key_code: u32) -> bool {
        self.rt().config
            .features
            .quick_input
            .trigger_keys
            .iter()
            .filter_map(|k| Self::quick_input_trigger_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 退出快捷输入模式并清空状态
    pub(crate) fn exit_quick_input(&self, state: &mut State) {
        state.active = None;
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 由缓冲生成日期/计算器候选，刷新组合区（前缀 + 缓冲）
    pub(crate) fn update_quick_input_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let prefix = state.quick_input_prefix.clone();
        if state.quick_input_buffer.is_empty() {
            state.preedit = prefix;
            return;
        }
        state.preedit = format!("{}{}", prefix, state.quick_input_buffer);
        let dp = self.rt().config.features.quick_input.decimal_places;
        let texts =
            wind_quick_input::generate_quick_input_candidates(&state.quick_input_buffer, dp);
        state.candidates = texts
            .into_iter()
            .enumerate()
            .map(|(i, t)| Candidate {
                text: t,
                natural_order: i as i32,
                ..Default::default()
            })
            .collect();
    }

    /// 快捷输入模式组合区刷新结果（UpdateComposition）
    pub(crate) fn quick_input_composition(&self, state: &State) -> KeyAction {
        let display = state.preedit.clone();
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 快捷输入模式下的按键处理
    pub(crate) fn handle_quick_input_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        // 表达式模式：`-`/`=` 是运算符输入，不当翻页（include_printable=false）。
        if let Some(act) = self.apply_nav_key(state, data, false) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_quick_input(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 退格：空缓冲退出，否则删末字符（可退到仅前缀）
                if state.quick_input_buffer.is_empty() {
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                state.quick_input_buffer.pop();
                self.update_quick_input_candidates(state);
                if state.candidates.is_empty() {
                    self.notify_ui_hide();
                } else {
                    self.notify_ui_update(state);
                }
                self.quick_input_composition(state)
            }
            keymap::VK_SPACE => {
                // 空格：上屏当前高亮候选；无候选则退出
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏缓冲原文（空则上屏前缀字符）
                let out = if state.quick_input_buffer.is_empty() {
                    state.quick_input_prefix.clone()
                } else {
                    state.quick_input_buffer.clone()
                };
                self.exit_quick_input(state);
                self.notify_ui_hide();
                if out.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(out, true)
                }
            }
            keymap::VK_A..=keymap::VK_Z if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
                // 字母 a-z 按标签选当前页候选（a=第1个）
                let (start, end) = self.page_range(state);
                let idx = start + (data.key_code - 0x41) as usize;
                if idx < end {
                    let text = state.candidates[idx].text.clone();
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::Consumed
                }
            }
            _ => {
                // 再次按触发键且缓冲为空：按标点配置上屏前缀字符并退出
                if state.quick_input_buffer.is_empty() && self.is_quick_input_trigger(data.key_code)
                {
                    let ch = state.quick_input_prefix.chars().next().unwrap_or(';');
                    let out = self.convert_punct_char(state, ch);
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    return Self::commit_action(out, true);
                }
                // 可打印字符（数字/运算符/点/括号等）累积到缓冲
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = quick_input_char(data.key_code, shift) {
                    if state.quick_input_buffer.chars().count() < 20 {
                        state.quick_input_buffer.push(ch);
                        self.update_quick_input_candidates(state);
                        if state.candidates.is_empty() {
                            self.notify_ui_hide();
                        } else {
                            self.notify_ui_update(state);
                        }
                    }
                    self.quick_input_composition(state)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }

    /// 顶屏当前高亮候选（若有）并进入快捷输入模式。
    pub(crate) fn commit_and_enter_quick_input(&self, state: &mut State, key_code: u32) -> KeyAction {
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let idx = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            self.record_selection(&state.input_buffer, &t);
            Some(format!("{prefix}{t}"))
        } else if !prefix.is_empty() {
            Some(prefix)
        } else {
            None
        };
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::QuickInput);
        state.quick_input_buffer.clear();
        state.quick_input_prefix = Self::quick_input_prefix_for(key_code).to_string();
        self.update_quick_input_candidates(state);
        self.notify_ui_update(state);
        let prefix = state.quick_input_prefix.clone();
        match committed {
            Some(text) => KeyAction::InsertText {
                text,
                new_composition: Some(prefix),
                mode_changed: false,
                chinese_mode: true,
                has_new_composition: true,
            },
            None => KeyAction::UpdateComposition {
                text: prefix.clone(),
                caret_pos: prefix.chars().count() as u32,
            },
        }
    }
}
