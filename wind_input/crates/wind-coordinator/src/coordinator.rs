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

use wind_keys::keymap;
use crate::pipeline::{ModeKind, Rewind};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use wind_bridge::handler::*;
use wind_bridge::push::{PushConfig, PushServer};
use wind_candidate::Candidate;
use wind_config::Config;
use wind_config::hotkey::{self, CompiledHotkeys};
use wind_engine::EngineManager;
use wind_ipc::protocol::{
    EVENT_KEY_DOWN, EVENT_KEY_UP, MOD_ALT, MOD_CTRL, MOD_SHIFT, calc_key_hash,
};
use wind_store::Store;
use wind_store::freq::FreqRecord;
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
    use keymap::*;
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
        VK_SEMICOLON => (';', ':'),
        VK_EQUAL => ('=', '+'),
        VK_COMMA => (',', '<'),
        VK_MINUS => ('-', '_'),
        VK_PERIOD => ('.', '>'),
        VK_SLASH => ('/', '?'),
        VK_BACKTICK => ('`', '~'),
        VK_LBRACKET => ('[', '{'),
        VK_BACKSLASH => ('\\', '|'),
        VK_RBRACKET => (']', '}'),
        VK_QUOTE => ('\'', '"'),
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

/// 英文输入大小写模式（临时英文候选适配用，对齐 Go detectCasePattern）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EnCase {
    Lower,
    Upper,
    Title,
    Mixed,
}

/// 检测缓冲的大小写模式（仅看字母）。
fn detect_en_case(s: &str) -> EnCase {
    let letters: Vec<char> = s.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    if letters.is_empty() {
        return EnCase::Lower;
    }
    if letters.iter().all(|c| c.is_ascii_lowercase()) {
        return EnCase::Lower;
    }
    if letters.len() > 1 && letters.iter().all(|c| c.is_ascii_uppercase()) {
        return EnCase::Upper;
    }
    // 首字母大写、其余小写（含单个大写字母如 "A"）→ Title
    if letters[0].is_ascii_uppercase() && letters[1..].iter().all(|c| c.is_ascii_lowercase()) {
        return EnCase::Title;
    }
    EnCase::Mixed
}

/// 把词库单词适配为输入的大小写模式（对齐 Go adaptCase）。
fn adapt_en_case(word: &str, case: EnCase) -> String {
    match case {
        EnCase::Lower => word.to_lowercase(),
        EnCase::Upper => word.to_uppercase(),
        EnCase::Title => {
            let mut chars = word.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
                None => String::new(),
            }
        }
        EnCase::Mixed => word.to_string(),
    }
}

/// VK + shift → 可打印 ASCII 字符（字母按 shift 决定大小写、数字/符号复用 punct_char）。
/// 用于网址模式原样累积与前缀探测。非可打印键返回 None。
fn printable_char(key_code: u32, shift: bool) -> Option<char> {
    match key_code {
        keymap::VK_A..=keymap::VK_Z => {
            let base = (key_code - 0x41) as u8;
            Some(if shift {
                (b'A' + base) as char
            } else {
                (b'a' + base) as char
            })
        }
        keymap::VK_0..=keymap::VK_9 if !shift => Some((b'0' + (key_code - 0x30) as u8) as char),
        _ => punct_char(key_code, shift),
    }
}

/// 引擎一次转换请求的候选上限（boost 重排后截断到 9）
const ENGINE_MAX_CANDIDATES: usize = 50;

/// 自动造词（L）写入临时层的初始权重与每次复选增量（保守默认；后续可接 schema.learning 配置）。
const LEARN_ADD_WEIGHT: i32 = 800;
const LEARN_WEIGHT_DELTA: i32 = 40;

/// 当前 unix 秒（拼音衰减分以此对 last_used 计龄；与 store record_freq 同口径）。
fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

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
    /// 拼音类组合区「已转换前缀」（逐步转换：选中的汉字累积于此、留在组合区不上屏，
    /// 全部转换完才整体上屏）。内部存简体原文，输出时再 s2t。仅拼音/临拼/混输文本透镜使用，
    /// 码表（五笔）选词消费整串、绝不进入此态。见 docs/redesign/pinyin-composition-enhance.md。
    committed_text: String,
    /// 已转换前缀的分段记录 (消费码, 汉字)：供退格逐段回退与完整上屏时自动造词。
    committed_segs: Vec<(String, String)>,
    /// 当前激活的独占输入模式（临时拼音/快捷输入/临时英文）。`None` = 普通输入。
    /// 单点决策的唯一真相源：结构上保证同一时刻至多一个独占模式（见 `pipeline.rs`）。
    active: Option<ModeKind>,
    /// 临时拼音输入缓冲（拼音串）
    temp_pinyin_buffer: String,
    /// 临时拼音目标方案 id（如 "pinyin"）
    temp_pinyin_schema: String,
    /// 临时拼音组合区前缀字符（触发键，如 "`"）
    temp_pinyin_prefix: String,
    /// 快捷输入缓冲（如 "1+2*3" / "12.25"）
    quick_input_buffer: String,
    /// 快捷输入组合区前缀字符（触发键，如 ";"）
    quick_input_prefix: String,
    /// 临时英文输入缓冲
    temp_english_buffer: String,
    /// 网址模式输入缓冲（原样累积的 URL 文本）
    url_buffer: String,
    /// 统一夺取回退登记（仅在夺取式模式激活时为 Some，见 pipeline::Rewind）
    rewind: Option<Rewind>,
    /// 特殊模式编码缓冲（自带码表的查询码）
    special_buffer: String,
    /// 当前特殊模式下标（= features.special_modes 索引；仅 active==Special 时有效）
    special_id: u8,
    /// 临时 mix 编码缓冲
    mix_buffer: String,
    /// 当前 mix 模式下标（= features.mix_modes 索引；仅 active==Mix 时有效）
    mix_id: u8,
    /// mix 数字模式（仅含 quick_input 成员时有效）：首字符数字/符号 → true（表达式：数字/符号
    /// 输入、字母选词）；首字符字母 → false（拼音/英文：字母输入、数字选词）。
    mix_numeric: bool,
    caret_x: i32,
    caret_y: i32,
    caret_height: i32,
    /// 菜单是否打开（打开时键盘事件转发给菜单窗口；UI 自管导航）
    menu_open: bool,
    /// 菜单目标候选（页内下标 + 文本），供候选词条操作/复制
    menu_target_page_local: usize,
    menu_target_text: String,
}

/// 智能符号模式待命态：press1 提交一个参与集合内的中文标点后武装，等待时限内同键 press2
/// 触发替换。对齐 Go `smartSymbol*` 字段。
#[derive(Default)]
struct SmartSymbolArm {
    armed: bool,
    /// 武装的触发键（原始英文标点字符）
    key: char,
    /// press1 产出的中文标点串（…… 为多 rune），删除数 = 其 rune 数
    str: String,
    /// 武装时刻（None=未武装）；用于时限判定
    at: Option<std::time::Instant>,
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
    /// 配置驱动的候选导航键分类器（翻页/高亮，普通模式与各 overlay 共用）
    nav_keys: keymap::NavKeys,
    /// 标点转换器（引号左右状态）
    punct: Mutex<PunctuationConverter>,
    /// 智能符号模式待命态（同键连按删中文标点改英文）
    smart_symbol: Mutex<SmartSymbolArm>,
    /// 短语层（system.phrases.toml；$Y$M$D 模板）
    phrases: wind_phrase::PhraseLayer,
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
    /// 命令栏（cmdbar）服务束（ime/config/dict 等动作后端），构造后由 init_cmdbar 装配。
    pub(crate) cmdbar_services: std::sync::OnceLock<wind_cmdbar::Services>,
    /// 自身 Weak 引用：$CC 命令在独立线程异步执行（避免持 state 锁回调自锁方法致死锁）。
    pub(crate) self_weak: std::sync::OnceLock<std::sync::Weak<Coordinator>>,
    /// 上屏历史环形缓冲（index 0 = 最近）：供命令栏 `last(n)` 取最近上屏文本。
    recent_commits: Mutex<std::collections::VecDeque<String>>,
    /// preedit 嵌入模式运行时态（命令栏 ime.toggle("preedit") 切换；初值随配置，暂不持久化）。
    preedit_embedded: Mutex<bool>,
    /// 候选窗隐藏开关（命令栏 ime.toggle("candwin") 切换；隐藏时 notify_ui_update 不显示候选）。
    hide_candidate_window: Mutex<bool>,
    /// 候选布局方向运行时态（命令栏 ime.toggle("layout") 切换；true=竖排，初值随配置，持久化）。
    candidate_vertical: Mutex<bool>,
}

/// 短语候选权重基准（高于普通候选，使短语展开排在前列）
const PHRASE_WEIGHT_BASE: i32 = 40_000_000;

/// 一次候选刷新后的输入结局（码表全码/空码策略，仅正向输入字母时消费）。
enum InputOutcome {
    /// 正常更新候选，继续组合。
    Normal,
    /// 全码自动上屏该文本。
    AutoCommit(String),
    /// 满码空码：清空缓冲。
    Clear,
}

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
        let coordinator = Self::build(
            config,
            data_dir.as_deref(),
            push_server,
            ui_tx,
            user_dir,
            store,
        );

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
        // 下发候选布局方向（ui.candidate.layout == "vertical"）。
        let vertical = coordinator.config.ui.candidate.layout.eq_ignore_ascii_case("vertical");
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetCandidateLayout(vertical));
        // 下发预编辑嵌入模式：preedit_mode == "embedded" 且非 inline_preedit（inline 时编码内联在应用）。
        let cand_cfg = &coordinator.config.ui.candidate;
        let embedded =
            !cand_cfg.inline_preedit && cand_cfg.preedit_mode.eq_ignore_ascii_case("embedded");
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetPreeditEmbedded(embedded));
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
        let nav_keys =
            keymap::NavKeys::from_config(&config.input.page_keys, &config.input.highlight_keys);

        // 短语层：从 data 目录加载 system.phrases.toml
        let phrases = match data_dir {
            Some(d) => {
                let p = d.join("system.phrases.toml");
                let layer = wind_phrase::PhraseLayer::load(&p);
                if !layer.is_empty() {
                    info!("Loaded phrases from {}", p.display());
                }
                layer
            }
            None => wind_phrase::PhraseLayer::default(),
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

        // 标点转换器：注入自定义映射（四状态）。
        let mut punct_conv = PunctuationConverter::new();
        punct_conv.set_custom_mappings(
            config.input.punct_custom.enabled,
            config.input.punct_custom.mappings.clone(),
        );

        // preedit 嵌入模式运行时初值（与 new() 下发 SetPreeditEmbedded 的判定一致）。
        // 在 config 被移入结构体前先算出（结构体字段顺序会先移走 config）。
        let preedit_embedded_init = !config.ui.candidate.inline_preedit
            && config
                .ui
                .candidate
                .preedit_mode
                .eq_ignore_ascii_case("embedded");

        // 候选布局方向运行时初值（与下方 SetCandidateLayout 下发一致；config 移入前先算）。
        let candidate_vertical_init = config
            .ui
            .candidate
            .layout
            .eq_ignore_ascii_case("vertical");

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
                committed_text: String::new(),
                committed_segs: Vec::new(),
                active: None,
                temp_pinyin_buffer: String::new(),
                temp_pinyin_schema: String::new(),
                temp_pinyin_prefix: String::new(),
                quick_input_buffer: String::new(),
                quick_input_prefix: String::new(),
                temp_english_buffer: String::new(),
                url_buffer: String::new(),
                rewind: None,
                special_buffer: String::new(),
                special_id: 0,
                mix_buffer: String::new(),
                mix_id: 0,
                mix_numeric: false,
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
            nav_keys,
            punct: Mutex::new(punct_conv),
            smart_symbol: Mutex::new(SmartSymbolArm::default()),
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
            cmdbar_services: std::sync::OnceLock::new(),
            self_weak: std::sync::OnceLock::new(),
            recent_commits: Mutex::new(std::collections::VecDeque::new()),
            preedit_embedded: Mutex::new(preedit_embedded_init),
            hide_candidate_window: Mutex::new(false),
            candidate_vertical: Mutex::new(candidate_vertical_init),
        });
        // 命令栏：装配 Services（ime/config/dict 后端）+ 自身 Weak 引用。
        coordinator.init_cmdbar();
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
        // 上屏历史（命令栏 last(n) 用）：最近置前，限 16 条。
        {
            let mut h = self
                .recent_commits
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            h.push_front(text.to_string());
            if h.len() > 16 {
                h.truncate(16);
            }
        }
        if let Some(store) = &self.store {
            let schema = self.engine_mgr.active_schema_id();
            if let Err(e) = store.record_freq(&schema, code, text) {
                warn!("record_freq failed: {}", e);
            }
        }
    }

    /// 上屏历史快照（index 0 = 最近）。供命令栏 `last(n)` 读取。
    pub(crate) fn recent_commits_snapshot(&self) -> Vec<String> {
        self.recent_commits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 词频重排（独立维度，**绝不改 weight**）：按 redb 词频记录做档位感知的 used-first 稳定
    /// 重排——用过的候选（count>0）按策略上浮，未用候选保持基础(权重)序。对齐 frequency.md §3。
    ///
    /// 策略（engine.codetable.freq_strategy）：
    /// - `step`（默认/逐次提升）：count 降序、last_used 降序 tiebreak（累积使用才爬升，抗误选）。
    /// - `top`（一次到顶/MRU）：last_used 降序、count 降序 tiebreak（最近选的置该档之首）。
    ///
    /// 主开关 `learning.freq.enabled` 关闭则完全不重排（修"配置说关、代码却排"的潜在 bug）。
    /// 引擎类型分流：码表/混输走永久 used-first（§3），纯拼音走衰减软置前（§4）。
    /// 注：每候选一次 redb 点查（mmap 微秒级）；后续可下沉到引擎排序层。
    fn apply_freq_rerank(&self, candidates: &mut [Candidate], code: &str) {
        let Some(store) = &self.store else {
            return;
        };
        if code.is_empty() || candidates.len() < 2 {
            return;
        }
        let settings = self.engine_mgr.freq_settings();
        if !settings.enabled {
            return;
        }
        let schema = self.engine_mgr.active_schema_id();
        let input_len = code.len();
        // 取每个"消费整串"候选的词频记录。分段子候选（consumed_length < 整串，如「nihao」里的「你」
        // 只消费「ni」）的词频归属其自身前缀码，不能被整串码的历史计数上浮——否则单字会浮到整句
        // 「你好」之上。consumed_length==0 表示引擎未标注（码表型），视为整串匹配。
        let recs: std::collections::HashMap<String, FreqRecord> = candidates
            .iter()
            .filter_map(|c| {
                let consumes_all = c.consumed_length == 0 || c.consumed_length >= input_len;
                if !consumes_all {
                    return None;
                }
                match store.get_freq(&schema, code, &c.text) {
                    Ok(Some(r)) if r.count > 0 => Some((c.text.clone(), r)),
                    _ => None,
                }
            })
            .collect();
        if recs.is_empty() {
            return;
        }
        // 词频重排归属 engine 排序层（frequency.md §5/§7）：本协调器只负责取词频记录、按引擎
        // 类型分流到纯函数。码表/混输永久 used-first（§3），纯拼音衰减软置前（§4）。
        if self.engine_mgr.is_pinyin() {
            wind_engine::freq_rerank::rerank_pinyin_decay(candidates, &recs, now_unix_secs());
        } else {
            wind_engine::freq_rerank::rerank_codetable_usedfirst(
                candidates,
                &recs,
                code,
                settings.strategy,
            );
        }
    }

    /// 当前活跃方案 ID（测试/诊断用）
    pub fn active_schema_id(&self) -> String {
        self.engine_mgr.active_schema_id()
    }

    /// 当前是否中文模式（测试/诊断用）
    pub fn is_chinese_mode(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chinese_mode
    }

    /// 设置简繁开关（测试/诊断用）。返回是否生效（数据缺失则 false）。
    pub fn debug_set_s2t(&self, on: bool) -> bool {
        if self.s2t.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return false;
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .s2t_enabled = on;
        true
    }

    /// 候选总数（测试/诊断用）
    pub fn debug_candidate_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .candidates
            .len()
    }

    /// 是否还有更多候选未加载（测试/诊断用）
    pub fn debug_has_more(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_more
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
        s.candidates[start..end]
            .iter()
            .map(|c| c.text.clone())
            .collect()
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
    /// 返回 (引擎候选数, 输入结局)。结局含全码自动上屏 / 满码空码清空；自动上屏文本经
    /// shadow 复核后才放行，避免上屏被置顶删词移除的候选。调用方仅在「正向输入字母」时消费。
    fn build_candidates(&self, state: &mut State, limit: usize) -> (usize, InputOutcome) {
        // 分段上屏进行中（committed 前缀非空 ⟺ 来自拼音选词——五笔候选 consumed_length=0
        // 永不部分匹配）：剩余编码强制按拼音方案转换，避免混输让五笔抢首选（你↑选后 hao→虚）。
        let result = if !state.committed_text.is_empty()
            && !self.config.schema.primary_pinyin.is_empty()
        {
            self.engine_mgr.convert_with(
                &self.config.schema.primary_pinyin,
                &state.input_buffer,
                limit,
            )
        } else {
            self.engine_mgr.convert(&state.input_buffer, limit)
        };
        // 组合区只显示输入码/拼音
        state.preedit = if result.preedit_display.is_empty() {
            state.input_buffer.clone()
        } else {
            result.preedit_display
        };
        let engine_count = result.candidates.len();
        // 引擎给出的全码自动上屏意向（基于引擎候选；下方 shadow 后复核存活性）。
        let auto_commit = if result.should_commit && !result.commit_text.is_empty() {
            Some(result.commit_text.clone())
        } else {
            None
        };
        let should_clear = result.should_clear;

        let mut candidates = result.candidates;
        if !self.phrases.is_empty() {
            let recent = self.recent_commits_snapshot();
            let max_disp = self.config.input.phrase.max_display_chars;
            // 剪贴板读取回调注入 wind-phrase（其不依赖平台 UI 层）：精确码命令 display
            // 含 {clip()}（如 coad）时按需读取；非 windows 返回空。
            let clip = |_n: i64| -> String {
                #[cfg(windows)]
                {
                    wind_ui::popup_menu::get_clipboard_text()
                }
                #[cfg(not(windows))]
                {
                    String::new()
                }
            };
            for hit in self.phrases.lookup(&state.input_buffer, &recent, &clip) {
                let is_command = hit.command_src.is_some();
                candidates.push(Candidate {
                    text: Self::clamp_candidate_display(&hit.text, max_disp),
                    weight: PHRASE_WEIGHT_BASE + hit.weight,
                    is_phrase: true,
                    // $CC 命令短语：标记 is_command，phrase_template 暂存命令源；
                    // 选中时由 commit_selected 拦截，执行动作而非上屏 display 标签。
                    is_command,
                    phrase_template: hit.command_src.unwrap_or_default(),
                    ..Default::default()
                });
            }
            // 前缀导航：敲 `zz`/`co` 等前缀（长度 ≥ min_prefix_length）列出所有该前缀的
            // marker 短语。**$CC 命令** → is_command（选中直接执行，group_code 作执行输入
            // 上下文）；**$SS/$AA 组** → is_group（选中补全到完整码再展开成员，二级选择）。
            let min_prefix = self.config.input.phrase.min_prefix_length;
            for hit in self
                .phrases
                .lookup_prefix(&state.input_buffer, &recent, min_prefix)
            {
                let code = hit.nav_code.unwrap_or_default();
                let text = Self::clamp_candidate_display(&hit.text, max_disp);
                if let Some(src) = hit.command_src {
                    candidates.push(Candidate {
                        text,
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_command: true,
                        phrase_template: src,
                        group_code: code,
                        comment: hit.comment,
                        ..Default::default()
                    });
                } else {
                    candidates.push(Candidate {
                        text: text.clone(),
                        weight: PHRASE_WEIGHT_BASE + hit.weight,
                        is_phrase: true,
                        is_group: true,
                        group_code: code,
                        group_name: text,
                        comment: hit.comment,
                        ..Default::default()
                    });
                }
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
        // 复核：仅当上屏目标在最终候选中仍存在（未被 shadow 删除）才放行自动上屏。
        let outcome = match auto_commit.filter(|t| state.candidates.iter().any(|c| &c.text == t)) {
            Some(_) => {
                // 一致性：自动上屏文本取「实际显示的首候选」，与空格/点选同源，杜绝
                // "显示藏、全码上屏駏"的漂移（首候选已由档位排序保证是五笔精确全码）。
                match state.candidates.first() {
                    Some(c) => InputOutcome::AutoCommit(c.text.clone()),
                    None => InputOutcome::Normal,
                }
            }
            None if should_clear => InputOutcome::Clear,
            None => InputOutcome::Normal,
        };
        (engine_count, outcome)
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
    /// 返回输入结局（全码自动上屏 / 满码空码清空）；多数调用方忽略，仅正向输入字母时消费。
    fn update_candidates(&self, state: &mut State) -> InputOutcome {
        state.candidates.clear();
        state.preedit = state.input_buffer.clone();
        if state.input_buffer.is_empty() {
            state.has_more = false;
            state.candidate_input.clear();
            // 缓冲空但有已转换前缀（逐步转换中删空剩余拼音）：组合区仍显示前缀。
            state.preedit = state.committed_text.clone();
            return InputOutcome::Normal;
        }
        let limit = self.initial_candidate_limit(&state.input_buffer);
        let (engine_count, outcome) = self.build_candidates(state, limit);
        // 拼音逐步转换：组合区 = 已转换前缀 + 剩余拼音显示（前缀恒空于码表模式，无副作用）。
        if !state.committed_text.is_empty() {
            state.preedit = format!("{}{}", state.committed_text, state.preedit);
        }
        state.candidate_input = state.input_buffer.clone();
        state.candidate_limit = limit;
        // 引擎返回数达到上限 → 可能还有更多未加载
        state.has_more = engine_count >= limit;
        // 候选变化：复位翻页与高亮（含清除鼠标悬停）
        state.current_page = 0;
        state.selected_index = 0;
        state.hover_index = -1;
        outcome
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
        // 翻页扩展不消费全码自动上屏（仅正向输入字母时才上屏）。
        let (engine_count, _) = self.build_candidates(state, new_limit);
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

    /// 候选导航键的统一执行（配置驱动，见 `keymap::NavKeys`）：高亮上下 + 翻页。
    /// 普通模式与所有候选模式共用；`include_printable` 区分码表型（`-`/`=` 作翻页）与
    /// 文本/表达式型（临英/快捷输入，`-`/`=` 作输入，不当导航）。命中返回 Some。
    fn apply_nav_key(
        &self,
        state: &mut State,
        data: &KeyEventData,
        include_printable: bool,
    ) -> Option<KeyAction> {
        if state.candidates.is_empty() {
            return None;
        }
        let shift = data.modifiers & MOD_SHIFT != 0;
        let action = self
            .nav_keys
            .classify(data.key_code, shift, include_printable)?;
        let changed = match action {
            keymap::NavAction::HighlightUp => self.move_up(state),
            keymap::NavAction::HighlightDown => self.move_down(state),
            keymap::NavAction::PagePrev => self.page_prev(state),
            keymap::NavAction::PageNext => self.page_next(state),
        };
        if changed {
            self.notify_ui_update(state);
        }
        Some(KeyAction::Consumed)
    }

    /// overlay 候选模式的导航分派：码表型（特殊/临拼，及不含 quick_input 的 mix）`-`/`=` 作翻页；
    /// 文本型（临英）、表达式型（快捷输入）、含 quick_input 的 mix（`-`/`=` 是运算符输入）不把
    /// `-`/`=` 当导航。由 active 自判。
    fn handle_candidate_nav(&self, state: &mut State, data: &KeyEventData) -> Option<KeyAction> {
        let include_printable = match state.active {
            Some(ModeKind::Special(_)) | Some(ModeKind::TempPinyin) => true,
            Some(ModeKind::Mix(idx)) => !self.mix_has_quick_input(idx),
            _ => false,
        };
        self.apply_nav_key(state, data, include_printable)
    }

    // ───────────────────────── 临时拼音 ─────────────────────────

    /// 触发键名 → VK（统一映射，见 `keymap`；不含 z，z 混合模式后置实现）
    fn temp_pinyin_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk(key)
    }

    /// VK → 组合区前缀字符（统一映射，见 `keymap`；缺省回退反引号）
    fn temp_pinyin_prefix_for(key_code: u32) -> char {
        keymap::vk_to_prefix_char(key_code).unwrap_or('`')
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

    /// 退出临时拼音模式并清空相关状态（含逐步转换的已转换前缀）
    fn exit_temp_pinyin(&self, state: &mut State) {
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_schema.clear();
        state.temp_pinyin_prefix.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 用临时拼音目标方案转换缓冲，刷新候选与组合区（前缀 + 已转换汉字 + 剩余拼音）
    fn update_temp_pinyin_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let prefix = format!("{}{}", state.temp_pinyin_prefix, state.committed_text);
        if state.temp_pinyin_buffer.is_empty() {
            state.preedit = prefix;
            return;
        }
        let Some(schema) = self.overlay_engine_schema(state) else {
            state.preedit = format!("{}{}", prefix, state.temp_pinyin_buffer);
            return;
        };
        let result =
            self.engine_mgr
                .convert_with(&schema, &state.temp_pinyin_buffer, ENGINE_MAX_CANDIDATES);
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

    /// 临时拼音选词 —— 组合区逐步转换（C）。部分匹配并入 committed 前缀留模式内（不上屏）；
    /// 完整匹配整体上屏 committed+候选（前缀触发键不输出）+ 造词，退出。返回最终 KeyAction。
    fn commit_temp_pinyin_selected(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        let total = state.temp_pinyin_buffer.len();
        let consumed = cand.consumed_length;
        let code = Self::cand_code(&state.temp_pinyin_buffer, cand);
        let partial =
            consumed > 0 && consumed < total && state.temp_pinyin_buffer.is_char_boundary(consumed);
        self.record_selection(&code, &cand.text);
        if partial {
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.temp_pinyin_buffer = state.temp_pinyin_buffer[consumed..].to_string();
            self.update_temp_pinyin_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            state.committed_segs.push((code, cand.text.clone()));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state);
            let out = self.maybe_s2t(state, &final_simplified);
            self.exit_temp_pinyin(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 临时拼音模式下的按键处理
    fn handle_temp_pinyin_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：退出
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // Backspace：分步撤销——有已转换段先退回最后一段（你→ni，码并回缓冲前部）；
                // 否则删剩余拼音末字符；皆空则退出。
                if let Some((code, _)) = state.committed_segs.pop() {
                    state.committed_text =
                        state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
                    state.temp_pinyin_buffer = format!("{}{}", code, state.temp_pinyin_buffer);
                    self.update_temp_pinyin_candidates(state);
                    let display = state.preedit.clone();
                    self.notify_ui_update(state);
                    return KeyAction::UpdateComposition {
                        caret_pos: display.chars().count() as u32,
                        text: display,
                    };
                }
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
                    caret_pos: display.chars().count() as u32,
                    text: display,
                }
            }
            keymap::VK_SPACE => {
                // 空格：选当前高亮候选（逐步转换）
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
                } else {
                    self.exit_temp_pinyin(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏「当前显示」= 已转换前缀 + 剩余拼音原码（已选中文照样上屏），退出。
                let out = format!("{}{}", state.committed_text, state.temp_pinyin_buffer);
                let out = self.maybe_s2t(state, &out);
                self.exit_temp_pinyin(state);
                self.notify_ui_hide();
                if out.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(out, true)
                }
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字键选当前页第 N 个
                let (start, end) = self.page_range(state);
                let idx = start + (data.key_code - 0x31) as usize;
                if idx < end {
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_A..=keymap::VK_Z if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
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
            _ => {
                // 二三候选键
                if data.modifiers & MOD_SHIFT == 0 {
                    if let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(state);
                        let idx = start + offset;
                        if idx < end {
                            let cand = state.candidates[idx].clone();
                            return self.commit_temp_pinyin_selected(state, &cand);
                        }
                    }
                }
                // 其它键：有候选则上屏高亮候选（分段则保留剩余拼音）；否则退出清空。
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_temp_pinyin_selected(state, &cand)
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
        keymap::key_name_to_vk(key)
    }

    /// VK → 组合区前缀字符（统一映射，见 `keymap`；缺省回退分号）
    fn quick_input_prefix_for(key_code: u32) -> char {
        keymap::vk_to_prefix_char(key_code).unwrap_or(';')
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
        // prev_char 为 UTF-16 单元（非 VK），数字 '0'..='9' = 0x30..=0x39
        (0x30..=0x39).contains(&prev_char)
    }

    /// 按当前中英标点/全半角配置转换一个标点字符为上屏文本（无 prev_char 上下文）。
    /// 用于独占模式（快捷输入/临时英文）等不涉及数字后智能的场景。
    fn convert_punct_char(&self, state: &State, ch: char) -> String {
        self.convert_punct(state, ch, 0)
    }

    /// 标点转换单点流水线（对齐 Go `convertPunct`，固定优先级）：
    ///   1. 自定义映射（四状态：中半 0 / 英全 1 / 中全 2 / 英半 3，按当前中英标点+全半角选列）
    ///   2. 数字后智能转换（命中则该标点按英文输出，不转中文）
    ///   3. 中文标点转换（引号左右交替状态机）
    ///   4. 全半角转换
    /// `prev_char` 为光标前一字符的 UTF-16 单元（0=不可用），用于数字后智能判定。
    fn convert_punct(&self, state: &State, ch: char, prev_char: u16) -> String {
        let effective_ch_punct = state.chinese_punct;
        let smart_en = effective_ch_punct && self.is_smart_punct_after_digit(ch, prev_char);
        let is_chinese_punct = effective_ch_punct && !smart_en;

        // 1. 自定义映射优先（四状态均可配置）。
        if self.config.input.punct_custom.enabled {
            let col_idx = if is_chinese_punct && state.full_width {
                2 // 中文全角
            } else if is_chinese_punct {
                0 // 中文半角
            } else if state.full_width {
                1 // 英文全角
            } else {
                3 // 英文半角
            };
            if let Some(text) = self
                .punct
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .lookup_custom(ch, col_idx)
            {
                return text;
            }
        }

        // 2~4. 默认转换：中文标点（含引号状态机）→ 全半角。
        let mut piece = ch.to_string();
        if is_chinese_punct {
            if let Some(c) = self
                .punct
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .to_chinese(ch)
            {
                piece = c;
            }
        }
        if state.full_width {
            piece = to_full_width(&piece);
        }
        piece
    }

    /// 智能符号模式判定时限（非法值回退 500ms）。
    fn smart_symbol_timeout(&self) -> std::time::Duration {
        let ms = self.config.input.smart_symbol_timeout_ms;
        let ms = if ms <= 0 { 500 } else { ms };
        std::time::Duration::from_millis(ms as u64)
    }

    /// 纯查表读自定义标点映射的指定列（不碰转换器引号状态），供智能符号无副作用计算用。
    /// 与 `PunctuationConverter::lookup_custom` 的非引号分支等价。
    fn smart_symbol_custom_lookup(&self, ch: char, col_idx: usize) -> Option<String> {
        let vals = self
            .config
            .input
            .punct_custom
            .mappings
            .get(&ch.to_string())?;
        let v = vals.get(col_idx)?;
        if v.is_empty() { None } else { Some(v.clone()) }
    }

    /// 无副作用地计算 `ch` 在当前模式下的标点产物，**镜像** `convert_punct` 优先级
    /// （自定义列 > 中/英转换 > 全半角）。对齐 Go `computePunctStrPure`。
    ///   - `chinese=true`：算中文标点产物（武装/匹配用，引号经 peek 预测不改状态）。
    ///   - `chinese=false`：算英文标点产物（替换用，即该键英文模式下输出）。
    /// 引号有状态、键名特殊，此处保守跳过自定义、走标准引号/英文产物。
    fn compute_punct_str_pure(&self, state: &State, ch: char, chinese: bool) -> Option<String> {
        let full_width = state.full_width;
        let is_quote = ch == '\'' || ch == '"';

        if !is_quote && self.config.input.punct_custom.enabled {
            let col_idx = if chinese && full_width {
                Some(2) // 中文全角
            } else if chinese {
                Some(0) // 中文半角
            } else if full_width {
                Some(1) // 英文全角
            } else {
                None // 英文半角：无自定义（col 3 由 convert_punct 用，pure 计算走原样）
            };
            if let Some(ci) = col_idx {
                if let Some(v) = self.smart_symbol_custom_lookup(ch, ci) {
                    return Some(v);
                }
            }
        }

        let mut s = ch.to_string();
        if chinese {
            s = self
                .punct
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .peek_chinese_str(ch)?;
        }
        if full_width {
            s = to_full_width(&s);
        }
        Some(s)
    }

    /// 判断中文标点串 `cn` 是否在用户配置的参与集合内（子串包含匹配，支持多字符/引号）。
    fn smart_symbol_participates(&self, cn: &str) -> bool {
        !cn.is_empty() && self.config.input.smart_symbol_chars.contains(cn)
    }

    /// 计算 `ch` 当前会产生的「参与集合内的中文标点串」用于武装；不参与返回 None。
    /// 对齐 Go `smartSymbolArmStr`：仅中文标点模式 + 非数字后智能 + 在参与集合内。
    fn smart_symbol_arm_str(&self, state: &State, ch: char, prev_char: u16) -> Option<String> {
        if !state.chinese_punct {
            return None;
        }
        if self.is_smart_punct_after_digit(ch, prev_char) {
            return None;
        }
        let cn = self.compute_punct_str_pure(state, ch, true)?;
        if !self.smart_symbol_participates(&cn) {
            return None;
        }
        // 与自动配对互斥：被配对的符号（单字符且在配对表）不武装智能符号。否则 press1 插入配对
        // 并回退光标至中间，press2 时 prevChar 恰为配对左符号 → 误删左符号改英文、留下中文右符号。
        if cn.chars().count() == 1 {
            let c0 = cn.chars().next().unwrap();
            if self.is_auto_pair_char(state, c0) {
                return None;
            }
        }
        Some(cn)
    }

    /// 智能符号替换判定（在标点分支入口调用）。对齐 Go `trySmartSymbolReplace`：
    ///   - 返回 Some：本次为 press2 触发，调用方应直接返回该替换响应（短路）。
    ///   - 返回 None：未触发；已按需更新武装态，调用方继续普通标点流程。
    fn try_smart_symbol_replace(
        &self,
        state: &State,
        ch: char,
        prev_char: u16,
    ) -> Option<KeyAction> {
        if !self.config.input.smart_symbol_mode {
            return None;
        }
        let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());

        // press2 触发：仍在中文标点模式 + 已武装 + 同键 + 时限内 + 光标前字符为武装串末位 rune。
        // 匹配的是 press1 的产物，故引号（“→”）也能命中。
        if arm.armed
            && ch == arm.key
            && state.chinese_punct
            && arm
                .at
                .map(|t| t.elapsed() < self.smart_symbol_timeout())
                .unwrap_or(false)
        {
            let armed_runes: Vec<char> = arm.str.chars().collect();
            if let Some(&last) = armed_runes.last() {
                if last as u32 == prev_char as u32 {
                    if let Some(rep) = self.compute_punct_str_pure(state, ch, false) {
                        arm.armed = false;
                        // 吃掉一个引号后回退引号交替状态，使下次同引号仍从左引号开始。
                        if ch == '\'' || ch == '"' {
                            self.punct
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .revert_last_quote(ch);
                        }
                        debug!(
                            "SmartSymbol: replace prev chinese punct with english (count={})",
                            armed_runes.len()
                        );
                        return Some(KeyAction::ReplaceBackward {
                            count: armed_runes.len() as u32,
                            text: rep,
                        });
                    }
                }
            }
        }

        // 未触发：尝试以本次按键的中文产物武装，等待下次同键快速重复。
        match self.smart_symbol_arm_str(state, ch, prev_char) {
            Some(cn) => {
                arm.armed = true;
                arm.key = ch;
                arm.str = cn;
                arm.at = Some(std::time::Instant::now());
            }
            None => arm.armed = false,
        }
        None
    }

    /// 解除智能符号待命态（焦点变化/模式切换等的防御性复位）。
    fn disarm_smart_symbol(&self) {
        self.smart_symbol
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .armed = false;
    }

    /// 退出快捷输入模式并清空状态
    fn exit_quick_input(&self, state: &mut State) {
        state.active = None;
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
        // 表达式模式：`-`/`=` 是运算符输入，不当翻页（include_printable=false）。
        if let Some(act) = self.apply_nav_key(state, data, false) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_quick_input(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
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
            keymap::VK_SPACE => {
                // 空格：上屏当前高亮候选；无候选则退出
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
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
            keymap::VK_RETURN => {
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
            keymap::VK_A..=keymap::VK_Z if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 => {
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
                if state.quick_input_buffer.is_empty() && self.is_quick_input_trigger(data.key_code)
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
        state.active = None;
        state.temp_english_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
    }

    /// 刷新临时英文候选：首候选=用户原始输入，其后为英文词库前缀匹配（大小写适配）。
    /// 需 `shift_temp_english.show_english_candidates` 开启才查词库；词库为固定 id "english" 方案。
    fn update_temp_english_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        let buf = state.temp_english_buffer.clone();
        state.preedit = buf.clone();
        if buf.is_empty() {
            return;
        }
        // 首候选始终是用户所打原文（保证能上屏自己输入的内容）。
        let mut cands = vec![Candidate {
            text: buf.clone(),
            natural_order: 0,
            ..Default::default()
        }];
        if let Some(schema) = self.overlay_engine_schema(state) {
            let lower = buf.to_lowercase();
            let case = detect_en_case(&buf);
            let result = self.engine_mgr.convert_with(&schema, &lower, 60);
            let mut seen = std::collections::HashSet::new();
            seen.insert(lower);
            for (i, c) in result.candidates.into_iter().enumerate() {
                let cl = c.text.to_lowercase();
                if !seen.insert(cl.clone()) {
                    continue;
                }
                // 词库全小写词按输入大小写适配；专有词（iPhone/Aaron）保持原样。
                let display = if case != EnCase::Lower && c.text == cl {
                    adapt_en_case(&c.text, case)
                } else {
                    c.text
                };
                cands.push(Candidate {
                    text: display,
                    natural_order: (i + 1) as i32,
                    ..Default::default()
                });
            }
        }
        state.candidates = cands;
    }

    /// 探针是否恰好等于某个网址前缀（精确匹配，对齐 Go urlActivationResidual 的全匹配语义）。
    fn is_url_prefix(&self, probe: &str) -> bool {
        self.config
            .input
            .url_input
            .prefixes
            .iter()
            .any(|p| !p.is_empty() && p == probe)
    }

    /// 进入网址模式：以补全前缀的完整文本作初始缓冲，清空普通输入/候选，隐藏候选窗。
    /// 网址模式无候选，仅在组合区原样显示累积文本。
    /// 同时登记夺取回退：snapshot=夺取前的正常输入（=前缀去掉补全键），host_text=完整前缀。
    fn enter_url_mode(&self, state: &mut State, buffer: String) -> KeyAction {
        // 夺取前的正常 input_buffer 即回退快照（前缀的最后一字符是刚补全的那一键）。
        let snapshot = state.input_buffer.clone();
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Url);
        state.url_buffer = buffer.clone();
        state.rewind = Some(Rewind {
            snapshot,
            host_text: buffer,
        });
        self.notify_ui_hide();
        let disp = state.url_buffer.clone();
        debug!("Entered URL mode (buffer={})", disp);
        KeyAction::UpdateComposition {
            text: disp.clone(),
            caret_pos: disp.chars().count() as u32,
        }
    }

    /// 退出网址模式并清空相关状态（含作废回退登记）。
    fn exit_url_mode(&self, state: &mut State) {
        state.active = None;
        state.url_buffer.clear();
        state.preedit.clear();
        state.rewind = None;
    }

    /// 当前夺取式模式的 buffer（用于回退边界判定）。非夺取式模式返回 None。
    fn active_hijack_buffer<'a>(&self, state: &'a State) -> Option<&'a str> {
        match state.active {
            Some(ModeKind::Url) => Some(&state.url_buffer),
            // z 临拼夺取（后续 S3/S4 接入）：Some(&state.temp_pinyin_buffer)
            _ => None,
        }
    }

    /// 是否可回退：已登记 + 当前模式 buffer 已退回到夺取边界（== 登记时的 host_text）。
    fn can_rewind(&self, state: &State) -> bool {
        match (&state.rewind, self.active_hijack_buffer(state)) {
            (Some(rw), Some(buf)) => buf == rw.host_text,
            _ => false,
        }
    }

    /// 执行夺取回退：撤销夺取，把快照回放到正常码表输入流并重算候选。
    fn rewind_hijack(&self, state: &mut State) -> KeyAction {
        let snapshot = state.rewind.take().map(|r| r.snapshot).unwrap_or_default();
        // 退出当前夺取式模式（目前仅 URL；z 临拼接入后在此扩展 match）。
        match state.active {
            Some(ModeKind::Url) => self.exit_url_mode(state),
            _ => self.reset_exclusive_modes(state),
        }
        state.input_buffer = snapshot;
        self.update_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!(
            "rewind_hijack: restored normal input '{}'",
            state.input_buffer
        );
        KeyAction::UpdateComposition {
            text: display,
            caret_pos: state.input_buffer.chars().count() as u32,
        }
    }

    /// 网址模式按键处理：可见 ASCII 原样累积；空格/回车上屏原文；退格删空退出；Esc 放弃。
    fn handle_url_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let comp = |buf: &str| KeyAction::UpdateComposition {
            text: buf.to_string(),
            caret_pos: buf.chars().count() as u32,
        };
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：放弃退出（无上屏）
                self.exit_url_mode(state);
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 退格：删尾字符；删空则退出
                state.url_buffer.pop();
                if state.url_buffer.is_empty() {
                    self.exit_url_mode(state);
                    KeyAction::ClearComposition
                } else {
                    comp(&state.url_buffer)
                }
            }
            keymap::VK_SPACE | keymap::VK_RETURN => {
                // 空格/回车：上屏当前缓冲原文（不做全半角/标点转换）
                let text = state.url_buffer.clone();
                self.exit_url_mode(state);
                if text.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(text, true)
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = printable_char(data.key_code, shift) {
                    state.url_buffer.push(ch);
                    comp(&state.url_buffer)
                } else {
                    // 方向键等非可打印键：消费但不改缓冲（首版不支持光标内编辑）
                    KeyAction::Consumed
                }
            }
        }
    }

    // ───────────────────────── 特殊模式 ─────────────────────────

    /// 引导键名 → VK（特殊模式触发；统一映射 + 额外支持单字母 a-z 引导键，见 `keymap`）。
    fn special_trigger_vk(key: &str) -> Option<u32> {
        keymap::key_name_to_vk_with_letters(key)
    }

    /// 找出 key_code 匹配的特殊模式下标（按配置顺序先到先得；最多 256 个）。
    fn match_special_trigger(&self, key_code: u32) -> Option<u8> {
        for (i, m) in self.config.features.special_modes.iter().enumerate() {
            if i > u8::MAX as usize {
                break;
            }
            if m.trigger_keys
                .iter()
                .filter_map(|k| Self::special_trigger_vk(k))
                .any(|vk| vk == key_code)
            {
                return Some(i as u8);
            }
        }
        None
    }

    /// 特殊模式引用的方案 id（features.special_modes[idx].schema）。
    fn special_schema(&self, idx: u8) -> Option<String> {
        self.config
            .features
            .special_modes
            .get(idx as usize)
            .map(|m| m.schema.clone())
            .filter(|s| !s.is_empty())
    }

    /// 当前 overlay 模式背后的方案 id —— "模式即方案" 的单一映射（M4）。
    /// 引擎驱动型模式（临拼/特殊/临英）返回 Some(scheme)；无词典模式（快捷/URL）返回 None。
    /// overlay 候选查询统一经此取方案再走 `convert_with`；M5 临时 mix 复用此映射枚举成员方案。
    ///
    /// 说明：激活「触发条件」因各模式高度异构（Shift+字母 / 无修饰触发键 / schema 查找 /
    /// 缓冲扩展夺取）保持 S4d `try_activate_mode` 的显式优先级链，不强塞统一表（避免死抽象）。
    fn overlay_engine_schema(&self, state: &State) -> Option<String> {
        match state.active {
            Some(ModeKind::TempPinyin) => {
                (!state.temp_pinyin_schema.is_empty()).then(|| state.temp_pinyin_schema.clone())
            }
            Some(ModeKind::Special(idx)) => self.special_schema(idx),
            Some(ModeKind::TempEnglish) => self
                .config
                .input
                .shift_temp_english
                .show_english_candidates
                .then(|| "english".to_string()),
            _ => None,
        }
    }

    /// 进入特殊模式（其方案须可加载，由激活点 ensure_schema 保证）。清空普通输入，初始化空编码缓冲。
    fn enter_special_mode(&self, state: &mut State, idx: u8) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Special(idx));
        state.special_id = idx;
        state.special_buffer.clear();
        self.update_special_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered special mode idx={}", idx);
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 退出特殊模式并清空相关状态（码表缓存保留供复用）。
    fn exit_special_mode(&self, state: &mut State) {
        state.active = None;
        state.special_buffer.clear();
        state.candidates.clear();
        state.preedit.clear();
    }

    /// 按当前编码缓冲刷新特殊模式候选（经其引用方案的引擎查询，复用方案 CodeTableSpec 全码策略）。
    /// 返回 Some(text) 表示该方案的全码策略请求自动上屏。
    fn update_special_candidates(&self, state: &mut State) -> Option<String> {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        state.preedit = state.special_buffer.clone();
        if state.special_buffer.is_empty() {
            return None;
        }
        let schema = self.overlay_engine_schema(state)?;
        let result = self
            .engine_mgr
            .convert_with(&schema, &state.special_buffer, 100);
        state.candidates = result.candidates;
        // 自动上屏由方案码表引擎的 should_auto_commit 决定（prefix_free≈全码唯一、fixed_length 等
        // 映射到该方案的 [engine.codetable] 配置）；复核上屏目标仍在候选中。
        if result.should_commit
            && !result.commit_text.is_empty()
            && state
                .candidates
                .iter()
                .any(|c| c.text == result.commit_text)
        {
            return Some(result.commit_text);
        }
        None
    }

    /// 特殊模式按键处理：编码累积 + 候选选择 + 三档自动上屏；空格选高亮、回车上屏编码原文。
    fn handle_special_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                // Esc：放弃退出
                self.exit_special_mode(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 退格：删编码；空则退出。删除时不触发自动上屏。
                state.special_buffer.pop();
                if state.special_buffer.is_empty() {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    self.update_special_candidates(state);
                    let display = state.preedit.clone();
                    self.notify_ui_update(state);
                    KeyAction::UpdateComposition {
                        text: display.clone(),
                        caret_pos: display.chars().count() as u32,
                    }
                }
            }
            keymap::VK_SPACE => {
                // 空格：有候选选高亮上屏；无候选退出
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    let text = state.candidates[idx].text.clone();
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏编码原文
                let text = state.special_buffer.clone();
                self.exit_special_mode(state);
                self.notify_ui_hide();
                if text.is_empty() {
                    KeyAction::ClearComposition
                } else {
                    Self::commit_action(text, true)
                }
            }
            keymap::VK_1..=keymap::VK_9 => {
                // 数字 1-9 选当前页候选
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if gi < end {
                    let text = state.candidates[gi].text.clone();
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_A..=keymap::VK_Z => {
                // 字母：小写归一累积编码
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.special_buffer.push(ch);
                if let Some(text) = self.update_special_candidates(state) {
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    return Self::commit_action(text, true);
                }
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 二三候选键 → 选候选
                if !shift {
                    if let Some(offset) = self.select_key_offset(data.key_code) {
                        let (start, end) = self.page_range(state);
                        let gi = start + offset;
                        if gi < end {
                            let text = state.candidates[gi].text.clone();
                            self.exit_special_mode(state);
                            self.notify_ui_hide();
                            return Self::commit_action(text, true);
                        }
                    }
                }
                // 其它可打印标点：顶屏当前高亮候选 + 转换后标点，退出
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let committed = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        state.candidates[idx].text.clone()
                    } else {
                        String::new()
                    };
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_special_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", committed, punct), true)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }

    // ───────────────────────── 临时 mix 模式 ─────────────────────────

    /// 找出 key_code 匹配的 mix 模式下标（按配置顺序先到先得）。
    fn match_mix_trigger(&self, key_code: u32) -> Option<u8> {
        for (i, m) in self.config.features.mix_modes.iter().enumerate() {
            if i > u8::MAX as usize {
                break;
            }
            if m.trigger_keys
                .iter()
                .filter_map(|k| Self::special_trigger_vk(k))
                .any(|vk| vk == key_code)
            {
                return Some(i as u8);
            }
        }
        None
    }

    /// mix 模式可加载的成员方案列表（过滤空/不可加载）。
    /// mix 可用的真实方案成员（过滤空 / 不可加载 / 内置 quick_input）。
    fn mix_members(&self, idx: u8) -> Vec<String> {
        self.config
            .features
            .mix_modes
            .get(idx as usize)
            .map(|m| {
                m.members
                    .iter()
                    .filter(|s| {
                        !s.is_empty() && *s != "quick_input" && self.engine_mgr.ensure_schema(s)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// mix 是否含内置类方案 quick_input（日期/计算）成员——启用「首字符数字/字母决定选词逻辑」。
    fn mix_has_quick_input(&self, idx: u8) -> bool {
        self.config
            .features
            .mix_modes
            .get(idx as usize)
            .map(|m| m.members.iter().any(|s| s == "quick_input"))
            .unwrap_or(false)
    }

    /// 选中当前页第 `page_offset`（0=首选）候选。
    /// 文本透镜（拼音/英文）走组合区逐步转换：部分匹配并入 committed 前缀、裁剪缓冲、重转剩余
    /// （剩余仍由 mix 成员方案出候选，不落五笔），留模式内不上屏；完整匹配整体上屏 + 造词。
    /// 数字透镜（计算）的候选恒整体上屏。
    fn mix_select(&self, state: &mut State, page_offset: usize) -> KeyAction {
        let (start, end) = self.page_range(state);
        let gi = start + page_offset;
        if gi >= end {
            return KeyAction::Consumed;
        }
        let cand = state.candidates[gi].clone();
        let numeric = self.mix_has_quick_input(state.mix_id) && state.mix_numeric;
        let total = state.mix_buffer.len();
        let consumed = cand.consumed_length;
        let partial = !numeric
            && consumed > 0
            && consumed < total
            && state.mix_buffer.is_char_boundary(consumed);
        if partial {
            let code = Self::cand_code(&state.mix_buffer, &cand);
            self.record_selection(&code, &cand.text);
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.mix_buffer = state.mix_buffer[consumed..].to_string();
            self.update_mix_candidates(state);
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            let out = format!("{}{}", state.committed_text, cand.text);
            if !numeric {
                let code = Self::cand_code(&state.mix_buffer, &cand);
                self.record_selection(&code, &cand.text);
                state.committed_segs.push((code, cand.text.clone()));
                self.learn_phrase_on_commit(state);
            }
            let out = self.maybe_s2t(state, &out);
            self.exit_mix_mode(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// 进入 mix 模式（至少一个成员方案可加载，由激活点保证）。
    fn enter_mix_mode(&self, state: &mut State, idx: u8) -> KeyAction {
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::Mix(idx));
        state.mix_id = idx;
        state.mix_buffer.clear();
        state.mix_numeric = false; // 由首字符（数字/字母）决定
        self.update_mix_candidates(state);
        self.notify_ui_update(state);
        let display = state.preedit.clone();
        debug!("Entered mix mode idx={}", idx);
        KeyAction::UpdateComposition {
            text: display.clone(),
            caret_pos: display.chars().count() as u32,
        }
    }

    /// 退出 mix 模式并清空相关状态（含逐步转换的已转换前缀）。
    fn exit_mix_mode(&self, state: &mut State) {
        state.active = None;
        state.mix_buffer.clear();
        state.committed_text.clear();
        state.committed_segs.clear();
        state.candidates.clear();
        state.preedit.clear();
    }

    /// 刷新 mix 候选：按配置成员序逐个查询、合并、按文本去重。
    /// "quick_input" 是内置类方案（日期/计算），用 generate_quick_input_candidates 计算；
    /// 其余为真实方案经 convert_with。数字模式只取 quick_input（表达式），文本模式只取真实方案
    /// （拼音/英文），避免互相污染候选。
    fn update_mix_candidates(&self, state: &mut State) {
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
        // 组合区 = 已转换前缀（文本透镜逐步转换累积）+ 剩余缓冲。
        state.preedit = format!("{}{}", state.committed_text, state.mix_buffer);
        if state.mix_buffer.is_empty() {
            return;
        }
        let numeric = self.mix_has_quick_input(state.mix_id) && state.mix_numeric;
        let members = self
            .config
            .features
            .mix_modes
            .get(state.mix_id as usize)
            .map(|m| m.members.clone())
            .unwrap_or_default();
        let mut cands: Vec<Candidate> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // 文本透镜：取首个真实方案的 preedit_display（拼音含音节分隔 "ni hao"）作组合区显示。
        let mut text_display: Option<String> = None;
        for member in &members {
            if member == "quick_input" {
                if !numeric {
                    continue; // 文本模式跳过计算
                }
                let dp = self.config.features.quick_input.decimal_places;
                for t in crate::quick_input::generate_quick_input_candidates(&state.mix_buffer, dp)
                {
                    if seen.insert(t.clone()) {
                        cands.push(Candidate {
                            text: t,
                            ..Default::default()
                        });
                    }
                }
            } else {
                if numeric {
                    continue; // 数字模式跳过真实方案（表达式无拼音/英文意义）
                }
                if !self.engine_mgr.ensure_schema(member) {
                    continue;
                }
                let result = self.engine_mgr.convert_with(member, &state.mix_buffer, 50);
                if text_display.is_none() && !result.preedit_display.is_empty() {
                    text_display = Some(result.preedit_display.clone());
                }
                for c in result.candidates {
                    if seen.insert(c.text.clone()) {
                        cands.push(c);
                    }
                }
            }
        }
        // 文本透镜用音节分隔显示；数字透镜（计算）保持原始表达式。
        if let Some(disp) = text_display {
            state.preedit = format!("{}{}", state.committed_text, disp);
        }
        state.candidates = cands;
    }

    /// 数字 lens（计算/表达式）：数字与符号（含 `=`）作输入，字母作选词。
    /// 仅含 quick_input 成员的 mix 在首字符为数字/符号时进入。返回该键应输入的字符。
    fn mix_numeric_input_char(key_code: u32, shift: bool) -> Option<char> {
        if (keymap::VK_A..=keymap::VK_Z).contains(&key_code) {
            None // 字母在数字 lens 作选词，不输入
        } else {
            printable_char(key_code, shift) // 数字 + 任意符号（含 = + - * / . 等）入缓冲
        }
    }

    /// mix 模式按键处理 —— 双透镜统一管线（见架构说明）。
    /// 首字符确定 lens：数字/符号 → 数字 lens（符号输入、字母选词）；字母 → 文本 lens
    /// （字母输入、数字选词、`-`/`=` 翻页）。每键顺序：控制键 → ①输入字符 → ②翻页/高亮
    /// → ③本 lens 选词键 → ④配置二三候选键 → ⑤其它标点顶屏。
    fn handle_mix_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_mix_candidates(state);
            let d = state.preedit.clone();
            this.notify_ui_update(state);
            KeyAction::UpdateComposition {
                text: d.clone(),
                caret_pos: d.chars().count() as u32,
            }
        };
        let commit_text = |this: &Self, state: &mut State, t: String| -> KeyAction {
            this.exit_mix_mode(state);
            this.notify_ui_hide();
            if t.is_empty() {
                KeyAction::ClearComposition
            } else {
                Self::commit_action(t, true)
            }
        };
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_mix_mode(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // 分步撤销：文本透镜有已转换段先退回最后一段（你→ni，码并回缓冲前部）。
                if let Some((code, _)) = state.committed_segs.pop() {
                    state.committed_text =
                        state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
                    state.mix_buffer = format!("{}{}", code, state.mix_buffer);
                    return refresh(self, state);
                }
                state.mix_buffer.pop();
                if state.mix_buffer.is_empty() {
                    self.exit_mix_mode(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    refresh(self, state)
                }
            }
            keymap::VK_SPACE => {
                // 空格：选当前高亮候选（文本透镜逐步转换）
                if state.candidates.is_empty() {
                    let out = self
                        .maybe_s2t(state, &format!("{}{}", state.committed_text, state.mix_buffer));
                    commit_text(self, state, out)
                } else {
                    let (start, _) = self.page_range(state);
                    let gi = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    self.mix_select(state, gi - start)
                }
            }
            keymap::VK_RETURN => {
                // 回车：上屏「已转换前缀 + 缓冲原文」（如完整表达式 100+200=300，或已转中文+剩余拼音）
                let out =
                    self.maybe_s2t(state, &format!("{}{}", state.committed_text, state.mix_buffer));
                commit_text(self, state, out)
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                let calc = self.mix_has_quick_input(state.mix_id);
                // 首字符确定 lens：非字母可打印字符（数字/符号）→ 数字 lens。
                if state.mix_buffer.is_empty() {
                    let is_letter = (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code);
                    state.mix_numeric =
                        calc && !is_letter && printable_char(data.key_code, shift).is_some();
                }
                let numeric = calc && state.mix_numeric;

                // ① 输入字符（按 lens）
                let input = if numeric {
                    Self::mix_numeric_input_char(data.key_code, shift)
                } else if (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code) {
                    Some((b'a' + (data.key_code - keymap::VK_A) as u8) as char)
                } else {
                    None
                };
                if let Some(ch) = input {
                    state.mix_buffer.push(ch);
                    return refresh(self, state);
                }

                // ② 翻页/高亮（输入字符已消费；数字 lens 的 -/= 已作输入吃掉）
                if let Some(act) = self.apply_nav_key(state, data, true) {
                    return act;
                }

                // ③ 本 lens 选词键：数字 lens 用字母（a=首选），文本 lens 用数字（1=首选）
                let sel = if numeric {
                    (keymap::VK_A..=keymap::VK_Z)
                        .contains(&data.key_code)
                        .then(|| (data.key_code - keymap::VK_A) as usize)
                } else {
                    (keymap::VK_1..=keymap::VK_9)
                        .contains(&data.key_code)
                        .then(|| (data.key_code - keymap::VK_1) as usize)
                };
                if let Some(off) = sel {
                    return self.mix_select(state, off);
                }

                // ④ 配置二三候选键
                if !shift {
                    if let Some(offset) = self.select_key_offset(data.key_code) {
                        return self.mix_select(state, offset);
                    }
                }

                // ⑤ 其它标点：顶屏「已转换前缀 + 当前高亮候选」+ 转换后标点，退出
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let head = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        format!("{}{}", state.committed_text, state.candidates[idx].text)
                    } else {
                        state.committed_text.clone()
                    };
                    let head = self.maybe_s2t(state, &head);
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_mix_mode(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", head, punct), true)
                } else {
                    KeyAction::Consumed
                }
            }
        }
    }

    /// 临时英文模式按键处理（首版：缓冲累积 + 空格/回车/标点上屏，暂无词库候选）
    fn handle_temp_english_key(&self, state: &mut State, data: &KeyEventData) -> KeyAction {
        // 候选感知刷新后返回组合区动作。
        let refresh = |this: &Self, state: &mut State| -> KeyAction {
            this.update_temp_english_candidates(state);
            let d = state.preedit.clone();
            this.notify_ui_update(state);
            KeyAction::UpdateComposition {
                text: d.clone(),
                caret_pos: d.chars().count() as u32,
            }
        };
        // 上屏文本（可选全角）+ 退出。
        let commit_text = |this: &Self, state: &mut State, t: String| -> KeyAction {
            let text = if state.full_width {
                to_full_width(&t)
            } else {
                t
            };
            this.exit_temp_english(state);
            this.notify_ui_hide();
            if text.is_empty() {
                KeyAction::ClearComposition
            } else {
                Self::commit_action(text, true)
            }
        };
        if let Some(act) = self.handle_candidate_nav(state, data) {
            return act;
        }
        match data.key_code {
            keymap::VK_ESCAPE => {
                self.exit_temp_english(state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                state.temp_english_buffer.pop();
                if state.temp_english_buffer.is_empty() {
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    KeyAction::ClearComposition
                } else {
                    refresh(self, state)
                }
            }
            keymap::VK_SPACE => {
                // 空格：上屏当前高亮候选（首候选=原始输入）
                let text = if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(state)
                        .min(state.candidates.len() - 1);
                    state.candidates[idx].text.clone()
                } else {
                    state.temp_english_buffer.clone()
                };
                commit_text(self, state, text)
            }
            keymap::VK_RETURN => {
                // 回车：上屏原始输入文本（不取候选）
                let text = state.temp_english_buffer.clone();
                commit_text(self, state, text)
            }
            keymap::VK_A..=keymap::VK_Z => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                let base = data.key_code - 0x41;
                let ch = if shift {
                    (b'A' + base as u8) as char
                } else {
                    (b'a' + base as u8) as char
                };
                state.temp_english_buffer.push(ch);
                refresh(self, state)
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字：有词库候选（>1，即除原文外还有匹配）时按页选词；否则作输入（英文含数字 v2）
                let (start, end) = self.page_range(state);
                let gi = start + (data.key_code - 0x31) as usize;
                if state.candidates.len() > 1 && gi < end {
                    let text = state.candidates[gi].text.clone();
                    commit_text(self, state, text)
                } else {
                    let ch = (b'0' + (data.key_code - 0x30) as u8) as char;
                    state.temp_english_buffer.push(ch);
                    refresh(self, state)
                }
            }
            0x30 if data.modifiers & MOD_SHIFT == 0 => {
                state.temp_english_buffer.push('0');
                refresh(self, state)
            }
            _ => {
                // 其它（标点等）：上屏当前高亮候选 + 转换后标点，退出
                let shift = data.modifiers & MOD_SHIFT != 0;
                if let Some(ch) = punct_char(data.key_code, shift) {
                    let base = if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(state)
                            .min(state.candidates.len() - 1);
                        state.candidates[idx].text.clone()
                    } else {
                        state.temp_english_buffer.clone()
                    };
                    let base = if state.full_width {
                        to_full_width(&base)
                    } else {
                        base
                    };
                    let punct = self.convert_punct_char(state, ch);
                    self.exit_temp_english(state);
                    self.notify_ui_hide();
                    Self::commit_action(format!("{}{}", base, punct), true)
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
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let idx = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            self.record_selection(&state.input_buffer, &t);
            Some(format!("{prefix}{t}"))
        } else if !prefix.is_empty() {
            Some(prefix)
        } else {
            None
        };
        state.input_buffer.clear();
        state.candidates.clear();
        // 进入临时拼音
        state.active = Some(ModeKind::TempPinyin);
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
        let prefix = self.take_committed(state); // 拼音逐步转换的已转换前缀一并上屏
        let committed = if !state.candidates.is_empty() {
            let idx = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let t = state.candidates[idx].text.clone();
            self.record_selection(&state.input_buffer, &t);
            Some(format!("{prefix}{t}"))
        } else if !prefix.is_empty() {
            Some(prefix)
        } else {
            None
        };
        state.input_buffer.clear();
        state.candidates.clear();
        state.active = Some(ModeKind::QuickInput);
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

    /// 拼音类「消费码」：候选自带 code（拼音段）则用之，否则退回整个输入缓冲。
    fn cand_code(buf: &str, cand: &Candidate) -> String {
        if cand.code.is_empty() {
            buf.to_string()
        } else {
            cand.code.clone()
        }
    }

    /// 取出并清空「已转换前缀」（简体），用于非选词的终结性上屏（回车/空格上屏原码/标点键）。
    /// 码表模式恒为空串，无副作用。
    fn take_committed(&self, state: &mut State) -> String {
        state.committed_segs.clear();
        std::mem::take(&mut state.committed_text)
    }

    /// 清空拼音逐步转换的组合态（已转换前缀 + 缓冲 + 候选）。
    fn reset_pinyin_composition(&self, state: &mut State) {
        state.committed_text.clear();
        state.committed_segs.clear();
        state.input_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
    }

    /// 主输入路拼音选词 —— 组合区逐步转换（C）。
    /// 部分匹配（候选只消费缓冲前缀）：把汉字并入 `committed_text` 前缀、裁剪缓冲、重转剩余，
    /// **留在组合区不上屏到应用**，返回 UpdateComposition。
    /// 完整匹配（消费整串）：整体上屏 `committed_text + 候选` 到应用，触发自动造词（L），清空。
    /// 规整短语/命令候选显示文本：换行/制表 → 空格（杜绝多行候选），超长截断加省略号。
    /// `max` 为最大字符数（`input.phrase.max_display_chars`），0 表示不限制。
    fn clamp_candidate_display(s: &str, max: usize) -> String {
        let one_line: String = s
            .chars()
            .map(|c| {
                if c == '\n' || c == '\r' || c == '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if max == 0 || one_line.chars().count() <= max {
            one_line
        } else {
            let head: String = one_line.chars().take(max).collect();
            format!("{head}…")
        }
    }

    /// 前缀导航候选选中：把输入缓冲补全到该组完整码并重查候选（展开成员/精确命令），
    /// 实现"敲 zz → 选标点 → 展开标点字符"的二级选择。返回新 preedit 显示文本。
    fn complete_to_group_code(&self, state: &mut State, group_code: &str) -> String {
        state.input_buffer = group_code.to_string();
        let _ = self.update_candidates(state);
        self.notify_ui_update(state);
        state.preedit.clone()
    }

    fn commit_selected(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        // 前缀导航候选：补全输入到该组完整码并重查展开（二级选择，不上屏组名）。
        if cand.is_group {
            let code = cand.group_code.clone();
            let display = self.complete_to_group_code(state, &code);
            return KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            };
        }
        // $CC 命令候选：执行动作而非上屏 display 标签。
        if cand.is_command {
            return self.commit_command(state, cand);
        }
        let total = state.input_buffer.len();
        let consumed = cand.consumed_length;
        let code = Self::cand_code(&state.input_buffer, cand);
        let partial =
            consumed > 0 && consumed < total && state.input_buffer.is_char_boundary(consumed);
        // 词频按候选实际编码记账（分段时为前缀码，如「ni」而非整串「nihao」）。
        self.record_selection(&code, &cand.text);
        if partial {
            state.committed_segs.push((code, cand.text.clone()));
            state.committed_text.push_str(&cand.text);
            state.input_buffer = state.input_buffer[consumed..].to_string();
            let _ = self.update_candidates(state); // preedit 已含前缀（update_candidates 内拼接）
            let display = state.preedit.clone();
            self.notify_ui_update(state);
            KeyAction::UpdateComposition {
                caret_pos: display.chars().count() as u32,
                text: display,
            }
        } else {
            state.committed_segs.push((code, cand.text.clone()));
            let final_simplified = format!("{}{}", state.committed_text, cand.text);
            self.learn_phrase_on_commit(state); // 自动造词（多段组成的词）
            let out = self.maybe_s2t(state, &final_simplified);
            self.reset_pinyin_composition(state);
            self.notify_ui_hide();
            Self::commit_action(out, true)
        }
    }

    /// $CC 命令候选选中：清理组合区、隐藏 UI，把命令源放独立线程异步执行。
    /// **异步是必须的**：控制器经 Weak 回调 handle_menu_command 等自锁方法，而此刻本线程
    /// 仍持 state 锁（std::sync::Mutex 非可重入），同线程重入即死锁——交独立线程待本次按键
    /// 处理释放锁后再跑（对齐 Go「不在 SearchCommand 持锁路径里再 Lock」的约束）。
    fn commit_command(&self, state: &mut State, cand: &Candidate) -> KeyAction {
        let src = cand.phrase_template.clone();
        // 命令 nav（从前缀列举选中）携完整码 group_code，用它作执行输入上下文
        // （让 code()/input() 等按完整码求值）；精确码命令 group_code 空 → 用当前缓冲。
        let input = if cand.group_code.is_empty() {
            state.input_buffer.clone()
        } else {
            cand.group_code.clone()
        };
        self.reset_pinyin_composition(state);
        self.notify_ui_hide();
        self.spawn_command(src, input);
        // ClearComposition 而非 Consumed：清掉应用里已输入的命令码（如 "coen"），
        // 否则 composition 残留（Consumed 仅吞键、不结束 composition）。type() 的上屏文本
        // 由命令线程经 push 管道单独提交。
        KeyAction::ClearComposition
    }

    /// 在独立线程执行命令源（解析→求值→按序跑动作；type 文本经 push 提交、其余为副作用）。
    fn spawn_command(&self, src: String, input: String) {
        let Some(this) = self.self_weak.get().and_then(std::sync::Weak::upgrade) else {
            warn!("cmdbar: self_weak 未装配，命令跳过");
            return;
        };
        std::thread::spawn(move || {
            this.run_command_candidate(&src, &input);
        });
    }

    /// 把命令产生的文本经 push 管道提交给活动客户端（命令在独立线程执行，走 push 而非 KeyAction）。
    pub(crate) fn push_commit_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let encoded = wind_ipc::codec::encode_commit_text(text, None, false, true, false);
        self.push_server.push_commit_to_active(&encoded);
    }

    /// cmdbar 能力 wrapper（被 handle_cmdbar 控制器经 Weak 回调）。各方法自锁，**禁止**在持
    /// state 锁时调用（spawn_command 已确保在独立线程、未持锁时执行）。
    pub(crate) fn cmd_ime_toggle(&self, target: &str) {
        match target {
            "cn-en" => {
                self.handle_menu_command("toggle_mode");
            }
            "fullshape" => {
                self.handle_menu_command("toggle_width");
                // handle_menu_command 只 push_state_update，不刷工具栏；菜单路径由调用方
                // 补 notify_toolbar，命令栏路径同样需要补，否则工具栏全/半角状态不更新。
                self.notify_toolbar();
            }
            "s2t" => {
                self.handle_menu_command("toggle_s2t");
                self.notify_toolbar();
            }
            "toolbar" => self.toggle_toolbar(),
            "preedit" => self.cmd_toggle_preedit(),
            "candwin" => self.cmd_toggle_candwin(),
            "layout" => self.cmd_toggle_layout(),
            other => {
                warn!("ime.toggle: 暂不支持 target {:?}（Rust 平台能力待补）", other)
            }
        }
    }

    /// 切换 preedit 编码显示模式（top ↔ embedded），下发 UI（运行时态，暂不持久化）。
    fn cmd_toggle_preedit(&self) {
        let embedded = {
            let mut e = self
                .preedit_embedded
                .lock()
                .unwrap_or_else(|x| x.into_inner());
            *e = !*e;
            *e
        };
        let _ = self.ui_tx.send(UiCommand::SetPreeditEmbedded(embedded));
        // 持久化到用户层 ui.candidate.preedit_mode（重启后保留）。
        if let Err(e) = Config::set_user_string(
            &["ui", "candidate", "preedit_mode"],
            if embedded { "embedded" } else { "top" },
        ) {
            warn!("ime.toggle preedit: 持久化失败: {}", e);
        }
        self.show_tip(if embedded { "编码:嵌入" } else { "编码:顶部" });
    }

    /// 切换候选窗显隐（运行时态）。隐藏时下次刷新即不显示候选。
    fn cmd_toggle_candwin(&self) {
        let hidden = {
            let mut h = self
                .hide_candidate_window
                .lock()
                .unwrap_or_else(|x| x.into_inner());
            *h = !*h;
            *h
        };
        if hidden {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
        }
        self.show_tip(if hidden { "候选窗:隐藏" } else { "候选窗:显示" });
    }

    /// 切换候选布局方向（横排 ↔ 竖排），下发 UI 并持久化。命令栏 ime.toggle("layout")。
    /// 切换时 composition 已清（命令选中即 ClearComposition），下次输入按新方向渲染。
    fn cmd_toggle_layout(&self) {
        let vertical = {
            let mut v = self
                .candidate_vertical
                .lock()
                .unwrap_or_else(|x| x.into_inner());
            *v = !*v;
            *v
        };
        let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(vertical));
        // 持久化 ui.candidate.layout（重启后保留）。
        if let Err(e) = Config::set_user_string(
            &["ui", "candidate", "layout"],
            if vertical { "vertical" } else { "horizontal" },
        ) {
            warn!("ime.toggle layout: 持久化失败: {}", e);
        }
        self.show_tip(if vertical { "候选:竖排" } else { "候选:横排" });
    }

    /// 切换输入方案并持久化 `schema.active` 到用户层配置（重启后保留）。
    pub(crate) fn cmd_set_schema(&self, id: &str) {
        self.switch_schema(id);
        if let Err(e) = Config::set_user_string(&["schema", "active"], id) {
            warn!("ime.schema: 持久化 schema.active 失败: {}", e);
        }
    }

    /// 循环切换主题并持久化；dir="prev" 向前，其余向后。返回新主题显示名。
    pub(crate) fn cmd_theme_cycle(&self, dir: &str) -> String {
        let list = self.list_themes(); // Vec<(id, name)>
        if list.is_empty() {
            return String::new();
        }
        let cur = self
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let pos = list.iter().position(|(id, _)| *id == cur).unwrap_or(0);
        let n = list.len();
        let next = if dir == "prev" {
            (pos + n - 1) % n
        } else {
            (pos + 1) % n
        };
        self.select_theme(next);
        list[next].1.clone()
    }

    /// 加词到用户层（code 为空时暂不支持自动推导编码）。
    pub(crate) fn cmd_dict_add(&self, text: &str, code: &str) -> anyhow::Result<()> {
        let Some(store) = &self.store else {
            anyhow::bail!("dict.add: 无 store");
        };
        if code.is_empty() {
            anyhow::bail!("dict.add: code 为空（Rust 端暂未支持自动推导编码）");
        }
        let schema = self.engine_mgr.active_schema_id();
        store.add_user_word(&schema, code, text, 100)?;
        Ok(())
    }

    /// 自动造词（L）：仅当用户**分步**组成（committed_segs ≥2 段、合并 ≥2 字）才学。
    /// 完整拼音码 = 各段码拼接；词 = 各段汉字拼接。写入临时层（需临时层，达阈值由 store 晋升路线处理）。
    fn learn_phrase_on_commit(&self, state: &State) {
        if state.committed_segs.len() < 2 {
            return;
        }
        let code: String = state.committed_segs.iter().map(|(c, _)| c.as_str()).collect();
        let text: String = state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
        if text.chars().count() < 2 || code.is_empty() {
            return;
        }
        let Some(store) = &self.store else { return };
        let schema = self.engine_mgr.active_schema_id();
        // add_weight/delta 取保守默认；晋升计数阈值由临时层累积达成（后续可接入 schema.learning 配置）。
        if let Err(e) = store.learn_temp_word(&schema, &code, &text, LEARN_ADD_WEIGHT, LEARN_WEIGHT_DELTA) {
            warn!("learn_temp_word failed: {}", e);
        } else {
            debug!("auto-learned phrase: {} -> {}", code, text);
        }
    }

    fn notify_ui_update(&self, state: &State) {
        if state.candidates.is_empty() && state.input_buffer.is_empty() {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            return;
        }
        // candwin 切换：用户隐藏候选窗时不显示（仍可盲打/自动上屏）。
        if *self
            .hide_candidate_window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            return;
        }
        let t_nu = std::time::Instant::now();
        // 仅推送当前页候选（窗口按 1..N 编号，翻页后重新编号）
        let (start, end) = self.page_range(state);
        // 数字键需录入表达式的场景用字母标签（a/b/c）选词：旧快捷输入，以及 mix 的数字模式。
        let alpha = state.active == Some(ModeKind::QuickInput)
            || (matches!(state.active, Some(ModeKind::Mix(_))) && state.mix_numeric);
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
                    comment: c.comment.clone(),
                }
            })
            .collect();
        // 翻页信息改为结构化字段传给候选窗（窗口内渲染独立的页码指示）
        let total_pages = self.total_pages(state);
        let selected = state.selected_index.min(items.len().saturating_sub(1));
        // 悬停目标独立于选中项：候选越界视为无悬停，翻页器 tag 原样透传
        let hover = match state.hover_index {
            h if (0..wind_ui::manager::HOVER_PAGE_PREV).contains(&h) => {
                if (h as usize) < items.len() { h } else { -1 }
            }
            h => h, // 翻页器 tag / -1
        };
        // 有效光标坐标判定：高度>0、非 (0,0)、在合理范围；无效则回退到最近有效坐标
        let (cx, cy, ch) = (state.caret_x, state.caret_y, state.caret_height);
        let valid = ch > 0 && !(cx == 0 && cy == 0) && cx.abs() < 32000 && cy.abs() < 32000;
        let (caret_x, caret_y, caret_height, caret_valid) = {
            let mut lv = self
                .last_valid_caret
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if valid {
                *lv = (cx, cy, ch);
                (cx, cy, ch, true)
            } else if lv.2 > 0 {
                (lv.0, lv.1, lv.2, true) // 回退到最近有效坐标，避免跑到屏幕左上角
            } else {
                (cx, cy, ch, false) // 尚无任何有效坐标：临时显示，待有效坐标到达再重定位
            }
        };
        *self
            .awaiting_caret
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = !caret_valid;
        let n_items = items.len();
        // preedit 是否在候选窗显示，按配置门控：inline_preedit=true 时组合串内联显示在应用内
        // （应用侧嵌入编码），候选窗不再重复显示 preedit 条。否则照常显示（默认 top 模式）。
        let preedit = if self.config.ui.candidate.inline_preedit {
            String::new()
        } else {
            state.preedit.clone()
        };
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit,
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
        tracing::debug!(
            "notify_ui_update: build+send {:?} (n={})",
            t_nu.elapsed(),
            n_items
        );
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
                    let _ = self
                        .ui_tx
                        .send(UiCommand::OpenPath(d.display().to_string()));
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
        let (id, name) = list[index].clone();
        *self.theme_name.lock().unwrap_or_else(|e| e.into_inner()) = id.clone();
        let dark = *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner());
        self.push_theme(&id, dark);
        self.persist_theme(&id);
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

    /// 主题搜索目录：用户主题目录（%APPDATA%\WindInput\themes，优先覆盖）+ 安装主题目录。
    /// 用户目录靠前 → 同名主题用户版覆盖内置；base 继承跨目录解析（用户主题可 `base: _base`）。
    fn theme_search_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Some(d) = Config::user_config_dir() {
            dirs.push(d.join("themes"));
        }
        if let Some(d) = &self.themes_dir {
            dirs.push(d.clone());
        }
        dirs
    }

    /// 加载并下发指定主题（失败保留当前）。跨用户+安装目录解析（含 base 继承）。
    fn push_theme(&self, name: &str, is_dark: bool) {
        let dirs = self.theme_search_dirs();
        if dirs.is_empty() {
            return;
        }
        match wind_theme::load_resolved_dirs(&dirs, name, is_dark) {
            Ok(t) => {
                info!("Loaded theme: {} (dark={})", name, is_dark);
                let _ = self.ui_tx.send(UiCommand::SetTheme(Box::new(t)));
            }
            Err(e) => warn!("Failed to load theme {}: {}", name, e),
        }
    }

    /// 列出可用主题：(id, 显示名)。扫用户+安装目录，含 theme.yaml、非 `_` 前缀；
    /// 显示名取 meta.name（缺则用 id），按 (meta.order, id) 排序。
    fn list_themes(&self) -> Vec<(String, String)> {
        let dirs = self.theme_search_dirs();
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<(String, String, i32)> = Vec::new();
        for dir in &dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in rd.filter_map(|e| e.ok()) {
                if !e.path().is_dir() {
                    continue;
                }
                let Ok(id) = e.file_name().into_string() else {
                    continue;
                };
                if id.starts_with('_') || !dir.join(&id).join("theme.yaml").exists() {
                    continue;
                }
                if !seen.insert(id.clone()) {
                    continue;
                }
                let meta = wind_theme::read_meta(&dirs, &id);
                let name = meta
                    .as_ref()
                    .map(|m| m.name.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.clone());
                let order = meta.as_ref().map(|m| m.order).unwrap_or(0);
                rows.push((id, name, order));
            }
        }
        rows.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter().map(|(id, name, _)| (id, name)).collect()
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
        let mut schema_children =
            vec![M::leaf("英文", cmd(MenuCmd::SchemaEnglish), true, !chinese)];
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
        for (i, (id, name)) in themes.iter().enumerate() {
            theme_children.push(M::leaf(
                name.clone(),
                cmd(MenuCmd::ThemeSelect(i)),
                true,
                *id == cur_theme,
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
            s2t_children.push(M::leaf(
                *label,
                cmd(MenuCmd::S2tVariant(i)),
                true,
                s2t_variant == *id,
            ));
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
        let _ = self
            .ui_tx
            .send(UiCommand::ShowCandidateMenu { items, x, y });
    }

    fn is_menu_open(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .menu_open
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
            0x26
            | 0x28
            | 0x25
            | 0x27
            | keymap::VK_RETURN
            | keymap::VK_SPACE
            | keymap::VK_ESCAPE => {
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
        let _ = self
            .ui_tx
            .send(UiCommand::ShowCandidateMenu { items, x, y });
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

    /// 判断标点字符 `ch` 是否参与当前生效的自动配对（作为左符号或右符号）。
    /// 智能符号与自动配对互斥的判定依据（见 `smart_symbol_arm_str`）。
    fn is_auto_pair_char(&self, state: &State, ch: char) -> bool {
        match self.active_pairs(state.chinese_punct) {
            Some(pairs) => pairs.iter().any(|(l, r)| *l == ch || *r == ch),
            None => false,
        }
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
        // 前缀导航候选：补全输入到完整码并重查展开（二级选择，鼠标点击同键盘选中）。
        if state.candidates[idx].is_group {
            let code = state.candidates[idx].group_code.clone();
            self.complete_to_group_code(&mut state, &code);
            return;
        }
        // $CC 命令候选：执行动作而非上屏 display 标签（释放锁后异步执行，避免重入死锁）。
        if state.candidates[idx].is_command {
            let src = state.candidates[idx].phrase_template.clone();
            // 命令 nav 携完整码 group_code 作执行输入；精确码命令用当前缓冲。
            let gc = state.candidates[idx].group_code.clone();
            let input = if gc.is_empty() {
                state.input_buffer.clone()
            } else {
                gc
            };
            state.active = None;
            drop(state);
            self.notify_ui_hide();
            self.spawn_command(src, input);
            return;
        }
        let text = state.candidates[idx].text.clone();
        let chinese_mode = state.chinese_mode;
        let out = self.commit_candidate(&mut state, &text);
        // 鼠标提交后彻底复位各输入模式，避免遗留状态
        state.active = None;
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.temp_english_buffer.clear();
        drop(state);

        self.notify_ui_hide();
        let encoded = wind_ipc::codec::encode_commit_text(&out, None, false, chinese_mode, false);
        // 仅推给活动客户端，避免广播导致多个 TSF 端重复上屏
        self.push_server.push_commit_to_active(&encoded);
        debug!(
            "mouse_select: committed '{}' (page_local={})",
            out, page_local
        );
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
            icon_label: if state.chinese_mode {
                "中".into()
            } else {
                "英".into()
            },
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
            s.chinese_mode,
            s.full_width,
            s.chinese_punct,
            s.toolbar_visible,
            s.caps_lock,
            false,
            &s.key_down_hotkeys,
            &s.key_up_hotkeys,
            &s.icon_label,
        );
        self.push_server.push_to_active(&encoded);
    }

    fn push_state_update(&self) {
        let s = self.build_status();
        let encoded = wind_ipc::codec::encode_state_push(
            s.chinese_mode,
            s.full_width,
            s.chinese_punct,
            s.toolbar_visible,
            s.caps_lock,
            &s.icon_label,
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

    /// 空缓冲模式激活的单一入口（对齐 key-pipeline.md §2.1 优先级链）。
    /// 优先级：临时英文(Shift+字母) > 快捷输入 > 临时拼音 > 特殊模式。命中返回激活 KeyAction，
    /// 都不命中返回 None（落普通输入）。URL 前缀夺取是「缓冲扩展夺取」语义，不在此链，单独处理。
    fn try_activate_mode(&self, state: &mut State, data: &KeyEventData) -> Option<KeyAction> {
        // 临时英文：Shift+字母（空缓冲 + 无候选 + 已启用）
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && self.config.input.shift_temp_english.enabled
            && data.modifiers & MOD_SHIFT != 0
            && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
            && (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code)
        {
            let ch = (b'A' + (data.key_code - 0x41) as u8) as char; // 首字母大写
            state.active = Some(ModeKind::TempEnglish);
            state.temp_english_buffer = ch.to_string();
            self.update_temp_english_candidates(state);
            let disp = state.preedit.clone();
            self.notify_ui_update(state);
            debug!("Entered temp English mode (buffer={})", disp);
            return Some(KeyAction::UpdateComposition {
                text: disp.clone(),
                caret_pos: disp.chars().count() as u32,
            });
        }

        // 快捷输入已退役为内置类方案 mix 成员（quick_input），不再独立激活：
        // 想要纯快捷输入，配一个 members=["quick_input"] 的 mix 即可。; 默认走「快捷」融合 mix。

        // 临时拼音：码表方案 + 空缓冲 + 匹配触发键 + 无修饰键（不要求候选空）
        if state.input_buffer.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
            && self.is_temp_pinyin_trigger(data.key_code)
        {
            if let Some(target) = self.engine_mgr.temp_pinyin_target() {
                state.active = Some(ModeKind::TempPinyin);
                state.temp_pinyin_schema = target;
                state.temp_pinyin_buffer.clear();
                state.temp_pinyin_prefix = Self::temp_pinyin_prefix_for(data.key_code).to_string();
                self.update_temp_pinyin_candidates(state);
                let display = state.preedit.clone();
                self.notify_ui_update(state);
                debug!(
                    "Entered temp pinyin mode (prefix={})",
                    state.temp_pinyin_prefix
                );
                return Some(KeyAction::UpdateComposition {
                    text: display.clone(),
                    caret_pos: display.chars().count() as u32,
                });
            }
        }

        // 特殊模式：空缓冲 + 无候选 + 无修饰键 + 引导键匹配（优先级最低）。
        // 码表不可用时不拦截该键，返回 None 继续普通流程。
        if state.input_buffer.is_empty()
            && state.candidates.is_empty()
            && data.modifiers & (MOD_CTRL | MOD_ALT | MOD_SHIFT) == 0
        {
            if let Some(idx) = self.match_special_trigger(data.key_code) {
                // 方案可加载才进入（否则不拦截该键，落普通流程）。
                if let Some(schema) = self.special_schema(idx) {
                    if self.engine_mgr.ensure_schema(&schema) {
                        return Some(self.enter_special_mode(state, idx));
                    }
                }
            }
            // 临时 mix：含 quick_input 或至少一个可加载成员方案才进入（优先级最低）。
            if let Some(idx) = self.match_mix_trigger(data.key_code) {
                if self.mix_has_quick_input(idx) || !self.mix_members(idx).is_empty() {
                    return Some(self.enter_mix_mode(state, idx));
                }
            }
        }

        None
    }

    /// 复位三种独占输入模式（临时英文/临时拼音/快捷输入）的状态。仅清空，不负责上屏；
    /// 调用方需在调用前取出待上屏文本（如模式切换时的临时英文缓冲）。
    fn reset_exclusive_modes(&self, state: &mut State) {
        let dirty = state.active.is_some();
        state.active = None;
        state.temp_english_buffer.clear();
        state.temp_pinyin_buffer.clear();
        state.temp_pinyin_prefix.clear();
        state.quick_input_buffer.clear();
        state.quick_input_prefix.clear();
        state.url_buffer.clear();
        state.rewind = None;
        state.special_buffer.clear();
        state.mix_buffer.clear();
        state.mix_numeric = false;
        // 清理可能残留的组合显示（临时拼音/快捷输入会产生候选与 preedit）
        state.input_buffer.clear();
        state.candidates.clear();
        state.preedit.clear();
        // 拼音逐步转换的已转换前缀一并丢弃（焦点/模式切换不保留半成品组合）。
        state.committed_text.clear();
        state.committed_segs.clear();
        // 焦点/模式切换：解除智能符号待命，避免跨上下文误触发替换。
        self.disarm_smart_symbol();
        if dirty {
            debug!("reset_exclusive_modes: cleared residual exclusive input mode state");
        }
    }

    /// 切换中英文时取消当前输入：清空缓冲/候选/preedit，并按 `hotkeys.commit_on_switch`
    /// 决定是否把已输入的原始编码上屏（仅在切到英文且有待输入时）。返回待上屏文本。
    fn take_input_on_mode_switch(&self, state: &mut State, chinese: bool) -> String {
        // 独占模式优先：临时英文残留按“模式切换上屏”语义提交，临时拼音/快捷输入丢弃。
        // 独占模式下 input_buffer 必为空，与下方普通组合分支互斥，故在此提前返回。
        if state.active.is_some() {
            let text = if state.active == Some(ModeKind::TempEnglish)
                && !state.temp_english_buffer.is_empty()
            {
                if state.full_width {
                    to_full_width(&state.temp_english_buffer)
                } else {
                    state.temp_english_buffer.clone()
                }
            } else {
                String::new()
            };
            self.reset_exclusive_modes(state);
            self.notify_ui_hide();
            return text;
        }
        let has_pending = !state.input_buffer.is_empty() || !state.committed_text.is_empty();
        let commit = has_pending && !chinese && self.config.hotkeys.commit_on_switch;
        let text = if commit {
            // 切到英文且配置上屏：把「已转换前缀 + 剩余原码」一并上屏。
            let prefix = self.take_committed(state);
            self.maybe_s2t(state, &format!("{}{}", prefix, state.input_buffer))
        } else {
            String::new()
        };
        state.committed_text.clear();
        state.committed_segs.clear();
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
                debug!(
                    "Hotkey matched (key_down): {} (0x{:08X})",
                    action, norm_hash
                );
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

        // 统一夺取回退：夺取式模式（URL/后续 z 临拼）中，退到夺取边界再按退格 →
        // 撤销夺取、把快照回放回正常码表输入流（而非停在无候选的独占模式）。
        // 须先于下方单点分派，否则退格会被模式处理器按普通删字符消费。
        if data.key_code == keymap::VK_BACK && self.can_rewind(&state) {
            return self.rewind_hijack(&mut state);
        }

        // 已激活独占模式：单点分派到专用处理器（唯一入口，见 pipeline.rs）。
        match state.active {
            Some(ModeKind::TempPinyin) => return self.handle_temp_pinyin_key(&mut state, data),
            Some(ModeKind::QuickInput) => return self.handle_quick_input_key(&mut state, data),
            Some(ModeKind::TempEnglish) => return self.handle_temp_english_key(&mut state, data),
            Some(ModeKind::Url) => return self.handle_url_key(&mut state, data),
            Some(ModeKind::Special(_)) => return self.handle_special_key(&mut state, data),
            Some(ModeKind::Mix(_)) => return self.handle_mix_key(&mut state, data),
            None => {}
        }

        // 空缓冲模式激活：单一入口，优先级链见 try_activate_mode（对齐 key-pipeline.md §2.1）。
        if let Some(act) = self.try_activate_mode(&mut state, data) {
            return act;
        }

        // Ctrl/Alt 组合（非热键）：有输入则清空并隐藏候选窗，否则透传。
        // 必须 notify_ui_hide：否则候选窗残留（如 Ctrl+A 时卡死，需再输入才复位）。
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                self.reset_pinyin_composition(&mut state);
                self.notify_ui_hide();
                return KeyAction::ClearComposition;
            }
            return KeyAction::PassThrough;
        }

        // ── 网址模式激活（夺取式）──
        // 普通输入累积时，若 input_buffer + 当前键字符 恰好等于某前缀（如 "www."/"http"），
        // 则夺取进入网址模式。置于主分派前，确保「补全前缀的那一键」（字母或 '.'）先被截获，
        // 不落入普通码表/标点处理。前缀按惯例小写，故探针用小写字母对齐 input_buffer。
        if self.config.input.url_input.enabled {
            let shift = data.modifiers & MOD_SHIFT != 0;
            if let Some(ch) = printable_char(data.key_code, shift) {
                let probe = format!("{}{}", state.input_buffer, ch.to_ascii_lowercase());
                if self.is_url_prefix(&probe) {
                    return self.enter_url_mode(&mut state, probe);
                }
            }
        }

        debug!(
            "key_event: code=0x{:02X} mods=0x{:04X} chinese={} buf='{}'",
            data.key_code, data.modifiers, state.chinese_mode, state.input_buffer
        );

        // 候选翻页/高亮：配置驱动统一处理（普通模式为码表型，`-`/`=` 可作翻页）。
        // 仅有候选时生效；无候选时下方 match 的回退臂负责透传方向/翻页键。
        if let Some(act) = self.apply_nav_key(&mut state, data, true) {
            return act;
        }

        match data.key_code {
            keymap::VK_ESCAPE => {
                // Escape：取消整个组合（含已转换前缀），不上屏
                self.reset_pinyin_composition(&mut state);
                self.notify_ui_hide();
                KeyAction::ClearComposition
            }
            keymap::VK_BACK => {
                // Backspace：分步撤销——有已转换段则先把最后一段退回拼音（你→ni，码并回剩余
                // 缓冲前部、重转），否则删剩余拼音末字符。
                if !state.committed_segs.is_empty() {
                    let (code, _) = state.committed_segs.pop().unwrap();
                    state.committed_text =
                        state.committed_segs.iter().map(|(_, t)| t.as_str()).collect();
                    state.input_buffer = format!("{}{}", code, state.input_buffer);
                    self.update_candidates(&mut state);
                    let display = state.preedit.clone();
                    self.notify_ui_update(&state);
                    KeyAction::UpdateComposition {
                        caret_pos: display.chars().count() as u32,
                        text: display,
                    }
                } else if !state.input_buffer.is_empty() {
                    state.input_buffer.pop();
                    self.update_candidates(&mut state);
                    if state.input_buffer.is_empty() {
                        self.notify_ui_hide();
                        KeyAction::ClearComposition
                    } else {
                        let display = state.preedit.clone();
                        self.notify_ui_update(&state);
                        KeyAction::UpdateComposition {
                            // 光标按显示串字符数（拼音 preedit 含分词空格，与原始字节长不同）。
                            caret_pos: display.chars().count() as u32,
                            text: display,
                        }
                    }
                } else {
                    KeyAction::PassThrough
                }
            }
            keymap::VK_SPACE => {
                // Space：选当前高亮候选 / 上屏编码
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(&state)
                        .min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_selected(&mut state, &cand)
                } else if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    // 无候选：上屏「已转换前缀 + 剩余拼音原码」。
                    let prefix = self.take_committed(&mut state);
                    let text = self.maybe_s2t(&state, &format!("{}{}", prefix, state.input_buffer));
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            keymap::VK_RETURN => {
                // Enter：上屏「当前显示」= 已转换前缀 + 剩余原码（已选中文照样上屏），退出
                if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    let prefix = self.take_committed(&mut state);
                    let text = self.maybe_s2t(&state, &format!("{}{}", prefix, state.input_buffer));
                    state.input_buffer.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            keymap::VK_1..=keymap::VK_9 if data.modifiers & MOD_SHIFT == 0 => {
                // 数字键 1-9 选当前页第 N 个候选（Shift+数字走标点分支）
                let (start, end) = self.page_range(&state);
                let in_page = (data.key_code - 0x31) as usize;
                let idx = start + in_page;
                if idx < end {
                    let cand = state.candidates[idx].clone();
                    self.commit_selected(&mut state, &cand)
                } else if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    let prefix = self.take_committed(&mut state);
                    let mut text = format!("{}{}", prefix, state.input_buffer);
                    state.input_buffer.clear();
                    state.candidates.clear();
                    // 数字键 vk keymap::VK_1..=keymap::VK_9 即 ASCII '1'..='9'
                    text.push(data.key_code as u8 as char);
                    let text = self.maybe_s2t(&state, &text);
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    KeyAction::PassThrough
                }
            }
            keymap::VK_A..=keymap::VK_Z => {
                // A-Z 字母累积
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                state.input_buffer.push(ch);

                // 顶码上屏：缓冲超过满码长且整串无匹配 → 顶前 N 码首选，余码续打
                // （schema.top_code_commit；置于候选刷新前，对齐 Go handleAlphaKey）。
                if let Some((top_text, remainder)) =
                    self.engine_mgr.handle_top_code(&state.input_buffer)
                {
                    let buf = state.input_buffer.clone();
                    let prefix = &buf[..buf.len().saturating_sub(remainder.len())];
                    self.record_selection(prefix, &top_text);
                    state.input_buffer = remainder.clone();
                    let _ = self.update_candidates(&mut state); // 余码候选（不再消费其结局）
                    let preedit = state.preedit.clone();
                    self.notify_ui_update(&state);
                    let has_comp = !remainder.is_empty();
                    return KeyAction::InsertText {
                        text: top_text,
                        new_composition: has_comp.then_some(preedit),
                        mode_changed: false,
                        chinese_mode: true,
                        has_new_composition: has_comp,
                    };
                }

                // 全码自动上屏 / 满码空码清空（schema.auto_commit_at_full / clear_on_empty_max）。
                match self.update_candidates(&mut state) {
                    InputOutcome::AutoCommit(text) => {
                        let out = self.commit_candidate(&mut state, &text);
                        self.notify_ui_hide();
                        return Self::commit_action(out, true);
                    }
                    InputOutcome::Clear => {
                        state.input_buffer.clear();
                        state.candidates.clear();
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    InputOutcome::Normal => {}
                }
                let display = state.preedit.clone();
                self.notify_ui_update(&state);
                KeyAction::UpdateComposition {
                    // 光标按显示串字符数（拼音 preedit 含分词空格，与原始字节长不同）。
                    caret_pos: display.chars().count() as u32,
                    text: display,
                }
            }
            keymap::VK_UP | keymap::VK_DOWN | keymap::VK_PRIOR | keymap::VK_NEXT => {
                // 方向/翻页键回退臂：有候选时翻页/高亮已由上面的 apply_nav_key（配置驱动）处理，
                // 这里只剩"无候选"情形——无组合则透传给应用，有组合则消费。
                if state.input_buffer.is_empty() && state.committed_text.is_empty() {
                    KeyAction::PassThrough
                } else {
                    KeyAction::Consumed
                }
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
                            let cand = state.candidates[idx].clone();
                            return self.commit_selected(&mut state, &cand);
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
                    // 智能符号模式：同键连按删中文标点改英文（press2 短路返回）。
                    // 须在候选提交逻辑之前：press2 时无待输入，依赖光标前字符匹配武装态。
                    if let Some(act) = self.try_smart_symbol_replace(&state, ch, data.prev_char) {
                        return act;
                    }
                    // 标点/符号键：先上屏已转换前缀 + 首选候选（若有输入），再追加（转换后的）标点
                    let committed = self.take_committed(&mut state);
                    let mut out = self.maybe_s2t(&state, &committed);
                    if !state.candidates.is_empty() {
                        let idx = self
                            .highlighted_global_index(&state)
                            .min(state.candidates.len() - 1);
                        let t = state.candidates[idx].text.clone();
                        self.record_selection(&state.input_buffer, &t);
                        out.push_str(&self.maybe_s2t(&state, &t));
                    } else if !state.input_buffer.is_empty() {
                        out.push_str(&state.input_buffer);
                    }
                    let had_input = !state.input_buffer.is_empty()
                        || !state.candidates.is_empty()
                        || !committed.is_empty();
                    state.input_buffer.clear();
                    state.candidates.clear();

                    // 标点单点流水线：自定义映射 > 数字后智能 > 中文标点 > 全半角。
                    let piece = self.convert_punct(&state, ch, data.prev_char);
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
                        if let Some((_, right)) = pairs.iter().find(|(l, _)| *l == pch).copied() {
                            self.pair_tracker
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(pch, right);
                            let cursor_offset = out.encode_utf16().count() as u32;
                            let text = format!("{}{}", out, right);
                            return KeyAction::InsertTextWithCursor {
                                text,
                                cursor_offset,
                            };
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
            self.reset_exclusive_modes(&mut s); // 失焦丢弃临时英文/拼音/快捷输入残留
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
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ime_active = true;
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
            self.reset_exclusive_modes(&mut s); // 切走本输入法时丢弃独占模式残留
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
            self.reset_exclusive_modes(&mut state); // 系统模式切换时丢弃独占模式残留
        }
    }

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = !state.chinese_mode;
        let chinese = state.chinese_mode;
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese);
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.disarm_smart_symbol();
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
        self.disarm_smart_symbol();
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
        let now_valid = data.height > 0 && !(data.x == 0 && data.y == 0) && data.x.abs() < 32000;
        let awaiting = *self
            .awaiting_caret
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let tk = data.trigger_key as u32; // 协议为 u16，统一按 VK(u32) 比对
        let text = if tk == keymap::VK_SPACE {
            if !state.candidates.is_empty() {
                state.candidates[0].text.clone()
            } else {
                state.input_buffer.clone()
            }
        } else if tk == keymap::VK_RETURN {
            state.input_buffer.clone()
        } else if (keymap::VK_1..=keymap::VK_9).contains(&tk) {
            let idx = (tk - keymap::VK_1) as usize;
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
