//! 标点编排 + 智能符号状态机
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 纯转换逻辑在 wind-punct crate；此处是 coordinator 包装（锁转换器/读状态）+ 智能符号
//! 连按替换的状态机（武装/触发/解除）。

use crate::coordinator::{Coordinator, State};
use tracing::debug;
use wind_bridge::handler::KeyAction;
use wind_config::config::SmartMethod;

impl Coordinator {
    /// 数字后智能标点：在中文标点模式下，若 ch 在智能标点列表且光标前一字符为数字，
    /// 则该标点应按英文（半角）输出（如 "3." 不转成 "3。"）。
    pub(crate) fn is_smart_punct_after_digit(&self, ch: char, prev_char: u16) -> bool {
        wind_punct::is_smart_punct_after_digit(&self.rt().config.input, ch, prev_char)
    }

    /// 按当前中英标点/全半角配置转换一个标点字符为上屏文本（无 prev_char 上下文）。
    /// 用于独占模式（快捷输入/临时英文）等不涉及数字后智能的场景。
    pub(crate) fn convert_punct_char(&self, state: &State, ch: char) -> String {
        self.convert_punct(state, ch, 0)
    }

    /// 标点转换单点流水线（对齐 Go `convertPunct`，固定优先级）：
    ///   1. 自定义映射（四状态：中半 0 / 英全 1 / 中全 2 / 英半 3，按当前中英标点+全半角选列）
    ///   2. 数字后智能转换（命中则该标点按英文输出，不转中文）
    ///   3. 中文标点转换（引号左右交替状态机）
    ///   4. 全半角转换
    /// `prev_char` 为光标前一字符的 UTF-16 单元（0=不可用），用于数字后智能判定。
    pub(crate) fn convert_punct(&self, state: &State, ch: char, prev_char: u16) -> String {
        let mut conv = self.punct.lock().unwrap_or_else(|e| e.into_inner());
        wind_punct::convert_punct(
            &mut conv,
            &self.rt().config.input,
            state.chinese_punct,
            state.full_width,
            ch,
            prev_char,
        )
    }

    /// 智能符号模式判定时限（非法值回退 500ms）。
    pub(crate) fn smart_symbol_timeout(&self) -> std::time::Duration {
        let ms = self.rt().config.input.symbol.smart_timeout_ms;
        let ms = if ms <= 0 { 500 } else { ms };
        std::time::Duration::from_millis(ms as u64)
    }

    /// 无副作用地计算 `ch` 在当前模式下的标点产物，**镜像** `convert_punct` 优先级
    /// （自定义列 > 中/英转换 > 全半角）。对齐 Go `computePunctStrPure`。
    ///   - `chinese=true`：算中文标点产物（武装/匹配用，引号经 peek 预测不改状态）。
    ///   - `chinese=false`：算英文标点产物（替换用，即该键英文模式下输出）。
    /// 引号有状态、键名特殊，此处保守跳过自定义、走标准引号/英文产物。
    pub(crate) fn compute_punct_str_pure(
        &self,
        state: &State,
        ch: char,
        chinese: bool,
    ) -> Option<String> {
        let conv = self.punct.lock().unwrap_or_else(|e| e.into_inner());
        wind_punct::compute_punct_str_pure(
            &conv,
            &self.rt().config.input,
            state.full_width,
            ch,
            chinese,
        )
    }

    /// 判断中文标点串 `cn` 是否在用户配置的参与集合内（子串包含匹配，支持多字符/引号）。
    pub(crate) fn smart_symbol_participates(&self, cn: &str) -> bool {
        wind_punct::participates(&self.rt().config.input, cn)
    }

    /// 计算 `ch` 当前会产生的「参与集合内的中文标点串」用于武装；不参与返回 None。
    /// 对齐 Go `smartSymbolArmStr`：仅中文标点模式 + 非数字后智能 + 在参与集合内。
    pub(crate) fn smart_symbol_arm_str(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
    ) -> Option<String> {
        if !state.chinese_punct {
            return None;
        }
        if self.is_smart_punct_after_digit(ch, prev_char) {
            return None;
        }
        let cn = self.compute_punct_str_pure(state, ch, true)?;
        if !self.smart_symbol_participates(&cn) {
            return None;
        }
        // 与自动配对互斥：被配对的符号（单字符且在配对表）不武装智能符号。否则 press1 插入配对
        // 并回退光标至中间，press2 时 prevChar 恰为配对左符号 → 误删左符号改英文、留下中文右符号。
        if cn.chars().count() == 1 {
            let c0 = cn.chars().next().unwrap();
            if self.is_auto_pair_char(state, c0) {
                return None;
            }
        }
        Some(cn)
    }

    /// 智能符号替换判定（在标点分支入口调用）。对齐 Go `trySmartSymbolReplace`：
    ///   - 返回 Some：本次为 press2 触发，调用方应直接返回该替换响应（短路）。
    ///   - 返回 None：未触发；已按需更新武装态，调用方继续普通标点流程。
    pub(crate) fn try_smart_symbol_replace(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
    ) -> Option<KeyAction> {
        if !self.rt().config.input.symbol.smart_mode {
            return None;
        }
        let method = self.rt().config.input.symbol.smart_method.clone();
        let timeout_ms = self.smart_symbol_timeout().as_millis() as u32;
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());

        // ── press2 判定 ──────────────────────────────────────────────────────────
        if arm.armed
            && ch == arm.key
            && state.chinese_punct
            && arm
                .at
                .map(|t| t.elapsed() < self.smart_symbol_timeout())
                .unwrap_or(false)
        {
            match method {
                SmartMethod::HoldComposition => {
                    if arm.held_text.is_some() {
                        // 正常 hold 路径：press1 时无活跃编码，组合态内无需 prev_char 验证，直接提交英文。
                        if let Some(rep) = self.compute_punct_str_pure(state, ch, false) {
                            arm.armed = false;
                            arm.held_text = None;
                            if ch == '\'' || ch == '"' {
                                self.punct
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .revert_last_quote(ch);
                            }
                            debug!(
                                "SmartSymbol(HoldComposition): press2, commit english: {}",
                                rep
                            );
                            return Some(KeyAction::InsertText {
                                text: rep,
                                new_composition: None,
                                mode_changed: false,
                                chinese_mode: state.chinese_mode,
                                has_new_composition: false,
                            });
                        }
                    } else {
                        // press1 时有活跃编码，中文标点已作文本顶屏提交，非组合态 → 降级走
                        // DeleteReplace 路径：检查 prev_char 再 ReplaceBackward。
                        // prev_char==0 视为"宿主读不回文档"（微信/Windows Terminal 等 Qt/
                        // ConPTY 宿主的 TSF OnEndEdit 里 GetSelection/GetText 经常拿不到内容），
                        // 而不是"确定不匹配"——press2 时光标前必然至少有 press1 刚提交的符号，
                        // 真读到 0 只可能是读失败。此时退回只信服务端自己的武装态
                        // （armed+key+timeout，与文档内容无关），否则永远判定不是 press2。
                        let armed_runes: Vec<char> = arm.str.chars().collect();
                        if let Some(&last) = armed_runes.last()
                            && (prev_char == 0 || last as u32 == prev_char as u32)
                            && let Some(rep) = self.compute_punct_str_pure(state, ch, false)
                        {
                            arm.armed = false;
                            if ch == '\'' || ch == '"' {
                                self.punct
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .revert_last_quote(ch);
                            }
                            debug!(
                                "SmartSymbol(HoldComposition->fallback): press2, replace chinese punct with english, count={}",
                                armed_runes.len()
                            );
                            return Some(KeyAction::ReplaceBackward {
                                count: armed_runes.len() as u32,
                                text: rep,
                            });
                        }
                    }
                }
                SmartMethod::DeleteReplace => {
                    // 光标前字符须与武装串末位匹配；prev_char==0 视为宿主读不回文档
                    // （见上面 HoldComposition->fallback 分支的同类注释），退回只信武装态。
                    let armed_runes: Vec<char> = arm.str.chars().collect();
                    if let Some(&last) = armed_runes.last()
                        && (prev_char == 0 || last as u32 == prev_char as u32)
                        && let Some(rep) = self.compute_punct_str_pure(state, ch, false)
                    {
                        arm.armed = false;
                        if ch == '\'' || ch == '"' {
                            self.punct
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .revert_last_quote(ch);
                        }
                        debug!(
                            "SmartSymbol(DeleteReplace): replace prev chinese punct with english, count={}",
                            armed_runes.len()
                        );
                        return Some(KeyAction::ReplaceBackward {
                            count: armed_runes.len() as u32,
                            text: rep,
                        });
                    }
                }
            }
        }

        // ── press1：尝试武装 ─────────────────────────────────────────────────────
        match self.smart_symbol_arm_str(state, ch, prev_char) {
            Some(cn) => {
                arm.key = ch;
                arm.str = cn.clone();
                arm.at = Some(std::time::Instant::now());

                match method {
                    SmartMethod::HoldComposition => {
                        let has_input =
                            !state.input_buffer.is_empty() || !state.committed_text.is_empty();
                        arm.armed = true;
                        if has_input {
                            // 有活跃编码时，不短路进入 hold composition——让调用方的标点分支
                            // 检测 hold_pending_commit 并生成 CommitAndHoldComposition：先顶屏
                            // 上屏候选，再开 HoldComposition 放入中文标点。
                            arm.held_text = None;
                            arm.hold_pending_commit = true;
                        } else {
                            arm.held_text = Some(cn.clone());
                            debug!(
                                "SmartSymbol(HoldComposition): press1, hold composition: {}, timeout={}ms",
                                cn, timeout_ms
                            );
                            // 短路返回：由 C++ 端负责开启组合态和计时，不走普通标点流程
                            return Some(KeyAction::HoldComposition {
                                text: cn,
                                timeout_ms,
                            });
                        }
                    }
                    SmartMethod::DeleteReplace => {
                        arm.armed = true;
                        arm.held_text = None;
                        // 返回 None：调用方继续普通标点流程（CommitText "，"）
                    }
                }
            }
            None => {
                arm.armed = false;
                arm.held_text = None;
            }
        }
        None
    }

    /// 解除智能符号待命态（焦点变化/模式切换等的防御性复位）。
    pub(crate) fn disarm_smart_symbol(&self) {
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
        arm.armed = false;
        arm.held_text = None;
        arm.hold_pending_commit = false;
        // 注：HoldComposition 模式下若组合尚未提交，C++ 端的 SetTimer 计时器会在 timeout
        // 到期后自动提交中文符号，或在焦点切换时由 OnCompositionTerminated 自然结束。
    }
}
