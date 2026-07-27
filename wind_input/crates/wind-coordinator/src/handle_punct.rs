//! 标点编排 + 智能符号状态机
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 纯转换逻辑在 wind-punct crate；此处是 coordinator 包装（锁转换器/读状态）+ 智能符号
//! 连按替换的状态机（武装/触发/解除）。

use crate::coordinator::{Coordinator, State, full_width_source_char, numpad_char, punct_char};
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData, MessageHandler};
use wind_config::config::SmartMethod;
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

/// 恰好单字符则返回之，否则 None（自定义映射可为多字符串，不能充当配对符）。
fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

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

    /// 引号 × 自动配对：若 `ch` 是引号、且其**当前模式下的实际左右形**在生效配对表中构成一对，
    /// 则把交替态**钉回「左」**并返回 true（该引号已由配对接管，按下即开新的一对）。
    ///
    /// 不钉的后果就是「一次出对、一次出单」交替循环：`to_chinese` 每次按引号都翻转交替开关，
    /// 但开了配对后**一次按键已经把左右两个引号都吐出去了**，开关却只前进一格 → 下次按键
    /// 给出右引号 → 既不是左符号（不插对）、又走右符号分支（跳出或裸提交单个右引号）。
    /// 两套状态机（交替开关 / 配对栈）就此错位。钉死在左即把引号的左右判定**单一收口**到配对栈。
    ///
    /// 左右形取自 [`wind_punct::quote_forms`]（含自定义映射的 `"1`/`"2` 两行），**不可**直接用
    /// 内置的 `quote_pair`：那样判定按 `“”`、上屏按自定义值，用户把引号自定义成 `「」` 后判定
    /// 不命中却照样配对插入，错位复发。同时这也是「第二次」那行在配对下的唯一出路——钉左之后
    /// 它不再由交替态取用，而是作为**右符号**由配对补出。
    ///
    /// 多字符自定义值（如 `……`）不能充当配对符，此时返回 false 落普通标点流程。
    ///
    /// 须在标点流水线**之前**调用：钉的是本次转换的输入态，兼带清掉历史残留的「右」态。
    pub(crate) fn pin_quote_left_if_paired(&self, state: &State, ch: char) -> bool {
        let Some((l, r)) = wind_punct::quote_forms(
            &self.rt().config.input,
            state.chinese_punct,
            state.full_width,
            ch,
        ) else {
            return false;
        };
        let (Some(l), Some(r)) = (single_char(&l), single_char(&r)) else {
            return false;
        };
        let Some(pairs) = self.active_pairs(state.chinese_punct) else {
            return false;
        };
        if !pairs.iter().any(|p| *p == (l, r)) {
            return false;
        }
        self.punct
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pin_quote_left(ch);
        true
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
    ///
    /// **判据是「实际会上屏的产物」，含自定义映射的产物**（用户拍板，不给自定义键开后门）：
    /// 把 `"2` 配成 `￥` 而 `￥` 在 `symbol.smart_chars` 里 → 按第二次引号照常进入 `￥` 预览态、
    /// 再按一次换英文。不想要就把该符号从 `smart_chars` 移除，纯配置解决。
    ///
    /// 另注意三者的优先级：**智能符号（短路） > 自定义映射（定产物） > 自动配对（按产物补右符）**。
    /// 武装并 `HoldComposition` 会直接 return，配对逻辑当次完全不执行——排查「配对忽然不生效」
    /// 时先看这里有没有把键截走（真机指纹：日志出现 `SmartSymbol(HoldComposition): press1`）。
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
                            // 必须是 CommitReplacingHeld 而非 InsertText：held 的中文符号此刻
                            // 正显示在 C++ 的组合态里，press2 要**覆盖**它。普通 InsertText 在
                            // hold 活跃时是追加语义（held 并入前缀一起上屏），会打出「。.」。
                            return Some(KeyAction::CommitReplacingHeld {
                                text: rep,
                                chinese_mode: state.chinese_mode,
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

    /// 英文模式 + 全角：按键经完整标点流水线转全角后上屏（含自动配对）。
    ///
    /// **必须出字**：这些键已被 TSF 在 `OnTestKeyDown` 的 `english_fullwidth` 分支吃下
    /// （Letter|Number|Punctuation|Space，含小键盘）。此处返回 None → 调用方 PassThrough →
    /// 形成 `OnTestKeyDown(TRUE)+OnKeyDown(FALSE)` 的「吃了再吐」翻转，而 Chrome/Electron 等
    /// 严格 TSF 宿主不会回退合成 WM_CHAR，键会直接丢失（半角宿主则表现为出半角）。
    /// 故此处接住的键集必须覆盖 C++ 的吃键集，二者须同增同减。
    pub(crate) fn handle_english_full_width(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        let shift = data.modifiers & MOD_SHIFT != 0;
        let is_letter = (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code);
        // 键被吃下后系统不再代劳大小写，故由 CapsLock 镜像 XOR Shift 定。镜像每键都用事件
        // 携带的 toggles 快照校准（见 handle_key_event 开头），英文模式下同样可靠。
        let effective_shift = if is_letter {
            shift != state.caps_lock
        } else {
            shift
        };
        // 覆盖面须 ⊇ C++ 全角吃键集（含空格/小键盘），由 full_width_source_char 统一收口。
        let ch = full_width_source_char(data.key_code, effective_shift)?;

        // 「英全」= 英文标点 + 全角（自定义映射四态的列 1）。经完整流水线而非裸 to_full_width，
        // 确保用户自定义中英文符号生效（与 CapsLock+全角 路径同构）。
        let saved_punct = state.chinese_punct;
        state.chinese_punct = false;
        let piece = self.convert_punct_char(state, ch);
        let pairs = self.english_pairs_via_pipeline(state);
        state.chinese_punct = saved_punct;

        // 统计：C++ `OnKeyTraceDown` 在全角态主动跳过计数（注释「will be eaten by
        // OnTestKeyDown for full-width conversion」），把英文统计让给本路径；不记则恒为 0。
        self.record_english_key_stat(data.key_code, shift);

        debug!("English full-width: {:?} -> {:?}", ch, piece);

        if let Some(pairs) = pairs {
            let pch = piece.chars().last().unwrap_or(' ');
            // 智能跳过：输右符号且栈顶正是它 → 光标右移越过，不重复插入。
            // 对称配对（`＂＂` 等左右同形）除外：按键不携带开/闭这一位，无从判断跳出还是嵌套，
            // 故一律按「开新的一对」处理，跳出交给 `auto_pair.jump_out_keys`。
            // 非对称配对是否跳出由该列表里的 `right_symbol` 决定。
            if self.rt().jump_out_on_right_symbol
                && pairs.iter().any(|(l, r)| *r == pch && *l != *r)
            {
                let mut tr = self.pair_tracker.lock().unwrap_or_else(|e| e.into_inner());
                if tr.peek().is_some_and(|e| e.right == pch) {
                    tr.pop();
                    return Some(KeyAction::MoveCursorRight);
                }
                tr.clear();
            }
            // 插入配对：左符号 → 补右符号，光标置于其间。
            if let Some((_, right)) = pairs.iter().find(|(l, _)| *l == pch).copied() {
                self.pair_tracker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(pch, right);
                let cursor_offset = piece.encode_utf16().count() as u32;
                return Some(KeyAction::InsertTextWithCursor {
                    text: format!("{}{}", piece, right),
                    cursor_offset,
                });
            }
        }
        Some(Self::commit_action(piece, false))
    }

    /// 英文态下的配对表：把 `english_pairs` 的左右符号各过一遍**同一条**标点转换，即
    /// 「打出什么就配对什么」——用户改「英全 / 英半」列时配对自动跟随，无需另设配对自定义。
    /// 列随 `state.full_width` 定（英全 1 / 英半 3）。
    ///
    /// **不可复用 `cn_pairs`**：那是中文标点（【】《》「」），与 ASCII 的全角形并非同一字符
    /// （`to_full_width('[')` = `［` U+FF3B，而 cn_pairs 里是 `【` U+3010；只有 `（）｛｝`
    /// 恰好重合）。混用会出现「打 `[` 出 `【` 却配 `］`」的错位。
    ///
    /// **必须用 `peek_custom` 而非 `convert_punct_char`**：后者命中自定义映射时会推进引号交替
    /// 态，而本函数每次按键都要把整张配对表过一遍 → 一次按键就把引号态推进 N 格
    /// （`english_pairs` 含引号时立刻错位）。构造配对表是**查询**，不该有副作用。
    ///
    /// 调用前须已置 `state.chinese_punct = false`（「英文标点」列语义）。
    fn english_pairs_via_pipeline(&self, state: &State) -> Option<Vec<(char, char)>> {
        let rt = self.rt();
        if !rt.config.input.auto_pair.english {
            return None;
        }
        let col = wind_punct::punct_col_idx(false, state.full_width);
        let conv = self.punct.lock().unwrap_or_else(|e| e.into_inner());
        let form = |c: char| -> Option<char> {
            let s = conv
                .peek_custom(&rt.config.input.punct, c, col)
                .unwrap_or_else(|| {
                    let raw = c.to_string();
                    if state.full_width {
                        wind_transform::fullwidth::to_full_width(&raw)
                    } else {
                        raw
                    }
                });
            single_char(&s)
        };
        let pairs: Vec<(char, char)> = rt
            .en_pairs
            .iter()
            .filter_map(|(l, r)| Some((form(*l)?, form(*r)?)))
            .collect();
        (!pairs.is_empty()).then_some(pairs)
    }

    /// 英文模式 + 半角：只接手「英半列有自定义映射覆盖」的标点键，按英半列（col 3）出字。
    ///
    /// 为什么需要这条分支：英文模式非全角时 TSF 默认**直接透传**标点键，引擎收不到，四列里的
    /// 「英半」因此是打不到的死格（真机日志指纹：`decision=passthrough_not_handled`）。现由
    /// core 把「哪些源字符配了英半列」推给 DLL，DLL 精确吃下这些键转发过来，此处出字。
    ///
    /// **必须出字**（同 `handle_english_full_width` 的铁律）：接手判据与推给 DLL 的集合同源
    /// （`custom_english_punct_chars`），集合内即必有非空英半列值，故 `convert_punct` 必然命中
    /// 自定义、必然返回该值。返回 None 只发生在「键不在集合里」——那 DLL 本来就没吃它。
    ///
    /// 引号在此照常按左右形交替（`"1` → `"2` → `"1`…），交替态与中文侧共用同一个转换器，
    /// 中英模式切换时由 `punct.reset()` 归零（`coordinator.rs` 的两处 mode_switch）。
    ///
    /// **已知限制**（窄边缘，未修）：英文模式下配对栈此时分成两半——本路径接手的键入
    /// 协调器的 `pair_tracker`，其余英文标点仍入 DLL 的 `_englishPairEngine`。故若用户把某键的
    /// 英半列自定义成一个配对左符号，它插入的那对无法用 Tab 跳出（DLL 的跳出判据看的是自己那个
    /// 空栈）。要修需让 DLL 的跳出判定也认协调器侧的待跳出深度（中文侧 `_pairPendingDepth` 已有
    /// 同类机制可循）。触发条件是「自定义产物恰为配对符 + 英文模式 + 用跳出键」，故暂记不修。
    pub(crate) fn handle_english_custom_punct(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        let shift = data.modifiers & MOD_SHIFT != 0;
        let ch = punct_char(data.key_code, shift)?;
        if !self.rt().custom_en_punct_chars.contains(&ch) {
            return None; // DLL 未吃此键（判据同源），透传给宿主
        }

        // 「英半」= 英文标点 + 半角（列 3）。英文模式恒按英文标点列输出，与工具栏的
        // 中/英标点开关无关——这与改造前「DLL 透传、宿主出 ASCII」的既有语义一致。
        let saved_punct = state.chinese_punct;
        state.chinese_punct = false;
        let piece = self.convert_punct_char(state, ch);
        let pairs = self.english_pairs_via_pipeline(state);
        state.chinese_punct = saved_punct;

        // 统计：与全角路径同理，键被吃下后 C++ 的英文计数不再经过原路径，此处补记。
        self.record_english_key_stat(data.key_code, shift);
        debug!("English custom punct: {:?} -> {:?}", ch, piece);

        // 配对：DLL 对被吃下的键会跳过本地英文配对（让位 core），故此处须自行处理，
        // 否则「自定义产物恰是配对左符」时配对能力凭空消失。逻辑与全角路径同构。
        if let Some(pairs) = pairs {
            let pch = piece.chars().last().unwrap_or(' ');
            if self.rt().jump_out_on_right_symbol
                && pairs.iter().any(|(l, r)| *r == pch && *l != *r)
            {
                let mut tr = self.pair_tracker.lock().unwrap_or_else(|e| e.into_inner());
                if tr.peek().is_some_and(|e| e.right == pch) {
                    tr.pop();
                    return Some(KeyAction::MoveCursorRight);
                }
                tr.clear();
            }
            if let Some((_, right)) = pairs.iter().find(|(l, _)| *l == pch).copied() {
                self.pair_tracker
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(pch, right);
                let cursor_offset = piece.encode_utf16().count() as u32;
                return Some(KeyAction::InsertTextWithCursor {
                    text: format!("{}{}", piece, right),
                    cursor_offset,
                });
            }
        }
        Some(Self::commit_action(piece, false))
    }

    /// 英文模式按键的统计归类：**镜像** C++ `_RecordEnglishKeyTrace` 的分桶（Shift+数字算标点、
    /// 小键盘数字算数字…），保证全角/半角两条路径统计口径一致。
    fn record_english_key_stat(&self, key_code: u32, shift: bool) {
        let (chars, digits, puncts, spaces) = if (keymap::VK_A..=keymap::VK_Z).contains(&key_code) {
            (1, 0, 0, 0)
        } else if (keymap::VK_0..=keymap::VK_9).contains(&key_code) {
            if shift { (0, 0, 1, 0) } else { (0, 1, 0, 0) }
        } else if key_code == keymap::VK_SPACE {
            (0, 0, 0, 1)
        } else if numpad_char(key_code).is_some_and(|c| c.is_ascii_digit()) {
            (0, 1, 0, 0)
        } else {
            (0, 0, 1, 0)
        };
        self.handle_english_stats(chars, digits, puncts, spaces);
    }

    /// 清空配对跟踪栈（焦点/模式切换等的防御性复位，与 `disarm_smart_symbol` 并列调用）。
    /// 焦点/模式一旦切换，旧的「光标紧贴右符号」假设即失效，残留栈会让跳出键/右符号跳出误判，
    /// 故必须清空。
    pub(crate) fn clear_pair_tracker(&self) {
        self.pair_tracker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}
