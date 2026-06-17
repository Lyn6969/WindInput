//! 主题加载管理器
//!
//! 与 Go 版本 `wind_input/pkg/theme/manager.go` 对齐。
//! 负责按名加载主题并缓存当前已解析结果，支持暗色模式重解析。

use crate::resolved::ResolvedTheme;
use std::path::PathBuf;
use std::sync::RwLock;

/// 主题管理器
pub struct ThemeManager {
    themes_dir: PathBuf,
    name: RwLock<String>,
    is_dark: RwLock<bool>,
    current: RwLock<ResolvedTheme>,
}

impl ThemeManager {
    pub fn new(themes_dir: PathBuf) -> Self {
        Self {
            themes_dir,
            name: RwLock::new("default".into()),
            is_dark: RwLock::new(false),
            current: RwLock::new(ResolvedTheme::default()),
        }
    }

    /// 加载主题（失败保留旧主题，返回错误）。
    pub fn load(&self, name: &str, is_dark: bool) -> anyhow::Result<()> {
        let t = ResolvedTheme::load(&self.themes_dir, name, is_dark)?;
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = t;
        *self.name.write().unwrap_or_else(|e| e.into_inner()) = name.to_string();
        *self.is_dark.write().unwrap_or_else(|e| e.into_inner()) = is_dark;
        Ok(())
    }

    /// 当前已解析主题（副本）。
    pub fn resolved(&self) -> ResolvedTheme {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 切换暗色模式（用当前主题名重解析）。
    pub fn set_dark_mode(&self, is_dark: bool) {
        let name = self.name.read().unwrap_or_else(|e| e.into_inner()).clone();
        let _ = self.load(&name, is_dark);
    }
}
