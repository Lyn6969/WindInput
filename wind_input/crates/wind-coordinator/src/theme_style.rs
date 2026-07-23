//! 主题明暗（亮 / 暗 / 跟随系统）的类型与系统明暗探测。
//!
//! # 为什么单独成型而不是继续用 `u8`
//!
//! 此前运行时明暗态是裸 `u8`（0 跟随 / 1 亮 / 2 暗），而三个消费点一律写成
//! `let dark = style == 2;`——「跟随系统」于是静默退化成亮色，且 `match` 的 `_` 通配分支
//! 让编译器全程沉默。改成枚举后 [`ThemeStyle::resolve_dark`] 是唯一的明暗出口，
//! 新增分支必须在此显式回答，漏处理会编译失败而不是悄悄按亮色跑。

use tracing::debug;

/// 主题明暗设置（`ui.theme.style` 的运行时形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeStyle {
    /// 跟随系统明暗（默认）
    #[default]
    System,
    /// 恒亮色
    Light,
    /// 恒暗色
    Dark,
}

impl ThemeStyle {
    /// 解析 `ui.theme.style` 配置值；未知值按跟随系统（与配置默认一致）。
    pub fn from_config(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }

    /// 回写 `ui.theme.style` 的配置值。
    pub fn as_config(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// 菜单协议编码（`MenuCmd::ThemeStyle(u8)`：0 跟随 / 1 亮 / 2 暗）。
    /// 该编码跨进程传给 macOS `.app` 的 `NSMenuItem.tag`，取值不可变更。
    pub fn as_menu_id(self) -> u8 {
        match self {
            Self::System => 0,
            Self::Light => 1,
            Self::Dark => 2,
        }
    }

    /// 由菜单协议编码还原；未知值按跟随系统。
    pub fn from_menu_id(id: u8) -> Self {
        match id {
            1 => Self::Light,
            2 => Self::Dark,
            _ => Self::System,
        }
    }

    /// 本次渲染该用暗色吗——主题解析的**唯一**明暗出口。
    ///
    /// `System` 每次调用都实时探测：系统明暗可在运行中被用户改掉，缓存会让
    /// 「切了系统主题但输入法没跟上」重新出现。探测本身是一次注册表读，量级远低于
    /// 随后的主题解析与重绘，不必优化。
    pub fn resolve_dark(self) -> bool {
        match self {
            Self::Light => false,
            Self::Dark => true,
            Self::System => system_prefers_dark(),
        }
    }

    /// 菜单/气泡展示名。
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "亮色",
            Self::Dark => "暗色",
        }
    }
}

/// 系统当前是否为深色模式。
///
/// Windows 读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize` 下的
/// `AppsUseLightTheme`（REG_DWORD，0=深色 / 1=浅色）。注意与同键下的 `SystemUsesLightTheme`
/// 区分：后者管任务栏与开始菜单，前者才是「应用」的明暗——输入法浮层属应用范畴。
///
/// 键在 Win10 1803 之前不存在，且用户可能从未动过明暗设置（此时值缺失）。缺失与读取失败
/// 一律按浅色：深色是更"重"的假设，猜错会让浅底主题上叠深色文本导致不可读，浅色兜底更安全。
#[cfg(windows)]
pub fn system_prefers_dark() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let dark = hkcu
        .open_subkey(KEY)
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false);
    debug!("系统明暗探测: AppsUseLightTheme → dark={}", dark);
    dark
}

/// 非 Windows 恒浅色：macOS 的明暗由 `.app` 侧 `effectiveAppearance` 决定，尚未接回本进程。
#[cfg(not(windows))]
pub fn system_prefers_dark() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip() {
        for s in [ThemeStyle::System, ThemeStyle::Light, ThemeStyle::Dark] {
            assert_eq!(ThemeStyle::from_config(s.as_config()), s);
            assert_eq!(ThemeStyle::from_menu_id(s.as_menu_id()), s);
        }
        // 大小写与空白容错（配置可手写）
        assert_eq!(ThemeStyle::from_config("  Dark "), ThemeStyle::Dark);
        // 未知值与空值回落跟随系统（与 config.toml 默认一致）
        assert_eq!(ThemeStyle::from_config(""), ThemeStyle::System);
        assert_eq!(ThemeStyle::from_config("auto"), ThemeStyle::System);
        // 菜单协议编码固定，跨进程传给 macOS .app，不可变更
        assert_eq!(ThemeStyle::System.as_menu_id(), 0);
        assert_eq!(ThemeStyle::Light.as_menu_id(), 1);
        assert_eq!(ThemeStyle::Dark.as_menu_id(), 2);
    }

    #[test]
    fn resolve_dark_explicit_ignores_system() {
        // 显式明暗不受系统设置影响；System 与实时探测一致。
        assert!(!ThemeStyle::Light.resolve_dark());
        assert!(ThemeStyle::Dark.resolve_dark());
        assert_eq!(ThemeStyle::System.resolve_dark(), system_prefers_dark());
    }
}
