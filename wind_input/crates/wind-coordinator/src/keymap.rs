//! 触发键名 ↔ 虚拟键码（VK）统一映射。
//!
//! 所有独占模式（临时拼音 / 快捷输入 / 特殊模式）共用此表，避免各写各的键名解析
//! 导致「某模式认得 backslash、某模式不认」的不一致（曾因此使特殊模式无法触发）。
//!
//! 单一真相源是 [`KEY_TABLE`]：每行用具名 VK 常量声明「VK + 组合区前缀字符 + 键名别名」，
//! [`key_name_to_vk`] 与 [`vk_to_prefix_char`] 两个方向均由它派生，新增键只改一处。

/// 符号 / OEM 虚拟键码常量（Windows Virtual-Key Codes）。
/// 统一在此定义，杜绝散落各处的 `0xBA` 之类裸十六进制字面量。
pub const VK_SEMICOLON: u32 = 0xBA; // ; :  VK_OEM_1
pub const VK_EQUAL: u32 = 0xBB; // = +  VK_OEM_PLUS
pub const VK_COMMA: u32 = 0xBC; // , <  VK_OEM_COMMA
pub const VK_MINUS: u32 = 0xBD; // - _  VK_OEM_MINUS
pub const VK_PERIOD: u32 = 0xBE; // . >  VK_OEM_PERIOD
pub const VK_SLASH: u32 = 0xBF; // / ?  VK_OEM_2
pub const VK_BACKTICK: u32 = 0xC0; // ` ~  VK_OEM_3
pub const VK_LBRACKET: u32 = 0xDB; // [ {  VK_OEM_4
pub const VK_BACKSLASH: u32 = 0xDC; // \ |  VK_OEM_5
pub const VK_RBRACKET: u32 = 0xDD; // ] }  VK_OEM_6
pub const VK_QUOTE: u32 = 0xDE; // ' "  VK_OEM_7

/// 单个键的定义：虚拟键码、组合区前缀字符、可接受的键名别名（全小写）。
struct KeyDef {
    vk: u32,
    prefix: char,
    names: &'static [&'static str],
}

/// 触发键映射的单一真相源。两个方向的查询函数均从此派生。
const KEY_TABLE: &[KeyDef] = &[
    KeyDef {
        vk: VK_BACKTICK,
        prefix: '`',
        names: &["backtick", "grave", "`"],
    },
    KeyDef {
        vk: VK_SEMICOLON,
        prefix: ';',
        names: &["semicolon", ";"],
    },
    KeyDef {
        vk: VK_QUOTE,
        prefix: '\'',
        names: &["quote", "'"],
    },
    KeyDef {
        vk: VK_COMMA,
        prefix: ',',
        names: &["comma", ","],
    },
    KeyDef {
        vk: VK_PERIOD,
        prefix: '.',
        names: &["period", "."],
    },
    KeyDef {
        vk: VK_SLASH,
        prefix: '/',
        names: &["slash", "/"],
    },
    KeyDef {
        vk: VK_LBRACKET,
        prefix: '[',
        names: &["lbracket", "["],
    },
    KeyDef {
        vk: VK_RBRACKET,
        prefix: ']',
        names: &["rbracket", "]"],
    },
    KeyDef {
        vk: VK_BACKSLASH,
        prefix: '\\',
        names: &["backslash", "\\"],
    },
    KeyDef {
        vk: VK_MINUS,
        prefix: '-',
        names: &["minus", "-"],
    },
    KeyDef {
        vk: VK_EQUAL,
        prefix: '=',
        names: &["equal", "equals", "="],
    },
];

/// 触发键名 → VK。支持规范名 / 别名 / 单字符；大小写与首尾空白不敏感。
/// 字母键由调用方按需处理（见 [`key_name_to_vk_with_letters`]）。
pub fn key_name_to_vk(name: &str) -> Option<u32> {
    let k = name.trim().to_lowercase();
    KEY_TABLE
        .iter()
        .find(|d| d.names.contains(&k.as_str()))
        .map(|d| d.vk)
}

/// 同 [`key_name_to_vk`]，但额外接受单字母 a-z 作触发键（特殊模式引导键常用）。
pub fn key_name_to_vk_with_letters(name: &str) -> Option<u32> {
    if let Some(vk) = key_name_to_vk(name) {
        return Some(vk);
    }
    let k = name.trim().to_lowercase();
    let bytes = k.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        // VK for 'A'..='Z' 与 ASCII 大写一致（0x41..=0x5A）。
        return Some(0x41 + (bytes[0] - b'a') as u32);
    }
    None
}

/// VK → 组合区前缀字符。无映射时返回 None（调用方自定默认）。
pub fn vk_to_prefix_char(vk: u32) -> Option<char> {
    KEY_TABLE.iter().find(|d| d.vk == vk).map(|d| d.prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_aliases_resolve() {
        assert_eq!(key_name_to_vk("backtick"), Some(VK_BACKTICK));
        assert_eq!(key_name_to_vk("grave"), Some(VK_BACKTICK));
        assert_eq!(key_name_to_vk("`"), Some(VK_BACKTICK));
        // 所有模式现在都认得 backslash（曾经的特殊模式不触发根因）。
        assert_eq!(key_name_to_vk("backslash"), Some(VK_BACKSLASH));
        assert_eq!(key_name_to_vk("\\"), Some(VK_BACKSLASH));
        assert_eq!(key_name_to_vk("EQUALS"), Some(VK_EQUAL)); // 大小写不敏感
        assert_eq!(key_name_to_vk(" semicolon "), Some(VK_SEMICOLON)); // 去空白
    }

    #[test]
    fn letters_only_with_letter_variant() {
        assert_eq!(key_name_to_vk("z"), None);
        assert_eq!(key_name_to_vk_with_letters("z"), Some(0x5A));
        assert_eq!(key_name_to_vk_with_letters("a"), Some(0x41));
        assert_eq!(key_name_to_vk_with_letters("backslash"), Some(VK_BACKSLASH));
    }

    #[test]
    fn prefix_char_roundtrip() {
        assert_eq!(vk_to_prefix_char(VK_BACKTICK), Some('`'));
        assert_eq!(vk_to_prefix_char(VK_SLASH), Some('/'));
        assert_eq!(vk_to_prefix_char(VK_BACKSLASH), Some('\\'));
        assert_eq!(vk_to_prefix_char(0x41), None); // 字母无前缀定义
    }
}
