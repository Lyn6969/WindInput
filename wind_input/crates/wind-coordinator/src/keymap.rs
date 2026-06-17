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

// 控制 / 编辑 / 导航虚拟键码（统一定义，杜绝散落的裸十六进制）。
pub const VK_BACK: u32 = 0x08; // 退格
pub const VK_TAB: u32 = 0x09;
pub const VK_RETURN: u32 = 0x0D; // 回车
pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_SPACE: u32 = 0x20;
pub const VK_PRIOR: u32 = 0x21; // PageUp
pub const VK_NEXT: u32 = 0x22; // PageDown
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
// 字母 / 数字区间端点（区间用 VK_A..=VK_Z / VK_0..=VK_9 表达，VK 与 ASCII 大写/数字一致）。
pub const VK_A: u32 = 0x41;
pub const VK_Z: u32 = 0x5A;
pub const VK_0: u32 = 0x30;
pub const VK_9: u32 = 0x39;
pub const VK_1: u32 = 0x31;

/// 候选导航动作（翻页 / 高亮移动）。统一分类的结果，见 [`NavKeys`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavAction {
    PagePrev,
    PageNext,
    HighlightUp,
    HighlightDown,
}

/// 一个导航键绑定：(键码, 是否需 Shift, 动作, 该键是否为可打印字符)。
/// `printable=true`（如 `-`/`=`/`[`/`]`）在文本/表达式模式（临英/快捷输入）中作输入而非导航，
/// 由 `classify(..., include_printable=false)` 排除；专用导航键（PageUp/Down、方向键、Tab）恒生效。
#[derive(Clone, Copy)]
struct NavBind {
    key: u32,
    shift: bool,
    action: NavAction,
    printable: bool,
}

/// 配置驱动的候选导航键分类器。从 `input.page_keys` / `input.highlight_keys` 组名编译一次，
/// 普通模式与所有 overlay 模式共用 [`classify`](NavKeys::classify)，消除各处硬编码翻页/高亮判断。
#[derive(Clone, Default)]
pub struct NavKeys {
    binds: Vec<NavBind>,
}

impl NavKeys {
    /// 从配置组名编译。page 组：pageupdown / minus_equal / brackets / shift_tab；
    /// highlight 组：arrows / tab。未识别组名忽略。
    pub fn from_config(page_groups: &[String], highlight_groups: &[String]) -> Self {
        use NavAction::*;
        let mut binds = Vec::new();
        let mut push = |key, shift, action, printable| {
            binds.push(NavBind {
                key,
                shift,
                action,
                printable,
            })
        };
        for g in page_groups {
            match g.trim().to_lowercase().as_str() {
                "pageupdown" => {
                    push(VK_PRIOR, false, PagePrev, false);
                    push(VK_NEXT, false, PageNext, false);
                }
                "minus_equal" => {
                    push(VK_MINUS, false, PagePrev, true);
                    push(VK_EQUAL, false, PageNext, true);
                }
                "brackets" => {
                    push(VK_LBRACKET, false, PagePrev, true);
                    push(VK_RBRACKET, false, PageNext, true);
                }
                "shift_tab" => {
                    push(VK_TAB, true, PagePrev, false);
                    push(VK_TAB, false, PageNext, false);
                }
                _ => {}
            }
        }
        for g in highlight_groups {
            match g.trim().to_lowercase().as_str() {
                "arrows" => {
                    push(VK_UP, false, HighlightUp, false);
                    push(VK_DOWN, false, HighlightDown, false);
                }
                "tab" => {
                    push(VK_TAB, true, HighlightUp, false);
                    push(VK_TAB, false, HighlightDown, false);
                }
                _ => {}
            }
        }
        Self { binds }
    }

    /// 分类一个键。`include_printable=false` 时排除可打印导航键（`-`/`=`/`[`/`]`），
    /// 供输入需要这些字符的模式（临英/快捷输入）使用，避免吞掉输入语义。
    pub fn classify(
        &self,
        key_code: u32,
        shift: bool,
        include_printable: bool,
    ) -> Option<NavAction> {
        self.binds
            .iter()
            .find(|b| b.key == key_code && b.shift == shift && (include_printable || !b.printable))
            .map(|b| b.action)
    }
}

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
    fn nav_classify_config_driven() {
        let nk = NavKeys::from_config(
            &["pageupdown".into(), "minus_equal".into()],
            &["arrows".into(), "tab".into()],
        );
        // 专用导航键恒生效
        assert_eq!(
            nk.classify(VK_PRIOR, false, false),
            Some(NavAction::PagePrev)
        );
        assert_eq!(
            nk.classify(VK_NEXT, false, false),
            Some(NavAction::PageNext)
        );
        assert_eq!(
            nk.classify(VK_UP, false, false),
            Some(NavAction::HighlightUp)
        );
        assert_eq!(
            nk.classify(VK_DOWN, false, false),
            Some(NavAction::HighlightDown)
        );
        // tab=下移、shift+tab=上移
        assert_eq!(
            nk.classify(VK_TAB, false, false),
            Some(NavAction::HighlightDown)
        );
        assert_eq!(
            nk.classify(VK_TAB, true, false),
            Some(NavAction::HighlightUp)
        );
        // -/= 仅在 include_printable 时作翻页（码表模式 true，文本模式 false）
        assert_eq!(
            nk.classify(VK_MINUS, false, true),
            Some(NavAction::PagePrev)
        );
        assert_eq!(
            nk.classify(VK_EQUAL, false, true),
            Some(NavAction::PageNext)
        );
        assert_eq!(nk.classify(VK_MINUS, false, false), None);
        assert_eq!(nk.classify(VK_EQUAL, false, false), None);
    }

    #[test]
    fn prefix_char_roundtrip() {
        assert_eq!(vk_to_prefix_char(VK_BACKTICK), Some('`'));
        assert_eq!(vk_to_prefix_char(VK_SLASH), Some('/'));
        assert_eq!(vk_to_prefix_char(VK_BACKSLASH), Some('\\'));
        assert_eq!(vk_to_prefix_char(0x41), None); // 字母无前缀定义
    }
}
