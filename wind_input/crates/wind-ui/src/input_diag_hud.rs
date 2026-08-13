//! 输入诊断 HUD：非激活置顶浮窗，右键「高级」开关控制显隐，可拖动，双击复制。
//!
//! 复用 [`crate::window::LayeredWindow`]（默认已带 `WS_EX_NOACTIVATE | WS_EX_TOPMOST |
//! WS_EX_TOOLWINDOW | WS_EX_LAYERED`：不进任务栏、不抢焦点、透明渲染）+ [`TextRenderer`] +
//! [`crate::view::View`] 盒模型，仿 `status_tip.rs`/`toast.rs`。MVP 用固定深色半透明底 + 白字，
//! 不接主题。拖动/双击复制在窗口过程（`wnd_proc`）经 [`WindowMouse`] 处理。

use crate::text::dwrite::TextRenderer;
use crate::view::{Align, Edges, Layout, View};
use crate::window::LayeredWindow;

/// 分区显示开关（右键菜单「显示分类」）。
///
/// 默认**全开**——诊断工具的默认形态应该是「什么都看得见」，隐藏是用户为了省地方
/// 主动做的选择。故手写 `Default` 而非 `derive`（derive 会给出全 false = 空窗口）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiagSections {
    /// 进程 / 禁用态 / InputScope / 激活位。
    pub input: bool,
    /// 焦点·顶层·前台窗口链。
    pub window: bool,
    /// DocMgr / Context / 会话号。
    pub tsf: bool,
    /// 白名单 / 活跃 / band。
    pub host: bool,
}

impl Default for DiagSections {
    fn default() -> Self {
        Self {
            input: true,
            window: true,
            tsf: true,
            host: true,
        }
    }
}

impl DiagSections {
    /// 按分区序号取值（0=输入态 1=窗口 2=TSF 3=HostRender）。序号是菜单 id 的载荷，
    /// 越界返回 true（不隐藏）——未知序号让内容照常显示，比让它凭空消失安全。
    pub fn get(&self, idx: u8) -> bool {
        match idx {
            0 => self.input,
            1 => self.window,
            2 => self.tsf,
            3 => self.host,
            _ => true,
        }
    }

    /// 翻转指定分区；越界为 no-op。
    pub fn toggle(&mut self, idx: u8) {
        match idx {
            0 => self.input = !self.input,
            1 => self.window = !self.window,
            2 => self.tsf = !self.tsf,
            3 => self.host = !self.host,
            _ => {}
        }
    }

    /// 分区序号 → 菜单标签。与 [`Self::get`]/[`Self::toggle`] 的序号是同一套。
    pub fn label(idx: u8) -> &'static str {
        match idx {
            0 => "输入态",
            1 => "窗口",
            2 => "TSF",
            3 => "HostRender",
            _ => "?",
        }
    }

    /// 全部分区序号（菜单构建用，保证不漏项）。
    pub const ALL: [u8; 4] = [0, 1, 2, 3];
}

#[derive(Clone, Debug)]
pub struct InputDiagView {
    pub process_name: String,
    pub pid: u32,
    pub disabled: bool,
    pub reason_text: String,
    pub mask: u64,
    /// 本输入法是否在为某宿主服务（协调器 `State::ime_active`）。
    pub ime_active: bool,
    /// 焦点是否落在可编辑控件里（协调器 `State::has_edit_context`）。
    ///
    /// 与 `ime_active` 正交，两者都为真工具栏才显示。把它们摆进 HUD 是因为
    /// 「工具栏该显示却没显示 / 该隐藏却不隐藏」只能靠这两位区分，此前只能翻服务端日志。
    pub has_edit_context: bool,
    /// 窗口链 / TSF 上下文 / host-render 运行态。
    pub window: WindowDiagView,
    /// 分区显示开关（右键菜单）。
    pub sections: DiagSections,
    /// 窗口置顶（右键菜单可关）。关掉是为了让 HUD 沉到被观察窗口之下——
    /// 它自己挡住要看的东西是排查时的常见困扰。
    pub topmost: bool,
    /// 冻结中（右键菜单「停止刷新」）。**仅用于在 HUD 上打标**：真正的冻结在协调器侧
    /// （不再推送新快照），这里只负责让用户看得见"现在显示的不是实时值"。
    ///
    /// ⚠ 冻结而不标注，是诊断工具最坏的失败方式之一——用户会拿着一份旧快照当现状读。
    pub frozen: bool,
}

impl Default for InputDiagView {
    /// 分区全开、置顶、不冻结。**不能 derive**：那会给出全 false，等于默认空窗口 + 不置顶。
    fn default() -> Self {
        Self {
            process_name: String::new(),
            pid: 0,
            disabled: false,
            reason_text: String::new(),
            mask: 0,
            ime_active: false,
            has_edit_context: false,
            window: WindowDiagView::default(),
            sections: DiagSections::default(),
            topmost: true,
            frozen: false,
        }
    }
}

/// 窗口 / TSF 上下文诊断快照（DLL 上报 + 服务端补齐的 host-render 运行态）。
///
/// 与 [`InputDiagView`] 的输入态字段**分开存**：两者上报时机不同（禁用态随 compartment
/// 变更走，窗口快照随焦点走），合成一个就得回答「只到了一半时另一半算什么」。
#[derive(Clone, Debug, Default)]
pub struct WindowDiagView {
    /// **本快照的来源进程**（上报它的那个 DLL 实例的 `GetCurrentProcessId`）。
    ///
    /// ⚠ 与 [`InputDiagView::pid`] **不保证是同一个进程**：输入态随 focus_gained /
    /// compartment 变更走，窗口快照随焦点走，两个槽由不同进程的上报各自覆盖。多进程宿主
    /// （Win10 任务栏搜索：explorer + searchapp 各有一个 DLL 实例）下并排显示两份数据，
    /// 就隐含承诺了它们同源——不同源时必须显式说破，否则读者会脑补出一个不存在的完整画面。
    pub pid: u32,
    /// 来源进程名（服务端按 `pid` 补齐）。
    pub process_name: String,
    /// 焦点窗口句柄（仅展示与同一性比较——跨进程句柄在服务进程里调用无效）。
    pub focus_hwnd: u64,
    /// 焦点窗口类名。
    pub focus_class: String,
    /// 焦点窗口句柄的来源域标签（"TSF" / "GUI" / "前台" / "无"）。
    ///
    /// ⚠ 没有这一项，三条来源完全不同的句柄就混成一个字段了——尤其"前台"域的窗口
    /// 可能根本不属于本进程，拿它当"焦点窗口"去推 per-app 判据必然推错。
    ///
    /// 存**已翻译的字符串**而非协议里的 u8：`wind-ipc` 只在 windows/macos target 下才是
    /// 本 crate 的依赖（CI 的 test 跑 Linux 宿主），视图层引不到那个枚举。与 `reason_text`
    /// 同一惯例——映射只发生在协调器一处。
    pub focus_source_label: String,
    /// 顶层窗口句柄（`GetAncestor(GA_ROOT)`）。
    pub root_hwnd: u64,
    /// 顶层窗口类名——**per-app 窗口级判据取这个**（控件自身类名跨版本不稳定）。
    pub root_class: String,
    /// 顶层窗口 z-band（0 = 取不到）。
    pub root_band: u32,
    /// 前台窗口句柄。
    pub fg_hwnd: u64,
    /// 前台窗口类名。
    pub fg_class: String,
    /// 前台窗口所属进程 id。
    pub fg_pid: u32,
    /// 前台窗口所属进程名（服务端按 `fg_pid` 补齐；DLL 不上报）。
    pub fg_process_name: String,
    /// 焦点 DocMgr 指针值（实例同一性标识）。
    pub docmgr_id: u64,
    /// 焦点 Context 指针值（实例同一性标识）。
    pub context_id: u64,
    /// DLL 焦点会话序号（与服务端日志对齐用）。
    pub focus_session_id: u32,
    /// 本次焦点是否换了 DocMgr。
    pub docmgr_changed: bool,
    /// DLL 侧 host-render band 窗口当前 band（0 = 未建）。
    pub host_band: u32,
    /// 进程是否命中 host-render 白名单（服务端现算）。
    pub host_whitelisted: bool,
    /// host-render 是否有活跃目标且就是本进程（服务端现算）。
    pub host_active: bool,
    /// 已收到过至少一次快照。
    ///
    /// **「没数据」和「数据是 0」必须能分辨**：采集开关刚推给 DLL 时还没有任何快照，
    /// 若此时照常渲染一排 0，用户会把"尚未采集"读成"band 确实是 0"，进而据此得出
    /// 完全错误的结论。这一位就是为了让 HUD 能说出"还没采到"。
    pub received: bool,
}

impl WindowDiagView {
    /// 前台窗口是否属于**别的**进程。
    ///
    /// ⚠ 比较基准必须是**本快照自己的** `pid`，不能是 [`InputDiagView::pid`]——后者是
    /// 「最后一次输入态上报」的进程，多进程宿主下与本快照可能根本不是一个进程，用它比
    /// 会得到一条与事实无关的告警（真机实测：快照与前台同属 searchapp，却因输入态那半
    /// 停在 explorer 而报出「前台属于其他进程」）。
    ///
    /// 这条信号本身仍是关键的：焦点进程与前台进程分家时，per-app 配置按进程名匹配就
    /// 描述不了当前场景。
    pub fn foreground_is_other_process(&self) -> bool {
        self.fg_pid != 0 && self.pid != 0 && self.fg_pid != self.pid
    }

    /// 本快照与输入态分区是否来自**不同进程**（`input_pid` = [`InputDiagView::pid`]）。
    /// 两者都非 0 且不等时为真——此时 HUD 上下两半描述的是两个进程。
    pub fn differs_from_input_process(&self, input_pid: u32) -> bool {
        self.pid != 0 && input_pid != 0 && self.pid != input_pid
    }
}

/// 句柄的短展示形式。`0` 显示为 `-`——**空句柄和小地址必须一眼可分**，
/// 否则 `0x0` 会被读成"拿到了一个句柄"。
fn hwnd_str(h: u64) -> String {
    if h == 0 {
        "-".to_string()
    } else {
        format!("0x{h:X}")
    }
}

/// COM 指针的实例标识：取低 32 位。完整 64 位指针（`0x7FF6…`）会把行撑得很宽，
/// 而这里只需要回答"还是不是刚才那个实例"，低 32 位足够区分同进程内的不同对象。
fn inst_str(p: u64) -> String {
    if p == 0 {
        "-".to_string()
    } else {
        format!("0x{:08X}", p as u32)
    }
}

/// 类名的展示形式：空串显示为 `?`，与"类名就是空"区分不开的风险由采集端消除
/// （取不到时本就是空串，这里统一渲染成 `?` 表示未知）。
fn class_str(s: &str) -> &str {
    if s.is_empty() { "?" } else { s }
}

/// 纯格式化：诊断文本行（可单测）。
///
/// 分区渲染：输入态 → 窗口 → TSF → HostRender。窗口/TSF/HostRender 三段依赖 DLL 的
/// 诊断快照，未收到时**只出一行"未采集"**而不是一排占位 0（见 [`WindowDiagView::received`]）。
pub fn format_diag_lines(v: &InputDiagView) -> Vec<String> {
    let name = if v.process_name.is_empty() {
        "(未知)"
    } else {
        &v.process_name
    };
    let yn = |b: bool| if b { "是" } else { "否" };
    let s = &v.sections;
    let mut lines: Vec<String> = Vec::new();

    // 冻结标注置顶行：显示的已不是实时值，这件事必须先说，否则用户会拿旧快照当现状。
    if v.frozen {
        lines.push("⏸ 已停止刷新".to_string());
    }

    if s.input {
        lines.push(format!("{} ({})", name, v.pid));
        lines.push(format!(
            "禁用态: {}  原因: {}",
            yn(v.disabled),
            v.reason_text
        ));
        lines.push(format!("InputScope: 0x{:X}", v.mask));
        lines.push(format!(
            "激活: {} 可编辑上下文: {}",
            yn(v.ime_active),
            yn(v.has_edit_context)
        ));
    }

    let w = &v.window;
    // 「未采集」提示挂在窗口区：三个依赖快照的分区里它排第一，用户最可能先打开它。
    // 三个分区都关掉时不出这条——那时用户是主动不看，不需要被提醒去切焦点。
    let need_snapshot = s.window || s.tsf || s.host;

    if s.window {
        lines.push("── 窗口 ──".to_string());
        if w.received {
            // 快照来源进程。与首行进程不同时**必须点破**：那说明 HUD 上下两半描述的是
            // 两个进程，而并排显示天然让人以为是同一个。Win10 任务栏搜索就是这个形态
            // （explorer 与 searchapp 各有一个 DLL 实例，各自覆盖不同的槽）。
            let src_name = if w.process_name.is_empty() {
                "?".to_string()
            } else {
                w.process_name.clone()
            };
            if w.differs_from_input_process(v.pid) {
                lines.push(format!("⚠ 本节来自 {}({})，非上方进程", src_name, w.pid));
            } else {
                lines.push(format!("来源: {}({})", src_name, w.pid));
            }
            lines.push(format!(
                "焦点: {} [{}] {}",
                hwnd_str(w.focus_hwnd),
                class_str(&w.focus_source_label),
                class_str(&w.focus_class)
            ));
            lines.push(format!(
                "顶层: {} {} band={}",
                hwnd_str(w.root_hwnd),
                class_str(&w.root_class),
                w.root_band
            ));
            let fg_name = if w.fg_process_name.is_empty() {
                "?".to_string()
            } else {
                w.fg_process_name.clone()
            };
            lines.push(format!(
                "前台: {} {}({}) {}",
                hwnd_str(w.fg_hwnd),
                fg_name,
                w.fg_pid,
                class_str(&w.fg_class)
            ));
            // 只在"前台属于别的进程"时加这一行——它是异常信号，常态下不该占地方。
            // per-app 配置按进程名匹配，而这一行正是"进程名不足以描述当前场景"的证据。
            if w.foreground_is_other_process() {
                lines.push("⚠ 前台窗口属于其他进程".to_string());
            }
        }
    }

    if s.tsf {
        lines.push("── TSF ──".to_string());
        if w.received {
            lines.push(format!(
                "DocMgr: {}{}",
                inst_str(w.docmgr_id),
                if w.docmgr_changed {
                    " (本次已换)"
                } else {
                    ""
                }
            ));
            lines.push(format!(
                "Context: {}  会话#{}",
                inst_str(w.context_id),
                w.focus_session_id
            ));
        }
    }

    if s.host {
        lines.push("── HostRender ──".to_string());
        if w.received {
            lines.push(format!(
                "白名单: {}  活跃: {}  band={}",
                yn(w.host_whitelisted),
                yn(w.host_active),
                w.host_band
            ));
        }
    }

    // 采集开关是随 HUD 打开才推给 DLL 的，此后要等下一次焦点事件才有数据。
    // 明说"切一下焦点"，否则用户会以为功能坏了。放在末尾出一次，而不是每个分区各出一次。
    if need_snapshot && !w.received {
        lines.push("(未采集：切换一次输入焦点)".to_string());
    }

    // 分区全关会渲染出一个空窗口——那看起来和"HUD 坏了"一模一样。给一行可操作的提示。
    if lines.is_empty() {
        lines.push("(所有分区已隐藏：右键 →「显示分类」)".to_string());
    }
    lines
}

/// 固定深色半透明底 + 白字（MVP，不接主题）。
const BG: [u8; 4] = [32, 32, 36, 235];
const FG: [u8; 4] = [240, 240, 245, 255];
/// 无主题兜底字号（逻辑像素）。
const FONT_PX: f32 = 14.0;
/// 初始位置距屏幕右下角边距（逻辑像素）。
const MARGIN: i32 = 24;

/// 拖动交互状态（与 `wnd_proc` 共享）：仅在 Windows 有意义，非 Windows 为死代码占位。
struct DragState {
    hwnd: crate::sys::HWND,
    /// 是否正在拖动（`WM_LBUTTONDOWN` → true，`WM_LBUTTONUP` → false）。
    dragging: bool,
    /// 按下时鼠标屏幕坐标与窗口左上角的偏移，拖动时保持该偏移。
    grab_dx: i32,
    grab_dy: i32,
    /// 上次左键按下时间与坐标，用于手动判定双击（窗口类未启用 CS_DBLCLKS）。
    last_down_ms: u64,
    last_down_x: i32,
    last_down_y: i32,
    /// 当前诊断文本（双击复制用），由 `show_or_update` 刷新。
    copy_text: String,
    /// 回送协调器的事件通道（右键请求菜单）。同 `tooltip.rs` 的做法：菜单树与动作分发
    /// 都归协调器，UI 侧只负责报告"用户在哪儿右键了"。
    events: std::sync::mpsc::Sender<crate::manager::UiEvent>,
}

impl DragState {
    fn new(
        hwnd: crate::sys::HWND,
        events: std::sync::mpsc::Sender<crate::manager::UiEvent>,
    ) -> Self {
        Self {
            hwnd,
            dragging: false,
            grab_dx: 0,
            grab_dy: 0,
            last_down_ms: 0,
            last_down_x: i32::MIN,
            last_down_y: i32::MIN,
            copy_text: String::new(),
            events,
        }
    }
}

/// 当前毫秒时间戳（单调够用；仅用于双击间隔判定）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl crate::window::WindowMouse for DragState {
    fn on_message(
        &mut self,
        _hwnd: crate::sys::HWND,
        msg: u32,
        _wparam: crate::sys::WPARAM,
        _lparam: crate::sys::LPARAM,
    ) -> Option<crate::sys::LRESULT> {
        use crate::sys::{
            HWND_TOPMOST, LRESULT, ReleaseCapture, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
            SetCapture, SetWindowPos, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN,
        };
        match msg {
            WM_RBUTTONDOWN => {
                // 拖动中右键：先收拖动，否则 capture 会留在窗口上、菜单收不到点击。
                if self.dragging {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                }
                let (sx, sy) = cursor_screen();
                let _ = self
                    .events
                    .send(crate::manager::UiEvent::RequestInputDiagMenu { x: sx, y: sy });
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                // 记录抓取偏移（鼠标屏幕坐标 − 窗口左上角），并 SetCapture 以持续收到移动。
                let (mx, my) = cursor_screen();
                let (wx, wy) = window_origin(self.hwnd);
                self.grab_dx = mx - wx;
                self.grab_dy = my - wy;
                self.dragging = true;
                unsafe {
                    SetCapture(self.hwnd);
                }
                // 手动双击判定：与上次按下间隔 < 400ms 且位移 < 6px → 视为双击，复制文本。
                let t = now_ms();
                let dbl = t.saturating_sub(self.last_down_ms) < 400
                    && (mx - self.last_down_x).abs() < 6
                    && (my - self.last_down_y).abs() < 6;
                if dbl {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    if !self.copy_text.is_empty() {
                        crate::popup_menu::set_clipboard_text(&self.copy_text);
                    }
                    self.last_down_ms = 0; // 消费本次双击，避免三击连锁
                } else {
                    self.last_down_ms = t;
                    self.last_down_x = mx;
                    self.last_down_y = my;
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                if self.dragging {
                    let (mx, my) = cursor_screen();
                    let nx = mx - self.grab_dx;
                    let ny = my - self.grab_dy;
                    unsafe {
                        let _ = SetWindowPos(
                            self.hwnd,
                            HWND_TOPMOST,
                            nx,
                            ny,
                            0,
                            0,
                            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                    }
                    return Some(LRESULT(0));
                }
                None
            }
            WM_LBUTTONUP => {
                if self.dragging {
                    self.dragging = false;
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    return Some(LRESULT(0));
                }
                None
            }
            _ => None,
        }
    }
}

/// 取鼠标屏幕坐标（失败回退 (0,0)）。
fn cursor_screen() -> (i32, i32) {
    let mut pt = crate::sys::POINT::default();
    unsafe {
        let _ = crate::sys::GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

/// 取窗口左上角屏幕坐标（失败回退 (0,0)）。
fn window_origin(hwnd: crate::sys::HWND) -> (i32, i32) {
    let mut r = crate::sys::RECT::default();
    unsafe {
        let _ = crate::sys::GetWindowRect(hwnd, &mut r);
    }
    (r.left, r.top)
}

/// 每次刷新时如何给窗口定位。抽成纯枚举是为了让"三档"本身可单测——
/// 真正的坐标计算依赖 Win32（显示器/工作区），但**选哪一档**是纯逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionPlan {
    /// 首次显示：落屏幕右下角。
    InitialCorner,
    /// 已被拖出屏幕外：复位右下角，保证还能被抓回来。
    ResetCorner,
    /// 常规：以窗口当前实际位置为基准钳回工作区。
    ClampCurrent,
}

/// 定位档位决策。
///
/// `ClampCurrent` 这一档是内容变长时不被屏幕边缘吞掉的关键：分区展开会让高度显著增长，
/// 左上角还在屏内、右下角却已出界。钳制保持左上角尽量贴近原位，只把溢出部分推回来
/// ——比整个复位到右下角温和，用户挑的位置基本还在。
///
/// 拖动过程不经过这里（`wnd_proc` 直接 `SetWindowPos`），所以拖到哪当次就是哪；
/// 下一次内容更新才钳回屏内。这正是「手动移动当次不管、下次更新自动恢复」。
fn plan_position(positioned: bool, visible: bool) -> PositionPlan {
    if !positioned {
        PositionPlan::InitialCorner
    } else if visible {
        PositionPlan::ClampCurrent
    } else {
        PositionPlan::ResetCorner
    }
}

/// 本次 `show` 该用哪一档 z 序。
///
/// `applied` = 上次已应用的置顶态（`None` = 还没显示过），`want` = 本次期望的。
///
/// ★ **非置顶下的后续刷新必须 `Keep`**，这是本函数存在的全部理由：`LayeredWindow::show`
/// 无条件 `HWND_TOPMOST`，若非置顶时每次都走 `NoTopmost`，窗口会被反复插到「非置顶组
/// 顶部」——于是每刷新一次就重新盖住记事本，用户看到的是「置顶开关没用」。只有第一次
/// 需要真的移出置顶组，之后放手不管，别的窗口被激活时自然就盖过来了。
fn plan_zorder(applied: Option<bool>, want: bool) -> crate::window::ShowZOrder {
    use crate::window::ShowZOrder;
    if want {
        // 置顶：每次都重申。候选窗/宿主窗口都可能在此期间抢到更高 z 位。
        ShowZOrder::Topmost
    } else if applied == Some(false) {
        ShowZOrder::Keep
    } else {
        // 首次显示即非置顶，或刚从置顶切过来 → 真正移出置顶组（清 WS_EX_TOPMOST）。
        ShowZOrder::NoTopmost
    }
}

/// 输入诊断 HUD 窗口
pub struct InputDiagHud {
    window: LayeredWindow,
    renderer: TextRenderer,
    scale: f32,
    /// 拖动/双击状态（注册进 `wnd_proc`）。show_or_update 刷新其 copy_text。
    state: std::rc::Rc<std::cell::RefCell<DragState>>,
    /// 最近一次 show 使用的窗口左上角屏幕坐标：首次为右下角，之后每次刷新从窗口实际位置
    /// 同步（尊重用户拖动）；仅当被拖出屏幕外时复位回右下角。
    pos: (i32, i32),
    /// 是否已定位过（避免每次 update 都重置到右下角，尊重用户拖动）。
    positioned: bool,
    /// 已应用的置顶态。`None` = 还没显示过。
    ///
    /// 记住它是为了区分「切换到非置顶的那一次」和「后续刷新」：前者要真的把窗口移出置顶组
    /// 并沉下去，后者必须**完全不动 z 序**——否则每次刷新都把它顶回所有普通窗口之上，
    /// 用户看到的就是「置顶开关没用」。
    applied_topmost: Option<bool>,
}

impl InputDiagHud {
    pub fn new(events: std::sync::mpsc::Sender<crate::manager::UiEvent>) -> Result<Self, String> {
        let scale = dpi_scale();
        let window = LayeredWindow::create(None, 240, 120, "WindInputDiagHud")?;
        let renderer = TextRenderer::new("Microsoft YaHei UI", FONT_PX * scale)?;
        let state = std::rc::Rc::new(std::cell::RefCell::new(DragState::new(
            window.hwnd(),
            events,
        )));
        window.register_mouse(state.clone());
        let pos = initial_bottom_right(240, 120, scale);
        Ok(Self {
            window,
            renderer,
            scale,
            state,
            pos,
            positioned: false,
            applied_topmost: None,
        })
    }

    /// 渲染 4 行诊断文本并显示/更新窗口（首次落右下角，之后保持当前位置尊重拖动）。
    pub fn show_or_update(&mut self, v: &InputDiagView) {
        let lines = format_diag_lines(v);
        // 双击复制文本：整块以换行连接。
        self.state.borrow_mut().copy_text = lines.join("\n");

        let s = self.scale;
        let mut col = View::container(Layout::Column)
            .bg(BG)
            .pad(Edges::xy(12.0 * s, 9.0 * s))
            .gap(4.0 * s);
        col.corner_radius = 8.0 * s;
        for line in &lines {
            col = col.child(View::leaf(line.clone(), FG).text_align(Align::Start));
        }
        col.layout(0.0, 0.0, &self.renderer);
        let (w_f, h_f) = col.measured_size();
        let w = (w_f.ceil() as u32).max(80);
        let h = (h_f.ceil() as u32).max(40);
        self.window.resize(w, h);
        {
            let buf = self.window.buffer_mut();
            buf.fill(0);
        }
        {
            let (bw, bh) = self.window.size();
            let buf = self.window.buffer_mut();
            col.paint(buf, bw, bh, &self.renderer);
        }
        if let Err(e) = self.window.update() {
            tracing::warn!("InputDiagHud update failed: {}", e);
        }
        // 定位三档（每次更新都重算，故"手动拖动"只在**当次**说了算）：
        //   1. 首次      → 右下角
        //   2. 已被拖出屏幕外（可见余量不足）→ 复位右下角，保证还能被抓回来
        //   3. 其余      → 以窗口**当前实际位置**为基准钳回工作区
        //
        // 第 3 档是内容变长时不被屏幕边缘吞掉的关键：分区展开会让高度显著增长，
        // 原来的左上角还在屏内、右下角却已经出界了。钳制保持左上角尽量贴近原位，
        // 只把溢出的部分推回来——比"整个复位到右下角"温和得多，用户挑的位置基本还在。
        //
        // 拖动过程本身不经过这里（wnd_proc 里直接 SetWindowPos），所以拖到哪就是哪；
        // 下一次内容更新才会把它钳回屏内。这正是「手动移动当次不管、下次更新恢复」。
        let (cx, cy) = window_origin(self.window.hwnd());
        self.pos = match plan_position(
            self.positioned,
            window_visible_on_screen(cx, cy, w as i32, h as i32),
        ) {
            PositionPlan::InitialCorner | PositionPlan::ResetCorner => {
                initial_bottom_right(w, h, self.scale)
            }
            PositionPlan::ClampCurrent => crate::sys::clamp_to_work_area(cx, cy, w, h),
        };
        self.positioned = true;
        // z 序三档。**关键在于非置顶时后续刷新必须 Keep**：`show()` 无条件插入置顶组，
        // 每次刷新都会把窗口顶回所有普通窗口之上——那正是「关了置顶却仍压着记事本」的成因。
        let plan = plan_zorder(self.applied_topmost, v.topmost);
        self.window.show_z(self.pos.0, self.pos.1, plan);
        // 刚移出置顶组时再补一步：直接插到当前前台窗口之后，立刻沉下去。
        // 否则窗口停在「非置顶组顶部」，用户得先去点一下记事本才看得出效果，
        // 而在那之前他多半已经认定开关坏了。
        if plan == crate::window::ShowZOrder::NoTopmost {
            self.sink_below_foreground();
        }
        self.applied_topmost = Some(v.topmost);
    }

    /// 把窗口插到当前前台窗口之后（仅在刚取消置顶时调用）。
    ///
    /// ⚠ 前台窗口若**自身是置顶窗口**则跳过：`SetWindowPos` 的语义是「插到 topmost 窗口
    /// 之后的窗口也会变成 topmost」，那样刚清掉的 `WS_EX_TOPMOST` 会当场被重新戴上，
    /// 净效果与没关一样。
    fn sink_below_foreground(&self) {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, SWP_NOACTIVATE, SWP_NOMOVE,
                SWP_NOSIZE, SetWindowPos, WS_EX_TOPMOST,
            };
            let fg = GetForegroundWindow();
            if fg == HWND::default() || fg == self.window.hwnd() {
                return;
            }
            let ex = GetWindowLongPtrW(fg, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOPMOST.0 != 0 {
                return; // 前台自己是置顶窗口，插到它后面会把我们也变回置顶
            }
            let _ = SetWindowPos(
                self.window.hwnd(),
                fg,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    /// 复制当前显示的诊断文本到剪贴板（右键菜单「复制全部内容」）。
    /// 文本取自最近一次渲染的行——所见即所得，包含分区隐藏后的结果。
    pub fn copy_text(&self) {
        let text = self.state.borrow().copy_text.clone();
        if !text.is_empty() {
            crate::popup_menu::set_clipboard_text(&text);
        }
    }

    pub fn hide(&mut self) {
        self.window.hide();
    }
}

/// 系统 DPI 缩放因子（仿 status_tip.rs）。
fn dpi_scale() -> f32 {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::*;
        unsafe {
            let hdc = GetDC(HWND::default());
            let dpi = GetDeviceCaps(hdc, LOGPIXELSY);
            ReleaseDC(HWND::default(), hdc);
            if dpi > 0 { dpi as f32 / 96.0 } else { 1.0 }
        }
    }
    #[cfg(not(windows))]
    {
        1.0
    }
}

/// 主屏右下角初始坐标 = 屏幕尺寸 − 窗口尺寸 − 边距（边距按 DPI 缩放）。
#[cfg_attr(not(windows), allow(unused_variables))]
fn initial_bottom_right(w: u32, h: u32, scale: f32) -> (i32, i32) {
    let margin = (MARGIN as f32 * scale).round() as i32;
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        unsafe {
            let sw = GetSystemMetrics(SM_CXSCREEN);
            let sh = GetSystemMetrics(SM_CYSCREEN);
            let x = (sw - w as i32 - margin).max(0);
            let y = (sh - h as i32 - margin).max(0);
            (x, y)
        }
    }
    #[cfg(not(windows))]
    {
        (margin, margin)
    }
}

/// 窗口矩形与屏幕虚拟区域的可见交集在两个方向上是否都 ≥ `min`（纯几何，可单测）。
/// 用"可见余量"而非"任意相交"：只露极少（如 1px）也视为屏外，保证用户能重新抓到窗口。
/// 参数是两个矩形加一个阈值，展开成标量便于逐项单测，不包成 Rect 对象。
#[allow(clippy::too_many_arguments)]
fn rect_visible(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    min: i32,
) -> bool {
    let overlap_w = (x + w).min(vx + vw) - x.max(vx);
    let overlap_h = (y + h).min(vy + vh) - y.max(vy);
    overlap_w >= min && overlap_h >= min
}

/// 窗口是否"在屏幕内"（拖出屏外判据）：与虚拟屏（多显示器合并区域）可见交集 ≥ 24px。
#[cfg_attr(not(windows), allow(unused_variables))]
fn window_visible_on_screen(x: i32, y: i32, w: i32, h: i32) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            // 指标异常（0）时保守视为屏内，避免误复位用户拖动的位置。
            if vw <= 0 || vh <= 0 {
                return true;
            }
            rect_visible(x, y, w, h, vx, vy, vw, vh, 24)
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_visible_keeps_fully_and_partially_onscreen() {
        // 完全在屏内
        assert!(rect_visible(100, 100, 240, 120, 0, 0, 1920, 1080, 24));
        // 部分露出但可见余量充足（左侧被切到只剩 100px 宽仍可见）
        assert!(rect_visible(1820, 100, 240, 120, 0, 0, 1920, 1080, 24));
    }

    #[test]
    fn rect_visible_resets_when_offscreen() {
        // 拖到顶部外，仅剩 10px 高可见 < 24 → 判定屏外
        assert!(!rect_visible(100, -110, 240, 120, 0, 0, 1920, 1080, 24));
        // 完全拖到右侧屏外
        assert!(!rect_visible(1920, 100, 240, 120, 0, 0, 1920, 1080, 24));
        // 完全拖到左侧屏外（负坐标）
        assert!(!rect_visible(-240, 100, 240, 120, 0, 0, 1920, 1080, 24));
    }

    #[test]
    fn rect_visible_multi_monitor_left_virtual_origin() {
        // 副屏在主屏左侧：虚拟屏原点为负；窗口在副屏内应判定屏内
        assert!(rect_visible(-1800, 200, 240, 120, -1920, 0, 3840, 1080, 24));
    }

    fn base_view() -> InputDiagView {
        InputDiagView {
            process_name: "chrome.exe".into(),
            pid: 4242,
            disabled: true,
            reason_text: "compartment".into(),
            mask: 1 << 31,
            ime_active: true,
            has_edit_context: false,
            window: WindowDiagView::default(),
            ..Default::default()
        }
    }

    #[test]
    fn format_lines_shape() {
        let lines = format_diag_lines(&base_view());
        let text = lines.join("\n");
        assert!(lines[0].contains("chrome.exe"));
        assert!(lines[0].contains("4242"));
        assert!(lines[1].contains("禁用态: 是"));
        assert!(lines[1].contains("compartment"));
        assert!(lines[2].contains("0x")); // mask 十六进制
        // 两个状态位取值不同，确保没有把同一个变量渲染两遍
        assert!(lines[3].contains("激活: 是"));
        assert!(lines[3].contains("可编辑上下文: 否"));
        assert!(text.contains("── 窗口 ──"));
    }

    /// 未收到快照时**只出一行提示**，不得渲染一排占位 0。
    /// 「没数据」被读成「数据是 0」正是诊断工具最坏的失败方式——它不会报错，
    /// 只会让人拿着 band=0 去下结论。
    #[test]
    fn unreceived_window_section_says_so_instead_of_showing_zeros() {
        let lines = format_diag_lines(&base_view());
        let text = lines.join("\n");
        assert!(text.contains("未采集"), "应明确说明尚未采集: {text}");
        assert!(
            !text.contains("band="),
            "未采集时不得渲染 band 占位值: {text}"
        );
        assert!(
            !text.contains("DocMgr"),
            "未采集时不得渲染 TSF 分区: {text}"
        );
    }

    fn received_window() -> WindowDiagView {
        WindowDiagView {
            pid: 4242, // 与 base_view().pid 同源（常态）
            process_name: "chrome.exe".into(),
            focus_hwnd: 0xA1B2C3,
            focus_class: "Edit".into(),
            focus_source_label: "TSF".into(),
            root_hwnd: 0x11223344,
            root_class: "Shell_TrayWnd".into(),
            root_band: 1,
            fg_hwnd: 0x556677,
            fg_class: "Windows.UI.Core.CoreWindow".into(),
            fg_pid: 777,
            fg_process_name: "SearchApp.exe".into(),
            docmgr_id: 0x7FF6_1234_5678_9ABC,
            context_id: 0x7FF6_1234_5678_0000,
            focus_session_id: 19,
            docmgr_changed: true,
            host_band: 6,
            host_whitelisted: true,
            host_active: true,
            received: true,
        }
    }

    #[test]
    fn received_window_section_renders_all_groups() {
        let mut v = base_view();
        v.window = received_window();
        let text = format_diag_lines(&v).join("\n");
        assert!(text.contains("0xA1B2C3"), "焦点句柄: {text}");
        assert!(text.contains("[TSF]"), "来源标记必须可见: {text}");
        assert!(text.contains("Shell_TrayWnd"), "顶层类名: {text}");
        assert!(text.contains("band=1"));
        assert!(text.contains("SearchApp.exe(777)"));
        assert!(text.contains("── TSF ──"));
        // 指针只取低 32 位（完整 64 位会把行撑得过宽）。
        assert!(text.contains("0x56789ABC"), "DocMgr 实例 id: {text}");
        assert!(text.contains("(本次已换)"));
        assert!(text.contains("会话#19"));
        assert!(text.contains("── HostRender ──"));
        assert!(text.contains("白名单: 是"));
        assert!(text.contains("band=6"));
    }

    /// 跨进程前台是异常信号，只在成立时出现；常态下不占地方。
    /// 这一行正是「进程名不足以描述当前场景」的证据——per-app 配置只按进程名匹配。
    #[test]
    fn foreground_other_process_warning_is_conditional() {
        let mut v = base_view();
        v.window = received_window(); // 快照 pid=4242，fg_pid=777
        assert!(
            format_diag_lines(&v)
                .join("\n")
                .contains("⚠ 前台窗口属于其他进程")
        );

        v.window.fg_pid = v.window.pid; // 与快照同进程 → 不该出现
        assert!(!format_diag_lines(&v).join("\n").contains("⚠ 前台窗口"));

        // fg_pid 未知（0）只是采集失败，不得误报成"跨进程"。
        v.window.fg_pid = 0;
        assert!(!format_diag_lines(&v).join("\n").contains("⚠ 前台窗口"));
    }

    /// ★ 跨进程告警的基准必须是**快照自己的 pid**，不是首行那个。
    ///
    /// 真机形态（Win10 任务栏搜索）：快照与前台同属 searchapp，输入态那半却停在 explorer。
    /// 拿首行 pid 作基准就会报出一条与事实无关的「前台属于其他进程」。
    #[test]
    fn foreground_warning_uses_snapshot_pid_not_input_pid() {
        let mut v = base_view();
        v.pid = 7172; // 输入态来自 explorer
        v.window = received_window();
        v.window.pid = 8704; // 快照来自 searchapp
        v.window.process_name = "searchapp.exe".into();
        v.window.fg_pid = 8704; // 前台也是 searchapp —— 与快照同进程
        v.window.fg_process_name = "searchapp.exe".into();

        let text = format_diag_lines(&v).join("\n");
        assert!(
            !text.contains("⚠ 前台窗口属于其他进程"),
            "快照与前台同进程，不得因首行 pid 不同而误报: {text}"
        );
    }

    /// 上下两半来自不同进程时必须点破——并排显示天然让人以为它们同源。
    #[test]
    fn cross_process_snapshot_is_called_out() {
        let mut v = base_view();
        v.pid = 7172;
        v.window = received_window();
        v.window.pid = 8704;
        v.window.process_name = "searchapp.exe".into();
        let text = format_diag_lines(&v).join("\n");
        assert!(text.contains("本节来自 searchapp.exe(8704)"), "{text}");
        assert!(text.contains("非上方进程"), "{text}");

        // 同进程（常态）→ 平铺直叙地报来源，不加警示。
        v.window.pid = v.pid;
        v.window.process_name = "explorer.exe".into();
        let text = format_diag_lines(&v).join("\n");
        assert!(text.contains("来源: explorer.exe(7172)"), "{text}");
        assert!(!text.contains("非上方进程"), "{text}");
    }

    /// pid 未知（0）不算「不同进程」——那只是采集失败。
    #[test]
    fn unknown_pid_is_not_a_cross_process_signal() {
        let w = WindowDiagView {
            pid: 0,
            ..received_window()
        };
        assert!(!w.differs_from_input_process(7172));
        assert!(!w.foreground_is_other_process());
        assert!(!received_window().differs_from_input_process(0));
    }

    /// 定位三档。**关键是第二条**：窗口在屏内时也要走钳制，而不是原样沿用当前坐标
    /// ——内容变长（展开分区）后左上角还在屏内、右下角已经出界，原样沿用就会被屏幕边缘
    /// 吞掉一截。这是本次「窗口变大时自动调整位置」需求的落点。
    #[test]
    fn position_plan_three_cases() {
        assert_eq!(plan_position(false, true), PositionPlan::InitialCorner);
        assert_eq!(plan_position(false, false), PositionPlan::InitialCorner);
        assert_eq!(plan_position(true, true), PositionPlan::ClampCurrent);
        assert_eq!(plan_position(true, false), PositionPlan::ResetCorner);
    }

    /// z 序档位。**第三条是这次修的 bug**：非置顶下的后续刷新必须 `Keep`，
    /// 否则 `show()` 的无条件 `HWND_TOPMOST` 会让每次刷新都把窗口顶回普通窗口之上，
    /// 表现为「关掉置顶却仍压着记事本」。
    #[test]
    fn zorder_plan_covers_switch_and_steady_state() {
        use crate::window::ShowZOrder;
        // 置顶：每次都重申（此间可能有别的窗口抢到更高 z 位）。
        assert_eq!(plan_zorder(None, true), ShowZOrder::Topmost);
        assert_eq!(plan_zorder(Some(true), true), ShowZOrder::Topmost);
        assert_eq!(plan_zorder(Some(false), true), ShowZOrder::Topmost);
        // 刚从置顶切到非置顶 / 首次即非置顶 → 真正移出置顶组。
        assert_eq!(plan_zorder(Some(true), false), ShowZOrder::NoTopmost);
        assert_eq!(plan_zorder(None, false), ShowZOrder::NoTopmost);
        // 已经是非置顶 → 放手不管，让别的窗口盖过来。
        assert_eq!(plan_zorder(Some(false), false), ShowZOrder::Keep);
    }

    /// 默认全开：诊断工具的默认形态是"什么都看得见"。
    /// `derive(Default)` 会给出全 false（空窗口），故此处钉死手写实现的语义。
    #[test]
    fn sections_default_all_on() {
        let s = DiagSections::default();
        assert!(s.input && s.window && s.tsf && s.host);
        assert!(InputDiagView::default().topmost, "默认置顶");
        assert!(!InputDiagView::default().frozen, "默认不冻结");
        for i in DiagSections::ALL {
            assert!(s.get(i), "分区 {i} 默认应开");
        }
        // 越界序号不隐藏内容——未知序号让内容照常显示比让它凭空消失安全。
        assert!(s.get(9));
    }

    #[test]
    fn sections_toggle_only_target() {
        let mut s = DiagSections::default();
        s.toggle(2); // TSF
        assert!(!s.tsf);
        assert!(s.input && s.window && s.host, "其余分区不受牵连");
        s.toggle(2);
        assert!(s.tsf, "再切回来");
        s.toggle(9); // 越界 = no-op
        assert_eq!(s, DiagSections::default());
    }

    #[test]
    fn hidden_sections_are_omitted() {
        let mut v = base_view();
        v.window = received_window();
        v.sections = DiagSections {
            input: false,
            window: true,
            tsf: false,
            host: false,
        };
        let text = format_diag_lines(&v).join("\n");
        assert!(text.contains("── 窗口 ──"));
        assert!(text.contains("Shell_TrayWnd"));
        assert!(!text.contains("InputScope"), "输入态已隐藏: {text}");
        assert!(!text.contains("── TSF ──"), "TSF 已隐藏: {text}");
        assert!(!text.contains("── HostRender ──"), "Host 已隐藏: {text}");
    }

    /// 分区全关会渲染空窗口——那和"HUD 坏了"在屏幕上一模一样，必须给一行能自救的提示。
    #[test]
    fn all_sections_hidden_shows_recovery_hint() {
        let mut v = base_view();
        v.sections = DiagSections {
            input: false,
            window: false,
            tsf: false,
            host: false,
        };
        let lines = format_diag_lines(&v);
        assert_eq!(lines.len(), 1, "只出提示行: {lines:?}");
        assert!(
            lines[0].contains("显示分类"),
            "提示要指出怎么恢复: {lines:?}"
        );
    }

    /// 冻结必须在 HUD 上有标注：显示的已不是实时值，不说清楚用户会拿旧快照当现状读。
    #[test]
    fn frozen_is_labeled_at_top() {
        let mut v = base_view();
        v.frozen = true;
        let lines = format_diag_lines(&v);
        assert!(
            lines[0].contains("已停止刷新"),
            "冻结标注须在首行: {lines:?}"
        );
        v.frozen = false;
        assert!(!format_diag_lines(&v).join("\n").contains("已停止刷新"));
    }

    /// 「未采集」提示只在**依赖快照的分区至少开一个**时出现。
    /// 三个分区都关掉的人是主动不看，再提醒他"切一次焦点"就成了噪音。
    #[test]
    fn unreceived_hint_follows_snapshot_sections() {
        let mut v = base_view(); // received=false
        assert!(format_diag_lines(&v).join("\n").contains("未采集"));

        v.sections = DiagSections {
            input: true,
            window: false,
            tsf: false,
            host: false,
        };
        assert!(
            !format_diag_lines(&v).join("\n").contains("未采集"),
            "快照分区全关时不该再提示"
        );
    }

    /// 空句柄必须与"拿到了一个小地址句柄"可分辨。
    #[test]
    fn zero_handle_renders_as_dash_not_0x0() {
        let mut v = base_view();
        v.window = WindowDiagView {
            received: true,
            ..Default::default()
        };
        let text = format_diag_lines(&v).join("\n");
        assert!(text.contains("焦点: -"), "空句柄应显示为 -: {text}");
        assert!(!text.contains("0x0 "), "不得渲染成 0x0: {text}");
    }
}
