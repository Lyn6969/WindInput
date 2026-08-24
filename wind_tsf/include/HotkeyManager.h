#pragma once

#include "Globals.h"
#include "BinaryProtocol.h"
#include <vector>
#include <unordered_set>

// Hotkey type (what action the key triggers)
enum class HotkeyType {
    None,           // Not a hotkey
    ToggleMode,     // Toggle Chinese/English mode (KeyUp triggered)
    Hotkey,         // Generic hotkey (KeyDown triggered)
    Letter,         // Letter input
    Number,         // Number for candidate selection
    Punctuation,    // Punctuation input
    Backspace,
    Enter,
    Escape,
    Space,
    Tab,
    PageKey,        // Page up/down
    CursorKey,      // Cursor movement (Left/Right/Home/End)
    SelectKey,      // Select candidate 2/3
};

class CHotkeyManager
{
public:
    CHotkeyManager();
    ~CHotkeyManager();

    // Update hotkey whitelist from Go service (binary protocol)
    void UpdateHotkeys(const std::vector<uint32_t>& keyDownHotkeys,
                       const std::vector<uint32_t>& keyUpHotkeys);

    // Check if a KeyDown should be intercepted (O(1) lookup)
    // Returns true if the key matches a KeyDown hotkey in the whitelist
    // 语义：两模式都吃（始终 pfEaten=TRUE）
    BOOL IsKeyDownHotkey(uint32_t keyHash) const;

    // 仅中文模式吃。命中后中文 → 吃；英文 → 透传。
    BOOL IsKeyDownChineseOnlyHotkey(uint32_t keyHash) const;

    // 仅中文模式 + 有 composition / 候选时吃。其它情形透传。
    BOOL IsKeyDownSessionHotkey(uint32_t keyHash) const;

    // 「仅注册转发」的键（翻页键组 / 选词键组）。与 _keyDownHotkeys 叠加的正交标记：
    // 命中它的键在无会话时必须**放行并继续往下走**（不是 return），交给 ClassifyInputKey
    // 按普通标点处理。真动作热键不带此标记，任何时候都吃。
    BOOL IsKeyDownForwardOnlyHotkey(uint32_t keyHash) const;

    // Check if a KeyUp should be intercepted (O(1) lookup)
    // Returns true if the key matches a KeyUp hotkey in the whitelist
    BOOL IsKeyUpHotkey(uint32_t keyHash) const;

    // 该 keyup 键**只有会话语义**（keys.session_actions 里的绑定），没有 toggle 语义。
    //
    // 用途只有一个：CapsLock 的 keydown 该不该吃。配成 toggle_mode_keys 时恒吃（那是它
    // 本来的契约——专职切中英文、不再切大小写）；只配成会话态绑定（如打字时翻页）时，
    // 无会话必须放行，否则用户在**任何时候**都切不动大小写锁定，而他只是想让它在打字时翻页。
    //
    // 两者都配时按 toggle 语义（恒吃）——toggle 要求 keydown 一定被吃，会话语义只是条件吃，
    // 取严格的那个才不会让 toggle 静默失效。
    BOOL IsKeyUpSessionOnlyHotkey(uint32_t keyHash) const;

    // Check if a virtual key is a toggle mode key (Shift/Ctrl for mode switch)
    // This is a fallback that works even without hotkey whitelist sync
    static BOOL IsToggleModeKeyByVK(WPARAM vk);

    // Check if any hotkeys are configured
    BOOL HasHotkeys() const { return !_keyDownHotkeys.empty() || !_keyUpHotkeys.empty(); }

    // Check if a key is a basic input key (letter, number, punctuation)
    // These don't need hotkey lookup, just basic classification
    static HotkeyType ClassifyInputKey(WPARAM vk, uint32_t modifiers);

    // Check if key is punctuation
    static BOOL IsPunctuationKey(WPARAM vk);

    // Convert virtual key to punctuation character
    static wchar_t VirtualKeyToPunctuation(WPARAM vk, BOOL shiftPressed);

    // Calculate key hash for lookup
    static uint32_t CalcKeyHash(uint32_t modifiers, uint32_t keyCode);

    // Get current modifier state
    static uint32_t GetCurrentModifiers();

    // Normalize modifiers for function hotkey matching
    // This strips specific left/right modifiers, keeping only generic modifiers
    // E.g., (ModCtrl | ModLCtrl) -> ModCtrl
    static uint32_t NormalizeModifiers(uint32_t modifiers);

    // Log current configuration (for debugging)
    void LogConfig() const;

    // Hotkey policy bits (与 Go 侧 ipc.HotkeyPolicy* 对齐).
    // Go 在 keyDown 哈希高 2 位编码该热键的"何时吃"策略；C++ 收到后剥离 policy 位、
    // 按 bit 把哈希分流到 _keyDownHotkeys / _keyDownChineseOnly / _keyDownSession.
    static constexpr uint32_t HOTKEY_POLICY_CHINESE_ONLY = 0x40000000;
    static constexpr uint32_t HOTKEY_POLICY_SESSION      = 0x80000000;
    // 全局拦截位（正交，与 CHINESE_ONLY 叠加）：TSF 在中文+文本框时用 RegisterHotKey
    // 把这些键注册为系统级热键，规避 Chromium 类宿主的加速键双处理。
    static constexpr uint32_t HOTKEY_POLICY_GLOBAL       = 0x20000000;
    // 仅注册转发位（正交，与上面三选一叠加）：翻页/选词键组这类无动作的登记项。
    static constexpr uint32_t HOTKEY_POLICY_FORWARD_ONLY = 0x10000000;
    static constexpr uint32_t HOTKEY_POLICY_MASK         = HOTKEY_POLICY_CHINESE_ONLY | HOTKEY_POLICY_SESSION
                                                         | HOTKEY_POLICY_GLOBAL | HOTKEY_POLICY_FORWARD_ONLY;

    // 需全局拦截的热键 raw hash（GLOBAL 位命中，已剥 policy）。供 RegisterHotKey 反解 (mods,vk)。
    const std::unordered_set<uint32_t>& GlobalHotkeys() const { return _globalHotkeys; }

    // SESSION 策略的 keyDown 热键 raw hash（已剥 policy）。供候选热键的 RegisterHotKey
    // 反解 (mods, vk)——语义正好对上：SESSION ＝「只在有会话时吃」，而候选热键正是候选
    // 可见时注册、消失时卸载（见 CTextService::NotifyCandidatesVisibilityChanged）。
    //
    // ★ 有了它，C++ 侧就不必再自己知道「置顶是 Ctrl+数字、删除是 Ctrl+Shift+数字」——
    // 那两组修饰键原先在 _RegisterCandidateHotkeys 与 WM_HOTKEY 分发处各硬编码一份，
    // 与服务端的配置值域是**第三、第四份真相源**，配置里改成别的组合后这条通路照旧只
    // 注册老组合，新组合直接落到宿主手里（2026-08-24 Ctrl+Alt+数字 实测现场）。
    const std::unordered_set<uint32_t>& SessionHotkeys() const { return _keyDownSession; }

private:
    // Hotkey whitelist (KeyDown triggered) — 两模式都吃
    std::unordered_set<uint32_t> _keyDownHotkeys;

    // 仅中文模式吃
    std::unordered_set<uint32_t> _keyDownChineseOnly;

    // 仅中文模式 + 有 session 吃
    std::unordered_set<uint32_t> _keyDownSession;

    // 需全局拦截（RegisterHotKey）的热键 raw hash，与 _keyDownChineseOnly 叠加（正交标记）
    std::unordered_set<uint32_t> _globalHotkeys;

    // 仅注册转发的键 raw hash（翻页/选词键组），与 _keyDownHotkeys 叠加（正交标记）
    std::unordered_set<uint32_t> _keyDownForwardOnly;

    // Hotkey whitelist (KeyUp triggered - for toggle mode keys)
    std::unordered_set<uint32_t> _keyUpHotkeys;

    // keyup 登记按语义再分两个正交子集（都已剥 policy 位，与 _keyUpHotkeys 同 key）：
    // 带 SESSION 位的进 _keyUpSession（keys.session_actions 的绑定），其余进 _keyUpNonSession
    // （toggle_mode / select_candidate / schema_bound）。同一个键可能两边都有。
    std::unordered_set<uint32_t> _keyUpSession;
    std::unordered_set<uint32_t> _keyUpNonSession;
};
