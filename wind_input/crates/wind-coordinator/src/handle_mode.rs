//! 模式 / 方案 / 主题切换
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 简繁、方案切换、主题切换、mix 融合模式、引擎方案叠加。

use crate::coordinator::{Coordinator, State};
use crate::pipeline::ModeKind;
use tracing::{debug, info, warn};
use wind_bridge::handler::KeyAction;
use wind_config::Config;
use wind_ui::manager::UiCommand;
use wind_ui::toast::{ToastKind, ToastPosition};

use crate::coordinator::{printable_char, punct_char};
use wind_bridge::handler::KeyEventData;
use wind_candidate::Candidate;
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

impl Coordinator {
    /// 设置简繁开关（测试/诊断用）。返回是否生效（数据缺失则 false）。
    pub fn debug_set_s2t(&self, on: bool) -> bool {
        if self.s2t.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return false;
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .s2t_enabled = on;
        true
    }

    /// 当前 overlay 模式背后的方案 id —— "模式即方案" 的单一映射（M4）。
    /// 引擎驱动型模式（临拼/特殊/临英）返回 Some(scheme)；无词典模式（快捷/URL）返回 None。
    /// overlay 候选查询统一经此取方案再走 `convert_with`；M5 临时 mix 复用此映射枚举成员方案。
    ///
    /// 说明：激活「触发条件」因各模式高度异构（Shift+字母 / 无修饰触发键 / schema 查找 /
    /// 缓冲扩展夺取）保持 S4d `try_activate_mode` 的显式优先级链，不强塞统一表（避免死抽象）。
    pub(crate) fn overlay_engine_schema(&self, state: &State) -> Option<String> {
        match state.active {
            Some(ModeKind::TempPinyin) => {
                (!state.temp_pinyin_schema.is_empty()).then(|| state.temp_pinyin_schema.clone())
            }
            Some(ModeKind::Special(idx)) => self.special_schema(idx),
            Some(ModeKind::TempEnglish) => self
                .rt()
                .config
                .input
                .temp_english
                .show_candidates
                .then(|| "english".to_string()),
            _ => None,
        }
    }

    /// mix 模式可加载的成员方案列表（过滤空/不可加载）。
    /// mix 可用的真实方案成员（过滤空 / 不可加载 / 内置 quick_input）。
    pub(crate) fn mix_members(&self, idx: u8) -> Vec<String> {
        self.rt()
            .config
            .schema
            .mix_modes
            .get(idx as usize)
            .map(|m| {
                m.members
                    .iter()
                    .filter(|s| {
                        !s.is_empty() && *s != "quick_input" && self.engine_mgr.ensure_schema(s)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// mix 是否含内置类方案 quick_input（日期/计算）成员——启用「首字符数字/字母决定选词逻辑」。
    pub(crate) fn mix_has_quick_input(&self, idx: u8) -> bool {
        self.rt()
            .config
            .schema
            .mix_modes
            .get(idx as usize)
            .map(|m| m.members.iter().any(|s| s == "quick_input"))
            .unwrap_or(false)
    }

    /// 进入 mix 模式（至少一个成员方案可加载，由激活点保证）。
    pub(crate) fn enter_mix_mode(&self, state: &mut State, idx: u8, key_code: u32) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Mix(idx));
        state.mix_id = idx;
        state.mix_buffer.clear();
        state.mix_numeric = false; // 由首字符（数字/字母）决定
        // 显示态前缀（进入键符号，如 ";"）：只显示不消费，让用户看到按下的键。
        state.mix_prefix = keymap::vk_to_prefix_char(key_code)
            .map(|c| c.to_string())
            .unwrap_or_default();
        self.update_mix_candidates(state);
        // 快捷输入「强制竖排」：含 quick_input 成员的 mix（如默认「快捷」融合，; 触发），
        // 进入时切竖排候选并记住原布局，退出恢复（与独立快捷输入模式一致）。
        if self.mix_has_quick_input(idx) && self.rt().config.schema.quick_input.force_vertical {
            let cur = self
                .rt()
                .config
                .ui
                .candidate
                .layout
                .eq_ignore_ascii_case("vertical");
            state.quick_saved_vertical = Some(cur);
            let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(true));
        }
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered mix mode idx={}", idx);
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 顶屏当前高亮候选（若有）并进入 mix 融合模式。
    /// 用于缓冲非空 / 有候选时按下融合触发键（如 `;`）——对齐 `commit_and_enter_temp_pinyin`：
    /// 先把已转换前缀 + 高亮候选上屏，再进融合模式。
    /// （空缓冲 + 无候选的进入由 handle_lifecycle 的 `enter_mix_mode` 直接处理。）
    pub(crate) fn commit_and_enter_mix_mode(
        &self,
        state: &mut State,
        idx: u8,
        key_code: u32,
    ) -> KeyAction {
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let i = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let t = state.candidates[i].text.clone();
            self.record_selection(&state.input_buffer, &t);
            Some(format!("{prefix}{t}"))
        } else if !prefix.is_empty() {
            Some(prefix)
        } else {
            None
        };
        // enter_mix_mode 内部清空 input_buffer/candidates、建组合区前缀、刷 UI 并返回 UpdateComposition。
        let enter = self.enter_mix_mode(state, idx, key_code);
        match committed {
            Some(text) => {
                let new_comp = match &enter {
                    KeyAction::UpdateComposition { text, .. } => text.clone(),
                    _ => state.preedit.clone(),
                };
                KeyAction::InsertText {
                    text,
                    new_composition: Some(new_comp),
                    mode_changed: false,
                    chinese_mode: true,
                    has_new_composition: true,
                }
            }
            None => enter,
        }
    }

    /// 退出 mix 模式并清空相关状态（含逐步转换的已转换前缀）。
    pub(crate) fn exit_mix_mode(&self, state: &mut State) {
        state.active = None;
        state.mix_buffer.clear();
        state.mix_prefix.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
        // 强制竖排退出：恢复进入前布局。
        if let Some(prev) = state.quick_saved_vertical.take() {
            let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(prev));
        }
    }

    /// 若开启简繁转换，把简体文本转为繁体（数据缺失则原样返回）。
    pub(crate) fn maybe_s2t(&self, state: &State, text: &str) -> String {
        if state.s2t_enabled
            && let Some(conv) = self.s2t.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            return conv.convert(text);
        }
        text.to_string()
    }

    /// 切换输入方案并持久化 `schema.active` 到用户层配置（重启后保留）。
    pub(crate) fn cmd_set_schema(&self, id: &str) {
        self.switch_schema(id);
        if let Err(e) = Config::set_user_string(&["schema", "active"], id) {
            warn!("ime.schema: 持久化 schema.active 失败: {}", e);
        }
    }

    /// 循环切换主题并持久化；dir="prev" 向前，其余向后。返回新主题显示名。
    pub(crate) fn cmd_theme_cycle(&self, dir: &str) -> String {
        let list = self.list_themes(); // Vec<(id, name)>
        if list.is_empty() {
            return String::new();
        }
        let cur = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let pos = list.iter().position(|(id, _)| *id == cur).unwrap_or(0);
        let n = list.len();
        let next = if dir == "prev" {
            (pos + n - 1) % n
        } else {
            (pos + 1) % n
        };
        self.select_theme(next);
        list[next].1.clone()
    }

    /// 选择第 N 个输入方案（隐含切到中文模式）。
    pub(crate) fn select_schema(&self, index: usize) {
        let list = self.engine_mgr.available_schemas().to_vec();
        if index >= list.len() {
            return;
        }
        let id = list[index].clone();
        self.engine_mgr.switch_schema(&id);
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.chinese_mode = true;
            s.input_buffer.clear();
            s.candidates.clear();
        }
        self.push_state_update();
        self.notify_toolbar();
        self.notify_ui_hide();
        self.show_status();
    }

    /// 选择第 N 个主题。
    pub(crate) fn select_theme(&self, index: usize) {
        let list = self.list_themes();
        if index >= list.len() {
            return;
        }
        let (id, name) = list[index].clone();
        *self.theme_name.lock().unwrap_or_else(|e| e.into_inner()) = id.clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        self.push_theme(&id, dark);
        self.persist_theme(&id);
        self.show_tip(&format!("主题: {}", name));
    }

    /// 设置主题明暗（0 跟随/1 亮/2 暗），用当前主题重解析,并持久化到 config.ui.theme.style。
    pub(crate) fn set_theme_style(&self, style: u8) {
        let dark = style == 2;
        *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner()) = dark;
        let style_str = match style {
            1 => "light",
            2 => "dark",
            _ => "system",
        };
        let _ = Config::set_user_string(&["ui", "theme", "style"], style_str);
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.push_theme(&name, dark);
        self.show_tip(if dark { "暗色" } else { "亮色" });
    }

    /// 持久化主题选择。以 config.ui.theme.name 为单一源(设置页/右键统一,reload 据此应用);
    /// 兼写 theme.txt 供旧版/快速回退。
    pub(crate) fn persist_theme(&self, name: &str) {
        let _ = Config::set_user_string(&["ui", "theme", "name"], name);
        if let Some(p) = &self.theme_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, name);
        }
    }

    /// 主题搜索目录：用户主题目录（%APPDATA%\WindInput\themes，优先覆盖）+ 安装主题目录。
    /// 用户目录靠前 → 同名主题用户版覆盖内置；base 继承跨目录解析（用户主题可 `base: _base`）。
    pub(crate) fn theme_search_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Some(d) = Config::user_config_dir() {
            dirs.push(d.join("themes"));
        }
        if let Some(d) = &self.themes_dir {
            dirs.push(d.clone());
        }
        dirs
    }

    /// 加载并下发指定主题（失败保留当前）。跨用户+安装目录解析（含 base 继承）。
    pub(crate) fn push_theme(&self, name: &str, is_dark: bool) {
        let dirs = self.theme_search_dirs();
        if dirs.is_empty() {
            return;
        }
        match wind_theme::load_resolved_dirs(&dirs, name, is_dark) {
            Ok(t) => {
                info!("Loaded theme: {} (dark={})", name, is_dark);
                let _ = self.ui_tx.send(UiCommand::SetTheme(Box::new(t)));
            }
            Err(e) => warn!("Failed to load theme {}: {}", name, e),
        }
    }

    /// 列出可用主题：(id, 显示名)。扫用户+安装目录，含 theme.yaml、非 `_` 前缀；
    /// 显示名取 meta.name（缺则用 id），按 (meta.order, id) 排序。
    pub(crate) fn list_themes(&self) -> Vec<(String, String)> {
        let dirs = self.theme_search_dirs();
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<(String, String, i32)> = Vec::new();
        for dir in &dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.filter_map(|e| e.ok()) {
                if !e.path().is_dir() {
                    continue;
                }
                let Ok(id) = e.file_name().into_string() else {
                    continue;
                };
                if id.starts_with('_') || !dir.join(&id).join("theme.yaml").exists() {
                    continue;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let meta = wind_theme::read_meta(&dirs, &id);
                let name = meta
                    .as_ref()
                    .map(|m| m.name.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.clone());
                let order = meta.as_ref().map(|m| m.order).unwrap_or(0);
                rows.push((id, name, order));
            }
        }
        rows.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter().map(|(id, name, _)| (id, name)).collect()
    }

    /// 方案显示名（友好名优先，未知回退 id）
    pub(crate) fn schema_display_name(id: &str) -> String {
        match id {
            "wubi86" => "五笔".to_string(),
            "pinyin" => "拼音".to_string(),
            "shuangpin" => "双拼".to_string(),
            "wubi86_pinyin" => "五笔拼音".to_string(),
            other => other.to_string(),
        }
    }

    /// 当前激活模式的指示名 (全称, 短称)；None = 无可指示模式（普通输入/网址模式）。
    /// 临时拼音/双拼按目标方案派生（拼/双）；mix/special 取配置 name + short_name。
    pub(crate) fn mode_indicator_names(&self, state: &State) -> Option<(String, String)> {
        match state.active? {
            ModeKind::TempPinyin => {
                let disp = Self::schema_display_name(&state.temp_pinyin_schema);
                let short = disp
                    .chars()
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                Some((format!("临时{}", disp), short))
            }
            ModeKind::TempEnglish => Some(("临时英文".to_string(), "英".to_string())),
            ModeKind::Url => Some(("网址输入".to_string(), "网址".to_string())),
            ModeKind::Mix(i) => {
                let rt = self.rt();
                let m = rt.config.schema.mix_modes.get(i as usize)?;
                let full = if m.name.is_empty() {
                    "快捷".to_string()
                } else {
                    m.name.clone()
                };
                let short = Self::short_or_first(&m.short_name, &full);
                Some((full, short))
            }
            ModeKind::Special(i) => {
                let rt = self.rt();
                let m = rt.config.schema.special_modes.get(i as usize)?;
                let full = m.name.clone();
                let short = Self::short_or_first(&m.short_name, &full);
                Some((full, short))
            }
        }
    }

    /// 短称：配置非空则用之，否则取全称首字。
    fn short_or_first(short: &str, full: &str) -> String {
        if !short.trim().is_empty() {
            short.trim().to_string()
        } else {
            full.chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_default()
        }
    }

    /// 按 ui.mode_indicator.style 解析出当前应显示的指示文本；None = 不显示。
    pub(crate) fn mode_indicator_text(&self, state: &State) -> Option<String> {
        use wind_config::ModeIndicatorStyle;
        let (full, short) = self.mode_indicator_names(state)?;
        match self.rt().config.ui.mode_indicator.parsed_style() {
            ModeIndicatorStyle::None => None,
            ModeIndicatorStyle::Full => Some(full),
            ModeIndicatorStyle::Short => Some(short),
        }
    }

    /// 切换方案：清空输入并推送状态
    pub(crate) fn switch_schema(&self, schema_id: &str) {
        // 目标方案缓存尚未就绪（后台预热未到/正在构建）：显示「准备中」并放弃本次切换，
        // 避免在 IME 线程同步重熔大词库卡顿。预热完成后用户再切即时生效。
        if !self.engine_mgr.is_loaded(schema_id) {
            let name = self.engine_mgr.schema_name(schema_id);
            self.show_tip(&format!(
                "{}准备中…",
                if name.is_empty() { schema_id } else { &name }
            ));
            return;
        }
        if self.engine_mgr.switch_schema(schema_id) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.input_buffer.clear();
            state.candidates.clear();
            drop(state);
            self.notify_ui_hide();
            self.push_state_update();
        }
    }

    pub(crate) fn cycle_schema(&self) {
        if let Some(next) = self.engine_mgr.cycle_schema() {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.input_buffer.clear();
            state.candidates.clear();
            state.preedit.clear();
            drop(state);
            self.notify_ui_hide();
            self.push_state_update();
            self.show_status();
            self.notify_toolbar();
            info!("Cycled to schema: {}", next);
        }
    }

    /// 判断 key_code 是否为配置的 toggle 模式键（从编译后的 key_up 热键提取 vk 低 16 位）。
    /// TSF 仅在干净单击时于 keyUp 转发这些键，故据此判定即可直接切换。
    pub(crate) fn is_toggle_mode_keycode(&self, key_code: u32) -> bool {
        self.rt()
            .compiled_hotkeys
            .key_up
            .iter()
            .any(|e| (e.match_hash & 0xFFFF) == key_code)
    }

    /// 找出 key_code 匹配的 mix 模式下标（按配置顺序先到先得）。
    pub(crate) fn match_mix_trigger(&self, key_code: u32) -> Option<u8> {
        for (i, m) in self.rt().config.schema.mix_modes.iter().enumerate() {
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

    /// 选中当前页第 `page_offset`（0=首选）候选。
    /// 文本透镜（拼音/英文）走组合区逐步转换：部分匹配并入 committed 前缀、裁剪缓冲、重转剩余
    /// （剩余仍由 mix 成员方案出候选，不落五笔），留模式内不上屏；完整匹配整体上屏 + 造词。
    /// 数字透镜（计算）的候选恒整体上屏。
    pub(crate) fn mix_select(&self, state: &mut State, page_offset: usize) -> KeyAction {
        let (start, end) = self.page_range(state);
        let gi = start + page_offset;
        if gi >= end {
            return KeyAction::Consumed;
        }
        let cand = state.candidates[gi].clone();
        let numeric = self.mix_has_quick_input(state.mix_id) && state.mix_numeric;
        let total = state.mix_buffer.len();
        let consumed = cand.consumed_length;
        let partial = !numeric
            && consumed > 0
            && consumed < total
            && state.mix_buffer.is_char_boundary(consumed);
        if partial {
            let code = Self::cand_code(&state.mix_buffer, &cand);
            self.record_selection(&code, &cand.text);
            self.record_commit(
                &cand.text,
                code.len() as u32,
                page_offset as i32,
                wind_store::stats::CommitSource::Mix,
            );
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.mix_buffer = state.mix_buffer[consumed..].to_string();
            self.update_mix_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            let out = format!("{}{}", state.committed_text, cand.text);
            let code_len = if numeric {
                0
            } else {
                Self::cand_code(&state.mix_buffer, &cand).len() as u32
            };
            if !numeric {
                let code = Self::cand_code(&state.mix_buffer, &cand);
                self.record_selection(&code, &cand.text);
                state.committed_segs.push((code, cand.text.clone()));
                self.learn_phrase_on_commit(state);
            }
            // 输入统计：混合模式上屏（计算结果 code_len=0；选词用候选码长）。
            self.record_commit(
                &cand.text,
                code_len,
                page_offset as i32,
                wind_store::stats::CommitSource::Mix,
            );
            let out = self.maybe_s2t(state, &out);
            self.exit_mix_mode(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 刷新 mix 候选：按配置成员序逐个查询、合并、按文本去重。
    /// "quick_input" 是内置类方案（日期/计算），用 generate_quick_input_candidates 计算；
    /// 其余为真实方案经 convert_with。数字模式只取 quick_input（表达式），文本模式只取真实方案
    /// （拼音/英文），避免互相污染候选。
    pub(crate) fn update_mix_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        // 组合区 = 显示态前缀 + 已转换前缀（文本透镜逐步转换累积）+ 剩余缓冲。
        state.preedit = format!(
            "{}{}{}",
            state.mix_prefix, state.committed_text, state.mix_buffer
        );
        if state.mix_buffer.is_empty() {
            return;
        }
        let numeric = self.mix_has_quick_input(state.mix_id) && state.mix_numeric;
        let members = self
            .rt()
            .config
            .schema
            .mix_modes
            .get(state.mix_id as usize)
            .map(|m| m.members.clone())
            .unwrap_or_default();
        let mut cands: Vec<Candidate> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // 文本透镜：取首个真实方案的 preedit_display（拼音含音节分隔 "ni hao"）作组合区显示。
        let mut text_display: Option<String> = None;
        for member in &members {
            if member == "quick_input" {
                if !numeric {
                    continue; // 文本模式跳过计算
                }
                let dp = self.rt().config.schema.quick_input.decimal_places;
                for t in wind_quick_input::generate_quick_input_candidates(&state.mix_buffer, dp) {
                    if seen.insert(t.clone()) {
                        cands.push(Candidate {
                            text: t,
                            ..Default::default()
                        });
                    }
                }
            } else {
                if numeric {
                    continue; // 数字模式跳过真实方案（表达式无拼音/英文意义）
                }
                if !self.engine_mgr.ensure_schema(member) {
                    continue;
                }
                let result = self.engine_mgr.convert_with(member, &state.mix_buffer, 50);
                if text_display.is_none() && !result.preedit_display.is_empty() {
                    text_display = Some(result.preedit_display.clone());
                }
                for c in result.candidates {
                    if seen.insert(c.text.clone()) {
                        cands.push(c);
                    }
                }
            }
        }
        // 文本透镜用音节分隔显示；数字透镜（计算）保持原始表达式。
        if let Some(disp) = text_display {
            state.preedit = format!("{}{}{}", state.mix_prefix, state.committed_text, disp);
        }
        state.candidates = cands;
    }

    /// 数字 lens（计算/表达式）：数字与符号（含 `=`）作输入，字母作选词。
    /// 仅含 quick_input 成员的 mix 在首字符为数字/符号时进入。返回该键应输入的字符。
    pub(crate) fn mix_numeric_input_char(key_code: u32, shift: bool) -> Option<char> {
        if (keymap::VK_A..=keymap::VK_Z).contains(&key_code) {
            None // 字母在数字 lens 作选词，不输入
        } else {
            printable_char(key_code, shift) // 数字 + 任意符号（含 = + - * / . 等）入缓冲
        }
    }

    /// mix 模式按键处理 —— 双透镜统一管线（见架构说明）。
    /// 首字符确定 lens：数字/符号 → 数字 lens（符号输入、字母选词）；字母 → 文本 lens
    /// （字母输入、数字选词、`-`/`=` 翻页）。每键顺序：控制键 → ①输入字符 → ②翻页/高亮
    /// → ③本 lens 选词键 → ④配置二三候选键 → ⑤其它标点顶屏。
    pub(crate) fn handle_mix_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_mix_candidates(state);
            let d = state.preedit.clone();
            this.notify_ui_update(state);
            KeyAction::UpdateComposition {
                text: d.clone(),
                caret_pos: d.chars().count() as u32,
            }
        };
        let commit_text = |this: &Self, state: &mut State, t: String| -> KeyAction {
            this.exit_mix_mode(state);
            this.notify_ui_hide();
            if t.is_empty() {
                KeyAction::ClearComposition
            } else {
                Self::commit_action(t, true)
            }
        };
        // 进入键二次按下（缓冲空 + 无已转换前缀）：按中英标点配置上屏该符号并退出。
        // 必须前置于下方数字透镜——否则 ; 等会被 printable_char 当表达式字符吞进缓冲。
        if state.mix_buffer.is_empty()
            && state.committed_text.is_empty()
            && self.match_mix_trigger(data.key_code) == Some(state.mix_id)
            && let Some(ch) = punct_char(data.key_code, data.modifiers & MOD_SHIFT != 0)
        {
            let out = self.convert_punct_char(state, ch);
            self.record_commit(&out, 0, -1, wind_store::stats::CommitSource::Punctuation);
            self.exit_mix_mode(state);
            self.notify_ui_hide();
            return Self::commit_action(out, true);
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_mix_mode(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 分步撤销：文本透镜有已转换段先退回最后一段（你→ni，码并回缓冲前部）。
                if let Some((code, _)) = state.committed_segs.pop() {
                    state.committed_text = state
                        .committed_segs
                        .iter()
                        .map(|(_, t)| t.as_str())
                        .collect();
                    state.mix_buffer = format!("{}{}", code, state.mix_buffer);
                    return refresh(self, state);
                }
                state.mix_buffer.pop();
                if state.mix_buffer.is_empty() {
                    self.exit_mix_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    refresh(self, state)
                }
            }
            keymap::VK_SPACE => {
                // 空格：选当前高亮候选（文本透镜逐步转换）
                if state.candidates.is_empty() {
                    // 上屏剩余原码：committed 段已在各次选词记过，此处只记 mix_buffer 避免重复。
                    self.record_commit(
                        &state.mix_buffer,
                        state.mix_buffer.len() as u32,
                        -1,
                        wind_store::stats::CommitSource::Mix,
                    );
                    let out = self.maybe_s2t(
                        state,
                        &format!("{}{}", state.committed_text, state.mix_buffer),
                    );
                    commit_text(self, state, out)
                } else {
                    let (start, _) = self.page_range(state);
                    let gi = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    self.mix_select(state, gi - start)
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏「已转换前缀 + 缓冲原文」（如完整表达式 100+200=300，或已转中文+剩余拼音）
                self.record_commit(
                    &state.mix_buffer,
                    state.mix_buffer.len() as u32,
                    -1,
                    wind_store::stats::CommitSource::Mix,
                );
                let out = self.maybe_s2t(
                    state,
                    &format!("{}{}", state.committed_text, state.mix_buffer),
                );
                commit_text(self, state, out)
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                let calc = self.mix_has_quick_input(state.mix_id);
                // 首字符确定 lens：非字母可打印字符（数字/符号）→ 数字 lens。
                if state.mix_buffer.is_empty() {
                    let is_letter = (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code);
                    state.mix_numeric =
                        calc && !is_letter && printable_char(data.key_code, shift).is_some();
                }
                let numeric = calc && state.mix_numeric;

                // ① 输入字符（按 lens）
                let input = if numeric {
                    Self::mix_numeric_input_char(data.key_code, shift)
                } else if (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code) {
                    Some((b'a' + (data.key_code - keymap::VK_A) as u8) as char)
                } else {
                    None
                };
                if let Some(ch) = input {
                    state.mix_buffer.push(ch);
                    return refresh(self, state);
                }

                // ② 翻页/高亮（输入字符已消费；数字 lens 的 -/= 已作输入吃掉）
                if let Some(act) = self.apply_nav_key(state, data, true) {
                    return act;
                }

                // ③ 本 lens 选词键：数字 lens 用字母（a=首选），文本 lens 用数字（1=首选）
                let sel = if numeric {
                    (keymap::VK_A..=keymap::VK_Z)
                        .contains(&data.key_code)
                        .then(|| (data.key_code - keymap::VK_A) as usize)
                } else {
                    (keymap::VK_1..=keymap::VK_9)
                        .contains(&data.key_code)
                        .then(|| (data.key_code - keymap::VK_1) as usize)
                };
                if let Some(off) = sel {
                    return self.mix_select(state, off);
                }

                // ④ 配置二三候选键
                if !shift && let Some(offset) = self.select_key_offset(data.key_code) {
                    return self.mix_select(state, offset);
                }

                // ⑤ 其它标点：顶屏「已转换前缀 + 当前高亮候选」+ 转换后标点，退出
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let head = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        format!("{}{}", state.committed_text, state.candidates[idx].text)
                    } else {
                        state.committed_text.clone()
                    };
                    let head = self.maybe_s2t(state, &head);
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_mix_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", head, punct), true)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }
}
