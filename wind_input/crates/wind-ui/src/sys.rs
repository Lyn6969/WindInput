//! 平台基础类型/常量/光标函数的统一入口（跨平台 shim）。
//!
//! Windows 上直接复用 `windows` crate 的真实类型与 API；其它平台（Linux/macOS）
//! 提供 mock 占位，使 UI 层（候选窗/工具栏/菜单）的鼠标消息处理代码能够在非
//! Windows 平台上**编译并跑测试**。这些 mock 在非 Windows 上属于死代码——平台没有
//! Win32 消息泵，on_message 不会被触发——仅用于满足类型/链接。
//!
//! 真正的平台行为差异（LayeredWindow、DirectWrite 文本、剪贴板、消息循环）由各模块
//! 自身的 `#[cfg]` 分支处理，本模块只兜底“句柄/常量/光标”这类基础符号。

#[cfg(windows)]
mod imp {
    pub use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    pub use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, ReleaseCapture, SetCapture, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    };
    pub use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, HWND_TOPMOST, IDC_ARROW, IDC_SIZEALL, LoadCursorW, SW_HIDE,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetCursor, SetWindowPos, ShowWindow,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_SETCURSOR,
    };
    // WM_MOUSELEAVE 不在 WindowsAndMessaging 模块内，直接以字面量定义（与 Win32 一致）。
    pub const WM_MOUSELEAVE: u32 = 0x02A3;
}

#[cfg(not(windows))]
#[allow(non_snake_case)] // mock 故意沿用 Win32 PascalCase 函数名，保持调用点与 Windows 一致
mod imp {
    //! 非 Windows mock。类型/常量值尽量贴近 Win32 语义，但仅用于编译占位。

    // ---- 基础句柄/消息类型 ----
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct HWND(pub isize);
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct WPARAM(pub usize);
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct LPARAM(pub isize);
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct LRESULT(pub isize);
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct HCURSOR(pub isize);
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct HINSTANCE(pub isize);

    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct POINT {
        pub x: i32,
        pub y: i32,
    }
    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    pub struct RECT {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    /// 光标资源名占位（对应 Win32 的 `PCWSTR` 资源 ID）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct CursorName(pub usize);

    /// 虚拟键码占位（对应 Win32 的 `VIRTUAL_KEY` newtype，`.0` 取值方式一致）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct VIRTUAL_KEY(pub u16);

    // ---- 常量 ----
    pub const VK_LBUTTON: VIRTUAL_KEY = VIRTUAL_KEY(0x01);
    pub const VK_RBUTTON: VIRTUAL_KEY = VIRTUAL_KEY(0x02);
    pub const VK_MBUTTON: VIRTUAL_KEY = VIRTUAL_KEY(0x04);
    pub const IDC_ARROW: CursorName = CursorName(32512);
    pub const IDC_SIZEALL: CursorName = CursorName(32646);
    pub const SW_HIDE: i32 = 0;
    pub const HWND_TOPMOST: HWND = HWND(-1);
    pub const SWP_NOSIZE: u32 = 0x0001;
    pub const SWP_NOZORDER: u32 = 0x0004;
    pub const SWP_NOACTIVATE: u32 = 0x0010;
    pub const WM_LBUTTONDOWN: u32 = 0x0201;
    pub const WM_LBUTTONUP: u32 = 0x0202;
    pub const WM_RBUTTONDOWN: u32 = 0x0204;
    pub const WM_MOUSEMOVE: u32 = 0x0200;
    pub const WM_MOUSEWHEEL: u32 = 0x020A;
    pub const WM_SETCURSOR: u32 = 0x0020;
    pub const WM_MOUSELEAVE: u32 = 0x02A3;

    // ---- 光标/窗口 API（mock：无副作用）----
    /// # Safety
    /// mock 不解引用指针；签名与 Win32 对齐以便调用点不变。
    pub unsafe fn GetCursorPos(_lppoint: *mut POINT) -> Result<(), ()> {
        Ok(())
    }
    pub unsafe fn LoadCursorW(_inst: Option<HINSTANCE>, _name: CursorName) -> Result<HCURSOR, ()> {
        Ok(HCURSOR(0))
    }
    pub unsafe fn SetCursor(_cursor: HCURSOR) -> HCURSOR {
        HCURSOR(0)
    }
    pub unsafe fn SetCapture(_hwnd: HWND) -> HWND {
        HWND(0)
    }
    pub unsafe fn ReleaseCapture() -> Result<(), ()> {
        Ok(())
    }
    /// mock：恒返回「未按下」。非 Windows 无全局按键状态可查，靠它驱动的轮询
    /// （菜单外点击）在此平台自然静默。
    pub unsafe fn GetAsyncKeyState(_vk: i32) -> i16 {
        0
    }
    pub unsafe fn GetWindowRect(_hwnd: HWND, _rect: *mut RECT) -> Result<(), ()> {
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn SetWindowPos(
        _hwnd: HWND,
        _after: HWND,
        _x: i32,
        _y: i32,
        _cx: i32,
        _cy: i32,
        _flags: u32,
    ) -> Result<(), ()> {
        Ok(())
    }
    pub unsafe fn ShowWindow(_hwnd: HWND, _cmd: i32) -> bool {
        true
    }
}

pub use imp::*;

/// 按**内容矩形**把窗口钳制在所在显示器内——软阴影允许溢出屏幕。
///
/// 与 [`clamp_to_work_area`] 有两处刻意的差别：
///
/// 1. **按内容而非窗口矩形**。软阴影画在窗口缓冲里，窗口矩形四周比可见内容大一圈
///    （`blur=8` 的主题约 29px）。若按窗口矩形钳制，可见内容离屏幕边缘还有整整一个
///    阴影宽度时就被拦下，用户感受是「明明还没到边就拖不过去了」。阴影是视觉修饰，
///    溢出屏幕不会有任何问题。
/// 2. **钳到 `rcMonitor` 而非 `rcWork`**，即允许摆到任务栏上方。候选窗是临时浮层，
///    用户可能正需要把它挪到任务栏那条让开正文区域；工具栏是常驻的，仍应避开任务栏，
///    故那边继续用 [`clamp_to_work_area`]。
///
/// 入参与返回都是**窗口**左上（含阴影），`margin` = (left, top, right, bottom) 扩边。
// 非 Windows 下无显示器查询，除 x/y 外的参数仅 Windows 分支使用。
#[cfg_attr(not(windows), allow(unused_variables))]
pub fn clamp_content_to_monitor(
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    margin: (i32, i32, i32, i32),
) -> (i32, i32) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        let (ml, mt, mr, mb) = margin;
        let (cw, ch) = (w as i32 - ml - mr, h as i32 - mt - mb);
        // 扩边大于窗口本身属异常数据，此时退回按整窗钳制，至少不会算出负尺寸。
        if cw <= 0 || ch <= 0 {
            return clamp_to_work_area(x, y, w, h);
        }
        let (cx, cy) = (x + ml, y + mt);
        unsafe {
            // 以内容中心选显示器：用左上角会让窗口刚过屏幕边界时归属跳变。
            let pt = POINT {
                x: cx + cw / 2,
                y: cy + ch / 2,
            };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let m = mi.rcMonitor;
                return clamp_content_in_bounds(
                    x,
                    y,
                    margin,
                    (cw, ch),
                    (m.left, m.top, m.right, m.bottom),
                );
            }
        }
    }
    (x, y)
}

/// [`clamp_content_to_monitor`] 的纯几何内核：把内容矩形钳进 `bounds`，返回**窗口**左上。
///
/// 抽出来是为了可测——真正要锁住的性质是「内容贴边时窗口左上可以为负」，即阴影被允许
/// 溢出屏幕。旧的按窗口矩形钳制永远算不出负值，那正是「拖不到边」的直接原因。
///
/// `bounds` = (left, top, right, bottom)；`content` = (宽, 高)。
fn clamp_content_in_bounds(
    x: i32,
    y: i32,
    margin: (i32, i32, i32, i32),
    content: (i32, i32),
    bounds: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (ml, mt, _, _) = margin;
    let (cw, ch) = content;
    let (bl, bt, br, bb) = bounds;
    let (cx, cy) = (x + ml, y + mt);
    // 先按右/下边界回拉，再按左/上边界兜底：内容比屏幕还大时保证左上角可见。
    let nx = (cx.min(br - cw)).max(bl);
    let ny = (cy.min(bb - ch)).max(bt);
    (nx - ml, ny - mt)
}

/// 将 (x,y,w,h) 钳制到所在（或最近）显示器工作区内，保证窗口完整可见。
/// 用于拖动窗口时防止拖出桌面/拖入任务栏，以及切换显示器 / 远程连接后旧坐标落到屏外时拉回。
/// 多显示器下 `MonitorFromPoint(NEAREST)` 会随光标过界切到目标显示器。
// 非 Windows 下无显示器工作区查询，w/h 仅 Windows 分支使用。
#[cfg_attr(not(windows), allow(unused_variables))]
pub fn clamp_to_work_area(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    #[cfg(windows)]
    {
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
        };
        unsafe {
            let pt = POINT { x, y };
            let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if GetMonitorInfoW(mon, &mut mi).as_bool() {
                let wa = mi.rcWork;
                let (wi, hi) = (w as i32, h as i32);
                let mut nx = x;
                let mut ny = y;
                if nx + wi > wa.right {
                    nx = wa.right - wi;
                }
                if ny + hi > wa.bottom {
                    ny = wa.bottom - hi;
                }
                if nx < wa.left {
                    nx = wa.left;
                }
                if ny < wa.top {
                    ny = wa.top;
                }
                return (nx, ny);
            }
        }
    }
    (x, y)
}

#[cfg(test)]
mod clamp_tests {
    use super::clamp_content_in_bounds;

    /// 1920×1080 主屏，_qingfeng 主题量级的阴影：内容 420×44，四周扩边 (29,29,29,33)，
    /// 故窗口为 478×106。
    const MARGIN: (i32, i32, i32, i32) = (29, 29, 29, 33);
    const CONTENT: (i32, i32) = (420, 44);
    const SCREEN: (i32, i32, i32, i32) = (0, 0, 1920, 1080);

    /// 内容贴左边缘时，**窗口左上必须为负**——阴影溢出屏幕正是本次修复的要点。
    /// 按窗口矩形钳制的旧实现永远算不出负值，内容会被卡在离屏幕边 29px 处。
    #[test]
    fn content_can_touch_screen_edge_with_shadow_overflowing() {
        // 试图拖到窗口左上为 (-200,-200)（远超边界）
        let (wx, wy) = clamp_content_in_bounds(-200, -200, MARGIN, CONTENT, SCREEN);
        assert_eq!(
            (wx + MARGIN.0, wy + MARGIN.1),
            (0, 0),
            "内容左上应恰好贴屏幕左上角"
        );
        assert_eq!((wx, wy), (-29, -29), "窗口左上为负 = 阴影溢出屏幕");
    }

    /// 内容贴右/下边缘：内容右边 = 1920、下边 = 1080（含任务栏区域）。
    #[test]
    fn content_can_touch_bottom_right_including_taskbar_area() {
        let (wx, wy) = clamp_content_in_bounds(9999, 9999, MARGIN, CONTENT, SCREEN);
        let (cx, cy) = (wx + MARGIN.0, wy + MARGIN.1);
        assert_eq!(
            (cx + CONTENT.0, cy + CONTENT.1),
            (1920, 1080),
            "内容右下贴屏幕右下"
        );
    }

    /// 未越界时原样返回，不得有任何偏移。
    #[test]
    fn in_bounds_position_is_untouched() {
        assert_eq!(
            clamp_content_in_bounds(500, 400, MARGIN, CONTENT, SCREEN),
            (500, 400)
        );
    }

    /// 副屏在主屏左侧（负坐标空间）同样成立。
    #[test]
    fn works_on_a_monitor_at_negative_coordinates() {
        let left_screen = (-1920, 0, 0, 1080);
        let (wx, wy) = clamp_content_in_bounds(-9999, 500, MARGIN, CONTENT, left_screen);
        assert_eq!(wx + MARGIN.0, -1920, "内容左边贴副屏左边缘");
        assert_eq!(wy, 500, "垂直方向未越界，保持不动");
    }

    /// 内容比可用区域还大时，优先保证**左上角**可见（否则窗口会被推到区域外）。
    /// 取 300×30，使 420×44 的内容在两个方向都放不下。
    #[test]
    fn oversized_content_keeps_top_left_visible() {
        let tiny = (0, 0, 300, 30);
        let (wx, wy) = clamp_content_in_bounds(100, 100, MARGIN, CONTENT, tiny);
        assert_eq!((wx + MARGIN.0, wy + MARGIN.1), (0, 0));
    }

    /// 只有一个方向放不下时，另一个方向必须保持不动——不能被"顺手"一起拉到边上。
    #[test]
    fn only_the_overflowing_axis_is_adjusted() {
        let narrow = (0, 0, 300, 1080); // 宽度放不下，高度绰绰有余
        let (wx, wy) = clamp_content_in_bounds(100, 400, MARGIN, CONTENT, narrow);
        assert_eq!(wx + MARGIN.0, 0, "水平超宽 → 贴左");
        assert_eq!(wy, 400, "垂直未越界 → 原样不动");
    }
}
