//! 功能菜单与工具栏
//!
//! 主菜单 / 候选右键菜单的构建与分派、工具栏点击/刷新/位置持久化。
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, FILTER_MODES};
use wind_bridge::handler::MessageHandler;
use wind_config::Config;
use wind_keys::keymap;
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
            MenuCmd::FilterMode(i) => self.set_filter_mode(i),
            MenuCmd::ThemeSelect(i) => self.select_theme(i),
            MenuCmd::ThemeStyle(style) => self.set_theme_style(style),
            MenuCmd::ToggleToolbar => self.toggle_toolbar(),
            MenuCmd::ReloadConfig => {
                self.reload_user_config();
            }
            MenuCmd::RestartService => self.restart_service(),
            MenuCmd::OpenSettings => self.open_settings(None),
            MenuCmd::OpenDictionary => self.open_settings(Some("dict")),
            MenuCmd::OpenAbout => self.open_settings(Some("about")),
            MenuCmd::TakeScreenshot => {
                if let Some(dir) = screenshots_dir() {
                    let _ = self.ui_tx.send(UiCommand::TakeScreenshot { dir });
                }
            }
            MenuCmd::ScreenshotCandidateToClipboard => {
                let _ = self.ui_tx.send(UiCommand::ScreenshotCandidateToClipboard);
            }
            MenuCmd::OpenConfigDir => self.open_dir(Config::user_config_dir()),
            MenuCmd::OpenAppDir => self.open_dir(
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.to_path_buf())),
            ),
            MenuCmd::OpenLogDir => self.open_dir(Config::log_dir()),
        }
    }

    /// 在文件管理器中打开目录（高级菜单「打开…目录」共用）。
    /// 目录可能尚未创建（如日志目录在首条日志前不存在），先 best-effort 建目录，
    /// 否则资源管理器会弹「找不到路径」。
    fn open_dir(&self, dir: Option<std::path::PathBuf>) {
        let Some(d) = dir else {
            tracing::warn!("open_dir: 目录不可用");
            return;
        };
        let _ = std::fs::create_dir_all(&d);
        let _ = self
            .ui_tx
            .send(UiCommand::OpenPath(d.display().to_string()));
    }

    /// 统一的「打开设置」入口：优先启动同目录的 wind_setting 桌面应用并跳转到指定页
    /// （`--page <name>`，name 为 wind_setting cli 的规范页 id：
    /// schema/input/keys/ui/dict/advanced/about，旧 web 别名如 dictionary 不被识别）；
    /// 找不到桌面应用再回退到内嵌 web 配置（签发 token 构造 URL，page 以 `#<name>` 片段附加）。
    /// page=None 打开默认页。设置/词库管理/关于等菜单项统一经此函数。
    ///
    /// 执行路径：有 TSF 连接时经 IPC 让宿主进程执行 ShellExecuteW（有前台权限，能拉窗口到前面）；
    /// 无 TSF 连接时回退到服务进程侧直接启动。
    pub(crate) fn open_settings(&self, page: Option<&str>) {
        // macOS：经 CmdOpenSettings(0x0507) 让 .app 用 LaunchServices 按 bundleID 启动/激活
        // 设置应用（app 侧 ModeStatusController.openSettings 已实现）。settings_app_path 拼 .exe，
        // macOS 恒为 None，旧路径会误落到已废弃的 web 分支并 WARN 失败，故此处直接短路。
        #[cfg(target_os = "macos")]
        {
            let encoded = wind_ipc::codec::encode_open_settings(page.unwrap_or(""));
            self.push_server.push_to_active(&encoded);
            return;
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(app) = crate::coordinator::settings_app_path() {
            let args = page.map(|p| format!("--page {p}")).unwrap_or_default();
            if self.push_server.has_clients() {
                self.push_shell_exec(&app, &args);
            } else {
                let _ = self.ui_tx.send(UiCommand::OpenApp { path: app, args });
            }
        } else if let Some(url) = crate::coordinator::settings_url() {
            let url = match page {
                Some(p) => format!("{url}#{p}"),
                None => url,
            };
            if self.push_server.has_clients() {
                self.push_shell_exec(&url, "");
            } else {
                let _ = self.ui_tx.send(UiCommand::OpenPath(url));
            }
        } else {
            tracing::warn!("打开设置失败：未找到 wind_setting 程序，web 服务也未就绪");
        }
    }

    /// 用户开关常驻工具栏（菜单）。仅翻转 toolbar_visible，显隐交 notify_toolbar
    /// 单点决策（结合 ime_active）。
    pub(crate) fn toggle_toolbar(&self) {
        let vis = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.toolbar_visible = !s.toolbar_visible;
            s.toolbar_visible
        };
        // 持久化到 config.ui.toolbar.visible(单一源:与设置页统一,reload 不会覆盖菜单选择)。
        let _ = Config::set_user_bool(&["ui", "toolbar", "visible"], vis);
        self.notify_toolbar();
    }

    /// 循环切换到下一个主题，重绘并持久化选择。
    /// 构建并显示功能主菜单（对齐 Go 统一菜单：方案/主题子菜单 + 勾选态）。
    /// x/y 为屏幕坐标；i32::MIN 表示由 UI 取光标位置。
    /// above=true：菜单在 (x,y) 上方弹出（工具栏触发，避免遮挡工具栏）；
    /// y_bottom 为工具栏底边，上方空间不足时改为从 y_bottom 向下弹出。
    pub(crate) fn show_main_menu(&self, x: i32, y: i32, y_bottom: i32, above: bool) {
        let items = self.build_main_menu_items();
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.menu_open = true;
            s.menu_target_page_local = 0;
            s.menu_target_text = String::new();
        }
        let _ = self.ui_tx.send(UiCommand::ShowCandidateMenu {
            items,
            x,
            y,
            y_bottom,
            above,
        });
    }

    /// macOS 精简功能菜单（IMK 输入源菜单 + 候选框右键空白菜单共用）。
    /// 相比 Windows 完整菜单，只保留必要项、且【无子菜单】（IMK 输入源菜单无法可靠处理嵌套子菜单）：
    ///   组1 输入方案（展开）：英文 + 各方案单选
    ///   组2 中文标点 / 全角 / 简入繁出
    ///   组3 重启服务
    ///   设置…
    /// 主题/工具栏/检索范围/重载配置/高级/词库/关于 移除（配置类交由设置应用）。
    #[cfg(target_os = "macos")]
    pub(crate) fn build_menu_items_macos(&self) -> Vec<wind_ui::manager::MenuItemSpec> {
        use wind_ui::manager::MenuItemSpec as M;
        let (chinese, punct, full, s2t) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.chinese_mode, s.chinese_punct, s.full_width, s.s2t_enabled)
        };
        let cmd = |c: MenuCmd| MenuKind::Command(c);
        let active = self.engine_mgr.active_schema_id();
        let schemas = self.engine_mgr.available_schemas().to_vec();

        let mut items = vec![M::leaf("英文", cmd(MenuCmd::SchemaEnglish), true, !chinese)];
        for (i, id) in schemas.iter().enumerate() {
            items.push(M::leaf(
                self.engine_mgr.schema_name(id),
                cmd(MenuCmd::SchemaSelect(i)),
                true,
                chinese && *id == active,
            ));
        }
        items.push(M::separator());
        items.push(M::leaf("中文标点", cmd(MenuCmd::TogglePunct), true, punct));
        items.push(M::leaf("全角", cmd(MenuCmd::ToggleWidth), true, full));
        items.push(M::leaf("简入繁出", cmd(MenuCmd::ToggleS2t), true, s2t));
        items.push(M::separator());
        items.push(M::leaf(
            "重启服务",
            cmd(MenuCmd::RestartService),
            true,
            false,
        ));
        items.push(M::separator());
        items.push(M::leaf("设置…", cmd(MenuCmd::OpenSettings), true, false));
        items
    }

    /// 构建功能主菜单项树（纯构建，不改状态/不弹窗）。
    /// Windows 经 `show_main_menu` 进程内渲染；macOS 经 `query_main_menu_encoded` 序列化下发给 `.app` 原生 NSMenu。
    pub(crate) fn build_main_menu_items(&self) -> Vec<wind_ui::manager::MenuItemSpec> {
        use wind_ui::manager::MenuItemSpec as M;
        let (chinese, punct, full, s2t, filter_mode, toolbar_vis) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.chinese_mode,
                s.chinese_punct,
                s.full_width,
                s.s2t_enabled,
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
                    self.engine_mgr.schema_name(id),
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
        let style = *self.theme_style.lock().unwrap_or_else(|e| e.into_inner());
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
        theme_children.push(M::leaf(
            "跟随系统",
            cmd(MenuCmd::ThemeStyle(0)),
            true,
            style == 0,
        ));
        theme_children.push(M::leaf(
            "亮色",
            cmd(MenuCmd::ThemeStyle(1)),
            true,
            style == 1,
        ));
        theme_children.push(M::leaf(
            "暗色",
            cmd(MenuCmd::ThemeStyle(2)),
            true,
            style == 2,
        ));

        // 检索范围子菜单：过滤模式单选
        let filter_children: Vec<_> = FILTER_MODES
            .iter()
            .enumerate()
            .map(|(i, (m, label))| {
                M::leaf(*label, cmd(MenuCmd::FilterMode(i)), true, filter_mode == *m)
            })
            .collect();

        // 高级子菜单：截图等不常用功能 + 打开各数据目录（分隔线独立成组）
        let advanced_children = vec![
            M::leaf(
                "截图所有窗口到文件",
                cmd(MenuCmd::TakeScreenshot),
                true,
                false,
            ),
            M::leaf(
                "截图候选窗口到剪贴板",
                cmd(MenuCmd::ScreenshotCandidateToClipboard),
                true,
                false,
            ),
            M::separator(),
            M::leaf("打开应用程序目录", cmd(MenuCmd::OpenAppDir), true, false),
            M::leaf("打开用户数据目录", cmd(MenuCmd::OpenConfigDir), true, false),
            M::leaf("打开日志目录", cmd(MenuCmd::OpenLogDir), true, false),
        ];

        let items = vec![
            M::submenu("输入方案", schema_children),
            M::leaf("全角", cmd(MenuCmd::ToggleWidth), true, full),
            M::leaf("中文标点", cmd(MenuCmd::TogglePunct), true, punct),
            M::leaf("简入繁出", cmd(MenuCmd::ToggleS2t), true, s2t),
            M::submenu("检索范围", filter_children),
            M::separator(),
            M::leaf("显示工具栏", cmd(MenuCmd::ToggleToolbar), true, toolbar_vis),
            M::submenu("主题", theme_children),
            M::separator(),
            M::leaf("重载配置", cmd(MenuCmd::ReloadConfig), true, false),
            M::leaf("重启服务", cmd(MenuCmd::RestartService), true, false),
            M::separator(),
            M::submenu("高级", advanced_children),
            M::separator(),
            M::leaf("词库管理...", cmd(MenuCmd::OpenDictionary), true, false),
            M::leaf("设置...", cmd(MenuCmd::OpenSettings), true, false),
            M::separator(),
            M::leaf(
                format!(
                    "关于 v{}{}",
                    env!("WIND_APP_VERSION"),
                    if wind_config::variant::is_dev() { " (Dev)" } else { "" }
                ),
                cmd(MenuCmd::OpenAbout),
                true,
                false,
            ),
        ];
        items
    }

    /// 把 `MenuItemSpec` 树映射为线格式 `MenuNode` 树（id 由 `MenuKind::to_menu_id` 派生）。
    #[cfg(target_os = "macos")]
    pub(crate) fn menu_items_to_nodes(
        items: &[wind_ui::manager::MenuItemSpec],
    ) -> Vec<wind_ipc::codec::MenuNode> {
        use wind_ui::manager::MenuKind;
        items
            .iter()
            .map(|it| wind_ipc::codec::MenuNode {
                id: it.kind.to_menu_id(),
                separator: matches!(it.kind, MenuKind::Separator),
                checked: it.checked,
                disabled: !it.enabled,
                label: it.label.clone(),
                children: Self::menu_items_to_nodes(&it.children),
            })
            .collect()
    }

    // macOS 用 IMK 原生菜单, 不走协调器弹出菜单键转发 (见 coordinator handle_key_event 门控)。
    #[cfg_attr(target_os = "macos", allow(dead_code))]
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
    #[cfg_attr(target_os = "macos", allow(dead_code))]
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
    /// 词条操作的启用态/删除文案按候选来源动态化（对齐 Go window_mouse 菜单状态规则）：
    /// - 置顶/前移：首项禁用；后移：末项禁用；拼音普通候选禁全部调位（无稳定位置语义）。
    /// - 删除：短语→「禁用短语」（软删可恢复）；用户词/临时词→真删；系统词→「隐藏候选」（shadow）。
    /// - overlay 模式（临拼/临英/特殊等，编码不在 input_buffer）：仅提供复制。
    pub(crate) fn show_candidate_menu(&self, page_local: usize, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let cand = state.candidates[idx].clone();
        let word = cand.text.clone();
        let code = state.input_buffer.clone();
        let total = state.candidates.len();
        let overlay = state.active.is_some();
        drop(state);

        let op = |o: CandidateOp| MenuKind::Op(o);
        // overlay 模式编码不走 input_buffer（临拼等各持独立缓冲），shadow/词库操作无处落键——
        // 仅保留复制，不再整个菜单拒开；调整/删除待各 overlay 语义定型后接入。
        let items = if overlay || code.is_empty() {
            vec![M::leaf("复制", MenuKind::Copy, true, false)]
        } else {
            let schema = self.engine_mgr.active_schema_id();
            let has_rule = self.shadow_has_rule(&schema, &code, &word);
            // 拼音普通候选禁调位：动态权重 + 衰减软置前与 pin 位置语义冲突；命令候选仍可调。
            let is_pinyin = matches!(
                self.engine_mgr.current_engine_type(),
                Some(wind_engine::EngineType::Pinyin)
            );
            let group_member = candidate_is_group_member(&cand);
            let movable = !(is_pinyin && !cand.is_command) && !group_member;
            let (delete_label, delete_enabled) = candidate_delete_menu(&cand);

            vec![
                M::leaf("置顶", op(CandidateOp::MoveTop), movable && idx > 0, false),
                M::leaf("前移", op(CandidateOp::MoveUp), movable && idx > 0, false),
                M::leaf(
                    "后移",
                    op(CandidateOp::MoveDown),
                    movable && idx + 1 < total,
                    false,
                ),
                M::leaf(delete_label, op(CandidateOp::Delete), delete_enabled, false),
                M::leaf("恢复默认", op(CandidateOp::Reset), has_rule, false),
                M::separator(),
                M::leaf("复制", MenuKind::Copy, true, false),
            ]
        };
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.menu_open = true;
            state.menu_target_page_local = page_local;
            state.menu_target_text = word;
        }
        // 候选右键菜单在光标处向下弹出（above=false，y_bottom 不使用）。
        let _ = self.ui_tx.send(UiCommand::ShowCandidateMenu {
            items,
            x,
            y,
            y_bottom: y,
            above: false,
        });
    }

    /// 读取当前光标所在显示器对应的工具栏位置。
    pub(crate) fn toolbar_pos_for_cursor(&self) -> Option<(i32, i32)> {
        let (cx, cy) = cursor_pos();
        let key = monitor_key_from_point(cx, cy);
        let map = self
            .toolbar_positions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.get(&key).copied()
    }

    /// 持久化工具栏位置（按光标所在显示器 key 独立存储，best-effort）。
    pub(crate) fn save_toolbar_pos(&self, x: i32, y: i32) {
        let (cx, cy) = cursor_pos();
        let key = monitor_key_from_point(cx, cy);
        {
            let mut map = self
                .toolbar_positions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            map.insert(key, (x, y));
        }
        if let Some(state_dir) = Config::state_dir() {
            let map = self
                .toolbar_positions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut rs = wind_config::RuntimeState::load(&state_dir);
            rs.toolbar_positions = map.clone();
            let _ = rs.save(&state_dir);
        }
    }

    /// 工具栏单元格点击：复用菜单命令切换状态（内部已推送 C++），再刷新工具栏显示。
    pub(crate) fn mouse_toolbar(&self, action: ToolbarAction) {
        match action {
            ToolbarAction::OpenSettings => {
                self.open_settings(None);
                return;
            }
            ToolbarAction::ToggleS2t => {
                self.handle_menu_command("toggle_s2t");
                self.notify_toolbar();
                return;
            }
            _ => {}
        }
        let cmd = match action {
            ToolbarAction::ToggleMode => "toggle_mode",
            ToolbarAction::SwitchEngine => "switch_engine",
            ToolbarAction::TogglePunct => "toggle_punct",
            ToolbarAction::ToggleWidth => "toggle_width",
            ToolbarAction::ToggleS2t | ToolbarAction::OpenSettings => unreachable!(),
        };
        self.handle_menu_command(cmd);
        self.notify_toolbar();
    }

    /// 焦点/激活切换路径专用：先用缓存值立即同步通知（无阻塞），
    /// 再后台刷新全屏缓存，若状态变化则再次通知。
    /// 保证 bridge handler 线程立即返回，缓存刷新在独立线程完成。
    /// 非焦点路径（模式切换/菜单操作）直接调 notify_toolbar()，缓存值仍然有效。
    pub(crate) fn notify_toolbar_async(&self) {
        // 立即用缓存值通知，bridge 线程无阻塞
        self.notify_toolbar();
        // hide_in_fullscreen 关闭时缓存永远为 false，无需后台刷新
        if !self.rt().config.ui.toolbar.hide_in_fullscreen {
            return;
        }
        let Some(weak) = self.self_weak.get().cloned() else {
            return;
        };
        std::thread::spawn(move || {
            let is_fs = crate::is_foreground_fullscreen();
            if let Some(c) = weak.upgrade() {
                let prev = c
                    .fullscreen_cached
                    .swap(is_fs, std::sync::atomic::Ordering::Relaxed);
                if prev != is_fs {
                    // 全屏态发生变化，用新值重新通知
                    c.notify_toolbar();
                }
            }
        });
    }

    /// 推送当前状态到常驻工具栏（中英/方案/标点/全半角）
    /// 工具栏可见性单点决策 + 内容刷新。对齐 Go toolbar_reducer 的合取公式：
    /// 仅当 `ime_active && toolbar_visible` 时显示（UpdateToolbar 会刷内容+定位+显示），
    /// 否则下发 HideToolbar。所有调用点（启动/切模式/切方案/激活/失活）经此单点决策，
    /// 不再各自直接显示，根治”工具栏总是显示、切走输入法不隐藏”。
    pub(crate) fn notify_toolbar(&self) {
        // 前台应用全屏时隐藏工具栏（读缓存，由 notify_toolbar_async 后台刷新，无阻塞）。
        let hide_fullscreen = self.rt().config.ui.toolbar.hide_in_fullscreen
            && self
                .fullscreen_cached
                .load(std::sync::atomic::Ordering::Relaxed);
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !(s.ime_active && s.toolbar_visible) || hide_fullscreen {
            drop(s);
            let _ = self.ui_tx.send(UiCommand::HideToolbar);
            return;
        }
        let (chinese_mode, caps_lock) = (s.chinese_mode, s.caps_lock);
        drop(s);
        // 有效中文：中文模式且大写锁定未开（对齐 Go effectiveChinese）。
        let effective_chinese = chinese_mode && !caps_lock;
        let icon_label = if effective_chinese {
            let id = self.engine_mgr.active_schema_id();
            let lbl = self.engine_mgr.schema_icon_label(&id);
            if lbl.is_empty() {
                "中".to_string()
            } else {
                lbl
            }
        } else if caps_lock {
            "A".to_string()
        } else {
            "英".to_string()
        };
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let tb = ToolbarState {
            chinese_mode,
            icon_label,
            caps_lock,
            full_width: s.full_width,
            chinese_punct: s.chinese_punct,
            s2t_enabled: s.s2t_enabled,
            // 简繁格：已启用时才在工具栏显示（默认 false 不显示）
            s2t_shown: s.s2t_enabled,
        };
        drop(s);
        let _ = self.ui_tx.send(UiCommand::UpdateToolbar(tb));
    }
}

/// 是否 $SS/$AA 展开后的组成员候选：顺序/成员由短语定义决定，禁一切调整
/// （改动走编辑短语路径，不允许 shadow 双轨漂移；对齐 Go isGroupMember 规则）。
/// 组导航候选本身（is_group，text 是组名）不算成员：可禁用整组。
pub(crate) fn candidate_is_group_member(cand: &wind_candidate::Candidate) -> bool {
    cand.is_phrase
        && !cand.is_group
        && (cand.phrase_template.starts_with("$SS") || cand.phrase_template.starts_with("$AA"))
}

/// 右键「删除」菜单项的动态文案与可用性（按候选来源，对齐 Go computeDeleteMenuLabel）：
/// 短语→禁用短语（软删可恢复）；用户词/临时词→真删；系统词→shadow 隐藏。
/// 单字同样允许隐藏（旧版的单字保护已取消：shadow 按 code+word 键控，只隐藏该编码下的
/// 该字，其它编码仍可打出，且设置页可恢复，不存在"某字彻底打不出"）。
/// Windows 菜单构建与 macOS 禁用位推送共用，避免两处规则漂移。
pub(crate) fn candidate_delete_menu(cand: &wind_candidate::Candidate) -> (&'static str, bool) {
    if candidate_is_group_member(cand) {
        ("删除词条", false)
    } else if cand.is_phrase {
        // 静态短语前缀命中（is_prefix 且无完整码）定位不到 store 记录 → 暂禁。
        ("禁用短语", !cand.is_prefix || !cand.group_code.is_empty())
    } else if cand.meta.is_user_dict {
        ("删除用户词", true)
    } else if cand.meta.is_temp_dict {
        ("删除临时词", true)
    } else {
        // 系统词（码表/拼音）：shadow 软隐藏。
        ("隐藏候选", true)
    }
}

/// 返回当前鼠标光标的屏幕坐标；获取失败时返回 (0, 0)。
fn cursor_pos() -> (i32, i32) {
    #[cfg(target_os = "windows")]
    {
        use std::mem::zeroed;
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt: POINT = unsafe { zeroed() };
        unsafe {
            let _ = GetCursorPos(&mut pt);
        }
        (pt.x, pt.y)
    }
    #[cfg(not(target_os = "windows"))]
    {
        (0, 0)
    }
}

/// 根据屏幕坐标计算显示器 key（工作区右下角："workRight,workBottom"）。
/// 找不到显示器时返回 "0,0"（退化为单显示器情况下的无键状态）。
fn monitor_key_from_point(x: i32, y: i32) -> String {
    #[cfg(target_os = "windows")]
    {
        use std::mem::{size_of, zeroed};
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        unsafe {
            let pt = POINT { x, y };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmon, &mut mi).as_bool() {
                return format!("{},{}", mi.rcWork.right, mi.rcWork.bottom);
            }
        }
        "0,0".to_string()
    }
    #[cfg(not(target_os = "windows"))]
    {
        "0,0".to_string()
    }
}

/// 截图保存目录：用户配置目录下的 `screenshots/` 子目录。
/// 返回 None 表示无法确定用户目录（portable 模式但找不到 exe 路径等极罕见情况）。
fn screenshots_dir() -> Option<String> {
    Config::user_config_dir().map(|d| d.join("screenshots").display().to_string())
}
