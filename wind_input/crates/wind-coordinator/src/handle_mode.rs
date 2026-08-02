//! 模式 / 方案 / 主题切换
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 简繁、方案切换、主题切换、mix 融合模式、引擎方案叠加。

use crate::coordinator::{Coordinator, State};
use crate::pipeline::ModeKind;
use crate::preedit_cursor;
use crate::theme_style::ThemeStyle;
use tracing::{debug, info, warn};
use wind_bridge::handler::KeyAction;
use wind_config::Config;
use wind_ui::manager::UiCommand;

use crate::coordinator::{numpad_char, printable_char, punct_char};
use wind_bridge::handler::KeyEventData;
use wind_candidate::Candidate;
use wind_ipc::protocol::MOD_SHIFT;
use wind_keys::keymap;

impl Coordinator {
    /// 当前是否处于临时拼音模式（测试/诊断用）。
    pub fn debug_in_temp_pinyin(&self) -> bool {
        matches!(
            self.state.lock().unwrap_or_else(|e| e.into_inner()).active,
            Some(ModeKind::TempPinyin)
        )
    }

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

    /// mix 成员占位符解析：`$primary_pinyin` → `schema.primary_pinyin`（空=全拼）。
    /// 字面方案 id 原样返回——显式写 "pinyin" 即精确要全拼，永不被替换。
    /// 关联函数（入参 primary 而非读 self.rt()）：调用方多在已持 rt() 的闭包内，避免嵌套借用。
    pub(crate) fn resolve_mix_member(member: &str, primary_pinyin: &str) -> String {
        if member != wind_config::config::MIX_MEMBER_PRIMARY_PINYIN {
            return member.to_string();
        }
        if primary_pinyin.is_empty() {
            wind_config::config::DEFAULT_PINYIN_SCHEMA.to_string()
        } else {
            primary_pinyin.to_string()
        }
    }

    /// mix 模式的成员方案 id 列表（占位符已解析，未过滤）。
    fn mix_members_resolved(&self, idx: u8) -> Vec<String> {
        let rt = self.rt();
        let primary = rt.config.schema.primary_pinyin.clone();
        rt.config
            .schema
            .mix_modes
            .get(idx as usize)
            .map(|m| {
                m.members
                    .iter()
                    .map(|s| Self::resolve_mix_member(s, &primary))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// mix 可用的**真实方案**成员（过滤空 / 不可加载 / 快捷输入内置来源）。
    ///
    /// 快捷来源（`quick_input.*`）没有 `.schema.toml`，由协调器直接产候选，故排除在外。
    /// 英文候选的开关**只看 members 有无**——旧的 `quick_input.enable_english` 旁路已废弃
    /// （它与 members 构成双真相源，且这里与 `update_mix_candidates` 各过滤一遍）。
    pub(crate) fn mix_members(&self, idx: u8) -> Vec<String> {
        self.mix_members_resolved(idx)
            .into_iter()
            .filter(|s| {
                !s.is_empty()
                    && !wind_quick_input::is_quick_member(s)
                    && self.engine_mgr.ensure_schema(s)
            })
            .collect()
    }

    /// mix 是否含**任一**快捷输入内置来源（计算/日期/数字/重复）。
    /// 用于「进入条件」与「强制竖排」——只配了重复上屏的 mix 也算快捷输入。
    pub(crate) fn mix_has_quick_input(&self, idx: u8) -> bool {
        self.rt()
            .config
            .schema
            .mix_modes
            .get(idx as usize)
            .map(|m| {
                m.members
                    .iter()
                    .any(|s| wind_quick_input::is_quick_member(s))
            })
            .unwrap_or(false)
    }

    /// mix 是否含**表达式类**来源（计算/日期/数字）——启用数字透镜：
    /// 首字符数字/符号进表达式录入、字母作选词、`-`/`=` 是运算符而非翻页键。
    ///
    /// 刻意与 [`Self::mix_has_quick_input`] 分开：`quick_input.repeat` 不录入表达式，
    /// 只配了它的 mix 若开数字透镜，数字键会变成录不进任何候选的死输入。
    pub(crate) fn mix_has_quick_numeric(&self, idx: u8) -> bool {
        self.rt()
            .config
            .schema
            .mix_modes
            .get(idx as usize)
            .map(|m| {
                m.members
                    .iter()
                    .any(|s| wind_quick_input::QuickSource::from_member(s).is_some())
            })
            .unwrap_or(false)
    }

    /// 进入 mix 模式（至少一个成员方案可加载，由激活点保证）。
    pub(crate) fn enter_mix_mode(&self, state: &mut State, idx: u8, key_code: u32) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Mix(idx));
        state.mix_id = idx;
        state.mix_buffer.clear();
        state.mix_cursor = 0;
        state.mix_numeric = false; // 由首字符（数字/字母）决定
        // 显示态前缀（进入键符号，如 ";"）：只显示不消费，让用户看到按下的键。
        state.mix_prefix = keymap::vk_to_prefix_char(key_code)
            .map(|c| c.to_string())
            .unwrap_or_default();
        self.update_mix_candidates(state);
        // 候选布局（本 mix 的 candidate_layout）由 notify_ui_update → sync_candidate_layout
        // 统一重算，这里不再自己保存/切换布局（见 layout.rs）。
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
        // 命令候选顶屏 → 执行命令（与按空格一致），不进模式、不上屏 display 标签。
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
            let code = Self::freq_code(&state.input_buffer, &state.candidates[i]);
            self.record_selection(&code, &t, state.candidates[i].source);
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
                self.commit_then_new_composition(text, new_comp)
            }
            None => enter,
        }
    }

    /// 退出 mix 模式并清空相关状态（含逐步转换的已转换前缀）。
    /// mix：回退最后一个已转换段——把它消费的码并回缓冲**前部**并重转，光标落码末尾
    /// （理由同主输入的 `pop_committed_seg`）。Backspace（段优先）与 Delete（删空后）共用。
    fn pop_mix_seg(
        &self,
        state: &mut State,
        refresh: &dyn Fn(&Self, &mut State) -> KeyAction,
    ) -> KeyAction {
        let Some((raw_code, _, _, _, _)) = state.committed_segs.pop() else {
            return KeyAction::Consumed;
        };
        state.committed_text = state
            .committed_segs
            .iter()
            .map(|(_, _, t, _, _)| t.as_str())
            .collect();
        state.mix_buffer = format!("{}{}", raw_code, state.mix_buffer);
        state.mix_cursor = state.mix_buffer.len();
        refresh(self, state)
    }

    pub(crate) fn exit_mix_mode(&self, state: &mut State) {
        state.active = None;
        state.mix_buffer.clear();
        state.mix_cursor = 0;
        state.mix_repeat = false;
        state.mix_prefix.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
        // 布局无需在此恢复：active 已清空，下一次 notify_ui_update 会自动算回全局基线。
    }

    /// 候选的**出口文本**（显示与上屏同源）：1对多变体候选（`s2t_override`）直接用覆盖
    /// 文本，其余按需简繁转换。凡「拿某条候选去显示/上屏」一律走本函数，勿直接
    /// `maybe_s2t(&c.text)`——否则变体候选会退化回默认转换结果（选「齣」出的却是「出」）。
    pub(crate) fn cand_s2t_text(&self, state: &State, c: &Candidate) -> String {
        match &c.s2t_override {
            Some(t) => t.clone(),
            None => self.maybe_s2t(state, &c.text),
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
        self.sync_chaizi_assets(); // 拆字库/字根字体随活跃方案切换（变更检测，未变不动）
        self.sync_comment_dicts(); // 方案专属注释库（`schemas` 字段）同理
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
        if let Err(e) = Config::set_user_string(&["schema", "active"], &id) {
            warn!("select_schema: 持久化 schema.active 失败: {}", e);
        }
    }

    /// 选择第 N 个主题。
    pub(crate) fn select_theme(&self, index: usize) {
        let list = self.list_themes();
        if index >= list.len() {
            return;
        }
        let (id, name) = list[index].clone();
        *self.theme_name.lock().unwrap_or_else(|e| e.into_inner()) = id.clone();
        let dark = self.resolve_theme_dark();
        self.push_theme(&id, dark);
        self.persist_theme(&id);
        self.show_tip(&format!("主题: {}", name));
    }

    /// 当前该用暗色吗：读运行时明暗设置，`system` 交由实时探测系统明暗。
    pub(crate) fn resolve_theme_dark(&self) -> bool {
        self.theme_style
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_dark()
    }

    /// 设置主题明暗（菜单协议编码：0 跟随/1 亮/2 暗），用当前主题重解析并持久化到
    /// config.ui.theme.style。
    pub(crate) fn set_theme_style(&self, style: u8) {
        let style = ThemeStyle::from_menu_id(style);
        *self.theme_style.lock().unwrap_or_else(|e| e.into_inner()) = style;
        let _ = Config::set_user_string(&["ui", "theme", "style"], style.as_config());
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.push_theme(&name, style.resolve_dark());
        self.show_tip(style.label());
    }

    /// 系统「浅色/深色模式」切换的响应（UI 线程截获 WM_SETTINGCHANGE 后回送）。
    ///
    /// 仅 `system` 需要动作——显式选了亮/暗的用户不该被系统设置改写。
    pub(crate) fn on_system_theme_changed(&self) {
        let style = *self.theme_style.lock().unwrap_or_else(|e| e.into_inner());
        if style != ThemeStyle::System {
            return;
        }
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let dark = style.resolve_dark();
        tracing::info!("系统明暗切换 → 重解析主题 {} (dark={})", name, dark);
        self.push_theme(&name, dark);
    }

    /// 持久化主题选择。config.ui.theme.name 为单一源（设置页/右键统一，reload 据此应用）。
    pub(crate) fn persist_theme(&self, name: &str) {
        let _ = Config::set_user_string(&["ui", "theme", "name"], name);
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
                // 记录主题定义的序号槽位，供 index_label 裁决「用户 > 主题 > 默认」。
                *self
                    .theme_index_labels
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = t.views.index_labels.clone();
                let _ = self.ui_tx.send(UiCommand::SetTheme(Box::new(t)));
            }
            Err(e) => warn!("Failed to load theme {}: {}", name, e),
        }
    }

    /// 列出可用主题：(id, 显示名)。程序目录主题优先（按 order/id 排序），
    /// 用户目录独有主题排后（忽略 order，按 id 排序）。
    ///
    /// 唯一排序实现：右键菜单与 RPC `theme.list`（[`Self::web_theme_list`]）
    /// 均基于此结果，避免两处各自扫描目录导致顺序不一致。
    pub(crate) fn list_themes(&self) -> Vec<(String, String)> {
        self.list_themes_full()
            .into_iter()
            .map(|(id, name, _builtin)| (id, name))
            .collect()
    }

    /// [`Self::list_themes`] 的完整版本，附带是否内置（程序目录）标记。
    pub(crate) fn list_themes_full(&self) -> Vec<(String, String, bool)> {
        let all_dirs = self.theme_search_dirs();
        let user_dir = Config::user_config_dir().map(|d| d.join("themes"));

        // 扫描程序目录主题，按 (order, id) 排序
        let mut prog_rows: Vec<(String, String, i32)> = Vec::new();
        if let Some(dir) = &self.themes_dir {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.filter_map(|e| e.ok()) {
                    if !e.path().is_dir() {
                        continue;
                    }
                    let Ok(id) = e.file_name().into_string() else {
                        continue;
                    };
                    if id.starts_with('_') || !dir.join(&id).join("theme.toml").exists() {
                        continue;
                    }
                    let meta = wind_theme::read_meta(&all_dirs, &id);
                    let name = meta
                        .as_ref()
                        .map(|m| m.name.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| id.clone());
                    let order = meta.as_ref().map(|m| m.order).unwrap_or(0);
                    prog_rows.push((id, name, order));
                }
            }
            prog_rows.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        }

        let prog_ids: std::collections::HashSet<String> =
            prog_rows.iter().map(|(id, _, _)| id.clone()).collect();

        // 扫描用户目录独有主题（与程序目录不重叠），按 id 排序，忽略 order
        let mut user_rows: Vec<(String, String)> = Vec::new();
        if let Some(udir) = &user_dir {
            if let Ok(rd) = std::fs::read_dir(udir) {
                for e in rd.filter_map(|e| e.ok()) {
                    if !e.path().is_dir() {
                        continue;
                    }
                    let Ok(id) = e.file_name().into_string() else {
                        continue;
                    };
                    if id.starts_with('_') || !udir.join(&id).join("theme.toml").exists() {
                        continue;
                    }
                    if prog_ids.contains(&id) {
                        continue;
                    }
                    let meta = wind_theme::read_meta(&all_dirs, &id);
                    let name = meta
                        .as_ref()
                        .map(|m| m.name.clone())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| id.clone());
                    user_rows.push((id, name));
                }
            }
            user_rows.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let mut result: Vec<(String, String, bool)> = prog_rows
            .into_iter()
            .map(|(id, name, _order)| (id, name, true))
            .collect();
        result.extend(user_rows.into_iter().map(|(id, name)| (id, name, false)));
        result
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
                // 瘦身条目只写 schema + trigger_keys：name/short_name 缺省时从被引用方案文件派生
                // （schema.name / schema.icon_label），避免与方案文件重复。
                let (name, short_name, schema) = {
                    let rt = self.rt();
                    let m = rt.config.schema.special_modes.get(i as usize)?;
                    (m.name.clone(), m.short_name.clone(), m.schema.clone())
                };
                let full = if name.is_empty() {
                    self.engine_mgr.schema_name(&schema)
                } else {
                    name
                };
                let short = if short_name.is_empty() {
                    let icon = self.engine_mgr.schema_icon_label(&schema);
                    if icon.is_empty() {
                        Self::short_or_first("", &full)
                    } else {
                        icon
                    }
                } else {
                    Self::short_or_first(&short_name, &full)
                };
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
            self.sync_chaizi_assets(); // 拆字库/字根字体随活跃方案切换（变更检测，未变不动）
            self.sync_comment_dicts(); // 方案专属注释库（`schemas` 字段）同理
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
            self.sync_chaizi_assets(); // 拆字库/字根字体随活跃方案切换（变更检测，未变不动）
            self.sync_comment_dicts(); // 方案专属注释库（`schemas` 字段）同理
            // 「切换模式时取消大小写锁定」延伸：切方案的意图是用新方案输中文，
            // 配置开启时取消 CapsLock，且若当前为英文模式一并归位中文。
            let caps_cancelled = self.cancel_caps_on_switch();
            let bundle = self.rt();
            let cancel_cfg = bundle.config.input.capslock.cancel_on_mode_switch;
            let follow = bundle.config.input.punct.follow_mode;
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let to_chinese = cancel_cfg && !state.chinese_mode;
            if caps_cancelled || to_chinese {
                state.chinese_mode = true;
                if follow {
                    state.chinese_punct = true;
                }
            }
            state.input_buffer.clear();
            state.candidates.clear();
            state.preedit.clear();
            drop(state);
            if caps_cancelled || to_chinese {
                self.record_app_mode(true);
                self.record_last_state();
            }
            self.notify_ui_hide();
            self.push_state_update();
            self.show_status();
            self.notify_toolbar();
            info!("Cycled to schema: {}", next);
            if let Err(e) = Config::set_user_string(&["schema", "active"], &next) {
                warn!("cycle_schema: 持久化 schema.active 失败: {}", e);
            }
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
        // $AA/$SS 组折叠候选：补全编码到完整码并重查展开（二级选择，不上屏组名）。
        if cand.is_group {
            state.mix_buffer = cand.group_code.clone();
            state.mix_cursor = state.mix_buffer.len(); // 补全到完整码：光标落末尾
            self.update_mix_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            return KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            };
        }
        // $CC 命令候选：执行动作（退出混输后异步跑），不走文本/分段上屏。
        let code = state.mix_buffer.clone();
        if let Some(act) =
            self.overlay_commit_command(state, &cand, &code, |s, st| s.exit_mix_mode(st))
        {
            return act;
        }
        let numeric = self.mix_has_quick_numeric(state.mix_id) && state.mix_numeric;
        let total = state.mix_buffer.len();
        let consumed = cand.consumed_length;
        let partial = !numeric
            && consumed > 0
            && consumed < total
            && state.mix_buffer.is_char_boundary(consumed);
        if partial {
            let code = Self::cand_code(&state.mix_buffer, &cand);
            // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
            self.record_selection(
                &Self::freq_code(&state.mix_buffer, &cand),
                &cand.text,
                cand.source,
            );
            self.record_commit(
                &cand.text,
                code.len() as u32,
                page_offset as i32,
                wind_store::stats::CommitSource::Mix,
            );
            state.committed_segs.push((
                Self::raw_consumed_code(&state.mix_buffer, consumed, true),
                code,
                cand.text.clone(),
                cand.source,
                cand.boundary,
            ));
            state.committed_text.push_str(&cand.text);
            state.mix_buffer = state.mix_buffer[consumed..].to_string();
            // 分步确认消费掉前缀码：光标落剩余码末尾
            state.mix_cursor = state.mix_buffer.len();
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
                // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
                let code = Self::freq_code(&state.mix_buffer, &cand);
                self.record_selection(&code, &cand.text, cand.source);
                state.committed_segs.push((
                    state.mix_buffer.clone(), // 消费整串：回退码即整个缓冲
                    code,
                    cand.text.clone(),
                    cand.source,
                    cand.boundary,
                ));
                self.learn_phrase_on_commit(state);
            } else {
                // 数字透镜（计算/日期/金额）无编码可记词频，但同样是一次上屏：
                // 单独记历史，使「算完再按 ; 空格」能重复刚上屏的结果。
                self.push_commit_history(&cand.text);
            }
            // 输入统计：混合模式上屏（计算结果 code_len=0；选词用候选码长）。
            self.record_commit(
                &cand.text,
                code_len,
                page_offset as i32,
                wind_store::stats::CommitSource::Mix,
            );
            // 变体候选末段用覆盖文本；普通候选整体转换（保留 STPhrases 跨段词级消歧）。
            let out = match &cand.s2t_override {
                Some(t) => format!("{}{}", self.maybe_s2t(state, &state.committed_text), t),
                None => self.maybe_s2t(state, &out),
            };
            self.exit_mix_mode(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 刷新 mix 候选：按配置成员序逐个查询、合并、按文本去重。
    ///
    /// 成员分三类：快捷输入内置来源（`quick_input.calc/.date/.number`，由
    /// `wind_quick_input` 直接算）、重复上屏（`quick_input.repeat`，取上屏历史，**仅空缓冲时**）、
    /// 真实方案（拼音/英文等，经 `convert_with`）。
    ///
    /// 数字透镜只取内置来源（表达式无拼音/英文意义），文本透镜只取真实方案，避免互相污染。
    /// **成员顺序即候选优先级**——把 `quick_input.calc` 排在最前即得「计算结果作首选」。
    pub(crate) fn update_mix_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        state.mix_repeat = false;
        // 组合区 = 显示态前缀 + 已转换前缀（文本透镜逐步转换累积）+ 剩余缓冲。
        state.preedit = format!(
            "{}{}{}",
            state.mix_prefix, state.committed_text, state.mix_buffer
        );
        // 默认主体 = 原始缓冲；文本透镜若给出音节分隔显示，下方会覆盖为该显示串。
        state.overlay_body = state.mix_buffer.clone();
        if state.mix_buffer.is_empty() {
            self.inject_mix_repeat_candidate(state);
            return;
        }
        let numeric = self.mix_has_quick_numeric(state.mix_id) && state.mix_numeric;
        let members = self.mix_members_resolved(state.mix_id);
        let mut cands: Vec<Candidate> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // 文本透镜：取首个真实方案的 preedit_display（拼音含音节分隔 "ni hao"）作组合区显示。
        let mut text_display: Option<String> = None;
        for member in &members {
            if let Some(src) = wind_quick_input::QuickSource::from_member(member) {
                if !numeric {
                    continue; // 文本模式跳过表达式类来源
                }
                let dp = self.rt().config.schema.quick_input.decimal_places;
                for t in wind_quick_input::generate(src, &state.mix_buffer, dp) {
                    if !t.is_empty() && seen.insert(t.clone()) {
                        cands.push(Candidate {
                            text: t,
                            ..Default::default()
                        });
                    }
                }
            } else if wind_quick_input::is_quick_member(member) {
                // quick_input.repeat：仅空缓冲时有候选（上面已 return），此处无动作。
                // 旧值 quick_input 若漏迁移也落这里——不产候选，胜过按未知方案去加载。
                continue;
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
            state.overlay_body = disp; // 供光标换算（含引擎插入的音节分隔符）
        }
        // 统一展开汇聚点：混输成员词库候选内 `$` 特殊语法在此展开（见 finalize_candidates）。
        state.candidates = self.finalize_candidates(cands, &state.mix_buffer);
        // 简繁 1对多变体展开（约束见 expand_s2t_variants 文档）。
        self.expand_s2t_variants(state);
    }

    /// 空缓冲时注入「重复上屏」候选（成员 `quick_input.repeat`）：把上次上屏的内容
    /// 摆成唯一候选，按空格即再上屏一次。
    ///
    /// 这是快捷输入的固有能力（Go 版 `handleQuickInputRepeat`），Rust 重写为 mix 成员时丢失。
    /// 复用 `recent_commits` 上屏历史，与 z 键重复上屏、加词推荐同一事实源。
    ///
    /// 置 `state.mix_repeat` 标记而非在候选上加字段：这条候选与输入缓冲无对应关系
    /// （码为空），选词记录、造词、标点顶屏三条路径都必须绕开它，用一个状态位表达
    /// 「当前候选区是重复候选」比让每条路径各自去嗅探候选特征更难写错。
    fn inject_mix_repeat_candidate(&self, state: &mut State) {
        if !state.committed_text.is_empty() {
            return; // 模式内已逐步上屏过内容：此时的空缓冲不是「刚进来」，不插重复
        }
        let has_repeat = self
            .rt()
            .config
            .schema
            .mix_modes
            .get(state.mix_id as usize)
            .map(|m| {
                m.members
                    .iter()
                    .any(|s| s == wind_quick_input::MEMBER_REPEAT)
            })
            .unwrap_or(false);
        if !has_repeat {
            return;
        }
        let Some(text) = self
            .recent_commits_snapshot()
            .into_iter()
            .find(|t| !t.is_empty())
        else {
            return;
        };
        state.candidates = vec![Candidate {
            text,
            ..Default::default()
        }];
        state.mix_repeat = true;
    }

    /// 数字 lens（计算/表达式）：数字与符号（含 `=`）作输入，字母作选词。
    /// 仅含 quick_input 成员的 mix 在首字符为数字/符号时进入。返回该键应输入的字符。
    pub(crate) fn mix_numeric_input_char(key_code: u32, shift: bool) -> Option<char> {
        if (keymap::VK_A..=keymap::VK_Z).contains(&key_code) {
            None // 字母在数字 lens 作选词，不输入
        } else {
            // 数字 + 任意符号（含 = + - * / . 等）入缓冲；小键盘键回退 numpad_char，
            // 使小键盘数字/运算符与主键盘区在数字透镜里表达式输入一致（问题：快捷输入下
            // 小键盘不生效）。
            printable_char(key_code, shift).or_else(|| numpad_char(key_code))
        }
    }

    /// mix 模式按键处理 —— 双透镜统一管线（见架构说明）。
    /// 首字符确定 lens：数字/符号 → 数字 lens（符号输入、字母选词）；字母 → 文本 lens
    /// （字母输入、数字选词、`-`/`=` 翻页）。每键顺序：控制键 → ①输入字符 → ②翻页/高亮
    /// → ③本 lens 选词键 → ④配置二三候选键 → ⑤其它标点顶屏。
    pub(crate) fn handle_mix_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        // 编码区光标移动（左右 / Home / End）。注：数字透镜下 -/= 等是输入字符，但方向键
        // 在两个透镜里都不是输入，故可在分派前统一拦截。
        if let Some(act) = self.overlay_cursor_key(state, data) {
            return act;
        }
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_mix_candidates(state);
            let d = state.preedit.clone();
            let caret_pos = this.overlay_caret(state);
            this.notify_ui_update(state);
            KeyAction::UpdateComposition { text: d, caret_pos }
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
        // 顺带武装智能符号：时限内再按同键即换英文形（`;` → `；` → `;`），否则这个键被模式
        // 占着、英文形没有通路。press2 的拦截在 try_activate_mode 开头，早于模式激活链。
        if state.mix_buffer.is_empty()
            && state.committed_text.is_empty()
            && data.modifiers & MOD_SHIFT == 0
            && self.match_mix_trigger(data.key_code) == Some(state.mix_id)
            && let Some(ch) = punct_char(data.key_code, false)
        {
            let out = self.convert_punct_char(state, ch);
            self.arm_smart_symbol_after_commit(state, ch, &out);
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
            keymap::VK_BACK | keymap::VK_DELETE => {
                // Backspace：段回退**优先于光标**（文本透镜有已转换段先退回最后一段，你→ni，
                // 码并回缓冲前部）；否则删光标前一字符。Delete 只删光标后一字符、删空后才回退段
                // ——与主输入同构的刻意不对称。缓冲删空则退出。
                let backward = data.key_code == keymap::VK_BACK;
                if backward && !state.committed_segs.is_empty() {
                    return self.pop_mix_seg(state, &refresh);
                }
                if state.mix_buffer.is_empty() {
                    if backward {
                        self.exit_mix_mode(state);
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    return KeyAction::Consumed; // Delete 且缓冲空：只吃键，不改退出语义
                }
                let removed = {
                    let mut ed =
                        preedit_cursor::BufEdit::new(&mut state.mix_buffer, &mut state.mix_cursor);
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
                if state.mix_buffer.is_empty() {
                    if !state.committed_segs.is_empty() {
                        return self.pop_mix_seg(state, &refresh);
                    }
                    self.exit_mix_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    refresh(self, state)
                }
            }
            keymap::VK_SPACE => {
                // 重复上屏：整体上屏上次内容，不记选词/不造词（该候选无对应编码）。
                if state.mix_repeat && !state.candidates.is_empty() {
                    let text = state.candidates[0].text.clone();
                    self.record_commit(&text, 0, 0, wind_store::stats::CommitSource::Mix);
                    // 重复上屏本身也入历史：连按两次仍重复同一内容（而非取到更早的一条）。
                    self.push_commit_history(&text);
                    let out = self.maybe_s2t(state, &text);
                    return commit_text(self, state, out);
                }
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
                // clear 模式：整段放弃，不上屏任何内容（含已选词的 committed_text）。
                // 须先于下方各分支——此前该判断只写在「空缓冲」分支内，导致「打了码再回车」
                // 仍走非空缓冲路径无条件上屏原码，配置形同虚设（与主输入路径行为不一致）。
                if self.enter_clears_composition() {
                    return commit_text(self, state, String::new());
                }
                // 空缓冲（只按了模式键、无已转换前缀）：commit 模式上屏模式键符号本身
                // （原样不转换，如 ;）。
                if state.mix_buffer.is_empty() && state.committed_text.is_empty() {
                    if !state.mix_prefix.is_empty() {
                        let sym = state.mix_prefix.clone();
                        self.record_commit(
                            &sym,
                            0,
                            -1,
                            wind_store::stats::CommitSource::Punctuation,
                        );
                        return commit_text(self, state, sym);
                    }
                    return commit_text(self, state, String::new());
                }
                // 非空缓冲：上屏「已转换前缀 + 缓冲原文」（原行为不变，如 ;nihao → nihao）
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
                let calc = self.mix_has_quick_numeric(state.mix_id);
                // 首字符确定 lens：非字母可打印字符（数字/符号）→ 数字 lens。
                if state.mix_buffer.is_empty() {
                    let is_letter = (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code);
                    // 首字符判定数字透镜：主键盘可打印字符或小键盘键（0x60-0x6F）均可触发，
                    // 使小键盘数字/运算符也能进入快捷输入表达式。
                    state.mix_numeric = calc
                        && !is_letter
                        && (printable_char(data.key_code, shift).is_some()
                            || numpad_char(data.key_code).is_some());
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
                    preedit_cursor::BufEdit::new(&mut state.mix_buffer, &mut state.mix_cursor)
                        .insert(ch);
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

                // ⑤ 其它标点：顶屏「已转换前缀 + 当前高亮候选」+ 转换后标点，退出。
                // 小键盘键（direct 语义）回退 numpad_char 复用此路——仅**文本透镜**会走到这里，
                // 数字透镜的小键盘早在 ① mix_numeric_input_char 作表达式字符入缓冲。
                // follow_main 时键已在入口归一化为主键盘键。
                if let Some(ch) =
                    punct_char(data.key_code, shift).or_else(|| numpad_char(data.key_code))
                {
                    // 重复上屏候选不参与顶屏：它是「空缓冲时的备选动作」而非本次输入的转换结果，
                    // 顶屏它等于用户没打字却被塞进上次的内容。此时按标点 = 空缓冲按标点。
                    let has_head = !state.mix_repeat && !state.candidates.is_empty();
                    // 高亮候选为组/命令：走统一选中（组→展开重查，命令→执行动作），标点不单独上屏。
                    if has_head {
                        let (start, _) = self.page_range(state);
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        if state.candidates[idx].is_group || state.candidates[idx].is_command {
                            return self.mix_select(state, idx - start);
                        }
                    }
                    // 高亮是变体候选时末段用覆盖文本；否则整体转换（保留跨段词级消歧）。
                    let head = if has_head {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        match &state.candidates[idx].s2t_override {
                            Some(t) => {
                                format!("{}{}", self.maybe_s2t(state, &state.committed_text), t)
                            }
                            None => self.maybe_s2t(
                                state,
                                &format!("{}{}", state.committed_text, state.candidates[idx].text),
                            ),
                        }
                    } else {
                        self.maybe_s2t(state, &state.committed_text.clone())
                    };
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

#[cfg(test)]
mod mix_numpad_tests {
    use crate::coordinator::Coordinator;

    #[test]
    fn numpad_keys_feed_numeric_lens() {
        // 小键盘数字 / 运算符 → 表达式字符（此前只认主键盘区，快捷输入下小键盘被吞）。
        assert_eq!(Coordinator::mix_numeric_input_char(0x60, false), Some('0')); // Numpad0
        assert_eq!(Coordinator::mix_numeric_input_char(0x69, false), Some('9')); // Numpad9
        assert_eq!(Coordinator::mix_numeric_input_char(0x6B, false), Some('+')); // Numpad +
        assert_eq!(Coordinator::mix_numeric_input_char(0x6D, false), Some('-')); // Numpad -
        assert_eq!(Coordinator::mix_numeric_input_char(0x6A, false), Some('*')); // Numpad *
        assert_eq!(Coordinator::mix_numeric_input_char(0x6F, false), Some('/')); // Numpad /
        assert_eq!(Coordinator::mix_numeric_input_char(0x6E, false), Some('.')); // Numpad .
        // 主键盘区数字仍正常（回归保护）。
        assert_eq!(Coordinator::mix_numeric_input_char(0x31, false), Some('1')); // VK_1
        // 字母在数字透镜里作选词，不作输入。
        assert_eq!(Coordinator::mix_numeric_input_char(0x41, false), None); // 'A'
    }

    /// mix 成员占位符解析：$primary_pinyin 跟随主拼音方案，字面 id 精确解释。
    #[test]
    fn resolve_mix_member_placeholder_vs_literal() {
        use wind_config::config::MIX_MEMBER_PRIMARY_PINYIN as PH;
        assert_eq!(
            Coordinator::resolve_mix_member(PH, "shoudao"),
            "shoudao",
            "占位符应解析为主拼音方案"
        );
        assert_eq!(
            Coordinator::resolve_mix_member(PH, ""),
            "pinyin",
            "主拼音方案为空时占位符回退全拼"
        );
        // 字面 id 一律原样——"pinyin" 表示「就要全拼」，不被主拼音方案替换。
        assert_eq!(
            Coordinator::resolve_mix_member("pinyin", "shoudao"),
            "pinyin",
            "字面 pinyin 不应被替换"
        );
        assert_eq!(
            Coordinator::resolve_mix_member("quick_input", "shoudao"),
            "quick_input"
        );
        assert_eq!(
            Coordinator::resolve_mix_member("english", "shoudao"),
            "english"
        );
    }
}
