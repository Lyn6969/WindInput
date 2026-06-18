//! 模式 / 方案 / 主题切换
//!
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。
//! 简繁、方案切换、主题切换、mix 融合模式、引擎方案叠加。

use crate::coordinator::{Coordinator, State, S2T_VARIANTS};
use crate::pipeline::ModeKind;
use tracing::{debug, info, warn};
use wind_bridge::handler::KeyAction;
use wind_config::Config;
use wind_ui::manager::UiCommand;

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
                .config
                .input
                .shift_temp_english
                .show_english_candidates
                .then(|| "english".to_string()),
            _ => None,
        }
    }

    /// mix 模式可加载的成员方案列表（过滤空/不可加载）。
    /// mix 可用的真实方案成员（过滤空 / 不可加载 / 内置 quick_input）。
    pub(crate) fn mix_members(&self, idx: u8) -> Vec<String> {
        self.config
            .features
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
        self.config
            .features
            .mix_modes
            .get(idx as usize)
            .map(|m| m.members.iter().any(|s| s == "quick_input"))
            .unwrap_or(false)
    }

    /// 进入 mix 模式（至少一个成员方案可加载，由激活点保证）。
    pub(crate) fn enter_mix_mode(&self, state: &mut State, idx: u8) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Mix(idx));
        state.mix_id = idx;
        state.mix_buffer.clear();
        state.mix_numeric = false; // 由首字符（数字/字母）决定
        self.update_mix_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered mix mode idx={}", idx);
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 退出 mix 模式并清空相关状态（含逐步转换的已转换前缀）。
    pub(crate) fn exit_mix_mode(&self, state: &mut State) {
        state.active = None;
        state.mix_buffer.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
    }

    /// 若开启简繁转换，把简体文本转为繁体（数据缺失则原样返回）。
    pub(crate) fn maybe_s2t(&self, state: &State, text: &str) -> String {
        if state.s2t_enabled
            && let Some(conv) = self.s2t.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
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
        self.show_tip(&id);
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

    /// 设置主题明暗（0 跟随/1 亮/2 暗），用当前主题重解析。
    pub(crate) fn set_theme_style(&self, style: u8) {
        let dark = style == 2;
        *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner()) = dark;
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.push_theme(&name, dark);
        self.show_tip(if dark { "暗色" } else { "亮色" });
    }

    /// 切换简繁变体（0=s2t 1=s2tw 2=s2twp 3=s2hk），重载转换器并刷新候选显示。
    pub(crate) fn set_s2t_variant(&self, index: usize) {
        let (variant, label) = match S2T_VARIANTS.get(index) {
            Some(v) => *v,
            None => return,
        };
        let dir = match &self.opencc_dir {
            Some(d) => d.clone(),
            None => {
                self.show_tip("简繁数据缺失");
                return;
            }
        };
        match wind_transform::s2t::Converter::load_variant(&dir, variant) {
            Some(conv) => {
                *self.s2t.lock().unwrap_or_else(|e| e.into_inner()) = Some(conv);
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_variant = variant.to_string();
                }
                // 组合中则按新变体重渲染候选显示
                let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !s.candidates.is_empty() {
                    self.notify_ui_update(&s);
                }
                drop(s);
                self.show_tip(label);
            }
            None => self.show_tip("简繁数据缺失"),
        }
    }

    /// 持久化主题选择到 theme.txt。
    pub(crate) fn persist_theme(&self, name: &str) {
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

    /// 切换方案：清空输入并推送状态
    pub(crate) fn switch_schema(&self, schema_id: &str) {
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
            self.show_tip(&Self::schema_display_name(&next));
            self.notify_toolbar();
            info!("Cycled to schema: {}", next);
        }
    }

    /// 判断 key_code 是否为配置的 toggle 模式键（从编译后的 key_up 热键提取 vk 低 16 位）。
    /// TSF 仅在干净单击时于 keyUp 转发这些键，故据此判定即可直接切换。
    pub(crate) fn is_toggle_mode_keycode(&self, key_code: u32) -> bool {
        self.compiled_hotkeys
            .key_up
            .iter()
            .any(|e| (e.match_hash & 0xFFFF) == key_code)
    }
}
