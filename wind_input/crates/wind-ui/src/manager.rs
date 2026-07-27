//! UI 管理器 + 消息循环
//!
//! 与 Go 版本 `wind_input/internal/ui/manager.go` 对齐。
//! 在独立线程中运行 Win32 消息循环，通过通道接收 UI 更新命令。

use crate::candidate_window::{CandidateItem, CandidateWindow, CandidateWindowConfig};
use crate::toast::{ToastKind, ToastPosition};

/// re-export：使协调器以 `wind_ui::manager::InputDiagView` 统一引用。
pub use crate::input_diag_hud::InputDiagView;
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
        /// 编码区插入符位置：`preedit` 内的**字节**偏移（恒在字符边界）。自绘 preedit 栏据此
        /// 画竖线；等于 `preedit.len()` 即光标在末尾。
        preedit_caret: usize,
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
        /// 固定位置模式（ui.candidate.position_mode=fixed）：忽略光标，用 fixed_x/fixed_y 定位。
        fixed: bool,
        /// 固定位置的**内容左上**屏幕坐标；(0,0) 表示尚未设定，由 UI 侧落到屏幕默认锚点。
        fixed_x: i32,
        fixed_y: i32,
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
    /// 显示/更新输入诊断 HUD（右键「高级」开）。惰性创建，可拖动，双击复制。
    ShowInputDiag(crate::input_diag_hud::InputDiagView),
    /// 隐藏输入诊断 HUD。
    HideInputDiag,
    /// 更新常驻工具栏状态（中英/方案/标点/全半角）
    UpdateToolbar(crate::toolbar::ToolbarState),
    /// 隐藏工具栏
    HideToolbar,
    /// 设置工具栏位置（启动时恢复持久化位置）
    SetToolbarPos { x: i32, y: i32 },
    /// 工具栏自动隐藏配置（开关 + 超时毫秒）。来自 ui.toolbar.auto_hide / auto_hide_delay，
    /// 协调器 apply_ui_config（启动 + 配置重载）下发。
    SetToolbarAutoHide { enabled: bool, delay_ms: u64 },
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
    /// 候选窗在光标上方时交换编码栏与候选栏位置。来自 ui.candidate.swap_preedit_when_above。
    SetCandidateSwapWhenAbove(bool),
    /// 翻页栏并入编码栏行右对齐显示。来自 ui.candidate.pager_in_preedit。
    SetPagerInPreedit(bool),
    /// 翻页栏显示覆盖（""跟随主题/"hide"/"auto"/"always"）。来自 ui.candidate.pager_bar_display。
    SetPagerDisplay(String),
    /// 页码文字显示覆盖（""跟随主题/"show"/"hide"）。来自 ui.candidate.page_number_display。
    SetPageNumberDisplay(String),
    /// 拆字字根字体（PUA 字根字符渲染）：TTF 文件路径 + DWrite 家族名（取自方案 [engine.chaizi]）。
    SetTooltipChaiziFont { path: String, family: String },
    /// 显示菜单（候选右键菜单 / 功能主菜单；UI 自管导航与子菜单）。
    /// above=true：菜单底边对齐 (x,y) 向上展开（工具栏菜单用，避免遮挡工具栏）；
    /// y_bottom 为锚点区域下边界，上方空间不足时改为从 y_bottom 向下弹出。
    ShowCandidateMenu {
        items: Vec<MenuItemSpec>,
        x: i32,
        y: i32,
        y_bottom: i32,
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
    /// 启动应用程序并传参（如 wind_setting.exe `--page dict`）。
    OpenApp { path: String, args: String },
    /// 截图所有可见 UI 窗口，保存到 dir 目录（由协调器根据 config 确定）。
    TakeScreenshot { dir: String },
    /// 将候选窗口截图复制到剪贴板（候选不可见则提示）。
    ScreenshotCandidateToClipboard,
    /// 截图状态提示气泡到文件（状态提示右键菜单「截图此窗口」）。
    ScreenshotStatusTip { dir: std::path::PathBuf },
    /// 复制悬停提示（编码反查气泡）文本到剪贴板（其右键菜单「复制内容」）。
    CopyTooltipText,
    /// 截图悬停提示到文件（其右键菜单「截图此窗口」）。
    ScreenshotTooltip { dir: std::path::PathBuf },
    /// 设置悬停提示右键菜单打开状态（开启时抑制其 WM_MOUSELEAVE 自动隐藏）。
    SetTooltipMenuOpen(bool),
    /// 标记状态气泡的右键菜单开/关（打开期间抑制自动隐藏）。
    SetStatusMenuOpen(bool),
    /// 请求上报状态气泡当前位置：UI 侧回 `UiEvent::StatusTipMoved`。
    /// 供「固定位置」开关把当前实际位置落盘，而不是跳到陈旧的 custom_x/custom_y。
    ReportStatusTipPos,
    /// 请求上报候选窗当前位置：UI 侧回 `UiEvent::CandidateWindowMoved`。
    /// 供「定位方式」切到 fixed 时就地固定——窗口正显示着就用它当前的位置，
    /// 不显示则不上报（协调器留空，首显时由 UI 落到屏幕默认锚点）。
    ReportCandidatePos,
    /// 注册全局热键（Win32 RegisterHotKey，线程级）。覆盖式：先反注册旧列表再注册新列表，
    /// 空列表 = 仅清除已注册项。来自 keys.global_hotkeys（协调器构建，启动/配置重载时下发）。
    RegisterGlobalHotkeys(Vec<GlobalHotkeyEntry>),
    /// 关闭 UI
    Shutdown,
    /// 注入 host-render 管理器（Windows）；协调器 `set_host_render` 后下发，
    /// UI 线程收到后在消息循环中激活 SHM 分流路径。
    #[cfg(windows)]
    SetHostRender(HostRenderArc),
}

/// `HostRenderManager` 不派生 Debug，包一层使 UiCommand 可 derive Debug。
#[cfg(windows)]
pub struct HostRenderArc(pub std::sync::Arc<wind_bridge::host_render_windows::HostRenderManager>);

#[cfg(windows)]
impl std::fmt::Debug for HostRenderArc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostRenderManager")
    }
}

/// 全局热键条目（协调器按 keys.global_hotkeys 构建，UI 线程经 Win32 RegisterHotKey 注册）。
/// `modifiers` 为 Win32 RegisterHotKey 修饰位（MOD_ALT=0x1/MOD_CONTROL=0x2/MOD_SHIFT=0x4/
/// MOD_WIN=0x8），与 wind-ipc 的 MOD_* 位序不同（ALT/SHIFT 互换），转换在协调器侧完成。
#[derive(Debug, Clone)]
pub struct GlobalHotkeyEntry {
    /// RegisterHotKey 热键 ID（UI 线程内唯一即可）
    pub id: i32,
    /// Win32 修饰位
    pub modifiers: u32,
    /// Windows 虚拟键码
    pub vk: u32,
    /// 触发后回送协调器的热键动作名（与 dispatch_hotkey 的 action 一致）
    pub action: String,
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
    /// 打开用户数据目录（配置/词库等用户数据所在目录）
    OpenConfigDir,
    /// 打开应用程序目录（exe 所在目录，高级菜单）
    OpenAppDir,
    /// 打开日志文件目录（高级菜单）
    OpenLogDir,
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
    /// 切换输入诊断 HUD 显隐（高级菜单）
    ToggleInputDiagnostics,
    /// 切换密码框强制英文（高级菜单，临时测试入口）
    TogglePasswordSuppress,
    /// 状态提示气泡：切换常驻显示（display_mode always/temp）
    StatusToggleAlways,
    /// 状态提示气泡：恢复默认位置（position_mode=follow_caret）
    StatusResetPosition,
    /// 状态提示气泡：截图此窗口
    StatusScreenshot,
    /// 悬停提示（编码反查气泡）：复制内容
    TooltipCopy,
    /// 悬停提示（编码反查气泡）：截图此窗口
    TooltipScreenshot,
    /// 状态提示气泡：切换固定位置（position_mode fixed/follow_caret）
    StatusTogglePinned,
    /// 为当前焦点应用设置候选窗首显策略（compat.toml 的 first_show_mode）。
    /// 参数：0=wait 1=fast 2=instant。三档互斥，UI 上呈现为子菜单单选。
    FirstShowMode(u8),
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

impl MenuKind {
    /// 稳定菜单 id：macOS `.app` 把它写进 `NSMenuItem.tag`，选中后经 `CmdMenuAction`
    /// 原样回传，Rust 据此还原动作。构建菜单树（下发）与处理回传（还原）共用此映射，
    /// 二者必须一致。`Submenu`/`Separator` 不回传，恒为 0。
    /// id 区间：1 复制｜10-19 词条操作｜100-199 固定命令｜1000+ 方案｜2000+ 主题｜3000+ 过滤｜4000+ 明暗。
    pub fn to_menu_id(self) -> i32 {
        match self {
            MenuKind::Separator | MenuKind::Submenu => 0,
            MenuKind::Copy => 1,
            MenuKind::Op(op) => match op {
                CandidateOp::MoveTop => 10,
                CandidateOp::MoveUp => 11,
                CandidateOp::MoveDown => 12,
                CandidateOp::Delete => 13,
                CandidateOp::Reset => 14,
            },
            MenuKind::Command(cmd) => match cmd {
                MenuCmd::SchemaEnglish => 100,
                MenuCmd::TogglePunct => 101,
                MenuCmd::ToggleWidth => 102,
                MenuCmd::ToggleS2t => 103,
                MenuCmd::ToggleToolbar => 104,
                MenuCmd::ReloadConfig => 105,
                MenuCmd::RestartService => 106,
                MenuCmd::OpenConfigDir => 107,
                MenuCmd::OpenDictionary => 108,
                MenuCmd::OpenSettings => 109,
                MenuCmd::OpenAbout => 110,
                MenuCmd::TakeScreenshot => 111,
                MenuCmd::ScreenshotCandidateToClipboard => 112,
                MenuCmd::OpenAppDir => 113,
                MenuCmd::OpenLogDir => 114,
                MenuCmd::StatusToggleAlways => 115,
                MenuCmd::StatusResetPosition => 116,
                MenuCmd::StatusScreenshot => 117,
                MenuCmd::TooltipCopy => 118,
                MenuCmd::TooltipScreenshot => 119,
                MenuCmd::StatusTogglePinned => 122,
                MenuCmd::ToggleInputDiagnostics => 120,
                MenuCmd::TogglePasswordSuppress => 121,
                MenuCmd::FirstShowMode(m) => 5000 + m as i32,
                MenuCmd::SchemaSelect(i) => 1000 + i as i32,
                MenuCmd::ThemeSelect(i) => 2000 + i as i32,
                MenuCmd::FilterMode(i) => 3000 + i as i32,
                MenuCmd::ThemeStyle(s) => 4000 + s as i32,
            },
        }
    }

    /// 由回传的菜单 id 还原动作；未知 id / 不可点击项返回 None。
    pub fn from_menu_id(id: i32) -> Option<MenuKind> {
        let cmd = match id {
            1 => return Some(MenuKind::Copy),
            10 => return Some(MenuKind::Op(CandidateOp::MoveTop)),
            11 => return Some(MenuKind::Op(CandidateOp::MoveUp)),
            12 => return Some(MenuKind::Op(CandidateOp::MoveDown)),
            13 => return Some(MenuKind::Op(CandidateOp::Delete)),
            14 => return Some(MenuKind::Op(CandidateOp::Reset)),
            100 => MenuCmd::SchemaEnglish,
            101 => MenuCmd::TogglePunct,
            102 => MenuCmd::ToggleWidth,
            103 => MenuCmd::ToggleS2t,
            104 => MenuCmd::ToggleToolbar,
            105 => MenuCmd::ReloadConfig,
            106 => MenuCmd::RestartService,
            107 => MenuCmd::OpenConfigDir,
            108 => MenuCmd::OpenDictionary,
            109 => MenuCmd::OpenSettings,
            110 => MenuCmd::OpenAbout,
            111 => MenuCmd::TakeScreenshot,
            112 => MenuCmd::ScreenshotCandidateToClipboard,
            113 => MenuCmd::OpenAppDir,
            114 => MenuCmd::OpenLogDir,
            115 => MenuCmd::StatusToggleAlways,
            116 => MenuCmd::StatusResetPosition,
            117 => MenuCmd::StatusScreenshot,
            118 => MenuCmd::TooltipCopy,
            119 => MenuCmd::TooltipScreenshot,
            122 => MenuCmd::StatusTogglePinned,

            120 => MenuCmd::ToggleInputDiagnostics,
            121 => MenuCmd::TogglePasswordSuppress,
            1000..=1999 => MenuCmd::SchemaSelect((id - 1000) as usize),
            2000..=2999 => MenuCmd::ThemeSelect((id - 2000) as usize),
            3000..=3999 => MenuCmd::FilterMode((id - 3000) as usize),
            4000..=4999 => MenuCmd::ThemeStyle((id - 4000) as u8),
            5000..=5999 => MenuCmd::FirstShowMode((id - 5000) as u8),
            _ => return None,
        };
        Some(MenuKind::Command(cmd))
    }
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
    /// above=true：菜单在 (x,y) 上方弹出（工具栏触发，避免遮挡工具栏）；
    /// y_bottom 为工具栏底边，上方空间不足时改为从 y_bottom 向下弹出。
    RequestMainMenu {
        x: i32,
        y: i32,
        y_bottom: i32,
        above: bool,
    },
    /// 菜单项激活（携带动作）：UI 自管导航/子菜单，仅把最终动作回送协调器
    MenuAction(MenuKind),
    /// 关闭菜单（点击菜单外 / ESC / 右键）
    MenuClose,
    /// 全局热键触发（线程级 RegisterHotKey 的 WM_HOTKEY），携带热键动作名
    GlobalHotkey(String),
    /// 状态提示气泡被拖动到新位置（内容左上屏幕坐标），供协调器持久化
    StatusTipMoved { x: i32, y: i32 },
    /// 候选窗被拖动到新位置（内容左上屏幕坐标）。协调器仅在 fixed 模式下持久化；
    /// follow_caret 模式的拖动是"本次组合内临时挪开"，不落盘。
    CandidateWindowMoved { x: i32, y: i32 },
    /// 右键状态提示气泡请求弹出菜单（屏幕坐标）
    RequestStatusMenu { x: i32, y: i32 },
    /// 右键悬停提示（编码反查气泡）请求弹出菜单（屏幕坐标）
    RequestTooltipMenu { x: i32, y: i32 },
    /// 系统「浅色/深色模式」已切换（Win32 `WM_SETTINGCHANGE`/`ImmersiveColorSet`）。
    /// 协调器仅在 `ui.theme.style = "system"` 时据此重解析主题，其余明暗为用户显式指定。
    SystemThemeChanged,
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
    ///
    /// 注意 [`UiManager::new`] 只负责 spawn 本线程即返回 `Ok`——窗口创建成功与否**不影响**
    /// 它的返回值。因此本函数一旦提前 `return`，主线程毫不知情：服务照常启动、输入照常工作，
    /// 而候选窗/工具栏/托盘/状态气泡**全部消失**。开机早期窗口站尚未就绪时
    /// `CreateWindowExW` 失败正是这种场景，且唯一痕迹是下面那条 `error!`——
    /// 偏偏主日志的 non_blocking worker 也可能已经死了。故这些分支同时写启动轨迹。
    fn ui_thread(rx: mpsc::Receiver<UiCommand>, event_tx: mpsc::Sender<UiEvent>) {
        wind_config::startup_trace::stage("ui-thread-begin");

        // 创建候选窗口
        let config = CandidateWindowConfig::default();
        let mut candidate_window = match CandidateWindow::new(config, event_tx.clone()) {
            Ok(w) => {
                info!("Candidate window created");
                wind_config::startup_trace::stage("ui-candidate-window-ok");
                w
            }
            Err(e) => {
                error!("Failed to create candidate window: {}", e);
                // UI 线程就此退出 = 全部 GUI 消失，这是最需要留痕的一步。
                wind_config::startup_trace::stage(&format!("ui-candidate-window-FAILED: {e}"));
                return;
            }
        };

        // 状态提示气泡（best-effort，失败不影响候选窗口）
        let mut status_tip = match crate::status_tip::StatusTip::new(event_tx.clone()) {
            Ok(t) => Some(t),
            Err(e) => {
                error!("Failed to create status tip: {}", e);
                None
            }
        };
        let mut tip_hide_at: Option<std::time::Instant> = None;
        // 最近一次显示所用的自动隐藏时长（毫秒），交互结束后据此重新计时。
        let mut tip_duration_ms: u64 = 0;

        // 输入诊断 HUD（惰性创建：首次 ShowInputDiag 时构造，best-effort）
        let mut input_diag_hud: Option<crate::input_diag_hud::InputDiagHud> = None;

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

        // 已注册的全局热键（RegisterHotKey hwnd=NULL 绑定本线程；WM_HOTKEY 落线程消息队列，
        // 无目标窗口，DispatchMessage 不路由，须在下方消息泵中直接截获）。
        #[cfg(windows)]
        let mut global_hotkeys: Vec<GlobalHotkeyEntry> = Vec::new();

        // host-render 管理器（Windows）：由 SetHostRender 命令注入；None = 本地 LayeredWindow 路径。
        #[cfg(windows)]
        let mut host_render: Option<
            std::sync::Arc<wind_bridge::host_render_windows::HostRenderManager>,
        > = None;

        // 所有窗口构造完毕、即将进入消息泵。走到这里说明 GUI 该有的都建起来了；
        // 若客户仍报「无 GUI」，问题就在消息泵或显示逻辑，而非创建失败。
        wind_config::startup_trace::stage("ui-thread-loop");

        // Win32 消息循环 + 通道接收
        // 待处理命令队列：每轮排空通道并合并连续候选更新（只渲染最新一帧），
        // 避免长按翻页/连按方向键时 UpdateCandidates 堆积、松键后仍继续刷新。
        let mut pending: std::collections::VecDeque<UiCommand> = std::collections::VecDeque::new();
        'main: loop {
            // 状态提示气泡到期自动隐藏。
            // 用户正在与气泡交互（拖动 / 悬停其上 / 右键菜单打开）时**顺延**而非隐藏：
            // 否则气泡会在被操作的过程中凭空消失。交互结束后重新获得完整一份时长。
            if let Some(deadline) = tip_hide_at {
                let interacting = status_tip.as_ref().is_some_and(|t| t.interacting());
                if interacting {
                    tip_hide_at = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_millis(tip_duration_ms.max(1)),
                    );
                } else if std::time::Instant::now() >= deadline {
                    if let Some(t) = &status_tip {
                        t.hide();
                    }
                    #[cfg(windows)]
                    if let Some(hr) = &host_render {
                        use wind_ipc::protocol::HOST_WINDOW_STATUS;
                        hr.hide_kind(HOST_WINDOW_STATUS);
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
                    // 线程级全局热键：WM_HOTKEY 无目标窗口，须在泵中截获并回送协调器
                    if msg.message == WM_HOTKEY {
                        let id = msg.wParam.0 as i32;
                        if let Some(e) = global_hotkeys.iter().find(|e| e.id == id) {
                            debug!("UI: global hotkey triggered: {}", e.action);
                            let _ = event_tx.send(UiEvent::GlobalHotkey(e.action.clone()));
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // 系统明暗切换：wnd_proc 在上面 PeekMessage 期间被系统回调置标记（该消息是
                // SendMessage 广播，不入队列，泵里截不到），故在泵之后取走并回送协调器。
                if crate::window::take_system_color_changed() {
                    debug!("UI: 系统明暗设置变更 → 通知协调器");
                    let _ = event_tx.send(UiEvent::SystemThemeChanged);
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
            if let Some((text, x, y, ch, ox, oy, dur, fixed, fx, fy)) = tip_debounce.poll()
                && let Some(t) = &mut status_tip
            {
                // host-render 分流：有活跃目标且写帧成功 → SHM + 本地隐藏；否则本地显示。
                let mut host_ok = false;
                #[cfg(windows)]
                if let Some(hr) = &host_render
                    && let Some(target) = hr.active_target()
                {
                    use wind_bridge::shared_render_frame::FrameParams;
                    use wind_ipc::protocol::HOST_WINDOW_STATUS;
                    let fo = if fixed {
                        t.render_frame_fixed(&text, fx, fy)
                    } else {
                        t.render_frame(&text, x, y, ch, ox, oy)
                    };
                    if let Some((bgra, w, h, sx, sy, sw)) = fo {
                        let p = FrameParams {
                            sequence: 0,
                            x: sx,
                            y: sy,
                            width: w,
                            height: h,
                            bgra: &bgra,
                            rects: &[],
                            rendered_hover_index: -1,
                            target_instance_id: 0,
                            software_shadow: sw,
                        };
                        match hr.write_frame_for_kind(HOST_WINDOW_STATUS, &target, &p) {
                            Ok(()) => {
                                t.hide();
                                host_ok = true;
                            }
                            Err(e) => {
                                tracing::warn!("host render 写 status 帧失败，回退本地: {}", e);
                            }
                        }
                    }
                }
                if !host_ok {
                    if fixed {
                        t.show_fixed(&text, fx, fy);
                    } else {
                        t.show(&text, x, y, ch, ox, oy);
                    }
                }
                // dur==0 → 常驻(always):不设隐藏时刻;否则按配置时长自动隐藏。
                tip_duration_ms = dur;
                tip_hide_at = if dur == 0 {
                    None
                } else {
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(dur))
                };
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
                        preedit_caret,
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
                        fixed,
                        fixed_x,
                        fixed_y,
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
                            preedit_caret,
                            &mode_label,
                            candidates,
                            selected,
                            hover,
                            page,
                            total_pages,
                        );
                        candidate_window.set_position(caret_x, caret_y, caret_height, caret_valid);
                        candidate_window.set_fixed_position(fixed.then_some((fixed_x, fixed_y)));
                        // host-render 分流：有活跃目标时渲染到 SHM，本地窗口互斥隐藏。
                        // 无目标或 host-render 未注入时落本地 LayeredWindow 路径（零改动）。
                        #[cfg(windows)]
                        if let Some(hr) = &host_render
                            && try_host_render_candidates(hr, &mut candidate_window)
                        {
                            continue; // 跳过本地 show()，分流完成
                        }
                        candidate_window.show();
                    }
                    UiCommand::HideCandidates => {
                        debug!("UI: HideCandidates");
                        // host-render 侧先 hide（hide 必达，幂等双发）
                        #[cfg(windows)]
                        if let Some(hr) = &host_render {
                            use wind_ipc::protocol::{HOST_WINDOW_CANDIDATE, HOST_WINDOW_TOOLTIP};
                            hr.hide_kind(HOST_WINDOW_CANDIDATE);
                            hr.hide_kind(HOST_WINDOW_TOOLTIP);
                        }
                        candidate_window.hide();
                        if let Some(m) = &mut popup_menu {
                            m.hide();
                        }
                    }
                    UiCommand::ShowCandidateMenu {
                        items,
                        x,
                        y,
                        y_bottom,
                        above,
                    } => {
                        debug!("UI: ShowMenu ({} items) at ({},{})", items.len(), x, y);
                        if let Some(m) = &mut popup_menu {
                            m.show(items, x, y, y_bottom, above);
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
                        // 悬停提示（编码反查气泡）
                        if candidate_window.tooltip_is_visible() {
                            let path = dir.join(format!("tooltip_{ts}.png"));
                            match candidate_window.tooltip_capture_to_file(&path) {
                                Ok(_) => {
                                    saved += 1;
                                    info!("Screenshot saved: {:?}", path);
                                }
                                Err(e) => tracing::warn!("Screenshot tooltip: {}", e),
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
                    UiCommand::ScreenshotStatusTip { dir } => {
                        let ts = crate::screenshot::timestamp();
                        let (msg, kind) = match &status_tip {
                            Some(st) if st.is_visible() => {
                                let path = dir.join(format!("status_tip_{ts}.png"));
                                match st.capture_to_file(&path) {
                                    Ok(_) => {
                                        info!("Screenshot saved: {:?}", path);
                                        // 存盘的同时进剪贴板：截完就能直接粘贴，省去翻目录。
                                        let clip = st.capture_to_clipboard();
                                        if let Err(e) = &clip {
                                            tracing::warn!(
                                                "Screenshot status_tip clipboard: {}",
                                                e
                                            );
                                        }
                                        let suffix = if clip.is_ok() {
                                            "（已复制到剪贴板）"
                                        } else {
                                            ""
                                        };
                                        (
                                            format!(
                                                "状态提示气泡已截图{}\n{}",
                                                suffix,
                                                path.display()
                                            ),
                                            ToastKind::Success,
                                        )
                                    }
                                    Err(e) => {
                                        tracing::warn!("Screenshot status_tip: {}", e);
                                        (format!("截图失败：{}", e), ToastKind::Error)
                                    }
                                }
                            }
                            _ => ("状态提示气泡未显示，无法截图".to_string(), ToastKind::Info),
                        };
                        if let Some(t) = &mut toast {
                            t.show(&msg, ToastPosition::BottomRight, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now() + std::time::Duration::from_millis(3000),
                            );
                        }
                    }
                    UiCommand::CopyTooltipText => {
                        let text = candidate_window.tooltip_text().to_string();
                        let (msg, kind) = if !text.is_empty() {
                            crate::popup_menu::set_clipboard_text(&text);
                            ("提示内容已复制".to_string(), ToastKind::Success)
                        } else {
                            ("提示内容为空，无法复制".to_string(), ToastKind::Info)
                        };
                        if let Some(t) = &mut toast {
                            t.show(&msg, ToastPosition::BottomRight, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now() + std::time::Duration::from_millis(3000),
                            );
                        }
                    }
                    UiCommand::ScreenshotTooltip { dir } => {
                        let ts = crate::screenshot::timestamp();
                        let (msg, kind) = if candidate_window.tooltip_is_visible() {
                            let path = dir.join(format!("tooltip_{ts}.png"));
                            match candidate_window.tooltip_capture_to_file(&path) {
                                Ok(_) => {
                                    info!("Screenshot saved: {:?}", path);
                                    // 存盘的同时进剪贴板：截完就能直接粘贴，省去翻目录。
                                    let clip = candidate_window.tooltip_capture_to_clipboard();
                                    if let Err(e) = &clip {
                                        tracing::warn!("Screenshot tooltip clipboard: {}", e);
                                    }
                                    let suffix = if clip.is_ok() {
                                        "（已复制到剪贴板）"
                                    } else {
                                        ""
                                    };
                                    (
                                        format!("提示气泡已截图{}\n{}", suffix, path.display()),
                                        ToastKind::Success,
                                    )
                                }
                                Err(e) => {
                                    tracing::warn!("Screenshot tooltip: {}", e);
                                    (format!("截图失败：{}", e), ToastKind::Error)
                                }
                            }
                        } else {
                            ("提示气泡未显示，无法截图".to_string(), ToastKind::Info)
                        };
                        if let Some(t) = &mut toast {
                            t.show(&msg, ToastPosition::BottomRight, kind);
                            toast_hide_at = Some(
                                std::time::Instant::now() + std::time::Duration::from_millis(3000),
                            );
                        }
                    }
                    UiCommand::SetStatusMenuOpen(open) => {
                        if let Some(st) = &status_tip {
                            st.set_menu_open(open);
                        }
                    }
                    UiCommand::ReportStatusTipPos => {
                        if let Some(st) = &status_tip
                            && st.is_visible()
                        {
                            let (x, y) = st.content_origin();
                            let _ = event_tx.send(UiEvent::StatusTipMoved { x, y });
                        }
                    }
                    UiCommand::ReportCandidatePos => {
                        if candidate_window.is_visible() {
                            let (x, y) = candidate_window.content_origin();
                            let _ = event_tx.send(UiEvent::CandidateWindowMoved { x, y });
                        }
                    }
                    UiCommand::SetTooltipMenuOpen(open) => {
                        candidate_window.tooltip_set_menu_open(open);
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
                        #[cfg(windows)]
                        if let Some(hr) = &host_render {
                            use wind_ipc::protocol::HOST_WINDOW_STATUS;
                            hr.hide_kind(HOST_WINDOW_STATUS);
                        }
                        tip_hide_at = None;
                    }
                    UiCommand::ShowInputDiag(v) => {
                        // 惰性创建：失败仅记 error，不影响其它窗口。
                        if input_diag_hud.is_none() {
                            match crate::input_diag_hud::InputDiagHud::new() {
                                Ok(h) => input_diag_hud = Some(h),
                                Err(e) => error!("Failed to create input diag HUD: {}", e),
                            }
                        }
                        if let Some(h) = input_diag_hud.as_mut() {
                            h.show_or_update(&v);
                        }
                    }
                    UiCommand::HideInputDiag => {
                        if let Some(h) = input_diag_hud.as_mut() {
                            h.hide();
                        }
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
                    UiCommand::SetToolbarAutoHide { enabled, delay_ms } => {
                        debug!(
                            "UI: SetToolbarAutoHide enabled={} delay={}ms",
                            enabled, delay_ms
                        );
                        if let Some(t) = &mut toolbar {
                            t.set_auto_hide(enabled, delay_ms);
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
                            // host 模式下 visible=true 表示「内容在 host 窗口可见」，重绘须走
                            // host 分流重写 SHM 帧，不得弹本地窗口（否则与 host 窗双显）。
                            #[cfg(windows)]
                            let host_handled = match &host_render {
                                Some(hr) => try_host_render_candidates(hr, &mut candidate_window),
                                None => false,
                            };
                            #[cfg(not(windows))]
                            let host_handled = false;
                            if !host_handled {
                                candidate_window.show();
                            }
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
                    UiCommand::SetCandidateSwapWhenAbove(swap) => {
                        candidate_window.set_swap_preedit_when_above(swap);
                    }
                    UiCommand::SetPagerInPreedit(on) => {
                        candidate_window.set_pager_in_preedit(on);
                    }
                    UiCommand::SetPagerDisplay(mode) => {
                        candidate_window.set_pager_display(mode);
                    }
                    UiCommand::SetPageNumberDisplay(mode) => {
                        candidate_window.set_page_number_display(mode);
                    }
                    UiCommand::SetTooltipChaiziFont { path, family } => {
                        candidate_window.set_tooltip_chaizi_font(&path, &family);
                    }
                    UiCommand::RegisterGlobalHotkeys(entries) => {
                        #[cfg(windows)]
                        {
                            use windows::Win32::UI::Input::KeyboardAndMouse::{
                                HOT_KEY_MODIFIERS, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey,
                            };
                            // 覆盖式：先反注册旧列表（配置重载可能改键/删项），再注册新列表
                            for e in &global_hotkeys {
                                let _ = unsafe { UnregisterHotKey(HWND::default(), e.id) };
                            }
                            for e in &entries {
                                let mods = HOT_KEY_MODIFIERS(e.modifiers | MOD_NOREPEAT.0);
                                match unsafe { RegisterHotKey(HWND::default(), e.id, mods, e.vk) } {
                                    Ok(()) => debug!(
                                        "UI: registered global hotkey {} (mods=0x{:X} vk=0x{:02X})",
                                        e.action, e.modifiers, e.vk
                                    ),
                                    // 失败（组合被其它程序占用等）仅告警，不影响其余热键
                                    Err(err) => tracing::warn!(
                                        "UI: register global hotkey {} failed: {}",
                                        e.action,
                                        err
                                    ),
                                }
                            }
                            global_hotkeys = entries;
                        }
                        #[cfg(not(windows))]
                        {
                            let _ = entries;
                        }
                    }
                    #[cfg(windows)]
                    UiCommand::SetHostRender(hr) => {
                        debug!("UI: SetHostRender");
                        host_render = Some(hr.0);
                    }
                    UiCommand::Shutdown => {
                        info!("UI: Shutdown");
                        // host-render 全部隐藏（Shutdown 必达）
                        #[cfg(windows)]
                        if let Some(hr) = &host_render {
                            hr.hide_all();
                        }
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

        // 消息泵退出 = GUI 全部失效，而主线程同样不会察觉（见函数文档）。
        wind_config::startup_trace::stage("ui-thread-EXIT");
    }
}

impl Drop for UiManager {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(UiCommand::Shutdown);
    }
}

/// host-render 候选分流：有活跃目标时渲染候选帧（含悬停 tooltip 帧联动）写 SHM，
/// 本地窗口互斥隐藏（hide_local_window_only，保留跨帧防抖/粘滞状态）。
/// 返回 true = 已由 host 路径处理（调用方跳过本地 show）；false = 无目标/写帧失败 → 走本地路径。
#[cfg(windows)]
fn try_host_render_candidates(
    hr: &std::sync::Arc<wind_bridge::host_render_windows::HostRenderManager>,
    candidate_window: &mut CandidateWindow,
) -> bool {
    use wind_bridge::shared_render_frame::FrameParams;
    use wind_ipc::protocol::{HOST_WINDOW_CANDIDATE, HOST_WINDOW_TOOLTIP, HostRenderHitRect};
    let Some(target) = hr.active_target() else {
        return false;
    };
    match candidate_window.render_frame() {
        Some(frame) => {
            let rects: Vec<HostRenderHitRect> = frame
                .hit_rects
                .iter()
                .map(|(idx, r)| HostRenderHitRect {
                    // 翻页按钮的内部 tag（HOVER_PAGE_PREV/NEXT = 100000/100001）重映射为
                    // SHM/C++ 线约定（-1 上页 / -2 下页，HostWindow.cpp _HitTest）——与
                    // manager_macos.rs 的 darwin 重映射对齐。正数 tag 会被 C++ 当候选索引，
                    // 点击翻页变成 mouse_select(100000) 被丢弃（真机踩坑：翻页点击无效）。
                    index: match *idx {
                        i if i == HOVER_PAGE_PREV => -1,
                        i if i == HOVER_PAGE_NEXT => -2,
                        i => i,
                    },
                    x: r.x as i32,
                    y: r.y as i32,
                    w: r.w as i32,
                    h: r.h as i32,
                })
                .collect();
            let params = FrameParams {
                sequence: 0,
                x: frame.screen_x,
                y: frame.screen_y,
                width: frame.width,
                height: frame.height,
                bgra: &frame.buf,
                rects: &rects,
                // C++ 以此为 hover 去重基线（_UpdateHitRects → _lastHoverIndex），值域是
                // C++ hover 约定（-1 无 / -2 上页 / -3 下页）——内部 tag 须同步重映射。
                rendered_hover_index: match candidate_window.hover() {
                    i if i == HOVER_PAGE_PREV => -2,
                    i if i == HOVER_PAGE_NEXT => -3,
                    i => i,
                },
                target_instance_id: 0,
                software_shadow: frame.software_shadow,
            };
            match hr.write_frame_for_kind(HOST_WINDOW_CANDIDATE, &target, &params) {
                Ok(()) => {
                    candidate_window.hide_local_window_only();
                    // 悬停 tooltip 帧联动：有悬停写帧，无悬停隐藏（幂等）。
                    match candidate_window.render_tooltip_frame(frame.screen_x, frame.screen_y) {
                        Some((tt_buf, tt_w, tt_h, tt_x, tt_y, tt_shadow)) => {
                            let tt_params = FrameParams {
                                sequence: 0,
                                x: tt_x,
                                y: tt_y,
                                width: tt_w,
                                height: tt_h,
                                bgra: &tt_buf,
                                rects: &[],
                                rendered_hover_index: -1,
                                target_instance_id: 0,
                                software_shadow: tt_shadow,
                            };
                            if let Err(e) =
                                hr.write_frame_for_kind(HOST_WINDOW_TOOLTIP, &target, &tt_params)
                            {
                                tracing::warn!("host render 写 tooltip 帧失败: {}", e);
                                hr.hide_kind(HOST_WINDOW_TOOLTIP);
                            }
                        }
                        None => hr.hide_kind(HOST_WINDOW_TOOLTIP),
                    }
                    true
                }
                Err(e) => {
                    // 写帧失败必须回退本地窗口，不得静默丢帧
                    tracing::warn!("host render 写帧失败，回退本地窗口: {}", e);
                    false
                }
            }
        }
        None => {
            // 无内容可渲染：隐藏 host 侧 + 本地侧，幂等
            hr.hide_kind(HOST_WINDOW_CANDIDATE);
            hr.hide_kind(HOST_WINDOW_TOOLTIP);
            candidate_window.hide();
            true
        }
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

#[cfg(test)]
mod menu_id_tests {
    use super::*;

    /// `to_menu_id` 与 `from_menu_id` 是**两份手写的 match**，新增 MenuCmd 变体必须同时改两处。
    /// 漏掉 `from_menu_id` 那侧的表现是「点了菜单毫无反应」且不留任何日志——极难联想到 id 映射。
    /// 本测试锁住双向一致性，顺带也能抓出两个变体撞同一 id 的情况（撞号时 round-trip 必然不等）。
    ///
    /// ⚠ 编译器抓不到下面这张列表的遗漏：新增 MenuCmd 变体时请手动补一行。
    #[test]
    fn menu_cmd_id_roundtrip() {
        let all = [
            MenuCmd::SchemaEnglish,
            MenuCmd::TogglePunct,
            MenuCmd::ToggleWidth,
            MenuCmd::ToggleS2t,
            MenuCmd::ToggleToolbar,
            MenuCmd::ReloadConfig,
            MenuCmd::RestartService,
            MenuCmd::OpenConfigDir,
            MenuCmd::OpenAppDir,
            MenuCmd::OpenLogDir,
            MenuCmd::OpenDictionary,
            MenuCmd::OpenSettings,
            MenuCmd::OpenAbout,
            MenuCmd::TakeScreenshot,
            MenuCmd::ScreenshotCandidateToClipboard,
            MenuCmd::ToggleInputDiagnostics,
            MenuCmd::TogglePasswordSuppress,
            MenuCmd::FirstShowMode(0),
            MenuCmd::FirstShowMode(1),
            MenuCmd::FirstShowMode(2),
            MenuCmd::StatusToggleAlways,
            MenuCmd::StatusResetPosition,
            MenuCmd::StatusScreenshot,
            MenuCmd::StatusTogglePinned,
            MenuCmd::TooltipCopy,
            MenuCmd::TooltipScreenshot,
            MenuCmd::SchemaSelect(0),
            MenuCmd::SchemaSelect(7),
            MenuCmd::ThemeSelect(3),
            MenuCmd::FilterMode(2),
            MenuCmd::ThemeStyle(1),
        ];
        for cmd in all {
            let id = MenuKind::Command(cmd).to_menu_id();
            let back = MenuKind::from_menu_id(id).unwrap_or_else(|| {
                panic!("{cmd:?} → id={id} 无法反解析（from_menu_id 漏了该 id）")
            });
            assert_eq!(
                back,
                MenuKind::Command(cmd),
                "{cmd:?} → id={id} → {back:?}：双向映射不一致（多为 id 撞号）"
            );
        }
    }

    /// 不可点击项（分隔符 / 子菜单）恒为 0，且 0 不得反解析成任何动作——
    /// 否则点到分隔符会误触发某个命令。
    #[test]
    fn non_clickable_ids_are_inert() {
        assert_eq!(MenuKind::Separator.to_menu_id(), 0);
        assert_eq!(MenuKind::Submenu.to_menu_id(), 0);
        assert!(MenuKind::from_menu_id(0).is_none());
    }
}
