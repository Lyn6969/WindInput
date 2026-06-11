//! UI 管理器 + 消息循环
//!
//! 与 Go 版本 `wind_input/internal/ui/manager.go` 对齐。

use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

/// UI 命令
pub enum UiCommand {
    ShowCandidates { x: i32, y: i32 },
    HideCandidates,
    UpdateComposition(String),
    ShowToast(String),
    Shutdown,
}

/// UI 管理器
pub struct UiManager {
    cmd_tx: mpsc::Sender<UiCommand>,
}

impl UiManager {
    pub fn new() -> anyhow::Result<(Self, mpsc::Receiver<UiCommand>)> {
        let (tx, rx) = mpsc::channel(256);
        Ok((Self { cmd_tx: tx }, rx))
    }

    pub fn sender(&self) -> mpsc::Sender<UiCommand> {
        self.cmd_tx.clone()
    }
}
