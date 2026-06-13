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
        selected: usize,
        caret_x: i32,
        caret_y: i32,
    },
    /// 隐藏候选窗口
    HideCandidates,
    /// 显示状态提示气泡（中英/标点/全半角/方案切换），约 1 秒后自动隐藏
    ShowStatusTip { text: String, x: i32, y: i32 },
    /// 关闭 UI
    Shutdown,
}

/// UI 管理器（在独立线程中运行）
pub struct UiManager {
    cmd_tx: mpsc::Sender<UiCommand>,
    _thread: std::thread::JoinHandle<()>,
}

impl UiManager {
    pub fn new() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<UiCommand>();

        let thread = std::thread::Builder::new()
            .name("ui-manager".into())
            .spawn(move || {
                Self::ui_thread(rx);
            })?;

        Ok(Self {
            cmd_tx: tx,
            _thread: thread,
        })
    }

    pub fn sender(&self) -> mpsc::Sender<UiCommand> {
        self.cmd_tx.clone()
    }

    /// UI 线程主循环
    fn ui_thread(rx: mpsc::Receiver<UiCommand>) {
        // 创建候选窗口
        let config = CandidateWindowConfig::default();
        let mut candidate_window = match CandidateWindow::new(config) {
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
                            caret_x,
                            caret_y,
                        } => {
                            debug!(
                                "UI: UpdateCandidates ({} items, selected={}, pos={},{})",
                                candidates.len(),
                                selected,
                                caret_x,
                                caret_y
                            );
                            candidate_window.update(&preedit, candidates, selected);
                            candidate_window.set_position(caret_x, caret_y);
                            candidate_window.show();
                        }
                        UiCommand::HideCandidates => {
                            debug!("UI: HideCandidates");
                            candidate_window.hide();
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
                        UiCommand::Shutdown => {
                            info!("UI: Shutdown");
                            candidate_window.hide();
                            if let Some(t) = &status_tip {
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
