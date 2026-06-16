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
use wind_store::Store;
use wind_transform::fullwidth::to_full_width;
use wind_transform::punctuation::PunctuationConverter;
use wind_ui::candidate_window::CandidateItem;
use wind_ui::manager::{
    CandidateOp, MenuCmd, MenuItemSpec, MenuKind, ToolbarAction, UiCommand, UiEvent, UiManager,
};
use wind_ui::toolbar::ToolbarState;

/// VK + shift → 该键产生的 ASCII 标点/符号字符（字母键返回 None，由拼音/码表处理）。
/// 解析配对表（每项 2 字符 "（）"）为 (左,右) 字符对，忽略非法项。
fn parse_pairs(list: &[String]) -> Vec<(char, char)> {
    list.iter()
        .filter_map(|s| {
            let mut it = s.chars();
            match (it.next(), it.next(), it.next()) {
                (Some(l), Some(r), None) => Some((l, r)),
                _ => None,
            }
        })
        .collect()
}

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
/// 简繁变体（与 Go config.S2TVariant 对齐）：(opencc 变体名, 菜单显示名)
const S2T_VARIANTS: [(&str, &str); 4] = [
    ("s2t", "标准繁体"),
    ("s2tw", "台湾繁体"),
    ("s2twp", "台湾繁体（含词汇）"),
    ("s2hk", "香港繁体"),
];

/// 检索范围过滤模式（与 Go config.FilterMode 对齐）：(模式, 菜单显示名)
const FILTER_MODES: [(wind_candidate::FilterMode, &str); 3] = [
    (wind_candidate::FilterMode::Smart, "智能模式"),
    (wind_candidate::FilterMode::General, "常用字"),
    (wind_candidate::FilterMode::Gb18030, "全部字符"),
];

/// 重启信号通道（对齐 Go restartRequestCh）：菜单"重启服务"→ main 重拉进程。
static RESTART_TX: std::sync::OnceLock<std::sync::mpsc::Sender<()>> = std::sync::OnceLock::new();

/// 创建重启信号通道，返回接收端（main 在创建协调器前调用并阻塞等待）。
pub fn restart_signal() -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = RESTART_TX.set(tx);
    rx
}

/// 请求重启服务（菜单触发；向 main 发信号，由 main 释放单例并重拉自身）。
pub fn request_restart() {
    if let Some(tx) = RESTART_TX.get() {
        let _ = tx.send(());
    }
}

struct State {
    chinese_mode: bool,
    full_width: bool,
    chinese_punct: bool,
    /// 简繁转换开关（运行时切换；commit 时把简体输出转繁体）
    s2t_enabled: bool,
    /// 简繁变体（s2t/s2tw/s2twp/s2hk；运行时切换）
    s2t_variant: String,
    /// 检索范围过滤模式（smart/general/gb18030；运行时切换）
    filter_mode: wind_candidate::FilterMode,
    /// 用户是否开启常驻工具栏（菜单开关；与“当前是否激活”正交）。
    toolbar_visible: bool,
    /// 本输入法当前是否处于激活态：IME_ACTIVATED/FocusGained 置真；
    /// IME_DEACTIVATED（切换输入法）与 FocusLost（失焦，含“每应用独立输入法”下切到
    /// 别的输入法的应用）置假。工具栏仅在激活态显示，对齐 Go toolbar_reducer 的
    /// `imeActivated && userWantsVisible` 公式；隐藏经 UI 层 50ms 防抖消除切换闪烁。
    ime_active: bool,
    caps_lock: bool,
    input_buffer: String,
    /// 组合区显示文本（拼音含音节分隔 "ni hao"；码表为原始编码）。
    /// 仅显示输入码/拼音，绝不包含候选列表。
    preedit: String,
    candidates: Vec<Candidate>,
    /// 当前页内高亮候选下标（0-based，相对当前页）——键盘选中项，空格上屏的目标
    selected_index: usize,
    /// 鼠标悬停目标（原始 tag）：-1 无，0..N 候选页内下标，或翻页器 tag。
    /// 与 selected_index 相互独立：悬停只是视觉提示，不改变空格上屏的目标。
    hover_index: i32,
    /// 当前页码（0-based）
    current_page: usize,
    /// 动态分级加载：当前候选对应的输入码
    candidate_input: String,
    /// 动态分级加载：当前加载上限
    candidate_limit: usize,
    /// 动态分级加载：是否可能还有更多前缀候选未加载
    has_more: bool,
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
    /// 临时英文模式（Shift+字母触发，临时输入英文）
    temp_english_mode: bool,
    /// 临时英文输入缓冲
    temp_english_buffer: String,
    caret_x: i32,
    caret_y: i32,
    caret_height: i32,
    /// 菜单是否打开（打开时键盘事件转发给菜单窗口；UI 自管导航）
    menu_open: bool,
    /// 菜单目标候选（页内下标 + 文本），供候选词条操作/复制
    menu_target_page_local: usize,
    menu_target_text: String,
}

/// 中央协调器
pub struct Coordinator {
    state: Mutex<State>,
    push_server: Arc<PushServer>,
    config: Config,
    ui_tx: std::sync::mpsc::Sender<UiCommand>,
    engine_mgr: EngineManager,
    /// redb 持久化存储（用户词/临时词/词频/影子规则）；None=无持久化（headless 测试）。
    store: Option<Arc<Store>>,
    compiled_hotkeys: CompiledHotkeys,
    /// 标点转换器（引号左右状态）
    punct: Mutex<PunctuationConverter>,
    /// 短语层（system.phrases.toml；$Y$M$D 模板）
    phrases: crate::phrases::PhraseLayer,
    /// 简繁转换器（OpenCC；None=数据缺失不可用）。变体可运行时切换，故置于 Mutex。
    s2t: Mutex<Option<wind_transform::s2t::Converter>>,
    /// OpenCC 数据目录（运行时按变体重载转换器用）
    opencc_dir: Option<std::path::PathBuf>,
    /// 通用规范汉字表（检索范围"常用字"判定；空集时退化为不过滤）
    common_chars: wind_candidate::CommonChars,
    // Shadow 规则已迁至 redb（self.store 的 SHADOW 表）。
    /// 工具栏位置持久化文件路径（toolbar_pos.txt；None=不持久化）
    toolbar_pos_path: Option<std::path::PathBuf>,
    /// 候选反查（编码/拆字/拼音）供悬停提示
    reverse: crate::reverse::ReverseLookup,
    /// 标点配对：中/英配对表（left,right）+ 跟踪栈（用于智能跳过）
    cn_pairs: Vec<(char, char)>,
    en_pairs: Vec<(char, char)>,
    pair_tracker: Mutex<wind_transform::pair_tracker::PairTracker>,
    /// 最近一次有效光标坐标 (x,y,height)；用于无效坐标时回退，避免候选窗跑到左上角
    last_valid_caret: Mutex<(i32, i32, i32)>,
    /// 正在等待有效光标坐标（首次连接尚未拿到时为 true）；拿到后触发重定位
    awaiting_caret: Mutex<bool>,
    /// 主题目录（data/themes）
    themes_dir: Option<std::path::PathBuf>,
    /// 当前主题名
    theme_name: Mutex<String>,
    /// 主题暗色模式
    theme_dark: Mutex<bool>,
    /// 主题选择持久化文件（theme.txt）
    theme_path: Option<std::path::PathBuf>,
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
        let (ui_tx, event_rx) = match UiManager::new() {
            Ok(mut ui) => {
                let tx = ui.sender();
                let rx = ui.take_event_rx();
                std::mem::forget(ui); // 进程生命周期内保持 UI 线程存活
                (tx, rx)
            }
            Err(e) => {
                warn!("Failed to create UI manager: {}", e);
                let (tx, _rx) = std::sync::mpsc::channel();
                (tx, None)
            }
        };

        // 用户配置目录：theme.txt 等小型 UI 偏好的锚点（词频已迁 redb，不再用 freq.tsv）。
        let user_dir =
            Config::user_config_dir().or_else(|| data_dir.as_deref().map(|d| d.to_path_buf()));
        // redb 用户数据库（本机数据，不随漫游；debug 变体已隔离到 WindInputDebug）。
        let store = Config::local_dir().and_then(|d| {
            let _ = std::fs::create_dir_all(&d);
            let p = d.join("userdata.redb");
            match Store::open(&p) {
                Ok(s) => {
                    info!("Opened redb store: {}", p.display());
                    Some(Arc::new(s))
                }
                Err(e) => {
                    warn!("Failed to open redb store {}: {}", p.display(), e);
                    None
                }
            }
        });
        let coordinator =
            Self::build(config, data_dir.as_deref(), push_server, ui_tx, user_dir, store);

        // 鼠标事件处理线程：候选窗的点击/悬停/滚轮经此回到协调器
        if let Some(rx) = event_rx {
            let c = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                for ev in rx {
                    c.handle_ui_event(ev);
                }
                debug!("UI event channel closed");
            });
        }

        // 恢复持久化的工具栏位置
        if let Some((x, y)) = coordinator.load_toolbar_pos() {
            let _ = coordinator.ui_tx.send(UiCommand::SetToolbarPos { x, y });
        }

        // 加载并下发初始主题
        let name = coordinator
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        coordinator.push_theme(&name, false);
        coordinator
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
        Self::build(config, data_dir.as_deref(), push_server, ui_tx, None, None)
    }

    fn build(
        config: Config,
        data_dir: Option<&Path>,
        push_server: Arc<PushServer>,
        ui_tx: std::sync::mpsc::Sender<UiCommand>,
        user_dir: Option<std::path::PathBuf>,
        store: Option<Arc<Store>>,
    ) -> Arc<Self> {
        // 注入 redb Store：码表引擎注册用户词/临时词层，用户词进候选合并。
        let engine_mgr = EngineManager::with_store(&config, data_dir, store.clone());
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
        let opencc_dir = data_dir.map(|d| d.join("opencc"));
        let s2t_variant = if config.features.s2t.variant.is_empty() {
            "s2t".to_string()
        } else {
            config.features.s2t.variant.clone()
        };
        let s2t = opencc_dir.as_ref().and_then(|dir| {
            let conv = wind_transform::s2t::Converter::load_variant(dir, &s2t_variant);
            if conv.is_some() {
                info!("Loaded S2T converter (variant={})", s2t_variant);
            }
            conv
        });

        // 词频已迁 redb（self.store 的 FREQ 表，选词时 record_freq）。

        // 标点配对表（解析为 (左,右) 字符对）
        let cn_pairs = parse_pairs(&config.input.auto_pair.chinese_pairs);
        let en_pairs = parse_pairs(&config.input.auto_pair.english_pairs);

        // 通用规范汉字表（检索范围"常用字"判定）
        let common_chars = wind_candidate::CommonChars::load(
            &data_dir
                .map(|d| d.join("schemas").join("common_chars.txt"))
                .unwrap_or_default(),
        );
        if common_chars.is_empty() {
            warn!("common_chars.txt 缺失，检索范围过滤将退化为不过滤");
        } else {
            info!("Loaded common chars table");
        }

        // 候选反查表（拆字/拼音）
        let reverse = crate::reverse::ReverseLookup::load(data_dir);
        if !reverse.is_empty() {
            info!("Loaded reverse-lookup (chaizi/pinyin)");
        }

        // Shadow 规则已迁至 redb（self.store 的 SHADOW 表，事务持久），不再用 shadow.json。
        // 工具栏位置属本机状态（不随漫游）：放 %LOCALAPPDATA%\WindInput
        let toolbar_pos_path = Config::local_dir().map(|d| d.join("toolbar_pos.txt"));
        let theme_path = user_dir.as_ref().map(|d| d.join("theme.txt"));
        let themes_dir = data_dir.map(|d| d.join("themes"));
        // 初始主题名：theme.txt（用户上次选择）> config.ui.theme > "default"
        let initial_theme = theme_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                let t = config.ui.theme.name.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            })
            .unwrap_or_else(|| "default".to_string());

        let coordinator = Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: config.general.default_chinese_mode,
                full_width: config.general.default_full_width,
                chinese_punct: config.general.default_chinese_punct,
                s2t_enabled: config.features.s2t.enabled,
                s2t_variant: s2t_variant.clone(),
                filter_mode: wind_candidate::FilterMode::from_str(&config.input.filter_mode),
                toolbar_visible: true,
                ime_active: false, // 启动未激活：工具栏待 IME_ACTIVATED/FocusGained 才显示
                caps_lock: false,
                input_buffer: String::new(),
                preedit: String::new(),
                candidates: Vec::new(),
                selected_index: 0,
                hover_index: -1,
                current_page: 0,
                candidate_input: String::new(),
                candidate_limit: 0,
                has_more: false,
                temp_pinyin_mode: false,
                temp_pinyin_buffer: String::new(),
                temp_pinyin_schema: String::new(),
                temp_pinyin_prefix: String::new(),
                quick_input_mode: false,
                quick_input_buffer: String::new(),
                quick_input_prefix: String::new(),
                temp_english_mode: false,
                temp_english_buffer: String::new(),
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
                menu_open: false,
                menu_target_page_local: 0,
                menu_target_text: String::new(),
            }),
            push_server,
            config,
            ui_tx,
            engine_mgr,
            store,
            compiled_hotkeys,
            punct: Mutex::new(PunctuationConverter::new()),
            phrases,
            s2t: Mutex::new(s2t),
            opencc_dir,
            common_chars,
            toolbar_pos_path,
            reverse,
            cn_pairs,
            en_pairs,
            pair_tracker: Mutex::new(wind_transform::pair_tracker::PairTracker::new()),
            last_valid_caret: Mutex::new((0, 0, 0)),
            awaiting_caret: Mutex::new(false),
            themes_dir,
            theme_name: Mutex::new(initial_theme),
            theme_dark: Mutex::new(false),
            theme_path,
        });
        // 启动即显示常驻工具栏（反映初始 中英/方案/标点/全半角）
        coordinator.notify_toolbar();
        coordinator
    }

    /// 记录一次选词到 redb FREQ（词频维度：count+1、last_used=now，按 schema+code+text）。
    /// 词频是与权重解耦的独立维度（frequency.md），仅记真实使用数据；redb 事务即时持久。
    fn record_selection(&self, code: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(store) = &self.store {
            let schema = self.engine_mgr.active_schema_id();
            if let Err(e) = store.record_freq(&schema, code, text) {
                warn!("record_freq failed: {}", e);
            }
        }
    }

    /// 词频重排（独立维度，**绝不改 weight**）：按 redb 词频 count 做 used-first 稳定重排——
    /// 用过的候选（count>0）按 count 降序上浮，未用候选保持基础(权重)序。对齐 frequency.md §3。
    /// 注：每候选一次 redb 点查（mmap 微秒级）；S1 将下沉到引擎排序层。
    fn apply_freq_rerank(&self, candidates: &mut [Candidate], code: &str) {
        let Some(store) = &self.store else {
            return;
        };
        if code.is_empty() || candidates.len() < 2 {
            return;
        }
        let schema = self.engine_mgr.active_schema_id();
        let counts: std::collections::HashMap<String, u32> = candidates
            .iter()
            .filter_map(|c| match store.get_freq(&schema, code, &c.text) {
                Ok(Some(r)) if r.count > 0 => Some((c.text.clone(), r.count)),
                _ => None,
            })
            .collect();
        if counts.is_empty() {
            return;
        }
        // sort_by 稳定：count 相等（含均为 0）保持原序，故未用候选不被打乱。
        candidates.sort_by(|a, b| {
            let ca = counts.get(&a.text).copied().unwrap_or(0);
            let cb = counts.get(&b.text).copied().unwrap_or(0);
            cb.cmp(&ca)
        });
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
        if self.s2t.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return false;
        }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).s2t_enabled = on;
        true
    }

    /// 候选总数（测试/诊断用）
    pub fn debug_candidate_count(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).candidates.len()
    }

    /// 是否还有更多候选未加载（测试/诊断用）
    pub fn debug_has_more(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).has_more
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

    /// 候选词条操作（测试/诊断用）
    pub fn debug_candidate_op(&self, op: CandidateOp, page_local: usize) {
        self.candidate_op(op, page_local);
    }

    /// 首次加载候选上限（对齐 Go：短前缀小批量分级加载，长前缀近全量）。
    fn initial_candidate_limit(&self, input: &str) -> usize {
        let len = input.chars().count();
        match self.engine_mgr.current_engine_type() {
            Some(wind_engine::engine::EngineType::CodeTable) => match len {
                0 | 1 => 100,
                2 => 300,
                _ => 1000,
            },
            // 拼音 / 混输
            _ => 300,
        }
    }

    /// 用给定上限转换并构建候选（引擎 + 词频 boost + 短语 + 排序去重）。
    /// 返回引擎候选数（不含短语），供判断 has_more。不复位翻页/高亮。
    fn build_candidates(&self, state: &mut State, limit: usize) -> usize {
        let result = self.engine_mgr.convert(&state.input_buffer, limit);
        // 组合区只显示输入码/拼音
        state.preedit = if result.preedit_display.is_empty() {
            state.input_buffer.clone()
        } else {
            result.preedit_display
        };
        let engine_count = result.candidates.len();

        let mut candidates = result.candidates;
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
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.text.clone()));
        // 检索范围过滤（填充常用标志后按模式过滤；对齐 Go 引擎内过滤）
        self.apply_filter(state, &mut candidates);
        // 用户词频重排（独立维度，used-first，绝不改 weight；frequency.md §3）
        self.apply_freq_rerank(&mut candidates, &state.input_buffer);
        // Shadow 规则：删除过滤 + 置顶/移动重排（优先级最高，排序后应用）
        self.apply_shadow(&mut candidates, &state.input_buffer);
        state.candidates = candidates;
        engine_count
    }

    /// 按当前检索范围过滤候选：先填充 is_common（常用字表），再按模式过滤。
    /// Gb18030 或数据缺失时不过滤（避免误删）。
    fn apply_filter(&self, state: &State, candidates: &mut Vec<Candidate>) {
        let mode = state.filter_mode;
        if mode == wind_candidate::FilterMode::Gb18030 || self.common_chars.is_empty() {
            return;
        }
        for c in candidates.iter_mut() {
            // 短语保留（is_phrase 已置位）；其余按常用字表判定
            if !c.is_phrase {
                c.is_common = self.common_chars.is_string_common(&c.text);
            }
        }
        let taken = std::mem::take(candidates);
        *candidates = wind_candidate::filter_candidates(taken, mode);
    }

    /// 应用 Shadow 规则：先按 deleted 过滤，再把 pinned 按目标位置重排。
    fn apply_shadow(&self, candidates: &mut Vec<Candidate>, code: &str) {
        if code.is_empty() {
            return;
        }
        let Some(store) = &self.store else {
            return;
        };
        let schema = self.engine_mgr.active_schema_id();
        let rec = match store.get_shadow_rules(&schema, code) {
            Ok(Some(r)) => r,
            _ => return,
        };
        if !rec.deleted.is_empty() {
            candidates.retain(|c| !rec.deleted.iter().any(|d| d == &c.text));
        }
        // 按 position 升序应用，使后续插入考虑前面已就位的项
        let mut pins = rec.pinned.clone();
        pins.sort_by_key(|p| p.position);
        for pin in pins {
            if let Some(cur) = candidates.iter().position(|c| c.text == pin.word) {
                let cand = candidates.remove(cur);
                let at = pin.position.min(candidates.len());
                candidates.insert(at, cand);
            }
        }
    }

    /// 根据输入缓冲更新候选（动态分级加载：首次小批量，翻页到边界再扩展）。
    fn update_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.preedit = state.input_buffer.clone();
        if state.input_buffer.is_empty() {
            state.has_more = false;
            state.candidate_input.clear();
            return;
        }
        let limit = self.initial_candidate_limit(&state.input_buffer);
        let engine_count = self.build_candidates(state, limit);
        state.candidate_input = state.input_buffer.clone();
        state.candidate_limit = limit;
        // 引擎返回数达到上限 → 可能还有更多未加载
        state.has_more = engine_count >= limit;
        // 候选变化：复位翻页与高亮（含清除鼠标悬停）
        state.current_page = 0;
        state.selected_index = 0;
        state.hover_index = -1;
    }

    /// 扩展候选（翻页/下移到边界时调用）：上限翻倍（≤5000）重新加载，保持当前页/高亮。
    fn expand_candidates(&self, state: &mut State) {
        if !state.has_more || state.candidate_input != state.input_buffer {
            return;
        }
        let new_limit = (state.candidate_limit.saturating_mul(2)).min(5000);
        if new_limit <= state.candidate_limit {
            state.has_more = false;
            return;
        }
        let prev_len = state.candidates.len();
        let engine_count = self.build_candidates(state, new_limit);
        if state.candidates.len() <= prev_len {
            // 没有新增 → 已到底
            state.has_more = false;
            return;
        }
        state.candidate_limit = new_limit;
        state.has_more = engine_count >= new_limit;
        // 保持当前页/高亮不变（build_candidates 未改动它们）
    }

    /// 每页候选数（来自配置，至少 1）
    fn per_page(&self) -> usize {
        self.config.ui.candidate.per_page.max(1)
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
        state.hover_index = -1;
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
        state.hover_index = -1;
        if state.candidates.is_empty() {
            return false;
        }
        // 接近末页且有更多 → 先动态扩展加载
        if state.has_more && state.current_page + 2 >= self.total_pages(state) {
            self.expand_candidates(state);
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
        state.hover_index = -1;
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
        state.hover_index = -1;
        // 接近末页且有更多 → 先动态扩展加载，使新页可达
        if state.has_more && state.current_page + 2 >= self.total_pages(state) {
            self.expand_candidates(state);
        }
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

        // 临时拼音候选按词库权重排序（其词频维度涉及特殊模式配置归属，待 S1 引擎层处理）。
        let mut candidates = result.candidates;
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
                    self.record_selection(&state.input_buffer, &text);
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
                    self.record_selection(&state.input_buffer, &text);
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
                            self.record_selection(&state.input_buffer, &text);
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
                    self.record_selection(&state.input_buffer, &text);
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

    /// 数字后智能标点：在中文标点模式下，若 ch 在智能标点列表且光标前一字符为数字，
    /// 则该标点应按英文（半角）输出（如 "3." 不转成 "3。"）。
    fn is_smart_punct_after_digit(&self, ch: char, prev_char: u16) -> bool {
        if !self.config.input.smart_punct_after_digit {
            return false;
        }
        let list = &self.config.input.smart_punct_list;
        let in_list = if list.is_empty() {
            ch == '.' || ch == ','
        } else {
            list.contains(ch)
        };
        if !in_list {
            return false;
        }
        // prev_char 为 UTF-16 单元，数字 '0'..='9' = 0x30..=0x39
        (0x30..=0x39).contains(&prev_char)
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

    // ───────────────────────── 临时英文 ─────────────────────────

    /// 退出临时英文模式并清空状态
    fn exit_temp_english(&self, state: &mut State) {
        state.temp_english_mode = false;
        state.temp_english_buffer.clear();
        state.preedit.clear();
    }

    /// 临时英文模式按键处理（首版：缓冲累积 + 空格/回车/标点上屏，暂无词库候选）
    fn handle_temp_english_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let comp = |buf: &str| KeyAction::UpdateComposition {
            text: buf.to_string(),
            caret_pos: buf.chars().count() as u32,
        };
        match data.key_code {
            0x1B => {
                // Esc：放弃退出
                self.exit_temp_english(state);
                KeyAction::ClearComposition
            }
            0x08 => {
                // 退格：删字符，空则退出
                state.temp_english_buffer.pop();
                if state.temp_english_buffer.is_empty() {
                    self.exit_temp_english(state);
                    KeyAction::ClearComposition
                } else {
                    comp(&state.temp_english_buffer)
                }
            }
            0x20 | 0x0D => {
                // 空格/回车：上屏缓冲
                let mut text = state.temp_english_buffer.clone();
                if state.full_width {
                    text = to_full_width(&text);
                }
                self.exit_temp_english(state);
                if text.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(text, true)
                }
            }
            0x41..=0x5A => {
                // 字母：Shift 大写，否则小写
                let shift = data.modifiers & MOD_SHIFT != 0;
                let base = data.key_code - 0x41;
                let ch = if shift {
                    (b'A' + base as u8) as char
                } else {
                    (b'a' + base as u8) as char
                };
                state.temp_english_buffer.push(ch);
                comp(&state.temp_english_buffer)
            }
            0x30..=0x39 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字直接入缓冲（英文常含数字，如 v2）
                let ch = (b'0' + (data.key_code - 0x30) as u8) as char;
                state.temp_english_buffer.push(ch);
                comp(&state.temp_english_buffer)
            }
            _ => {
                // 其它（标点等）：上屏缓冲 + 转换后的标点，退出
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let mut text = state.temp_english_buffer.clone();
                    if state.full_width {
                        text = to_full_width(&text);
                    }
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_temp_english(state);
                    Self::commit_action(format!("{}{}", text, punct), true)
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
            self.record_selection(&state.input_buffer, &t);
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
            self.record_selection(&state.input_buffer, &t);
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
            if let Some(conv) = self.s2t.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                return conv.convert(text);
            }
        }
        text.to_string()
    }

    /// 提交某个候选（记录原始简体词频后清空状态），返回上屏文本（按需简繁转换）。
    fn commit_candidate(&self, state: &mut State, text: &str) -> String {
        self.record_selection(&state.input_buffer, text);
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
        let t_nu = std::time::Instant::now();
        // 仅推送当前页候选（窗口按 1..N 编号，翻页后重新编号）
        let (start, end) = self.page_range(state);
        // 快捷输入用字母标签（a/b/c，因数字键需录入表达式），其余用数字
        let alpha = state.quick_input_mode;
        let items: Vec<CandidateItem> = state.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let disp = self.maybe_s2t(state, &c.text);
                // 反查提示：优先逐字编码/拼音，无则回退引擎给的整体编码
                let mut tooltip = self.reverse.tooltip_for(&disp);
                if tooltip.is_empty() && !c.code.is_empty() {
                    tooltip = c.code.clone();
                }
                CandidateItem {
                    // 开启简繁时显示也转繁体（内部候选仍存简体，用于词频/匹配）
                    text: disp,
                    code: c.code.clone(),
                    label: if alpha {
                        ((b'a' + i as u8) as char).to_string()
                    } else {
                        (i + 1).to_string()
                    },
                    tooltip,
                }
            })
            .collect();
        // 翻页信息改为结构化字段传给候选窗（窗口内渲染独立的页码指示）
        let total_pages = self.total_pages(state);
        let selected = state.selected_index.min(items.len().saturating_sub(1));
        // 悬停目标独立于选中项：候选越界视为无悬停，翻页器 tag 原样透传
        let hover = match state.hover_index {
            h if (0..wind_ui::manager::HOVER_PAGE_PREV).contains(&h) => {
                if (h as usize) < items.len() {
                    h
                } else {
                    -1
                }
            }
            h => h, // 翻页器 tag / -1
        };
        // 有效光标坐标判定：高度>0、非 (0,0)、在合理范围；无效则回退到最近有效坐标
        let (cx, cy, ch) = (state.caret_x, state.caret_y, state.caret_height);
        let valid = ch > 0 && !(cx == 0 && cy == 0) && cx.abs() < 32000 && cy.abs() < 32000;
        let (caret_x, caret_y, caret_height, caret_valid) = {
            let mut lv = self.last_valid_caret.lock().unwrap_or_else(|e| e.into_inner());
            if valid {
                *lv = (cx, cy, ch);
                (cx, cy, ch, true)
            } else if lv.2 > 0 {
                (lv.0, lv.1, lv.2, true) // 回退到最近有效坐标，避免跑到屏幕左上角
            } else {
                (cx, cy, ch, false) // 尚无任何有效坐标：临时显示，待有效坐标到达再重定位
            }
        };
        *self.awaiting_caret.lock().unwrap_or_else(|e| e.into_inner()) = !caret_valid;
        let n_items = items.len();
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit: state.preedit.clone(),
            candidates: items,
            selected,
            hover,
            page: state.current_page + 1,
            total_pages,
            caret_x,
            caret_y,
            caret_height,
            caret_valid,
        });
        tracing::debug!("notify_ui_update: build+send {:?} (n={})", t_nu.elapsed(), n_items);
    }

    fn notify_ui_hide(&self) {
        let _ = self.ui_tx.send(UiCommand::HideCandidates);
    }

    // ———————————————— 鼠标交互（来自 UI 线程的反向事件）————————————————

    /// 分发 UI 鼠标事件（在专用线程中执行，可安全加锁/推送）
    fn handle_ui_event(&self, ev: UiEvent) {
        match ev {
            UiEvent::CandidateSelect(i) => self.mouse_select(i),
            UiEvent::Page(dir) => self.mouse_page(dir),
            UiEvent::Hover(i) => self.mouse_hover(i),
            UiEvent::Toolbar(a) => self.mouse_toolbar(a),
            UiEvent::ToolbarMoved { x, y } => self.save_toolbar_pos(x, y),
            UiEvent::CandidateOp { op, page_local } => self.candidate_op(op, page_local),
            UiEvent::RequestCandidateMenu { page_local, x, y } => {
                self.show_candidate_menu(page_local, x, y)
            }
            UiEvent::RequestMainMenu { x, y } => self.show_main_menu(x, y),
            UiEvent::MenuAction(kind) => self.menu_action(kind),
            UiEvent::MenuClose => self.menu_close(),
        }
    }

    /// 菜单项激活：UI 已自管导航/子菜单，这里仅按动作派发。
    fn menu_action(&self, kind: MenuKind) {
        let (page_local, text) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.menu_target_page_local, s.menu_target_text.clone())
        };
        self.menu_close();
        match kind {
            MenuKind::Op(op) => self.candidate_op(op, page_local),
            MenuKind::Copy => {
                let _ = self.ui_tx.send(UiCommand::CopyToClipboard(text));
            }
            MenuKind::Command(cmd) => self.run_menu_cmd(cmd),
            MenuKind::Submenu | MenuKind::Separator => {}
        }
    }

    /// 执行功能主菜单命令
    fn run_menu_cmd(&self, cmd: MenuCmd) {
        match cmd {
            MenuCmd::SchemaEnglish => {
                self.handle_system_mode_switch(false);
                self.notify_toolbar();
                self.notify_ui_hide();
            }
            MenuCmd::SchemaSelect(i) => self.select_schema(i),
            MenuCmd::TogglePunct => {
                self.handle_menu_command("toggle_punct");
                self.notify_toolbar();
            }
            MenuCmd::ToggleWidth => {
                self.handle_menu_command("toggle_width");
                self.notify_toolbar();
            }
            MenuCmd::ToggleS2t => {
                self.handle_menu_command("toggle_s2t");
                self.notify_toolbar();
            }
            MenuCmd::S2tVariant(i) => self.set_s2t_variant(i),
            MenuCmd::FilterMode(i) => self.set_filter_mode(i),
            MenuCmd::ThemeSelect(i) => self.select_theme(i),
            MenuCmd::ThemeStyle(style) => self.set_theme_style(style),
            MenuCmd::ToggleToolbar => self.toggle_toolbar(),
            MenuCmd::ReloadConfig => self.reload_config(),
            MenuCmd::RestartService => self.restart_service(),
            MenuCmd::OpenConfigDir
            | MenuCmd::OpenDictionary
            | MenuCmd::OpenSettings
            | MenuCmd::OpenAbout => {
                if let Some(d) = Config::user_config_dir() {
                    let _ = self.ui_tx.send(UiCommand::OpenPath(d.display().to_string()));
                }
            }
        }
    }

    /// 选择第 N 个输入方案（隐含切到中文模式）。
    fn select_schema(&self, index: usize) {
        let list = self.engine_mgr.available_schemas().to_vec();
        if index >= list.len() {
            return;
        }
        let id = list[index].clone();
        self.engine_mgr.switch_schema(&id);
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.chinese_mode = true;
            s.input_buffer.clear();
            s.candidates.clear();
        }
        self.push_state_update();
        self.notify_toolbar();
        self.notify_ui_hide();
        self.show_tip(&id);
    }

    /// 选择第 N 个主题。
    fn select_theme(&self, index: usize) {
        let list = self.list_themes();
        if index >= list.len() {
            return;
        }
        let name = list[index].clone();
        *self.theme_name.lock().unwrap_or_else(|e| e.into_inner()) = name.clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        self.push_theme(&name, dark);
        self.persist_theme(&name);
        self.show_tip(&format!("主题: {}", name));
    }

    /// 设置主题明暗（0 跟随/1 亮/2 暗），用当前主题重解析。
    fn set_theme_style(&self, style: u8) {
        let dark = style == 2;
        *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner()) = dark;
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.push_theme(&name, dark);
        self.show_tip(if dark { "暗色" } else { "亮色" });
    }

    /// 切换简繁变体（0=s2t 1=s2tw 2=s2twp 3=s2hk），重载转换器并刷新候选显示。
    fn set_s2t_variant(&self, index: usize) {
        let (variant, label) = match S2T_VARIANTS.get(index) {
            Some(v) => *v,
            None => return,
        };
        let dir = match &self.opencc_dir {
            Some(d) => d.clone(),
            None => {
                self.show_tip("简繁数据缺失");
                return;
            }
        };
        match wind_transform::s2t::Converter::load_variant(&dir, variant) {
            Some(conv) => {
                *self.s2t.lock().unwrap_or_else(|e| e.into_inner()) = Some(conv);
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_variant = variant.to_string();
                }
                // 组合中则按新变体重渲染候选显示
                let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if !s.candidates.is_empty() {
                    self.notify_ui_update(&s);
                }
                drop(s);
                self.show_tip(label);
            }
            None => self.show_tip("简繁数据缺失"),
        }
    }

    /// 切换检索范围（0 智能/1 常用字/2 全部字符），以新范围重过滤并刷新候选。
    fn set_filter_mode(&self, index: usize) {
        let (mode, label) = match FILTER_MODES.get(index) {
            Some(&(m, l)) => (m, l),
            None => return,
        };
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.filter_mode == mode {
                return;
            }
            s.filter_mode = mode;
        }
        // 组合中：以新范围重建候选并刷新
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.input_buffer.is_empty() {
            self.update_candidates(&mut s);
            self.notify_ui_update(&s);
        }
        drop(s);
        self.show_tip(label);
    }

    /// 用户开关常驻工具栏（菜单）。仅翻转 toolbar_visible，显隐交 notify_toolbar
    /// 单点决策（结合 ime_active）。
    fn toggle_toolbar(&self) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.toolbar_visible = !s.toolbar_visible;
        }
        self.notify_toolbar();
    }

    /// 重启服务进程：隐藏 UI 后向 main 发重启信号（main 释放单例并重拉自身）。
    fn restart_service(&self) {
        info!("Restart service requested from menu");
        self.notify_ui_hide();
        let _ = self.ui_tx.send(UiCommand::HideToolbar);
        crate::request_restart();
    }

    /// 重载配置（best-effort：重新下发当前主题）。
    fn reload_config(&self) {
        let name = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        self.push_theme(&name, dark);
        self.show_tip("已重载");
    }

    /// 持久化主题选择到 theme.txt。
    fn persist_theme(&self, name: &str) {
        if let Some(p) = &self.theme_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, name);
        }
    }

    /// 加载并下发指定主题（失败保留当前）。
    fn push_theme(&self, name: &str, is_dark: bool) {
        let dir = match &self.themes_dir {
            Some(d) => d,
            None => return,
        };
        match wind_theme::ResolvedTheme::load(dir, name, is_dark) {
            Ok(t) => {
                info!("Loaded theme: {} (dark={})", name, is_dark);
                let _ = self.ui_tx.send(UiCommand::SetTheme(Box::new(t)));
            }
            Err(e) => warn!("Failed to load theme {}: {}", name, e),
        }
    }

    /// 列出可用主题（themes 下含 theme.yaml、非 `_` 前缀的目录，按名排序）。
    fn list_themes(&self) -> Vec<String> {
        let dir = match &self.themes_dir {
            Some(d) => d,
            None => return Vec::new(),
        };
        let mut names: Vec<String> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('_'))
                .filter(|n| dir.join(n).join("theme.yaml").exists())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// 循环切换到下一个主题，重绘并持久化选择。
    /// 构建并显示功能主菜单（对齐 Go 统一菜单：方案/主题子菜单 + 勾选态）。
    /// x/y 为屏幕坐标；i32::MIN 表示由 UI 取光标位置。
    fn show_main_menu(&self, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let (chinese, punct, full, s2t, s2t_variant, filter_mode, toolbar_vis) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.chinese_mode,
                s.chinese_punct,
                s.full_width,
                s.s2t_enabled,
                s.s2t_variant.clone(),
                s.filter_mode,
                s.toolbar_visible,
            )
        };
        let cmd = |c: MenuCmd| MenuKind::Command(c);

        // 输入方案子菜单：英文 + 方案单选
        let active = self.engine_mgr.active_schema_id();
        let schemas = self.engine_mgr.available_schemas().to_vec();
        let mut schema_children = vec![M::leaf("英文", cmd(MenuCmd::SchemaEnglish), true, !chinese)];
        if !schemas.is_empty() {
            schema_children.push(M::separator());
            for (i, id) in schemas.iter().enumerate() {
                schema_children.push(M::leaf(
                    id.clone(),
                    cmd(MenuCmd::SchemaSelect(i)),
                    true,
                    chinese && *id == active,
                ));
            }
        }

        // 主题子菜单：主题单选 + 亮/暗
        let themes = self.list_themes();
        let cur_theme = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        let mut theme_children = Vec::new();
        for (i, name) in themes.iter().enumerate() {
            theme_children.push(M::leaf(
                name.clone(),
                cmd(MenuCmd::ThemeSelect(i)),
                true,
                *name == cur_theme,
            ));
        }
        if !theme_children.is_empty() {
            theme_children.push(M::separator());
        }
        theme_children.push(M::leaf("亮色", cmd(MenuCmd::ThemeStyle(1)), true, !dark));
        theme_children.push(M::leaf("暗色", cmd(MenuCmd::ThemeStyle(2)), true, dark));

        // 简入繁出子菜单：启用开关 + 变体单选
        let mut s2t_children = vec![
            M::leaf("启用", cmd(MenuCmd::ToggleS2t), true, s2t),
            M::separator(),
        ];
        for (i, (id, label)) in S2T_VARIANTS.iter().enumerate() {
            s2t_children.push(M::leaf(*label, cmd(MenuCmd::S2tVariant(i)), true, s2t_variant == *id));
        }

        // 检索范围子菜单：过滤模式单选
        let filter_children: Vec<_> = FILTER_MODES
            .iter()
            .enumerate()
            .map(|(i, (m, label))| {
                M::leaf(*label, cmd(MenuCmd::FilterMode(i)), true, filter_mode == *m)
            })
            .collect();

        let items = vec![
            M::submenu("输入方案", schema_children),
            M::leaf("全角", cmd(MenuCmd::ToggleWidth), true, full),
            M::leaf("中文标点", cmd(MenuCmd::TogglePunct), true, punct),
            M::submenu("简入繁出", s2t_children),
            M::submenu("检索范围", filter_children),
            M::separator(),
            M::leaf("显示工具栏", cmd(MenuCmd::ToggleToolbar), true, toolbar_vis),
            M::submenu("主题", theme_children),
            M::separator(),
            M::leaf("重载配置", cmd(MenuCmd::ReloadConfig), true, false),
            M::leaf("重启服务", cmd(MenuCmd::RestartService), true, false),
            M::separator(),
            M::leaf("词库管理...", cmd(MenuCmd::OpenDictionary), true, false),
            M::leaf("设置...", cmd(MenuCmd::OpenSettings), true, false),
            M::separator(),
            M::leaf("关于", cmd(MenuCmd::OpenAbout), true, false),
        ];
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.menu_open = true;
            s.menu_target_page_local = 0;
            s.menu_target_text = String::new();
        }
        let _ = self.ui_tx.send(UiCommand::ShowCandidateMenu { items, x, y });
    }

    fn is_menu_open(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).menu_open
    }

    /// 关闭菜单
    fn menu_close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.menu_open {
            state.menu_open = false;
            drop(state);
            let _ = self.ui_tx.send(UiCommand::HideMenu);
        }
    }

    /// 菜单打开时转发导航键给菜单窗口；返回 true 表示已消费。
    fn forward_menu_key(&self, key_code: u32) -> bool {
        if !self.is_menu_open() {
            return false;
        }
        match key_code {
            // 方向键/回车/空格/ESC → 菜单窗口处理（导航/下钻/返回/激活/关闭）
            0x26 | 0x28 | 0x25 | 0x27 | 0x0D | 0x20 | 0x1B => {
                let _ = self.ui_tx.send(UiCommand::MenuKey(key_code));
            }
            // 其它键：关闭菜单并吞掉
            _ => self.menu_close(),
        }
        true
    }

    /// 构建右键候选菜单项并下发给 UI 显示。
    fn show_candidate_menu(&self, page_local: usize, x: i32, y: i32) {
        use wind_ui::manager::MenuItemSpec as M;
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() || state.input_buffer.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let word = state.candidates[idx].text.clone();
        let code = state.input_buffer.clone();
        let total = state.candidates.len();
        drop(state);

        let schema = self.engine_mgr.active_schema_id();
        let has_rule = self.shadow_has_rule(&schema, &code, &word);
        let multi_char = word.chars().count() > 1;
        let op = |o: CandidateOp| MenuKind::Op(o);

        let items = vec![
            M::leaf("置顶", op(CandidateOp::MoveTop), true, false),
            M::leaf("前移", op(CandidateOp::MoveUp), idx > 0, false),
            M::leaf("后移", op(CandidateOp::MoveDown), idx + 1 < total, false),
            M::leaf("删除", op(CandidateOp::Delete), multi_char, false),
            M::leaf("恢复默认", op(CandidateOp::Reset), has_rule, false),
            M::separator(),
            M::leaf("复制", MenuKind::Copy, true, false),
        ];
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.menu_open = true;
            state.menu_target_page_local = page_local;
            state.menu_target_text = word;
        }
        let _ = self.ui_tx.send(UiCommand::ShowCandidateMenu { items, x, y });
    }

    /// 候选词条操作（右键菜单）：调整 Shadow 规则并即时重排重绘。
    /// code 取当前输入码（state.input_buffer）；按方案隔离。
    fn candidate_op(&self, op: CandidateOp, page_local: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() || state.input_buffer.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let word = state.candidates[idx].text.clone();
        let code = state.input_buffer.clone();
        let schema = self.engine_mgr.active_schema_id();

        // 单字无规则保护：避免把某个单字彻底锁死（在写规则前判定）
        if matches!(op, CandidateOp::Delete) && word.chars().count() <= 1 {
            debug!("candidate_op: 拒绝删除单字 '{}'", word);
            return;
        }
        let last = state.candidates.len().saturating_sub(1);
        if let Some(store) = &self.store {
            // None cand_id：码表静态词无动态短语 id。redb 事务持久，无需显式落盘。
            let r = match op {
                CandidateOp::MoveTop => store.pin_shadow(&schema, &code, &word, None, 0),
                CandidateOp::MoveUp => {
                    store.pin_shadow(&schema, &code, &word, None, idx.saturating_sub(1))
                }
                CandidateOp::MoveDown => {
                    store.pin_shadow(&schema, &code, &word, None, (idx + 1).min(last))
                }
                CandidateOp::Delete => store.delete_shadow(&schema, &code, &word),
                CandidateOp::Reset => store.remove_shadow_rule(&schema, &code, &word),
            };
            if let Err(e) = r {
                warn!("shadow op failed: {}", e);
            }
        }

        // 重新构建候选（会重新应用 Shadow）并重绘
        self.update_candidates(&mut state);
        self.notify_ui_update(&state);
    }

    /// 影子规则：当前 code 是否对 word 有规则（置顶/删除），决定菜单"恢复默认"可用性。
    fn shadow_has_rule(&self, schema: &str, code: &str, word: &str) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        matches!(
            store.get_shadow_rules(schema, code),
            Ok(Some(rec))
                if rec.pinned.iter().any(|p| p.word == word) || rec.deleted.iter().any(|d| d == word)
        )
    }

    /// 读取持久化的工具栏位置（"x y" 文本）
    fn load_toolbar_pos(&self) -> Option<(i32, i32)> {
        let p = self.toolbar_pos_path.as_ref()?;
        let content = std::fs::read_to_string(p).ok()?;
        let mut it = content.split_whitespace();
        let x: i32 = it.next()?.parse().ok()?;
        let y: i32 = it.next()?.parse().ok()?;
        Some((x, y))
    }

    /// 持久化工具栏位置（best-effort）
    fn save_toolbar_pos(&self, x: i32, y: i32) {
        if let Some(p) = &self.toolbar_pos_path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, format!("{} {}", x, y));
        }
    }

    /// 当前模式下生效的配对表（按中/英标点 + 各自开关）
    fn active_pairs(&self, chinese_punct: bool) -> Option<&Vec<(char, char)>> {
        if chinese_punct {
            if self.config.input.auto_pair.chinese {
                return Some(&self.cn_pairs);
            }
        } else if self.config.input.auto_pair.english {
            return Some(&self.en_pairs);
        }
        None
    }

    /// 工具栏单元格点击：复用菜单命令切换状态（内部已推送 C++），再刷新工具栏显示。
    fn mouse_toolbar(&self, action: ToolbarAction) {
        let cmd = match action {
            ToolbarAction::ToggleMode => "toggle_mode",
            ToolbarAction::SwitchEngine => "switch_engine",
            ToolbarAction::TogglePunct => "toggle_punct",
            ToolbarAction::ToggleWidth => "toggle_width",
        };
        self.handle_menu_command(cmd);
        self.notify_toolbar();
    }

    /// 点击选词：提交页内第 N 个候选，经 push 管道异步上屏（对齐 Go PushCommitText）。
    fn mouse_select(&self, page_local: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let (start, end) = self.page_range(&state);
        let idx = start + page_local;
        if idx >= end || idx >= state.candidates.len() {
            return;
        }
        let text = state.candidates[idx].text.clone();
        let chinese_mode = state.chinese_mode;
        let out = self.commit_candidate(&mut state, &text);
        // 鼠标提交后彻底复位各输入模式，避免遗留状态
        state.temp_pinyin_mode = false;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.quick_input_mode = false;
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.temp_english_mode = false;
        state.temp_english_buffer.clear();
        drop(state);

        self.notify_ui_hide();
        let encoded =
            wind_ipc::codec::encode_commit_text(&out, None, false, chinese_mode, false);
        // 仅推给活动客户端，避免广播导致多个 TSF 端重复上屏
        self.push_server.push_commit_to_active(&encoded);
        debug!("mouse_select: committed '{}' (page_local={})", out, page_local);
    }

    /// 滚轮翻页：dir<0 上一页，dir>0 下一页；仅重绘候选窗，不上屏。
    fn mouse_page(&self, dir: i32) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let changed = if dir < 0 {
            self.page_prev(&mut state)
        } else {
            self.page_next(&mut state)
        };
        if changed {
            self.notify_ui_update(&state);
        }
    }

    /// 悬停高亮：设置独立的悬停目标（候选或翻页器），不改键盘选中项，重绘。
    /// target<0 表示离开。空格上屏仍以 selected_index 为准。
    fn mouse_hover(&self, target: i32) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let new_hover = if target == wind_ui::manager::HOVER_PAGE_PREV
            || target == wind_ui::manager::HOVER_PAGE_NEXT
        {
            target // 翻页器悬停
        } else if target >= 0 {
            let (start, end) = self.page_range(&state);
            if (target as usize) < end - start {
                target
            } else {
                -1
            }
        } else {
            -1
        };
        if state.hover_index != new_hover {
            state.hover_index = new_hover;
            self.notify_ui_update(&state);
        }
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
    /// 工具栏可见性单点决策 + 内容刷新。对齐 Go toolbar_reducer 的合取公式：
    /// 仅当 `ime_active && toolbar_visible` 时显示（UpdateToolbar 会刷内容+定位+显示），
    /// 否则下发 HideToolbar。所有调用点（启动/切模式/切方案/激活/失活）经此单点决策，
    /// 不再各自直接显示，根治“工具栏总是显示、切走输入法不隐藏”。
    fn notify_toolbar(&self) {
        let schema_label = Self::schema_display_name(&self.engine_mgr.active_schema_id());
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !(s.ime_active && s.toolbar_visible) {
            drop(s);
            let _ = self.ui_tx.send(UiCommand::HideToolbar);
            return;
        }
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
                if self.s2t.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
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

    /// 切换中英文时取消当前输入：清空缓冲/候选/preedit，并按 `hotkeys.commit_on_switch`
    /// 决定是否把已输入的原始编码上屏（仅在切到英文且有待输入时）。返回待上屏文本。
    fn take_input_on_mode_switch(&self, state: &mut State, chinese: bool) -> String {
        let commit =
            !state.input_buffer.is_empty() && !chinese && self.config.hotkeys.commit_on_switch;
        let text = if commit {
            state.input_buffer.clone()
        } else {
            String::new()
        };
        state.input_buffer.clear();
        state.candidates.clear();
        state.preedit.clear();
        text
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
                let (status, commit_text) = self.handle_toggle_mode();
                // 切到英文且 hotkeys.commit_on_switch=true 且有待输入：上屏原始编码并同时切换模式。
                // （commit_text 仅在切英文时非空，见 take_input_on_mode_switch）
                if !commit_text.is_empty() {
                    return KeyAction::InsertText {
                        text: commit_text,
                        new_composition: None,
                        mode_changed: true,
                        chinese_mode: false,
                        has_new_composition: false,
                    };
                }
                if let Some(status) = status {
                    return KeyAction::StatusUpdate(status);
                }
            }
            return KeyAction::PassThrough;
        }
        if data.event_type != EVENT_KEY_DOWN {
            return KeyAction::PassThrough;
        }

        // ── 右键菜单打开时：方向键/回车/ESC 由菜单消费（优先于一切）──
        if self.is_menu_open() && self.forward_menu_key(data.key_code) {
            return KeyAction::Consumed;
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

        // 临时英文模式：路由到专用处理器（独占按键）
        if state.temp_english_mode {
            return self.handle_temp_english_key(&mut state, data);
        }

        // 触发临时英文：Shift+字母（中文模式 + 空缓冲 + 无候选 + 已启用）
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && self.config.input.shift_temp_english.enabled
            && data.modifiers & MOD_SHIFT != 0
            && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
            && (0x41..=0x5A).contains(&data.key_code)
        {
            let ch = (b'A' + (data.key_code - 0x41) as u8) as char; // 首字母大写
            state.temp_english_mode = true;
            state.temp_english_buffer = ch.to_string();
            self.notify_ui_hide();
            let buf_disp = state.temp_english_buffer.clone();
            debug!("Entered temp English mode (buffer={})", buf_disp);
            return KeyAction::UpdateComposition {
                text: buf_disp.clone(),
                caret_pos: buf_disp.chars().count() as u32,
            };
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

        // Ctrl/Alt 组合（非热键）：有输入则清空并隐藏候选窗，否则透传。
        // 必须 notify_ui_hide：否则候选窗残留（如 Ctrl+A 时卡死，需再输入才复位）。
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            if !state.input_buffer.is_empty() {
                state.input_buffer.clear();
                state.candidates.clear();
                self.notify_ui_hide();
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
                        self.record_selection(&state.input_buffer, &t);
                        out.push_str(&self.maybe_s2t(&state, &t));
                    } else if !state.input_buffer.is_empty() {
                        out.push_str(&state.input_buffer);
                    }
                    let had_input = !state.input_buffer.is_empty() || !state.candidates.is_empty();
                    state.input_buffer.clear();
                    state.candidates.clear();

                    // 数字后智能标点：光标前为数字时该标点按英文输出（如 3. 不转 3。）
                    let smart_en = self.is_smart_punct_after_digit(ch, data.prev_char);
                    // 中文标点转换；智能标点命中或非中文标点模式时按全角/原样输出
                    let piece = if state.chinese_punct && !smart_en {
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
                    // 标点配对（对齐 Go）：插入配对 + 智能跳过
                    let pch = piece.chars().last().unwrap_or(' ');
                    if let Some(pairs) = self.active_pairs(state.chinese_punct) {
                        // 智能跳过：仅无候选前缀（out 即标点本身）时，输右括号→光标右移
                        if out == piece && pairs.iter().any(|(_, r)| *r == pch) {
                            let mut tr =
                                self.pair_tracker.lock().unwrap_or_else(|e| e.into_inner());
                            if tr.peek().map_or(false, |e| e.right == pch) {
                                tr.pop();
                                return KeyAction::MoveCursorRight;
                            }
                            tr.clear();
                        }
                        // 插入配对：左括号 → 补右括号，光标置于其间
                        if let Some((_, right)) =
                            pairs.iter().find(|(l, _)| *l == pch).copied()
                        {
                            self.pair_tracker
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(pch, right);
                            let cursor_offset = out.encode_utf16().count() as u32;
                            let text = format!("{}{}", out, right);
                            return KeyAction::InsertTextWithCursor { text, cursor_offset };
                        }
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
            // 焦点进入文本框 = 本输入法激活（对齐 Go HandleFocusGained → SetIMEActivated(true)）。
            // 不依赖 IME_ACTIVATED 的到达时机，确保工具栏在焦点到达时即可显示。
            state.ime_active = true;
        }
        // 记录活动客户端：鼠标点击的 commit 只推给它，避免广播多发
        if data.client_token != 0 {
            self.push_server.set_active_token(data.client_token);
        }
        let status = self.build_status();
        self.push_activation_status();
        self.notify_toolbar(); // 激活态 → 工具栏显示
        Some(status)
    }

    fn handle_focus_lost(&self) {
        // 词频已即时写入 redb（事务持久），失焦无需再落盘。
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 失焦即视为非激活并隐藏工具栏：用户开启系统“为每个应用窗口使用不同输入法”时，
            // 切到使用别的输入法的应用不会触发 IME_DEACTIVATED，只有 FocusLost。工具栏隐藏经
            // UI 层 50ms 防抖——若紧接着 FocusGained（同输入法切窗/切文本框）会取消隐藏，无闪烁。
            s.ime_active = false;
            // 焦点切换后旧 composition 上下文已失效，清理输入态，避免候选残留到新焦点。
            s.input_buffer.clear();
            s.preedit.clear();
            s.candidates.clear();
            s.menu_open = false; // 复位菜单态，否则下一个键被 forward_menu_key 吞掉
        }
        self.notify_toolbar(); // 隐藏工具栏（防抖）
        self.notify_ui_hide(); // 隐藏候选窗 + 弹出菜单（HideCandidates 连带关菜单）
    }

    fn get_current_mode(&self) -> (bool, bool) {
        // FocusGained 同步路径回传 ModePush：仅锁+读两字段，DLL 正同步阻塞等本值。
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.chinese_mode, s.full_width)
    }

    fn handle_ime_activated(&self, client_token: u64) -> Option<StatusUpdateData> {
        if client_token != 0 {
            self.push_server.set_active_token(client_token);
        }
        self.state.lock().unwrap_or_else(|e| e.into_inner()).ime_active = true;
        let status = self.build_status();
        self.push_activation_status();
        self.notify_toolbar(); // 激活态 → 工具栏显示
        Some(status)
    }

    fn handle_ime_deactivated(&self) {
        // 切走本输入法（换到别的 IME / 非输入法应用）：清激活态、清输入、隐藏全部 UI。
        // 对齐 Go SetIMEActivated(false)（隐藏工具栏 + hideUI），根治“切走仍残留显示”。
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.ime_active = false;
            s.input_buffer.clear();
            s.preedit.clear();
            s.candidates.clear();
            s.menu_open = false;
        }
        self.notify_toolbar(); // 非激活态 → notify_toolbar 内部下发 HideToolbar
        self.notify_ui_hide(); // 隐藏候选窗 + 弹出菜单
    }

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
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese);
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.push_state_update();
        self.show_tip(if chinese { "中" } else { "英" });
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
        (Some(self.build_status()), commit_text)
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = chinese_mode;
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese_mode);
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.push_state_update();
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
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
            "toggle_s2t" => {
                let on = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_enabled = !s.s2t_enabled;
                    s.s2t_enabled
                };
                self.show_tip(if on { "繁" } else { "简" });
                Some(self.build_status())
            }
            _ => None,
        }
    }

    fn handle_composition_terminated(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.input_buffer.clear();
        state.candidates.clear();
        // 复位菜单状态：点击别处会终止 composition 并经 notify_ui_hide 隐藏菜单窗口，
        // 但若不清 menu_open，下一个键会被 forward_menu_key 当作菜单键吞掉（首字符失效）。
        state.menu_open = false;
        drop(state);
        self.notify_ui_hide();
    }

    fn handle_caret_update(&self, data: &CaretData) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
        // 首次连接尚无有效坐标时，候选窗临时显示在左上角；待有效坐标到达即重定位。
        let now_valid =
            data.height > 0 && !(data.x == 0 && data.y == 0) && data.x.abs() < 32000;
        let awaiting = *self.awaiting_caret.lock().unwrap_or_else(|e| e.into_inner());
        let composing = !state.candidates.is_empty() || !state.input_buffer.is_empty();
        if awaiting && now_valid && composing {
            self.notify_ui_update(&state);
        }
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
        let code = state.input_buffer.clone(); // 清空前捕获输入码，供词频记录
        state.input_buffer.clear();
        state.candidates.clear();
        // 与 handle_key_event 的选词路径保持一致：记录词频用于学习排序
        self.record_selection(&code, &text);
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

    fn handle_show_context_menu(&self, x: i32, y: i32) {
        self.show_main_menu(x, y);
    }
}
