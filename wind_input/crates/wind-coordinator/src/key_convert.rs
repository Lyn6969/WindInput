//! 键位换算纯函数：VK ↔ 字符、修饰位序转换、小键盘归一、大小写变形候选。
//!
//! 全部为无状态查表/换算，coordinator 与各 handle_* 模块共用。
//! （自 coordinator.rs 平移，纯搬运。）

use wind_ipc::protocol::{MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use wind_keys::keymap;

/// wind 修饰位（SHIFT=0x1/CTRL=0x2/ALT=0x4/WIN=0x8，见 wind-ipc MOD_*）→ Win32 位序
/// （ALT=0x1/CTRL=0x2/SHIFT=0x4/WIN=0x8，即 ALT 与 SHIFT 互换）。
/// RegisterHotKey 的 fsModifiers 与 DirectSwitchHotkeys 的 Modifiers 低位（TF_MOD_*）同用此位序。
pub(crate) fn wind_mods_to_win32(mods: u32) -> u32 {
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

/// VK + shift → 该键产生的 ASCII 标点/符号字符（字母键返回 None，由拼音/码表处理）。
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
pub(crate) fn char_to_main_vk(ch: char) -> Option<u32> {
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
