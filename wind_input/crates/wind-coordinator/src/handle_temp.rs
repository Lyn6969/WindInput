//! 临时拼音 / 临时英文输入模式
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 触发键判定、进入/退出、候选刷新、按键处理、选词上屏。

use crate::coordinator::{
    adapt_en_case, detect_en_case, punct_char, EnCase, ENGINE_MAX_CANDIDATES, Coordinator, State,
};
use crate::pipeline::ModeKind;
use wind_ipc::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT};
use wind_transform::fullwidth::to_full_width;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_keys::keymap;

impl Coordinator {
    /// 触发键名 → VK（统一映射，见 `keymap`；不含 z，z 混合模式后置实现）
    pub(crate) fn temp_pinyin_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk(key)
    }

    /// VK → 组合区前缀字符（统一映射，见 `keymap`；缺省回退反引号）
    pub(crate) fn temp_pinyin_prefix_for(key_code: u32) -> char {
        keymap::vk_to_prefix_char(key_code).unwrap_or('`')
    }

    /// 当前按键是否匹配配置的临时拼音触发键
    pub(crate) fn is_temp_pinyin_trigger(&self, key_code: u32) -> bool {
        self.rt().config
            .input
            .temp_pinyin
            .trigger_keys
            .iter()
            .filter_map(|k| Self::temp_pinyin_trigger_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 退出临时拼音模式并清空相关状态（含逐步转换的已转换前缀）
    pub(crate) fn exit_temp_pinyin(&self, state: &mut State) {
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_schema.clear();
        state.temp_pinyin_prefix.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 用临时拼音目标方案转换缓冲，刷新候选与组合区（前缀 + 已转换汉字 + 剩余拼音）
    pub(crate) fn update_temp_pinyin_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let prefix = format!("{}{}", state.temp_pinyin_prefix, state.committed_text);
        if state.temp_pinyin_buffer.is_empty() {
            state.preedit = prefix;
            return;
        }
        let Some(schema) = self.overlay_engine_schema(state) else {
            state.preedit = format!("{}{}", prefix, state.temp_pinyin_buffer);
            return;
        };
        let result =
            self.engine_mgr
                .convert_with(&schema, &state.temp_pinyin_buffer, ENGINE_MAX_CANDIDATES);
        let display = if result.preedit_display.is_empty() {
            state.temp_pinyin_buffer.clone()
        } else {
            result.preedit_display
        };
        state.preedit = format!("{}{}", prefix, display);

        // 临时拼音候选按词库权重排序（其词频维度涉及特殊模式配置归属，待 S1 引擎层处理）。
        let mut candidates = result.candidates;
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(ENGINE_MAX_CANDIDATES);
        state.candidates = candidates;
    }

    /// 临时拼音选词 —— 组合区逐步转换（C）。部分匹配并入 committed 前缀留模式内（不上屏）；
    /// 完整匹配整体上屏 committed+候选（前缀触发键不输出）+ 造词，退出。返回最终 KeyAction。
    pub(crate) fn commit_temp_pinyin_selected(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        let total = state.temp_pinyin_buffer.len();
        let consumed = cand.consumed_length;
        let code = Self::cand_code(&state.temp_pinyin_buffer, cand);
        let partial =
            consumed > 0 && consumed < total && state.temp_pinyin_buffer.is_char_boundary(consumed);
        self.record_selection(&code, &cand.text);
        if partial {
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.temp_pinyin_buffer = state.temp_pinyin_buffer[consumed..].to_string();
            self.update_temp_pinyin_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            state.committed_segs.push((code, cand.text.clone()));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state);
            let out = self.maybe_s2t(state, &final_simplified);
            self.exit_temp_pinyin(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 临时拼音模式下的按键处理
    pub(crate) fn handle_temp_pinyin_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        // 进入键二次按下（缓冲空 + 无已转换前缀）：按中英标点配置上屏该符号并退出。
        if state.temp_pinyin_buffer.is_empty()
            && state.committed_text.is_empty()
            && self.is_temp_pinyin_trigger(data.key_code)
        {
            let ch = state
                .temp_pinyin_prefix
                .chars()
                .next()
                .or_else(|| punct_char(data.key_code, data.modifiers & MOD_SHIFT != 0));
            if let Some(ch) = ch {
                let out = self.convert_punct_char(state, ch);
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                return Self::commit_action(out, true);
            }
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：退出
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // Backspace：分步撤销——有已转换段先退回最后一段（你→ni，码并回缓冲前部）；
                // 否则删剩余拼音末字符；皆空则退出。
                if let Some((code, _)) = state.committed_segs.pop() {
                    state.committed_text =
                        state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
                    state.temp_pinyin_buffer = format!("{}{}", code, state.temp_pinyin_buffer);
                    self.update_temp_pinyin_candidates(state);
                    let display = state.preedit.clone();
                    self.notify_ui_update(state);
                    return KeyAction::UpdateComposition {
                        caret_pos: display.chars().count() as u32,
                        text: display,
                    };
                }
                if state.temp_pinyin_buffer.is_empty() {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                state.temp_pinyin_buffer.pop();
                if state.temp_pinyin_buffer.is_empty() {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    caret_pos: display.chars().count() as u32,
                    text: display,
                }
            }
            keymap::VK_SPACE => {
                // 空格：选当前高亮候选（逐步转换）
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏「当前显示」= 已转换前缀 + 剩余拼音原码（已选中文照样上屏），退出。
                let out = format!("{}{}", state.committed_text, state.temp_pinyin_buffer);
                let out = self.maybe_s2t(state, &out);
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                if out.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(out, true)
                }
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字键选当前页第 N 个
                let (start, end) = self.page_range(state);
                let idx = start + (data.key_code - 0x31) as usize;
                if idx < end {
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_A..=keymap::VK_Z if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
                // 字母累积拼音
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.temp_pinyin_buffer.push(ch);
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                }
            }
            _ => {
                // 二三候选键
                if data.modifiers & MOD_SHIFT == 0
                    && let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(state);
                        let idx = start + offset;
                        if idx < end {
                            let cand = state.candidates[idx].clone();
                            return self.commit_temp_pinyin_selected(state, &cand);
                        }
                    }
                // 其它键：有候选则上屏高亮候选（分段则保留剩余拼音）；否则退出清空。
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
        }
    }

    /// 退出临时英文模式并清空状态
    pub(crate) fn exit_temp_english(&self, state: &mut State) {
        state.active = None;
        state.temp_english_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
    }

    /// 刷新临时英文候选：首候选=用户原始输入，其后为英文词库前缀匹配（大小写适配）。
    /// 需 `shift_temp_english.show_english_candidates` 开启才查词库；词库为固定 id "english" 方案。
    pub(crate) fn update_temp_english_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let buf = state.temp_english_buffer.clone();
        state.preedit = buf.clone();
        if buf.is_empty() {
            return;
        }
        // 首候选始终是用户所打原文（保证能上屏自己输入的内容）。
        let mut cands = vec![Candidate {
            text: buf.clone(),
            natural_order: 0,
            ..Default::default()
        }];
        if let Some(schema) = self.overlay_engine_schema(state) {
            let lower = buf.to_lowercase();
            let case = detect_en_case(&buf);
            let result = self.engine_mgr.convert_with(&schema, &lower, 60);
            let mut seen = std::collections::HashSet::new();
            seen.insert(lower);
            for (i, c) in result.candidates.into_iter().enumerate() {
                let cl = c.text.to_lowercase();
                if !seen.insert(cl.clone()) {
                    continue;
                }
                // 词库全小写词按输入大小写适配；专有词（iPhone/Aaron）保持原样。
                let display = if case != EnCase::Lower && c.text == cl {
                    adapt_en_case(&c.text, case)
                } else {
                    c.text
                };
                cands.push(Candidate {
                    text: display,
                    natural_order: (i + 1) as i32,
                    ..Default::default()
                });
            }
        }
        state.candidates = cands;
    }

    /// 临时英文模式按键处理（首版：缓冲累积 + 空格/回车/标点上屏，暂无词库候选）
    pub(crate) fn handle_temp_english_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        // 候选感知刷新后返回组合区动作。
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_temp_english_candidates(state);
            let d = state.preedit.clone();
            this.notify_ui_update(state);
            KeyAction::UpdateComposition {
                text: d.clone(),
                caret_pos: d.chars().count() as u32,
            }
        };
        // 上屏文本（可选全角）+ 退出。
        let commit_text = |this: &Self, state: &mut State, t: String| -> KeyAction {
            let text = if state.full_width {
                to_full_width(&t)
            } else {
                t
            };
            this.exit_temp_english(state);
            this.notify_ui_hide();
            if text.is_empty() {
                KeyAction::ClearComposition
            } else {
                Self::commit_action(text, true)
            }
        };
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_temp_english(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                state.temp_english_buffer.pop();
                if state.temp_english_buffer.is_empty() {
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    refresh(self, state)
                }
            }
            keymap::VK_SPACE => {
                // 空格：上屏当前高亮候选（首候选=原始输入）
                let text = if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    state.candidates[idx].text.clone()
                } else {
                    state.temp_english_buffer.clone()
                };
                commit_text(self, state, text)
            }
            keymap::VK_RETURN => {
                // 回车：上屏原始输入文本（不取候选）
                let text = state.temp_english_buffer.clone();
                commit_text(self, state, text)
            }
            keymap::VK_A..=keymap::VK_Z => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                let base = data.key_code - 0x41;
                let ch = if shift {
                    (b'A' + base as u8) as char
                } else {
                    (b'a' + base as u8) as char
                };
                state.temp_english_buffer.push(ch);
                refresh(self, state)
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字：有词库候选（>1，即除原文外还有匹配）时按页选词；否则作输入（英文含数字 v2）
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if state.candidates.len() > 1 && gi < end {
                    let text = state.candidates[gi].text.clone();
                    commit_text(self, state, text)
                } else {
                    let ch = (b'0' + (data.key_code - 0x30) as u8) as char;
                    state.temp_english_buffer.push(ch);
                    refresh(self, state)
                }
            }
            0x30 if data.modifiers & MOD_SHIFT == 0 => {
                state.temp_english_buffer.push('0');
                refresh(self, state)
            }
            _ => {
                // 其它（标点等）：上屏当前高亮候选 + 转换后标点，退出
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let base = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        state.candidates[idx].text.clone()
                    } else {
                        state.temp_english_buffer.clone()
                    };
                    let base = if state.full_width {
                        to_full_width(&base)
                    } else {
                        base
                    };
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", base, punct), true)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }

    /// 顶屏当前高亮候选（若有）并进入临时拼音模式（对齐 Go decideBufferedTrigger 的 actEnterMode）。
    /// 有候选：上屏高亮候选 + 原子开启临时拼音组合；空码：丢弃缓冲后进入。
    pub(crate) fn commit_and_enter_temp_pinyin(
        &self,
        state: &mut State,
        key_code: u32,
        target: String,
    ) -> KeyAction {
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
        // 进入临时拼音
        state.active = Some(ModeKind::TempPinyin);
        state.temp_pinyin_schema = target;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix = Self::temp_pinyin_prefix_for(key_code).to_string();
        self.update_temp_pinyin_candidates(state);
        self.notify_ui_update(state);
        let prefix = state.temp_pinyin_prefix.clone();
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
