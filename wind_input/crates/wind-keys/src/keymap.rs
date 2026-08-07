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
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_DELETE: u32 = 0x2E; // 前删（光标后一字符）
// 字母 / 数字区间端点（区间用 VK_A..=VK_Z / VK_0..=VK_9 表达，VK 与 ASCII 大写/数字一致）。
pub const VK_A: u32 = 0x41;
pub const VK_Z: u32 = 0x5A;
pub const VK_0: u32 = 0x30;
pub const VK_9: u32 = 0x39;
pub const VK_1: u32 = 0x31;
// 纯修饰键的左右具体键码。只在「修饰键被配成功能键」的路径出现：中英文切换键、
// 二三候选键的 lrshift/lrctrl 组——这两条都由 TSF 在 keyup 转发**具体**键码（笼统的
// VK_SHIFT/VK_CONTROL 已在那边解析成左右），故服务端只需认这四个。
// 四个连号（0xA0..=0xA3），可用 `VK_LSHIFT..=VK_RCONTROL` 表达「是不是纯修饰键」。
pub const VK_LSHIFT: u32 = 0xA0;
pub const VK_RSHIFT: u32 = 0xA1;
pub const VK_LCONTROL: u32 = 0xA2;
pub const VK_RCONTROL: u32 = 0xA3;

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

/// 配置驱动的候选导航键分类器。从 `keys.page_keys` / `keys.highlight_keys` 组名编译一次，
/// 普通模式与所有 overlay 模式共用 [`classify`](NavKeys::classify)，消除各处硬编码翻页/高亮判断。
#[derive(Clone, Default)]
pub struct NavKeys {
    binds: Vec<NavBind>,
}

impl NavKeys {
    /// 从配置组名编译。page 组：pageupdown / minus_equal / brackets / comma_period / shift_tab；
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
                "comma_period" => {
                    push(VK_COMMA, false, PagePrev, true);
                    push(VK_PERIOD, false, PageNext, true);
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

/// 同 [`key_name_to_vk`]，但额外接受单字母 a-z。
///
/// **不再供引导键使用**——引导键（临拼 / 特殊模式 / 临时 mix 的 `trigger_keys`）一律只认
/// 符号键，字母的特殊能力走方案级 `schema.codetable.z_key_action`。当前唯一消费者是
/// `key_inject`（按键注入按名字找 VK，与引导键语义无关）。
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

/// 纯修饰键名 → VK（`lshift` / `rshift` / `lctrl` / `rctrl`，别名见实现）。
/// 大小写与首尾空白不敏感；非修饰键返回 None。
///
/// # 为什么单开一张表，不并进 [`KEY_TABLE`]
///
/// `KEY_TABLE` 是**引导键**的配置解析入口（`trigger_keys` 五处、`special_trigger_vk`），
/// 而引导键走 keydown。修饰键在 keydown 上根本不工作——它必须走 keyup 轻敲
/// （keydown 不能吃、`Ctrl+A` 的第一下会误触发、按住会连触发，见
/// `KeyEventSink.cpp::_IsPureModifierKey`）。并进去就等于让引导键的设置项接受一个
/// 「配得上、永远不触发」的值，与临拼触发键里那个失效的 `z` 选项同型（已修，勿再造）。
///
/// 另一半理由是 `KeyDef.prefix`：修饰键没有字符，填什么都是假的，而 `vk_to_prefix_char`
/// 拿它写组合区。
///
/// 当前唯一消费者是方案级 `[key_actions]`（见 `Coordinator::bound_action_for`）。
pub fn modifier_name_to_vk(name: &str) -> Option<u32> {
    match name.trim().to_lowercase().as_str() {
        "lshift" | "leftshift" | "left_shift" => Some(VK_LSHIFT),
        "rshift" | "rightshift" | "right_shift" => Some(VK_RSHIFT),
        "lctrl" | "lcontrol" | "leftctrl" | "left_ctrl" => Some(VK_LCONTROL),
        "rctrl" | "rcontrol" | "rightctrl" | "right_ctrl" => Some(VK_RCONTROL),
        _ => None,
    }
}

/// 该 VK 是否为纯修饰键（四个连号，见常量定义处说明）。
pub fn is_pure_modifier_vk(vk: u32) -> bool {
    (VK_LSHIFT..=VK_RCONTROL).contains(&vk)
}

/// VK → 组合区前缀字符。无映射时返回 None（调用方自定默认）。
pub fn vk_to_prefix_char(vk: u32) -> Option<char> {
    KEY_TABLE.iter().find(|d| d.vk == vk).map(|d| d.prefix)
}

/// 同 [`vk_to_prefix_char`]，但字母 VK 返回其小写字母。
///
/// 与 [`key_name_to_vk`] 拒绝字母**不矛盾**——两者方向与职责不同：
/// - `key_name_to_vk`（名字 → VK）是**配置解析**，必须严格。认了字母就等于允许把编码键
///   配成引导键，而全局配置无从表达「这张码表里它是死码」。
/// - `vk_to_prefix_char*`（VK → 字符）是**呈现**，字母有天然合法的显示形态。z 经方案级
///   `z_key_action` 进模式后，组合区要显示用户按下的那个 `z`；用不带字母的版本会得到
///   空前缀，用户看不到自己按了什么。
pub fn vk_to_prefix_char_with_letters(vk: u32) -> Option<char> {
    if let Some(c) = vk_to_prefix_char(vk) {
        return Some(c);
    }
    (VK_A..=VK_Z)
        .contains(&vk)
        .then(|| (b'a' + (vk - VK_A) as u8) as char)
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

    /// 两个方向的严格度刻意不同：名字→VK 拒绝字母（配置解析，见
    /// `Coordinator::special_trigger_vk`），VK→字符接受字母（呈现，组合区要显示按下的 z）。
    #[test]
    fn prefix_char_accepts_letters_while_key_name_does_not() {
        assert_eq!(vk_to_prefix_char(VK_Z), None);
        assert_eq!(vk_to_prefix_char_with_letters(VK_Z), Some('z'));
        assert_eq!(vk_to_prefix_char_with_letters(VK_A), Some('a'));
        // 符号仍走 KEY_TABLE，两个版本一致。
        assert_eq!(vk_to_prefix_char_with_letters(VK_BACKTICK), Some('`'));
        assert_eq!(vk_to_prefix_char_with_letters(VK_SEMICOLON), Some(';'));
        // 非字母非符号（如数字键）两个版本都没有映射。
        assert_eq!(vk_to_prefix_char_with_letters(0x31), None);
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
    fn comma_period_pages() {
        // 设置页提供 comma_period 选项，但组名曾未被识别（走 `_ => {}` 静默忽略）→
        // 无翻页绑定，逗号/句号落到标点臂直接上屏。
        let nk = NavKeys::from_config(&["comma_period".into()], &[]);
        assert_eq!(
            nk.classify(VK_COMMA, false, true),
            Some(NavAction::PagePrev)
        );
        assert_eq!(
            nk.classify(VK_PERIOD, false, true),
            Some(NavAction::PageNext)
        );
        // 可打印键：文本/表达式模式（临英/快捷输入）里仍作输入字符，不夺为翻页。
        assert_eq!(nk.classify(VK_COMMA, false, false), None);
        assert_eq!(nk.classify(VK_PERIOD, false, false), None);
    }

    #[test]
    fn prefix_char_roundtrip() {
        assert_eq!(vk_to_prefix_char(VK_BACKTICK), Some('`'));
        assert_eq!(vk_to_prefix_char(VK_SLASH), Some('/'));
        assert_eq!(vk_to_prefix_char(VK_BACKSLASH), Some('\\'));
        assert_eq!(vk_to_prefix_char(0x41), None); // 字母无前缀定义
    }

    /// 修饰键走**独立**的解析入口，不进 `KEY_TABLE`——后者是引导键（keydown）的配置
    /// 解析口，而修饰键只在 keyup 轻敲上工作。混在一起就会让引导键设置项接受一个
    /// 配得上却永不触发的值（临拼触发键里那个失效的 `z` 选项就是这么来的，已修）。
    #[test]
    fn modifier_names_resolve_only_via_dedicated_entry() {
        assert_eq!(modifier_name_to_vk("rshift"), Some(VK_RSHIFT));
        assert_eq!(modifier_name_to_vk("RShift"), Some(VK_RSHIFT)); // 大小写不敏感
        assert_eq!(modifier_name_to_vk(" lctrl "), Some(VK_LCONTROL)); // 首尾空白不敏感
        assert_eq!(modifier_name_to_vk("rcontrol"), Some(VK_RCONTROL)); // 别名
        assert_eq!(modifier_name_to_vk("backslash"), None); // 符号键不归它管
        // 反向：引导键的解析口**不认**修饰键，否则 trigger_keys 里能配 rshift 却永不触发。
        assert_eq!(key_name_to_vk("rshift"), None);
        assert_eq!(key_name_to_vk_with_letters("rshift"), None);
    }

    /// 纯修饰键判定覆盖四个连号，且不误伤相邻 VK。
    #[test]
    fn pure_modifier_vk_range_is_exact() {
        for vk in [VK_LSHIFT, VK_RSHIFT, VK_LCONTROL, VK_RCONTROL] {
            assert!(is_pure_modifier_vk(vk), "0x{vk:02X} 应是纯修饰键");
        }
        assert!(!is_pure_modifier_vk(0x9F)); // 区间下界之前
        assert!(!is_pure_modifier_vk(0xA4)); // VK_LMENU，区间上界之后
        assert!(!is_pure_modifier_vk(VK_Z));
    }
}
