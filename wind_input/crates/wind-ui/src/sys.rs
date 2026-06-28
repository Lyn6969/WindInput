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
    pub use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
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

    // ---- 常量 ----
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
