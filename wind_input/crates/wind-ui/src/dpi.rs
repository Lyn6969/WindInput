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
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
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

/// macOS：取点 (x,y) 所在显示器的 backing 缩放（Retina = 2.0）。
///
/// 用 Core Graphics 的 `CGDisplay`（**不依赖主线程**，故可在 forwarder 工作线程调用——
/// 这正是 render_frame 的运行线程；`NSScreen::backingScaleFactor` 需主线程，在此恒失败回退 1.0，
/// 故弃用）。缩放 = 像素宽 / 点宽（`pixels_wide / bounds.width`）。
/// 命中点的显示器；无命中（点在屏外）或取屏失败回退主屏，再失败回退 1.0。
#[cfg(target_os = "macos")]
pub fn scale_for_point(x: i32, y: i32) -> f32 {
    use core_graphics::display::CGDisplay;
    use core_graphics::geometry::CGPoint;
    let display = CGDisplay::displays_with_point(CGPoint::new(x as f64, y as f64), 1)
        .ok()
        .and_then(|(ids, _count)| ids.first().copied())
        .map(CGDisplay::new)
        .unwrap_or_else(CGDisplay::main);
    // 注意：CGDisplayPixelsWide（display.pixels_wide()）返回的是【点】宽，Retina 上不是
    // 原生像素，故 pixels_wide/bounds.width 恒=1 → 候选框按 1x 渲染、Retina 上模糊。
    // 正确：用当前显示【模式】的原生像素宽 / 点宽得 backing 倍率（Retina=2）。
    if let Some(mode) = display.display_mode() {
        let px = mode.pixel_width() as f64;
        let pt = mode.width() as f64;
        if pt > 0.0 {
            let s = (px / pt) as f32;
            if s >= 1.0 {
                return s;
            }
        }
    }
    1.0
}

#[cfg(all(not(windows), not(target_os = "macos")))]
pub fn scale_for_point(_x: i32, _y: i32) -> f32 {
    1.0
}

#[cfg(target_os = "macos")]
#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟：缩放至少 1.0（CGDisplay 不依赖主线程，Retina 上应为 2.0，非 Retina 1.0）。
    #[test]
    fn scale_for_point_at_least_one() {
        let s = scale_for_point(0, 0);
        assert!(s >= 1.0, "scale 应 >= 1.0，实际 {s}");
    }
}
