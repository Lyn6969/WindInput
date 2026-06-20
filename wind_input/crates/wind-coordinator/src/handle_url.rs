//! 网址输入模式（劫持缓冲 + 回退）
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, State, printable_char};
use crate::pipeline::{ModeKind, Rewind};
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

impl Coordinator {
    /// 探针是否恰好等于某个网址前缀（精确匹配，对齐 Go urlActivationResidual 的全匹配语义）。
    pub(crate) fn is_url_prefix(&self, probe: &str) -> bool {
        self.rt()
            .config
            .input
            .url_input
            .prefixes
            .iter()
            .any(|p| !p.is_empty() && p == probe)
    }

    /// 进入网址模式：以补全前缀的完整文本作初始缓冲，清空普通输入/候选，隐藏候选窗。
    /// 网址模式无候选，仅在组合区原样显示累积文本。
    /// 同时登记夺取回退：snapshot=夺取前的正常输入（=前缀去掉补全键），host_text=完整前缀。
    pub(crate) fn enter_url_mode(&self, state: &mut State, buffer: String) -> KeyAction {
        // 夺取前的正常 input_buffer 即回退快照（前缀的最后一字符是刚补全的那一键）。
        let snapshot = state.input_buffer.clone();
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Url);
        state.url_buffer = buffer.clone();
        state.rewind = Some(Rewind {
            snapshot,
            host_text: buffer,
        });
        self.notify_ui_hide();
        let disp = state.url_buffer.clone();
        debug!("Entered URL mode (buffer={})", disp);
        KeyAction::UpdateComposition {
            text: disp.clone(),
            caret_pos: disp.chars().count() as u32,
        }
    }

    /// 退出网址模式并清空相关状态（含作废回退登记）。
    pub(crate) fn exit_url_mode(&self, state: &mut State) {
        state.active = None;
        state.url_buffer.clear();
        state.preedit.clear();
        state.rewind = None;
    }

    /// 当前夺取式模式的 buffer（用于回退边界判定）。非夺取式模式返回 None。
    pub(crate) fn active_hijack_buffer<'a>(&self, state: &'a State) -> Option<&'a str> {
        match state.active {
            Some(ModeKind::Url) => Some(&state.url_buffer),
            // z 临拼夺取（后续 S3/S4 接入）：Some(&state.temp_pinyin_buffer)
            _ => None,
        }
    }

    /// 是否可回退：已登记 + 当前模式 buffer 已退回到夺取边界（== 登记时的 host_text）。
    pub(crate) fn can_rewind(&self, state: &State) -> bool {
        match (&state.rewind, self.active_hijack_buffer(state)) {
            (Some(rw), Some(buf)) => buf == rw.host_text,
            _ => false,
        }
    }

    /// 执行夺取回退：撤销夺取，把快照回放到正常码表输入流并重算候选。
    pub(crate) fn rewind_hijack(&self, state: &mut State) -> KeyAction {
        let snapshot = state.rewind.take().map(|r| r.snapshot).unwrap_or_default();
        // 退出当前夺取式模式（目前仅 URL；z 临拼接入后在此扩展 match）。
        match state.active {
            Some(ModeKind::Url) => self.exit_url_mode(state),
            _ => self.reset_exclusive_modes(state),
        }
        state.input_buffer = snapshot;
        self.update_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!(
            "rewind_hijack: restored normal input '{}'",
            state.input_buffer
        );
        KeyAction::UpdateComposition {
            text: display,
            caret_pos: state.input_buffer.chars().count() as u32,
        }
    }

    /// 网址模式按键处理：可见 ASCII 原样累积；空格/回车上屏原文；退格删空退出；Esc 放弃。
    pub(crate) fn handle_url_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let comp = |buf: &str| KeyAction::UpdateComposition {
            text: buf.to_string(),
            caret_pos: buf.chars().count() as u32,
        };
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：放弃退出（无上屏）
                self.exit_url_mode(state);
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 退格：删尾字符；删空则退出
                state.url_buffer.pop();
                if state.url_buffer.is_empty() {
                    self.exit_url_mode(state);
                    KeyAction::ClearComposition
                } else {
                    comp(&state.url_buffer)
                }
            }
            keymap::VK_SPACE | keymap::VK_RETURN => {
                // 空格/回车：上屏当前缓冲原文（不做全半角/标点转换）
                let text = state.url_buffer.clone();
                self.exit_url_mode(state);
                if text.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(text, true)
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = printable_char(data.key_code, shift) {
                    state.url_buffer.push(ch);
                    comp(&state.url_buffer)
                } else {
                    // 方向键等非可打印键：消费但不改缓冲（首版不支持光标内编辑）
                    KeyAction::Consumed
                }
            }
        }
    }
}
