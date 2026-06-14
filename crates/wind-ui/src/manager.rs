//! UI 管理器 + 消息循环
//!
//! 与 Go 版本 `wind_input/internal/ui/manager.go` 对齐。
//! 在独立线程中运行 Win32 消息循环，通过通道接收 UI 更新命令。

use crate::candidate_window::{CandidateItem, CandidateWindow, CandidateWindowConfig};
use std::sync::mpsc;
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::*;

/// UI 命令
#[derive(Debug)]
pub enum UiCommand {
    /// 更新候选列表
    UpdateCandidates {
        preedit: String,
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
    },
    /// 隐藏候选窗口
    HideCandidates,
    /// 显示状态提示气泡（中英/标点/全半角/方案切换），约 1 秒后自动隐藏
    ShowStatusTip { text: String, x: i32, y: i32 },
    /// 更新常驻工具栏状态（中英/方案/标点/全半角）
    UpdateToolbar(crate::toolbar::ToolbarState),
    /// 隐藏工具栏
    HideToolbar,
    /// 显示右键候选菜单（协调器构建好菜单项后下发）
    ShowCandidateMenu {
        items: Vec<MenuItemSpec>,
        x: i32,
        y: i32,
        /// 初始高亮项（菜单项下标）
        selected: usize,
    },
    /// 更新菜单高亮项（键盘/悬停导航时，仅重绘不移位）
    UpdateMenuHighlight(usize),
    /// 隐藏右键菜单
    HideMenu,
    /// 写剪贴板（菜单"复制"由协调器驱动 → UI 侧执行）
    CopyToClipboard(String),
    /// 关闭 UI
    Shutdown,
}

/// 工具栏单元格动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarAction {
    /// 中/英切换
    ToggleMode,
    /// 切换输入方案
    SwitchEngine,
    /// 中/英标点切换
    TogglePunct,
    /// 全/半角切换
    ToggleWidth,
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

/// 右键候选菜单项的动作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind {
    /// 词条操作（置顶/移动/删除/恢复）
    Op(CandidateOp),
    /// 复制候选文本（UI 侧写剪贴板）
    Copy,
    /// 分隔线（不可点击）
    Separator,
}

/// 菜单项规格（由协调器构建，含启用态）
#[derive(Debug, Clone)]
pub struct MenuItemSpec {
    pub label: String,
    pub kind: MenuKind,
    pub enabled: bool,
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
    /// 候选词条操作（页内下标 + 动作）
    CandidateOp { op: CandidateOp, page_local: usize },
    /// 右键候选请求弹出菜单（页内下标 + 屏幕坐标）；协调器据此构建菜单项回送
    RequestCandidateMenu { page_local: usize, x: i32, y: i32 },
    /// 菜单内鼠标悬停项（-1 表示无）→ 协调器更新高亮
    MenuHover(i32),
    /// 菜单项点击激活（菜单项下标）
    MenuActivate(usize),
    /// 关闭菜单（点击菜单外 / 右键）
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
        loop {
            // 状态提示气泡到期自动隐藏
            if let Some(deadline) = tip_hide_at {
                if std::time::Instant::now() >= deadline {
                    if let Some(t) = &status_tip {
                        t.hide();
                    }
                    tip_hide_at = None;
                }
            }
            // 非阻塞处理 Win32 消息
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // 非阻塞接收 UI 命令
            match rx.try_recv() {
                Ok(cmd) => {
                    match cmd {
                        UiCommand::UpdateCandidates {
                            preedit,
                            candidates,
                            selected,
                            hover,
                            page,
                            total_pages,
                            caret_x,
                            caret_y,
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
                            candidate_window.update(&preedit, candidates, selected, hover, page, total_pages);
                            candidate_window.set_position(caret_x, caret_y);
                            candidate_window.show();
                        }
                        UiCommand::HideCandidates => {
                            debug!("UI: HideCandidates");
                            candidate_window.hide();
                            if let Some(m) = &mut popup_menu {
                                m.hide();
                            }
                        }
                        UiCommand::ShowCandidateMenu { items, x, y, selected } => {
                            debug!("UI: ShowCandidateMenu ({} items) at ({},{})", items.len(), x, y);
                            if let Some(m) = &mut popup_menu {
                                m.show(items, x, y, selected);
                            }
                        }
                        UiCommand::UpdateMenuHighlight(sel) => {
                            if let Some(m) = &mut popup_menu {
                                m.set_highlight(sel);
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
                        UiCommand::ShowStatusTip { text, x, y } => {
                            debug!("UI: ShowStatusTip '{}' at ({},{})", text, x, y);
                            if let Some(t) = &mut status_tip {
                                t.show(&text, x, y);
                                tip_hide_at = Some(
                                    std::time::Instant::now()
                                        + std::time::Duration::from_millis(1000),
                                );
                            }
                        }
                        UiCommand::UpdateToolbar(tb_state) => {
                            debug!("UI: UpdateToolbar {:?}", tb_state);
                            if let Some(t) = &mut toolbar {
                                t.update(&tb_state);
                            }
                        }
                        UiCommand::HideToolbar => {
                            debug!("UI: HideToolbar");
                            if let Some(t) = &mut toolbar {
                                t.hide();
                            }
                        }
                        UiCommand::Shutdown => {
                            info!("UI: Shutdown");
                            candidate_window.hide();
                            if let Some(t) = &status_tip {
                                t.hide();
                            }
                            if let Some(t) = &mut toolbar {
                                t.hide();
                            }
                            break;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // 没有命令，短暂休眠避免 CPU 空转
                    std::thread::sleep(std::time::Duration::from_millis(8));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    info!("UI: Channel disconnected, shutting down");
                    break;
                }
            }
        }
    }
}

impl Drop for UiManager {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(UiCommand::Shutdown);
    }
}
