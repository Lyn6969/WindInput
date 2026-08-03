#pragma once

#include <msctf.h>
#include <ctfutb.h>
#include <string>

class CTextService;
struct ServiceResponse;

// Menu item IDs for language bar right-click menu
#define MENU_ID_TOGGLE_MODE      1
#define MENU_ID_TOGGLE_WIDTH     2
#define MENU_ID_TOGGLE_PUNCT     3
#define MENU_ID_TOGGLE_TOOLBAR   4
#define MENU_ID_OPEN_SETTINGS    5
#define MENU_ID_DICTIONARY       6
#define MENU_ID_ABOUT            7
#define MENU_ID_EXIT             8

// Language bar button for showing Chinese/English mode
class CLangBarItemButton : public ITfLangBarItemButton,
                           public ITfSource
{
public:
    CLangBarItemButton(CTextService* pTextService);
    ~CLangBarItemButton();

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj);
    STDMETHODIMP_(ULONG) AddRef();
    STDMETHODIMP_(ULONG) Release();

    // ITfLangBarItem
    STDMETHODIMP GetInfo(TF_LANGBARITEMINFO* pInfo);
    STDMETHODIMP GetStatus(DWORD* pdwStatus);
    STDMETHODIMP Show(BOOL fShow);
    STDMETHODIMP GetTooltipString(BSTR* pbstrToolTip);

    // ITfLangBarItemButton
    STDMETHODIMP OnClick(TfLBIClick click, POINT pt, const RECT* prcArea);
    STDMETHODIMP InitMenu(ITfMenu* pMenu);
    STDMETHODIMP OnMenuSelect(UINT wID);
    STDMETHODIMP GetIcon(HICON* phIcon);
    STDMETHODIMP GetText(BSTR* pbstrText);

    // ITfSource
    STDMETHODIMP AdviseSink(REFIID riid, IUnknown* punk, DWORD* pdwCookie);
    STDMETHODIMP UnadviseSink(DWORD dwCookie);

    // Initialization
    BOOL Initialize();
    void Uninitialize();

    // Update the button when mode changes
    void UpdateLangBarButton(BOOL bChineseMode);

    // Update the button when Caps Lock state changes
    void UpdateCapsLockState(BOOL bCapsLock);

    // Update the button when keyboard disabled state changes
    void UpdateKeyboardDisabled(BOOL bDisabled);

    // 输入可用性（与中英模式**正交**）：语言栏图标据此显示禁用态/强制英文。
    // 两者都在 TSF 侧本地判定、不经 IPC——判据源就在 DLL 内（_hasTextInputContext
    // 与 IsPasswordSuppressActive），绕一圈 IPC 反而引入陈旧窗口。
    // 调用方须先做迟滞：这两个量随 DocMgr 抖动高频翻转，详见 _ScheduleLangBarStateSync。
    void UpdateInputAvailability(BOOL bNoEditContext, BOOL bPasswordField);

    // Update both mode and Caps Lock state
    void UpdateState(BOOL bChineseMode, BOOL bCapsLock);

    // Update full status (called when receiving status_update from Go service)
    // iconLabel: display text from Go service (e.g., "中", "英", "A", "拼", "五")
    void UpdateFullStatus(BOOL bChineseMode, BOOL bFullWidth, BOOL bChinesePunct, BOOL bToolbarVisible, BOOL bCapsLock, const wchar_t* iconLabel = nullptr);

    // Thread-safe update from async thread (posts message to UI thread)
    void PostUpdateFullStatus(BOOL bChineseMode, BOOL bFullWidth, BOOL bChinesePunct, BOOL bToolbarVisible, BOOL bCapsLock, const wchar_t* iconLabel = nullptr);

    // Thread-safe commit text from async thread (posts message to UI thread)
    // This ensures EndComposition is called before InsertText on the correct thread
    void PostCommitText(const std::wstring& text);

    // Thread-safe replace-backward from async thread (undo commit push):
    // delete `count` chars before caret then insert text on the UI thread
    void PostReplaceBackward(int count, const std::wstring& text);

    // Thread-safe clear composition from async thread (posts message to UI thread)
    // Used when mode is toggled via menu while there's an active composition
    void PostClearComposition();

    // Thread-safe update composition from async thread (posts message to UI thread)
    // Used for mouse click partial confirm in pinyin mode
    void PostUpdateComposition(const std::wstring& text, int caretPos);

    // Thread-safe service-ready notification from async reader thread.
    // Triggers _DoFullStateSync() on the TSF thread so the toolbar appears
    // after service restart without waiting for a focus/key event.
    void PostServiceReady();

    // Thread-safe activation status from async reader thread.
    // 触发时机：Go 收到异步化后的 CmdIMEActivated / CmdFocusGained 完成 handler 后通过
    // push pipe 推送的 CMD_ACTIVATION_STATUS_PUSH。TSF 线程上调用 TextService 的
    // ApplyActivationStatusResponse, 等价于原同步 ReceiveResponse 路径的
    // _SyncStateFromResponse + _EnsureHostRenderSetup。
    void PostActivationStatus(const ServiceResponse& response);

    // Schedule a 50ms fallback caret retry on the TSF thread.
    // Used as a safety net when an app does not fire OnLayoutChange promptly.
    void PostDelayedCaretPositionUpdate();

    // Cancel a pending delayed caret retry (called when OnLayoutChange fires).
    void CancelDelayedCaretPositionUpdate();

    // Force refresh the language bar icon (used when focus is gained)
    void ForceRefresh();

    // Set the input method type label displayed in Chinese mode
    // label: "中"(default), "拼"(Pinyin), "五"(Wubi), "双"(Shuangpin), etc.
    void SetInputTypeLabel(const wchar_t* label);

private:
    // Message window for cross-thread updates
    HWND _hMsgWnd;
    static LRESULT CALLBACK _MsgWndProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam);
    static const UINT WM_UPDATE_STATUS;
    static const UINT WM_COMMIT_TEXT;
    static const UINT WM_CLEAR_COMPOSITION;
    static const UINT WM_UPDATE_COMPOSITION;
    static const UINT WM_SERVICE_READY;
    static const UINT WM_ACTIVATION_STATUS;
    static const UINT WM_REPLACE_BACKWARD;

    // Packed status for message passing
    struct StatusUpdateData {
        BOOL bChineseMode;
        BOOL bFullWidth;
        BOOL bChinesePunct;
        BOOL bToolbarVisible;
        BOOL bCapsLock;
        wchar_t iconLabel[8];  // Icon label from Go service (e.g., "中", "英", "拼")
    };

    // Data for commit text message
    struct CommitTextData {
        std::wstring text;
    };

    // Data for replace-backward message (undo commit)
    struct ReplaceBackwardData {
        int count;
        std::wstring text;
    };

    // Data for update composition message
    struct UpdateCompositionData {
        std::wstring text;
        int caretPos;
    };

    // Show popup menu manually (Windows 11 workaround)
    void _ShowPopupMenu(POINT pt);

    LONG _refCount;
    CTextService* _pTextService;
    ITfLangBarItemSink* _pLangBarItemSink;
    DWORD _dwCookie;
    BOOL _bChineseMode;
    BOOL _bCapsLock;           // Caps Lock state
    BOOL _bFullWidth;          // Full-width mode (全角)
    BOOL _bChinesePunct;       // Chinese punctuation mode (中文标点)
    BOOL _bToolbarVisible;     // Toolbar visibility
    BOOL _bKeyboardDisabled;   // Keyboard disabled by system (线程级 compartment)
    // 焦点不在可编辑控件里（CAD 绘图区、浏览器非输入区、QQ 密码框那种 READONLY DocMgr）。
    // 与 _bKeyboardDisabled 的成因不同，但对用户是同一件事：一个键也打不进去。
    BOOL _bNoEditContext;
    // 密码框：已被强制英文（判据 = IsPasswordSuppressActive，含策略开关与线程级早退）。
    // 实测 Chrome/Edge 网页登录页会置 context 级 KEYBOARD_DISABLED 从而命中；
    // QQ 密码框**不**走这条路（它让焦点落到 READONLY DocMgr，命中的是 _bNoEditContext）。
    BOOL _bPasswordField;
    BOOL _bDarkMode;           // System dark mode state (cached, updated on status change)

    // Input method type label for Chinese mode display
    // Default: "中", future values: "拼"(Pinyin), "五"(Wubi), "双"(Shuangpin)
    wchar_t _inputTypeLabel[4];

    // GUID for this language bar item
    static const GUID _guidLangBarItemButton;
};
