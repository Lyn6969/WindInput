//! macOS host-render forwarder：把协调器下发的 `UiCommand` 转成 push 帧。
//!
//! Windows 侧 `ui_thread` 直接驱动 LayeredWindow 呈现；macOS 侧无进程内窗口，
//! 候选/工具栏/提示统一光栅化进 POSIX SHM，再经 push 管道通知 .app 端取帧呈现。
//! 用 `#[cfg(unix)]` 让本模块在 Linux/macOS 都编译，便于在开发机直接跑测试。

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use crate::candidate_window::{CandidateWindow, CandidateWindowConfig};
use crate::manager::{UiCommand, UiEvent};
use crate::toast::{ToastKind, ToastPosition};
use wind_bridge::HostRenderSink;
use wind_bridge::shared_memory_posix::PosixSharedMemory;
use wind_ipc::codec::*;
use wind_ipc::protocol::*;

const SHM_MAX: usize = MAX_SHARED_RENDER_SIZE;

pub struct Forwarder {
    win: CandidateWindow,
    shm: Option<PosixSharedMemory>,
    sink: Arc<dyn HostRenderSink>,
    suffix: String,
    /// 保活 dummy 事件接收端，避免 CandidateWindow 内部 send 报错。
    _ev_rx: Receiver<UiEvent>,
}

impl Forwarder {
    pub fn new(sink: Arc<dyn HostRenderSink>, suffix: String) -> Self {
        let (ev_tx, ev_rx) = std::sync::mpsc::channel();
        let win = CandidateWindow::new(CandidateWindowConfig::default(), ev_tx)
            .expect("create candidate window (mock/raster host)");
        Self {
            win,
            shm: None,
            sink,
            suffix,
            _ev_rx: ev_rx,
        }
    }

    fn ensure_shm(&mut self) -> Option<&mut PosixSharedMemory> {
        if self.shm.is_none() {
            match PosixSharedMemory::create(&wind_bridge::endpoint::shm_name(&self.suffix), SHM_MAX)
            {
                Ok(s) => self.shm = Some(s),
                Err(e) => {
                    tracing::warn!("create SHM failed: {}", e);
                    return None;
                }
            }
        }
        self.shm.as_mut()
    }

    pub fn handle(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::UpdateCandidates {
                preedit,
                preedit_caret,
                mode_label,
                candidates,
                selected,
                hover,
                page,
                total_pages,
                caret_x,
                caret_y,
                caret_height,
                caret_valid,
                // 固定位置模式只对本地自绘窗口（Windows）有意义：macOS 走 IMKit 转发，
                // 候选窗由系统按光标定位，Rust 侧无从干预，故此处显式丢弃。
                fixed: _,
                fixed_x: _,
                fixed_y: _,
            } => {
                tracing::debug!(
                    "forwarder UpdateCandidates: n={} preedit={:?} caret=({},{},{}) valid={}",
                    candidates.len(),
                    preedit,
                    caret_x,
                    caret_y,
                    caret_height,
                    caret_valid
                );
                // hover tooltip 文本（反查码在 CandidateItem.tooltip）。
                let tip = if hover >= 0 {
                    candidates
                        .get(hover as usize)
                        .map(|c| c.tooltip.clone())
                        .filter(|s| !s.is_empty())
                } else {
                    None
                };
                self.win.update(
                    &preedit,
                    preedit_caret,
                    &mode_label,
                    candidates,
                    selected,
                    hover,
                    page,
                    total_pages,
                );
                self.win
                    .set_position(caret_x, caret_y, caret_height, caret_valid);
                match self.win.render_frame() {
                    Some(f) => {
                        let (sx, sy, w, h, scale, soft) = (
                            f.screen_x,
                            f.screen_y,
                            f.width,
                            f.height,
                            f.scale,
                            f.software_shadow,
                        );
                        // 翻页器命中矩形的内部 tag(HOVER_PAGE_PREV/NEXT=100000/100001)重映射为
                        // Swift/Go 约定的 -1(上页)/-2(下页)，否则 100000>=0 会被 .app 误当候选选中
                        // (index 100000) → 翻页失效；候选 tag(>=0)原样。对齐 Go forwarder_darwin。
                        let rects: Vec<(i32, i32, i32, i32, i32)> = f
                            .hit_rects
                            .iter()
                            .map(|(i, r)| {
                                let wire = if *i == crate::manager::HOVER_PAGE_PREV {
                                    -1
                                } else if *i == crate::manager::HOVER_PAGE_NEXT {
                                    -2
                                } else {
                                    *i
                                };
                                (wire, r.x as i32, r.y as i32, r.w as i32, r.h as i32)
                            })
                            .collect();
                        let buf = f.buf;
                        // 先写 SHM 像素并取 seq；shm 建失败则整帧放弃——
                        // 不能只推命中矩形/tooltip 而无底帧，否则 .app 拿到无像素的命中区（不一致）。
                        let seq = match self.ensure_shm() {
                            Some(shm) => shm.write_frame(sx, sy, w, h, &buf),
                            None => return,
                        };
                        let mut flags = SharedRenderHeader::FLAG_VISIBLE
                            | SharedRenderHeader::FLAG_CONTENT_READY;
                        if soft {
                            flags |= SharedRenderHeader::FLAG_SOFTWARE_SHADOW;
                        }
                        self.sink.push_frame(&encode_host_render_frame(
                            seq,
                            sx,
                            sy,
                            w,
                            h,
                            flags,
                            scale.round().max(1.0) as u32,
                        ));
                        self.sink.push_frame(&encode_candidate_rects(&rects));
                        match tip {
                            Some(t) => self.sink.push_frame(&encode_tooltip_show(&t, "", "", "")),
                            None => self.sink.push_frame(&encode_tooltip_hide()),
                        }
                        tracing::debug!(
                            "forwarder pushed host-render frame seq={} {}x{} at ({},{}) scale={}",
                            seq,
                            w,
                            h,
                            sx,
                            sy,
                            scale
                        );
                    }
                    None => {
                        tracing::debug!("forwarder render_frame=None → hide");
                        self.hide_frame();
                    }
                }
            }
            UiCommand::HideCandidates => self.hide_frame(),
            UiCommand::UpdateToolbar(s) => {
                let mut flags = STATUS_TOOLBAR_VISIBLE;
                if s.chinese_mode {
                    flags |= STATUS_CHINESE_MODE;
                }
                if s.full_width {
                    flags |= STATUS_FULL_WIDTH;
                }
                if s.chinese_punct {
                    flags |= STATUS_CHINESE_PUNCT;
                }
                let mode = if s.chinese_mode { 1 } else { 0 };
                self.sink
                    .push_frame(&encode_mode_status(flags, mode, &s.icon_label));
            }
            UiCommand::HideToolbar => {
                self.sink.push_frame(&encode_mode_status(0, 0, ""));
            }
            UiCommand::ShowStatusTip {
                text,
                x,
                y,
                caret_height,
                offset_x,
                offset_y,
                duration_ms,
                fixed,
                fixed_x,
                fixed_y,
            } => {
                // wire 仅传最终屏幕 (x,y)；fixed/offset 在此算定。
                // 跟随光标时 y 是 caret 顶端，须 +caret_height 落到 caret 底端下方，否则气泡
                // 贴在 caret 顶端盖住输入位（与候选窗 render_frame 的 y+caret_height 对齐）。
                let (fx, fy) = if fixed {
                    (fixed_x, fixed_y)
                } else {
                    (x + offset_x, y + offset_y + caret_height)
                };
                self.sink.push_frame(&encode_status_show(
                    &text,
                    "",
                    "",
                    fx,
                    fy,
                    duration_ms as i32,
                ));
            }
            UiCommand::HideStatusTip => {
                self.sink.push_frame(&encode_status_hide());
            }
            UiCommand::ShowToast {
                text,
                position,
                kind,
                duration_ms,
            } => {
                let pos = match position {
                    ToastPosition::Center => "center",
                    ToastPosition::TopCenter => "top_center",
                    ToastPosition::BottomCenter => "bottom_center",
                    ToastPosition::TopLeft => "top_left",
                    ToastPosition::TopRight => "top_right",
                    ToastPosition::BottomLeft => "bottom_left",
                    ToastPosition::BottomRight => "bottom_right",
                };
                // accent 取 ToastKind 对应强调色（与 toast.rs ToastKind::accent 一致）。
                let accent = match kind {
                    ToastKind::Info => "#409EFF",
                    ToastKind::Success => "#52C46E",
                    ToastKind::Error => "#F56C6C",
                };
                self.sink.push_frame(&encode_toast_show(
                    "",
                    &text,
                    "",
                    "",
                    accent,
                    pos,
                    duration_ms as i32,
                    0,
                ));
            }
            UiCommand::SetTheme(t) => self.win.set_theme(*t),
            UiCommand::SetCandidateLayout(v) => self.win.set_vertical(v),
            UiCommand::SetPreeditEmbedded(v) => self.win.set_preedit_embedded(v),
            UiCommand::SetCandidateFontSize(s) => self.win.set_font_size_override(s),
            UiCommand::SetCandidateFontFamily(f) => self.win.set_font_family(&f),
            UiCommand::SetTooltipDelay(d) => self.win.set_tooltip_delay(d),
            UiCommand::SetCandidateFlipWhenAbove(v) => self.win.set_flip_when_above(v),
            UiCommand::SetCandidateSwapWhenAbove(v) => self.win.set_swap_preedit_when_above(v),
            UiCommand::SetPagerInPreedit(v) => self.win.set_pager_in_preedit(v),
            UiCommand::SetPagerDisplay(m) => self.win.set_pager_display(m),
            UiCommand::SetPageNumberDisplay(m) => self.win.set_page_number_display(m),
            UiCommand::SetTooltipChaiziFont { path, family } => {
                self.win.set_chaizi_font(&path, &family)
            }
            UiCommand::Shutdown => {}
            UiCommand::CopyToClipboard(text) => crate::popup_menu::set_clipboard_text(&text),
            // 其余（SetToolbarPos / SetToolbarCorner / ShowCandidateMenu / MenuKey /
            // HideMenu / OpenPath）：darwin 侧由 .app 原生处理或属 W9，留桩。
            other => {
                tracing::debug!("forwarder: 暂未处理 {:?}", std::mem::discriminant(&other));
            }
        }
    }

    fn hide_frame(&mut self) {
        if let Some(shm) = self.shm.as_mut() {
            let seq = shm.write_hidden();
            self.sink
                .push_frame(&encode_host_render_frame(seq, 0, 0, 0, 0, 0, 1));
        }
    }
}

pub fn forwarder_thread(rx: Receiver<UiCommand>, sink: Arc<dyn HostRenderSink>, suffix: String) {
    let mut fwd = Forwarder::new(sink, suffix);
    tracing::info!("macOS host-render forwarder started");
    for cmd in rx {
        if matches!(cmd, UiCommand::Shutdown) {
            break;
        }
        fwd.handle(cmd);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::candidate_window::CandidateItem;
    use std::sync::{Arc, Mutex};

    struct CapSink(Arc<Mutex<Vec<Vec<u8>>>>);
    impl wind_bridge::HostRenderSink for CapSink {
        fn push_frame(&self, f: &[u8]) {
            self.0.lock().unwrap().push(f.to_vec());
        }
    }
    fn cmd_of(f: &[u8]) -> u16 {
        u16::from_le_bytes([f[2], f[3]])
    }
    fn item(t: &str) -> CandidateItem {
        CandidateItem {
            text: t.into(),
            code: String::new(),
            label: String::new(),
            tooltip: String::new(),
            comment: String::new(),
            no_index: false,
        }
    }
    fn mk(cap: Arc<Mutex<Vec<Vec<u8>>>>, suffix: &str) -> Forwarder {
        Forwarder::new(Arc::new(CapSink(cap)), suffix.into())
    }

    #[test]
    fn update_candidates_emits_frame_and_rects() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t1");
        f.handle(UiCommand::UpdateCandidates {
            preedit: "a".into(),
            preedit_caret: 1,
            mode_label: "".into(),
            candidates: vec![item("中"), item("国")],
            selected: 0,
            hover: -1,
            page: 1,
            total_pages: 1,
            caret_x: 100,
            caret_y: 200,
            caret_height: 20,
            caret_valid: true,
            fixed: false,
            fixed_x: 0,
            fixed_y: 0,
        });
        let v = cap.lock().unwrap();
        assert!(
            v.iter()
                .any(|x| cmd_of(x) == wind_ipc::protocol::CMD_HOST_RENDER_FRAME)
        );
        assert!(
            v.iter()
                .any(|x| cmd_of(x) == wind_ipc::protocol::CMD_CANDIDATE_RECTS)
        );
    }

    #[test]
    fn hide_emits_hidden_frame() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t2");
        // 先显示一帧建 shm。
        f.handle(UiCommand::UpdateCandidates {
            preedit: "a".into(),
            preedit_caret: 1,
            mode_label: "".into(),
            candidates: vec![item("中")],
            selected: 0,
            hover: -1,
            page: 1,
            total_pages: 1,
            caret_x: 10,
            caret_y: 20,
            caret_height: 20,
            caret_valid: true,
            fixed: false,
            fixed_x: 0,
            fixed_y: 0,
        });
        cap.lock().unwrap().clear();
        f.handle(UiCommand::HideCandidates);
        let v = cap.lock().unwrap();
        let hr = v
            .iter()
            .find(|x| cmd_of(x) == wind_ipc::protocol::CMD_HOST_RENDER_FRAME)
            .expect("hidden frame");
        // payload flags @ 帧 offset 8+20=28，VISIBLE 位应为 0。
        assert_eq!(u32::from_le_bytes(hr[28..32].try_into().unwrap()) & 0x1, 0);
    }

    #[test]
    fn update_toolbar_emits_mode_status() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t3");
        f.handle(UiCommand::UpdateToolbar(
            crate::toolbar::ToolbarState::default(),
        ));
        assert!(
            cap.lock()
                .unwrap()
                .iter()
                .any(|x| cmd_of(x) == wind_ipc::protocol::CMD_MODE_STATUS)
        );
    }

    #[test]
    fn status_tip_fixed_overrides_coords() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t4");
        f.handle(UiCommand::ShowStatusTip {
            text: "中".into(),
            x: 10,
            y: 20,
            caret_height: 18,
            offset_x: 3,
            offset_y: 4,
            duration_ms: 1000,
            fixed: true,
            fixed_x: 500,
            fixed_y: 600,
        });
        let v = cap.lock().unwrap();
        let fr = v
            .iter()
            .find(|x| cmd_of(x) == wind_ipc::protocol::CMD_STATUS_SHOW)
            .expect("status_show frame");
        // payload = textLen+text + bgLen + fgLen + x:i32 + y:i32 + dur:i32。
        // "中"=3 字节 → text 段 4+3=7；bg/fg 空各 4 字节 → x 从 payload offset 7+4+4=15 起；帧 +8。
        let off = 8 + 15;
        assert_eq!(
            i32::from_le_bytes(fr[off..off + 4].try_into().unwrap()),
            500
        ); // fixed_x
        assert_eq!(
            i32::from_le_bytes(fr[off + 4..off + 8].try_into().unwrap()),
            600
        ); // fixed_y
    }

    #[test]
    fn status_tip_non_fixed_applies_offset() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t5");
        f.handle(UiCommand::ShowStatusTip {
            text: "x".into(),
            x: 10,
            y: 20,
            caret_height: 0,
            offset_x: 3,
            offset_y: 4,
            duration_ms: 0,
            fixed: false,
            fixed_x: 0,
            fixed_y: 0,
        });
        let v = cap.lock().unwrap();
        let fr = v
            .iter()
            .find(|x| cmd_of(x) == wind_ipc::protocol::CMD_STATUS_SHOW)
            .unwrap();
        // "x"=1 → text 段 4+1=5；bg/fg 空 → x 从 payload offset 5+4+4=13 起；帧 +8。
        let off = 8 + 13;
        assert_eq!(i32::from_le_bytes(fr[off..off + 4].try_into().unwrap()), 13); // 10+3
        assert_eq!(
            i32::from_le_bytes(fr[off + 4..off + 8].try_into().unwrap()),
            24
        ); // 20+4
    }

    #[test]
    fn hide_status_tip_and_toast_and_toolbar_emit() {
        let cap = Arc::new(Mutex::new(Vec::new()));
        let mut f = mk(cap.clone(), "_t6");
        f.handle(UiCommand::HideStatusTip);
        f.handle(UiCommand::ShowToast {
            text: "ok".into(),
            position: crate::toast::ToastPosition::Center,
            kind: crate::toast::ToastKind::Success,
            duration_ms: 2000,
        });
        f.handle(UiCommand::HideToolbar);
        let v = cap.lock().unwrap();
        assert!(
            v.iter()
                .any(|x| cmd_of(x) == wind_ipc::protocol::CMD_STATUS_HIDE)
        );
        let toast = v
            .iter()
            .find(|x| cmd_of(x) == wind_ipc::protocol::CMD_TOAST_SHOW)
            .expect("toast frame");
        // position 段：title(空,4) message("ok",4+2=6) bg(4) fg(4) accent(#52C46E,4+7=11) position(...)
        // 校验 position 字符串 = "center"。
        let p = &toast[8..];
        let mut o = 0usize;
        let mut read = || {
            let n = u32::from_le_bytes(p[o..o + 4].try_into().unwrap()) as usize;
            let s = String::from_utf8(p[o + 4..o + 4 + n].to_vec()).unwrap();
            o += 4 + n;
            s
        };
        assert_eq!(read(), ""); // title
        assert_eq!(read(), "ok"); // message
        let _ = read(); // bg
        let _ = read(); // fg
        assert_eq!(read(), "#52C46E"); // accent (Success)
        assert_eq!(read(), "center"); // position
        assert!(
            v.iter()
                .any(|x| cmd_of(x) == wind_ipc::protocol::CMD_MODE_STATUS)
        ); // HideToolbar → mode_status
    }
}
