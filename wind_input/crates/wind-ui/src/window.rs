//! Win32 Layered Window 封装（跨平台）
//!
//! 用于候选窗口、工具栏等浮层。Windows 上使用 UpdateLayeredWindow 实现透明渲染；
//! 非 Windows 平台提供 mock 实现（持有 BGRA 缓冲区，show/hide/update 为空操作），
//! 使上层窗口逻辑能在 Linux 上编译与跑测试。

use std::cell::RefCell;
use std::rc::Rc;

use crate::sys::{HWND, LPARAM, LRESULT, WPARAM};

/// 浮层窗口鼠标消息处理器（由具体窗口实现，如候选窗）。
/// 返回 `Some(lresult)` 表示已处理；`None` 交回默认处理。
///
/// 非 Windows 平台上没有 Win32 消息泵，该 trait 的实现不会被调用，仅用于类型占位。
pub trait WindowMouse {
    fn on_message(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT>;
}

#[cfg(windows)]
mod platform {
    use super::{Rc, RefCell, WindowMouse};
    use std::collections::HashMap;
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    thread_local! {
        /// hwnd → 鼠标处理器（仅 UI 线程访问，wnd_proc 与窗口同线程）
        static MOUSE_HANDLERS: RefCell<HashMap<isize, Rc<RefCell<dyn WindowMouse>>>> =
            RefCell::new(HashMap::new());
    }

    /// Layered Window 封装
    pub struct LayeredWindow {
        hwnd: HWND,
        width: u32,
        height: u32,
        /// BGRA 像素缓冲区
        buffer: Vec<u8>,
    }

    impl LayeredWindow {
        pub fn create(
            parent: Option<HWND>,
            width: u32,
            height: u32,
            class_name: &str,
        ) -> Result<Self, String> {
            unsafe {
                let instance =
                    GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {}", e))?;

                let class_wide: Vec<u16> = class_name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();

                // 加载箭头光标（避免鼠标繁忙状态）
                let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

                let wnd_class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(Self::wnd_proc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: instance.into(),
                    hbrBackground: HBRUSH::default(),
                    lpszMenuName: windows::core::PCWSTR::null(),
                    lpszClassName: windows::core::PCWSTR(class_wide.as_ptr()),
                    hIcon: HICON::default(),
                    hIconSm: HICON::default(),
                    hCursor: cursor,
                };

                RegisterClassExW(&wnd_class);

                let style = WS_POPUP;
                let ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;

                let hwnd = CreateWindowExW(
                    ex_style,
                    windows::core::PCWSTR(class_wide.as_ptr()),
                    windows::core::PCWSTR(class_wide.as_ptr()),
                    style,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    parent.unwrap_or(HWND::default()),
                    HMENU::default(),
                    instance,
                    None,
                )
                .map_err(|e| format!("CreateWindowExW: {}", e))?;

                let buffer = vec![0u8; (width * height * 4) as usize];

                Ok(Self {
                    hwnd,
                    width,
                    height,
                    buffer,
                })
            }
        }

        pub fn hwnd(&self) -> HWND {
            self.hwnd
        }

        /// 注册鼠标处理器（绑定到本窗口 hwnd）
        pub fn register_mouse(&self, handler: Rc<RefCell<dyn WindowMouse>>) {
            let key = self.hwnd.0 as isize;
            MOUSE_HANDLERS.with(|m| {
                m.borrow_mut().insert(key, handler);
            });
        }

        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }

        pub fn buffer_mut(&mut self) -> &mut [u8] {
            &mut self.buffer
        }

        pub fn resize(&mut self, width: u32, height: u32) {
            self.width = width;
            self.height = height;
            self.buffer.resize((width * height * 4) as usize, 0);
        }

        pub fn update(&self) -> Result<(), String> {
            unsafe {
                let hdc_screen = GetDC(HWND::default());
                let hdc_mem = CreateCompatibleDC(hdc_screen);

                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: self.width as i32,
                        biHeight: -(self.height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        biSizeImage: 0,
                        biXPelsPerMeter: 0,
                        biYPelsPerMeter: 0,
                        biClrUsed: 0,
                        biClrImportant: 0,
                    },
                    ..std::mem::zeroed()
                };

                let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                let hbitmap =
                    CreateDIBSection(hdc_mem, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
                        .map_err(|e| format!("CreateDIBSection: {}", e))?;

                std::ptr::copy_nonoverlapping(
                    self.buffer.as_ptr(),
                    bits_ptr as *mut u8,
                    self.buffer.len(),
                );

                let old_bmp = SelectObject(hdc_mem, hbitmap);

                let size = SIZE {
                    cx: self.width as i32,
                    cy: self.height as i32,
                };
                let source = POINT { x: 0, y: 0 };

                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };

                let result = UpdateLayeredWindow(
                    self.hwnd,
                    hdc_screen,
                    None,
                    Some(&size),
                    hdc_mem,
                    Some(&source),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                );

                SelectObject(hdc_mem, old_bmp);
                let _ = DeleteObject(hbitmap);
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(HWND::default(), hdc_screen);

                if result.is_err() {
                    return Err(format!("UpdateLayeredWindow: {:?}", result));
                }

                Ok(())
            }
        }

        pub fn show(&self, x: i32, y: i32) {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    x,
                    y,
                    self.width as i32,
                    self.height as i32,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            }
        }

        pub fn hide(&self) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }

        pub fn clear(&mut self) {
            self.buffer.fill(0);
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        /// 将当前 BGRA buffer 保存为 PNG 文件（截图用）。
        pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
            crate::screenshot::save_bgra_to_png(&self.buffer, self.width, self.height, path)
        }

        /// 将当前 BGRA buffer 复制到剪贴板（截图用）。
        pub fn capture_to_clipboard(&self) -> Result<(), String> {
            crate::screenshot::copy_bgra_to_clipboard(&self.buffer, self.width, self.height)
        }

        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            // 不抢焦点：点击浮层不激活窗口，目标应用保持前台
            if msg == WM_MOUSEACTIVATE {
                return LRESULT(MA_NOACTIVATE as isize);
            }
            // 命中测试：返回 HTCLIENT 才能收到鼠标消息
            if msg == WM_NCHITTEST {
                return LRESULT(HTCLIENT as isize);
            }
            // 鼠标相关消息派发给已注册处理器（先取出 Rc 释放注册表借用，避免重入冲突）
            if matches!(
                msg,
                WM_LBUTTONDOWN
                    | WM_LBUTTONUP
                    | WM_RBUTTONDOWN
                    | WM_MOUSEMOVE
                    | crate::sys::WM_MOUSELEAVE
                    | WM_MOUSEWHEEL
                    | WM_SETCURSOR
            ) {
                let key = hwnd.0 as isize;
                let handler = MOUSE_HANDLERS.with(|m| m.borrow().get(&key).cloned());
                if let Some(h) = handler {
                    if let Some(lr) = h.borrow_mut().on_message(hwnd, msg, wparam, lparam) {
                        return lr;
                    }
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }

    impl Drop for LayeredWindow {
        fn drop(&mut self) {
            let key = self.hwnd.0 as isize;
            MOUSE_HANDLERS.with(|m| {
                m.borrow_mut().remove(&key);
            });
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{HWND, Rc, RefCell, WindowMouse};

    /// Layered Window 的非 Windows mock：持有 BGRA 缓冲区，窗口操作为空实现。
    pub struct LayeredWindow {
        width: u32,
        height: u32,
        buffer: Vec<u8>,
        /// 保留注册的鼠标处理器以维持 API 一致（非 Windows 下永不触发）。
        _mouse: RefCell<Option<Rc<RefCell<dyn WindowMouse>>>>,
    }

    impl LayeredWindow {
        pub fn create(
            _parent: Option<HWND>,
            width: u32,
            height: u32,
            _class_name: &str,
        ) -> Result<Self, String> {
            Ok(Self {
                width,
                height,
                buffer: vec![0u8; (width * height * 4) as usize],
                _mouse: RefCell::new(None),
            })
        }

        pub fn hwnd(&self) -> HWND {
            HWND::default()
        }

        pub fn register_mouse(&self, handler: Rc<RefCell<dyn WindowMouse>>) {
            *self._mouse.borrow_mut() = Some(handler);
        }

        pub fn buffer(&self) -> &[u8] {
            &self.buffer
        }

        pub fn buffer_mut(&mut self) -> &mut [u8] {
            &mut self.buffer
        }

        pub fn resize(&mut self, width: u32, height: u32) {
            self.width = width;
            self.height = height;
            self.buffer.resize((width * height * 4) as usize, 0);
        }

        pub fn update(&self) -> Result<(), String> {
            Ok(())
        }

        pub fn show(&self, _x: i32, _y: i32) {}

        pub fn hide(&self) {}

        pub fn clear(&mut self) {
            self.buffer.fill(0);
        }

        pub fn size(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        pub fn capture_to_file(&self, path: &std::path::Path) -> Result<(), String> {
            crate::screenshot::save_bgra_to_png(&self.buffer, self.width, self.height, path)
        }

        pub fn capture_to_clipboard(&self) -> Result<(), String> {
            crate::screenshot::copy_bgra_to_clipboard(&self.buffer, self.width, self.height)
        }
    }
}

pub use platform::LayeredWindow;

// 非 Windows mock 的冒烟测试：仅验证 mock 的缓冲区契约（尺寸/resize/clear）。
// 边界：真实 Layered Window 行为（UpdateLayeredWindow 透明渲染、show/hide 定位、
// wnd_proc 鼠标消息分发）在非 Windows 是空实现，**不在此覆盖，须 Windows 实测**。
#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn mock_window_buffer_matches_size() {
        let mut w = LayeredWindow::create(None, 10, 4, "test").unwrap();
        assert_eq!(w.size(), (10, 4));
        assert_eq!(w.buffer().len(), 10 * 4 * 4);

        w.resize(20, 5);
        assert_eq!(w.size(), (20, 5));
        assert_eq!(w.buffer().len(), 20 * 5 * 4);

        // buffer_mut 写入后 clear 应清零
        w.buffer_mut()[0] = 0xAB;
        assert_eq!(w.buffer()[0], 0xAB);
        w.clear();
        assert!(w.buffer().iter().all(|&b| b == 0));
    }

    #[test]
    fn mock_window_show_hide_update_are_noops() {
        let w = LayeredWindow::create(None, 2, 2, "test").unwrap();
        w.show(1, 1);
        w.hide();
        assert!(w.update().is_ok());
    }
}
