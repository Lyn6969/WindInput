//! DPI 缩放：按目标显示器实时计算，支持多显示器不同缩放的动态切换。
//!
//! Per-Monitor DPI 感知下，窗口跨显示器后缩放因子会变化。旧实现用
//! `GetDC(HWND::default())` 取的是**主显示器** DPI，且只在窗口 `new()` 时读一次缓存，
//! 换显示器后既拿不到目标显示器 DPI、也不会更新 —— 这就是"切显示器缩放不对"的根因。
//!
//! 这里按"窗口将要显示的点"实时取其所在显示器的有效 DPI；各瞬态窗口（候选/气泡/菜单）
//! 在 `show()` 时调用即可随光标所在显示器自动适配。

/// 取得点 (x, y) 所在显示器的有效 DPI 缩放（96dpi = 1.0）。
/// 失败回退 1.0。
#[cfg(windows)]
pub fn scale_for_point(x: i32, y: i32) -> f32 {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut dpi_x: u32 = 0;
        let mut dpi_y: u32 = 0;
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok() && dpi_y > 0 {
            return dpi_y as f32 / 96.0;
        }
        1.0
    }
}

#[cfg(not(windows))]
pub fn scale_for_point(_x: i32, _y: i32) -> f32 {
    1.0
}
