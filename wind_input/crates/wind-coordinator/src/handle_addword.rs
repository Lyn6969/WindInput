//! 造词 / 加词：选中后自动造词、命令栏 dict.add 加用户词。
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, LEARN_ADD_WEIGHT, LEARN_WEIGHT_DELTA, State};
use tracing::{debug, warn};
use wind_bridge::handler::{KeyAction, KeyEventData};
use wind_ipc::protocol::MOD_CTRL;
use wind_keys::keymap;
use wind_ui::candidate_window::CandidateItem;
use wind_ui::manager::UiCommand;

/// 最小加词长度
const ADD_WORD_MIN_LEN: usize = 1;
/// 默认加词长度
const ADD_WORD_DEFAULT_LEN: usize = 2;
/// 最大加词长度（对齐 Go 默认上限；码表方案的 encoder 限制暂未接入）
const ADD_WORD_MAX_LEN: usize = 20;
/// 手动加词默认权重（略高于系统词库归一化中位 1000，对齐 Go addWordMaxWeight）
const ADD_WORD_WEIGHT: i32 = 1200;

impl Coordinator {
    /// 加词到用户层（code 为空时暂不支持自动推导编码）。
    pub(crate) fn cmd_dict_add(&self, text: &str, code: &str) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            anyhow::bail!("dict.add: 无 store");
        };
        if code.is_empty() {
            anyhow::bail!("dict.add: code 为空（Rust 端暂未支持自动推导编码）");
        }
        let schema = self.engine_mgr.active_schema_id();
        store.add_user_word(&schema, code, text, 100)?;
        Ok(())
    }

    /// 自动造词（L）：仅当用户**分步**组成（committed_segs ≥2 段、合并 ≥2 字）才学。
    /// 完整拼音码 = 各段码拼接；词 = 各段汉字拼接。写入临时层（需临时层，达阈值由 store 晋升路线处理）。
    pub(crate) fn learn_phrase_on_commit(&self, state: &State) {
        if state.committed_segs.len() < 2 {
            return;
        }
        // 自动造词闸门：拼音方案读 [pinyin.auto_learn]，码表/混输读有效 [codetable.auto_phrase]
        // （混输继承主码表行为）。开关关闭直接跳过；min_len 为造词最小字数（0 回退 2）。
        let (enabled, min_len) = if self.engine_mgr.is_pinyin() {
            let al = self.engine_mgr.auto_learn_settings();
            (al.enabled, al.min_word_length)
        } else {
            let ap = self.engine_mgr.codetable_settings().auto_phrase;
            (ap.enabled, ap.min_phrase_len)
        };
        if !enabled {
            return;
        }
        let code: String = state
            .committed_segs
            .iter()
            .map(|(c, _)| c.as_str())
            .collect();
        let text: String = state
            .committed_segs
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        let min_len = if min_len == 0 { 2 } else { min_len };
        if text.chars().count() < min_len || code.is_empty() {
            return;
        }
        let Some(store) = &self.store else { return };
        let schema = self.engine_mgr.active_schema_id();
        // add_weight/delta 取保守默认；晋升计数阈值由临时层累积达成（后续可接入 schema.learning 配置）。
        if let Err(e) =
            store.learn_temp_word(&schema, &code, &text, LEARN_ADD_WEIGHT, LEARN_WEIGHT_DELTA)
        {
            warn!("learn_temp_word failed: {}", e);
        } else {
            debug!("auto-learned phrase: {} -> {}", code, text);
        }
    }

    // ──────────────────────────────────────────────────────────────────────
    // 快捷加词模式（对齐 Go internal/coordinator/handle_addword.go）
    // 从最近上屏字符中选取末尾 N 字组词，自动计算编码后加入用户词库。
    // ──────────────────────────────────────────────────────────────────────

    /// 还原最近上屏字符池：`recent_commits` 最新在前，反转为时间序（旧→新）后展开为字符，
    /// 取末尾 `max_len` 个（最近输入的字符）。
    fn add_word_recent_chars(&self, max_len: usize) -> Vec<char> {
        let snap = self.recent_commits_snapshot(); // 最新在前
        let mut chars: Vec<char> = Vec::new();
        for s in snap.iter().rev() {
            chars.extend(s.chars());
        }
        let n = chars.len();
        if n > max_len {
            chars.split_off(n - max_len)
        } else {
            chars
        }
    }

    /// 取当前选取的词（字符池末尾 `add_word_len` 个字符）。
    fn add_word_current_word(&self, state: &State) -> String {
        let n = state.add_word_chars.len();
        let len = state.add_word_len.min(n);
        state.add_word_chars[n - len..].iter().collect()
    }

    /// 进入加词模式：取最近字符、默认词长 2、强制竖排、占位 composition。
    pub(crate) fn enter_add_word_mode(&self, state: &mut State) -> KeyAction {
        // 清理任何未上屏的输入/候选/独占模式残留
        self.reset_exclusive_modes(state);
        self.reset_pinyin_composition(state);
        self.notify_ui_hide();

        state.add_word_chars = self.add_word_recent_chars(ADD_WORD_MAX_LEN);
        state.add_word_active = true;

        // 强制竖排（对齐 Go），退出时恢复进入前布局。
        let cur = self
            .rt()
            .config
            .ui
            .candidate
            .layout
            .eq_ignore_ascii_case("vertical");
        state.add_word_saved_vertical = Some(cur);
        let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(true));

        if state.add_word_chars.len() < ADD_WORD_MIN_LEN {
            state.add_word_len = 0;
            state.add_word_code.clear();
        } else {
            state.add_word_len = ADD_WORD_DEFAULT_LEN.min(state.add_word_chars.len());
            self.update_add_word_code(state);
        }

        self.show_add_word_preview(state);

        // 占位 composition：激活 C++ 侧 composition，转发后续 ↑↓/Enter/Esc 给我们处理。
        KeyAction::UpdateComposition {
            text: " ".to_string(),
            caret_pos: 0,
        }
    }

    /// 退出加词模式：清状态、恢复布局、隐藏候选窗。
    pub(crate) fn exit_add_word_mode(&self, state: &mut State) {
        state.add_word_active = false;
        state.add_word_chars.clear();
        state.add_word_len = 0;
        state.add_word_code.clear();
        if let Some(prev) = state.add_word_saved_vertical.take() {
            let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(prev));
        }
        self.notify_ui_hide();
    }

    /// 调整加词长度（↑ +1 / ↓ -1），夹在 [1, min(字符数, 上限)]。
    pub(crate) fn adjust_add_word_length(&self, state: &mut State, delta: i32) -> KeyAction {
        if state.add_word_chars.len() < ADD_WORD_MIN_LEN {
            return KeyAction::Consumed;
        }
        let max_len = ADD_WORD_MAX_LEN.min(state.add_word_chars.len());
        let mut new_len = state.add_word_len as i32 + delta;
        new_len = new_len.clamp(ADD_WORD_MIN_LEN as i32, max_len as i32);
        let new_len = new_len as usize;
        if new_len != state.add_word_len {
            state.add_word_len = new_len;
            self.update_add_word_code(state);
            self.show_add_word_preview(state);
        }
        KeyAction::Consumed
    }

    /// 确认加词：写入用户词库（权重 1200）并广播 dict.changed；编码为空则中止。
    pub(crate) fn confirm_add_word(&self, state: &mut State) -> KeyAction {
        if state.add_word_len < ADD_WORD_MIN_LEN || state.add_word_chars.len() < ADD_WORD_MIN_LEN {
            self.exit_add_word_mode(state);
            return KeyAction::ClearComposition;
        }
        let word = self.add_word_current_word(state);
        let code = state.add_word_code.clone();
        if code.is_empty() {
            warn!("addword: 无法计算编码，放弃加词 word={}", word);
            self.exit_add_word_mode(state);
            return KeyAction::ClearComposition;
        }
        if let Some(store) = &self.store {
            let schema = self.engine_mgr.active_schema_id();
            match store.add_user_word(&schema, &code, &word, ADD_WORD_WEIGHT) {
                Ok(_) => {
                    // 注：dict.changed 广播在 RPC dispatch 层（EventSink），协调器不持有该 sink，
                    // 故此处不发事件——与现有 web_dict_add 一致；设置端用户词库视图重开时刷新。
                    debug!("addword: 已加词 {} -> {}", code, word);
                }
                Err(e) => warn!("addword: 写库失败 {}", e),
            }
        }
        self.exit_add_word_mode(state);
        KeyAction::ClearComposition
    }

    /// Ctrl+Enter：转到设置端加词编辑界面，预填当前 词/编码/方案。
    pub(crate) fn open_add_word_dialog(&self, state: &mut State) -> KeyAction {
        let (word, code) = if state.add_word_len >= ADD_WORD_MIN_LEN
            && state.add_word_chars.len() >= ADD_WORD_MIN_LEN
        {
            (
                self.add_word_current_word(state),
                state.add_word_code.clone(),
            )
        } else {
            (String::new(), String::new())
        };
        let schema = self.engine_mgr.active_schema_id();
        self.exit_add_word_mode(state);

        let mut page = String::from("add-word");
        if !word.is_empty() {
            page.push_str(" --text=");
            page.push_str(&word);
        }
        if !code.is_empty() {
            page.push_str(" --code=");
            page.push_str(&code);
        }
        if !schema.is_empty() {
            page.push_str(" --schema=");
            page.push_str(&schema);
        }
        self.open_settings(Some(&page));
        KeyAction::ClearComposition
    }

    /// 更新当前加词的编码（按方案：拼音生成 / 码表反查）。
    fn update_add_word_code(&self, state: &mut State) {
        if state.add_word_len < ADD_WORD_MIN_LEN || state.add_word_chars.len() < state.add_word_len
        {
            state.add_word_code.clear();
            return;
        }
        let word = self.add_word_current_word(state);
        state.add_word_code = self.calc_add_word_code(&word);
    }

    /// 为词计算编码（对齐设置端 `dict.encode` / web_dict_encode）：
    /// 拼音方案走引擎词级消歧，无果回退逐字反查表；码表方案走五笔词组取码（逐字反查组合，
    /// 支持词库中尚不存在的新词）。
    fn calc_add_word_code(&self, word: &str) -> String {
        let schema = self.engine_mgr.active_schema_id();
        let is_pinyin = self
            .engine_mgr
            .schema_engine_type(&schema)
            .map(|t| t == "pinyin")
            .unwrap_or(false);
        if is_pinyin {
            self.engine_mgr
                .generate_word_pinyin(&schema, word)
                .unwrap_or_else(|| self.reverse.gen_pinyin(word))
        } else {
            self.reverse.wubi_word_code(word)
        }
    }

    /// 显示加词预览候选窗（两行：标题行提示 + 词行编码；均为提示行，no_index 不渲染序号）。
    fn show_add_word_preview(&self, state: &State) {
        // 提示行构造：no_index=true 完全不显示序号（避免默认主题空圆圈）。
        let row = |text: String, comment: String| CandidateItem {
            text,
            code: String::new(),
            label: String::new(),
            tooltip: String::new(),
            comment,
            no_index: true,
        };
        let candidates = if state.add_word_chars.len() < ADD_WORD_MIN_LEN
            || state.add_word_len < ADD_WORD_MIN_LEN
        {
            vec![
                row("快捷加词".into(), "Esc关闭".into()),
                row("无最近输入".into(), "请先输入文字后再使用".into()),
            ]
        } else {
            let word = self.add_word_current_word(state);
            let code_comment = if state.add_word_code.is_empty() {
                "无法计算编码".to_string()
            } else {
                state.add_word_code.clone()
            };
            vec![
                row(
                    "快捷加词".into(),
                    "↑↓调整长度  Enter添加  Ctrl+Enter编辑  Esc取消".into(),
                ),
                row(word, code_comment),
            ]
        };
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit: String::new(),
            mode_label: String::new(),
            candidates,
            selected: usize::MAX, // 两行均为提示、非可选候选，不高亮任何行
            hover: -1,
            page: 1,
            total_pages: 1,
            caret_x: state.caret_x,
            caret_y: state.caret_y,
            caret_height: state.caret_height,
            caret_valid: true,
        });
    }

    /// 加词模式下的按键分派（对齐 Go handleAddWordKey）。
    pub(crate) fn handle_add_word_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let has_ctrl = data.modifiers & MOD_CTRL != 0;
        match data.key_code {
            keymap::VK_ESCAPE | keymap::VK_BACK => {
                self.exit_add_word_mode(state);
                KeyAction::ClearComposition
            }
            keymap::VK_UP => self.adjust_add_word_length(state, 1),
            keymap::VK_DOWN => self.adjust_add_word_length(state, -1),
            keymap::VK_RETURN if has_ctrl => self.open_add_word_dialog(state),
            keymap::VK_RETURN => self.confirm_add_word(state),
            // 加词模式下消费所有按键，避免误操作退出。
            _ => KeyAction::Consumed,
        }
    }
}

#[cfg(test)]
mod tests {
    //! 快捷加词状态机单元测试：无头 Coordinator + 临时 store，覆盖纯逻辑
    //! （字符还原/词长调整/确认写库）。编码计算依赖引擎，headless 下为空，
    //! 故写库测试手动注入 add_word_code。
    use crate::coordinator::Coordinator;
    use std::sync::Arc;
    use wind_config::Config;
    use wind_store::Store;

    fn coord(tag: &str) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!("wind_addword_{tag}.redb"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        Coordinator::new_headless_with_store(Config::default(), None, store)
    }

    /// 按时间序模拟上屏：最早先入，最新最后（push_front 保证最新在前，对齐运行时）。
    fn push_commits(c: &Coordinator, items: &[&str]) {
        let mut h = c.recent_commits.lock().unwrap();
        for it in items {
            h.push_front(it.to_string());
        }
    }

    #[test]
    fn recent_chars_order_and_truncate() {
        let c = coord("recent");
        push_commits(&c, &["你", "好", "世界"]);
        assert_eq!(
            c.add_word_recent_chars(20).iter().collect::<String>(),
            "你好世界"
        );
        assert_eq!(
            c.add_word_recent_chars(2).iter().collect::<String>(),
            "世界"
        );
    }

    #[test]
    fn enter_sets_default_len_and_word() {
        let c = coord("enter");
        push_commits(&c, &["你", "好"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert!(st.add_word_active);
        assert_eq!(st.add_word_len, 2);
        assert_eq!(c.add_word_current_word(&st), "你好");
    }

    #[test]
    fn enter_single_char_caps_len_to_one() {
        let c = coord("single");
        push_commits(&c, &["好"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert_eq!(st.add_word_len, 1);
        assert_eq!(c.add_word_current_word(&st), "好");
    }

    #[test]
    fn enter_no_history_zero_len() {
        let c = coord("empty");
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert!(st.add_word_active);
        assert_eq!(st.add_word_len, 0);
        assert!(st.add_word_code.is_empty());
    }

    #[test]
    fn adjust_length_clamps() {
        let c = coord("adjust");
        push_commits(&c, &["一", "二", "三"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert_eq!(st.add_word_len, 2);
        c.adjust_add_word_length(&mut st, 1);
        assert_eq!(st.add_word_len, 3);
        c.adjust_add_word_length(&mut st, 1); // 上限 = 字符数 3
        assert_eq!(st.add_word_len, 3);
        assert_eq!(c.add_word_current_word(&st), "一二三");
        c.adjust_add_word_length(&mut st, -5); // 下限 1
        assert_eq!(st.add_word_len, 1);
        assert_eq!(c.add_word_current_word(&st), "三");
    }

    #[test]
    fn confirm_empty_code_aborts_without_write() {
        let c = coord("abort");
        push_commits(&c, &["你", "好"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert!(st.add_word_code.is_empty(), "headless 无引擎，编码应为空");
        c.confirm_add_word(&mut st);
        assert!(!st.add_word_active, "确认后应退出加词模式");
        drop(st);
        let schema = c.engine_mgr.active_schema_id();
        let store = c.store.as_ref().unwrap();
        // 编码为空时不应写任何用户词；遍历常见空码均无记录。
        assert!(store.get_user_words(&schema, "").unwrap().is_empty());
    }

    #[test]
    fn confirm_with_code_writes_user_word() {
        let c = coord("write");
        push_commits(&c, &["你", "好"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        st.add_word_code = "nihao".to_string(); // headless 无引擎，手动注入编码
        c.confirm_add_word(&mut st);
        assert!(!st.add_word_active);
        drop(st);
        let schema = c.engine_mgr.active_schema_id();
        let store = c.store.as_ref().unwrap();
        let recs = store.get_user_words(&schema, "nihao").unwrap();
        assert_eq!(recs.len(), 1, "应写入 1 条用户词");
        assert_eq!(recs[0].text, "你好");
        assert_eq!(recs[0].weight, 1200);
    }

    #[test]
    fn exit_resets_state() {
        let c = coord("exit");
        push_commits(&c, &["你", "好"]);
        let mut st = c.state.lock().unwrap();
        c.enter_add_word_mode(&mut st);
        assert!(st.add_word_active);
        c.exit_add_word_mode(&mut st);
        assert!(!st.add_word_active);
        assert!(st.add_word_chars.is_empty());
        assert_eq!(st.add_word_len, 0);
        assert!(st.add_word_code.is_empty());
    }
}
