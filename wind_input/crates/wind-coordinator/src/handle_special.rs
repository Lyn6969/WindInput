//! 特殊方案输入模式
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, State, numpad_char, punct_char};
use crate::pipeline::ModeKind;
use crate::preedit_cursor;
use tracing::debug;
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_candidate::Candidate;
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

impl Coordinator {
    /// 引导键名 → VK（特殊模式触发；统一映射 + 额外支持单字母 a-z 引导键，见 `keymap`）。
    pub(crate) fn special_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk_with_letters(key)
    }

    /// 找出 key_code 匹配的特殊模式下标（按配置顺序先到先得；最多 256 个）。
    pub(crate) fn match_special_trigger(&self, key_code: u32) -> Option<u8> {
        for (i, m) in self.rt().config.schema.special_modes.iter().enumerate() {
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

    /// 按 id 在 `schema.special_modes` 配置序中定位下标（与 `match_special_trigger` 的 u8 下标语义
    /// 一致，最多 256 项）。供直达热键 `enter_special:<id>` 分发定位；未找到返回 None。
    pub(crate) fn special_mode_idx(&self, id: &str) -> Option<u8> {
        self.rt()
            .config
            .schema
            .special_modes
            .iter()
            .take(u8::MAX as usize + 1)
            .position(|m| m.effective_id() == id)
            .map(|i| i as u8)
    }

    /// 顶屏当前普通输入的半成品（复用 `take_committed` + 高亮候选）并进入特殊模式，
    /// 供直达热键与「缓冲非空/有候选时按引导键」两处共用（对齐 mix/临拼的 commit_and_enter）。
    /// key_code=0 是热键哨兵：`vk_to_prefix_char(0)` 返回 None → `special_prefix` 为空，满足
    /// 「热键进入不写引导符」；引导键进入传真实 VK，组合区写引导符（与空缓冲进入一致）。
    /// 方案须可加载（调用方 `ensure_schema` 保证）。
    pub(crate) fn commit_and_enter_special_mode(
        &self,
        state: &mut State,
        idx: u8,
        key_code: u32,
    ) -> KeyAction {
        // 命令候选顶屏 → 执行命令（与按空格一致），不进模式。
        if let Some(act) = self.top_commit_command_guard(state) {
            return act;
        }
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let i = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let t = state.candidates[i].text.clone();
            // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
            let freq_code = self.freq_code(&state.input_buffer, &state.candidates[i]);
            self.record_selection(&freq_code, &t, state.candidates[i].source);
            Some(format!("{prefix}{t}"))
        } else if !prefix.is_empty() {
            Some(prefix)
        } else {
            None
        };
        // enter_special_mode 内部清空 input_buffer/candidates、建组合区（key_code=0 → 前缀空）、刷 UI。
        let enter = self.enter_special_mode(state, idx, key_code);
        match committed {
            Some(text) => {
                let new_comp = match &enter {
                    KeyAction::UpdateComposition { text, .. } => text.clone(),
                    _ => state.preedit.clone(),
                };
                self.commit_then_new_composition(text, new_comp)
            }
            None => enter,
        }
    }

    /// 当前特殊模式是否开启「进入即展示候选」（`show_all_on_enter`；按 special_id 定位配置）。
    fn special_mode_show_all(&self, state: &State) -> bool {
        self.rt()
            .config
            .schema
            .special_modes
            .get(state.special_id as usize)
            .map(|m| m.show_all_on_enter)
            .unwrap_or(false)
    }

    /// 特殊模式引用的方案 id（features.special_modes[idx].schema）。
    pub(crate) fn special_schema(&self, idx: u8) -> Option<String> {
        self.rt()
            .config
            .schema
            .special_modes
            .get(idx as usize)
            .map(|m| m.schema.clone())
            .filter(|s| !s.is_empty())
    }

    /// 进入特殊模式（其方案须可加载，由激活点 ensure_schema 保证）。清空普通输入，初始化空编码缓冲。
    pub(crate) fn enter_special_mode(
        &self,
        state: &mut State,
        idx: u8,
        key_code: u32,
    ) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Special(idx));
        state.special_id = idx;
        state.special_buffer.clear();
        state.special_cursor = 0;
        // 显示态前缀（进入键符号，如 "\"）：只显示不消费。
        state.special_prefix = keymap::vk_to_prefix_char(key_code)
            .map(|c| c.to_string())
            .unwrap_or_default();
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
        state.special_cursor = 0;
        state.special_prefix.clear();
        state.candidates.clear();
        state.preedit.clear();
    }

    /// 按当前编码缓冲刷新特殊模式候选（经其引用方案的引擎查询，复用方案 CodeTableSpec 全码策略）。
    /// 返回 Some(候选) 表示该方案的全码策略请求自动上屏该候选（`$CC` 命令候选由调用方
    /// 走命令执行路径，普通候选上屏其文本）。
    pub(crate) fn update_special_candidates(&self, state: &mut State) -> Option<Candidate> {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        // 组合区 = 显示态前缀 + 编码缓冲（前缀只显示不参与查询）。
        state.preedit = format!("{}{}", state.special_prefix, state.special_buffer);
        if state.special_buffer.is_empty() {
            // 进入即展示：该模式开启 show_all_on_enter 时，空码枚举方案码表首页候选（按 weight
            // 降序）供浏览，UI 按 per_page 分页；经 finalize 展开词条内特殊语法（浏览态无输入
            // 上下文，input 传空）。未开则维持空白（原行为，敲码才出候选）。
            if self.special_mode_show_all(state)
                && let Some(schema) = self.overlay_engine_schema(state)
            {
                let raw = self.engine_mgr.enumerate_with(&schema, 100);
                state.candidates = self.finalize_candidates(raw, "");
            }
            return None;
        }
        let schema = self.overlay_engine_schema(state)?;
        let result = self
            .engine_mgr
            .convert_with(&schema, &state.special_buffer, 100);
        // 统一展开汇聚点：快符表内 `$AA/$SS/$CC` 等特殊语法在此炸开/标命令（见 finalize_candidates）。
        state.candidates = self.finalize_candidates(result.candidates, &state.special_buffer);
        // 空码补全对齐主码表方案（`single_code_input` + `single_code_complete`）：精确匹配模式下
        // 当前编码无精确候选、但更长前缀有候选时，引擎「备货不 push」把首个更长编码候选放进
        // `completion_hint`（见 codetable/engine.rs），交由掌握最终列表的调用方判空后取一条。
        // 特殊模式此前只消费 `result.candidates`、丢弃了这条旁路 → 屏幕全空；此处补上收口，
        // 与主路径 `update_candidates` 一致（见 handle_candidate.rs 的补全收口）。引擎已在
        // `show_code_hint` 循环里给它标好「剩余编码」注释，直接采纳即可。
        if state.candidates.is_empty() {
            state.candidates.extend(result.completion_hint);
        }
        // 词频重排与候选调整：归属**特殊方案自身**，与写端 `record_selection_in` 同一个 id。
        // 取自同一处（`effective_data_schema`）是硬要求——读写分别取自不同的地方，会得到
        // 「写进 qsym、读的是 wubi86」：记账看着成功，候选顺序永远不动。
        //
        // 放在补全收口**之后**：补全出来的候选也该参与重排（与主路径
        // `update_candidates` 的次序一致）。
        let owner = self.effective_data_schema(state);
        self.apply_freq_rerank_in(
            owner.as_deref(),
            &mut state.candidates,
            &state.special_buffer,
        );
        self.apply_shadow_in(
            owner.as_deref(),
            &mut state.candidates,
            &state.special_buffer,
        );
        // 自动上屏由方案码表引擎的 should_auto_commit 决定（prefix_free≈全码唯一、fixed_length 等
        // 映射到该方案的 [engine.codetable] 配置）；复核上屏目标仍在候选中。`$CC` 命令词条经
        // finalize_candidates 展开后 text 已改写为 display 标签，而引擎意向 commit_text 是原始
        // `$CC` 源 → 按 phrase_template 补匹配，返回命中候选整条供调用方按命令/文本分流。
        if result.should_commit && !result.commit_text.is_empty() {
            let t = &result.commit_text;
            return state
                .candidates
                .iter()
                .find(|c| &c.text == t || (c.is_command && &c.phrase_template == t))
                .cloned();
        }
        None
    }

    /// 特殊模式选中某候选（全局下标 `gi`）：`$AA`/`$SS` 组折叠候选 → 补全编码到完整码重查展开（二级选择）；
    /// `$CC` 命令候选 → 执行动作（退出后异步跑，触发键码不上屏）；否则文本上屏。
    /// 统一空格 / 数字键 / 二三候选键的选中入口，保证组/命令候选选中行为一致。
    pub(crate) fn commit_special_candidate(&self, state: &mut State, gi: usize) -> KeyAction {
        let cand = state.candidates[gi].clone();
        // $AA/$SS 组折叠候选：补全编码到完整码并重查展开（不上屏组名）。
        if cand.is_group {
            state.special_buffer = cand.group_code.clone();
            state.special_cursor = state.special_buffer.len(); // 补全到完整码：光标落末尾
            self.update_special_candidates(state);
            let display = state.preedit.clone();
            let caret_pos = self.overlay_caret(state);
            self.notify_ui_update(state);
            return KeyAction::UpdateComposition {
                text: display,
                caret_pos,
            };
        }
        let code = state.special_buffer.clone();
        if let Some(act) =
            self.overlay_commit_command(state, &cand, &code, |s, st| s.exit_special_mode(st))
        {
            return act;
        }
        // 词频记账**归属特殊方案自身**（与主方案同层级，只是用特殊按键进入）。
        // 记账码用输入码：特殊方案是码表语义，`a`/`ab`/`abc` 是三个独立码位，
        // 与 `freq_code` 对 CodeTable 来源的口径一致。
        //
        // 此前这里只有 record_commit（统计），完全不记词频——特殊模式的候选顺序
        // 因此永远是词库原序，用户选过多少次都不会往前走。
        self.record_selection_in(
            self.effective_data_schema(state).as_deref(),
            &code,
            &cand.text,
            cand.source,
        );
        self.record_commit(
            &cand.text,
            state.special_buffer.len() as u32,
            -1,
            wind_store::stats::CommitSource::SpecialMode,
        );
        self.exit_special_mode(state);
        self.notify_ui_hide();
        Self::commit_action(cand.text, true)
    }

    /// 特殊模式按键处理：编码累积 + 候选选择 + 三档自动上屏；空格选高亮、回车上屏编码原文。
    pub(crate) fn handle_special_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        // Ctrl/Alt 组合守卫（见 `overlay_ctrl_alt_guard`）：必须最先，否则组合键会落到
        // 下方各臂被当成编码输入。
        if let Some(act) =
            self.overlay_ctrl_alt_guard(state, data, !state.special_buffer.is_empty(), |s, st| {
                s.exit_special_mode(st)
            })
        {
            return act;
        }
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        // 编码区光标移动（左右 / Home / End）；置于候选导航之后，导航键优先。
        if let Some(act) = self.overlay_cursor_key(state, data) {
            return act;
        }
        // 进入键二次按下（缓冲空）：按中英标点配置上屏该符号并退出。
        // 顺带武装智能符号：时限内再按同键即换英文形，否则这个键被模式占着、英文形没有通路。
        // press2 的拦截在 try_activate_mode 开头，早于模式激活链。
        if state.special_buffer.is_empty()
            && self.match_special_trigger(data.key_code) == Some(state.special_id)
            && let Some(ch) = punct_char(data.key_code, data.modifiers & MOD_SHIFT != 0)
        {
            let out = self.convert_punct_char(state, ch);
            self.arm_smart_symbol_after_commit(state, ch, &out);
            self.record_commit(&out, 0, -1, wind_store::stats::CommitSource::Punctuation);
            self.exit_special_mode(state);
            self.notify_ui_hide();
            return Self::commit_action(out, true);
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：放弃退出
                self.exit_special_mode(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK | keymap::VK_DELETE => {
                // 退格删光标前 / Delete 删光标后；缓冲被删空则退出（本就空缓冲时只有退格退出，
                // 保持原语义）。删除时不触发自动上屏。
                let backward = data.key_code == keymap::VK_BACK;
                let removed = {
                    let mut ed = preedit_cursor::BufEdit::new(
                        &mut state.special_buffer,
                        &mut state.special_cursor,
                    );
                    if backward {
                        ed.backspace()
                    } else {
                        ed.delete()
                    }
                };
                if state.special_buffer.is_empty() && (removed || backward) {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else if removed {
                    self.update_special_candidates(state);
                    let display = state.preedit.clone();
                    let caret_pos = self.overlay_caret(state);
                    self.notify_ui_update(state);
                    KeyAction::UpdateComposition {
                        text: display,
                        caret_pos,
                    }
                } else {
                    // 退格时光标已在最左 / Delete 时已在末尾：吃掉不透传。
                    KeyAction::Consumed
                }
            }
            keymap::VK_SPACE => {
                // 空格：有候选选高亮上屏（命令候选执行动作）；无候选退出
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    self.commit_special_candidate(state, idx)
                } else {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // clear 模式：整段放弃，不上屏任何内容。须先于下方各分支——此前该判断只写在
                // 「空缓冲」分支内，导致「打了码再回车」仍走非空缓冲路径无条件上屏编码原文，
                // 配置形同虚设（与主输入路径行为不一致）。
                if self.enter_clears_composition() {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                // 空缓冲（只按了模式键、还没敲编码）：commit 模式上屏模式键符号本身
                // （原样不转换，补输被占用的符号，如 \）。
                if state.special_buffer.is_empty() {
                    if !state.special_prefix.is_empty() {
                        let sym = state.special_prefix.clone();
                        self.record_commit(
                            &sym,
                            0,
                            -1,
                            wind_store::stats::CommitSource::Punctuation,
                        );
                        self.exit_special_mode(state);
                        self.notify_ui_hide();
                        return Self::commit_action(sym, true);
                    }
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                // 非空缓冲：上屏编码原文（原行为不变）
                let text = state.special_buffer.clone();
                self.record_commit(
                    &text,
                    text.len() as u32,
                    -1,
                    wind_store::stats::CommitSource::SpecialMode,
                );
                self.exit_special_mode(state);
                self.notify_ui_hide();
                Self::commit_action(text, true)
            }
            keymap::VK_1..=keymap::VK_9 => {
                // 数字 1-9 选当前页候选（命令候选执行动作）
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if gi < end {
                    self.commit_special_candidate(state, gi)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_A..=keymap::VK_Z => {
                // 字母：小写归一，在光标处插入
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                preedit_cursor::BufEdit::new(&mut state.special_buffer, &mut state.special_cursor)
                    .insert(ch);
                if let Some(cand) = self.update_special_candidates(state) {
                    // $CC 命令候选自动命中：与手动选中同路（退出模式 + 异步执行动作）。
                    let code = state.special_buffer.clone();
                    if let Some(act) = self.overlay_commit_command(state, &cand, &code, |s, st| {
                        s.exit_special_mode(st)
                    }) {
                        return act;
                    }
                    self.record_commit(
                        &cand.text,
                        state.special_buffer.len() as u32,
                        -1,
                        wind_store::stats::CommitSource::SpecialMode,
                    );
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    return Self::commit_action(cand.text, true);
                }
                let display = state.preedit.clone();
                let caret_pos = self.overlay_caret(state);
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos,
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 二三候选键 → 选候选（命令候选执行动作）
                if !shift && let Some(offset) = self.select_key_offset(data.key_code) {
                    let (start, end) = self.page_range(state);
                    let gi = start + offset;
                    if gi < end {
                        return self.commit_special_candidate(state, gi);
                    }
                }
                // 其它可打印标点：顶屏当前高亮候选 + 转换后标点，退出。
                // 小键盘键（direct 语义）回退 numpad_char 复用此路：特殊模式缓冲是编码，
                // 数字非法 → 顶屏候选再输出该字符，与主路径 direct 同构。follow_main 时键已在
                // 入口归一化为主键盘键，走上面的数字选词臂。
                if let Some(ch) =
                    punct_char(data.key_code, shift).or_else(|| numpad_char(data.key_code))
                {
                    let hi = if state.candidates.is_empty() {
                        None
                    } else {
                        Some(
                            self.highlighted_global_index(state)
                                .min(state.candidates.len() - 1),
                        )
                    };
                    // 高亮候选为组/命令：走统一选中（组→展开重查，命令→执行动作），
                    // 触发标点不单独上屏（语义同 top_commit_command_guard）。
                    if let Some(idx) = hi
                        && (state.candidates[idx].is_group || state.candidates[idx].is_command)
                    {
                        return self.commit_special_candidate(state, idx);
                    }
                    let committed = hi
                        .map(|idx| state.candidates[idx].text.clone())
                        .unwrap_or_default();
                    let punct = self.convert_punct_char(state, ch);
                    self.record_commit(
                        &committed,
                        state.special_buffer.len() as u32,
                        -1,
                        wind_store::stats::CommitSource::SpecialMode,
                    );
                    self.record_commit(&punct, 0, -1, wind_store::stats::CommitSource::Punctuation);
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

#[cfg(test)]
mod tests {
    //! 直达热键进入特殊模式/临拼的单元测试（无头 Coordinator + 临时 store）。
    //! headless 下无引擎，故只覆盖不依赖引擎查询的行为：id→idx 定位、空前缀进入、半成品上屏。
    use super::*;
    use crate::coordinator::Coordinator;
    use std::sync::Arc;
    use wind_candidate::Candidate;
    use wind_config::Config;
    use wind_config::config::SpecialModeConfig;
    use wind_store::Store;

    fn coord_with(tag: &str, cfg: Config) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!("wind_special_hk_{tag}.redb"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        Coordinator::new_headless_with_store(cfg, None, store)
    }

    fn cfg_with_modes(ids: &[&str]) -> Config {
        let mut cfg = Config::default();
        cfg.schema.special_modes = ids
            .iter()
            .map(|id| SpecialModeConfig {
                id: (*id).to_string(),
                ..Default::default()
            })
            .collect();
        cfg
    }

    #[test]
    fn special_mode_idx_locates_by_config_order() {
        let c = coord_with("idx", cfg_with_modes(&["rare", "sym", "cjk"]));
        assert_eq!(c.special_mode_idx("rare"), Some(0));
        assert_eq!(c.special_mode_idx("sym"), Some(1));
        assert_eq!(c.special_mode_idx("cjk"), Some(2));
        // 未知 id → None（分发点据此安全吞键，不 panic）
        assert_eq!(c.special_mode_idx("nope"), None);
    }

    #[test]
    fn commit_and_enter_special_writes_no_guide_prefix() {
        let c = coord_with("enter_empty", cfg_with_modes(&["rare"]));
        let mut st = c.state.lock().unwrap();
        st.chinese_mode = true;
        // 空缓冲进入：无半成品可上屏 → 返回 UpdateComposition，组合区无引导符。
        let act = c.commit_and_enter_special_mode(&mut st, 0, 0);
        assert_eq!(st.active, Some(ModeKind::Special(0)));
        assert!(
            st.special_prefix.is_empty(),
            "热键进入不应写引导符（special_prefix 应空）"
        );
        assert!(matches!(act, KeyAction::UpdateComposition { .. }));
    }

    #[test]
    fn commit_and_enter_special_commits_pending_candidate() {
        let c = coord_with("enter_commit", cfg_with_modes(&["rare"]));
        let mut st = c.state.lock().unwrap();
        st.chinese_mode = true;
        // 模拟普通输入半成品：编码 + 高亮候选。
        st.input_buffer = "aa".to_string();
        st.candidates = vec![Candidate {
            text: "工".to_string(),
            ..Default::default()
        }];
        st.selected_index = 0;
        st.current_page = 0;
        let act = c.commit_and_enter_special_mode(&mut st, 0, 0);
        // 进入前的高亮候选应作为 InsertText 上屏，随后进入目标模式、组合区无引导符。
        match act {
            KeyAction::InsertText { text, .. } => assert_eq!(text, "工"),
            other => panic!("应上屏半成品并进入模式，实际 {other:?}"),
        }
        assert_eq!(st.active, Some(ModeKind::Special(0)));
        assert!(st.special_prefix.is_empty());
        assert!(st.candidates.is_empty());
    }

    #[test]
    fn commit_and_enter_temp_pinyin_zero_keycode_has_no_prefix() {
        let c = coord_with("temp_zero", Config::default());
        let mut st = c.state.lock().unwrap();
        st.chinese_mode = true;
        // key_code=0 哨兵：进入临拼但组合区无引导符（对齐特殊模式）。
        let _ = c.commit_and_enter_temp_pinyin(&mut st, 0, "pinyin".to_string());
        assert_eq!(st.active, Some(ModeKind::TempPinyin));
        assert!(
            st.temp_pinyin_prefix.is_empty(),
            "直达热键（key_code=0）进入临拼不应写引导符"
        );
    }

    /// 「顶屏 + 进模式」收尾按 top_commit_mode 分流（与顶码上屏统一）：
    /// direct_commit（默认）+ 引导符新组合 → 真提交 + 延迟组合；新组合为空 → 直接真提交；
    /// pre_confirm → InsertText 聚合。
    #[test]
    fn commit_then_new_composition_follows_top_commit_mode() {
        let c = coord_with("ctnc_direct", Config::default());
        match c.commit_then_new_composition("可能".to_string(), "`".to_string()) {
            KeyAction::CommitThenDeferComposition {
                commit_text,
                deferred_composition,
                ..
            } => {
                assert_eq!(commit_text, "可能");
                assert_eq!(deferred_composition, "`");
            }
            other => panic!("direct_commit 有新组合应走真提交+延迟组合，实际 {other:?}"),
        }
        match c.commit_then_new_composition("可能".to_string(), String::new()) {
            KeyAction::InsertText {
                text,
                new_composition,
                has_new_composition,
                ..
            } => {
                assert_eq!(text, "可能");
                assert!(new_composition.is_none() && !has_new_composition);
            }
            other => panic!("新组合为空应直接真提交，实际 {other:?}"),
        }

        let mut cfg = Config::default();
        cfg.input.top_commit_mode = wind_config::TopCommitMode::PreConfirm;
        let c = coord_with("ctnc_pre", cfg);
        match c.commit_then_new_composition("可能".to_string(), "`".to_string()) {
            KeyAction::InsertText {
                text,
                new_composition,
                has_new_composition,
                ..
            } => {
                assert_eq!(text, "可能");
                assert_eq!(new_composition.as_deref(), Some("`"));
                assert!(has_new_composition);
            }
            other => panic!("pre_confirm 应走 InsertText 聚合，实际 {other:?}"),
        }
    }
}
