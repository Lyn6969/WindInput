//! 临时拼音 / 临时英文输入模式
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 触发键判定、进入/退出、候选刷新、按键处理、选词上屏。

use crate::coordinator::{
    Coordinator, ENGINE_MAX_CANDIDATES, State, TEMP_PINYIN_MAX_CANDIDATES, en_case_variants,
    numpad_char, punct_char,
};
use crate::pipeline::{ModeKind, Rewind};
use crate::preedit_cursor;
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_ipc::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT};
use wind_keys::keymap;
use wind_transform::fullwidth::to_full_width;

impl Coordinator {
    /// 触发键名 → VK（**符号**触发键统一映射，见 `keymap`）。字母触发键（z）刻意不经此——
    /// 走独立的 [`Self::matched_letter_temp_trigger`] + 三重身份裁决（对齐 Go `matchTempPinyinTrigger`
    /// 排除 z + `judgeZFirstTrigger`），避免 z 在符号语义路径（如缓冲非空顶屏进临拼）被误触发。
    pub(crate) fn temp_pinyin_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk(key)
    }

    /// VK → 组合区前缀字符（统一映射，见 `keymap`；缺省回退反引号）
    pub(crate) fn temp_pinyin_prefix_for(key_code: u32) -> char {
        keymap::vk_to_prefix_char(key_code).unwrap_or('`')
    }

    /// 当前按键是否匹配配置的**符号**临时拼音触发键（字母触发键见 `matched_letter_temp_trigger`）
    pub(crate) fn is_temp_pinyin_trigger(&self, key_code: u32) -> bool {
        self.rt()
            .config
            .input
            .temp_pinyin
            .trigger_keys
            .iter()
            .filter_map(|k| Self::temp_pinyin_trigger_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 临时拼音「字母触发键」（如 z）匹配：当前键是字母、且该字母在 `trigger_keys` 中配置时返回
    /// 该字母（小写）。与符号触发键分开——符号键无条件触发，字母键需经三重身份裁决
    /// （repeat / 正常码字母 / 临时拼音），对齐 Go `judgeZFirstTrigger`。
    pub(crate) fn matched_letter_temp_trigger(&self, key_code: u32) -> Option<char> {
        if !(keymap::VK_A..=keymap::VK_Z).contains(&key_code) {
            return None;
        }
        let ch = (b'a' + (key_code - keymap::VK_A) as u8) as char;
        let hit = self
            .rt()
            .config
            .input
            .temp_pinyin
            .trigger_keys
            .iter()
            .any(|k| {
                let k = k.trim().to_lowercase();
                k.len() == 1 && k.as_bytes()[0] == ch as u8
            });
        hit.then_some(ch)
    }

    /// z 三重身份裁决的「活码前缀」判据（对齐 Go `HasPrefix`）：码表引擎候选（含 BFS 前缀扫描）
    /// 或短语层存在以 `code` 开头的条目 → true。用于区分 z 作正常码字母（有前缀，如自定义 `zhang`）
    /// vs 临时拼音触发（死前缀，如标准五笔 86 的 z）。
    pub(crate) fn has_code_prefix(&self, code: &str) -> bool {
        if code.is_empty() {
            return false;
        }
        // 码表 / 用户词（convert 内含前缀扫描）。
        if !self.engine_mgr.convert(code, 1).candidates.is_empty() {
            return true;
        }
        // 短语层：精确或前缀命中。
        let phrases = self.phrases.read().unwrap_or_else(|e| e.into_inner());
        if phrases.is_empty() {
            return false;
        }
        let recent = self.recent_commits_snapshot();
        let clip = |_n: i64| String::new();
        !phrases.lookup(code, &recent, &clip).is_empty()
            || !phrases.lookup_prefix(code, &recent, 1).is_empty()
    }

    /// z-fallback 夺取（对齐 Go `decideEngineDefaultZFallback` + `enterTempPinyinFromZBuffer`）：
    /// **码表引擎** + 缓冲以 z 开头 + z 配为字母触发键，且缓冲加新键 `ch` 后 `z…` 不再是活码前缀，
    /// 则判定首 z 实为拼音触发键——抛弃首 z，`buffer[1:]+ch` 作临时拼音编码切入，并武装退格 rewind
    /// （首次退格还原到正常码表输入流 `buffer+ch`）。返回 `Some` 表示已夺取，`None` 表示不夺取。
    /// 混输引擎排除（避免 `zhang` 丢首字母，对齐 Go 门禁）。
    pub(crate) fn try_z_fallback(&self, state: &mut State, ch: char) -> Option<KeyAction> {
        if !matches!(
            self.engine_mgr.current_engine_type(),
            Some(wind_engine::EngineType::CodeTable)
        ) {
            return None;
        }
        if !state.input_buffer.starts_with('z') {
            return None;
        }
        // z 必须配为字母触发键。
        if self.matched_letter_temp_trigger(keymap::VK_Z).is_none() {
            return None;
        }
        let combined = format!("{}{}", state.input_buffer, ch);
        // 加新键后仍是活码前缀（如 zhang 存在时的 "zh"）→ 不夺取，继续正常码表。
        if self.has_code_prefix(&combined) {
            return None;
        }
        let target = self.engine_mgr.temp_pinyin_target()?;
        // residual = 去掉首 z + 新键；snapshot = 正常码流（combined），供退格 rewind 还原。
        let residual = format!("{}{}", &state.input_buffer[1..], ch);
        state.active = Some(ModeKind::TempPinyin);
        state.temp_pinyin_schema = target;
        state.temp_pinyin_buffer = residual.clone();
        state.temp_pinyin_cursor = state.temp_pinyin_buffer.len();
        state.temp_pinyin_prefix = "z".to_string();
        state.rewind = Some(Rewind {
            snapshot: combined,
            host_text: residual,
        });
        state.input_buffer.clear();
        state.candidates.clear();
        self.update_temp_pinyin_candidates(state);
        let display = state.preedit.clone();
        self.notify_ui_update(state);
        debug!(
            "z-fallback: hijacked to temp pinyin (buffer={})",
            state.temp_pinyin_buffer
        );
        Some(KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        })
    }

    /// 当前按键是否匹配配置的临时英文触发键
    pub(crate) fn is_temp_english_trigger(&self, key_code: u32) -> bool {
        self.rt()
            .config
            .input
            .temp_english
            .trigger_keys
            .iter()
            .filter_map(|k| keymap::key_name_to_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 临拼：回退最后一个已转换段——把它消费的码并回缓冲**前部**并重转，光标落码末尾
    /// （理由同主输入的 `pop_committed_seg`）。Backspace（段优先）与 Delete（删空后）共用。
    fn pop_temp_pinyin_seg(&self, state: &mut State) -> KeyAction {
        let Some((raw_code, _, _, _, _)) = state.committed_segs.pop() else {
            return KeyAction::Consumed;
        };
        state.committed_text = state
            .committed_segs
            .iter()
            .map(|(_, _, t, _, _)| t.as_str())
            .collect();
        state.temp_pinyin_buffer = format!("{}{}", raw_code, state.temp_pinyin_buffer);
        state.temp_pinyin_cursor = state.temp_pinyin_buffer.len();
        self.update_temp_pinyin_candidates(state);
        let display = state.preedit.clone();
        let caret_pos = self.overlay_caret(state);
        self.notify_ui_update(state);
        KeyAction::UpdateComposition {
            caret_pos,
            text: display,
        }
    }

    /// 退出临时拼音模式并清空相关状态（含逐步转换的已转换前缀）
    pub(crate) fn exit_temp_pinyin(&self, state: &mut State) {
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_cursor = 0;
        state.temp_pinyin_schema.clear();
        state.temp_pinyin_prefix.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 临拼向引擎取数的上限：目标方案是拼音类才取全量。
    ///
    /// 目标方案来自用户配置 `schema.primary_pinyin`，**该配置没有类型校验**——手改配置文件
    /// 指向码表方案时，取全量会是 34.9MB 峰值 + 39.6ms 的严重劣化（码表单字母候选达 5472 条）。
    /// 故按引擎类型分流：非拼音类退回原有小批量。
    ///
    /// 用 `loaded_engine_type` 而非 `schema_engine_type`：后者每次都读文件 + 解析 TOML，
    /// 本函数在逐键路径上。目标方案此时必然已加载（`temp_pinyin_target` 内部调过 `ensure_loaded`）。
    fn temp_pinyin_limit(&self, schema: &str) -> usize {
        match self.engine_mgr.loaded_engine_type(schema) {
            Some(wind_engine::EngineType::Pinyin) => TEMP_PINYIN_MAX_CANDIDATES,
            _ => ENGINE_MAX_CANDIDATES,
        }
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
            state.overlay_body = state.temp_pinyin_buffer.clone();
            return;
        };
        let limit = self.temp_pinyin_limit(&schema);
        let result = self
            .engine_mgr
            .convert_with(&schema, &state.temp_pinyin_buffer, limit);
        let display = if result.preedit_display.is_empty() {
            state.temp_pinyin_buffer.clone()
        } else {
            result.preedit_display
        };
        state.preedit = format!("{}{}", prefix, display);
        state.overlay_body = display; // 供光标换算（含引擎插入的音节分隔符，与缓冲不同形）

        // 临时拼音候选按词库权重排序（其词频维度涉及特殊模式配置归属，待 S1 引擎层处理）。
        let mut candidates = result.candidates;
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        // 截断值必须跟取数上限同源：这两处曾同用一个常量兼任「取多少」与「留多少」，
        // 只改一处会出现「取了 5000 条又砍回 50」。
        candidates.truncate(limit);
        // 统一展开汇聚点：临时拼音词库候选内 `$` 特殊语法在此展开（见 finalize_candidates）。
        let mut candidates = self.finalize_candidates(candidates, &state.temp_pinyin_buffer);
        // 检索范围过滤，与主路径同序：mark_common（判定，无条件）→ apply_filter（按模式裁剪）。
        // **必须在 finalize 之后**：过滤的保留条件含 `is_command` / `is_group`，而这两个标志
        // 正是 finalize_candidates 展开 `$CC`/`$AA` 时才置位的，提前过滤会把命令/组候选误删。
        //
        // 临拼此前完全不接过滤——「检索范围」设置对它从来无效，且默认 smart 下临拼比主路径
        // 多出数百个生僻字候选（实测 `ying`：临拼 299 条 vs 主路径 76 条）。
        self.mark_common(&mut candidates);
        self.apply_filter(state, &mut candidates);
        state.candidates = candidates;
        // 简繁 1对多变体展开（约束见 expand_s2t_variants 文档）。
        self.expand_s2t_variants(state);
    }

    /// 临时拼音选词 —— 组合区逐步转换（C）。部分匹配并入 committed 前缀留模式内（不上屏）；
    /// 完整匹配整体上屏 committed+候选（前缀触发键不输出）+ 造词，退出。返回最终 KeyAction。
    pub(crate) fn commit_temp_pinyin_selected(
        &self,
        state: &mut State,
        cand: &Candidate,
        candidate_pos: i32,
    ) -> KeyAction {
        // $AA/$SS 组折叠候选：补全编码到完整码并重查展开（二级选择，不上屏组名）。
        if cand.is_group {
            state.temp_pinyin_buffer = cand.group_code.clone();
            state.temp_pinyin_cursor = state.temp_pinyin_buffer.len(); // 补全到完整码：光标落末尾
            self.update_temp_pinyin_candidates(state);
            let display = state.preedit.clone();
            let caret_pos = self.overlay_caret(state);
            self.notify_ui_update(state);
            return KeyAction::UpdateComposition {
                caret_pos,
                text: display,
            };
        }
        // $CC 命令候选：执行动作（退出临拼后异步跑），不走文本/分段上屏。
        let cmd_code = state.temp_pinyin_buffer.clone();
        if let Some(act) =
            self.overlay_commit_command(state, cand, &cmd_code, |s, st| s.exit_temp_pinyin(st))
        {
            return act;
        }
        let total = state.temp_pinyin_buffer.len();
        let consumed = cand.consumed_length;
        let code = Self::cand_code(&state.temp_pinyin_buffer, cand);
        let partial =
            consumed > 0 && consumed < total && state.temp_pinyin_buffer.is_char_boundary(consumed);
        // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
        self.record_selection(
            &self.freq_code(&state.temp_pinyin_buffer, cand),
            &cand.text,
            cand.source,
        );
        // 输入统计：每次临拼选词记一段（来源临时拼音）。
        self.record_commit(
            &cand.text,
            code.len() as u32,
            candidate_pos,
            wind_store::stats::CommitSource::TempPinyin,
        );
        let raw_code = Self::raw_consumed_code(&state.temp_pinyin_buffer, consumed, partial);
        if partial {
            state.committed_segs.push((
                raw_code,
                code,
                cand.text.clone(),
                cand.source,
                cand.boundary,
            ));
            state.committed_text.push_str(&cand.text);
            state.temp_pinyin_buffer = state.temp_pinyin_buffer[consumed..].to_string();
            // 分步确认消费掉前缀码：光标落剩余码末尾
            state.temp_pinyin_cursor = state.temp_pinyin_buffer.len();
            self.update_temp_pinyin_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            state.committed_segs.push((
                raw_code,
                code,
                cand.text.clone(),
                cand.source,
                cand.boundary,
            ));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state);
            // 变体候选末段用覆盖文本；普通候选整体转换（保留 STPhrases 跨段词级消歧）。
            let out = match &cand.s2t_override {
                Some(t) => format!("{}{}", self.maybe_s2t(state, &state.committed_text), t),
                None => self.maybe_s2t(state, &final_simplified),
            };
            self.exit_temp_pinyin(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 临时拼音模式下的按键处理
    pub(crate) fn handle_temp_pinyin_key(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> KeyAction {
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        // 编码区光标移动（左右 / Home / End）；置于候选导航之后，导航键优先。
        if let Some(act) = self.overlay_cursor_key(state, data) {
            return act;
        }
        // 进入键二次按下（缓冲空 + 无已转换前缀）：按中英标点配置上屏该符号并退出。
        // 顺带武装智能符号：时限内再按同键即换英文形——否则这个键被模式占着，英文形没有通路
        // （空闲态一按就又进模式）。press2 的拦截在 try_activate_mode 开头，早于模式激活链。
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
                self.arm_smart_symbol_after_commit(state, ch, &out);
                self.record_commit(&out, 0, -1, wind_store::stats::CommitSource::Punctuation);
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
            keymap::VK_BACK | keymap::VK_DELETE => {
                // Backspace：段回退**优先于光标**（有已转换段先退回最后一段，你→ni，码并回缓冲
                // 前部）；否则删光标前一字符。Delete 只删光标后一字符、删空后才回退段——与主输入
                // 同构的刻意不对称（见 coordinator.rs 的 VK_DELETE 臂）。皆空则退出。
                let backward = data.key_code == keymap::VK_BACK;
                if backward && !state.committed_segs.is_empty() {
                    return self.pop_temp_pinyin_seg(state);
                }
                if state.temp_pinyin_buffer.is_empty() {
                    if backward {
                        self.exit_temp_pinyin(state);
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    // Delete 且剩余拼音已空（只剩只读前缀）：吃掉，不改变退出语义。
                    return KeyAction::Consumed;
                }
                let removed = {
                    let mut ed = preedit_cursor::BufEdit::new(
                        &mut state.temp_pinyin_buffer,
                        &mut state.temp_pinyin_cursor,
                    );
                    if backward {
                        ed.backspace()
                    } else {
                        ed.delete()
                    }
                };
                if !removed {
                    // 退格时光标已在最左 / Delete 时已在末尾：吃掉不透传。
                    return KeyAction::Consumed;
                }
                if state.temp_pinyin_buffer.is_empty() {
                    if !state.committed_segs.is_empty() {
                        return self.pop_temp_pinyin_seg(state);
                    }
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                let caret_pos = self.overlay_caret(state);
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    caret_pos,
                    text: display,
                }
            }
            keymap::VK_SPACE => {
                // 空格：选当前高亮候选（逐步转换）
                if !state.candidates.is_empty() {
                    let (start, _) = self.page_range(state);
                    let idx = (start + state.selected_index).min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand, (idx - start) as i32)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // clear 模式：整段放弃，不上屏任何内容（含已选词的 committed_text）。
                // 须先于下方各分支——此前该判断只写在「空缓冲」分支内，导致「打了码再回车」
                // 仍走非空缓冲路径无条件上屏原码，配置形同虚设（与主输入路径行为不一致）。
                if self.enter_clears_composition() {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                // 空缓冲（只按了模式键、无已转换前缀）：commit 模式上屏模式键符号本身
                // （原样不转换，如 `）。
                if state.temp_pinyin_buffer.is_empty() && state.committed_text.is_empty() {
                    if !state.temp_pinyin_prefix.is_empty() {
                        let sym = state.temp_pinyin_prefix.clone();
                        self.record_commit(
                            &sym,
                            0,
                            -1,
                            wind_store::stats::CommitSource::Punctuation,
                        );
                        self.exit_temp_pinyin(state);
                        self.notify_ui_hide();
                        return Self::commit_action(sym, true);
                    }
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                // 非空缓冲：上屏「已转换前缀 + 剩余拼音原码」（原行为不变，如 `nihao → nihao）。
                // committed 段已在选词时记过，此处只记剩余拼音原码避免重复。
                self.record_commit(
                    &state.temp_pinyin_buffer,
                    state.temp_pinyin_buffer.len() as u32,
                    -1,
                    wind_store::stats::CommitSource::TempPinyin,
                );
                let out = self.maybe_s2t(
                    state,
                    &format!("{}{}", state.committed_text, state.temp_pinyin_buffer),
                );
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
                    self.commit_temp_pinyin_selected(state, &cand, (data.key_code - 0x31) as i32)
                } else {
                    KeyAction::Consumed
                }
            }
            k if let Some(ch) = numpad_char(k) => {
                // 小键盘 direct 语义（follow_main 时键已在入口归一化为主键盘键，走上面的数字
                // 选词臂）：拼音缓冲是**编码**，数字不是合法拼音 → 顶屏「已转换前缀 + 当前高亮
                // 候选」再接着输出该字符并退出，已打的码不丢。
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    let code = state.temp_pinyin_buffer.clone();
                    // 命令候选：执行命令，不追加字符（与主路径 direct 一致）。
                    if let Some(act) = self
                        .overlay_commit_command(state, &cand, &code, |s, st| s.exit_temp_pinyin(st))
                    {
                        return act;
                    }
                    self.record_selection(&code, &cand.text, cand.source);
                    self.record_commit(
                        &cand.text,
                        code.len() as u32,
                        (idx - self.page_range(state).0) as i32,
                        wind_store::stats::CommitSource::TempPinyin,
                    );
                    state.committed_text.push_str(&cand.text);
                }
                let head = self.maybe_s2t(state, &state.committed_text.clone());
                let tail = if state.full_width {
                    to_full_width(&ch.to_string())
                } else {
                    ch.to_string()
                };
                self.record_commit(&tail, 0, -1, wind_store::stats::CommitSource::Punctuation);
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                Self::commit_action(format!("{}{}", head, tail), true)
            }
            keymap::VK_A..=keymap::VK_Z if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
                // 字母累积拼音
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                preedit_cursor::BufEdit::new(
                    &mut state.temp_pinyin_buffer,
                    &mut state.temp_pinyin_cursor,
                )
                .insert(ch);
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                let caret_pos = self.overlay_caret(state);
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos,
                }
            }
            _ => {
                // 二三候选键
                if data.modifiers & MOD_SHIFT == 0
                    && let Some(offset) = self.select_key_offset(data.key_code)
                {
                    let (start, end) = self.page_range(state);
                    let idx = start + offset;
                    if idx < end {
                        let cand = state.candidates[idx].clone();
                        return self.commit_temp_pinyin_selected(state, &cand, offset as i32);
                    }
                }
                // 其它键：有候选则上屏高亮候选（分段则保留剩余拼音）；否则退出清空。
                if !state.candidates.is_empty() {
                    let (start, _) = self.page_range(state);
                    let idx = (start + state.selected_index).min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand, (idx - start) as i32)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
        }
    }

    /// 退出临时英文模式并清空状态
    /// 临英缓冲的光标位插入（字母 / 数字 / 空格 / 符号五个入口共用）。
    fn temp_english_insert(state: &mut State, ch: char) {
        preedit_cursor::BufEdit::new(
            &mut state.temp_english_buffer,
            &mut state.temp_english_cursor,
        )
        .insert(ch);
    }

    pub(crate) fn exit_temp_english(&self, state: &mut State) {
        state.active = None;
        state.temp_english_buffer.clear();
        state.temp_english_cursor = 0;
        state.temp_english_prefix.clear();
        state.preedit.clear();
        state.candidates.clear();
    }

    /// 刷新临时英文候选：`原文 → 大小写变形 → 英文词库前缀匹配（保持词库原文）`。
    /// 需 `temp_english.show_candidates` 开启才产出变形与词库候选；词库为固定 id "english" 方案。
    ///
    /// 词库候选**不再按输入形态适配大小写**（旧 `adapt_en_case` 已删）——临英由 Shift+字母进入，
    /// 缓冲首字母恒大写，旧适配便把整列候选强制套成 `Hello`/`Help`，而词库 86% 的词本是小写。
    /// 大小写改由 [`en_case_variants`] 产出的显式变形候选承载，位置紧随原文（1-3 号键即可取到
    /// 三种形态），词库候选顺延其后。
    ///
    /// 去重按**精确文本**（旧实现按小写去重）：变形候选之间恰是同一小写形态的不同大小写，
    /// 小写去重会把它们全部抹掉。精确去重同时仍能挡住与原文/变形重复的词库项
    /// （如缓冲 `Hello` 时词库的 `hello` 被变形候选先占位挡下）。
    pub(crate) fn update_temp_english_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let buf = state.temp_english_buffer.clone();
        state.preedit = format!("{}{}", state.temp_english_prefix, buf);
        if buf.is_empty() {
            return;
        }
        // 首候选始终是用户所打原文（保证能上屏自己输入的内容）。
        let mut cands = vec![Candidate {
            text: buf.clone(),
            natural_order: 0,
            ..Default::default()
        }];
        let mut seen = std::collections::HashSet::new();
        seen.insert(buf.clone());
        let mut push = |text: String, cands: &mut Vec<Candidate>| {
            if !seen.insert(text.clone()) {
                return;
            }
            let order = cands.len() as i32;
            cands.push(Candidate {
                text,
                natural_order: order,
                ..Default::default()
            });
        };
        if let Some(schema) = self.overlay_engine_schema(state) {
            // 大小写变形（全小写 / 首字母大写 / 全大写，去掉与原文相同者）。
            // 可关：变形项每条都占一个候选位，每页 5 条时能吃掉一半。
            if self.rt().config.input.temp_english.case_variants {
                for v in en_case_variants(&buf) {
                    push(v, &mut cands);
                }
            }
            let result = self
                .engine_mgr
                .convert_with(&schema, &buf.to_lowercase(), 60);
            for c in result.candidates {
                push(c.text, &mut cands);
            }
        }
        // 统一展开汇聚点：临时英文词库候选内 `$` 特殊语法在此展开（见 finalize_candidates）。
        state.candidates = self.finalize_candidates(cands, &buf);
    }

    /// 临英选中候选（全局下标 `gi`）的命令前置守卫：`$CC` 命令候选 → 执行动作（退出临英后异步跑），
    /// 返回 `Some(action)`；非命令 → `None`，调用方按各自文本上屏语义继续。
    fn temp_english_try_command(&self, state: &mut State, gi: usize) -> Option<KeyAction> {
        let cand = state.candidates[gi].clone();
        let code = state.temp_english_buffer.clone();
        self.overlay_commit_command(state, &cand, &code, |s, st| s.exit_temp_english(st))
    }

    /// 临时英文模式按键处理（首版：缓冲累积 + 空格/回车/标点上屏，暂无词库候选）
    pub(crate) fn handle_temp_english_key(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> KeyAction {
        // 候选感知刷新后返回组合区动作。
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_temp_english_candidates(state);
            let d = state.preedit.clone();
            let caret_pos = this.overlay_caret(state);
            this.notify_ui_update(state);
            KeyAction::UpdateComposition { text: d, caret_pos }
        };
        // 上屏文本（可选全角）+ 退出。
        let commit_text = |this: &Self, state: &mut State, t: String| -> KeyAction {
            let text = if state.full_width {
                to_full_width(&t)
            } else {
                t
            };
            // 临时英文上屏（独占模式，无分段 committed）：来源临英，英文无编码故 code_len=0。
            this.record_commit(&text, 0, -1, wind_store::stats::CommitSource::TempEnglish);
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
        // 编码区光标移动（左右 / Home / End）；置于候选导航之后，导航键优先。
        if let Some(act) = self.overlay_cursor_key(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_temp_english(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK | keymap::VK_DELETE => {
                // 退格删光标前 / Delete 删光标后；缓冲被删空则退出（本就空缓冲时只有退格退出）。
                let backward = data.key_code == keymap::VK_BACK;
                let removed = {
                    let mut ed = preedit_cursor::BufEdit::new(
                        &mut state.temp_english_buffer,
                        &mut state.temp_english_cursor,
                    );
                    if backward {
                        ed.backspace()
                    } else {
                        ed.delete()
                    }
                };
                if state.temp_english_buffer.is_empty() && (removed || backward) {
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else if removed {
                    refresh(self, state)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_SPACE => {
                // space_as_input：空格作为输入字符入缓冲，仅回车上屏（对齐 Go）。
                // 上屏职责随之转给回车，且回车此时取**高亮候选**而非原文（见下方 VK_RETURN）。
                if self.rt().config.input.temp_english.space_as_input {
                    Self::temp_english_insert(state, ' ');
                    refresh(self, state)
                } else {
                    // 空格：上屏当前高亮候选（首候选=原始输入）；命令候选执行动作
                    let text = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        if let Some(act) = self.temp_english_try_command(state, idx) {
                            return act;
                        }
                        state.candidates[idx].text.clone()
                    } else {
                        state.temp_english_buffer.clone()
                    };
                    commit_text(self, state, text)
                }
            }
            keymap::VK_RETURN => {
                // clear 模式在临英**只管空缓冲**：临英缓冲装的是英文原文而非「编码」，
                // 且 `space_as_input` 开启后空格被占作输入字符、上屏职责整个压在回车上——
                // 若 clear 一并管辖非空缓冲，本模式将一个上屏通路都不剩（只余 Esc 放弃整段），
                // 打进去的内容永远出不来。故非空缓冲无条件走下方上屏路径，不读该配置。
                // 空缓冲本就没有内容可上屏，clear 语义照旧：不回显触发键字符。
                if self.enter_clears_composition() && state.temp_english_buffer.is_empty() {
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                // space_as_input：空格已被占作输入字符，回车接过「上屏高亮候选」的职责——
                // 否则该配置下一个选词键都不剩（allow_symbols 再开，数字键也让位于输入），
                // 候选窗形同虚设。未导航时高亮就在首候选（=用户原文），故对「回车上屏原文」
                // 的既有直觉向下兼容：只有主动导航过才会上屏别的候选。
                if self.rt().config.input.temp_english.space_as_input
                    && !state.candidates.is_empty()
                {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    if let Some(act) = self.temp_english_try_command(state, idx) {
                        return act;
                    }
                    let text = state.candidates[idx].text.clone();
                    return commit_text(self, state, text);
                }
                // 回车：上屏原始输入文本（不取候选）；缓冲空时上屏触发键字符（触发键透传）
                let text = if state.temp_english_buffer.is_empty() {
                    state.temp_english_prefix.clone()
                } else {
                    state.temp_english_buffer.clone()
                };
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
                Self::temp_english_insert(state, ch);
                refresh(self, state)
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // allow_symbols 开：数字是合法英文内容（hello2 / mp3 / x64），一律入缓冲——
                // 该开关的语义是「英文原文优先于选词」，此前它只接到下方标点臂，数字臂完全没读它，
                // 于是开了开关也仍被候选抢走；连带 `0` 走独立臂无条件入缓冲，成了「0 能打、1-9 不能」
                // 的不一致。此时选词改走：方向/翻页键导航高亮 + 空格上屏（回车仍上屏原文）。
                let digits_as_input = self.rt().config.input.temp_english.allow_symbols;
                // 数字：有词库候选（>1，即除原文外还有匹配）时按页选词；否则作输入（英文含数字 v2）
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if !digits_as_input && state.candidates.len() > 1 && gi < end {
                    if let Some(act) = self.temp_english_try_command(state, gi) {
                        return act;
                    }
                    let text = state.candidates[gi].text.clone();
                    commit_text(self, state, text)
                } else {
                    let ch = (b'0' + (data.key_code - 0x30) as u8) as char;
                    Self::temp_english_insert(state, ch);
                    refresh(self, state)
                }
            }
            0x30 if data.modifiers & MOD_SHIFT == 0 => {
                Self::temp_english_insert(state, '0');
                refresh(self, state)
            }
            k if let Some(ch) = numpad_char(k) => {
                // 小键盘 direct 语义（follow_main 时键已在入口归一化成主键盘键，不到达这里）：
                // 临英缓冲是**文本**不是编码，数字/运算符都是合法内容 → 直接入缓冲，
                // 「英文数字连输」得以在默认配置下可用。此前小键盘落到下方标点臂被
                // punct_char 判 None 后静默 Consumed，故临英下小键盘数字完全打不出。
                Self::temp_english_insert(state, ch);
                refresh(self, state)
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 二三候选键（默认 `;` `'`）→ 选候选。临英此前是**唯一**没接
                // `select_key_offset` 的模式处理器（主流程 / 临拼 / 特殊 / mix 都接了），
                // 于是次选键一路落到下方标点臂，被判成「上屏高亮候选 + 标点」——用户按 `;`
                // 想选第 2 候选，实得首候选被直接上屏并退出临英。
                // 与数字臂同构地受 allow_symbols 抑制：该开关的语义是符号/数字「入缓冲，
                // 而非上屏退出**或选词**」（见 config.toml 该项说明）。
                // 越界（页内候选不足）不在此处理，落下方标点臂保持既有语义。
                if !shift
                    && !self.rt().config.input.temp_english.allow_symbols
                    && let Some(offset) = self.select_key_offset(data.key_code)
                {
                    let (start, end) = self.page_range(state);
                    let gi = start + offset;
                    if gi < end {
                        if let Some(act) = self.temp_english_try_command(state, gi) {
                            return act;
                        }
                        let text = state.candidates[gi].text.clone();
                        return commit_text(self, state, text);
                    }
                }
                // 其它（标点等）：上屏当前高亮候选 + 转换后标点，退出
                if let Some(ch) = punct_char(data.key_code, shift) {
                    // allow_symbols：可见符号直接入缓冲累积（如 C++），不上屏退出（对齐 Go）。
                    if self.rt().config.input.temp_english.allow_symbols {
                        Self::temp_english_insert(state, ch);
                        return refresh(self, state);
                    }
                    let base = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        if let Some(act) = self.temp_english_try_command(state, idx) {
                            return act;
                        }
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
                    self.record_commit(&base, 0, -1, wind_store::stats::CommitSource::TempEnglish);
                    self.record_commit(&punct, 0, -1, wind_store::stats::CommitSource::Punctuation);
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
        // 命令候选顶屏 → 执行命令（与按空格一致），不进模式、不上屏 display 标签。
        if let Some(act) = self.top_commit_command_guard(state) {
            return act;
        }
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let (start, _) = self.page_range(state);
            let idx = (start + state.selected_index).min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
            let code = self.freq_code(&state.input_buffer, &state.candidates[idx]);
            self.record_selection(&code, &t, state.candidates[idx].source);
            // 进入临时拼音前顶屏高亮候选（来源候选；prefix 段已在选词时记过）。
            self.record_commit(
                &t,
                state.input_buffer.len() as u32,
                (idx - start) as i32,
                wind_store::stats::CommitSource::Candidate,
            );
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
        // key_code == 0 是直达热键哨兵：不写引导符（temp_pinyin_prefix_for 对未映射键会兜底
        // 反引号，故此处显式取空，对齐 enter_special_mode 的 key_code=0 语义）。
        state.temp_pinyin_prefix = if key_code == 0 {
            String::new()
        } else {
            Self::temp_pinyin_prefix_for(key_code).to_string()
        };
        self.update_temp_pinyin_candidates(state);
        self.notify_ui_update(state);
        let prefix = state.temp_pinyin_prefix.clone();
        match committed {
            Some(text) => self.commit_then_new_composition(text, prefix),
            None => KeyAction::UpdateComposition {
                text: prefix.clone(),
                caret_pos: prefix.chars().count() as u32,
            },
        }
    }
}
