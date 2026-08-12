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

use crate::handle_mode::MixLens;
use crate::pipeline::{ModeKind, Rewind};
use crate::preedit_cursor;
use crate::theme_style::ThemeStyle;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, trace, warn};
use wind_keys::keymap;

use wind_bridge::handler::*;
use wind_bridge::push::{PushConfig, PushServer};
use wind_candidate::{Candidate, CandidateSource};
use wind_config::Config;
use wind_config::PreeditDisplay;
use wind_config::hotkey::{self, CompiledHotkeys};
use wind_engine::EngineManager;
use wind_ipc::protocol::{
    EVENT_KEY_DOWN, EVENT_KEY_UP, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN, calc_key_hash,
};
use wind_store::Store;
use wind_store::stat_collector::{StatCollector, StatEvent};
use wind_store::stats::CommitSource;
use wind_transform::fullwidth::to_full_width;
use wind_transform::punctuation::PunctuationConverter;
use wind_ui::candidate_window::CandidateItem;
use wind_ui::manager::{GlobalHotkeyEntry, UiCommand, UiEvent};
// UiManager 仅 Windows LayeredWindow 路径用；macOS 走 host-render forwarder。
#[cfg(not(target_os = "macos"))]
use wind_ui::manager::UiManager;
use wind_ui::toast::{ToastKind, ToastPosition};

/// caret_use_top 兼容下保留给「上方显示」避让正文的最小行高（物理像素）。微信 reflow 后的
/// 权威帧通常上报真实行高（~20px，随 DPI 缩放），直接取用；仅退化帧（height=1）落到此下限，
/// 保证上方候选窗底边抬到正文之上而不遮挡。偏大只是多留空隙，故取一个稳妥的正文行高量级。
const CARET_USE_TOP_MIN_LINE_H: i32 = 18;

/// direct_commit 顶码余码新组合的 keyup 兜底定时器时长（ms）。见 top-commit-mode 设计文档 §5。
pub(crate) const DEFERRED_COMPOSITION_FALLBACK_MS: u32 = 150;

/// wind 修饰位（SHIFT=0x1/CTRL=0x2/ALT=0x4/WIN=0x8，见 wind-ipc MOD_*）→ Win32 位序
/// （ALT=0x1/CTRL=0x2/SHIFT=0x4/WIN=0x8，即 ALT 与 SHIFT 互换）。
/// RegisterHotKey 的 fsModifiers 与 DirectSwitchHotkeys 的 Modifiers 低位（TF_MOD_*）同用此位序。
fn wind_mods_to_win32(mods: u32) -> u32 {
    const WIN32_MOD_ALT: u32 = 0x0001;
    const WIN32_MOD_CONTROL: u32 = 0x0002;
    const WIN32_MOD_SHIFT: u32 = 0x0004;
    const WIN32_MOD_WIN: u32 = 0x0008;
    let mut win = 0u32;
    if mods & MOD_SHIFT != 0 {
        win |= WIN32_MOD_SHIFT;
    }
    if mods & MOD_CTRL != 0 {
        win |= WIN32_MOD_CONTROL;
    }
    if mods & MOD_ALT != 0 {
        win |= WIN32_MOD_ALT;
    }
    if mods & MOD_WIN != 0 {
        win |= WIN32_MOD_WIN;
    }
    win
}

/// 取进程 ID 对应的可执行文件名（如 "Weixin.exe"）。对齐 Go `bridge.GetProcessName`：
/// OpenProcess(QUERY_LIMITED_INFORMATION) + QueryFullProcessImageNameW，取末段文件名。
/// 失败（进程已退出/权限不足）返回空串。
#[cfg(windows)]
fn process_name(pid: u32) -> String {
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    if pid == 0 {
        return String::new();
    }
    unsafe {
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return String::new(),
        };
        let mut buf = [0u16; MAX_PATH as usize];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
    }
}

/// 非 Windows（测试/交叉编译）下无进程名概念，返回空串 → 不命中任何兼容规则。
#[cfg(not(windows))]
fn process_name(_pid: u32) -> String {
    String::new()
}

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

/// 解析配对跳出键名 → VK 码集合。支持 tab / enter(return) / space / escape(esc)；
/// 大小写与首尾空白不敏感，未知名忽略。这些非可打印键不在 keymap 的 KEY_TABLE
/// （引导/触发用的 OEM 符号键）内，故在此单独映射。
fn parse_jump_out_keys(list: &[String]) -> std::collections::HashSet<u32> {
    list.iter()
        .filter_map(|s| match s.trim().to_lowercase().as_str() {
            "tab" => Some(keymap::VK_TAB),
            "enter" | "return" => Some(keymap::VK_RETURN),
            "space" => Some(keymap::VK_SPACE),
            "escape" | "esc" => Some(keymap::VK_ESCAPE),
            // `right_symbol` 不是键名（右符号是哪个键取决于配对表），由
            // `parse_jump_out_on_right_symbol` 单独解析成开关。
            _ => None,
        })
        .collect()
}

/// `jump_out_keys` 是否含「右符号键本身」这一特殊值 → 打 `）` 跳出已插入的 `（）`。
/// 与 VK 集合分开表示：右符号不是固定按键，取决于当前生效的配对表。
fn parse_jump_out_on_right_symbol(list: &[String]) -> bool {
    list.iter()
        .any(|s| s.trim().to_lowercase() == wind_config::config::JUMP_OUT_RIGHT_SYMBOL)
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

/// 小键盘键 → 主键盘等价键 `(vk, 是否需 Shift)`。非小键盘键返回 None。
///
/// `numpad_behavior = follow_main` 的**唯一实现手段**：在分派前把小键盘键重写成主键盘等价键，
/// 此后全部模式（普通 / 临拼 / 临英 / 特殊 / mix / URL）自动与主键盘一致，无需各 handler
/// 各自复制一份数字键语义——「各处自行实现」正是小键盘在多数模式下被静默吞掉的成因。
///
/// 运算符须连 Shift 一并归一（主键盘 `*` = Shift+8、`+` = Shift+=），归一后 `punct_char`
/// 自然给出正确字符，且 `if modifiers & MOD_SHIFT == 0` 的选词臂会正确地不匹配。
pub(crate) fn numpad_to_main(key_code: u32) -> Option<(u32, bool)> {
    use keymap::*;
    Some(match key_code {
        0x60..=0x69 => (key_code - 0x60 + VK_0, false), // Numpad0-9 → 主键盘 0-9
        0x6A => (0x38, true),                           // * = Shift+8
        0x6B => (VK_EQUAL, true),                       // + = Shift+=
        0x6D => (VK_MINUS, false),                      // -
        0x6E => (VK_PERIOD, false),                     // .
        0x6F => (VK_SLASH, false),                      // /
        _ => return None,
    })
}

/// 全角态下「TSF 已吃下的键」→ 待转换的源字符。
///
/// **覆盖面必须 ⊇ C++ 的全角吃键集**（`KeyEventSink.cpp` 的 `english_fullwidth` /
/// `chinese_fullwidth_number` / `chinese_fullwidth_space` 三个分支：Letter|Number|
/// Punctuation|Space，含小键盘）。返回 None 会让调用方 PassThrough → 键已被吃下 →
/// 「吃了再吐」→ 严格 TSF 宿主(Chrome/Electron)直接丢键。C++ 吃键分支增删时须同步此处。
///
/// 空格与小键盘都不在 `printable_char` 覆盖内（`punct_char` 无 VK_SPACE），故在此收口，
/// 供英文全角与 CapsLock+全角两条路径共用，避免两处各记一套而漂移。
pub(crate) fn full_width_source_char(key_code: u32, shift: bool) -> Option<char> {
    if key_code == keymap::VK_SPACE {
        return Some(' ');
    }
    printable_char(key_code, shift).or_else(|| numpad_char(key_code))
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

/// 用户输入的大小写**变形候选**：按 全小写 → 首字母大写 → 全大写 的固定次序产出，
/// 并剔除与原文相同的那一项（原文自身是首候选，无需重复）。纯 ASCII 语义即够用——
/// 临英缓冲只可能由 VK 字母 / 数字 / ASCII 标点组成。
///
/// 之所以是「枚举三形态」而非旧的「检测输入形态 → 适配词库候选」（`detect_en_case` /
/// `adapt_en_case`，已删）：Shift+字母是临英的进入方式，缓冲首字母**恒为大写**，
/// 于是旧检测恒返回 Title，把整列词库候选强制套成 `Hello`/`Help`/`Held`，
/// 而词库里 86% 的词本是小写。触发方式的副作用被当成了用户的大小写意图。
/// 现在词库候选一律保持原文，大小写改由用户在这几个变形候选里显式选。
///
/// 副产物：对全大写、混合大小写输入也自洽——原文是哪种形态，缺的另两种就自动补齐。
/// 无字母的缓冲（如 `123`）三形态皆等于原文，返回空表。
pub(crate) fn en_case_variants(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    let mut chars = lower.chars();
    let title = match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    // 三形态间也可能互等（单个小写字母 "a" → title/upper 同为 "A"），故一并有序去重。
    let mut out: Vec<String> = Vec::with_capacity(3);
    for v in [lower, title, s.to_uppercase()] {
        if v != s && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// 可打印字符 → 主键盘 VK（无 Shift 态）。找不到返回 `None`。
///
/// [`punct_char`] 的反向查询。仅供配置体检使用（启动一次），故用线性扫描而非反查表——
/// 建表反而多一份需要与 `punct_char` 保持同步的真相源。
fn char_to_main_vk(ch: char) -> Option<u32> {
    (0x20u32..=0xFF).find(|&vk| punct_char(vk, false) == Some(ch))
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

/// 临时拼音（overlay 模式）向拼音引擎取数的上限。
///
/// **为什么这里可以直接取全量、而主路径要分批**：拼音引擎的 `max_candidates` 只用于最后
/// 一步 `truncate`，召回/整句/排序全是全量做的（见 `pinyin/mod.rs`）。实测 `yi` 取 50 与取
/// 5000 的耗时（6.2ms vs 6.4ms）与峰值内存（778KB）**完全相同**——小 limit 省不到任何成本，
/// 只是把已构造好的候选丢掉。而临拼**没有翻页扩容通路**（`expand_candidates` 的守卫比对的是
/// `input_buffer`，临拼的码在 `temp_pinyin_buffer` 里），一次取不全就永远取不到：
/// 这正是「临拼下 `ying` 打不出「瑩」（该字在第 158 位）」的成因。
///
/// 取全量后翻页天然可穷尽——翻页只是对 `state.candidates` 切片，无需重新查询。
/// 实测拼音候选上界为 916（`yi`），5000 留足余量。
///
/// ⚠️ **该值只对拼音类引擎安全**。码表单字母候选可达 5472 条（`r`），取全量峰值 34.9MB、
/// 耗时 39.6ms，绝不可用；故取数前须按目标方案的引擎类型分流（见 `temp_pinyin_limit`）。
pub(crate) const TEMP_PINYIN_MAX_CANDIDATES: usize = 5000;

/// 自动造词（L）写入临时层的初始权重（保守默认，低于手动加词；后续可接 schema.learning 配置）。
/// 复选次数只用于晋升判定（见 `Store::learn_temp_word`），不再驱动权重增长——
/// 晋升入用户词库时统一取 `wind_store::temp_words::PROMOTED_WEIGHT`。
pub(crate) const LEARN_ADD_WEIGHT: i32 = 800;

/// 自提交宽限期：本输入法吐字后这段时间内收到的 `SelectionChanged` 视为宿主回声，
/// 不当作用户移动光标（见 `handle_selection_changed`）。
///
/// **已由真机日志校准**（2026-07-20，记事本/Chrome/EverEdit 混合样本 n≈280）：
/// - 自提交回声：3.6 ~ 10.7ms，离群值 62.9ms / 78.9ms
/// - 用户真实光标移动：最小 322.8ms，其余 453ms / 828ms / 1.4s / 70s
///
/// 两类之间 79ms→323ms 是一段空白，200ms 落在正中，上下均有 2.5 倍以上余量。
/// 取值过小 → 回声被误判为用户操作，序列被切碎、造词失效；取值过大 → 用户上屏后
/// 短时间内的真实光标移动漏掉一次终止（由 idle 超时兜底）。
/// 重新校准方法：把 `handle_selection_changed` 的 TRACE 打开，重跑分布。
pub(crate) const SELF_COMMIT_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// 首显长兜底：坐标不可信时等待权威坐标的上限。
///
/// 两个用处同一语义——「这一帧的坐标值得等，因为手里那份不能用」：
/// - `handle_caret_pending`：宿主明说「组合刚起、坐标待定」（`wait` 档）；
/// - `fire_pending_first_show`：`fast` 档短兜底到期，但坐标缓存未经当前插入点验证。
///
/// 取值来自 `wait` 档既有行为（长期作默认档，用户未反馈过「候选窗要等半秒」）。实测
/// Excel 首次输入建单元格编辑上下文需 454ms、真坐标 558ms 到达，是已知最慢的一档。
pub(crate) const FIRST_SHOW_LONG_FALLBACK_MS: u64 = 600;

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
/// macOS 经 CmdOpenSettings(0x0507) 让 .app 直接启动设置应用，不走 URL/exe 路径，故仅非 macOS。
#[cfg(not(target_os = "macos"))]
pub(crate) fn settings_url() -> Option<String> {
    SETTINGS_URL_PROVIDER.get().and_then(|f| f())
}

/// 取同目录下 wind_setting 设置应用的可执行路径（None=不存在）。
/// 由当前 exe 名推导变体：wind_input[_dev].exe → wind_setting[_dev].exe，
/// 故无需感知编译期变体，正式/dev 版自动对应。
/// macOS 经 CmdOpenSettings(0x0507) 由 .app 按 bundleID 启动设置应用，不需可执行路径，故仅非 macOS。
#[cfg(not(target_os = "macos"))]
pub(crate) fn settings_app_path() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_str()?; // wind_input 或 wind_input_dev
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
    /// 检索范围的**临时**放宽（手动触发：末页再按翻页键 / 专用热键）。
    /// 设计见 `docs/design/smart-filter-scope-relax.md` §5。
    ///
    /// **只在内存、绝不写配置**——这是与 `set_filter_mode` 的关键区别，后者会持久化到
    /// `input.filter_mode`。本次组合结束（缓冲清空）即失效，失效收口在
    /// `handle_key_event_policed`（清空路径十几处，散点接线必漏）。
    ///
    /// 放宽时把**全部**被滤候选带 `is_scope_filtered` 标记**追加到末尾**，与自动补充同一
    /// 呈现方式（区别只在补多少：自动补到一页、手动放全部）。
    ///
    /// ⚠️ 曾设计成「按真实顺序插入，与菜单切『全部字符』所见一致」，**已否决**：翻页是线性
    /// 前进的动作，翻到末尾再翻却让新字插到第 1 页（实测 `dwi` 的字权重 8999 占三简位，正好
    /// 排到第 1 页第 2 位），视口要么跳回页首、要么原地不动，两种都突兀。菜单切换是全局持久
    /// 的换档，末页翻页是临时的渐进探索——语义不同，不必对齐呈现。
    pub(crate) scope_relaxed: bool,
    /// 用户是否开启常驻工具栏（菜单开关；与“当前是否激活”正交）。
    pub(crate) toolbar_visible: bool,
    /// 本输入法当前是否处于激活态：IME_ACTIVATED/FocusGained 置真；
    /// IME_DEACTIVATED（切换输入法）与 FocusLost 的 `Thread` reason（整个应用失去前台，
    /// 含“每应用独立输入法”下切到别的输入法的应用）置假。
    ///
    /// ⚠ 本字段只表达「本输入法是否在为某个宿主服务」，**不表达「焦点在不在可编辑控件
    /// 里」**——后者是 [`Self::has_edit_context`]。两者变化时机不同（前者随应用切换，
    /// 后者随控件切换），曾经挤在这一个布尔量里，导致应用内点到非文本框时无法表达，
    /// 工具栏永不隐藏（实测 LogExpert / 文件管理器，2026-07-26）。
    pub(crate) ime_active: bool,
    /// 焦点当前是否落在可编辑控件里。focus_gained 置真；FocusLost 的 `CtxLost` /
    /// `NoEditCtx` / `Thread` reason 置假（`DocChanged` 不动——换文档后由随后的
    /// focus_gained 或 no-edit-ctx 分支重新定夺）。
    ///
    /// 与 [`Self::ime_active`] 正交：应用还在前台、输入法仍激活，但焦点可能落在
    /// 不可输入的地方（文件列表、日志面板），此时工具栏应当隐藏。
    pub(crate) has_edit_context: bool,
    pub(crate) caps_lock: bool,
    pub(crate) input_buffer: String,
    /// `input_buffer` 的「原始大小写」影子串：用户按 Shift+字母打出的大写只存在这里。
    /// 空 = 没有大写；与缓冲失配同样视为没有大写（见 `preedit_cursor::cased_is_valid`）。
    ///
    /// **缓冲本身恒为全小写**——引擎查询、顶码判定、词频记账、加词取码一律按它，大小写对
    /// 匹配零影响。本字段只出现在两个出口：组合区显示，以及「上屏原码」（回车/空格空码/
    /// 标点顶屏）。读写走 `preedit_cursor::BufEdit::new_cased`，勿裸改。
    pub(crate) input_buffer_cased: String,
    /// 编码区光标：`input_buffer` 内的字节偏移，定义域 `[0, input_buffer.len()]`。
    /// 恒指向剩余编码内部——已转换前缀（`committed_text`）是只读前缀，光标进不去（Home 只到
    /// 剩余编码开头）。光标**不参与引擎查询**：`update_candidates` 恒查整串，移动光标不重算
    /// 候选（对齐 Go `inputCursorPos`）。所有读写走 `preedit_cursor::BufEdit`，勿裸改。
    pub(crate) input_cursor_pos: usize,
    /// 组合区显示文本（拼音含音节分隔 "ni'hao"；码表为原始编码）。
    /// 仅显示输入码/拼音，绝不包含候选列表。
    pub(crate) preedit: String,
    /// 拼音音节拆分形态（不含已转换前缀）。供「混输高亮跟随」：高亮拼音候选 → preedit 用此
    /// 拆分串；高亮码表/五笔候选 → 用原始码（input_buffer）。空串 = 无拆分形态（码表/无拼音，
    /// 恒原始码）。每次 build_candidates 重置；非普通模式（active!=None）不读取。
    pub(crate) preedit_split_body: String,
    /// **全拼降级**的音节拆分形态（双拼方案下把击键按全拼切分，`zaijian` → `zai'jian`）。
    /// 高亮到 `is_fullpinyin_fallback` 的候选时 preedit 用它；其余情形不读。
    /// 空串 = 无此形态（非双拼 / 开关关 / 支路无产出）。每次 build_candidates 重置。
    pub(crate) preedit_fp_body: String,
    /// 候选调整（shadow）规则的**归一编码**；空串 = 落回 `input_buffer`（击键原样）。
    ///
    /// 取自 `ConvertResult::shadow_code`，与 `preedit_split_body` 同生命周期（每次
    /// `build_candidates` 重置）。存在的唯一理由是双拼：`data_schema_id` 已把全拼与双拼
    /// 折叠成同一个 schema，若 key 继续取击键，双拼的 `hc` 与全拼的 `hao` 会落成两个互不
    /// 相认的键。归一后两者共享同一条规则。全拼恒空串（恒等，存量规则零迁移）。
    ///
    /// ⚠️ **读写两端必须同取此值**（`shadow_code_of`）：读端 `apply_shadow`、写端
    /// `candidate_op_scope`、菜单灰显 `shadow_has_rule` 若有一处漏改，失配是**完全静默**的
    /// ——规则写得进去、读不出来，界面毫无异常。守门测试见 `handle_candidate.rs` 的
    /// `every_shadow_read_goes_through_normalized_code`。
    pub(crate) shadow_code: String,
    pub(crate) candidates: Vec<Candidate>,
    /// 当前页内高亮候选下标（0-based，相对当前页）——键盘选中项，空格上屏的目标
    pub(crate) selected_index: usize,
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
    /// 已分步上屏的段：`(raw_code, code, text, source, boundary)`。
    /// 供退格逐段回退与完整上屏时自动造词；来源用于混输自动造词的"全段同源"归属路由（P2d）。
    ///
    /// # 为什么记两份码
    ///
    /// 两个消费者要的量纲天生不同，**不可合并**：
    /// - `raw_code` = **原始输入空间**的消费码（双拼下是击键 `hc`）。退格回退（`pop_*_seg`）
    ///   把它并回输入缓冲，故必须与缓冲同域。
    /// - `code` = **全拼语义**码（`hao`）。词频记账与自动造词（`learn_phrase_on_commit`）
    ///   用它，且 `boundary` 的位移量按 `code.len()` 算——换成双拼击键会写坏用户词库
    ///   并让音节边界位全错。
    ///
    /// 引擎侧只把 `consumed_length` 回映射到原始输入空间，`code` 刻意保持全拼语义
    /// （见 `wind_engine::pinyin` 中 `map_consumed_length` 与 Fix A 的注释）。曾因这里
    /// 只记全拼码，双拼下退格把 `hao` 并回击键缓冲 `ma` → 重解析成 `ha|o|ma` 而错乱。
    /// 非双拼场景两者恒相等。
    ///
    /// boundary = 该段 code 的音节边界（见 `wind_dict::binformat::DictEntry::boundary`）；
    /// 段自身可能是多音节整词（选「你好」→ 段码 nihao、段内边界 ni|hao），故自动造词拼接
    /// 各段时须把段内边界平移到全局位置，不能只按「一段一音节」记。
    pub(crate) committed_segs: Vec<(String, String, String, CandidateSource, u64)>,
    /// 当前激活的独占输入模式（临时拼音/快捷输入/临时英文）。`None` = 普通输入。
    /// 单点决策的唯一真相源：结构上保证同一时刻至多一个独占模式（见 `pipeline.rs`）。
    pub(crate) active: Option<ModeKind>,
    /// 各 overlay 模式组合区显示主体（= preedit 去掉只读前缀的部分），供光标位置换算。
    /// 仅临拼 / mix 需要维护——它们的主体是引擎 `preedit_display`（含插入的音节分隔符），
    /// 与缓冲不同形；临英 / 特殊 / URL 的主体恒等于自身缓冲，直接用缓冲即可（见
    /// `overlay_caret_parts`）。缓冲空时可能为 stale，但此时光标必为 0、换算不读它，无害。
    pub(crate) overlay_body: String,
    /// 临时拼音输入缓冲（拼音串）
    pub(crate) temp_pinyin_buffer: String,
    /// 临时拼音编码区光标（`temp_pinyin_buffer` 内字节偏移）。下同，各 overlay 缓冲各带一个。
    pub(crate) temp_pinyin_cursor: usize,
    /// 临时拼音目标方案 id（如 "pinyin"）
    pub(crate) temp_pinyin_schema: String,
    /// 临时拼音组合区前缀字符（触发键，如 "`"）
    pub(crate) temp_pinyin_prefix: String,
    /// 临时英文输入缓冲
    pub(crate) temp_english_buffer: String,
    /// 临时英文编码区光标（`temp_english_buffer` 内字节偏移）
    pub(crate) temp_english_cursor: usize,
    /// 临时英文前缀字符（触发键符号，如 "/"；触发键进入时非空，Shift+字母进入时为空）
    pub(crate) temp_english_prefix: String,
    /// 网址模式输入缓冲（原样累积的 URL 文本）
    pub(crate) url_buffer: String,
    /// 网址模式编码区光标（`url_buffer` 内字节偏移）
    pub(crate) url_cursor: usize,
    /// 统一夺取回退登记（仅在夺取式模式激活时为 Some，见 pipeline::Rewind）
    pub(crate) rewind: Option<Rewind>,
    /// 特殊模式编码缓冲（自带码表的查询码）
    pub(crate) special_buffer: String,
    /// 特殊模式编码区光标（`special_buffer` 内字节偏移）。
    /// 注：Go 版特殊模式**不支持**光标（尾加尾删），此处随共享层一并补齐，不再留缺口。
    pub(crate) special_cursor: usize,
    /// 当前特殊模式下标（= `EngineManager::overlay_modes()` 注册表下标；仅 active==Special 时有效）
    pub(crate) special_id: u8,
    /// 当前特殊模式的 `[overlay]` 段**快照**（进入时填、退出时清）。
    ///
    /// 快照而非每次查注册表，三个理由：
    /// 1. `comment::template_for` 返回借用 `cfg` 的 `&str`（刻意不分配），临时 Vec 借不出来；
    /// 2. 布局/注释取值在候选更新路径上，省掉每次的整表 clone；
    /// 3. ★ 注册表按 id 排序，装一个新 overlay 方案会让其后方案的下标平移——快照让
    ///    「模式进行中装了方案」不至于把当前模式的行为换成隔壁那个的。
    ///
    /// 这不是 `layout.rs` 反对的那种「进入时保存、退出时回放」：快照的是**只读配置**，
    /// 随 `active = None` 自然失效，没有需要被回放的动作，声明式重算的性质不变。
    pub(crate) overlay_spec: Option<wind_config::OverlaySpec>,
    /// 特殊模式显示态前缀（进入键符号，如 "\"；只显示不消费，组合区前缀，对齐临时拼音）
    pub(crate) special_prefix: String,
    /// 临时 mix 编码缓冲
    pub(crate) mix_buffer: String,
    /// mix 编码区光标（`mix_buffer` 内字节偏移）
    pub(crate) mix_cursor: usize,
    /// mix 模式显示态前缀（进入键符号，如 ";"；只显示不消费，组合区前缀）
    pub(crate) mix_prefix: String,
    /// 当前 mix 模式下标（= features.mix_modes 索引；仅 active==Mix 时有效）
    pub(crate) mix_id: u8,
    /// 当前候选区是「重复上屏」候选（成员 `quick_input.repeat`，空缓冲时注入上次上屏内容）。
    ///
    /// 该候选没有对应编码，只能整体上屏：选词记录、造词、标点顶屏三条路径据此绕开它。
    /// 由 `update_mix_candidates` 每次装配时重置，故任何一次输入都会自动清掉。
    pub(crate) mix_repeat: bool,
    pub(crate) caret_x: i32,
    pub(crate) caret_y: i32,
    pub(crate) caret_height: i32,
    /// 上面这组坐标的来源（`wind_ipc::protocol::caret_source::*`）。
    ///
    /// **与坐标成对写入**——凡是写 `caret_x/y` 的地方都必须同时写它，否则来源会指向上一次的
    /// 坐标，比没有这个字段更危险。焦点气泡靠它判断「这组坐标够不够格拿来定位」：
    /// TSF 域出自当前 context，GUI 域是跨窗口的 Win32 光标，两者不是同一件东西。
    pub(crate) caret_source: i32,
    /// 菜单是否打开（打开时键盘事件转发给菜单窗口；UI 自管导航）
    pub(crate) menu_open: bool,
    /// 菜单打开时刻，供焦点路径的关闭守卫用（见 `menu_close_on_focus_change`）。
    /// **必须与 `menu_open = true` 成对写入**：漏写会让守卫读到上一次打开的时间戳，
    /// 于是刚弹出的菜单被一条迟到的焦点事件当场关掉。
    pub(crate) menu_opened_at: Option<std::time::Instant>,
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
    /// `add_word_code` 的音节边界（见 `wind_dict::binformat::DictEntry::boundary`）；
    /// 0 = 无信息（码表反查/逐字兜底）。与 code 同生同灭，入库时一并写入用户词。
    pub(crate) add_word_boundary: u64,
}

/// 智能符号模式待命态：press1 提交一个参与集合内的标点后武装，等待时限内同键 press2
/// 触发替换。对齐 Go `smartSymbol*` 字段。
#[derive(Default)]
pub(crate) struct SmartSymbolArm {
    pub(crate) armed: bool,
    /// 武装的触发键（原始英文标点字符）
    pub(crate) key: char,
    /// press1 产出的标点串（…… 为多 rune），删除数 = 其 rune 数。
    /// 正向存中文串、反向存英文串——恒等于**实际上屏的那个串**，press2 的删除数按它算。
    pub(crate) str: String,
    /// 替换方向：false=正向（press1 中文 → press2 英文，原有语义）；
    /// true=反向（press1 英文 → press2 中文）。反向来源：数字后智能标点、英文标点状态、
    /// 英文输入模式。
    pub(crate) reverse: bool,
    /// press1 当时的 `(chinese_mode, chinese_punct)` 快照。press2 要求两者都没变——三种上下文
    /// （中文标点 / 英文标点 / 英文输入模式）各有独立开关与独立产物，press1 后用户切了模式，
    /// 再按同键就该当成全新 press1，而不是在新上下文里按旧方向删字。
    pub(crate) mode_snapshot: (bool, bool),
    /// 武装时刻（None=未武装）；用于时限判定
    pub(crate) at: Option<std::time::Instant>,
    /// HoldComposition 模式下 press1 进入组合态的中文文本（用于 disarm 时清理）。
    /// DeleteReplace 模式下始终为 None。
    pub(crate) held_text: Option<String>,
    /// HoldComposition + has_input 时 press1 设为 true：已武装但调用方须先顶屏上屏候选，
    /// 再开 HoldComposition；coordinator 标点分支检测此标志并生成 CommitAndHoldComposition。
    pub(crate) hold_pending_commit: bool,
}

/// 配置 + 其轻量派生缓存的不可变快照；运行时整体原子替换以支持热重载。
/// 重型组件（引擎/方案/词典）不在内，仍需重启才能完全切换。
pub(crate) struct ConfigBundle {
    pub(crate) config: Config,
    pub(crate) compiled_hotkeys: CompiledHotkeys,
    /// 会话态按键绑定（`keys.session_actions` 编译一次）。**不只是导航**——二期起还装
    /// `cancel`，故不叫 `nav_keys`。动作值域在 `wind-config`，表在 `wind-keys`，两者由
    /// 本结构体所在的 crate 拼起来（唯一同时看得见两边的地方）。
    pub(crate) session_keys: keymap::KeyBinds<wind_config::SessionAction>,
    pub(crate) cn_pairs: Vec<(char, char)>,
    pub(crate) en_pairs: Vec<(char, char)>,
    /// 配对跳出键的 VK 码集合（预解析自 `auto_pair.jump_out_keys`，空=不启用）。
    pub(crate) jump_out_keys: std::collections::HashSet<u32>,
    /// 输入右符号本身是否跳出（`jump_out_keys` 含 `right_symbol`）。对称配对不受此项影响。
    pub(crate) jump_out_on_right_symbol: bool,
    /// 「英半列有自定义标点映射」的源字符集合（预解析自 `punct.custom_mappings`，空=英文模式
    /// 行为与历史一致）。这是 DLL 吃键与本侧出字的**同源判据**，且在英文标点键的热路径上每键
    /// 都要查——故预计算，别在按键时重新遍历 `custom_mappings`。有序集合使推送字节可复现。
    pub(crate) custom_en_punct_chars: std::collections::BTreeSet<char>,
}

/// 所有方案 `[key_actions]` 里绑过的纯修饰键 VK（并集）。
///
/// 取并集而非活跃方案那一份：`CompiledHotkeys` 随 activation 推给 C++，按活跃方案裁剪
/// 就得在每次切方案后重推，漏一次的表现是「刚切完方案这个键不灵、点下别的窗口又灵了」。
/// 并集是静态的，代价只是别的方案里多转发一个不动作的 keyup（keydown 侧纯修饰键一律
/// 放行，宿主无感）。理由详见 [`EngineManager::all_key_action_keys`]。
fn schema_bound_modifier_vks(mgr: &EngineManager) -> std::collections::BTreeSet<u32> {
    mgr.all_key_action_keys()
        .iter()
        .filter_map(|name| keymap::modifier_name_to_vk(name))
        .collect()
}

/// 加载期告警：`keys.session_actions` 里认不出的键名 / 动词。
///
/// ★ 静默忽略与「这个功能坏了」完全同形——用户无从分辨自己拼错了、还是该功能压根没实现。
/// 这是 `is_supported_key_action` 当初立的口径，本表沿用。
///
/// 分两条报而不是合并成一条：键名错与动词错的修法不同，合并后用户还要自己二选一去试。
fn warn_unknown_session_actions(config: &Config) {
    for (name, verb) in &config.keys.session_actions {
        if wind_config::SessionAction::parse_checked(verb).is_none() {
            warn!(
                "keys.session_actions[\"{name}\"] = \"{verb}\"：动词无法识别，该绑定被忽略。\
                 可选 page_prev / page_next / highlight_up / highlight_down / none",
            );
            continue;
        }
        if keymap::session_key_name_to_vk(name).is_none() {
            warn!(
                "keys.session_actions[\"{name}\"]：键名无法识别，该绑定被忽略。\
                 可选 tab / shift+tab / capslock / pageup / pagedown / up / down / left / \
                 right / home / end，以及符号键 minus / equal / lbracket / rbracket / \
                 comma / period / semicolon / quote / slash / backtick / backslash",
            );
        }
    }
}

impl ConfigBundle {
    /// `schema_bound_modifiers` = 所有方案 `[key_actions]` 里出现过的**纯修饰键** VK
    /// （见 [`Coordinator::schema_bound_modifier_vks`]）。它们要追加进 `key_up` 转发集，
    /// 否则 TSF 根本不把这些键的 keyup 送过来——`CompiledHotkeys` 编译自全局 config，
    /// 方案文件不在其中，这是 keyup 类绑定唯一的可达性来源。
    fn build(mut config: Config, schema_bound_modifiers: &std::collections::BTreeSet<u32>) -> Self {
        // 归一化 + 存量迁移。放在这里而不是只在 `Config::load()` 里：本函数是**所有**
        // 配置生效的必经之路（启动、热重载、RPC 改配置后的 `refresh_config_in_memory`、
        // 测试直接构造）。挂在 load 上会漏掉后三条——设置页保存一次就绕过了迁移，
        // 而消费点已改成只读新表，表现是「保存后引导键全失效」。`normalize` 幂等。
        config.normalize();
        let mut compiled_hotkeys = hotkey::Compiler::new(config.clone()).compile();
        // action 用专门的 `schema_bound` 而不是 `toggle_mode`：`is_toggle_mode_keycode` 按
        // action 过滤，混用会让「只在某方案里绑了 rshift」的键在所有方案里都切中英文
        // （与 `select_key_groups` 那次踩的是同一个坑，见该函数的 ⚠ 注释）。
        for vk in schema_bound_modifiers {
            // 修饰键的 hash 要带通用位+具体位，与 `compile_toggle_mode_key` 同构：
            // C++ `GetCurrentModifiers()` 对修饰键同时返回两者，只带一边匹配不上。
            if let Some(hash) = hotkey::compile_modifier_key_up_hash(*vk) {
                compiled_hotkeys.key_up.push(hotkey::HotkeyEntry {
                    tsf_hash: hash,
                    match_hash: hash,
                    action: "schema_bound".to_string(),
                });
            }
        }
        warn_unknown_session_actions(&config);
        // 会话态按键绑定。数据源是 `effective_session_actions()`＝四组键组配置的展开结果
        // ⊕ `session_actions`（后者优先）。
        //
        // ★ 合并只在这里发生，**配置文件里两套各自保持原样**——设置页的四个勾选框读的正是
        // 存储层，折算若写回存储，界面就永远显示为空。判据见该函数的文档。
        //
        // ★ 这里是两个 crate 的接缝：动作值域（`SessionAction`）在 `wind-config`，绑定表
        // （`KeyBinds`）在 `wind-keys`，而 `wind-config` 不能反向依赖 `wind-keys`（后者经
        // `wind-cmdbar` 依赖它，加进去成环）。本函数是唯一同时看得见两者的地方。
        //
        // 表**直接持有 `SessionAction`**，不再翻译成某个中间枚举——一期那层 `NavAction`
        // 映射在加 `cancel` 时立刻成了瓶颈（新动词没有对应的 `NavAction`）。
        // 显式 `none` 与写错的动词都在此过滤掉；后者由上一行的 `warn_unknown_session_actions`
        // 报出来，静默忽略与「功能坏了」完全同形。
        let effective_session = config.keys.effective_session_actions();
        let session_keys =
            keymap::KeyBinds::from_binds(effective_session.iter().filter_map(|(name, verb)| {
                let action = wind_config::SessionAction::parse(verb);
                action.is_enabled().then_some((name.as_str(), action))
            }));
        let cn_pairs = parse_pairs(&config.input.auto_pair.chinese_pairs);
        let en_pairs = parse_pairs(&config.input.auto_pair.english_pairs);
        let jump_out_keys = parse_jump_out_keys(&config.input.auto_pair.jump_out_keys);
        let jump_out_on_right_symbol =
            parse_jump_out_on_right_symbol(&config.input.auto_pair.jump_out_keys);
        // 英文模式下需要 DLL 吃下转发的标点键 = 「配了英半列自定义」∪「英文智能符号参与集」。
        // 两个来源都是「英文半角下 DLL 默认透传、core 却需要收到」的键，合并成一份推送即可
        // （DLL 侧判据是数据驱动的字符集查表，集合变大自动多吃，无需改 C++）。
        let custom_en_punct_chars: std::collections::BTreeSet<char> =
            wind_punct::custom_english_punct_chars(&config.input)
                .into_iter()
                .chain(wind_punct::english_smart_source_chars(&config.input))
                .collect();
        Self {
            config,
            compiled_hotkeys,
            session_keys,
            cn_pairs,
            en_pairs,
            jump_out_keys,
            jump_out_on_right_symbol,
            custom_en_punct_chars,
        }
    }
}

/// 当前焦点进程派生的 caret 兼容态，字段取自 `compat.toml` 的 `[[apps]]` 规则。
///
/// focus_gained / ime_activated 时按 `client_token` 高 32 位的 PID 解析进程名并缓存
/// （见 `update_active_compat`），避免每次 caret 更新重复 OpenProcess。
///
/// 用命名结构体而非元组：两个 bool 语义完全不同，`(u32, bool, bool)` 的 `.1`/`.2`
/// 在调用点无从分辨——本仓已有多次「下标/名字与实际语义脱节」的返工。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ActiveCompat {
    /// 已解析的焦点进程 PID（0 = 尚未解析，此时其余字段无意义）。
    pub(crate) pid: u32,
    /// 用 caret rect 的 top 而非 bottom 定位候选窗。微信等 WebView 宿主的 GetTextExt
    /// height 在 1↔20px 间跳变致 bottom 漂移，top 稳定。
    pub(crate) caret_use_top: bool,
    /// 候选窗首显策略（见 `AppCompatRule::first_show_mode`）。三档互斥。
    pub(crate) first_show_mode: wind_config::app_compat::FirstShowMode,
    /// 本进程是否配了初始状态规则（`initial_mode` / `initial_punct` 任一非空）。
    ///
    /// 用途是判定「本次焦点切换是否**进出**了规则应用」：规则的副作用必须严格限制在
    /// 规则应用的进出，不能外溢。若判据退化成「规则表非空」，那么只要用户配过任意
    /// 一条规则，**任意两个应用之间**的切换都会触发重算——`global + remember=false`
    /// （出厂默认）下这会把模式重置成配置默认，用户在 Word 手切的英文切到 Chrome
    /// 就没了，与 Everything 毫无关系。
    pub(crate) has_initial_rule: bool,
    /// 本进程的符号自动配对开关；`None` = 跟随全局 `input.auto_pair.*`。
    ///
    /// ⚠ 消费点三条，缺一即半截修复（见 `AppCompatRule::auto_pair` 的说明）：中文标点态、
    /// 英文标点流水线、以及推给 DLL 的英文配对配置——纯英文模式的配对完全在 C++ 侧独立
    /// 处理，协调器收不到那些键，只关前两条的话切到英文模式配对照旧。
    pub(crate) auto_pair: Option<bool>,
    /// 本进程的智能符号替换方案；`None` = 跟随全局 `input.symbol.smart_method`。
    pub(crate) smart_method: Option<wind_config::config::SmartMethod>,
    /// 光标坐标校正偏移（像素，正=右/下）。宿主报告的 caret 系统性偏移时用，
    /// 与 `caret_use_top` 在同两处消费（`apply_focus_caret` / `handle_caret_update`）。
    pub(crate) caret_offset_x: i32,
    pub(crate) caret_offset_y: i32,
}

/// 焦点切换时是否需要重算初始状态（即是否调用 `apply_initial_mode`）。
///
/// 抽成模块级纯函数是为了能直接单测这个判据本身。内联在 `handle_focus_gained` 里时，
/// 唯一的覆盖方式是构造完整 `FocusData` 并走那条带 UI/IPC 副作用的路径，于是「门控条件
/// 写错」这类缺陷极易漏网——本仓已有多次「门控退化后测试仍全绿」的先例。
///
/// - `crossed`      焦点是否**跨进程**切入。同应用内的焦点跳转为 false，否则用户手切的
///                  模式会在换输入框时被拉回初始值（「初始值」与「锁定」的分界线）。
/// - `per_app`      `state_scope="app"` 的既有按应用记忆语义。
/// - `old_has_rule` / `new_has_rule`
///                  切换前后的进程是否配了 compat.toml 初始状态规则。两者**取或**，使规则
///                  同时覆盖「进入规则应用」和「离开规则应用」两个方向：只看 new 会让从
///                  Everything 切出去后英文状态残留给下一个应用；而放宽成「规则表非空」
///                  又会让任意两个无规则应用之间的切换也重算，把用户手切的状态冲掉。
pub(crate) fn should_reapply_initial(
    crossed: bool,
    per_app: bool,
    old_has_rule: bool,
    new_has_rule: bool,
) -> bool {
    crossed && (per_app || old_has_rule || new_has_rule)
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
    /// 码表自动造词的连续单字缓冲。**独立于 `State`**：终止信号多来自 IPC 回调
    /// （焦点丢失 / IME 停用 / 光标移动），那些路径不持 `state` 锁，塞进 `State` 会
    /// 逼出跨锁调用。见 `auto_phrase` 模块头注释。
    pub(crate) auto_phrase: Mutex<crate::auto_phrase::AutoPhraseBuf>,
    /// 最近一次**本输入法自己**向宿主吐字的时刻（由 `commit_action` 统一打点）。
    ///
    /// 用途只有一个：宿主插入我们提交的文字后会回送 `SelectionChanged`，它和「用户真的
    /// 移动了光标」在协议层**长得一模一样**，只能靠时间区分。若不区分，每上屏一个字就会
    /// 被自己的回声判成「用户移动光标」→ flush → 缓冲永远只有 1 个字 → 造词恒不触发。
    ///
    /// **打点必须收口在 `commit_action` 一处**：漏掉任一吐字路径，该路径的回声就会切碎序列。
    pub(crate) last_self_commit: Mutex<Option<std::time::Instant>>,
    /// 自动造词写入计数，供临时词库淘汰按次节流（见 `maybe_evict_temp`）。
    pub(crate) auto_phrase_writes: std::sync::atomic::AtomicUsize,
    /// CapsLock 全局低级键盘钩子。
    ///
    /// ★ **只有用户在 `keys.session_actions` 里真的配了 `capslock` 时才是 `Some`**。没配的
    /// 用户进程里根本不存在全局键盘钩子——这是本功能唯一的风险控制手段（用户明确要求）。
    ///
    /// 为什么非钩子不可：CapsLock 的锁定态由系统在 TSF **之前**维护，`pfEaten` 压不住；
    /// 而「让它翻转再回敲复原」在快速连按下有竞态（大写会卡住），还会被厂商 OSD 工具
    /// 观测到并弹窗。详见 `wind_keys::capslock_hook` 模块文档。
    pub(crate) capslock_hook: Mutex<Option<wind_keys::capslock_hook::CapsLockHook>>,
    /// 钩子线程 → 动作消费线程的投递口。
    ///
    /// 在 `new` 里就建好并起好消费线程（那里才有 `Arc<Self>`），钩子装卸只是复用它。
    /// 消费线程空闲时阻塞在 channel 上，未装钩子时零开销。
    capslock_press_tx: std::sync::mpsc::Sender<()>,
    /// 短语层（系统+用户，来自 store，仅 enabled）。变更后可 rebuild_phrases 重建。
    pub(crate) phrases: std::sync::RwLock<wind_phrase::PhraseLayer>,
    /// 最近一次解析的系统短语条目（启动时填充；"恢复默认"重读文件成功后刷新）。
    /// 作为重读失败（文件缺失/TOML 语法错误）时的回退，避免把库里系统短语清空。
    pub(crate) system_phrase_entries: std::sync::RwLock<Vec<wind_phrase::SystemPhraseEntry>>,
    /// system.phrases.toml 路径（None=无 data_dir，如 headless 测试）。
    /// "恢复默认"据此重读文件，使手工编辑无需重启服务即可生效。
    pub(crate) system_phrase_path: Option<std::path::PathBuf>,
    /// 简繁转换器（OpenCC；None=数据缺失不可用）。变体由配置 features.s2t.variant 决定，
    /// 启动时加载；菜单仅提供开/关。置于 Mutex 兼容 reload 时整体替换。
    pub(crate) s2t: Mutex<Option<wind_transform::s2t::Converter>>,
    /// 通用规范汉字表（检索范围"常用字"判定；空集时退化为不过滤）
    pub(crate) common_chars: wind_candidate::CommonChars,
    // Shadow 规则已迁至 redb（self.store 的 SHADOW 表）。
    /// 工具栏位置，按显示器 key（"workRight,workBottom"）独立记录。
    pub(crate) toolbar_positions: Mutex<std::collections::HashMap<String, (i32, i32)>>,
    /// 工具栏当前所在显示器的 key（None=尚未定位）。`sync_toolbar_monitor` 的去重依据：
    /// notify_toolbar 在每次模式切换/焦点事件上都跑，无此缓存就会把用户拖动过的位置
    /// 反复重置回记忆值。拖动落盘时（`save_toolbar_pos`）同步更新，否则拖到别的屏之后
    /// 这里仍记着旧 key，下一次校正会被误判为「屏没变」而跳过。
    pub(crate) current_toolbar_monitor: Mutex<Option<String>>,
    /// 候选反查（编码/拆字/拼音）供悬停提示与加词出码；拆字段随主码表方案
    /// 热重载（见 `sync_chaizi_assets`），拼音段启动加载后不变。
    pub(crate) reverse: std::sync::RwLock<wind_reverse::ReverseLookup>,
    /// 快捷输入格式表（`system.quick.toml`，支持用户目录整份覆盖）。
    ///
    /// 启动加载后不变，故无锁：与 `system.phrases.toml` 同语义——**改完必须重启服务**，
    /// 全仓的覆盖点都没有文件监视器。加载失败已在 `FormatTable::load` 内回落内置默认表，
    /// 此处恒是一张可用的表。
    pub(crate) quick_formats: wind_quick_input::FormatTable,
    /// 拆字资产当前生效状态（库解析路径 / 已下发字根字体），reload 变更检测用。
    pub(crate) chaizi_assets: Mutex<ChaiziAssets>,
    /// 注释词库当前生效的解析路径列表（顺序即优先级），reload 变更检测用。
    /// 见 `sync_comment_dicts`。
    pub(crate) comment_dict_paths: Mutex<Vec<std::path::PathBuf>>,
    /// 标点配对跟踪栈（用于智能跳过）；中/英配对表在 rt bundle 内。
    pub(crate) pair_tracker: Mutex<wind_transform::pair_tracker::PairTracker>,
    /// 最近一次有效光标坐标 (x,y,height)；用于无效坐标时回退，避免候选窗跑到左上角
    last_valid_caret: Mutex<(i32, i32, i32)>,
    /// 延迟首次显示：新组合首帧不立即显示候选窗，待 handle_caret_update 收到 reflow 后的权威坐标、
    /// 或兜底 timer 超时再首显，避免在 reflow 前的陈旧坐标处先显示再跳（对齐 Go pendingFirstShow）。
    pending_first_show: Mutex<bool>,
    /// 上述兜底 timer 的代际令牌：每次 arm 自增，超时回调比对以作废被新按键取代的旧 timer。
    pending_first_show_token: Mutex<u64>,
    /// 本次组合候选窗是否已首次显示过（true=后续刷新可立即下发；false=首帧需延迟）。
    candidate_shown: Mutex<bool>,
    /// 显示授权：handle_caret_update / 兜底 timer 在调 notify_ui_update 前置位以放行首帧显示；
    /// 按键路径不置位，首帧改为 arm 延迟。notify_ui_update 内 swap 消费。
    show_authorized: std::sync::atomic::AtomicBool,
    /// 候选窗当前是否正在**反转排列**候选项（`ui.candidate.flip_when_above` 真正生效）。
    ///
    /// 由 UI 侧 `UiEvent::CandidateFlipped` 单向写入：判据要窗口尺寸 + 屏幕工作区才算得出
    /// （还叠加模式级强制横/竖排），协调器读配置推不出来，故只镜像不推导。
    /// 消费点唯一：[`Coordinator::apply_session_action`] 用它把 `highlight_up`/`highlight_down`
    /// 的走向翻过来，见那里的说明。
    candidate_flipped: std::sync::atomic::AtomicBool,
    /// 鼠标悬停目标（原始 tag）：-1 无，0..N 候选页内下标，或翻页器 tag。
    /// 与 `State::selected_index` 相互独立：悬停只是视觉提示，不改变空格上屏的目标。
    ///
    /// # ★★ 为什么不放在 `State` 里
    ///
    /// 它的生命周期是**候选窗会话**（窗口一隐藏就该归零），不是输入状态。放在 `State` 里时，
    /// 清空只能由每个候选装填点手工执行——主路径 `update_candidates` 做了，
    /// 特殊模式 / 临拼 / 混输 / 快捷输入的 8 个装填点全部漏了，于是悬停高亮与 tooltip 跨组合、
    /// 跨模式存活（用户 2026-08-12 反馈「再次弹出时悬停被记忆」）。普通输入下每敲一键都重走
    /// 主路径，残留被持续覆盖掉，**故该缺陷在主路径上物理不可观测**。
    ///
    /// 移出为原子量后，[`Coordinator::clear_hover`] **不需要 state 锁**，才能安放进
    /// [`Coordinator::notify_ui_hide`]——那里有 40+ 个调用点，无法逐一确认是否已持锁，
    /// 加锁即埋死锁。「窗口隐藏即清空悬停」这句话至此才在真相源上成立，而不只在 UI 侧的
    /// 防抖状态（`CandidateMouse::reset_hover`）上成立。
    hover_index: std::sync::atomic::AtomicI32,
    /// 本轮组合的首显是否用了**非权威**坐标（fast 的试探采样 / instant 沿用的旧坐标）。
    /// 置位后，该轮第一次权威坐标到达时改用放宽的容差判断要不要校正——校正动作本身
    /// 才是抖动的观感来源，小偏差不动比「跳一下修正」更稳。组合结束时复位。
    pub(crate) first_show_was_provisional: std::sync::atomic::AtomicBool,
    /// 坐标缓存是否已被**当前插入点**验证过（= `state.caret_*` 还算不算数）。
    ///
    /// `fast` 档短兜底的隐含前提是「手里的旧坐标 ≈ 当前插入点」——同一行连打时它只差一个
    /// 字宽，所以拿它首显毫无问题。本标志就是那个前提的显式化：
    ///
    /// - **置位**：[`Coordinator::handle_caret_update`] 采纳一帧权威坐标（与
    ///   `last_authoritative_caret` 同一处，同一条「够格当基准」的判据）。
    /// - **清位**：焦点到达（换 DocMgr，坐标属于上一个文档/单元格/应用）、
    ///   用户移动光标（[`Coordinator::handle_selection_changed`] 的非回声分支，
    ///   同一 DocMgr 内点到别处）。
    ///
    /// 清位后 `fast` 的 25ms 短兜底会退让为 [`FIRST_SHOW_LONG_FALLBACK_MS`] 长兜底（判据在
    /// [`Coordinator::arm_pending_first_show`]）：此时「快」没有意义，只会把候选窗快速显示
    /// 到一个错误位置、再当着用户的面跳回来。
    ///
    /// ⚠ **不复用 `last_authoritative_caret.2`**：那个字段回答的是「有没有可比的基准值」
    /// （probe 判据用），本字段回答「手里的值可不可信」。当前取值恰好一致，但两者对边缘
    /// 输入的期望会分化，合用一个必有一方错。
    caret_cache_verified: std::sync::atomic::AtomicBool,
    /// 本轮组合的首显是否已进入「长兜底等待」（首帧信任门命中）。
    ///
    /// 唯一用途是让后续按键**不重置**那段等待的计时——见
    /// [`Coordinator::arm_pending_first_show`] 里对该死结的说明。`reset_first_show` 复位。
    first_show_extended: std::sync::atomic::AtomicBool,
    /// `ui.status.show_on_focus` 的焦点气泡正等一个 TSF 权威坐标。
    ///
    /// 焦点事件到达时坐标常常还只是 GUI 回退值（`OnSetFocus` 拿不到同步 edit session 锁），
    /// 直接拿它定位就是用户反馈的「还没输入时定位非常不准」。故置位挂起，由
    /// [`Coordinator::handle_caret_update`] 在权威坐标到来时消费并补显示。
    ///
    /// **刻意不配兜底 timer**：超时后能做的只有「拿不可信坐标显示」，正是本机制要挡的事。
    /// 等不到就不显示，失焦/下一次焦点事件清位。
    pending_focus_tip: std::sync::atomic::AtomicBool,
    /// 上一次弹过焦点气泡的宿主（`client_token`，DLL 实例级 = 每进程一个）。
    ///
    /// **气泡的语义是「切到了新的输入宿主」，不是「换了 docMgr」**。一个宿主内部可以有多个
    /// docMgr 并频繁互切：Excel 在单元格里起输入时切一次、输入完焦点落到公式编辑栏又切一次，
    /// 若按 docMgr 计就成了「输入一次闪两下」（同一单元格内连续输入反而不闪，因为中途不换
    /// docMgr）——这个「闪的时机与用户的操作节奏对不上」正是它扰人的原因。
    /// 故以 token 去重：同 token 只在首次进入时弹，离开该宿主（`FocusLostReason::Thread`）时清零。
    last_focus_tip_token: Mutex<u64>,
    /// 上一次按键时刻，仅用于算出下面那个「相邻按键间隔」。
    pub(crate) last_key_at: Mutex<Option<std::time::Instant>>,
    /// **相邻两次按键**的间隔（毫秒），fast 档据此判断是否处于连续快速输入。
    ///
    /// ⚠ 必须是「按键与按键之间」，不能用 `last_key_at.elapsed()`——后者是「距上次按键多久」，
    /// 而试探坐标恒在按键后 10ms 内到达，那个条件永远成立、判据会被完全绕过。本功能就这么
    /// 空跑过一轮：日志里 163 次全报「连续输入 7~13ms」，而实际脚本节奏是 60ms。
    pub(crate) last_key_interval_ms: Mutex<Option<u64>>,
    /// 上一轮组合最终采纳的**权威** caret 坐标 (x, y, valid)，供首显试探采样做判据。
    ///
    /// 为什么这个能当判据：首帧 reflow 未完成时，宿主的 GetTextExt 返回的正是上一轮那个
    /// 位置（实测 WPS 连续两次返回上一轮终值，第三次才更新）；而真正 reflow 之后，光标
    /// 必然因新插入的组合内容而移动。所以「与上一轮权威坐标不同」≈「宿主已经 reflow」。
    /// 误判方向是安全的：判成「未 reflow」只是退回等 debounce（慢而不错）。
    pub(crate) last_authoritative_caret: Mutex<(i32, i32, bool)>,
    /// 组合起点屏幕坐标 (x, y, valid)：嵌入预编辑模式（编码插入宿主、光标随输入右移）下候选窗锚此处
    /// （缓冲头部），不随输入移动。同一组合只锁定首个有效值（handle_caret_update），组合结束复位。
    composition_start: Mutex<(i32, i32, bool)>,
    /// 应用兼容规则表（compat.toml，系统层 + 用户层覆盖）。按焦点进程名查规则。
    ///
    /// 用 Mutex 而非不可变字段：右键菜单切换 per-app 开关后要写用户层并**立即重载**。
    /// 只更新 `active_compat` 缓存是不够的——切到别的应用再切回来时 pid 变化两次，
    /// `update_active_compat` 会拿这张表重新解析，用旧表就会把刚才的切换悄悄回滚。
    pub(crate) app_compat: Mutex<wind_config::app_compat::AppCompat>,
    /// 启动时的 (系统数据目录, 用户配置目录)，供 compat.toml 热重载复用同一口径。
    /// 不用 `Config::data_dir()` 等静态函数：便携版/测试会传入自定义路径，静态函数
    /// 拿到的是默认安装位置，重载后规则会与初次加载不一致。
    pub(crate) compat_dirs: (Option<std::path::PathBuf>, Option<std::path::PathBuf>),
    /// 当前焦点进程派生的 caret 兼容态，见 [`ActiveCompat`]。
    pub(crate) active_compat: Mutex<ActiveCompat>,
    /// pid → 进程名（小写）缓存，`update_active_compat` 填充，会话级只增不清。
    /// 供 FOCUS_GAINED 同步路径（`get_current_mode`）免 OpenProcess 查询进程名。
    pub(crate) pid_names: Mutex<HashMap<u32, String>>,
    /// 按应用独立中英状态表（`input.default.state_scope = "app"` 时启用）：
    /// 进程名（小写）→ chinese_mode，会话级记忆（服务重启即清，见计划决策）。
    mode_states: Mutex<HashMap<String, bool>>,
    /// 用户最后一次主动切换后的 (中英, 全半角, 中英标点) 内存镜像；
    /// remember_last_state=true 时随切换同步落盘 state.toml（`record_last_state`）。
    runtime_last: Mutex<(bool, bool, bool)>,
    /// 最近一次 CapsLock 取消注入的时刻（`cancel_caps_on_switch` 冷却，防振荡回路放大）。
    last_caps_inject: Mutex<Option<std::time::Instant>>,
    /// 前台上下文快照 `(app, title, sel)`，供命令直通车 app()/title()/sel() 取值。
    /// darwin `.app` 经 CMD_FRONT_CONTEXT 于聚焦时上报；其它平台暂空。
    front_ctx: Mutex<(String, String, String)>,
    /// 主题目录（data/themes）
    pub(crate) themes_dir: Option<std::path::PathBuf>,
    /// 当前主题名
    pub(crate) theme_name: Mutex<String>,
    /// 主题颜色风格：0=跟随系统 1=亮色 2=暗色
    pub(crate) theme_style: Mutex<ThemeStyle>,
    /// 状态气泡上一次显示的文本，用于抑制"内容没变却重复弹窗"。
    /// 关掉某个内容段后（如全半角），切换该状态不再改变气泡文本，此时应当整个不弹窗。
    /// 在 `show_status` 做文本比对而非判断"这次变的是哪个字段"，是因为后者要给全部
    /// 十余个调用点传参，而文本比对一处生效、且将来新增状态项零成本。
    pub(crate) last_status_text: Mutex<String>,
    /// `toggle_schema:<id>` 的**来源**：`(从哪个方案按进来, 写入时的方案变更代际)`。
    ///
    /// 刻意只存运行时、不落配置：它描述的是「用户此刻的往返意图」，不是偏好。持久化会让
    /// 重启后第一次按跳到一个用户早忘了的方案——那正是「回到来源」这个语义最容易失信的
    /// 时刻。无有效来源时按 `toggle_schema` 到已在的方案是 no-op（不切走）。
    ///
    /// # 为什么带代际，而不是在切方案时清空
    ///
    /// 切 active 方案在协调器侧有**五条路径**（循环键 / 直达热键 / 命令栏 / 菜单
    /// `select_schema` / 设置页 RPC），其中只有两条走 `finish_user_schema_switch`——
    /// 那个"统一收尾"从来就没统一到全部。散点补清空必漏，且漏掉的表现是「往返键把人送回
    /// 几步之前的方案」，低频且难复现。
    ///
    /// 改为记下写入时 `EngineManager::schema_generation()` 的值，读取时比对是否仍相等：
    /// 期间**任何**路径切过方案，代际就对不上，来源自动失效。零散点接线。
    ///
    /// 只比对方案 id 是不够的——「切走又切回来」与「从未变过」在 id 上完全同形。
    ///
    /// 第三项是**触发键 VK**（0 = 非方案级绑定触发，如全局热键）。有它，回程才真正
    /// 「不依赖目标方案的配置」：去程后该键在目标方案里临时获得回程语义，哪怕目标方案
    /// 的 `[key_actions]` 是空的。
    ///
    /// ★ 没有这一项时，「五笔按 RShift 去英文方案」要求英文方案**自己也配一遍** RShift
    /// 才回得来——设计文档 §5 原本断言 `toggle_schema` 对锁死「从结构上免疫」，那只覆盖了
    /// 「回到哪」，没覆盖「怎么按得动」。测试里复现过。
    pub(crate) schema_toggle_origin: Mutex<Option<(String, u64, u32)>>,
    /// 当前主题定义的序号槽位字符（views.index.labels）；push_theme 载入时刷新。
    /// 序号优先级：用户配置 index_labels > 本字段 > 默认数字。
    pub(crate) theme_index_labels: Mutex<Vec<String>>,
    /// 命令栏（cmdbar）服务束（ime/config/dict 等动作后端），构造后由 init_cmdbar 装配。
    pub(crate) cmdbar_services: std::sync::OnceLock<wind_cmdbar::Services>,
    /// 自身 Weak 引用：$CC 命令在独立线程异步执行（避免持 state 锁回调自锁方法致死锁）。
    pub(crate) self_weak: std::sync::OnceLock<std::sync::Weak<Coordinator>>,
    /// 上屏历史环形缓冲（index 0 = 最近）：供命令栏 `last(n)` 取最近上屏文本。
    pub(crate) recent_commits: Mutex<std::collections::VecDeque<String>>,
    /// 撤销上屏（`ime.undo_commit`）删除量：最近一次「同步落到光标前」的字符数（UTF-16 单元，
    /// 与 TSF ShiftStart / macOS NSRange 同量纲）。**刻意与 `recent_commits` 分离**——历史队列
    /// 记「上过什么」（供 last/加词，深度 16），本值记「光标前紧邻的还是不是它、有几个字」这一
    /// 时效态。默认 1 → undo 永远有动作；每次上屏经 `note_commit_action` 覆盖 → 只有「刚输入完
    /// 那次」精准删多个；撤销一次即复位 1、焦点变化亦复位 → 之后回落删 1（宁可少删多按几次，
    /// 也不按陈旧计数误删多个）。
    pub(crate) last_commit_len: std::sync::atomic::AtomicUsize,
    /// 编码显示方式运行时态（命令栏 ime.toggle("preedit") 循环切换；初值随配置）。
    /// 统一权威：决定候选窗是否显示 preedit（in_app→不显示）及是否内联首单元（embedded）。
    pub(crate) preedit_display: Mutex<PreeditDisplay>,
    /// 候选窗隐藏开关（命令栏 ime.toggle("candwin") 切换；隐藏时 notify_ui_update 不显示候选）。
    hide_candidate_window: Mutex<bool>,
    /// 候选布局方向运行时态（命令栏 ime.toggle("layout") 切换；true=竖排，初值随配置，持久化）。
    ///
    /// 这是布局方向的**基线真相源**——模式级覆盖（`layout.rs`）在它之上叠加，不改写它。
    pub(crate) candidate_vertical: Mutex<bool>,
    /// 上次真正下发给 UI 的候选方向（`layout.rs` 的去重缓存，避免每次按键重发致重排抖动）。
    /// 与 `candidate_vertical` 的区别：后者是基线，本字段是**叠加模式意图后实际生效**的值。
    pub(crate) candidate_layout_sent: Mutex<bool>,
    /// 输入统计采集器（内存聚合 + 后台 flush，与 store 共享 Arc）；None=无持久化/headless。
    pub(crate) stat_collector: Option<StatCollector>,
    /// 本次按键是否已被具体上屏路径记录统计（AtomicBool，避免与 state 锁冲突致死锁）。
    pub(crate) stat_recorded: std::sync::atomic::AtomicBool,
    /// 全屏状态缓存：由 notify_toolbar_async 在后台线程异步刷新，notify_toolbar 直接读取，
    /// 消除 bridge handler 线程上的 SHQueryUserNotificationState 阻塞。
    pub(crate) fullscreen_cached: std::sync::atomic::AtomicBool,
    /// 全屏探测的单飞闸：已有探测在途时跳过新的。焦点变化是成串来的，而探的是同一个
    /// 全局前台状态，此前每次都 spawn 一个线程。见 `notify_toolbar_async`。
    pub(crate) fullscreen_probing: std::sync::atomic::AtomicBool,
    /// host-render 管理器（Windows）：与 `BridgeServer` 共享同一 `Arc` 实例。
    /// 服务入口经 `set_host_render` 注入一次；Task 6/7 据此写候选/工具提示/状态帧并隐藏。
    /// 采用 `OnceLock`（与 `self_weak`/`cmdbar_services` 同一构造后注入惯例），
    /// 避免为其贯穿 `new`/`new_headless` 等构造器签名。
    #[cfg(windows)]
    #[allow(dead_code)] // Task 6/7 接线写帧/隐藏后即被读取
    host_render: std::sync::OnceLock<Arc<wind_bridge::host_render_windows::HostRenderManager>>,
    /// 最近一次输入诊断快照（compartment 禁用态 / InputScope 密码位），供 Task 6 HUD 展示。
    pub(crate) last_input_diag: Mutex<crate::input_diag::InputDiagState>,
    /// 最近一次窗口 / TSF 上下文诊断快照（`CMD_DIAG_SNAPSHOT`）。
    /// 与 `last_input_diag` 分开存：两者上报时机不同，合成一个就得回答「只到了一半算什么」。
    pub(crate) last_window_diag: Mutex<crate::input_diag::WindowDiagView>,
    /// 密码框强制英文抑制态：命中密码 InputScope 时置 true，输入闸据此强制英文透传
    /// （**不改 `chinese_mode` 持久值**）。
    ///
    /// 呈现：2026-08-04 起工具栏模式格显 "英" 且不高亮（`ToolbarState::password_suppress`），
    /// TSF 语言栏图标同样显 "英"（C++ 侧本地判 `IsPasswordSuppressActive`，不经 IPC）。
    /// 此前的「图标保持不变」是对齐 Go 旧版的决策，已按用户反馈推翻——图标显方案标签
    /// 而键已被全放行，用户无从知道自己打不出中文。
    /// ⚠ 呈现与输入闸是两条独立的路：改这里的展示**不会**改变是否抑制，反之亦然。
    pub(crate) password_suppress: std::sync::atomic::AtomicBool,
    /// 密码框抑制策略开关（默认 true）；关闭时 `apply_input_diag` 不再置位 `password_suppress`。
    pub(crate) password_suppress_enabled: std::sync::atomic::AtomicBool,
    /// 输入诊断 HUD 是否可见（Task 6/7 接线；本任务先占位默认 false）。
    pub(crate) input_diag_hud_visible: std::sync::atomic::AtomicBool,
    /// HUD 分区显示开关（右键菜单「显示分类」）。会话级，不持久化。
    pub(crate) input_diag_sections: Mutex<wind_ui::manager::DiagSections>,
    /// HUD 冻结中（右键菜单「停止刷新」）：新快照不再推给 UI。
    ///
    /// 冻结落在**推送**这一层而不是 UI 渲染层：数据照常进 `last_*_diag`（解冻后立即有
    /// 最新值），只是不往屏幕上送。若改在 UI 侧丢弃，解冻后得等下一次焦点事件才恢复。
    pub(crate) input_diag_frozen: std::sync::atomic::AtomicBool,
    /// HUD 窗口置顶（右键菜单）。默认开——诊断浮窗被盖住就失去意义。
    pub(crate) input_diag_topmost: std::sync::atomic::AtomicBool,
}

/// 拆字资产当前生效状态：库的解析后绝对路径 + 已下发的字根字体（路径, DWrite 家族名）。
/// 变更检测用——库变了才重载反查表，字体变了才重发（渲染端每次 set 都重建字体集）。
#[derive(Default)]
pub(crate) struct ChaiziAssets {
    pub(crate) db: Option<std::path::PathBuf>,
    pub(crate) font: Option<(String, String)>,
}

/// 一次候选刷新后的输入结局（码表全码/空码策略，仅正向输入字母时消费）。
pub(crate) enum InputOutcome {
    /// 正常更新候选，继续组合。
    Normal,
    /// 全码自动上屏该文本。
    AutoCommit(String),
    /// 全码唯一命中含副作用 `$CC` 命令：清组合并异步执行（无同步上屏文本，
    /// 语义与空格选中命令一致，见 `commit_command`）。
    AutoCommand(Box<Candidate>),
    /// 满码空码：清空缓冲。
    Clear,
}

impl Coordinator {
    /// 生产构造器：从 exe 同目录加载配置，启动候选窗口 UI 线程
    pub fn new(push_server: Arc<PushServer>) -> Arc<Self> {
        let data_dir = Config::data_dir();
        let config = Config::load(data_dir.as_deref()).unwrap_or_default();
        info!("Active schema: {}", config.active_schema());

        // UI 管理器（候选窗口线程）。
        // macOS 无进程内窗口：把 UiCommand 喂给 host-render forwarder，光栅化进 POSIX SHM
        // 再经 push 管道推帧给 .app。其余平台走 Windows LayeredWindow 的 UiManager。
        #[cfg(target_os = "macos")]
        let (ui_tx, event_rx) = {
            let (tx, rx) = std::sync::mpsc::channel::<UiCommand>();
            // 候选/菜单的**鼠标**交互确实经 push/bridge 协议从 .app 回流，不走这里；
            // 但进程内仍有 UiEvent 源——全局热键由服务进程自己注册（语义要求本输入法
            // 未激活时也生效，.app 只在被 IMK 拉起后才在），触发后经本通道回协调器。
            // 后续拖动落点回报（CandidateWindowMoved / StatusTipMoved）等也走这条。
            let (ev_tx, ev_rx) = std::sync::mpsc::channel::<UiEvent>();
            let sink: Arc<dyn wind_bridge::HostRenderSink> = push_server.clone();
            let suffix = push_server.suffix().to_string();
            if let Err(e) = std::thread::Builder::new()
                .name("ui-forwarder-macos".into())
                .spawn(move || wind_ui::manager_macos::forwarder_thread(rx, ev_tx, sink, suffix))
            {
                warn!("Failed to spawn macOS host-render forwarder: {}", e);
            }
            (tx, Some(ev_rx))
        };
        #[cfg(not(target_os = "macos"))]
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

        // 用户配置目录（%APPDATA%\WindInput）：config.toml / userdata.redb / 词频等用户偏好。
        let user_dir =
            Config::user_config_dir().or_else(|| data_dir.as_deref().map(|d| d.to_path_buf()));
        // redb 用户数据库（用户偏好数据：词频、自定义词、shadow 规则，应随用户漫游）。
        let store = user_dir.as_deref().and_then(|d| {
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
            None, // 生产路径：override 目录由 EngineManager 取用户配置目录下的默认值
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

        // 注册 keys.global_hotkeys 全局热键（RegisterHotKey）：启动即注册，
        // 不依赖 IME 激活——全局热键的语义就是在本输入法未激活时也生效。
        coordinator.sync_global_hotkeys();

        // CapsLock 全局钩子：仅当 keys.session_actions 里配了 capslock 才安装。
        // （动作消费线程已在内部构造函数里起好，见那里。）
        coordinator.sync_capslock_hook();

        // 同步 activate_ime 到 DirectSwitchHotkeys 注册表：同样启动即同步（该热键的
        // 语义就是在本输入法未激活时切换过来），且不依赖 UI 线程创建成功
        // （Go 版把同步放在 UI 回调装配里，UI 创建失败会静默跳过——已规避）。
        coordinator.sync_direct_switch_hotkey();

        // 后台预热：提前构建其余方案的引擎与缓存（拼音 merged/unigram、码表 per-dict），
        // 避免首次切换到拼音/临时拼音/码表时同步重熔大词库造成几十秒卡顿。
        // single-flight 构建锁保证预热与用户切换不重复构建；按方案顺序逐个建（后台低频）。
        {
            let c = Arc::clone(&coordinator);
            std::thread::spawn(move || {
                let active = c.engine_mgr.active_schema_id();
                // available_schemas 只含「可切换的方案」。临时拼音 / 临时英文的目标引擎
                // **不在其中**（它们是模式的实现，不是可切换方案），此前因此漏出预热范围：
                // 实测首次按引导键进临拼时才同步加载 52 万词条的拼音库 + 英文库，用户感到
                // 顿一下。两者都只在启用时才预热，不给没开这些功能的用户白付内存。
                let mut targets: Vec<String> = c.engine_mgr.available_schemas().to_vec();
                // ⚠ `temp_pinyin_target()` **自身就会 `ensure_loaded`**（它的语义是「可用才
                // 返回」），故这一行本身即完成了临拼引擎的加载，下面循环里那次只是复查跳过。
                // 看着绕，但比在此复制一份「开关 + 方案适用性 + 目标解析」的判据强——那套判据
                // 是所有临拼入口的公共门卫，抄一份必然漂移。
                if let Some(t) = c.engine_mgr.temp_pinyin_target() {
                    targets.push(t);
                }
                if c.rt().config.input.temp_english.show_candidates {
                    targets.push("english".to_string());
                }
                for id in targets {
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

        // 恢复持久化的工具栏位置（按前台窗口所在显示器的 key 查找）。
        // 与运行期换屏走同一个函数——判据分成两套迟早漂移。
        coordinator.init_toolbar_pos();

        // 加载并下发初始主题。明暗必须走 resolve_theme_dark（system 实时探测系统明暗），
        // 不能硬编码 false——否则跟随系统在**冷启动**这一刻永远回落亮色（实时跟随另有 WM_SETTINGCHANGE
        // 路径，故只在启动瞬间错、切一次系统主题就"自愈"，与 theme_style 的其余消费点保持同一出口）。
        let name = coordinator
            .theme_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        coordinator.push_theme(&name, coordinator.resolve_theme_dark());
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
        let _ = coordinator.ui_tx.send(UiCommand::SetCandidateSwapWhenAbove(
            rt0.config.ui.candidate.swap_preedit_when_above,
        ));
        let _ = coordinator.ui_tx.send(UiCommand::SetPagerInPreedit(
            rt0.config.ui.candidate.pager_in_preedit,
        ));
        let _ = coordinator
            .ui_tx
            .send(UiCommand::SetTooltipDelay(rt0.config.ui.tooltip.delay));
        // 拆字字根字体（PUA 字根渲染）：路径 + DWrite 家族名取自主码表方案 [engine.chaizi]。
        // 库已在 build 内加载，此处仅补发字体（sync 按变更检测，重复调用幂等）。
        coordinator.sync_chaizi_assets();
        // 注释词库首次加载（`[[ui.comment_dicts]]`，出厂为空数组=不加载任何库）。
        coordinator.sync_comment_dicts();
        // 统一应用外观项（幂等）：补齐上面手动块未含的候选字体族 / 翻页栏 / 页码 / 字号跟随主题，
        // 使首次启动即按 config 应用（与 reload_user_config 同一路径）。
        coordinator.apply_ui_config();
        coordinator
    }

    /// 注入 host-render 管理器（Windows）。服务入口在构造 `BridgeServer` 后调用一次，
    /// 与其共享同一 `Arc` 实例。重复注入静默忽略（`OnceLock` 语义）。
    #[cfg(windows)]
    pub fn set_host_render(&self, mgr: Arc<wind_bridge::host_render_windows::HostRenderManager>) {
        let _ = self.host_render.set(mgr.clone());
        // 把同一 Arc 传给 UI 线程，使其在消息循环中激活 SHM 分流路径（Task 7）。
        let _ = self.ui_tx.send(wind_ui::manager::UiCommand::SetHostRender(
            wind_ui::manager::HostRenderArc(mgr),
        ));
    }

    /// 取已注入的 host-render 管理器（Windows）；未注入返回 None。供 Task 6/7 写帧/隐藏。
    #[cfg(windows)]
    pub(crate) fn host_render(
        &self,
    ) -> Option<&Arc<wind_bridge::host_render_windows::HostRenderManager>> {
        self.host_render.get()
    }

    /// 把 `app_compat` 现算的 HostRender 白名单同步给 manager。
    ///
    /// 白名单来自 compat.toml 的 `host_render = true` 规则（`AppCompatRule::host_render`），
    /// 不是 config.toml 字段——调用点是每次 `app_compat` 被重新加载之后（menu 写规则、
    /// 未来若加设置页开关同理），而非常规配置热重载（compat.toml 与 config.toml 是两个
    /// 独立文件，后者变了不代表前者变了）。
    #[cfg(windows)]
    pub(crate) fn sync_host_render_whitelist(&self) {
        if let Some(mgr) = self.host_render() {
            let processes = self
                .app_compat
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .host_render_processes();
            mgr.set_whitelist(processes);
        }
    }

    /// 当前是否处于 host-render 受限宿主模式（SearchHost.exe / 开始菜单搜索框等）。
    /// `active_target()` 每次现查（无缓存），避免跨帧持有失效目标；它仅在 active 连接
    /// **已完成 setup** 时返回 Some，而 setup 会拒绝白名单外进程——故此判定天然经过
    /// 白名单过滤，语义为「确实在 host 渲染」（比按事件源 pid 查白名单更严格）。
    /// 非 Windows 编译始终返回 false，零开销。
    pub(crate) fn host_render_active(&self) -> bool {
        #[cfg(windows)]
        return self
            .host_render()
            .map(|m| m.active_target().is_some())
            .unwrap_or(false);
        #[cfg(not(windows))]
        return false;
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
        Self::build(config, data_dir, push_server, ui_tx, None, None, None)
    }

    /// 无头 + **指定方案 override 目录**（测试用）。
    ///
    /// `new_headless` 让 `EngineManager` 自己取 `Config::user_config_dir()/schema_overrides`
    /// ——那是**真实用户目录**，测试写进去会污染用户配置，于是一切「方案级覆盖」的行为都
    /// 没法在集成测试里验证。方案级 `[key_actions]` 的分派 bug 正是因此漏到了真机上。
    pub fn new_headless_with_override(
        config: Config,
        data_dir: Option<&Path>,
        override_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        let (ui_tx, _rx) = std::sync::mpsc::channel();
        drop(_rx);
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        Self::build(
            config,
            data_dir,
            push_server,
            ui_tx,
            None,
            None,
            override_dir,
        )
    }

    /// 无头 + **保留 UI 通道接收端**（测试用）。
    ///
    /// `new_headless` 丢弃 rx，于是一切「发给 UI 的内容」在测试里都不可见——而候选的注释段、
    /// 悬停提示这些是在**发送路径上**算出来的，不回写 `state.candidates`。要验证它们只有两条路：
    /// 收这个 rx，或者另写一个「按同样规则再算一遍」的 debug 方法。后者是假测试的经典形态——
    /// 它证明不了生产路径接对了，决策函数写好但消费端没接的情况照样全绿。
    pub fn new_headless_with_ui(
        config: Config,
        data_dir: Option<&Path>,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<UiCommand>) {
        let (ui_tx, rx) = std::sync::mpsc::channel();
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        (
            Self::build(config, data_dir, push_server, ui_tx, None, None, None),
            rx,
        )
    }

    /// 无头 + 注入 redb store（测试用）：用于 web_data_rpc 数据域契约测试。
    pub fn new_headless_with_store(
        config: Config,
        data_dir: Option<&Path>,
        store: Arc<Store>,
    ) -> Arc<Self> {
        Self::new_headless_with_store_override(config, data_dir, store, None)
    }

    /// 无头 + store + **指定方案 override 目录**（测试用）。
    ///
    /// 特殊模式的实例集合来自「带 `[overlay]` 段的已安装方案」，而测试不能往真实
    /// `data/schemas` 里写方案文件。走 override 层即可：`read_schema` 会把它深合并进
    /// 方案，效果等同该方案自带 `[overlay]` 段，同时保住真实词库不动。
    pub fn new_headless_with_store_override(
        config: Config,
        data_dir: Option<&Path>,
        store: Arc<Store>,
        override_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        let (ui_tx, _rx) = std::sync::mpsc::channel();
        drop(_rx);
        let push_server = Arc::new(PushServer::new(PushConfig {
            suffix: String::new(),
            write_timeout_ms: 30_000,
        }));
        Self::build(
            config,
            data_dir,
            push_server,
            ui_tx,
            None,
            Some(store),
            override_dir,
        )
    }

    fn build(
        config: Config,
        data_dir: Option<&Path>,
        push_server: Arc<PushServer>,
        ui_tx: std::sync::mpsc::Sender<UiCommand>,
        user_dir: Option<std::path::PathBuf>,
        store: Option<Arc<Store>>,
        override_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        // 注入 redb Store：码表引擎注册用户词/临时词层，用户词进候选合并。
        // override_dir 为 None 时由 EngineManager 取默认（用户配置目录下的 schema_overrides）。
        let engine_mgr = match override_dir {
            Some(od) => {
                EngineManager::with_store_override(&config, data_dir, store.clone(), Some(od))
            }
            None => EngineManager::with_store(&config, data_dir, store.clone()),
        };
        // 应用兼容规则：系统层(data/compat.toml) + 用户层覆盖。供焦点进程按名查规则
        // （如微信 caret_use_top）。
        let app_compat = wind_config::app_compat::AppCompat::load(data_dir, user_dir.as_deref());
        // 配置的轻量派生缓存集中到 ConfigBundle（支持运行时热替换）。
        let schema_mods = schema_bound_modifier_vks(&engine_mgr);
        let bundle = ConfigBundle::build(config.clone(), &schema_mods);
        info!(
            "Compiled hotkeys: {} key_down, {} key_up",
            bundle.compiled_hotkeys.key_down.len(),
            bundle.compiled_hotkeys.key_up.len()
        );

        // 短语层（方案 B）：TOML 变更时同步进 store，再从 store（仅 enabled）建层。
        // 启动解析的条目缓存进结构体，作为"恢复默认"重读文件失败时的回退。
        let mut system_phrase_entries: Vec<wind_phrase::SystemPhraseEntry> = Vec::new();
        // 用户目录同名文件整体替代安装目录那份（覆盖替换，非合并）。
        // ⚠️ 解析在此**一次定死**：后续 `current_system_phrase_entries` 的重读走同一路径，
        // 故运行时新放的覆盖文件要下次启动才生效（与全仓其它覆盖点一致，无文件监视）。
        let system_phrase_path = Config::resolve_data_file(data_dir, "system.phrases.toml");
        if system_phrase_path.is_none() && data_dir.is_some() {
            warn!("system.phrases.toml 缺失（用户/安装目录均未找到），系统短语将为空");
        }
        let phrases = {
            if let Some(store) = store.as_ref() {
                if let Some(p) = system_phrase_path.as_ref() {
                    let entries = wind_phrase::PhraseLayer::parse_system_entries(p);
                    // 内容哈希：条目稳定序列化后哈希
                    let hash = phrase_entries_hash(&entries);
                    // 自愈：哈希不一致（TOML 改动）或表内系统短语为空（被删/未初始化）时才同步。
                    // 仅凭哈希会漏掉"系统短语从表中丢失但 TOML 未变"的场景。
                    let sys_empty = store
                        .list_system_phrases()
                        .map(|v| v.is_empty())
                        .unwrap_or(false);
                    if store.phrase_sys_hash().ok().flatten().as_deref() != Some(hash.as_str())
                        || sys_empty
                    {
                        let sys: Vec<wind_store::phrases::SystemPhrase> = entries
                            .iter()
                            .map(|e| wind_store::phrases::SystemPhrase {
                                code: e.code.clone(),
                                text: e.text.clone(),
                                weight: e.weight,
                                position: e.position,
                            })
                            .collect();
                        if let Ok(st) = store.sync_system_phrases(&sys) {
                            info!(
                                "Synced system phrases: +{} ~{} -{}",
                                st.added, st.updated, st.removed
                            );
                            let _ = store.set_phrase_sys_hash(&hash);
                        }
                    }
                    system_phrase_entries = entries;
                }
                let recs = store
                    .enabled_phrases_for_input()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.code, p.text, p.weight, p.position, p.is_system));
                std::sync::RwLock::new(wind_phrase::PhraseLayer::from_records(recs))
            } else {
                std::sync::RwLock::new(wind_phrase::PhraseLayer::default())
            }
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

        // 通用规范汉字表（检索范围"常用字"判定）。用户目录同名文件整体替代（见
        // docs/architecture/user-override.md）——自定义"常用字"范围是这张表的主要用途。
        let common_chars = wind_candidate::CommonChars::load(
            &Config::resolve_schema_resource(data_dir, "common_chars.txt").unwrap_or_default(),
        );
        if common_chars.is_empty() {
            warn!("common_chars.txt 缺失，检索范围过滤将退化为不过滤");
        } else {
            info!("Loaded common chars table");
        }

        // 候选反查表（拆字/拼音）：拆字库路径取自主码表方案 [engine.chaizi].db_path（相对 schemas/，
        // 用户方案目录优先——第三方方案的拆字库只在用户目录下）。
        let chaizi_db = engine_mgr
            .chaizi_spec()
            .filter(|c| !c.db_path.is_empty())
            .and_then(|c| {
                let p = Config::resolve_schema_resource(data_dir, &c.db_path);
                if p.is_none() {
                    warn!(
                        "拆字库不存在（用户/系统 schemas 目录均未找到）: {}",
                        c.db_path
                    );
                }
                p
            });
        // 快捷输入格式表：日期/数字/金额/计算候选的文本与组内顺序。同样支持用户整份覆盖，
        // 是给高频输入者的高级特性，普通用户不会碰到（缺文件时回落内置默认表，行为与出厂一致）。
        let quick_formats = wind_quick_input::FormatTable::load(
            Config::resolve_data_file(data_dir, "system.quick.toml").as_deref(),
        );
        // 表达式条目在这里预检一次：写错的表达式在运行期只表现为「那条候选不出现」，
        // 没有预检就没有任何线索（热路径不能每次按键都告警）。
        crate::quick_eval::precheck(&quick_formats);

        // 拼音读音表同样支持用户覆盖（整体替代）：改多音字取音、补生僻字读音都靠换这张表。
        let pinyin_map = Config::resolve_data_file(data_dir, "pinyin_map.txt");
        if pinyin_map.is_none() && data_dir.is_some() {
            warn!("pinyin_map.txt 缺失（用户/安装目录均未找到），逐字拼音反查将不可用");
        }
        let reverse =
            wind_reverse::ReverseLookup::load(pinyin_map.as_deref(), chaizi_db.as_deref());
        if !reverse.is_empty() {
            info!("Loaded reverse-lookup (chaizi/pinyin)");
        }

        // Shadow 规则已迁至 redb（self.store 的 SHADOW 表，事务持久），不再用 shadow.json。
        // 从 state.toml 加载工具栏位置（按显示器 key 独立存储）。
        let runtime_state = Config::state_dir()
            .map(|d| wind_config::RuntimeState::load(&d))
            .unwrap_or_default();
        let toolbar_positions_init = runtime_state.toolbar_positions.clone();
        let themes_dir = data_dir.map(|d| d.join("themes"));
        // 初始主题名：config.ui.theme.name 为单一源，未设置则回退 FALLBACK_THEME。
        let cfg_theme = config.ui.theme.name.trim();
        let initial_theme = if !cfg_theme.is_empty() {
            cfg_theme.to_string()
        } else {
            crate::handle_mode::FALLBACK_THEME.to_string()
        };
        // 初始明暗：config.ui.theme.style（system 跟随系统实时探测，见 ThemeStyle::resolve_dark）。
        let theme_style_init = ThemeStyle::from_config(&config.ui.theme.style);

        // 标点转换器：只持引号交替态，自定义映射每次从实时配置读（故此处无需注入——
        // 曾在此注入一份副本且仅此一次，设置页改自定义标点必须重启服务才生效）。
        let punct_conv = PunctuationConverter::new();

        // 编码显示方式运行时初值（config 移入结构体前先算）。
        let preedit_display_init = config.ui.candidate.preedit();

        // 候选布局方向运行时初值（与下方 SetCandidateLayout 下发一致；config 移入前先算）。
        let candidate_vertical_init = config.ui.candidate.layout.eq_ignore_ascii_case("vertical");

        // 候选窗显隐运行时初值（ui.candidate.hide_window；此前恒为 false，配置不生效）。
        let hide_candidate_window_init = config.ui.candidate.hide_window;

        // 统计采集器：与 store 共享 Arc，内存聚合 + 后台定时 flush。
        let stat_collector = store.clone().map(StatCollector::new);
        // 启动初始状态：remember_last_state=true 时从 state.toml 恢复上次三态，否则用配置默认。
        let d = &config.input.default;
        let (init_chinese, init_full, init_punct) = if d.remember_last_state {
            (
                runtime_state.last_chinese_mode,
                runtime_state.last_full_width,
                runtime_state.last_chinese_punct,
            )
        } else {
            (d.chinese_mode, d.full_width, d.chinese_punct)
        };
        let (capslock_press_tx, capslock_press_rx) = std::sync::mpsc::channel::<()>();
        let coordinator = Arc::new(Self {
            state: Mutex::new(State {
                chinese_mode: init_chinese,
                full_width: init_full,
                chinese_punct: init_punct,
                s2t_enabled: config.input.s2t.enabled,
                filter_mode: wind_candidate::FilterMode::from_str(&config.input.filter_mode),
                scope_relaxed: false,
                toolbar_visible: config.ui.toolbar.visible, // 启动初值来自配置(运行时可菜单切换)
                ime_active: false, // 启动未激活：工具栏待 IME_ACTIVATED/FocusGained 才显示
                has_edit_context: false, // 同上：焦点尚未落到任何可编辑控件
                caps_lock: false,
                input_buffer: String::new(),
                input_buffer_cased: String::new(),
                input_cursor_pos: 0,
                preedit: String::new(),
                preedit_split_body: String::new(),
                preedit_fp_body: String::new(),
                shadow_code: String::new(),
                candidates: Vec::new(),
                selected_index: 0,
                current_page: 0,
                candidate_input: String::new(),
                candidate_limit: 0,
                has_more: false,
                committed_text: String::new(),
                committed_segs: Vec::new(),
                active: None,
                overlay_body: String::new(),
                temp_pinyin_buffer: String::new(),
                temp_pinyin_cursor: 0,
                temp_pinyin_schema: String::new(),
                temp_pinyin_prefix: String::new(),
                temp_english_buffer: String::new(),
                temp_english_cursor: 0,
                temp_english_prefix: String::new(),
                url_buffer: String::new(),
                url_cursor: 0,
                rewind: None,
                special_buffer: String::new(),
                special_cursor: 0,
                special_id: 0,
                overlay_spec: None,
                special_prefix: String::new(),
                mix_buffer: String::new(),
                mix_cursor: 0,
                mix_id: 0,
                mix_prefix: String::new(),
                mix_repeat: false,
                caret_x: 0,
                caret_y: 0,
                caret_height: 0,
                caret_source: wind_ipc::protocol::caret_source::UNKNOWN,
                menu_open: false,
                menu_opened_at: None,
                menu_target_page_local: 0,
                menu_target_text: String::new(),
                add_word_active: false,
                add_word_chars: Vec::new(),
                add_word_len: 0,
                add_word_code: String::new(),
                add_word_boundary: 0,
            }),
            push_server,
            rt: std::sync::RwLock::new(std::sync::Arc::new(bundle)),
            ui_tx,
            engine_mgr,
            store,
            punct: Mutex::new(punct_conv),
            capslock_hook: Mutex::new(None),
            capslock_press_tx,
            smart_symbol: Mutex::new(SmartSymbolArm::default()),
            auto_phrase: Mutex::new(crate::auto_phrase::AutoPhraseBuf::new()),
            last_self_commit: Mutex::new(None),
            auto_phrase_writes: std::sync::atomic::AtomicUsize::new(0),
            phrases,
            system_phrase_entries: std::sync::RwLock::new(system_phrase_entries),
            system_phrase_path,
            s2t: Mutex::new(s2t),
            common_chars,
            toolbar_positions: Mutex::new(toolbar_positions_init),
            current_toolbar_monitor: Mutex::new(None),
            reverse: std::sync::RwLock::new(reverse),
            quick_formats,
            chaizi_assets: Mutex::new(ChaiziAssets {
                db: chaizi_db,
                font: None, // 字体在 new() 经 sync_chaizi_assets 下发（headless 无 UI 不发）
            }),
            // 空初值 + new() 里的 sync_comment_dicts 首次加载：与拆字字体同一套「声明式变更
            // 检测」，构造期不做 IO，加载与热重载走同一条路径（不会出现只在启动生效的分叉）。
            comment_dict_paths: Mutex::new(Vec::new()),
            pair_tracker: Mutex::new(wind_transform::pair_tracker::PairTracker::new()),
            last_valid_caret: Mutex::new((0, 0, 0)),
            pending_first_show: Mutex::new(false),
            pending_first_show_token: Mutex::new(0),
            candidate_shown: Mutex::new(false),
            show_authorized: std::sync::atomic::AtomicBool::new(false),
            candidate_flipped: std::sync::atomic::AtomicBool::new(false),
            hover_index: std::sync::atomic::AtomicI32::new(-1),
            composition_start: Mutex::new((0, 0, false)),
            last_authoritative_caret: Mutex::new((0, 0, false)),
            last_key_at: Mutex::new(None),
            last_key_interval_ms: Mutex::new(None),
            first_show_was_provisional: std::sync::atomic::AtomicBool::new(false),
            caret_cache_verified: std::sync::atomic::AtomicBool::new(false),
            first_show_extended: std::sync::atomic::AtomicBool::new(false),
            pending_focus_tip: std::sync::atomic::AtomicBool::new(false),
            last_focus_tip_token: Mutex::new(0),
            app_compat: Mutex::new(app_compat),
            compat_dirs: (
                data_dir.map(|d| d.to_path_buf()),
                user_dir.as_ref().map(|d| d.to_path_buf()),
            ),
            active_compat: Mutex::new(ActiveCompat::default()),
            pid_names: Mutex::new(HashMap::new()),
            mode_states: Mutex::new(HashMap::new()),
            runtime_last: Mutex::new((init_chinese, init_full, init_punct)),
            last_caps_inject: Mutex::new(None),
            front_ctx: Mutex::new((String::new(), String::new(), String::new())),
            themes_dir,
            theme_name: Mutex::new(initial_theme),
            last_status_text: Mutex::new(String::new()),
            schema_toggle_origin: Mutex::new(None),
            theme_style: Mutex::new(theme_style_init),
            theme_index_labels: Mutex::new(Vec::new()),
            cmdbar_services: std::sync::OnceLock::new(),
            self_weak: std::sync::OnceLock::new(),
            recent_commits: Mutex::new(std::collections::VecDeque::new()),
            last_commit_len: std::sync::atomic::AtomicUsize::new(1),
            preedit_display: Mutex::new(preedit_display_init),
            hide_candidate_window: Mutex::new(hide_candidate_window_init),
            candidate_vertical: Mutex::new(candidate_vertical_init),
            candidate_layout_sent: Mutex::new(candidate_vertical_init),
            stat_collector,
            stat_recorded: std::sync::atomic::AtomicBool::new(false),
            fullscreen_cached: std::sync::atomic::AtomicBool::new(false),
            fullscreen_probing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(windows)]
            host_render: std::sync::OnceLock::new(),
            last_input_diag: Mutex::new(Default::default()),
            last_window_diag: Mutex::new(Default::default()),
            password_suppress: std::sync::atomic::AtomicBool::new(false),
            password_suppress_enabled: std::sync::atomic::AtomicBool::new(true),
            input_diag_hud_visible: std::sync::atomic::AtomicBool::new(false),
            input_diag_sections: Mutex::new(Default::default()),
            input_diag_frozen: std::sync::atomic::AtomicBool::new(false),
            input_diag_topmost: std::sync::atomic::AtomicBool::new(true),
        });
        // CapsLock 钩子的动作消费线程。钩子回调只做非阻塞投递（它超时会被系统静默移除且
        // 无从察觉），真正的动作在这里执行，可安全加锁。未装钩子时它一直阻塞在 channel 上。
        // 起在这里而非 `new`：只有此处能同时拿到 `Arc<Self>` 与 receiver。
        {
            let c = Arc::clone(&coordinator);
            std::thread::Builder::new()
                .name("capslock-action".into())
                .spawn(move || {
                    for _ in capslock_press_rx {
                        c.handle_capslock_hook_press();
                    }
                    debug!("CapsLock 钩子事件通道已关闭");
                })
                .ok();
        }
        // 命令栏：装配 Services（ime/config/dict 后端）+ 自身 Weak 引用。
        coordinator.init_cmdbar();
        // 启动即显示常驻工具栏（反映初始 中英/方案/标点/全半角）
        coordinator.notify_toolbar();
        // 码元集与按键功能的冲突体检（只告警）。默认字符集下直接返回，无开销。
        coordinator.warn_code_char_conflicts();
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

    /// 前台上下文快照 `(app, title, sel)`，供命令栏 `app()/title()/sel()` 读取。
    pub(crate) fn front_ctx_snapshot(&self) -> (String, String, String) {
        self.front_ctx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 取当前「配置 + 派生缓存」快照（Arc 克隆，开销低）。所有配置读取经此。
    pub(crate) fn rt(&self) -> std::sync::Arc<ConfigBundle> {
        self.rt.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 回车键是否配置为「清空编码」（`input.enter_behavior = "clear"`）。
    ///
    /// 回车有五条彼此独立的处理路径（主输入 / 临时拼音 / 临时英文 / 混合输入 / 特殊模式），
    /// 此判据由它们共用。此前各路径内联比较字符串，且**只判在「空缓冲」分支上**，
    /// 于是「打了码再按回车」时配置静默失效、照旧上屏原码；收口成单一具名判据，
    /// 使「某条路径没接」退化为「没有调用点」这种更容易发现的缺失。
    pub(crate) fn enter_clears_composition(&self) -> bool {
        self.rt().config.input.enter_behavior == "clear"
    }

    /// 焦点/IME 激活时按 client_token 高 32 位的 PID 解析焦点进程名，缓存其 caret 兼容态
    /// （对齐 Go `HandleFocusGained` 设置 activeCompatRule）。按 pid 缓存：同进程命中直接返回，
    /// 避免每次焦点事件重复 OpenProcess。仅在重型/异步段调用，不在 DLL 同步阻塞路径上。
    fn update_active_compat(&self, client_token: u64) {
        let pid = (client_token >> 32) as u32;
        if pid == 0 {
            return;
        }
        // 缓存优先于反查：macOS 的 `.app` 随焦点事件把宿主 bundle id 送进 `pid_names`
        // （服务进程那边 `process_name` 恒返回空串），此处必须先读缓存才能拿到宿主名。
        // Windows 上首次见到该 pid 时缓存为空 → 照常 OpenProcess 反查，行为不变。
        //
        // ⚠ 在取 `active_compat` 锁**之前**读缓存：本函数末尾是「先 drop(ac) 再锁
        // pid_names」，两把锁在此嵌套会引入一个方向相反的持有序。
        let cached_name = self.cached_proc_name(client_token);
        let mut ac = self.active_compat.lock().unwrap_or_else(|e| e.into_inner());
        if ac.pid == pid {
            return; // 同进程，规则已缓存
        }
        let name = if cached_name.is_empty() {
            process_name(pid)
        } else {
            cached_name
        };
        let (next, rule_matched, rule_initial_mode, rule_initial_punct) = {
            let table = self.app_compat.lock().unwrap_or_else(|e| e.into_inner());
            let rule = table.get_rule(&name);
            let initial_mode = rule.and_then(|r| r.initial_mode);
            let initial_punct = rule.and_then(|r| r.initial_punct);
            (
                ActiveCompat {
                    pid,
                    caret_use_top: rule.map(|r| r.caret_use_top).unwrap_or(false),
                    first_show_mode: rule.map(|r| r.first_show_mode).unwrap_or_default(),
                    has_initial_rule: initial_mode.is_some() || initial_punct.is_some(),
                    auto_pair: rule.and_then(|r| r.auto_pair),
                    smart_method: rule.and_then(|r| r.smart_method),
                    caret_offset_x: rule.map(|r| r.caret_offset_x).unwrap_or(0),
                    caret_offset_y: rule.map(|r| r.caret_offset_y).unwrap_or(0),
                },
                rule.is_some(),
                initial_mode,
                initial_punct,
            )
        };
        // 无条件记录（对齐 Go handle_lifecycle.go:698）。原实现仅在 caret_use_top=true 时打，
        // 规则未命中与「命中但全 false」在日志里无从区分，查「某应用兼容项没生效」时看不到
        // 究竟是没匹配上进程名还是字段没读到。
        debug!(
            "Compat rule for process={name}: matched={} caret_use_top={} first_show_mode={} initial_mode={} initial_punct={} auto_pair={} smart_method={} caret_offset=({},{})",
            rule_matched,
            next.caret_use_top,
            next.first_show_mode.as_config(),
            rule_initial_mode
                .map(|m| m.as_config())
                .unwrap_or("(follow-global)"),
            rule_initial_punct
                .map(|m| m.as_config())
                .unwrap_or("(follow-global)"),
            match next.auto_pair {
                Some(true) => "on",
                Some(false) => "off",
                None => "(follow-global)",
            },
            match next.smart_method {
                Some(wind_config::config::SmartMethod::DeleteReplace) => "delete_replace",
                Some(wind_config::config::SmartMethod::HoldComposition) => "hold_composition",
                None => "(follow-global)",
            },
            next.caret_offset_x,
            next.caret_offset_y
        );
        *ac = next;
        drop(ac);
        // 顺带填 pid→进程名缓存，供 FOCUS_GAINED 同步路径免 OpenProcess 查询（per-app 状态）。
        if !name.is_empty() {
            self.pid_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid, name.to_lowercase());
        }
    }

    /// 按 client_token 高 32 位的 PID 查已缓存的进程名（小写）。未缓存返回空串。
    /// 仅 HashMap 查询，可用于 DLL 同步阻塞路径。
    fn cached_proc_name(&self, client_token: u64) -> String {
        let pid = (client_token >> 32) as u32;
        if pid == 0 {
            return String::new();
        }
        self.pid_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .cloned()
            .unwrap_or_default()
    }

    /// 消费一次输入诊断上报（compartment 禁用态 + InputScope 掩码）：更新 `last_input_diag`
    /// 快照，并按 `password_suppress_enabled` 开关决定是否强制英文抑制（密码框场景）。
    pub(crate) fn apply_input_diag(&self, pid: u32, disabled: bool, reason_byte: u8, mask: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let reason = crate::input_diag::reason_from(disabled, mask);
        // 本地一律以 mask/disabled 经 reason_from 推导 reason 作准；上报的 reason_byte
        // 仅供展示/日志参考，不参与本地决策（避免"双重来源"歧义）。
        let _ = reason_byte; // 上游已按 mask/disabled 推导 reason；保留形参对齐上报字段序。
        let name = if pid != 0 {
            self.cached_proc_name((pid as u64) << 32)
        } else {
            String::new()
        };
        // 抑制：命中密码 InputScope 位 且 策略开关开 → 强制英文。
        //
        // ⚠ 曾经这里还有一条 `&& !disabled`，理由是「disabled 时 DLL 已放行所有键、引擎收不到
        // 键，抑制 moot」。那条推理错在 `disabled` 的层级：DLL 放行看的是**线程级**
        // KEYBOARD_DISABLED，而 Windows 侧当时往这个字段传的是**context 级**的密码框判定。
        // 于是 Chromium 网页密码框（只置 context 级）被这条判据整个否掉——键没被放行、抑制也
        // 不生效，密码框里照打中文，高级菜单的开关看着像坏了。2026-07-27 两侧一并修正：
        // `disabled` 统一为线程级语义，密码信号只走 mask。
        //
        // 现在 disabled 只参与 `reason_from` 的展示推导，不再进决策——单一来源，避免再次歧义。
        // 线程级 disabled 为真时本判据仍可能算出 suppress=true，这是**安全的**：那时 DLL 在
        // OnTestKeyDown 开头就全放行了，一个键都不会送到引擎，suppress 取值无从被观测。
        // 危险的只有反方向（core 抑制而 DLL 吃键 → 「吃了再吐」丢键），故不变量是
        // **core.suppress ⊆ C++.suppress**，见 C++ `IsPasswordSuppressActive`。
        let suppress = crate::input_diag::is_password_scope(mask)
            && self.password_suppress_enabled.load(Relaxed);
        self.password_suppress.store(suppress, Relaxed);
        {
            let mut d = self
                .last_input_diag
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *d = crate::input_diag::InputDiagState {
                pid,
                process_name: name,
                disabled,
                reason,
                mask,
            };
        }
        self.push_input_diag_hud_if_visible();
    }

    /// 消费一次诊断快照：存 DLL 上报的窗口链 / TSF 实例。
    ///
    /// ⚠ host-render 运行态（白名单 / 活跃）**不在这里算**——它们是服务端随时可查的实时值，
    /// 存进快照就等于被冻结在「快照到达那一刻」。而 `active_target` 恰恰要到**首次按键**
    /// 才置位（searchapp/SearchHost 这类 transient DocMgr 宿主不发 focus_gained，note_focus
    /// 只能走 CMD_KEY_EVENT），快照却在 OnSetFocus 就发出了 ⇒ 存下来的必然是 `活跃: 否`，
    /// 让人误判成 host render 没生效。现算在 [`Self::push_input_diag_hud`]。
    pub(crate) fn apply_diag_snapshot(&self, snap: &wind_ipc::protocol::DiagSnapshotPayload) {
        // 进程名：服务端按 pid 现查（DLL 不上报——它未必有权限打开别的进程）。
        // 快照来源进程与前台进程分别查：多进程宿主下它们本就可能不同，而「本快照来自谁」
        // 是判读整份数据的前提（见 `WindowDiagView::pid`）。
        let proc_name = |pid: u32| {
            if pid != 0 {
                self.cached_proc_name((pid as u64) << 32)
            } else {
                String::new()
            }
        };
        let process_name = proc_name(snap.pid);
        let fg_process_name = proc_name(snap.fg_pid);

        {
            let mut w = self
                .last_window_diag
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *w = crate::input_diag::WindowDiagView {
                pid: snap.pid,
                process_name,
                focus_hwnd: snap.focus_hwnd,
                focus_class: snap.focus_class.clone(),
                focus_source_label: wind_ipc::protocol::window_source::label(
                    snap.focus_hwnd_source,
                )
                .to_string(),
                root_hwnd: snap.root_hwnd,
                root_class: snap.root_class.clone(),
                root_band: snap.root_band,
                fg_hwnd: snap.fg_hwnd,
                fg_class: snap.fg_class.clone(),
                fg_pid: snap.fg_pid,
                fg_process_name,
                docmgr_id: snap.docmgr_id,
                context_id: snap.context_id,
                focus_session_id: snap.focus_session_id,
                docmgr_changed: snap.docmgr_changed(),
                host_band: snap.host_band,
                // 这两项由 push 时现算填入（见本函数文档），此处留默认值。
                host_whitelisted: false,
                host_active: false,
                received: true,
            };
        }
        self.push_input_diag_hud_if_visible();
    }

    /// 下发诊断快照采集开关给 DLL（随 HUD 显隐 + 握手时）。
    ///
    /// 采集要查三次窗口类名 + band，故默认关；**握手时必须也推一次**——DLL 每次重连都从
    /// 默认值（关）起步，只在切换时推会让重连后的宿主永远不采集，而 SearchHost 这类
    /// transient 宿主恰恰最常重连，也恰恰最需要 HUD（它是 AppContainer，写不了日志）。
    pub fn push_diag_snapshot_config(&self, client_token: u64) {
        let enabled = self
            .input_diag_hud_visible
            .load(std::sync::atomic::Ordering::Relaxed);
        let value = wind_ipc::codec::encode_diag_snapshot_value(enabled);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_DIAG_SNAPSHOT,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// HUD 推送（数据到达路径）：HUD 可见且**未冻结**时下发一帧。
    pub(crate) fn push_input_diag_hud_if_visible(&self) {
        self.push_input_diag_hud(false);
    }

    /// HUD 推送。`force=true` 时无视冻结照常下发。
    ///
    /// ⚠ 冻结只该挡住**数据变化**引起的刷新，不该挡住用户自己的操作（切分区/切置顶/
    /// 切冻结本身）。两者混为一谈的后果是"点了菜单屏幕毫无反应"——而那与"菜单坏了"
    /// 在用户眼里完全一样。故所有菜单动作一律走 `force=true`。
    pub(crate) fn push_input_diag_hud(&self, force: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.input_diag_hud_visible.load(Relaxed) {
            return;
        }
        if !force && self.input_diag_frozen.load(Relaxed) {
            return;
        }
        let d = self
            .last_input_diag
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 取 state 快照：HUD 要显示决定工具栏可见性的两个正交状态位。
        // 先 drop 掉 last_input_diag 的锁再取 state 锁，避免与其它路径形成反序嵌套。
        let (process_name, pid, disabled, reason, mask) =
            (d.process_name.clone(), d.pid, d.disabled, d.reason, d.mask);
        drop(d);
        let (ime_active, has_edit_context) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.ime_active, s.has_edit_context)
        };
        // 窗口快照独立取（锁序：last_input_diag → state → last_window_diag，全程不嵌套）。
        let mut window = self
            .last_window_diag
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // host-render 运行态**在此现算**，不沿用快照里的值：它们随时可查，存进快照就会被
        // 冻结在快照到达那一刻（详见 `apply_diag_snapshot` 文档）。
        //
        // ⚠ 必须按**快照来源进程**的 pid 直查，不得走 `ActiveCompat` 全局焦点槽——开始菜单
        // 弹出会连带激活兄弟进程污染该槽，那正是当初 avail 位被污染、DLL 陷入销毁重建循环的
        // 成因（`docs/redesign/host-render-windows-port.md` §11.2）。
        #[cfg(windows)]
        if window.pid != 0
            && let Some(mgr) = self.host_render()
        {
            window.host_whitelisted = mgr.is_process_whitelisted(window.pid);
            window.host_active = mgr.active_target().is_some_and(|t| t.pid == window.pid);
        }
        let sections = *self
            .input_diag_sections
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let view = wind_ui::manager::InputDiagView {
            process_name,
            pid,
            disabled,
            reason_text: crate::input_diag::reason_label(reason).to_string(),
            mask,
            ime_active,
            has_edit_context,
            window,
            sections,
            topmost: self.input_diag_topmost.load(Relaxed),
            frozen: self.input_diag_frozen.load(Relaxed),
        };
        let _ = self
            .ui_tx
            .send(wind_ui::manager::UiCommand::ShowInputDiag(view));
    }

    /// 查 `compat.toml` 中该进程的初始中英规则；`None` = 未配置（不干预）。
    ///
    /// 仅 HashMap 查询，无 OpenProcess，故可用于 DLL 同步阻塞路径（`get_current_mode`）。
    pub(crate) fn rule_initial_mode(
        &self,
        proc_name: &str,
    ) -> Option<wind_config::app_compat::InitialMode> {
        if proc_name.is_empty() {
            return None;
        }
        self.app_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_rule(proc_name)
            .and_then(|r| r.initial_mode)
    }

    /// 查 `compat.toml` 中该进程的初始中英标点规则；`None` = 未配置（不干预）。
    pub(crate) fn rule_initial_punct(
        &self,
        proc_name: &str,
    ) -> Option<wind_config::app_compat::InitialMode> {
        if proc_name.is_empty() {
            return None;
        }
        self.app_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_rule(proc_name)
            .and_then(|r| r.initial_punct)
    }

    /// 决策进程 `proc_name` 的中英初始状态（初始状态语义的单一内聚点）。
    ///
    /// 顺序：**按应用规则表（compat.toml）** → per-app 记忆表 → 全局记忆 / 配置默认。
    ///
    /// ⚠ 规则表排在记忆表**之前**是刻意的，与此处原 `TODO(app_rules)` 注释设想的位置相反。
    /// 原设想是「首次进入时生效，之后跟随用户手切」，那个语义对 Everything / Listary 这类
    /// **常驻隐藏式**窗口不成立：进程始终不退出，会话级记忆表里「首次」只有一次，用户从第二次
    /// 唤出起规则就再也不生效。放到记忆表之前，配合 `apply_initial_mode` 的跨进程守卫，语义
    /// 才是「每次从别的应用切进来都套用，停留在该应用期间尊重手切」。
    ///
    /// 规则是**初始值不是锁定**：它只在焦点跨进程切入的那一刻参与决策，此后用户手切自由，
    /// 且同应用内的焦点跳转不会重新套用（守卫见 `apply_initial_mode` 调用点）。
    fn initial_chinese_mode_for(&self, proc_name: &str) -> bool {
        let bundle = self.rt();
        let d = &bundle.config.input.default;
        if let Some(m) = self.rule_initial_mode(proc_name) {
            return m.is_chinese();
        }
        if d.per_app_scope()
            && !proc_name.is_empty()
            && let Some(&m) = self
                .mode_states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(proc_name)
        {
            return m;
        }
        if d.remember_last_state {
            self.runtime_last
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .0
        } else {
            d.chinese_mode
        }
    }

    /// 用户主动切换中英/全半角/标点后记录"最后状态"镜像；
    /// remember_last_state=true 时同步落盘 state.toml（复用 toolbar_positions 的 load-modify-save 模式）。
    /// 必须在释放 state 锁后调用。
    pub(crate) fn record_last_state(&self) {
        let (c, f, p) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (s.chinese_mode, s.full_width, s.chinese_punct)
        };
        *self.runtime_last.lock().unwrap_or_else(|e| e.into_inner()) = (c, f, p);
        if self.rt().config.input.default.remember_last_state
            && let Some(dir) = Config::state_dir()
        {
            let mut rs = wind_config::RuntimeState::load(&dir);
            rs.last_chinese_mode = c;
            rs.last_full_width = f;
            rs.last_chinese_punct = p;
            let _ = rs.save(&dir);
        }
    }

    /// state_scope="app" 时把中英状态写回当前前台进程的记忆表（进程名取自 pid 缓存）。
    pub(crate) fn record_app_mode(&self, chinese: bool) {
        if !self.rt().config.input.default.per_app_scope() {
            return;
        }
        let pid = self
            .active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pid;
        let name = self
            .pid_names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .cloned()
            .unwrap_or_default();
        if !name.is_empty() {
            self.mode_states
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name, chinese);
        }
    }

    /// 「切换模式时取消大小写锁定」（input.capslock.cancel_on_mode_switch）：
    /// CapsLock 开着时 `effective_chinese = chinese_mode && !caps_lock` 恒为英文大写，
    /// 切中英/切方案"看似无效"。开启该配置后，切换动作先物理敲击 CapsLock 取消系统
    /// 锁定并同步镜像，让切换真正生效。返回是否执行了取消（供调用方决定归位语义）。
    /// 需在未持有 state 锁时调用。
    pub(crate) fn cancel_caps_on_switch(&self) -> bool {
        if !self.rt().config.input.capslock.cancel_on_mode_switch {
            return false;
        }
        {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !s.caps_lock {
                return false;
            }
        }
        // 防抖：同一轮切换动作内不重复注入（一次注入的系统回环在几十 ms 内完成）。
        // 振荡回路的主熔断在 C++ 侧（OPENCLOSE 的 CapsLock 联动抑制 + Ctrl 判据），
        // 此处窗口必须远小于用户连续两轮「开大写→切换」的最短间隔——曾设 1500ms，
        // 实测会吞掉快节奏的第二轮合法请求（表现为"有时要按两次"），勿再调大。
        {
            let mut last = self
                .last_caps_inject
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(t) = *last
                && t.elapsed() < std::time::Duration::from_millis(300)
            {
                debug!("cancel_on_mode_switch: 注入防抖期内，跳过");
                return false;
            }
            *last = Some(std::time::Instant::now());
        }
        // SendInput 敲击 VK_CAPITAL；失败（非 Windows/注入受限）不动镜像，行为退回未配置。
        if let Err(e) = wind_keys::key_inject::tap_caps_lock() {
            warn!("cancel_on_mode_switch: 注入 CapsLock 失败: {e}");
            return false;
        }
        // 乐观同步镜像（后续按键立即按新状态处理）；注入回环的 CapsLock key_up
        // 状态通知（toggles bit=0）随后到达时与此幂等。
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .caps_lock = false;
        true
    }

    /// IME 激活 / 焦点切换（重型段）时按配置矩阵落地初始状态。
    /// `reset_aux`＝激活场景：remember=false 时同时重置全半角/标点为配置默认
    /// （焦点切换场景不重置——同一激活期内切窗口不动全半角/标点）。
    /// 需在未持有 state 锁时调用。
    fn apply_initial_mode(&self, client_token: u64, reset_aux: bool) {
        let bundle = self.rt();
        let d = &bundle.config.input.default;
        let proc = self.cached_proc_name(client_token);
        let chinese = self.initial_chinese_mode_for(&proc);
        let rule_punct = self.rule_initial_punct(&proc);
        let follow = bundle.config.input.punct.follow_mode;
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if reset_aux && !d.remember_last_state {
            s.full_width = d.full_width;
            s.chinese_punct = d.chinese_punct;
        }
        if s.chinese_mode != chinese {
            s.chinese_mode = chinese;
            // 标点随中英文切换（对齐 handle_toggle_mode/handle_system_mode_switch）。
            if follow {
                s.chinese_punct = chinese;
            }
        }
        // per-app 标点规则**最后**落地，压过 follow_mode 的推导与 reset_aux 的重置。
        // 顺序反了的话，用户配了 initial_punct 却恰好开着 follow_mode 时，规则会被
        // 上面那行静默覆盖——「配了没反应、日志里也没有痕迹」正是本仓反复出现的形态。
        if let Some(p) = rule_punct {
            s.chinese_punct = p.is_chinese();
        }
    }

    /// 热重载用户配置：从磁盘重读 Config 并原子替换 bundle（轻量设置即时生效），
    /// 再 best-effort 刷新主题/工具栏。返回是否仍需重启才能完全生效。
    /// 轻量项（标点/智能符号/候选数/热键/配对/导航键等）即时生效；重型项（引擎/方案/
    /// 词典/字体）当前不在 bundle 内，需重启——为不打断使用，这里统一返回 false，
    /// 由调用方/用户按需重启。
    /// 同步拆字资产到当前来源方案（`chaizi_spec`：码表=自身、混输=其主码表成员、拼音=全局
    /// 主码表，与编码段同源）：库路径变了才重载反查表拆字段（含变为无配置时清空释放内存），
    /// 字根字体变了才重发（渲染端每次 set 都重建字体集，勿重复下发）。调用点=启动、方案切换
    /// （菜单/循环/设置页）、reload_user_config(schema_dirty)。资源相对路径按「用户方案目录
    /// 优先、回落系统数据目录」解析（与方案文件同规则）。
    pub(crate) fn sync_chaizi_assets(&self) {
        let data_dir = Config::data_dir();
        let spec = self.engine_mgr.chaizi_spec();
        let new_db = spec
            .as_ref()
            .filter(|c| !c.db_path.is_empty())
            .and_then(|c| {
                let p = Config::resolve_schema_resource(data_dir.as_deref(), &c.db_path);
                if p.is_none() {
                    warn!(
                        "拆字库不存在（用户/系统 schemas 目录均未找到）: {}",
                        c.db_path
                    );
                }
                p
            });
        let new_font = spec
            .as_ref()
            .filter(|c| !c.font_path.is_empty())
            .and_then(|c| {
                Config::resolve_schema_resource(data_dir.as_deref(), &c.font_path)
                    .map(|p| (p.to_string_lossy().into_owned(), c.font_family.clone()))
            });
        let mut assets = self.chaizi_assets.lock().unwrap_or_else(|e| e.into_inner());
        if assets.db != new_db {
            self.reverse
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .reload_chaizi(new_db.as_deref());
            assets.db = new_db;
        }
        if new_font != assets.font {
            // 变为 None 时仅不再重发（字体集无撤销接口；旧字体仅影响 PUA 段渲染，无害）。
            if let Some((path, family)) = &new_font {
                let _ = self.ui_tx.send(UiCommand::SetTooltipChaiziFont {
                    path: path.clone(),
                    family: family.clone(),
                });
            }
            assets.font = new_font;
        }
    }

    /// 同步注释词库（`[[ui.comment_dicts]]`）到反查表：解析路径列表，与上次生效的比对，
    /// **变了才重载**。调用点=启动、reload_user_config、切方案（switch/cycle）。
    ///
    /// 变更检测比的是**解析后的路径序列**（含顺序）而非配置结构：顺序即优先级，调换两个库
    /// 的位置必须触发重载；而只改 `label` 这类不影响加载的字段则不该重载。有了 `.wcmt`
    /// 缓存，重载本身只是重开 mmap，但切方案是高频操作，能不动就不动。
    ///
    /// **按活跃方案过滤**（`schemas` 字段，留空=全部）：一份大英汉词典挂在五笔方案上，
    /// 每次输入都要多走一次注定查不到的二分。方案专属的库因此只在其方案下加载 ——
    /// 这也是切方案要调本函数的原因。
    ///
    /// 路径**以 `schemas/` 为基准**解析（`resolve_schema_resource`，用户目录优先、回落安装
    /// 目录），与拆字库、字根字体这些方案附属资源同一规则 —— 注释库本就是同类东西：
    /// 放在 `schemas/` 下、随整机备份走（`user_schemas_dir` 递归打包）、不参与召回。
    /// 配置里因此写 `comments/xxx.dict.yaml` 而非 `schemas/comments/xxx.dict.yaml`。
    pub(crate) fn sync_comment_dicts(&self) {
        let data_dir = Config::data_dir();
        let specs = {
            let rt = self.rt();
            rt.config.ui.comment_dicts.clone()
        };
        let active = self.engine_mgr.active_schema_id();
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for s in specs
            .iter()
            .filter(|s| s.enabled && !s.path.is_empty() && s.applies_to(&active))
        {
            match Config::resolve_schema_resource(data_dir.as_deref(), &s.path) {
                // 按**解析后路径**去重：两条 spec 写不同的相对路径却指向同一个文件时
                // （`a.dict.yaml` 与 `./a.dict.yaml`，或用户目录与安装目录同名文件都被
                // 解析到同一处），只加载一次。重复加载除了浪费解析时间，还会让优先级
                // 判定变得依赖「第几次出现」——去重后靠前那条恒胜出。
                Some(p) if paths.contains(&p) => {
                    info!("注释词库重复挂载，已跳过: {} (id={})", p.display(), s.id)
                }
                Some(p) => paths.push(p),
                // 只 warn 不中断：一个库路径写错不该让其余库一起不加载。
                None => warn!(
                    "注释词库不存在（用户/安装目录均未找到）: {} (id={})",
                    s.path, s.id
                ),
            }
        }
        let mut cur = self
            .comment_dict_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *cur == paths {
            return;
        }
        // 注释库缓存与词库 .wdat **同根**：`comment_cache_path` 自己按源文件父目录名分
        // 命名空间（`schemas/comments/x.dict.yaml` → `<cache>/comments/x.wcmt`），与
        // `EngineManager::cache_path` 同构，不再另立一层专用目录。
        // 无缓存目录（便携/测试）时传 None，注释库退化为内存加载，功能不受影响。
        let cache_dir = Config::cache_dir();
        self.reverse
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .reload_comments(&paths, cache_dir.as_deref());
        *cur = paths;
    }

    /// 就地改写内存配置并重建 ConfigBundle，**不触发** reload_user_config 的那一整套副作用
    /// （toast、引擎热重建、热键重注册、主题下发、向 TSF 推 IPC 配置）。
    ///
    /// 用于「改动只影响少数几个 UI 字段、且发生频率高」的场景——典型是拖动窗口后落盘位置：
    /// 走 reload_user_config 会每拖一次弹一个「设置已更新」toast，明显不合适。
    /// 调用方仍需自行用 `Config::set_user_*` 把值写盘，本函数只负责让内存态立刻跟上。
    /// 候选窗定位参数 `(fixed, fixed_x, fixed_y)`，随每次 `UpdateCandidates` 下发。
    ///
    /// fixed 时 UI 侧忽略光标坐标，改用 `custom_x/custom_y`；`(0,0)` 表示"已开启固定
    /// 但用户还没拖过"，由 UI 落到屏幕默认锚点。快捷加词面板复用同一个候选窗实例，
    /// 因此也走这里——否则同一个窗口会在"加词时跟随、打字时固定"之间来回跳。
    pub(crate) fn candidate_fixed_pos(&self) -> (bool, i32, i32) {
        let rt = self.rt();
        let c = &rt.config.ui.candidate;
        (c.is_fixed_position(), c.custom_x, c.custom_y)
    }

    pub(crate) fn refresh_config_in_memory(&self, mutate: impl FnOnce(&mut Config)) {
        let mut cfg = self.rt().config.clone();
        mutate(&mut cfg);
        let mods = schema_bound_modifier_vks(&self.engine_mgr);
        let bundle = std::sync::Arc::new(ConfigBundle::build(cfg, &mods));
        *self.rt.write().unwrap_or_else(|e| e.into_inner()) = bundle;
        // 状态气泡去重缓存只在"内容配置不变"的前提下有效：改了 ui.status.items 之类后，
        // 同一状态该合成出不同文本，留着旧缓存会把改动后的第一次显示误判成"内容没变"而吞掉。
        self.last_status_text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn reload_user_config(&self) -> bool {
        match Config::load(Config::data_dir().as_deref()) {
            Ok(cfg) => {
                // 方案相关项（活跃/可用方案、全局上屏策略）是否变化：变了才热重建引擎，
                // 避免每次保存都丢词典缓存（拼音合并/unigram 重建开销大）。
                let old = self.rt();
                // schema 段已含全局 codetable/pinyin/mix（上屏策略/调频等）；temp_pinyin 在 input 段，
                // 引擎按需缓存，故一并纳入脏判定。
                let schema_dirty = old.config.schema != cfg.schema
                    || old.config.input.temp_pinyin != cfg.input.temp_pinyin;
                // 候选窗定位方式切换的边沿检测（见下方 ReportCandidatePos）。
                let cand_was_fixed = old.config.ui.candidate.is_fixed_position();
                drop(old);

                let mods = schema_bound_modifier_vks(&self.engine_mgr);
                let bundle = std::sync::Arc::new(ConfigBundle::build(cfg, &mods));
                let new_cfg = bundle.config.clone();
                *self.rt.write().unwrap_or_else(|e| e.into_inner()) = bundle;
                info!("User config hot-reloaded (schema_dirty={})", schema_dirty);
                // 同 refresh_config_in_memory：设置页改了 ui.status.items 后，旧的去重缓存
                // 会把改动后的第一次状态显示误判成"内容没变"而吞掉。
                self.last_status_text
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                // 注释词库跟随全局配置，**不在 schema_dirty 分支内**：`[[ui.comment_dicts]]`
                // 改动本身不会把 schema 标脏，放进那个分支等于「改了挂载列表没反应，
                // 直到下次切方案才生效」。自身按路径序列做变更检测，未变即空操作。
                self.sync_comment_dicts();

                if schema_dirty {
                    // 热重建方案集：清输入缓冲、刷新工具栏/状态，免重启切换方案。
                    self.engine_mgr.reload_from_config(&new_cfg);
                    // 主码表可能变更：拆字库/字根字体随之切换（变更检测，未变不动）。
                    self.sync_chaizi_assets();
                    // 再同步一次注释库：上面那次用的是**重建前**的活跃方案，而重建可能换掉
                    // 它（方案被删除、默认方案变更）。方案专属库（`schemas`）因此要在这里
                    // 复核一遍——两次调用都有变更检测，未变的那次是空操作。
                    self.sync_comment_dicts();
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
                    *self.theme_style.lock().unwrap_or_else(|e| e.into_inner()) =
                        ThemeStyle::from_config(&new_cfg.ui.theme.style);
                }
                // 同步工具栏显隐:设置页改 ui.toolbar.visible 后运行时态跟随,再刷新工具栏。
                // 运行时镜像态回灌：这些开关运行时读 state（菜单/热键直改），config 是持久化
                // 真相源，两者只在启动时拷贝一次是不够的——设置页改了必须在此跟随，否则要重启
                // 服务才生效（症状：设置页改「检索范围」无效、而右键菜单正常）。
                let filter_changed = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.toolbar_visible = new_cfg.ui.toolbar.visible;
                    s.s2t_enabled = new_cfg.input.s2t.enabled;
                    let new_mode = wind_candidate::FilterMode::from_str(&new_cfg.input.filter_mode);
                    let changed = s.filter_mode != new_mode;
                    s.filter_mode = new_mode;
                    changed
                };
                // 检索范围变了且正在组合：以新范围重过滤刷新（与 set_filter_mode 一致，
                // 否则当前这屏候选要等下一次按键才更新）。
                if filter_changed {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if !s.input_buffer.is_empty() {
                        self.update_candidates(&mut s);
                        self.notify_ui_update(&s);
                    }
                }
                self.apply_ui_config(); // 外观项（候选排列/编码显示/候选窗显隐）即时生效
                // 「定位方式」刚从跟随切到固定：若候选窗此刻正显示着，就地固定在它当前的位置，
                // 而不是跳到陈旧的 custom_x/custom_y（用户从没拖过时是 0,0，会窜到屏幕左上角）。
                // 窗口没显示则不上报，首显时由 UI 侧落到屏幕默认锚点。与 status_toggle_pinned 同构。
                if !cand_was_fixed && new_cfg.ui.candidate.is_fixed_position() {
                    let _ = self.ui_tx.send(UiCommand::ReportCandidatePos);
                }
                self.reload_config(); // 刷新主题/工具栏（候选窗下次输入按新配置）
                self.notify_toolbar(); // 工具栏显隐(visible/全屏)按新配置即时刷新
                self.sync_global_hotkeys(); // keys.global_hotkeys 增删/改键即时生效
                self.sync_direct_switch_hotkey(); // keys.activate_ime 改键/清空即时生效
                // capslock 绑定的增删即时生效：配上才装全局钩子，删掉立刻卸载。
                self.sync_capslock_hook();
                // 推送英文自动配对配置到 TSF 客户端（client_token=0 = 广播到所有活跃客户端）
                self.push_english_pair_config(0);
                self.push_jump_out_keys_config(0); // 配对跳出键同步（英文模式跳出 + 中文转发放行）
                self.push_password_suppress_config(0); // 密码框抑制策略（DLL 本地吃键门控）
                self.push_custom_en_punct_config(0); // 英半列自定义标点：DLL 据此吃键转发
                self.push_pair_state_ttl_config(0); // 配对状态时效（DLL 侧闸门据此判陈旧）
                // 诊断采集开关本身与配置文件无关（会话级），这里重推纯属幂等保险——
                // 与 password_suppress 同样处理，让"重载一次"能修好任何 DLL 侧状态漂移。
                self.push_diag_snapshot_config(0);
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

    /// 服务重启后由新进程在就绪时弹一次「服务已重启」提示。
    ///
    /// 「重启服务」把旧进程连同其 UI 窗口线程一起销毁，退出前发 toast 用户看不到，
    /// 故反馈须由重启拉起的新进程接力（main 解析 `--restarted` 标志，service-ready 后调本方法）。
    /// Toast 由本进程 wind-ui 窗口渲染，不经 push 下发、不依赖 TSF 客户端重连，故就绪即可见。
    pub fn show_restart_toast(&self) {
        self.show_toast(
            "服务已重启",
            ToastPosition::BottomCenter,
            ToastKind::Success,
        );
    }

    /// 触发截图所有可见 UI 窗口，保存到用户配置目录下的 screenshots/ 子目录。
    pub(crate) fn trigger_screenshot(&self) {
        if let Some(dir) = wind_config::Config::user_config_dir() {
            let dir = dir.join("screenshots").display().to_string();
            let _ = self.ui_tx.send(UiCommand::TakeScreenshot { dir });
        }
    }

    /// 按当前配置（bundle）重新下发外观相关 UI 指令并同步运行时态。
    /// 热重载用：候选排列方向 / 编码显示方式 / 候选窗显隐 改动即时生效（无需重启）。
    /// 与命令栏 ime.toggle 共写同一组运行时 Mutex；以 config 为准重置（config 为持久化真相源）。
    pub(crate) fn apply_ui_config(&self) {
        let bundle = self.rt();
        let cand = &bundle.config.ui.candidate;
        // 候选排列方向（ui.candidate.layout == "vertical"）：config 是**基线**的持久化真相源，
        // 但实际下发要叠加当前模式的布局意图（见 layout.rs）——热重载不能把模式级覆盖清掉。
        // 此前这里无条件下发 config 值：模式进行中改任意一项设置都会静默取消强制竖排，
        // 且因为不留痕迹而极难复现。
        let vertical = cand.layout.eq_ignore_ascii_case("vertical");
        *self
            .candidate_vertical
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = vertical;
        {
            // 调用点（启动 / 配置重载）均不持 state 锁；加锁顺序 state → candidate_layout_sent
            // 与 notify_ui_update 一致，不构成环。
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.sync_candidate_layout(&state);
        }
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
            self.clear_hover();
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
        // 上方时反转候选顺序 / 交换编码候选栏 / 翻页栏并入编码栏
        let _ = self
            .ui_tx
            .send(UiCommand::SetCandidateFlipWhenAbove(cand.flip_when_above));
        let _ = self.ui_tx.send(UiCommand::SetCandidateSwapWhenAbove(
            cand.swap_preedit_when_above,
        ));
        let _ = self
            .ui_tx
            .send(UiCommand::SetPagerInPreedit(cand.pager_in_preedit));
        // 悬停提示延迟（ui.tooltip.delay）
        let _ = self
            .ui_tx
            .send(UiCommand::SetTooltipDelay(bundle.config.ui.tooltip.delay));
        // 工具栏自动隐藏（ui.toolbar.auto_hide / auto_hide_delay 秒→毫秒；下限 1 秒防误设 0 即隐）。
        // apply_ui_config 为启动(:717)与配置重载(:1270)共用单点，设置页改动即时生效。
        let tb = &bundle.config.ui.toolbar;
        let _ = self.ui_tx.send(UiCommand::SetToolbarAutoHide {
            enabled: tb.auto_hide,
            delay_ms: u64::from(tb.auto_hide_delay.max(1)) * 1000,
        });
        let _ = self.ui_tx.send(UiCommand::SetToolbarVertical(tb.vertical));
    }

    /// 当前活跃方案 ID（测试/诊断用）
    pub fn active_schema_id(&self) -> String {
        self.engine_mgr.active_schema_id()
    }

    /// 当前**已启用**的方案列表（`schema.available`，测试/诊断用）。
    ///
    /// 与 [`Self::active_schema_id`] 不同，这里回答的是「哪些方案会被启动预热覆盖」。
    /// 测试用它守住「目标方案确实未启用」这个前提——失去前提的回归用例会在已启用
    /// 方案上空跑一遍、永远绿。
    pub fn debug_available_schemas(&self) -> Vec<String> {
        self.engine_mgr.available_schemas()
    }

    /// 推给 TSF 的 key_up 热键白名单（测试/诊断用）。
    ///
    /// 这正是 `push_activation_status` 发出去的那份，不是另算一遍——修饰键类绑定
    /// 「能不能被触发」完全取决于它在不在这里面，用旁路重算的值断言等于没测。
    pub fn debug_key_up_hotkeys(&self) -> Vec<u32> {
        self.rt().compiled_hotkeys.key_up_tsf_hashes()
    }

    /// 直接装载短语层（仅测试用）：`(code, text, weight, position, is_system)`。
    ///
    /// ★ 补的是一个**结构性**测试缺口：真机短语层经 redb `store` 建立，而 headless 测试的
    /// `store` 是 `None` → 短语层恒空 → 所有依赖短语的判据（`has_code_prefix` 的前缀命中、
    /// z 的活码身份、夺取回路的触发条件）在测试里全都走不到。测试演示的是「z 是死码」那条
    /// 分支，真机跑的是「z 有 37 条 `zz*` 前缀」那条——两边结构性分叉，测试再绿也盖不住真机。
    ///
    /// 这个缺口让「让位判据与候选构建门槛不同源」整个漏到真机（见 `has_code_prefix` 文档）。
    pub fn debug_install_phrases(&self, records: Vec<(String, String, i32, i32, bool)>) {
        *self.phrases.write().unwrap_or_else(|e| e.into_inner()) =
            wind_phrase::PhraseLayer::from_records(records);
    }

    /// 当前是否中文标点（测试/诊断用）
    pub fn is_chinese_punct(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chinese_punct
    }

    /// 当前是否中文模式（测试/诊断用）
    pub fn is_chinese_mode(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chinese_mode
    }

    /// 是否还有更多候选未加载（测试/诊断用）
    /// 当前激活的 overlay 模式类别名；`None` = 普通输入。仅供测试断言。
    pub fn debug_active_mode(&self) -> Option<&'static str> {
        match self.state.lock().unwrap_or_else(|e| e.into_inner()).active {
            Some(ModeKind::TempPinyin) => Some("temp_pinyin"),
            Some(ModeKind::TempEnglish) => Some("temp_english"),
            Some(ModeKind::Url) => Some("url"),
            Some(ModeKind::Special(_)) => Some("special"),
            Some(ModeKind::Mix(_)) => Some("mix"),
            None => None,
        }
    }

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

    /// 注入「候选窗当前是否反转排列」（测试/诊断用）。
    ///
    /// 刻意走 [`Coordinator::handle_ui_event`] 而非直接写字段——正式路径是 UI 线程发
    /// `UiEvent::CandidateFlipped`，测试入口跳过分发就测不到那条接线（同 `debug_candidate_op`）。
    pub fn debug_set_candidate_flipped(&self, flipped: bool) {
        self.handle_ui_event(UiEvent::CandidateFlipped(flipped));
    }

    /// 将统计采集器内存数据落库（测试/诊断用；生产由后台线程定时 flush）。
    pub fn debug_flush_stats(&self) {
        if let Some(c) = self.stat_collector.as_ref() {
            c.flush();
        }
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
            .map(|c| self.cand_s2t_text(&s, c))
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

    /// 清除鼠标悬停目标（无需 state 锁，见 [`Coordinator::hover_index`] 的说明）。
    ///
    /// 调用点＝一切「悬停不再对应屏幕上任何东西」的时刻：候选窗隐藏、候选列表重新装填、
    /// 键盘移动高亮/翻页。少接一处的后果是**静默的**——悬停高亮与 tooltip 会在下一次候选窗
    /// 出现时凭空复现，且鼠标从未移动过。
    pub(crate) fn clear_hover(&self) {
        self.hover_index
            .store(-1, std::sync::atomic::Ordering::Relaxed);
    }

    /// 当前鼠标悬停目标（原始 tag；-1 = 无）。
    pub(crate) fn hover_target(&self) -> i32 {
        self.hover_index.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 候选列表重新装填 / 组合清空后的**视图复位**：翻页归零、键盘高亮归零、鼠标悬停清除。
    ///
    /// # ★★ 三件事必须一起做
    ///
    /// 此前只有主路径 `update_candidates` 三件齐全，特殊模式 / 临拼 / 混输 / 快捷输入的
    /// 8 个装填点都只做了前两件——漏掉的第三件让悬停高亮与 tooltip 跨按键、跨组合、跨模式
    /// 存活（2026-08-12 用户反馈）。而普通输入每敲一键都重走主路径把残留覆盖掉，
    /// **该缺陷在主路径上物理不可观测**，只有 overlay 模式才露馅。
    ///
    /// 收进一处后，新增候选来源时能漏的只剩「忘了调用本函数」——比在三行里少写一行显眼得多。
    pub(crate) fn reset_candidate_view(&self, state: &mut State) {
        state.current_page = 0;
        state.selected_index = 0;
        self.clear_hover();
    }

    /// 上移高亮（页首回卷到上一页末项）；返回是否变化
    fn move_up(&self, state: &mut State) -> bool {
        self.clear_hover();
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
        self.clear_hover();
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
        self.clear_hover();
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
        self.clear_hover();
        // 接近末页且有更多 → 先动态扩展加载，使新页可达
        if state.has_more && state.current_page + 2 >= self.total_pages(state) {
            self.expand_candidates(state);
        }
        if state.current_page + 1 < self.total_pages(state) {
            state.current_page += 1;
            state.selected_index = 0;
            true
        } else {
            // 已在末页仍按向后翻页 ⇒「翻到底了还想看更多」＝明确的放宽意图。
            self.try_relax_scope_on_page_end(state)
        }
    }

    /// 组合结束（输入缓冲已空）时让临时放宽失效，恢复配置的检索范围档位。
    ///
    /// 判据取「缓冲是否为空」而非「是否发生了上屏」：上屏、ESC 取消、切焦点清空、模式切换
    /// 都会清空缓冲，一个判据全覆盖，无需逐路径接线。放宽期间敲字母/退格/翻页时缓冲非空，
    /// 状态得以保持——找生僻字常要改几次编码。
    fn expire_scope_override(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.scope_relaxed {
            return;
        }
        // ⚠️ 判据是「**当前模式的**输入缓冲已空」。临拼的码在 `temp_pinyin_buffer`，
        // 而它的 `input_buffer` 恒为空——用 `input_buffer` 一刀切会让临拼刚放宽就在下一次
        // 按键被清掉，且**静默**（用户只看到「按了没用」）。退出临拼后 `active` 已变回
        // 非 TempPinyin，走 `input_buffer` 分支照常失效。
        let ended = if matches!(s.active, Some(ModeKind::TempPinyin)) {
            s.temp_pinyin_buffer.is_empty()
        } else {
            s.input_buffer.is_empty()
        };
        if ended {
            s.scope_relaxed = false;
        }
    }

    /// 末页再按向后翻页键 → 临时放宽检索范围为「全部字符」，重建候选并翻到新增的那页。
    ///
    /// 设计见 `docs/design/smart-filter-scope-relax.md` §5。这是三类引擎**通用的主入口**：
    /// 码表候选少、翻两下就到底；拼音候选多，但用户找生僻字本就会一路翻页，翻到底同样是
    /// 明确信号。挂在既有的「翻不动就返回 false」分支上，不占任何键位。
    ///
    /// 返回是否真的发生变化（上层据此决定重绘）。放宽后若没有新增候选则**撤销**，
    /// 避免留下一个什么也没带来、却会影响后续按键的放宽态。
    fn try_relax_scope_on_page_end(&self, state: &mut State) -> bool {
        if !self.rt().config.input.scope_relax.page_end_key {
            return false;
        }
        // 已放宽过就不再重复。放宽是**智能档专属**的补偿：只有智能档会按「同码位有常用字」
        // 滤掉生僻字，也只有它需要一条把被滤掉的放回来的出路（见上方引用的设计文档，全篇
        // 以 `filter_mode = "smart"` 为前提）。常用字档若也能放宽，它与智能档的差异就被
        // 抹平了——用户选「常用字」要的正是一个稳定只出常用字的列表；`Gb18030` 本就不过滤，
        // 更无可放宽。
        if state.scope_relaxed || state.filter_mode != wind_candidate::FilterMode::Smart {
            return false;
        }
        // ⚠️ 临拼的码在 `temp_pinyin_buffer`，主路径的在 `input_buffer`——须按当前模式取。
        // 用 `input_buffer` 一刀切会让临拼**永远触发不了**（那边恒为空），且没有任何报错。
        let in_temp = matches!(state.active, Some(ModeKind::TempPinyin));
        let has_input = if in_temp {
            !state.temp_pinyin_buffer.is_empty()
        } else {
            !state.input_buffer.is_empty()
        };
        if !has_input {
            return false;
        }
        state.scope_relaxed = true;
        // 两条路径的候选重建函数不同：临拼走 overlay 的那套（主路径的 `build_candidates`
        // 读 `input_buffer`，在临拼下会构建出空列表）。
        let page_before = state.current_page;
        if in_temp {
            // ⚠️ `update_temp_pinyin_candidates` 会把 current_page/selected_index 归零，
            // 重建后须还原，否则用户翻到的位置丢失。
            self.update_temp_pinyin_candidates(state);
            state.current_page = page_before;
        } else {
            let limit = state.candidate_limit;
            self.build_candidates(state, limit);
        }
        // 判据取「列表里有没有真的出现被滤候选」，而非「总数是否变多」——候选受 limit 截断时
        // 总数可能不变，那样会误判成「没放出东西」而撤销。
        if !state.candidates.iter().any(|c| c.is_scope_filtered) {
            // 该码位本就没有被滤的字 → 原样撤销，别留一个什么也没带来、却会影响后续按键的放宽态
            state.scope_relaxed = false;
            return false;
        }
        // 放宽出来的候选**追加在末尾**，所以照常翻到下一页就能看到，与「继续往后翻」的动作
        // 语义完全一致。⚠️ 曾让放宽后的候选按真实顺序插入，结果 `dwi` 的新字（权重 8999 占
        // 三简位）落到第 1 页第 2 位，视口只能跳回页首——翻页翻着翻着跳回开头，很突兀。
        if state.current_page + 1 < self.total_pages(state) {
            state.current_page += 1;
            state.selected_index = 0;
        }
        true
    }

    /// 会话态按键绑定的统一执行（配置驱动，见 `keys.session_actions`）：翻页 / 移高亮 /
    /// 取消。普通模式与所有 overlay 模式共用；`include_printable` 区分码表型（`-`/`=` 作
    /// 翻页）与文本/表达式型（临英/快捷输入，`-`/`=` 作输入字符，不夺为动作）。
    ///
    /// 命中并执行返回 `Some`，未命中或条件不足返回 `None`（键回落调用方的原有处理）。
    ///
    /// # ★ 守卫按动作分，不按调用点分
    ///
    /// 导航类只在有候选时有意义（`requires_candidates`），`cancel` 则在「打了码还没出
    /// 候选」时也必须生效。判据挂在 `SessionAction` 上而不是写在这里的 `if`——本函数有
    /// 三个调用点（主输入 / mix / 候选导航），条件写死在函数体内还好，写到调用点上就是
    /// 三份要保持一致的守卫，那正是本仓栽过四次的形状。
    pub(crate) fn apply_session_action(
        &self,
        state: &mut State,
        data: &KeyEventData,
        include_printable: bool,
    ) -> Option<KeyAction> {
        let shift = data.modifiers & MOD_SHIFT != 0;
        let action = self
            .rt()
            .session_keys
            .classify(data.key_code, shift, include_printable)?;
        if action.requires_candidates() && state.candidates.is_empty() {
            return None;
        }
        let nav = match action {
            wind_config::SessionAction::HighlightUp => keymap::NavAction::HighlightUp,
            wind_config::SessionAction::HighlightDown => keymap::NavAction::HighlightDown,
            wind_config::SessionAction::PagePrev => keymap::NavAction::PagePrev,
            wind_config::SessionAction::PageNext => keymap::NavAction::PageNext,
            wind_config::SessionAction::Cancel => {
                // 无会话时放行：空闲按 Tab 该是宿主的制表符，不是「取消一个不存在的输入」。
                // 判据与 `cancel_session` 的适用范围一致，见那里。
                if !Self::has_input_session(state) {
                    return None;
                }
                return Some(self.cancel_session(state));
            }
            // 选词 / 以词定字**刻意不在这里执行**，返回 None 让键落到各自的既有消费点
            // （`select_char_index` 在本函数之前、`select_key_offset` 在数字选词臂之后）。
            //
            // ★ 理由是它们带 **overflow 语义**：候选不足 / 词长不够时要按
            // `keys.overflow.{select_key,select_char_key}` 分档处置（吞键 / 上屏高亮候选 /
            // 上屏并追加字符），而本函数只有「命中就执行」一种结局。搬进来就得把三档策略
            // 和各模式的选中出口一起搬，那是把两件事挤进一个函数。
            //
            // 收编改变的是**配置从哪来**（session_actions 而非 select_key_groups），
            // 不是执行路径——后者一行未动，故 overflow 与各模式的选中语义零回归。
            wind_config::SessionAction::SelectCandidate(_)
            | wind_config::SessionAction::SelectChar(_) => return None,
            // 表里只存启用项（`ConfigBundle::build` 过滤过），None 到不了这里。
            wind_config::SessionAction::None => return None,
        };
        // 候选被反转排列时，高亮移动按**屏幕上看到的方向**走：竖排 + 上翻 + flip_when_above
        // 三者同时成立时，屏幕从上到下是候选 n..1，此时 ↑ 对应的是候选序的「下一个」。
        // 不区分按键（↑/↓ 与 Shift+Tab/Tab 一并翻转）——这两组都绑在同一对
        // `highlight_up`/`highlight_down` 上，行为分叉会让「同一个动作两种走向」。
        //
        // **翻页键不在此列**：页与页之间没有空间关系（新页在原处整体替换），反转只发生在页内。
        //
        // 回卷语义无需另写：反转后视觉最下方是页内第 0 项，按 ↓ 越界 == `move_up` 的
        // 「页首回卷到上一页末项」，两者本就是同一件事。
        let flipped = self
            .candidate_flipped
            .load(std::sync::atomic::Ordering::Relaxed);
        let changed = match nav {
            keymap::NavAction::HighlightUp if flipped => self.move_down(state),
            keymap::NavAction::HighlightDown if flipped => self.move_up(state),
            keymap::NavAction::HighlightUp => self.move_up(state),
            keymap::NavAction::HighlightDown => self.move_down(state),
            keymap::NavAction::PagePrev => self.page_prev(state),
            keymap::NavAction::PageNext => self.page_next(state),
        };
        if changed {
            // 混输高亮跟随：普通模式下高亮在五笔↔拼音候选间移动可能切换 preedit 形态
            // （原始码 ↔ 音节拆分）。重算 preedit；若形态变化且嵌入编码（app_inline），须回传
            // 组合串使宿主内联编码同步；候选窗模式仅 notify_ui_update 刷新即可。
            // 门控：仅普通模式（active==None）且存在拆分形态——纯五笔(无拆分)/纯拼音(全拼音
            // 候选→形态恒定)均不触发，零回归。
            let mut composed: Option<KeyAction> = None;
            if state.active.is_none() && !state.preedit_split_body.is_empty() {
                let before = state.preedit.clone();
                self.sync_preedit_to_highlight(state);
                if state.preedit != before {
                    let in_app = self
                        .preedit_display
                        .lock()
                        .map(|m| m.in_app())
                        .unwrap_or(true);
                    if in_app {
                        let text = state.preedit.clone();
                        let caret_pos = text.chars().count() as u32;
                        composed = Some(KeyAction::UpdateComposition { text, caret_pos });
                    }
                }
            }
            self.notify_ui_update(state);
            if let Some(act) = composed {
                return Some(act);
            }
        }
        Some(KeyAction::Consumed)
    }

    /// 当前是否有输入会话：正在 overlay 模式里，或普通输入有编码 / 候选 / 已上屏段。
    ///
    /// 与 C++ 的 `_HasInputSession()`（`hasComposition || _hasCandidates`）**语义对齐**：
    /// overlay 模式一定持有 composition。两侧判据必须同构，否则会出现「C++ 吃了键、
    /// 服务端这边判定无会话不接管」的丢键，或反过来「C++ 放行了、这边却想处理」。
    ///
    /// ⚠️ 不能只判 buffer 非空：overlay 模式在**空缓冲**时按取消键同样要退出模式——
    /// 那时「退出」本身就是用户要的动作。
    pub(crate) fn has_input_session(state: &State) -> bool {
        state.active.is_some()
            || !state.input_buffer.is_empty()
            || !state.candidates.is_empty()
            || !state.committed_text.is_empty()
    }

    /// 放弃当前输入会话：清掉未上屏内容，并退出所在的 overlay 模式。**Esc 的语义单点**。
    ///
    /// # 收敛了六处逐字重复的实现
    ///
    /// 主输入路径与五个 overlay handler 此前各写一份 Esc 分支，形态完全一致
    /// （`exit_X` + `notify_ui_hide` + `ClearComposition`），**差异只在退出函数**，
    /// 而那按 `state.active` 分派即可。散着的代价不是重复本身，是「回车五条路径」
    /// 那次的形状：任何一条新逻辑都只惠及主路径，其余五处静默落后。
    ///
    /// ⚠️ 菜单（`menu_open`）与快捷加词（`add_word_active`）**刻意不收**：它们是模态窗口，
    /// 菜单的键直接转发给 UI 窗口自行解释（`UiCommand::MenuKey`），协调器这边根本不决定
    /// 语义；加词模式则消费全部按键。要让自定义取消键在那两处也生效，得改 `wind-ui` 的
    /// 键解释器，是另一层的事。
    pub(crate) fn cancel_session(&self, state: &mut State) -> KeyAction {
        match state.active {
            Some(ModeKind::TempPinyin) => self.exit_temp_pinyin(state),
            Some(ModeKind::TempEnglish) => self.exit_temp_english(state),
            Some(ModeKind::Url) => self.exit_url_mode(state),
            Some(ModeKind::Special(_)) => self.exit_special_mode(state),
            Some(ModeKind::Mix(_)) => self.exit_mix_mode(state),
            // 普通输入：取消整个组合，含已转换前缀（拼音分步上屏的那部分）一并丢弃。
            None => self.reset_pinyin_composition(state),
        }
        self.notify_ui_hide();
        KeyAction::ClearComposition
    }

    /// keyup-only 键（CapsLock / 纯修饰键）上的会话态绑定（`keys.session_actions`）。
    ///
    /// 这批键**只有 keyup 到得了服务端**：C++ 对纯修饰键的 keydown 一律放行不吃（吃掉会让
    /// AutoCAD 看不到修饰键、正交模式覆盖失效），CapsLock 的 keydown 则压根不转发给服务端。
    /// 所以它们的绑定只能在这里消费——挂到 keydown 链上是配得上、永不触发。
    ///
    /// 一期只有导航类动词，[`Self::apply_session_action`] 自带「无候选返回 `None`」的守卫，正好
    /// 实现「有会话归绑定、无会话归原语义」：空闲时按 CapsLock 仍然切大小写。
    ///
    /// ⚠️ 二期加 `clear` / `cancel` 时，判据要放宽到「有编码**或**有候选」——那时改**这一处**
    /// 的守卫，别在各调用点各判一次（Esc 散成七处就是那么来的）。
    fn handle_session_action_key_up(&self, data: &KeyEventData) -> Option<KeyAction> {
        if !keymap::is_key_up_only_vk(data.key_code) {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // include_printable 取值在这里无关紧要——keyup-only 键没有一个是可打印的。
        // 传 true 与主输入路径保持一致，免得日后有人照抄这行时带走一个错误的先例。
        self.apply_session_action(&mut state, data, true)
    }

    /// 该字符此刻能否进输入缓冲：缓冲为空时查**首码集**，否则查码元**全集**。
    ///
    /// 首码判据取 `input_buffer.is_empty()`，而不是「无候选且无已提交」——码是按
    /// `input_buffer` 查询的，缓冲空就是新一轮码的开头；分步上屏后续打的第一个字符
    /// 同样算首码，与引擎的查询语义保持一致。
    ///
    /// 默认码元集（`a-z`）下，字母恒为真、其余恒为假，与历史逐键等价。
    pub(crate) fn can_enter_buffer(&self, state: &State, ch: char) -> bool {
        if state.input_buffer.is_empty() {
            self.engine_mgr.active_is_leading_char(ch)
        } else {
            self.engine_mgr.active_is_code_char(ch)
        }
    }

    /// 非码元字符的处置：终结当前组合并输出该字符。
    ///
    /// ⚠️ **刻意不透传**。C++ 在中文模式下对字母键是**无条件吃**的
    /// （`KeyEventSink.cpp` 的 `chinese_letter` 分支，仅 CapsLock 透传例外），
    /// 此处返回 `PassThrough` 就构成「吃了再吐」：不补发 `WM_KEYDOWN` 的宿主
    /// （EverEdit 一类）直接丢字符，全角态下还会出半角。故一律由本侧出字——
    /// 铁律是「C++ 吃键集 ⊆ Rust 出字集」，见 project_fullwidth_eat_flip。
    ///
    /// 空组合时同样走这条路：`commit_highlight_then_char` 在无候选无已提交时
    /// 只输出该字符（并按全角态转换），正是需要的行为。
    pub(crate) fn reject_non_code_char(&self, state: &mut State, ch: char) -> KeyAction {
        let has_comp = !state.input_buffer.is_empty()
            || !state.committed_text.is_empty()
            || !state.candidates.is_empty();
        self.commit_highlight_then_char(state, ch, has_comp)
    }

    /// 码元字符进缓冲的公共通路：插入 → 顶码上屏 → 候选刷新 → 组合区更新。
    ///
    /// 字母臂与非字母码元闸门（[`Self::try_code_char_gate`]）共用本函数——两条路进来的
    /// 只是「哪个键产出了这个字符」不同，进缓冲之后的处置完全一致。**不要复制这段**：
    /// 顶码的显示首选一致性、自动上屏的记账码分流都在这里，复制出去必然漂移。
    ///
    /// `ch` 是进缓冲的小写码元，`raw` 是进影子串的原始形态（Shift 大写等）。
    pub(crate) fn accumulate_code_char(&self, state: &mut State, ch: char, raw: char) -> KeyAction {
        // 顶码前记住「即将成为前缀」的缓冲及其显示首选：顶码上屏文本须与用户实际所见的
        // 首候选一致——调频置顶 / shadow 在协调器层重排（apply_freq_rerank/apply_shadow），
        // 引擎 handle_top_code 内部 convert 看不到，会顶出权重首选而非显示首选（对齐 Go
        // 复用 ConvertEx 取 Candidates[0] 的一致性修复）。顶码绝大多数发生在「满码+1」，
        // 此时前缀恰为顶码前缓冲，state.candidates 正是其显示候选。
        let pre_buf = state.input_buffer.clone();
        // 顶码上屏候选 = 用户实际所见的**显示首选**：取顶码前缓冲（即将成为前缀）的显示
        // 首候选——它已过智能过滤 / 词频重排 / shadow，正是用户所见。保留整条候选（含
        // is_command / phrase_template / group_code），供顶码分流：码表候选 & 普通短语 →
        // 文本顶上屏；$CC 命令短语 → 求值执行。短语 source 为 `Phrase`（**不参与**本
        // filter，放行靠 is_phrase / is_command 显式判定，与 source 取值无关）；拼音/英文
        // 候选（拼音本就排首，或智能过滤掉生僻码表字后仅剩拼音，如「wang」只有生僻字
        // 「佢」被过滤、显示全是拼音）仍被排除 → 下方放弃顶码继续组合
        // （对齐「上屏须与显示一致 + 非码表/短语类不上屏」）。
        let pre_display_first = state
            .candidates
            .first()
            .cloned()
            .filter(|c| c.source == CandidateSource::CodeTable || c.is_phrase || c.is_command);
        // 在光标处插入（光标在末尾时等价于旧的 push）。后续顶码/候选刷新一律按整串
        // 缓冲判定，与光标位置无关——光标只是编辑位置，不参与引擎查询。
        preedit_cursor::BufEdit::new_cased(
            &mut state.input_buffer,
            &mut state.input_cursor_pos,
            &mut state.input_buffer_cased,
        )
        .insert_cased(ch, raw);

        // 顶码上屏：缓冲超过满码长且整串无匹配 → 顶前 N 码首选，余码续打
        // （schema.top_code_commit；置于候选刷新前，对齐 Go handleAlphaKey）。
        if let Some((engine_top, remainder)) = self.engine_mgr.handle_top_code(&state.input_buffer)
        {
            let buf = state.input_buffer.clone();
            let prefix: String = buf[..buf.len().saturating_sub(remainder.len())].to_string();
            // 顶码候选决策：
            // - prefix==顶码前缓冲（满码+1，最常见）：用显示首选候选（码表/普通短语/命令）；
            //   显示首选非码表且非短语 → None → 放弃顶码（继续组合让用户选拼音）。
            // - 否则（多级溢出，罕见 wubi 场景）：回退引擎码表顶码纯文本（无命令语义）。
            if prefix == pre_buf {
                match pre_display_first {
                    // $CC 命令短语顶码：纯文本命令（≈词条）同步求值文本走标准文本顶码；
                    // 含副作用命令（开应用/切设置等）异步执行 + 余码走标准流程。
                    Some(cand) if cand.is_command => {
                        let input = if cand.group_code.is_empty() {
                            prefix.clone()
                        } else {
                            cand.group_code.clone()
                        };
                        return match self.eval_command_text_only(&cand.phrase_template, &input) {
                            Some(text) => {
                                self.commit_top_text(state, &prefix, text, &remainder, cand.source)
                            }
                            None => self.top_commit_command_with_remainder(
                                state, &cand, &prefix, &remainder,
                            ),
                        };
                    }
                    // 码表候选 / 普通短语：文本顶上屏 + 余码续打。
                    Some(cand) => {
                        let source = cand.source;
                        return self.commit_top_text(state, &prefix, cand.text, &remainder, source);
                    }
                    // 显示首选是拼音/英文 → 放弃顶码，落到下方正常候选刷新继续组合。
                    None => {}
                }
            } else if !engine_top.is_empty() {
                // 多级溢出：引擎码表纯文本顶码（码表无字则 engine_top 空 → 放弃顶码，
                // 落到下方正常候选刷新继续组合）。此路来自引擎码表查询，确为码表来源。
                return self.commit_top_text(
                    state,
                    &prefix,
                    engine_top,
                    &remainder,
                    CandidateSource::CodeTable,
                );
            }
        }

        // 全码自动上屏 / 满码空码清空（schema.auto_commit_at_full / clear_on_empty_max）。
        match self.update_candidates(state) {
            InputOutcome::AutoCommit(text) => {
                // 自动上屏文本取自首候选（handle_candidate.rs 构造 AutoCommit 时同源）。
                // 记账码同取首候选（按来源分流，见 `freq_code`），无候选时退回输入缓冲。
                let (source, code) = state
                    .candidates
                    .first()
                    .map(|c| (c.source, self.freq_code(&state.input_buffer, c)))
                    .unwrap_or_else(|| (CandidateSource::default(), state.input_buffer.clone()));
                let out = self.commit_candidate(state, &text, None, source, &code);
                self.notify_ui_hide();
                return Self::commit_action(out, true);
            }
            // 含副作用命令自动命中：与空格选中命令同路（清组合 + 异步执行）。
            InputOutcome::AutoCommand(cand) => {
                return self.commit_command(state, &cand);
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
        let caret_pos = self.composition_caret(state);
        self.notify_ui_update(state);
        KeyAction::UpdateComposition {
            caret_pos,
            text: display,
        }
    }

    /// 非字母码元闸门：本方案把某个数字/符号配成了码元，且此刻允许它进缓冲 → 接管。
    ///
    /// 置于优先级链的「模式激活/URL 夺取之后、以词定字/翻页/大 match 之前」，于是组码中
    /// 的码元抢在选词键、翻页键、标点流水线之前——这正是「组码中码元优先」契约。
    /// 空缓冲时 `can_enter_buffer` 查的是**首码集**，数字默认不在其中 ⇒ 不接管，
    /// 数字键照常作选词/透传，用户不会失去「选第 1 个候选」与原生数字输入。
    ///
    /// 字母**不走这里**：它们在大 match 的字母臂处理，那里还有 z-fallback 等字母专属判定。
    ///
    /// ⚠️ 默认码元集 `a-z` 不含任何非字母字符 ⇒ 本闸门恒返回 `None`，与历史逐键等价。
    pub(crate) fn try_code_char_gate(
        &self,
        state: &mut State,
        data: &KeyEventData,
    ) -> Option<KeyAction> {
        // Ctrl/Alt 组合不是码元输入。上游已拦截，此处为纵深防御。
        if data.modifiers & (MOD_CTRL | MOD_ALT) != 0 {
            return None;
        }
        let shift = data.modifiers & MOD_SHIFT != 0;
        let ch = printable_char(data.key_code, shift)?;
        if ch.is_ascii_alphabetic() {
            return None;
        }
        // 缓冲恒存小写（与字母同域）；`ch` 作为原始形态进影子串。
        let lower = ch.to_ascii_lowercase();
        if !self.can_enter_buffer(state, lower) {
            return None;
        }
        Some(self.accumulate_code_char(state, lower, ch))
    }

    /// 码元字符集与既有按键功能的冲突清单：`(字符, 占用它的功能名)`，空 = 无冲突。
    ///
    /// 「组码中码元优先」意味着配成码元的符号会从翻页/次选/以词定字/引导键手里被夺走。
    /// 这是方案作者的选择，不该阻止；但必须让他知道——否则现场表现是「翻页键忽然不灵了」，
    /// 而两处配置分开看都合理，无从查起。
    ///
    /// 判定**反查现有函数**而非重新解析配置：`page_keys` 一类存的是键组名（`minus_equal`），
    /// 自己再解析一遍必然与 `NavKeys::from_config` 漂移。此处对码元集里的每个非字母字符
    /// 找回它的 VK，再逐个问那些判定函数「这个键归你吗」——判据因此永远与实际行为同源。
    ///
    /// 只查非字母：字母本就是默认码元，且字母触发键（z）有专门的裁决顺序，不构成冲突。
    pub fn code_char_conflicts(&self) -> Vec<(char, Vec<&'static str>)> {
        let charset = self.engine_mgr.active_input_chars();
        if charset.is_default_alpha() {
            // 默认集只有字母，不可能与符号类功能冲突——顺带免掉一整轮反查。
            return Vec::new();
        }
        let rt = self.rt();
        let mut out = Vec::new();
        for ch in charset.chars() {
            if ch.is_ascii_alphabetic() {
                continue;
            }
            let Some(vk) = char_to_main_vk(ch) else {
                continue;
            };
            let mut owners: Vec<&'static str> = Vec::new();

            // ── 组码中类占用：码元在组码中恒优先，故恒冲突 ──
            //
            // 数字选词是硬编码的 VK_1..=VK_9 / VK_0 臂，不经任何配置，故单独判。
            // 数字配成码元即等于放弃组码期间的数字选词（一刀切让位，见设计文档 §3.3）。
            if ch.is_ascii_digit() {
                owners.push("数字选词键");
            }
            // 会话态绑定：翻页/移高亮/取消都在组码期间抢这个键，故都算占用。
            // 措辞按实际动作分——设置页把这行原样显示给用户，笼统写「会话态按键」
            // 等于让用户自己去查是哪个功能占了。
            if let Some(a) = rt.session_keys.classify(vk, false, true) {
                owners.push(match a {
                    wind_config::SessionAction::Cancel => "取消键",
                    _ => "翻页/高亮键",
                });
            }
            if self.select_key_offset(vk).is_some() {
                owners.push("次选键");
            }
            if self.select_char_index(vk).is_some() {
                owners.push("以词定字键");
            }

            // ── 空缓冲类占用：模式引导键 ──
            //
            // ★ 只在该字符**可作首码**时才是真冲突。首码仲裁
            // （`code_char_takes_lead`）此时让引导键让位给码表 ⇒ 该模式再也进不去。
            // 不能作首码时两者井水不犯河水——模式只在空缓冲用、码元只在组码中用，
            // 报出来只会变成噪音，把真冲突淹掉。
            if charset.contains_leading(ch) {
                if self.match_special_trigger(vk).is_some() {
                    owners.push("特殊模式引导键");
                }
                if self.match_mix_trigger(vk).is_some() {
                    owners.push("快捷输入/混输引导键");
                }
                if self.is_temp_pinyin_trigger(vk) {
                    owners.push("临时拼音触发键");
                }
                if self.is_temp_english_trigger(vk) {
                    owners.push("临时英文触发键");
                }
            }
            if !owners.is_empty() {
                out.push((ch, owners));
            }
        }
        out
    }

    /// 启动时把 [`Self::code_char_conflicts`] 的结果写进日志。只告警，不改行为。
    pub(crate) fn warn_code_char_conflicts(&self) {
        let charset = self.engine_mgr.active_input_chars();
        for (ch, owners) in self.code_char_conflicts() {
            // 后果按「能否作首码」分档：首码意味着连空缓冲都归码表，被占用的模式引导键
            // 会彻底进不去；仅后续码则只影响组码期间。文案里直接给出化解办法，
            // 否则用户看到告警也不知道下一步该改哪。
            if charset.contains_leading(ch) {
                warn!(
                    "码元集含 {:?} 且允许其作首码，但该键原配作 {}；空缓冲时它将归码表，这些功能再也进不去。\
                     要两者共存：把它排除出 leading_chars（它便只在组码中作码元）",
                    ch,
                    owners.join(" / ")
                );
            } else {
                warn!(
                    "码元集含 {:?}（仅作后续码），该键同时配作 {}；组码中它归码表，这些功能在组码期间失效，空缓冲时不受影响",
                    ch,
                    owners.join(" / ")
                );
            }
        }
    }

    /// 普通模式「顶屏高亮候选 + 输出字符」：把已转换前缀与当前高亮候选一并上屏，再接该字符。
    /// 小键盘 direct 语义共用此路（编码型缓冲里数字不是合法编码，故终结当前组合而非入缓冲；
    /// 但**不丢弃**用户已打的码——顶屏它，对齐主键盘标点键的既有行为）。
    ///
    /// `has_comp` 由调用方在改动 state 前算好：空组合时无需隐藏候选窗。
    pub(crate) fn commit_highlight_then_char(
        &self,
        state: &mut State,
        ch: char,
        has_comp: bool,
    ) -> KeyAction {
        let committed = self.take_committed(state);
        let mut out = self.maybe_s2t(state, &committed);
        if !state.candidates.is_empty() {
            let idx = self
                .highlighted_global_index(state)
                .min(state.candidates.len() - 1);
            let cand = state.candidates[idx].clone();
            // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
            let freq_code = self.freq_code(&state.input_buffer, &cand);
            self.record_selection(&freq_code, &cand.text, cand.source);
            out.push_str(&self.cand_s2t_text(state, &cand));
        }
        state.input_buffer.clear();
        state.candidates.clear();
        if has_comp {
            self.notify_ui_hide();
        }
        // 英文补空格（`schema.english.commit_space`）**刻意不接这里**：本函数的用途是
        // 「顶掉高亮候选 + 紧接着上屏这个字符」，补了会得到 `hello ,` 这种断开的标点。
        // 不是漏接。
        out.push_str(&if state.full_width {
            to_full_width(&ch.to_string())
        } else {
            ch.to_string()
        });
        Self::commit_action(out, state.chinese_mode)
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
        state.input_buffer_cased.clear();
        state.input_cursor_pos = 0;
        state.preedit.clear();
        state.preedit_split_body.clear();
        state.preedit_fp_body.clear();
        state.shadow_code.clear();
        state.candidates.clear();
        self.reset_candidate_view(state);
    }

    /// cmdbar 能力 wrapper（被 handle_cmdbar 控制器经 Weak 回调）。各方法自锁，**禁止**在持
    /// state 锁时调用（spawn_command 已确保在独立线程、未持锁时执行）。
    /// 撤销最近一次上屏（cmdbar `ime.undo_commit`）：删除光标前 `last_commit_len` 个字符
    /// （UTF-16 单元），推 ReplaceBackward(N, "") 给活跃客户端（复用智能标点删除替换通道及其
    /// 全部宿主兼容修复）。计数语义见 [`Self::last_commit_len`]：默认 1 → 永远有动作；被最近
    /// 一次上屏覆盖 → 只精准删「刚输入完那次」；`swap(1)` 读取即复位 → 连续触发第二次起逐字删
    /// （数量不再可信，宁可少删多按几次，也不按陈旧计数误删多个）。
    ///
    /// v1 不校验光标前内容（用户主动触发；焦点变化/其它输入均已把计数刷回 1，故误删至多 1 个）；
    /// v2 预留 prevChar 比对。已知限制：SendInput 退格兜底宿主按「一次退格删一整字」处理时，
    /// emoji 会多删（兜底宿主 × emoji 双重边缘），留待后续按宿主特判。
    pub(crate) fn cmd_undo_commit(&self) {
        // 正在打字（缓冲非空）时不动作：ReplaceBackward 作用于已上屏文本，
        // 与组合态并存会把删除落进组合窗前的位置，语义混乱。
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.input_buffer.is_empty() {
                debug!("undo_commit: 输入缓冲非空，忽略");
                return;
            }
        }
        // 读取并复位为 1：撤销一次后计数即失效，下次 undo 退化删 1（除非其间又有新上屏刷新）。
        let count = self
            .last_commit_len
            .swap(1, std::sync::atomic::Ordering::Relaxed) as u32;
        if count == 0 {
            return;
        }
        debug!("undo_commit: 删除 {} 个 UTF-16 单元", count);
        let encoded = wind_ipc::codec::encode_replace_backward(count, "");
        let _ = self.push_server.push_commit_to_active(&encoded);
    }

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
            self.clear_hover();
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
        // 翻转的是**基线**；实际下发仍要叠加当前模式意图（见 layout.rs），否则在强制竖排的
        // 模式里切换会绕过覆盖直接改方向，且去重缓存与真实下发值脱节。
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.sync_candidate_layout(&state);
        }
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

    /// 第 `i` 个候选（0 基）的序号标签，按「用户配置 > 主题 > 默认数字」裁决：
    /// ① 用户 `ui.candidate.index_labels` 显式设了该槽位 → 用之；
    /// ② 否则当前主题 `views.index.labels` 有非空槽位 → 用之；
    /// ③ 否则回退默认 (i+1)。
    fn resolve_index_label(
        &self,
        cand_cfg: &wind_config::config::UiCandidateConfig,
        i: usize,
    ) -> String {
        if let Some(s) = cand_cfg.user_index_label(i) {
            return s;
        }
        if let Some(s) = self
            .theme_index_labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(i)
            .filter(|s| !s.is_empty())
        {
            return s.clone();
        }
        (i + 1).to_string()
    }

    pub(crate) fn notify_ui_update(&self, state: &State) {
        // CapsLock 钩子闸门：本函数是候选/编码状态变化后的必经出口，挂在这里覆盖面最大。
        // 放在最前面，使下方的 early return（无候选无编码 → 隐藏）也走得到。
        self.sync_capslock_gate(state);
        // 模式指示标记（拼/双/快/英/符）：仅在候选为空时显示（进入模式/无候选阶段），
        // 一旦有候选即隐藏，减少干扰。必须纳入下方"空则隐藏"守卫——否则进入模式时
        // 缓冲为空会直接隐藏，标记发不出。
        let mode_label = if state.candidates.is_empty() {
            self.mode_indicator_text(state).unwrap_or_default()
        } else {
            String::new()
        };
        if state.candidates.is_empty() && state.input_buffer.is_empty() && mode_label.is_empty() {
            self.clear_hover(); // 组合结束的最常见隐藏出口（不经 notify_ui_hide），须自行归零
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            self.reset_first_show();
            return;
        }
        // candwin 切换：用户隐藏候选窗时不显示（仍可盲打/自动上屏）。
        if *self
            .hide_candidate_window
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            self.clear_hover();
            let _ = self.ui_tx.send(UiCommand::HideCandidates);
            self.reset_first_show();
            return;
        }
        // 模式级候选布局：按当前模式意图叠加全局基线重算方向，与上次下发不同才下发。
        // 必须在下方 UpdateCandidates **之前**——同 channel 按序处理，UI 先改方向再填候选。
        // 这是「强制竖排/横排」的唯一执行点，模式进入/退出各处都不再自己动布局（见 layout.rs）。
        self.sync_candidate_layout(state);
        // 延迟首次显示：新组合首帧若非经授权（reflow 后权威坐标 / 兜底 timer）则不立即显示，
        // 改 arm 兜底 timer，待 handle_caret_update 的权威坐标或超时再首显。避免在 reflow 前的
        // 陈旧坐标处先显示、reflow 后再跳（根治"上屏后立即输入候选窗错位约一个上屏宽度"）。
        // 例外①：仅显示模式标记（无候选/无编码）时跳过延迟——进入模式时缓冲为空、无刚上屏文字，
        // 光标无 reflow 跳动风险，强制延迟只会让状态提示迟钝。
        // 注：host-render 受限宿主**不**跳过首帧延迟——曾以「服务端直绘 SHM 无需等 reflow」
        // 为由直显，结果首帧用的是陈旧 caret（SearchHost 的 caret 事件在首键后才到），
        // 显示后再跳位（真机踩坑）。本机制自带兜底 timer，受限宿主 caret 事件缺席时
        // 也会超时首显，不存在「永不显示」风险。
        let only_mode_label =
            !mode_label.is_empty() && state.candidates.is_empty() && state.input_buffer.is_empty();
        let authorized = self
            .show_authorized
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        // 例外②③：两个「不必等」的逃生口。对齐 Go handle_key_action.go:207-209——本仓移植时
        // 只搬了「等」的一侧，漏了 Go 用来跳过等待的这两项，故此前比 Go 原版更保守：无论坐标
        // 是否已就绪、宿主是否光标稳定，新组合首帧一律压到 reflow 权威坐标才显示。实测代价是
        // 按键→候选窗恒定 85~95ms（其中 C++ OnLayoutChange 的 50ms debounce 占大头），连打时
        // 候选窗只来得及显示 2~29ms，表现为「迟钝」。
        //   ② skip_caret_pending：compat.toml 把该宿主标记为「光标稳定、无 reflow 漂移」，
        //      直接首显。连打场景**只有这一项能生效**——③ 依赖的组合起点会被
        //      reset_first_show() 在每次上屏时复位（Go 的 clearState 同样如此）。
        //   ③ 坐标已就绪：已有过有效 caret 且本轮组合起点已锁定 ⇒ 没有漂移可等。
        //      对应 Go 的 `!caretValid || !compositionStartValid` 取反。
        let skip_caret_pending = self
            .active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_show_mode
            == wind_config::app_compat::FirstShowMode::Instant;
        let coords_ready = self
            .last_valid_caret
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .2
            > 0
            && self
                .composition_start
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .2;
        let shown = *self
            .candidate_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let is_first_frame = !authorized && !shown && !only_mode_label;
        if is_first_frame && !skip_caret_pending && !coords_ready {
            // 唯一的「等」出口。与下面的放行日志成对，两条合起来即可从服务端日志判定
            // 每一帧走了哪条路、以及是哪个逃生口生效——不必再对着 TSF 日志比时间戳。
            debug!(
                "first_show 闸门 → 等待权威坐标（arm {}ms 兜底）: skip_caret_pending=0 coords_ready=0",
                self.planned_first_show_timeout_ms()
            );
            self.arm_pending_first_show();
            return;
        }
        if is_first_frame {
            // instant 档用的是上一轮遗留的坐标，必然是「非权威」；coords_ready 那条是已锁定
            // 的本轮组合起点，属权威，不置位。
            if skip_caret_pending {
                self.first_show_was_provisional
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            debug!(
                "first_show 闸门 → 立即显示（逃生口）: instant={} coords_ready={}",
                skip_caret_pending as u8, coords_ready as u8
            );
        }
        let t_nu = std::time::Instant::now();
        // 仅推送当前页候选（窗口按 1..N 编号，翻页后重新编号）
        let (start, end) = self.page_range(state);
        // 候选序号标签有**三种**归属，旧实现是个 bool 只装得下前两种：
        //  - 数字透镜：数字键正在录表达式 → 选词改用字母标签 a/b/c
        //  - 自由输入：字母与数字**都是**字面输入，没有任何键能按序号选 → 干脆不画序号
        //    （画了就是骗人——用户会去按那个数字，结果把数字打进缓冲）
        //  - 其余：正常序号
        let mix_lens = matches!(state.active, Some(ModeKind::Mix(_))).then(|| self.mix_lens(state));
        let alpha = mix_lens == Some(MixLens::Numeric);
        let hide_index = mix_lens == Some(MixLens::Free);
        // 悬停提示/候选微调配置（热重载快照）
        let rt = self.rt();
        let cand_cfg = &rt.config.ui.candidate;
        let tip_cfg = &rt.config.ui.tooltip;
        // 命令直通车候选前缀标注（features.cmdbar.candidate_prefix）：仅命令候选(is_command)显示。
        let cmd_prefix = rt.config.input.cmdbar.candidate_prefix.as_str();
        // 检索范围放宽（自动补充）候选的前缀标注，见 docs/design/smart-filter-scope-relax.md
        let scope_prefix = rt.config.input.scope_relax.prefix.as_str();
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
        // 调试提示上下文：仅开启调试段时解析一次（mixed 归属 / 方案 id），循环内按候选来源选用。
        let dbg_ctx = if tip_cfg.debug_enabled {
            // 归属与读写两端同源（`effective_data_schema`）：特殊模式下若这里仍按 active 解析，
            // 调试段显示的计数与排序实际用的不是同一个 key——排查时会被它带偏，
            // 而这正是最难察觉的一种不一致。
            Some(self.build_debug_schema_ctx(self.effective_data_schema(state).as_deref()))
        } else {
            None
        };
        // 反查表读锁在候选循环外取一次（写方仅 sync_chaizi_assets 的热重载路径）。
        let reverse = self.reverse.read().unwrap_or_else(|e| e.into_inner());
        // 注释段（候选右侧灰字）模板，见 `crate::comment`。横竖各持一份、互不影响：
        // 两种排布的可用横向空间差一个数量级，能放什么本就不是同一个答案。
        // 模式级覆盖优先于全局（临英可只显示 ${dict}、临拼可整个关掉），见 `comment::template_for`。
        let comment_tpl =
            self.comment_template_for(&rt.config, state, self.desired_vertical(state));
        // [编码] 段来源方案（循环外解析一次）：码表方案=自身全部编码（码长升序 a/ab/abc）、
        // 混输=其主码表成员、拼音=全局主码表。编码按词查方案词库反查索引（word_codes_in），
        // 不按取码规则生成。候选并非用该编码方案直接输入时（来源方案≠活跃方案，或处于
        // 临时拼音/快捷输入反查模式）标题带来源方案名：[编码(五笔)]。
        let code_schema = tip_cfg
            .code_enabled
            .then(|| self.engine_mgr.code_source_schema())
            .filter(|s| !s.is_empty());
        let code_source_name = code_schema.as_deref().and_then(|sid| {
            let indirect = force_hint || sid != self.engine_mgr.active_schema_id();
            indirect.then(|| {
                let name = self.engine_mgr.schema_name(sid);
                if name.is_empty() {
                    sid.to_string()
                } else {
                    name
                }
            })
        });
        let items: Vec<CandidateItem> = state.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let full = self.cand_s2t_text(state, c);
                // 显示截断（超长加 …）：短语与普通候选统一按用户可配的 ui.candidate.max_chars。
                // 短语 text 在生成层已存完整原文（仅一行化），此处仅裁显示——上屏仍用完整原文。
                let disp = cand_cfg.truncate_display(&full);
                // 反查提示按截断后文本生成：超长候选（如长短语）逐字反查会撑爆气泡且显示不全，
                // 只提示实际显示出的字（… 为非 CJK，tooltip_for 自动滤除，不影响反查内容）。
                // [编码] 段按候选**完整原文**查词库（截断/繁化文本词库里没有；查不到=None 不显示）。
                let word_code = code_schema
                    .as_deref()
                    .map(|sid| self.engine_mgr.word_codes_in(sid, &c.text))
                    .filter(|s| !s.is_empty());
                let mut tooltip = reverse.tooltip_for(
                    &disp,
                    &tip_opts,
                    word_code.as_deref(),
                    code_source_name.as_deref(),
                );
                // 注释段（候选右侧灰字）：渲染当前排布对应的模板。
                // 与悬停提示无耦合——注释放不下的内容不往气泡里塞，气泡有自己的
                // `ui.tooltip.*` 三段（编码/拼音/拆字），塞了会与之重复。
                let comment = self.comment_for(
                    c,
                    comment_tpl,
                    cand_cfg.comment_max_chars,
                    &reverse,
                    pinyin_hint,
                );
                // 调试段：独立一行 [调试] + 来源/方案/编码/权重/序/词频。全关时不再兜底回填编码
                // （tooltip 各 provider 全关即真正为空，不显示气泡）。
                if let Some(ctx) = &dbg_ctx {
                    let dbg = self.debug_tooltip_section(c, &state.input_buffer, ctx);
                    if !tooltip.is_empty() {
                        tooltip.push('\n');
                    }
                    tooltip.push_str(&dbg);
                }
                CandidateItem {
                    // 命令候选加前缀标注（截断后再加,保证前缀不被截掉）。
                    // 检索范围放宽补进来的候选同理加标注（`input.scope_relax.prefix`），让用户
                    // 一眼看出「这几条是超出当前检索范围补来的」，而非词库里本该有的常用字。
                    text: if c.is_command && !cmd_prefix.is_empty() {
                        format!("{cmd_prefix}{disp}")
                    } else if c.is_scope_filtered && !scope_prefix.is_empty() {
                        format!("{scope_prefix}{disp}")
                    } else {
                        disp
                    },
                    code: c.code.clone(),
                    label: if alpha {
                        ((b'a' + i as u8) as char).to_string()
                    } else {
                        self.resolve_index_label(cand_cfg, i)
                    },
                    tooltip,
                    comment,
                    no_index: hide_index,
                }
            })
            .collect();
        // 翻页信息改为结构化字段传给候选窗（窗口内渲染独立的页码指示）
        let total_pages = self.total_pages(state);
        let selected = state.selected_index.min(items.len().saturating_sub(1));
        // 悬停目标独立于选中项：候选越界视为无悬停，翻页器 tag 原样透传
        let hover = match self.hover_target() {
            h if (0..wind_ui::manager::HOVER_PAGE_PREV).contains(&h) => {
                if (h as usize) < items.len() { h } else { -1 }
            }
            h => h, // 翻页器 tag / -1
        };
        // preedit 是否嵌入宿主（app_inline）：嵌入时编码插入宿主、光标随输入右移，候选窗须锚在
        // 组合起点（缓冲头部）而非跟随光标末尾；非嵌入时 preedit 在候选窗、宿主光标不动，用当前光标。
        // 该标志同时门控下方 preedit 是否下发候选窗渲染（嵌入时候选窗不重复显示 preedit）。
        let in_app = self
            .preedit_display
            .lock()
            .map(|m| m.in_app())
            .unwrap_or(true);
        // 坐标基准：嵌入模式且组合起点已锁定 → 用组合起点（钉在缓冲头部，不随输入移动）；否则当前光标。
        // 组合起点由 handle_caret_update 在本组合首个有效坐标处锁定。候选窗首显已由"延迟首显"门控
        // 保证发生在 reflow 后的权威坐标处。无效坐标回退最近有效坐标，避免跑到屏幕左上角。
        let cs = *self
            .composition_start
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (cx, cy, ch) = if in_app && cs.2 {
            (cs.0, cs.1, state.caret_height)
        } else {
            (state.caret_x, state.caret_y, state.caret_height)
        };
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
        let n_items = items.len();
        let preedit = if in_app {
            String::new()
        } else {
            state.preedit.clone()
        };
        // 编码区插入符位置（preedit 内字节偏移）。in_app 时组合区由宿主画，无需自绘插入符。
        let preedit_caret = if in_app {
            0
        } else {
            self.ui_caret_bytes(state).min(preedit.len())
        };
        let (cand_fixed, cand_fixed_x, cand_fixed_y) = self.candidate_fixed_pos();
        // mode_label 已在顶部计算（纳入空则隐藏守卫）：作为候选窗内联标记随候选窗一并显示。
        let _ = self.ui_tx.send(UiCommand::UpdateCandidates {
            preedit,
            preedit_caret,
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
            fixed: cand_fixed,
            fixed_x: cand_fixed_x,
            fixed_y: cand_fixed_y,
        });
        // 候选窗已下发显示：标记本组合已首显，后续刷新（翻页/选字/打字）即可立即下发不再延迟。
        *self
            .candidate_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        // macOS：把当前页候选右键菜单的禁用位随候选更新一并推给 `.app`，供其右键即时灰显。
        // Windows 的右键菜单在进程内 `show_candidate_menu` 实时算 enabled，不走此推送。
        #[cfg(target_os = "macos")]
        self.push_candidate_menu_flags(state, start, end);
        tracing::debug!(
            "notify_ui_update: build+send {:?} (n={})",
            t_nu.elapsed(),
            n_items
        );
    }

    /// macOS：计算当前页每候选的右键菜单禁用位并经 push 通道下发（CmdCandidateMenuFlags 0x0505）。
    /// 位定义与 Swift CandidatePanel 对齐：0x01 上移 / 0x02 下移 / 0x04 置顶 / 0x08 删除 / 0x10 恢复默认。
    /// 语义对齐进程内 `show_candidate_menu`（共用 candidate_delete_menu / candidate_op_scope 判定）：
    /// 首项禁上移/置顶、末项禁下移；拼音普通候选禁全部调位；删除按候选来源判定；
    /// 无 shadow 规则禁恢复默认；无词库落点整页全禁。
    /// 注：macOS 端「删除」文案固定，来源动态文案（禁用短语/删除用户词…）待协议扩展后接入。
    #[cfg(target_os = "macos")]
    pub(crate) fn push_candidate_menu_flags(&self, state: &State, start: usize, end: usize) {
        if !self.push_server.has_clients() || start >= end {
            return;
        }
        let total = state.candidates.len();
        // 无词库落点（无独立归属的 overlay / 空码浏览态）：整页全禁，只留复制——与 Windows 侧
        // `show_candidate_menu` 的「仅复制」分支同一判据（见 `candidate_op_scope`）。
        let Some(scope) = self.candidate_op_scope(state) else {
            let flags = vec![0x1Fu8; end.min(total).saturating_sub(start)];
            self.push_server
                .push_to_active(&wind_ipc::codec::encode_candidate_menu_flags(&flags));
            return;
        };
        let schema = scope.schema;
        let code = scope.code;
        let is_pinyin = matches!(scope.engine_type, Some(wind_engine::EngineType::Pinyin));
        let mut flags = Vec::with_capacity(end - start);
        for idx in start..end.min(total) {
            let cand = &state.candidates[idx];
            let word = &cand.text;
            let mut f = 0u8;
            if idx == 0 {
                f |= 0x01 | 0x04; // 首项：禁上移 + 禁置顶（已在首位，置顶是冗余规则）
            }
            if idx + 1 >= total {
                f |= 0x02; // 末项：禁下移
            }
            // 拼音普通候选：禁全部调位（无稳定位置语义）；命令候选例外。
            if is_pinyin && !cand.is_command {
                f |= 0x01 | 0x02 | 0x04;
            }
            let (_, deletable) = crate::handle_menu::candidate_delete_menu(cand);
            if !deletable {
                f |= 0x08;
            }
            let cand_id = (!cand.id.is_empty()).then(|| cand.id.as_str());
            if !self.shadow_has_rule(&schema, &code, word, cand_id) {
                f |= 0x10; // 无 shadow 规则：禁恢复默认
            }
            flags.push(f);
        }
        self.push_server
            .push_to_active(&wind_ipc::codec::encode_candidate_menu_flags(&flags));
    }

    pub(crate) fn notify_ui_hide(&self) {
        // 候选窗隐藏即会话终结：无条件收回 CapsLock 拦截。
        //
        // ★ 这里刻意**不查 state** 而是直接归零。闸门的两个方向后果不对称：少吃只是
        // 「CapsLock 绑定这一次没生效」，多吃却是「用户在别的应用里 CapsLock 按不动」。
        // 凡拿不准就归零。
        wind_keys::capslock_hook::set_should_eat(false);
        // 悬停归零同理：窗口没了，悬停目标不可能还有意义。UI 侧 `CandidateMouse::reset_hover`
        // 清的只是防抖闸门（决定何时**发**事件），高亮与 tooltip 读的是本值——不清这一句，
        // 特殊模式下窗口再次弹出时会带着上次的悬停高亮，鼠标却从未移动过。
        self.clear_hover();
        let _ = self.ui_tx.send(UiCommand::HideCandidates);
        self.reset_first_show();
    }

    /// 复位首显延迟状态（候选窗隐藏 / 组合结束）：下次新组合重新延迟首显，并作废未触发的兜底 timer。
    pub(crate) fn reset_first_show(&self) {
        self.first_show_was_provisional
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.first_show_extended
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *self
            .candidate_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
        *self
            .pending_first_show
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
        let mut t = self
            .pending_first_show_token
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *t = t.wrapping_add(1);
        drop(t);
        // 组合结束：复位组合起点锚定，下一组合重新锁定首个有效 compStart。
        *self
            .composition_start
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (0, 0, false);
    }

    /// 推迟首次显示候选窗：标记 pending 并启动兜底 timer。token 比对使后续按键的 arm 自动作废
    /// 旧 timer。handle_caret_pending 握手会把 wait 档延到 600ms（应对 OnLayoutChange burst 慢的应用）。
    fn arm_pending_first_show(&self) {
        // ★ 首帧信任门：`fast` 的短兜底建立在「手里的坐标 ≈ 当前插入点」之上，而焦点刚到达 /
        // 用户刚移动过光标时这个前提不成立（见 `caret_cache_verified`）。此时拿旧坐标首显
        // 必然是一次可见的错位加一次跳，「快」反而有害，让位给长兜底等权威坐标。
        //
        // ⚠⚠ **长等待一旦开始就不因后续按键重置**，这是本门能否成立的关键：闸门在候选窗
        // 显示前对**每一个字母**都会调到这里（`is_first_frame` 一直为真），而
        // `arm_pending_first_show_with_timeout` 每次都 bump token 重新计时。若照常重置，
        // 用户多打几个字母就把这段等待反复推后，长兜底静默退化回短兜底、错位照旧——正是
        // 「兜底超时长于组合寿命 ⇒ 永不到期」那个死结的镜像。Excel 建单元格编辑上下文要
        // 558ms，其间用户往往已经敲了三五个字母。
        //
        // 反过来，长等待到期后就**不再续**（`pending` 被 fire 消费掉，`extended` 保持置位），
        // 用旧坐标首显仍优于候选窗一直不出现。
        if self.first_show_needs_long_wait() {
            let already_waiting = *self
                .pending_first_show
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                && self
                    .first_show_extended
                    .load(std::sync::atomic::Ordering::Relaxed);
            if already_waiting {
                debug!("first_show 闸门 → 保持长兜底计时（坐标缓存仍未验证，不因本次按键重置）");
                return;
            }
            debug!(
                "first_show 闸门 → 坐标缓存未经当前插入点验证，改 arm {FIRST_SHOW_LONG_FALLBACK_MS}ms 长兜底"
            );
            self.first_show_extended
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.arm_pending_first_show_with_timeout(FIRST_SHOW_LONG_FALLBACK_MS);
            return;
        }
        self.arm_pending_first_show_with_timeout(self.first_show_fallback_ms());
    }

    fn first_show_mode_is_fast(&self) -> bool {
        self.active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_show_mode
            == wind_config::app_compat::FirstShowMode::Fast
    }

    /// 首帧信任门是否命中：`fast` 档且坐标缓存未经当前插入点验证。
    fn first_show_needs_long_wait(&self) -> bool {
        self.first_show_mode_is_fast()
            && !self
                .caret_cache_verified
                .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 本次 arm **实际**会用的超时值。
    ///
    /// 存在的唯一理由是给首显闸门的日志用：闸门原本直接打印 `first_show_fallback_ms()`，
    /// 而信任门命中时真正 arm 的是长兜底——日志说 25ms、实际等 600ms，排查时会被带偏。
    /// **判据分散在两处（一处算日志、一处定行为）就必然分叉**，故收敛到同一个函数。
    fn planned_first_show_timeout_ms(&self) -> u64 {
        if self.first_show_needs_long_wait() {
            FIRST_SHOW_LONG_FALLBACK_MS
        } else {
            self.first_show_fallback_ms()
        }
    }

    /// 本档位等不到坐标时的兜底超时。fast 档取远小于 wait 的值，理由见
    /// `fast_first_show_fallback_ms` 的字段注释（150ms 会让 fast 在 Word/记事本上退化成 wait）。
    fn first_show_fallback_ms(&self) -> u64 {
        if self.first_show_mode_is_fast() {
            self.rt().config.ui.candidate.fast_first_show_fallback_ms
        } else {
            150
        }
    }

    fn arm_pending_first_show_with_timeout(&self, ms: u64) {
        *self
            .pending_first_show
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = true;
        let token = {
            let mut t = self
                .pending_first_show_token
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *t = t.wrapping_add(1);
            *t
        };
        let Some(weak) = self.self_weak.get().cloned() else {
            return;
        };
        first_show_timer().arm(
            std::time::Instant::now() + std::time::Duration::from_millis(ms),
            token,
            weak,
        );
    }

    /// 兜底 timer 到期回调。由共享定时器线程调用。
    fn fire_pending_first_show(&self, token: u64) {
        // token/pending 校验：被新按键的 arm 取代、或已被首显/隐藏消费 → 放弃本次兜底。
        {
            let pending = *self
                .pending_first_show
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let tok = *self
                .pending_first_show_token
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !pending || tok != token {
                return;
            }
        }
        *self
            .pending_first_show
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = false;
        // 兜底超时：reflow 坐标迟迟未到，用当前 state 强制首显（坐标可能为按键前旧值，
        // 属慢应用降级，仍优于候选窗一直不显示）。
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let has_content = !state.candidates.is_empty()
            || !state.input_buffer.is_empty()
            || self.mode_indicator_text(&state).is_some();
        if has_content {
            // 用的既然是旧坐标，就必须按「非权威」记账，否则随后到达的权威坐标会被 3px 常规容差
            // 判成需要校正而跳一下——兜底路径本来就是抖动最容易被看见的地方。
            // 置位在 has_content 内：没真显示就不该留下"用过非权威坐标"的账。
            self.first_show_was_provisional
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.show_authorized
                .store(true, std::sync::atomic::Ordering::Relaxed);
            debug!("first_show 兜底 timer 到期 → 用现有坐标首显（非权威，享放宽容差）");
            self.notify_ui_update(&state);
        }
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
            UiEvent::RequestMainMenu(anchor) => self.show_main_menu(anchor),
            UiEvent::MenuAction(kind) => self.menu_action(kind),
            UiEvent::MenuClose => {
                // ESC / 点击别处关闭：无动作派发，可直接解除 tooltip 隐藏抑制。
                self.menu_close();
                self.clear_tooltip_menu_flag();
            }
            UiEvent::GlobalHotkey(action) => self.handle_global_hotkey(&action),
            UiEvent::StatusTipMoved { x, y } => self.save_status_tip_pos(x, y),
            UiEvent::CandidateWindowMoved { x, y } => self.save_candidate_pos(x, y),
            UiEvent::RequestStatusMenu { x, y } => self.show_status_menu(x, y),
            UiEvent::RequestTooltipMenu { x, y } => self.show_tooltip_menu(x, y),
            UiEvent::RequestInputDiagMenu { x, y } => self.show_input_diag_menu(x, y),
            UiEvent::SystemThemeChanged => self.on_system_theme_changed(),
            UiEvent::CandidateFlipped(v) => self
                .candidate_flipped
                .store(v, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// 切换检索范围（0 智能/1 常用字/2 全部字符），以新范围重过滤并刷新候选。
    /// 持久化到 `config.input.filter_mode`（单一源：与设置页统一，reload 不会覆盖菜单选择）。
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
        if let Err(e) = Config::set_user_string(&["input", "filter_mode"], mode.as_config()) {
            warn!("set_filter_mode: 持久化 input.filter_mode 失败: {}", e);
        }
        self.refresh_config_in_memory(|c| c.input.filter_mode = mode.as_config().to_string());
        // 组合中：以新范围重建候选并刷新
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !s.input_buffer.is_empty() {
            self.update_candidates(&mut s);
            self.notify_ui_update(&s);
        }
        drop(s);
        self.show_tip(label);
    }

    /// 持久化简繁开关到 `config.input.s2t.enabled`（单一源：与设置页统一，reload 不会覆盖
    /// 菜单/热键选择）。菜单与热键两条切换路径共用，避免只改一处留下不对称。
    pub(crate) fn persist_s2t_enabled(&self, on: bool) {
        if let Err(e) = Config::set_user_bool(&["input", "s2t", "enabled"], on) {
            warn!("toggle_s2t: 持久化 input.s2t.enabled 失败: {}", e);
        }
        self.refresh_config_in_memory(|c| c.input.s2t.enabled = on);
    }

    /// 影子规则：当前 code 是否对该候选有规则（置顶/删除），决定菜单"恢复默认"可用性。
    ///
    /// `cand_id` 取候选的稳定 id（短语候选非空）：动态短语的规则 `word` 记的是写入当天的
    /// 求值文本，只按 word 查会在次日恒判「无规则」——菜单「恢复默认」永久灰显，用户既改
    /// 不动也清不掉。判据与 `apply_shadow` / `candidate_op` 的写入端保持同一把键。
    pub(crate) fn shadow_has_rule(
        &self,
        schema: &str,
        code: &str,
        word: &str,
        cand_id: Option<&str>,
    ) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        // 折叠到 data_schema_id（与 apply_shadow/candidate_op 一致），拼音族共享。
        let schema = self.engine_mgr.data_schema_id(schema);
        matches!(
            store.get_shadow_rules(&schema, code),
            Ok(Some(rec)) if rec.has_target(word, cand_id)
        )
    }

    /// 当前焦点应用是否启用符号自动配对。per-app 规则（`compat.toml` 的 `auto_pair`）
    /// 优先，未配则跟随全局——全局开关仍在各自的 `input.auto_pair.chinese/english` 里，
    /// 本函数只回答「这个宿主要不要一刀切关掉」。
    ///
    /// ⚠ 三个消费点必须都问它：`active_pairs()`、`english_pairs_via_pipeline()`、
    /// `push_english_pair_config()`。前两条走协调器，第三条是 C++ 侧英文配对引擎——
    /// 纯英文模式的标点键根本到不了协调器，漏接它等于「切到英文就又配对了」。
    pub(crate) fn auto_pair_allowed_here(&self) -> bool {
        self.active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .auto_pair
            .unwrap_or(true)
    }

    /// 当前模式下生效的配对表（按中/英标点 + 各自开关）
    pub(crate) fn active_pairs(&self, chinese_punct: bool) -> Option<Vec<(char, char)>> {
        // per-app 关闭：返回 None 等价于「配对表为空」，插对与右符号跳出一并失效。
        // 在取表这一层收口，而不是在每个使用点各加一个 if——后者是本仓栽过四次的形态。
        if !self.auto_pair_allowed_here() {
            return None;
        }
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
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            // 候选已清空 → 悬停不再对应屏幕上任何东西，归零后返回（无候选时窗口本就不显示，
            // 不必重绘）。★ 必须**归零而非早退**：早退会让「鼠标移出候选窗」发出的那条
            // `Hover(-1)` 在候选恰好清空时被整个吞掉，旧值一路残留到下一次候选窗显示。
            self.clear_hover();
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
        if self
            .hover_index
            .swap(new_hover, std::sync::atomic::Ordering::Relaxed)
            != new_hover
        {
            self.notify_ui_update(&state);
        }
    }

    pub(crate) fn build_status(&self) -> StatusUpdateData {
        let (chinese_mode, full_width, chinese_punct, toolbar_visible, caps_lock) = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.chinese_mode,
                s.full_width,
                s.chinese_punct,
                s.toolbar_visible,
                s.caps_lock,
            )
        };
        // 有效中文：中文模式且大写锁定未开（对齐 Go effectiveChinese = chineseMode && !capsLockOn）。
        let effective_chinese = chinese_mode && !caps_lock;
        let icon_label = if effective_chinese {
            let id = self.engine_mgr.active_schema_id();
            let lbl = self.engine_mgr.schema_icon_label(&id);
            if lbl.is_empty() {
                "中".to_string()
            } else {
                lbl
            }
        } else if caps_lock {
            "A".to_string()
        } else {
            "英".to_string()
        };
        StatusUpdateData {
            chinese_mode,
            full_width,
            chinese_punct,
            toolbar_visible,
            caps_lock,
            icon_label,
            key_down_hotkeys: self.rt().compiled_hotkeys.key_down_tsf_hashes(),
            key_up_hotkeys: self.rt().compiled_hotkeys.key_up_tsf_hashes(),
        }
    }

    /// `client_token` = 触发本次 activation 的客户端 token（高 32 位 = PID，
    /// BinaryProtocol.h PushTokenHandshake 约定）。hostRenderAvail 位**必须**按
    /// 事件源 PID 查白名单（对齐 Go PushActivationStatusToActiveClient(status, processID)）——
    /// 不能用全局焦点槽：开始菜单弹出会连带激活 StartMenuExperienceHost 等兄弟进程，
    /// 其激活事件若污染全局槽，推给 SearchHost 的 avail 位会错置 0，触发 DLL
    /// 「flag missing after reconnect」销毁重建循环（真机踩坑）。
    fn push_activation_status(&self, client_token: u64) {
        let s = self.build_status();
        debug!(
            "push_activation_status: chinese={} key_down={:?} key_up={:?}",
            s.chinese_mode, s.key_down_hotkeys, s.key_up_hotkeys
        );
        #[cfg(windows)]
        let host_render_avail = {
            let pid = (client_token >> 32) as u32;
            pid != 0
                && self
                    .host_render()
                    .map(|m| m.is_process_whitelisted(pid))
                    .unwrap_or(false)
        };
        #[cfg(not(windows))]
        let host_render_avail = {
            let _ = client_token;
            false
        };
        let encoded = wind_ipc::codec::encode_activation_status_push(
            s.chinese_mode,
            s.full_width,
            s.chinese_punct,
            s.toolbar_visible,
            s.caps_lock,
            host_render_avail,
            &s.key_down_hotkeys,
            &s.key_up_hotkeys,
            &s.icon_label,
        );
        // 定向投递给事件源客户端（精确 token 匹配）。push_to_active 实为广播——广播会把
        // 按别的进程计算的 hostRenderAvail 位污染给无关客户端（真机踩坑：开始菜单弹出时
        // StartMenuExperienceHost 等兄弟实例的激活推送被 SearchHost 收到，avail=0 触发
        // Band 窗口销毁重建循环）。事件源无 push 连接时丢弃，绝不兜底转发。
        if client_token != 0 {
            if !self.push_server.push_to_token(client_token, &encoded) {
                debug!("activation push: 事件源 token 无 push 连接，丢弃（防污染不广播）");
            }
        } else {
            // 无 token 的旧路径（不应出现于当前 DLL）：保持原广播行为
            self.push_server.push_to_active(&encoded);
        }
    }

    /// push 客户端完成 token 握手后的补推握手（仅 Windows；由 main.rs 注册到 PushServer）。
    /// 场景：服务重启后，白名单受限宿主（SearchHost 等 locked/transient DocMgr）重连时
    /// 既不发 focus_gained（被 DLL OnSetFocus 跳过）也不重发 IME_ACTIVATED——没有任何
    /// activation push 会到达，DLL 的 host 窗口挂着死 SHM 永不重新 setup（真机踩坑：
    /// 服务重启后概率性停留普通渲染）。此处对白名单 pid 定向补推一帧 activation status
    /// （avail=1），触发 C++ ApplyActivationStatusResponse → _EnsureHostRenderSetup
    /// （forceRefresh）→ 重新握手 setup。非白名单进程不推，零影响。
    #[cfg(windows)]
    pub fn on_push_client_connected(&self, client_token: u64) {
        let pid = (client_token >> 32) as u32;
        if pid == 0 {
            return;
        }

        // 推送英文自动配对配置到新连接的客户端（不受 host-render 白名单限制，
        // 所有 TSF 实例都需要收到此配置才能在英文模式下正确处理标点配对）。
        self.push_english_pair_config(client_token);
        self.push_jump_out_keys_config(client_token); // 配对跳出键（英文模式跳出 + 中文转发放行）
        self.push_password_suppress_config(client_token); // 密码框抑制策略（DLL 本地吃键门控）
        self.push_custom_en_punct_config(client_token); // 英半列自定义标点：DLL 据此吃键转发
        self.push_pair_state_ttl_config(client_token); // 配对状态时效（DLL 侧闸门据此判陈旧）
        // 诊断采集开关：DLL 每次重连都从默认值（关）起步，握手不推则 HUD 开着也收不到
        // 新连接宿主的快照——而最需要它的 SearchHost 恰恰是最常重连的那类。
        self.push_diag_snapshot_config(client_token);

        let Some(mgr) = self.host_render() else {
            return;
        };
        if !mgr.is_process_whitelisted(pid) {
            return;
        }
        tracing::info!("push 客户端注册补推 activation（host-render 白名单宿主）pid={pid}");
        self.push_activation_status(client_token);
    }

    /// 指定 PID 的进程是否启用符号自动配对（per-app 规则，未配则跟随全局）。
    ///
    /// ⚠ **按 PID 直查规则表，绝不走 `active_compat` 焦点槽**：本函数的调用方是推送路径，
    /// 目标客户端未必是当前焦点进程（新客户端握手、配置变更广播都会推给后台进程）。
    /// 拿焦点槽的值会把焦点应用的规则套到别人头上——同 `host_render` 的既有纪律。
    fn auto_pair_allowed_for_pid(&self, pid: u32) -> bool {
        if pid == 0 {
            return true;
        }
        let name = {
            let cached = self
                .pid_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&pid)
                .cloned();
            cached.unwrap_or_else(|| process_name(pid))
        };
        if name.is_empty() {
            return true;
        }
        self.app_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_rule(&name)
            .and_then(|r| r.auto_pair)
            .unwrap_or(true)
    }

    /// 推送英文自动配对配置到指定客户端（或逐个推给所有活跃客户端）。
    ///
    /// 这是 per-app 自动配对开关的**第三条**消费通路：纯英文模式的配对完全由 C++ 侧
    /// `_englishPairEngine` 处理，那些标点键根本到不了协调器，只关另两条的话「切到英文
    /// 模式又配上了」。故 enabled 必须按**目标进程**现算，不能全局广播同一个值。
    pub fn push_english_pair_config(&self, client_token: u64) {
        let rt = self.rt();
        let make = |token: u64| {
            let pid = (token >> 32) as u32;
            let enabled = rt.config.input.auto_pair.english && self.auto_pair_allowed_for_pid(pid);
            let value = wind_ipc::codec::encode_english_pairs_value(enabled, &rt.en_pairs);
            wind_ipc::codec::encode_sync_config(
                wind_ipc::protocol::CONFIG_KEY_ENGLISH_PAIRS,
                &value,
            )
        };
        if client_token != 0 {
            self.push_server
                .push_to_token(client_token, &make(client_token));
        } else {
            self.push_server.push_per_client(make);
        }
    }

    /// 下发配对状态时效给 DLL。吃键闸门（`_pairPendingDepth`）在 DLL 侧，它必须能本地判定
    /// 状态是否陈旧——只有协调器过期而 DLL 照吃跳出键的话，协调器回 PassThrough 已太晚
    /// （「吃了再吐」丢键）。故 TTL 以 DLL 侧判据为准，此处只推阈值。
    pub fn push_pair_state_ttl_config(&self, client_token: u64) {
        let secs = self.rt().config.input.auto_pair.state_ttl_secs;
        let value = wind_ipc::codec::encode_pair_state_ttl_value(secs);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_PAIR_STATE_TTL,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 下发密码框抑制策略开关给 DLL。DLL 据此 + 自身持有的 InputScope 掩码在
    /// `OnTestKeyDown` 本地判定是否放行；判据两侧必须一致（见 `apply_input_diag` 与
    /// C++ `IsPasswordSuppressActive`），漂移即「吃了再吐」丢键。
    /// 开关是会话级运行时态（右键菜单「高级」可切），故握手时与每次切换后都要推。
    pub fn push_password_suppress_config(&self, client_token: u64) {
        let enabled = self
            .password_suppress_enabled
            .load(std::sync::atomic::Ordering::Relaxed);
        let value = wind_ipc::codec::encode_password_suppress_value(enabled);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_PASSWORD_SUPPRESS,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 下发「英文模式下 DLL 需吃键转发」的源字符集合给 DLL。两个来源合成一份推送：
    ///   - 配了**英半列自定义**的键（`wind_punct::custom_english_punct_chars`）；
    ///   - 开了 `symbol.english_mode` 时的**英文智能符号参与集**（`english_smart_source_chars`）。
    ///
    /// 英文模式（非全角）下 DLL 默认直接透传标点键、引擎收不到，上面两件事因此都无从发生；
    /// DLL 据此集合精确吃下这些键并转发（集合为空 = 完全保持历史行为）。**吃键集必须 ⊆ 出字集**：
    /// 出字方 `handle_english_custom_punct` 与本推送共用 `rt().custom_en_punct_chars` 作判据，
    /// 同源即不会漂移；两侧一旦不一致就是「吃了再吐」丢键（Chrome/Electron 不回退合成 WM_CHAR）。
    /// 集合内没配英半自定义的键会出原样 ASCII（与透传等价），故并入是安全的。
    pub fn push_custom_en_punct_config(&self, client_token: u64) {
        // BTreeSet 迭代天然有序 → 推送字节可复现（与 jump_out_keys 排序同理）。
        let chars: Vec<char> = self.rt().custom_en_punct_chars.iter().copied().collect();
        let value = wind_ipc::codec::encode_custom_en_punct_value(&chars);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_CUSTOM_EN_PUNCT,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// 推送配对跳出键（VK 码集合）到 TSF 客户端。TSF 英文模式配对直接据此跳出；
    /// 中文模式据此在「有待跳出配对」时放行转发（真正裁决仍在协调器）。
    pub fn push_jump_out_keys_config(&self, client_token: u64) {
        let rt = self.rt();
        // HashSet 迭代序不稳定，排序保证推送字节可复现。
        let mut vks: Vec<u32> = rt.jump_out_keys.iter().copied().collect();
        vks.sort_unstable();
        let value = wind_ipc::codec::encode_jump_out_keys_value(rt.jump_out_on_right_symbol, &vks);
        let msg = wind_ipc::codec::encode_sync_config(
            wind_ipc::protocol::CONFIG_KEY_JUMP_OUT_KEYS,
            &value,
        );
        if client_token != 0 {
            self.push_server.push_to_token(client_token, &msg);
        } else {
            self.push_server.push_to_active(&msg);
        }
    }

    /// macOS：把命令直通车按键合成帧（CmdKeyTap/Seq/Hold/Release/Type）推给活跃 `.app`。
    /// 服务进程（LaunchAgent）无辅助功能授权无法 post CGEvent，改由 `.app` 侧 KeySynthesizer
    /// 合成（`.app` 有授权）。只投活跃前台客户端，与 commit 同队列保证与 type() 上屏文本的顺序。
    #[cfg(target_os = "macos")]
    pub(crate) fn push_cmdbar_key_frame(&self, encoded: &[u8]) {
        self.push_server.push_commit_to_active(encoded);
    }

    /// macOS 的 open/proc.run/设置均改为进程内执行或 CmdOpenSettings，不再经此 IPC，故仅非 macOS。
    ///
    /// `dir` = 被启动进程的工作目录（空串 = 不指定，由 TSF 侧沿用调用进程当前目录）；
    /// `verb` / `show` = ShellExecute 的动词与初始窗口状态（空串 = open / normal）。
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn push_shell_exec(
        &self,
        target: &str,
        params: &str,
        dir: &str,
        verb: &str,
        show: &str,
    ) {
        let encoded = wind_ipc::codec::encode_shell_exec(target, params, dir, verb, show);
        // 带副作用操作（启动/激活外部程序）只投给活跃（前台）客户端，与 push_commit 语义一致。
        // 若广播全部客户端，多个后台 TSF 进程会竞相 ShellExecuteW，非前台进程启动的 wind_setting
        // 第二实例无前台权限，其 SetForegroundWindow 失败，导致窗口有较大概率停在后台。
        self.push_server.push_commit_to_active(&encoded);
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

    /// 焦点事件携带的 caret 落缓存的**唯一入口**。
    ///
    /// 焦点 caret 有两条到达路径——同步段的 [`Self::handle_focus_gained_caret`] 与重型段的
    /// [`Self::handle_focus_gained`]——而**重型段必然晚于同步段执行**（见 `server.rs::handle_client`：
    /// 同步段先回 `ModePush` 解除 DLL 阻塞，重型段延后到响应写出之后才跑）。
    ///
    /// 此前重型段自己直写 `state.caret_*`，既没有 `height == 0` 守卫也不做 `caret_use_top`
    /// 变换，于是把同步段刚做好的两道处理**整个抹掉**：退化矩形进了缓存，微信一类宿主的
    /// 坐标差一个行高。两处口径分裂既不编译报错也不 panic，只表现为「焦点后第一次定位偏一行」，
    /// 是典型的看不见的分裂。故合并到此，两条路径都必须经由它。
    /// 应用 per-app 的光标坐标兼容变换：`caret_use_top` 抬升 + `caret_offset_*` 校正。
    ///
    /// ★ **两个调用点必须都走它**（`apply_focus_caret` 与 `handle_caret_update`）。
    /// `caret_use_top` 原本就是分头写在这两处的，任何新增变换只要漏一处，症状就是
    /// 「有时生效有时不生效」——取决于本次坐标是走焦点路径还是常规更新路径，极难归因。
    ///
    /// 偏移校正针对的是**宿主报告的坐标本身系统性偏移**（如 Windows Terminal，别家输入法
    /// 同样偏）。与主题里的候选窗偏移不是一回事：那个是候选窗相对光标的布局（样式层），
    /// 这个修的是光标坐标（兼容层），故候选窗/状态气泡/HUD 等所有消费者一并受益。
    ///
    /// 组合起点坐标同步平移以保持锚点一致；为 0（未提供）时不动，避免把「没有值」
    /// 变成「一个偏移后的假值」。
    fn apply_caret_compat(&self, data: &mut CaretData) {
        let (use_top, dx, dy) = {
            let ac = self.active_compat.lock().unwrap_or_else(|e| e.into_inner());
            (ac.caret_use_top, ac.caret_offset_x, ac.caret_offset_y)
        };
        if use_top && data.height > 0 {
            let raw_h = data.height;
            data.y -= raw_h;
            data.height = raw_h.max(CARET_USE_TOP_MIN_LINE_H);
            if data.composition_start_y != 0 {
                data.composition_start_y -= raw_h;
            }
        }
        if dx != 0 || dy != 0 {
            data.x += dx;
            data.y += dy;
            if data.composition_start_x != 0 {
                data.composition_start_x += dx;
            }
            if data.composition_start_y != 0 {
                data.composition_start_y += dy;
            }
        }
    }

    fn apply_focus_caret(&self, data: &CaretData, via: &str) {
        // 独立日志行：与 handle_caret_update 区分开，否则无法从日志判断焦点坐标走的是哪条路
        // （2026-08-01 那轮修复第一版就因为看不出这点，白跑了一轮真机验证）。
        tracing::debug!(
            "{via} (no-show): x={} y={} h={} src={}",
            data.x,
            data.y,
            data.height,
            wind_ipc::protocol::caret_source::name(data.source)
        );
        // height==0 = 宿主尚未 reflow，GetTextExt 返回退化矩形，坐标不可信。
        if data.height == 0 {
            return;
        }
        let mut data = *data;
        self.apply_caret_compat(&mut data);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
        state.caret_source = data.source;
    }

    /// 在当前光标下方显示状态提示气泡（中英/标点/全半角/方案切换）
    pub(crate) fn show_tip(&self, text: &str) {
        let bundle = self.rt();
        let si = &bundle.config.ui.status;
        // 禁用则完全不显示状态提示气泡。
        if !si.enabled {
            return;
        }
        // 空文本不弹窗：ui.status.items 全部取消勾选时合成文本为空，此前会渲染出一个
        // 什么都没有的小气泡（本地窗口路径无空文本判断，只有 host-render 的 render_frame 有）。
        // 与设置页「全部取消则不显示气泡」的说明保持一致。
        if text.trim().is_empty() {
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
        // 记录实际显示出去的文本，供 show_status 去重。临时提示（模式标记/主题名等）
        // 也记在这里：它们会覆盖掉旧的状态文本，从而使随后的同名状态气泡照常显示，
        // 不会被误判成"内容没变"。
        *self
            .last_status_text
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = text.to_string();
    }

    /// 隐藏状态提示气泡（常驻模式失焦时调用）。
    pub(crate) fn hide_tip(&self) {
        // 挂起中的焦点气泡一并作废：焦点都走了，那次挂起等来的权威坐标也已经属于别的上下文，
        // 补显示出来就是「切走之后气泡才姗姗弹出」。
        self.pending_focus_tip
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let _ = self.ui_tx.send(UiCommand::HideStatusTip);
        // 清空去重缓存：否则重新获焦时"常驻显示"会因文本与隐藏前相同而不弹。
        self.last_status_text
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// 常驻(always)模式且启用时,显示当前合成状态(激活/获焦时调用)。temp 模式不在此显示。
    pub(crate) fn show_persistent_status_if_always(&self) {
        let si = &self.rt().config.ui.status;
        if si.enabled && si.display_mode.eq_ignore_ascii_case("always") {
            self.show_tip(&self.status_indicator_text());
        }
    }

    /// `ui.status.show_on_focus`：焦点切到新输入框时强制显示一次状态气泡。
    ///
    /// **不走 [`Self::show_status`]**：那条路会因「文本与上次相同」整个跳过，而焦点切换正是
    /// 「状态没变但仍要提示」的场景——走去重就等于这个开关在同状态下完全不生效。
    ///
    /// ## 坐标可信度闸门
    ///
    /// `follow_caret` 模式下只在坐标属 TSF 语义域时才显示。理由：`OnSetFocus` 不是按键上下文，
    /// 同步 edit session 必被宿主拒绝，回退链交出的是**跨窗口的** Win32 光标——Word 只在正文行
    /// 维护它，标题行上取到的是别处的陈旧值（实测偏差 814px）。用那种坐标弹气泡，正是用户
    /// 反馈的「还没输入时定位非常不准」。
    ///
    /// 拿不到就**不显示**，不做任何回退。下界 = 和没有这个功能一样好，不存在比原状更差的分支；
    /// 而弹在错误位置是实实在在的负价值。DLL 侧排队档会在 1~2ms 内补一条 TSF 坐标，
    /// 由 [`Self::handle_caret_update`] 消费本次挂起并补显示，故绝大多数宿主上并不会真的落空。
    ///
    /// `fixed` 模式不读 caret（用 custom_x/custom_y），故不受本闸门约束，一律直接显示。
    /// `client_token` 用于按**宿主**去重，见 [`Self::last_focus_tip_token`]：同一宿主内部换
    /// docMgr（Excel 单元格 ↔ 公式编辑栏）不该重复弹。
    pub(crate) fn show_focus_status_if_enabled(&self, client_token: u64) {
        let si = &self.rt().config.ui.status;
        if !si.enabled || !si.show_on_focus {
            return;
        }
        // 宿主去重。放在最前面：后面几条分支（fixed / TSF 闸门 / 挂起）都属于「这一次该怎么弹」，
        // 而这里回答的是**该不该弹**，语义在先。token=0 是旧 DLL 未携带的占位，不参与去重。
        if client_token != 0 {
            let mut last = self
                .last_focus_tip_token
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *last == client_token {
                debug!("focus_tip → 跳过: 同一宿主内换 docMgr（token={client_token:#x}）");
                return;
            }
            *last = client_token;
        }
        // always 模式已由 show_persistent_status_if_always 在同一处焦点回调里显示过，
        // 这里再来一次只会重复下发同一帧。
        if si.display_mode.eq_ignore_ascii_case("always") {
            return;
        }
        if si.position_mode.eq_ignore_ascii_case("fixed") {
            self.show_tip(&self.status_indicator_text());
            return;
        }
        let source = {
            let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.caret_source
        };
        if wind_ipc::protocol::caret_source::is_tsf(source) {
            self.pending_focus_tip
                .store(false, std::sync::atomic::Ordering::Relaxed);
            self.show_tip(&self.status_indicator_text());
        } else {
            // 挂起，等 DLL 补来的 TSF 坐标。挂起在下次焦点事件/失焦时作废，不设超时兜底——
            // 超时到期只能拿现有的不可信坐标显示，那正是本闸门要挡的东西。
            debug!(
                "focus_tip → 挂起: 坐标来源 {} 非 TSF 域，等待权威坐标",
                wind_ipc::protocol::caret_source::name(source)
            );
            self.pending_focus_tip
                .store(true, std::sync::atomic::Ordering::Relaxed);
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
        // 内容段过滤：ui.status.items 未列出的段不参与拼接。空列表 = 全部显示
        // （既是未配置时的合理默认，也让无此键的旧配置行为不变）。
        let items = self.rt().config.ui.status.items.clone();
        let show = |k: &str| items.is_empty() || items.iter().any(|i| i == k);

        let mut parts: Vec<String> = Vec::new();
        // 方案 / 中英 / 大写锁定。三者共用首个槽位：关掉 caps 段时大写锁定不再顶替，
        // 落回正常的中英/方案显示。
        if caps && show("caps") {
            parts.push("A".into());
        } else if !show("schema") {
            // 方案段关闭：首槽整体略过（含英文态标记）
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
        // 标点（本段启用时总显示）：英文模式（含大写锁定）下固定显示半角，
        // 不看内部 punct_cn 状态。
        if show("punct") {
            let effective_chinese = chinese && !caps;
            parts.push(if effective_chinese && punct_cn {
                "。".into()
            } else {
                ".".into()
            });
        }
        // 全角（仅全角时）
        if full && show("full_width") {
            parts.push("全".into());
        }
        // 繁（仅繁体时）
        if s2t && show("s2t") {
            parts.push("繁".into());
        }
        parts.join(" ")
    }

    /// 显示合成的核心状态气泡（中英/标点/全半角/简繁/方案切换共用）。
    ///
    /// 文本与上次显示的完全相同时**整个跳过**，不弹窗——用户通过 `ui.status.items`
    /// 关掉某段后，切换该状态不再改变气泡文本，弹一个和上次一模一样的气泡纯属噪声。
    pub(crate) fn show_status(&self) {
        let text = self.status_indicator_text();
        {
            let last = self
                .last_status_text
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if *last == text {
                return;
            }
        }
        self.show_tip(&text);
    }

    /// 分发热键动作；返回是否已处理
    pub(crate) fn dispatch_hotkey(&self, action: &str) -> bool {
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
                self.record_last_state();
                self.push_state_update();
                self.show_status();
                self.notify_toolbar();
                true
            }
            "toggle_punct" => {
                let effective_chinese = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_mode && !s.caps_lock
                };
                if effective_chinese {
                    {
                        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        s.chinese_punct = !s.chinese_punct;
                    }
                    self.record_last_state();
                    self.push_state_update();
                    self.show_status();
                    self.notify_toolbar();
                }
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
                let on = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.s2t_enabled = !s.s2t_enabled;
                    s.s2t_enabled
                };
                self.persist_s2t_enabled(on);
                self.show_status();
                // 工具栏「繁」格随切即刷（对齐 toggle_full_width 与菜单路径）。缺这步时
                // 只有一闪而过的状态气泡，工具栏状态滞后到下次刷新事件，被误感知为“切换卡”。
                self.notify_toolbar();
                true
            }
            "toggle_toolbar" => {
                self.toggle_toolbar();
                true
            }
            "open_settings" => {
                self.open_settings(None);
                true
            }
            "take_screenshot" => {
                self.trigger_screenshot();
                true
            }
            // macOS 专属：Windows 上该动作由 ctfmon 原生处理，本进程收不到、也不该处理。
            #[cfg(target_os = "macos")]
            "activate_ime" => wind_ui::input_source_macos::select_self(),
            _ => {
                debug!("Unhandled hotkey action: {}", action);
                false
            }
        }
    }

    /// 全局热键触发（Win32 RegisterHotKey 的 WM_HOTKEY，UI 线程回送）：统一走 dispatch_hotkey。
    /// 此路径无 TSF 按键上下文，需要 composition 的动作（add_word）不参与全局注册
    /// （见 build_global_hotkey_entries），直接复用分发即可。
    fn handle_global_hotkey(&self, action: &str) {
        debug!("Global hotkey: {}", action);
        self.dispatch_hotkey(action);
    }

    /// 从 keys.global_hotkeys（动作名列表）构建全局热键条目（Win32 RegisterHotKey /
    /// macOS Carbon RegisterEventHotKey）。对齐 Go buildGlobalHotkeyEntries：仅支持无需
    /// 按键上下文的动作。
    ///
    /// activate_ime 是个例外，不读 keys.global_hotkeys：Windows 上它由 ctfmon 从
    /// DirectSwitchHotkeys 注册表直接接管（见 `sync_direct_switch_hotkey`），macOS 无对应
    /// 机制，改由本进程注册 Carbon 热键并调 TISSelectInputSource（见函数末尾的 macOS 分支）。
    fn build_global_hotkey_entries(&self) -> Vec<GlobalHotkeyEntry> {
        let rt = self.rt();
        let k = &rt.config.keys;
        let supported: [(&str, &str); 7] = [
            ("switch_engine", k.switch_engine.as_str()),
            ("toggle_full_width", k.toggle_full_width.as_str()),
            ("toggle_punct", k.toggle_punct.as_str()),
            ("toggle_toolbar", k.toggle_toolbar.as_str()),
            ("open_settings", k.open_settings.as_str()),
            ("take_screenshot", k.take_screenshot.as_str()),
            ("toggle_s2t", k.toggle_s2t.as_str()),
        ];
        let mut entries: Vec<GlobalHotkeyEntry> = Vec::new();
        for name in &k.global_hotkeys {
            let Some((_, value)) = supported.iter().find(|(n, _)| *n == name.as_str()) else {
                warn!("global_hotkeys: 不支持的动作 {:?}，忽略", name);
                continue;
            };
            let Some(hash) = hotkey::parse_hotkey(value) else {
                warn!("global_hotkeys: {} 的热键 {:?} 解析失败，忽略", name, value);
                continue;
            };
            // key_hash 布局 = (wind 修饰位 << 16) | vk（见 wind-config hotkey.rs）
            let (mods, vk) = (hash >> 16, hash & 0xFFFF);
            entries.push(GlobalHotkeyEntry {
                id: entries.len() as i32 + 1,
                modifiers: wind_mods_to_win32(mods),
                vk,
                action: name.clone(),
            });
        }
        // macOS：activate_ime 也走本进程的 Carbon 全局热键。
        //
        // 它**不**读 keys.global_hotkeys——那个列表是「哪些动作要额外提升为全局」的开关，
        // 而 activate_ime 的语义本来就只有全局一种（本输入法没激活时才需要它）。Windows 上
        // 它同样不在该列表里，是由 ctfmon 从注册表直接接管的；macOS 无对应机制，只能自己注册。
        // 判据因此是「配了就注册」，与 sync_direct_switch_hotkey 的 Windows 分支一致。
        #[cfg(target_os = "macos")]
        {
            let hotkey = self.rt().config.keys.activate_ime.trim().to_string();
            if !hotkey.is_empty() && !hotkey.eq_ignore_ascii_case("none") {
                match hotkey::parse_hotkey(&hotkey) {
                    Some(hash) => entries.push(GlobalHotkeyEntry {
                        id: entries.len() as i32 + 1,
                        modifiers: wind_mods_to_win32(hash >> 16),
                        vk: hash & 0xFFFF,
                        action: "activate_ime".to_string(),
                    }),
                    None => warn!("activate_ime 热键 {:?} 解析失败，忽略", hotkey),
                }
            }
        }
        entries
    }

    /// 配置里是否给 CapsLock 配了会话态绑定（决定要不要装全局钩子）。
    ///
    /// 判据取**编译后的绑定表**而非原始配置串：动词写错、键名写错的条目在 `ConfigBundle::build`
    /// 里已被剔除，那些情况不该装钩子（用户的配置根本不会生效，装了纯属白担全局钩子的风险）。
    pub fn capslock_bound(&self) -> bool {
        self.rt()
            .session_keys
            .classify(keymap::VK_CAPITAL, false, true)
            .is_some()
    }

    /// 按配置装/卸 CapsLock 全局钩子（启动与配置热重载时调用）。
    ///
    /// ★ 幂等：已装且仍该装 → 不动（重复 `SetWindowsHookExW` 会留下卸不掉的旧钩子）。
    pub(crate) fn sync_capslock_hook(&self) {
        let want = self.capslock_bound();
        let mut slot = self.capslock_hook.lock().unwrap_or_else(|e| e.into_inner());
        if want == slot.is_some() {
            return;
        }
        if !want {
            // Drop 即卸载（内部会先停拦截再停消息泵）。
            *slot = None;
            wind_keys::capslock_hook::set_should_eat(false);
            info!("CapsLock 未配置会话态绑定 → 全局钩子已卸载");
            return;
        }
        // 钩子回调在钩子线程执行，**必须只做非阻塞投递**：它超时会被系统静默移除且无从察觉。
        // 故这里只 send，真正的动作在 new 起的消费线程里做（可安全加锁）。
        let tx = self.capslock_press_tx.clone();
        match wind_keys::capslock_hook::CapsLockHook::install(Box::new(move || {
            let _ = tx.send(());
        })) {
            Ok(h) => {
                *slot = Some(h);
                // 立刻按当前会话状态校准一次，避免装好后到下一次按键之间状态为默认值。
                let eat = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    Self::has_input_session(&s)
                };
                wind_keys::capslock_hook::set_should_eat(eat);
            }
            Err(e) => {
                // 装不上就退化为「CapsLock 绑定不生效」，不影响其余功能。绝不回退到
                // 「翻转再回敲」——那条路已被真机否掉（竞态 + 厂商 OSD 弹窗）。
                tracing::error!("CapsLock 全局钩子安装失败，该绑定将不生效: {e}");
            }
        }
    }

    /// 同步「钩子此刻该不该吃 CapsLock」。
    ///
    /// ★★ 这个标志为 true 的时间窗必须尽量短。钩子是**全局**的：标志滞留意味着用户在
    /// **别的应用**里按 CapsLock 也切不动大小写——比功能不生效糟糕得多。故凡是会改变
    /// 「有没有输入会话」的出口都要调它，宁可多调（幂等的原子写，开销可忽略）。
    pub(crate) fn sync_capslock_gate(&self, state: &State) {
        // 未装钩子时也照常写：装钩子那一刻会重新校准，这里写了不会有副作用。
        wind_keys::capslock_hook::set_should_eat(Self::has_input_session(state));
    }

    /// 钩子报告「CapsLock 被按下」（在专用消费线程执行，可安全加锁）。
    ///
    /// 走的是与键盘路径**同一个** `apply_session_action`，故动词值域、守卫、各模式的翻页
    /// 出口都不会分叉。钩子只负责「这个键被按了」，「按了该干什么」仍归那一张表。
    fn handle_capslock_hook_press(&self) {
        let action = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 合成一个 keyup 事件：CapsLock 在键盘路径上本来就只有 keyup 到得了这里
            // （见 `handle_session_action_key_up`），保持同形以免两条路径的守卫产生差异。
            let data = KeyEventData {
                key_code: keymap::VK_CAPITAL,
                scan_code: 0,
                modifiers: 0,
                event_type: EVENT_KEY_UP,
                toggles: 0,
                event_seq: 0,
                prev_char: 0,
            };
            self.apply_session_action(&mut state, &data, true)
        };
        // 候选窗刷新已在 `apply_session_action` 内部完成（`notify_ui_update`），此处无须再推。
        // 返回值是给 TSF 的按键结果，而钩子路径**没有 TSF 按键上下文**可回传——与既有的
        // 全局热键路径（`handle_global_hotkey`）同一处境。
        //
        // ⚠️ 已知限制：`app_inline`（编码嵌入宿主）模式下，需要回写宿主内联串的结果
        // （`UpdateComposition` / `ClearComposition`）无法送达，宿主里的编码会滞留到下一次
        // 真实按键。翻页/高亮在候选窗模式下不受影响——那是本功能的主诉求。
        match action {
            Some(KeyAction::Consumed) | None => {}
            Some(_) => {
                debug!(
                    "CapsLock 钩子：该动作需回写宿主内联编码，钩子路径无法回传（app_inline 下会滞留）"
                );
            }
        }
    }

    /// 注册/刷新全局热键（启动与配置热重载时调用）。空列表也下发，用于清除旧注册。
    pub(crate) fn sync_global_hotkeys(&self) {
        let entries = self.build_global_hotkey_entries();
        debug!("sync_global_hotkeys: {} entries", entries.len());
        let _ = self.ui_tx.send(UiCommand::RegisterGlobalHotkeys(entries));
    }

    /// 同步 activate_ime 到 Windows DirectSwitchHotkeys 注册表（启动与配置热重载时调用）。
    /// 该热键由 ctfmon 原生处理（per-app 切换到本输入法），本进程不参与按键分发；
    /// 未配置/解析失败 → 仅清理注册表旧条目。
    ///
    /// 非 Windows 为空操作：macOS 的 activate_ime 走 `build_global_hotkey_entries` 里的
    /// Carbon 注册（切换是**全局**的，非 per-app——系统无对应 API，差异不可消除）。
    pub(crate) fn sync_direct_switch_hotkey(&self) {
        #[cfg(windows)]
        {
            let hotkey = self.rt().config.keys.activate_ime.trim().to_string();
            let entry = if hotkey.is_empty() || hotkey.eq_ignore_ascii_case("none") {
                None
            } else {
                match hotkey::parse_hotkey(&hotkey) {
                    // DirectSwitch Modifiers 低位与 Win32 RegisterHotKey 同位序（TF_MOD_*）
                    Some(hash) => Some((wind_mods_to_win32(hash >> 16), hash & 0xFFFF)),
                    None => {
                        warn!(
                            "activate_ime 热键 {:?} 解析失败，仅清理注册表旧条目",
                            hotkey
                        );
                        None
                    }
                }
            };
            crate::direct_switch::sync(&hotkey, entry);
        }
    }

    /// 放弃整段输入、上屏原码时该**归还**的引导符（不归还则为空串）。
    ///
    /// 三个同源出口共用：临拼回车 / mix 回车 / 切中英文（`take_input_on_mode_switch`）。
    /// 只改其中一处就会造成「回车带 z、切英文不带」这类不一致，故判据收在这里。
    ///
    /// # 为什么字母归还、符号不归还
    ///
    /// 符号引导键（`` ` ``、`;`）在码表里不产出编码，用户按它只可能是为了开模式；字母
    /// （z）在码表里是**合法编码字符**，按下时它既可能是开关也可能是码。放弃整段的语义正是
    /// 「别猜了，把我打的原样给我」，此时吞掉那个字母就是猜错了还不还。z-fallback 进来的
    /// 更是如此——那个 z 是从 `input_buffer` 里抢走的真实击键。
    ///
    /// # 为什么 committed_text 非空就不归还
    ///
    /// 用户已经在模式内选过词，说明他认可了这次进入，引导符归模式所有；再吐出来只会得到
    /// 「z你好ma」这种谁也不想要的东西。
    pub(crate) fn guide_to_return(prefix: &str, committed_text: &str) -> String {
        if committed_text.is_empty()
            && prefix
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            prefix.to_string()
        } else {
            String::new()
        }
    }

    /// 切换中英文时取消当前输入：清空缓冲/候选/preedit，并按 `hotkeys.commit_on_switch`
    /// 决定是否把已输入的原始编码上屏（仅在切到英文且有待输入时）。返回待上屏文本。
    fn take_input_on_mode_switch(&self, state: &mut State, chinese: bool) -> String {
        // 独占模式的「模式切换上屏」策略：
        // - 临时英文：残留缓冲按模式切换语义无条件提交（英文原文，可全角）。
        // - 临时拼音 / mix（含快捷输入）：与下方普通组合一致，遵循 keys.commit_on_switch——
        //   切英文且有待输入且开关开时上屏「已转换前缀 committed_text + 剩余原码缓冲」，否则
        //   清空；触发键前缀（`/;）不输出，与各自回车上屏一致。
        // - 其余独占模式（网址）：丢弃。
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
            } else if let Some((buf, prefix)) = match state.active {
                Some(ModeKind::TempPinyin) => Some((
                    state.temp_pinyin_buffer.clone(),
                    state.temp_pinyin_prefix.clone(),
                )),
                Some(ModeKind::Mix(_)) => {
                    Some((state.mix_buffer.clone(), state.mix_prefix.clone()))
                }
                _ => None,
            } {
                // 临拼 / mix：镜像普通组合的 commit_on_switch，且对齐各自的回车上屏语义。
                let has_pending = !buf.is_empty() || !state.committed_text.is_empty();
                if !chinese && self.rt().config.keys.commit_on_switch {
                    if has_pending {
                        // 有待输入：上屏「引导字母 + 已转换前缀 committed_text + 剩余原码」。
                        // 符号引导符不输出、字母引导符归还，判据见 `guide_to_return`
                        // ——与临拼/mix 的回车上屏共用同一条，三处必须同进同出。
                        // committed 段已在选词时记过，此处只记本次实际上屏的原码（来源模式切换）。
                        let guide = Self::guide_to_return(&prefix, &state.committed_text);
                        let code = format!("{}{}", guide, buf);
                        self.record_commit(&code, code.len() as u32, -1, CommitSource::ModeSwitch);
                        let raw = format!("{}{}{}", guide, state.committed_text, buf);
                        self.maybe_s2t(state, &raw)
                    } else if !prefix.is_empty() && !self.enter_clears_composition() {
                        // 只按了模式进入符（缓冲空）：原样上屏该前缀符号本身，与回车空缓冲上屏一致
                        // （enter_behavior=clear 时回车也不上屏，故一并放弃）。
                        self.record_commit(&prefix, 0, -1, CommitSource::Punctuation);
                        prefix
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
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
            // 上屏原码 → 同回车，用用户所打的大小写形态（缓冲本身恒小写）。
            let raw_code =
                preedit_cursor::cased_or_buffer(&state.input_buffer, &state.input_buffer_cased)
                    .to_string();
            // 模式切换上屏：committed 段已在选词时记过，此处只记剩余原码（来源模式切换）。
            self.record_commit(
                &raw_code,
                raw_code.len() as u32,
                -1,
                CommitSource::ModeSwitch,
            );
            self.maybe_s2t(state, &format!("{}{}", prefix, raw_code))
        } else {
            String::new()
        };
        state.committed_text.clear();
        state.committed_segs.clear();
        state.input_buffer.clear();
        state.input_buffer_cased.clear();
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

    /// 从 store 重建短语层（短语类 RPC 改动后调用，使输入期即时生效）。
    pub(crate) fn rebuild_phrases(&self) {
        let recs: Vec<(String, String, i32, i32, bool)> = match self.store.as_ref() {
            Some(store) => store
                .enabled_phrases_for_input()
                .unwrap_or_default()
                .into_iter()
                .map(|p| (p.code, p.text, p.weight, p.position, p.is_system))
                .collect(),
            None => Vec::new(),
        };
        let mut g = self.phrases.write().unwrap_or_else(|e| {
            warn!("phrases 写锁中毒，恢复后重建");
            e.into_inner()
        });
        *g = wind_phrase::PhraseLayer::from_records(recs);
    }

    /// 当前有效的系统短语条目：重读 system.phrases.toml，为空则回退启动缓存。
    ///
    /// 重读使手工编辑 TOML 后无需重启服务。`parse_system_entries` 对"文件缺失"与
    /// "TOML 语法错误"同样返回空，二者不可区分，故重读为空时回退到启动缓存——
    /// 否则一个语法错误就会让调用方的 sync 把库里系统短语全部删除。
    pub(crate) fn current_system_phrase_entries(
        &self,
        reason: &str,
    ) -> Vec<wind_phrase::SystemPhraseEntry> {
        let reread = self
            .system_phrase_path
            .as_ref()
            .map(|p| wind_phrase::PhraseLayer::parse_system_entries(p))
            .unwrap_or_default();

        if reread.is_empty() {
            if self.system_phrase_path.is_some() {
                warn!(
                    "{reason}：重读 system.phrases.toml 为空（文件缺失或语法错误），沿用启动缓存"
                );
            }
            self.system_phrase_entries
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        } else {
            // 重读成功：刷新缓存，后续回退以最新文件内容为准。
            let mut g = self
                .system_phrase_entries
                .write()
                .unwrap_or_else(|e| e.into_inner());
            *g = reread.clone();
            reread
        }
    }

    /// 把**缺失**的系统短语条目补回库里（不动任何已存在的行）。
    ///
    /// 用户短语遮蔽同键系统条目时该行**归属用户**（`is_system=false`，见
    /// `Store::add_phrase`），于是任何「清空用户短语」的动作都会把它连同遮蔽关系一起删掉——
    /// 库里该 `(code,text)` 彻底消失，系统条目也随之不见。sync 平时只在 TOML 哈希变化或
    /// 「系统恢复默认」时才跑，不补这一次，被遮蔽过的系统短语要等到下次哈希变动才回来。
    ///
    /// **两个调用点**（漏一个就等于那条路上的系统短语静默丢失）：设置页「清空用户短语」、
    /// 备份还原的 replace 模式（`restore_backup` 内部会先 `reset_user_phrases`）。
    ///
    /// ⚠️ **走 `ensure_system_phrases` 而非 `sync_system_phrases`**：后者会用 TOML 值覆盖已存在
    /// 系统行的 weight/position，那样一次「清空用户短语」会顺带把用户在系统短语列表里改过的
    /// 权重重置掉——用户没要求这件事。补齐只应补缺失的。
    pub(crate) fn restore_missing_system_phrases(&self, reason: &str) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let entries = self.current_system_phrase_entries(reason);
        if entries.is_empty() {
            return;
        }
        let sys: Vec<wind_store::phrases::SystemPhrase> = entries
            .iter()
            .map(|e| wind_store::phrases::SystemPhrase {
                code: e.code.clone(),
                text: e.text.clone(),
                weight: e.weight,
                position: e.position,
            })
            .collect();
        match store.ensure_system_phrases(&sys) {
            Ok(n) if n > 0 => info!("{reason}：补回 {n} 条缺失的系统短语"),
            Err(e) => warn!("{reason}：系统短语补齐失败: {e}"),
            _ => {}
        }
    }

    /// 恢复默认系统短语：重读 system.phrases.toml → 强制同步入库 + 全部启用 + 重建输入层。
    pub(crate) fn restore_system_phrases(&self) -> usize {
        let Some(store) = self.store.as_ref() else {
            return 0;
        };

        let entries = self.current_system_phrase_entries("恢复默认");
        if entries.is_empty() {
            return 0;
        }

        let sys: Vec<wind_store::phrases::SystemPhrase> = entries
            .iter()
            .map(|e| wind_store::phrases::SystemPhrase {
                code: e.code.clone(),
                text: e.text.clone(),
                weight: e.weight,
                position: e.position,
            })
            .collect();
        // 先认领：历史上 `add_phrase`/wdict 导入撞键时会把系统行降级成用户行，此后
        // `sync_system_phrases` 的 `!cur.is_system → continue` 分支永远跳过它，该条目
        // 从「系统短语」列表里再也回不来。「恢复默认」是显式动作，在此把归属改回去。
        // 必须排在 sync 之前，认领后的行才能被 sync 刷新 weight/position。
        match store.reclaim_system_phrases(&sys) {
            Ok(n) if n > 0 => info!("恢复默认：认领回 {n} 条被降级的系统短语"),
            Err(e) => warn!("恢复默认：系统短语认领失败: {e}"),
            _ => {}
        }
        if let Err(e) = store.sync_system_phrases(&sys) {
            warn!("恢复默认：系统短语同步失败: {e}");
            return 0;
        }
        // 哈希随之更新，否则下次启动会因哈希不符再同步一次（无害但多余）。
        let _ = store.set_phrase_sys_hash(&phrase_entries_hash(&entries));

        let n = store.reset_system_enabled().unwrap_or(0);
        self.rebuild_phrases();
        entries.len().max(n)
    }
}

impl Coordinator {
    /// 失焦类事件的归属校验：`client_token` 不是当前活动客户端时判为**陈旧事件**并丢弃。
    ///
    /// 必要性来自 DLL 侧刻意安排的时序：DocMgr 级失焦是噪声信号（VSCode 实测一次应用切换
    /// 伴随 5 次），故 focus_lost 不在那里发，改由 `OnKillThreadFocus` 发出——实测**比
    /// DocMgr 级失焦晚约 100ms**（见 TextService.cpp 失焦分支注释）。而新宿主的
    /// focus_gained 在十几毫秒内就送达，于是跨宿主切换时到达顺序恒为
    /// 「新宿主 focus_gained → 旧宿主 focus_lost」。
    ///
    /// `ime_active` 是全局单例（不区分客户端），无校验时后者会把前者刚建立的激活态清掉：
    /// 工具栏闪一下即隐藏。服务端日志指纹＝`UpdateToolbar` 后约 90ms 紧跟一条 `HideToolbar`，
    /// 且此后长时间没有新的 `UpdateToolbar`。
    ///
    /// 两种放行情形：`client_token == 0`（旧 DLL 不带 token，保持既有行为）、
    /// `active == 0`（尚无任何客户端获焦，无从判定归属）。
    ///
    /// 注意本校验**只挡跨宿主**：同一进程内多个 DocMgr 共用一个 token，宿主自身在两个
    /// DocMgr 间抖动时 token 相同、一律放行——那条路径是 doc_changed 先发 focus_lost 紧接
    /// focus_gained，间隔 <10ms，由 UI 层 50ms 隐藏防抖吸收。
    pub(crate) fn is_stale_focus_event(&self, client_token: u64, what: &str) -> bool {
        let active = self.push_server.active_token();
        if client_token == 0 || active == 0 || client_token == active {
            return false;
        }
        tracing::debug!(
            "{}: 丢弃陈旧失焦 token={:#x} active={:#x}（旧宿主迟到的失焦，不动激活态与 UI）",
            what,
            client_token,
            active
        );
        true
    }
}

/// 解析扩展信封里的 `{"x":123,"y":456}` 落点 body。
///
/// 非法/缺字段/越界一律返回 `None` 交调用方忽略，而不是取 0 兜底：位置类消息拿默认值
/// 比丢掉一次拖动坏得多——`(0,0)` 会被当成合法坐标落盘，候选窗就此跑到屏幕左上角。
fn decode_ext_point(body: &[u8]) -> Option<(i32, i32)> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let x = v.get("x")?.as_i64()?;
    let y = v.get("y")?.as_i64()?;
    Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?))
}

/// `shot.result` → Toast 文案。
///
/// 抽成纯函数是为了可测：这里全是措辞分支，而措辞正是**必须与 Windows 侧
/// `manager.rs` 逐字一致**的东西——两平台同一操作得到不同说法是最没必要的分叉，
/// 而这种分叉不会有任何编译或运行期信号。
fn shot_result_message(v: &serde_json::Value) -> (String, ToastKind) {
    let results = v.get("results").and_then(|r| r.as_array());
    let ok = |r: &serde_json::Value| r.get("ok").and_then(|b| b.as_bool()) == Some(true);
    if v.get("mode").and_then(|m| m.as_str()) == Some("all") {
        // 「截图所有窗口」：本进程截的候选窗数量由请求原样带回，与 `.app` 这边的成功数
        // 相加，合成**一条** Toast（各弹各的会连弹三四条）。
        let n = v.get("already").and_then(|n| n.as_u64()).unwrap_or(0) as usize
            + results.map_or(0, |a| a.iter().filter(|r| ok(r)).count());
        let dir = v.get("dir").and_then(|d| d.as_str()).unwrap_or("");
        if n == 0 {
            return ("没有可见窗口可截图".to_string(), ToastKind::Info);
        }
        return if v.get("already_clipboard").and_then(|b| b.as_bool()) == Some(true) {
            (
                format!("已保存 {n} 张截图（候选已复制到剪贴板）\n{dir}"),
                ToastKind::Success,
            )
        } else {
            (format!("已保存 {n} 张截图\n{dir}"), ToastKind::Success)
        };
    }
    // 单窗截图（气泡/提示自身右键菜单里的「截图此窗口」）。
    let Some(r) = results.and_then(|a| a.first()) else {
        return ("截图失败：无结果".to_string(), ToastKind::Error);
    };
    let label = match r.get("target").and_then(|t| t.as_str()) {
        Some("tooltip") => "悬停提示",
        _ => "状态提示气泡",
    };
    if ok(r) {
        let path = r.get("path").and_then(|p| p.as_str()).unwrap_or("");
        let suffix = if r.get("clipboard").and_then(|b| b.as_bool()) == Some(true) {
            "（已复制到剪贴板）"
        } else {
            ""
        };
        (format!("{label}已截图{suffix}\n{path}"), ToastKind::Success)
    } else {
        match r.get("reason").and_then(|x| x.as_str()) {
            // 不可见不是错误：用户在气泡消失之后才点的菜单，如实告知即可。
            Some("not_visible") | None => (format!("{label}未显示，无法截图"), ToastKind::Info),
            Some(e) => (format!("截图失败：{e}"), ToastKind::Error),
        }
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
                self.record_last_state();
                self.push_state_update();
                self.show_status();
                Some(self.build_status())
            }
            "toggle_punct" => {
                let effective_chinese = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    s.chinese_mode && !s.caps_lock
                };
                if effective_chinese {
                    {
                        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                        s.chinese_punct = !s.chinese_punct;
                    }
                    self.record_last_state();
                    self.push_state_update();
                    self.show_status();
                }
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
                self.persist_s2t_enabled(on);
                self.show_status();
                Some(self.build_status())
            }
            _ => None,
        }
    }

    /// macOS `.app` 查询功能主菜单：构建菜单树并编码为 `CmdMenuShow` 帧字节。
    /// Windows 走进程内 `show_main_menu` 渲染，不用此路径（返回空帧亦无害）。
    fn query_menu_encoded(&self, simplified: bool) -> Vec<u8> {
        #[cfg(target_os = "macos")]
        {
            // IMK 输入源菜单用精简树(无子菜单)；候选框右键/菜单栏指示器用完整树(带子菜单，
            // 经 inProcess 直接投递，AppKit 能正确处理嵌套子菜单)。
            let items = if simplified {
                self.build_menu_items_macos()
            } else {
                self.build_main_menu_items()
            };
            let nodes = Self::menu_items_to_nodes(&items);
            wind_ipc::codec::encode_menu_show(&nodes)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = simplified;
            Vec::new()
        }
    }

    /// macOS `.app` 回传统一菜单选择：由菜单 id 还原动作并派发。
    fn handle_menu_action_id(&self, id: i32) {
        if let Some(kind) = wind_ui::manager::MenuKind::from_menu_id(id) {
            self.menu_action(kind);
        } else {
            tracing::debug!("handle_menu_action_id: 未知菜单 id {}", id);
        }
    }

    /// macOS `.app` 上报前台上下文（聚焦时快照）：缓存 app/title/sel 供命令直通车取值。
    fn handle_front_context(&self, app: &str, title: &str, sel: &str) {
        let mut fc = self.front_ctx.lock().unwrap_or_else(|e| e.into_inner());
        *fc = (app.to_string(), title.to_string(), sel.to_string());
    }

    /// 鼠标左键点选候选（macOS `.app` / Windows host-render DLL）：
    /// ≥0 复用 `mouse_select`（提交页内第 N 个候选）；负值为翻页按钮
    /// （-1 上页 / -2 下页，对齐 Go HandleCandidateSelect 的分流），复用本地窗口
    /// 点击翻页的 `mouse_page` 路径（翻页后经 notify_ui_update 重推帧）。
    fn handle_candidate_select(&self, page_local_index: i32) {
        match page_local_index {
            -1 => self.mouse_page(-1),
            -2 => self.mouse_page(1),
            v if v >= 0 => self.mouse_select(v as usize),
            _ => {}
        }
    }

    /// host 候选框的鼠标滚轮。
    ///
    /// 语义 = **上下方向键调整高亮项**（`move_up`/`move_down`），到页边界自然翻到相邻页，
    /// 不是整页翻动。这是 Windows 上既有的行为，两平台共用本实现。
    ///
    /// 此前是 trait 上的空实现（"统一接入点便于后续按配置实现"），于是 Windows 的
    /// host-render DLL 一直在发这个帧、服务端收下什么也不做——滚轮在**两个平台**都无效。
    /// 不加配置项：滚动候选框就是要动高亮，没有第二种合理解释。
    ///
    /// `delta` 是 `WHEEL_DELTA`(120) 的倍数、正=上滚（Win32 约定，macOS 侧按同一约定折算）。
    /// 一次事件可能跨多格（高速滚轮/触控板惯性），故按格数循环。上限 `MAX_NOTCHES` 防
    /// 惯性滚动一次跳过几十项——那既不是用户意图，也会让候选窗疯狂重绘。
    fn handle_candidate_scroll(&self, delta: i32) {
        const WHEEL_DELTA: i32 = 120;
        const MAX_NOTCHES: i32 = 5;
        if delta == 0 {
            return;
        }
        // 不足一格也算一格：触控板的单次轻扫 delta 可能小于 120，直接整除会得 0（滚不动）。
        let notches = (delta.abs() / WHEEL_DELTA).max(1).min(MAX_NOTCHES);
        let up = delta > 0;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.candidates.is_empty() {
            return;
        }
        let mut changed = false;
        for _ in 0..notches {
            let moved = if up {
                self.move_up(&mut state)
            } else {
                self.move_down(&mut state)
            };
            if !moved {
                break; // 已到首/末项，继续滚也没有更多可动
            }
            changed = true;
        }
        if changed {
            self.notify_ui_update(&state);
        }
    }

    /// 鼠标 hover 候选/翻页器：复用进程内路径的 `mouse_hover`（置 hover_index + 重绘高亮帧）。
    /// 两端线约定不同（按编译平台分支，事件源平台互斥）：
    /// - macOS `.app`：候选 ≥0；翻页器 -1(上页)/-2(下页)；无悬停 i32::MIN 哨兵。
    /// - Windows host DLL（HostWindow.cpp `_OnMouseMove`）：候选 ≥0；无悬停 -1；
    ///   翻页器 -2(上页)/-3(下页)——rect 表的 -1/-2 因 hover 需要独立的「无」被平移一位。
    fn handle_candidate_hover(&self, page_local_index: i32) {
        #[cfg(windows)]
        let target = match page_local_index {
            -2 => wind_ui::manager::HOVER_PAGE_PREV,
            -3 => wind_ui::manager::HOVER_PAGE_NEXT,
            v if v >= 0 => v,
            _ => -1,
        };
        #[cfg(not(windows))]
        let target = match page_local_index {
            -1 => wind_ui::manager::HOVER_PAGE_PREV,
            -2 => wind_ui::manager::HOVER_PAGE_NEXT,
            v if v >= 0 => v,
            _ => -1,
        };
        self.mouse_hover(target);
    }

    /// 扩展信封（`CMD_EXT`）：低频消息统一入口。**未知 kind 安静忽略**——旧服务收到新
    /// `.app` 发的新 kind 只当没看见，而不是解析失败把连接搞坏（见 `ext_kind` 的演进约定）。
    fn handle_ext(&self, kind: &str, body: &[u8]) {
        use wind_ipc::protocol::ext_kind;
        match kind {
            // 拖动落点回报。落不落盘由 save_* 按当前定位方式自行判定：固定位置=重新摆放，
            // 跟随光标=只是临时挪开，不写配置。
            ext_kind::POS_CANDIDATE | ext_kind::POS_STATUS_TIP => {
                let Some((x, y)) = decode_ext_point(body) else {
                    tracing::warn!("扩展消息 {kind} 的 body 不是 {{x,y}}，忽略");
                    return;
                };
                if kind == ext_kind::POS_CANDIDATE {
                    self.save_candidate_pos(x, y);
                } else {
                    self.save_status_tip_pos(x, y);
                }
            }
            // 原生浮窗截图的结果（`.app` 动手，服务端只管文案）。
            ext_kind::SHOT_RESULT => match serde_json::from_slice(body) {
                Ok(v) => {
                    let (msg, kind) = shot_result_message(&v);
                    self.show_toast(&msg, ToastPosition::BottomRight, kind);
                }
                Err(e) => tracing::warn!("shot.result 载荷无法解析：{e}"),
            },
            _ => tracing::debug!("未处理的扩展消息 kind={kind}"),
        }
    }

    /// macOS `.app` 候选右键动作：动作串 → 词条操作/复制，作用于页内下标候选。
    fn handle_candidate_context_menu(&self, page_local_index: i32, action: &str) {
        use wind_ui::manager::{CandidateOp, UiCommand};
        if page_local_index < 0 {
            return;
        }
        let page_local = page_local_index as usize;
        let op = match action {
            "move_top" => CandidateOp::MoveTop,
            "move_up" => CandidateOp::MoveUp,
            "move_down" => CandidateOp::MoveDown,
            "delete" => CandidateOp::Delete,
            "reset_default" => CandidateOp::Reset,
            "copy" => {
                // 解析页内下标对应候选文本，交 UI 侧写剪贴板（macOS 走 popup_menu::set_clipboard_text）。
                let text = {
                    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    let (start, end) = self.page_range(&state);
                    let idx = start + page_local;
                    if idx < end && idx < state.candidates.len() {
                        state.candidates[idx].text.clone()
                    } else {
                        String::new()
                    }
                };
                if !text.is_empty() {
                    let _ = self.ui_tx.send(UiCommand::CopyToClipboard(text));
                }
                return;
            }
            other => {
                tracing::debug!("handle_candidate_context_menu: 未知动作 {}", other);
                return;
            }
        };
        self.candidate_op(op, page_local);
    }

    fn handle_show_context_menu(&self, x: i32, y: i32) {
        // 弹出菜单窗口 (popup_menu.rs / ShowCandidateMenu·MenuKey·HideMenu UiCommand) 是
        // Windows 专有；macOS 由 IMK 原生 NSMenu 渲染菜单 (InputController.menu())。
        // macOS 上 IMK 频繁调 menu() → Swift 发 CMD_SHOW_CONTEXT_MENU 仅为「查询菜单项」，
        // 若在此调 show_main_menu 会把协调器置 menu_open=true 并经 forward_menu_key 吞掉后续
        // 所有按键，而 macOS 无弹窗、永不回 MenuClose → 输入被永久卡死 (打字无响应)。
        #[cfg(not(target_os = "macos"))]
        self.show_main_menu(wind_ui::manager::MenuAnchor::at_point(x, y));
        #[cfg(target_os = "macos")]
        let _ = (x, y);
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
        // 自提交打点 + 码表自动造词投喂。与 record_input_stats 同一收口理由：上屏路径有
        // 40+ 个返回点，且约 10 处绕过 commit_action 直接构造 InsertText，散点接线必漏。
        self.note_commit_action(&action);
        // PassThrough / UpdateComposition 时 C++ 侧会调 FlushHoldCompositionIfActive 提交旧符号；
        // coordinator 需同步清除 held_text，防止后续标点的 pre_held_text 捡到已提交的旧值
        // 而造成二次提交（"。。="）。仅在 held_text 非空时操作，避免干扰无 Hold 状态的武装态。
        match &action {
            KeyAction::PassThrough
            | KeyAction::NotHandled
            | KeyAction::UpdateComposition { .. } => {
                let mut arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
                if arm.held_text.is_some() {
                    arm.held_text = None;
                    arm.armed = false;
                    arm.hold_pending_commit = false;
                }
            }
            _ => {}
        }
        // 检索范围临时放宽的失效：本次组合结束（缓冲已空）即恢复配置档位。
        // 与 record_input_stats / note_commit_action 同一收口理由——`input_buffer.clear()`
        // 有十几个调用点（上屏/取消/切焦点/模式切换），散点接线必漏。放在按键处理的唯一出口，
        // 天然覆盖全部结束路径。用户选字上屏后下一次输入即回到智能档；而放宽期间继续敲字母、
        // 退格改码、翻页都不会丢状态（缓冲非空），符合「找生僻字常要改几次编码」的实际。
        self.expire_scope_override();
        // 配对状态保活：须在 handle_key_event **之后**刷新，否则本次按键的陈旧判定
        // 会先被自己刷新掉，TTL 永不触发。栈空时是空操作。
        self.touch_pair_state();
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

        // ── 小键盘归一化（numpad_behavior = follow_main）──
        // 「同主键盘区数字」的语义 = 小键盘键就是主键盘键，故在此改写键码后交由既有主键盘
        // 逻辑接管，一处生效于所有模式。置于最前（仅晚于统计复位）：模式分派、热键、英文
        // 直通等所有后续判断都应看到归一化后的键。direct 时不改写，各模式走自己的 numpad 臂。
        let normalized;
        let data = match numpad_to_main(data.key_code) {
            Some((vk, need_shift)) if self.rt().config.input.numpad_behavior == "follow_main" => {
                normalized = KeyEventData {
                    key_code: vk,
                    modifiers: if need_shift {
                        data.modifiers | MOD_SHIFT
                    } else {
                        data.modifiers
                    },
                    ..data.clone()
                };
                &normalized
            }
            _ => data,
        };
        debug!(
            "handle_key_event: type={} code=0x{:02X} mods=0x{:04X}",
            data.event_type, data.key_code, data.modifiers
        );
        // 记录按键时刻：fast 档据此判断「连续快速输入」（见 handle_caret_probe）。
        // 记录打字节奏：算出**相邻两次按键**的间隔，供 fast 档判断连续输入（见 handle_caret_probe）。
        {
            let now = std::time::Instant::now();
            let prev = self
                .last_key_at
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replace(now);
            if let Some(p) = prev {
                *self
                    .last_key_interval_ms
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    Some(now.duration_since(p).as_millis() as u64);
            }
        }

        // 用每键携带的 toggles 快照（C++ 前台线程 GetKeyState 实时采集）校准 CapsLock 镜像。
        // 专门的 VK_CAPITAL key_up 状态通知在英文模式（TSF 不吃该键）或用户于其它应用/
        // 输入法期间切换大写时不会到达，镜像会陈旧——表现为 cancel_on_mode_switch 在
        // "英文+大写"场景读到 caps_lock=false 而跳过取消。服务进程自身 GetKeyState 的
        // toggle 位跨线程不可靠，故以事件快照为权威。
        {
            let caps_now = (data.toggles & 0x01) != 0;
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if s.caps_lock != caps_now {
                debug!(
                    "CapsLock mirror recalibrated from key toggles: {}",
                    caps_now
                );
                s.caps_lock = caps_now;
            }
        }

        // ── key_up：toggle 模式键（Shift/Ctrl/CapsLock）直接切换 ──
        // 关键：TSF 对 toggle 键会"吃掉 keydown 不转发"，仅在 C++ 侧判定为干净单击后
        // 于 keyUp 转发该键事件（_SendKeyToService(..., KEY_EVENT_UP)）。因此服务端
        // 收到 toggle 键的 keyUp 即应直接切换，无需 keydown/pending（对齐 Go HandleKeyEvent）。
        if data.event_type == EVENT_KEY_UP {
            // 修饰键作二三候选键（select_key_groups 含 lrshift / lrctrl）：**先于**下面一切。
            // 同一个键可能多个身份都配了（设置页会提示冲突，但配置文件里拦不住），既有裁决是
            // 「有候选选词、无候选切换」——输入到一半按 Ctrl 想选词的意图远比切中英文常见，而
            // 空闲时按 Ctrl 除了切换也没别的可做。无候选/越界时返回 None 落到下面各分支。
            //
            // ⚠ 2026-08-10 从 CapsLock 分支**之后**上移到这里。CapsLock 永远不在
            // `select_key_vks` 的值域里（那边只有 semicolon/quote/comma/period/lrshift/lrctrl），
            // 故这次上移对 CapsLock 是无副作用的空转；上移的目的是让下面新增的会话态绑定
            // 也排在选词之后，保住「选词优先」这条既有裁决。
            if let Some(act) = self.handle_select_key_up(data) {
                return act;
            }
            // 会话态绑定里的 keyup-only 键（`capslock = "page_prev"` 那类）。
            //
            // ★ **必须先于**下面 CapsLock 的状态同步分支：那条会调 `take_input_on_mode_switch`
            // 把正在打的编码上屏或丢弃。配了 CapsLock 翻页的用户每翻一页就毁一次输入，
            // 现象是「翻页时编码莫名没了」——极难联想到是大小写同步干的。
            //
            // 无候选时本函数返回 None，键照常落到下面的原有处理（CapsLock 仍切大小写、
            // 修饰键仍切中英文）。「有会话归绑定、无会话归原语义」正是两张表的分野。
            if let Some(act) = self.handle_session_action_key_up(data) {
                return act;
            }
            // CapsLock 单独处理：C++ 侧总是发送此 key_up（不经 key_up_tsf_hashes 过滤），
            // 故须先于 is_toggle_mode_keycode 检查。同步真实大写锁定状态，不翻转 chinese_mode
            // （对齐 Go handleCapsLockStateNoLock：capsLockOn 跟随 data.toggles & 0x01）。
            if data.key_code == 0x14
            /* VK_CAPITAL */
            {
                let caps_lock_on = (data.toggles & 0x01) != 0;
                debug!("CapsLock state notification: on={}", caps_lock_on);
                let had_pending = {
                    let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    !s.input_buffer.is_empty()
                        || !s.committed_text.is_empty()
                        || !s.candidates.is_empty()
                };
                let commit_text = {
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    // 切大写时按"切英文"语义处理待输入（commit_on_switch）；切回小写时直接丢弃。
                    let text = self.take_input_on_mode_switch(&mut s, !caps_lock_on);
                    s.caps_lock = caps_lock_on;
                    text
                };
                self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
                self.push_state_update();
                self.show_status();
                self.notify_toolbar();
                self.notify_ui_hide();
                if !commit_text.is_empty() || had_pending {
                    let chinese_mode = self
                        .state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .chinese_mode;
                    return KeyAction::InsertText {
                        text: commit_text,
                        new_composition: None,
                        mode_changed: false,
                        chinese_mode,
                        has_new_composition: false,
                    };
                }
                return KeyAction::StatusUpdate(self.build_status());
            }
            // 方案级 `[key_actions]` 绑在修饰键上的功能（`rshift = "toggle_schema:english"`）。
            // **先于** is_toggle_mode_keycode：同一个键两处都配时，方案级是更具体的声明，
            // 与 keydown 侧「方案表命中即跳过全局链」同一裁决方向。
            //
            // 只处理纯修饰键：有字符的键归 keydown 的 try_activate_mode 管（英文模式下
            // 必须让它出字），两条路各管一半、不重叠。判据是键的形态而非动词类别，
            // 见 docs/design/schema-key-actions.md §4.1。
            if keymap::is_pure_modifier_vk(data.key_code) {
                if let Some(act) = self.handle_bound_modifier_key_up(data.key_code) {
                    return act;
                }
            }
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
        // 仅非 macOS：弹出菜单窗口是 Windows 专有，macOS 用 IMK 原生菜单自行消费键，
        // 协调器不应吞键 (否则 menu_open 一旦被置真会永久卡死输入，见 handle_show_context_menu)。
        #[cfg(not(target_os = "macos"))]
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
            } else if action == "open_add_word_dialog" {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    return self.open_add_word_from_history(&mut state);
                }
            } else if action == "enter_temp_pinyin" {
                // 临拼直达热键：进入前先上屏半成品（commit_and_enter_temp_pinyin 内含），
                // 传 key_code=0 → 组合区无引导符。已在临拼态则幂等；中文模式下一律吞键
                // （不放行，避免把该组合键泄漏给宿主）。
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    if state.active != Some(ModeKind::TempPinyin)
                        && let Some(target) = self.engine_mgr.temp_pinyin_target()
                    {
                        return self.commit_and_enter_temp_pinyin(&mut state, 0, target);
                    }
                    return KeyAction::Consumed;
                }
            } else if let Some(id) = action.strip_prefix("enter_special:") {
                // 特殊模式直达热键：按 id 定位配置序 idx（与 match_special_trigger 下标语义一致）。
                // 已在该模式则幂等；未知 id / 方案不可加载均安全吞键（不放行以免误触）。
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    if let Some(idx) = self.special_mode_idx(id)
                        && state.active != Some(ModeKind::Special(idx))
                        && let Some(schema) = self.special_schema(idx)
                        && self.engine_mgr.ensure_schema(&schema)
                    {
                        // key_code=0 哨兵：热键进入不写引导符。
                        return self.commit_and_enter_special_mode(&mut state, idx, 0);
                    }
                    return KeyAction::Consumed;
                }
            } else if let Some(id) = action.strip_prefix("toggle_schema:") {
                // 方案往返热键（keys.key_actions）：切过去，再按一次回来源。
                // 与 switch_schema 同样**不判 chinese_mode**——回程尤其要在英文态按得动。
                //
                // trigger_vk 传 0：全局热键在所有方案里都生效，不需要「回程键临时授权」
                // 那套（那是方案级绑定专有的问题，见 `schema_return_key_action`）。
                self.toggle_schema_by_id(id, 0);
                return KeyAction::StatusUpdate(self.build_status());
            } else if let Some(id) = action.strip_prefix("switch_schema:") {
                // 方案直达热键：切 active 方案。**不判 chinese_mode**——与循环键
                // (`switch_engine`) 同策略。切方案在英文态下同样该生效，否则切到英文方案后
                // 这条路径就失效了，用户回不到中文方案。
                self.switch_schema_by_id(id);
                return KeyAction::StatusUpdate(self.build_status());
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

        // 密码框强制英文抑制：透传（不改 chinese_mode 持久值）。图标另有呈现（显 "英"），
        // 走 ToolbarState/语言栏的独立字段，与本判据无耦合——详见 password_suppress 字段注释。
        // 须先于下方全角分支——密码框里不该出全角字符，一律半角透传。
        // 注：透传要真生效，C++ 侧必须也没吃这个键，否则「吃了再吐」丢键（见 TSF 待办）。
        if self
            .password_suppress
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return KeyAction::PassThrough;
        }

        // 配对跳出键：**全模式统一前置判定**，必须早于下面的英文模式分支——英文模式对普通键
        // 直接 PassThrough，判定放在中文路径里就永远跑不到（旧实现即如此，是「英文模式跳不出
        // 中文里打的配对」的根因之一）。守卫与失效方向见 try_jump_out。
        if let Some(act) = self.try_jump_out(&state, data) {
            return act;
        }

        // 英文模式
        if !state.chinese_mode {
            // 全角：键已被 TSF 的 `english_fullwidth` 分支吃下等 Rust 出字，此处必须转换，
            // 否则 PassThrough 会形成「吃了再吐」→ 严格 TSF 宿主丢键（见 handle_english_full_width）。
            // Ctrl/Alt 组合不参与：C++ 的 ClassifyInputKey 对其返回 None，本就不吃。
            if state.full_width
                && data.modifiers & (MOD_CTRL | MOD_ALT) == 0
                && let Some(act) = self.handle_english_full_width(&mut state, data)
            {
                return act;
            }
            // 半角英文 + 该标点键配了「英半」列：DLL 已按 core 推送的字符集合吃下此键
            // （`english_custom_punct` 分支），此处必须出字，否则同样「吃了再吐」丢键。
            // 未配的键 handle 返回 None → 落到下方透传，行为与历史完全一致。
            if data.modifiers & (MOD_CTRL | MOD_ALT) == 0
                && let Some(act) = self.handle_english_custom_punct(&mut state, data)
            {
                return act;
            }
            // 半角英文：透传，宿主自然出字（保留 WM_KEYDOWN 原生语义）。
            return KeyAction::PassThrough;
        }

        // CapsLock 开：大写语义，不进中文输入流。
        // 全角开：将按键转为正确大小写的英文字符再做全角转换后上屏。
        // 全角关：TSF 层在无 session 时已透传；有 session（切换前残留）时由此兜底 PassThrough。
        // Ctrl/Alt 组合不拦截（让下方热键/清空逻辑处理）。
        if state.caps_lock && data.modifiers & (MOD_CTRL | MOD_ALT) == 0 {
            if state.full_width {
                let shift = data.modifiers & MOD_SHIFT != 0;
                let is_letter = (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code);
                // CapsLock 对字母大小写取反：CapsLock ON + no Shift → 大写；Shift → 小写。
                // printable_char 以 shift=true 产生大写，故字母键时翻转 shift。
                let effective_shift = if is_letter { !shift } else { shift };
                // 用 full_width_source_char 而非 printable_char：C++ 在中文全角下也吃
                // 空格(chinese_fullwidth_space)与小键盘(chinese_fullwidth_number)，
                // 而这两者都不在 printable_char 覆盖内 → 曾落下方 PassThrough → 丢键。
                if let Some(ch) = full_width_source_char(data.key_code, effective_shift) {
                    // 经完整标点转换流水线（自定义映射"英全"列 → 全半角），
                    // 而非直接 to_full_width，确保用户自定义映射生效。
                    // 临时置 chinese_punct=false 对应"英全"状态（不走中文标点转换）。
                    let saved_punct = state.chinese_punct;
                    state.chinese_punct = false;
                    let text = self.convert_punct_char(&state, ch);
                    state.chinese_punct = saved_punct;
                    return Self::commit_action(text, true);
                }
            }
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

        // 方案级表的 A 类状态切换（`backslash = "toggle_punct"` 这类）。
        //
        // 与紧随其后的 `try_activate_mode` 分属两半：B 类建 overlay、要 `&mut State`，
        // 故在锁内；A/C 类只改全局状态，目标函数（dispatch_hotkey / toggle_schema_by_id）
        // 各自加锁，**必须锁外执行**——判定在这里做完，guard 就地 drop 掉。
        //
        // 位置在英文模式分水岭之后，与 B 类同：有字符的键在英文态必须能出字。代价是
        // `toggle_mode` 那类「用来离开英文态」的动作在此不可达，故它们限修饰键（keyup 路径），
        // 见 `BoundAction::requires_modifier_key`。
        if let Some(action) = self.bound_lock_free_action_for_keydown(&state, data) {
            drop(state);
            if let Some(act) = self.run_lock_free_bound_action(&action, data.key_code) {
                return act;
            }
            // 门卫没过：不吞键，重新取锁走原有链路（与各模式门卫同策略）。
            state = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
            "key_event: code=0x{:02X} mods=0x{:04X} chinese={} full={} caps={} buf='{}'",
            data.key_code,
            data.modifiers,
            state.chinese_mode,
            state.full_width,
            state.caps_lock,
            state.input_buffer
        );

        // 非字母码元闸门：本方案把某个数字/符号配成了码元（如 `a-z0-9` 要打 `Win10`、
        // `a-x/` 要打含 `/` 的词条）→ 进缓冲，抢在以词定字/翻页/数字选词/标点流水线之前。
        //
        // 位置即契约（见 docs/design/codetable-input-chars.md「组码中码元优先，空缓冲让位」）：
        // 置于模式激活与 URL 夺取**之后**，故空缓冲下的引导键、临拼/临英触发键、URL 前缀
        // 一概不受影响；置于下方各闸门**之前**，故组码中这些键归码表而非选词/翻页。
        //
        // 空缓冲时闸门查的是**首码集**：数字默认不在其中 ⇒ 不接管 ⇒ 数字键照常选词/透传，
        // 用户不会失去「选第 1 个候选」和原生数字输入。
        //
        // ⚠️ 默认码元集 a-z 不含任何非字母字符 ⇒ 恒不命中，与历史逐键等价（零回归）。
        if let Some(act) = self.try_code_char_gate(&mut state, data) {
            return act;
        }

        // 以词定字（select_char）：配置的成对标点键从当前高亮候选词逐字上屏（对齐 Go
        // handleEngineDefault——select_char 优先于翻页键，故置于 apply_session_action 之前）。默认
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
        if let Some(act) = self.apply_session_action(&mut state, data, true) {
            return act;
        }

        // 数字小键盘 —— direct（默认）：IME 不把该键解释为选词，但**已打的码不丢**：先顶屏当前
        // 高亮候选（含逐步转换的已转换前缀），再接着输出该小键盘字符。
        // follow_main 时键已在 handle_key_event 入口归一化为主键盘等价键，永不到达此处。
        if let Some(npc) = numpad_char(data.key_code) {
            // 命令候选顶屏 → 执行命令（与按空格一致），不上屏 display 标签、不追加该字符。
            if let Some(act) = self.top_commit_command_guard(&mut state) {
                return act;
            }
            let has_comp = !state.input_buffer.is_empty()
                || !state.committed_text.is_empty()
                || !state.candidates.is_empty();
            return self.commit_highlight_then_char(&mut state, npc, has_comp);
        }

        // ── z-fallback 夺取：**必须早于下面的按键分派** ──
        //
        // 缓冲以 z 开头、加上这一键后 `z…` 破活码前缀 ⇒ 首 z 实为引导键，抛弃它、
        // 残余码切进目标模式（见 `try_z_fallback`，内含全部门禁：码表引擎 / z 有绑定 /
        // 目标接得住这个字符 / 破前缀）。
        //
        // ★ 放在 match **之前**而不是各臂里：数字键在缓冲非空时是选词键、符号走标点
        // 流水线，两条都会当场把键消费掉——夺取判定挂在臂里就永远轮不到。原先只挂在
        // 字母臂上，于是 `z = "mix:quick_mix"` 的用户「进了快捷输入却算不了数」，而同一个
        // mix 用 `;` 进就正常（`;` 首键直接进模式，之后所有键都归 mix 处理）。
        //
        // 单点而非三处各接一次：这仓已多次栽在「N 条通路只接了 N-1 条」上
        // （见 project_mixed_overflow_vs_topcode）。
        if data.modifiers & (MOD_CTRL | MOD_ALT) == 0 {
            let probe = if (keymap::VK_A..=keymap::VK_Z).contains(&data.key_code) {
                Some((b'a' + (data.key_code - keymap::VK_A) as u8) as char)
            } else if (keymap::VK_0..=keymap::VK_9).contains(&data.key_code) {
                Some((b'0' + (data.key_code - keymap::VK_0) as u8) as char)
            } else {
                punct_char(data.key_code, data.modifiers & MOD_SHIFT != 0)
            };
            if let Some(ch) = probe
                && let Some(act) = self.try_z_fallback(&mut state, ch)
            {
                return act;
            }
        }

        match data.key_code {
            // Escape：取消整个组合（含已转换前缀），不上屏。实现收口在 `cancel_session`
            // ——`keys.session_actions` 里绑 `cancel` 的键走的是同一个函数，两条通路
            // 行为必然一致。
            keymap::VK_ESCAPE => self.cancel_session(&mut state),
            keymap::VK_BACK => {
                // Backspace：分步撤销——有已转换段则先把最后一段退回拼音（你→ni，码并回剩余
                // 缓冲前部、重转），否则删光标前一个字符。
                // 段回退**优先于光标**（不看光标位置，对齐 Go handleBackspace 的分支顺序）。
                if !state.committed_segs.is_empty() {
                    self.pop_committed_seg(&mut state)
                } else if !state.input_buffer.is_empty() {
                    let st = &mut *state;
                    let deleted = preedit_cursor::BufEdit::new_cased(
                        &mut st.input_buffer,
                        &mut st.input_cursor_pos,
                        &mut st.input_buffer_cased,
                    )
                    .backspace();
                    if !deleted {
                        // 缓冲非空但光标已在最左：吃掉不透传，否则宿主会删到组合区之前的正文。
                        KeyAction::Consumed
                    } else {
                        self.update_candidates(&mut state);
                        if state.input_buffer.is_empty() {
                            self.notify_ui_hide();
                            KeyAction::ClearComposition
                        } else {
                            let display = state.preedit.clone();
                            let caret_pos = self.composition_caret(&state);
                            self.notify_ui_update(&state);
                            KeyAction::UpdateComposition {
                                caret_pos,
                                text: display,
                            }
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
                    // 上屏的是**用户所打的形态**：Shift+字母的大写存在影子串里，缓冲恒小写。
                    let raw_code = preedit_cursor::cased_or_buffer(
                        &state.input_buffer,
                        &state.input_buffer_cased,
                    )
                    .to_string();
                    // 上屏剩余拼音原码：prefix(committed) 段已在选词时记过，此处只记 input_buffer 避免重复。
                    self.record_commit(
                        &raw_code,
                        raw_code.len() as u32,
                        -1,
                        CommitSource::RawInput,
                    );
                    let mut text = self.maybe_s2t(&state, &format!("{}{}", prefix, raw_code));
                    // 英文补空格（`schema.english.commit_space`）：本分支上屏的是**输入缓冲
                    // 原码**（词库里没有的自造词），无候选可依，故用方案口径
                    // `english_space_enabled` 而非候选口径。与选中候选补空格一致——两者都是
                    // 「一个英文词打完了」，行为分叉才是意外。
                    //
                    // ⚠️ 下方 VK_RETURN 分支代码与本块**逐行同形**，但**刻意不补**：回车是
                    // 终结性动作（多伴随换行/提交意图），语义与「接着打下一个词」相反。改这里
                    // 时别顺手把那边也改了。
                    if self.english_space_enabled() {
                        text.push(' ');
                    }
                    state.input_buffer.clear();
                    state.input_buffer_cased.clear();
                    state.candidates.clear();
                    self.notify_ui_hide();
                    Self::commit_action(text, true)
                } else {
                    // 空缓冲空格：经标点流水线转换（自定义映射「空格」行四态可覆盖；
                    // 内建默认仅全角态转全角空格 U+3000，对齐设置端展示基线与微软拼音）。
                    // 流水线原样返回 " " 时（半角态无自定义）维持透传，保留宿主对
                    // 空格键的原生语义（如网页滚动）。
                    let text = self.convert_punct(&state, ' ', data.prev_char);
                    if text == " " {
                        return KeyAction::PassThrough;
                    }
                    self.record_commit(&text, 0, -1, CommitSource::Punctuation);
                    Self::commit_action(text, true)
                }
            }
            keymap::VK_RETURN => {
                // Enter：按 enter_behavior 配置（对齐 Go handleEnter）——"clear" 清空编码
                // (不上屏)；否则(commit)上屏「已转换前缀 + 剩余原码」。
                if !state.input_buffer.is_empty() || !state.committed_text.is_empty() {
                    if self.enter_clears_composition() {
                        state.committed_text.clear();
                        state.committed_segs.clear();
                        state.input_buffer.clear();
                        state.candidates.clear();
                        self.notify_ui_hide();
                        return KeyAction::ClearComposition;
                    }
                    let prefix = self.take_committed(&mut state);
                    // 上屏的是**用户所打的形态**：Shift+字母的大写存在影子串里，缓冲恒小写。
                    let raw_code = preedit_cursor::cased_or_buffer(
                        &state.input_buffer,
                        &state.input_buffer_cased,
                    )
                    .to_string();
                    // 上屏剩余拼音原码：prefix(committed) 段已在选词时记过，此处只记 input_buffer 避免重复。
                    self.record_commit(
                        &raw_code,
                        raw_code.len() as u32,
                        -1,
                        CommitSource::RawInput,
                    );
                    // ⚠️ 本块与上方 VK_SPACE 空码分支逐行同形，唯一差别是**不补英文空格**
                    // （`schema.english.commit_space`）：回车是终结性动作，多伴随换行/提交
                    // 意图，与空格「接着打下一个词」的语义相反。这是刻意的不对称，不是漏接。
                    let text = self.maybe_s2t(&state, &format!("{}{}", prefix, raw_code));
                    state.input_buffer.clear();
                    state.input_buffer_cased.clear();
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
                if state.candidates.is_empty()
                    && state.input_buffer.is_empty()
                    && state.committed_text.is_empty()
                {
                    let digit = (b'0' + num as u8) as char;
                    // 全角：C++ 为此专门在无 session 时也吃数字（`chinese_fullwidth_number`
                    // 分支），故必须出字——透传会「吃了再吐」→ 严格 TSF 宿主丢键、宽松宿主出
                    // 半角（旧行为：1-9 各应用表现不一，而 `0` 因无此臂落标点流水线反而正常）。
                    // 走完整流水线而非裸 to_full_width，与 `0`/小键盘/CapsLock 各路径一致。
                    if state.full_width {
                        let text = self.convert_punct(&state, digit, data.prev_char);
                        self.record_commit(&text, 0, -1, CommitSource::Punctuation);
                        return Self::commit_action(text, true);
                    }
                    // 半角无候选：透传，纯数字键由宿主出字（保留原生按键语义）。
                    // 对齐 Go：recordCommit(key, 0, -1, SourcePunctuation) 后再 return nil。
                    self.record_commit(&digit.to_string(), 0, -1, CommitSource::Punctuation);
                    return KeyAction::PassThrough;
                }
                self.handle_number_key_select(&mut state, num)
            }
            keymap::VK_0
                if data.modifiers & MOD_SHIFT == 0
                    && !(state.candidates.is_empty()
                        && state.input_buffer.is_empty()
                        && state.committed_text.is_empty()) =>
            {
                // 数字键 0 选当前页第 10 个候选（对齐通行约定 0=第10；越界按
                // overflow.number_key 处理）。follow_main 归一化后小键盘 0 走此臂，与主键盘一致。
                // 空缓冲下的 0 不进此臂（guard 排除）→ 落兜底标点流水线，保持全角态输出全角 ０
                // 及自定义标点映射——0 曾靠「不在数字选词臂、落兜底」才正确，见 fullwidth 修复。
                self.handle_number_key_select(&mut state, 10)
            }
            keymap::VK_A..=keymap::VK_Z => {
                // A-Z 字母累积。缓冲恒存小写：z-fallback 探针、顶码判定、引擎查询、词频记账
                // 全部只看它，大小写对匹配零影响。
                let ch = (b'a' + (data.key_code - 0x41) as u8) as char;
                // Shift+字母的大写只进影子串，供组合区显示与「上屏原码」还原用户所打的形态
                // （打 `aBC` 回车得 `aBC`）。CapsLock 在中文输入流里到不了这一步——上面
                // `state.caps_lock` 分支已整段接管，故此处只需判 Shift。
                let raw = if data.modifiers & MOD_SHIFT != 0 {
                    ch.to_ascii_uppercase()
                } else {
                    ch
                };
                // 注：z-fallback 夺取已上移到 match **之前**统一处理（数字/符号臂同样需要它，
                // 而那两条会当场消费掉按键）。故此处不再调用。
                //
                // 非码元字母（如 `input_chars = "a-x"` 下的 y/z）：不进缓冲，终结组合并出字。
                //
                // ★ **必须在 z-fallback 之后**。z 常同时是「非码元」（a-x 方案）与
                // 「临时拼音触发键」，若先判非码元，z 会被当成普通字符顶上屏，临拼永远
                // 进不去——同理，空缓冲下的模式激活在更上游的 try_activate_mode 已处理完。
                // 上移之后这条顺序仍然成立（夺取在 match 前，更早）。
                //
                // 默认码元集 a-z 下本判定恒不命中，与历史逐键等价（零回归）。
                if !self.can_enter_buffer(&state, ch) {
                    return self.reject_non_code_char(&mut state, raw);
                }
                self.accumulate_code_char(&mut state, ch, raw)
            }
            keymap::VK_LEFT | keymap::VK_RIGHT | keymap::VK_HOME | keymap::VK_END => {
                // 编码区光标移动（对齐 Go handleCursorLeft/Right/Home/End 的三态语义）：
                // ① 无组合 → 透传，宿主照常移动文档光标；② 有剩余编码 → 编码区内移动；
                // ③ 已在边界 / 只剩只读的已转换前缀 → 吃掉不透传（否则宿主光标会跳出组合区）。
                // 左右键若被用户配成翻页/高亮键，上面的 apply_session_action 已先行拦截，走不到这里
                // ——「配了别的功能」即等价于放弃光标移动。
                if state.input_buffer.is_empty() {
                    if state.committed_text.is_empty() {
                        KeyAction::PassThrough
                    } else {
                        KeyAction::Consumed
                    }
                } else {
                    let st = &mut *state;
                    let mut ed = preedit_cursor::BufEdit::new(
                        &mut st.input_buffer,
                        &mut st.input_cursor_pos,
                    );
                    let moved = match data.key_code {
                        keymap::VK_LEFT => ed.move_left(),
                        keymap::VK_RIGHT => ed.move_right(),
                        keymap::VK_HOME => ed.home(),
                        _ => ed.end(),
                    };
                    if moved {
                        // 光标移动**不重算候选**（不调 update_candidates）：光标不参与引擎查询，
                        // 候选与 preedit 文本均不变，只是 caret 位置变了。但仍须 notify_ui_update
                        // ——自绘编码栏要据新 caret 重画插入符（"不重算候选" ≠ "不刷新 UI"）。
                        let display = state.preedit.clone();
                        let caret_pos = self.composition_caret(&state);
                        self.notify_ui_update(&state);
                        KeyAction::UpdateComposition {
                            caret_pos,
                            text: display,
                        }
                    } else {
                        KeyAction::Consumed
                    }
                }
            }
            keymap::VK_DELETE => {
                // 前删（删光标后一个字符，光标不动）。与 Backspace 刻意不对称：Backspace 一上来
                // 就回退已转换段，Delete 只删剩余编码、不碰前缀（对齐 Go handleDelete）。
                if state.input_buffer.is_empty() {
                    if state.committed_text.is_empty() {
                        KeyAction::PassThrough
                    } else {
                        KeyAction::Consumed
                    }
                } else {
                    let st = &mut *state;
                    let deleted = preedit_cursor::BufEdit::new_cased(
                        &mut st.input_buffer,
                        &mut st.input_cursor_pos,
                        &mut st.input_buffer_cased,
                    )
                    .delete();
                    if !deleted {
                        // 光标已在末尾，前方无字符可删。
                        KeyAction::Consumed
                    } else if state.input_buffer.is_empty() && !state.committed_segs.is_empty() {
                        // 剩余编码被删空但仍有已转换段：回退最后一段（对齐 Go handleDelete）。
                        self.pop_committed_seg(&mut state)
                    } else {
                        self.update_candidates(&mut state);
                        if state.input_buffer.is_empty() {
                            self.notify_ui_hide();
                            KeyAction::ClearComposition
                        } else {
                            let display = state.preedit.clone();
                            let caret_pos = self.composition_caret(&state);
                            self.notify_ui_update(&state);
                            KeyAction::UpdateComposition {
                                caret_pos,
                                text: display,
                            }
                        }
                    }
                }
            }
            keymap::VK_UP | keymap::VK_DOWN | keymap::VK_PRIOR | keymap::VK_NEXT => {
                // 方向/翻页键回退臂：有候选时翻页/高亮已由上面的 apply_session_action（配置驱动）处理，
                // 这里只剩"无候选"情形——无组合则透传给应用，有组合则消费。
                if state.input_buffer.is_empty() && state.committed_text.is_empty() {
                    KeyAction::PassThrough
                } else {
                    KeyAction::Consumed
                }
            }
            keymap::VK_QUOTE | keymap::VK_BACKTICK
                if data.modifiers & MOD_SHIFT == 0
                    && !state.input_buffer.is_empty()
                    && self.pinyin_separator_key(data.key_code) =>
            {
                // 拼音手动音节分隔符：把 `'` 压入缓冲作硬边界（引擎按 `'` 强制切分、查询前剥除、
                // preedit 原样保留含末尾 `'`）。走与字母键一致的候选刷新路径。
                // 置于选词/标点分派（`_` 臂）之前：分隔符模式下该键优先作分隔符而非三选键——
                // auto 模式仅在 `'` 未被占作选择键时才拦截 `'`（见 pinyin_separator_key）。
                {
                    let st = &mut *state;
                    preedit_cursor::BufEdit::new(&mut st.input_buffer, &mut st.input_cursor_pos)
                        .insert('\'');
                }
                match self.update_candidates(&mut state) {
                    InputOutcome::AutoCommit(text) => {
                        // 记账码取首候选（按来源分流，见 `freq_code`），与上一处 AutoCommit 同口径。
                        let (source, code) = state
                            .candidates
                            .first()
                            .map(|c| (c.source, self.freq_code(&state.input_buffer, c)))
                            .unwrap_or_else(|| {
                                (CandidateSource::default(), state.input_buffer.clone())
                            });
                        let out = self.commit_candidate(&mut state, &text, None, source, &code);
                        self.notify_ui_hide();
                        return Self::commit_action(out, true);
                    }
                    // 含副作用命令自动命中：与空格选中命令同路（清组合 + 异步执行）。
                    InputOutcome::AutoCommand(cand) => {
                        return self.commit_command(&mut state, &cand);
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
                let caret_pos = self.composition_caret(&state);
                self.notify_ui_update(&state);
                KeyAction::UpdateComposition {
                    caret_pos,
                    text: display,
                }
            }
            _ => {
                let shift = data.modifiers & MOD_SHIFT != 0;
                // 触发键优先级链（对齐 Go decideBufferedTrigger，缓冲非空/有候选时）：
                if !shift {
                    // B/C. 二/三候选键 + 候选足够 → 选候选
                    //
                    // ★ 双拼韵母键（微软/搜狗/紫光的 `;` = ing）**到不了这里**：它们已由
                    // 上游的非字母码元闸门 `try_code_char_gate` 接管进缓冲。此处原有一段
                    // `is_shuangpin_final` 局部避让，只做到「跳过选词」而没人接住那个键——
                    // 它接着流到 D0 的模式引导键（`;` 出厂绑 quick_mix）和下方标点流水线，
                    // 于是 `ing` 韵母仍旧打不出。三条拦截通路只挡了一条，是典型的半截修复。
                    // 现由码元集单点仲裁（拼音引擎的 `input_chars` 从双拼布局推导）。
                    let mut select_overflow: Option<char> = None;
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
                        // 仅在有 input session 时才标记越界；空缓冲+空候选（完全空闲态）
                        // 应回落到下方普通标点流程，否则 ' / ; 在中文空闲模式下永远被吞。
                        if !state.input_buffer.is_empty() || !state.candidates.is_empty() {
                            select_overflow = punct_char(data.key_code, false);
                        }
                    }
                    // D0. 方案级按键功能表（`[key_actions]`）先于全局引导键裁决。
                    //
                    // ★ 这是进模式的**第二条通路**（顶字 + 进模式），与空缓冲的
                    // `try_activate_mode` 并列。两条都必须接同一个裁决，否则方案里写的
                    // `none` 只挡得住一条——空码按 `;` 会被这里接管，表现为「禁用没生效」。
                    // 本臂的模式触发判定不要求缓冲非空，故空码同样走到这里。
                    match self.bound_key_decision(data.key_code) {
                        crate::handle_lifecycle::BoundKeyDecision::Act(action) => {
                            if let Some(act) = self.commit_and_enter_bound_action(
                                &mut state,
                                &action,
                                data.key_code,
                            ) {
                                return act;
                            }
                            // 门卫没过：不吞键，落普通流程（与空缓冲进入同策略）。
                        }
                        // 让位：跳过下面全部模式触发判定，落普通流程。
                        crate::handle_lifecycle::BoundKeyDecision::Yield => {}
                        crate::handle_lifecycle::BoundKeyDecision::NotBound => {
                            // D. 模式触发键 → 顶屏高亮候选 + 进模式。
                            // 特殊模式引导键（判定顺序对齐空缓冲时 handle_lifecycle：special 先于
                            // mix）——方案不可加载则不拦截，落普通流程（与空缓冲进入同守卫）。
                            // 传真实 key_code → 组合区写引导符，与空缓冲进入一致。
                            if let Some(act) =
                                self.try_global_trigger_commit_enter(&mut state, data)
                            {
                                return act;
                            }
                        }
                    }
                    // E. 次/三选键越界且非模式触发键 → 按 input.overflow.select_key 处理
                    if let Some(ch) = select_overflow {
                        return self.handle_overflow_select_key(&mut state, ch, data.prev_char);
                    }
                }
                if let Some(ch) = punct_char(data.key_code, shift) {
                    // 快照 held_text：非参与集合的标点会在 try_smart_symbol_replace 中解除武装
                    // 并清空 held_text，须在此前保存，以便下方普通标点流程将旧符号纳入 CommitText。
                    // 加超时防护：若 arm.at 已超出 timeout，说明 C++ timer 已自然触发提交，
                    // held_text 已过期——不再使用，防止二次提交（"。" → 等待 >500ms → "=" → "。。="）。
                    let pre_held_text = {
                        let arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
                        let timeout = self.smart_symbol_timeout();
                        let still_in_window =
                            arm.at.map(|t| t.elapsed() < timeout).unwrap_or(false);
                        if still_in_window {
                            arm.held_text.clone()
                        } else {
                            None
                        }
                    };
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
                            // 码表/混输：读有效码表配置（全局 schema.codetable + 方案 override）。
                            _ => self.engine_mgr.codetable_settings().punct_commit,
                        };
                        if !punct_commit {
                            return KeyAction::Consumed;
                        }
                        // HoldComposition + has_input：arm 已设 hold_pending_commit，
                        // 顶屏上屏候选后开 HoldComposition 放入中文标点。
                        let hold_info = {
                            let arm = self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
                            if arm.armed && arm.hold_pending_commit {
                                Some((
                                    arm.str.clone(),
                                    self.smart_symbol_timeout().as_millis() as u32,
                                ))
                            } else {
                                None
                            }
                        };
                        if let Some((hold_text, timeout_ms)) = hold_info {
                            // 命令候选顶屏 → 执行命令（与按空格一致），不走智能符号 Hold。
                            if let Some(act) = self.top_commit_command_guard(&mut state) {
                                return act;
                            }
                            let committed = self.take_committed(&mut state);
                            let mut commit_text = self.maybe_s2t(&state, &committed);
                            if !state.candidates.is_empty() {
                                let (start, _) = self.page_range(&state);
                                let idx =
                                    (start + state.selected_index).min(state.candidates.len() - 1);
                                let cand = state.candidates[idx].clone();
                                // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
                                let freq_code = self.freq_code(&state.input_buffer, &cand);
                                self.record_selection(&freq_code, &cand.text, cand.source);
                                self.record_commit(
                                    &cand.text,
                                    state.input_buffer.len() as u32,
                                    (idx - start) as i32,
                                    CommitSource::Candidate,
                                );
                                commit_text.push_str(&self.cand_s2t_text(&state, &cand));
                            } else if !state.input_buffer.is_empty() {
                                // 无候选顶屏的是原码 → 同回车，用用户所打的大小写形态。
                                commit_text.push_str(preedit_cursor::cased_or_buffer(
                                    &state.input_buffer,
                                    &state.input_buffer_cased,
                                ));
                            }
                            state.input_buffer.clear();
                            state.candidates.clear();
                            {
                                let mut arm =
                                    self.smart_symbol.lock().unwrap_or_else(|e| e.into_inner());
                                arm.held_text = Some(hold_text.clone());
                                arm.hold_pending_commit = false;
                            }
                            self.record_commit(&hold_text, 0, -1, CommitSource::Punctuation);
                            self.notify_ui_hide();
                            return KeyAction::CommitAndHoldComposition {
                                commit_text,
                                hold_text,
                                timeout_ms,
                            };
                        }
                    }
                    // 命令候选顶屏 → 执行命令（与按空格一致），不上屏 display 标签、不追加标点。
                    if let Some(act) = self.top_commit_command_guard(&mut state) {
                        return act;
                    }
                    // 标点/符号键：先上屏已转换前缀 + 首选候选（若有输入），再追加（转换后的）标点
                    let committed = self.take_committed(&mut state);
                    let mut out = self.maybe_s2t(&state, &committed);
                    // 若此前有 HoldComposition 残留（非参与集合标点令 arm 解除武装），
                    // 将旧符号纳入 out 首部：CommitText 原子替换 TSF 组合态，timer 被 CancelHoldTimer
                    // 取消，旧符号不会二次提交，也不会因组合态被覆盖而丢失。
                    if let Some(ref held) = pre_held_text {
                        out = format!("{}{}", held, out);
                    }
                    if !state.candidates.is_empty() {
                        let (start, _) = self.page_range(&state);
                        let idx = (start + state.selected_index).min(state.candidates.len() - 1);
                        let cand = state.candidates[idx].clone();
                        // 记账码：码表按输入码（码位独立），拼音/英文按候选码。见 `freq_code`。
                        let freq_code = self.freq_code(&state.input_buffer, &cand);
                        self.record_selection(&freq_code, &cand.text, cand.source);
                        // 标点上屏前先记被顶出的高亮候选（来源候选）。
                        self.record_commit(
                            &cand.text,
                            state.input_buffer.len() as u32,
                            (idx - start) as i32,
                            CommitSource::Candidate,
                        );
                        out.push_str(&self.cand_s2t_text(&state, &cand));
                    } else if !state.input_buffer.is_empty() {
                        // 无候选顶屏的是原码 → 同回车，用用户所打的大小写形态。
                        out.push_str(preedit_cursor::cased_or_buffer(
                            &state.input_buffer,
                            &state.input_buffer_cased,
                        ));
                    }
                    let had_input = !state.input_buffer.is_empty()
                        || !state.candidates.is_empty()
                        || !committed.is_empty();
                    state.input_buffer.clear();
                    state.candidates.clear();

                    // CapsLock + 无待提交内容：TSF 层应已透传此键，coordinator 不应收到；
                    // 防御性兜底——直接透传让系统产生原始 WM_KEYDOWN + WM_CHAR。
                    if state.caps_lock && !had_input {
                        return KeyAction::PassThrough;
                    }

                    // 标点单点流水线：自定义映射 > 数字后智能 > 中文标点 > 全半角。
                    // CapsLock 开时大写语义等同英文模式，临时关闭中文标点转换。
                    let saved_chinese_punct = state.chinese_punct;
                    if state.caps_lock {
                        state.chinese_punct = false;
                    }
                    // 引号交替态钉左：开了配对后一次按键即产出完整一对，交替开关不参与决策。
                    let quote_paired = self.pin_quote_left_if_paired(&state, ch);
                    let piece = self.convert_punct(&state, ch, data.prev_char);
                    state.chinese_punct = saved_chinese_punct;
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
                        // 智能跳过：仅无候选前缀（out 即标点本身）时，输右括号→光标右移。
                        // 引号一律不走此路（`quote_paired` 中文引号 / `*l != *r` 对称英文引号）：
                        // 对称配对的按键不携带开/闭这一位，
                        // 无从判断用户想跳出还是想嵌套新的一对，故取消右符号处理、跳出交给跳出键。
                        // 非对称配对（括号类）则由 `right_symbol` 开关决定是否跳出。
                        if out == piece
                            && !quote_paired
                            && self.rt().jump_out_on_right_symbol
                            && pairs.iter().any(|(l, r)| *r == pch && *l != *r)
                        {
                            let mut tr =
                                self.pair_tracker.lock().unwrap_or_else(|e| e.into_inner());
                            // 同 handle_punct：多字符右段配不上单个标点按键，只能 Tab/Enter 跳出。
                            if tr.peek().is_some_and(|e| e.right_is_char(pch)) {
                                tr.pop();
                                return KeyAction::MoveCursorRight { count: 1 };
                            }
                            tr.clear();
                        }
                        // 插入配对：左括号 → 补右括号，光标置于其间
                        if let Some((_, right)) = pairs.iter().find(|(l, _)| *l == pch).copied() {
                            self.push_pair(pch, right);
                            // 右引号已由本次配对补出，交替开关不该停在「右」——否则一旦中途
                            // 关掉配对，遗留的右态会让下一个引号直接出闭引号。
                            if quote_paired {
                                self.punct
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .pin_quote_left(ch);
                            }
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
        // 与 handle_focus_lost 的 token 日志配对：只有两边都记 token，才能从日志算出
        // 「同一实例 gained 后多久自己 lost」——区分 DocMgr 抖动与真实离开就靠这个间隔。
        tracing::debug!(
            "handle_focus_gained: token={:#x} scope={:#x}",
            data.client_token,
            data.input_scope_mask
        );
        // 切进新的可编辑上下文同样是「用户动了别处」。⚠️focus_gained **没有任何去重**
        // （每次 DocMgr 获焦都发一条，Excel 同一 DocMgr 6ms 抖动、VSCode 一次切换 5 次都会
        // 各发一条），全靠 menu_close_on_focus_change 的守卫期挡住刚弹出的菜单。
        self.menu_close_on_focus_change("focus_gained");
        // 焦点 caret 走与同步段同一个入口。**不要在这里直写 `state.caret_*`**——重型段晚于
        // 同步段执行，直写会把同步段的 height 守卫与 caret_use_top 变换整个覆盖掉。
        // 详见 apply_focus_caret 的文档注释。
        self.apply_focus_caret(
            &CaretData {
                x: data.x,
                y: data.y,
                height: data.height,
                composition_start_x: data.composition_start_x,
                composition_start_y: data.composition_start_y,
                source: data.caret_source,
            },
            "handle_focus_gained",
        );
        // 组合起点锚定作废：焦点事件意味着**换了 docMgr**。组合本身可能还在（buffer 未清），
        // 但它的宿主位置可能整体迁移——Excel 输入时会在「单元格」与「公式编辑栏」两个 docMgr
        // 之间来回切，实测组合从 (593,572) 迁到 (1457,959)。而锚定「同一组合只锁一次、之后
        // 不再更新」的隐含前提正是**起点不会移动**，这里恰好证伪。
        //
        // 不作废的后果是候选窗钉死在旧 docMgr 上：协调器拿 state.caret_* 判出 reshow，下发时
        // 却用锁死的组合起点，日志上表现为「reshow: dx=1297 说要重定位，UI pos 却纹丝不动」。
        // 清掉后由下一帧 caret_update 就地重锁，候选窗跟到新位置。
        *self
            .composition_start
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (0, 0, false);
        // 坐标缓存作废（同上一段的理由，只是作用在另一个消费者上）：刚写进 state 的那份
        // 是**焦点事件随包携带**的坐标，宿主此刻多半还没 reflow，甚至根本还没建好新文档的
        // 编辑上下文（Excel 实测 454ms）。它够格当"没有更好选择时的兜底显示位置"，但不够格
        // 让 fast 档判定"可以跳过等待了"。
        self.caret_cache_verified
            .store(false, std::sync::atomic::Ordering::Relaxed);
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 焦点进入文本框 = 本输入法激活（对齐 Go HandleFocusGained → SetIMEActivated(true)）。
            // 不依赖 IME_ACTIVATED 的到达时机，确保工具栏在焦点到达时即可显示。
            state.ime_active = true;
            // DLL 只对「有可编辑上下文」的 DocMgr 发 focus_gained（无上下文走 NoEditCtx
            // 分支），故收到本命令即等价于"焦点在可编辑控件里"。这是 has_edit_context
            // 唯一的置真路径之一，另一处是 handle_ime_activated 的兜底。
            state.has_edit_context = true;
        }
        // 撤销上屏计数复位：进入新文本框，光标前是新上下文，下次 undo 退化删 1
        // （首次聚焦无配对 focus_lost 时，本处兜底）。
        self.last_commit_len
            .store(1, std::sync::atomic::Ordering::Relaxed);
        // 配对状态归属校验（防御性）：配对栈是全局单栈、不分宿主，栈顶有可能是别的宿主压的。
        // 真实失焦已在 handle_focus_lost 清过栈，能活到这里的只有 CtxLost 噪声，故本校验
        // 正常不触发；留着是因为成本为零，且「全局单栈」这个事实没变。
        self.clear_pair_tracker_if_foreign(data.client_token);
        // 记录活动客户端：鼠标点击的 commit 只推给它，避免广播多发
        if data.client_token != 0 {
            self.push_server.set_active_token(data.client_token);
        }
        // 解析焦点进程的 caret 兼容态（微信 caret_use_top 等）。本段为 FOCUS_GAINED 的重型
        // 后置段（DLL 阻塞响应已写出），同步 OpenProcess 不影响首键延迟。
        // ⚠ 必须在 update_active_compat **之前**取旧值：该函数会整体覆写 active_compat，
        // 跑完之后读到的已是新进程的规则，「切换前那个应用有没有初始规则」就永远取不到了。
        // 漏掉这点不会编译报错、不会 panic，只表现为「从规则应用切出去后模式不恢复」。
        let new_pid = (data.client_token >> 32) as u32;
        // macOS：宿主名只能由 `.app` 告知（服务进程的 `process_name` 恒空）。必须**先于**
        // update_active_compat 落进缓存，否则那边读到空名 → compat 规则匹配不上、per-app
        // 记忆表查不到，整条按应用链路静默退化成全局行为。Windows 恒为空串，不进此分支。
        if !data.bundle_id.is_empty() && new_pid != 0 {
            self.pid_names
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(new_pid, data.bundle_id.to_lowercase());
        }
        let (old_pid, old_has_rule) = {
            let ac = self.active_compat.lock().unwrap_or_else(|e| e.into_inner());
            (ac.pid, ac.has_initial_rule)
        };
        self.update_active_compat(data.client_token);
        let new_has_rule = self
            .active_compat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_initial_rule;
        // per-app 状态：进程名已入缓存，按规则表/记忆表/默认值切换本应用中英状态。若与同步段
        // get_current_mode 回传值不同（该进程首次聚焦），随后的 push_activation_status 推送修正。
        //
        // 两个条件的分工：
        //   crossed      焦点**跨进程**切入才重算。同应用内的焦点跳转（Everything 的搜索框
        //                ↔ 结果列表）不重算，否则用户手切的模式会被反复拉回初始值——这正是
        //                「初始值」与「锁定」的分界线。
        //   per_app / has_rule
        //                per_app_scope 是既有的按应用记忆语义；has_rule 把 compat.toml 规则的
        //                影响严格限制在**进出规则应用**这一步。判据若退化成「规则表非空」，
        //                则任意两个应用之间的切换都会重算，global+remember=false（出厂默认）下
        //                会把用户在 Word 手切的英文在切到 Chrome 时重置掉，与规则应用无关。
        //
        // 取舍：per_app_scope 下同进程重复 focus_gained 不再重算（此前每次都算）。记忆表由
        // record_app_mode 与当前状态保持同步，重算结果恒等于现值，故语义无变化；代价是失去了
        // 一条隐式的 compartment 脏事件自愈路径，该自愈在 IME_ACTIVATED 路径仍然保留。
        let crossed = new_pid != 0 && old_pid != new_pid;
        if should_reapply_initial(
            crossed,
            self.rt().config.input.default.per_app_scope(),
            old_has_rule,
            new_has_rule,
        ) {
            self.apply_initial_mode(data.client_token, false);
        }
        let status = self.build_status();
        self.push_activation_status(data.client_token);
        self.notify_toolbar_async(); // 激活态 → 工具栏显示（异步，避免 is_foreground_fullscreen 阻塞 bridge 线程）
        self.show_persistent_status_if_always(); // 常驻模式:获焦即显示状态
        // ui.status.show_on_focus：切到新宿主时提示一次。按 client_token 去重——同一宿主内换
        // docMgr（Excel 单元格 ↔ 公式栏）不重复弹，见 last_focus_tip_token。
        self.show_focus_status_if_enabled(data.client_token);
        let pid = (data.client_token >> 32) as u32;
        self.apply_input_diag(pid, data.disabled, data.reason, data.input_scope_mask);
        Some(status)
    }

    fn handle_focus_lost(&self, client_token: u64, reason: FocusLostReason) {
        // 独立日志行：失焦此前在服务端日志里完全不可见，只能靠 TSF 日志反推 HideToolbar
        // 的来源（2026-07-26 工具栏闪隐排查即因此多绕一圈）。token 便于与 DLL 日志的
        // `Sending focus_lost token=…` 对齐到具体宿主实例。
        tracing::debug!(
            "handle_focus_lost: token={:#x} reason={:?}",
            client_token,
            reason
        );
        // ★★ CapsLock 钩子闸门兜底归零，**先于 stale 判定**：钩子是全局的，闸门若因任何
        // 疏漏滞留在 true，用户切到别的应用后按 CapsLock 就完全失灵——这是本功能唯一
        // 会伤到「没在用输入法的时刻」的故障方向，必须在最宽的路径上归零。
        // 与 menu_close 同理放在 stale 判定之前：陈旧失焦同样证明用户动了别处。
        wind_keys::capslock_hook::set_should_eat(false);
        // 关菜单**先于** stale 判定与 reason 分流：菜单的生命周期与输入态无关，
        // 陈旧失焦/噪声层失焦同样证明用户动了别处。详见 menu_close_on_focus_change。
        self.menu_close_on_focus_change("focus_lost");
        if self.is_stale_focus_event(client_token, "handle_focus_lost") {
            return;
        }
        // 三项后果彼此独立，由 reason 决定各自是否发生（矩阵见 FocusLostReason）。
        // 一刀切地全做，就是 CtxLost 清输入态复发「首字符直接上屏」的由来；
        // 一刀切地全不做，就是应用内点到非文本框工具栏永不隐藏的由来。
        let clears_input = reason.clears_input();
        // 词频已即时写入 redb（事务持久），失焦无需再落盘。
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if reason.clears_ime_active() {
                // 整个应用失去前台。用户开启系统“为每个应用窗口使用不同输入法”时，切到用
                // 别的输入法的应用不会触发 IME_DEACTIVATED，只有 FocusLost。工具栏隐藏经
                // UI 层 50ms 防抖——紧接着若有 FocusGained 会取消隐藏，无闪烁。
                s.ime_active = false;
                // 真正离开了这个宿主 ⇒ 焦点气泡的去重记录作废，下次再进来该重新提示一次。
                // **只在这一档清**：CtxLost/DocChanged 是宿主内部换 docMgr 的噪声，清了就等于
                // 按 docMgr 计数，Excel 下又会变回「输入一次闪两下」。
                *self
                    .last_focus_tip_token
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = 0;
            }
            if reason.clears_edit_context() {
                // 焦点不在可编辑控件里了 → 工具栏隐藏。DocChanged 不走这里：换文档后
                // 由随后的 focus_gained（可编辑）或 NoEditCtx（不可编辑）重新定夺。
                s.has_edit_context = false;
            }
            if clears_input {
                // 焦点切换后旧 composition 上下文已失效，清理输入态，避免候选残留到新焦点。
                s.input_buffer.clear();
                s.preedit.clear();
                s.candidates.clear();
                // 复位菜单态，否则下一个键被 forward_menu_key 吞掉。
                // **本处刻意不受 MENU_FOCUS_GUARD 保护**：下面的 notify_ui_hide 会经
                // HideCandidates 无条件隐藏菜单窗口，此时若把 menu_open 留成 true，就成了
                // 「窗口没了、键还被吞」的状态不一致——比守卫失效更糟。
                s.menu_open = false;
                s.menu_opened_at = None;
                self.reset_exclusive_modes(&mut s); // 失焦丢弃临时英文/拼音/快捷输入残留
            }
        }
        if clears_input {
            // 失焦即清配对状态。**曾尝试按 reason 细分保留**（弹框夺走前台时光标其实还在
            // 括号中间），2026-07-29 真机后放弃：配对状态存在 core 全局单栈与**每个宿主进程
            // 各自一份**的 DLL 计数两处，而开启「为每个应用配置不同输入法」后切换应用会让
            // 整个 IME 上下文重建；更根本的是焦点离开期间用户做了什么（点走光标、删掉括号）
            // 输入法完全无法感知，保留状态本质上是猜测。实测「大部分情况不行」——
            // 一个大部分情况下失效的功能比没有更糟，用户拍板放弃。
            //
            // 注意「同一焦点内」的陈旧风险与本项无关，仍由 state_ttl_secs 兜底。
            self.clear_pair_tracker();
            // 撤销上屏计数复位：换窗/换文本框后光标前已非「刚上屏那段」，下次 undo 退化删 1。
            self.last_commit_len
                .store(1, std::sync::atomic::Ordering::Relaxed);
            // 失焦即清抑制态：密码框失焦到下次 focus_gained 之间无控件收键，suppress 残留虽不
            // 可利用，但属状态卫生隐患——独立 atomic，无锁依赖，不与上面的 state 锁冲突。
            self.password_suppress
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        // 工具栏可见性无论哪种 reason 都要重算：ime_active 与 has_edit_context 任一变化都影响它。
        self.notify_toolbar_async(); // 防抖，异步避免阻塞 bridge 线程
        if clears_input {
            self.notify_ui_hide(); // 隐藏候选窗 + 弹出菜单（HideCandidates 连带关菜单）
            self.hide_tip(); // 失焦隐藏状态提示（常驻模式尤需）
            self.terminate_auto_phrase("focus_lost"); // 换窗口 = 一段输入结束
        }
        // CtxLost 刻意不碰候选窗：输入态还在（Excel 抖动保护），候选窗应跟随输入态而非
        // 焦点。真正离开时随后的 DocChanged / Thread 会收口。
    }

    fn get_current_mode(&self, client_token: u64) -> (bool, bool) {
        // FocusGained 同步路径回传 ModePush：DLL 正同步阻塞等本值，仅允许锁+HashMap 查询，
        // 严禁 OpenProcess 等跨进程调用。
        // 命中规则表 / 记忆表 → 先切到目标状态再回传，消除首键竞态；都未命中（进程首次聚焦
        // 且无规则）保持现状，由重型段 handle_focus_gained 修正。
        //
        // 本段早于重型段的 update_active_compat，此刻 active_compat.pid 仍是**上一个**进程，
        // 正好用来判断本次是否跨进程切入。同应用内跳转不重算，理由同 handle_focus_gained：
        // 否则用户手切的模式会在换个输入框时被拉回初始值。
        let new_pid = (client_token >> 32) as u32;
        let crossed = new_pid != 0
            && self
                .active_compat
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pid
                != new_pid;
        if crossed {
            let proc = self.cached_proc_name(client_token);
            if !proc.is_empty() {
                // 规则表优先于记忆表，与 `initial_chinese_mode_for` **必须同序**：两处顺序不
                // 一致会让同步回传值与重型段的落地值不同，表现为首键按 A 上屏、随后被 push 成 B。
                let target = self
                    .rule_initial_mode(&proc)
                    .map(|m| m.is_chinese())
                    .or_else(|| {
                        if self.rt().config.input.default.per_app_scope() {
                            self.mode_states
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get(&proc)
                                .copied()
                        } else {
                            None
                        }
                    });
                let rule_punct = self.rule_initial_punct(&proc);
                if target.is_some() || rule_punct.is_some() {
                    let follow = self.rt().config.input.punct.follow_mode;
                    let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(chinese) = target
                        && s.chinese_mode != chinese
                    {
                        s.chinese_mode = chinese;
                        if follow {
                            s.chinese_punct = chinese;
                        }
                    }
                    // 与 apply_initial_mode 同序：显式标点规则最后落地，压过 follow 推导。
                    if let Some(p) = rule_punct {
                        s.chinese_punct = p.is_chinese();
                    }
                    return (s.chinese_mode, s.full_width);
                }
            }
        }
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (s.chinese_mode, s.full_width)
    }

    fn handle_ime_activated(&self, client_token: u64) -> Option<StatusUpdateData> {
        if client_token != 0 {
            self.push_server.set_active_token(client_token);
        }
        // 切回本输入法时同样刷新焦点进程的 caret 兼容态（异步段，不阻塞 DLL）。
        self.update_active_compat(client_token);
        // 激活初始状态矩阵：remember=false 重置为配置默认（含全半角/标点）；
        // remember=true 保持全局记忆；state_scope="app" 恢复该应用的会话记忆。
        // 同时构成对 compartment 脏事件污染的自愈兜底（详见 TextService.cpp 门卫修复）。
        self.apply_initial_mode(client_token, true);
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.ime_active = true;
            // 兜底置真：宿主主动激活本输入法，通常意味着焦点已进入输入框。
            // 若某些宿主 IME_ACTIVATED 之后不补发 focus_gained，而这里不置位，
            // has_edit_context 将永远停在 false —— 工具栏再也不显示。
            // 该字段的失效方向不对称：多显示只是碍眼，永不显示是功能失效，故取宽松侧。
            s.has_edit_context = true;
        }
        let status = self.build_status();
        self.push_activation_status(client_token);
        self.notify_toolbar_async(); // 激活态 → 工具栏显示（异步，避免 is_foreground_fullscreen 阻塞 bridge 线程）
        self.show_persistent_status_if_always(); // 常驻模式:激活即显示状态
        Some(status)
    }

    fn handle_ime_deactivated(&self, client_token: u64) {
        tracing::debug!("handle_ime_deactivated: token={:#x}", client_token);
        // 同 handle_focus_lost：关菜单先于 stale 判定。下面清 menu_open 的那段仍保留
        // （非陈旧路径的完整清理），两处幂等叠加无副作用。
        self.menu_close_on_focus_change("ime_deactivated");
        // 与 focus_lost 同源的乱序风险：切走本输入法时旧宿主的 IME_DEACTIVATED 同样可能
        // 晚于新宿主的 focus_gained 到达（两者都是 fire-and-forget 异步写）。
        if self.is_stale_focus_event(client_token, "handle_ime_deactivated") {
            return;
        }
        // 切走本输入法（换到别的 IME / 非输入法应用）：清激活态、清输入、隐藏全部 UI。
        // 对齐 Go SetIMEActivated(false)（隐藏工具栏 + hideUI），根治“切走仍残留显示”。
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.ime_active = false;
            s.has_edit_context = false; // 切走本输入法：谈不上焦点在不在可编辑控件里
            s.input_buffer.clear();
            s.preedit.clear();
            s.candidates.clear();
            s.menu_open = false;
            s.menu_opened_at = None;
            self.reset_exclusive_modes(&mut s); // 切走本输入法时丢弃独占模式残留
        }
        self.notify_toolbar_async(); // 非激活态 → notify_toolbar 内部下发 HideToolbar（异步）
        self.notify_ui_hide(); // 隐藏候选窗 + 弹出菜单
        self.hide_tip(); // 切走本输入法隐藏状态提示
        self.terminate_auto_phrase("ime_deactivated"); // 切走输入法 = 一段输入结束
    }

    fn handle_mode_notify(&self, flags: u32) {
        let chinese_mode = (flags & wind_ipc::protocol::STATUS_CHINESE_MODE) != 0;
        let clear_input = (flags & wind_ipc::protocol::STATUS_MODE_CHANGED) != 0;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.chinese_mode = chinese_mode;
            if clear_input {
                state.input_buffer.clear();
                state.candidates.clear();
                self.reset_exclusive_modes(&mut state); // 系统模式切换时丢弃独占模式残留
            }
        }
        self.record_app_mode(chinese_mode);
        self.record_last_state();
    }

    fn handle_toggle_mode(&self) -> (Option<StatusUpdateData>, String) {
        // 「切换模式时取消大小写锁定」：CapsLock 开时按切换键，语义是"回到可输入中文
        // 的状态"（对齐搜狗）——取消锁定并归位中文，而非翻转 chinese_mode；否则
        // chinese_mode 原本为 true（被 CapsLock 压制）时翻转反而落到英文，切换仍然无效。
        let caps_cancelled = self.cancel_caps_on_switch();
        // 中英切换 = 一段输入结束。须在取 state 锁之前调用：terminate_auto_phrase 内部
        // 走词库 IO，不可在持 state 锁时进行。
        self.terminate_auto_phrase("toggle_mode");
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = if caps_cancelled {
            true
        } else {
            !state.chinese_mode
        };
        let chinese = state.chinese_mode;
        // 标点随中英文切换（对齐 Go）：开启 punct_follow_mode 时，标点中/英跟随当前模式。
        if self.rt().config.input.punct.follow_mode {
            state.chinese_punct = chinese;
        }
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese);
        drop(state);
        self.record_app_mode(chinese);
        self.record_last_state();
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.disarm_smart_symbol();
        // 配对栈**刻意不清**：中英切换既不移动光标也不消除已插入的右符号，「光标紧贴右符号」
        // 这个前提仍然成立，清掉只会让用户切走再切回后 Tab/Enter 跳不出去。真正让前提失效的
        // 是失焦与组合被终止，那两处仍清（见 clear_pair_tracker 的其余调用点）。
        // C++ 侧同源：模式切换路径调 ResetComposingState(TRUE) 保留 _pairPendingDepth，
        // 否则中文模式下 Enter 会被会话门控挡在 DLL 里，根本到不了这里。
        self.push_state_update();
        self.show_status();
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
        (Some(self.build_status()), commit_text)
    }

    fn handle_system_mode_switch(&self, chinese_mode: bool) -> (Option<StatusUpdateData>, String) {
        // 「切换模式时取消大小写锁定」：目标模式由外部指定（Ctrl+Space/KBLSwitch），
        // 仅取消 CapsLock 让目标模式真正生效，不改写目标。
        let _ = self.cancel_caps_on_switch();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.chinese_mode = chinese_mode;
        // 标点随中英文切换（对齐 Go）：开启 punct_follow_mode 时，标点跟随模式。
        if self.rt().config.input.punct.follow_mode {
            state.chinese_punct = chinese_mode;
        }
        let commit_text = self.take_input_on_mode_switch(&mut state, chinese_mode);
        drop(state);
        self.record_app_mode(chinese_mode);
        self.record_last_state();
        self.punct.lock().unwrap_or_else(|e| e.into_inner()).reset();
        self.disarm_smart_symbol();
        // 配对栈刻意不清，理由同 handle_toggle_mode。
        self.push_state_update();
        self.show_status(); // 与 Shift 切换（handle_toggle_mode）统一：Ctrl+Space/外部切换也显示中/英提示
        self.notify_toolbar();
        self.notify_ui_hide(); // 取消输入：隐藏候选窗
        (Some(self.build_status()), commit_text)
    }

    fn handle_composition_terminated(&self) {
        // SearchHost.exe / 开始菜单等受限宿主：搜索框不支持 TSF composition，
        // DLL 每次设置 composition 后宿主立即终止，属伪终止事件。
        // Rust 版无 last_key_time 竞态窗口（对照 Go handle_lifecycle.go:559-572），
        // host-render 激活时直接忽略清缓冲动作以保留输入状态与候选，
        // 下一按键的 UpdateComposition 会自动重建 composition。
        // （host_render_active() 仅在 active 连接已通过白名单 setup 时为 true，
        //   不会误伤白名单外的普通宿主。）
        if self.host_render_active() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // 必须整体复位（含 active/temp_pinyin_*/mix_* 等 overlay 状态），不能只清 input_buffer：
        // 临时拼音/快捷输入的缓冲与前缀不在 input_buffer 里，只清后者会让模式残留——
        // 真机现象：` 进临拼后点鼠标移光标，候选窗随 notify_ui_hide 消失但模式还在，
        // 再按 d 仍走 handle_temp_pinyin_key，组合区诡异地显示 `d。
        // reset_exclusive_modes 内含 disarm_smart_symbol 与强制竖排布局恢复。
        // 此回调仅在 TSF 意外终止组合时触发（焦点切换、宿主强制 EndComposition 等）；
        // 我们自己的 CommitText 不触发（_pComposition 已提前置 nullptr，走"Already released"分支）。
        // 因此在此 disarm 是安全的：意外中断必然使 HoldComposition 失效，旧 held_text 不可再用。
        self.reset_exclusive_modes(&mut state);
        // 复位菜单状态：点击别处会终止 composition 并经 notify_ui_hide 隐藏菜单窗口，
        // 但若不清 menu_open，下一个键会被 forward_menu_key 当作菜单键吞掉（首字符失效）。
        state.menu_open = false;
        drop(state);
        self.clear_pair_tracker(); // 组合意外终止：配对上下文失效，清栈防跳出键误判
        self.notify_ui_hide();
    }

    fn handle_caret_update(&self, data: &CaretData) {
        // compStart 必须打：它是「本轮 composition 的 reflow 坐标是否已到」的唯一判据
        // （compStart=(0,0) ⇒ 该帧来自 idle 更新，组合还没建立/还没 reflow），也是
        // coords_ready 逃生口与嵌入模式定位锚点的来源。此前只打 x/y/h，查候选窗定位问题时
        // 必须去翻 TSF 日志对时间戳才能补上这一维。
        tracing::debug!(
            "handle_caret_update: x={} y={} h={} compStart=({},{}) src={}",
            data.x,
            data.y,
            data.height,
            data.composition_start_x,
            data.composition_start_y,
            wind_ipc::protocol::caret_source::name(data.source)
        );
        // height==0：宿主尚未 reflow，GetTextExt 返回退化矩形，坐标不可靠 → 跳过（不更新缓存、
        // 不触发显示），等 OnLayoutChange 后的有效坐标（对齐 Go HandleCaretUpdate）。
        if data.height == 0 {
            return;
        }
        // 应用兼容规则 caret_use_top（对齐 Go HandleCaretUpdate 的 rect.bottom→rect.top）：
        // 微信等 WebView 的 GetTextExt 返回 height 不稳定（1↔20px），rect.bottom 随之漂移 ~20px，
        // 但 rect.top 始终稳定（≤1px，≈正文底端）。改用 top 定位：Y -= height，使候选窗下方显示
        // 锚在稳定的 top（wind-ui 下方公式 = caret_y + gap，不读 height，故下方不受 height 影响）。
        //
        // 关键：height 不能压成 1。上方显示时 wind-ui 用 caret_top = caret_y - height 推算正文顶端
        // （above 底边 = caret_y - height - gap）；若 height=1 则正文顶端被当成 top-1（≈正文底端），
        // 候选窗会整条压住正文/光标。故保留真实行高 raw_h，并对退化帧（raw_h=1）取下限兜底，
        // 让上方显示正确避让正文（偏大只是多留空隙，偏小才会遮挡——宁大勿小）。
        // 组合起点 Y 同步上移以保持锚点一致。后续逻辑全部基于变换后的本地副本。
        let mut data = *data;
        self.apply_caret_compat(&mut data);
        let data = &data;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (prev_x, prev_y) = (state.caret_x, state.caret_y);
        state.caret_x = data.x;
        state.caret_y = data.y;
        state.caret_height = data.height;
        state.caret_source = data.source;
        let now_valid =
            !(data.x == 0 && data.y == 0) && data.x.abs() < 32000 && data.y.abs() < 32000;
        if !now_valid {
            debug!("caret_update → 丢弃: 坐标无效（(0,0) 哨兵或越界）");
            return;
        }
        // 消费焦点气泡的挂起：DLL 在焦点路径拿不到同步锁时会异步补一条权威坐标，这就是它。
        // **必须在下面的 `composing` 闸门之前**——焦点刚到达时用户还没输入，`composing` 恒 false，
        // 放在闸门之后等于永远不执行（而且完全静默）。
        //
        // 只认 TSF 域：本闸门存在的全部意义就是不拿 GUI 回退坐标定位气泡。
        if self
            .pending_focus_tip
            .load(std::sync::atomic::Ordering::Relaxed)
            && wind_ipc::protocol::caret_source::is_tsf(data.source)
        {
            self.pending_focus_tip
                .store(false, std::sync::atomic::Ordering::Relaxed);
            debug!(
                "focus_tip → 补显示: 等到权威坐标 ({},{}) src={}",
                data.x,
                data.y,
                wind_ipc::protocol::caret_source::name(data.source)
            );
            // 先放锁再显示：show_tip 内部要重新取 state 锁读坐标，持锁调用会自死锁。
            drop(state);
            self.show_tip(&self.status_indicator_text());
            state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        }
        let composing = !state.candidates.is_empty() || !state.input_buffer.is_empty();
        if !composing {
            // 常态、非异常：上屏后到下一键之间宿主仍会上报 caret。注意坐标**已在上面写入
            // state.caret_x/y**，只是不做显示决策——这一条解释了「按键前明明收到过正确坐标，
            // 候选窗却还在等 reflow」，是排查首显延迟时最容易看漏的一环。
            debug!("caret_update → 仅更新缓存: 无组合（无候选且缓冲空），不做显示决策");
            return;
        }
        // 组合起点锚定：同一组合只接受首个有效 compStart，后续即便携带新值也不覆盖（防部分控件
        // GetRange 让起点随输入漂移，致候选窗随输入右移）。500px 校验排除 logical/physical 坐标系不一致。
        {
            let mut cs = self
                .composition_start
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !cs.2 && (data.composition_start_x != 0 || data.composition_start_y != 0) {
                let dx = (data.composition_start_x - data.x).abs();
                let dy = (data.composition_start_y - data.y).abs();
                // ⚠ 500px 校验的前提是**两者同源**——它想抓的是「同一个 context 报出的两个坐标
                // 却相差离谱」这种坐标系不一致。当 caret 本身来自 GUI 回退等非 TSF 通道时，
                // 它和组合起点压根不是一个语义域，比较毫无意义。桌面输入实测：caret=(0,1388)
                // 是任务栏残留的 Win32 光标、compStart=(473,217) 才是真实组合位置，dy=1171
                // 让这道闸门把**唯一正确的数据**当异常丢弃了。
                // 故非 TSF 源直接采信组合起点——此时它比 caret 可信得多。
                if !wind_ipc::protocol::caret_source::is_tsf(data.source)
                    && data.source != wind_ipc::protocol::caret_source::UNKNOWN
                {
                    *cs = (data.composition_start_x, data.composition_start_y, true);
                    debug!(
                        "组合起点锁定: ({},{})（跳过距离校验：caret 源={} 非 TSF，与组合起点不同源）",
                        data.composition_start_x,
                        data.composition_start_y,
                        wind_ipc::protocol::caret_source::name(data.source)
                    );
                } else if dx < 500 && dy < 500 {
                    *cs = (data.composition_start_x, data.composition_start_y, true);
                    debug!(
                        "组合起点锁定: ({},{})（本组合内不再更新；coords_ready 逃生口据此成立）",
                        data.composition_start_x, data.composition_start_y
                    );
                } else {
                    debug!(
                        "组合起点丢弃: ({},{}) 距 caret dx={dx} dy={dy} ≥500px（疑 logical/physical 坐标系不一致，caret 源={}）",
                        data.composition_start_x,
                        data.composition_start_y,
                        wind_ipc::protocol::caret_source::name(data.source)
                    );
                }
            }
        }
        // 记录本帧为「上一轮权威坐标」，供下一轮组合的试探采样做判据。
        // 放在这里（已过有效性与 composing 守卫）而非函数入口：只有真正被采纳为定位依据的
        // 坐标才有资格当基准，否则会把 idle 帧、退化帧混进来，判据立刻失真。
        *self
            .last_authoritative_caret
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (data.x, data.y, true);
        // 同一条「够格」判据的第二个消费者：坐标缓存自此对应当前插入点，fast 档的短兜底
        // 可以放心拿它首显（见 caret_cache_verified 的字段注释）。
        self.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 消费首显等待：本次为 reflow 后权威坐标。
        let was_pending = {
            let mut pfs = self
                .pending_first_show
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let w = *pfs;
            *pfs = false;
            w
        };
        if was_pending {
            // 延迟的首次显示：用本权威坐标无条件首显（不过滤）。
            debug!("caret_update → 首显: 消费 pending_first_show，本帧作权威坐标");
            self.show_authorized
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.notify_ui_update(&state);
        } else if *self
            .candidate_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            // 已显示后的坐标更新：≤3px 微移跳过 reshow（吞掉宿主 caret 微调，如 WPS 的 2px 偏移）；
            // 显著变化（换行 / reflow 修正）才 reshow，由 UI 层 4px 位置阈值再次过滤微移。
            let dx = (data.x - prev_x).abs();
            let dy = (data.y - prev_y).abs();
            // 首显用过非权威坐标时，本轮**第一次**权威坐标改用放宽的容差：偏差在
            // 「行高 × settle_ratio」以内就不校正。抖动的观感来自校正动作本身而非坐标偏差
            // ——十几像素的偏移用户根本不会注意，跳一下却很显眼（多数输入法也这么处理）。
            // 换行/重排的偏差通常 ≥2 个行高，远超此阈值，仍会正常校正。
            let settle = if self
                .first_show_was_provisional
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                let ratio = self.rt().config.ui.candidate.first_show_settle_ratio;
                let h = data.height.max(state.caret_height).max(1) as f32;
                (h * ratio.max(0.0)) as i32
            } else {
                0
            };
            let tol = settle.max(3); // 常规微移过滤下限保持 3px 不变
            if dx <= tol && dy <= tol {
                debug!("caret_update → 忽略: 微移 dx={dx} dy={dy}（≤{tol}px，不 reshow）");
                return;
            }
            debug!("caret_update → reshow: dx={dx} dy={dy}");
            self.show_authorized
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.notify_ui_update(&state);
        } else {
            // 隐式出口：既没在等首显、候选窗也没显示着。此前这里静默结束，日志上与
            // 「首显」「reshow」无从区分——查「候选窗为什么没出现」时这一条最要紧，
            // 因为它说明本帧坐标到了但没有任何一方消费它。
            debug!("caret_update → 无动作: 未等待首显且候选窗未显示，本帧仅落缓存");
        }
    }

    /// focus_gained 随包携带的 caret：只更新坐标缓存，**不做任何显示决策**。
    ///
    /// 焦点事件带来的坐标是「切换发生的那一刻」的值，宿主可能还没 reflow。若把它交给
    /// [`Self::handle_caret_update`]，会被当成 reflow 后的权威坐标消费掉首显等待，候选窗
    /// 就在中间位置先显示一次再跳到最终位置。Excel 单元格激活实测三段坐标：
    /// 1025,687（选中态）→ **1369,1036（焦点事件，非权威）** → 1590,1092（reflow 后）。
    ///
    /// caret_use_top 变换要照做——坐标缓存本身必须与 handle_caret_update 写入的口径一致，
    /// 否则首键前的兜底坐标会和后续更新差一个行高。
    fn handle_focus_gained_caret(&self, data: &CaretData) {
        self.apply_focus_caret(data, "handle_focus_gained_caret");
    }

    fn handle_caret_probe(&self, data: &CaretData) {
        // 首帧 reflow 期间 DLL 逐次采样上报（CMD_CARET_PROBE）。默认**完全忽略**——
        // 不开 fast_first_show 的宿主必须保持「等 reflow 权威坐标」的原行为，一字不差。
        let compat = *self.active_compat.lock().unwrap_or_else(|e| e.into_inner());
        if compat.first_show_mode != wind_config::app_compat::FirstShowMode::Fast {
            debug!(
                "caret_probe → 忽略: 当前档位={} 非 fast",
                compat.first_show_mode.as_config()
            );
            return;
        }
        // 只在正等首显时有意义：已显示 / 未 arm 的帧交给常规 caret_update 路径。
        if !*self
            .pending_first_show
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            debug!("caret_probe → 忽略: 未在等待首显（已首显过 / 未 arm）");
            return;
        }
        // 退化 rect（无高度）一律不采信：实测 WPS 首帧曾采到 top==bottom 的样本，
        // 其 x 与真实位置差 1687px，采信即大幅错位。
        if data.height <= 0 {
            debug!("caret_probe → 丢弃: 退化 rect（h<=0）");
            return;
        }
        // ★ 首帧信任门（第二条通路）：坐标缓存未经当前插入点验证时，本函数下面两条判据
        // **全都失去判断力**，必须一起让位给长兜底。
        //
        // 判据 1 靠「≠ 上一轮权威坐标」推断「宿主已 reflow」，其成立前提是那个基准与当前
        // 插入点**可比**。焦点刚切换时基准属于另一个单元格/文档/应用，probe 值当然不等于
        // 它 ⇒ 判据恒成立 ⇒ 必然采信一个还没 reflow 的坐标。判据 2（连打快路径）同理：
        // 跨焦点的"上一次按键间隔"说明不了当前这一帧的坐标可信。
        //
        // ⚠ 实测（2026-08-03 Excel）：闸门刚 arm 了 600ms 长兜底，6ms 后 probe 就用
        // (1299,535) 抢先首显，而 200ms 后真坐标是 (1344,744) ⇒ 显示后跳一次。
        // **信任门只接在兜底 timer 上是不够的——首显有多条通路，否决判据必须每条都接。**
        if self.first_show_needs_long_wait() {
            debug!(
                "caret_probe → 继续等待: 坐标缓存未经当前插入点验证，本轮判据无基准可比（x={} y={}）",
                data.x, data.y
            );
            return;
        }
        // 快路径：连续快速输入时直接采信首条采样，不再比对上一轮权威坐标。
        // 依据是连打时光标沿同一行顺序前移、不发生重排，坐标本来就八九不离十；而这种节奏下
        // 用户对「跟手」的敏感度远高于十几像素的偏差。窗口可经
        // ui.candidate.fast_typing_window_ms 调整，0 = 关闭本快路径。
        let fast_window = self.rt().config.ui.candidate.fast_typing_window_ms;
        if fast_window > 0 {
            let interval = *self
                .last_key_interval_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ms) = interval
                && ms <= fast_window
            {
                debug!(
                    "caret_probe → 提前首显(按键间隔 {ms}ms≤{fast_window}ms): x={} y={}",
                    data.x, data.y
                );
                self.first_show_was_provisional
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.handle_caret_update(data);
                return;
            }
        }
        // 判据：与上一轮权威坐标不同 ⇒ 宿主已 reflow ⇒ 本帧可信。
        // 尚无上一轮基准时（焦点刚到达的首次输入）直接采信：此时没有「旧值」可疑。
        let (lx, ly, has_base) = *self
            .last_authoritative_caret
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if has_base && data.x == lx && data.y == ly {
            debug!("caret_probe → 继续等待: 坐标仍等于上一轮权威 ({lx},{ly})，宿主尚未 reflow");
            return;
        }
        debug!(
            "caret_probe → 提前首显: x={} y={} h={}（基准 ({lx},{ly}) has_base={has_base}）",
            data.x, data.y, data.height
        );
        // 复用权威路径：更新坐标缓存 + 消费等待 + 首显。若判错，随后到达的真权威坐标
        // 会经 handle_caret_update 按放宽后的容差决定是否校正。
        self.first_show_was_provisional
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle_caret_update(data);
    }

    fn handle_caret_pending(&self) {
        // DLL 新组合在 reflow 完成前发来的"坐标待定"握手（_compositionJustStarted）：
        // 仅当正等待首显时，延长兜底超时到 600ms，避免 OnLayoutChange burst 慢的应用（如 EverEdit）
        // 在真实坐标到达前被 150ms 兜底用旧坐标抢先显示。
        if !*self
            .pending_first_show
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            return;
        }
        // fast 档刻意不接受这次延长：它的短兜底就是为「坐标要 60~190ms 才到」的宿主设计的，
        // 延到 600ms 等于把 fast 重新变回 wait（而组合往往活不到 100ms，兜底根本不会到期）。
        //
        // ⚠ 唯独坐标缓存不可信时**不能**在这里提前放弃延长：那种情况下短兜底会走
        // `fire_pending_first_show` 的首帧信任门自行延长，两处口径必须一致，否则表现为
        // 「握手到得早就短兜底、到得晚反而正确」这种随 IPC 时序摇摆的行为。
        //
        // ⚠ 坐标缓存不可信时 fast 档同样**不在这里**延长：那种情况的等待时长已由
        // `arm_pending_first_show` 的首帧信任门决定（且刻意不因后续事件重置）。握手若也插
        // 一脚就成了第二个真相源，表现为「握手到得早就长等、到得晚就短兜底」这种随 IPC
        // 时序摇摆的行为。
        if self.first_show_mode_is_fast() {
            debug!("caret_pending → 忽略延长: fast 档兜底时长在 arm 时已按坐标可信度定");
            return;
        }
        self.arm_pending_first_show_with_timeout(FIRST_SHOW_LONG_FALLBACK_MS);
    }

    /// 宿主报告「光标移动且当前无 composition」（C++ `TextService::OnEndEdit`，守卫
    /// `selChanged && _pComposition == nullptr`）。
    ///
    /// 这是码表自动造词**唯一能感知到「用户敲了空格/回车结束一句」的途径**：码表每选一字
    /// 就上屏并关闭 composition，此后 Space/Enter 被 TSF 直接透传给宿主，协调器根本收不到
    /// 按键（`KeyEventSink.cpp:398/966/1024` —— Backspace/Enter/Escape 仅在有 composition
    /// 或 input session 时才拦截）。
    ///
    /// # 自提交宽限期
    ///
    /// 本输入法自己提交文字后，宿主插入文本同样导致光标移动 → 同样回送本事件，且在协议层
    /// **与用户真实光标移动完全无法区分**，只能靠时间判别。若不区分，每上屏一个字就被自己
    /// 的回声判成「用户移动光标」→ flush → 缓冲永远只有 1 个字 → 造词恒不触发。
    ///
    /// 宽限值取 [`SELF_COMMIT_GRACE`]，已由真机日志校准（见该常量注释的实测分布）。
    ///
    /// 回声分支**不做任何动作**，故只记 TRACE：它的频率恒等于上屏频率（每上屏一个字必有
    /// 一条），放在 DEBUG 会把真正有信息量的「用户移动光标 → 终止序列」淹掉。需要重新
    /// 校准 `SELF_COMMIT_GRACE` 时开 TRACE 即可拿回完整分布。
    fn handle_selection_changed(&self, _prev_char: u16) {
        let since = self
            .last_self_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|t| t.elapsed());
        let is_echo = since.is_some_and(|d| d < SELF_COMMIT_GRACE);
        if is_echo {
            trace!("selection_changed: since_self_commit={since:?} → 自提交回声，忽略");
            return;
        }
        debug!("selection_changed: since_self_commit={since:?} → 用户移动光标");
        // 坐标缓存随之过期：用户在同一 DocMgr 内点到了别处（不发 focus_gained），而宿主
        // 只在有 composition 时才回送 caret_update，所以缓存里仍是上次输入的位置。fast 档
        // 若拿它给下一次输入的首帧定位，候选窗会先出现在旧位置再跳过来。
        // ★ 复用本判据是安全的：它的两个误判方向对本用途都不致命——误判成回声只是维持
        //   现状（不比现在差），误判成移动只是让下一次首显多等一程（慢而不错）。
        self.caret_cache_verified
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.terminate_auto_phrase("selection_changed");
    }

    fn handle_commit_request(&self, data: &CommitRequestData) -> Option<CommitResultData> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.input_buffer.is_empty() {
            return None;
        }
        let tk = data.trigger_key as u32; // 协议为 u16，统一按 VK(u32) 比对
        // 取上屏文本、来源与记账码：命中候选取候选 source，退回原码分支为 None（不可归因）。
        // 记账码按来源分流（见 `freq_code`）——码表按输入码、拼音/英文按候选码；退回原码的
        // 分支上屏的就是缓冲本身，无候选可依，用输入码。
        let cand_meta = |c: &Candidate| {
            (
                c.text.clone(),
                c.source,
                self.freq_code(&state.input_buffer, c),
            )
        };
        let raw = || {
            (
                state.input_buffer.clone(),
                CandidateSource::None,
                state.input_buffer.clone(),
            )
        };
        // ⚠️ 这是一条**独立于按键路径的上屏通路**（DLL 侧 TSF 排水 / 顶码延迟提交发起），
        // 补空格必须在此单独接线——只改 `commit_selected` 会得到「键盘空格补了、排水路径没补」
        // 的间歇性不一致。第四元 `append_space` 按分支显式给出，不在末尾统一判断：四个分支
        // 的答案各不相同，统一判断迟早把它们抹平。
        let (text, source, freq_code, append_space) = if tk == keymap::VK_SPACE {
            match state.candidates.first() {
                // 空格选首选：候选口径（与 `commit_selected` 同）。
                Some(c) => {
                    let (t, s, f) = cand_meta(c);
                    let ap = self.english_appends_space(s);
                    (t, s, f, ap)
                }
                // 空格退回原码：无候选可依，方案口径（与 VK_SPACE 空码分支同）。
                None => {
                    let (t, s, f) = raw();
                    (t, s, f, self.english_space_enabled())
                }
            }
        } else if tk == keymap::VK_RETURN {
            // 回车恒不补：终结性动作，与按键路径的 VK_RETURN 分支同口径。
            let (t, s, f) = raw();
            (t, s, f, false)
        } else if (keymap::VK_1..=keymap::VK_9).contains(&tk) {
            match state.candidates.get((tk - keymap::VK_1) as usize) {
                // 数字键选词：候选口径（「所有选中方式一律补」）。
                Some(c) => {
                    let (t, s, f) = cand_meta(c);
                    let ap = self.english_appends_space(s);
                    (t, s, f, ap)
                }
                // 数字键越界退回原码：**不补**。按键路径下此情形走
                // `handle_overflow_number_key`，候选为空时直接吞键不上屏——本分支是 DLL 侧
                // 独有的兜底，没有对应的键盘行为可对齐，保守不补。
                None => {
                    let (t, s, f) = raw();
                    (t, s, f, false)
                }
            }
        } else {
            // 未知触发键：不可归因，不补。
            let (t, s, f) = raw();
            (t, s, f, false)
        };
        state.input_buffer.clear();
        state.candidates.clear();
        // 与 handle_key_event 的选词路径保持一致：记录词频用于学习排序
        self.record_selection(&freq_code, &text, source);
        // 补空格**必须在记账之后**：`record_selection` 记的是词本身，带上尾空格会写出
        // 「hello 」这种与读取端（`apply_freq_rerank` 按候选文本查）永远对不上的词频键。
        let text = if append_space {
            format!("{text} ")
        } else {
            text
        };
        // 上屏即组合结束：复位首显延迟状态，使下一组合首帧重新延迟到 reflow 后的权威坐标，
        // 避免其锁定到本组合旧坐标（"上屏后立即输入候选窗错位"主场景）。
        self.reset_first_show();
        Some(CommitResultData {
            barrier_seq: data.barrier_seq,
            text,
            new_composition: String::new(),
            mode_changed: false,
            chinese_mode: state.chinese_mode,
        })
    }

    fn handle_host_render_failed(&self, reason: u32) {
        // DLL 侧 host-render 初始化/映射失败：记录告警。后续（Task 6/7）可据此回退渲染路径。
        warn!("host-render 失败上报 reason={reason}（DLL 退回进程内渲染）");
    }

    fn handle_input_state_report(&self, pid: u32, disabled: bool, reason: u8, mask: u64) {
        self.apply_input_diag(pid, disabled, reason, mask);
    }

    fn handle_diag_snapshot(&self, snap: &wind_ipc::protocol::DiagSnapshotPayload) {
        self.apply_diag_snapshot(snap);
    }
}

/// 对 SystemPhraseEntry 列表做稳定内容哈希（用于启动时判断 TOML 是否有变更）。
/// 使用标准库 DefaultHasher，无新依赖。
fn phrase_entries_hash(entries: &[wind_phrase::SystemPhraseEntry]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    entries.len().hash(&mut h);
    for e in entries {
        e.code.hash(&mut h);
        e.text.hash(&mut h);
        e.weight.hash(&mut h);
        e.position.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// 悬停调试段的方案归属上下文（每次候选推送解析一次，镜像 `apply_freq_rerank` 的方案解析）。
struct DebugSchemaCtx {
    /// 是否混输方案（英文/码表/拼音候选各归子方案）。
    is_mixed: bool,
    /// 非混输统一方案 id（拼音族折叠为 "pinyin"）。
    schema: String,
    /// 混输码表子方案 id（英文候选亦归此）。
    ct_id: Option<String>,
    /// 混输拼音子方案 id。
    py_id: Option<String>,
}

impl Coordinator {
    /// 解析调试归属上下文。`schema_override` = 生效方案（特殊模式），`None` 用 active。
    fn build_debug_schema_ctx(&self, schema_override: Option<&str>) -> DebugSchemaCtx {
        use wind_candidate::CandidateSource as S;
        let active = schema_override
            .map(str::to_string)
            .unwrap_or_else(|| self.engine_mgr.active_schema_id());
        let is_mixed = self.engine_mgr.schema_engine_type(&active).as_deref() == Some("mixed");
        let schema = self.engine_mgr.data_schema_id(&active);
        let (ct_id, py_id) = if is_mixed {
            (
                self.engine_mgr.write_data_schema_id(&active, S::CodeTable),
                self.engine_mgr.write_data_schema_id(&active, S::Pinyin),
            )
        } else {
            (None, None)
        };
        DebugSchemaCtx {
            is_mixed,
            schema,
            ct_id,
            py_id,
        }
    }

    /// 候选归属的方案 id（混输按来源取子方案，非混输取统一方案）。
    fn debug_schema_id_for(&self, c: &Candidate, ctx: &DebugSchemaCtx) -> String {
        use wind_candidate::CandidateSource as S;
        if ctx.is_mixed {
            match c.source {
                S::CodeTable | S::English => ctx.ct_id.clone().unwrap_or_default(),
                S::Pinyin => ctx.py_id.clone().unwrap_or_default(),
                _ => ctx.schema.clone(),
            }
        } else {
            ctx.schema.clone()
        }
    }

    /// 候选来源标签：短语（系统/用户 + 组/成员）优先，其次用户/临时词库，再按来源 + 方案名。
    /// 混输下英文候选归码表体系。
    fn debug_source_label(&self, c: &Candidate, ctx: &DebugSchemaCtx) -> String {
        use wind_candidate::CandidateSource as S;
        if c.is_phrase {
            let kind = if c.meta.is_system_phrase {
                "系统短语"
            } else {
                "用户短语"
            };
            if c.is_group {
                return format!("{kind}·组");
            }
            if c.phrase_template.starts_with("$SS") || c.phrase_template.starts_with("$AA") {
                return format!("{kind}·成员");
            }
            return kind.to_string();
        }
        if c.meta.is_user_dict {
            return "用户词库".to_string();
        }
        if c.meta.is_temp_dict {
            return "临时词库".to_string();
        }
        match c.source {
            S::CodeTable => format!(
                "码表·{}",
                Self::schema_display_name(&self.debug_schema_id_for(c, ctx))
            ),
            S::English => "码表·英文".to_string(),
            S::Pinyin => {
                let sid = self.debug_schema_id_for(c, ctx);
                if sid.is_empty() || sid == "pinyin" {
                    "拼音".to_string()
                } else {
                    format!("拼音·{}", Self::schema_display_name(&sid))
                }
            }
            S::Phrase => "短语".to_string(),
            S::None => "系统词".to_string(),
        }
    }

    /// 候选词频使用次数（按候选归属方案点查 redb FREQ；无 store/无记录 → 0）。
    ///
    /// 查询码走 [`Self::freq_code`]（按来源分流：拼音/英文用候选存储码，码表用输入码）。
    ///
    /// 拼音侧不能用击键缓冲——双拼 `siyr`/分隔符 `xi'an`/前缀补全下与候选码不同域，用后者
    /// 查恒 miss、显示恒 0；码表侧反过来必须用输入码，否则 `d`/`de`/`def` 三个码位串扰。
    ///
    /// 与 `apply_freq_rerank` 及写入端 `record_selection` 同口径，**三处必须一致**——
    /// 本处不同步的后果最隐蔽：调试信息显示的计数与排序实际用的那条不是同一个 key，
    /// 排查时会被它带偏。
    fn debug_freq_count(&self, c: &Candidate, input_code: &str, ctx: &DebugSchemaCtx) -> u32 {
        let Some(store) = &self.store else {
            return 0;
        };
        let sid = self.debug_schema_id_for(c, ctx);
        let code = self.freq_code(input_code, c);
        if sid.is_empty() || code.is_empty() {
            return 0;
        }
        store
            .get_freq(&sid, &code, &c.text)
            .ok()
            .flatten()
            .map(|r| r.count)
            .unwrap_or(0)
    }

    /// 候选调试信息段：`[调试]` 独占一行 + 来源行 + 合并的（编码/权重/序/词频/标记）行。
    /// 保持约 3 行；来源区分系统/用户短语、用户/临时词库、码表(方案)、拼音、英文。
    fn debug_tooltip_section(
        &self,
        c: &Candidate,
        input_code: &str,
        ctx: &DebugSchemaCtx,
    ) -> String {
        let source = self.debug_source_label(c, ctx);
        let count = self.debug_freq_count(c, input_code, ctx);
        let mut parts: Vec<String> = Vec::new();
        if !c.code.is_empty() {
            parts.push(format!("码 {}", c.code));
        }
        parts.push(format!("权 {}", c.weight));
        parts.push(format!("序 {}", c.natural_order));
        parts.push(format!("用 {count}次"));
        if c.has_shadow {
            parts.push("✎已调整".to_string());
        }
        format!("[调试]\n来源: {source}\n{}", parts.join(" · "))
    }
}

// ———————————————— 首显兜底 timer（进程内共享单线程）————————————————

/// 首显兜底的共享定时器：只保留**最近一次** arm 的待触发任务。
///
/// 此前每次 arm 都 `thread::spawn` 一个线程去 `sleep`，靠 token 让被取代的那些醒来后自行
/// 放弃——日志实测一小时创建两千余个线程。既然 token 已经保证「只有最新一次有效」，被作废
/// 的任务就没有理由继续占着线程；改成覆盖式待办后语义反而更直白：待办本身只有一个。
///
/// 本线程只做「等到点 + 回调」，**绝不在此执行可能阻塞的调用**（如前台窗口探测）——
/// 一次慢调用就会拖垮兜底的 150ms 时限。需要后台跑阻塞探测的场景另行处理。
struct FirstShowTimer {
    /// `(到期时刻, token, 协调器弱引用)`；`None` = 空闲。
    pending: Mutex<Option<(std::time::Instant, u64, std::sync::Weak<Coordinator>)>>,
    cv: std::sync::Condvar,
}

static FIRST_SHOW_TIMER: std::sync::OnceLock<Arc<FirstShowTimer>> = std::sync::OnceLock::new();

/// 取共享定时器，首次调用时懒启动其线程。
fn first_show_timer() -> &'static Arc<FirstShowTimer> {
    FIRST_SHOW_TIMER.get_or_init(|| {
        let timer = Arc::new(FirstShowTimer {
            pending: Mutex::new(None),
            cv: std::sync::Condvar::new(),
        });
        let worker = timer.clone();
        let _ = std::thread::Builder::new()
            .name("first-show-timer".into())
            .spawn(move || worker.run());
        timer
    })
}

impl FirstShowTimer {
    /// 覆盖式登记：新的 arm 直接顶掉旧的（与原先"旧线程靠 token 自行作废"等价）。
    fn arm(&self, deadline: std::time::Instant, token: u64, coord: std::sync::Weak<Coordinator>) {
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some((deadline, token, coord));
        self.cv.notify_one();
    }

    fn run(&self) {
        let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let deadline = match guard.as_ref() {
                Some((d, _, _)) => *d,
                None => {
                    // 空闲：睡到下一次 arm
                    guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
            };
            let now = std::time::Instant::now();
            if now < deadline {
                // 等待期间可能被新的 arm 顶掉，醒来后重新取 deadline 判断
                let (g, _) = self
                    .cv
                    .wait_timeout(guard, deadline - now)
                    .unwrap_or_else(|e| e.into_inner());
                guard = g;
                continue;
            }
            let Some((_, token, coord)) = guard.take() else {
                continue;
            };
            // 回调期间释放锁，否则回调里若触发新的 arm 会自锁
            drop(guard);
            if let Some(c) = coord.upgrade() {
                c.fire_pending_first_show(token);
            }
            guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        }
    }
}

#[cfg(test)]
mod first_show_timer_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn idle_timer() -> FirstShowTimer {
        FirstShowTimer {
            pending: Mutex::new(None),
            cv: std::sync::Condvar::new(),
        }
    }

    /// 覆盖式登记：这是取代「spawn 多个线程靠 token 自行作废」的等价语义——
    /// 待办任何时刻只有一个，且必须是最近一次 arm 的那个。
    #[test]
    fn arm_replaces_previous_pending() {
        let t = idle_timer();
        let dead = std::sync::Weak::<Coordinator>::new();
        let base = Instant::now();

        t.arm(base + Duration::from_secs(10), 1, dead.clone());
        t.arm(base + Duration::from_secs(20), 2, dead.clone());
        t.arm(base + Duration::from_secs(30), 3, dead);

        let g = t.pending.lock().unwrap();
        let (deadline, token, _) = g.as_ref().expect("应有待办");
        assert_eq!(*token, 3, "只应保留最近一次 arm 的 token");
        assert_eq!(
            *deadline,
            base + Duration::from_secs(30),
            "到期时刻也应随最近一次 arm 更新"
        );
    }

    /// 线程真的会在到期后回调；且协调器已释放时安全跳过（不 panic）。
    #[test]
    fn fires_after_deadline_and_tolerates_dead_coordinator() {
        let t = Arc::new(idle_timer());
        let worker = t.clone();
        std::thread::spawn(move || worker.run());

        t.arm(
            Instant::now() + Duration::from_millis(30),
            7,
            std::sync::Weak::<Coordinator>::new(), // upgrade 必失败，走"协调器已没了"分支
        );

        // 到期后待办应被取走（说明线程确实醒来处理了），且不 panic
        std::thread::sleep(Duration::from_millis(200));
        assert!(t.pending.lock().unwrap().is_none(), "到期后待办应已被消费");
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
        let b = ConfigBundle::build(cfg, &Default::default());
        assert_eq!(b.cn_pairs, vec![('（', '）'), ('【', '】')]);
        assert_eq!(b.en_pairs, vec![('(', ')')]);
    }

    #[test]
    fn parse_jump_out_keys_maps_names_to_vk() {
        // 支持的键名（大小写/空白不敏感），未知名忽略。
        let set = parse_jump_out_keys(&[
            " Tab ".into(),
            "ENTER".into(),
            "space".into(),
            "esc".into(),
            "unknown".into(),
        ]);
        assert!(set.contains(&keymap::VK_TAB));
        assert!(set.contains(&keymap::VK_RETURN)); // enter → VK_RETURN
        assert!(set.contains(&keymap::VK_SPACE));
        assert!(set.contains(&keymap::VK_ESCAPE)); // esc → VK_ESCAPE
        assert_eq!(set.len(), 4); // "unknown" 被忽略
        // "return" 别名等价 enter
        assert!(parse_jump_out_keys(&["return".into()]).contains(&keymap::VK_RETURN));
        // 空配置 → 空集（不启用）
        assert!(parse_jump_out_keys(&[]).is_empty());
    }

    #[test]
    fn config_bundle_parses_jump_out_keys() {
        let mut cfg = Config::default();
        cfg.input.auto_pair.jump_out_keys = vec!["tab".into(), "enter".into()];
        let b = ConfigBundle::build(cfg, &Default::default());
        assert!(b.jump_out_keys.contains(&keymap::VK_TAB));
        assert!(b.jump_out_keys.contains(&keymap::VK_RETURN));
        assert_eq!(b.jump_out_keys.len(), 2);
    }

    #[test]
    fn config_bundle_carries_config_values() {
        // 改配置 → 重建 bundle → bundle.config 反映新值（热重载替换后读取生效的基础）。
        let mut cfg = Config::default();
        cfg.input.symbol.smart_mode = true;
        cfg.ui.candidate.per_page = 9;
        let b = ConfigBundle::build(cfg, &Default::default());
        assert!(b.config.input.symbol.smart_mode);
        assert_eq!(b.config.ui.candidate.per_page, 9);
    }
}

#[cfg(test)]
mod mode_comment_e2e_tests {
    //! 模式级注释模板走到**发往 UI 的候选**上——决策函数 `comment::template_for` 的单元测试
    //! 证明不了消费端接上了它（本仓反复出现的「半接线」欠账）。
    //!
    //! 注释段在发送路径上算、不回写 `state.candidates`，故这里收 UI 通道断言。放在 crate 内
    //! 而非 tests/ 下，是因为要预置 caret 绕过首显闸门——headless 无宿主坐标，首帧会被
    //! `first_show` 闸门拦下不下发候选（见 `ready_coords_bypass_first_show_wait`）。
    use super::*;

    /// 造协调器并把坐标预置成「已就绪」，使候选能立即下发。
    fn coord_with_ui(cfg: Config) -> (Arc<Coordinator>, std::sync::mpsc::Receiver<UiCommand>) {
        let (c, rx) = Coordinator::new_headless_with_ui(cfg, None);
        *c.last_valid_caret.lock().unwrap() = (100, 200, 20);
        *c.composition_start.lock().unwrap() = (100, 200, true);
        (c, rx)
    }

    /// 直接驱动候选下发：造一条候选、进指定模式，然后走真实的 `notify_ui_update`。
    ///
    /// 候选带 `comment`（`${code_hint}` 的取值源）——模板**必须含至少一个非空变量**，
    /// 否则「变量全空则整个模板输出空串」的隐式可选段规则会让纯字面量模板恒渲染成空，
    /// 三个用例会一起拿到 `Some("")`，看起来像「模板没生效」其实是测试自己写错了。
    fn emit(c: &Arc<Coordinator>, active: Option<ModeKind>) {
        {
            let mut st = c.state.lock().unwrap();
            st.active = active;
            st.candidates = vec![wind_candidate::Candidate {
                text: "测".into(),
                comment: "码".into(),
                ..Default::default()
            }];
            st.input_buffer = "a".into();
        }
        let st = c.state.lock().unwrap();
        c.notify_ui_update(&st);
    }

    /// 取最近一条 `UpdateCandidates` 里首候选的注释段。
    fn last_comment(rx: &std::sync::mpsc::Receiver<UiCommand>) -> Option<String> {
        let mut found = None;
        // 排空取**最后**一条：一次刷新会发多条 UI 命令，取第一条会拿到上一轮残留。
        while let Ok(cmd) = rx.try_recv() {
            if let UiCommand::UpdateCandidates { candidates, .. } = cmd {
                found = candidates.first().map(|c| c.comment.clone());
            }
        }
        found
    }

    fn cfg_with_templates() -> Config {
        let mut c = Config::default();
        // 用字面量而非变量，断言才不依赖词库内容
        c.ui.candidate.comment_template_vertical = "全局${code_hint}".into();
        c.ui.candidate.comment_template_horizontal = "全局${code_hint}".into();
        c
    }

    #[test]
    fn mode_override_reaches_ui() {
        let mut cfg = cfg_with_templates();
        cfg.input.temp_english.comment_template_vertical = Some("临英${code_hint}".into());
        cfg.input.temp_english.comment_template_horizontal = Some("临英${code_hint}".into());
        let (c, rx) = coord_with_ui(cfg);

        emit(&c, None);
        assert_eq!(
            last_comment(&rx),
            Some("全局码".to_string()),
            "无模式时取全局模板"
        );

        emit(&c, Some(ModeKind::TempEnglish));
        assert_eq!(
            last_comment(&rx),
            Some("临英码".to_string()),
            "临英期间必须改用模式级模板——只测 template_for 抓不到消费端没接线"
        );
    }

    /// ★ 空串 = 本模式不显示注释（与「跟随全局」是两回事），且这条语义要一路走到 UI。
    #[test]
    fn empty_override_hides_comment_at_ui() {
        let mut cfg = cfg_with_templates();
        cfg.input.temp_pinyin.comment_template_vertical = Some(String::new());
        cfg.input.temp_pinyin.comment_template_horizontal = Some(String::new());
        let (c, rx) = coord_with_ui(cfg);

        emit(&c, Some(ModeKind::TempPinyin));
        assert_eq!(
            last_comment(&rx),
            Some(String::new()),
            "空串必须让本模式不显示注释，而不是回落全局"
        );
    }

    /// 退出模式后自动回到全局模板——声明式重算的自愈性，无需任何「恢复」动作。
    #[test]
    fn leaving_mode_restores_global_template() {
        let mut cfg = cfg_with_templates();
        cfg.input.temp_english.comment_template_vertical = Some("临英${code_hint}".into());
        cfg.input.temp_english.comment_template_horizontal = Some("临英${code_hint}".into());
        let (c, rx) = coord_with_ui(cfg);

        emit(&c, Some(ModeKind::TempEnglish));
        assert_eq!(last_comment(&rx), Some("临英码".to_string()));
        emit(&c, None);
        assert_eq!(
            last_comment(&rx),
            Some("全局码".to_string()),
            "退出模式后应自动算回全局，不依赖任何显式恢复"
        );
    }
}

#[cfg(test)]
mod caret_compat_tests {
    //! caret_use_top 兼容变换：微信等 WebView 下把候选窗定位基准从 rect.bottom 改为 rect.top。
    use super::*;

    fn coord() -> Arc<Coordinator> {
        Coordinator::new_headless(Config::default(), None)
    }

    fn caret(y: i32, height: i32) -> CaretData {
        CaretData {
            x: 100,
            y,
            height,
            composition_start_x: 100,
            composition_start_y: y,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        }
    }

    /// 组合起点锚定的 500px 校验必须**只在 caret 与组合起点同源时**生效。
    fn far_comp_start(source: i32) -> CaretData {
        // 桌面输入实测形态：caret (0,1388) 是 GUI 回退取到的任务栏残留光标，
        // compStart (473,217) 才是真实组合位置，两者 dy=1171 ≥500px。
        CaretData {
            x: 0,
            y: 1388,
            height: 20,
            composition_start_x: 473,
            composition_start_y: 217,
            source,
        }
    }

    fn lock_comp_start_with(source: i32) -> bool {
        let c = coord();
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "ab".to_string(); // 置为组合中，否则 caret_update 只落缓存
        }
        c.handle_caret_update(&far_comp_start(source));
        c.composition_start.lock().unwrap().2
    }

    #[test]
    fn non_tsf_caret_skips_composition_start_distance_check() {
        // caret 来自 GUI 回退时，它与组合起点根本不是一个语义域，距离比较无意义。
        // 旧行为在此把**唯一正确**的组合起点当异常丢弃了（桌面输入定位到任务栏的直接原因）。
        assert!(
            lock_comp_start_with(wind_ipc::protocol::caret_source::GUI_CARET),
            "caret 为 GUI 回退源时应跳过距离校验、直接锁定组合起点"
        );
    }

    #[test]
    fn tsf_caret_still_rejects_far_composition_start() {
        // 反向对照：同样的距离，caret 若来自 TSF 域则 500px 保护仍须生效——同源却相差离谱，
        // 那才是它本来要抓的坐标系不一致。**缺了这条，上面那个测试无法区分「按来源放行」
        // 与「干脆不再校验」**，把保护删光也能让它变绿。
        assert!(
            !lock_comp_start_with(wind_ipc::protocol::caret_source::TSF_SELECTION),
            "TSF 同源时超 500px 仍应判为坐标系不一致而丢弃"
        );
    }

    #[test]
    fn caret_use_top_shifts_y_to_top_and_keeps_real_line_height() {
        let c = coord();
        // 模拟焦点进程命中 caret_use_top 规则。
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1234,
            caret_use_top: true,
            ..Default::default()
        };
        c.handle_caret_update(&caret(200, 20));
        let s = c.state.lock().unwrap();
        // bottom(200) → top：200 - 20 = 180（下方显示锚此稳定值）。
        assert_eq!(s.caret_y, 180);
        // 保留真实行高 20（> 下限）供上方显示避让正文，而非压成 1。
        assert_eq!(s.caret_height, 20);
    }

    #[test]
    fn caret_use_top_degenerate_height_floored_to_min() {
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1234,
            caret_use_top: true,
            ..Default::default()
        };
        // 退化帧 height=1：top 仍稳定（bottom-1），但行高落到下限避免上方遮挡。
        c.handle_caret_update(&caret(200, 1));
        let s = c.state.lock().unwrap();
        assert_eq!(s.caret_y, 199);
        assert_eq!(s.caret_height, CARET_USE_TOP_MIN_LINE_H);
    }

    /// 走一次 notify_ui_update 的首显闸门，返回「是否 arm 了等待」。
    /// 缓冲非空是必要前提，否则会先命中「空则隐藏」守卫、根本到不了闸门。
    fn armed_after_first_frame(c: &Arc<Coordinator>) -> bool {
        {
            let mut s = c.state.lock().unwrap();
            s.input_buffer = "a".to_string();
        }
        let s = c.state.lock().unwrap();
        c.notify_ui_update(&s);
        drop(s);
        *c.pending_first_show.lock().unwrap()
    }

    /// 造一个「正等首显、且已有上一轮权威坐标」的局面，返回喂入 probe 后是否仍在等待。
    fn still_waiting_after_probe(c: &Arc<Coordinator>, probe: CaretData) -> bool {
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
        }
        *c.last_authoritative_caret.lock().unwrap() = (500, 300, true);
        // 与生产代码同源：`last_authoritative_caret` 置 true 和 `caret_cache_verified` 置 true
        // 是 `handle_caret_update` 里**同一行判据**下的两个动作，现实中不可能只有前者。
        // 二者不复用同一个字段，是因为清位不同——前者从不清（跨焦点仍为 true），后者在焦点
        // 到达/用户移动光标时清零。probe 判据需要的恰恰是后者（"基准可比"），拿前者判就会
        // 在焦点切换后把另一个单元格的坐标当基准，必然误判成"已 reflow"。
        c.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *c.pending_first_show.lock().unwrap() = true;
        c.handle_caret_probe(&probe);
        *c.pending_first_show.lock().unwrap()
    }

    fn probe_at(x: i32, y: i32, height: i32) -> CaretData {
        CaretData {
            x,
            y,
            height,
            composition_start_x: x,
            composition_start_y: y,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        }
    }

    fn set_mode(c: &Arc<Coordinator>, mode: wind_config::app_compat::FirstShowMode) {
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1234,
            first_show_mode: mode,
            ..Default::default()
        };
    }

    /// fast 档的兜底必须远短于 wait 档：Word 这类宿主不发 OnLayoutChange、组合坐标 60~190ms
    /// 才到，而连打时组合只活 27~57ms，150ms 兜底永远等不到到期 ⇒ fast 退化成 wait、候选窗不显示。
    #[test]
    fn fast_mode_uses_short_first_show_fallback() {
        use wind_config::app_compat::FirstShowMode;
        let c = coord();
        set_mode(&c, FirstShowMode::Wait);
        assert_eq!(c.first_show_fallback_ms(), 150, "wait 档保持既有 150ms");
        set_mode(&c, FirstShowMode::Instant);
        assert_eq!(
            c.first_show_fallback_ms(),
            150,
            "instant 档走逃生口不 arm，取值无所谓但不应被 fast 的短值污染"
        );
        set_mode(&c, FirstShowMode::Fast);
        let cfg = c.rt().config.ui.candidate.fast_first_show_fallback_ms;
        assert_eq!(c.first_show_fallback_ms(), cfg);
        assert!(cfg < 150, "fast 档兜底必须短于 wait 档，否则本修复失效");
    }

    /// DLL 的「坐标待定」握手会把 wait 档延长到 600ms。fast 档必须拒绝这次延长，
    /// 否则短兜底当场作废、又变回干等。观察点取 token：arm 会 bump 它，early return 不会。
    #[test]
    fn caret_pending_does_not_extend_fast_mode_timeout() {
        use wind_config::app_compat::FirstShowMode;
        let c = coord();
        set_mode(&c, FirstShowMode::Fast);
        *c.pending_first_show.lock().unwrap() = true;
        let before = *c.pending_first_show_token.lock().unwrap();
        c.handle_caret_pending();
        assert_eq!(
            *c.pending_first_show_token.lock().unwrap(),
            before,
            "fast 档不得重 arm（token 未变即未重 arm）"
        );
    }

    /// 上一条的对照：wait 档必须照旧延长，证明那条不是被别的守卫挡住的。
    #[test]
    fn caret_pending_still_extends_wait_mode_timeout() {
        use wind_config::app_compat::FirstShowMode;
        let c = coord();
        set_mode(&c, FirstShowMode::Wait);
        *c.pending_first_show.lock().unwrap() = true;
        let before = *c.pending_first_show_token.lock().unwrap();
        c.handle_caret_pending();
        assert_ne!(
            *c.pending_first_show_token.lock().unwrap(),
            before,
            "wait 档应重 arm 到 600ms"
        );
    }

    // ── ui.status.show_on_focus：焦点气泡与坐标可信度闸门 ────────────────────────────

    /// 造一个开了 `show_on_focus` 的协调器，并**保留 UI 通道接收端**——「气泡有没有真的发出去」
    /// 只能从 `ui_tx` 上观察。用 debug 方法「按同样规则再算一遍」是假测试：决策函数写对但
    /// 生产路径没接上时，那种测试照样全绿。
    fn coord_focus_tip(
        show_on_focus: bool,
        position_mode: &str,
    ) -> (Arc<Coordinator>, std::sync::mpsc::Receiver<UiCommand>) {
        let mut cfg = Config::default();
        cfg.ui.status.enabled = true;
        cfg.ui.status.show_on_focus = show_on_focus;
        cfg.ui.status.display_mode = "temp".to_string();
        cfg.ui.status.position_mode = position_mode.to_string();
        Coordinator::new_headless_with_ui(cfg, None)
    }

    /// 通道里是否收到了「显示状态气泡」指令。
    fn got_status_tip(rx: &std::sync::mpsc::Receiver<UiCommand>) -> bool {
        rx.try_iter()
            .any(|c| matches!(c, UiCommand::ShowStatusTip { .. }))
    }

    /// 把坐标缓存设成指定来源。
    fn set_caret(c: &Arc<Coordinator>, x: i32, y: i32, source: i32) {
        let mut st = c.state.lock().unwrap();
        st.caret_x = x;
        st.caret_y = y;
        st.caret_height = 25;
        st.caret_source = source;
    }

    /// 两个不同宿主的 client_token。用具名常量而非字面量，是因为下面「同宿主不重复弹」
    /// 那组用例的全部含义就在于**这两个值相不相等**，字面量会让它退化成看不出意图的魔数。
    const TOKEN_A: u64 = 0x1111_0000_0001;
    const TOKEN_B: u64 = 0x2222_0000_0001;

    /// 同一宿主内换 docMgr（Excel 单元格 ↔ 公式编辑栏）不得重复弹气泡。
    /// 这是「输入一次闪两下」的直接成因——闪的时机与用户的操作节奏对不上。
    #[test]
    fn focus_tip_skips_same_host_docmgr_switch() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(
            &c,
            100,
            200,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx), "首次进入该宿主应弹一次");

        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(
            !got_status_tip(&rx),
            "同一 token = 同一宿主内换 docMgr，不得重复弹"
        );
    }

    /// 反向对照：换了宿主必须照弹。
    /// **缺了这条，上一条用「弹过一次就再也不弹」的实现也能通过**——那会让切换应用时
    /// 气泡彻底消失，比重复弹更糟。
    #[test]
    fn focus_tip_shows_again_for_different_host() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(
            &c,
            100,
            200,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx));

        c.show_focus_status_if_enabled(TOKEN_B);
        assert!(got_status_tip(&rx), "换宿主必须重新提示一次");
    }

    /// 离开宿主（Thread 级失焦）后再回来，应当重新提示。
    /// ⚠ 只有 Thread 档清去重记录：CtxLost/DocChanged 是宿主内换 docMgr 的噪声，
    /// 若也清就等于按 docMgr 计数，Excel 下会退回「输入一次闪两下」。
    #[test]
    fn focus_tip_resets_after_leaving_host() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(
            &c,
            100,
            200,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx));

        // docMgr 级失焦：不清记录，回来仍不弹
        c.handle_focus_lost(TOKEN_A, wind_bridge::handler::FocusLostReason::CtxLost);
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(!got_status_tip(&rx), "CtxLost 属 docMgr 噪声，不该解除去重");

        // 真正离开宿主：清记录，回来重新弹
        c.handle_focus_lost(TOKEN_A, wind_bridge::handler::FocusLostReason::Thread);
        set_caret(
            &c,
            100,
            200,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx), "离开宿主后再进入应重新提示");
    }

    /// follow_caret 下，坐标来自 GUI 回退时**不得**直接弹气泡——那正是用户反馈的
    /// 「还没输入时定位非常不准」：`OnSetFocus` 拿不到同步锁，回退链交出的是跨窗口的
    /// Win32 光标（Word 标题行实测偏差 814px）。应转为挂起等权威坐标。
    #[test]
    fn focus_tip_defers_when_caret_source_is_not_tsf() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(&c, 0, 1388, wind_ipc::protocol::caret_source::GUI_CARET);
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(
            !got_status_tip(&rx),
            "GUI 回退坐标不可作气泡锚点，此时不得下发显示"
        );
        assert!(
            c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "应转为挂起，等 DLL 补来的权威坐标"
        );
    }

    /// 上一条的续集：权威坐标到达后必须补显示，且挂起位清掉。
    ///
    /// ⚠ 消费点必须在 `handle_caret_update` 的 `composing` 闸门**之前**——焦点刚到达时用户
    /// 还没输入，`composing` 恒 false，放在闸门之后就是永远不执行且完全静默。本用例正是
    /// 钉住这个顺序：`input_buffer` 特意留空。
    #[test]
    fn focus_tip_shows_when_authoritative_caret_arrives() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(&c, 0, 1388, wind_ipc::protocol::caret_source::GUI_CARET);
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(!got_status_tip(&rx));

        c.handle_caret_update(&CaretData {
            x: 473,
            y: 217,
            height: 28,
            composition_start_x: 0,
            composition_start_y: 0,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        });
        assert!(got_status_tip(&rx), "等到 TSF 权威坐标后应补显示气泡");
        assert!(
            !c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "补显示后挂起位必须清掉，否则下一帧坐标会再弹一次"
        );
    }

    /// 反向对照：非 TSF 域的坐标即便到达也**不得**解除挂起。
    /// 少了这条，上一条用「任何 caret_update 都补显示」的实现也能通过。
    #[test]
    fn focus_tip_stays_pending_for_non_tsf_caret_update() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(&c, 0, 1388, wind_ipc::protocol::caret_source::GUI_CARET);
        c.show_focus_status_if_enabled(TOKEN_A);
        let _ = got_status_tip(&rx); // 排空

        c.handle_caret_update(&CaretData {
            x: 10,
            y: 20,
            height: 20,
            composition_start_x: 0,
            composition_start_y: 0,
            source: wind_ipc::protocol::caret_source::GUI_CARET,
        });
        assert!(!got_status_tip(&rx), "又一个 GUI 回退坐标，仍不该显示");
        assert!(
            c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "挂起必须保持，直到真的等到 TSF 坐标"
        );
    }

    /// 坐标本就可信时立即显示，不该被闸门误伤。
    #[test]
    fn focus_tip_shows_immediately_for_tsf_caret() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(
            &c,
            473,
            217,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx), "TSF 域坐标应立即显示");
        assert!(
            !c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "无需挂起"
        );
    }

    /// fixed 模式压根不读 caret（用 custom_x/custom_y），故不受可信度闸门约束。
    /// 把闸门一刀切地套到所有模式上，会让固定位置的用户永远看不到焦点气泡。
    #[test]
    fn focus_tip_ignores_caret_source_in_fixed_mode() {
        let (c, rx) = coord_focus_tip(true, "fixed");
        set_caret(&c, 0, 1388, wind_ipc::protocol::caret_source::GUI_CARET);
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx), "fixed 模式不读 caret，应照常显示");
    }

    /// 反向对照：开关关闭时一律不显示。
    /// 少了这条，「无条件显示」的实现能让上面四条里的三条通过。
    #[test]
    fn focus_tip_silent_when_disabled() {
        let (c, rx) = coord_focus_tip(false, "follow_caret");
        set_caret(
            &c,
            473,
            217,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(!got_status_tip(&rx), "show_on_focus=false 时不得显示");
        assert!(
            !c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "开关关闭时连挂起都不该发生"
        );
    }

    /// 焦点气泡必须绕过 `show_status` 的**文本**去重。
    ///
    /// 焦点切换正是「状态文本没变但仍要提示」的场景——走文本去重路径的话，连着切两个宿主
    /// 只有第一次会弹，而这恰恰是本功能最主要的使用场景，等于开关基本无效。
    ///
    /// ⚠ 与 [`focus_tip_skips_same_host_docmgr_switch`] 的**宿主**去重是两回事，别混：
    /// 这里换的是宿主（TOKEN_A → TOKEN_B），本就该弹；那里是同一宿主内换 docMgr，不该弹。
    /// 本用例原先第二次也传同一 token，测到的其实是宿主去重引入前的旧语义。
    #[test]
    fn focus_tip_bypasses_text_dedup() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(
            &c,
            473,
            217,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(got_status_tip(&rx), "第一次焦点切换应显示");
        // 状态一字未改，模拟切到**另一个宿主**的输入框
        c.show_focus_status_if_enabled(TOKEN_B);
        assert!(
            got_status_tip(&rx),
            "文本相同也必须再显示一次——文本去重会让这个开关形同虚设"
        );
    }

    /// 失焦要作废挂起中的焦点气泡，否则权威坐标晚到时会在**已经切走之后**才弹出来。
    #[test]
    fn hide_tip_cancels_pending_focus_tip() {
        let (c, rx) = coord_focus_tip(true, "follow_caret");
        set_caret(&c, 0, 1388, wind_ipc::protocol::caret_source::GUI_CARET);
        c.show_focus_status_if_enabled(TOKEN_A);
        assert!(
            c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        c.hide_tip();
        assert!(
            !c.pending_focus_tip
                .load(std::sync::atomic::Ordering::Relaxed),
            "失焦后挂起必须作废"
        );
        let _ = got_status_tip(&rx); // 排空
        c.handle_caret_update(&CaretData {
            x: 473,
            y: 217,
            height: 28,
            composition_start_x: 0,
            composition_start_y: 0,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        });
        assert!(
            !got_status_tip(&rx),
            "失焦之后到达的权威坐标不得再触发补显示"
        );
    }

    /// 焦点 caret 的 `height == 0`（宿主尚未 reflow 的退化矩形）不得进缓存。
    ///
    /// 这条守卫原先只在同步段的 `handle_focus_gained_caret` 里有，重型段
    /// `handle_focus_gained` 自己直写 `state.caret_*`——而**重型段必然晚于同步段执行**，
    /// 于是守卫被后到的直写整个抹掉。两处口径分裂既不报错也不 panic，只表现为定位偏一行。
    #[test]
    fn focus_caret_degenerate_rect_does_not_overwrite_cache() {
        let c = coord();
        set_caret(
            &c,
            473,
            217,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.apply_focus_caret(
            &CaretData {
                x: 9999,
                y: 9999,
                height: 0, // 退化矩形
                composition_start_x: 0,
                composition_start_y: 0,
                source: wind_ipc::protocol::caret_source::GUI_CARET,
            },
            "test",
        );
        let st = c.state.lock().unwrap();
        assert_eq!(st.caret_x, 473, "退化帧不得覆盖已有的好坐标");
        assert_eq!(st.caret_y, 217);
        assert_eq!(
            st.caret_source,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
            "来源必须与坐标同进退——只回滚其一等于伪造了一个不存在的组合"
        );
    }

    /// 上一条只证明了守卫**存在于** `apply_focus_caret`，证明不了重型段真的路由过去。
    /// 这条走 `handle_focus_gained` 生产入口：它一旦退回自己直写 `state.caret_*`，本用例即红。
    ///
    /// 顺带钉住 `caret_use_top` 也在重型段生效——那是同一次覆写抹掉的第二样东西。
    #[test]
    fn handle_focus_gained_routes_caret_through_shared_guard() {
        let c = coord();
        set_caret(
            &c,
            473,
            217,
            wind_ipc::protocol::caret_source::TSF_SELECTION,
        );
        c.handle_focus_gained(&FocusData {
            x: 9999,
            y: 9999,
            height: 0, // 退化矩形：同步段会挡，重型段直写则不会
            composition_start_x: 0,
            composition_start_y: 0,
            client_token: 0,
            input_scope_mask: 0,
            disabled: false,
            reason: 0,
            caret_source: wind_ipc::protocol::caret_source::GUI_CARET,
            bundle_id: String::new(),
        });
        let st = c.state.lock().unwrap();
        assert_eq!(
            st.caret_x, 473,
            "重型段必须经 apply_focus_caret；直写会让退化帧覆盖好坐标"
        );
        assert_eq!(st.caret_y, 217);
        assert_eq!(
            st.caret_source,
            wind_ipc::protocol::caret_source::TSF_SELECTION
        );
    }

    /// 焦点事件必须作废组合起点锚定。
    ///
    /// 锚定「同一组合只锁一次、之后不再更新」的前提是**起点不会移动**，而 focus_gained 意味着
    /// 换了 docMgr——Excel 输入时在「单元格」与「公式编辑栏」之间来回切，组合整体迁移（实测
    /// 从 (593,572) 到 (1457,959)），锚点若不作废，候选窗就钉死在旧 docMgr 上：协调器拿
    /// state.caret_* 判出 reshow，下发却用锁死的组合起点，日志表现为「reshow 说要重定位、
    /// UI 位置纹丝不动」。
    #[test]
    fn focus_gained_invalidates_composition_start_anchor() {
        let c = coord();
        *c.composition_start.lock().unwrap() = (593, 572, true);
        c.handle_focus_gained(&FocusData {
            x: 1457,
            y: 959,
            height: 37,
            composition_start_x: 0,
            composition_start_y: 0,
            client_token: TOKEN_A,
            input_scope_mask: 0,
            disabled: false,
            reason: 0,
            caret_source: wind_ipc::protocol::caret_source::TSF_SELECTION,
            bundle_id: String::new(),
        });
        assert!(
            !c.composition_start.lock().unwrap().2,
            "换 docMgr 后组合起点必须作废，交由下一帧 caret_update 就地重锁"
        );
    }

    /// `caret_use_top` 变换在重型段同样要生效。
    /// 该变换原先只在同步段做，重型段的直写把它抹掉，表现为微信一类宿主定位差一个行高。
    #[test]
    fn handle_focus_gained_applies_caret_use_top() {
        let c = coord();
        {
            let mut ac = c.active_compat.lock().unwrap();
            ac.caret_use_top = true;
        }
        c.handle_focus_gained(&FocusData {
            x: 100,
            y: 300,
            height: 30,
            composition_start_x: 0,
            composition_start_y: 0,
            client_token: 0,
            input_scope_mask: 0,
            disabled: false,
            reason: 0,
            caret_source: wind_ipc::protocol::caret_source::TSF_SELECTION,
            bundle_id: String::new(),
        });
        let st = c.state.lock().unwrap();
        assert_eq!(
            st.caret_y,
            300 - 30,
            "caret_use_top 应把 Y 上移一个行高；重型段直写则原样落缓存"
        );
    }

    /// 造一个 fast 档协调器并指定坐标缓存可信与否。
    fn fast_coord(verified: bool) -> Arc<Coordinator> {
        let c = coord();
        set_mode(&c, wind_config::app_compat::FirstShowMode::Fast);
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
            st.caret_x = 100;
            st.caret_y = 200;
            st.caret_height = 25;
        }
        c.caret_cache_verified
            .store(verified, std::sync::atomic::Ordering::Relaxed);
        c
    }

    /// 首帧信任门：坐标缓存未经当前插入点验证时不得走短兜底——拿旧坐标首显正是
    /// Excel「进单元格第一个字漂移」的成因（手里那份属于上一个单元格）。
    #[test]
    fn untrusted_caret_arms_long_fallback() {
        let c = fast_coord(false);
        c.arm_pending_first_show();
        assert!(
            c.first_show_extended
                .load(std::sync::atomic::Ordering::Relaxed),
            "坐标不可信时应进入长兜底等待"
        );
        assert!(*c.pending_first_show.lock().unwrap());
    }

    /// 反向对照：坐标可信时必须照常走短兜底，否则信任门就成了无差别拖慢，
    /// fast 档整个失去意义。
    #[test]
    fn trusted_caret_keeps_short_fallback() {
        let c = fast_coord(true);
        c.arm_pending_first_show();
        assert!(
            !c.first_show_extended
                .load(std::sync::atomic::Ordering::Relaxed),
            "坐标可信时不应进入长兜底"
        );
        assert_eq!(
            c.first_show_fallback_ms(),
            c.rt().config.ui.candidate.fast_first_show_fallback_ms
        );
    }

    /// ★ 长等待不得被后续按键重置。闸门在候选窗显示前对**每一个字母**都会调 arm，若照常
    /// bump token 重新计时，用户多打几个字母就把这段等待反复推后 → 长兜底静默退化回短兜底、
    /// 错位照旧。Excel 建单元格上下文要 558ms，其间用户往往已敲了三五个字母。
    ///
    /// 这是「兜底超时长于组合寿命 ⇒ 永不到期」那个死结的镜像，独立守一条测试。
    #[test]
    fn long_fallback_survives_subsequent_keystrokes() {
        let c = fast_coord(false);
        c.arm_pending_first_show();
        let token = *c.pending_first_show_token.lock().unwrap();
        // 用户继续输入：闸门对第 2、3 个字母同样调 arm
        c.arm_pending_first_show();
        c.arm_pending_first_show();
        assert_eq!(
            *c.pending_first_show_token.lock().unwrap(),
            token,
            "后续按键不得重置长兜底计时，否则等待被无限推后"
        );
    }

    /// 反向对照：坐标可信的正常连打必须照旧每次重新计时（既有行为，不能被上一条误伤）。
    #[test]
    fn short_fallback_still_rearms_per_keystroke() {
        let c = fast_coord(true);
        c.arm_pending_first_show();
        let token = *c.pending_first_show_token.lock().unwrap();
        c.arm_pending_first_show();
        assert_ne!(
            *c.pending_first_show_token.lock().unwrap(),
            token,
            "短兜底路径的既有行为是每次按键重新计时"
        );
    }

    /// 长兜底到期后不再续：用旧坐标首显仍优于候选窗一直不出现。
    #[test]
    fn long_fallback_shows_when_it_finally_expires() {
        let c = fast_coord(false);
        c.arm_pending_first_show();
        // ⚠ token 必须先 let 绑定再传：写成 `fire(*c...lock().unwrap())` 会让临时 MutexGuard
        // 活到整个语句结束（Rust 临时值生命周期），而 fire 内部要再锁同一个 Mutex ⇒ 自死锁。
        let token = *c.pending_first_show_token.lock().unwrap();
        c.fire_pending_first_show(token);
        assert!(
            !*c.pending_first_show.lock().unwrap(),
            "长兜底到期必须放行，否则候选窗永不出现"
        );
        assert!(
            c.first_show_was_provisional
                .load(std::sync::atomic::Ordering::Relaxed),
            "用的是旧坐标，须记为非权威以享放宽容差"
        );
    }

    /// wait/instant 档一字不变：它们的长兜底由 caret_pending 握手负责，信任门若也插一脚，
    /// 两条路径叠加会让 wait 最坏等到 1200ms。
    #[test]
    fn trust_gate_does_not_touch_wait_mode() {
        let c = fast_coord(false);
        set_mode(&c, wind_config::app_compat::FirstShowMode::Wait);
        c.arm_pending_first_show();
        assert!(
            !c.first_show_extended
                .load(std::sync::atomic::Ordering::Relaxed),
            "wait 档不受信任门影响"
        );
        assert_eq!(c.first_show_fallback_ms(), 150, "wait 档保持既有 150ms");
    }

    /// 闸门日志打印的超时必须等于实际 arm 的超时。此前闸门直接打 `first_show_fallback_ms()`，
    /// 信任门命中时会「日志说 25ms、实际等 600ms」——排查首显延迟时这种分叉最坑人。
    #[test]
    fn logged_timeout_matches_actual_arm() {
        let c = fast_coord(false);
        assert_eq!(
            c.planned_first_show_timeout_ms(),
            FIRST_SHOW_LONG_FALLBACK_MS,
            "信任门命中时闸门日志须报长兜底"
        );
        c.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            c.planned_first_show_timeout_ms(),
            c.rt().config.ui.candidate.fast_first_show_fallback_ms,
            "未命中时须报 fast 短兜底"
        );
    }

    /// 上屏 / 组合结束必须复位长等待标记——「这一轮已在长等待中」是**每轮独立**的事实，
    /// 跨轮残留会让 `already_waiting` 的判据失去意义（当前因 `pending` 同时被复位而侥幸
    /// 不出错，但那是巧合不是设计）。
    #[test]
    fn reset_first_show_clears_extended_flag() {
        let c = fast_coord(false);
        c.arm_pending_first_show();
        assert!(
            c.first_show_extended
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        c.reset_first_show();
        assert!(
            !c.first_show_extended
                .load(std::sync::atomic::Ordering::Relaxed),
            "组合结束必须复位，否则下一轮 arm 被永久跳过"
        );
    }

    /// 焦点到达 = 换了 DocMgr，此刻 state 里那份是焦点事件随包携带的坐标（宿主多半还没
    /// reflow，Excel 甚至还没建好编辑上下文），不够格让 fast 跳过等待。
    #[test]
    fn focus_gained_invalidates_caret_cache() {
        let c = coord();
        c.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        c.handle_focus_gained(&FocusData {
            x: 100,
            y: 300,
            height: 30,
            composition_start_x: 0,
            composition_start_y: 0,
            client_token: 0,
            input_scope_mask: 0,
            disabled: false,
            reason: 0,
            caret_source: wind_ipc::protocol::caret_source::TSF_SELECTION,
            bundle_id: String::new(),
        });
        assert!(
            !c.caret_cache_verified
                .load(std::sync::atomic::Ordering::Relaxed),
            "焦点到达必须作废坐标缓存的可信标记"
        );
    }

    /// 用户在同一 DocMgr 内点到别处：不发 focus_gained，宿主也只在有 composition 时才回送
    /// caret_update，所以缓存里仍是上次输入的位置——必须作废。
    #[test]
    fn user_caret_move_invalidates_cache_but_self_commit_echo_does_not() {
        let c = coord();
        c.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        c.handle_selection_changed(0);
        assert!(
            !c.caret_cache_verified
                .load(std::sync::atomic::Ordering::Relaxed),
            "用户移动光标必须作废坐标缓存"
        );

        // 反向对照：自提交回声（上屏后宿主插入文本导致的光标移动）不得作废，否则每上屏
        // 一个字就作废一次，fast 档在连打时完全退化。
        c.caret_cache_verified
            .store(true, std::sync::atomic::Ordering::Relaxed);
        *c.last_self_commit.lock().unwrap() = Some(std::time::Instant::now());
        c.handle_selection_changed(0);
        assert!(
            c.caret_cache_verified
                .load(std::sync::atomic::Ordering::Relaxed),
            "自提交回声不得作废坐标缓存"
        );
    }

    /// 兜底首显用的是按键前的旧坐标，必须记为「非权威」，否则随后到达的权威坐标会被 3px
    /// 常规容差判成要校正而跳一下——兜底路径正是抖动最容易被看见的地方。
    #[test]
    fn fallback_first_show_marks_provisional() {
        let c = coord();
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
            st.caret_x = 100;
            st.caret_y = 200;
            st.caret_height = 25;
        }
        *c.pending_first_show.lock().unwrap() = true;
        let token = *c.pending_first_show_token.lock().unwrap();
        c.fire_pending_first_show(token);
        assert!(
            c.first_show_was_provisional
                .load(std::sync::atomic::Ordering::Relaxed),
            "兜底显示后应置位 provisional 以享放宽容差"
        );
    }

    /// 首显用过非权威坐标后，随后到达的权威坐标若只差不到 80% 行高，不得 reshow。
    /// 抖动的观感来自校正动作本身——这条钉住「小偏差不动」的行为。
    #[test]
    fn provisional_first_show_tolerates_small_correction() {
        let c = coord();
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
            st.caret_x = 100;
            st.caret_y = 200;
            st.caret_height = 25;
        }
        *c.candidate_shown.lock().unwrap() = true;
        c.first_show_was_provisional
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 偏差 15px < 25 × 0.8 = 20px ⇒ 应被吞掉
        c.handle_caret_update(&CaretData {
            x: 115,
            y: 200,
            height: 25,
            composition_start_x: 115,
            composition_start_y: 200,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        });
        assert_eq!(
            c.last_valid_caret.lock().unwrap().0,
            0,
            "小于 80% 行高的偏差不应触发 reshow（未走到 notify_ui_update）"
        );
    }

    /// 换行那种大偏差必须照常校正——容差放宽不能把真正的错位也一起吞掉。
    #[test]
    fn provisional_first_show_still_corrects_large_jump() {
        let c = coord();
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
            st.caret_x = 900;
            st.caret_y = 200;
            st.caret_height = 25;
        }
        *c.candidate_shown.lock().unwrap() = true;
        c.first_show_was_provisional
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // 换行：x 回行首、y 下移两行（实测 EverEdit 曾出现 dx=156 dy=194）
        c.handle_caret_update(&CaretData {
            x: 726,
            y: 250,
            height: 25,
            composition_start_x: 726,
            composition_start_y: 250,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        });
        assert_eq!(
            c.last_valid_caret.lock().unwrap().0,
            726,
            "换行级偏差必须校正"
        );
    }

    /// 容差只作用于「首显用过非权威坐标」的那一次：常规光标更新仍按 3px 走，
    /// 否则正常输入时的小幅移动会被误吞、候选窗跟不上光标。
    #[test]
    fn settle_tolerance_applies_only_after_provisional_first_show() {
        let c = coord();
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
            st.caret_x = 100;
            st.caret_y = 200;
            st.caret_height = 25;
        }
        *c.candidate_shown.lock().unwrap() = true;
        // 未置位 first_show_was_provisional
        c.handle_caret_update(&CaretData {
            x: 115,
            y: 200,
            height: 25,
            composition_start_x: 115,
            composition_start_y: 200,
            source: wind_ipc::protocol::caret_source::TSF_SELECTION,
        });
        assert_eq!(
            c.last_valid_caret.lock().unwrap().0,
            115,
            "常规路径下 15px 偏移仍应 reshow"
        );
    }

    #[test]
    fn probe_ignored_unless_fast_mode() {
        // `wait` 档的底线：退回该档的宿主必须拿到「等 reflow 权威坐标」的原行为，
        // probe 一条都不许消费。
        //
        // ⚠ 2026-08-03 前本条靠 `coord()` 的默认档恰好是 `wait` 来表达，默认档改成
        // `fast` 后那个前提失效，故改为显式设档。**测试若靠「默认值恰好是某值」间接
        // 表达语义，默认值一变它就从"守住语义"退化成"守住巧合"。**
        let c = coord();
        set_mode(&c, wind_config::app_compat::FirstShowMode::Wait);
        assert!(
            still_waiting_after_probe(&c, probe_at(800, 600, 24)),
            "非 fast 档时 probe 必须被完全忽略"
        );
    }

    /// ★ 首显有多条通路，信任门必须每条都接。本条守住 `caret_probe` 这条——它绕过闸门
    /// 直接首显，实测（2026-08-03 Excel）在闸门刚 arm 600ms 长兜底后 **6ms** 就用
    /// `(1299,535)` 抢先显示，而 200ms 后真坐标是 `(1344,744)` ⇒ 显示后跳一次。
    ///
    /// 根因是 probe 的两条判据在坐标缓存失效时**都失去判断力**：判据 1 靠「≠ 上一轮权威
    /// 坐标」推断宿主已 reflow，而焦点切换后那个基准属于另一个单元格，probe 值当然不等于
    /// 它 ⇒ 判据恒成立；判据 2 的"上次按键间隔"跨了焦点，同样说明不了当前帧可信。
    #[test]
    fn probe_defers_to_long_wait_when_cache_unverified() {
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        {
            let mut st = c.state.lock().unwrap();
            st.input_buffer = "a".to_string();
        }
        // 有基准、坐标也「变了」——判据 1 本会采信；但缓存未验证，基准不可比。
        *c.last_authoritative_caret.lock().unwrap() = (500, 300, true);
        c.caret_cache_verified
            .store(false, std::sync::atomic::Ordering::Relaxed);
        *c.pending_first_show.lock().unwrap() = true;
        c.handle_caret_probe(&probe_at(800, 600, 24));
        assert!(
            *c.pending_first_show.lock().unwrap(),
            "坐标缓存未验证时 probe 不得提前首显，须让位给长兜底等真坐标"
        );

        // 连打快路径（判据 2）同样要被拦住，否则换个入口照样绕过去。
        *c.last_key_interval_ms.lock().unwrap() = Some(60);
        c.handle_caret_probe(&probe_at(500, 300, 24));
        assert!(
            *c.pending_first_show.lock().unwrap(),
            "连打快路径也必须过信任门——只堵判据 1 等于没堵"
        );
    }

    #[test]
    fn probe_releases_first_show_when_caret_moved() {
        // 坐标已不同于上一轮权威 ⇒ 宿主已 reflow ⇒ 采信并提前首显。
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        assert!(
            !still_waiting_after_probe(&c, probe_at(800, 600, 24)),
            "坐标已变应提前首显"
        );
    }

    /// 连打快路径必须由**相邻按键间隔**驱动，不能由「距上次按键多久」驱动。
    ///
    /// 后者恒成立（试探坐标总在按键后 10ms 内到达），会让判据被完全绕过——本功能就这么
    /// 空跑过一轮。这条测试构造「间隔很大（慢速手打）」的局面：此时即使坐标等于上一轮权威
    /// （即宿主尚未 reflow），也必须继续等待，绝不能被快路径放行。
    #[test]
    fn slow_typing_does_not_take_fast_path() {
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        // 慢速手打：相邻按键间隔 800ms，远超默认 100ms 窗口
        *c.last_key_interval_ms.lock().unwrap() = Some(800);
        assert!(
            still_waiting_after_probe(&c, probe_at(500, 300, 24)),
            "慢速输入下不得走连打快路径，须回落到「≠上一轮权威」判据"
        );
    }

    /// 连打（间隔在窗口内）时直接采信首条试探坐标——即使它等于上一轮权威坐标。
    /// 依据：连打时光标沿同一行顺序前移、不重排，跟手比精确更重要。
    #[test]
    fn fast_typing_takes_fast_path_even_when_caret_unchanged() {
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        *c.last_key_interval_ms.lock().unwrap() = Some(60); // 与真机脚本同节奏
        assert!(
            !still_waiting_after_probe(&c, probe_at(500, 300, 24)),
            "连打间隔内应走快路径立即首显"
        );
    }

    #[test]
    fn probe_keeps_waiting_when_caret_equals_previous() {
        // 与上一轮权威坐标相同 ⇒ 宿主尚未 reflow（实测 WPS 前两次采样即如此）⇒ 继续等。
        // 采信它就会把候选窗定在上一轮的位置，正是要避免的抖动。
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        assert!(
            still_waiting_after_probe(&c, probe_at(500, 300, 24)),
            "坐标等于上一轮权威时必须继续等待"
        );
    }

    #[test]
    fn probe_rejects_degenerate_rect() {
        // 退化 rect（h<=0）：实测 WPS 采到过 top==bottom 的样本，其 x 与真实位置差 1687px。
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1,
            first_show_mode: wind_config::app_compat::FirstShowMode::Fast,
            ..Default::default()
        };
        assert!(
            still_waiting_after_probe(&c, probe_at(9999, 8888, 0)),
            "退化 rect 不得采信"
        );
    }

    #[test]
    fn default_host_waits_for_authoritative_caret() {
        // 对照组：无 compat 规则、坐标未就绪 → 保持原行为，等 reflow 权威坐标。
        // 这条也是另外两个测试的有效性保证：若闸门被改成恒放行，此测试会挂。
        let c = coord();
        assert!(
            armed_after_first_frame(&c),
            "默认宿主首帧应 arm 等待权威坐标"
        );
    }

    #[test]
    fn instant_mode_bypasses_first_show_wait() {
        // 逃生口②：compat.toml 标记「光标稳定」的宿主直接首显。连打场景只有这一项能生效。
        let c = coord();
        *c.active_compat.lock().unwrap() = ActiveCompat {
            pid: 1234,
            first_show_mode: wind_config::app_compat::FirstShowMode::Instant,
            ..Default::default()
        };
        assert!(
            !armed_after_first_frame(&c),
            "instant 档应立即首显，不得 arm 等待"
        );
    }

    #[test]
    fn ready_coords_bypass_first_show_wait() {
        // 逃生口③：已有过有效 caret 且本轮组合起点已锁定 ⇒ 没有漂移可等。
        // 对应 Go 的 `!caretValid || !compositionStartValid` 取反。
        let c = coord();
        *c.last_valid_caret.lock().unwrap() = (100, 200, 20);
        *c.composition_start.lock().unwrap() = (100, 200, true);
        assert!(!armed_after_first_frame(&c), "坐标已就绪时不应再等 reflow");
    }

    #[test]
    fn ready_coords_requires_both_caret_and_composition_start() {
        // 逃生口③的两个分量必须同时成立：只有 caret 有效、组合起点未锁定时仍须等待
        // ——组合起点未锁定正说明本轮 composition 的 reflow 坐标还没到。
        let c = coord();
        *c.last_valid_caret.lock().unwrap() = (100, 200, 20);
        // composition_start 保持 (0,0,false)
        assert!(
            armed_after_first_frame(&c),
            "仅 caret 有效、组合起点未锁定时应继续等待"
        );
    }

    #[test]
    fn no_rule_keeps_bottom_coordinates() {
        let c = coord();
        // 未命中规则（默认 (0,false)）：坐标保持原样，不做 top 变换。
        c.handle_caret_update(&caret(200, 20));
        let s = c.state.lock().unwrap();
        assert_eq!(s.caret_y, 200);
        assert_eq!(s.caret_height, 20);
    }

    #[test]
    fn update_active_compat_extracts_pid_and_caches() {
        let c = coord();
        // client_token = PID<<32 | instance。PID=0（无效）不更新缓存。
        c.update_active_compat(0);
        assert_eq!(*c.active_compat.lock().unwrap(), ActiveCompat::default());
        // 合法 PID：headless（非真实进程）下 process_name 取不到名字 → caret_use_top=false，
        // 但 pid 应被缓存（避免重复 OpenProcess）。
        let token = (4321u64 << 32) | 7;
        c.update_active_compat(token);
        assert_eq!(c.active_compat.lock().unwrap().pid, 4321);
    }

    #[test]
    fn update_active_compat_prefers_cached_name_over_process_lookup() {
        // macOS 路径：宿主名由 `.app` 随焦点事件送进 pid_names，`process_name` 恒空串。
        // 缓存必须优先于反查，否则 compat 规则永远匹配不到宿主。
        let c = coord();
        let pid = 5150u32;
        let token = (pid as u64) << 32 | 3;
        c.pid_names
            .lock()
            .unwrap()
            .insert(pid, "com.apple.textedit".into());
        let mut rules = Vec::new();
        wind_config::app_compat::set_first_show_mode(
            &mut rules,
            "com.apple.textedit",
            wind_config::app_compat::FirstShowMode::Fast,
        );
        *c.app_compat.lock().unwrap() = wind_config::app_compat::AppCompat::from_rules(rules);
        c.update_active_compat(token);
        assert_eq!(
            c.active_compat.lock().unwrap().first_show_mode,
            wind_config::app_compat::FirstShowMode::Fast,
            "缓存里的 bundle id 必须参与 compat 规则匹配"
        );
    }
}

#[cfg(test)]
mod initial_mode_tests {
    //! 初始状态语义矩阵验证：激活重置 / 全局记忆 / per-app 独立（均纯内存，无词典/UI 依赖）。
    use super::*;

    fn coord_with(f: impl FnOnce(&mut Config)) -> Arc<Coordinator> {
        let mut cfg = Config::default();
        f(&mut cfg);
        Coordinator::new_headless(cfg, None)
    }

    /// 注入焦点进程（headless 下 OpenProcess 取不到真实进程名，手动填缓存）。
    fn set_focus_proc(c: &Arc<Coordinator>, pid: u32, name: &str) {
        c.active_compat.lock().unwrap().pid = pid;
        c.pid_names.lock().unwrap().insert(pid, name.to_string());
    }

    fn token(pid: u32) -> u64 {
        ((pid as u64) << 32) | 1
    }

    /// global + remember=false（默认）：状态被污染成英文后，激活时重置回配置默认（中文），
    /// 全半角/标点一并重置——本 bug 的核心修复。
    #[test]
    fn activation_resets_to_default_when_not_remembering() {
        let c = coord_with(|cfg| {
            cfg.input.default.remember_last_state = false;
            cfg.input.default.chinese_mode = true;
            cfg.input.default.full_width = false;
            cfg.input.default.chinese_punct = true;
        });
        {
            let mut s = c.state.lock().unwrap();
            s.chinese_mode = false; // 模拟 compartment 脏事件污染
            s.full_width = true;
            s.chinese_punct = false;
        }
        c.apply_initial_mode(token(100), true);
        let s = c.state.lock().unwrap();
        assert!(s.chinese_mode);
        assert!(!s.full_width);
        assert!(s.chinese_punct);
    }

    /// global + remember=true：激活时保持用户最后一次主动切换的状态，不重置。
    #[test]
    fn activation_keeps_last_state_when_remembering() {
        let c = coord_with(|cfg| {
            cfg.input.default.remember_last_state = true;
            cfg.input.default.chinese_mode = true;
        });
        {
            let mut s = c.state.lock().unwrap();
            s.chinese_mode = false; // 用户切到英文
            s.full_width = true;
        }
        // 直接注入"最后状态"内存镜像（不调 record_last_state，避免测试写真实 state.toml）。
        *c.runtime_last.lock().unwrap() = (false, true, true);
        c.apply_initial_mode(token(100), true);
        let s = c.state.lock().unwrap();
        assert!(!s.chinese_mode, "remember=true 激活不得重置回默认");
        assert!(s.full_width);
    }

    /// scope=app：首见进程用配置默认；record_app_mode 写表后按进程恢复各自状态。
    #[test]
    fn per_app_scope_remembers_mode_per_process() {
        let c = coord_with(|cfg| {
            cfg.input.default.state_scope = "app".into();
            cfg.input.default.chinese_mode = true;
        });
        // 游戏进程：首见 → 默认中文；用户切英文 → 写表。
        set_focus_proc(&c, 100, "game.exe");
        assert!(
            c.initial_chinese_mode_for("game.exe"),
            "首见进程应为配置默认"
        );
        c.state.lock().unwrap().chinese_mode = false;
        c.record_app_mode(false);
        // 切到聊天进程：首见 → 默认中文。
        set_focus_proc(&c, 200, "chat.exe");
        c.apply_initial_mode(token(200), false);
        assert!(c.state.lock().unwrap().chinese_mode);
        // 切回游戏进程：恢复英文记忆。
        set_focus_proc(&c, 100, "game.exe");
        c.apply_initial_mode(token(100), false);
        assert!(!c.state.lock().unwrap().chinese_mode);
    }

    /// scope=app：FOCUS_GAINED 同步路径（get_current_mode）命中记忆表时先切换再回传；
    /// 未入缓存的进程保持现状（由重型段修正）。
    #[test]
    fn get_current_mode_switches_per_app_on_cache_hit() {
        let c = coord_with(|cfg| {
            cfg.input.default.state_scope = "app".into();
            cfg.input.default.chinese_mode = true;
        });
        // 焦点原本在别的进程。同步段先于重型段的 update_active_compat 执行，此刻
        // active_compat.pid 仍是**上一个**进程——`crossed` 判据正是靠这一点识别「跨进程
        // 切入」。故夹具必须把旧进程留在 active_compat 里，只把新进程名喂进 pid_names。
        set_focus_proc(&c, 1, "other.exe");
        c.pid_names
            .lock()
            .unwrap()
            .insert(100, "game.exe".to_string());
        c.mode_states
            .lock()
            .unwrap()
            .insert("game.exe".to_string(), false);
        // 当前全局是中文，焦点到 game.exe → 同步切英文并回传。
        let (chinese, _) = c.get_current_mode(token(100));
        assert!(!chinese);
        assert!(!c.state.lock().unwrap().chinese_mode);
        // 未缓存的 pid（首次聚焦）：保持现状不误切。
        let (chinese, _) = c.get_current_mode(token(999));
        assert!(!chinese, "未知进程应回传当前状态");
    }

    /// global（默认作用域）：get_current_mode 不做 per-app 切换，直接回权威状态。
    #[test]
    fn get_current_mode_global_scope_passthrough() {
        let c = coord_with(|_| {});
        c.state.lock().unwrap().chinese_mode = false;
        let (chinese, _) = c.get_current_mode(token(100));
        assert!(!chinese);
    }

    /// 注入 compat.toml 的应用规则（纯内存，不碰文件系统）。
    fn set_rule(
        c: &Arc<Coordinator>,
        process: &str,
        mode: Option<wind_config::app_compat::InitialMode>,
        punct: Option<wind_config::app_compat::InitialMode>,
    ) {
        use wind_config::app_compat::{AppCompat, AppCompatRule};
        *c.app_compat.lock().unwrap() = AppCompat::from_rules(vec![AppCompatRule {
            process: process.to_string(),
            initial_mode: mode,
            initial_punct: punct,
            ..Default::default()
        }]);
    }

    /// 应用规则**压过** per-app 记忆表。
    ///
    /// 顺序反了（规则排记忆表之后）对 Everything / Listary 这类**常驻隐藏式**进程等于
    /// 只在开机后第一次唤出时生效：进程不退出，会话级记忆表里「首次」永远只有一次。
    #[test]
    fn app_rule_beats_per_app_memory() {
        use wind_config::app_compat::InitialMode as IM;
        let c = coord_with(|cfg| {
            cfg.input.default.state_scope = "app".into();
            cfg.input.default.chinese_mode = true;
        });
        set_rule(&c, "everything.exe", Some(IM::English), None);
        c.mode_states
            .lock()
            .unwrap()
            .insert("everything.exe".into(), true); // 记忆表说中文
        assert!(
            !c.initial_chinese_mode_for("everything.exe"),
            "规则必须压过记忆表，否则对常驻进程只生效一次"
        );
        // 没有规则的进程仍旧走记忆表，既有语义不变。
        c.mode_states
            .lock()
            .unwrap()
            .insert("game.exe".into(), true);
        assert!(c.initial_chinese_mode_for("game.exe"));
    }

    /// 重算门控的完整矩阵。这是本功能唯一容易写错又最难从现象反推的地方，
    /// 逐条锁死；每条注释即该组合对应的真实场景。
    #[test]
    fn reapply_gate_matrix() {
        // 同应用内焦点跳转（Everything 搜索框 ↔ 结果列表）：一律不重算，保住用户手切。
        assert!(!should_reapply_initial(false, true, true, true));
        // 跨进程、无 per_app、两边都没规则（Word → Chrome）：不动。放宽成「规则表非空」
        // 就会在这里重算，把用户在 Word 手切的英文冲成配置默认。
        assert!(!should_reapply_initial(true, false, false, false));
        // 进入规则应用（Word → Everything）。
        assert!(should_reapply_initial(true, false, false, true));
        // **离开**规则应用（Everything → Word）：只看 new_has_rule 会漏掉这条，
        // 表现为 Everything 的英文残留给之后的每一个应用。
        assert!(should_reapply_initial(true, false, true, false));
        // per_app 既有语义不受规则影响。
        assert!(should_reapply_initial(true, true, false, false));
    }

    /// 显式 `initial_punct` 压过 `follow_mode` 的推导。
    /// 顺序反了的话，用户配了标点规则却恰好开着 follow_mode 时它会被静默覆盖。
    #[test]
    fn initial_punct_rule_beats_follow_mode() {
        use wind_config::app_compat::InitialMode as IM;
        let c = coord_with(|cfg| {
            cfg.input.punct.follow_mode = true;
            cfg.input.default.chinese_mode = true;
        });
        set_rule(&c, "everything.exe", Some(IM::English), Some(IM::Chinese));
        set_focus_proc(&c, 100, "everything.exe");
        c.apply_initial_mode(token(100), false);
        let s = c.state.lock().unwrap();
        assert!(!s.chinese_mode, "规则要求初始英文");
        assert!(
            s.chinese_punct,
            "initial_punct=chinese 必须压过 follow_mode 推出的英文标点"
        );
    }

    /// 同步路径：跨进程切入规则应用时当场回传英文（消除首键竞态），
    /// 而同应用内的再次 focus_gained 不得把用户手切的模式拉回规则值。
    #[test]
    fn get_current_mode_rule_applies_only_on_cross_process_switch() {
        use wind_config::app_compat::InitialMode as IM;
        let c = coord_with(|cfg| cfg.input.default.chinese_mode = true);
        set_rule(&c, "everything.exe", Some(IM::English), None);
        // 焦点原本在别的进程（active_compat.pid=1），现在切入 everything.exe。
        set_focus_proc(&c, 1, "other.exe");
        c.pid_names
            .lock()
            .unwrap()
            .insert(100, "everything.exe".to_string());
        let (chinese, _) = c.get_current_mode(token(100));
        assert!(!chinese, "跨进程切入规则应用 → 同步段即回传英文");

        // 重型段已把 active_compat.pid 更新为 100；用户随后手切回中文。
        c.active_compat.lock().unwrap().pid = 100;
        c.state.lock().unwrap().chinese_mode = true;
        let (chinese, _) = c.get_current_mode(token(100));
        assert!(
            chinese,
            "同应用内跳转不得把手切的中文拉回规则的英文——规则是初始值不是锁定"
        );
    }

    /// cancel_on_mode_switch=false（默认）：CapsLock 开着按切换键，保持翻转语义、不动 CapsLock。
    /// （注入路径涉及真实 SendInput，不在单测覆盖，真机验证。）
    #[test]
    fn toggle_mode_keeps_caps_when_cancel_disabled() {
        let c = coord_with(|_| {});
        {
            let mut s = c.state.lock().unwrap();
            s.caps_lock = true;
            s.chinese_mode = true;
        }
        c.handle_toggle_mode();
        let s = c.state.lock().unwrap();
        assert!(s.caps_lock, "配置关不得动 CapsLock");
        assert!(!s.chinese_mode, "配置关保持原翻转语义");
    }

    /// cancel_on_mode_switch=true 但 CapsLock 未开：不注入、正常翻转。
    #[test]
    fn toggle_mode_normal_flip_when_caps_off() {
        let c = coord_with(|cfg| cfg.input.capslock.cancel_on_mode_switch = true);
        c.state.lock().unwrap().chinese_mode = false;
        c.handle_toggle_mode();
        let s = c.state.lock().unwrap();
        assert!(s.chinese_mode, "caps 未开时应正常翻转");
        assert!(!s.caps_lock);
    }

    /// 决策顺序：per-app 表命中优先于全局默认。
    #[test]
    fn initial_mode_decision_order() {
        let c = coord_with(|cfg| {
            cfg.input.default.state_scope = "app".into();
            cfg.input.default.chinese_mode = true;
        });
        c.mode_states
            .lock()
            .unwrap()
            .insert("x.exe".to_string(), false);
        assert!(!c.initial_chinese_mode_for("x.exe"), "表命中优先");
        assert!(c.initial_chinese_mode_for("y.exe"), "未命中落默认");
        assert!(c.initial_chinese_mode_for(""), "空进程名落默认");
    }
}

#[cfg(test)]
mod capslock_tests {
    //! CapsLock 大写模式路由验证（不需要词典文件）。
    //! 覆盖三条路径：字母透传 / 标点透传 / 全角提交。
    use super::*;

    fn coord_cn() -> Arc<Coordinator> {
        let mut cfg = Config::default();
        cfg.input.default.chinese_mode = true;
        // 关闭智能符号，避免 CommitAndHoldComposition 干扰标点断言
        cfg.input.symbol.smart_mode = false;
        Coordinator::new_headless(cfg, None)
    }

    /// 构造最简按键事件
    fn kev(key_code: u32, event_type: u8) -> KeyEventData {
        KeyEventData {
            key_code,
            scan_code: 0,
            modifiers: 0,
            event_type,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        }
    }

    /// 向 coordinator 注入 CapsLock 状态（模拟 C++ 端发 key_up + toggles 位）。
    fn set_caps_lock(c: &Coordinator, on: bool) {
        let mut ev = kev(0x14 /* VK_CAPITAL */, EVENT_KEY_UP);
        ev.toggles = if on { 0x01 } else { 0x00 };
        c.handle_key_event(&ev);
    }

    /// CapsLock 开启期间的按键事件：真实 C++ 每键都带 toggles 快照（GetKeyState 实时值），
    /// caps 开着时 bit0=1。handle_key_event 入口会按此快照校准镜像，故必须如实构造。
    fn kev_caps(key_code: u32, event_type: u8) -> KeyEventData {
        let mut ev = kev(key_code, event_type);
        ev.toggles = 0x01;
        ev
    }

    /// 每键 toggles 快照校准镜像：英文模式（TSF 不吃 VK_CAPITAL）或在其它应用/输入法
    /// 期间切换大写时，专门的状态通知不会到达、镜像陈旧——此校准是 cancel_on_mode_switch
    /// 在"英文+大写"场景能生效的前提（真机回归：切方案取消不了 CapsLock 的根因）。
    #[test]
    fn key_event_toggles_recalibrates_caps_mirror() {
        let c = coord_cn();
        assert!(!c.state.lock().unwrap().caps_lock);
        // 未收到过 VK_CAPITAL 通知，但按键快照显示 caps 已开 → 入口校准。
        c.handle_key_event(&kev_caps(0x41, EVENT_KEY_DOWN));
        assert!(
            c.state.lock().unwrap().caps_lock,
            "入口应按 toggles 快照校准 CapsLock 镜像"
        );
    }

    // ── 字母透传 ────────────────────────────────────────────────────────────

    #[test]
    fn capslock_on_letter_passthrough() {
        let c = coord_cn();
        set_caps_lock(&c, true);
        // 字母 A：中文 + CapsLock + 无 session → 系统产生大写 A，coordinator 不介入
        let action = c.handle_key_event(&kev_caps(0x41, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::PassThrough),
            "中文+CapsLock+字母应透传，实际: {:?}",
            action
        );
    }

    #[test]
    fn capslock_off_letter_enters_chinese_flow() {
        let c = coord_cn();
        // CapsLock 关：字母进入中文输入流
        let action = c.handle_key_event(&kev(0x41, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::UpdateComposition { .. }),
            "CapsLock关+字母应进输入流，实际: {:?}",
            action
        );
    }

    // ── 标点透传（无 input session）──────────────────────────────────────────

    #[test]
    fn capslock_on_punct_no_session_passthrough() {
        let c = coord_cn();
        set_caps_lock(&c, true);
        // VK 0xBC = ','，无 input_buffer → 透传给系统
        let action = c.handle_key_event(&kev_caps(0xBC, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::PassThrough),
            "中文+CapsLock+无session+标点应透传，实际: {:?}",
            action
        );
    }

    #[test]
    fn capslock_off_punct_commits_chinese_punct() {
        let c = coord_cn();
        let action = c.handle_key_event(&kev(0xBC, EVENT_KEY_DOWN));
        // CapsLock 关 + 中文标点：',' → "，"
        let text = match &action {
            KeyAction::InsertText { text, .. } => text.clone(),
            other => panic!("CapsLock关+逗号应上屏中文标点，实际: {:?}", other),
        };
        assert_eq!(text, "，", "实际文本: {:?}", text);
    }

    // ── 智能符号 HoldComposition：press2 的替换语义 ──────────────────────────

    /// press1 把中文符号放进 TSF 组合（hold 预览态），press2 必须返回
    /// `CommitReplacingHeld` 而非普通 `InsertText`。
    ///
    /// 两者在 IPC 载荷上完全同构，C++ 端只能靠这个 action 带的 flags 位判断该
    /// **覆盖**还是**追加** held 符号。退回 InsertText 的后果是 press2 打出「，,」
    /// ——中文符号被并入前缀跟着一起上屏了。
    #[test]
    fn smart_symbol_hold_press2_replaces_held_symbol() {
        let mut cfg = Config::default();
        cfg.input.default.chinese_mode = true;
        cfg.input.symbol.smart_mode = true;
        cfg.input.symbol.smart_method = wind_config::config::SmartMethod::HoldComposition;
        let c = Coordinator::new_headless(cfg, None);

        // press1：空缓冲 + 中文标点 → 中文符号进组合态，等 press2
        let a1 = c.handle_key_event(&kev(0xBC, EVENT_KEY_DOWN));
        match &a1 {
            KeyAction::HoldComposition { text, .. } => {
                assert_eq!(text, "，", "press1 应把中文逗号放进组合")
            }
            other => panic!("press1 应开 hold 组合，实际: {:?}", other),
        }

        // press2：超时窗口内重按同键 → 英文符号 + 替换语义
        let a2 = c.handle_key_event(&kev(0xBC, EVENT_KEY_DOWN));
        match &a2 {
            KeyAction::CommitReplacingHeld { text, .. } => {
                assert_eq!(text, ",", "press2 应换成英文逗号")
            }
            other => panic!(
                "press2 必须返回 CommitReplacingHeld（替换语义），实际: {:?}",
                other
            ),
        }
    }

    // ── 全角模式：提交全角字符 ───────────────────────────────────────────────

    #[test]
    fn capslock_on_fullwidth_letter_commits_uppercase_fullwidth() {
        let c = coord_cn();
        c.state.lock().unwrap().full_width = true;
        set_caps_lock(&c, true);
        // CapsLock ON + 无 Shift + 字母 A → 大写 A → 全角 "Ａ"
        let action = c.handle_key_event(&kev_caps(0x41, EVENT_KEY_DOWN));
        match &action {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(
                    text, "Ａ",
                    "CapsLock+全角+A应输出全角大写，实际: {:?}",
                    text
                );
            }
            other => panic!("CapsLock+全角+字母应上屏，实际: {:?}", other),
        }
    }

    #[test]
    fn capslock_on_fullwidth_shift_letter_commits_lowercase_fullwidth() {
        let c = coord_cn();
        c.state.lock().unwrap().full_width = true;
        set_caps_lock(&c, true);
        // CapsLock ON + Shift + 字母 A → 翻转大小写 → 小写 a → 全角 "ａ"
        let mut ev = kev_caps(0x41, EVENT_KEY_DOWN);
        ev.modifiers = MOD_SHIFT;
        let action = c.handle_key_event(&ev);
        match &action {
            KeyAction::InsertText { text, .. } => {
                assert_eq!(
                    text, "ａ",
                    "CapsLock+Shift+全角+A应输出全角小写，实际: {:?}",
                    text
                );
            }
            other => panic!("CapsLock+Shift+全角+字母应上屏，实际: {:?}", other),
        }
    }

    #[test]
    fn capslock_on_fullwidth_punct_commits_fullwidth() {
        let c = coord_cn();
        c.state.lock().unwrap().full_width = true;
        set_caps_lock(&c, true);
        // ',' 经英全列转换后上屏（不透传）
        let action = c.handle_key_event(&kev_caps(0xBC, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::InsertText { .. }),
            "CapsLock+全角+标点应上屏，实际: {:?}",
            action
        );
    }

    // ── CapsLock 状态切换正确传播 ────────────────────────────────────────────

    #[test]
    fn capslock_toggle_updates_state() {
        let c = coord_cn();
        assert!(
            !c.state.lock().unwrap().caps_lock,
            "初始 CapsLock 应为 false"
        );
        set_caps_lock(&c, true);
        assert!(
            c.state.lock().unwrap().caps_lock,
            "set_caps_lock(true) 后应为 true"
        );
        set_caps_lock(&c, false);
        assert!(
            !c.state.lock().unwrap().caps_lock,
            "set_caps_lock(false) 后应为 false"
        );
    }
}

#[cfg(test)]
mod focus_ownership_tests {
    //! 失焦事件的客户端归属校验：旧宿主迟到的 focus_lost 不得清掉新宿主刚建立的激活态。
    //!
    //! 复现自 2026-07-26 的工具栏缺陷——从 Windows Terminal 切到记事本，记事本
    //! focus_gained 让工具栏显示，86ms 后 Terminal 的 OnKillThreadFocus 才发出 focus_lost，
    //! 把 `ime_active` 清成 false，工具栏闪一下即隐藏。
    use super::*;

    /// 已有宿主 `token` 处于激活态、且焦点在可编辑控件里的协调器。
    fn activated(token: u64) -> Arc<Coordinator> {
        let c = Coordinator::new_headless(Config::default(), None);
        c.push_server.set_active_token(token);
        let mut s = c.state.lock().unwrap();
        s.ime_active = true;
        s.has_edit_context = true;
        drop(s);
        c
    }

    /// 四种 reason 的后果矩阵——本设计的核心契约。
    ///
    /// 三项后果彼此独立，任何一格改错都会复活一个已修的缺陷：
    /// - `CtxLost` 那行的「输入态不清」＝ Excel「首字符不进编码、直接上屏」的防线；
    /// - `DocChanged` 那行的「ime_active 不动」＝ 同宿主换文档不再误关工具栏；
    /// - 各行的 `has_edit_context`＝ 应用内点到非文本框时工具栏能否隐藏。
    #[test]
    fn focus_lost_reason_consequence_matrix() {
        // (reason, ime_active 保留?, has_edit_context 保留?, 输入态保留?)
        let cases = [
            (FocusLostReason::Thread, false, false, false),
            (FocusLostReason::DocChanged, true, true, false),
            (FocusLostReason::CtxLost, true, false, true),
            (FocusLostReason::NoEditCtx, true, false, false),
        ];
        for (reason, keep_ime, keep_edit, keep_input) in cases {
            let c = activated(NOTEPAD);
            c.state.lock().unwrap().input_buffer.push_str("abc");

            c.handle_focus_lost(NOTEPAD, reason);

            let s = c.state.lock().unwrap();
            assert_eq!(
                s.ime_active, keep_ime,
                "{reason:?}: ime_active 应为 {keep_ime}"
            );
            assert_eq!(
                s.has_edit_context, keep_edit,
                "{reason:?}: has_edit_context 应为 {keep_edit}"
            );
            assert_eq!(
                !s.input_buffer.is_empty(),
                keep_input,
                "{reason:?}: 输入态保留应为 {keep_input}"
            );
        }
    }

    /// CtxLost 来自 DocMgr 噪声层（Excel 同一 DocMgr 6ms 内掉了又回），在那里清输入态
    /// 就是「首字符直接上屏」的根因。单独立一条守住这个不变量。
    #[test]
    fn ctx_lost_never_touches_input_buffer() {
        let c = activated(NOTEPAD);
        c.state.lock().unwrap().input_buffer.push_str("nihao");
        c.handle_focus_lost(NOTEPAD, FocusLostReason::CtxLost);
        assert_eq!(
            c.state.lock().unwrap().input_buffer,
            "nihao",
            "CtxLost 绝不可清输入态，否则复发 Excel 首字符丢失"
        );
    }

    /// 陈旧失焦被丢弃时，四种 reason 都不得改动任何**输入/激活**状态。
    ///
    /// ⚠️ 菜单是刻意的例外（见 `stale_focus_lost_still_closes_menu`）：关菜单在 stale 判定
    /// 之前执行，因为「这条失焦不该动激活态」不等于「没发生焦点变动」。往本测试里补断言时
    /// 别顺手把菜单也算进"任何状态"。
    #[test]
    fn stale_focus_lost_is_inert_for_all_reasons() {
        for reason in [
            FocusLostReason::Thread,
            FocusLostReason::DocChanged,
            FocusLostReason::CtxLost,
            FocusLostReason::NoEditCtx,
        ] {
            let c = activated(NOTEPAD);
            c.handle_focus_lost(TERMINAL, reason);
            let s = c.state.lock().unwrap();
            assert!(s.ime_active, "{reason:?}: 陈旧失焦不得清 ime_active");
            assert!(
                s.has_edit_context,
                "{reason:?}: 陈旧失焦不得清 has_edit_context"
            );
        }
    }

    const NOTEPAD: u64 = 0x0000_3644_0000_0001;
    const TERMINAL: u64 = 0x0000_3ECC_0000_0001;

    #[test]
    fn stale_focus_lost_keeps_activation() {
        let c = activated(NOTEPAD);
        c.handle_focus_lost(TERMINAL, FocusLostReason::Thread);
        assert!(
            c.state.lock().unwrap().ime_active,
            "旧宿主迟到的失焦不得清激活态，否则工具栏闪一下即隐藏"
        );
    }

    #[test]
    fn own_focus_lost_clears_activation() {
        let c = activated(NOTEPAD);
        c.handle_focus_lost(NOTEPAD, FocusLostReason::Thread);
        assert!(
            !c.state.lock().unwrap().ime_active,
            "当前活动客户端自己失焦仍须正常清激活态"
        );
    }

    #[test]
    fn legacy_zero_token_still_clears() {
        let c = activated(NOTEPAD);
        c.handle_focus_lost(0, FocusLostReason::Thread);
        assert!(
            !c.state.lock().unwrap().ime_active,
            "旧 DLL 不带 token，保守放行以保持既有行为"
        );
    }

    #[test]
    fn stale_ime_deactivated_keeps_activation() {
        let c = activated(NOTEPAD);
        c.handle_ime_deactivated(TERMINAL);
        assert!(
            c.state.lock().unwrap().ime_active,
            "IME_DEACTIVATED 与 focus_lost 同为异步写，乱序风险相同"
        );
    }

    #[test]
    fn own_ime_deactivated_clears_activation() {
        let c = activated(NOTEPAD);
        c.handle_ime_deactivated(NOTEPAD);
        assert!(!c.state.lock().unwrap().ime_active);
    }

    // ———————————————— 焦点变化关闭菜单 ————————————————
    //
    // 菜单是模态 UI，任何焦点变动都该终结它；而输入态清理必须保守。此前两者绑在同一个
    // `clears_input` 上，于是 CtxLost 豁免 / 陈旧失焦丢弃这两道为保护输入态而设的闸门
    // 顺带把关菜单也吞了——表现为「切走窗口菜单还挂着」。以下几条守住解耦后的语义。

    /// 构造「菜单已打开 `age` 时长」的状态。
    /// `checked_sub` 失败（机器刚启动不足 `age`）时落到 `None`，守卫按"无时间戳=不豁免"
    /// 处理，与本组测试期望的方向一致，故无需特殊处理。
    fn open_menu(c: &Coordinator, age: std::time::Duration) {
        let mut s = c.state.lock().unwrap();
        s.menu_open = true;
        s.menu_opened_at = std::time::Instant::now().checked_sub(age);
    }

    /// 打开够久的菜单
    fn open_menu_settled(c: &Coordinator) {
        open_menu(c, crate::handle_menu::MENU_FOCUS_GUARD * 4);
    }

    /// `CtxLost` 是本组的关键用例：它**不清输入态**（Excel 首字符防线），但**必须关菜单**。
    /// 两者从此各行其是——这正是本次解耦要证明的事。
    #[test]
    fn ctx_lost_closes_menu_but_keeps_input() {
        let c = activated(NOTEPAD);
        c.state.lock().unwrap().input_buffer.push_str("nihao");
        open_menu_settled(&c);

        c.handle_focus_lost(NOTEPAD, FocusLostReason::CtxLost);

        let s = c.state.lock().unwrap();
        assert!(!s.menu_open, "CtxLost 必须关菜单（它是一次真实的焦点变动）");
        assert_eq!(
            s.input_buffer, "nihao",
            "CtxLost 仍绝不可清输入态，否则复发 Excel 首字符丢失"
        );
    }

    /// 陈旧失焦同样要关菜单：判成 stale 只说明「这条失焦不该动激活态」，
    /// 不说明「没发生焦点变动」。跨宿主切换时旧宿主的失焦恒被判 stale，
    /// 若跟着一起丢弃，切走应用后菜单就永远挂着。
    #[test]
    fn stale_focus_lost_still_closes_menu() {
        let c = activated(NOTEPAD);
        open_menu_settled(&c);

        c.handle_focus_lost(TERMINAL, FocusLostReason::Thread);

        let s = c.state.lock().unwrap();
        assert!(!s.menu_open, "陈旧失焦也要关菜单");
        assert!(s.ime_active, "但仍不得清激活态（工具栏闪隐的老缺陷）");
    }

    /// 切进新的可编辑上下文也算外部动作。
    #[test]
    fn focus_gained_closes_menu() {
        let c = activated(NOTEPAD);
        open_menu_settled(&c);
        c.handle_focus_gained(&FocusData {
            x: 10,
            y: 20,
            height: 16,
            composition_start_x: 0,
            composition_start_y: 0,
            client_token: NOTEPAD,
            input_scope_mask: 0,
            disabled: false,
            reason: 0,
            caret_source: wind_ipc::protocol::caret_source::TSF_SELECTION,
            bundle_id: String::new(),
        });
        assert!(!c.state.lock().unwrap().menu_open);
    }

    /// 守卫期：菜单刚弹出时到达的焦点事件是「打开菜单这个动作本身」的尾迹，不是用户切走。
    /// 跨宿主切换时旧宿主 focus_lost 实测晚约 100ms，从任务栏语言栏图标点开菜单正落在这个
    /// 窗口里——不豁免就会「菜单弹出即消失」。
    ///
    /// 用 `CtxLost` 而非 `Thread`：后者走 `clears_input` 分支，那里会无条件复位菜单态
    /// （因为 `notify_ui_hide` 已把窗口隐藏，留 `menu_open=true` 反而状态不一致），
    /// 刻意不受守卫保护，拿它测守卫会测错对象。
    #[test]
    fn menu_survives_focus_event_within_guard() {
        let c = activated(NOTEPAD);
        open_menu(&c, std::time::Duration::from_millis(0));

        c.handle_focus_lost(NOTEPAD, FocusLostReason::CtxLost);

        assert!(
            c.state.lock().unwrap().menu_open,
            "守卫期内的焦点事件不得关掉刚弹出的菜单"
        );
    }

    /// 同一宿主内多个 DocMgr 共用一个 token，一律放行——那层抖动（doc_changed 先发
    /// focus_lost 紧接 focus_gained，间隔 <10ms）由 UI 层 50ms 隐藏防抖吸收，不归本校验管。
    #[test]
    fn same_host_doc_churn_is_not_stale() {
        let c = activated(NOTEPAD);
        assert!(!c.is_stale_focus_event(NOTEPAD, "test"));
    }

    /// 服务端刚启动、尚无任何客户端获焦：无从判定归属，放行。
    #[test]
    fn no_active_client_is_not_stale() {
        let c = Coordinator::new_headless(Config::default(), None);
        assert!(!c.is_stale_focus_event(TERMINAL, "test"));
    }
}

#[cfg(test)]
mod per_app_compat_tests {
    //! per-app 兼容规则：自动配对开关、智能符号方案、光标坐标校正。
    use super::*;
    use wind_config::config::SmartMethod;

    fn coord_with(cfg: Config) -> Arc<Coordinator> {
        Coordinator::new_headless(cfg, None)
    }

    /// CaretData 无 `Default`，测试里显式构造（字段少，且显式写出更能看清哪些参与变换）。
    fn caret(x: i32, y: i32, height: i32, cs_x: i32, cs_y: i32) -> CaretData {
        CaretData {
            x,
            y,
            height,
            composition_start_x: cs_x,
            composition_start_y: cs_y,
            source: 0,
        }
    }

    fn pair_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.input.auto_pair.chinese = true;
        cfg.input.auto_pair.english = true;
        cfg.input.auto_pair.chinese_pairs = vec!["（）".to_string()];
        cfg.input.auto_pair.english_pairs = vec!["()".to_string()];
        cfg
    }

    /// per-app 关闭后，`active_pairs` 在**中英两种标点态**都必须返回 None。
    ///
    /// 分别断言两种标点态而不是只测一种：全局开关本来就是 chinese / english 两个独立字段，
    /// 只在其中一条上加闸门是本仓反复出现的「半截修复」形态。
    #[test]
    fn auto_pair_rule_off_kills_both_punct_modes() {
        let c = coord_with(pair_cfg());
        assert!(c.active_pairs(true).is_some(), "默认（未配规则）应跟随全局");
        assert!(c.active_pairs(false).is_some());

        c.active_compat.lock().unwrap().auto_pair = Some(false);
        assert!(c.active_pairs(true).is_none(), "中文标点态应被关掉");
        assert!(c.active_pairs(false).is_none(), "英文标点态应被关掉");

        // 显式启用 = 跟随全局的开关，不是无条件开。
        c.active_compat.lock().unwrap().auto_pair = Some(true);
        assert!(c.active_pairs(true).is_some());
    }

    /// `is_auto_pair_char` 建立在 `active_pairs` 之上，规则关闭后必须一并失效——
    /// 它是「智能符号与自动配对互斥」的判据，若还认为字符参与配对，智能符号会被误让位。
    #[test]
    fn auto_pair_rule_off_releases_smart_symbol_interlock() {
        let c = coord_with(pair_cfg());
        c.state.lock().unwrap().chinese_punct = true;
        {
            let state = c.state.lock().unwrap();
            assert!(c.is_auto_pair_char(&state, '（'), "默认应认为参与配对");
        }

        c.active_compat.lock().unwrap().auto_pair = Some(false);
        {
            let state = c.state.lock().unwrap();
            assert!(!c.is_auto_pair_char(&state, '（'), "规则关闭后互锁应解除");
        }
    }

    /// 光标坐标校正：两个消费点（`apply_focus_caret` / `handle_caret_update`）共用
    /// `apply_caret_compat`，此处直接锁住那个变换本身。
    #[test]
    fn caret_offset_shifts_coordinates() {
        let c = coord_with(Config::default());
        {
            let mut ac = c.active_compat.lock().unwrap();
            ac.caret_offset_x = -3;
            ac.caret_offset_y = 7;
        }
        let mut data = caret(100, 200, 20, 0, 0);
        c.apply_caret_compat(&mut data);
        assert_eq!((data.x, data.y), (97, 207));
        assert_eq!(data.height, 20, "偏移不应改动行高");
        // compStart 为 0 表示"未提供"，不能被平移成一个假坐标。
        assert_eq!((data.composition_start_x, data.composition_start_y), (0, 0));

        // compStart 有真值时随之平移，保持与 caret 的锚点关系。
        let mut with_cs = caret(100, 200, 20, 50, 180);
        c.apply_caret_compat(&mut with_cs);
        assert_eq!(
            (with_cs.composition_start_x, with_cs.composition_start_y),
            (47, 187)
        );
    }

    /// 零偏移必须是彻底的 no-op：未配规则的应用绝不能因为这条链路而坐标漂移。
    #[test]
    fn caret_offset_zero_is_noop() {
        let c = coord_with(Config::default());
        let orig = caret(100, 200, 20, 50, 180);
        let mut data = orig;
        c.apply_caret_compat(&mut data);
        assert_eq!((data.x, data.y), (orig.x, orig.y));
        assert_eq!(
            (data.composition_start_x, data.composition_start_y),
            (orig.composition_start_x, orig.composition_start_y)
        );
    }

    /// 智能符号方案：per-app 覆盖优先，未配则跟随全局。
    #[test]
    fn smart_method_per_app_overrides_global() {
        let mut cfg = Config::default();
        cfg.input.symbol.smart_method = SmartMethod::DeleteReplace;
        let c = coord_with(cfg);
        assert_eq!(c.effective_smart_method(), SmartMethod::DeleteReplace);

        c.active_compat.lock().unwrap().smart_method = Some(SmartMethod::HoldComposition);
        assert_eq!(c.effective_smart_method(), SmartMethod::HoldComposition);

        c.active_compat.lock().unwrap().smart_method = None;
        assert_eq!(
            c.effective_smart_method(),
            SmartMethod::DeleteReplace,
            "清除规则应回到全局值"
        );
    }
}

#[cfg(test)]
mod input_diag_tests {
    //! last_input_diag 存储 + 密码框强制英文抑制。
    use super::*;
    use crate::input_diag::InputDiagReason;

    fn test_coordinator() -> Arc<Coordinator> {
        Coordinator::new_headless(Config::default(), None)
    }

    #[test]
    fn password_scope_sets_suppress_and_state() {
        let c = test_coordinator();
        c.apply_input_diag(1234, false, /*reason*/ 2, 1 << 31);
        assert!(
            c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        let d = c.last_input_diag.lock().unwrap();
        assert_eq!(d.reason, InputDiagReason::InputScopePassword);
        assert_eq!(d.pid, 1234);
    }

    #[test]
    fn suppress_cleared_when_mask_clears() {
        let c = test_coordinator();
        c.apply_input_diag(1, false, 2, 1 << 31);
        c.apply_input_diag(1, false, 0, 0);
        assert!(
            !c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn disabled_policy_no_suppress_when_off() {
        let c = test_coordinator();
        c.password_suppress_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
        c.apply_input_diag(1, false, 2, 1 << 31);
        assert!(
            !c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    /// 构造最简按键事件（对齐 capslock_tests::kev 的写法）。
    fn kev(key_code: u32, event_type: u8) -> KeyEventData {
        KeyEventData {
            key_code,
            scan_code: 0,
            modifiers: 0,
            event_type,
            toggles: 0,
            event_seq: 0,
            prev_char: 0,
        }
    }

    /// 真实输入路径验证：密码框抑制期间字母键必须透传（强制英文），
    /// 解除抑制后同一按键应回到中文组词流——防止「只改图标不拦输入」的回归。
    #[test]
    fn password_suppress_forces_english_passthrough() {
        let mut cfg = Config::default();
        cfg.input.default.chinese_mode = true;
        let c = Coordinator::new_headless(cfg, None);
        assert!(
            c.state.lock().unwrap().chinese_mode,
            "前置条件：应处于中文模式"
        );

        let pid = 4321u32;
        c.apply_input_diag(pid, false, 2, 1 << 31);
        assert!(
            c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed),
            "前置条件：密码框抑制应已置位"
        );
        let action = c.handle_key_event(&kev(0x41 /* VK_A */, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::PassThrough),
            "密码框抑制期间字母键应强制透传（英文），实际: {:?}",
            action
        );
        assert!(
            c.state.lock().unwrap().chinese_mode,
            "抑制不应改动 chinese_mode 持久值（图标保持不变）"
        );

        // 解除抑制：mask 清零。
        c.apply_input_diag(pid, false, 0, 0);
        assert!(
            !c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        let action = c.handle_key_event(&kev(0x41 /* VK_A */, EVENT_KEY_DOWN));
        assert!(
            !matches!(action, KeyAction::PassThrough),
            "解除抑制后字母键应进入中文组词流，不应透传，实际: {:?}",
            action
        );
    }

    #[test]
    fn toggle_hud_flips_visibility() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = test_coordinator();
        assert!(!c.input_diag_hud_visible.load(Relaxed));
        c.toggle_input_diag_hud();
        assert!(c.input_diag_hud_visible.load(Relaxed));
        c.toggle_input_diag_hud();
        assert!(!c.input_diag_hud_visible.load(Relaxed));
    }

    #[test]
    fn toggle_password_suppress_flips_enabled() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = test_coordinator();
        assert!(c.password_suppress_enabled.load(Relaxed)); // 默认开
        c.toggle_password_suppress();
        assert!(!c.password_suppress_enabled.load(Relaxed));
    }

    #[test]
    fn focus_lost_clears_password_suppress() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = test_coordinator();
        c.apply_input_diag(1234, false, 2, 1 << 31);
        assert!(
            c.password_suppress.load(Relaxed),
            "前置条件：密码框抑制应已置位"
        );
        c.handle_focus_lost(0, FocusLostReason::Thread);
        assert!(
            !c.password_suppress.load(Relaxed),
            "失焦后应清除密码框抑制态，避免残留到下次 focus_gained 之前"
        );
    }

    /// 回归（2026-07-27）：Chromium 网页密码框必须强制英文，**即便上报的 disabled=true**。
    ///
    /// 此前判据里有一条 `&& !disabled`，本意是「compartment 禁用时 DLL 已全放行、抑制 moot」。
    /// 但 DLL 放行看的是**线程级** KEYBOARD_DISABLED，而 Windows 侧当时往 `disabled` 字段传的
    /// 是 **context 级**的 `_focusIsPassword` —— 网页密码框恒为 true，于是抑制被自我否决：
    /// 键没被放行、中文照打，高级菜单的开关看着像坏了。
    ///
    /// ⚠ 本用例的要害是 `disabled=true`。改动前所有密码框用例都传 false（macOS 只发 mask、
    /// 不发 disabled，走的正是那条路），恰好绕开失效分支，所以旧代码测试全绿。
    /// **动这条判据时必须保住这个取值**，否则回归保护形同虚设。
    #[test]
    fn password_scope_suppresses_even_when_disabled_flag_set() {
        let mut cfg = Config::default();
        cfg.input.default.chinese_mode = true;
        let c = Coordinator::new_headless(cfg, None);

        // disabled=true + 密码位：正是 Chromium 网页密码框改动前的上报组合。
        c.apply_input_diag(4321, true, 1, 1 << 31);
        assert!(
            c.password_suppress
                .load(std::sync::atomic::Ordering::Relaxed),
            "context 级密码框（disabled=true）必须触发强制英文抑制"
        );

        let action = c.handle_key_event(&kev(0x41 /* VK_A */, EVENT_KEY_DOWN));
        assert!(
            matches!(action, KeyAction::PassThrough),
            "密码框里字母键应强制透传为英文，实际: {:?}",
            action
        );
        assert!(
            c.state.lock().unwrap().chinese_mode,
            "抑制不应改动 chinese_mode 持久值（图标保持不变）"
        );
    }

    /// disabled 只参与 `reason_from` 的展示推导，**不参与** suppress 决策——单一来源。
    ///
    /// 本用例取代旧的 `compartment_disabled_does_not_set_suppress`：那条断言同样的输入
    /// （disabled=true + 密码位）**不该**置 suppress，把「compartment 禁用 ⇒ DLL 已放行所有键」
    /// 这条前提固化成了契约。前提只对**线程级** KEYBOARD_DISABLED 成立，而当时 Windows 侧
    /// 往该字段传的是 context 级的密码框判定 —— 契约锁住的恰是 bug 本身。reason 断言保留。
    #[test]
    fn disabled_flag_drives_reason_display_not_suppression() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = test_coordinator();

        // 线程级禁用 + 密码位：reason 展示为 compartment（优先级最高），suppress 仍置位。
        // suppress=true 在此场景无害：DLL 已全放行，引擎收不到键，取值无从被观测。
        c.apply_input_diag(1, true, 1, 1 << 31);
        assert_eq!(
            c.last_input_diag.lock().unwrap().reason,
            crate::input_diag::InputDiagReason::CompartmentDisabled,
            "disabled=true 时 reason 展示应为 compartment"
        );
        assert!(
            c.password_suppress.load(Relaxed),
            "reason 的展示优先级不应反过来否决抑制决策"
        );

        // 无密码位：无论 disabled 与否都不抑制。
        c.apply_input_diag(1, true, 1, 0);
        assert!(
            !c.password_suppress.load(Relaxed),
            "mask 无密码位时不应抑制"
        );
    }

    /// 策略开关关闭后，即便命中密码位也不抑制（高级菜单的逃生阀必须真的管用）。
    #[test]
    fn disabled_switch_defeats_password_scope() {
        use std::sync::atomic::Ordering::Relaxed;
        let c = test_coordinator();
        c.toggle_password_suppress();
        assert!(
            !c.password_suppress_enabled.load(Relaxed),
            "前置条件：开关已关"
        );

        c.apply_input_diag(1, true, 1, 1 << 31);
        assert!(
            !c.password_suppress.load(Relaxed),
            "开关关闭时密码框不应强制英文"
        );
        c.apply_input_diag(1, false, 2, 1 << 63);
        assert!(
            !c.password_suppress.load(Relaxed),
            "数字密码位同样受开关约束"
        );
    }
}

#[cfg(test)]
mod ext_envelope_tests {
    //! 扩展信封 `pos.*` / `shot.*` 的 body 解析与文案，以及滚轮的高亮移动。
    use super::*;

    fn coord() -> Arc<Coordinator> {
        Coordinator::new_headless(Config::default(), None)
    }

    #[test]
    fn decodes_well_formed_point() {
        assert_eq!(
            decode_ext_point(br#"{"x":123,"y":-456}"#),
            Some((123, -456))
        );
        // 多余字段照常忽略——JSON body 的向前兼容就靠这条。
        assert_eq!(
            decode_ext_point(br#"{"x":1,"y":2,"screen":"builtin"}"#),
            Some((1, 2))
        );
    }

    /// 滚轮 = 上下键调整高亮项，到页边界翻到相邻页。
    ///
    /// 回归意义：`handle_candidate_scroll` 长期是 trait 上的空实现，Windows 的
    /// host-render DLL 一直在发这个帧、服务端收下什么也不做——滚轮在两个平台都无效。
    #[test]
    fn scroll_moves_highlight_and_crosses_pages() {
        use wind_candidate::Candidate;
        let c = coord();
        let per_page = {
            let mut s = c.state.lock().unwrap();
            s.candidates = (0..12)
                .map(|i| Candidate {
                    text: i.to_string(),
                    ..Default::default()
                })
                .collect();
            s.selected_index = 0;
            s.current_page = 0;
            drop(s);
            c.per_page(None)
        };
        assert!(per_page >= 2 && per_page < 12, "本用例要求每页 2..12 项");

        // 下滚一格 → 高亮下移一项（不是翻一页）
        c.handle_candidate_scroll(-120);
        assert_eq!(c.state.lock().unwrap().selected_index, 1);

        // 一路滚到页尾再一格 → 跨到下一页首项
        for _ in 0..(per_page - 1) {
            c.handle_candidate_scroll(-120);
        }
        {
            let s = c.state.lock().unwrap();
            assert_eq!(s.current_page, 1, "页尾再下滚应翻到下一页");
            assert_eq!(s.selected_index, 0, "跨页后高亮落在首项");
        }

        // 上滚回卷到上一页末项
        c.handle_candidate_scroll(120);
        {
            let s = c.state.lock().unwrap();
            assert_eq!(s.current_page, 0);
            assert_eq!(s.selected_index, per_page - 1);
        }
    }

    /// 触控板一次轻扫的 delta 可能不足一格（<120）——整除会得 0，滚轮就"滚不动"。
    #[test]
    fn scroll_with_sub_notch_delta_still_moves_one() {
        use wind_candidate::Candidate;
        let c = coord();
        {
            let mut s = c.state.lock().unwrap();
            s.candidates = (0..5)
                .map(|i| Candidate {
                    text: i.to_string(),
                    ..Default::default()
                })
                .collect();
        }
        c.handle_candidate_scroll(-13);
        assert_eq!(c.state.lock().unwrap().selected_index, 1);
    }

    /// 惯性滚动一次可能带来极大的 delta；不设上限会一口气跳过几十项并疯狂重绘。
    #[test]
    fn scroll_is_capped_per_event() {
        use wind_candidate::Candidate;
        let c = coord();
        {
            let mut s = c.state.lock().unwrap();
            s.candidates = (0..200)
                .map(|i| Candidate {
                    text: i.to_string(),
                    ..Default::default()
                })
                .collect();
        }
        c.handle_candidate_scroll(-120 * 50);
        let s = c.state.lock().unwrap();
        let moved = s.current_page * c.per_page(None) + s.selected_index;
        assert_eq!(moved, 5, "单次事件最多移动 MAX_NOTCHES 项");
    }

    /// 无候选时不得有任何动作（也不该 panic）。
    #[test]
    fn scroll_without_candidates_is_noop() {
        let c = coord();
        c.handle_candidate_scroll(-120);
        assert_eq!(c.state.lock().unwrap().selected_index, 0);
    }

    /// 「截图所有窗口」：两侧数量相加，合成一条 Toast。
    ///
    /// 分开弹是最容易写出来的实现，也是最烦人的——候选窗 + 气泡 + 提示 + Toast 全可见时
    /// 会连弹四条通知。`already` 由服务端放进请求、`.app` 原样带回，就是为了不为这一次
    /// 往返在任何一边留状态。
    #[test]
    fn shot_all_sums_both_sides_into_one_message() {
        let v = serde_json::json!({
            "mode": "all",
            "dir": "/tmp/shots",
            "already": 1,                    // 候选窗（服务进程截的）
            "already_clipboard": true,
            "results": [
                {"target": "status_tip", "ok": true},
                {"target": "tooltip", "ok": false, "reason": "not_visible"},
                {"target": "toast", "ok": true},
            ],
        });
        let (msg, kind) = super::shot_result_message(&v);
        assert_eq!(msg, "已保存 3 张截图（候选已复制到剪贴板）\n/tmp/shots");
        assert!(matches!(kind, ToastKind::Success));
    }

    /// 一个都没截到不是错误：用户可能就是在没有任何浮窗时点的菜单。
    #[test]
    fn shot_all_with_nothing_visible_is_info() {
        let v = serde_json::json!({
            "mode": "all", "already": 0, "dir": "/tmp",
            "results": [{"target": "status_tip", "ok": false, "reason": "not_visible"}],
        });
        let (msg, kind) = super::shot_result_message(&v);
        assert_eq!(msg, "没有可见窗口可截图");
        assert!(matches!(kind, ToastKind::Info));
    }

    /// 单窗截图的三种结局：成功带路径、不可见（Info 不是 Error）、真失败。
    #[test]
    fn shot_single_wording_by_outcome() {
        let mk = |r: serde_json::Value| {
            super::shot_result_message(&serde_json::json!({ "results": [r] }))
        };
        let (msg, kind) = mk(serde_json::json!({
            "target": "tooltip", "ok": true, "clipboard": true, "path": "/tmp/t.png"
        }));
        assert_eq!(msg, "悬停提示已截图（已复制到剪贴板）\n/tmp/t.png");
        assert!(matches!(kind, ToastKind::Success));

        let (msg, kind) = mk(serde_json::json!({
            "target": "status_tip", "ok": false, "reason": "not_visible"
        }));
        assert_eq!(msg, "状态提示气泡未显示，无法截图");
        assert!(matches!(kind, ToastKind::Info), "不可见不该报成错误");

        let (msg, kind) = mk(serde_json::json!({
            "target": "status_tip", "ok": false, "reason": "render_failed"
        }));
        assert_eq!(msg, "截图失败：render_failed");
        assert!(matches!(kind, ToastKind::Error));
    }

    /// 缺字段 / 非整数 / 越界 / 不是 JSON —— 一律 None。
    ///
    /// 关键在于**不能取 0 兜底**：`(0, 0)` 会被当成合法坐标落盘成 custom_x/y，
    /// 候选窗下次就跑到屏幕左上角，而用户只是拖了一下。
    #[test]
    fn rejects_malformed_bodies() {
        for bad in [
            &br#"{"x":1}"#[..],            // 缺 y
            br#"{"y":1}"#,                 // 缺 x
            br#"{"x":1.5,"y":2}"#,         // 非整数
            br#"{"x":"1","y":"2"}"#,       // 字符串
            br#"{"x":99999999999,"y":0}"#, // 越出 i32
            br#"[1,2]"#,                   // 不是对象
            b"not json",
            b"",
        ] {
            assert_eq!(decode_ext_point(bad), None, "body={:?} 应被拒", bad);
        }
    }
}

#[cfg(test)]
mod hover_reset_tests {
    //! 鼠标悬停目标（`Coordinator::hover_index`）的**清空覆盖面**。
    //!
    //! 本组测试锁的是一个曾经静默存在的缺陷：悬停目标此前是 `State` 的字段，清空只能由每个
    //! 候选装填点手工执行。主路径 `update_candidates` 做了，overlay 各路径（特殊模式 / 临拼 /
    //! 临英 / 混输·快捷输入 / 拼音组合复位）全部漏了——悬停高亮与 tooltip 于是跨按键、跨组合、
    //! 跨模式存活，用户看到的是「候选窗再次弹出时，鼠标没动却已经有一项被高亮并弹出了 tooltip」。
    //!
    //! ★ 该缺陷在主路径上**物理不可观测**：普通输入每敲一键都重走 `update_candidates`，
    //! 残留被持续覆盖掉。所以只测普通输入路径等于什么都没测——下面必须逐个 overlay 入口点名。
    use super::*;

    fn coord() -> Arc<Coordinator> {
        Coordinator::new_headless(Config::default(), None)
    }

    /// 造一页候选，好让 `mouse_hover` 有合法落点（它对空候选另有分支，见下面的专项测试）。
    fn seed_candidates(c: &Coordinator, n: usize) {
        let mut st = c.state.lock().unwrap();
        st.candidates = (0..n)
            .map(|i| wind_candidate::Candidate {
                text: i.to_string(),
                ..Default::default()
            })
            .collect();
    }

    /// **反向对照**：悬停确实设得上。
    ///
    /// 少了本条，下面所有「××之后归零」都可能因为悬停压根没设上而全部假绿——本仓
    /// 「测了个恒为真的断言」已经栽过不止一次。
    #[test]
    fn mouse_hover_sets_target() {
        let c = coord();
        seed_candidates(&c, 5);
        c.mouse_hover(2);
        assert_eq!(c.hover_target(), 2, "有候选时悬停应设得上");
    }

    /// 候选窗隐藏 = 会话终结，悬停必须归零。
    ///
    /// 这是根治点：`notify_ui_hide` 有 40+ 个调用点，把清空放在这里，任何一条隐藏通路都覆盖到。
    /// （UI 侧 `CandidateMouse::reset_hover` 清的是防抖闸门，决定何时**发**事件；
    /// 高亮与 tooltip 读的是本值，两者不是一回事。）
    #[test]
    fn notify_ui_hide_clears_hover() {
        let c = coord();
        seed_candidates(&c, 5);
        c.mouse_hover(2);
        c.notify_ui_hide();
        assert_eq!(c.hover_target(), -1, "候选窗隐藏后悬停必须归零");
    }

    /// 每一个 overlay 候选装填入口，装填后都必须已清除悬停。
    ///
    /// 逐个点名而不是抽样：它们是**平行的独立落点**，历史上正是「主路径做了、其余全漏」。
    /// 新增候选来源时若忘了 `reset_candidate_view`，本测试不会自动覆盖到——但把入口逐个
    /// 列在这里，至少让「又多了一个装填点」这件事在评审时看得见。
    #[test]
    fn every_overlay_refill_clears_hover() {
        // (入口名, 调用) —— 名字进断言消息，失败时直接指出是哪条路径漏了。
        let cases: Vec<(&str, fn(&Coordinator, &mut State))> = vec![
            ("特殊模式 update_special_candidates", |c, st| {
                let _ = c.update_special_candidates(st);
            }),
            ("临时拼音 update_temp_pinyin_candidates", |c, st| {
                c.update_temp_pinyin_candidates(st)
            }),
            ("临时英文 update_temp_english_candidates", |c, st| {
                c.update_temp_english_candidates(st)
            }),
            ("混输·快捷输入 update_mix_candidates", |c, st| {
                c.update_mix_candidates(st)
            }),
            ("拼音组合复位 reset_pinyin_composition", |c, st| {
                c.reset_pinyin_composition(st)
            }),
        ];
        for (name, refill) in cases {
            let c = coord();
            seed_candidates(&c, 5);
            c.mouse_hover(2);
            assert_eq!(c.hover_target(), 2, "{name}：前置条件——悬停应已设上");

            let mut st = c.state.lock().unwrap();
            refill(&c, &mut st);
            assert_eq!(c.hover_target(), -1, "{name}：候选重新装填后悬停必须清除");
        }
    }

    /// 「鼠标移出候选窗」这条 `Hover(-1)` 在候选恰好已清空时**不能被吞掉**。
    ///
    /// 旧实现在 `mouse_hover` 开头对空候选直接 early-return，于是离开事件丢失、旧值残留。
    /// 「候选没了」正是最该归零的时刻，拿它当早退条件恰好搞反了。
    #[test]
    fn leaving_clears_hover_even_when_candidates_already_empty() {
        let c = coord();
        seed_candidates(&c, 5);
        c.mouse_hover(2);
        c.state.lock().unwrap().candidates.clear();

        c.mouse_hover(-1);
        assert_eq!(c.hover_target(), -1, "候选已空时的离开事件不能被吞掉");
    }

    /// 键盘操作（移动高亮 / 翻页）同样取消悬停：两种高亮并存时视觉上会有两个「选中项」。
    /// 此前这四处是仅有的清空点之一，改造成 `clear_hover` 后需确认语义没丢。
    #[test]
    fn keyboard_navigation_clears_hover() {
        let c = coord();
        seed_candidates(&c, 5);
        c.mouse_hover(2);
        let mut st = c.state.lock().unwrap();
        assert!(c.move_down(&mut st), "前置条件——应能下移");
        assert_eq!(c.hover_target(), -1, "键盘移动高亮后悬停应取消");
    }
}
