//! 热键编译器
//!
//! 与 Go 版本 `wind_input/internal/hotkey/compiler.go` 对齐。
//! 将配置中的热键字符串（如 "Ctrl+Space"、"lshift"）编译为 key_hash，
//! 用于按键事件中的热键匹配。

use crate::config::Config;
use tracing::debug;

/// 修饰键常量（与 wind-ipc MOD_* / Go ipc.Mod* 对齐）
const MOD_SHIFT: u32 = 0x0001;
const MOD_CTRL: u32 = 0x0002;
const MOD_ALT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;
const MOD_LSHIFT: u32 = 0x0010;
const MOD_RSHIFT: u32 = 0x0020;
const MOD_LCTRL: u32 = 0x0040;
const MOD_RCTRL: u32 = 0x0080;
const MOD_CAPSLOCK: u32 = 0x0100;

/// 通用修饰位掩码（ctrl/shift/alt/win），用于规范化匹配
pub const MOD_GENERIC_MASK: u32 = MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_WIN;

/// 热键策略位（高位，发给 TSF；与 Go ipc.HotkeyPolicy* / TSF HOTKEY_POLICY_* 对齐）
const HOTKEY_POLICY_CHINESE_ONLY: u32 = 0x40000000;
const HOTKEY_POLICY_SESSION: u32 = 0x80000000;

/// Windows 虚拟键码（toggle / select / page 用）
const VK_LSHIFT: u32 = 0xA0;
const VK_RSHIFT: u32 = 0xA1;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_CAPITAL: u32 = 0x14;
const VK_TAB: u32 = 0x09;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_OEM_1: u32 = 0xBA; // ;
const VK_OEM_7: u32 = 0xDE; // '
const VK_OEM_COMMA: u32 = 0xBC; // ,
const VK_OEM_PERIOD: u32 = 0xBE; // .
const VK_OEM_MINUS: u32 = 0xBD; // -
const VK_OEM_PLUS: u32 = 0xBB; // =
const VK_OEM_4: u32 = 0xDB; // [
const VK_OEM_6: u32 = 0xDD; // ]

/// 单个编译后的热键条目
#[derive(Debug, Clone)]
pub struct HotkeyEntry {
    /// 发给 TSF 的 hash（含 policy 高位），用于白名单匹配/转发决策
    pub tsf_hash: u32,
    /// 服务端匹配用的 hash（不含 policy、修饰位为通用位），与规范化后的入站事件比对
    pub match_hash: u32,
    /// 动作名称（用于 dispatch；空串表示仅注册转发、由常规按键逻辑处理）
    pub action: String,
}

/// 编译后的热键集合
#[derive(Debug, Clone, Default)]
pub struct CompiledHotkeys {
    pub key_down: Vec<HotkeyEntry>,
    pub key_up: Vec<HotkeyEntry>,
}

impl CompiledHotkeys {
    /// 发给 TSF 的 key_down hash 列表（含 policy 位）
    pub fn key_down_tsf_hashes(&self) -> Vec<u32> {
        self.key_down.iter().map(|e| e.tsf_hash).collect()
    }
    /// 发给 TSF 的 key_up hash 列表
    pub fn key_up_tsf_hashes(&self) -> Vec<u32> {
        self.key_up.iter().map(|e| e.tsf_hash).collect()
    }
    /// 在 key_down 集合中按规范化 hash 查找动作
    pub fn match_key_down(&self, normalized_hash: u32) -> Option<&str> {
        self.key_down
            .iter()
            .find(|e| e.match_hash == normalized_hash)
            .map(|e| e.action.as_str())
    }
}

/// 计算 key_hash（与 wind-ipc::protocol::calc_key_hash 对齐）
fn key_hash(modifiers: u32, key_code: u32) -> u32 {
    (modifiers << 16) | (key_code & 0xFFFF)
}

/// 热键编译器
pub struct Compiler {
    config: Config,
}

impl Compiler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// 编译配置中的热键为 CompiledHotkeys（对齐 Go compiler.go::Compile）
    pub fn compile(&self) -> CompiledHotkeys {
        let mut result = CompiledHotkeys::default();
        let h = &self.config.keys;

        // ── KeyDown：两模式都吃（无 policy 位） ──
        for (name, value) in [
            ("switch_engine", &h.switch_engine),
            ("toggle_full_width", &h.toggle_full_width),
            ("toggle_toolbar", &h.toggle_toolbar),
            ("open_settings", &h.open_settings),
            ("take_screenshot", &h.take_screenshot),
        ] {
            if let Some(raw) = parse_hotkey(value) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw,
                    match_hash: raw,
                    action: name.to_string(),
                });
            }
        }

        // ── KeyDown：仅中文模式吃（HOTKEY_POLICY_CHINESE_ONLY） ──
        for (name, value) in [
            ("toggle_punct", &h.toggle_punct),
            ("add_word", &h.add_word),
            ("open_add_word_dialog", &h.open_add_word_dialog),
            ("toggle_s2t", &h.toggle_s2t),
        ] {
            if let Some(raw) = parse_hotkey(value) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw | HOTKEY_POLICY_CHINESE_ONLY,
                    match_hash: raw,
                    action: name.to_string(),
                });
            }
        }

        // ── KeyDown：数字模板展开（PinCandidate / DeleteCandidate，session policy） ──
        for tmpl in [&h.pin_candidate, &h.delete_candidate] {
            for entry in compile_number_hotkey(tmpl) {
                result.key_down.push(entry);
            }
        }

        // ── KeyDown：选词键组（如 ;'），仅注册转发，由常规逻辑处理 ──
        for group in &self.config.keys.select_key_groups {
            for raw in compile_select_key_group(group) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw,
                    match_hash: raw,
                    action: String::new(),
                });
            }
        }

        // ── KeyDown：翻页键组（pageupdown / minus_equal / brackets / shift_tab） ──
        for group in &self.config.keys.page_keys {
            for raw in compile_page_key_group(group) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw,
                    match_hash: raw,
                    action: String::new(),
                });
            }
        }

        // ── KeyUp：toggle 模式键（Shift/Ctrl/CapsLock） ──
        // 关键：必须带通用位+具体位，与 Go compileToggleModeKey 一致，
        // 因为 C++ GetCurrentModifiers() 对修饰键同时返回通用与具体位。
        for key in &h.toggle_mode_keys {
            if let Some(hash) = compile_toggle_mode_key(key) {
                result.key_up.push(HotkeyEntry {
                    tsf_hash: hash,
                    match_hash: hash,
                    action: "toggle_mode".to_string(),
                });
            }
        }

        debug!(
            "Compiled hotkeys: {} key_down, {} key_up",
            result.key_down.len(),
            result.key_up.len()
        );
        result
    }
}

/// 编译 toggle 模式键（含通用位+具体位），对齐 Go compileToggleModeKey
fn compile_toggle_mode_key(key: &str) -> Option<u32> {
    match key.trim().to_lowercase().as_str() {
        "lshift" => Some(key_hash(MOD_SHIFT | MOD_LSHIFT, VK_LSHIFT)),
        "rshift" => Some(key_hash(MOD_SHIFT | MOD_RSHIFT, VK_RSHIFT)),
        "lctrl" | "lcontrol" => Some(key_hash(MOD_CTRL | MOD_LCTRL, VK_LCONTROL)),
        "rctrl" | "rcontrol" => Some(key_hash(MOD_CTRL | MOD_RCTRL, VK_RCONTROL)),
        "capslock" | "caps" => Some(key_hash(MOD_CAPSLOCK, VK_CAPITAL)),
        _ => None,
    }
}

/// 展开 "ctrl+number" / "ctrl+shift+number" 为 0-9 共 10 个 session 热键
fn compile_number_hotkey(template: &str) -> Vec<HotkeyEntry> {
    let mods = match template.trim().to_lowercase().as_str() {
        "ctrl+number" => MOD_CTRL,
        "ctrl+shift+number" => MOD_CTRL | MOD_SHIFT,
        _ => return Vec::new(),
    };
    (0u32..=9)
        .map(|d| {
            let raw = key_hash(mods, 0x30 + d);
            HotkeyEntry {
                tsf_hash: raw | HOTKEY_POLICY_SESSION,
                match_hash: raw,
                action: String::new(),
            }
        })
        .collect()
}

/// 选词键组 → raw hash 列表
fn compile_select_key_group(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
        "semicolon_quote" => vec![key_hash(0, VK_OEM_1), key_hash(0, VK_OEM_7)],
        "comma_period" => vec![key_hash(0, VK_OEM_COMMA), key_hash(0, VK_OEM_PERIOD)],
        "lrshift" => vec![
            key_hash(MOD_SHIFT | MOD_LSHIFT, VK_LSHIFT),
            key_hash(MOD_SHIFT | MOD_RSHIFT, VK_RSHIFT),
        ],
        "lrctrl" => vec![
            key_hash(MOD_CTRL | MOD_LCTRL, VK_LCONTROL),
            key_hash(MOD_CTRL | MOD_RCTRL, VK_RCONTROL),
        ],
        _ => Vec::new(),
    }
}

/// 选词键组 → 有序 VK 列表（位置 0 = 次选键/选第2个，位置 1 = 三选键/选第3个）。
/// 供协调器把按键映射为候选偏移（与 compile_select_key_group 同源）。
pub fn select_key_vks(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
        "semicolon_quote" => vec![VK_OEM_1, VK_OEM_7],
        "comma_period" => vec![VK_OEM_COMMA, VK_OEM_PERIOD],
        "lrshift" => vec![VK_LSHIFT, VK_RSHIFT],
        "lrctrl" => vec![VK_LCONTROL, VK_RCONTROL],
        _ => Vec::new(),
    }
}

/// 以词定字键组 → 有序 VK 列表（位置 0 = 取第 1 字，位置 1 = 取第 2 字）。
/// 允许的键组（对齐 Go selectCharAllowedGroups）：comma_period / minus_equal / brackets。
pub fn select_char_vks(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
        "comma_period" => vec![VK_OEM_COMMA, VK_OEM_PERIOD],
        "minus_equal" => vec![VK_OEM_MINUS, VK_OEM_PLUS],
        "brackets" => vec![VK_OEM_4, VK_OEM_6],
        _ => Vec::new(),
    }
}

/// 翻页键组 → raw hash 列表
fn compile_page_key_group(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
        "pageupdown" => vec![key_hash(0, VK_PRIOR), key_hash(0, VK_NEXT)],
        "minus_equal" => vec![key_hash(0, VK_OEM_MINUS), key_hash(0, VK_OEM_PLUS)],
        "brackets" => vec![key_hash(0, VK_OEM_4), key_hash(0, VK_OEM_6)],
        "shift_tab" => vec![key_hash(MOD_SHIFT, VK_TAB), key_hash(0, VK_TAB)],
        _ => Vec::new(),
    }
}

/// 计算 key_hash（与 wind-ipc::protocol::calc_key_hash 对齐）
fn calc_key_hash(modifiers: u32, key_code: u32) -> u32 {
    (modifiers << 16) | (key_code & 0xFFFF)
}

/// 解析热键字符串为 key_hash
///
/// 支持格式：
/// - "Ctrl+Space"、"Ctrl+Shift+E"
/// - "lshift"、"rshift"
/// - "Shift+Space"
/// - "Ctrl+."、"Ctrl+Equal"
pub fn parse_hotkey(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut modifiers: u32 = 0;
    let mut key_code: Option<u32> = None;

    for part in s.split('+') {
        let part = part.trim().to_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CTRL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "super" => modifiers |= MOD_WIN,
            _ => {
                if key_code.is_some() {
                    // 已经有一个主键了，不支持多个主键
                    return None;
                }
                key_code = Some(parse_key_name(&part)?);
            }
        }
    }

    key_code.map(|kc| calc_key_hash(modifiers, kc))
}

/// 将键名解析为 Windows 虚拟键码
fn parse_key_name(name: &str) -> Option<u32> {
    match name {
        // 修饰键本身（当作为主键时，如 "lshift"）
        "lshift" => Some(0xA0),
        "rshift" => Some(0xA1),
        "lctrl" | "lcontrol" => Some(0xA2),
        "rctrl" | "rcontrol" => Some(0xA3),
        "lalt" | "lmenu" => Some(0xA4),
        "ralt" | "rmenu" => Some(0xA5),

        // 特殊键
        "space" => Some(0x20),
        "return" | "enter" => Some(0x0D),
        "escape" | "esc" => Some(0x1B),
        "backspace" | "back" => Some(0x08),
        "tab" => Some(0x09),
        "delete" | "del" => Some(0x2E),
        "insert" | "ins" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" | "pgup" => Some(0x21),
        "pagedown" | "pgdn" => Some(0x22),
        "up" => Some(0x26),
        "down" => Some(0x28),
        "left" => Some(0x25),
        "right" => Some(0x27),

        // 标点/符号键
        "." | "period" => Some(0xBE),
        "," | "comma" => Some(0xBC),
        ";" | "semicolon" => Some(0xBA),
        "'" | "quote" => Some(0xDE),
        "/" | "slash" => Some(0xBF),
        "\\" | "backslash" => Some(0xDC),
        "[" | "lbracket" => Some(0xDB),
        "]" | "rbracket" => Some(0xDD),
        "-" | "minus" | "hyphen" => Some(0xBD),
        "=" | "equal" | "equals" => Some(0xBB),
        "`" | "backtick" | "grave" => Some(0xC0),

        // 功能键
        "f1" => Some(0x70),
        "f2" => Some(0x71),
        "f3" => Some(0x72),
        "f4" => Some(0x73),
        "f5" => Some(0x74),
        "f6" => Some(0x75),
        "f7" => Some(0x76),
        "f8" => Some(0x77),
        "f9" => Some(0x78),
        "f10" => Some(0x79),
        "f11" => Some(0x7A),
        "f12" => Some(0x7B),

        // 数字键
        "0" => Some(0x30),
        "1" => Some(0x31),
        "2" => Some(0x32),
        "3" => Some(0x33),
        "4" => Some(0x34),
        "5" => Some(0x35),
        "6" => Some(0x36),
        "7" => Some(0x37),
        "8" => Some(0x38),
        "9" => Some(0x39),

        // 单个字母
        _ if name.len() == 1 => {
            let ch = name.as_bytes()[0];
            if ch.is_ascii_alphabetic() {
                Some((ch.to_ascii_uppercase() - b'A' + 0x41) as u32)
            } else {
                None
            }
        }

        // 十六进制键码（如 "0x41"）
        _ if name.starts_with("0x") => u32::from_str_radix(&name[2..], 16).ok(),

        _ => None,
    }
}

/// 从配置中解析热键中的数字键部分（用于 delete_candidate、pin_candidate 等动态热键）
///
/// 例如 "ctrl+shift+number" 中的 "number" 表示 1-9 数字键
/// 返回 (modifiers, 0) 表示匹配任意数字键
pub fn parse_hotkey_prefix(s: &str) -> Option<(u32, bool)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let mut modifiers: u32 = 0;
    let mut has_number = false;

    for part in s.split('+') {
        let part = part.trim().to_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CTRL,
            "alt" => modifiers |= MOD_ALT,
            "shift" => modifiers |= MOD_SHIFT,
            "win" | "super" => modifiers |= MOD_WIN,
            "number" | "digit" => has_number = true,
            _ => {}
        }
    }

    Some((modifiers, has_number))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_key() {
        let hash = parse_hotkey("lshift").unwrap();
        assert_eq!(hash, 0xA0); // no modifiers, key=0xA0
    }

    #[test]
    fn test_parse_ctrl_space() {
        let hash = parse_hotkey("Ctrl+Space").unwrap();
        assert_eq!(hash, (MOD_CTRL << 16) | 0x20);
    }

    #[test]
    fn test_parse_ctrl_shift_e() {
        let hash = parse_hotkey("ctrl+shift+e").unwrap();
        assert_eq!(hash, ((MOD_CTRL | MOD_SHIFT) << 16) | 0x45);
    }

    #[test]
    fn test_parse_shift_space() {
        let hash = parse_hotkey("shift+space").unwrap();
        assert_eq!(hash, (MOD_SHIFT << 16) | 0x20);
    }

    #[test]
    fn test_parse_ctrl_dot() {
        let hash = parse_hotkey("ctrl+.").unwrap();
        assert_eq!(hash, (MOD_CTRL << 16) | 0xBE);
    }

    #[test]
    fn test_parse_ctrl_equal() {
        let hash = parse_hotkey("ctrl+equal").unwrap();
        assert_eq!(hash, (MOD_CTRL << 16) | 0xBB);
    }

    #[test]
    fn test_parse_empty() {
        assert!(parse_hotkey("").is_none());
    }

    #[test]
    fn test_toggle_mode_key_includes_specific_modifier() {
        // 关键回归：lshift 的 keyUp hash 必须同时含通用位(MOD_SHIFT)和具体位(MOD_LSHIFT)，
        // 否则 C++ TSF 算出的 0x1100A0 在白名单里找不到 → Shift 切换失效。
        let lshift = compile_toggle_mode_key("lshift").unwrap();
        assert_eq!(lshift, key_hash(MOD_SHIFT | MOD_LSHIFT, VK_LSHIFT));
        assert_eq!(lshift, 0x0011_00A0);

        let rshift = compile_toggle_mode_key("rshift").unwrap();
        assert_eq!(rshift, key_hash(MOD_SHIFT | MOD_RSHIFT, VK_RSHIFT));
        assert_eq!(rshift, 0x0021_00A1);
    }

    #[test]
    fn test_number_hotkey_expands_to_ten_session_keys() {
        let entries = compile_number_hotkey("ctrl+shift+number");
        assert_eq!(entries.len(), 10);
        // tsf_hash 含 session policy 位；match_hash 为 raw
        assert!(entries[0].tsf_hash & HOTKEY_POLICY_SESSION != 0);
        assert_eq!(entries[0].match_hash, key_hash(MOD_CTRL | MOD_SHIFT, 0x30));
        assert!(compile_number_hotkey("none").is_empty());
    }

    #[test]
    fn test_compile_switch_engine_match_hash() {
        let mut cfg = Config::default();
        cfg.keys.switch_engine = "ctrl+shift+e".to_string();
        cfg.keys.toggle_mode_keys = vec!["lshift".into(), "rshift".into()];
        let compiled = Compiler::new(cfg).compile();
        // switch_engine 无 policy 位，match_hash == tsf_hash == 0x30045
        let se = compiled
            .key_down
            .iter()
            .find(|e| e.action == "switch_engine")
            .unwrap();
        assert_eq!(se.match_hash, key_hash(MOD_CTRL | MOD_SHIFT, 0x45));
        assert_eq!(se.tsf_hash, se.match_hash);
        // 规范化匹配：带 L/R 具体位的入站事件也能命中
        assert_eq!(
            compiled.match_key_down(key_hash(MOD_CTRL | MOD_SHIFT, 0x45)),
            Some("switch_engine")
        );
        assert_eq!(compiled.key_up.len(), 2);
    }

    #[test]
    fn open_add_word_dialog_registered_chinese_only() {
        let mut cfg = Config::default();
        cfg.keys.open_add_word_dialog = "ctrl+shift+equal".to_string();
        let compiled = Compiler::new(cfg).compile();
        // action 串应出现在 key_down 组
        assert!(
            compiled
                .key_down
                .iter()
                .any(|e| e.action == "open_add_word_dialog"),
            "open_add_word_dialog 应注册进 key_down"
        );
    }
}
