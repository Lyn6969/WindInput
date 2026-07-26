#pragma once

#include "Globals.h"
#include "IPCClient.h"
#include <string>
#include <cstdint>
#include <map>
#include <set>
#include <vector>
#include <utility>

class CTextService;

// Lightweight pair engine for English auto-pair (C++ side, for English mode)
class PairEngine {
public:
    struct Entry { wchar_t left; wchar_t right; };

    void SetEnabled(bool enabled) { _enabled = enabled; }
    bool IsEnabled() const { return _enabled; }

    void SetPairs(const std::vector<std::pair<wchar_t, wchar_t>>& pairs) {
        _pairMap.clear();
        _rightSet.clear();
        for (auto& p : pairs) {
            _pairMap[p.first] = p.second;
            _rightSet.insert(p.second);
        }
        _stack.clear();
    }

    bool IsLeft(wchar_t ch) const { return _pairMap.count(ch) > 0; }
    bool IsRight(wchar_t ch) const { return _rightSet.count(ch) > 0; }
    wchar_t GetRight(wchar_t left) const {
        auto it = _pairMap.find(left);
        return it != _pairMap.end() ? it->second : 0;
    }

    void Push(wchar_t left, wchar_t right) { _stack.push_back({left, right}); }
    bool Peek(Entry& entry) const {
        if (_stack.empty()) return false;
        entry = _stack.back();
        return true;
    }
    bool Pop(Entry& entry) {
        if (_stack.empty()) return false;
        entry = _stack.back();
        _stack.pop_back();
        return true;
    }
    void Clear() { _stack.clear(); }
    bool IsEmpty() const { return _stack.empty(); }

private:
    std::map<wchar_t, wchar_t> _pairMap;
    std::set<wchar_t> _rightSet;
    std::vector<Entry> _stack;
    bool _enabled = false;
};

class CKeyEventSink : public ITfKeyEventSink,
                      public ITfKeyTraceEventSink
{
public:
    CKeyEventSink(CTextService* pTextService);
    ~CKeyEventSink();

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj);
    STDMETHODIMP_(ULONG) AddRef();
    STDMETHODIMP_(ULONG) Release();

    // ITfKeyEventSink
    STDMETHODIMP OnSetFocus(BOOL fForeground);
    STDMETHODIMP OnTestKeyDown(ITfContext* pContext, WPARAM wParam, LPARAM lParam, BOOL* pfEaten);
    STDMETHODIMP OnKeyDown(ITfContext* pContext, WPARAM wParam, LPARAM lParam, BOOL* pfEaten);
    STDMETHODIMP OnTestKeyUp(ITfContext* pContext, WPARAM wParam, LPARAM lParam, BOOL* pfEaten);
    STDMETHODIMP OnKeyUp(ITfContext* pContext, WPARAM wParam, LPARAM lParam, BOOL* pfEaten);
    STDMETHODIMP OnPreservedKey(ITfContext* pContext, REFGUID rguid, BOOL* pfEaten);

    // ITfKeyTraceEventSink
    STDMETHODIMP OnKeyTraceDown(WPARAM wParam, LPARAM lParam);
    STDMETHODIMP OnKeyTraceUp(WPARAM wParam, LPARAM lParam);

    // Initialize/Uninitialize
    BOOL Initialize();
    void Uninitialize();

    // Reset composing state (called when focus is lost or input field changes)
    // 注意: _lastPassthroughDigit 不在此清零。它是跨 IME 会话的上下文信号，
    // 用于 Excel/WPS cell-select(按数字直通) → cell-edit(按标点) 这种焦点切换
    // 场景的数字后智能标点判断。残留由按键事件路径（_SendKeyToService 非智能
    // 标点目标键清零）和光标 Y 跨行检测兜底，不应在 IME 会话状态重置时一起清。
    void ResetComposingState() { _isComposing = FALSE; _hasCandidates = FALSE; _needsCompositionResync = FALSE; _resyncDeadline = 0; _resyncFailStreak = 0; _skipKeyCount = 0; _pendingPairAction = {}; _englishPairEngine.Clear(); _pairPendingDepth = 0; }

    // Flush pending English pass-through stats before focus/mode teardown.
    void FlushEnglishStats();

    // Status queries
    BOOL IsStatsTrackingEnglish() const { return _statsEnabled && _statsTrackEnglish; }
    BOOL IsEnglishAutoPairEnabled() const { return _englishPairEngine.IsEnabled(); }

    // Handle config sync from Go service (called from async reader thread)
    void OnSyncConfig(const std::string& key, const std::vector<uint8_t>& value);

    // Called when composition is unexpectedly terminated by the application
    // This resets state and notifies Go service to clear input buffer
    void OnCompositionUnexpectedlyTerminated();

    // 从 RegisterHotKey/WM_HOTKEY 路径派发一个键事件给 Go 服务。
    // 用于 Pin/Delete 候选热键被系统级 RegisterHotKey 拦截后，转发给我们的常规处理流程。
    // vk: 虚拟键码（Win32 VK_*）；mods: 内部 KEYMOD_* 修饰位（不是 TSF MOD_*）。
    BOOL DispatchHotkey(uint32_t vk, uint32_t mods);

    // 供 CTextService 的 SendInput 兜底路径（CommitText/InsertText/ReplacePrecedingChars）
    // 调用：把即将注入的按键标记为"自生成"，OnTestKeyDown/OnTestKeyUp 见到后直接放行，
    // 不会被本 IME 自己的按键逻辑当成真实用户按键二次处理。用于 TSF EditSession 兜底
    // 到 SendInput 的宿主（部分终端模拟器/微信/纯文本编辑器）——否则注入的退格/字符键
    // 会被自己的钩子截获重新处理，导致文字重复上屏（同 _SimulatePairKey 用的机制）。
    void MarkSyntheticKey(WORD vk) { _PushSkipKey(vk); }

private:
    static constexpr uint32_t ENGLISH_STATS_REPORT_COUNT = 5;
    static constexpr ULONGLONG ENGLISH_STATS_REPORT_INTERVAL_MS = 5000;

    LONG _refCount;
    CTextService* _pTextService;
    DWORD _dwKeySinkCookie;
    DWORD _dwKeyTraceSinkCookie;
    bool _statsEnabled = true;
    bool _statsTrackEnglish = true;

    // State
    BOOL _isComposing;
    BOOL _hasCandidates;         // True if there are candidates to select
    // 配对跳出键（VK 码集合，由 core 经 CONFIG_KEY_JUMP_OUT_KEYS 推送）。英文模式配对直接
    // 据此跳出；中文模式仅用于「有待跳出配对」时放行 Enter 等被会话门控的键转发给协调器。
    std::set<UINT> _jumpOutKeys;
    // 待跳出配对深度：收到 InsertTextWithCursor(配对插入)时 +1，MoveCursorRight(跳出)时 -1，
    // 会话/焦点复位归零（见 ResetComposingState）。中文模式据此判断 Enter 等键是否该转发。
    int _pairPendingDepth = 0;
    bool _IsJumpOutKey(UINT vk) const { return _jumpOutKeys.count(vk) > 0; }
    // 输入右符号本身是否跳出（配置 jump_out_keys 里的 `right_symbol` 特殊值）。右符号不是
    // 固定按键（取决于配对表），故与 _jumpOutKeys 分开存。对称配对（引号）不受此项影响。
    bool _jumpOutOnRightSymbol = false;
    // IPC 失败后置位：本地 composition 已强制复位，但 Go 侧可能仍持有活跃会话状态。
    // 下一次按键前提下视作"有会话"，让 ENTER/ESC 也能发给 Go 走重握手；
    // 任何一次成功 ReceiveResponse 之后清旗，状态由响应处理路径自然重建。
    BOOL _needsCompositionResync;
    // resync 自愈窗口：deadline 到期或连续失败超限后自动放弃，避免 Go/IPC 长时间不可用
    // 时把 ENTER/ESC/Ctrl+Alt 等键永久吃掉。失败 streak 在响应成功后清零。
    DWORD _resyncDeadline;        // GetTickCount() 时间戳，0 表示无 deadline
    int   _resyncFailStreak;      // 连续 IPC 失败次数，超过 RESYNC_MAX_RETRIES 强制降级 passthrough
    static constexpr DWORD RESYNC_WINDOW_MS = 3000;
    static constexpr int   RESYNC_MAX_RETRIES = 3;
    BOOL _IsResyncActive();       // 读旗+过期检查；过期会自动清旗

    // 「当前是否有活跃输入会话」的**唯一判据**——决定各类键归本输入法还是透传宿主。
    // 曾以三份等价表达式散落在 OnTestKeyDown / OnKeyDown / session 热键三处，direct_commit
    // 顶码新增 defer 真空期时只有部分被更新，导致该窗口内空格/退格直落宿主（见
    // project_top_commit_mode）。任何新的「算不算有会话」的状态，只加到这里。
    BOOL _HasInputSession();

    WCHAR _lastPassthroughDigit; // Last digit key that passed through (for smart punct fallback in apps where TSF can't read text)
    uint32_t _pendingKeyUpKey;   // Key code of pending KeyUp toggle key
    uint32_t _pendingKeyUpModifiers; // Modifiers when KeyDown was pressed
    DWORD    _pendingKeyDownTime;    // GetTickCount() when toggle key was pressed down

    // Maximum duration (ms) for a toggle key press to count as a "tap"
    // Long presses beyond this threshold will NOT trigger mode toggle
    static constexpr DWORD TOGGLE_TAP_THRESHOLD_MS = 500;

    // ========================================================================
    // Modifier key state machine (replaces GetAsyncKeyState for consistency)
    // ========================================================================
    uint32_t _modsState;         // Current modifier state (maintained by KeyDown/KeyUp)
    uint16_t _eventSeq;          // Monotonic event sequence number

    // State machine update methods
    void _UpdateModsOnKeyDown(WPARAM vk);
    void _UpdateModsOnKeyUp(WPARAM vk);
    uint32_t _GetModsSnapshot() const { return _modsState; }
    uint8_t _GetTogglesSnapshot() const;
    uint16_t _GetNextEventSeq() { return _eventSeq++; }

    // Sync state from Go response
    void _SyncStateFromResponse(uint32_t statusFlags);

    // ========================================================================
    // Barrier mechanism for async commit requests
    // ========================================================================
    struct PendingBarrier
    {
        uint16_t barrierSeq;
        std::wstring composition;  // Composition at request time
        DWORD requestTime;         // GetTickCount() at request
        bool waiting;
    };

    uint16_t _nextBarrierSeq;
    PendingBarrier _pendingCommit;

    // Barrier timeout (if Go doesn't respond, fallback handling)
    static constexpr DWORD BARRIER_TIMEOUT_MS = 500;

    BOOL _SendCommitRequest(uint16_t barrierSeq, uint16_t triggerKey, uint32_t mods, const std::string& inputBuffer);
    void _HandleCommitResult(uint16_t barrierSeq, const std::wstring& text, const std::wstring& newComp, bool modeChanged, bool chineseMode);
    void _CheckBarrierTimeout();

    // ========================================================================
    // Helper methods
    // ========================================================================
    BOOL _IsMatchingKeyUp(WPARAM wParam, uint32_t pendingKey);
    // Dispatch the pending toggle key (Shift/Ctrl) to Go service and clear _pendingKeyUpKey.
    // Called from both OnTestKeyUp and OnKeyUp; clearing in the first caller makes the second a no-op.
    // Returns TRUE if a toggle was matched (caller should set pfEaten=TRUE).
    BOOL _DispatchPendingToggleKeyUp(WPARAM wParam);

    // 记录「该切换键正等 keyup 触发切换」。OnTestKeyDown 与 OnKeyDown 都会调用：
    // 纯修饰键放行后 TSF 未必再调 OnKeyDown，只在后者记录会让切换失灵。幂等。
    void _MarkPendingToggleKey(WPARAM wParam, uint32_t modifiers);
    BOOL _SendKeyToService(uint32_t keyCode, uint32_t modifiers, uint8_t eventType);
    BOOL _HandleServiceResponse(); // Returns TRUE if key was handled, FALSE to pass through

    // Context state checking (for browser non-editable area detection)
    BOOL _IsContextReadOnly(ITfContext* pContext);

    // ========================================================================
    // Auto-pair key simulation (deferred + skip list approach)
    // ========================================================================
    void _SimulatePairKey(WORD vk);
    static bool _AreModifiersHeld();

    // Pending auto-pair action (deferred until modifiers released)
    struct PendingPairAction {
        WORD vk = 0;
        int count = 0;
        bool active = false;
    };
    PendingPairAction _pendingPairAction;

    // English auto-pair engine (handles pairing in English mode)
    PairEngine _englishPairEngine;

    // Skip list: SendInput keys generated by auto-pair / CommitText·InsertText·
    // ReplacePrecedingChars 兜底路径的，应该绕过 IME 处理。后者可能一次注入一整段
    // 文本（每字符一个 skip 条目），容量比原来只服务 auto-pair 时留宽一些。
    static constexpr int MAX_SKIP_KEYS = 64;
    WORD _skipKeys[MAX_SKIP_KEYS] = {};
    int _skipKeyCount = 0;
    void _PushSkipKey(WORD vk);
    BOOL _TryConsumeSkipKey(WPARAM wParam);

    // English mode input stats counter
    struct EnglishStatsCounter {
        uint32_t chars = 0;   // a-z, A-Z
        uint32_t digits = 0;  // 0-9
        uint32_t puncts = 0;  // punctuation/symbols
        uint32_t spaces = 0;  // spaces
        ULONGLONG lastReportTick = 0;

        uint32_t Total() const { return chars + digits + puncts + spaces; }

        void StartIfIdle() {
            if (Total() == 0 || lastReportTick == 0)
                lastReportTick = GetTickCount64();
        }

        uint32_t ElapsedMs() const {
            if (lastReportTick == 0)
                return 0;
            ULONGLONG elapsed = GetTickCount64() - lastReportTick;
            return elapsed > UINT32_MAX ? UINT32_MAX : (uint32_t)elapsed;
        }

        void RecordChar(char ch) {
            StartIfIdle();
            if ((ch >= 'a' && ch <= 'z') || (ch >= 'A' && ch <= 'Z')) chars++;
            else if (ch >= '0' && ch <= '9') digits++;
            else if (ch == ' ') spaces++;
            else if (ch >= 0x21 && ch <= 0x7E) puncts++;
        }

        bool ShouldReport() const {
            uint32_t total = Total();
            return total >= ENGLISH_STATS_REPORT_COUNT ||
                   (total > 0 && lastReportTick != 0 && GetTickCount64() - lastReportTick >= ENGLISH_STATS_REPORT_INTERVAL_MS);
        }

        void Reset() {
            chars = digits = puncts = spaces = 0;
            lastReportTick = 0;
        }
    };
    EnglishStatsCounter _englishStats;
    void _RecordEnglishKeyTrace(WPARAM wParam, uint32_t modifiers);
    void _ReportEnglishStats();
};
