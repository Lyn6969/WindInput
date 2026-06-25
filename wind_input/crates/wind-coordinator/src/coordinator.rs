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

use crate::pipeline::{ModeKind, Rewind};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};
use wind_keys::keymap;

use wind_bridge::handler::*;
use wind_bridge::push::{PushConfig, PushServer};
use wind_candidate::Candidate;
use wind_config::Config;
use wind_config::PreeditDisplay;
use wind_config::hotkey::{self, CompiledHotkeys};
use wind_engine::EngineManager;
use wind_ipc::protocol::{
    EVENT_KEY_DOWN, EVENT_KEY_UP, MOD_ALT, MOD_CTRL, MOD_SHIFT, calc_key_hash,
};
use wind_store::Store;
use wind_store::stat_collector::{StatCollector, StatEvent};
use wind_store::stats::CommitSource;
use wind_transform::fullwidth::to_full_width;
use wind_transform::punctuation::PunctuationConverter;
use wind_ui::candidate_window::CandidateItem;
use wind_ui::manager::{UiCommand, UiEvent, UiManager};
use wind_ui::toast::{ToastKind, ToastPosition};

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

pub(crate) fn punct_char(key_code: u32, shift: bool) -> Option<char> {
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

/// 小键盘键码 → 字符（数字 0-9 / 运算符 * + - / / 小数点 .）。非小键盘键返回 None。
pub(crate) fn numpad_char(key_code: u32) -> Option<char> {
    match key_code {
        0x60..=0x69 => Some((b'0' + (key_code - 0x60) as u8) as char),
        0x6A => Some('*'),
        0x6B => Some('+'),
        0x6D => Some('-'),
        0x6E => Some('.'),
        0x6F => Some('/'),
        _ => None,
    }
}

/// 英文输入大小写模式（临时英文候选适配用，对齐 Go detectCasePattern）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnCase {
    Lower,
    Upper,
    Title,
    Mixed,
}

/// 检测缓冲的大小写模式（仅看字母）。
pub(crate) fn detect_en_case(s: &str) -> EnCase {
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
pub(crate) fn adapt_en_case(word: &str, case: EnCase) -> String {
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
pub(crate) fn printable_char(key_code: u32, shift: bool) -> Option<char> {
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
pub(crate) const ENGINE_MAX_CANDIDATES: usize = 50;

/// 自动造词（L）写入临时层的初始权重与每次复选增量（保守默认；后续可接 schema.learning 配置）。
pub(crate) const LEARN_ADD_WEIGHT: i32 = 800;
pub(crate) const LEARN_WEIGHT_DELTA: i32 = 40;

/// 当前 unix 秒（拼音衰减分以此对 last_used 计龄；与 store record_freq 同口径）。
pub(crate) fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 协调器输入状态
/// 检索范围过滤模式（与 Go config.FilterMode 对齐）：(模式, 菜单显示名)
pub(crate) const FILTER_MODES: [(wind_candidate::FilterMode, &str); 3] = [
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

/// 「设置」菜单的网页配置 URL 提供者：由 main 注入（捕获 web_state 的 Weak 句柄，
/// 调用时签发 token 构造 URL）。本 crate 仅持有闭包、不依赖 wind-webapi，保持解耦；
/// 返回 None 表示未注入或 web 服务尚未就绪。
#[allow(clippy::type_complexity)]
static SETTINGS_URL_PROVIDER: std::sync::OnceLock<Box<dyn Fn() -> Option<String> + Send + Sync>> =
    std::sync::OnceLock::new();

/// 注入「设置」网页配置 URL 提供者（main 在启动 web 服务后调用一次）。
pub fn set_settings_url_provider(f: Box<dyn Fn() -> Option<String> + Send + Sync>) {
    let _ = SETTINGS_URL_PROVIDER.set(f);
}

/// 取「设置」网页配置 URL（None=未注入或服务未就绪）。
pub(crate) fn settings_url() -> Option<String> {
    SETTINGS_URL_PROVIDER.get().and_then(|f| f())
}

/// 取同目录下 wind_setting 设置应用的可执行路径（None=不存在）。
/// 由当前 exe 名推导变体：wind_input[_debug].exe → wind_setting[_debug].exe，
/// 故无需感知 debug_variant 特性，正式/调试版自动对应。
pub(crate) fn settings_app_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_str()?; // wind_input 或 wind_input_debug
    let setting = stem.replacen("wind_input", "wind_setting", 1);
    let path = dir.join(format!("{setting}.exe"));
    path.exists().then(|| path.display().to_string())
}

pub(crate) struct State {
    pub(crate) chinese_mode: bool,
    pub(crate) full_width: bool,
    pub(crate) chinese_punct: bool,
    /// 简繁转换开关（运行时切换；commit 时把简体输出转繁体）
    pub(crate) s2t_enabled: bool,
    /// 检索范围过滤模式（smart/general/gb18030；运行时切换）
    pub(crate) filter_mode: wind_candidate::FilterMode,
    /// 用户是否开启常驻工具栏（菜单开关；与“当前是否激活”正交）。
    pub(crate) toolbar_visible: bool,
    /// 本输入法当前是否处于激活态：IME_ACTIVATED/FocusGained 置真；
    /// IME_DEACTIVATED（切换输入法）与 FocusLost（失焦，含“每应用独立输入法”下切到
    /// 别的输入法的应用）置假。工具栏仅在激活态显示，对齐 Go toolbar_reducer 的
    /// `imeActivated && userWantsVisible` 公式；隐藏经 UI 层 50ms 防抖消除切换闪烁。
    pub(crate) ime_active: bool,
    pub(crate) caps_lock: bool,
    pub(crate) input_buffer: String,
    /// 组合区显示文本（拼音含音节分隔 "ni hao"；码表为原始编码）。
    /// 仅显示输入码/拼音，绝不包含候选列表。
    pub(crate) preedit: String,
    pub(crate) candidates: Vec<Candidate>,
    /// 当前页内高亮候选下标（0-based，相对当前页）——键盘选中项，空格上屏的目标
    pub(crate) selected_index: usize,
    /// 鼠标悬停目标（原始 tag）：-1 无，0..N 候选页内下标，或翻页器 tag。
    /// 与 selected_index 相互独立：悬停只是视觉提示，不改变空格上屏的目标。
    pub(crate) hover_index: i32,
    /// 当前页码（0-based）
    pub(crate) current_page: usize,
    /// 动态分级加载：当前候选对应的输入码
    pub(crate) candidate_input: String,
    /// 动态分级加载：当前加载上限
    pub(crate) candidate_limit: usize,
    /// 动态分级加载：是否可能还有更多前缀候选未加载
    pub(crate) has_more: bool,
    /// 拼音类组合区「已转换前缀」（逐步转换：选中的汉字累积于此、留在组合区不上屏，
    /// 全部转换完才整体上屏）。内部存简体原文，输出时再 s2t。仅拼音/临拼/混输文本透镜使用，
    /// 码表（五笔）选词消费整串、绝不进入此态。见 docs/redesign/pinyin-composition-enhance.md。
    pub(crate) committed_text: String,
    /// 已转换前缀的分段记录 (消费码, 汉字)：供退格逐段回退与完整上屏时自动造词。
    pub(crate) committed_segs: Vec<(String, String)>,
    /// 当前激活的独占输入模式（临时拼音/快捷输入/临时英文）。`None` = 普通输入。
    /// 单点决策的唯一真相源：结构上保证同一时刻至多一个独占模式（见 `pipeline.rs`）。
    pub(crate) active: Option<ModeKind>,
    /// 临时拼音输入缓冲（拼音串）
    pub(crate) temp_pinyin_buffer: String,
    /// 临时拼音目标方案 id（如 "pinyin"）
    pub(crate) temp_pinyin_schema: String,
    /// 临时拼音组合区前缀字符（触发键，如 "`"）
    pub(crate) temp_pinyin_prefix: String,
    /// 融合「快捷」含 quick_input 成员时「强制竖排」记住进入前的布局（Some(原 vertical) = 已强制，退出恢复）。
    pub(crate) quick_saved_vertical: Option<bool>,
    /// 临时英文输入缓冲
    pub(crate) temp_english_buffer: String,
    /// 网址模式输入缓冲（原样累积的 URL 文本）
    pub(crate) url_buffer: String,
    /// 统一夺取回退登记（仅在夺取式模式激活时为 Some，见 pipeline::Rewind）
    pub(crate) rewind: Option<Rewind>,
    /// 特殊模式编码缓冲（自带码表的查询码）
    pub(crate) special_buffer: String,
    /// 当前特殊模式下标（= features.special_modes 索引；仅 active==Special 时有效）
    pub(crate) special_id: u8,
    /// 特殊模式显示态前缀（进入键符号，如 "\"；只显示不消费，组合区前缀，对齐临时拼音）
    pub(crate) special_prefix: String,
    /// 临时 mix 编码缓冲
    pub(crate) mix_buffer: String,
    /// mix 模式显示态前缀（进入键符号，如 ";"；只显示不消费，组合区前缀）
    pub(crate) mix_prefix: String,
    /// 当前 mix 模式下标（= features.mix_modes 索引；仅 active==Mix 时有效）
    pub(crate) mix_id: u8,
    /// mix 数字模式（仅含 quick_input 成员时有效）：首字符数字/符号 → true（表达式：数字/符号
    /// 输入、字母选词）；首字符字母 → false（拼音/英文：字母输入、数字选词）。
    pub(crate) mix_numeric: bool,
    pub(crate) caret_x: i32,
    pub(crate) caret_y: i32,
    pub(crate) caret_height: i32,
    /// 菜单是否打开（打开时键盘事件转发给菜单窗口；UI 自管导航）
    pub(crate) menu_open: bool,
    /// 菜单目标候选（页内下标 + 文本），供候选词条操作/复制
    pub(crate) menu_target_page_local: usize,
    pub(crate) menu_target_text: String,
    /// 快捷加词模式（对齐 Go addWordState）：候选窗内从最近上屏字符选字组词加入用户词库。
    /// 与 `active`（独占输入模式）正交：加词模式不处理编码输入，仅 ↑↓ 调词长 / Enter 确认。
    pub(crate) add_word_active: bool,
    /// 加词候选字符池（最近上屏字符，时间序：旧→新，末尾为最近一字）。
    pub(crate) add_word_chars: Vec<char>,
    /// 当前选取的词长（取 `add_word_chars` 末尾 N 字；0 = 无可用字符）。
    pub(crate) add_word_len: usize,
    /// 当前词自动计算的编码（拼音生成 / 码表反查；空 = 无法计算，确认时中止）。
    pub(crate) add_word_code: String,
    /// 加词模式「强制竖排」时记住进入前的布局（Some(原 vertical) = 已强制，退出恢复）。
    pub(crate) add_word_saved_vertical: Option<bool>,
}

/// 智能符号模式待命态：press1 提交一个参与集合内的中文标点后武装，等待时限内同键 press2
/// 触发替换。对齐 Go `smartSymbol*` 字段。
#[derive(Default)]
pub(crate) struct SmartSymbolArm {
    pub(crate) armed: bool,
    /// 武装的触发键（原始英文标点字符）
    pub(crate) key: char,
    /// press1 产出的中文标点串（…… 为多 rune），删除数 = 其 rune 数
    pub(crate) str: String,
    /// 武装时刻（None=未武装）；用于时限判定
    pub(crate) at: Option<std::time::Instant>,
}

/// 配置 + 其轻量派生缓存的不可变快照；运行时整体原子替换以支持热重载。
/// 重型组件（引擎/方案/词典）不在内，仍需重启才能完全切换。
pub(crate) struct ConfigBundle {
    pub(crate) config: Config,
    pub(crate) compiled_hotkeys: CompiledHotkeys,
    pub(crate) nav_keys: keymap::NavKeys,
    pub(crate) cn_pairs: Vec<(char, char)>,
    pub(crate) en_pairs: Vec<(char, char)>,
}

impl ConfigBundle {
    fn build(config: Config) -> Self {
        let compiled_hotkeys = hotkey::Compiler::new(config.clone()).compile();
        let nav_keys =
            keymap::NavKeys::from_config(&config.keys.page_keys, &config.keys.highlight_keys);
        let cn_pairs = parse_pairs(&config.input.auto_pair.chinese_pairs);
        let en_pairs = parse_pairs(&config.input.auto_pair.english_pairs);
        Self {
            config,
            compiled_hotkeys,
            nav_keys,
            cn_pairs,
            en_pairs,
        }
    }
}

/// 中央协调器
pub struct Coordinator {
    pub(crate) state: Mutex<State>,
    pub(crate) push_server: Arc<PushServer>,
    /// 配置 + 轻量派生缓存快照（RwLock<Arc<>> 原子替换支持热重载）。
    /// 访问统一经 `self.rt()`。
    rt: std::sync::RwLock<std::sync::Arc<ConfigBundle>>,
    pub(crate) ui_tx: std::sync::mpsc::Sender<UiCommand>,
    pub(crate) engine_mgr: EngineManager,
    /// redb 持久化存储（用户词/临时词/词频/影子规则）；None=无持久化（headless 测试）。
    pub(crate) store: Option<Arc<Store>>,
    /// 标点转换器（引号左右状态）
    pub(crate) punct: Mutex<PunctuationConverter>,
    /// 智能符号模式待命态（同键连按删中文标点改英文）
    pub(crate) smart_symbol: Mutex<SmartSymbolArm>,
    /// 短语层（system.phrases.toml；$Y$M$D 模板）
    pub(crate) phrases: wind_phrase::PhraseLayer,
    /// 简繁转换器（OpenCC；None=数据缺失不可用）。变体由配置 features.s2t.variant 决定，
    /// 启动时加载；菜单仅提供开/关。置于 Mutex 兼容 reload 时整体替换。
    pub(crate) s2t: Mutex<Option<wind_transform::s2t::Converter>>,
    /// 通用规范汉字表（检索范围"常用字"判定；空集时退化为不过滤）
    pub(crate) common_chars: wind_candidate::CommonChars,
    // Shadow 规则已迁至 redb（self.store 的 SHADOW 表）。
    /// 工具栏位置持久化文件路径（toolbar_pos.txt；None=不持久化）
    pub(crate) toolbar_pos_path: Option<std::path::PathBuf>,
    /// 候选反查（编码/拆字/拼音）供悬停提示
    pub(crate) reverse: wind_reverse::ReverseLookup,
    /// 标点配对跟踪栈（用于智能跳过）；中/英配对表在 rt bundle 内。
    pair_tracker: Mutex<wind_transform::pair_tracker::PairTracker>,
    /// 最近一次有效光标坐标 (x,y,height)；用于无效坐标时回退，避免候选窗跑到左上角
    last_valid_caret: Mutex<(i32, i32, i32)>,
    /// 正在等待有效光标坐标（首次连接尚未拿到时为 true）；拿到后触发重定位
    awaiting_caret: Mutex<bool>,
    /// 主题目录（data/themes）
    pub(crate) themes_dir: Option<std::path::PathBuf>,
    /// 当前主题名
    pub(crate) theme_name: Mutex<String>,
    /// 主题暗色模式
    pub(crate) theme_dark: Mutex<bool>,
    /// 主题选择持久化文件（theme.txt）
    pub(crate) theme_path: Option<std::path::PathBuf>,
    /// 命令栏（cmdbar）服务束（ime/config/dict 等动作后端），构造后由 init_cmdbar 装配。
    pub(crate) cmdbar_services: std::sync::OnceLock<wind_cmdbar::Services>,
    /// 自身 Weak 引用：$CC 命令在独立线程异步执行（避免持 state 锁回调自锁方法致死锁）。
    pub(crate) self_weak: std::sync::OnceLock<std::sync::Weak<Coordinator>>,
    /// 上屏历史环形缓冲（index 0 = 最近）：供命令栏 `last(n)` 取最近上屏文本。
    pub(crate) recent_commits: Mutex<std::collections::VecDeque<String>>,
    /// 编码显示方式运行时态（命令栏 ime.toggle("preedit") 循环切换；初值随配置）。
    /// 统一权威：决定候选窗是否显示 preedit（in_app→不显示）及是否内联首单元（embedded）。
    pub(crate) preedit_display: Mutex<PreeditDisplay>,
    /// 候选窗隐藏开关（命令栏 ime.toggle("candwin") 切换；隐藏时 notify_ui_update 不显示候选）。
    hide_candidate_window: Mutex<bool>,
    /// 候选布局方向运行时态（命令栏 ime.toggle("layout") 切换；true=竖排，初值随配置，持久化）。
    candidate_vertical: Mutex<bool>,
    /// 输入统计采集器（内存聚合 + 后台 flush，与 store 共享 Arc）；None=无持久化/headless。
    pub(crate) stat_collector: Option<StatCollector>,
    /// 本次按键是否已被具体上屏路径记录统计（AtomicBool，避免与 state 锁冲突致死锁）。
    pub(crate) stat_recorded: std::sync::atomic::AtomicBool,
}

/// 短语候选权重基准（高于普通候选，使短语展开排在前列）
pub(crate) const PHRASE_WEIGHT_BASE: i32 = 40_000_000;

/// 一次候选刷新后的输入结局（码表全码/空码策略，仅正向输入字母时消费）。
pub(crate) enum InputOutcome {
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

        // 后台预热：提前构建其余方案的引擎与缓存（拼音 merged/unigram、码表 per-dict），
        // 避免首次切换到拼音/临时拼音/码表时同步重熔大词库造成几十秒卡顿。
        // single-flight 构建锁保证预热与用户切换不重复构建；按方案顺序逐个建（后台低频）。
        {
            let c = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                let active = c.engine_mgr.active_schema_id();
                for id in c.engine_mgr.available_schemas() {
                    if id == active || c.engine_mgr.is_loaded(&id) {
                        continue;
                    }
                    let t0 = std::time::Instant::now();
                    if c.engine_mgr.prewarm_schema(&id) {
                        debug!("Prewarmed schema {} in {:?}", id, t0.elapsed());
                    } else {
                        debug!("Prewarm skipped/failed for schema {}", id);
                    }
                }
                debug!("Schema prewarm done");
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
        let vertical = coordinator
            .rt()
            .config
            .ui
            .candidate
            .layout
            .eq_ignore_ascii_case("vertical");
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetCandidateLayout(vertical));
        // 下发预编辑内联模式：仅 candidate_inline 需内联候选首单元（app_inline 不显示、candidate_top 独立条）。
        let embedded = coordinator.rt().config.ui.candidate.preedit().embedded();
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetPreeditEmbedded(embedded));
        // 候选字号覆盖 + 悬停提示延迟初值
        let rt0 = coordinator.rt();
        let _ = coordinator.ui_tx.send(UiCommand::SetCandidateFontSize(
            rt0.config.ui.candidate.font_size,
        ));
        let _ = coordinator.ui_tx.send(UiCommand::SetCandidateFlipWhenAbove(
            rt0.config.ui.candidate.flip_when_above,
        ));
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetTooltipDelay(rt0.config.ui.tooltip.delay));
        // 拆字字根字体（PUA 字根渲染）：随安装数据目录下发，存在才发。
        if let Some(dir) = data_dir.as_deref() {
            let font = dir.join("schemas/wubi86/HeiTiZiGen.ttf");
            if font.is_file() {
                let _ = coordinator.ui_tx.send(UiCommand::SetTooltipChaiziFont(
                    font.to_string_lossy().into_owned(),
                ));
            }
        }
        // 统一应用外观项（幂等）：补齐上面手动块未含的候选字体族 / 翻页栏 / 页码 / 字号跟随主题，
        // 使首次启动即按 config 应用（与 reload_user_config 同一路径）。
        coordinator.apply_ui_config();
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
        Self::build(config, data_dir, push_server, ui_tx, None, None)
    }

    /// 无头 + 注入 redb store（测试用）：用于 web_data_rpc 数据域契约测试。
    pub fn new_headless_with_store(
        config: Config,
        data_dir: Option<&Path>,
        store: Arc<Store>,
    ) -> Arc<Self> {
        let (ui_tx, _rx) = std::sync::mpsc::channel();
        drop(_rx);
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        Self::build(config, data_dir, push_server, ui_tx, None, Some(store))
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
        // 配置的轻量派生缓存集中到 ConfigBundle（支持运行时热替换）。
        let bundle = ConfigBundle::build(config.clone());
        info!(
            "Compiled hotkeys: {} key_down, {} key_up",
            bundle.compiled_hotkeys.key_down.len(),
            bundle.compiled_hotkeys.key_up.len()
        );

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
        let s2t_variant = if config.input.s2t.variant.is_empty() {
            "s2t".to_string()
        } else {
            config.input.s2t.variant.clone()
        };
        let s2t = opencc_dir.as_ref().and_then(|dir| {
            let conv = wind_transform::s2t::Converter::load_variant(dir, &s2t_variant);
            if conv.is_some() {
                info!("Loaded S2T converter (variant={})", s2t_variant);
            }
            conv
        });

        // 词频已迁 redb（self.store 的 FREQ 表，选词时 record_freq）。

        // 标点配对表（中/英）已在 ConfigBundle 内构建。

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
        let reverse = wind_reverse::ReverseLookup::load(data_dir);
        if !reverse.is_empty() {
            info!("Loaded reverse-lookup (chaizi/pinyin)");
        }

        // Shadow 规则已迁至 redb（self.store 的 SHADOW 表，事务持久），不再用 shadow.json。
        // 工具栏位置属本机状态（不随漫游）：放 %LOCALAPPDATA%\WindInput
        let toolbar_pos_path = Config::local_dir().map(|d| d.join("toolbar_pos.txt"));
        let theme_path = user_dir.as_ref().map(|d| d.join("theme.txt"));
        let themes_dir = data_dir.map(|d| d.join("themes"));
        // 初始主题名：config.ui.theme.name（设置页/右键统一写此为准）> theme.txt（旧版迁移回退）> "default"
        let cfg_theme = config.ui.theme.name.trim();
        let initial_theme = if !cfg_theme.is_empty() {
            cfg_theme.to_string()
        } else {
            theme_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "default".to_string())
        };
        // 初始明暗：config.ui.theme.style == "dark"(否则亮/跟随系统按亮处理)。修启动不应用风格。
        let theme_dark_init = config.ui.theme.style.eq_ignore_ascii_case("dark");

        // 标点转换器：注入自定义映射（四状态）。
        let mut punct_conv = PunctuationConverter::new();
        punct_conv.set_custom_mappings(
            config.input.punct.custom_enabled,
            config.input.punct.custom_mappings.clone(),
        );

        // 编码显示方式运行时初值（config 移入结构体前先算）。
        let preedit_display_init = config.ui.candidate.preedit();

        // 候选布局方向运行时初值（与下方 SetCandidateLayout 下发一致；config 移入前先算）。
        let candidate_vertical_init = config.ui.candidate.layout.eq_ignore_ascii_case("vertical");

        // 候选窗显隐运行时初值（ui.candidate.hide_window；此前恒为 false，配置不生效）。
        let hide_candidate_window_init = config.ui.candidate.hide_window;

        // 统计采集器：与 store 共享 Arc，内存聚合 + 后台定时 flush。
        let stat_collector = store.clone().map(StatCollector::new);
        let coordinator = Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: config.input.default.chinese_mode,
                full_width: config.input.default.full_width,
                chinese_punct: config.input.default.chinese_punct,
                s2t_enabled: config.input.s2t.enabled,
                filter_mode: wind_candidate::FilterMode::from_str(&config.input.filter_mode),
                toolbar_visible: config.ui.toolbar.visible, // 启动初值来自配置(运行时可菜单切换)
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
                quick_saved_vertical: None,
                temp_english_buffer: String::new(),
                url_buffer: String::new(),
                rewind: None,
                special_buffer: String::new(),
                special_id: 0,
                special_prefix: String::new(),
                mix_buffer: String::new(),
                mix_id: 0,
                mix_prefix: String::new(),
                mix_numeric: false,
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
                menu_open: false,
                menu_target_page_local: 0,
                menu_target_text: String::new(),
                add_word_active: false,
                add_word_chars: Vec::new(),
                add_word_len: 0,
                add_word_code: String::new(),
                add_word_saved_vertical: None,
            }),
            push_server,
            rt: std::sync::RwLock::new(std::sync::Arc::new(bundle)),
            ui_tx,
            engine_mgr,
            store,
            punct: Mutex::new(punct_conv),
            smart_symbol: Mutex::new(SmartSymbolArm::default()),
            phrases,
            s2t: Mutex::new(s2t),
            common_chars,
            toolbar_pos_path,
            reverse,
            pair_tracker: Mutex::new(wind_transform::pair_tracker::PairTracker::new()),
            last_valid_caret: Mutex::new((0, 0, 0)),
            awaiting_caret: Mutex::new(false),
            themes_dir,
            theme_name: Mutex::new(initial_theme),
            theme_dark: Mutex::new(theme_dark_init),
            theme_path,
            cmdbar_services: std::sync::OnceLock::new(),
            self_weak: std::sync::OnceLock::new(),
            recent_commits: Mutex::new(std::collections::VecDeque::new()),
            preedit_display: Mutex::new(preedit_display_init),
            hide_candidate_window: Mutex::new(hide_candidate_window_init),
            candidate_vertical: Mutex::new(candidate_vertical_init),
            stat_collector,
            stat_recorded: std::sync::atomic::AtomicBool::new(false),
        });
        // 命令栏：装配 Services（ime/config/dict 后端）+ 自身 Weak 引用。
        coordinator.init_cmdbar();
        // 启动即显示常驻工具栏（反映初始 中英/方案/标点/全半角）
        coordinator.notify_toolbar();
        coordinator
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

    /// 取当前「配置 + 派生缓存」快照（Arc 克隆，开销低）。所有配置读取经此。
    pub(crate) fn rt(&self) -> std::sync::Arc<ConfigBundle> {
        self.rt.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 热重载用户配置：从磁盘重读 Config 并原子替换 bundle（轻量设置即时生效），
    /// 再 best-effort 刷新主题/工具栏。返回是否仍需重启才能完全生效。
    /// 轻量项（标点/智能符号/候选数/热键/配对/导航键等）即时生效；重型项（引擎/方案/
    /// 词典/字体）当前不在 bundle 内，需重启——为不打断使用，这里统一返回 false，
    /// 由调用方/用户按需重启。
    pub fn reload_user_config(&self) -> bool {
        match Config::load(Config::data_dir().as_deref()) {
            Ok(cfg) => {
                // 方案相关项（活跃/可用方案、全局上屏策略）是否变化：变了才热重建引擎，
                // 避免每次保存都丢词典缓存（拼音合并/unigram 重建开销大）。
                let old = self.rt();
                let schema_dirty = old.config.schema != cfg.schema
                    || old.config.input.code_commit != cfg.input.code_commit;
                drop(old);

                let bundle = std::sync::Arc::new(ConfigBundle::build(cfg));
                let new_cfg = bundle.config.clone();
                *self.rt.write().unwrap_or_else(|e| e.into_inner()) = bundle;
                info!("User config hot-reloaded (schema_dirty={})", schema_dirty);

                if schema_dirty {
                    // 热重建方案集：清输入缓冲、刷新工具栏/状态，免重启切换方案。
                    self.engine_mgr.reload_from_config(&new_cfg);
                    {
                        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        s.input_buffer.clear();
                        s.candidates.clear();
                        s.preedit.clear();
                    }
                    self.notify_ui_hide();
                    self.push_state_update();
                    self.notify_toolbar(); // 方案名变化 → 刷新工具栏标签
                }
                // 同步主题选择:设置页改 config.ui.theme.* 后内存态须跟随,reload_config 才会下发新主题
                // (此前 reload_config 只重推旧内存主题 → 设置页切主题不生效)。
                {
                    let name = new_cfg.ui.theme.name.trim();
                    if !name.is_empty() {
                        *self.theme_name.lock().unwrap_or_else(|e| e.into_inner()) =
                            name.to_string();
                    }
                    *self.theme_dark.lock().unwrap_or_else(|e| e.into_inner()) =
                        new_cfg.ui.theme.style.eq_ignore_ascii_case("dark");
                }
                // 同步工具栏显隐:设置页改 ui.toolbar.visible 后运行时态跟随,再刷新工具栏。
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.toolbar_visible = new_cfg.ui.toolbar.visible;
                }
                self.apply_ui_config(); // 外观项（候选排列/编码显示/候选窗显隐）即时生效
                self.reload_config(); // 刷新主题/工具栏（候选窗下次输入按新配置）
                self.notify_toolbar(); // 工具栏显隐(visible/全屏)按新配置即时刷新
                self.show_toast(
                    "设置已更新",
                    ToastPosition::BottomCenter,
                    ToastKind::Success,
                );
                false
            }
            Err(e) => {
                tracing::error!("热重载配置失败: {}", e);
                self.show_toast(
                    "配置加载失败",
                    ToastPosition::BottomCenter,
                    ToastKind::Error,
                );
                true
            }
        }
    }

    /// 显示一次性通知 toast（约 2.5 秒后自动隐藏）。供配置热重载、词库就绪、错误等一次性事件。
    pub(crate) fn show_toast(&self, text: &str, position: ToastPosition, kind: ToastKind) {
        let _ = self.ui_tx.send(UiCommand::ShowToast {
            text: text.to_string(),
            position,
            kind,
            duration_ms: 2500,
        });
    }

    /// 按当前配置（bundle）重新下发外观相关 UI 指令并同步运行时态。
    /// 热重载用：候选排列方向 / 编码显示方式 / 候选窗显隐 改动即时生效（无需重启）。
    /// 与命令栏 ime.toggle 共写同一组运行时 Mutex；以 config 为准重置（config 为持久化真相源）。
    pub(crate) fn apply_ui_config(&self) {
        let bundle = self.rt();
        let cand = &bundle.config.ui.candidate;
        // 候选排列方向（ui.candidate.layout == "vertical"）
        let vertical = cand.layout.eq_ignore_ascii_case("vertical");
        *self
            .candidate_vertical
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = vertical;
        let _ = self.ui_tx.send(UiCommand::SetCandidateLayout(vertical));
        // 编码显示方式（ui.candidate.preedit_display）
        let mode = cand.preedit();
        *self
            .preedit_display
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mode;
        let _ = self
            .ui_tx
            .send(UiCommand::SetPreeditEmbedded(mode.embedded()));
        // 候选窗显隐（ui.candidate.hide_window）
        let hidden = cand.hide_window;
        *self
            .hide_candidate_window
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = hidden;
        if hidden {
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
        }
        // 候选字号覆盖（ui.candidate.font_size，0=跟随主题）；font_size_follow_theme=true 时强制跟随。
        let font_size = if cand.font_size_follow_theme {
            0.0
        } else {
            cand.font_size
        };
        let _ = self.ui_tx.send(UiCommand::SetCandidateFontSize(font_size));
        // 候选字体族（ui.font.family；空=默认）。
        let _ = self.ui_tx.send(UiCommand::SetCandidateFontFamily(
            bundle.config.ui.font.family.clone(),
        ));
        // 翻页栏 / 页码显示覆盖（ui.candidate.pager_bar_display / page_number_display）
        let _ = self
            .ui_tx
            .send(UiCommand::SetPagerDisplay(cand.pager_bar_display.clone()));
        let _ = self.ui_tx.send(UiCommand::SetPageNumberDisplay(
            cand.page_number_display.clone(),
        ));
        // 上方时反转候选顺序（ui.candidate.flip_when_above）
        let _ = self
            .ui_tx
            .send(UiCommand::SetCandidateFlipWhenAbove(cand.flip_when_above));
        // 悬停提示延迟（ui.tooltip.delay）
        let _ = self
            .ui_tx
            .send(UiCommand::SetTooltipDelay(bundle.config.ui.tooltip.delay));
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

    /// 每页候选数（来自配置，至少 1）
    pub(crate) fn per_page(&self, active: Option<ModeKind>) -> usize {
        let bundle = self.rt();
        let cand = &bundle.config.ui.candidate;
        // overlay 模式(临拼/快捷/短语/临英等,state.active 非空)用扩展档(配置>0 时)。
        if active.is_some() && cand.per_page_extended > 0 {
            cand.per_page_extended.max(1)
        } else {
            cand.per_page.max(1)
        }
    }

    /// 总页数（至少 1）
    fn total_pages(&self, state: &State) -> usize {
        let pp = self.per_page(state.active);
        state.candidates.len().div_ceil(pp).max(1)
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
    pub(crate) fn apply_nav_key(
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
            .rt()
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

    // ───────────────────────── 临时拼音 ─────────────────────────

    // ───────────────────────── 快捷输入 ─────────────────────────

    // ───────────────────────── 临时英文 ─────────────────────────

    // ───────────────────────── 特殊模式 ─────────────────────────

    // ───────────────────────── 临时 mix 模式 ─────────────────────────

    /// 取出并清空「已转换前缀」（简体），用于非选词的终结性上屏（回车/空格上屏原码/标点键）。
    /// 码表模式恒为空串，无副作用。
    pub(crate) fn take_committed(&self, state: &mut State) -> String {
        state.committed_segs.clear();
        std::mem::take(&mut state.committed_text)
    }

    /// 清空拼音逐步转换的组合态（已转换前缀 + 缓冲 + 候选）。
    pub(crate) fn reset_pinyin_composition(&self, state: &mut State) {
        state.committed_text.clear();
        state.committed_segs.clear();
        state.input_buffer.clear();
        state.preedit.clear();
        state.candidates.clear();
        state.current_page = 0;
        state.selected_index = 0;
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
                warn!(
                    "ime.toggle: 暂不支持 target {:?}（Rust 平台能力待补）",
                    other
                )
            }
        }
    }

    /// 循环切换编码显示方式（内嵌应用 → 候选顶部 → 候选内联 → ...），下发 UI 并持久化。
    fn cmd_toggle_preedit(&self) {
        let mode = {
            let mut m = self
                .preedit_display
                .lock()
                .unwrap_or_else(|x| x.into_inner());
            *m = m.next();
            *m
        };
        // 候选窗内联标志（仅 candidate_inline 为 true）；in_app 由 notify_ui_update 读运行时态门控。
        let _ = self
            .ui_tx
            .send(UiCommand::SetPreeditEmbedded(mode.embedded()));
        // 持久化到用户层 ui.candidate.preedit_display（重启后保留）。
        if let Err(e) =
            Config::set_user_string(&["ui", "candidate", "preedit_display"], mode.as_config())
        {
            warn!("ime.toggle preedit: 持久化失败: {}", e);
        }
        self.show_tip(mode.label());
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
        self.show_tip(if hidden {
            "候选窗:隐藏"
        } else {
            "候选窗:显示"
        });
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
        self.show_tip(if vertical {
            "候选:竖排"
        } else {
            "候选:横排"
        });
    }

    pub(crate) fn notify_ui_update(&self, state: &State) {
        // 模式指示标记（拼/双/快/英/符）：仅在候选为空时显示（进入模式/无候选阶段），
        // 一旦有候选即隐藏，减少干扰。必须纳入下方"空则隐藏"守卫——否则进入模式时
        // 缓冲为空会直接隐藏，标记发不出。
        let mode_label = if state.candidates.is_empty() {
            self.mode_indicator_text(state).unwrap_or_default()
        } else {
            String::new()
        };
        if state.candidates.is_empty() && state.input_buffer.is_empty() && mode_label.is_empty() {
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
        // 数字键需录入表达式的场景用字母标签（a/b/c）选词：mix 的数字模式（含 quick_input 成员）。
        let alpha = matches!(state.active, Some(ModeKind::Mix(_))) && state.mix_numeric;
        // 悬停提示/候选微调配置（热重载快照）
        let rt = self.rt();
        let cand_cfg = &rt.config.ui.candidate;
        let tip_cfg = &rt.config.ui.tooltip;
        // 命令直通车候选前缀标注（features.cmdbar.candidate_prefix）：仅命令候选(is_command)显示。
        let cmd_prefix = rt.config.input.cmdbar.candidate_prefix.as_str();
        // 编码提示(反查):对拼音来源候选,用主码表真实反查索引填 comment(实际填充见下方候选构造,
        // 受 source==Pinyin 守卫)。门控两类:
        //  - 普通拼音/混输方案:跟随方案 show_code_hint(pinyin_show_code_hint 解析,混输取次方案);
        //  - overlay 反查模式(临时拼音 / 快捷输入(mix)内拼音):**无视开关强制显示**
        //    (对齐 Go AddCodeHintsForced)——这些模式本身就是"用拼音反查码表编码",必须出码。
        // 码表类方案/候选的剩余编码由码表引擎在 convert 内填,不在此处理。
        let force_hint = matches!(
            state.active,
            Some(ModeKind::TempPinyin) | Some(ModeKind::Mix(_))
        );
        let pinyin_hint = force_hint || self.engine_mgr.pinyin_show_code_hint();
        let tip_opts = wind_reverse::TooltipOptions {
            code: tip_cfg.code_enabled,
            pinyin: tip_cfg.pinyin_enabled,
            heteronyms: tip_cfg.pinyin_heteronyms,
            max_readings: tip_cfg.pinyin_max_readings,
            chaizi: tip_cfg.chaizi_enabled,
        };
        let items: Vec<CandidateItem> = state.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // 反查提示用完整文本（截断只影响显示，不影响"如何输入"提示）
                let full = self.maybe_s2t(state, &c.text);
                let mut tooltip = self.reverse.tooltip_for(&full, &tip_opts);
                if tip_cfg.debug_enabled {
                    let dbg = debug_tooltip_section(c);
                    if !dbg.is_empty() {
                        if !tooltip.is_empty() {
                            tooltip.push('\n');
                        }
                        tooltip.push_str(&dbg);
                    }
                }
                if tooltip.is_empty() && !c.code.is_empty() {
                    tooltip = c.code.clone();
                }
                CandidateItem {
                    // 开启简繁时显示也转繁体（内部候选仍存简体，用于词频/匹配）；按 max_chars 截断显示。
                    // 命令候选加前缀标注（截断后再加,保证前缀不被截掉）。
                    text: {
                        let disp = cand_cfg.truncate_display(&full);
                        if c.is_command && !cmd_prefix.is_empty() {
                            format!("{cmd_prefix}{disp}")
                        } else {
                            disp
                        }
                    },
                    code: c.code.clone(),
                    label: if alpha {
                        ((b'a' + i as u8) as char).to_string()
                    } else {
                        cand_cfg.index_label(i)
                    },
                    tooltip,
                    // 编码提示:码表候选的剩余编码由码表引擎在 convert 时已填入 c.comment(故 !empty 时保留);
                    // 拼音候选用码表真实反查索引填(仅 text 在码表词库中存在才显示,不生成、不臆测)。
                    comment: if !c.comment.is_empty() {
                        c.comment.clone()
                    } else if pinyin_hint && c.source == wind_candidate::CandidateSource::Pinyin {
                        // 反查码:仅对**拼音来源**候选,用主码表反向索引取该词在码表里的**实际**
                        // 编码(不存在则不显示)。不按字生成码——生成码常与码表实际码不一致,会提示
                        // 出打不出的码。对齐 Go addCodeHintsFromCodetable(Source==Pinyin && 空 comment)。
                        // (码表来源候选的剩余编码已由码表引擎在 convert 时填入 c.comment。)
                        self.engine_mgr.codetable_reverse_hint(&c.text)
                    } else {
                        String::new()
                    },
                    no_index: false,
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
        // preedit 是否在候选窗显示，按运行时编码显示方式门控：app_inline 时组合串内嵌应用，
        // 候选窗不再重复显示 preedit；candidate_top/inline 时照常下发由候选窗渲染。
        let in_app = self
            .preedit_display
            .lock()
            .map(|m| m.in_app())
            .unwrap_or(true);
        let preedit = if in_app {
            String::new()
        } else {
            state.preedit.clone()
        };
        // mode_label 已在顶部计算（纳入空则隐藏守卫）：作为候选窗内联标记随候选窗一并显示。
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit,
            mode_label,
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

    pub(crate) fn notify_ui_hide(&self) {
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

    /// 切换检索范围（0 智能/1 常用字/2 全部字符），以新范围重过滤并刷新候选。
    pub(crate) fn set_filter_mode(&self, index: usize) {
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

    /// 影子规则：当前 code 是否对 word 有规则（置顶/删除），决定菜单"恢复默认"可用性。
    pub(crate) fn shadow_has_rule(&self, schema: &str, code: &str, word: &str) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        matches!(
            store.get_shadow_rules(schema, code),
            Ok(Some(rec))
                if rec.pinned.iter().any(|p| p.word == word) || rec.deleted.iter().any(|d| d == word)
        )
    }

    /// 当前模式下生效的配对表（按中/英标点 + 各自开关）
    fn active_pairs(&self, chinese_punct: bool) -> Option<Vec<(char, char)>> {
        let rt = self.rt();
        if chinese_punct {
            if rt.config.input.auto_pair.chinese {
                return Some(rt.cn_pairs.clone());
            }
        } else if rt.config.input.auto_pair.english {
            return Some(rt.en_pairs.clone());
        }
        None
    }

    /// 判断标点字符 `ch` 是否参与当前生效的自动配对（作为左符号或右符号）。
    /// 智能符号与自动配对互斥的判定依据（见 `smart_symbol_arm_str`）。
    pub(crate) fn is_auto_pair_char(&self, state: &State, ch: char) -> bool {
        match self.active_pairs(state.chinese_punct) {
            Some(pairs) => pairs.iter().any(|(l, r)| *l == ch || *r == ch),
            None => false,
        }
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
            key_down_hotkeys: self.rt().compiled_hotkeys.key_down_tsf_hashes(),
            key_up_hotkeys: self.rt().compiled_hotkeys.key_up_tsf_hashes(),
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

    pub(crate) fn push_state_update(&self) {
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

    /// 在当前光标下方显示状态提示气泡（中英/标点/全半角/方案切换）
    pub(crate) fn show_tip(&self, text: &str) {
        let bundle = self.rt();
        let si = &bundle.config.ui.status;
        // 禁用则完全不显示状态提示气泡。
        if !si.enabled {
            return;
        }
        let (x, y, caret_height) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.caret_x, s.caret_y, s.caret_height)
        };
        // 常驻(always)→ duration_ms=0(UI 不自动隐藏);否则按 duration 自动隐藏。对齐 Go display_mode。
        let duration_ms = if si.display_mode.eq_ignore_ascii_case("always") {
            0
        } else {
            si.duration.max(1) as u64
        };
        // 位置模式 fixed:用固定屏幕坐标 custom_x/custom_y;否则跟随光标(caret + offset)。
        let fixed = si.position_mode.eq_ignore_ascii_case("fixed");
        let _ = self.ui_tx.send(UiCommand::ShowStatusTip {
            text: text.to_string(),
            x,
            y,
            caret_height,
            offset_x: si.offset_x,
            offset_y: si.offset_y,
            duration_ms,
            fixed,
            fixed_x: si.custom_x,
            fixed_y: si.custom_y,
        });
    }

    /// 隐藏状态提示气泡（常驻模式失焦时调用）。
    pub(crate) fn hide_tip(&self) {
        let _ = self.ui_tx.send(UiCommand::HideStatusTip);
    }

    /// 常驻(always)模式且启用时,显示当前合成状态(激活/获焦时调用)。temp 模式不在此显示。
    pub(crate) fn show_persistent_status_if_always(&self) {
        let si = &self.rt().config.ui.status;
        if si.enabled && si.display_mode.eq_ignore_ascii_case("always") {
            self.show_tip(&self.status_indicator_text());
        }
    }

    /// 合成当前 IME 核心状态文本：方案/中英(+大写) · 标点 · [全角] · [繁]。
    /// 默认态省略（半角/简体不显示），减少干扰；标点总显示（。/.）。
    pub(crate) fn status_indicator_text(&self) -> String {
        let (chinese, punct_cn, full, s2t, caps) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.chinese_mode,
                s.chinese_punct,
                s.full_width,
                s.s2t_enabled,
                s.caps_lock,
            )
        };
        let mut parts: Vec<String> = Vec::new();
        // 方案 / 中英 / 大写锁定
        if caps {
            parts.push("A".into());
        } else if !chinese {
            parts.push("英".into());
        } else {
            let id = self.engine_mgr.active_schema_id();
            // short 样式优先图标短称(icon_label)，无则回退全名；对齐 Go schema_name_style。
            let short = self.rt().config.ui.status.schema_name_style == "short";
            let label = if short {
                let icon = self.engine_mgr.schema_icon_label(&id);
                if icon.is_empty() {
                    self.engine_mgr.schema_name(&id)
                } else {
                    icon
                }
            } else {
                self.engine_mgr.schema_name(&id)
            };
            parts.push(if label.is_empty() {
                "中".into()
            } else {
                label
            });
        }
        // 标点（总显示）
        parts.push(if punct_cn { "。".into() } else { ".".into() });
        // 全角（仅全角时）
        if full {
            parts.push("全".into());
        }
        // 繁（仅繁体时）
        if s2t {
            parts.push("繁".into());
        }
        parts.join(" ")
    }

    /// 显示合成的核心状态气泡（中英/标点/全半角/简繁/方案切换共用）。
    pub(crate) fn show_status(&self) {
        self.show_tip(&self.status_indicator_text());
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
                self.show_status();
                self.notify_toolbar();
                true
            }
            "toggle_punct" => {
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_punct = !s.chinese_punct;
                }
                self.push_state_update();
                self.show_status();
                self.notify_toolbar();
                true
            }
            "toggle_s2t" => {
                if self.s2t.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
                    self.show_toast(
                        "简繁数据缺失",
                        ToastPosition::BottomCenter,
                        ToastKind::Error,
                    );
                    return true;
                }
                {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_enabled = !s.s2t_enabled;
                }
                self.show_status();
                true
            }
            "open_settings" => {
                self.open_settings(None);
                true
            }
            _ => {
                debug!("Unhandled hotkey action: {}", action);
                false
            }
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
        let commit = has_pending && !chinese && self.rt().config.keys.commit_on_switch;
        let text = if commit {
            // 切到英文且配置上屏：把「已转换前缀 + 剩余原码」一并上屏。
            let prefix = self.take_committed(state);
            // 模式切换上屏：committed 段已在选词时记过，此处只记剩余原码（来源模式切换）。
            self.record_commit(
                &state.input_buffer,
                state.input_buffer.len() as u32,
                -1,
                CommitSource::ModeSwitch,
            );
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

impl Coordinator {
    /// 从一次按键的最终 KeyAction 提取上屏文本，按中文/英文字符埋点到每日统计。
    /// 受 `features.stats.enabled` 控制；`track_english` 关闭时不计英文。无 store 静默跳过。
    /// 记录一次上屏事件到统计采集器。各上屏路径在已知码长/候选位/来源时调用，
    /// 并置位 stat_recorded，使顶层 record_input_stats 跳过兜底（避免重复计数）。
    /// 对齐 Go `recordCommit`：track_english 仅作用于 TSF 英文路径（Rust 暂无），
    /// 普通上屏按 4 分类记录全部字符。
    pub(crate) fn record_commit(
        &self,
        text: &str,
        code_len: u32,
        candidate_pos: i32,
        source: CommitSource,
    ) {
        if text.is_empty() {
            return;
        }
        let collector = match self.stat_collector.as_ref() {
            Some(c) => c,
            None => return,
        };
        if !self.rt().config.stats.enabled {
            return;
        }
        let (chinese, english, punct, other) = wind_store::stats::classify_chars_full(text);
        collector.record(StatEvent {
            timestamp: chrono::Local::now(),
            chinese,
            english,
            punct,
            other,
            code_len,
            candidate_pos,
            schema_id: self.active_schema_id(),
            source,
        });
        self.stat_recorded
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 顶层兜底统计（对齐 Go `recordCommitFallback`）：若本次按键已被具体上屏路径
    /// 记录则跳过；否则按文本推测来源（含非 ASCII→候选，纯 ASCII→标点）记录，
    /// 码长/候选位未知置 0/-1。
    pub(crate) fn record_input_stats(&self, action: &KeyAction) {
        if self
            .stat_recorded
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let text = match action {
            KeyAction::InsertText { text, .. } => text.as_str(),
            KeyAction::InsertTextWithCursor { text, .. } => text.as_str(),
            _ => return,
        };
        if text.is_empty() {
            return;
        }
        let source = if text.chars().any(|ch| !ch.is_ascii()) {
            CommitSource::Candidate
        } else {
            CommitSource::Punctuation
        };
        self.record_commit(text, 0, -1, source);
    }
}

impl MessageHandler for Coordinator {
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

    fn handle_show_context_menu(&self, x: i32, y: i32) {
        self.show_main_menu(x, y);
    }

    fn handle_english_stats(&self, chars: u32, digits: u32, puncts: u32, spaces: u32) {
        // TSF 侧英文模式统计（对齐 Go RecordTSFEnglish）。
        // chars→english, digits+spaces→other（对齐 classify_chars_full 行为）, puncts→punct。
        let collector = match self.stat_collector.as_ref() {
            Some(c) => c,
            None => return,
        };
        let cfg = &self.rt().config.stats;
        if !cfg.enabled || !cfg.track_english {
            return;
        }
        if chars == 0 && digits == 0 && puncts == 0 && spaces == 0 {
            return;
        }
        collector.record(StatEvent {
            timestamp: chrono::Local::now(),
            chinese: 0,
            english: chars,
            punct: puncts,
            other: digits.saturating_add(spaces),
            code_len: 0,
            candidate_pos: -1,
            schema_id: self.active_schema_id(),
            source: CommitSource::TsfDirect,
        });
    }

    fn preedit_uses_placeholder(&self) -> bool {
        // 非 app_inline（候选窗自显 preedit）→ 应用侧用占位空格，不重复显示编码。
        self.preedit_display
            .lock()
            .map(|m| !m.in_app())
            .unwrap_or(false)
    }

    /// bridge 真正入口：在按键处理之上统一埋点输入统计（上屏文本字符数），
    /// 再做 preedit 占位后处理。集中在此避免修改 40+ 个 commit 返回点（对齐旧 Go
    /// HandleKeyEvent 末尾的 recordCommitFallback 思路）。
    fn handle_key_event_policed(&self, data: &KeyEventData) -> KeyAction {
        let action = self.handle_key_event(data);
        self.record_input_stats(&action);
        if self.preedit_uses_placeholder() {
            action.with_composition_placeholder()
        } else {
            action
        }
    }

    fn handle_key_event(&self, data: &KeyEventData) -> KeyAction {
        // 每次按键开始重置统计标志：具体上屏路径调 record_commit 置位，
        // 顶层 record_input_stats 仅在未置位时兜底（对齐 Go handle_key_event 开头 reset）。
        self.stat_recorded
            .store(false, std::sync::atomic::Ordering::Relaxed);
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
                // 切换前是否有未上屏的编码/候选（决定是否需要结束应用 composition）。
                let had_pending = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    !s.input_buffer.is_empty()
                        || !s.committed_text.is_empty()
                        || !s.candidates.is_empty()
                };
                let (status, commit_text) = self.handle_toggle_mode();
                let chinese_after = status.as_ref().map(|s| s.chinese_mode).unwrap_or(false);
                // 切英文（中→英）有待输入：commit_on_switch=true 上屏原始编码，否则空 commit。
                // 两种都返回 InsertText：空文本 + 有 composition 时 C++ CommitText 仍会
                // EndComposition，清掉应用里残留的编码（StatusUpdate 分支不结束 composition，
                // 是“切英文后编码不清空”的根因）；mode_changed 同时更新中英图标。
                if !commit_text.is_empty() || had_pending {
                    return KeyAction::InsertText {
                        text: commit_text,
                        new_composition: None,
                        mode_changed: true,
                        chinese_mode: chinese_after,
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
        if let Some(action) = self.rt().compiled_hotkeys.match_key_down(norm_hash)
            && !action.is_empty()
        {
            debug!(
                "Hotkey matched (key_down): {} (0x{:08X})",
                action, norm_hash
            );
            let action = action.to_string();
            // 加词热键需返回占位 composition（激活 C++ 转发全部按键），不符 dispatch_hotkey
            // 的「bool→StatusUpdate」契约，故在此特判直接返回 KeyAction。仅中文模式响应。
            if action == "add_word" {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    return self.enter_add_word_mode(&mut state);
                }
            } else if self.dispatch_hotkey(&action) {
                return KeyAction::StatusUpdate(self.build_status());
            }
        }

        // ── 候选词操作热键（Ctrl+数字 置顶/删除）──
        // 这两组在编译期仅注册转发（action 为空，上方匹配不触发），实际语义在此分派。
        // 须先于下方 Ctrl/Alt 组合「清空隐藏候选」分支，否则 Ctrl+数字 会被当作普通组合吞掉。
        if let Some(act) = self.handle_candidate_action_hotkey(data) {
            return act;
        }

        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        // 快捷加词模式：消费全部按键（↑↓调词长/Enter确认/Esc退出），先于英文透传与单点分派。
        if state.add_word_active {
            return self.handle_add_word_key(&mut state, data);
        }

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
        if self.rt().config.input.url.enabled {
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

        // 以词定字（select_char）：配置的成对标点键从当前高亮候选词逐字上屏（对齐 Go
        // handleEngineDefault——select_char 优先于翻页键，故置于 apply_nav_key 之前）。默认
        // `select_char_keys` 为空 → select_char_index 恒 None → 跳过（零回归）。仅在缓冲非空或
        // 有候选时拦截；空缓冲且无候选时放行，让 `,`/`.` 作普通标点（对齐 Go 空缓冲回退标点）。
        if data.modifiers & MOD_SHIFT == 0
            && (!state.input_buffer.is_empty() || !state.candidates.is_empty())
            && let Some(char_index) = self.select_char_index(data.key_code)
        {
            return self.handle_select_char_with_overflow(
                &mut state,
                char_index,
                data.key_code,
                data.prev_char,
            );
        }

        // 候选翻页/高亮：配置驱动统一处理（普通模式为码表型，`-`/`=` 可作翻页）。
        // 仅有候选时生效；无候选时下方 match 的回退臂负责透传方向/翻页键。
        if let Some(act) = self.apply_nav_key(&mut state, data, true) {
            return act;
        }

        // 数字小键盘（对齐 Go）：follow_main 把数字键 1-9 视同主键盘数字（选当前页候选）；
        // direct（默认）IME 直接输出小键盘字符（先丢弃当前未上屏编码）。仅中文模式到达此处。
        if let Some(npc) = numpad_char(data.key_code) {
            let follow_main = self.rt().config.input.numpad_behavior == "follow_main";
            if follow_main {
                let has_comp = !state.input_buffer.is_empty()
                    || !state.committed_text.is_empty()
                    || !state.candidates.is_empty();
                if let Some(d) = npc.to_digit(10) {
                    // 数字键：完全等同主键盘数字键——空缓冲透传输出数字，否则选词/越界 overflow。
                    // `0` 对齐主键盘选第 10 个候选（num=10）。
                    // 对齐 Go：空缓冲透传前先记录统计（SourcePunctuation）。
                    if !has_comp {
                        self.record_commit(&npc.to_string(), 0, -1, CommitSource::Punctuation);
                        return KeyAction::PassThrough;
                    }
                    let num = if d == 0 { 10 } else { d as usize };
                    return self.handle_number_key_select(&mut state, num);
                }
                // 运算符 / 小数点：等同主键盘标点——有组合先顶字上屏高亮候选，再输出该字符。
                let committed = self.take_committed(&mut state);
                let mut out = self.maybe_s2t(&state, &committed);
                if !state.candidates.is_empty() {
                    let idx = self
                        .highlighted_global_index(&state)
                        .min(state.candidates.len() - 1);
                    let t = state.candidates[idx].text.clone();
                    self.record_selection(&state.input_buffer, &t);
                    out.push_str(&self.maybe_s2t(&state, &t));
                }
                state.input_buffer.clear();
                state.candidates.clear();
                if has_comp {
                    self.notify_ui_hide();
                }
                out.push_str(&if state.full_width {
                    to_full_width(&npc.to_string())
                } else {
                    npc.to_string()
                });
                return Self::commit_action(out, state.chinese_mode);
            }
            // direct（默认）：丢弃当前未上屏编码，直接输出小键盘字符。
            if !state.input_buffer.is_empty()
                || !state.committed_text.is_empty()
                || !state.candidates.is_empty()
            {
                state.committed_text.clear();
                state.committed_segs.clear();
                state.input_buffer.clear();
                state.candidates.clear();
                self.notify_ui_hide();
            }
            let out = if state.full_width {
                to_full_width(&npc.to_string())
            } else {
                npc.to_string()
            };
            return Self::commit_action(out, state.chinese_mode);
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
                    state.committed_text = state
                        .committed_segs
                        .iter()
                        .map(|(_, t)| t.as_str())
                        .collect();
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
                    let (start, _) = self.page_range(&state);
                    let idx = (start + state.selected_index).min(state.candidates.len() - 1);
                    let cand = state.candidates[idx].clone();
                    self.commit_selected(&mut state, &cand, (idx - start) as i32)
                } else if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    // 空码空格：按 space_on_empty_behavior（对齐 Go handleSpace 空码分支）——
                    // "clear" 清空编码；否则上屏「已转换前缀 + 剩余拼音原码」。
                    if self.rt().config.input.space_on_empty_behavior == "clear" {
                        state.committed_text.clear();
                        state.committed_segs.clear();
                        state.input_buffer.clear();
                        state.candidates.clear();
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    let prefix = self.take_committed(&mut state);
                    // 上屏剩余拼音原码：prefix(committed) 段已在选词时记过，此处只记 input_buffer 避免重复。
                    self.record_commit(
                        &state.input_buffer,
                        state.input_buffer.len() as u32,
                        -1,
                        CommitSource::RawInput,
                    );
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
                // Enter：按 enter_behavior 配置（对齐 Go handleEnter）——"clear" 清空编码
                // (不上屏)；否则(commit)上屏「已转换前缀 + 剩余原码」。
                if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    if self.rt().config.input.enter_behavior == "clear" {
                        state.committed_text.clear();
                        state.committed_segs.clear();
                        state.input_buffer.clear();
                        state.candidates.clear();
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    let prefix = self.take_committed(&mut state);
                    // 上屏剩余拼音原码：prefix(committed) 段已在选词时记过，此处只记 input_buffer 避免重复。
                    self.record_commit(
                        &state.input_buffer,
                        state.input_buffer.len() as u32,
                        -1,
                        CommitSource::RawInput,
                    );
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
                // 数字键 1-9 选当前页第 N 个候选；越界按 input.overflow.number_key 处理
                // （ignore 吞键 / commit 上屏高亮 / commit_and_input 顶字+数字，对齐 Go）。
                let num = (data.key_code - 0x31) as usize + 1; // 1..=9
                // 无候选时保持透传：纯数字键应输出数字（不拦截空缓冲下的数字）。
                // 对齐 Go：recordCommit(key, 0, -1, SourcePunctuation) 后再 return nil。
                if state.candidates.is_empty()
                    && state.input_buffer.is_empty()
                    && state.committed_text.is_empty()
                {
                    let digit = (b'0' + num as u8) as char;
                    self.record_commit(&digit.to_string(), 0, -1, CommitSource::Punctuation);
                    return KeyAction::PassThrough;
                }
                self.handle_number_key_select(&mut state, num)
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
                    // 顶码即上屏首选（pos=0），code_len=被顶出的前缀码长。
                    self.record_commit(&top_text, prefix.len() as u32, 0, CommitSource::Candidate);
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
                    // 双拼韵母键避让：正在输入双拼（缓冲非空）且该键是当前布局的韵母键时，
                    // 跳过选词分支，让该键作为编码输入累积（对齐 Go IsShuangpinFinalKey）。
                    let is_shuangpin_final = !state.input_buffer.is_empty()
                        && punct_char(data.key_code, false)
                            .map(|c| self.engine_mgr.shuangpin_final_key(c as u8))
                            .unwrap_or(false);
                    let mut select_overflow: Option<char> = None;
                    if !is_shuangpin_final {
                        if let Some(offset) = self.select_key_offset(data.key_code) {
                            let (start, end) = self.page_range(&state);
                            let idx = start + offset;
                            if idx < end {
                                let cand = state.candidates[idx].clone();
                                return self.commit_selected(&mut state, &cand, offset as i32);
                            }
                            // E. 越界：记下触发键字符，延后到模式触发判定之后再按 overflow 策略处理
                            // （对齐 Go decideBufferedTrigger——次/三选键越界时 overflow 排在
                            // 模式激活之后，故 `;` 候选不足时优先进快捷输入而非 overflow）。
                            select_overflow = punct_char(data.key_code, false);
                        }
                    }
                    // D. 模式触发键 → 顶屏高亮候选 + 进模式。
                    // 融合「快捷」（现唯一的快捷输入形态，成员含日期/计算/拼音/英文）——对齐空缓冲
                    // 时 handle_lifecycle 的 enter_mix_mode，使有无候选都进同一融合模式。
                    if let Some(idx) = self.match_mix_trigger(data.key_code)
                        && (self.mix_has_quick_input(idx) || !self.mix_members(idx).is_empty())
                    {
                        return self.commit_and_enter_mix_mode(&mut state, idx, data.key_code);
                    }
                    if self.is_temp_pinyin_trigger(data.key_code)
                        && let Some(target) = self.engine_mgr.temp_pinyin_target()
                    {
                        return self.commit_and_enter_temp_pinyin(
                            &mut state,
                            data.key_code,
                            target,
                        );
                    }
                    // E. 次/三选键越界且非模式触发键 → 按 input.overflow.select_key 处理
                    if let Some(ch) = select_overflow {
                        return self.handle_overflow_select_key(&mut state, ch, data.prev_char);
                    }
                }
                if let Some(ch) = punct_char(data.key_code, shift) {
                    // 智能符号模式：同键连按删中文标点改英文（press2 短路返回）。
                    // 须在候选提交逻辑之前：press2 时无待输入，依赖光标前字符匹配武装态。
                    if let Some(act) = self.try_smart_symbol_replace(&state, ch, data.prev_char) {
                        return act;
                    }
                    // 标点顶码上屏开关：有编码/已确认前缀时，码表/混输按方案
                    // engine.codetable.punct_commit 决定是否顶字上屏。
                    // 关闭时标点「直接无效」——吞掉该键、保留编码继续输入（不顶字、不透传上屏
                    // 英文标点）。该功能少用，吞键比 Go 的 `return nil` 透传更符合预期。
                    // TODO(拼音标点顶码)：拼音引擎也应有独立 punct_commit 配置（默认开），
                    // 待相关引擎配置重构落定后接入；当前拼音恒顶字上屏（等价默认开）。
                    let has_input =
                        !state.input_buffer.is_empty() || !state.committed_text.is_empty();
                    if has_input {
                        let punct_commit = match self.engine_mgr.current_engine_type() {
                            Some(wind_engine::EngineType::Pinyin) => true,
                            _ => self
                                .engine_mgr
                                .schema_merged(&self.engine_mgr.active_schema_id())
                                .map(|s| s.engine.codetable.punct_commit)
                                .unwrap_or(false),
                        };
                        if !punct_commit {
                            return KeyAction::Consumed;
                        }
                    }
                    // 标点/符号键：先上屏已转换前缀 + 首选候选（若有输入），再追加（转换后的）标点
                    let committed = self.take_committed(&mut state);
                    let mut out = self.maybe_s2t(&state, &committed);
                    if !state.candidates.is_empty() {
                        let (start, _) = self.page_range(&state);
                        let idx = (start + state.selected_index).min(state.candidates.len() - 1);
                        let t = state.candidates[idx].text.clone();
                        self.record_selection(&state.input_buffer, &t);
                        // 标点上屏前先记被顶出的高亮候选（来源候选）。
                        self.record_commit(
                            &t,
                            state.input_buffer.len() as u32,
                            (idx - start) as i32,
                            CommitSource::Candidate,
                        );
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
                    // 标点字符（候选部分已在标点前顶屏候选处记 Candidate；标点候选已 set
                    // stat_recorded，故此处必须显式记标点，否则顶层 fallback 会跳过它）。
                    self.record_commit(&piece, 0, -1, CommitSource::Punctuation);
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
                            if tr.peek().is_some_and(|e| e.right == pch) {
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
        self.show_persistent_status_if_always(); // 常驻模式:获焦即显示状态
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
        self.hide_tip(); // 失焦隐藏状态提示（常驻模式尤需）
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
        self.show_persistent_status_if_always(); // 常驻模式:激活即显示状态
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
        self.hide_tip(); // 切走本输入法隐藏状态提示
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
        // 标点随中英文切换（对齐 Go）：开启 punct_follow_mode 时，标点中/英跟随当前模式。
        if self.rt().config.input.punct.follow_mode {
            state.chinese_punct = chinese;
        }
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese);
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.disarm_smart_symbol();
        self.push_state_update();
        self.show_status();
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
        (Some(self.build_status()), commit_text)
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = chinese_mode;
        // 标点随中英文切换（对齐 Go）：开启 punct_follow_mode 时，标点跟随模式。
        if self.rt().config.input.punct.follow_mode {
            state.chinese_punct = chinese_mode;
        }
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese_mode);
        drop(state);
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.disarm_smart_symbol();
        self.push_state_update();
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
        (Some(self.build_status()), commit_text)
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
}

/// 候选调试信息段（tooltip debug provider）：编码/权重/引擎/标记。空候选返回空串。
fn debug_tooltip_section(c: &Candidate) -> String {
    let mut lines = Vec::new();
    if !c.code.is_empty() {
        lines.push(format!("编码: {}", c.code));
    }
    lines.push(format!("权重: {}", c.weight));
    lines.push(format!("引擎: {:?}", c.source));
    if c.has_shadow {
        lines.push("标记: 已调整".to_string());
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("调试\n{}", lines.join("\n"))
    }
}

#[cfg(test)]
mod reload_tests {
    //! 热重载基础：验证 ConfigBundle 能从 Config 正确重建轻量派生缓存。
    //! （reload_user_config 走磁盘 IO 不在此测；这里测其核心——从配置重建派生状态。）
    use super::*;

    #[test]
    fn config_bundle_rebuilds_pairs_from_config() {
        let mut cfg = Config::default();
        cfg.input.auto_pair.chinese_pairs = vec!["（）".to_string(), "【】".to_string()];
        cfg.input.auto_pair.english_pairs = vec!["()".to_string()];
        let b = ConfigBundle::build(cfg);
        assert_eq!(b.cn_pairs, vec![('（', '）'), ('【', '】')]);
        assert_eq!(b.en_pairs, vec![('(', ')')]);
    }

    #[test]
    fn config_bundle_carries_config_values() {
        // 改配置 → 重建 bundle → bundle.config 反映新值（热重载替换后读取生效的基础）。
        let mut cfg = Config::default();
        cfg.input.symbol.smart_mode = true;
        cfg.ui.candidate.per_page = 9;
        let b = ConfigBundle::build(cfg);
        assert!(b.config.input.symbol.smart_mode);
        assert_eq!(b.config.ui.candidate.per_page, 9);
    }
}
