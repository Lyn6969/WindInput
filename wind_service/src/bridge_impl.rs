//! 最小可输入协调器：实现基本中文五笔/拼音输入
//!
//! 集成真实词典（rime codetable .dict.yaml）和配置（config.toml）。
//! 与 Go 版 coordinator 对齐的协议行为。

use std::sync::{Arc, Mutex};
use wind_bridge::handler::*;
use wind_bridge::push::PushServer;
use wind_config::Config;
use wind_dict::cached::CachedDict;
use wind_dict::codetable::CodetableDict;
use wind_engine::pinyin::syllable::SyllableTrie;
use wind_engine::pinyin::dag::Dag;
use wind_engine::pinyin::viterbi::{ViterbiDecoder, WordNode};
use wind_engine::pinyin::scorer::AbbrevMatcher;
use wind_store::freq::FreqTracker;
use wind_ipc::protocol::{EVENT_KEY_DOWN, MOD_CTRL, MOD_ALT};
use wind_ui::manager::{UiCommand, UiManager};
use wind_ui::candidate_window::CandidateItem;
use tracing::{info, debug, warn};

/// 候选词
#[derive(Debug, Clone)]
struct Candidate {
    text: String,
    code: String,
    weight: i32,
    order: i32,
}

/// 协调器状态
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

/// 最小可输入协调器
pub struct MinimalCoordinator {
    state: Mutex<State>,
    push_server: Arc<PushServer>,
    config: Config,
    /// 主词典（五笔/拼音等，自动使用 mmap 缓存）
    dict: Option<CachedDict>,
    /// UI 管理器（候选窗口）
    ui_tx: std::sync::mpsc::Sender<UiCommand>,
    /// 音节 Trie（拼音模式用于连续输入切分）
    syllable_trie: Option<SyllableTrie>,
    /// Viterbi 解码器
    viterbi: ViterbiDecoder,
    /// 词频跟踪器
    freq_tracker: FreqTracker,
}

impl MinimalCoordinator {
    pub fn new(push_server: Arc<PushServer>) -> Arc<Self> {
        // 加载配置
        let data_dir = Config::data_dir();
        let config = Config::load(data_dir.as_deref()).unwrap_or_default();
        let schema_id = config.active_schema().to_string();

        info!("Active schema: {}", schema_id);

        // 加载词典
        let dict = Self::load_dictionary(&schema_id, data_dir.as_deref());

        // 创建 UI 管理器（候选窗口线程）
        let ui_tx = match UiManager::new() {
            Ok(ui) => {
                let tx = ui.sender();
                // 保持 UiManager 存活（它在 drop 时会发送 Shutdown）
                std::mem::forget(ui);
                tx
            }
            Err(e) => {
                warn!("Failed to create UI manager: {}", e);
                // 创建一个不会被使用的通道
                let (tx, _rx) = std::sync::mpsc::channel();
                tx
            }
        };

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
            dict,
            ui_tx,
            syllable_trie: if schema_id == "pinyin" {
                Some(SyllableTrie::new())
            } else {
                None
            },
            viterbi: ViterbiDecoder::new(),
            freq_tracker: FreqTracker::new(),
        })
    }

    /// 加载指定 schema 的词典
    fn load_dictionary(schema_id: &str, data_dir: Option<&Path>) -> Option<CachedDict> {
        use std::path::Path;

        let data_dir = data_dir?;

        // 读取 schema 定义获取词典路径
        let schema_path = data_dir.join("schemas").join(format!("{}.schema.yaml", schema_id));
        if !schema_path.exists() {
            warn!("Schema file not found: {}", schema_path.display());
            return None;
        }

        let schema_content = std::fs::read_to_string(&schema_path).ok()?;
        let schema_yaml: serde_yaml::Value = serde_yaml::from_str(&schema_content).ok()?;

        // 从 dictionaries 列表中找到默认词典
        let dictionaries = schema_yaml.get("dictionaries")?.as_sequence()?;
        let default_dict = dictionaries.iter().find(|d| {
            d.get("default").and_then(|v| v.as_bool()).unwrap_or(false)
        }).or_else(|| dictionaries.first());

        let dict_entry = default_dict?;
        let dict_path = dict_entry.get("path")?.as_str()?;
        let dict_type = dict_entry.get("type").and_then(|v| v.as_str()).unwrap_or("rime_codetable");

        let full_path = data_dir.join("schemas").join(dict_path);
        info!("Loading dictionary: {} (type={})", full_path.display(), dict_type);

        match dict_type {
            "rime_codetable" | "rime_pinyin" => {
                // rime_pinyin 格式有 import_tables 引用子词典
                if dict_type == "rime_pinyin" {
                    Self::load_rime_pinyin_dict(&full_path, data_dir)
                } else {
                    match CachedDict::load(&full_path) {
                        Ok(dict) => {
                            info!("Dictionary loaded: {} entries", dict.len());
                            Some(dict)
                        }
                        Err(e) => {
                            warn!("Failed to load dictionary: {}", e);
                            None
                        }
                    }
                }
            }
            _ => {
                warn!("Unsupported dictionary type: {}", dict_type);
                None
            }
        }
    }

    /// 加载 rime_pinyin 格式词典（支持 import_tables 引用子词典）
    fn load_rime_pinyin_dict(dict_path: &Path, _data_dir: &Path) -> Option<CachedDict> {
        info!("load_rime_pinyin_dict: {}", dict_path.display());

        // 检查合并后的 .wdb 缓存
        let merged_wdb = dict_path.with_extension("merged.wdb");
        if merged_wdb.exists() {
            match CachedDict::load(&merged_wdb) {
                Ok(dict) => {
                    info!("Using merged mmap cache: {} ({} entries)", merged_wdb.display(), dict.len());
                    return Some(dict);
                }
                Err(e) => {
                    warn!("Failed to load merged cache: {}", e);
                }
            }
        }

        let content = match std::fs::read_to_string(dict_path) {
            Ok(c) => {
                info!("Read {} bytes from dict file", c.len());
                c
            }
            Err(e) => {
                warn!("Failed to read dict file: {}", e);
                return None;
            }
        };

        // rime dict 格式有多文档标记（--- 和 ...），需要提取头部 YAML
        let yaml_section = if let Some(start) = content.find("---") {
            let after_start = &content[start + 3..];
            if let Some(end) = after_start.find("...") {
                &after_start[..end]
            } else {
                after_start
            }
        } else {
            &content
        };

        let yaml: serde_yaml::Value = match serde_yaml::from_str(yaml_section) {
            Ok(y) => y,
            Err(e) => {
                warn!("Failed to parse YAML header: {}", e);
                return None;
            }
        };

        // 获取词典所在目录
        let dict_dir = match dict_path.parent() {
            Some(d) => d,
            None => {
                warn!("No parent directory for dict path");
                return None;
            }
        };
        info!("Dict directory: {}", dict_dir.display());

        // 直接从子词典 mmap 写入合并 .wdb，不经过内存合并
        let merged_wdb = dict_path.with_extension("merged.wdb");
        let mut writer = wind_dict::binformat::DictWriter::new();
        let mut total_entries = 0usize;

        // 收集所有子词典路径
        let mut sub_paths: Vec<std::path::PathBuf> = Vec::new();
        sub_paths.push(dict_path.to_path_buf());

        if let Some(import_tables) = yaml.get("import_tables").and_then(|v| v.as_sequence()) {
            for table_ref in import_tables {
                if let Some(table_name) = table_ref.as_str() {
                    let sub_path = dict_dir.join(format!("{}.dict.yaml", table_name));
                    if sub_path.exists() {
                        sub_paths.push(sub_path);
                    }
                }
            }
        }

        // 逐个加载子词典并直接导出到 writer
        for sub_path in &sub_paths {
            match CachedDict::load(sub_path) {
                Ok(sub_dict) => {
                    let count = sub_dict.len();
                    info!("  Loading {} entries from {}", count, sub_path.display());
                    // 使用 search_prefix 获取所有条目（限制合理大小）
                    let entries = sub_dict.search_prefix("", 500_000);
                    for (code, text, weight, _order) in entries {
                        writer.add(code, vec![(text, weight)]);
                    }
                    total_entries += count;
                }
                Err(e) => {
                    warn!("  Failed to load {}: {}", sub_path.display(), e);
                }
            }
        }

        if total_entries == 0 {
            warn!("No entries loaded from pinyin dictionary");
            return None;
        }

        info!("Writing merged .wdb cache ({} entries)...", total_entries);
        match writer.write(&merged_wdb) {
            Ok(_) => {
                info!("Wrote merged .wdb cache: {}", merged_wdb.display());
                match CachedDict::load(&merged_wdb) {
                    Ok(dict) => {
                        info!("Using merged mmap cache ({} entries)", dict.len());
                        return Some(dict);
                    }
                    Err(e) => {
                        warn!("Failed to open merged cache: {}", e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to write merged cache: {}", e);
            }
        }

        // 回退：加载到内存
        let merged = CodetableDict::load(dict_path).ok()?;
        Some(CachedDict::Memory(merged))
    }

    /// 从 CachedDict 提取所有条目（用于合并子词典）
    fn extract_all_entries(dict: &CachedDict) -> Vec<(String, String, i32, i32)> {
        // 使用空前缀搜索获取所有条目（限制为大数）
        dict.search_prefix("", 1_000_000)
            .into_iter()
            .map(|(code, text, weight, order)| (code, text, weight, order))
            .collect()
    }

    /// 构建当前状态的 StatusUpdateData
    fn build_status(&self) -> StatusUpdateData {
        let state = self.state.lock().unwrap();
        StatusUpdateData {
            chinese_mode: state.chinese_mode,
            full_width: state.full_width,
            chinese_punct: state.chinese_punct,
            toolbar_visible: state.toolbar_visible,
            caps_lock: state.caps_lock,
            icon_label: if state.chinese_mode { "中".to_string() } else { "英".to_string() },
            key_down_hotkeys: vec![],
            key_up_hotkeys: vec![],
        }
    }

    /// 推送 ActivationStatusPush 到 push pipe
    fn push_activation_status(&self) {
        let status = self.build_status();
        info!("Pushing ActivationStatusPush: chinese_mode={}", status.chinese_mode);
        let encoded = wind_ipc::codec::encode_activation_status_push(
            status.chinese_mode,
            status.full_width,
            status.chinese_punct,
            status.toolbar_visible,
            status.caps_lock,
            false,
            &status.key_down_hotkeys,
            &status.key_up_hotkeys,
            &status.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    /// 推送 StatePush 到 push pipe
    fn push_state_update(&self) {
        let status = self.build_status();
        debug!("Pushing StatePush: chinese_mode={}", status.chinese_mode);
        let encoded = wind_ipc::codec::encode_state_push(
            status.chinese_mode,
            status.full_width,
            status.chinese_punct,
            status.toolbar_visible,
            status.caps_lock,
            &status.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    /// 根据输入缓冲区更新候选词
    ///
    /// 完整流程（对齐 Go 版 convertCore）：
    /// 1. 精确查找
    /// 2. Viterbi 长句解码（拼音模式）
    /// 3. DAG 子短语查找
    /// 4. 前缀查找
    /// 5. 缩写匹配
    /// 6. 频率 boost 排序
    fn update_candidates(
        state: &mut State,
        dict: Option<&CachedDict>,
        syllable_trie: Option<&SyllableTrie>,
        viterbi: &ViterbiDecoder,
        freq_tracker: &FreqTracker,
    ) {
        state.candidates.clear();
        if state.input_buffer.is_empty() {
            return;
        }

        let input = &state.input_buffer;

        if let Some(dict) = dict {
            // 1. 精确查找（完整匹配）
            let exact = dict.search(input);
            if !exact.is_empty() {
                for (text, weight, order) in exact {
                    let boosted = weight + freq_tracker.get_boost(&text) as i32;
                    state.candidates.push(Candidate {
                        text,
                        code: input.clone(),
                        weight: boosted,
                        order,
                    });
                }
            }

            if let Some(trie) = syllable_trie {
                let dag = Dag::build(input, trie);
                let syllables = dag.maximum_match();

                // 2. Viterbi 长句解码（>=2 个音节时）
                if syllables.len() >= 2 {
                    // 构建 lattice：按 endPos 索引的词节点
                    let input_len = input.len();
                    let mut lattice: Vec<Vec<WordNode>> = vec![Vec::new(); input_len + 1];

                    // 对每个起始位置，尝试 1~6 个连续音节组合
                    for start in 0..syllables.len() {
                        for end in (start + 1)..=syllables.len().min(start + 6) {
                            let code: String = syllables[start..end].join("");
                            let results = dict.search(&code);
                            for (text, weight, _order) in &results {
                                // 计算该词在输入中的字符位置
                                let char_start: usize = syllables[..start].iter().map(|s| s.len()).sum();
                                let char_end: usize = syllables[..end].iter().map(|s| s.len()).sum();
                                if char_end <= input_len {
                                    let log_prob = viterbi.word_log_prob(text, *weight);
                                    lattice[char_end].push(WordNode {
                                        start: char_start,
                                        end: char_end,
                                        word: text.clone(),
                                        log_prob,
                                    });
                                }
                            }
                        }
                    }

                    // Viterbi 解码
                    let result = viterbi.decode(&lattice, input_len);
                    if !result.words.is_empty() {
                        let sentence: String = result.words.join("");
                        if !sentence.is_empty() && !state.candidates.iter().any(|c| c.text == sentence) {
                            let boosted = (result.log_prob as i32).max(1) + freq_tracker.get_boost(&sentence) as i32;
                            state.candidates.insert(0, Candidate {
                                text: sentence,
                                code: input.clone(),
                                weight: boosted,
                                order: 0,
                            });
                        }
                    }
                }

                // 3. DAG 子短语查找
                if syllables.len() >= 2 {
                    for start in 0..syllables.len() {
                        for end in (start + 1)..=syllables.len().min(start + 6) {
                            let code: String = syllables[start..end].join("");
                            if code == *input { continue; }
                            let results = dict.search(&code);
                            for (text, weight, order) in results {
                                if !state.candidates.iter().any(|c| c.text == text) {
                                    let boosted = weight + freq_tracker.get_boost(&text) as i32;
                                    state.candidates.push(Candidate {
                                        text,
                                        code: code.clone(),
                                        weight: boosted,
                                        order,
                                    });
                                }
                            }
                        }
                    }
                }

                // 4. 前缀查找
                let prefix_results = dict.search_prefix(input, 30);
                for (code, text, weight, order) in prefix_results {
                    if !state.candidates.iter().any(|c| c.text == text) {
                        let boosted = weight + freq_tracker.get_boost(&text) as i32;
                        state.candidates.push(Candidate {
                            text,
                            code,
                            weight: boosted,
                            order,
                        });
                    }
                }

                // 5. 缩写匹配
                if AbbrevMatcher::is_abbreviation(input, trie) {
                    let abbrev_results = AbbrevMatcher::find_candidates(input, trie, dict, 10);
                    for abbrev in abbrev_results {
                        if !state.candidates.iter().any(|c| c.text == abbrev.text) {
                            state.candidates.push(Candidate {
                                text: abbrev.text,
                                code: abbrev.code,
                                weight: abbrev.weight,
                                order: 999999,
                            });
                        }
                    }
                }
            } else {
                // 五笔模式：前缀查找
                let prefix = dict.search_prefix(input, 50);
                for (code, text, weight, order) in prefix {
                    let boosted = weight + freq_tracker.get_boost(&text) as i32;
                    state.candidates.push(Candidate {
                        text,
                        code,
                        weight: boosted,
                        order,
                    });
                }
            }
        }

        // 6. 按权重排序，截取前 9 个
        state.candidates.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.order.cmp(&b.order)));
        state.candidates.truncate(9);
    }

    /// 构建预编辑显示文本
    fn build_preedit_display(input: &str, candidates: &[Candidate]) -> String {
        let mut display = String::new();
        display.push_str(input);
        if !candidates.is_empty() {
            display.push_str(" [");
            for (i, cand) in candidates.iter().enumerate() {
                if i > 0 {
                    display.push(' ');
                }
                display.push_str(&format!("{}.{}", i + 1, cand.text));
            }
            display.push(']');
        }
        display
    }

    /// 通知 UI 更新候选窗口
    fn notify_ui_update(&self, state: &State) {
        if state.candidates.is_empty() && state.input_buffer.is_empty() {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            return;
        }

        let items: Vec<CandidateItem> = state.candidates.iter().map(|c| CandidateItem {
            text: c.text.clone(),
            code: c.code.clone(),
        }).collect();

        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit: state.input_buffer.clone(),
            candidates: items,
            selected: 0,
            caret_x: state.caret_x,
            caret_y: state.caret_y,
        });
    }

    /// 通知 UI 隐藏候选窗口
    fn notify_ui_hide(&self) {
        let _ = self.ui_tx.send(UiCommand::HideCandidates);
    }
}

use std::path::Path;

impl MessageHandler for MinimalCoordinator {
    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction {
        if data.event_type != EVENT_KEY_DOWN {
            return KeyAction::PassThrough;
        }

        let mut state = self.state.lock().unwrap();

        // Shift 键切换中英文模式
        if data.key_code == 0xA0 || data.key_code == 0xA1 {
            if state.input_buffer.is_empty() {
                state.chinese_mode = !state.chinese_mode;
                drop(state);
                self.push_state_update();
                return KeyAction::StatusUpdate(self.build_status());
            }
        }

        // 英文模式：直接透传
        if !state.chinese_mode {
            return KeyAction::PassThrough;
        }

        // Ctrl/Alt 组合键透传
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            if !state.input_buffer.is_empty() {
                state.input_buffer.clear();
                state.candidates.clear();
                return KeyAction::ClearComposition;
            }
            return KeyAction::PassThrough;
        }

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
                    Self::update_candidates(&mut state, self.dict.as_ref(), self.syllable_trie.as_ref(), &self.viterbi, &self.freq_tracker);
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
                // Space
                if !state.candidates.is_empty() {
                    let text = state.candidates[0].text.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x0D => {
                // Enter
                if !state.input_buffer.is_empty() {
                    let text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x31..=0x39 => {
                // 数字键 1-9 选择候选
                let idx = (data.key_code - 0x31) as usize;
                if idx < state.candidates.len() {
                    let text = state.candidates[idx].text.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else if !state.input_buffer.is_empty() {
                    // 数字超出候选范围，上屏原始输入 + 数字
                    let mut text = state.input_buffer.clone();
                    state.input_buffer.clear();
                    state.candidates.clear();
                    let digit = (b'0' + data.key_code as u8) as char;
                    text.push(digit);
                    self.notify_ui_hide();
                    KeyAction::InsertText {
                        text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: false,
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            0x41..=0x5A => {
                // A-Z 字母键
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.input_buffer.push(ch);
                Self::update_candidates(&mut state, self.dict.as_ref(), self.syllable_trie.as_ref(), &self.viterbi, &self.freq_tracker);
                let display = Self::build_preedit_display(&state.input_buffer, &state.candidates);
                self.notify_ui_update(&state);
                KeyAction::UpdateComposition {
                    text: display,
                    caret_pos: state.input_buffer.len() as u32,
                }
            }
            _ => {
                if !state.input_buffer.is_empty() {
                    KeyAction::Consumed
                } else {
                    KeyAction::PassThrough
                }
            }
        }
    }

    fn handle_focus_gained(&self, data: &FocusData) -> Option<StatusUpdateData> {
        {
            let mut state = self.state.lock().unwrap();
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
        info!("IME activated, pushing ActivationStatusPush");
        let status = self.build_status();
        self.push_activation_status();
        Some(status)
    }

    fn handle_ime_deactivated(&self) {}

    fn handle_mode_notify(&self, flags: u32) {
        let chinese_mode = (flags & wind_ipc::protocol::STATUS_CHINESE_MODE) != 0;
        let clear_input = (flags & wind_ipc::protocol::STATUS_MODE_CHANGED) != 0;
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = chinese_mode;
        if clear_input {
            state.input_buffer.clear();
            state.candidates.clear();
        }
    }

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = !state.chinese_mode;

        let commit_text = if !state.input_buffer.is_empty() && !state.chinese_mode {
            let text = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            text
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };

        drop(state);
        self.push_state_update();
        let status = self.build_status();
        (Some(status), commit_text)
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap();
        state.chinese_mode = chinese_mode;

        let commit_text = if !state.input_buffer.is_empty() && !chinese_mode {
            let text = state.input_buffer.clone();
            state.input_buffer.clear();
            state.candidates.clear();
            text
        } else {
            state.input_buffer.clear();
            state.candidates.clear();
            String::new()
        };

        drop(state);
        self.push_state_update();
        let status = self.build_status();
        (Some(status), commit_text)
    }

    fn handle_menu_command(&self, command: &str) -> Option<StatusUpdateData> {
        info!("Menu command: {}", command);
        match command {
            "toggle_mode" => {
                let (status, _) = self.handle_toggle_mode();
                status
            }
            "toggle_width" => {
                let mut state = self.state.lock().unwrap();
                state.full_width = !state.full_width;
                drop(state);
                self.push_state_update();
                Some(self.build_status())
            }
            "toggle_punct" => {
                let mut state = self.state.lock().unwrap();
                state.chinese_punct = !state.chinese_punct;
                drop(state);
                self.push_state_update();
                Some(self.build_status())
            }
            _ => {
                debug!("Unknown menu command: {}", command);
                None
            }
        }
    }

    fn handle_composition_terminated(&self) {
        let mut state = self.state.lock().unwrap();
        state.input_buffer.clear();
        state.candidates.clear();
        drop(state);
        self.notify_ui_hide();
    }

    fn handle_caret_update(&self, data: &CaretData) {
        let mut state = self.state.lock().unwrap();
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
    }

    fn handle_caret_pending(&self) {}

    fn handle_selection_changed(&self, _prev_char: u16) {}

    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData> {
        let mut state = self.state.lock().unwrap();

        if state.input_buffer.is_empty() {
            return None;
        }

        let trigger_key = data.trigger_key;
        let text = if trigger_key == 0x20 {
            if !state.candidates.is_empty() {
                state.candidates[0].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else if trigger_key == 0x0D {
            state.input_buffer.clone()
        } else if trigger_key >= 0x31 && trigger_key <= 0x39 {
            let idx = (trigger_key - 0x31) as usize;
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
