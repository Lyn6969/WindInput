//! Win32 Layered Window 封装
//!
//! 用于候选窗口、工具栏等浮层。使用 UpdateLayeredWindow 实现透明渲染。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use tracing::{debug, error, info};

/// 浮层窗口鼠标消息处理器（由具体窗口实现，如候选窗）。
/// 返回 `Some(lresult)` 表示已处理；`None` 交回默认处理。
pub trait WindowMouse {
    fn on_message(&mut self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM)
        -> Option<LRESULT>;
}

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
            let instance = GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {}", e))?;

            let class_wide: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();

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
            let hbitmap = CreateDIBSection(
                hdc_mem,
                &bmi,
                DIB_RGB_COLORS,
                &mut bits_ptr,
                None,
                0,
            )
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
            DeleteObject(hbitmap);
            DeleteDC(hdc_mem);
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
        DefWindowProcW(hwnd, msg, wparam, lparam)
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
