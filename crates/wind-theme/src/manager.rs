//! 主题加载管理器
//!
//! 与 Go 版本 `wind_input/pkg/theme/manager.go` 对齐。

use crate::resolved::ResolvedV3;
use crate::theme::Theme;
use std::path::PathBuf;
use std::sync::RwLock;

/// 主题管理器
pub struct ThemeManager {
    themes_dir: PathBuf,
    current: RwLock<Option<ResolvedV3>>,
}

impl ThemeManager {
    pub fn new(themes_dir: PathBuf) -> Self {
        Self {
            themes_dir,
            current: RwLock::new(None),
        }
    }

    /// 加载主题
    pub fn load_theme(&self, _name: &str) -> anyhow::Result<()> {
        // TODO
        Ok(())
    }

    /// 获取当前已解析的主题
    pub fn get_resolved(&self) -> Option<ResolvedV3> {
        self.current.read().unwrap().clone()
    }

    /// 设置暗色模式
    pub fn set_dark_mode(&self, _is_dark: bool) {
        // TODO: 重新解析当前主题
    }
}
