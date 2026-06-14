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
use wind_ui::toolbar::ToolbarState;

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

/// VK + shift → 快捷输入表达式字符（数字/运算符/点/括号；含小键盘）。其它返回 None。
fn quick_input_char(key_code: u32, shift: bool) -> Option<char> {
    // 小键盘
    match key_code {
        0x60..=0x69 => return Some((b'0' + (key_code - 0x60) as u8) as char),
        0x6A => return Some('*'),
        0x6B => return Some('+'),
        0x6D => return Some('-'),
        0x6E => return Some('.'),
        0x6F => return Some('/'),
        _ => {}
    }
    // 主键盘：复用 punct_char，仅保留表达式有效字符
    let c = punct_char(key_code, shift)?;
    if c.is_ascii_digit() || matches!(c, '+' | '-' | '*' | '/' | '.' | '(' | ')') {
        Some(c)
    } else {
        None
    }
}

/// 引擎一次转换请求的候选上限（boost 重排后截断到 9）
const ENGINE_MAX_CANDIDATES: usize = 50;

/// 协调器输入状态
struct State {
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    /// 简繁转换开关（运行时切换；commit 时把简体输出转繁体）
    s2t_enabled: bool,
    toolbar_visible: bool,
    caps_lock: bool,
    input_buffer: String,
    /// 组合区显示文本（拼音含音节分隔 "ni hao"；码表为原始编码）。
    /// 仅显示输入码/拼音，绝不包含候选列表。
    preedit: String,
    candidates: Vec<Candidate>,
    /// 当前页内高亮候选下标（0-based，相对当前页）
    selected_index: usize,
    /// 当前页码（0-based）
    current_page: usize,
    /// 临时拼音模式（码表方案下经触发键临时切到拼音反查）
    temp_pinyin_mode: bool,
    /// 临时拼音输入缓冲（拼音串）
    temp_pinyin_buffer: String,
    /// 临时拼音目标方案 id（如 "pinyin"）
    temp_pinyin_schema: String,
    /// 临时拼音组合区前缀字符（触发键，如 "`"）
    temp_pinyin_prefix: String,
    /// 快捷输入模式（分号触发：日期/计算器）
    quick_input_mode: bool,
    /// 快捷输入缓冲（如 "1+2*3" / "12.25"）
    quick_input_buffer: String,
    /// 快捷输入组合区前缀字符（触发键，如 ";"）
    quick_input_prefix: String,
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
    /// 词频持久化文件路径（None=不持久化）
    freq_path: Option<std::path::PathBuf>,
    /// 自上次落盘以来的新增选词数（达阈值触发保存）
    freq_dirty: Mutex<u32>,
    /// 短语层（system.phrases.toml；$Y$M$D 模板）
    phrases: crate::phrases::PhraseLayer,
    /// 简繁转换器（OpenCC；None=数据缺失不可用）
    s2t: Option<wind_transform::s2t::Converter>,
}

/// 短语候选权重基准（高于普通候选，使短语展开排在前列）
const PHRASE_WEIGHT_BASE: i32 = 40_000_000;

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

        // 词频持久化文件：优先用户配置目录，其次 data 目录
        let freq_path = Config::user_config_dir()
            .or_else(|| data_dir.as_deref().map(|d| d.to_path_buf()))
            .map(|d| d.join("freq.tsv"));
        Self::build(config, data_dir.as_deref(), push_server, ui_tx, freq_path)
    }

    /// 无头构造器（测试用）：跳过 UI 线程，不做词频持久化（避免污染真实文件）。
    pub fn new_headless(config: Config, data_dir: Option<&Path>) -> Arc<Self> {
        // 无头模式无 UI 消费端：丢弃 rx，notify_ui_* 的 send 会静默失败（已用 `let _ =` 忽略）
        let (ui_tx, _rx) = std::sync::mpsc::channel();
        drop(_rx);
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        Self::build(config, data_dir.as_deref(), push_server, ui_tx, None)
    }

    fn build(
        config: Config,
        data_dir: Option<&Path>,
        push_server: Arc<PushServer>,
        ui_tx: std::sync::mpsc::Sender<UiCommand>,
        freq_path: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        let engine_mgr = EngineManager::new(&config, data_dir);
        let compiled_hotkeys = hotkey::Compiler::new(config.clone()).compile();
        info!(
            "Compiled hotkeys: {} key_down, {} key_up",
            compiled_hotkeys.key_down.len(),
            compiled_hotkeys.key_up.len()
        );

        // 短语层：从 data 目录加载 system.phrases.toml
        let phrases = match data_dir {
            Some(d) => {
                let p = d.join("system.phrases.toml");
                let layer = crate::phrases::PhraseLayer::load(&p);
                if !layer.is_empty() {
                    info!("Loaded phrases from {}", p.display());
                }
                layer
            }
            None => crate::phrases::PhraseLayer::default(),
        };

        // 简繁转换器：从 data/opencc 加载（变体来自配置，默认 s2t）
        let s2t = data_dir.and_then(|d| {
            let variant = if config.features.s2t.variant.is_empty() {
                "s2t"
            } else {
                &config.features.s2t.variant
            };
            let conv = wind_transform::s2t::Converter::load_variant(&d.join("opencc"), variant);
            if conv.is_some() {
                info!("Loaded S2T converter (variant={})", variant);
            }
            conv
        });

        let freq_tracker = FreqTracker::new();
        if let Some(p) = &freq_path {
            match freq_tracker.load_from_file(p) {
                Ok(_) => info!("Loaded freq: {} entries from {}", freq_tracker.len(), p.display()),
                Err(e) => warn!("Failed to load freq {}: {}", p.display(), e),
            }
        }

        let coordinator = Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: config.general.default_chinese_mode,
                full_width: config.general.default_full_width,
                chinese_punct: config.general.default_chinese_punct,
                s2t_enabled: config.features.s2t.enabled,
                toolbar_visible: true,
                caps_lock: false,
                input_buffer: String::new(),
                preedit: String::new(),
                candidates: Vec::new(),
                selected_index: 0,
                current_page: 0,
                temp_pinyin_mode: false,
                temp_pinyin_buffer: String::new(),
                temp_pinyin_schema: String::new(),
                temp_pinyin_prefix: String::new(),
                quick_input_mode: false,
                quick_input_buffer: String::new(),
                quick_input_prefix: String::new(),
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
            }),
            push_server,
            config,
            ui_tx,
            engine_mgr,
            freq_tracker,
            compiled_hotkeys,
            punct: Mutex::new(PunctuationConverter::new()),
            freq_path,
            freq_dirty: Mutex::new(0),
            phrases,
            s2t,
        });
        // 启动即显示常驻工具栏（反映初始 中英/方案/标点/全半角）
        coordinator.notify_toolbar();
        coordinator
    }

    /// 记录一次选词并按阈值落盘（脏计数达到 8 或后续 focus_lost 时保存）。
    fn record_selection(&self, word: &str) {
        if word.is_empty() {
            return;
        }
        self.freq_tracker.record_selection(word);
        let mut dirty = self.freq_dirty.lock().unwrap_or_else(|e| e.into_inner());
        *dirty += 1;
        if *dirty >= 8 {
            *dirty = 0;
            drop(dirty);
            self.save_freq();
        }
    }

    /// 立即把词频落盘（focus_lost / 阈值触发）
    fn save_freq(&self) {
        if let Some(p) = &self.freq_path {
            if let Err(e) = self.freq_tracker.save_to_file(p) {
                warn!("Failed to save freq {}: {}", p.display(), e);
            }
        }
    }

    /// 当前活跃方案 ID（测试/诊断用）
    pub fn active_schema_id(&self) -> String {
        self.engine_mgr.active_schema_id()
    }

    /// 当前是否中文模式（测试/诊断用）
    pub fn is_chinese_mode(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).chinese_mode
    }

    /// 设置简繁开关（测试/诊断用）。返回是否生效（数据缺失则 false）。
    pub fn debug_set_s2t(&self, on: bool) -> bool {
        if self.s2t.is_none() {
            return false;
        }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).s2t_enabled = on;
        true
    }

    /// 候选总数（测试/诊断用）
    pub fn debug_candidate_count(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).candidates.len()
    }

    /// 分页信息 (当前页0-based, 页内高亮0-based, 总页数)（测试/诊断用）
    pub fn debug_page_info(&self) -> (usize, usize, usize) {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.current_page, s.selected_index, self.total_pages(&s))
    }

    /// 当前页候选文本列表（内部简体；测试/诊断用）
    pub fn debug_page_texts(&self) -> Vec<String> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = self.page_range(&s);
        s.candidates[start..end].iter().map(|c| c.text.clone()).collect()
    }

    /// 当前页候选的"显示文本"（应用简繁后，与候选窗口一致；测试/诊断用）
    pub fn debug_page_display_texts(&self) -> Vec<String> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (start, end) = self.page_range(&s);
        s.candidates[start..end]
            .iter()
            .map(|c| self.maybe_s2t(&s, &c.text))
            .collect()
    }

    /// 根据输入缓冲更新候选（委托引擎 + 应用词频 boost）
    fn update_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.preedit = state.input_buffer.clone();
        if state.input_buffer.is_empty() {
            return;
        }
        let result = self
            .engine_mgr
            .convert(&state.input_buffer, ENGINE_MAX_CANDIDATES);

        // 组合区只显示输入码/拼音（拼音含音节分隔 "ni hao"），绝不含候选列表
        if !result.preedit_display.is_empty() {
            state.preedit = result.preedit_display;
        }

        let mut candidates = result.candidates;
        // 运行时词频 boost
        for c in &mut candidates {
            c.weight += self.freq_tracker.get_boost(&c.text) as i32;
        }
        // 短语层：输入码命中短语 code → 展开模板候选（高权重，排前列）
        if !self.phrases.is_empty() {
            for (text, w) in self.phrases.lookup(&state.input_buffer) {
                candidates.push(Candidate {
                    text,
                    weight: PHRASE_WEIGHT_BASE + w,
                    is_phrase: true,
                    ..Default::default()
                });
            }
        }
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        // 按文本去重（保留排序后首现 = 最高权重），避免短语与引擎候选重复
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.text.clone()));
        // 保留多页候选用于翻页（不再截断到单页）
        candidates.truncate(ENGINE_MAX_CANDIDATES);
        state.candidates = candidates;
        // 候选变化：复位翻页与高亮
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 每页候选数（来自配置，至少 1）
    fn per_page(&self) -> usize {
        self.config.ui.per_page.max(1)
    }

    /// 若 key_code 是配置的二/三候选键，返回页内候选偏移（1=次选/第2项，2=三选/第3项）。
    fn select_key_offset(&self, key_code: u32) -> Option<usize> {
        for group in &self.config.input.select_key_groups {
            let vks = hotkey::select_key_vks(group);
            if let Some(pos) = vks.iter().position(|vk| *vk == key_code) {
                return Some(pos + 1);
            }
        }
        None
    }

    /// 总页数（至少 1）
    fn total_pages(&self, state: &State) -> usize {
        let pp = self.per_page();
        state.candidates.len().div_ceil(pp).max(1)
    }

    /// 当前页候选切片的 [start, end) 区间
    fn page_range(&self, state: &State) -> (usize, usize) {
        let pp = self.per_page();
        let start = state.current_page * pp;
        let end = (start + pp).min(state.candidates.len());
        (start, end)
    }

    /// 当前高亮候选的全局下标（页起点 + 页内高亮）
    fn highlighted_global_index(&self, state: &State) -> usize {
        let (start, _) = self.page_range(state);
        start + state.selected_index
    }

    /// 上移高亮（页首回卷到上一页末项）；返回是否变化
    fn move_up(&self, state: &mut State) -> bool {
        if state.candidates.is_empty() {
            return false;
        }
        if state.selected_index > 0 {
            state.selected_index -= 1;
        } else if state.current_page > 0 {
            state.current_page -= 1;
            let (s, e) = self.page_range(state);
            state.selected_index = e - s - 1;
        } else {
            return false;
        }
        true
    }

    /// 下移高亮（页尾回卷到下一页首项）；返回是否变化
    fn move_down(&self, state: &mut State) -> bool {
        if state.candidates.is_empty() {
            return false;
        }
        let (s, e) = self.page_range(state);
        let page_count = e - s;
        if state.selected_index + 1 < page_count {
            state.selected_index += 1;
        } else if state.current_page + 1 < self.total_pages(state) {
            state.current_page += 1;
            state.selected_index = 0;
        } else {
            return false;
        }
        true
    }

    /// 上一页（高亮归零）；返回是否变化
    fn page_prev(&self, state: &mut State) -> bool {
        if state.current_page > 0 {
            state.current_page -= 1;
            state.selected_index = 0;
            true
        } else {
            false
        }
    }

    /// 下一页（高亮归零）；返回是否变化
    fn page_next(&self, state: &mut State) -> bool {
        if state.current_page + 1 < self.total_pages(state) {
            state.current_page += 1;
            state.selected_index = 0;
            true
        } else {
            false
        }
    }

    // ───────────────────────── 临时拼音 ─────────────────────────

    /// 触发键名 → VK（不含 z，z 混合模式后置实现）
    fn temp_pinyin_trigger_vk(key: &str) -> Option<u32> {
        match key.trim().to_lowercase().as_str() {
            "backtick" | "grave" | "`" => Some(0xC0),
            "semicolon" | ";" => Some(0xBA),
            "quote" | "'" => Some(0xDE),
            "comma" | "," => Some(0xBC),
            "period" | "." => Some(0xBE),
            "slash" | "/" => Some(0xBF),
            "lbracket" | "[" => Some(0xDB),
            "rbracket" | "]" => Some(0xDD),
            _ => None,
        }
    }

    /// VK → 组合区前缀字符
    fn temp_pinyin_prefix_for(key_code: u32) -> &'static str {
        match key_code {
            0xC0 => "`",
            0xBA => ";",
            0xDE => "'",
            0xBC => ",",
            0xBE => ".",
            0xBF => "/",
            0xDB => "[",
            0xDD => "]",
            _ => "`",
        }
    }

    /// 当前按键是否匹配配置的临时拼音触发键
    fn is_temp_pinyin_trigger(&self, key_code: u32) -> bool {
        self.config
            .input
            .temp_pinyin
            .trigger_keys
            .iter()
            .filter_map(|k| Self::temp_pinyin_trigger_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 退出临时拼音模式并清空相关状态
    fn exit_temp_pinyin(&self, state: &mut State) {
        state.temp_pinyin_mode = false;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_schema.clear();
        state.temp_pinyin_prefix.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 用临时拼音目标方案转换缓冲，刷新候选与组合区（前缀 + 拼音）
    fn update_temp_pinyin_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let prefix = state.temp_pinyin_prefix.clone();
        if state.temp_pinyin_buffer.is_empty() {
            state.preedit = prefix;
            return;
        }
        let result = self.engine_mgr.convert_with(
            &state.temp_pinyin_schema,
            &state.temp_pinyin_buffer,
            ENGINE_MAX_CANDIDATES,
        );
        let display = if result.preedit_display.is_empty() {
            state.temp_pinyin_buffer.clone()
        } else {
            result.preedit_display
        };
        state.preedit = format!("{}{}", prefix, display);

        let mut candidates = result.candidates;
        for c in &mut candidates {
            c.weight += self.freq_tracker.get_boost(&c.text) as i32;
        }
        candidates.sort_by(|a, b| {
            b.weight
                .cmp(&a.weight)
                .then(a.natural_order.cmp(&b.natural_order))
        });
        candidates.truncate(ENGINE_MAX_CANDIDATES);
        state.candidates = candidates;
    }

    /// 临时拼音模式下的按键处理
    fn handle_temp_pinyin_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        match data.key_code {
            0x1B => {
                // Esc：退出
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            0x08 => {
                // Backspace：删字符，空则退出
                if state.temp_pinyin_buffer.is_empty() {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                state.temp_pinyin_buffer.pop();
                if state.temp_pinyin_buffer.is_empty() {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                }
            }
            0x20 | 0x0D => {
                // Space/Enter：上屏高亮候选并退出
                if !state.candidates.is_empty() {
                    let idx = self.highlighted_global_index(state).min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.record_selection(&text);
                    let out = self.maybe_s2t(state, &text);
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    Self::commit_action(out, true)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            0x31..=0x39 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字键选当前页第 N 个
                let (start, end) = self.page_range(state);
                let idx = start + (data.key_code - 0x31) as usize;
                if idx < end {
                    let text = state.candidates[idx].text.clone();
                    self.record_selection(&text);
                    let out = self.maybe_s2t(state, &text);
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    Self::commit_action(out, true)
                } else {
                    KeyAction::Consumed
                }
            }
            0x41..=0x5A if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
                // 字母累积拼音
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.temp_pinyin_buffer.push(ch);
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                }
            }
            0x26 | 0x28 => {
                // 上/下方向键
                let changed = if data.key_code == 0x26 {
                    self.move_up(state)
                } else {
                    self.move_down(state)
                };
                if changed {
                    self.notify_ui_update(state);
                }
                KeyAction::Consumed
            }
            0x21 | 0x22 => {
                // 翻页
                let changed = if data.key_code == 0x21 {
                    self.page_prev(state)
                } else {
                    self.page_next(state)
                };
                if changed {
                    self.notify_ui_update(state);
                }
                KeyAction::Consumed
            }
            _ => {
                // 二三候选键
                if data.modifiers & MOD_SHIFT == 0 {
                    if let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(state);
                        let idx = start + offset;
                        if idx < end {
                            let text = state.candidates[idx].text.clone();
                            self.record_selection(&text);
                            let out = self.maybe_s2t(state, &text);
                            self.exit_temp_pinyin(state);
                            self.notify_ui_hide();
                            return Self::commit_action(out, true);
                        }
                    }
                }
                // 其它键：先上屏高亮候选退出，再让标点字符按普通流程？
                // 简化：有候选则上屏高亮候选并退出（吞掉该键）；否则退出清空。
                if !state.candidates.is_empty() {
                    let idx = self.highlighted_global_index(state).min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.record_selection(&text);
                    let out = self.maybe_s2t(state, &text);
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    Self::commit_action(out, true)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
        }
    }

    // ───────────────────────── 快捷输入 ─────────────────────────

    /// 触发键名 → VK
    fn quick_input_trigger_vk(key: &str) -> Option<u32> {
        match key.trim().to_lowercase().as_str() {
            "semicolon" | ";" => Some(0xBA),
            "backtick" | "grave" | "`" => Some(0xC0),
            "quote" | "'" => Some(0xDE),
            "comma" | "," => Some(0xBC),
            "period" | "." => Some(0xBE),
            "slash" | "/" => Some(0xBF),
            "lbracket" | "[" => Some(0xDB),
            "rbracket" | "]" => Some(0xDD),
            _ => None,
        }
    }

    /// VK → 组合区前缀字符
    fn quick_input_prefix_for(key_code: u32) -> &'static str {
        match key_code {
            0xBA => ";",
            0xC0 => "`",
            0xDE => "'",
            0xBC => ",",
            0xBE => ".",
            0xBF => "/",
            0xDB => "[",
            0xDD => "]",
            _ => ";",
        }
    }

    /// 当前按键是否匹配配置的快捷输入触发键
    fn is_quick_input_trigger(&self, key_code: u32) -> bool {
        self.config
            .features
            .quick_input
            .trigger_keys
            .iter()
            .filter_map(|k| Self::quick_input_trigger_vk(k))
            .any(|vk| vk == key_code)
    }

    /// 按当前中英标点/全半角配置转换一个标点字符为上屏文本。
    fn convert_punct_char(&self, state: &State, ch: char) -> String {
        if state.chinese_punct {
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
        }
    }

    /// 退出快捷输入模式并清空状态
    fn exit_quick_input(&self, state: &mut State) {
        state.quick_input_mode = false;
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 由缓冲生成日期/计算器候选，刷新组合区（前缀 + 缓冲）
    fn update_quick_input_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let prefix = state.quick_input_prefix.clone();
        if state.quick_input_buffer.is_empty() {
            state.preedit = prefix;
            return;
        }
        state.preedit = format!("{}{}", prefix, state.quick_input_buffer);
        let dp = self.config.features.quick_input.decimal_places;
        let texts =
            crate::quick_input::generate_quick_input_candidates(&state.quick_input_buffer, dp);
        state.candidates = texts
            .into_iter()
            .enumerate()
            .map(|(i, t)| Candidate {
                text: t,
                natural_order: i as i32,
                ..Default::default()
            })
            .collect();
    }

    /// 快捷输入模式组合区刷新结果（UpdateComposition）
    fn quick_input_composition(&self, state: &State) -> KeyAction {
        let display = state.preedit.clone();
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 快捷输入模式下的按键处理
    fn handle_quick_input_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        match data.key_code {
            0x1B => {
                self.exit_quick_input(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            0x08 => {
                // 退格：空缓冲退出，否则删末字符（可退到仅前缀）
                if state.quick_input_buffer.is_empty() {
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    return KeyAction::ClearComposition;
                }
                state.quick_input_buffer.pop();
                self.update_quick_input_candidates(state);
                if state.candidates.is_empty() {
                    self.notify_ui_hide();
                } else {
                    self.notify_ui_update(state);
                }
                self.quick_input_composition(state)
            }
            0x20 => {
                // 空格：上屏当前高亮候选；无候选则退出
                if !state.candidates.is_empty() {
                    let idx = self.highlighted_global_index(state).min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            0x0D => {
                // 回车：上屏缓冲原文（空则上屏前缀字符）
                let out = if state.quick_input_buffer.is_empty() {
                    state.quick_input_prefix.clone()
                } else {
                    state.quick_input_buffer.clone()
                };
                self.exit_quick_input(state);
                self.notify_ui_hide();
                if out.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(out, true)
                }
            }
            0x26 | 0x28 => {
                let changed = if data.key_code == 0x26 {
                    self.move_up(state)
                } else {
                    self.move_down(state)
                };
                if changed {
                    self.notify_ui_update(state);
                }
                KeyAction::Consumed
            }
            0x21 | 0x22 => {
                let changed = if data.key_code == 0x21 {
                    self.page_prev(state)
                } else {
                    self.page_next(state)
                };
                if changed {
                    self.notify_ui_update(state);
                }
                KeyAction::Consumed
            }
            0x41..=0x5A if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
                // 字母 a-z 按标签选当前页候选（a=第1个）
                let (start, end) = self.page_range(state);
                let idx = start + (data.key_code - 0x41) as usize;
                if idx < end {
                    let text = state.candidates[idx].text.clone();
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::Consumed
                }
            }
            _ => {
                // 再次按触发键且缓冲为空：按标点配置上屏前缀字符并退出
                if state.quick_input_buffer.is_empty()
                    && self.is_quick_input_trigger(data.key_code)
                {
                    let ch = state.quick_input_prefix.chars().next().unwrap_or(';');
                    let out = self.convert_punct_char(state, ch);
                    self.exit_quick_input(state);
                    self.notify_ui_hide();
                    return Self::commit_action(out, true);
                }
                // 可打印字符（数字/运算符/点/括号等）累积到缓冲
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = quick_input_char(data.key_code, shift) {
                    if state.quick_input_buffer.chars().count() < 20 {
                        state.quick_input_buffer.push(ch);
                        self.update_quick_input_candidates(state);
                        if state.candidates.is_empty() {
                            self.notify_ui_hide();
                        } else {
                            self.notify_ui_update(state);
                        }
                    }
                    self.quick_input_composition(state)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }

    /// 顶屏当前高亮候选（若有）并进入临时拼音模式（对齐 Go decideBufferedTrigger 的 actEnterMode）。
    /// 有候选：上屏高亮候选 + 原子开启临时拼音组合；空码：丢弃缓冲后进入。
    fn commit_and_enter_temp_pinyin(
        &self,
        state: &mut State,
        key_code: u32,
        target: String,
    ) -> KeyAction {
        let committed = if !state.candidates.is_empty() {
            let idx = self.highlighted_global_index(state).min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            self.record_selection(&t);
            Some(t)
        } else {
            None
        };
        state.input_buffer.clear();
        state.candidates.clear();
        // 进入临时拼音
        state.temp_pinyin_mode = true;
        state.temp_pinyin_schema = target;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix = Self::temp_pinyin_prefix_for(key_code).to_string();
        self.update_temp_pinyin_candidates(state);
        self.notify_ui_update(state);
        let prefix = state.temp_pinyin_prefix.clone();
        match committed {
            Some(text) => KeyAction::InsertText {
                text,
                new_composition: Some(prefix),
                mode_changed: false,
                chinese_mode: true,
                has_new_composition: true,
            },
            None => KeyAction::UpdateComposition {
                text: prefix.clone(),
                caret_pos: prefix.chars().count() as u32,
            },
        }
    }

    /// 顶屏当前高亮候选（若有）并进入快捷输入模式。
    fn commit_and_enter_quick_input(&self, state: &mut State, key_code: u32) -> KeyAction {
        let committed = if !state.candidates.is_empty() {
            let idx = self.highlighted_global_index(state).min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            self.record_selection(&t);
            Some(t)
        } else {
            None
        };
        state.input_buffer.clear();
        state.candidates.clear();
        state.quick_input_mode = true;
        state.quick_input_buffer.clear();
        state.quick_input_prefix = Self::quick_input_prefix_for(key_code).to_string();
        self.update_quick_input_candidates(state);
        self.notify_ui_update(state);
        let prefix = state.quick_input_prefix.clone();
        match committed {
            Some(text) => KeyAction::InsertText {
                text,
                new_composition: Some(prefix),
                mode_changed: false,
                chinese_mode: true,
                has_new_composition: true,
            },
            None => KeyAction::UpdateComposition {
                text: prefix.clone(),
                caret_pos: prefix.chars().count() as u32,
            },
        }
    }

    /// 若开启简繁转换，把简体文本转为繁体（数据缺失则原样返回）。
    fn maybe_s2t(&self, state: &State, text: &str) -> String {
        if state.s2t_enabled {
            if let Some(conv) = &self.s2t {
                return conv.convert(text);
            }
        }
        text.to_string()
    }

    /// 提交某个候选（记录原始简体词频后清空状态），返回上屏文本（按需简繁转换）。
    fn commit_candidate(&self, state: &mut State, text: &str) -> String {
        self.record_selection(text);
        let out = self.maybe_s2t(state, text);
        state.input_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        out
    }

    fn notify_ui_update(&self, state: &State) {
        if state.candidates.is_empty() && state.input_buffer.is_empty() {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            return;
        }
        // 仅推送当前页候选（窗口按 1..N 编号，翻页后重新编号）
        let (start, end) = self.page_range(state);
        // 快捷输入用字母标签（a/b/c，因数字键需录入表达式），其余用数字
        let alpha = state.quick_input_mode;
        let items: Vec<CandidateItem> = state.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| CandidateItem {
                // 开启简繁时显示也转繁体（内部候选仍存简体，用于词频/匹配）
                text: self.maybe_s2t(state, &c.text),
                code: c.code.clone(),
                label: if alpha {
                    ((b'a' + i as u8) as char).to_string()
                } else {
                    (i + 1).to_string()
                },
            })
            .collect();
        // 多页时在组合区追加页码指示（如 "ni hao (1/3)"）
        let total_pages = self.total_pages(state);
        let preedit = if total_pages > 1 {
            format!("{} ({}/{})", state.preedit, state.current_page + 1, total_pages)
        } else {
            state.preedit.clone()
        };
        let selected = state.selected_index.min(items.len().saturating_sub(1));
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit,
            candidates: items,
            selected,
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

    /// 推送当前状态到常驻工具栏（中英/方案/标点/全半角）
    fn notify_toolbar(&self) {
        let schema_label = Self::schema_display_name(&self.engine_mgr.active_schema_id());
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let tb = ToolbarState {
            chinese_mode: s.chinese_mode,
            schema_label,
            full_width: s.full_width,
            chinese_punct: s.chinese_punct,
        };
        drop(s);
        let _ = self.ui_tx.send(UiCommand::UpdateToolbar(tb));
    }

    /// 在当前光标上方显示状态提示气泡（中英/标点/全半角/方案切换）
    fn show_tip(&self, text: &str) {
        let (x, y) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.caret_x, s.caret_y)
        };
        let _ = self.ui_tx.send(UiCommand::ShowStatusTip {
            text: text.to_string(),
            x,
            y,
        });
    }

    /// 方案显示名（友好名优先，未知回退 id）
    fn schema_display_name(id: &str) -> String {
        match id {
            "wubi86" => "五笔".to_string(),
            "pinyin" => "拼音".to_string(),
            "shuangpin" => "双拼".to_string(),
            "wubi86_pinyin" => "五笔拼音".to_string(),
            other => other.to_string(),
        }
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
            state.preedit.clear();
            drop(state);
            self.notify_ui_hide();
            self.push_state_update();
            self.show_tip(&Self::schema_display_name(&next));
            self.notify_toolbar();
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
                let full = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.full_width = !s.full_width;
                    s.full_width
                };
                self.push_state_update();
                self.show_tip(if full { "全角" } else { "半角" });
                self.notify_toolbar();
                true
            }
            "toggle_punct" => {
                let cn = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_punct = !s.chinese_punct;
                    s.chinese_punct
                };
                self.push_state_update();
                self.show_tip(if cn { "中文标点" } else { "英文标点" });
                self.notify_toolbar();
                true
            }
            "toggle_s2t" => {
                if self.s2t.is_none() {
                    self.show_tip("简繁数据缺失");
                    return true;
                }
                let on = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_enabled = !s.s2t_enabled;
                    s.s2t_enabled
                };
                self.show_tip(if on { "繁體" } else { "简体" });
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

        // 临时拼音模式：路由到专用处理器（独占按键）
        if state.temp_pinyin_mode {
            return self.handle_temp_pinyin_key(&mut state, data);
        }

        // 快捷输入模式：路由到专用处理器（独占按键）
        if state.quick_input_mode {
            return self.handle_quick_input_key(&mut state, data);
        }

        // 触发快捷输入：空缓冲 + 无候选 + 匹配触发键 + 无修饰键
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && self.is_quick_input_trigger(data.key_code)
        {
            state.quick_input_mode = true;
            state.quick_input_buffer.clear();
            state.quick_input_prefix = Self::quick_input_prefix_for(data.key_code).to_string();
            self.update_quick_input_candidates(&mut state);
            let display = state.preedit.clone();
            self.notify_ui_update(&state);
            debug!("Entered quick input mode (prefix={})", state.quick_input_prefix);
            return KeyAction::UpdateComposition {
                text: display.clone(),
                caret_pos: display.chars().count() as u32,
            };
        }

        // 触发临时拼音：码表方案 + 空缓冲 + 匹配触发键 + 无修饰键
        if state.input_buffer.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && self.is_temp_pinyin_trigger(data.key_code)
        {
            if let Some(target) = self.engine_mgr.temp_pinyin_target() {
                state.temp_pinyin_mode = true;
                state.temp_pinyin_schema = target;
                state.temp_pinyin_buffer.clear();
                state.temp_pinyin_prefix =
                    Self::temp_pinyin_prefix_for(data.key_code).to_string();
                self.update_temp_pinyin_candidates(&mut state);
                let display = state.preedit.clone();
                self.notify_ui_update(&state);
                debug!("Entered temp pinyin mode (prefix={})", state.temp_pinyin_prefix);
                return KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                };
            }
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
                        let display = state.preedit.clone();
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
                // Space：选当前高亮候选 / 上屏编码
                if !state.candidates.is_empty() {
                    let idx = self.highlighted_global_index(&state).min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    let out = self.commit_candidate(&mut state, &text);
                    self.notify_ui_hide();
                    Self::commit_action(out, true)
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
                // 数字键 1-9 选当前页第 N 个候选（Shift+数字走标点分支）
                let (start, end) = self.page_range(&state);
                let in_page = (data.key_code - 0x31) as usize;
                let idx = start + in_page;
                if idx < end {
                    let text = state.candidates[idx].text.clone();
                    let out = self.commit_candidate(&mut state, &text);
                    self.notify_ui_hide();
                    Self::commit_action(out, true)
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
                let display = state.preedit.clone();
                self.notify_ui_update(&state);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos: state.input_buffer.len() as u32,
                }
            }
            0x26 | 0x28 => {
                // 上/下方向键：移动高亮（跨页回卷）
                if state.candidates.is_empty() {
                    return if state.input_buffer.is_empty() {
                        KeyAction::PassThrough
                    } else {
                        KeyAction::Consumed
                    };
                }
                let changed = if data.key_code == 0x26 {
                    self.move_up(&mut state)
                } else {
                    self.move_down(&mut state)
                };
                if changed {
                    self.notify_ui_update(&state);
                }
                KeyAction::Consumed
            }
            0x21 | 0x22 => {
                // PageUp / PageDown：翻页
                if state.candidates.is_empty() {
                    return if state.input_buffer.is_empty() {
                        KeyAction::PassThrough
                    } else {
                        KeyAction::Consumed
                    };
                }
                let changed = if data.key_code == 0x21 {
                    self.page_prev(&mut state)
                } else {
                    self.page_next(&mut state)
                };
                if changed {
                    self.notify_ui_update(&state);
                }
                KeyAction::Consumed
            }
            0xBD | 0xBB
                if !state.candidates.is_empty() && data.modifiers & MOD_SHIFT == 0 =>
            {
                // '-' / '=' 翻页（仅有候选且无 Shift 时；否则落入标点分支）
                let changed = if data.key_code == 0xBD {
                    self.page_prev(&mut state)
                } else {
                    self.page_next(&mut state)
                };
                if changed {
                    self.notify_ui_update(&state);
                }
                KeyAction::Consumed
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 触发键优先级链（对齐 Go decideBufferedTrigger，缓冲非空/有候选时）：
                if !shift {
                    // B/C. 二/三候选键 + 候选足够 → 选候选
                    if let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(&state);
                        let idx = start + offset;
                        if idx < end {
                            let text = state.candidates[idx].text.clone();
                            let out = self.commit_candidate(&mut state, &text);
                            self.notify_ui_hide();
                            return Self::commit_action(out, true);
                        }
                    }
                    // D. 模式触发键 → 顶屏高亮候选 + 进模式（快捷输入 > 临时拼音）
                    if self.is_quick_input_trigger(data.key_code) {
                        return self.commit_and_enter_quick_input(&mut state, data.key_code);
                    }
                    if self.is_temp_pinyin_trigger(data.key_code) {
                        if let Some(target) = self.engine_mgr.temp_pinyin_target() {
                            return self.commit_and_enter_temp_pinyin(
                                &mut state,
                                data.key_code,
                                target,
                            );
                        }
                    }
                }
                if let Some(ch) = punct_char(data.key_code, shift) {
                    // 标点/符号键：先上屏首选候选（若有输入），再追加（转换后的）标点
                    let mut out = String::new();
                    if !state.candidates.is_empty() {
                        let idx = self.highlighted_global_index(&state).min(state.candidates.len() - 1);
                        let t = state.candidates[idx].text.clone();
                        self.record_selection(&t);
                        out.push_str(&self.maybe_s2t(&state, &t));
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

    fn handle_focus_lost(&self) {
        // 失焦是稳定的落盘时机，把累积词频持久化
        self.save_freq();
    }

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
        let chinese = state.chinese_mode;
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
        self.show_tip(if chinese { "中" } else { "英" });
        self.notify_toolbar();
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
        self.notify_toolbar();
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
        self.record_selection(&text);
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
