//! wind-coordinator: 中央协调器（按键路由、候选管理、模式切换）
//!
//! 与 Go 版本 `wind_input/internal/coordinator/` 对齐。

pub mod coordinator;
pub mod handle_addword;
pub mod handle_candidate;
pub mod handle_cmdbar;
pub mod handle_config;
pub mod handle_key;
pub mod handle_lifecycle;
pub mod handle_menu;
pub mod handle_mode;
pub mod handle_punct;
pub mod handle_special;
pub mod handle_temp;
pub mod handle_tooltip;
pub mod handle_url;
pub mod hotkey_match;
pub mod pipeline;
pub mod stats;
pub mod watchdog;
pub mod webdata;

pub use coordinator::{Coordinator, request_restart, restart_signal, set_settings_url_provider};

/// 前台窗口是否全屏（供工具栏 ui.toolbar.hide_in_fullscreen 判定）。
/// 对齐 Go foreground.IsForegroundFullscreen:① SHQueryUserNotificationState 报 D3D 独占/演示模式;
/// ② 前台窗口矩形 ⊇ 所在显示器物理矩形(F11/无边框全屏/远程桌面)。排除桌面/Shell 窗口。
/// 非 Windows 恒 false。
#[cfg(windows)]
pub(crate) fn is_foreground_fullscreen() -> bool {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::Shell::{
        QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == HWND::default() || hwnd == GetDesktopWindow() || hwnd == GetShellWindow() {
            return false;
        }
        // 判据①:系统通知状态(游戏 D3D 独占 / PPT 放映等系统级全屏)。
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
                return true;
            }
        }
        // 判据②:前台窗口矩形 ⊇ 显示器物理矩形。
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &mut wr).is_err() {
            return false;
        }
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        wr.left <= m.left && wr.top <= m.top && wr.right >= m.right && wr.bottom >= m.bottom
    }
}

/// 非 Windows:无全屏检测,恒 false(工具栏不因全屏隐藏)。
#[cfg(not(windows))]
pub(crate) fn is_foreground_fullscreen() -> bool {
    false
}
