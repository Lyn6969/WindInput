#pragma once

#include "Globals.h"
#include "BinaryProtocol.h" // HostWindowKind / HOST_WINDOW_KIND_COUNT for the host window array
#include <string>
#include <vector>
#include <utility>

// Forward declarations
class CKeyEventSink;
class CIPCClient;
class CLangBarItemButton;
class CCaretEditSession;
class CDisplayAttributeProvider;
class CHotkeyManager;
class CHostWindow;
struct ServiceResponse;

class CTextService : public ITfTextInputProcessorEx,
                     public ITfThreadMgrEventSink,
                     public ITfThreadFocusSink,
                     public ITfCompositionSink,
                     public ITfDisplayAttributeProvider,
                     public ITfTextLayoutSink,
                     public ITfTextEditSink,
                     public ITfCompartmentEventSink,
                     // ITfCandidateListUIElementBehavior 已继承 ITfCandidateListUIElement (已继承 ITfUIElement)，
                     // 只列一个最派生的即可。
                     public ITfCandidateListUIElementBehavior,
                     // ITfFunctionProvider — 通过 ITfSourceSingle::AdviseSingleSink 注册自己为
                     // 该 IME 实例的 Function Provider。这是其它成熟 TSF IME 都做的事，
                     // 让 Chromium / QQNT 等宿主将我们识别为"完整 IME"，走 IME-first 调度。
                     public ITfFunctionProvider
{
    friend class CUpdateCompositionEditSession;
    friend class CEndCompositionEditSession;
    friend class CCommitTextEditSession;
    friend class CReplaceBackwardEditSession;
    friend class CInsertTextEditSession;
public:
    CTextService();
    ~CTextService();

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj);
    STDMETHODIMP_(ULONG) AddRef();
    STDMETHODIMP_(ULONG) Release();

    // ITfTextInputProcessor
    STDMETHODIMP Activate(ITfThreadMgr* pThreadMgr, TfClientId tfClientId);
    STDMETHODIMP Deactivate();

    // ITfTextInputProcessorEx
    STDMETHODIMP ActivateEx(ITfThreadMgr* pThreadMgr, TfClientId tfClientId, DWORD dwFlags);

    // ITfThreadMgrEventSink
    STDMETHODIMP OnInitDocumentMgr(ITfDocumentMgr* pDocMgr);
    STDMETHODIMP OnUninitDocumentMgr(ITfDocumentMgr* pDocMgr);
    STDMETHODIMP OnSetFocus(ITfDocumentMgr* pDocMgrFocus, ITfDocumentMgr* pDocMgrPrevFocus);
    STDMETHODIMP OnPushContext(ITfContext* pContext);
    STDMETHODIMP OnPopContext(ITfContext* pContext);

    // ITfThreadFocusSink — 线程级焦点通知（应用进程 foreground 变化）。
    // 与 ITfThreadMgrEventSink::OnSetFocus（文档级别）不同。
    // 实现这个接口让我们在 TSF 注册表上看起来像"现代 IME"，让 Chromium / QQNT 等
    // 宿主走完整 IME-first 调度路径而非 fallback。
    STDMETHODIMP OnSetThreadFocus();
    STDMETHODIMP OnKillThreadFocus();

    // ITfUIElement — 候选 UI 元素基础接口。
    // 与 ITfCandidateListUIElement 一起使 IME 在 TSF 中表现为"现代 IME"，让
    // Chromium 类宿主走完整 IME-first 调度。当前用 stub 数据验证 Begin/EndUIElement
    // 注册本身是否影响调度。
    STDMETHODIMP GetDescription(BSTR* pbstrDescription);
    STDMETHODIMP GetGUID(GUID* pguid);
    STDMETHODIMP Show(BOOL bShow);
    STDMETHODIMP IsShown(BOOL* pbShow);

    // ITfCandidateListUIElement — 候选列表元数据（stub）。
    STDMETHODIMP GetUpdatedFlags(DWORD* pdwFlags);
    STDMETHODIMP GetDocumentMgr(ITfDocumentMgr** ppdim);
    STDMETHODIMP GetCount(UINT* puCount);
    STDMETHODIMP GetSelection(UINT* puIndex);
    STDMETHODIMP GetString(UINT uIndex, BSTR* pstr);
    STDMETHODIMP GetPageIndex(UINT* pIndex, UINT uSize, UINT* puPageCnt);
    STDMETHODIMP SetPageIndex(UINT* pIndex, UINT uPageCnt);
    STDMETHODIMP GetCurrentPage(UINT* puPage);

    // ITfCandidateListUIElementBehavior — 接收 TSF 对候选的操作（stub no-op）。
    STDMETHODIMP SetSelection(UINT nIndex);
    STDMETHODIMP Finalize(void);
    STDMETHODIMP Abort(void);

    // 候选可见状态变化时调用，控制 BeginUIElement / EndUIElement / UpdateUIElement.
    // hasCandidates: 新的候选可见状态。线程：与 KeyEventSink 状态变更同一线程。
    void NotifyCandidatesVisibilityChanged(BOOL hasCandidates);

    // ITfFunctionProvider — 把自己以 IID_ITfFunctionProvider 形式注册到 TSF 的
    // ITfSourceSingle（每个 IME 实例只有一个 function provider）。
    // 注意 GetDescription 与 ITfUIElement::GetDescription 同签名 (BSTR*)，
    // C++ 多继承合并为单一 vtable entry，复用同一实现即可（都是给宿主显示的字符串）。
    STDMETHODIMP GetType(GUID* pguid);
    STDMETHODIMP GetFunction(REFGUID rguid, REFIID riid, IUnknown** ppunk);

    // ITfCompositionSink
    STDMETHODIMP OnCompositionTerminated(TfEditCookie ecWrite, ITfComposition* pComposition);

    // ITfDisplayAttributeProvider
    STDMETHODIMP EnumDisplayAttributeInfo(IEnumTfDisplayAttributeInfo** ppEnum);
    STDMETHODIMP GetDisplayAttributeInfo(REFGUID guid, ITfDisplayAttributeInfo** ppInfo);

    // ITfTextLayoutSink
    STDMETHODIMP OnLayoutChange(ITfContext* pContext, TfLayoutCode lCode, ITfContextView* pView);

    // ITfTextEditSink
    STDMETHODIMP OnEndEdit(ITfContext* pContext, TfEditCookie ecReadOnly, ITfEditRecord* pEditRecord);

    // ITfCompartmentEventSink
    STDMETHODIMP OnChange(REFGUID rguid);

    // Get thread manager
    ITfThreadMgr* GetThreadMgr() { return _pThreadMgr; }

    // Get client ID
    TfClientId GetClientId() { return _tfClientId; }

    // Get IPC client
    CIPCClient* GetIPCClient() { return _pIPCClient; }

    // Get hotkey manager
    CHotkeyManager* GetHotkeyManager() { return _pHotkeyManager; }

    // Insert text into current context
    BOOL InsertText(const std::wstring& text);

    // Update composition text (Inline Composition)
    // noUnderline: 整段不设下划线显示属性（智能符号 HoldComposition 用，
    // 观感与已上屏文本一致；文本仍在组合态内，可被 press2 替换/超时提交）。
    BOOL UpdateComposition(const std::wstring& text, int caretPos, BOOL noUnderline = FALSE);

    // Commit text atomically (end composition + insert text in one EditSession)
    // fromHoldTimer=TRUE：来自智能符号 HoldComposition 超时收口（裸 WM_TIMER 回调）——
    // 改用异步编辑会话（TF_ES_ASYNCDONTCARE）且不走 SendInput 兜底，规避 Word 在
    // 计时器上下文拒发同步会话（TS_E_SYNCHRONOUS）导致的重复上屏。见 .cpp 注释。
    //
    // replacingHeld=TRUE：本次提交要**替换**掉 hold 预览态里那个待定的中文符号
    // （智能符号 press2：「。」→「.」）。默认 FALSE = 追加语义，held 符号并入 prefix
    // 与本次文本一起上屏——因为提交用的是组合 range 的 SetText，不并入就会被覆盖掉。
    // 由服务端在 CommitText 响应的 flags bit3 显式声明，见 COMMIT_FLAG_REPLACING_HELD。
    BOOL CommitText(const std::wstring& text, BOOL fromHoldTimer = FALSE,
                    BOOL replacingHeld = FALSE);

    // 把光标前 count 个已上屏字符替换为 text（智能符号纠错替换）。
    // 优先走 TSF 同步 EditSession（原子、不受输入队列时序/修饰键影响）；
    // 失败时回退到 SendInput（count 次 Backspace + Unicode 注入 text）。
    BOOL ReplacePrecedingChars(int count, const std::wstring& text);

    // End current composition.
    // pDocMgrHint: composition 所属的 DocMgr。**给出即权威**——实现不会再去问 GetFocus()，
    // 因为收口时机可能晚于焦点转移（doc_changed 路径），那时 GetFocus() 指向的是新文档，
    // 拿它跑 EditSession 会用新 context 的 cookie 去清旧 context 的 range。
    // 不给则回落 GetFocus()（其余调用点都在焦点未变时触发）。
    // 清空 composition 范围后再 EndComposition，否则 Excel/WPS 等表格类宿主会把残留
    // composition 文本提交到目标 doc。
    void EndComposition(ITfDocumentMgr* pDocMgrHint = nullptr);

    // Reset KeyEventSink composing state (called after push pipe commit/clear)
    // keepPairState=TRUE 时保留自动配对状态，语义见 CKeyEventSink::ResetComposingState。
    void ResetComposingState(BOOL keepPairState = FALSE);

    // 输入态整体清理：结束 composition + 通知服务端清 buffer + 复位 KeyEventSink 会话态。
    // 触发时机**不是**「失去焦点」而是「离开了原来那个文档」——失焦那一刻无从区分抖动
    // 与真正的切换（见 OnSetFocus 判据注释）。两条进入路径共用本函数（OnKillThreadFocus /
    // doc_changed），靠 _focusLostSent 去重。pDocMgrHint 传**离开的那个 doc**（composition
    // 就建在它上面），EndComposition 会直接采信它而不再问 GetFocus()——此刻焦点可能已经
    // 在新文档上了。
    // reason 取 FOCUS_LOST_REASON_*（THREAD / DOC_CHANGED），决定服务端清哪些状态。
    // sendFocusLost=FALSE 时只做本地清理、不通知服务端失焦：新 DocMgr 若会被
    // XamlIsland locked 守卫跳过 focus_gained，发出去的 focus_lost 就没有配对者，
    // 服务端 ime_active 会被永久清掉（实测 explorer 地址栏工具栏消失）。
    void CleanupInputStateForDocChange(ITfDocumentMgr* pDocMgrHint, uint8_t reason,
                                       BOOL sendFocusLost = TRUE);

    // 焦点离开可编辑控件时通知服务端隐藏工具栏（发 FOCUS_LOST_REASON_CTX_LOST）。
    // **只翻可见性标志，不碰输入态**——这是它能在 DocMgr 噪声层安全调用的前提，
    // 实现处有完整说明。靠 _editCtxReported 去重。
    void _ReportEditContextLost();

    // Top-code commit: accumulate the committed text into the pending prefix and
    // keep it INSIDE the composition (Microsoft IME behavior — the real document
    // commit is deferred to the final CommitText). See _pendingCommitPrefix.
    BOOL InsertTextAndStartComposition(const std::wstring& insertText, const std::wstring& newComposition);

    // Length (in wchars) of the pending top-code commit prefix shown at the head
    // of the composition. Used to segment display attributes and to offset the
    // composition-start coordinate reported to the engine (candidate anchor).
    size_t GetPendingCommitPrefixLength() const { return _pendingCommitPrefix.length(); }

    // 把「已决定要提交」的文本并入待提交前缀，但不结束组合、不真提交（真提交推迟到
    // 最终 CommitText）。用于智能标点顶屏的聚合：候选并入 prefix、中文符号仍作 held 放
    // 同一组合，规避「真提交+立即重开组合」被 diff 式宿主（微信/Tabby/终端）误读吞字。
    // 只并入承诺提交的候选——held 符号勿并入（press2 要替换它，见 CommitAndHold 处注释）。
    void PinCommitTextToPrefix(const std::wstring& text) { _pendingCommitPrefix += text; }

    // Get and consume cached character before caret (set by ITfTextEditSink::OnEndEdit).
    // Returns the cached value and clears it to prevent stale values persisting across
    // key events in apps where OnEndEdit fires late or not at all (e.g., WeChat).
    WCHAR ConsumeCachedPrevChar() { WCHAR c = _cachedPrevChar; _cachedPrevChar = 0; return c; }

    // Get and send caret position to Go Service
    BOOL GetCaretPosition(LONG* px, LONG* py, LONG* pHeight);
    void SendCaretPositionUpdate();

    // Get caret position using TSF APIs (more accurate for browsers)
    BOOL GetCaretPositionFromTSF(LONG* px, LONG* py, LONG* pHeight);
    BOOL GetCompositionStartPosition(LONG* px, LONG* py);

    // Input mode control
    void ToggleInputMode();
    void SetInputMode(BOOL bChineseMode);  // Set mode from service response (no IPC)
    void HandleCtrlSpaceToggle();          // Handle Ctrl+Space internally (bypasses system compartment toggle)
    BOOL IsChineseMode() { return _bChineseMode; }
    BOOL IsFullWidth() { return _bFullWidth; }
    BOOL IsKeyboardDisabled() { return _bKeyboardDisabled; }
    // 密码框强制英文抑制当前是否生效（**镜像** core 的 `apply_input_diag`：命中密码
    // InputScope 位 + compartment 未禁用 + 策略开关开）。DLL 必须能自行判定：吃键决策在
    // OnTestKeyDown 完成，早于 IPC，仅靠 core 回 PassThrough 会「吃了再吐」丢键。
    BOOL IsPasswordSuppressActive() const;
    void SetPasswordSuppressEnabled(BOOL bEnabled) { _passwordSuppressEnabled = bEnabled; }
    ULONGLONG GetFocusSessionId() const { return _focusSessionId; }
    // 记录 CapsLock 按键活动时刻（物理按键或服务端 cancel_on_mode_switch 的注入）。
    // Windows 输入系统会在 CapsLock 状态变化后联动写 OPENCLOSE compartment；
    // OnCompartmentChange 据此时间戳抑制该联动噪声，防止被误判为用户模式切换。
    void NoteCapsLockKeyActivity() { _lastCapsKeyTick = GetTickCount64(); }
    // 当前实例是否持有输入焦点（OnSetFocus 最后一次收到非 null 的 pDocMgrFocus）。
    // 用于服务重启时避免对无焦点实例触发工具栏显示。
    BOOL HasFocus() const { return _hasFocus; }
    // TRUE when the focused document manager has an editable (non-readonly,
    // non-transitory) context. FALSE when e.g. Chrome passes a doc manager
    // with no active text field (its context is TF_SD_READONLY).
    BOOL HasTextInputContext() const { return _hasTextInputContext; }
    // Lazy re-check via GetFocus() + _DocMgrHasEditableContext(). Updates and
    // returns _hasTextInputContext. Called from KeyEventSink when the cached
    // value is FALSE to handle late-arriving focus changes.
    BOOL RefreshTextInputContext();

    // Check if there's an active composition
    BOOL HasActiveComposition() { return _pComposition != nullptr; }

    // Clear the "composition just started" flag (used by timer fallback path).
    // 同时作废 EditSession 缓存：缓存是 StartComposition EditSession 内部抓的，
    // 那一刻宿主的 reflow 还没完成，缓存坐标是陈旧的。timer 触发时（reflow 已
    // 完成的时刻）必须强制 SendCaretPositionUpdate 走 GetCaretPosition 路径
    // 重新做 EditSession 查询，拿到 reflow 后的真实坐标。
    void ClearCompositionJustStarted()
    {
        _compositionJustStarted = FALSE;
        _hasCachedCaretPos = FALSE;
        _hasCachedCompStartPos = FALSE;
    }

    // Check if last edit session was async (Weasel optimization)
    BOOL IsAsyncEdit() { return _asyncEdit; }
    void ClearAsyncEdit() { _asyncEdit = FALSE; }

    // Update language bar Caps Lock state
    void UpdateCapsLockState(BOOL bCapsLock);

    // Send menu command to Go service
    void SendMenuCommand(const char* command);

    // Send show context menu request to Go service (screen coordinates)
    void SendShowContextMenu(int screenX, int screenY);

    // Update full status from Go service response
    // iconLabel: display text from Go service for taskbar icon (e.g., "中", "英", "A", "拼")
    void UpdateFullStatus(BOOL bChineseMode, BOOL bFullWidth, BOOL bChinesePunct, BOOL bToolbarVisible, BOOL bCapsLock, const wchar_t* iconLabel = nullptr);

    // HoldComposition: 开启组合显示 text，timeoutMs 毫秒后自动提交中文（智能符号方案）。
    // press2 到来前的任何 CommitText 调用会先通过 CancelHoldTimer 取消定时器。
    BOOL HoldComposition(const std::wstring& text, UINT timeoutMs);

    // 取消 HoldComposition 计时器（若活跃）。安全：_hHoldTimer==0 时为空操作。
    void CancelHoldTimer();

    // 若 HoldComposition 计时器活跃，立即提交中文符号（宿主中断组合时调用，如 PassThrough 键）。
    void FlushHoldCompositionIfActive();

    // HoldComposition 计时器是否活跃 ⇔ 组合内只有待定的中文符号（外加已承诺提交的 prefix），
    // 不含任何编码——「智能符号预览态」的精确判据。
    // ⚠️ 判据只能是计时器：`_pendingCommitPrefix` 非空在顶码 pre_confirm 聚合时同样成立，
    // 那是真输入会话，拿它当判据会把顶码路径一并误判掉。
    BOOL IsHoldCompositionActive() const { return _hHoldTimer != 0; }

    // 若 HoldComposition 计时器活跃，把 held 符号定格并入 _pendingCommitPrefix（不 commit、
    // 不动文档），供"定格旧符号 + 立即更新/开启组合"场景（连续智能符号、符号后快速输入）
    // 在单一 EditSession 内完成显示更新——规避「commit+立即重启组合」在 Chromium/WPS
    // 下被整锁 diff 误读成替换（与顶码聚合 7f616c2 同思路）。最终 CommitText 一次收口。
    void AbsorbHeldIntoPrefix();

    // direct_commit 顶码：真提交后，余码新组合延迟到触发键 keyup（或兜底定时器）才开。
    // 与 HoldComposition 计时器状态并列、互不干扰。见 top-commit-mode 设计文档 §5。
    void StashDeferredComposition(const std::wstring& composition, UINT fallbackMs);
    void StartDeferredCompositionIfPending();   // keyup / 兜底定时器 / flush 统一入口
    void CancelDeferredComposition();
    BOOL HasDeferredComposition() const { return !_deferredCompText.empty(); }

private:
    LONG _refCount;
    ITfThreadMgr* _pThreadMgr;
    TfClientId _tfClientId;
    DWORD _dwThreadMgrEventSinkCookie;
    DWORD _dwThreadFocusSinkCookie;
    DWORD _uiElementId;     // ITfUIElementMgr::BeginUIElement 返回的 ID；TF_INVALID_UIELEMENTID 表示未注册
    BOOL  _uiElementShown;  // 当前 IsShown 返回值
    ITfUIElementMgr* _pUIElementMgr;  // 缓存的 UI element 管理器引用，避免每次候选变化都 QI
    ITfSourceSingle* _pSourceSingle;  // 缓存的 ITfSourceSingle 引用（Function Provider 注册用）
    BOOL  _funcProviderRegistered;    // 是否已通过 AdviseSingleSink 注册

    // Win32 RegisterHotKey 支持 — 在候选可见时把 Ctrl+0..9 / Ctrl+Shift+0..9 注册为
    // 系统级热键，由 OS 在 WM_KEYDOWN 派发之前直接消费，规避 QQNT 类 Chromium 宿主的
    // 加速键双处理。无候选时立即 UnregisterHotKey 让宿主使用这些热键。
    HWND  _hHotkeyWnd;                // 隐藏消息窗口，接收 WM_HOTKEY
    ATOM  _hotkeyWndClass;            // RegisterClassEx 返回的窗口类原子
    BOOL  _hotkeysActive;             // 当前是否已 RegisterHotKey 候选热键（Ctrl+0..9 / Ctrl+Shift+0..9）
    // 加词热键（Ctrl+= 等）全局拦截：门卫比候选热键更严——中文模式 + 焦点在可编辑文本框 +
    // 非密码框 + 持有 thread focus 才注册，让抢占面积最小化，不干扰非文本框处的宿主快捷键。
    BOOL  _addWordHotkeysActive;      // 当前是否已 RegisterHotKey 加词热键
    bool  _focusIsPassword;           // 当前焦点是否密码框（KEYBOARD_DISABLED）；密码框不注册加词热键
    // 当前焦点的 InputScope 掩码（与 focus_gained / CMD_INPUT_STATE_REPORT 上报的同值）。
    // 上报给 core 之外自己也留一份：IsPasswordSuppressActive 的吃键门控须本地可算。
    UINT64 _focusInputScopeMask;
    BOOL  _passwordSuppressEnabled;   // 抑制策略开关（core 经 CONFIG_KEY_PASSWORD_SUPPRESS 推；默认开）
    // 已注册的加词热键 (RegisterHotKey id, raw hash)。raw hash 高16位=KEYMOD、低16位=VK，
    // 供 UnregisterHotKey 与 WM_HOTKEY 分发反解。最多两项（add_word / open_add_word_dialog）。
    std::vector<std::pair<int, uint32_t>> _addWordHotkeyIds;
    // 线程焦点门控：RegisterHotKey 在每个进程内对同一组合键独占。
    // 多进程 IME 实例同时尝试注册会导致 ERROR_HOTKEY_ALREADY_REGISTERED (1409)，
    // 让前台应用拿不到 WM_HOTKEY，反而让残留的后台进程吃掉。
    // 必须把所有 RegisterHotKey 与 thread focus 绑定：只有获得 thread focus 的
    // IME 实例才能注册，失去时立即全部卸载。
    BOOL  _hasThreadFocus;

    BOOL _InitHotkeyWindow();         // 创建窗口类 + 隐藏窗口
    void _UninitHotkeyWindow();       // 反向清理
    void _RegisterCandidateHotkeys(); // 注册 Ctrl+0..9 + Ctrl+Shift+0..9（候选可见时）
    void _UnregisterCandidateHotkeys();
    // 加词热键：Reevaluate 可从任意线程调用（内部 PostMessage 到 _hHotkeyWnd 保证在
    // 拥有该窗口的线程执行 RegisterHotKey）；_DoReevaluate/_Register/_Unregister 仅主线程。
    void _ReevaluateAddWordHotkey();   // 线程安全入口：post 消息触发重新评估
    void _DoReevaluateAddWordHotkey(); // 主线程：按门卫条件注册/注销
    void _RegisterAddWordHotkeys();
    void _UnregisterAddWordHotkeys();
    // 中英模式集中 setter：赋值 _bChineseMode 并触发加词热键重评（模式变化是门卫条件之一）。
    // 可从 async reader 线程调用（reeval 内部 post 到窗口线程）。
    void _SetChineseMode(BOOL v);
    static LRESULT CALLBACK _HotkeyWndProc(HWND hWnd, UINT msg, WPARAM wParam, LPARAM lParam);
    DWORD _activateFlags;  // ActivateEx flags (TF_TMAE_SECUREMODE, etc.)

    // Components
    CKeyEventSink* _pKeyEventSink;
    CIPCClient* _pIPCClient;
    CLangBarItemButton* _pLangBarItemButton;
    CHotkeyManager* _pHotkeyManager;
    // One host band window per kind (candidate / tooltip / status). Indexed by
    // HostWindowKind. _pHostWindow[HOST_WINDOW_CANDIDATE] is the candidate window
    // (also the z-order owner of the tooltip/status windows).
    CHostWindow* _pHostWindow[HOST_WINDOW_KIND_COUNT];

    // Input mode state
    BOOL _bChineseMode;
    BOOL _bFullWidth;
    BOOL _bKeyboardDisabled;   // GUID_COMPARTMENT_KEYBOARD_DISABLED
    ULONGLONG _focusSessionId;
    BOOL _hasFocus;             // 当前实例持有 TSF 输入焦点时为 TRUE（OnSetFocus 最后收到非 null pDocMgrFocus）
    BOOL _hasTextInputContext;  // TRUE when focused doc mgr has a real text-editing context (GetTextExt succeeds)

    // 焦点抖动免疫（见 TextService.cpp OnSetFocus 的判据注释）：缓存上一个真正活跃的
    // DocMgr，用于区分「同一文档抖回来」与「换了文档」。持 AddRef 保活是必须的——
    // 裸指针在旧对象释放后可能被新对象复用同一地址，导致「换了文档」被误判成抖动。
    ITfDocumentMgr* _pLastActiveDocMgr;
    // focus_lost 已发出且尚未被 focus_gained 复位。SendFocusLost 不幂等（服务端据此推进
    // 状态机），而清理可能从三条路径进入（换文档 / OnKillThreadFocus / 无可编辑上下文），
    // 故需去重。⚠ CTX_LOST **不**置本标志：它不是真失焦，置了会让随后真正的
    // thread_focus_lost 被吞掉，服务端的 ime_active 就永远清不掉（见 _ReportEditContextLost）。
    BOOL _focusLostSent;

    // 已向服务端上报「当前焦点在可编辑控件里」（focus_gained 送达时置位）。
    // 供 _ReportEditContextLost 在翻转沿去重——DocMgr 级失焦实测可达 60~98 次/秒，
    // 不去重会造成 IPC 洪泛。
    BOOL _editCtxReported;

    // Composition
    ITfComposition* _pComposition;
    // Top-code committed text kept at the head of the composition, not yet
    // committed to the document (Microsoft IME defers the real commit to the
    // final confirmation — verified via Chrome IME event probe: MS Wubi sends
    // compositionupdate '可能y' on top-code, compositionend only at the end).
    std::wstring _pendingCommitPrefix;
    std::wstring _lastCompositionText;  // Cache to skip redundant updates
    int _lastCaretPos = -1;             // Cache caret position to detect cursor movement
    BOOL _asyncEdit;  // Track if last RequestEditSession returned TF_S_ASYNC (Weasel optimization)

    // Cached caret position from edit session (for WebView apps where separate
    // CaretEditSession with TF_INVALID_COOKIE may be rejected)
    RECT _cachedCaretRect;
    RECT _cachedCompStartRect;
    BOOL _hasCachedCaretPos;
    BOOL _hasCachedCompStartPos;
    // Weasel 模式：StartComposition 后第一次 SendCaretPositionUpdate 不立即发 IPC，
    // 改为等 OnLayoutChange（reflow 完成的权威信号）或 50ms timer 兜底。
    BOOL _compositionJustStarted;
    // 首帧 reflow 期间已发出的试探采样次数（见 OnLayoutChange 与 CMD_CARET_PROBE）。
    // 每次 StartComposition 归零；限次上报，防 burst 长的宿主刷 IPC。
    int  _firstShowProbeSeq = 0;
    BOOL _needsFocusRecovery;
    LONG _lastFocusCaretX;
    LONG _lastFocusCaretY;
    LONG _lastFocusCaretHeight;
    BOOL _hasLastKnownCaretPos;
    LONG _lastKnownCaretX;
    LONG _lastKnownCaretY;
    LONG _lastKnownCaretHeight;

    // Display Attribute
    TfGuidAtom _gaDisplayAttributeInput;

    // ITfTextLayoutSink registration
    DWORD _dwLayoutSinkCookie;
    ITfContext* _pLayoutSinkContext;  // Context we registered the sink on
    void _AdviseTextLayoutSink(ITfContext* pContext);
    void _UnadviseTextLayoutSink();

    // Returns TRUE if pDocMgr has a non-null, writable, non-transitory top context.
    // Used to set _hasTextInputContext in OnSetFocus and RefreshTextInputContext.
    // Optional pDynFlagsOut receives dwDynamicFlags from TF_STATUS (0 if unavailable).
    BOOL _DocMgrHasEditableContext(ITfDocumentMgr* pDocMgr, DWORD* pDynFlagsOut = nullptr);

    // 读取焦点文档的 TSF InputScope 集合并编码为 bitmask（bit N = 枚举值 N 存在）。
    // 失败或无 InputScope 时返回 0。随 focus_gained 上报给 Go 端做密码框等决策。
    UINT64 _QueryInputScopeMask(ITfDocumentMgr* pDocMgr);

    // 判断焦点 context 是否被宿主置 GUID_COMPARTMENT_KEYBOARD_DISABLED（禁用输入法）。
    // Weasel/小狼毫用此判定密码框：Chromium 密码框置位、无痕普通框不置位，精确区分。
    bool _IsFocusKeyboardDisabled(ITfDocumentMgr* pDocMgr);

    // ITfTextEditSink registration
    DWORD _dwTextEditSinkCookie;
    ITfContext* _pTextEditSinkContext;  // Context we registered the sink on
    void _AdviseTextEditSink(ITfContext* pContext);
    void _UnadviseTextEditSink();

    // Cached character before caret (updated by OnEndEdit, consumed by KeyEventSink)
    WCHAR _cachedPrevChar;

    // Compartment event sink (GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)
    DWORD _dwOpenCloseSinkCookie;
    BOOL _bInCompartmentChange;  // Guard against re-entrant OnChange
    ULONGLONG _lastCapsKeyTick;  // 最近一次 CapsLock 按键活动（GetTickCount64），见 NoteCapsLockKeyActivity

    // 最近一次 ActivateEx 的时刻。激活后系统会写 compartment 做初始化同步，那不是用户
    // 操作——实测 ActivateEx 后 ~96ms 就有一次 CONVERSION 变化。焦点守卫改用
    // _hasThreadFocus 之后这类噪声不再被顺带挡住（激活时本应用正是前台），故需本时间戳。
    // 手法同 295350e 用 _lastCapsKeyTick 抑制 CapsLock 联动噪声。
    ULONGLONG _lastActivateTick;

    BOOL _InitOpenCloseCompartment();
    void _UninitOpenCloseCompartment();
    BOOL _SetOpenCloseCompartment(BOOL bOpen);

    // Compartment event sink (GUID_COMPARTMENT_KEYBOARD_DISABLED)
    DWORD _dwKeyboardDisabledSinkCookie;

    BOOL _InitKeyboardDisabledCompartment();
    void _UninitKeyboardDisabledCompartment();

    // Compartment event sink (GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)
    // 用 IME_CMODE_NATIVE 位向外界（KBLSwitch / 任务栏 / 第三方）表达中/英文状态。
    // OPENCLOSE 始终 TRUE 是内部约定（保证英文模式仍触发 OnTestKeyDown），
    // 真实的中英文模式由本 compartment 暴露。
    DWORD _dwConversionSinkCookie;
    BOOL _bInConversionChange;  // Guard against re-entrant OnChange for conversion compartment

    // HoldComposition 计时器状态（智能符号 HoldComposition 方案）
    UINT_PTR       _hHoldTimer = 0;           // SetTimer 返回的计时器 ID；0 表示无活跃计时器
    std::wstring   _heldCompositionText;      // press1 进入组合态的中文文本
    // 提交 held 中文符号收口。fromTimerCallback=TRUE 仅用于真正的 WM_TIMER 回调
    // （HoldTimerProc）——此上下文拿不到同步编辑会话，须走异步收口；Flush 路径
    // （PassThrough 透传、失焦 EndComposition）在按键同步上下文里调用，保持同步以
    // 确保与后续透传字符的先后顺序正确。
    void           OnHoldTimerExpired(BOOL fromTimerCallback = FALSE);
    static VOID CALLBACK HoldTimerProc(HWND hwnd, UINT uMsg, UINT_PTR idEvent, DWORD dwTime);

    // direct_commit 顶码：真提交后，余码新组合延迟到触发键 keyup（或兜底定时器）才开。
    // 与 HoldComposition 计时器状态并列、互不干扰。见 top-commit-mode 设计文档 §5。
    std::wstring   _deferredCompText;        // 待重开的余码组合；空=无待重开
    UINT_PTR       _hDeferredTimer = 0;      // keyup 兜底定时器 id；0=无
    static VOID CALLBACK DeferredTimerProc(HWND, UINT, UINT_PTR idEvent, DWORD);

    BOOL _InitConversionCompartment();
    void _UninitConversionCompartment();
    BOOL _SetConversionMode(BOOL bChinese);

    BOOL _InitThreadMgrEventSink();
    void _UninitThreadMgrEventSink();

    BOOL _InitKeyEventSink();
    void _UninitKeyEventSink();

    BOOL _InitIPCClient();
    void _UninitIPCClient();

    BOOL _InitLangBarButton();
    void _UninitLangBarButton();

    BOOL _InitDisplayAttribute();
    void _UninitDisplayAttribute();

    // State sync helper (internal): apply status response to local state
    void _SyncStateFromResponse(const ServiceResponse& response);
    void _EnsureHostRenderSetup(const ServiceResponse& response, BOOL forceRefresh);
    // 销毁宿主代理渲染窗口（释放共享内存映射 + 渲染线程 + Band 窗口）。
    // 仅在 Deactivate（IME 卸载）和 _EnsureHostRenderSetup（强制刷新/host render
    // 不可用）时调用。**不要**在失焦时调用：locked/transient DocMgr（SearchHost/任务
    // 管理器）会跳过 focus_gained，销毁后无法重建 → 候选永久不显示。失焦只需靠 Go 的
    // WriteHide 经本进程 event 隐藏窗口。空操作安全。
    void _DestroyHostWindow();

public:
    // Perform full state sync with Go service (sends IMEActivated + processes response).
    // Called after new/re-connection to ensure TSF and service state are consistent.
    void _DoFullStateSync();
    void TryRecoverFocusState();

    // ApplyActivationStatusResponse 应用一份从 push pipe 接收到的 activation status,
    // 等价于原同步路径 (_DoFullStateSync / TryRecoverFocusState) 收到 ReceiveResponse 后
    // 调 _SyncStateFromResponse + _EnsureHostRenderSetup 的组合动作。
    // 由 CLangBarItemButton::_MsgWndProc 在 WM_ACTIVATION_STATUS 上调用, 保证在 TSF 线程。
    void ApplyActivationStatusResponse(const ServiceResponse& response);

    // Get display attribute GUID atom for composition
    TfGuidAtom GetDisplayAttributeInputAtom() { return _gaDisplayAttributeInput; }
};
