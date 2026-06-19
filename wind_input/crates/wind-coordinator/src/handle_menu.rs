//! 功能菜单与工具栏
//!
//! 主菜单 / 候选右键菜单的构建与分派、工具栏点击/刷新/位置持久化。
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, FILTER_MODES, S2T_VARIANTS};
use wind_bridge::handler::MessageHandler;
use wind_keys::keymap;
use wind_config::Config;
use wind_ui::manager::{CandidateOp, MenuCmd, MenuKind, ToolbarAction, UiCommand};
use wind_ui::toolbar::ToolbarState;

impl Coordinator {
    /// 菜单项激活：UI 已自管导航/子菜单，这里仅按动作派发。
    pub(crate) fn menu_action(&self, kind: MenuKind) {
        let (page_local, text) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.menu_target_page_local, s.menu_target_text.clone())
        };
        self.menu_close();
        match kind {
            MenuKind::Op(op) => self.candidate_op(op, page_local),
            MenuKind::Copy => {
                let _ = self.ui_tx.send(UiCommand::CopyToClipboard(text));
            }
            MenuKind::Command(cmd) => self.run_menu_cmd(cmd),
            MenuKind::Submenu | MenuKind::Separator => {}
        }
    }

    /// 执行功能主菜单命令
    pub(crate) fn run_menu_cmd(&self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::SchemaEnglish => {
                self.handle_system_mode_switch(false);
                self.notify_toolbar();
                self.notify_ui_hide();
            }
            MenuCmd::SchemaSelect(i) => self.select_schema(i),
            MenuCmd::TogglePunct => {
                self.handle_menu_command("toggle_punct");
                self.notify_toolbar();
            }
            MenuCmd::ToggleWidth => {
                self.handle_menu_command("toggle_width");
                self.notify_toolbar();
            }
            MenuCmd::ToggleS2t => {
                self.handle_menu_command("toggle_s2t");
                self.notify_toolbar();
            }
            MenuCmd::S2tVariant(i) => self.set_s2t_variant(i),
            MenuCmd::FilterMode(i) => self.set_filter_mode(i),
            MenuCmd::ThemeSelect(i) => self.select_theme(i),
            MenuCmd::ThemeStyle(style) => self.set_theme_style(style),
            MenuCmd::ToggleToolbar => self.toggle_toolbar(),
            MenuCmd::ReloadConfig => self.reload_config(),
            MenuCmd::RestartService => self.restart_service(),
            MenuCmd::OpenSettings => {
                // 开启网页配置：经内嵌 web 服务签发 token 构造 URL，交系统默认浏览器打开。
                match crate::coordinator::settings_url() {
                    Some(url) => {
                        let _ = self.ui_tx.send(UiCommand::OpenPath(url));
                    }
                    None => tracing::warn!("打开设置失败：web 服务尚未就绪"),
                }
            }
            MenuCmd::OpenConfigDir | MenuCmd::OpenDictionary | MenuCmd::OpenAbout => {
                if let Some(d) = Config::user_config_dir() {
                    let _ = self
                        .ui_tx
                        .send(UiCommand::OpenPath(d.display().to_string()));
                }
            }
        }
    }

    /// 用户开关常驻工具栏（菜单）。仅翻转 toolbar_visible，显隐交 notify_toolbar
    /// 单点决策（结合 ime_active）。
    pub(crate) fn toggle_toolbar(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.toolbar_visible = !s.toolbar_visible;
        }
        self.notify_toolbar();
    }

    /// 循环切换到下一个主题，重绘并持久化选择。
    /// 构建并显示功能主菜单（对齐 Go 统一菜单：方案/主题子菜单 + 勾选态）。
    /// x/y 为屏幕坐标；i32::MIN 表示由 UI 取光标位置。
    pub(crate) fn show_main_menu(&self, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let (chinese, punct, full, s2t, s2t_variant, filter_mode, toolbar_vis) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.chinese_mode,
                s.chinese_punct,
                s.full_width,
                s.s2t_enabled,
                s.s2t_variant.clone(),
                s.filter_mode,
                s.toolbar_visible,
            )
        };
        let cmd = |c: MenuCmd| MenuKind::Command(c);

        // 输入方案子菜单：英文 + 方案单选
        let active = self.engine_mgr.active_schema_id();
        let schemas = self.engine_mgr.available_schemas().to_vec();
        let mut schema_children =
            vec![M::leaf("英文", cmd(MenuCmd::SchemaEnglish), true, !chinese)];
        if !schemas.is_empty() {
            schema_children.push(M::separator());
            for (i, id) in schemas.iter().enumerate() {
                schema_children.push(M::leaf(
                    id.clone(),
                    cmd(MenuCmd::SchemaSelect(i)),
                    true,
                    chinese && *id == active,
                ));
            }
        }

        // 主题子菜单：主题单选 + 亮/暗
        let themes = self.list_themes();
        let cur_theme = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        let mut theme_children = Vec::new();
        for (i, (id, name)) in themes.iter().enumerate() {
            theme_children.push(M::leaf(
                name.clone(),
                cmd(MenuCmd::ThemeSelect(i)),
                true,
                *id == cur_theme,
            ));
        }
        if !theme_children.is_empty() {
            theme_children.push(M::separator());
        }
        theme_children.push(M::leaf("亮色", cmd(MenuCmd::ThemeStyle(1)), true, !dark));
        theme_children.push(M::leaf("暗色", cmd(MenuCmd::ThemeStyle(2)), true, dark));

        // 简入繁出子菜单：启用开关 + 变体单选
        let mut s2t_children = vec![
            M::leaf("启用", cmd(MenuCmd::ToggleS2t), true, s2t),
            M::separator(),
        ];
        for (i, (id, label)) in S2T_VARIANTS.iter().enumerate() {
            s2t_children.push(M::leaf(
                *label,
                cmd(MenuCmd::S2tVariant(i)),
                true,
                s2t_variant == *id,
            ));
        }

        // 检索范围子菜单：过滤模式单选
        let filter_children: Vec<_> = FILTER_MODES
            .iter()
            .enumerate()
            .map(|(i, (m, label))| {
                M::leaf(*label, cmd(MenuCmd::FilterMode(i)), true, filter_mode == *m)
            })
            .collect();

        let items = vec![
            M::submenu("输入方案", schema_children),
            M::leaf("全角", cmd(MenuCmd::ToggleWidth), true, full),
            M::leaf("中文标点", cmd(MenuCmd::TogglePunct), true, punct),
            M::submenu("简入繁出", s2t_children),
            M::submenu("检索范围", filter_children),
            M::separator(),
            M::leaf("显示工具栏", cmd(MenuCmd::ToggleToolbar), true, toolbar_vis),
            M::submenu("主题", theme_children),
            M::separator(),
            M::leaf("重载配置", cmd(MenuCmd::ReloadConfig), true, false),
            M::leaf("重启服务", cmd(MenuCmd::RestartService), true, false),
            M::separator(),
            M::leaf("词库管理...", cmd(MenuCmd::OpenDictionary), true, false),
            M::leaf("设置...", cmd(MenuCmd::OpenSettings), true, false),
            M::separator(),
            M::leaf("关于", cmd(MenuCmd::OpenAbout), true, false),
        ];
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.menu_open = true;
            s.menu_target_page_local = 0;
            s.menu_target_text = String::new();
        }
        let _ = self
            .ui_tx
            .send(UiCommand::ShowCandidateMenu { items, x, y });
    }

    pub(crate) fn is_menu_open(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_open
    }

    /// 关闭菜单
    pub(crate) fn menu_close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.menu_open {
            state.menu_open = false;
            drop(state);
            let _ = self.ui_tx.send(UiCommand::HideMenu);
        }
    }

    /// 菜单打开时转发导航键给菜单窗口；返回 true 表示已消费。
    pub(crate) fn forward_menu_key(&self, key_code: u32) -> bool {
        if !self.is_menu_open() {
            return false;
        }
        match key_code {
            // 方向键/回车/空格/ESC → 菜单窗口处理（导航/下钻/返回/激活/关闭）
            0x26
            | 0x28
            | 0x25
            | 0x27
            | keymap::VK_RETURN
            | keymap::VK_SPACE
            | keymap::VK_ESCAPE => {
                let _ = self.ui_tx.send(UiCommand::MenuKey(key_code));
            }
            // 其它键：关闭菜单并吞掉
            _ => self.menu_close(),
        }
        true
    }

    /// 构建右键候选菜单项并下发给 UI 显示。
    pub(crate) fn show_candidate_menu(&self, page_local: usize, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() || state.input_buffer.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let word = state.candidates[idx].text.clone();
        let code = state.input_buffer.clone();
        let total = state.candidates.len();
        drop(state);

        let schema = self.engine_mgr.active_schema_id();
        let has_rule = self.shadow_has_rule(&schema, &code, &word);
        let multi_char = word.chars().count() > 1;
        let op = |o: CandidateOp| MenuKind::Op(o);

        let items = vec![
            M::leaf("置顶", op(CandidateOp::MoveTop), true, false),
            M::leaf("前移", op(CandidateOp::MoveUp), idx > 0, false),
            M::leaf("后移", op(CandidateOp::MoveDown), idx + 1 < total, false),
            M::leaf("删除", op(CandidateOp::Delete), multi_char, false),
            M::leaf("恢复默认", op(CandidateOp::Reset), has_rule, false),
            M::separator(),
            M::leaf("复制", MenuKind::Copy, true, false),
        ];
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.menu_open = true;
            state.menu_target_page_local = page_local;
            state.menu_target_text = word;
        }
        let _ = self
            .ui_tx
            .send(UiCommand::ShowCandidateMenu { items, x, y });
    }

    /// 读取持久化的工具栏位置（"x y" 文本）
    pub(crate) fn load_toolbar_pos(&self) -> Option<(i32, i32)> {
        let p = self.toolbar_pos_path.as_ref()?;
        let content = std::fs::read_to_string(p).ok()?;
        let mut it = content.split_whitespace();
        let x: i32 = it.next()?.parse().ok()?;
        let y: i32 = it.next()?.parse().ok()?;
        Some((x, y))
    }

    /// 持久化工具栏位置（best-effort）
    pub(crate) fn save_toolbar_pos(&self, x: i32, y: i32) {
        if let Some(p) = &self.toolbar_pos_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, format!("{} {}", x, y));
        }
    }

    /// 工具栏单元格点击：复用菜单命令切换状态（内部已推送 C++），再刷新工具栏显示。
    pub(crate) fn mouse_toolbar(&self, action: ToolbarAction) {
        let cmd = match action {
            ToolbarAction::ToggleMode => "toggle_mode",
            ToolbarAction::SwitchEngine => "switch_engine",
            ToolbarAction::TogglePunct => "toggle_punct",
            ToolbarAction::ToggleWidth => "toggle_width",
        };
        self.handle_menu_command(cmd);
        self.notify_toolbar();
    }

    /// 推送当前状态到常驻工具栏（中英/方案/标点/全半角）
    /// 工具栏可见性单点决策 + 内容刷新。对齐 Go toolbar_reducer 的合取公式：
    /// 仅当 `ime_active && toolbar_visible` 时显示（UpdateToolbar 会刷内容+定位+显示），
    /// 否则下发 HideToolbar。所有调用点（启动/切模式/切方案/激活/失活）经此单点决策，
    /// 不再各自直接显示，根治“工具栏总是显示、切走输入法不隐藏”。
    pub(crate) fn notify_toolbar(&self) {
        let schema_label = Self::schema_display_name(&self.engine_mgr.active_schema_id());
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !(s.ime_active && s.toolbar_visible) {
            drop(s);
            let _ = self.ui_tx.send(UiCommand::HideToolbar);
            return;
        }
        let tb = ToolbarState {
            chinese_mode: s.chinese_mode,
            schema_label,
            full_width: s.full_width,
            chinese_punct: s.chinese_punct,
        };
        drop(s);
        let _ = self.ui_tx.send(UiCommand::UpdateToolbar(tb));
    }
}
