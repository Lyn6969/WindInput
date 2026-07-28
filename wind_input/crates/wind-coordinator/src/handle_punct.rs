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

    /// press1 **实际会上屏**的串（无副作用镜像 `convert_punct`）。
    ///
    /// 与 [`Self::compute_punct_str_pure`] 的差别**只在英文半角这一格**：pure 那条路刻意不查
    /// 自定义（它的语义是「press2 的替换目标，须保持原样英文」），而武装串必须等于真正插进
    /// 文档的东西——用户把 `;` 的英半列配成 `#` 时，press1 上屏 `#` 而武装串若还是 `;`，
    /// press2 的 `prev_char` 比对就永远失配，功能静默失效。
    ///
    /// 三条反向通路（数字后智能 / 英文标点状态 / 英文输入模式）的 press1 都落在英文列，
    /// 故都必须走这里而不是 pure。
    fn press1_committed_str(&self, state: &State, ch: char, chinese: bool) -> Option<String> {
        if !chinese && !state.full_width {
            let bundle = self.rt();
            let conv = self.punct.lock().unwrap_or_else(|e| e.into_inner());
            // 英半列（col 3）：先查自定义，无值回落原样 ASCII——与 `convert_punct` 同一条路。
            if let Some(v) = wind_punct::custom_lookup(&conv, &bundle.config.input, ch, 3) {
                return Some(v);
            }
            return Some(ch.to_string());
        }
        self.compute_punct_str_pure(state, ch, chinese)
    }

    /// 智能符号三个总开关是否有任一开启（入口短路用；具体该走哪个由上下文判定各自再查）。
    fn any_smart_symbol_enabled(&self) -> bool {
        let s = &self.rt().config.input.symbol;
        s.smart_mode || s.english_punct_mode || s.english_mode
    }

    /// 计算 `ch` 本次按下会进入的武装态：`Some((press1 实际上屏串, reverse))`；不参与返回 None。
    /// 对齐 Go `smartSymbolArmStr`（Go 版只有正向）：仅中文标点模式 + 在参与集合内。
    ///
    /// **判据是「实际会上屏的产物」，含自定义映射的产物**（用户拍板，不给自定义键开后门）：
    /// 把 `"2` 配成 `￥` 而 `￥` 在 `symbol.smart_chars` 里 → 按第二次引号照常进入 `￥` 预览态、
    /// 再按一次换英文。不想要就把该符号从 `smart_chars` 移除，纯配置解决。
    ///
    /// **数字后智能标点走反向**（`reverse=true`）：`3.` 这类场景 press1 照旧出英文 `.`（数字后
    /// 语义不变），但**不再是终点**——时限内再按一次换回中文 `。`。此前这里直接 `return None`
    /// 拒绝武装，于是数字后想打中文标点只能去关掉「数字后智能」总开关，粒度粗到没法用。
    ///
    /// **参与集合恒按中文产物判定**：`symbol.smart_chars` 存的是中文标点（`。，？！…`），反向时
    /// 上屏的虽是英文形，参与与否仍问它的中文形——否则用户得再维护一份英文列表，且同一个键在
    /// 两个方向上会给出不一致的答案。**英文标点状态则相反**，按源字符查 `symbol.english_chars`
    /// （理由见 [`wind_punct::english_participates`]）。
    ///
    /// 另注意三者的优先级：**智能符号（短路） > 自定义映射（定产物） > 自动配对（按产物补右符）**。
    /// 武装并 `HoldComposition` 会直接 return，配对逻辑当次完全不执行——排查「配对忽然不生效」
    /// 时先看这里有没有把键截走（真机指纹：日志出现 `SmartSymbol(HoldComposition): press1`）。
    ///
    /// 只管中文输入模式下的两种上下文；英文输入模式走 [`Self::english_mode_smart_symbol`]
    /// （那条路的标点键要先从 DLL 手里要回来，state 上的 `chinese_punct` 也不代表实际产物列）。
    pub(crate) fn smart_symbol_arm_str(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
    ) -> Option<(String, bool)> {
        let bundle = self.rt();
        let sym = &bundle.config.input.symbol;
        // 上下文一：英文标点状态（中文输入 + 工具栏标点切英文）。恒反向，独立开关、独立集合。
        if !state.chinese_punct {
            if !sym.english_punct_mode
                || !wind_punct::english_participates(&bundle.config.input, ch)
            {
                return None;
            }
            let out = self.press1_committed_str(state, ch, false)?;
            return self.reject_if_auto_paired(state, out).map(|o| (o, true));
        }
        // 上下文二：中文标点状态。正向；其中数字后智能标点反向（press1 仍出英文）。
        if !sym.smart_mode {
            return None;
        }
        let reverse = self.is_smart_punct_after_digit(ch, prev_char);
        let cn = self.compute_punct_str_pure(state, ch, true)?;
        if !self.smart_symbol_participates(&cn) {
            return None;
        }
        // press1 实际上屏串：反向取英文产物——与普通标点流程一致（`convert_punct` 对数字后智能
        // 同样落英文列），两条路必须产出同一个串，否则 press2 的删除数就对不上光标前的内容。
        let out = if reverse {
            self.press1_committed_str(state, ch, false)?
        } else {
            cn
        };
        self.reject_if_auto_paired(state, out).map(|o| (o, reverse))
    }

    /// 与自动配对互斥：被配对的符号（单字符且在生效配对表）不武装智能符号，返回 None。
    ///
    /// 否则 press1 插入配对并回退光标至中间，press2 时 `prev_char` 恰为配对左符号 → 误删左符号
    /// 改成另一形态、留下孤零零的右符号。判据用**实际上屏串**：反向时插进文档的是英文形，
    /// 配对与否得问它。
    fn reject_if_auto_paired(&self, state: &State, out: String) -> Option<String> {
        if out.chars().count() == 1 {
            let c0 = out.chars().next().unwrap();
            if self.is_auto_pair_char(state, c0) {
                return None;
            }
        }
        Some(out)
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
        if !self.any_smart_symbol_enabled() {
            return None;
        }
        let method = self.rt().config.input.symbol.smart_method.clone();
        let timeout_ms = self.smart_symbol_timeout().as_millis() as u32;
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(act) = self.smart_symbol_press2(state, ch, prev_char, &method, &mut arm) {
            return Some(act);
        }

        // ── press1：尝试武装 ─────────────────────────────────────────────────────
        match self.smart_symbol_arm_str(state, ch, prev_char) {
            Some((out, reverse)) => {
                arm.key = ch;
                arm.str = out.clone();
                arm.at = Some(std::time::Instant::now());
                arm.reverse = reverse;
                arm.mode_snapshot = (state.chinese_mode, state.chinese_punct);

                match method {
                    SmartMethod::HoldComposition => {
                        let has_input =
                            !state.input_buffer.is_empty() || !state.committed_text.is_empty();
                        arm.armed = true;
                        if has_input {
                            // 有活跃编码时，不短路进入 hold composition——让调用方的标点分支
                            // 检测 hold_pending_commit 并生成 CommitAndHoldComposition：先顶屏
                            // 上屏候选，再开 HoldComposition 放入标点。
                            arm.held_text = None;
                            arm.hold_pending_commit = true;
                        } else {
                            arm.held_text = Some(out.clone());
                            debug!(
                                "SmartSymbol(HoldComposition): press1, hold composition: {}, reverse={}, timeout={}ms",
                                out, reverse, timeout_ms
                            );
                            // 短路返回：由 C++ 端负责开启组合态和计时，不走普通标点流程
                            return Some(KeyAction::HoldComposition {
                                text: out,
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

    /// press2 判定（同键、时限内）：命中则解除武装并返回替换响应，未命中返回 None 不动武装态。
    ///
    /// 替换目标由 `arm.reverse` 定：正向换英文产物、反向换中文产物。两个方向共用同一套删改/组合
    /// 覆盖机制——差别只在「取哪一列产物」，故此处只有 `!arm.reverse` 这一个分歧点。
    fn smart_symbol_press2(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
        method: &SmartMethod,
        arm: &mut crate::coordinator::SmartSymbolArm,
    ) -> Option<KeyAction> {
        if !arm.armed
            || ch != arm.key
            // 上下文必须与 press1 时一致：三种上下文各有独立开关与独立产物列，press1 后用户
            // 切了中英模式或标点模式，这一按就该当全新 press1，而不是按旧方向删掉文档里的字。
            || arm.mode_snapshot != (state.chinese_mode, state.chinese_punct)
            || !arm
                .at
                .map(|t| t.elapsed() < self.smart_symbol_timeout())
                .unwrap_or(false)
        {
            return None;
        }
        // 替换产物取**武装方向的另一侧**：正向 press1 出中文 → press2 出英文；反向反之。
        let target_chinese = arm.reverse;
        // 引号交替态修正：正向时 press1 已由 `convert_punct` 推进过一格，换英文后须退回；
        // 反向时 press1 出的是英文（未推进），而 press2 真吐了一个中文引号，须补进一格。
        // 引号只有左/右两态，「退回」与「补进」都是同一个翻转，故两个方向共用 revert_last_quote。
        let fix_quote = |this: &Self| {
            if ch == '\'' || ch == '"' {
                this.punct
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .revert_last_quote(ch);
            }
        };
        let dir = if target_chinese { "en->cn" } else { "cn->en" };

        match method {
            SmartMethod::HoldComposition if arm.held_text.is_some() => {
                // 正常 hold 路径：press1 时无活跃编码，组合态内无需 prev_char 验证，直接提交。
                let rep = self.compute_punct_str_pure(state, ch, target_chinese)?;
                arm.armed = false;
                arm.held_text = None;
                fix_quote(self);
                debug!(
                    "SmartSymbol(HoldComposition): press2 {}, commit: {}",
                    dir, rep
                );
                // 必须是 CommitReplacingHeld 而非 InsertText：held 的符号此刻正显示在 C++ 的
                // 组合态里，press2 要**覆盖**它。普通 InsertText 在 hold 活跃时是追加语义
                // （held 并入前缀一起上屏），会打出「。.」。
                Some(KeyAction::CommitReplacingHeld {
                    text: rep,
                    chinese_mode: state.chinese_mode,
                })
            }
            // 两条走删改的路：DeleteReplace 方案，以及 HoldComposition 方案下 press1 时有活跃编码
            // （标点已作文本顶屏提交、非组合态）或符号由模式进入键直接上屏（见
            // `arm_smart_symbol_after_commit`）——`held_text.is_none()` 即这两种降级情形。
            //
            // 光标前字符须与武装串末位匹配；prev_char==0 视为"宿主读不回文档"（微信/Windows
            // Terminal 等 Qt/ConPTY 宿主的 TSF OnEndEdit 里 GetSelection/GetText 经常拿不到内容），
            // 而不是"确定不匹配"——press2 时光标前必然至少有 press1 刚提交的符号，真读到 0 只可能
            // 是读失败。此时退回只信服务端自己的武装态（armed+key+timeout，与文档内容无关），
            // 否则永远判定不是 press2。
            _ => {
                let armed_runes: Vec<char> = arm.str.chars().collect();
                let &last = armed_runes.last()?;
                if prev_char != 0 && last as u32 != prev_char as u32 {
                    return None;
                }
                let rep = self.compute_punct_str_pure(state, ch, target_chinese)?;
                arm.armed = false;
                fix_quote(self);
                debug!(
                    "SmartSymbol(replace): press2 {}, count={}, text={}",
                    dir,
                    armed_runes.len(),
                    rep
                );
                Some(KeyAction::ReplaceBackward {
                    count: armed_runes.len() as u32,
                    text: rep,
                })
            }
        }
    }

    /// 「符号已实打实上屏」后的智能符号武装 —— 模式进入键二次按下专用。
    ///
    /// 场景：`;`（快捷输入）/ `` ` ``（临时拼音）/ `\`（特殊模式）这类**被模式占用的符号键**，
    /// 在模式内空缓冲时再按一次会上屏它的中文标点并退出模式（三处调用：`handle_mode.rs` /
    /// `handle_temp.rs` / `handle_special.rs`）。此前那就是终点——想要英文形没有任何便捷通路，
    /// 因为该键在空闲态一按就又进模式了。现在这一步顺手武装智能符号：时限内再按同键即换英文。
    ///
    /// **恒按删改语义武装**（`held_text=None`）：符号是经 `CommitText` 真上屏的，不存在组合态可
    /// 覆盖，故 press2 必须走 `ReplaceBackward`。用户选 `HoldComposition` 方案时亦然——
    /// `smart_symbol_press2` 的 `held_text.is_none()` 分支就是这条降级路径。
    ///
    /// **press2 的拦截点在 `try_activate_mode` 开头**（`handle_lifecycle.rs`），必须早于模式激活
    /// 链：空闲态按 `;` 会被模式进入抢走，永远走不到标点分支的智能符号判定，武装也就白武装。
    pub(crate) fn arm_smart_symbol_after_commit(&self, state: &State, ch: char, out: &str) {
        if !self.rt().config.input.symbol.smart_mode || !state.chinese_punct {
            return;
        }
        // 参与集合按实际上屏串判定（与 `smart_symbol_arm_str` 同一判据）：符号不在
        // `symbol.smart_chars` 里就不武装，行为与改造前完全一致。
        if !self.smart_symbol_participates(out) {
            return;
        }
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
        arm.armed = true;
        arm.reverse = false;
        arm.key = ch;
        arm.str = out.to_string();
        arm.at = Some(std::time::Instant::now());
        arm.mode_snapshot = (state.chinese_mode, state.chinese_punct);
        arm.held_text = None;
        arm.hold_pending_commit = false;
        debug!("SmartSymbol(mode trigger): armed after commit: {}", out);
    }

    /// 英文输入模式（`chinese_mode=false`）的智能符号：press2 判定 + press1 武装，一次收口。
    ///
    /// 与中文输入模式那条路分开的三个理由：
    ///   1. **键得先要回来**。英文半角下 DLL 默认透传标点键，引擎收不到；开了 `english_mode`
    ///      后 core 把 `english_chars` 并入 `CONFIG_KEY_CUSTOM_EN_PUNCT` 推送，DLL 才吃下转发。
    ///      因此本函数的调用点必须在 [`Self::handle_english_custom_punct`] /
    ///      [`Self::handle_english_full_width`] 里——那是英文模式下标点键唯一的落点。
    ///   2. **`state.chinese_punct` 在这条路上不代表产物列**。英文模式恒按英文列出字，与工具栏
    ///      的中/英标点开关无关（`handle_english_custom_punct` 里临时置 false 就是这个意思），
    ///      故上下文判定只能看 `chinese_mode`。
    ///   3. 开关独立（`english_mode` vs `english_punct_mode`）——用户可以只要其中一个。
    ///
    /// `committed` 是本次 press1 会上屏的串（调用方已算好，含配对/全角处理前的标点本体）。
    /// 返回 `Some` 表示这次是 press2，调用方应短路返回该替换响应。
    pub(crate) fn english_mode_smart_symbol(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
        committed: &str,
    ) -> Option<KeyAction> {
        let bundle = self.rt();
        if !bundle.config.input.symbol.english_mode {
            return None;
        }
        let method = bundle.config.input.symbol.smart_method.clone();
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(act) = self.smart_symbol_press2(state, ch, prev_char, &method, &mut arm) {
            return Some(act);
        }
        // press1：仅参与集合内的键武装。配对符天然被排除——调用点在配对分支**之后**，插对/跳出
        // 的键根本走不到这里（理由同 `reject_if_auto_paired`：press1 插了一对后 `prev_char`
        // 会是配对左符号，press2 会误删它）。
        if !wind_punct::english_participates(&bundle.config.input, ch) {
            arm.armed = false;
            arm.held_text = None;
            return None;
        }
        arm.armed = true;
        // 恒反向：英文模式 press1 出英文形，press2 换中文形。
        arm.reverse = true;
        arm.key = ch;
        arm.str = committed.to_string();
        arm.at = Some(std::time::Instant::now());
        arm.mode_snapshot = (state.chinese_mode, state.chinese_punct);
        // 恒按删改语义武装：英文模式下 press1 是经 `CommitText` 真上屏的（无组合态可覆盖），
        // 与模式进入键那条通路同理，press2 走 `ReplaceBackward` 降级分支。
        arm.held_text = None;
        arm.hold_pending_commit = false;
        debug!("SmartSymbol(english mode): press1 armed: {}", committed);
        None
    }

    /// 只做 press2 判定、不做 press1 武装 —— 供 `try_activate_mode` 抢在模式激活链之前调用。
    /// 武装职责仍单一收口在 `try_smart_symbol_replace`（标点分支）与
    /// `arm_smart_symbol_after_commit`（模式进入键），此处只负责「别让模式进入吃掉 press2」。
    pub(crate) fn try_smart_symbol_press2_only(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
    ) -> Option<KeyAction> {
        if !self.any_smart_symbol_enabled() {
            return None;
        }
        let method = self.rt().config.input.symbol.smart_method.clone();
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
        self.smart_symbol_press2(state, ch, prev_char, &method, &mut arm)
    }

    /// 解除智能符号待命态（焦点变化/模式切换等的防御性复位）。
    pub(crate) fn disarm_smart_symbol(&self) {
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
        arm.armed = false;
        arm.reverse = false;
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
        // 英文智能符号（`symbol.english_mode`）：同键连按把英文标点换成中文形。置于配对分支
        // **之后**——插对/跳出的键不参与武装，否则 press2 会误删配对左符号。
        if let Some(act) = self.english_mode_smart_symbol(state, ch, data.prev_char, &piece) {
            return Some(act);
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
        // 英文智能符号（`symbol.english_mode`）：同键连按把英文标点换成中文形。置于配对分支
        // **之后**——插对/跳出的键不参与武装，否则 press2 会误删配对左符号。
        //
        // 注意本函数的接手判据 `custom_en_punct_chars` 已在 `ConfigBundle::build` 里并入了
        // `english_chars`，所以开了 `english_mode` 的键即使没配英半自定义也会走到这里
        // （出原样 ASCII，与透传等价），press1 才有落点。
        if let Some(act) = self.english_mode_smart_symbol(state, ch, data.prev_char, &piece) {
            return Some(act);
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
