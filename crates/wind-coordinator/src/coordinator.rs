//! 中央协调器
//!
//! 与 Go 版本 `wind_input/internal/coordinator/coordinator.go` 对齐。
//!
//! 职责（按键优先级链的精简核心版）：
//! - key_up：Shift 释放触发模式切换
//! - key_down 热键匹配（切换引擎 / 全半角 / 标点 / 中英）
//! - Shift 待切换、Ctrl/Alt 透传
//! - 中文模式下的编辑键（Esc/Backspace/Space/Enter/数字选词/字母累积）
//!
//! 候选生成委托给 [`EngineManager`]，运行时词频 boost + 最终排序在本层应用。

use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use wind_bridge::handler::*;
use wind_bridge::push::{PushConfig, PushServer};
use wind_candidate::Candidate;
use wind_config::Config;
use wind_config::hotkey::{self, CompiledHotkeys};
use wind_engine::EngineManager;
use wind_ipc::protocol::{EVENT_KEY_DOWN, EVENT_KEY_UP, MOD_ALT, MOD_CTRL, MOD_SHIFT, calc_key_hash};
use wind_store::freq::FreqTracker;
use wind_transform::fullwidth::to_full_width;
use wind_transform::punctuation::PunctuationConverter;
use wind_ui::candidate_window::CandidateItem;
use wind_ui::manager::{UiCommand, UiManager};

/// VK + shift → 该键产生的 ASCII 标点/符号字符（字母键返回 None，由拼音/码表处理）。
fn punct_char(key_code: u32, shift: bool) -> Option<char> {
    let (base, shifted) = match key_code {
        0x30 => ('0', ')'),
        0x31 => ('1', '!'),
        0x32 => ('2', '@'),
        0x33 => ('3', '#'),
        0x34 => ('4', '$'),
        0x35 => ('5', '%'),
        0x36 => ('6', '^'),
        0x37 => ('7', '&'),
        0x38 => ('8', '*'),
        0x39 => ('9', '('),
        0xBA => (';', ':'),
        0xBB => ('=', '+'),
        0xBC => (',', '<'),
        0xBD => ('-', '_'),
        0xBE => ('.', '>'),
        0xBF => ('/', '?'),
        0xC0 => ('`', '~'),
        0xDB => ('[', '{'),
        0xDC => ('\\', '|'),
        0xDD => (']', '}'),
        0xDE => ('\'', '"'),
        _ => return None,
    };
    Some(if shift { shifted } else { base })
}

/// 引擎一次转换请求的候选上限（boost 重排后截断到 9）
const ENGINE_MAX_CANDIDATES: usize = 50;
/// 最终展示候选数
const DISPLAY_CANDIDATES: usize = 9;

/// 协调器输入状态
struct State {
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    input_buffer: String,
    candidates: Vec<Candidate>,
    caret_x: i32,
    caret_y: i32,
    caret_height: i32,
}

/// 中央协调器
pub struct Coordinator {
    state: Mutex<State>,
    push_server: Arc<PushServer>,
    config: Config,
    ui_tx: std::sync::mpsc::Sender<UiCommand>,
    engine_mgr: EngineManager,
    freq_tracker: FreqTracker,
    compiled_hotkeys: CompiledHotkeys,
    /// 标点转换器（引号左右状态）
    punct: Mutex<PunctuationConverter>,
}

impl Coordinator {
    /// 生产构造器：从 exe 同目录加载配置，启动候选窗口 UI 线程
    pub fn new(push_server: Arc<PushServer>) -> Arc<Self> {
        let data_dir = Config::data_dir();
        let config = Config::load(data_dir.as_deref()).unwrap_or_default();
        info!("Active schema: {}", config.active_schema());

        // UI 管理器（候选窗口线程）
        let ui_tx = match UiManager::new() {
            Ok(ui) => {
                let tx = ui.sender();
                std::mem::forget(ui); // 进程生命周期内保持 UI 线程存活
                tx
            }
            Err(e) => {
                warn!("Failed to create UI manager: {}", e);
                let (tx, _rx) = std::sync::mpsc::channel();
                tx
            }
        };

        Self::build(config, data_dir.as_deref(), push_server, ui_tx)
    }

    /// 无头构造器（测试用）：跳过 UI 线程，显式传入配置与数据目录。
    ///
    /// 用于对按键流程做端到端测试，不创建 Win32 窗口。
    pub fn new_headless(config: Config, data_dir: Option<&Path>) -> Arc<Self> {
        // 无头模式无 UI 消费端：丢弃 rx，notify_ui_* 的 send 会静默失败（已用 `let _ =` 忽略）
        let (ui_tx, _rx) = std::sync::mpsc::channel();
        drop(_rx);
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        Self::build(config, data_dir, push_server, ui_tx)
    }

    fn build(
        config: Config,
        data_dir: Option<&Path>,
        push_server: Arc<PushServer>,
        ui_tx: std::sync::mpsc::Sender<UiCommand>,
    ) -> Arc<Self> {
        let engine_mgr = EngineManager::new(&config, data_dir);
        let compiled_hotkeys = hotkey::Compiler::new(config.clone()).compile();
        info!(
            "Compiled hotkeys: {} key_down, {} key_up",
            compiled_hotkeys.key_down.len(),
            compiled_hotkeys.key_up.len()
        );

        Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: config.general.default_chinese_mode,
                full_width: config.general.default_full_width,
                chinese_punct: config.general.default_chinese_punct,
                toolbar_visible: true,
                caps_lock: false,
                input_buffer: String::new(),
                candidates: Vec::new(),
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
            }),
            push_server,
            config,
            ui_tx,
            engine_mgr,
            freq_tracker: FreqTracker::new(),
            compiled_hotkeys,
            punct: Mutex::new(PunctuationConverter::new()),
        })
    }

    /// 当前活跃方案 ID（测试/诊断用）
    pub fn active_schema_id(&self) -> String {
        self.engine_mgr.active_schema_id()
    }

    /// 当前是否中文模式（测试/诊断用）
    pub fn is_chinese_mode(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).chinese_mode
    }

    /// 根据输入缓冲更新候选（委托引擎 + 应用词频 boost）
    fn update_candidates(&self, state: &mut State) {
        state.candidates.clear();
        if state.input_buffer.is_empty() {
            return;
        }
        let result = self
            .engine_mgr
            .convert(&state.input_buffer, ENGINE_MAX_CANDIDATES);

        let mut candidates = result.candidates;
        // 运行时词频 boost
        for c in &mut candidates {
            c.weight += self.freq_tracker.get_boost(&c.text) as i32;
        }
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(DISPLAY_CANDIDATES);
        state.candidates = candidates;
    }

    /// 提交某个候选（记录词频后清空状态）
    fn commit_candidate(&self, state: &mut State, text: &str) {
        self.freq_tracker.record_selection(text);
        state.input_buffer.clear();
        state.candidates.clear();
    }

    fn build_preedit_display(input: &str, candidates: &[Candidate]) -> String {
        let mut display = String::from(input);
        if !candidates.is_empty() {
            display.push_str(" [");
            for (i, c) in candidates.iter().enumerate() {
                if i > 0 {
                    display.push(' ');
                }
                display.push_str(&format!("{}.{}", i + 1, c.text));
            }
            display.push(']');
        }
        display
    }

    fn notify_ui_update(&self, state: &State) {
        if state.candidates.is_empty() && state.input_buffer.is_empty() {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            return;
        }
        let items: Vec<CandidateItem> = state
            .candidates
            .iter()
            .map(|c| CandidateItem {
                text: c.text.clone(),
                code: c.code.clone(),
            })
            .collect();
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit: state.input_buffer.clone(),
            candidates: items,
            selected: 0,
            caret_x: state.caret_x,
            caret_y: state.caret_y,
        });
    }

    fn notify_ui_hide(&self) {
        let _ = self.ui_tx.send(UiCommand::HideCandidates);
    }

    fn build_status(&self) -> StatusUpdateData {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        StatusUpdateData {
            chinese_mode: state.chinese_mode,
            full_width: state.full_width,
            chinese_punct: state.chinese_punct,
            toolbar_visible: state.toolbar_visible,
            caps_lock: state.caps_lock,
            icon_label: if state.chinese_mode { "中".into() } else { "英".into() },
            key_down_hotkeys: self.compiled_hotkeys.key_down_tsf_hashes(),
            key_up_hotkeys: self.compiled_hotkeys.key_up_tsf_hashes(),
        }
    }

    fn push_activation_status(&self) {
        let s = self.build_status();
        debug!(
            "push_activation_status: chinese={} key_down={:?} key_up={:?}",
            s.chinese_mode, s.key_down_hotkeys, s.key_up_hotkeys
        );
        let encoded = wind_ipc::codec::encode_activation_status_push(
            s.chinese_mode, s.full_width, s.chinese_punct, s.toolbar_visible, s.caps_lock,
            false, &s.key_down_hotkeys, &s.key_up_hotkeys, &s.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    fn push_state_update(&self) {
        let s = self.build_status();
        let encoded = wind_ipc::codec::encode_state_push(
            s.chinese_mode, s.full_width, s.chinese_punct, s.toolbar_visible, s.caps_lock, &s.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    /// 切换方案：清空输入并推送状态
    fn switch_schema(&self, schema_id: &str) {
        if self.engine_mgr.switch_schema(schema_id) {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.input_buffer.clear();
            state.candidates.clear();
            drop(state);
            self.notify_ui_hide();
            self.push_state_update();
        }
    }

    fn cycle_schema(&self) {
        if let Some(next) = self.engine_mgr.cycle_schema() {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.input_buffer.clear();
            state.candidates.clear();
            drop(state);
            self.notify_ui_hide();
            self.push_state_update();
            info!("Cycled to schema: {}", next);
        }
    }

    /// 判断 key_code 是否为配置的 toggle 模式键（从编译后的 key_up 热键提取 vk 低 16 位）。
    /// TSF 仅在干净单击时于 keyUp 转发这些键，故据此判定即可直接切换。
    fn is_toggle_mode_keycode(&self, key_code: u32) -> bool {
        self.compiled_hotkeys
            .key_up
            .iter()
            .any(|e| (e.match_hash & 0xFFFF) == key_code)
    }

    /// 分发热键动作；返回是否已处理
    fn dispatch_hotkey(&self, action: &str) -> bool {
        match action {
            "toggle_mode" => {
                let (status, _) = self.handle_toggle_mode();
                status.is_some()
            }
            "switch_engine" => {
                self.cycle_schema();
                true
            }
            "toggle_full_width" => {
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.full_width = !s.full_width;
                }
                self.push_state_update();
                true
            }
            "toggle_punct" => {
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_punct = !s.chinese_punct;
                }
                self.push_state_update();
                true
            }
            _ => {
                debug!("Unhandled hotkey action: {}", action);
                false
            }
        }
    }

    fn commit_action(text: String, chinese_mode: bool) -> KeyAction {
        KeyAction::InsertText {
            text,
            new_composition: None,
            mode_changed: false,
            chinese_mode,
            has_new_composition: false,
        }
    }
}

impl MessageHandler for Coordinator {
    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction {
        debug!(
            "handle_key_event: type={} code=0x{:02X} mods=0x{:04X}",
            data.event_type, data.key_code, data.modifiers
        );

        // ── key_up：toggle 模式键（Shift/Ctrl/CapsLock）直接切换 ──
        // 关键：TSF 对 toggle 键会"吃掉 keydown 不转发"，仅在 C++ 侧判定为干净单击后
        // 于 keyUp 转发该键事件（_SendKeyToService(..., KEY_EVENT_UP)）。因此服务端
        // 收到 toggle 键的 keyUp 即应直接切换，无需 keydown/pending（对齐 Go HandleKeyEvent）。
        if data.event_type == EVENT_KEY_UP {
            if self.is_toggle_mode_keycode(data.key_code) {
                debug!("toggle_mode key_up: code=0x{:02X}", data.key_code);
                let (status, _) = self.handle_toggle_mode();
                if let Some(status) = status {
                    return KeyAction::StatusUpdate(status);
                }
            }
            return KeyAction::PassThrough;
        }
        if data.event_type != EVENT_KEY_DOWN {
            return KeyAction::PassThrough;
        }

        // ── key_down 热键匹配 ──
        // 规范化修饰位：TSF 转发的 modifiers 可能含 L/R 具体位，而 key_down 热键以
        // 通用位（ctrl/shift/alt/win）注册，故先掩掉具体位再比对 match_hash。
        let norm_mods = data.modifiers & hotkey::MOD_GENERIC_MASK;
        let norm_hash = calc_key_hash(norm_mods, data.key_code);
        if let Some(action) = self.compiled_hotkeys.match_key_down(norm_hash) {
            if !action.is_empty() {
                debug!("Hotkey matched (key_down): {} (0x{:08X})", action, norm_hash);
                let action = action.to_string();
                if self.dispatch_hotkey(&action) {
                    return KeyAction::StatusUpdate(self.build_status());
                }
            }
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // 英文模式：直接透传
        if !state.chinese_mode {
            return KeyAction::PassThrough;
        }

        // Ctrl/Alt 组合（非热键）：有输入则清空，否则透传
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            if !state.input_buffer.is_empty() {
                state.input_buffer.clear();
                state.candidates.clear();
                return KeyAction::ClearComposition;
            }
            return KeyAction::PassThrough;
        }

        debug!(
            "key_event: code=0x{:02X} mods=0x{:04X} chinese={} buf='{}'",
            data.key_code, data.modifiers, state.chinese_mode, state.input_buffer
        );

        match data.key_code {
            0x1B => {
                // Escape
                state.input_buffer.clear();
                state.candidates.clear();
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            0x08 => {
                // Backspace
                if !state.input_buffer.is_empty() {
                    state.input_buffer.pop();
                    self.update_candidates(&mut state);
                    if state.input_buffer.is_empty() {
                        self.notify_ui_hide();
                        KeyAction::ClearComposition
                    } else {
                        let display = Self::build_preedit_display(&state.input_buffer, &state.candidates);
                        self.notify_ui_update(&state);
                        KeyAction::UpdateComposition {
                            text: display,
                            caret_pos: state.input_buffer.len() as u32,
                        }
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x20 => {
                // Space：选首选 / 上屏编码
                if !state.candidates.is_empty() {
                    let text = state.candidates[0].text.clone();
                    self.commit_candidate(&mut state, &text);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            0x0D => {
                // Enter：上屏原始编码
                if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            0x31..=0x39 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字键 1-9 选词（Shift+数字走标点分支）
                let idx = (data.key_code - 0x31) as usize;
                if idx < state.candidates.len() {
                    let text = state.candidates[idx].text.clone();
                    self.commit_candidate(&mut state, &text);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else if !state.input_buffer.is_empty() {
                    let mut text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    // 数字键 vk 0x31..=0x39 即 ASCII '1'..='9'
                    text.push(data.key_code as u8 as char);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            0x41..=0x5A => {
                // A-Z 字母累积
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.input_buffer.push(ch);
                self.update_candidates(&mut state);
                let display = Self::build_preedit_display(&state.input_buffer, &state.candidates);
                self.notify_ui_update(&state);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos: state.input_buffer.len() as u32,
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = punct_char(data.key_code, shift) {
                    // 标点/符号键：先上屏首选候选（若有输入），再追加（转换后的）标点
                    let mut out = String::new();
                    if !state.candidates.is_empty() {
                        let t = state.candidates[0].text.clone();
                        self.freq_tracker.record_selection(&t);
                        out.push_str(&t);
                    } else if !state.input_buffer.is_empty() {
                        out.push_str(&state.input_buffer);
                    }
                    let had_input = !state.input_buffer.is_empty() || !state.candidates.is_empty();
                    state.input_buffer.clear();
                    state.candidates.clear();

                    // 中文标点转换；未配置中文标点时按全角/原样输出
                    let piece = if state.chinese_punct {
                        self.punct
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .to_chinese(ch)
                            .unwrap_or_else(|| {
                                if state.full_width {
                                    to_full_width(&ch.to_string())
                                } else {
                                    ch.to_string()
                                }
                            })
                    } else if state.full_width {
                        to_full_width(&ch.to_string())
                    } else {
                        ch.to_string()
                    };
                    out.push_str(&piece);
                    if had_input {
                        self.notify_ui_hide();
                    }
                    Self::commit_action(out, true)
                } else if !state.input_buffer.is_empty() {
                    KeyAction::Consumed
                } else {
                    KeyAction::PassThrough
                }
            }
        }
    }

    fn handle_focus_gained(&self, data: &FocusData) -> Option<StatusUpdateData> {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.caret_x = data.x;
            state.caret_y = data.y;
            state.caret_height = data.height;
        }
        let status = self.build_status();
        self.push_activation_status();
        Some(status)
    }

    fn handle_focus_lost(&self) {}

    fn handle_ime_activated(&self, _client_token: u64) -> Option<StatusUpdateData> {
        let status = self.build_status();
        self.push_activation_status();
        Some(status)
    }

    fn handle_ime_deactivated(&self) {}

    fn handle_mode_notify(&self, flags: u32) {
        let chinese_mode = (flags & wind_ipc::protocol::STATUS_CHINESE_MODE) != 0;
        let clear_input = (flags & wind_ipc::protocol::STATUS_MODE_CHANGED) != 0;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = chinese_mode;
        if clear_input {
            state.input_buffer.clear();
            state.candidates.clear();
        }
    }

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = !state.chinese_mode;
        let commit_text = if !state.input_buffer.is_empty() && !state.chinese_mode {
            let t = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            t
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.push_state_update();
        (Some(self.build_status()), commit_text)
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = chinese_mode;
        let commit_text = if !state.input_buffer.is_empty() && !chinese_mode {
            let t = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            t
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.push_state_update();
        (Some(self.build_status()), commit_text)
    }

    fn handle_menu_command(&self, command: &str) -> Option<StatusUpdateData> {
        info!("Menu command: {}", command);
        match command {
            "toggle_mode" => self.handle_toggle_mode().0,
            "toggle_width" => {
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.full_width = !s.full_width;
                }
                self.push_state_update();
                Some(self.build_status())
            }
            "toggle_punct" => {
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_punct = !s.chinese_punct;
                }
                self.push_state_update();
                Some(self.build_status())
            }
            "switch_engine" => {
                self.cycle_schema();
                Some(self.build_status())
            }
            _ => None,
        }
    }

    fn handle_composition_terminated(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.input_buffer.clear();
        state.candidates.clear();
        drop(state);
        self.notify_ui_hide();
    }

    fn handle_caret_update(&self, data: &CaretData) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
    }

    fn handle_caret_pending(&self) {}

    fn handle_selection_changed(&self, _prev_char: u16) {}

    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.input_buffer.is_empty() {
            return None;
        }
        let tk = data.trigger_key;
        let text = if tk == 0x20 {
            if !state.candidates.is_empty() {
                state.candidates[0].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else if tk == 0x0D {
            state.input_buffer.clone()
        } else if (0x31..=0x39).contains(&tk) {
            let idx = (tk - 0x31) as usize;
            if idx < state.candidates.len() {
                state.candidates[idx].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else {
            state.input_buffer.clone()
        };
        state.input_buffer.clear();
        state.candidates.clear();
        // 与 handle_key_event 的选词路径保持一致：记录词频用于学习排序
        if !text.is_empty() {
            self.freq_tracker.record_selection(&text);
        }
        Some(CommitResultData {
            barrier_seq: data.barrier_seq,
            text,
            new_composition: String::new(),
            mode_changed: false,
            chinese_mode: state.chinese_mode,
        })
    }

    fn handle_host_render_request(&self) {}
    fn handle_host_render_ready(&self) {}
}
