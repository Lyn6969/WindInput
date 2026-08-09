//! 热键编译器
//!
//! 与 Go 版本 `wind_input/internal/hotkey/compiler.go` 对齐。
//! 将配置中的热键字符串（如 "Ctrl+Space"、"lshift"）编译为 key_hash，
//! 用于按键事件中的热键匹配。

use crate::config::Config;
use tracing::{debug, warn};

/// `keys.key_actions` 里**组合键**条目支持的动词。
///
/// 白名单而非「解析得动就收」：写错的动词若静默进热键表，按下时分发端匹配不上、
/// 什么都不发生，而用户看不出是自己拼错了还是功能坏了。这里拦下并 warn，与
/// `global_hotkeys` 对不支持动作的处理同策略。
///
/// ★ **只管组合键**。单键条目走的是引导键通路（`Coordinator::bound_action_for`），
/// 值域是完整的 [`BoundAction`]，不经本函数——两条通路的分发端不同，能认的动词自然
/// 不同。用一张表管两条路的结果，是要么放行了热键分发端不认的（配了没反应），
/// 要么挡住了引导键通路完全支持的（能力凭空少一半）。
///
/// 值域语义见 docs/design/schema-key-actions.md §2。
fn is_supported_hotkey_action(action: &str) -> bool {
    hotkey_action_entry(action).is_some()
}

/// 组合键动词 → `(分发端 action, 策略位)`；不支持的动词返回 `None`。
///
/// ★★ **策略位必须按动词分**，不能一律不带：同一个位在两类机制下后果相反。
///
/// | 动词 | 策略位 | why |
/// |---|---|---|
/// | `toggle_schema:<id>` | 无 | 回程恰恰要在**非中文态**下按得动——带上 `CHINESE_ONLY` 就成了单程票，切到英文方案后回不来 |
/// | `special:<id>` | `CHINESE_ONLY \| GLOBAL` | 进 overlay 只在中文输入中途有意义；`GLOBAL` 让 TSF 用 `RegisterHotKey` 抢占，穿透 QQNT/Tabby 等 Chromium 宿主的同名加速键 |
///
/// ★ 动词形态在此做一次映射：引导键通路用 `special:<id>`（[`crate::BoundAction`] 的值域），
/// 而热键分发端认的是 `enter_special:<id>`。两条通路的分发端不同，动词形态也就不同——
/// 映射放在编译期，分发端零改动。
fn hotkey_action_entry(action: &str) -> Option<(String, u32)> {
    if let Some(id) = action.strip_prefix("toggle_schema:")
        && !id.trim().is_empty()
    {
        return Some((action.to_string(), 0));
    }
    if let Some(id) = action.strip_prefix("special:")
        && !id.trim().is_empty()
    {
        return Some((
            format!("enter_special:{}", id.trim()),
            HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL,
        ));
    }
    None
}

/// `keys.key_actions` 的一条条目该走哪条通路。由**键的形态**决定，不由动词决定。
///
/// 三条通路各有各的到达条件，判据见 docs/design/schema-key-actions.md §4.1 与 §4.4：
///
/// | 形态 | 通路 | 为什么 |
/// |---|---|---|
/// | 组合键（带 Ctrl/Alt/Shift/Win） | key_down 热键 → `dispatch_hotkey` | 不与输入争键，可全局拦截 |
/// | 纯修饰键（`rshift`） | key_up 轻敲 | keydown 不能吃（宿主要看到修饰键），只能在干净单击的 keyup 上判 |
/// | 单个有字符的键（`backtick`） | keydown 引导键链 | 英文模式下必须让它出字，故排在分水岭之后 |
///
/// ⚠ 单键**绝不能**编译进 key_down 热键表：`parse_hotkey("backtick")` 返回的是无修饰位的
/// 裸 VK，进表后 TSF 会把它当热键转发并吞掉，该符号就再也打不出来了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyActionRoute {
    /// 组合键：编译进 key_down 热键表。
    Hotkey,
    /// 纯修饰键：编译进 key_up 转发集。
    ModifierKeyUp,
    /// 单个有字符的键：不进任何热键表，由引导键链查表消费。
    LeadingKey,
}

/// 按键名判定通路。无法解析的键名返回 `None`（调用方 warn 后忽略）。
pub fn route_of_key_action(key: &str) -> Option<KeyActionRoute> {
    let raw = parse_hotkey(key)?;
    let has_modifier = (raw >> 16) & MOD_GENERIC_MASK != 0;
    if has_modifier {
        return Some(KeyActionRoute::Hotkey);
    }
    let vk = raw & 0xFFFF;
    if (VK_LSHIFT..=VK_RCONTROL).contains(&vk) {
        return Some(KeyActionRoute::ModifierKeyUp);
    }
    Some(KeyActionRoute::LeadingKey)
}

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
/// 全局拦截位（正交标记，与 CHINESE_ONLY 叠加）：TSF 侧在「中文模式 + 焦点在文本框」
/// 时用 Win32 RegisterHotKey 把这些键注册为系统级热键，让 OS 在 WM_KEYDOWN 派发前
/// 直接消费，规避 QQNT / Tabby 等 Chromium 类宿主无视 TSF pfEaten 契约的加速键双处理。
const HOTKEY_POLICY_GLOBAL: u32 = 0x20000000;
/// 「仅注册转发」标记：翻页键组 / 选词键组这类 action 为空的登记项——它们不是动作热键，
/// 只是让 TSF 认得这些键、在有会话时转发给引擎；无会话时必须放行，由 TSF 下游的
/// ClassifyInputKey 按普通标点处理（中文模式下要出中文标点）。
///
/// ⚠ 真动作热键**绝不能**带此位。TSF 侧的「无 Ctrl/Alt 且无会话就不吃」闸门只认这个标记；
/// 早先该闸门无差别地套在所有无 Ctrl/Alt 的 keydown 热键上，把 `shift+space`
/// （toggle_full_width）一并放行了，而 Space 在下游只有「有会话」和「已是全角」两条
/// 出路，半角空缓冲时无人接手 —— 严格 TSF 宿主（EverEdit）不再回调 OnKeyDown，
/// 全半角切换彻底失效；宽松宿主（记事本/Chromium）照调 OnKeyDown 才碰巧还能用。
const HOTKEY_POLICY_FORWARD_ONLY: u32 = 0x10000000;

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
                // 加词类热键额外叠加 GLOBAL 位：TSF 侧在中文+文本框时 RegisterHotKey 全局拦截，
                // 规避 Chromium 类宿主（QQNT/Tabby）的加速键双处理。其余 chinese-only 键不拦截，
                // 避免不必要地抢占宿主快捷键。
                let policy = if matches!(name, "add_word" | "open_add_word_dialog") {
                    HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL
                } else {
                    HOTKEY_POLICY_CHINESE_ONLY
                };
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw | policy,
                    match_hash: raw,
                    action: name.to_string(),
                });
            }
        }

        // ── KeyDown：临拼直达热键（CHINESE_ONLY | GLOBAL，与加词键同策略） ──
        // 与引导键共存：热键路径进入时组合区不写引导符（分发点传 key_code=0）。
        // GLOBAL 位使 TSF 在「中文 + 文本框」时 RegisterHotKey 全局拦截，穿透 QQNT/Tabby 等
        // Chromium 宿主的加速键双处理。
        //
        // 特殊模式的直达热键**不在这里**：它已收编进 `keys.key_actions`（写作
        // `"ctrl+shift+u" = "special:<方案id>"`），由上方的 `KeyActionRoute::Hotkey`
        // 分支按动词取策略位编译。原先那段遍历 `schema.special_modes[].hotkey` 的循环
        // 连同「id 为空则跳过」那条陷阱一并消失——身份现在就是方案 id，不可能为空。
        let mode_policy = HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL;
        if let Some(raw) = parse_hotkey(&self.config.input.temp_pinyin.hotkey) {
            result.key_down.push(HotkeyEntry {
                tsf_hash: raw | mode_policy,
                match_hash: raw,
                action: "enter_temp_pinyin".to_string(),
            });
        }

        // ── KeyDown：方案直达热键（切换 active 方案）──
        // **不带 CHINESE_ONLY**：与 `switch_engine` 循环键同策略（见上面第一组）。切方案是
        // 中英文两态下都该生效的操作——尤其"英文方案 → 中文方案"，要求恰恰是在非中文态下
        // 也能按。加了 CHINESE_ONLY 就会变成「切得过去、切不回来」。
        // 遍历顺序取决于 HashMap，故排序后再编译：热键冲突时（两个方案配了同一个键）
        // 谁先入列决定谁生效，不排序会让同一份配置在不同进程里表现不同。
        let mut schema_hotkeys: Vec<(&String, &String)> = h.schema_hotkeys.iter().collect();
        schema_hotkeys.sort_by(|a, b| a.0.cmp(b.0));
        for (schema_id, key) in schema_hotkeys {
            if schema_id.is_empty() {
                continue;
            }
            if let Some(raw) = parse_hotkey(key) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw,
                    match_hash: raw,
                    action: format!("switch_schema:{schema_id}"),
                });
            }
        }

        // ── KeyDown：按键功能表（keys.key_actions）──
        // **不带 CHINESE_ONLY**，理由与上面方案直达热键同：`toggle_schema` 的回程恰恰要在
        // 非中文态下按得动（切到英文方案后带上该位就回不来了）。
        //
        // ⚠ 后续接入别的动词时**策略位必须按动词分**，不能沿用这里的"一律不带"：进 overlay
        // 的动词（enter_special / temp_pinyin 那类）只在中文输入中途有意义，需要
        // CHINESE_ONLY | GLOBAL——同一个位在两类机制下后果相反（见上方 enter_special 那段）。
        //
        // BTreeMap 遍历即有序，无需像 schema_hotkeys 那样显式排序：撞键时的胜者顺序
        // 在任何进程里都一致。
        for (key, action) in &h.key_actions {
            let action = action.trim();
            if key.is_empty() || action.is_empty() {
                continue;
            }
            let Some(route) = route_of_key_action(key) else {
                warn!("keys.key_actions: 键 {key:?} 解析失败，忽略");
                continue;
            };
            let raw = match parse_hotkey(key) {
                Some(r) => r,
                None => continue, // route_of_key_action 已解析成功，此处不可达
            };
            match route {
                KeyActionRoute::Hotkey => {
                    let Some((dispatch_action, policy)) = hotkey_action_entry(action) else {
                        warn!("keys.key_actions: 组合键不支持动词 {action:?}（键 {key:?}），忽略");
                        continue;
                    };
                    result.key_down.push(HotkeyEntry {
                        // match_hash 恒不含策略位——策略位是给 TSF 看的转发/抢占指示，
                        // 服务端匹配只认裸 hash（与 enter_special / add_word 同构）。
                        tsf_hash: raw | policy,
                        match_hash: raw,
                        action: dispatch_action,
                    });
                }
                // 修饰键：只登记转发，动作由服务端按 `BoundAction` 裁决。
                // action 用 `schema_bound` 而非动词本身——`is_toggle_mode_keycode` 按 action
                // 过滤，塞进动词会让它认不出来（那条判据只认 `toggle_mode`）。
                KeyActionRoute::ModifierKeyUp => {
                    if let Some(hash) = compile_modifier_key_up_hash(raw & 0xFFFF) {
                        result.key_up.push(HotkeyEntry {
                            tsf_hash: hash,
                            match_hash: hash,
                            action: "schema_bound".to_string(),
                        });
                    }
                }
                // 单个有字符的键：**不进任何热键表**。进了 TSF 就会把它当热键吞掉，
                // 该符号再也打不出来。由引导键链（`bound_action_for`）查配置消费。
                KeyActionRoute::LeadingKey => {}
            }
        }

        // ── KeyDown：数字模板展开（PinCandidate / DeleteCandidate，session policy） ──
        for tmpl in [&h.pin_candidate, &h.delete_candidate] {
            for entry in compile_number_hotkey(tmpl) {
                result.key_down.push(entry);
            }
        }

        // ── KeyDown：选词键组（如 ;'），仅注册转发，由常规逻辑处理 ──
        // **修饰键组（lrshift / lrctrl）不进这里**，它们走下方的 key_up 段，理由见
        // `compile_select_modifier_group`。
        for group in &self.config.keys.select_key_groups {
            for raw in compile_select_key_group(group) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw | HOTKEY_POLICY_FORWARD_ONLY,
                    match_hash: raw,
                    action: String::new(),
                });
            }
        }

        // ── KeyDown：翻页键组（pageupdown / minus_equal / brackets / comma_period / shift_tab） ──
        for group in &self.config.keys.page_keys {
            for raw in compile_page_key_group(group) {
                result.key_down.push(HotkeyEntry {
                    tsf_hash: raw | HOTKEY_POLICY_FORWARD_ONLY,
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

        // ── KeyUp：修饰键作二三候选键（lrshift / lrctrl）──
        // 与 toggle 同一套 keyup 轻敲机制（见 `compile_select_modifier_group`）。
        // 同一个键可能既是切换键又是选词键（两条登记同 hash）：TSF 侧白名单是集合，重复无害；
        // 服务端按 action 区分——选词看 `keys.select_key_groups`，切换只认 action=="toggle_mode"，
        // 故消费端**不能**再用「key_up 里有这个 key_code」当切换判据（见 is_toggle_mode_keycode）。
        for group in &self.config.keys.select_key_groups {
            for hash in compile_select_modifier_group(group) {
                result.key_up.push(HotkeyEntry {
                    tsf_hash: hash,
                    match_hash: hash,
                    action: "select_candidate".to_string(),
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

/// 纯修饰键 VK → keyup hash（含通用位+具体位）。方案级 `[key_actions]` 绑修饰键时，
/// 用它把该键登记进 `key_up` 转发集——不登记 TSF 就不发这个 keyup，绑定形同虚设。
///
/// 与 [`compile_toggle_mode_key`] 同格式但入参是 VK 而非键名：调用方（协调器）手里
/// 已经是解析好的 VK（`keymap::modifier_name_to_vk`），再转回字符串只为了重新解析
/// 一次，中间多一层拼写契约就多一处静默失配的机会。
pub fn compile_modifier_key_up_hash(vk: u32) -> Option<u32> {
    match vk {
        VK_LSHIFT => Some(key_hash(MOD_SHIFT | MOD_LSHIFT, VK_LSHIFT)),
        VK_RSHIFT => Some(key_hash(MOD_SHIFT | MOD_RSHIFT, VK_RSHIFT)),
        VK_LCONTROL => Some(key_hash(MOD_CTRL | MOD_LCTRL, VK_LCONTROL)),
        VK_RCONTROL => Some(key_hash(MOD_CTRL | MOD_RCTRL, VK_RCONTROL)),
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

/// 选词键组 → keydown raw hash 列表。**只含可打印键**，修饰键组见
/// [`compile_select_modifier_group`]。
fn compile_select_key_group(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
        "semicolon_quote" => vec![key_hash(0, VK_OEM_1), key_hash(0, VK_OEM_7)],
        "comma_period" => vec![key_hash(0, VK_OEM_COMMA), key_hash(0, VK_OEM_PERIOD)],
        _ => Vec::new(),
    }
}

/// 选词键组里的**修饰键组** → keyup hash 列表（含通用位+具体位，与 toggle 键同格式）。
///
/// 为什么修饰键必须走 keyup 而不是 keydown（三条各自独立成立）：
/// - **纯修饰键的 keydown 不能吃**（TSF 侧 `_IsPureModifierKey` 的定论：吃掉会让 AutoCAD
///   等宿主看不到修饰键，正交模式覆盖失效并卡顿）。而 keydown 白名单的意义就是「让 TSF
///   吃下并转发」，对修饰键天然不成立。
/// - keydown 上判定会误触：`Ctrl+A` 的第一下 Ctrl 就会选走第 2 候选。
/// - 长按会连选：宿主对按住的键重复发 keydown（CAD 实测 28 秒 145 次）。
///
/// keyup 通路复用 TSF 已有的「轻敲」机制（`_MarkPendingToggleKey` + 500ms 阈值 +
/// 中途按别的键即取消），三条问题一次解决。
///
/// ⚠ 历史：这两组曾与 `;'` 一起注册进 keydown（带 FORWARD_ONLY），端到端从未生效过——
/// keydown 白名单查的是 `CalcKeyHash(通用修饰位, wParam)`，而 TSF 给的 wParam 是笼统的
/// `VK_CONTROL`，与这里登记的「具体位 + VK_LCONTROL」两个维度都对不上，永远不命中。
fn compile_select_modifier_group(group: &str) -> Vec<u32> {
    match group.trim().to_lowercase().as_str() {
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
/// 供协调器把按键映射为候选偏移（与 compile_select_key_group / compile_select_modifier_group 同源）。
///
/// **含修饰键组**：可打印键在 keydown 路径消费，修饰键在 keyup 路径消费（见
/// `compile_select_modifier_group`），两条路径共用本表——协调器只按 VK 查偏移，
/// 不关心它从哪条路来。
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
        "comma_period" => vec![key_hash(0, VK_OEM_COMMA), key_hash(0, VK_OEM_PERIOD)],
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

    #[test]
    fn forward_only_bit_marks_page_and_select_keys_only() {
        let mut cfg = Config::default();
        cfg.keys.toggle_full_width = "shift+space".to_string();
        cfg.keys.select_key_groups = vec!["semicolon_quote".into()];
        cfg.keys.page_keys = vec!["minus_equal".into(), "shift_tab".into()];
        let compiled = Compiler::new(cfg).compile();

        // ⚠ 判据不能用「action 为空」：pin/delete 候选的数字热键 action 同样是空串
        //（动作由服务端按 hash 自认），它们是 session 热键、不该带 FORWARD_ONLY。
        // 只有翻页键组 / 选词键组才是仅注册转发，故按 raw hash 精确点名。
        let forward_only_raw: Vec<u32> = [
            compile_select_key_group("semicolon_quote"),
            compile_page_key_group("minus_equal"),
            compile_page_key_group("shift_tab"),
        ]
        .concat();
        assert_eq!(forward_only_raw.len(), 6, "样例键组应展开出 6 个键");

        for e in &compiled.key_down {
            let expected = forward_only_raw.contains(&e.match_hash);
            assert_eq!(
                e.tsf_hash & HOTKEY_POLICY_FORWARD_ONLY != 0,
                expected,
                "hash=0x{:08X} action={:?} 的 FORWARD_ONLY 位不符预期",
                e.tsf_hash,
                e.action
            );
            // match_hash 是服务端匹配用的裸 hash，任何 policy 位都不该混进去
            assert_eq!(e.match_hash & HOTKEY_POLICY_FORWARD_ONLY, 0);
        }

        // 定桩：shift+space 无任何 policy 位，TSF 侧必须无条件吃。
        let fw = compiled
            .key_down
            .iter()
            .find(|e| e.action == "toggle_full_width")
            .expect("toggle_full_width 应在 key_down 组");
        assert_eq!(fw.tsf_hash, fw.match_hash);
        assert_eq!(fw.tsf_hash, key_hash(MOD_SHIFT, 0x20));
    }

    /// 修饰键选词组只进 key_up（action=select_candidate），绝不进 key_down。
    /// 回归点：曾与 `;'` 一起注册进 key_down，而 TSF 的 keydown 查表用的是
    /// 「通用修饰位 + 笼统 VK_CONTROL」，两个维度都对不上这里登记的「具体位 + VK_LCONTROL」，
    /// 于是这项配置端到端从未生效过——且即便对上了也不能吃（纯修饰键 keydown 必须放行）。
    #[test]
    fn select_modifier_group_registers_on_key_up_only() {
        let mut cfg = Config::default();
        cfg.keys.toggle_mode_keys = vec!["lshift".into(), "rshift".into()];
        cfg.keys.select_key_groups = vec!["lrctrl".into()];
        let compiled = Compiler::new(cfg).compile();

        assert!(
            !compiled
                .key_down
                .iter()
                .any(|e| matches!(e.match_hash & 0xFFFF, VK_LCONTROL | VK_RCONTROL)),
            "修饰键选词组不得出现在 key_down"
        );
        let sel: Vec<&HotkeyEntry> = compiled
            .key_up
            .iter()
            .filter(|e| e.action == "select_candidate")
            .collect();
        assert_eq!(sel.len(), 2, "lrctrl 应展开出左右两个 keyup 登记");
        assert_eq!(
            sel[0].match_hash,
            key_hash(MOD_CTRL | MOD_LCTRL, VK_LCONTROL)
        );
        assert_eq!(
            sel[1].match_hash,
            key_hash(MOD_CTRL | MOD_RCTRL, VK_RCONTROL)
        );
        // 与 toggle 登记同格式（通用位+具体位），否则 C++ GetCurrentModifiers 的双位哈希对不上。
        assert_eq!(
            compiled
                .key_up
                .iter()
                .filter(|e| e.action == "toggle_mode")
                .count(),
            2,
            "切换键登记不应被选词登记挤掉"
        );
    }

    /// 可打印选词组的通路不变：仍在 key_down 且带 FORWARD_ONLY，不进 key_up。
    #[test]
    fn printable_select_group_stays_on_key_down() {
        let mut cfg = Config::default();
        cfg.keys.select_key_groups = vec!["semicolon_quote".into()];
        let compiled = Compiler::new(cfg).compile();
        for raw in compile_select_key_group("semicolon_quote") {
            let e = compiled
                .key_down
                .iter()
                .find(|e| e.match_hash == raw)
                .expect("可打印选词键应在 key_down");
            assert!(e.tsf_hash & HOTKEY_POLICY_FORWARD_ONLY != 0);
        }
        assert!(
            !compiled
                .key_up
                .iter()
                .any(|e| e.action == "select_candidate"),
            "可打印选词键不该跑到 key_up 去"
        );
    }

    #[test]
    fn add_word_hotkeys_carry_global_policy() {
        let mut cfg = Config::default();
        cfg.keys.add_word = "ctrl+equal".to_string();
        cfg.keys.open_add_word_dialog = "ctrl+shift+equal".to_string();
        cfg.keys.toggle_punct = "ctrl+period".to_string();
        let compiled = Compiler::new(cfg).compile();

        let find = |a: &str| {
            compiled
                .key_down
                .iter()
                .find(|e| e.action == a)
                .unwrap()
                .clone()
        };

        // 加词两键：CHINESE_ONLY + GLOBAL 叠加
        for a in ["add_word", "open_add_word_dialog"] {
            let e = find(a);
            assert!(
                e.tsf_hash & HOTKEY_POLICY_GLOBAL != 0,
                "{a} 的 tsf_hash 应带 GLOBAL 位"
            );
            assert!(
                e.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY != 0,
                "{a} 的 tsf_hash 应仍带 CHINESE_ONLY 位"
            );
            // match_hash 是规范化的原始 hash，不含任何 policy 位
            assert_eq!(
                e.match_hash & (HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL),
                0,
                "{a} 的 match_hash 不应含 policy 位"
            );
        }

        // 其它 chinese-only 键（toggle_punct）不该被全局拦截，避免多抢宿主快捷键
        let tp = find("toggle_punct");
        assert!(
            tp.tsf_hash & HOTKEY_POLICY_GLOBAL == 0,
            "toggle_punct 不应带 GLOBAL 位"
        );
        assert!(tp.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY != 0);
    }

    /// 特殊模式直达热键现在写在 `keys.key_actions` 里（`special:<方案id>`），
    /// 编译时映射成分发端认的 `enter_special:<id>`，并带 CHINESE_ONLY | GLOBAL。
    #[test]
    fn special_mode_hotkey_compiles_with_global_policy() {
        let mut cfg = Config::default();
        cfg.keys
            .key_actions
            .insert("ctrl+shift+u".to_string(), "special:rare".to_string());
        let compiled = Compiler::new(cfg).compile();
        let e = compiled
            .key_down
            .iter()
            .find(|e| e.action == "enter_special:rare")
            .expect("key_actions 的 special:<id> 应编出 enter_special:<id>");
        // 与加词键同策略：CHINESE_ONLY | GLOBAL；match_hash 不含任何 policy 位
        assert!(e.tsf_hash & HOTKEY_POLICY_GLOBAL != 0);
        assert!(e.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY != 0);
        assert_eq!(
            e.match_hash & (HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL),
            0
        );
    }

    #[test]
    fn temp_pinyin_hotkey_compiles_with_global_policy() {
        let mut cfg = Config::default();
        cfg.input.temp_pinyin.hotkey = "ctrl+shift+p".to_string();
        let compiled = Compiler::new(cfg).compile();
        let e = compiled
            .key_down
            .iter()
            .find(|e| e.action == "enter_temp_pinyin")
            .expect("temp_pinyin.hotkey 应编出 enter_temp_pinyin");
        assert!(e.tsf_hash & HOTKEY_POLICY_GLOBAL != 0);
        assert!(e.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY != 0);
        assert_eq!(
            e.match_hash & (HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_GLOBAL),
            0
        );
    }

    /// 方案直达热键编出 `switch_schema:<id>`，且**不带 CHINESE_ONLY**。
    ///
    /// 这个 policy 位是本条测试的重点：带上它，切到英文方案后热键就不再响应，
    /// 用户切得过去、切不回来。特殊模式热键需要它（overlay 只在中文输入中途有意义），
    /// 方案切换恰恰相反——同一个位，两种机制下后果相反。
    #[test]
    /// `keys.key_actions` 编出对应动词，且与方案直达热键同策略（不带 CHINESE_ONLY）。
    ///
    /// policy 位对 `toggle_schema` 比对 `switch_schema` 更要命：带上它，切到英文方案后
    /// **回程那一下**就不响应了——功能恰好废掉一半，而"切过去"仍然好用，很容易被当成
    /// 「回程没实现」而不是「策略位配错」。
    #[test]
    fn key_actions_compile_toggle_schema_without_chinese_only_policy() {
        let mut cfg = Config::default();
        cfg.keys.key_actions.insert(
            "ctrl+shift+n".to_string(),
            "toggle_schema:english".to_string(),
        );
        let compiled = Compiler::new(cfg).compile();
        let e = compiled
            .key_down
            .iter()
            .find(|e| e.action == "toggle_schema:english")
            .expect("keys.key_actions 应编出 toggle_schema:<id>");
        assert_eq!(
            e.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY,
            0,
            "往返热键不得带 CHINESE_ONLY，否则从英文方案回不来"
        );
    }

    /// 不支持的动词与解析不了的键都被丢弃，不进热键表。
    ///
    /// 守的是「静默失效」：写错的动词若混进表里，按下时分发端匹配不上，表现是「按了没反应」，
    /// 与热键没注册上完全同形，用户无从分辨自己拼错了还是功能坏了。
    #[test]
    fn key_actions_drop_unknown_verbs_and_unparsable_keys() {
        let mut cfg = Config::default();
        cfg.keys.key_actions.insert(
            "ctrl+shift+n".to_string(),
            "no_such_verb:english".to_string(),
        );
        cfg.keys
            .key_actions
            .insert("ctrl+shift+m".to_string(), "toggle_schema:".to_string());
        cfg.keys.key_actions.insert(
            "这不是热键".to_string(),
            "toggle_schema:english".to_string(),
        );
        let n = Config::default();
        let base = Compiler::new(n).compile().key_down.len();
        let compiled = Compiler::new(cfg).compile();
        assert_eq!(compiled.key_down.len(), base, "三条非法项都不该进热键表");
    }

    #[test]
    fn schema_hotkey_compiles_without_chinese_only_policy() {
        let mut cfg = Config::default();
        cfg.keys
            .schema_hotkeys
            .insert("english".to_string(), "ctrl+shift+n".to_string());
        let compiled = Compiler::new(cfg).compile();
        let e = compiled
            .key_down
            .iter()
            .find(|e| e.action == "switch_schema:english")
            .expect("keys.schema_hotkeys 应编出 switch_schema:<id>");
        assert_eq!(
            e.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY,
            0,
            "方案切换热键不得带 CHINESE_ONLY，否则英文方案下切不回中文方案"
        );
        // 反向对照：同一份编译产物里，特殊模式热键确实是带 CHINESE_ONLY 的——
        // 否则「不带」这条断言在 policy 位整体失效时也会通过。
        let mut cfg2 = Config::default();
        cfg2.keys
            .key_actions
            .insert("ctrl+shift+u".to_string(), "special:rare".to_string());
        let c2 = Compiler::new(cfg2).compile();
        let e2 = c2
            .key_down
            .iter()
            .find(|e| e.action == "enter_special:rare")
            .unwrap();
        assert!(
            e2.tsf_hash & HOTKEY_POLICY_CHINESE_ONLY != 0,
            "对照组：特殊模式热键应带 CHINESE_ONLY"
        );
    }

    /// 多个方案热键按 schema id 排序编译——`HashMap` 迭代序不稳定，两个方案配了同一个
    /// 键时谁先入列决定谁生效，不排序会让同一份配置在不同进程启动时表现不同。
    ///
    /// 用 7 个 id：这条测试本质上是「乱序输入应得到有序输出」，而未排序的 `HashMap` 仍有
    /// 可能碰巧吐出升序。7 个元素把假绿概率压到 1/5040，n 少了这测试就不算数。
    #[test]
    fn schema_hotkeys_compile_in_sorted_order() {
        let ids = ["zzz", "aaa", "mmm", "ddd", "sss", "ggg", "ppp"];
        let mut cfg = Config::default();
        for id in ids {
            cfg.keys
                .schema_hotkeys
                .insert(id.to_string(), format!("ctrl+shift+{}", &id[..1]));
        }
        let compiled = Compiler::new(cfg).compile();
        let order: Vec<&str> = compiled
            .key_down
            .iter()
            .filter_map(|e| e.action.strip_prefix("switch_schema:"))
            .collect();
        let mut expect = ids.to_vec();
        expect.sort();
        assert_eq!(order, expect, "应按 schema id 升序编译");
    }

    /// 动词 id 为空（`special:`）不产生条目；`temp_pinyin.hotkey` 默认空同理。
    ///
    /// 「id 为空」原先是 `special_modes[]` 条目的一个真实陷阱（只写 schema 不写 id 的
    /// 条目会静默不注册热键）。身份收敛到方案 id 后它不可能为空，这里只剩防脏数据。
    #[test]
    fn empty_or_idless_mode_hotkey_produces_no_entry() {
        let mut cfg = Config::default();
        cfg.keys
            .key_actions
            .insert("ctrl+shift+u".to_string(), "special:".to_string());
        cfg.keys
            .key_actions
            .insert("ctrl+shift+i".to_string(), "special:   ".to_string());
        let compiled = Compiler::new(cfg).compile();
        assert!(
            !compiled
                .key_down
                .iter()
                .any(|e| e.action.starts_with("enter_special:") || e.action == "enter_temp_pinyin"),
            "空 hotkey / 空 id 不应产生直达热键条目"
        );
    }

    /// 修饰键的 keyup hash 必须带**通用位 + 具体位**：C++ `GetCurrentModifiers()` 对
    /// 修饰键同时返回两者，只带一边会匹配不上（表现为「绑了没反应」）。
    /// 与 `compile_toggle_mode_key` 同格式——两者服务于同一条 TSF keyup 通路。
    #[test]
    fn modifier_key_up_hash_matches_toggle_format() {
        assert_eq!(
            compile_modifier_key_up_hash(VK_RSHIFT),
            compile_toggle_mode_key("rshift"),
            "同一个键经两条入口应得到同一个 hash"
        );
        assert_eq!(
            compile_modifier_key_up_hash(VK_LCONTROL),
            compile_toggle_mode_key("lctrl")
        );
        // 低 16 位是 VK，供 is_pure_modifier_vk / 分派点反查。
        let h = compile_modifier_key_up_hash(VK_RSHIFT).unwrap();
        assert_eq!(h & 0xFFFF, VK_RSHIFT);
        // 非修饰键没有 keyup 形态（CapsLock 也不在此列：它走 toggle_mode_keys 那条）。
        assert_eq!(compile_modifier_key_up_hash(VK_OEM_1), None);
        assert_eq!(compile_modifier_key_up_hash(VK_CAPITAL), None);
    }

    /// `keys.key_actions` 按**键形态**分三条通路，不按动词。
    #[test]
    fn key_action_routes_split_by_key_shape() {
        use KeyActionRoute::*;
        assert_eq!(route_of_key_action("ctrl+shift+n"), Some(Hotkey));
        assert_eq!(route_of_key_action("ctrl+space"), Some(Hotkey));
        assert_eq!(route_of_key_action("rshift"), Some(ModifierKeyUp));
        assert_eq!(route_of_key_action("lctrl"), Some(ModifierKeyUp));
        assert_eq!(route_of_key_action("backtick"), Some(LeadingKey));
        assert_eq!(route_of_key_action("semicolon"), Some(LeadingKey));
        assert_eq!(route_of_key_action("z"), Some(LeadingKey));
        assert_eq!(route_of_key_action("不存在的键"), None);
    }

    /// ★★ 单个有字符的键**绝不能**进 key_down 热键表。
    ///
    /// `parse_hotkey("backtick")` 返回的是无修饰位的裸 VK（0xC0）。进表后 TSF 会把它
    /// 当热键转发并吞掉，于是 `` ` `` 这个符号在所有方案里都再也打不出来——而用户只是
    /// 想给它绑个功能。这条是本次收编最危险的一处，故单独立测。
    #[test]
    fn single_character_key_never_enters_keydown_hotkeys() {
        let mut cfg = Config::default();
        cfg.keys
            .key_actions
            .insert("backtick".into(), "temp_pinyin".into());
        cfg.keys
            .key_actions
            .insert("semicolon".into(), "mix:quick_mix".into());
        cfg.keys
            .key_actions
            .insert("rshift".into(), "toggle_mode".into());
        cfg.keys
            .key_actions
            .insert("ctrl+shift+n".into(), "toggle_schema:wubi86".into());
        let compiled = Compiler::new(cfg).compile();

        // ★ 判据是「有没有产生**带这个动词的** key_down 条目」，不是「这个 VK 在不在
        // key_down 里」——`;` / `'` 本来就被默认的选词键组 `semicolon_quote` 以
        // FORWARD_ONLY 登记着，按 VK 判会把那条误当成本段的产物，测了个寂寞。
        for verb in ["temp_pinyin", "mix:quick_mix"] {
            assert!(
                !compiled.key_down.iter().any(|e| e.action == verb),
                "单键条目 {verb} 不该产生 key_down 热键，实际 {:?}",
                compiled
                    .key_down
                    .iter()
                    .map(|e| &e.action)
                    .collect::<Vec<_>>()
            );
        }
        // 反向确认分流真的生效：选词键组那条仍在（action 为空的转发登记），
        // 说明上面的「没有」不是因为整段编译被跳过了。
        assert!(
            compiled
                .key_down
                .iter()
                .any(|e| (e.match_hash & 0xFFFF) == VK_OEM_1 && e.action.is_empty()),
            "选词键组的 `;` 转发登记应不受影响"
        );

        // 组合键照常进 key_down（收编不该动这条既有通路）。
        assert!(
            compiled
                .key_down
                .iter()
                .any(|e| e.action == "toggle_schema:wubi86"),
            "组合键条目应仍走热键通路"
        );
        // 修饰键进 key_up，且 action 是 schema_bound 而非动词本身——
        // `is_toggle_mode_keycode` 按 action 过滤，塞动词进去它就认不出来了。
        let up = compiled
            .key_up
            .iter()
            .find(|e| (e.match_hash & 0xFFFF) == VK_RSHIFT)
            .expect("rshift 应进 key_up 转发集");
        assert_eq!(up.action, "schema_bound");
    }
}
