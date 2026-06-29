//! UI 管理器 + 消息循环
//!
//! 与 Go 版本 `wind_input/internal/ui/manager.go` 对齐。
//! 在独立线程中运行 Win32 消息循环，通过通道接收 UI 更新命令。

use crate::candidate_window::{CandidateItem, CandidateWindow, CandidateWindowConfig};
use crate::toast::{ToastKind, ToastPosition};
use std::sync::mpsc;
use tracing::{debug, error, info};
#[cfg(windows)]
use windows::Win32::Foundation::HWND;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::*;

/// UI 命令
#[derive(Debug)]
pub enum UiCommand {
    /// 更新候选列表
    UpdateCandidates {
        preedit: String,
        /// 模式指示文本（拼/双/快/英/符 或全称）；空=不显示。有 preedit 空间时随候选窗持久显示。
        mode_label: String,
        candidates: Vec<CandidateItem>,
        /// 键盘选中项（页内下标），空格上屏目标
        selected: usize,
        /// 鼠标悬停项（页内下标），-1 表示无；与 selected 独立
        hover: i32,
        /// 当前页（1 起）
        page: usize,
        /// 总页数（含动态加载估计）
        total_pages: usize,
        caret_x: i32,
        caret_y: i32,
        /// 光标高度（用于上翻时定位到光标上方）
        caret_height: i32,
        /// 光标坐标是否有效（无效时窗口仅临时显示、不锁定锚点）
        caret_valid: bool,
    },
    /// 隐藏候选窗口
    HideCandidates,
    /// 一次性通知 toast（方案切换/词库就绪/错误等）；duration_ms 后自动隐藏。
    ShowToast {
        text: String,
        position: ToastPosition,
        kind: ToastKind,
        duration_ms: u64,
    },
    /// 显示状态提示气泡（中英/标点/全半角/方案切换），约 1 秒后自动隐藏。
    /// (x,y)=光标点(y 为底端)，caret_height 上翻定位用，offset_x/y 用户位置微调。
    ShowStatusTip {
        text: String,
        x: i32,
        y: i32,
        caret_height: i32,
        offset_x: i32,
        offset_y: i32,
        /// 自动隐藏时长（毫秒）；0=常驻不自动隐藏（display_mode=always）。
        duration_ms: u64,
        /// 固定位置模式（position_mode=fixed）：用 fixed_x/fixed_y 作屏幕坐标，忽略光标。
        fixed: bool,
        fixed_x: i32,
        fixed_y: i32,
    },
    /// 隐藏状态提示气泡（常驻模式失焦/切走输入法时）。
    HideStatusTip,
    /// 更新常驻工具栏状态（中英/方案/标点/全半角）
    UpdateToolbar(crate::toolbar::ToolbarState),
    /// 隐藏工具栏
    HideToolbar,
    /// 设置工具栏位置（启动时恢复持久化位置）
    SetToolbarPos { x: i32, y: i32 },
    /// 应用主题（协调器加载解析后下发）
    SetTheme(Box<wind_theme::Resolved>),
    /// 候选布局方向（true=竖排）。来自 ui.candidate.layout。
    SetCandidateLayout(bool),
    /// 预编辑嵌入模式（true=编码嵌入候选行首，不显示独立 preedit 条）。
    /// 来自 ui.candidate.preedit_display == "candidate_inline"。
    SetPreeditEmbedded(bool),
    /// 候选字号覆盖（0=跟随主题）。来自 ui.candidate.font_size。
    SetCandidateFontSize(f32),
    /// 候选字体族（空=默认）。来自 ui.font.family。
    SetCandidateFontFamily(String),
    /// 悬停提示激活延迟（毫秒）。来自 ui.tooltip.delay。
    SetTooltipDelay(i32),
    /// 候选窗在光标上方时反转候选顺序。来自 ui.candidate.flip_when_above。
    SetCandidateFlipWhenAbove(bool),
    /// 翻页栏显示覆盖（""跟随主题/"hide"/"auto"/"always"）。来自 ui.candidate.pager_bar_display。
    SetPagerDisplay(String),
    /// 页码文字显示覆盖（""跟随主题/"show"/"hide"）。来自 ui.candidate.page_number_display。
    SetPageNumberDisplay(String),
    /// 拆字字根字体路径（PUA 字根字符渲染）。空=不设。
    SetTooltipChaiziFont(String),
    /// 显示菜单（候选右键菜单 / 功能主菜单；UI 自管导航与子菜单）。
    /// above=true：菜单底边对齐 (x,y) 向上展开（工具栏菜单用，避免遮挡工具栏）。
    ShowCandidateMenu {
        items: Vec<MenuItemSpec>,
        x: i32,
        y: i32,
        above: bool,
    },
    /// 转发键给打开的菜单（方向键/回车/ESC/空格）；菜单窗无焦点，键由协调器转发
    MenuKey(u32),
    /// 隐藏菜单
    HideMenu,
    /// 写剪贴板（菜单"复制"由协调器驱动 → UI 侧执行）
    CopyToClipboard(String),
    /// 用资源管理器打开路径（菜单"打开配置目录"）
    OpenPath(String),
    /// 启动应用程序并传参（如 wind_setting.exe `--page dictionary`）。
    OpenApp { path: String, args: String },
    /// 截图所有可见 UI 窗口，保存到 dir 目录（由协调器根据 config 确定）。
    TakeScreenshot { dir: String },
    /// 将候选窗口截图复制到剪贴板（候选不可见则提示）。
    ScreenshotCandidateToClipboard,
    /// 关闭 UI
    Shutdown,
}

/// 翻页器命中/悬停 tag（远高于候选下标，避免冲突）
pub const HOVER_PAGE_PREV: i32 = 100_000;
pub const HOVER_PAGE_NEXT: i32 = 100_001;

/// 工具栏单元格动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    /// 中/英切换（合并方案显示）
    ToggleMode,
    /// 切换输入方案（保留供外部调用，工具栏不单独显示）
    SwitchEngine,
    /// 中/英标点切换
    TogglePunct,
    /// 全/半角切换
    ToggleWidth,
    /// 简/繁转换切换
    ToggleS2t,
    /// 打开设置
    OpenSettings,
}

/// 候选词条操作（右键菜单）；复制由 UI 侧直接处理，不在此列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOp {
    /// 置顶
    MoveTop,
    /// 前移
    MoveUp,
    /// 后移
    MoveDown,
    /// 删除（屏蔽）
    Delete,
    /// 恢复默认
    Reset,
}

/// 功能主菜单命令（对齐 Go 统一菜单）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCmd {
    /// 切到英文模式
    SchemaEnglish,
    /// 选择第 N 个输入方案
    SchemaSelect(usize),
    /// 中/英标点切换
    TogglePunct,
    /// 全/半角切换
    ToggleWidth,
    /// 简繁转换开关
    ToggleS2t,
    /// 检索范围过滤（0 智能/1 常用字/2 全部字符）
    FilterMode(usize),
    /// 选择第 N 个主题
    ThemeSelect(usize),
    /// 主题明暗（0 跟随/1 亮/2 暗）
    ThemeStyle(u8),
    /// 显示/隐藏工具栏
    ToggleToolbar,
    /// 重载配置
    ReloadConfig,
    /// 重启服务进程
    RestartService,
    /// 打开配置目录
    OpenConfigDir,
    /// 词库管理（暂兜底为打开配置目录）
    OpenDictionary,
    /// 设置（暂兜底为打开配置目录）
    OpenSettings,
    /// 关于（暂兜底）
    OpenAbout,
    /// 截图所有可见 UI 窗口到文件（高级菜单）
    TakeScreenshot,
    /// 截图候选窗口到剪贴板（高级菜单）
    ScreenshotCandidateToClipboard,
}

/// 菜单项的动作类型（右键候选菜单 + 功能主菜单共用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// 词条操作（置顶/移动/删除/恢复）
    Op(CandidateOp),
    /// 复制候选文本（UI 侧写剪贴板）
    Copy,
    /// 功能主菜单命令
    Command(MenuCmd),
    /// 子菜单父项（点击/回车进入 children）
    Submenu,
    /// 分隔线（不可点击）
    Separator,
}

/// 菜单项规格（由协调器构建）。支持勾选态与子菜单。
#[derive(Debug, Clone)]
pub struct MenuItemSpec {
    pub label: String,
    pub kind: MenuKind,
    pub enabled: bool,
    /// 勾选标记（当前方案/主题/开关态）
    pub checked: bool,
    /// 子菜单项（kind=Submenu 时有效）
    pub children: Vec<MenuItemSpec>,
}

impl MenuItemSpec {
    pub fn leaf(label: impl Into<String>, kind: MenuKind, enabled: bool, checked: bool) -> Self {
        Self {
            label: label.into(),
            kind,
            enabled,
            checked,
            children: Vec::new(),
        }
    }
    pub fn separator() -> Self {
        Self {
            label: String::new(),
            kind: MenuKind::Separator,
            enabled: false,
            checked: false,
            children: Vec::new(),
        }
    }
    pub fn submenu(label: impl Into<String>, children: Vec<MenuItemSpec>) -> Self {
        Self {
            label: label.into(),
            kind: MenuKind::Submenu,
            enabled: true,
            checked: false,
            children,
        }
    }
}

/// UI → 协调器的反向事件（鼠标交互）
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// 点击选中当前页内第 N 个候选（0 起）
    CandidateSelect(usize),
    /// 滚轮翻页：>0 下一页，<0 上一页
    Page(i32),
    /// 悬停到页内候选下标（-1 表示离开）
    Hover(i32),
    /// 工具栏单元格点击
    Toolbar(ToolbarAction),
    /// 工具栏被拖动到新位置（屏幕坐标），供协调器持久化
    ToolbarMoved { x: i32, y: i32 },
    /// 候选词条操作（页内下标 + 动作）
    CandidateOp { op: CandidateOp, page_local: usize },
    /// 右键候选请求弹出菜单（页内下标 + 屏幕坐标）；协调器据此构建菜单项回送
    RequestCandidateMenu { page_local: usize, x: i32, y: i32 },
    /// 请求功能主菜单（屏幕坐标）；来自候选窗空白/工具栏右键或设置键。
    /// above=true：菜单在 (x,y) 上方弹出（工具栏触发，避免遮挡工具栏）。
    RequestMainMenu { x: i32, y: i32, above: bool },
    /// 菜单项激活（携带动作）：UI 自管导航/子菜单，仅把最终动作回送协调器
    MenuAction(MenuKind),
    /// 关闭菜单（点击菜单外 / ESC / 右键）
    MenuClose,
}

/// UI 管理器（在独立线程中运行）
pub struct UiManager {
    cmd_tx: mpsc::Sender<UiCommand>,
    event_rx: Option<mpsc::Receiver<UiEvent>>,
    _thread: std::thread::JoinHandle<()>,
}

impl UiManager {
    pub fn new() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<UiCommand>();
        let (ev_tx, ev_rx) = mpsc::channel::<UiEvent>();

        let thread = std::thread::Builder::new()
            .name("ui-manager".into())
            .spawn(move || {
                Self::ui_thread(rx, ev_tx);
            })?;

        Ok(Self {
            cmd_tx: tx,
            event_rx: Some(ev_rx),
            _thread: thread,
        })
    }

    pub fn sender(&self) -> mpsc::Sender<UiCommand> {
        self.cmd_tx.clone()
    }

    /// 取出 UI 事件接收端（仅可取一次）；协调器据此处理鼠标交互。
    pub fn take_event_rx(&mut self) -> Option<mpsc::Receiver<UiEvent>> {
        self.event_rx.take()
    }

    /// UI 线程主循环
    fn ui_thread(rx: mpsc::Receiver<UiCommand>, event_tx: mpsc::Sender<UiEvent>) {
        // 创建候选窗口
        let config = CandidateWindowConfig::default();
        let mut candidate_window = match CandidateWindow::new(config, event_tx.clone()) {
            Ok(w) => {
                info!("Candidate window created");
                w
            }
            Err(e) => {
                error!("Failed to create candidate window: {}", e);
                return;
            }
        };

        // 状态提示气泡（best-effort，失败不影响候选窗口）
        let mut status_tip = match crate::status_tip::StatusTip::new() {
            Ok(t) => Some(t),
            Err(e) => {
                error!("Failed to create status tip: {}", e);
                None
            }
        };
        let mut tip_hide_at: Option<std::time::Instant> = None;

        // 一次性通知 toast（best-effort）
        let mut toast = match crate::toast::Toast::new() {
            Ok(t) => Some(t),
            Err(e) => {
                error!("Failed to create toast: {}", e);
                None
            }
        };
        let mut toast_hide_at: Option<std::time::Instant> = None;
        // 状态提示防抖：合并快速连续的提示（如连按切换），避免气泡闪烁
        // 载荷：(text, x, y, caret_height, offset_x, offset_y)
        // payload: (text, x, y, caret_h, off_x, off_y, duration_ms, fixed, fixed_x, fixed_y)
        let mut tip_debounce = crate::debounce::Debouncer::<(
            String,
            i32,
            i32,
            i32,
            i32,
            i32,
            u64,
            bool,
            i32,
            i32,
        )>::new(60);
        // 工具栏隐藏防抖：HideToolbar 不立即隐藏，延后 50ms；若期间收到 UpdateToolbar
        // （应用间切换的 FocusLost→FocusGained 串），取消隐藏并显示——消除 Alt+Tab 闪烁。
        let mut toolbar_hide_at: Option<std::time::Instant> = None;
        const TOOLBAR_HIDE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(50);

        // 右键候选弹出菜单（best-effort）
        let mut popup_menu = match crate::popup_menu::PopupMenu::new(event_tx.clone()) {
            Ok(m) => Some(m),
            Err(e) => {
                error!("Failed to create popup menu: {}", e);
                None
            }
        };

        // 常驻工具栏（best-effort，失败不影响其它窗口）
        let mut toolbar = match crate::toolbar::Toolbar::new(event_tx.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                error!("Failed to create toolbar: {}", e);
                None
            }
        };

        // Win32 消息循环 + 通道接收
        // 待处理命令队列：每轮排空通道并合并连续候选更新（只渲染最新一帧），
        // 避免长按翻页/连按方向键时 UpdateCandidates 堆积、松键后仍继续刷新。
        let mut pending: std::collections::VecDeque<UiCommand> = std::collections::VecDeque::new();
        'main: loop {
            // 状态提示气泡到期自动隐藏
            if let Some(deadline) = tip_hide_at {
                if std::time::Instant::now() >= deadline {
                    if let Some(t) = &status_tip {
                        t.hide();
                    }
                    tip_hide_at = None;
                }
            }
            // toast 到期自动隐藏
            if let Some(deadline) = toast_hide_at {
                if std::time::Instant::now() >= deadline {
                    if let Some(t) = &toast {
                        t.hide();
                    }
                    toast_hide_at = None;
                }
            }
            // 工具栏隐藏防抖到期：确认隐藏
            if let Some(deadline) = toolbar_hide_at {
                if std::time::Instant::now() >= deadline {
                    if let Some(t) = &mut toolbar {
                        t.hide();
                    }
                    toolbar_hide_at = None;
                }
            }
            // 非阻塞处理 Win32 消息（仅 Windows 有消息泵；非 Windows 为 mock，跳过）
            #[cfg(windows)]
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // 推进鼠标悬停防抖（稳定后才发出 Hover）
            candidate_window.tick();
            // 推进工具栏悬停高亮（按光标位置本地重绘）
            if let Some(t) = &mut toolbar {
                t.tick();
            }
            // 推进菜单（脏重绘 / 关闭）
            if let Some(m) = &mut popup_menu {
                m.tick();
            }

            // 推进状态提示防抖（稳定后才真正显示气泡）
            if let Some((text, x, y, ch, ox, oy, dur, fixed, fx, fy)) = tip_debounce.poll() {
                if let Some(t) = &mut status_tip {
                    if fixed {
                        t.show_fixed(&text, fx, fy);
                    } else {
                        t.show(&text, x, y, ch, ox, oy);
                    }
                    // dur==0 → 常驻(always):不设隐藏时刻;否则按配置时长自动隐藏。
                    tip_hide_at = if dur == 0 {
                        None
                    } else {
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(dur))
                    };
                }
            }

            // 排空通道：合并连续候选更新（只保留最新一条），其它命令保序
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        // 新候选更新若紧跟在另一候选更新之后，丢弃旧的（只渲染最新帧）
                        if matches!(cmd, UiCommand::UpdateCandidates { .. })
                            && matches!(pending.back(), Some(UiCommand::UpdateCandidates { .. }))
                        {
                            pending.pop_back();
                        }
                        pending.push_back(cmd);
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            let had_cmd = !pending.is_empty();
            // 一轮处理完所有待办（候选更新已合并为至多一条），不留积压到下一轮
            while let Some(cmd) = pending.pop_front() {
                match cmd {
                    UiCommand::UpdateCandidates {
                        preedit,
                        mode_label,
                        candidates,
                        selected,
                        hover,
                        page,
                        total_pages,
                        caret_x,
                        caret_y,
                        caret_height,
                        caret_valid,
                    } => {
                        debug!(
                            "UI: UpdateCandidates ({} items, selected={}, hover={}, page={}/{}, pos={},{})",
                            candidates.len(),
                            selected,
                            hover,
                            page,
                            total_pages,
                            caret_x,
                            caret_y
                        );
                        candidate_window.update(
                            &preedit,
                            &mode_label,
                            candidates,
                            selected,
                            hover,
                            page,
                            total_pages,
                        );
                        candidate_window.set_position(caret_x, caret_y, caret_height, caret_valid);
                        candidate_window.show();
                    }
                    UiCommand::HideCandidates => {
                        debug!("UI: HideCandidates");
                        candidate_window.hide();
                        if let Some(m) = &mut popup_menu {
                            m.hide();
                        }
                    }
                    UiCommand::ShowCandidateMenu { items, x, y, above } => {
                        debug!("UI: ShowMenu ({} items) at ({},{})", items.len(), x, y);
                        if let Some(m) = &mut popup_menu {
                            m.show(items, x, y, above);
                        }
                    }
                    UiCommand::MenuKey(key) => {
                        if let Some(m) = &mut popup_menu {
                            m.on_key(key);
                        }
                    }
                    UiCommand::HideMenu => {
                        if let Some(m) = &mut popup_menu {
                            m.hide();
                        }
                    }
                    UiCommand::CopyToClipboard(text) => {
                        crate::popup_menu::set_clipboard_text(&text);
                    }
                    UiCommand::OpenPath(path) => {
                        open_path(&path);
                    }
                    UiCommand::OpenApp { path, args } => {
                        open_app(&path, &args);
                    }
                    UiCommand::TakeScreenshot { dir } => {
                        let ts = crate::screenshot::timestamp();
                        let dir = std::path::PathBuf::from(&dir);
                        let mut saved = 0usize;
                        let mut candidate_to_clipboard = false;

                        // 候选窗口：保存文件 + 同时复制到剪贴板（与 Go 对齐）
                        if candidate_window.is_visible() {
                            let path = dir.join(format!("candidate_{ts}.png"));
                            match candidate_window.capture_to_file(&path) {
                                Ok(_) => {
                                    saved += 1;
                                    info!("Screenshot saved: {:?}", path);
                                    match candidate_window.capture_to_clipboard() {
                                        Ok(_) => candidate_to_clipboard = true,
                                        Err(e) => tracing::warn!("Screenshot clipboard: {}", e),
                                    }
                                }
                                Err(e) => tracing::warn!("Screenshot candidate: {}", e),
                            }
                        }
                        // 工具栏
                        if let Some(tb) = &toolbar {
                            if tb.is_visible() {
                                let path = dir.join(format!("toolbar_{ts}.png"));
                                match tb.capture_to_file(&path) {
                                    Ok(_) => {
                                        saved += 1;
                                        info!("Screenshot saved: {:?}", path);
                                    }
                                    Err(e) => tracing::warn!("Screenshot toolbar: {}", e),
                                }
                            }
                        }
                        // 状态提示
                        if let Some(st) = &status_tip {
                            if st.is_visible() {
                                let path = dir.join(format!("status_tip_{ts}.png"));
                                match st.capture_to_file(&path) {
                                    Ok(_) => {
                                        saved += 1;
                                        info!("Screenshot saved: {:?}", path);
                                    }
                                    Err(e) => tracing::warn!("Screenshot status_tip: {}", e),
                                }
                            }
                        }
                        // 右键菜单
                        if let Some(pm) = &popup_menu {
                            if pm.is_visible() {
                                let path = dir.join(format!("popup_menu_{ts}.png"));
                                match pm.capture_to_file(&path) {
                                    Ok(_) => {
                                        saved += 1;
                                        info!("Screenshot saved: {:?}", path);
                                    }
                                    Err(e) => tracing::warn!("Screenshot popup_menu: {}", e),
                                }
                            }
                        }
                        // Toast（通常不可见，有则顺带保存）
                        if let Some(t) = &toast {
                            if t.is_visible() {
                                let path = dir.join(format!("toast_{ts}.png"));
                                match t.capture_to_file(&path) {
                                    Ok(_) => {
                                        saved += 1;
                                        info!("Screenshot saved: {:?}", path);
                                    }
                                    Err(e) => tracing::warn!("Screenshot toast: {}", e),
                                }
                            }
                        }
                        info!("UI screenshots taken: {}, dir: {:?}", saved, dir);
                        // 结果 toast
                        if let Some(t) = &mut toast {
                            let msg = if saved > 0 {
                                if candidate_to_clipboard {
                                    format!(
                                        "已保存 {} 张截图（候选已复制到剪贴板）\n{}",
                                        saved,
                                        dir.display()
                                    )
                                } else {
                                    format!("已保存 {} 张截图\n{}", saved, dir.display())
                                }
                            } else {
                                "没有可见的 UI 窗口可截图".to_string()
                            };
                            let kind = if saved > 0 {
                                ToastKind::Success
                            } else {
                                ToastKind::Info
                            };
                            t.show(&msg, ToastPosition::BottomRight, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now() + std::time::Duration::from_millis(4000),
                            );
                        }
                    }
                    UiCommand::ScreenshotCandidateToClipboard => {
                        let (msg, kind) = if candidate_window.is_visible() {
                            match candidate_window.capture_to_clipboard() {
                                Ok(_) => {
                                    info!("Candidate screenshot copied to clipboard");
                                    ("候选窗口已截图到剪贴板".to_string(), ToastKind::Success)
                                }
                                Err(e) => {
                                    tracing::warn!("Screenshot to clipboard failed: {}", e);
                                    (format!("截图到剪贴板失败：{}", e), ToastKind::Error)
                                }
                            }
                        } else {
                            ("候选窗口未显示，无法截图".to_string(), ToastKind::Info)
                        };
                        if let Some(t) = &mut toast {
                            t.show(&msg, ToastPosition::BottomRight, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now() + std::time::Duration::from_millis(3000),
                            );
                        }
                    }
                    UiCommand::ShowStatusTip {
                        text,
                        x,
                        y,
                        caret_height,
                        offset_x,
                        offset_y,
                        duration_ms,
                        fixed,
                        fixed_x,
                        fixed_y,
                    } => {
                        debug!("UI: ShowStatusTip '{}' at ({},{})", text, x, y);
                        // 经防抖：合并快速连续提示，避免气泡闪烁
                        tip_debounce.trigger((
                            text,
                            x,
                            y,
                            caret_height,
                            offset_x,
                            offset_y,
                            duration_ms,
                            fixed,
                            fixed_x,
                            fixed_y,
                        ));
                    }
                    UiCommand::HideStatusTip => {
                        // 取消待显示的防抖项 + 立即隐藏 + 清隐藏计时(常驻模式失焦)。
                        tip_debounce.cancel();
                        if let Some(t) = &status_tip {
                            t.hide();
                        }
                        tip_hide_at = None;
                    }
                    UiCommand::ShowToast {
                        text,
                        position,
                        kind,
                        duration_ms,
                    } => {
                        debug!("UI: ShowToast '{}' ({:?},{:?})", text, position, kind);
                        if let Some(t) = &mut toast {
                            t.show(&text, position, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now()
                                    + std::time::Duration::from_millis(duration_ms.max(1)),
                            );
                        }
                    }
                    UiCommand::UpdateToolbar(tb_state) => {
                        debug!("UI: UpdateToolbar {:?}", tb_state);
                        toolbar_hide_at = None; // 取消待定隐藏（切回本输入法 → 保持显示）
                        if let Some(t) = &mut toolbar {
                            t.update(&tb_state);
                        }
                    }
                    UiCommand::HideToolbar => {
                        debug!(
                            "UI: HideToolbar (debounced {}ms)",
                            TOOLBAR_HIDE_DEBOUNCE.as_millis()
                        );
                        // 延后隐藏：50ms 内若有 UpdateToolbar 则取消，消除应用间切换闪烁。
                        toolbar_hide_at = Some(std::time::Instant::now() + TOOLBAR_HIDE_DEBOUNCE);
                    }
                    UiCommand::SetToolbarPos { x, y } => {
                        debug!("UI: SetToolbarPos ({},{})", x, y);
                        if let Some(t) = &mut toolbar {
                            t.set_pos(x, y);
                        }
                    }
                    UiCommand::SetTheme(theme) => {
                        debug!("UI: SetTheme (dark={})", theme.is_dark);
                        let t = *theme;
                        if let Some(tb) = &mut toolbar {
                            tb.set_theme(&t);
                            tb.repaint();
                        }
                        if let Some(m) = &mut popup_menu {
                            m.set_theme(&t);
                        }
                        if let Some(st) = &mut status_tip {
                            st.set_theme(&t);
                        }
                        if let Some(to) = &mut toast {
                            to.set_theme(&t);
                        }
                        candidate_window.set_theme(t); // 同时更新其 tooltip
                        if candidate_window.is_visible() {
                            candidate_window.show();
                        }
                    }
                    UiCommand::SetCandidateLayout(vertical) => {
                        candidate_window.set_vertical(vertical);
                    }
                    UiCommand::SetPreeditEmbedded(embedded) => {
                        candidate_window.set_preedit_embedded(embedded);
                    }
                    UiCommand::SetCandidateFontSize(size) => {
                        candidate_window.set_font_size_override(size);
                    }
                    UiCommand::SetCandidateFontFamily(family) => {
                        candidate_window.set_font_family(&family);
                    }
                    UiCommand::SetTooltipDelay(delay) => {
                        candidate_window.set_tooltip_delay(delay);
                    }
                    UiCommand::SetCandidateFlipWhenAbove(flip) => {
                        candidate_window.set_flip_when_above(flip);
                    }
                    UiCommand::SetPagerDisplay(mode) => {
                        candidate_window.set_pager_display(mode);
                    }
                    UiCommand::SetPageNumberDisplay(mode) => {
                        candidate_window.set_page_number_display(mode);
                    }
                    UiCommand::SetTooltipChaiziFont(path) => {
                        candidate_window.set_tooltip_chaizi_font(&path);
                    }
                    UiCommand::Shutdown => {
                        info!("UI: Shutdown");
                        candidate_window.hide();
                        if let Some(t) = &status_tip {
                            t.hide();
                        }
                        if let Some(t) = &toast {
                            t.hide();
                        }
                        if let Some(t) = &mut toolbar {
                            t.hide();
                        }
                        break 'main;
                    }
                }
            }
            if disconnected {
                info!("UI: Channel disconnected, shutting down");
                break 'main;
            }
            if !had_cmd {
                // 无命令，短暂休眠避免 CPU 空转
                std::thread::sleep(std::time::Duration::from_millis(8));
            }
        }
    }
}

impl Drop for UiManager {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(UiCommand::Shutdown);
    }
}

/// 用资源管理器打开路径（best-effort）
#[cfg(windows)]
fn open_path(path: &str) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open_path(_path: &str) {}

/// 启动可执行程序并传参（ShellExecute open + params）；args 为空时等价 open_path。
#[cfg(windows)]
fn open_app(path: &str, args: &str) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let params: Vec<u16> = args.encode_utf16().chain(std::iter::once(0)).collect();
    let params_ptr = if args.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(params.as_ptr())
    };
    unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            params_ptr,
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open_app(_path: &str, _args: &str) {}
