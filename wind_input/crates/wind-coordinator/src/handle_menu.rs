//! 功能菜单与工具栏
//!
//! 主菜单 / 候选右键菜单的构建与分派、工具栏点击/刷新/位置持久化。
//! 从 coordinator.rs 拆出（同 crate 内 `impl Coordinator` 块，组织性重构，无逻辑变更）。

use crate::coordinator::{Coordinator, FILTER_MODES};
use crate::theme_style::ThemeStyle;
use wind_bridge::handler::MessageHandler;
use wind_config::Config;
use wind_keys::keymap;
use wind_ui::manager::{CandidateOp, MenuCmd, MenuKind, ToolbarAction, UiCommand};
use wind_ui::toolbar::ToolbarState;

/// 把 (键, 值) 列表拼成设置程序的附加参数串（`--k=v`，空格分隔）。值为空的项跳过
/// ——设置端把"传了空串"和"没传"当同一回事，少一个参数更省事。
///
/// 值含空白时加双引号：参数串最终经 `ShellExecuteW` 的 params 交给目标进程，由
/// `CommandLineToArgvW` 重新切分，不加引号的 `--text=你 好` 会被拆成两个 argv，
/// 设置端只收得到 `--text=你`。引号在切分时会被剥掉，故设置端拿到的仍是裸值。
pub(crate) fn build_settings_args(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (k, v) in pairs {
        if v.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        if v.contains(char::is_whitespace) {
            out.push_str(&format!("--{k}=\"{v}\""));
        } else {
            out.push_str(&format!("--{k}={v}"));
        }
    }
    out
}

/// 组装设置程序的完整命令行参数串。
///
/// `--page <p>` 与附加参数各自独立成段：附加参数**不依附于页**（`--dark` / `--soft`
/// 这类没有页也有意义），故 `page=None` 时仍原样带上，不能因为没页就丢掉。
/// macOS 走 IPC 裸串、无命令行概念，故仅非 macOS 使用。
#[cfg(not(target_os = "macos"))]
pub(crate) fn settings_cmdline(page: Option<&str>, extra: &str) -> String {
    let mut out = String::new();
    if let Some(p) = page {
        out.push_str("--page ");
        out.push_str(p);
    }
    if !extra.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(extra);
    }
    out
}

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
        // 派发完再解除 tooltip 抑制：Tooltip 截图命令必须先于本次解除被处理，
        // 否则 tooltip 会在截图前被隐藏。详见 clear_tooltip_menu_flag 的说明。
        self.clear_tooltip_menu_flag();
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
            MenuCmd::OpenDictionary => self.open_dictionary(),
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
            MenuCmd::ToggleInputDiagnostics => self.toggle_input_diag_hud(),
            MenuCmd::TogglePasswordSuppress => self.toggle_password_suppress(),
            MenuCmd::FirstShowMode(m) => self.set_first_show_mode(m),
            MenuCmd::StatusToggleAlways => self.status_toggle_always(),
            MenuCmd::StatusTogglePinned => self.status_toggle_pinned(),
            MenuCmd::StatusResetPosition => self.status_reset_position(),
            MenuCmd::StatusScreenshot => {
                if let Some(dir) = screenshots_dir() {
                    let _ = self.ui_tx.send(UiCommand::ScreenshotStatusTip {
                        dir: std::path::PathBuf::from(dir),
                    });
                }
            }
            MenuCmd::TooltipCopy => {
                let _ = self.ui_tx.send(UiCommand::CopyTooltipText);
            }
            MenuCmd::TooltipScreenshot => {
                if let Some(dir) = screenshots_dir() {
                    let _ = self.ui_tx.send(UiCommand::ScreenshotTooltip {
                        dir: std::path::PathBuf::from(dir),
                    });
                }
            }
        }
    }

    /// 状态提示气泡右键菜单「常驻显示」：在 always/temp 间翻转 display_mode 并立即生效。
    /// 变为 always 时立即以常驻方式显示一次当前状态；变为 temp 时立即隐藏。
    pub(crate) fn status_toggle_always(&self) {
        let now_always = !self
            .rt()
            .config
            .ui
            .status
            .display_mode
            .eq_ignore_ascii_case("always");
        let mode = if now_always { "always" } else { "temp" };
        let _ = Config::set_user_string(&["ui", "status", "display_mode"], mode);
        self.refresh_config_in_memory(|c| c.ui.status.display_mode = mode.to_string());
        if now_always {
            self.show_persistent_status_if_always();
        } else {
            self.hide_tip();
        }
    }

    /// 状态提示气泡右键菜单「恢复默认位置」：改回跟随光标，custom_x/y 归零。
    pub(crate) fn status_reset_position(&self) {
        let _ = Config::set_user_string(&["ui", "status", "position_mode"], "follow_caret");
        let _ = Config::set_user_value(&["ui", "status", "custom_x"], toml::Value::Integer(0));
        let _ = Config::set_user_value(&["ui", "status", "custom_y"], toml::Value::Integer(0));
        self.refresh_config_in_memory(|c| {
            c.ui.status.position_mode = "follow_caret".to_string();
            c.ui.status.custom_x = 0;
            c.ui.status.custom_y = 0;
        });
    }

    /// 拖动状态提示气泡释放后的落位处理——**是否持久化取决于当前模式**：
    ///
    /// - `fixed`（固定坐标）：写回 `custom_x/custom_y`，永久生效。
    /// - `follow_caret`（跟随光标）：**不落盘**。拖动只是把气泡临时挪开，
    ///   下次状态变化重新显示时自然回到光标旁——UI 侧仅在拖动进行中锁定位置，
    ///   松手后的 `show()` 会照常按光标重新定位，无需在此做任何清理。
    ///
    /// 这样两种模式各自语义自洽：跟随模式拖动是临时的，固定模式拖动才是"重新摆放"。
    pub(crate) fn save_status_tip_pos(&self, x: i32, y: i32) {
        if !self
            .rt()
            .config
            .ui
            .status
            .position_mode
            .eq_ignore_ascii_case("fixed")
        {
            return;
        }
        let _ = Config::set_user_value(
            &["ui", "status", "custom_x"],
            toml::Value::Integer(x as i64),
        );
        let _ = Config::set_user_value(
            &["ui", "status", "custom_y"],
            toml::Value::Integer(y as i64),
        );
        self.refresh_config_in_memory(|c| {
            c.ui.status.custom_x = x;
            c.ui.status.custom_y = y;
        });
    }

    /// 拖动候选窗释放后的落位处理——**是否持久化取决于当前定位方式**：
    ///
    /// - `fixed`（固定位置）：写回 `ui.candidate.custom_x/custom_y`，永久生效。
    /// - `follow_caret`（跟随光标）：**不落盘**。拖动只是把候选窗临时挪开，
    ///   本次组合内保持不动，组合结束（`hide()` → `reset_drag()`）即恢复跟随光标。
    ///
    /// 与 `save_status_tip_pos` 同构：两种模式各自语义自洽，跟随模式的拖动是临时的，
    /// 固定模式的拖动才是"重新摆放"。
    pub(crate) fn save_candidate_pos(&self, x: i32, y: i32) {
        if !self.rt().config.ui.candidate.is_fixed_position() {
            return;
        }
        let (x, y) = avoid_unset_sentinel(x, y);
        let _ = Config::set_user_value(
            &["ui", "candidate", "custom_x"],
            toml::Value::Integer(x as i64),
        );
        let _ = Config::set_user_value(
            &["ui", "candidate", "custom_y"],
            toml::Value::Integer(y as i64),
        );
        self.refresh_config_in_memory(|c| {
            c.ui.candidate.custom_x = x;
            c.ui.candidate.custom_y = y;
        });
    }

    /// 状态提示气泡右键菜单「固定位置」：在 fixed / follow_caret 间翻转。
    ///
    /// 打开时**以气泡当前实际位置**落盘，而不是直接切到陈旧的 custom_x/custom_y——
    /// 否则用户拖到某处后点「固定位置」，气泡会跳到上次保存的（往往是 0,0）坐标。
    /// 做法：先把模式改成 fixed，再请 UI 上报当前位置，回来的 `StatusTipMoved`
    /// 经 `save_status_tip_pos` 落盘（该函数只在 fixed 模式下持久化，此时条件已满足）。
    pub(crate) fn status_toggle_pinned(&self) {
        let now_fixed = !self
            .rt()
            .config
            .ui
            .status
            .position_mode
            .eq_ignore_ascii_case("fixed");
        let mode = if now_fixed { "fixed" } else { "follow_caret" };
        let _ = Config::set_user_string(&["ui", "status", "position_mode"], mode);
        self.refresh_config_in_memory(|c| c.ui.status.position_mode = mode.to_string());
        if now_fixed {
            let _ = self.ui_tx.send(UiCommand::ReportStatusTipPos);
        }
    }

    /// 右键状态提示气泡请求的功能菜单：常驻显示 / 固定位置（均带勾选）/ 恢复默认位置 / 截图。
    pub(crate) fn show_status_menu(&self, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let si_always;
        let si_fixed;
        {
            let si = &self.rt().config.ui.status;
            si_always = si.display_mode.eq_ignore_ascii_case("always");
            si_fixed = si.position_mode.eq_ignore_ascii_case("fixed");
        }
        // 菜单打开期间抑制气泡自动隐藏，否则临时模式下菜单还开着气泡就没了。
        let _ = self.ui_tx.send(UiCommand::SetStatusMenuOpen(true));
        let cmd = |c: MenuCmd| MenuKind::Command(c);
        let items = vec![
            M::leaf(
                "常驻显示",
                cmd(MenuCmd::StatusToggleAlways),
                true,
                si_always,
            ),
            M::leaf("固定位置", cmd(MenuCmd::StatusTogglePinned), true, si_fixed),
            M::leaf(
                "恢复默认位置",
                cmd(MenuCmd::StatusResetPosition),
                true,
                false,
            ),
            M::leaf("截图此窗口", cmd(MenuCmd::StatusScreenshot), true, false),
        ];
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
            y_bottom: y,
            above: false,
        });
    }

    /// 右键悬停提示（编码反查气泡）请求的功能菜单：复制内容 / 截图此窗口。
    /// **先**发 SetTooltipMenuOpen(true) 抑制 tooltip 的 WM_MOUSELEAVE 自动隐藏——
    /// 右键弹出菜单后鼠标会移到菜单窗口上，若不抑制 tooltip 会当场消失，菜单就指向一个
    /// 已不存在的窗口，「截图此窗口」会截空。抑制标志在菜单关闭时由 menu_close 统一清除。
    pub(crate) fn show_tooltip_menu(&self, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let _ = self.ui_tx.send(UiCommand::SetTooltipMenuOpen(true));
        let cmd = |c: MenuCmd| MenuKind::Command(c);
        let items = vec![
            M::leaf("复制内容", cmd(MenuCmd::TooltipCopy), true, false),
            M::leaf("截图此窗口", cmd(MenuCmd::TooltipScreenshot), true, false),
        ];
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
            y_bottom: y,
            above: false,
        });
    }

    /// 切换输入诊断 HUD 显隐（高级菜单）：开启时立即推送当前快照，关闭时下发隐藏。
    pub(crate) fn toggle_input_diag_hud(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = !self.input_diag_hud_visible.load(Relaxed);
        self.input_diag_hud_visible.store(now, Relaxed);
        if now {
            self.push_input_diag_hud_if_visible();
        } else {
            let _ = self.ui_tx.send(UiCommand::HideInputDiag);
        }
    }

    /// 切换密码框强制英文抑制策略（高级菜单，临时测试入口）：关闭时立即解除当前生效的强制英文。
    pub(crate) fn toggle_password_suppress(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let now = !self.password_suppress_enabled.load(Relaxed);
        self.password_suppress_enabled.store(now, Relaxed);
        if !now {
            self.password_suppress.store(false, Relaxed);
        }
        // 同步给 DLL：吃键门控在 TSF 侧本地判定（早于 IPC），不推则开关对 DLL 无效——
        // 关掉抑制后 DLL 仍会放行所有键，这个「误置位时用来救场」的逃生阀就成了摆设。
        self.push_password_suppress_config(0);
    }

    /// 当前焦点进程名（小写，取自 `pid_names` 缓存）。未解析出进程时返回空串。
    pub(crate) fn active_process_name(&self) -> String {
        let pid = self
            .active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pid;
        if pid == 0 {
            return String::new();
        }
        self.pid_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }

    /// 为当前焦点应用设置候选窗首显策略，并写入用户层 compat.toml。
    ///
    /// 三步收口，缺一不可：
    ///   1. 写用户层 compat.toml（持久化，跨重启保留）；
    ///   2. **重载规则表**——只改运行时缓存不够，切到别的应用再切回来时
    ///      `update_active_compat` 会拿这张表重新解析，旧表会把本次设置悄悄回滚；
    ///   3. 刷新当前 `active_compat` 缓存，使本次设置对当前应用立即生效
    ///      （同 pid 时 `update_active_compat` 提前 return，不会自己刷）。
    pub(crate) fn set_first_show_mode(&self, mode_id: u8) {
        use wind_config::app_compat::FirstShowMode;
        let mode = match mode_id {
            1 => FirstShowMode::Fast,
            2 => FirstShowMode::Instant,
            _ => FirstShowMode::Wait,
        };
        let name = self.active_process_name();
        if name.is_empty() {
            // 焦点进程未解析（尚无焦点 / OpenProcess 失败）。菜单项此时应是禁用态，
            // 走到这里说明有别的路径调用，记一条便于排查——静默返回会让用户以为点了没反应。
            tracing::warn!("set_first_show_mode: 当前焦点进程未知，忽略本次设置");
            return;
        }
        let Some(user_dir) = self.compat_dirs.1.clone() else {
            tracing::warn!("set_first_show_mode: 无用户配置目录，无法持久化");
            return;
        };
        if let Err(e) = wind_config::app_compat::set_user_first_show_mode(&user_dir, &name, mode) {
            tracing::error!("set_first_show_mode: 写用户 compat.toml 失败: {e}");
            return;
        }
        // 2）重载整表（系统层 + 用户层），与启动时同一口径。
        let reloaded = wind_config::app_compat::AppCompat::load(
            self.compat_dirs.0.as_deref(),
            Some(user_dir.as_path()),
        );
        *self.app_compat.lock().unwrap_or_else(|e| e.into_inner()) = reloaded;
        // 3）当前应用立即生效。
        self.active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_show_mode = mode;
        tracing::info!("候选窗首显策略 for process={name}: {}", mode.as_config());
        self.show_status();
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
        self.open_settings_with(page, "");
    }

    /// 带附加参数的「打开设置」。`extra` 是**原样直通**给设置程序的命令行参数串
    /// （如 `--schema=wubi86 --type=shadow`），空串=无附加参数。
    ///
    /// 刻意不解析 `extra`：设置端每加一个参数就要同步改一遍宿主，才是真正难维护的。
    /// 宿主只负责拼接与投递，取值合法性由设置端自己判断（它会降级并提示，不会崩）。
    /// 内部调用方请用 [`build_settings_args`] 构造，含空白的值会被正确加引号。
    pub(crate) fn open_settings_with(&self, page: Option<&str>, extra: &str) {
        #[cfg(not(target_os = "macos"))]
        let args = settings_cmdline(page, extra);

        // macOS：经 CmdOpenSettings(0x0507) 让 .app 用 LaunchServices 按 bundleID 启动/激活
        // 设置应用（app 侧 ModeStatusController.openSettings 已实现）。settings_app_path 拼 .exe，
        // macOS 恒为 None，旧路径会误落到已废弃的 web 分支并 WARN 失败，故此处直接短路。
        // payload 沿用「页名后接参数」的裸串形态（既有 add-word 路径就是这样传的），
        // Swift 侧解析方式不变。
        #[cfg(target_os = "macos")]
        {
            let target = match (page, extra.is_empty()) {
                (Some(p), false) => format!("{p} {extra}"),
                (Some(p), true) => p.to_string(),
                (None, false) => extra.to_string(),
                (None, true) => String::new(),
            };
            let encoded = wind_ipc::codec::encode_open_settings(&target);
            self.push_server.push_to_active(&encoded);
            return;
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(app) = crate::coordinator::settings_app_path() {
            if self.push_server.has_clients() {
                self.push_shell_exec(&app, &args);
            } else {
                let _ = self.ui_tx.send(UiCommand::OpenApp { path: app, args });
            }
        } else if let Some(url) = crate::coordinator::settings_url() {
            // web 回退没有命令行概念：只带页锚点，附加参数丢弃（页仍能到位）。
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

    /// 菜单「词库管理」：直接落到当前正在用的方案域，而不是默认的快捷短语域。
    /// 用户从输入法菜单进词库，十有八九是要管当前这套方案的词。
    /// 方案 id 取不到时退化为不带参数，行为与从前一致。
    pub(crate) fn open_dictionary(&self) {
        let schema = self.engine_mgr.active_schema_id();
        self.open_settings_with(Some("dict"), &build_settings_args(&[("schema", &schema)]));
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
        // 内存 config 同步跟上（同 status_toggle_always）：落盘与内存不同步时，下一次
        // 未经重载的读取会拿到陈旧值。
        self.refresh_config_in_memory(|c| c.ui.toolbar.visible = vis);
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
        for s in [ThemeStyle::System, ThemeStyle::Light, ThemeStyle::Dark] {
            theme_children.push(M::leaf(
                s.label(),
                cmd(MenuCmd::ThemeStyle(s.as_menu_id())),
                true,
                style == s,
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
            M::separator(),
            // 候选窗首显策略（per-app，写用户层 compat.toml）。三档**互斥**，做成子菜单单选：
            // 布尔开关时代它们能同时打开，实测就因此出过「fast 配了却从未生效」——instant
            // 抢先放行，fast 的判据根本没机会跑。互斥语义现在由类型与 UI 双重保证。
            // 进程名未解析时整个子菜单禁用而非隐藏，菜单项位置保持稳定。
            {
                let proc = self.active_process_name();
                let cur = self
                    .active_compat
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .first_show_mode;
                use wind_config::app_compat::FirstShowMode as F;
                let enabled = !proc.is_empty();
                let label = if enabled {
                    format!("候选窗首显（{proc}）")
                } else {
                    "候选窗首显（当前应用未知）".to_string()
                };
                M::submenu(
                    label,
                    vec![
                        M::leaf(
                            "等待精确坐标（默认）",
                            cmd(MenuCmd::FirstShowMode(0)),
                            enabled,
                            cur == F::Wait,
                        ),
                        M::leaf(
                            "快速显示",
                            cmd(MenuCmd::FirstShowMode(1)),
                            enabled,
                            cur == F::Fast,
                        ),
                        M::leaf(
                            "立即显示（最快，可能抖动）",
                            cmd(MenuCmd::FirstShowMode(2)),
                            enabled,
                            cur == F::Instant,
                        ),
                    ],
                )
            },
            M::separator(),
            M::leaf(
                "输入诊断 HUD",
                cmd(MenuCmd::ToggleInputDiagnostics),
                true,
                self.input_diag_hud_visible
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            M::leaf(
                "密码框强制英文",
                cmd(MenuCmd::TogglePasswordSuppress),
                true,
                self.password_suppress_enabled
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
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
                    if wind_config::variant::is_dev() {
                        " (Dev)"
                    } else {
                        ""
                    }
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

    /// 关闭菜单。单点收口：所有菜单关闭路径（ESC/点击外部/动作执行完毕）都经此函数，
    /// 顺带清除 tooltip 右键菜单的 suppress_hide 抑制标志——不区分是否为 tooltip 菜单，
    /// 非 tooltip 菜单关闭时清除是无操作（tooltip 菜单未打开则标志本就是 false）。
    pub(crate) fn menu_close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.menu_open {
            state.menu_open = false;
            drop(state);
            let _ = self.ui_tx.send(UiCommand::HideMenu);
        }
    }

    /// 解除 Tooltip 的「菜单打开中」隐藏抑制。
    ///
    /// **必须在菜单动作派发之后调用，不能并进 `menu_close()`**：`menu_action()` 是先
    /// `menu_close()` 再 `run_menu_cmd()`，若在前者里解除，UI 线程会按序先处理解除
    /// （此时光标在菜单窗口上、不在 tooltip 上 → 立即隐藏 tooltip），再处理
    /// `ScreenshotTooltip`，于是截图恒定失败在「未显示」上。复制不受影响（文本已留存），
    /// 表现为「复制能用、截图不能用」。
    pub(crate) fn clear_tooltip_menu_flag(&self) {
        let _ = self.ui_tx.send(UiCommand::SetTooltipMenuOpen(false));
        // 状态气泡同理：菜单关掉后恢复自动隐藏计时。这里解除是安全的——它只影响
        // 隐藏抑制，不像 tooltip 那样会立即隐藏窗口，故不受"截图命令尚未处理"的时序制约。
        let _ = self.ui_tx.send(UiCommand::SetStatusMenuOpen(false));
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
            let cand_id = (!cand.id.is_empty()).then(|| cand.id.as_str());
            let has_rule = self.shadow_has_rule(&schema, &code, &word, cand_id);
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
        // 单飞：已有探测在途就跳过。探的是**同一个**全局前台状态，重复查没有意义，
        // 而焦点变化是成串来的（一次应用切换会连着触发多次），此前每次都 spawn 一个线程。
        //
        // 这里不并入 first-show 那个共享定时器：is_foreground_fullscreen 会阻塞
        // （异步化它正是 1abab9f 的目的），塞进定时器线程会拖垮兜底时限。
        if self
            .fullscreen_probing
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("fullscreen-probe".into())
            .spawn(move || {
                let is_fs = crate::is_foreground_fullscreen();
                if let Some(c) = weak.upgrade() {
                    let prev = c
                        .fullscreen_cached
                        .swap(is_fs, std::sync::atomic::Ordering::Relaxed);
                    c.fullscreen_probing
                        .store(false, std::sync::atomic::Ordering::Release);
                    if prev != is_fs {
                        // 全屏态发生变化，用新值重新通知
                        c.notify_toolbar();
                    }
                }
            });
        if spawned.is_err() {
            // 线程没起来就得把闸放回去，否则此后永远不再探测
            self.fullscreen_probing
                .store(false, std::sync::atomic::Ordering::Release);
        }
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
        // 四项合取：本输入法在服务某宿主（ime_active）、焦点在可编辑控件里
        // （has_edit_context）、用户开着工具栏（toolbar_visible）、且未处于全屏。
        // 前两项正交且缺一不可——只看 ime_active 会让应用内点到非文本框时工具栏不隐藏。
        if !(s.ime_active && s.has_edit_context && s.toolbar_visible) || hide_fullscreen {
            // 记录是哪一项否决了显示：UI 层日志只看得到「HideToolbar」，判不出成因，
            // 而四条路径的排查方向完全不同（激活态乱序 / 焦点离开输入框 / 用户关了开关 /
            // 全屏探测）。
            tracing::debug!(
                "notify_toolbar: 隐藏 ime_active={} has_edit_ctx={} toolbar_visible={} fullscreen={}",
                s.ime_active,
                s.has_edit_context,
                s.toolbar_visible,
                hide_fullscreen
            );
            drop(s);
            let _ = self.ui_tx.send(UiCommand::HideToolbar);
            self.push_input_diag_hud_if_visible(); // 见函数末尾同一行的说明
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
        // HUD 刷新收口于此（两个出口各一次）。诊断 HUD 展示的 ime_active /
        // has_edit_context 正是上面那道合取的输入，而**凡是改动它们的路径都必须调
        // notify_toolbar 才能生效**，所以这里是唯一不会漏的落点。
        // 反例（2026-07-26 实测）：起初只在 apply_input_diag 里推，于是 focus_gained
        // 之外的路径（CtxLost 等）改了状态却不刷新，HUD 一直显示上一次的快照。
        // 在此调用是安全的：state 锁已 drop，且 HUD 关闭时该函数首行即返回，零开销。
        self.push_input_diag_hud_if_visible();
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

/// 候选窗固定位置落盘前的哨兵规避。
///
/// UI 侧用 `(0, 0)` 表示"已开启固定但尚未设定位置"（落到屏幕默认锚点），可主屏工作区
/// 的左上角**往往正是** `(0, 0)`（任务栏在底部时）——用户真把候选窗拖到屏幕最左上角，
/// 落盘值就撞上哨兵，下次显示被判为"没设过"而跳回默认锚点，表现为"位置没被记住"。
///
/// 哨兵值与合法值域重叠是根因；这里在落盘侧下移 1px 避开：视觉不可察觉，语义无歧义。
fn avoid_unset_sentinel(x: i32, y: i32) -> (i32, i32) {
    if (x, y) == (0, 0) { (0, 1) } else { (x, y) }
}

#[cfg(test)]
mod tests {
    use super::avoid_unset_sentinel;

    /// 只有恰好 (0,0) 被规避，其余坐标（含含 0 分量与负坐标）必须原样落盘。
    #[test]
    fn only_the_exact_sentinel_is_nudged() {
        assert_eq!(avoid_unset_sentinel(0, 0), (0, 1), "撞哨兵 → 下移 1px");
        // 含 0 分量但非哨兵：不能动，否则用户贴左边/贴顶边的位置会被悄悄改掉
        assert_eq!(avoid_unset_sentinel(0, 5), (0, 5));
        assert_eq!(avoid_unset_sentinel(5, 0), (5, 0));
        // 负坐标：副屏位于主屏左侧/上方时屏幕坐标为负，属合法值
        assert_eq!(avoid_unset_sentinel(-1920, -100), (-1920, -100));
        assert_eq!(avoid_unset_sentinel(100, 200), (100, 200));
    }

    /// 规避结果自身绝不能再是哨兵，否则等于没修。
    #[test]
    fn nudged_result_is_never_the_sentinel() {
        assert_ne!(avoid_unset_sentinel(0, 0), (0, 0));
    }

    #[test]
    fn settings_args_skip_empty_and_quote_whitespace() {
        use super::build_settings_args;
        assert_eq!(build_settings_args(&[]), "");
        assert_eq!(build_settings_args(&[("schema", "")]), "", "空值整项跳过");
        assert_eq!(
            build_settings_args(&[("schema", "wubi86"), ("type", "shadow")]),
            "--schema=wubi86 --type=shadow"
        );
        assert_eq!(
            build_settings_args(&[("text", "a b")]),
            "--text=\"a b\"",
            "含空白必须加引号，否则会被 CommandLineToArgvW 拆成两个 argv"
        );
    }

    /// 附加参数不依附于页：没给页也要原样带上（`--dark`/`--soft` 无页也有意义）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn settings_cmdline_keeps_extra_without_page() {
        use super::settings_cmdline;
        assert_eq!(settings_cmdline(None, ""), "");
        assert_eq!(settings_cmdline(Some("dict"), ""), "--page dict");
        assert_eq!(
            settings_cmdline(Some("dict"), "--schema=wubi86 --type=shadow"),
            "--page dict --schema=wubi86 --type=shadow"
        );
        assert_eq!(
            settings_cmdline(None, "--dark"),
            "--dark",
            "无页时附加参数不得被丢弃"
        );
    }
}
