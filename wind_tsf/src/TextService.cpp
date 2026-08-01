#include "TextService.h"
#include "KeyEventSink.h"
#include "IPCClient.h"
#include "LangBarItemButton.h"
#include "CaretEditSession.h"
#include "DisplayAttributeInfo.h"
#include "HotkeyManager.h"
#include "HostWindow.h"
#include <vector>
#include <shellscalingapi.h>
#include <inputscope.h> // ITfInputScope / InputScope 枚举

// GUID_PROP_INPUTSCOPE 在 SDK 头中仅为 EXTERN_C 声明，其字节定义需某个 TU 启用 INITGUID
// 才会生成；直接引用会产生 LNK2019。这里本地定义该 GUID 值（与 inputscope.h 一致），
// 避免引入 <initguid.h> 把本文件所有 GUID 实体化而与已链接定义冲突。
static const GUID kGuidPropInputScope =
    { 0x1713dd5a, 0x68e7, 0x4a5b, { 0x9a, 0xf6, 0x59, 0x2a, 0x59, 0x5c, 0x77, 0x8d } };

// InputScope bit 常量（与 Go 端、inputscope.h 一致）
static const UINT64 kScopeBitPassword = 1ULL << 31; // IS_PASSWORD

// 输入诊断 HUD（Task 7）：由 disabled + InputScope mask 计算上报 reason，语义与 Rust
// coordinator 侧 reason_from 完全一致。reason: 0 None / 1 CompartmentDisabled /
// 2 InputScopePassword / 3 NumericPassword。disabled（compartment 命中）优先级最高。
static inline uint8_t ComputeInputReason(bool disabled, UINT64 mask)
{
    if (disabled) return 1;
    if (mask & (1ULL << 63)) return 3; // IS_NUMERIC_PASSWORD
    if (mask & (1ULL << 31)) return 2; // IS_PASSWORD
    return 0;
}

// TSF 标准 compartment GUID（本地定义，避免链接 TSF GUID 静态库产生 LNK2019）。
// 宿主（含 Chromium 系浏览器密码框）会在 context 上置 KEYBOARD_DISABLED 表示"禁用输入法"；
// 这是比 InputScope 更可靠的密码框信号（小狼毫/Weasel 即用此判定），且无痕普通框不会置位。
static const GUID kGuidCompartmentKeyboardDisabled =
    { 0x71a5b253, 0x1951, 0x466b, { 0x9f, 0xbc, 0x9c, 0x88, 0x08, 0xfa, 0x84, 0xf2 } };

// 判断屏幕坐标点是否落在前台窗口矩形之外（原点错误的越界坐标）。个别宿主（某些 OpenGL /
// 异常渲染应用）的 GetTextExt 会返回越界屏幕坐标，直接用会把候选框甩到屏幕角落。检出后
// 调用方应丢弃该坐标、回退到窗口相对的方法。参考 Weasel enhanced_position；仅在明显越界时
// 返回 true，光标在窗口内的正常情形不受影响。
static bool IsScreenPointOutsideForegroundWindow(LONG x, LONG y)
{
    HWND hwndFg = GetForegroundWindow();
    if (hwndFg == nullptr)
        return false;
    RECT rcWin = {};
    if (!GetWindowRect(hwndFg, &rcWin))
        return false;
    return (x < rcWin.left || x > rcWin.right || y < rcWin.top || y > rcWin.bottom);
}

// EditSession for reading the focused context's TSF InputScope set.
// 用于识别密码框等语义控件：GetInputScopes 返回的枚举值按位编码为 bitmask
// （bit N 表示枚举值 N 存在，如 IS_PASSWORD=31 → bit 31），交由 Go 端决策。
// 需要读锁：InputScope 属性值通过 ITfReadOnlyProperty::GetValue 读取，必须在
// 编辑会话的 read cookie 下进行。同步读锁在 OnSetFocus 期间通常可获得（与
// CCaretEditSession 一致）；获取失败时返回 0（视为默认/未知，行为同既有逻辑）。
class CQueryInputScopeEditSession : public ITfEditSession
{
public:
    CQueryInputScopeEditSession(ITfContext* pContext, UINT64* pMaskOut)
        : _refCount(1), _pContext(pContext), _pMaskOut(pMaskOut)
    {
        if (_pContext)
            _pContext->AddRef();
        if (_pMaskOut)
            *_pMaskOut = 0;
    }

    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj) override
    {
        if (ppvObj == nullptr)
            return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
            *ppvObj = static_cast<ITfEditSession*>(this);
        if (*ppvObj)
        {
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() override { return InterlockedIncrement(&_refCount); }
    STDMETHODIMP_(ULONG) Release() override
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0)
            delete this;
        return cr;
    }

    STDMETHODIMP DoEditSession(TfEditCookie ec) override
    {
        if (_pContext == nullptr || _pMaskOut == nullptr)
            return E_FAIL;

        ITfReadOnlyProperty* pProp = nullptr;
        if (FAILED(_pContext->GetAppProperty(kGuidPropInputScope, &pProp)) || pProp == nullptr)
            return S_OK;

        // 在两处 range 读取并合并 InputScope：
        //  - selection（光标/选区所在内容 range）：宿主常把 IS_PASSWORD 等设在内容 range 上
        //  - 文档起点空 range：兜底，反映文档级提示（如 IS_PRIVATE）
        // 某些宿主（Chromium 系浏览器）两者不一致，故都读，避免漏掉 IS_PASSWORD。
        ITfRange* ranges[2] = { nullptr, nullptr };
        TF_SELECTION sel = {};
        ULONG fetched = 0;
        if (SUCCEEDED(_pContext->GetSelection(ec, TF_DEFAULT_SELECTION, 1, &sel, &fetched)) && fetched > 0)
            ranges[0] = sel.range; // 持有引用
        ITfRange* pStart = nullptr;
        if (SUCCEEDED(_pContext->GetStart(ec, &pStart)))
            ranges[1] = pStart;

        for (int r = 0; r < 2; ++r)
        {
            if (ranges[r] == nullptr)
                continue;
            VARIANT var;
            VariantInit(&var);
            if (SUCCEEDED(pProp->GetValue(ec, ranges[r], &var)) && var.vt == VT_UNKNOWN && var.punkVal != nullptr)
            {
                ITfInputScope* pInputScope = nullptr;
                if (SUCCEEDED(var.punkVal->QueryInterface(IID_ITfInputScope, reinterpret_cast<void**>(&pInputScope))) && pInputScope != nullptr)
                {
                    InputScope* pScopes = nullptr;
                    UINT count = 0;
                    if (SUCCEEDED(pInputScope->GetInputScopes(&pScopes, &count)) && pScopes != nullptr)
                    {
                        for (UINT i = 0; i < count; ++i)
                        {
                            int v = static_cast<int>(pScopes[i]);
                            if (v >= 0 && v < 64)
                                *_pMaskOut |= (1ULL << v);
                        }
                        CoTaskMemFree(pScopes);
                    }
                    pInputScope->Release();
                }
            }
            VariantClear(&var);
        }

        for (int r = 0; r < 2; ++r)
            if (ranges[r] != nullptr)
                ranges[r]->Release();
        pProp->Release();
        return S_OK;
    }

private:
    ~CQueryInputScopeEditSession() { SafeRelease(_pContext); }

    LONG _refCount;
    ITfContext* _pContext;
    UINT64* _pMaskOut;
};

// EditSession for ending composition
// NOTE: This class takes ownership of the composition pointer passed to it.
// The composition will be ended and released when DoEditSession is called,
// or in the destructor if the edit session request fails.
class CEndCompositionEditSession : public ITfEditSession
{
public:
    // pComposition ownership is transferred to this object
    CEndCompositionEditSession(CTextService* pTextService, ITfComposition* pComposition)
        : _refCount(1), _pTextService(pTextService), _pComposition(pComposition)
    {
        _pTextService->AddRef();
        // Note: pComposition ownership is transferred, no AddRef needed
    }

    ~CEndCompositionEditSession()
    {
        _pTextService->Release();
        // If DoEditSession was never called (request failed), release the composition
        if (_pComposition != nullptr)
        {
            WIND_LOG_DEBUG(L"~CEndCompositionEditSession: Releasing orphaned composition\n");
            _pComposition->Release();
            _pComposition = nullptr;
        }
    }

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj)
    {
        if (ppvObj == nullptr) return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
        {
            *ppvObj = (ITfEditSession*)this;
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef()
    {
        return InterlockedIncrement(&_refCount);
    }

    STDMETHODIMP_(ULONG) Release()
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0) delete this;
        return cr;
    }

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec)
    {
        if (_pComposition != nullptr)
        {
            // Get the composition range and clear the text before ending
            // This prevents the composition text from being committed
            ITfRange* pRange = nullptr;
            if (SUCCEEDED(_pComposition->GetRange(&pRange)))
            {
                // Clear the composition text (set to empty string)
                pRange->SetText(ec, 0, L"", 0);
                pRange->Release();
            }

            _pComposition->EndComposition(ec);

            // Release the composition
            _pComposition->Release();
            _pComposition = nullptr;
            WIND_LOG_DEBUG(L"DoEditSession: Composition ended and released\n");
        }
        return S_OK;
    }

private:
    LONG _refCount;
    CTextService* _pTextService;
    ITfComposition* _pComposition;  // Owned composition pointer
};

// EditSession for committing text atomically (end composition + insert text in one session)
// This prevents race conditions where async EndComposition clears text inserted by a subsequent InsertText.
class CCommitTextEditSession : public ITfEditSession
{
public:
    // pComposition ownership is transferred to this object (may be nullptr if no active composition)
    CCommitTextEditSession(CTextService* pTextService, ITfContext* pContext,
                           ITfComposition* pComposition, const std::wstring& text)
        : _refCount(1), _pTextService(pTextService), _pContext(pContext),
          _pComposition(pComposition), _text(text), _success(FALSE)
    {
        _pTextService->AddRef();
        _pContext->AddRef();
    }

    ~CCommitTextEditSession()
    {
        _pTextService->Release();
        _pContext->Release();
        if (_pComposition != nullptr)
        {
            WIND_LOG_DEBUG(L"~CCommitTextEditSession: Releasing orphaned composition\n");
            _pComposition->Release();
            _pComposition = nullptr;
        }
    }

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj)
    {
        if (ppvObj == nullptr) return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
        {
            *ppvObj = (ITfEditSession*)this;
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() { return InterlockedIncrement(&_refCount); }
    STDMETHODIMP_(ULONG) Release()
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0) delete this;
        return cr;
    }

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec)
    {
        // 标准 IME 提交语义: 把最终上屏文字 SetText 到 composition range 后再
        // EndComposition, 让宿主应用通过 OnEndComposition 看到的 range 内容就是
        // 上屏文字, 与微软拼音/搜狗/Rime 一致。
        //
        // 历史实现 (SetText("") + EndComposition + InsertTextAtSelection) 等价于
        // "IME 取消了 composition + 旁路插入了一段普通文本", 跟打/统计类应用会
        // 把这次上屏误判为"非 IME 输入", 影响正确率统计。原子性由单一 EditSession
        // 保证 (历史 02a753f 修的浏览器异步竞态), 与此次模式调整正交。
        if (_pComposition != nullptr)
        {
            BOOL committedViaComposition = FALSE;
            ITfRange* pRange = nullptr;
            if (SUCCEEDED(_pComposition->GetRange(&pRange)))
            {
                // 把 composition range 内容替换为最终文字; _text 为空则等价于清空。
                pRange->SetText(ec, TF_ST_CORRECTION, _text.c_str(), (LONG)_text.length());
                // 光标定位到插入文本之后, 作为后续输入起点。
                pRange->Collapse(ec, TF_ANCHOR_END);
                TF_SELECTION sel = {};
                sel.range = pRange;
                sel.style.ase = TF_AE_NONE;
                sel.style.fInterimChar = FALSE;
                _pContext->SetSelection(ec, 1, &sel);
                pRange->Release();
                committedViaComposition = TRUE;
            }
            _pComposition->EndComposition(ec);
            _pComposition->Release();
            _pComposition = nullptr;
            if (committedViaComposition)
            {
                _success = TRUE;
                WIND_LOG_DEBUG(L"CCommitTextEditSession: SetText + EndComposition committed\n");
                return S_OK;
            }
            // GetRange 失败 (极少): composition 已结束但文字未写入, fallthrough
            // 到 InsertTextAtSelection 兜底, 避免静默丢字。
            WIND_LOG_DEBUG(L"CCommitTextEditSession: GetRange failed, falling back to InsertTextAtSelection\n");
        }

        // 无 active composition 的回退路径 (鼠标上屏 / 上述 GetRange 失败兜底): 走 InsertTextAtSelection。
        if (!_text.empty())
        {
            ITfInsertAtSelection* pInsertAtSel = nullptr;
            HRESULT hr = _pContext->QueryInterface(IID_ITfInsertAtSelection, (void**)&pInsertAtSel);
            if (FAILED(hr) || pInsertAtSel == nullptr)
            {
                WIND_LOG_DEBUG(L"CCommitTextEditSession: Failed to get ITfInsertAtSelection\n");
                return E_FAIL;
            }

            ITfRange* pRange = nullptr;
            hr = pInsertAtSel->InsertTextAtSelection(ec, 0, _text.c_str(), (LONG)_text.length(), &pRange);
            pInsertAtSel->Release();

            if (FAILED(hr))
            {
                WIND_LOG_DEBUG_FMT(L"CCommitTextEditSession: InsertTextAtSelection failed hr=0x%08X\n", hr);
                return hr;
            }

            if (pRange != nullptr)
            {
                pRange->Collapse(ec, TF_ANCHOR_END);
                TF_SELECTION sel = {};
                sel.range = pRange;
                sel.style.ase = TF_AE_NONE;
                sel.style.fInterimChar = FALSE;
                _pContext->SetSelection(ec, 1, &sel);
                pRange->Release();
            }
        }

        _success = TRUE;
        WIND_LOG_DEBUG(L"CCommitTextEditSession: Text committed via InsertTextAtSelection fallback\n");
        return S_OK;
    }

    BOOL GetSuccess() const { return _success; }

private:
    LONG _refCount;
    CTextService* _pTextService;
    ITfContext* _pContext;
    ITfComposition* _pComposition;  // Owned composition pointer
    std::wstring _text;
    BOOL _success;
};

// EditSession：把光标前 count 个字符替换为 text（智能符号纠错替换）。
// 在单一同步 EditSession 内完成：取选区光标 → 起点回退 count 字符 → SetText 覆盖。
// 同步、原子、不受输入队列时序与修饰键状态影响——优于"合成退格 + 提交"组合。
class CReplaceBackwardEditSession : public ITfEditSession
{
public:
    CReplaceBackwardEditSession(CTextService* pTextService, ITfContext* pContext,
                                int count, const std::wstring& text)
        : _refCount(1), _pTextService(pTextService), _pContext(pContext),
          _count(count), _text(text), _success(FALSE)
    {
        _pTextService->AddRef();
        _pContext->AddRef();
    }

    ~CReplaceBackwardEditSession()
    {
        _pTextService->Release();
        _pContext->Release();
    }

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj)
    {
        if (ppvObj == nullptr) return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
        {
            *ppvObj = (ITfEditSession*)this;
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() { return InterlockedIncrement(&_refCount); }
    STDMETHODIMP_(ULONG) Release()
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0) delete this;
        return cr;
    }

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec)
    {
        TF_SELECTION sel = {};
        ULONG fetched = 0;
        if (FAILED(_pContext->GetSelection(ec, TF_DEFAULT_SELECTION, 1, &sel, &fetched)) ||
            fetched == 0 || sel.range == nullptr)
        {
            WIND_LOG_DEBUG(L"CReplaceBackwardEditSession: GetSelection failed\n");
            return E_FAIL;
        }

        ITfRange* pRange = sel.range; // 取得所有权，末尾 Release

        // 先折叠到光标（选区末端），确保 range 锚定在光标处，再向前覆盖 count 个字符，
        // 即便此刻存在非空选区也只替换"光标前 count 字符"。
        pRange->Collapse(ec, TF_ANCHOR_END);

        LONG shifted = 0;
        HRESULT hr = pRange->ShiftStart(ec, -_count, &shifted, nullptr);
        if (FAILED(hr) || shifted != -_count)
        {
            // 无法回退足够字符（行首 / 不可编辑等）：放弃，交由调用方走 SendInput 兜底。
            WIND_LOG_DEBUG_FMT(L"CReplaceBackwardEditSession: ShiftStart failed hr=0x%08X shifted=%ld\n", hr, shifted);
            pRange->Release();
            return E_FAIL;
        }

        hr = pRange->SetText(ec, TF_ST_CORRECTION, _text.c_str(), (LONG)_text.length());
        if (FAILED(hr))
        {
            WIND_LOG_DEBUG_FMT(L"CReplaceBackwardEditSession: SetText failed hr=0x%08X\n", hr);
            pRange->Release();
            return hr;
        }

        // 诊断用：SetText 报成功后，回读同一 range 的实际内容核对是否真的等于 _text。
        // 部分宿主（Chromium/Qt 内嵌 TSFTextStore）对 SetText 报 S_OK 但实际渲染结果
        // 与 TSF 内部模型不一致（自身的编辑事务/diff 机制未正确落地），此时回读仍会
        // "看起来正确"——用于区分是我们这边 range 算错了，还是宿主渲染层的问题。
        {
            WCHAR readback[64] = {};
            ULONG readbackLen = 0;
            HRESULT hrRead = pRange->GetText(ec, 0, readback, 63, &readbackLen);
            std::wstring readbackStr(readback, readbackLen);
            WIND_LOG_DEBUG_FMT(L"CReplaceBackwardEditSession: readback hr=0x%08X text='%s' expected='%s' match=%d\n",
                               hrRead, readbackStr.c_str(), _text.c_str(),
                               (SUCCEEDED(hrRead) && readbackStr == _text) ? 1 : 0);
        }

        // 光标定位到替换文本之后。
        pRange->Collapse(ec, TF_ANCHOR_END);
        TF_SELECTION newSel = {};
        newSel.range = pRange;
        newSel.style.ase = TF_AE_NONE;
        newSel.style.fInterimChar = FALSE;
        _pContext->SetSelection(ec, 1, &newSel);

        pRange->Release();
        _success = TRUE;
        WIND_LOG_DEBUG(L"CReplaceBackwardEditSession: range replace committed\n");
        return S_OK;
    }

    BOOL GetSuccess() const { return _success; }

private:
    LONG _refCount;
    CTextService* _pTextService;
    ITfContext* _pContext;
    int _count;
    std::wstring _text;
    BOOL _success;
};

// EditSession for updating composition
class CUpdateCompositionEditSession : public ITfEditSession
{
public:
    CUpdateCompositionEditSession(CTextService* pTextService, ITfContext* pContext, const std::wstring& text, int caretPos = -1, BOOL noUnderline = FALSE)
        : _refCount(1), _pTextService(pTextService), _pContext(pContext), _text(text), _caretPos(caretPos), _noUnderline(noUnderline)
    {
        _pTextService->AddRef();
        _pContext->AddRef();
    }

    ~CUpdateCompositionEditSession()
    {
        _pTextService->Release();
        _pContext->Release();
    }

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj)
    {
        if (ppvObj == nullptr) return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
        {
            *ppvObj = (ITfEditSession*)this;
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef()
    {
        return InterlockedIncrement(&_refCount);
    }

    STDMETHODIMP_(ULONG) Release()
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0) delete this;
        return cr;
    }

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec)
    {
        HRESULT hr = S_OK;

        // 1. If no composition exists, start one
        if (_pTextService->_pComposition == nullptr)
        {
            // Get current selection (cursor position) to start composition there
            TF_SELECTION tfSelection;
            ULONG cFetched;
            if (FAILED(_pContext->GetSelection(ec, TF_DEFAULT_SELECTION, 1, &tfSelection, &cFetched)) || cFetched != 1)
            {
                return E_FAIL;
            }

            ITfContextComposition* pContextComp = nullptr;
            if (FAILED(_pContext->QueryInterface(IID_ITfContextComposition, (void**)&pContextComp)))
            {
                tfSelection.range->Release();
                return E_FAIL;
            }

            // Start composition
            hr = pContextComp->StartComposition(
                ec,
                tfSelection.range,
                (ITfCompositionSink*)_pTextService,
                &_pTextService->_pComposition);

            pContextComp->Release();
            tfSelection.range->Release();

            if (FAILED(hr) || _pTextService->_pComposition == nullptr)
            {
                WIND_LOG_ERROR(L"StartComposition failed\n");
                return E_FAIL;
            }
            WIND_LOG_DEBUG(L"StartComposition succeeded\n");
            // Weasel 模式：标记 composition 刚刚创建。下一次 SendCaretPositionUpdate
            // 不会立即发 IPC，而是等 OnLayoutChange 提供 reflow 后的权威坐标，
            // 50ms timer 兜底（应对不发 OnLayoutChange 的应用，如某些 CUAS 路径）。
            _pTextService->_compositionJustStarted = TRUE;
            _pTextService->_firstShowProbeSeq = 0; // 新组合开始：试探采样计数归零
        }

        // 2. Get range from composition
        ITfRange* pRange = nullptr;
        if (FAILED(_pTextService->_pComposition->GetRange(&pRange)))
        {
            return E_FAIL;
        }

        // 3. Set text
        // When composition text is empty (non-inline preedit mode), use a space as
        // placeholder so GetTextExt can return a valid caret rect. Without this,
        // apps like WPS return a degenerate rect (height=0) for zero-length ranges.
        // The cursor is positioned before the space (step 5), so visually there's
        // no offset. The placeholder is cleared on EndComposition/CommitText.
        BOOL isPlaceholder = _text.empty();
        static const wchar_t PLACEHOLDER[] = L" ";
        const wchar_t* textPtr = isPlaceholder ? PLACEHOLDER : _text.c_str();
        LONG textLen = isPlaceholder ? 1 : (LONG)_text.length();

        hr = pRange->SetText(ec, TF_ST_CORRECTION, textPtr, textLen);

        if (SUCCEEDED(hr))
        {
            // 4. Apply display attribute to show underline
            // Skip for placeholder text to avoid any visual artifacts
            if (!isPlaceholder)
                _SetDisplayAttribute(ec, pRange);

            // 5. Position cursor within composition
            ITfRange* pRangeForSel = nullptr;
            if (SUCCEEDED(_pTextService->_pComposition->GetRange(&pRangeForSel)))
            {
                if (isPlaceholder && textLen > 0)
                {
                    // Placeholder mode: position cursor BEFORE the placeholder character.
                    // This way GetTextExt returns valid coordinates at the original cursor
                    // position, while the placeholder space appears after it (like Bingling IME).
                    pRangeForSel->Collapse(ec, TF_ANCHOR_START);
                }
                else if (_caretPos >= 0 && _caretPos < (int)_text.length())
                {
                    // Move the range start to the caret position, then collapse to start
                    // This positions the cursor at the specified offset within the composition
                    LONG shifted = 0;
                    pRangeForSel->Collapse(ec, TF_ANCHOR_START);
                    pRangeForSel->ShiftEnd(ec, (LONG)_caretPos, &shifted, nullptr);
                    pRangeForSel->ShiftStart(ec, (LONG)_caretPos, &shifted, nullptr);
                }
                else
                {
                    // Default: cursor at end of composition
                    pRangeForSel->Collapse(ec, TF_ANCHOR_END);
                }

                TF_SELECTION sel = {};
                sel.range = pRangeForSel;
                sel.style.ase = TF_AE_NONE;
                sel.style.fInterimChar = FALSE;
                _pContext->SetSelection(ec, 1, &sel);

                pRangeForSel->Release();
            }
        }

        pRange->Release();

        // Cache caret position from within this valid edit session.
        // This is critical for WebView apps where a separate CCaretEditSession
        // with TF_INVALID_COOKIE would be rejected.
        // 但 composition 刚刚创建（_compositionJustStarted）时跳过缓存：宿主
        // 在此刻尚未完成 reflow，GetTextExt 返回的是 pre-reflow 旧坐标，写入
        // 缓存会让后续 timer 兜底取到陈旧值。等 timer/OnLayoutChange 路径走
        // GetCaretPosition fresh 查询。
        if (SUCCEEDED(hr) && !_pTextService->_compositionJustStarted)
        {
            _CacheCaretPosition(ec);
        }

        return hr;
    }

private:
    int _caretPos;         // Cursor position within composition (-1 = at end)
    BOOL _noUnderline;     // 整段不设下划线属性（智能符号 HoldComposition 观感对齐已上屏）

    void _CacheCaretPosition(TfEditCookie ec)
    {
        ITfContextView* pContextView = nullptr;
        if (FAILED(_pContext->GetActiveView(&pContextView)) || pContextView == nullptr)
            return;

        // Get current caret position (selection)
        TF_SELECTION sel[1];
        ULONG fetched = 0;
        if (SUCCEEDED(_pContext->GetSelection(ec, TF_DEFAULT_SELECTION, 1, sel, &fetched)) && fetched > 0 && sel[0].range != nullptr)
        {
            RECT caretRect = {};
            BOOL clipped = FALSE;
            if (SUCCEEDED(pContextView->GetTextExt(ec, sel[0].range, &caretRect, &clipped)))
            {
                // Skip degenerate rects (height=0) — apps like WPS may return
                // an invalid rect on the first composition before layout reflow.
                // Cache 仅作为 timer 兜底使用；OnLayoutChange 路径会清掉 cache
                // 并重新通过 fallback 查询，因此这里不再需要标记延迟重试。
                LONG h = caretRect.bottom - caretRect.top;
                // 同 GetCaretPositionFromTSF：跳过退化矩形(h<=0)与越界坐标，避免把不可信坐标
                // 缓存后被 timer 兜底取用。越界时不缓存，留待 fallback 链重新求解。
                if (h > 0 && !IsScreenPointOutsideForegroundWindow(caretRect.left, caretRect.top))
                {
                    _pTextService->_cachedCaretRect = caretRect;
                    _pTextService->_hasCachedCaretPos = TRUE;
                }
            }
            sel[0].range->Release();
        }

        // Get composition start position
        if (_pTextService->_pComposition != nullptr)
        {
            ITfRange* pCompRange = nullptr;
            if (SUCCEEDED(_pTextService->_pComposition->GetRange(&pCompRange)) && pCompRange != nullptr)
            {
                ITfRange* pStartRange = nullptr;
                if (SUCCEEDED(pCompRange->Clone(&pStartRange)) && pStartRange != nullptr)
                {
                    pStartRange->Collapse(ec, TF_ANCHOR_START);
                    // 组合头部若有待提交前缀（顶码聚合），上报的组合起点须偏移到
                    // 余码段起点——候选窗锚点跟随余码而非已顶出的文字。
                    LONG prefixLen = (LONG)_pTextService->GetPendingCommitPrefixLength();
                    if (prefixLen > 0)
                    {
                        LONG moved = 0;
                        pStartRange->ShiftEnd(ec, prefixLen, &moved, nullptr);
                        pStartRange->ShiftStart(ec, prefixLen, &moved, nullptr);
                    }
                    RECT compStartRect = {};
                    BOOL clipped = FALSE;
                    if (SUCCEEDED(pContextView->GetTextExt(ec, pStartRange, &compStartRect, &clipped)))
                    {
                        LONG compH = compStartRect.bottom - compStartRect.top;
                        if (compH > 0)
                        {
                            _pTextService->_cachedCompStartRect = compStartRect;
                            _pTextService->_hasCachedCompStartPos = TRUE;
                        }
                    }
                    pStartRange->Release();
                }
                pCompRange->Release();
            }
        }

        pContextView->Release();
    }

    void _SetDisplayAttribute(TfEditCookie ec, ITfRange* pRange)
    {
        // Get the display attribute atom from TextService
        TfGuidAtom gaDisplayAttr = _pTextService->GetDisplayAttributeInputAtom();
        if (gaDisplayAttr == TF_INVALID_GUIDATOM)
        {
            WIND_LOG_DEBUG(L"Display attribute not initialized\n");
            return;
        }

        // Get ITfProperty for display attribute
        ITfProperty* pDisplayAttrProp = nullptr;
        if (FAILED(_pContext->GetProperty(GUID_PROP_ATTRIBUTE, &pDisplayAttrProp)))
        {
            WIND_LOG_DEBUG(L"Failed to get GUID_PROP_ATTRIBUTE property\n");
            return;
        }

        VARIANT var;
        var.vt = VT_I4;
        var.lVal = gaDisplayAttr;

        // 分段显示属性（对齐微软 IME）：组合头部的待提交前缀（顶码已顶出的字）
        // 不带下划线——先 Clear 整段，再只对余码段 SetValue。前缀为 0 时即整段。
        pDisplayAttrProp->Clear(ec, pRange);

        // 整段无下划线模式（智能符号 HoldComposition）：中文符号留在组合态等待
        // press2 替换/超时提交，但观感上应与已上屏文本一致——Clear 后不再设值。
        if (_noUnderline)
        {
            WIND_LOG_DEBUG(L"Display attribute cleared (noUnderline mode)\n");
            pDisplayAttrProp->Release();
            return;
        }

        LONG prefixLen = (LONG)_pTextService->GetPendingCommitPrefixLength();
        ITfRange* pAttrRange = nullptr;
        if (prefixLen > 0)
        {
            if (SUCCEEDED(pRange->Clone(&pAttrRange)) && pAttrRange != nullptr)
            {
                LONG moved = 0;
                pAttrRange->ShiftStart(ec, prefixLen, &moved, nullptr);
            }
        }
        else
        {
            pAttrRange = pRange;
            pAttrRange->AddRef();
        }

        HRESULT hr = E_FAIL;
        if (pAttrRange != nullptr)
        {
            hr = pDisplayAttrProp->SetValue(ec, pAttrRange, &var);
            pAttrRange->Release();
        }
        if (FAILED(hr))
        {
            WIND_LOG_DEBUG(L"Failed to set display attribute\n");
        }
        else
        {
            WIND_LOG_DEBUG_FMT(L"Display attribute set (prefixLen=%ld)\n", prefixLen);
        }

        pDisplayAttrProp->Release();
    }

private:
    LONG _refCount;
    CTextService* _pTextService;
    ITfContext* _pContext;
    std::wstring _text;
};

static const LONG DEFAULT_CARET_HEIGHT = 20;

CTextService::CTextService()
    : _refCount(1)
    , _pThreadMgr(nullptr)
    , _tfClientId(TF_CLIENTID_NULL)
    , _dwThreadMgrEventSinkCookie(TF_INVALID_COOKIE)
    , _dwThreadFocusSinkCookie(TF_INVALID_COOKIE)
    , _uiElementId((DWORD)-1)
    , _uiElementShown(FALSE)
    , _pUIElementMgr(nullptr)
    , _pSourceSingle(nullptr)
    , _funcProviderRegistered(FALSE)
    , _hHotkeyWnd(nullptr)
    , _hotkeyWndClass(0)
    , _hotkeysActive(FALSE)
    , _addWordHotkeysActive(FALSE)
    , _focusIsPassword(false)
    , _focusInputScopeMask(0)
    , _passwordSuppressEnabled(TRUE)  // 默认开，与 core 的 password_suppress_enabled 初值一致
    , _hasThreadFocus(FALSE)
    , _activateFlags(0)
    , _pKeyEventSink(nullptr)
    , _pIPCClient(nullptr)
    , _pLangBarItemButton(nullptr)
    , _pHotkeyManager(nullptr)
    , _pHostWindow{}
    , _bChineseMode(TRUE)
    , _bFullWidth(FALSE)
    , _lastCapsKeyTick(0)
    , _lastActivateTick(0)
    , _focusSessionId(0)
    , _hasFocus(FALSE)
    , _hasTextInputContext(FALSE)
    , _pLastActiveDocMgr(nullptr)
    , _focusLostSent(FALSE)
    , _editCtxReported(FALSE)
    , _pComposition(nullptr)
    , _hasCachedCaretPos(FALSE)
    , _hasCachedCompStartPos(FALSE)
    , _compositionJustStarted(FALSE)
    , _needsFocusRecovery(FALSE)
    , _lastFocusCaretX(0)
    , _lastFocusCaretY(0)
    , _lastFocusCaretHeight(DEFAULT_CARET_HEIGHT)
    , _hasLastKnownCaretPos(FALSE)
    , _lastKnownCaretX(0)
    , _lastKnownCaretY(0)
    , _lastKnownCaretHeight(DEFAULT_CARET_HEIGHT)
    , _gaDisplayAttributeInput(TF_INVALID_GUIDATOM)
    , _dwLayoutSinkCookie(TF_INVALID_COOKIE)
    , _pLayoutSinkContext(nullptr)
    , _dwTextEditSinkCookie(TF_INVALID_COOKIE)
    , _pTextEditSinkContext(nullptr)
    , _cachedPrevChar(0)
    , _dwOpenCloseSinkCookie(TF_INVALID_COOKIE)
    , _bInCompartmentChange(FALSE)
    , _bKeyboardDisabled(FALSE)
    , _dwKeyboardDisabledSinkCookie(TF_INVALID_COOKIE)
    , _dwConversionSinkCookie(TF_INVALID_COOKIE)
    , _bInConversionChange(FALSE)
{
    ZeroMemory(&_cachedCaretRect, sizeof(_cachedCaretRect));
    ZeroMemory(&_cachedCompStartRect, sizeof(_cachedCompStartRect));
    DllAddRef();
}

CTextService::~CTextService()
{
    if (_pLastActiveDocMgr != nullptr)
    {
        _pLastActiveDocMgr->Release();
        _pLastActiveDocMgr = nullptr;
    }
    DllRelease();
}

STDAPI CTextService::QueryInterface(REFIID riid, void** ppvObj)
{
    if (ppvObj == nullptr)
        return E_INVALIDARG;

    *ppvObj = nullptr;

    if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfTextInputProcessor))
    {
        *ppvObj = (ITfTextInputProcessor*)this;
    }
    else if (IsEqualIID(riid, IID_ITfTextInputProcessorEx))
    {
        *ppvObj = (ITfTextInputProcessorEx*)this;
    }
    else if (IsEqualIID(riid, IID_ITfThreadMgrEventSink))
    {
        *ppvObj = (ITfThreadMgrEventSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfCompositionSink))
    {
        *ppvObj = (ITfCompositionSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfDisplayAttributeProvider))
    {
        *ppvObj = (ITfDisplayAttributeProvider*)this;
    }
    else if (IsEqualIID(riid, IID_ITfTextLayoutSink))
    {
        *ppvObj = (ITfTextLayoutSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfTextEditSink))
    {
        *ppvObj = (ITfTextEditSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfCompartmentEventSink))
    {
        *ppvObj = (ITfCompartmentEventSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfThreadFocusSink))
    {
        *ppvObj = (ITfThreadFocusSink*)this;
    }
    else if (IsEqualIID(riid, IID_ITfUIElement))
    {
        *ppvObj = (ITfUIElement*)(ITfCandidateListUIElementBehavior*)this;
    }
    else if (IsEqualIID(riid, IID_ITfCandidateListUIElement))
    {
        *ppvObj = (ITfCandidateListUIElement*)(ITfCandidateListUIElementBehavior*)this;
    }
    else if (IsEqualIID(riid, IID_ITfCandidateListUIElementBehavior))
    {
        *ppvObj = (ITfCandidateListUIElementBehavior*)this;
    }
    else if (IsEqualIID(riid, IID_ITfFunctionProvider))
    {
        *ppvObj = (ITfFunctionProvider*)this;
    }

    if (*ppvObj)
    {
        AddRef();
        return S_OK;
    }

    return E_NOINTERFACE;
}

STDAPI_(ULONG) CTextService::AddRef()
{
    return InterlockedIncrement(&_refCount);
}

STDAPI_(ULONG) CTextService::Release()
{
    LONG cr = InterlockedDecrement(&_refCount);

    if (cr == 0)
    {
        delete this;
    }

    return cr;
}

STDAPI CTextService::Activate(ITfThreadMgr* pThreadMgr, TfClientId tfClientId)
{
    return ActivateEx(pThreadMgr, tfClientId, 0);
}

STDAPI CTextService::ActivateEx(ITfThreadMgr* pThreadMgr, TfClientId tfClientId, DWORD dwFlags)
{
    WIND_LOG_INFO_FMT(L"TextService::ActivateEx called tfClientId=0x%08X dwFlags=0x%08X", tfClientId, dwFlags);

    // 起表：激活后的 compartment 变化是系统初始化同步，不是用户操作。
    // 见 _lastActivateTick 注释与下方两处 OnChange 守卫。
    _lastActivateTick = GetTickCount64();

    _activateFlags = dwFlags;

    WindHostProcessInfo currentHost;
    if (WindQueryCurrentProcessInfo(&currentHost))
        WindLogHostProcessInfo(4, L"compat.activate.current_host", currentHost);
    else
        WIND_LOG_WARN(L"compat.activate.current_host query failed");

    _pThreadMgr = pThreadMgr;
    _pThreadMgr->AddRef();

    _tfClientId = tfClientId;

    // Initialize thread manager event sink
    if (!_InitThreadMgrEventSink())
    {
        WIND_LOG_ERROR(L"_InitThreadMgrEventSink failed\n");
        Deactivate();
        return E_FAIL;
    }
    WIND_LOG_INFO(L"ThreadMgrEventSink initialized\n");

    // Initialize IPC client
    if (!_InitIPCClient())
    {
        WIND_LOG_ERROR(L"_InitIPCClient failed\n");
        Deactivate();
        return E_FAIL;
    }
    WIND_LOG_INFO(L"IPCClient initialized\n");

    // Initialize hotkey manager with default config
    _pHotkeyManager = new CHotkeyManager();
    WIND_LOG_INFO(L"HotkeyManager initialized\n");

    // Initialize key event sink
    if (!_InitKeyEventSink())
    {
        WIND_LOG_ERROR(L"_InitKeyEventSink failed\n");
        Deactivate();
        return E_FAIL;
    }
    WIND_LOG_INFO(L"KeyEventSink initialized\n");

    // 初始化 RegisterHotKey 用的隐藏消息窗口（候选可见时动态注册系统级热键）
    if (!_InitHotkeyWindow())
    {
        WIND_LOG_WARN(L"_InitHotkeyWindow failed (non-fatal, Ctrl+digit may double-process in Chromium hosts)\n");
    }

    // Initialize display attribute
    if (!_InitDisplayAttribute())
    {
        WIND_LOG_WARN(L"_InitDisplayAttribute failed (non-fatal)\n");
        // Not fatal, continue without display attribute
    }
    else
    {
        WIND_LOG_INFO(L"DisplayAttribute initialized\n");
    }

    // Initialize language bar button
    if (!_InitLangBarButton())
    {
        WIND_LOG_WARN(L"_InitLangBarButton failed (non-fatal)\n");
        // Not fatal, continue without language bar button
    }
    else
    {
        WIND_LOG_INFO(L"LangBarButton initialized\n");
    }

    // Initialize compartment event sink for GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
    // This allows us to respond when the system toggles the IME open/close state (e.g., Ctrl+Space)
    if (!_InitOpenCloseCompartment())
    {
        WIND_LOG_WARN(L"_InitOpenCloseCompartment failed (non-fatal)\n");
    }
    else
    {
        WIND_LOG_INFO(L"OpenCloseCompartment initialized\n");
    }

    // Initialize compartment event sink for GUID_COMPARTMENT_KEYBOARD_DISABLED
    // This allows us to stop intercepting keys when system disables keyboard input
    if (!_InitKeyboardDisabledCompartment())
    {
        WIND_LOG_WARN(L"_InitKeyboardDisabledCompartment failed (non-fatal)\n");
    }
    else
    {
        WIND_LOG_INFO(L"KeyboardDisabledCompartment initialized\n");
    }

    // Initialize INPUTMODE_CONVERSION compartment — exposes real Chinese/English mode
    // to external observers (KBLSwitch, Win11 taskbar). OPENCLOSE stays TRUE for our
    // internal OnTestKeyDown needs; this compartment carries the actual mode signal.
    if (!_InitConversionCompartment())
    {
        WIND_LOG_WARN(L"_InitConversionCompartment failed (non-fatal)\n");
    }
    else
    {
        WIND_LOG_INFO(L"ConversionCompartment initialized\n");
    }

    // Update caret position before notifying activation
    // This ensures status indicators appear at the correct position immediately
    SendCaretPositionUpdate();

    // Notify Go service that IME is activated and sync full state.
    // Uses _DoFullStateSync which also handles lazy connect (service may
    // still be starting after first install).
    _DoFullStateSync();

    // NOTE: Using synchronous IPC mode (no reader thread)
    // Reference: Weasel uses sync IPC with librime and it works well
    // The reader thread is not started - responses are received synchronously in OnKeyDown

    WIND_LOG_INFO(L"TextService::Activate completed successfully (sync IPC mode)\n");
    return S_OK;
}

STDAPI CTextService::Deactivate()
{
    WIND_LOG_INFO(L"TextService::Deactivate called\n");

    // 最先注销 compartment sinks：系统在输入法切换过程中会写 OPENCLOSE/CONVERSION
    // compartment，若 sink 仍挂着会被 OnChange 当作用户切换请求上报服务，
    // 把服务端权威中英状态污染成英文（表现为"切换输入法后默认变英文"）。
    _UninitOpenCloseCompartment();
    _UninitKeyboardDisabledCompartment();
    _UninitConversionCompartment();

    if (_pKeyEventSink != nullptr)
    {
        _pKeyEventSink->FlushEnglishStats();
    }

    // End any active composition before deactivating
    EndComposition();

    // 清理候选 UI 元素（必须在 ThreadMgr 释放之前）
    NotifyCandidatesVisibilityChanged(FALSE);

    // Unregister layout sink and edit sink
    _UnadviseTextLayoutSink();
    _UnadviseTextEditSink();

    // Release language bar button
    _UninitLangBarButton();

    // Release display attribute
    _UninitDisplayAttribute();

    // Release compartment event sinks
    _UninitOpenCloseCompartment();
    _UninitKeyboardDisabledCompartment();
    _UninitConversionCompartment();

    // 卸载 RegisterHotKey 隐藏窗口（必须在 KeyEventSink 释放之前，因为 WM_HOTKEY
    // 路径会回调 KeyEventSink）
    _UninitHotkeyWindow();

    // Release key event sink
    _UninitKeyEventSink();

    // Notify Go service that IME is being deactivated (before disconnecting)
    // This allows the service to hide the toolbar immediately
    if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        WIND_LOG_DEBUG(L"Sending ime_deactivated to service\n");
        // SendIMEDeactivated is async (fire-and-forget), no response expected
        _pIPCClient->SendIMEDeactivated();
    }

    // Release host window (before IPC client, so shared memory is still valid during shutdown)
    _DestroyHostWindow();

    // Release IPC client
    _UninitIPCClient();

    // Release hotkey manager
    if (_pHotkeyManager != nullptr)
    {
        delete _pHotkeyManager;
        _pHotkeyManager = nullptr;
    }

    // Release thread manager event sink
    _UninitThreadMgrEventSink();

    // Release thread manager
    SafeRelease(_pThreadMgr);

    _tfClientId = TF_CLIENTID_NULL;

    WIND_LOG_INFO(L"TextService::Deactivate completed\n");
    return S_OK;
}

BOOL CTextService::_InitThreadMgrEventSink()
{
    ITfSource* pSource = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfSource, (void**)&pSource);

    if (SUCCEEDED(hr))
    {
        hr = pSource->AdviseSink(IID_ITfThreadMgrEventSink,
                                 (ITfThreadMgrEventSink*)this,
                                 &_dwThreadMgrEventSinkCookie);

        // 并行 advise ITfThreadFocusSink — 线程级（进程 foreground）焦点通知。
        // 实现此接口让我们在 TSF 注册上看起来像"现代 IME"，让 Chromium / QQNT
        // 等宿主走完整 IME-first 调度路径而非 fallback，规避 Ctrl+数字 被双处理。
        HRESULT hrTf = pSource->AdviseSink(IID_ITfThreadFocusSink,
                                          (ITfThreadFocusSink*)this,
                                          &_dwThreadFocusSinkCookie);
        if (FAILED(hrTf))
        {
            WIND_LOG_WARN_FMT(L"AdviseSink(ITfThreadFocusSink) failed hr=0x%08X\n", (uint32_t)hrTf);
            _dwThreadFocusSinkCookie = TF_INVALID_COOKIE;
        }
        else
        {
            WIND_LOG_INFO(L"ITfThreadFocusSink advised\n");
        }

        pSource->Release();
    }

    // 缓存 ITfUIElementMgr，避免每次候选变化都 QueryInterface（NotifyCandidatesVisibilityChanged 使用）。
    if (_pUIElementMgr == nullptr)
    {
        HRESULT hrUI = _pThreadMgr->QueryInterface(IID_ITfUIElementMgr, (void**)&_pUIElementMgr);
        if (FAILED(hrUI))
        {
            _pUIElementMgr = nullptr;
        }
    }

    // 通过 ITfSourceSingle::AdviseSingleSink 把自己注册为该 IME 实例的 Function Provider。
    // 这是其它成熟 TSF IME 的标准做法，让 Chromium / QQNT 把我们识别为"现代 IME"，
    // 规避 Ctrl+数字 等热键被宿主同时处理。
    if (_pSourceSingle == nullptr)
    {
        HRESULT hrSS = _pThreadMgr->QueryInterface(IID_ITfSourceSingle, (void**)&_pSourceSingle);
        if (SUCCEEDED(hrSS) && _pSourceSingle != nullptr)
        {
            ITfFunctionProvider* pFP = static_cast<ITfFunctionProvider*>(this);
            HRESULT hrAdv = _pSourceSingle->AdviseSingleSink(_tfClientId, IID_ITfFunctionProvider, pFP);
            if (SUCCEEDED(hrAdv))
            {
                _funcProviderRegistered = TRUE;
                WIND_LOG_INFO(L"ITfFunctionProvider advised via ITfSourceSingle\n");
            }
            else
            {
                WIND_LOG_WARN_FMT(L"AdviseSingleSink(ITfFunctionProvider) failed hr=0x%08X\n", (uint32_t)hrAdv);
            }
        }
        else
        {
            WIND_LOG_WARN_FMT(L"QueryInterface(ITfSourceSingle) failed hr=0x%08X\n", (uint32_t)hrSS);
            _pSourceSingle = nullptr;
        }
    }

    return SUCCEEDED(hr);
}

void CTextService::_UninitThreadMgrEventSink()
{
    if (_dwThreadMgrEventSinkCookie != TF_INVALID_COOKIE || _dwThreadFocusSinkCookie != TF_INVALID_COOKIE)
    {
        ITfSource* pSource = nullptr;
        if (SUCCEEDED(_pThreadMgr->QueryInterface(IID_ITfSource, (void**)&pSource)))
        {
            if (_dwThreadMgrEventSinkCookie != TF_INVALID_COOKIE)
            {
                pSource->UnadviseSink(_dwThreadMgrEventSinkCookie);
            }
            if (_dwThreadFocusSinkCookie != TF_INVALID_COOKIE)
            {
                pSource->UnadviseSink(_dwThreadFocusSinkCookie);
            }
            pSource->Release();
        }
        _dwThreadMgrEventSinkCookie = TF_INVALID_COOKIE;
        _dwThreadFocusSinkCookie = TF_INVALID_COOKIE;
    }

    if (_pUIElementMgr != nullptr)
    {
        _pUIElementMgr->Release();
        _pUIElementMgr = nullptr;
    }

    if (_pSourceSingle != nullptr)
    {
        if (_funcProviderRegistered)
        {
            _pSourceSingle->UnadviseSingleSink(_tfClientId, IID_ITfFunctionProvider);
            _funcProviderRegistered = FALSE;
        }
        _pSourceSingle->Release();
        _pSourceSingle = nullptr;
    }
}

// ITfThreadFocusSink — 线程进入 foreground（应用窗口被激活）。
// 跨进程协调：当某个进程通过 race check 让出热键时，PostMessage 给当前前台
// 进程的 IME hidden window，让对方立即重试注册（避免对方等到下次 IME 事件
// 才发现热键空出来）。消息 ID 用 RegisterWindowMessage 全局注册，所有进程
// 取到相同 ID。
static UINT GetRetryHotkeyMessageId()
{
    static UINT s_msg = RegisterWindowMessageW(L"WindInputHotkeyRetry_v1");
    return s_msg;
}

// Foreground self-check timer：兜底**热键泄漏**（不是兜底焦点信号缺失）。
// ⚠ 此处旧注释称 OnKillThreadFocus 在 Chromium / Wails 类宿主「可能不触发」——
// 2026-07-20 实测证伪：Chrome 5/5、VSCode 5/5、Edge 11/11 次触发，零漏，与实际
// 切换严格一一对应；只是比 DocMgr 级失焦晚约 100ms。定时器偶尔抢先执行释放，
// 是在与这个迟到 100ms 的信号赛跑，不是在替补失踪的信号。
// 保留本定时器的真正理由：热键注册状态可能因 WM_HOTKEY 竞态等原因与焦点不同步，
// 且 RegisterHotKey 冲突（1409）的后果是前台应用彻底拿不到热键，值得一道独立防线。
// 只要本进程持有任何热键就需要定期校验，500ms 间隔既能尽快让出，开销又可忽略。
static constexpr UINT_PTR kFocusCheckTimerId = 0x57494E44; // 'WIND'
static constexpr UINT     kFocusCheckIntervalMs = 500;

// 慢焦点探针阈值：超过此值的 OnSetFocus 记一行 WARN。取 20ms 的依据——实测正常
// 焦点切换（开 DEBUG 日志、含两次进程信息采集）约 5~13ms，关日志后更低；20ms 既
// 不会被常规切换触发，又能抓住宿主 churn 焦点导致的堆积。
static constexpr double kSlowFocusWarnMs = 20.0;

// XamlIsland / transient locked DocMgr 标志：dynFlags 的 0x20 位稳定标记 Explorer 的
// XamlIsland 容器 DocMgr（RequestEditSession 对它返回 TF_E_NOLOCK）。OnSetFocus 对这类
// DocMgr 跳过 focus_gained（防 composition replay 到不稳定文档）。
// 提为文件级常量是因为 OnSetFocus 里有**两处**要判它：跳过 focus_gained 的守卫本身，
// 以及 doc_changed 收口——后者必须预判前者会不会命中，否则会发出一个没有配对
// focus_gained 的 focus_lost（详见两处注释）。
static constexpr DWORD kXamlIslandLockedFlag = 0x20;

// 激活静默期：ActivateEx 之后这段时间内的 compartment 变化视为系统初始化同步而非用户
// 操作。实测激活后 ~96ms 出现一次 CONVERSION 变化，取 250ms 留 2.6 倍余量。
//
// 不取更大值是因为它会和 _hasThreadFocus 的滞后叠加：后者由 500ms 自检定时器兜底
// （OnSetThreadFocus 可能漏，见 _MsgWndProc 里的反向纠正），两者相加就是切换应用后
// 「外部改模式的通道被忽略」的最长窗口。Ctrl+Space 不受影响——它在 OnTestKeyDown
// 就被拦截、由 OnKeyDown 自行处理，根本不走 compartment OnChange。
static constexpr ULONGLONG kActivateSettleMs = 250;

// Win32 RegisterHotKey 热键 ID（前置声明给 OnKillThreadFocus 使用）
// 窗口类名按 Debug / Release 区分（class name 是 per-process 不会跨进程冲突，但
// 命名约定与 pipe / CLSID 等其他跨进程资源保持一致，便于 Spy++ 等工具区分两版本）。
#ifdef WIND_DEV_VARIANT
static const wchar_t* kHotkeyWndClassName = L"WindInputHotkeyWndDebug";
static const wchar_t* kHotkeyWndTitle     = L"WindInputHotkeyDebug";
#else
static const wchar_t* kHotkeyWndClassName = L"WindInputHotkeyWnd";
static const wchar_t* kHotkeyWndTitle     = L"WindInputHotkey";
#endif
static constexpr int  kHotkeyIdPinBase    = 0x4000; // Pin: Ctrl+N → id = kHotkeyIdPinBase + N
static constexpr int  kHotkeyIdDelBase    = 0x4010; // Delete: Ctrl+Shift+N → id = kHotkeyIdDelBase + N
static constexpr int  kHotkeyIdAddWordBase = 0x4020; // 加词热键（add_word / open_add_word_dialog），最多 16 个

// 加词热键重新评估自触发消息：_ReevaluateAddWordHotkey 从任意线程 PostMessage 此消息到
// _hHotkeyWnd，主线程 WndProc 收到后执行真正的 RegisterHotKey/UnregisterHotKey（该 API
// 须在拥有窗口的线程调用）。WM_APP 段为窗口私有，隐藏窗口专用不冲突。
static constexpr UINT WM_WIND_REEVAL_ADDWORD = WM_APP + 0x51;

STDAPI CTextService::OnSetThreadFocus()
{
    WIND_LOG_DEBUG(L"OnSetThreadFocus called\n");
    _hasThreadFocus = TRUE;
    // 拿回 thread focus：候选可见性热键由 NotifyCandidatesVisibilityChanged 驱动，
    // 这里不主动补——切焦点时候选窗通常已经消失。
    // 加词热键不依赖候选，重新评估（若当前中文+文本框则重新注册）。
    _ReevaluateAddWordHotkey();
    return S_OK;
}

// ITfThreadFocusSink — 线程退出 foreground。
STDAPI CTextService::OnKillThreadFocus()
{
    WIND_LOG_DEBUG(L"OnKillThreadFocus called\n");
    _hasThreadFocus = FALSE;
    // 立即让出所有热键，让前台应用的 IME 实例能注册成功。
    if (_hotkeysActive)
    {
        _UnregisterCandidateHotkeys();
    }
    if (_addWordHotkeysActive)
    {
        _UnregisterAddWordHotkeys();
    }

    // 整个应用失去前台 = 真正离开了当前文档，在此收口输入态。
    // DocMgr 级失焦不再承担这件事（见 OnSetFocus 失焦分支注释），故必须由这里兜住，
    // 否则切走再切回同一文档时会残留上次没打完的 composition。
    // 传缓存的 DocMgr 作 hint：composition 建在它上面，而此刻 GetFocus() 未必还可用；
    // 不传则可能落到 forced cleanup，把残留文本提交进文档（Excel/WPS 表格的 'd' 漏字）。
    // 实测本回调在 Chrome/VSCode/Edge 各 5/5、5/5、11/11 次触发零漏，仅比 DocMgr
    // 级失焦晚约 100ms —— 该延迟用户不可感知，且远优于 500ms 自检定时器兜底。
    CleanupInputStateForDocChange(_pLastActiveDocMgr, FOCUS_LOST_REASON_THREAD);
    // 注意：失焦时**不**销毁 HostWindow。SearchHost/任务管理器等用 XamlIsland
    // locked/transient DocMgr，OnSetFocus 对其跳过 focus_gained（防 composition
    // replay），而 HostWindow 重建依赖 focus_gained → 一旦销毁就再也不会重建，
    // 候选永久不显示。per-PID event 模型下 HostWindow 可常驻：失焦时 Go 发 WriteHide
    // 经本进程 event 隐藏窗口即可，无需销毁。
    return S_OK;
}

// ============================================================================
// Win32 RegisterHotKey 支持
// 候选可见时把 Ctrl+0..9 + Ctrl+Shift+0..9 注册为系统级热键，OS 在 WM_KEYDOWN
// 派发之前直接消费，规避 QQNT 等 Chromium 类宿主的加速键双处理。无候选时立即
// UnregisterHotKey 让宿主重获这些键。机制来自第三方输入法的实测验证。
// ============================================================================

BOOL CTextService::_InitHotkeyWindow()
{
    if (_hHotkeyWnd != nullptr) return TRUE;

    HINSTANCE hInst = g_hInstance; // DLL 实例句柄（dllmain 设置）

    WNDCLASSEXW wc = {};
    wc.cbSize        = sizeof(wc);
    wc.lpfnWndProc   = _HotkeyWndProc;
    wc.hInstance     = hInst;
    wc.lpszClassName = kHotkeyWndClassName;
    _hotkeyWndClass = RegisterClassExW(&wc);
    if (_hotkeyWndClass == 0)
    {
        DWORD err = GetLastError();
        // ERROR_CLASS_ALREADY_EXISTS (1410) 是正常情况（同进程多次激活）
        if (err != 1410)
        {
            WIND_LOG_WARN_FMT(L"RegisterClassExW(hotkey) failed err=%u\n", err);
            return FALSE;
        }
    }

    // 消息专用窗口（HWND_MESSAGE 父窗口），不可见、不占桌面位置。
    _hHotkeyWnd = CreateWindowExW(0, kHotkeyWndClassName, kHotkeyWndTitle,
                                   0, 0, 0, 0, 0,
                                   HWND_MESSAGE, nullptr, hInst, nullptr);
    if (_hHotkeyWnd == nullptr)
    {
        WIND_LOG_WARN_FMT(L"CreateWindowEx(hotkey) failed err=%u\n", (uint32_t)GetLastError());
        return FALSE;
    }
    // 把 this 存到窗口数据，WndProc 用来取回 CTextService 实例
    SetWindowLongPtrW(_hHotkeyWnd, GWLP_USERDATA, (LONG_PTR)this);
    WIND_LOG_INFO_FMT(L"Hotkey window created hwnd=0x%p\n", _hHotkeyWnd);

    // 初始化 thread focus 状态：TSF 的 OnSetThreadFocus 仅在焦点 transition 时触发，
    // 若 IME 激活时本进程恰好已经是前台（典型场景：用户刚点中输入框），不能等
    // 它的通知。用 GetForegroundWindow 的进程 ID 做自检种子。
    HWND hFg = GetForegroundWindow();
    if (hFg != nullptr)
    {
        DWORD fgPid = 0;
        GetWindowThreadProcessId(hFg, &fgPid);
        _hasThreadFocus = (fgPid == GetCurrentProcessId());
        WIND_LOG_DEBUG_FMT(L"_InitHotkeyWindow: initial thread focus seed=%d (fgPid=%u ownPid=%u)\n",
                           (int)_hasThreadFocus, fgPid, GetCurrentProcessId());
    }
    // 启动前台自检 timer：兜底热键泄漏（非焦点信号缺失，实测数据见 kFocusCheckTimerId 注释）
    SetTimer(_hHotkeyWnd, kFocusCheckTimerId, kFocusCheckIntervalMs, nullptr);
    return TRUE;
}

void CTextService::_UninitHotkeyWindow()
{
    if (_hotkeysActive)
    {
        _UnregisterCandidateHotkeys();
    }
    if (_addWordHotkeysActive)
    {
        _UnregisterAddWordHotkeys();
    }
    if (_hHotkeyWnd != nullptr)
    {
        DestroyWindow(_hHotkeyWnd);
        _hHotkeyWnd = nullptr;
    }
    if (_hotkeyWndClass != 0)
    {
        UnregisterClassW(kHotkeyWndClassName, g_hInstance);
        _hotkeyWndClass = 0;
    }
}

void CTextService::_RegisterCandidateHotkeys()
{
    if (_hHotkeyWnd == nullptr || _hotkeysActive) return;
    // 没拿到 thread focus 时绝不注册 — 多进程 IME 实例竞争同一组热键会引发
    // ERROR_HOTKEY_ALREADY_REGISTERED (1409)，让前台应用 IME 实例反而注册不上。
    if (!_hasThreadFocus) return;

    int registered = 0;
    // Ctrl+0..9 (Pin)
    for (int n = 0; n <= 9; ++n)
    {
        if (RegisterHotKey(_hHotkeyWnd, kHotkeyIdPinBase + n, MOD_CONTROL | MOD_NOREPEAT, '0' + n))
        {
            registered++;
        }
    }
    // Ctrl+Shift+0..9 (Delete)
    for (int n = 0; n <= 9; ++n)
    {
        if (RegisterHotKey(_hHotkeyWnd, kHotkeyIdDelBase + n, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, '0' + n))
        {
            registered++;
        }
    }
    _hotkeysActive = TRUE;
    WIND_LOG_DEBUG_FMT(L"RegisterCandidateHotkeys: registered=%d/20\n", registered);
}

void CTextService::_UnregisterCandidateHotkeys()
{
    if (_hHotkeyWnd == nullptr || !_hotkeysActive) return;

    for (int n = 0; n <= 9; ++n)
    {
        UnregisterHotKey(_hHotkeyWnd, kHotkeyIdPinBase + n);
        UnregisterHotKey(_hHotkeyWnd, kHotkeyIdDelBase + n);
    }
    _hotkeysActive = FALSE;
    WIND_LOG_DEBUG(L"UnregisterCandidateHotkeys\n");
}

namespace
{
    // 内部 KEYMOD（SHIFT=1/CTRL=2/ALT=4/WIN=8）→ Win32 RegisterHotKey fsModifiers
    //（ALT=1/CTRL=2/SHIFT=4/WIN=8）。SHIFT 与 ALT 位互换，必须逐位映射，绝不能直传。
    UINT _ToWin32HotkeyMods(uint32_t keymod)
    {
        UINT f = 0;
        if (keymod & KEYMOD_CTRL)  f |= MOD_CONTROL;
        if (keymod & KEYMOD_SHIFT) f |= MOD_SHIFT;
        if (keymod & KEYMOD_ALT)   f |= MOD_ALT;
        if (keymod & KEYMOD_WIN)   f |= MOD_WIN;
        return f;
    }
}

// 中英模式集中 setter：赋值后触发加词热键重评（模式是门卫条件）。reeval 内部 post，
// 故本函数可从任意线程（含 async reader 线程）安全调用。
void CTextService::_SetChineseMode(BOOL v)
{
    _bChineseMode = v;
    _ReevaluateAddWordHotkey();
}

// 线程安全入口：RegisterHotKey/UnregisterHotKey 必须在拥有 _hHotkeyWnd 的线程调用，
// 而模式变化可能来自 async reader 线程（StatePushCallback）。统一 post 到窗口线程执行。
void CTextService::_ReevaluateAddWordHotkey()
{
    if (_hHotkeyWnd != nullptr)
    {
        PostMessageW(_hHotkeyWnd, WM_WIND_REEVAL_ADDWORD, 0, 0);
    }
}

// 主线程：按门卫条件（中文 + 文本框 + 非密码框 + thread focus）注册或注销加词热键。幂等。
void CTextService::_DoReevaluateAddWordHotkey()
{
    BOOL want = _hasThreadFocus && _bChineseMode && _hasTextInputContext && !_focusIsPassword;
    if (want && !_addWordHotkeysActive)
    {
        _RegisterAddWordHotkeys();
    }
    else if (!want && _addWordHotkeysActive)
    {
        _UnregisterAddWordHotkeys();
    }
}

void CTextService::_RegisterAddWordHotkeys()
{
    if (_hHotkeyWnd == nullptr || _addWordHotkeysActive) return;
    // 与候选热键同规：无 thread focus 绝不注册，避免多进程 IME 实例争抢同一组合键
    // 引发 ERROR_HOTKEY_ALREADY_REGISTERED (1409)。
    if (!_hasThreadFocus || _pHotkeyManager == nullptr) return;

    const auto& globals = _pHotkeyManager->GlobalHotkeys();
    int id = kHotkeyIdAddWordBase;
    int registered = 0;
    for (uint32_t rawHash : globals)
    {
        if (id >= kHotkeyIdAddWordBase + 16) break; // 安全上限
        uint32_t vk = rawHash & 0xFFFF;
        uint32_t keymod = rawHash >> 16;
        UINT fsMods = _ToWin32HotkeyMods(keymod) | MOD_NOREPEAT;
        if (RegisterHotKey(_hHotkeyWnd, id, fsMods, vk))
        {
            _addWordHotkeyIds.emplace_back(id, rawHash);
            registered++;
        }
        id++;
    }
    _addWordHotkeysActive = !_addWordHotkeyIds.empty();
    WIND_LOG_DEBUG_FMT(L"RegisterAddWordHotkeys: registered=%d/%d\n", registered, (int)globals.size());
}

void CTextService::_UnregisterAddWordHotkeys()
{
    if (_hHotkeyWnd == nullptr) return;
    for (const auto& kv : _addWordHotkeyIds)
    {
        UnregisterHotKey(_hHotkeyWnd, kv.first);
    }
    _addWordHotkeyIds.clear();
    _addWordHotkeysActive = FALSE;
    WIND_LOG_DEBUG(L"UnregisterAddWordHotkeys\n");
}

LRESULT CALLBACK CTextService::_HotkeyWndProc(HWND hWnd, UINT msg, WPARAM wParam, LPARAM lParam)
{
    // 加词热键重新评估（由 _ReevaluateAddWordHotkey 从任意线程 post）：在窗口线程执行
    // 真正的 RegisterHotKey/UnregisterHotKey。
    if (msg == WM_WIND_REEVAL_ADDWORD)
    {
        CTextService* self = reinterpret_cast<CTextService*>(GetWindowLongPtrW(hWnd, GWLP_USERDATA));
        if (self != nullptr)
        {
            self->_DoReevaluateAddWordHotkey();
        }
        return 0;
    }
    // 跨进程"热键已释放，请重试"通知：其他进程的 race check 让出热键时投递。
    // 我们立即重新评估并尝试注册。注意 msg ID 是动态分配的，必须用 if 而非 case。
    static UINT s_retryMsg = GetRetryHotkeyMessageId();
    if (msg == s_retryMsg)
    {
        CTextService* self = reinterpret_cast<CTextService*>(GetWindowLongPtrW(hWnd, GWLP_USERDATA));
        if (self != nullptr)
        {
            // 复核自己确实是前台再注册（这条消息可能来得稍晚，焦点已经又变了）。
            HWND hFg = GetForegroundWindow();
            DWORD fgPid = 0;
            if (hFg != nullptr) GetWindowThreadProcessId(hFg, &fgPid);
            if (fgPid == GetCurrentProcessId())
            {
                self->_hasThreadFocus = TRUE;
                // 候选可见性热键由 NotifyCandidatesVisibilityChanged 驱动；
                // 候选下一次出现时自然会重新注册。
                // 加词热键不依赖候选，须在此主动重评（已在窗口线程）。
                self->_DoReevaluateAddWordHotkey();
                WIND_LOG_DEBUG(L"Received hotkey retry signal, marked thread focus\n");
            }
        }
        return 0;
    }
    // 定时自检：兜底热键泄漏。每 500ms 跑一次，如果发现本进程不再前台但仍持有热键，
    // 主动释放并通知前台进程重试。注意本分支被 holdsAnyHotkey 门控，因此它纠正的是
    // 「热键状态」而非「_hasThreadFocus 的正确性」——不要把它当作焦点信号的兜底。
    if (msg == WM_TIMER && wParam == kFocusCheckTimerId)
    {
        CTextService* self = reinterpret_cast<CTextService*>(GetWindowLongPtrW(hWnd, GWLP_USERDATA));
        if (self != nullptr)
        {
            HWND hFg = GetForegroundWindow();
            DWORD fgPid = 0;
            if (hFg != nullptr) GetWindowThreadProcessId(hFg, &fgPid);
            BOOL nowForeground = (fgPid == GetCurrentProcessId());
            BOOL holdsAnyHotkey = self->_hotkeysActive || self->_addWordHotkeysActive;
            if (!nowForeground && holdsAnyHotkey)
            {
                WIND_LOG_DEBUG_FMT(L"FocusCheck timer: not foreground (fgPid=%u ownPid=%u), releasing\n",
                                   fgPid, GetCurrentProcessId());
                self->_hasThreadFocus = FALSE;
                if (self->_hotkeysActive) self->_UnregisterCandidateHotkeys();
                if (self->_addWordHotkeysActive) self->_UnregisterAddWordHotkeys();
                // 通知前台 IME 立即重试
                const wchar_t* classNames[] = { L"WindInputHotkeyWnd", L"WindInputHotkeyWndDebug" };
                UINT retryMsg = GetRetryHotkeyMessageId();
                for (auto cls : classNames)
                {
                    HWND target = nullptr;
                    while ((target = FindWindowExW(HWND_MESSAGE, target, cls, nullptr)) != nullptr)
                    {
                        DWORD targetPid = 0;
                        GetWindowThreadProcessId(target, &targetPid);
                        if (targetPid == fgPid)
                        {
                            PostMessageW(target, retryMsg, 0, 0);
                            break;
                        }
                    }
                }
            }
            else if (nowForeground && !self->_hasThreadFocus)
            {
                // 反向：成为前台但 _hasThreadFocus 还没更新（OnSetThreadFocus 也可能漏），
                // 标记一下；候选可见性热键由 NotifyCandidatesVisibilityChanged 驱动。
                self->_hasThreadFocus = TRUE;
                // 加词热键不依赖候选，须主动重评（已在窗口线程）。
                self->_DoReevaluateAddWordHotkey();
            }
        }
        return 0;
    }
    if (msg == WM_HOTKEY)
    {
        CTextService* self = reinterpret_cast<CTextService*>(GetWindowLongPtrW(hWnd, GWLP_USERDATA));
        if (self != nullptr && self->_pKeyEventSink != nullptr)
        {
            // 焦点切换竞态防护：用户按下 Ctrl+= 的瞬间焦点可能正从 App A 切到 App B，
            // 此时 A 的 OnKillThreadFocus 还没来得及 unregister，WM_HOTKEY 会落到
            // A 的 hidden window；如果直接处理，会把加词动作下发到已经失焦的 IME
            // 实例（错的 PID / 错的光标位置）。
            // 实时复核当前前台进程是否是自己，不是就主动 unregister 让出热键，
            // 这一次按键丢弃（用户下一次按时新前台已经注册好）。
            HWND hFg = GetForegroundWindow();
            DWORD fgPid = 0;
            if (hFg != nullptr)
            {
                GetWindowThreadProcessId(hFg, &fgPid);
            }
            if (fgPid != GetCurrentProcessId())
            {
                WIND_LOG_DEBUG_FMT(L"WM_HOTKEY race: not foreground (fgPid=%u ownPid=%u), releasing hotkeys\n",
                                   fgPid, GetCurrentProcessId());
                self->_hasThreadFocus = FALSE;
                if (self->_hotkeysActive) self->_UnregisterCandidateHotkeys();
                if (self->_addWordHotkeysActive) self->_UnregisterAddWordHotkeys();
                // 通知前台进程的 IME hidden window 立即重试注册（避免它要等下次
                // 候选变化才发现热键空了）。两个变体的 class name 都搜。
                const wchar_t* classNames[] = { L"WindInputHotkeyWnd", L"WindInputHotkeyWndDebug" };
                UINT retryMsg = GetRetryHotkeyMessageId();
                for (auto cls : classNames)
                {
                    HWND target = nullptr;
                    while ((target = FindWindowExW(HWND_MESSAGE, target, cls, nullptr)) != nullptr)
                    {
                        DWORD targetPid = 0;
                        GetWindowThreadProcessId(target, &targetPid);
                        if (targetPid == fgPid)
                        {
                            PostMessageW(target, retryMsg, 0, 0);
                            WIND_LOG_DEBUG_FMT(L"Posted retry to foreground IME hwnd=0x%p pid=%u\n", target, fgPid);
                            break;
                        }
                    }
                }
                return 0;
            }
            int id = (int)wParam;
            uint32_t vk = 0;
            uint32_t mods = 0;
            if (id >= kHotkeyIdPinBase && id < kHotkeyIdPinBase + 10)
            {
                vk = '0' + (id - kHotkeyIdPinBase);
                mods = KEYMOD_CTRL;
            }
            else if (id >= kHotkeyIdDelBase && id < kHotkeyIdDelBase + 10)
            {
                vk = '0' + (id - kHotkeyIdDelBase);
                mods = KEYMOD_CTRL | KEYMOD_SHIFT;
            }
            else if (id >= kHotkeyIdAddWordBase && id < kHotkeyIdAddWordBase + 16)
            {
                // 加词热键：从注册记录反解 (vk, KEYMOD)，下发给 coordinator 按 hash 匹配 action。
                for (const auto& kv : self->_addWordHotkeyIds)
                {
                    if (kv.first == id)
                    {
                        vk = kv.second & 0xFFFF;
                        mods = kv.second >> 16;
                        break;
                    }
                }
                // WM_HOTKEY 通路不经过 OnKeyDown 的 caret 更新；加词会建立占位 composition +
                // 预览候选窗，先补一次 caret 更新确保定位准确（对齐原 OnKeyDown 通路）。
                if (vk != 0)
                {
                    self->SendCaretPositionUpdate();
                }
            }
            if (vk != 0)
            {
                WIND_LOG_DEBUG_FMT(L"WM_HOTKEY id=0x%04X vk=0x%02X mods=0x%04X\n", id, vk, mods);
                self->_pKeyEventSink->DispatchHotkey(vk, mods);
            }
        }
        return 0;
    }
    return DefWindowProcW(hWnd, msg, wParam, lParam);
}

// ============================================================================
// ITfFunctionProvider
// 通过 ITfSourceSingle::AdviseSingleSink 把自己注册为该 IME 的 Function Provider。
// 其它成熟 TSF IME 都这么做，让 Chromium / QQNT 识别为完整 IME。
// 当前 stub 实现：GetFunction 一律返回 E_NOINTERFACE，不提供任何具体函数。
// 仅"注册存在"本身就足以达到识别效果。
// 注意 ITfFunctionProvider::GetDescription 与 ITfUIElement::GetDescription 同签名，
// C++ 多继承合并为单一实现，复用 ITfUIElement 那一份即可。
// ============================================================================

STDAPI CTextService::GetType(GUID* pguid)
{
    if (pguid == nullptr) return E_INVALIDARG;
    // 用 IME 本身的 CLSID 作为 function provider 类型标识
    *pguid = c_clsidTextService;
    return S_OK;
}

STDAPI CTextService::GetFunction(REFGUID rguid, REFIID riid, IUnknown** ppunk)
{
    if (ppunk == nullptr) return E_INVALIDARG;
    *ppunk = nullptr;
    // 不提供任何具体 function。如果未来需要支持 ITfFnSearchCandidateProvider /
    // ITfFnReverseConversion 等，在此处分发。
    return E_NOINTERFACE;
}

// ============================================================================
// ITfUIElement / ITfCandidateListUIElement / ITfCandidateListUIElementBehavior
// 当前阶段：用 stub 数据验证 ITfUIElementMgr::BeginUIElement 注册本身能否让
// Chromium / QQNT 走完整 IME-first 调度路径，规避 Ctrl+数字 被宿主同时处理。
// 候选数据由 Go-side UI 渲染，C++ 这里返回占位数据即可。
// ============================================================================

static const GUID kWindCandidateUIElementGuid =
    { 0xb3e54a91, 0x7c20, 0x4b6a, { 0xa1, 0x5e, 0x82, 0x09, 0x77, 0x55, 0x44, 0x33 } };

STDAPI CTextService::GetDescription(BSTR* pbstrDescription)
{
    if (pbstrDescription == nullptr) return E_INVALIDARG;
    *pbstrDescription = SysAllocString(L"WindInput Candidate List");
    return *pbstrDescription ? S_OK : E_OUTOFMEMORY;
}

STDAPI CTextService::GetGUID(GUID* pguid)
{
    if (pguid == nullptr) return E_INVALIDARG;
    *pguid = kWindCandidateUIElementGuid;
    return S_OK;
}

STDAPI CTextService::Show(BOOL bShow)
{
    WIND_LOG_DEBUG_FMT(L"ITfUIElement::Show(%d)\n", (int)bShow);
    _uiElementShown = bShow;
    return S_OK;
}

STDAPI CTextService::IsShown(BOOL* pbShow)
{
    if (pbShow == nullptr) return E_INVALIDARG;
    *pbShow = _uiElementShown;
    return S_OK;
}

STDAPI CTextService::GetUpdatedFlags(DWORD* pdwFlags)
{
    if (pdwFlags == nullptr) return E_INVALIDARG;
    *pdwFlags = TF_CLUIE_DOCUMENTMGR | TF_CLUIE_COUNT | TF_CLUIE_SELECTION
              | TF_CLUIE_STRING | TF_CLUIE_PAGEINDEX | TF_CLUIE_CURRENTPAGE;
    return S_OK;
}

STDAPI CTextService::GetDocumentMgr(ITfDocumentMgr** ppdim)
{
    if (ppdim == nullptr) return E_INVALIDARG;
    *ppdim = nullptr;
    if (_pThreadMgr)
    {
        _pThreadMgr->GetFocus(ppdim); // may set null when no focus; that's OK
    }
    return S_OK;
}

STDAPI CTextService::GetCount(UINT* puCount)
{
    if (puCount == nullptr) return E_INVALIDARG;
    *puCount = 1; // stub: 至少 1 个候选才能让 TSF 认为候选 UI "有意义"
    return S_OK;
}

STDAPI CTextService::GetSelection(UINT* puIndex)
{
    if (puIndex == nullptr) return E_INVALIDARG;
    *puIndex = 0;
    return S_OK;
}

STDAPI CTextService::GetString(UINT uIndex, BSTR* pstr)
{
    if (pstr == nullptr) return E_INVALIDARG;
    *pstr = SysAllocString(L"…"); // 占位
    return *pstr ? S_OK : E_OUTOFMEMORY;
}

STDAPI CTextService::GetPageIndex(UINT* pIndex, UINT uSize, UINT* puPageCnt)
{
    if (puPageCnt == nullptr) return E_INVALIDARG;
    *puPageCnt = 1;
    if (pIndex && uSize >= 1)
    {
        pIndex[0] = 0;
    }
    return S_OK;
}

STDAPI CTextService::SetPageIndex(UINT* pIndex, UINT uPageCnt)
{
    // no-op (read-only stub)
    return S_OK;
}

STDAPI CTextService::GetCurrentPage(UINT* puPage)
{
    if (puPage == nullptr) return E_INVALIDARG;
    *puPage = 0;
    return S_OK;
}

STDAPI CTextService::SetSelection(UINT nIndex)
{
    WIND_LOG_DEBUG_FMT(L"ITfCandidateListUIElementBehavior::SetSelection(%u)\n", nIndex);
    return S_OK; // no-op: TSF 不参与候选选择，Go 端处理
}

STDAPI CTextService::Finalize(void)
{
    WIND_LOG_DEBUG(L"ITfCandidateListUIElementBehavior::Finalize\n");
    return S_OK;
}

STDAPI CTextService::Abort(void)
{
    WIND_LOG_DEBUG(L"ITfCandidateListUIElementBehavior::Abort\n");
    return S_OK;
}

void CTextService::NotifyCandidatesVisibilityChanged(BOOL hasCandidates)
{
    // 候选可见 → 注册系统级热键拦截 Ctrl+0..9/Ctrl+Shift+0..9；候选消失 → 卸载，
    // 让宿主重新获得这些键。这是第三方输入法使用的成熟机制，规避 Chromium 类宿主
    // 的加速键双处理。
    if (hasCandidates && !_hotkeysActive)
    {
        _RegisterCandidateHotkeys();
    }
    else if (!hasCandidates && _hotkeysActive)
    {
        _UnregisterCandidateHotkeys();
    }

    if (_pUIElementMgr == nullptr) return;

    if (hasCandidates && _uiElementId == (DWORD)-1)
    {
        BOOL bShow = TRUE;
        // 通过 Behavior 路径解决菱形继承
        HRESULT hr = _pUIElementMgr->BeginUIElement(
            static_cast<ITfUIElement*>(static_cast<ITfCandidateListUIElementBehavior*>(this)),
            &bShow, &_uiElementId);
        if (SUCCEEDED(hr))
        {
            _uiElementShown = bShow;
            WIND_LOG_DEBUG_FMT(L"BeginUIElement ok id=%u show=%d\n", _uiElementId, (int)bShow);
        }
        else
        {
            WIND_LOG_WARN_FMT(L"BeginUIElement failed hr=0x%08X\n", (uint32_t)hr);
            _uiElementId = (DWORD)-1;
        }
    }
    else if (!hasCandidates && _uiElementId != (DWORD)-1)
    {
        HRESULT hr = _pUIElementMgr->EndUIElement(_uiElementId);
        WIND_LOG_DEBUG_FMT(L"EndUIElement id=%u hr=0x%08X\n", _uiElementId, (uint32_t)hr);
        _uiElementId = (DWORD)-1;
        _uiElementShown = FALSE;
    }
    else if (hasCandidates && _uiElementId != (DWORD)-1)
    {
        // 已注册，仅触发 update
        _pUIElementMgr->UpdateUIElement(_uiElementId);
    }
}

STDAPI CTextService::OnInitDocumentMgr(ITfDocumentMgr* pDocMgr)
{
    return S_OK;
}

STDAPI CTextService::OnUninitDocumentMgr(ITfDocumentMgr* pDocMgr)
{
    return S_OK;
}

STDAPI CTextService::OnSetFocus(ITfDocumentMgr* pDocMgrFocus, ITfDocumentMgr* pDocMgrPrevFocus)
{
    WIND_LOG_DEBUG_FMT(L"OnSetFocus called focus=0x%p prev=0x%p", pDocMgrFocus, pDocMgrPrevFocus);

    // 慢焦点探针：本函数在宿主 UI 线程上同步做 COM 查询（GetStatus / InputScope 的
    // TF_ES_SYNC 编辑会话 / GetCaretPosition）外加一次阻塞 IPC 往返——SendFocusGained
    // 内部是 send + ReceiveResponse，读超时 1500ms（见 IPCClient.cpp 与 IPCConfig）。
    // 焦点切换罕见时这个取舍是划算的（换来首键模式必然就绪），但宿主若高频 churn 焦点，
    // 每次都占住 UI 线程，WM_MOUSEMOVE 排其后 → 表现为鼠标光标卡顿。
    // 记 WARN 而非 DEBUG 的用意：WARN 恒进环形缓冲（见 WindLog::Output 的 ringWorthy），
    // 用户无需开启文件日志即可用 Ctrl+Shift+F11 导出证据——开 DEBUG 本身会加重卡顿，
    // 让被测对象因观测而改变。
    const LONGLONG focusProbeT0 = WindLog::PerfNow();
    double focusIpcMs = 0.0;

    _hasFocus = (pDocMgrFocus != nullptr);

    // If gaining focus (pDocMgrFocus is not null)
    if (pDocMgrFocus != nullptr)
    {
        _focusSessionId++;

        // ── 焦点抖动免疫：判据取「文档变没变」而非「失过焦没有」 ──
        // Excel 在 cell-select → cell-edit 时把**同一个** DocMgr 置空再设回（实测指针
        // 不变、间隔 6ms）；VSCode 一次应用切换伴随 5 次 DocMgr 焦点事件。DocMgr 级
        // 是噪声层，在失焦那一刻无从区分「抖动」与「真的换了文档」，因此不能在那里
        // 销毁输入态——那正是「Excel 首字符不进编码、直接上屏」的根因。
        // 把清理推迟到「另一个文档拿到焦点」时执行，抖动便自然被判为同一文档而跳过。
        // 同源做法见 Weasel（ThreadMgrEventSink.cpp）：DocMgr 级失焦完全不碰 composition。
        const BOOL isSameDocMgr = (_pLastActiveDocMgr != nullptr && pDocMgrFocus == _pLastActiveDocMgr);
        WIND_LOG_DEBUG_FMT(L"Focus gained focusSession=%llu sameDoc=%d doc=0x%p",
                           _focusSessionId, isSameDocMgr ? 1 : 0, pDocMgrFocus);

        if (!isSameDocMgr && _pLastActiveDocMgr != nullptr)
        {
            // 真的换了文档：在**旧** doc 上收口。传 hint 是必须的——此刻 GetFocus() 已指向
            // 新 doc，不传的话 EndComposition 会拿新 context 的 cookie 去清旧 context 的
            // range，轻则失败，重则动到新文档的内容。
            //
            // ⚠ 收口会发 focus_lost，而它**必须与随后的 focus_gained 配对**。下面的
            // XamlIsland locked 守卫会对 dynFlags&0x20 的新 DocMgr 跳过 focus_gained——
            // 两个决策各自都对，组合起来却让服务端只收到半边失焦：ime_active 被清掉后
            // 再没有东西恢复它，工具栏就此消失。实测 explorer 地址栏（2026-07-26）：点
            // 地址栏 → 换到 transient DocMgr → 发 lost、跳过 gained，用户停在该 DocMgr
            // 上正常打字，4 秒内再无任何 focus_gained（守卫旧注释断言的「后续稳定 DocMgr
            // 会补一个 gained」不成立）。
            // 故此处预判守卫是否将命中：会命中就不发这个 lost（焦点其实没离开本宿主的可
            // 输入上下文，只是换了个 transient 容器）。EndComposition 等本地清理照常做。
            // _DocMgrHasEditableContext 是纯查询（GetTop + GetStatus），提前问一次无副作用；
            // 只在真正换文档时多查一次，不在焦点热路径上。
            DWORD incomingDynFlags = 0;
            _DocMgrHasEditableContext(pDocMgrFocus, &incomingDynFlags);
            const BOOL willSkipFocusGained = (incomingDynFlags & kXamlIslandLockedFlag) != 0;
            CleanupInputStateForDocChange(_pLastActiveDocMgr, FOCUS_LOST_REASON_DOC_CHANGED,
                                          !willSkipFocusGained);
        }

        // Register ITfTextLayoutSink on the new context to receive
        // layout change notifications (for accurate candidate window positioning)
        ITfContext* pContext = nullptr;
        if (SUCCEEDED(pDocMgrFocus->GetTop(&pContext)) && pContext != nullptr)
        {
            _AdviseTextLayoutSink(pContext);
            _AdviseTextEditSink(pContext);
            pContext->Release();
        }

        // 这两行都在焦点热路径上：采集一次进程信息要 OpenProcess + 令牌查询 +
        // 映像路径 + GetWindowTextW。必须走带级别闸门的封装，不能裸调采集函数。
        WindLogCurrentProcessInfo(4, L"compat.focus.current_host");
        WindLogForegroundProcessInfo(4, L"compat.focus.foreground_host");

        // Force refresh the language bar button to ensure it's visible
        if (_pLangBarItemButton != nullptr)
        {
            _pLangBarItemButton->ForceRefresh();
        }

        // Reset composing state on focus gained to ensure clean state
        // This prevents stale composition state from affecting new input
        // 同一文档抖回来时**不**复位：_isComposing/_hasCandidates 一旦清零，
        // hasInputSession 即为假，紧接着的 Backspace / 空格 / 数字选字会被判为
        // 「无输入会话」而透传给宿主。上面 doc_changed 分支已负责真正换文档的复位。
        //
        // 配对状态同样在此复位（跨焦点保留已于 2026-07-29 放弃，理由见
        // CleanupInputStateForDocChange）。
        if (!isSameDocMgr && _pKeyEventSink != nullptr)
        {
            _pKeyEventSink->ResetComposingState();
        }

        // Detect whether the focused doc manager has a real editable context.
        // Use TSF context status flags (TF_SD_READONLY / TF_SS_TRANSITORY) rather than
        // GetTextExt: GetTextExt is a layout API and is not implemented by many frameworks
        // (JetBrains/Java Swing). Chrome marks its "no text field" context as TF_SD_READONLY,
        // which is the correct TSF-standard signal for "no writable text input".
        DWORD docMgrDynFlags = 0;
        _hasTextInputContext = _DocMgrHasEditableContext(pDocMgrFocus, &docMgrDynFlags);
        WIND_LOG_DEBUG_FMT(L"OnSetFocus: hasTextCtx=%d focusSession=%llu", _hasTextInputContext, _focusSessionId);

        // 读取焦点控件的 InputScope（密码框/邮箱/URL 等语义），随 focus_gained 上报给 Go 决策。
        // 仅对真正有可编辑上下文的文档查询，避免对无文本控件的 DocMgr 多跑一次同步读锁。
        UINT64 inputScopeMask = _hasTextInputContext ? _QueryInputScopeMask(pDocMgrFocus) : 0;

        // 密码框信号（Weasel/小狼毫做法）：宿主在 **context** 上置 GUID_COMPARTMENT_KEYBOARD_DISABLED
        // 表示"此控件禁用输入法"。Chromium 系浏览器密码框会置位，而无痕模式普通可编辑框不会，
        // 因此能精确区分密码框与隐私字段。
        // ⚠ 这是 context 级，与线程级的 `_bKeyboardDisabled` 是**两回事**：后者才是 OnTestKeyDown
        // 开头全放行的依据，网页密码框并不会置它。因此 context 级命中时键仍会被 DLL 吃下，
        // 必须靠补 IS_PASSWORD 位驱动抑制门控（IsPasswordSuppressActive / core apply_input_diag）
        // 来强制英文——mask 是这条信号唯一的出口，勿删。
        // InputScope 原始位（IS_PRIVATE/IS_SEARCH 等）仍随 mask 上报，留作将来扩展判断。
        // 密码框判据复用到加词热键门卫：密码框（中文已被抑制）不注册加词热键，缩小抢占面。
        _focusIsPassword = (_hasTextInputContext && _IsFocusKeyboardDisabled(pDocMgrFocus)) != FALSE;
        if (_focusIsPassword)
            inputScopeMask |= kScopeBitPassword;
        // 自留一份：IsPasswordSuppressActive 的吃键门控须在 OnTestKeyDown 本地算出（早于 IPC）。
        _focusInputScopeMask = inputScopeMask;

        // Get caret position for toolbar placement (separate concern from _hasTextInputContext)
        LONG caretX = 0, caretY = 0, caretHeight = 0;
        if (!GetCaretPosition(&caretX, &caretY, &caretHeight) && _hasLastKnownCaretPos)
        {
            caretX = _lastKnownCaretX;
            caretY = _lastKnownCaretY;
            caretHeight = _lastKnownCaretHeight;
            WIND_LOG_INFO_FMT(L"OnSetFocus: using last known caret position x=%ld y=%ld h=%ld", caretX, caretY, caretHeight);
        }
        WIND_LOG_DEBUG_FMT(
            L"compat.focus.caret focusSession=%llu x=%ld y=%ld height=%ld",
            _focusSessionId, caretX, caretY, caretHeight
        );
        _lastFocusCaretX = caretX;
        _lastFocusCaretY = caretY;
        _lastFocusCaretHeight = caretHeight > 0 ? caretHeight : DEFAULT_CARET_HEIGHT;

        // XamlIsland/transient locked DocMgr guard: dynFlags=0x20 consistently
        // marks Explorer's XamlIsland container DocMgrs where RequestEditSession
        // returns TF_E_NOLOCK. Sending focus_gained would cause the service to replay
        // composition into this unstable DocMgr; when the user then clicks away
        // the composition text is committed at screen position (0,0).
        // Skip focus_gained for these DocMgrs.
        //
        // ⚠ 旧注释断言「the subsequent stable DocMgr focus_gained will arrive」——**实测
        // 不成立**：explorer 地址栏点击后用户就停在这个 transient DocMgr 上正常打字，
        // 4 秒内再无第二个 focus_gained（2026-07-26 实测）。依赖它补配对是错的，因此
        // 上面的 doc_changed 收口会预判本守卫是否命中、命中则不发 focus_lost。
        // **改动本守卫的命中条件时，必须同步那一处的预判。**
        if (docMgrDynFlags & kXamlIslandLockedFlag)
        {
            WIND_LOG_INFO_FMT(
                L"OnSetFocus: skipping focus_gained for locked/transient DocMgr dynFlags=0x%X focusSession=%llu",
                docMgrDynFlags, _focusSessionId);
            // Fall through — sinks and LangBar are already set up above.
            // Do not send focus_gained IPC.
        }
        // No editable context (QQ Ctrl+1 切会话场景等)：新 DocMgr 没有任何可输入的
        // 文本控件 (_DocMgrHasEditableCtx -> 0)。发 focus_gained 会让 Go 把上一次
        // composition 状态 replay 回来 (UpdateComposition with residual buffer)，
        // 而 QQ 这边根本没地方接，结果是 IME 候选框残留、Go 内部 buffer 滞留。
        // 显式发 focus_lost 让 Go 强制清空 (clearState + hideUI)。
        else if (!_hasTextInputContext)
        {
            WIND_LOG_INFO_FMT(
                L"OnSetFocus: new DocMgr has no editable context, sending focus_lost focusSession=%llu",
                _focusSessionId);
            if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
            {
                // 同样计入 focusIpcMs：无可编辑上下文的宿主（CAD 绘图区、浏览器非输入区）
                // 走的是本分支而非 focus_gained，不计就会在最该测的场景里恒报 0。
                // SendFocusLost 是 async 写（无响应等待），但写超时仍有 300ms。
                const LONGLONG lostT0 = WindLog::PerfNow();
                // NO_EDIT_CTX：新文档确实没有可输入的地方，残留 buffer 无处可去必须清，
                // 工具栏也该隐藏；但宿主还在前台、输入法仍激活，故不动 ime_active。
                _pIPCClient->SendFocusLost(FOCUS_LOST_REASON_NO_EDIT_CTX);
                _focusLostSent = TRUE;
                _editCtxReported = FALSE; // 已告知服务端"没有可编辑上下文"，勿再补 CTX_LOST
                focusIpcMs += WindLog::PerfMsSince(lostT0);
            }
            _needsFocusRecovery = FALSE;
        }
        // ⚠ SendFocusGained 是**同步**的（send + ReceiveResponse，读超时 1500ms），
        // 不是 fire-and-forget——此处旧注释曾如此描述，与实现不符，已更正。
        // 同步是有意为之：Go 在响应里回传权威模式，使首个 OnTestKeyDown 之前模式必然
        // 就绪，根治「切过来首键上屏英文」（见 IPCClient::SendFocusGained 注释）。
        // 代价是每次焦点切换都在宿主 UI 线程上做一次 IPC 往返；重型 HandleFocusGained
        // 仍由 Go 在写响应之后异步执行，再经 push pipe 推 CMD_ACTIVATION_STATUS_PUSH,
        // AsyncReader → WM_ACTIVATION_STATUS → ApplyActivationStatusResponse 完成同步。
        // Lazy connect: 服务在 TSF 加载之后才启动也能覆盖（SendFocusGained 内部已处理）。
        else if (_pIPCClient != nullptr)
        {
            // 上报的 disabled 字段统一为**线程级** KEYBOARD_DISABLED（与 OnChange 里的
            // SendInputStateReport 同源），语义＝「系统禁用了输入法，DLL 已全放行」。
            // 密码框（context 级）不走这个字段，它已折进 mask 的 IS_PASSWORD 位——此前这里
            // 传 _focusIsPassword，让 core 把「密码框」误读成「键已放行」，抑制被自我否决。
            uint8_t inputReason = ComputeInputReason(_bKeyboardDisabled != FALSE, inputScopeMask);
            // 单独计时这一段：它是本函数里唯一会阻塞在别的进程上的调用，
            // 需要能和 COM/日志开销分开归因。
            const LONGLONG focusIpcT0 = WindLog::PerfNow();
            const BOOL focusSent = _pIPCClient->SendFocusGained(
                caretX, caretY, caretHeight, inputScopeMask, _bKeyboardDisabled != FALSE, inputReason);
            focusIpcMs += WindLog::PerfMsSince(focusIpcT0);
            if (focusSent)
            {
                WIND_LOG_DEBUG_FMT(L"FocusGained sent (sync) focusSession=%llu ipc=%.1fms",
                                   _focusSessionId, focusIpcMs);
                _needsFocusRecovery = FALSE;
                _focusLostSent = FALSE; // 新会话开始，下次离开文档时须再发一次 focus_lost
                // 服务端此刻已知「焦点在可编辑控件里」。置位后，焦点离开时
                // _ReportEditContextLost 才会在翻转沿补一条 CTX_LOST。
                _editCtxReported = TRUE;
                _pIPCClient->ClearNeedsSyncFlag();
            }
            else
            {
                WIND_LOG_WARN_FMT(L"FocusGained IPC send failed focusSession=%llu", _focusSessionId);
                _needsFocusRecovery = TRUE;
            }
        }

        // 记住本次活跃文档，供下次 OnSetFocus 比对。**必须 AddRef 保活**：仅存裸指针的话，
        // 旧 DocMgr 释放后新对象可能落在同一地址，"换了文档"会被误判成抖动而漏清理。
        // locked/transient（XamlIsland）不入缓存——上面已对其跳过 focus_gained 视作非事件，
        // 若缓存它，紧随其后的真实文档就会被判成"换了文档"，反而清掉刚输入的内容。
        if (!(docMgrDynFlags & kXamlIslandLockedFlag) && _pLastActiveDocMgr != pDocMgrFocus)
        {
            if (_pLastActiveDocMgr != nullptr)
                _pLastActiveDocMgr->Release();
            _pLastActiveDocMgr = pDocMgrFocus;
            _pLastActiveDocMgr->AddRef();
        }
    }

    // If losing focus (pDocMgrFocus is null)
    if (pDocMgrFocus == nullptr)
    {
        WIND_LOG_DEBUG_FMT(L"DocMgr focus lost focusSession=%llu (ctx_lost only; 输入态清理仍延后到 OnKillThreadFocus)",
                           _focusSessionId);

        if (_pKeyEventSink != nullptr)
        {
            _pKeyEventSink->FlushEnglishStats();
        }

        // ⚠ 这里**刻意不做**结束 composition / 清输入态 / 复位会话态这三件事。
        // 曾经做过，那正是「Excel 首字符不进编码、直接上屏」的根因：Excel 在
        // cell-select → cell-edit 时把同一个 DocMgr 置空再设回（实测指针不变、间隔 6ms），
        // 在此销毁输入态就把用户刚敲的首字符连同 composition 一起清掉了。
        // DocMgr 级失焦是噪声信号（VSCode 实测一次应用切换伴随 5 次 DocMgr 焦点事件），
        // 且在这一刻无从区分「抖动」与「真的换了文档」。
        //
        // 输入态清理仍由两条能分辨真伪的路径承担：
        //   1. 另一个文档拿到焦点   → OnSetFocus 的 doc_changed 分支（本函数上半部分）
        //   2. 整个应用失去前台     → OnKillThreadFocus（实测 Chrome/VSCode/Edge 各
        //      5/5、5/5、11/11 次触发，零漏；仅比本回调晚约 100ms）
        // 同源做法见 Weasel ThreadMgrEventSink.cpp（其 issue #185 就是同一个 Excel bug）。
        //
        // **但工具栏可见性不同**：它不需要"分辨真伪"，因为翻错了也只是闪一下，UI 层
        // 50ms 隐藏防抖会吸收，而漏报的代价是应用内点到非文本框后工具栏永不隐藏（实测
        // LogExpert / 文件管理器，2026-07-26）。故这里补一条 CTX_LOST——它只翻可见性
        // 标志、不碰输入态，是唯一能安全放在噪声层的通知。
        _ReportEditContextLost();

        // Unregister layout sink when losing focus
        _UnadviseTextLayoutSink();
        _UnadviseTextEditSink();

        // 失焦时**不**销毁 HostWindow（见 OnKillThreadFocus 注释）：locked/transient
        // DocMgr 会跳过 focus_gained，销毁后无法重建。靠 Go 的 WriteHide 隐藏即可。

        _needsFocusRecovery = FALSE;

        // 离开文本框：清门卫状态，下方 reeval 会注销加词热键，把 Ctrl+= 还给宿主。
        _hasTextInputContext = FALSE;
        _focusIsPassword = false;
        // 掩码随焦点走：不清会把上个控件的密码位带到新焦点，令抑制门控误放行。
        _focusInputScopeMask = 0;
    }

    // 焦点/文本框上下文变化后重新评估加词热键（gaining/losing 两分支汇合于此）。
    _ReevaluateAddWordHotkey();

    // 慢焦点探针收口（gaining/losing 两分支都经过这里）。见函数开头的说明。
    const double focusTotalMs = WindLog::PerfMsSince(focusProbeT0);
    if (focusTotalMs >= kSlowFocusWarnMs)
    {
        // hasTextCtx 决定走 focus_gained(同步往返) 还是 focus_lost(异步写)，
        // 是归因的关键分支位——CAD 绘图区一类宿主预期为 0。
        WIND_LOG_WARN_FMT(
            L"perf.focus.slow total=%.1fms ipc=%.1fms focusSession=%llu gaining=%d hasTextCtx=%d",
            focusTotalMs, focusIpcMs, _focusSessionId,
            pDocMgrFocus != nullptr ? 1 : 0, _hasTextInputContext ? 1 : 0);
    }

    return S_OK;
}

STDAPI CTextService::OnPushContext(ITfContext* pContext)
{
    return S_OK;
}

STDAPI CTextService::OnPopContext(ITfContext* pContext)
{
    return S_OK;
}

BOOL CTextService::_InitKeyEventSink()
{
    _pKeyEventSink = new CKeyEventSink(this);
    if (_pKeyEventSink == nullptr)
        return FALSE;

    return _pKeyEventSink->Initialize();
}

void CTextService::_UninitKeyEventSink()
{
    if (_pKeyEventSink != nullptr)
    {
        _pKeyEventSink->Uninitialize();
        _pKeyEventSink->Release();
        _pKeyEventSink = nullptr;
    }
}

// ============================================================================
// State sync helpers
// ============================================================================

void CTextService::_SyncStateFromResponse(const ServiceResponse& response)
{
    if (response.type != ResponseType::StatusUpdate)
        return;

    _SetChineseMode(response.IsChineseMode());
    _bFullWidth = response.IsFullWidth();

    // Keep compartment always OPEN so TSF calls OnTestKeyDown even in English mode.
    _SetOpenCloseCompartment(TRUE);
    // Sync真实中英文模式到 INPUTMODE_CONVERSION compartment（供 KBLSwitch / 任务栏读取）
    _SetConversionMode(_bChineseMode);

    // Sync full status to LangBarItemButton
    if (_pLangBarItemButton != nullptr)
    {
        BOOL bCapsLock = (GetKeyState(VK_CAPITAL) & 0x0001) != 0;
        _pLangBarItemButton->UpdateFullStatus(
            response.IsChineseMode(),
            response.IsFullWidth(),
            response.IsChinesePunct(),
            response.IsToolbarVisible(),
            bCapsLock,
            response.iconLabel.empty() ? nullptr : response.iconLabel.c_str()
        );
    }

    // Update hotkey whitelist if present
    if (response.HasHotkeys() && _pHotkeyManager != nullptr)
    {
        WIND_LOG_DEBUG(L"Updating hotkey whitelist from state sync\n");
        _pHotkeyManager->UpdateHotkeys(
            response.keyDownHotkeys,
            response.keyUpHotkeys
        );
    }

    WIND_LOG_INFO_FMT(L"State synced: mode=%d, width=%d, punct=%d, toolbar=%d, hostRender=%d\n",
        response.IsChineseMode(), response.IsFullWidth(),
        response.IsChinesePunct(), response.IsToolbarVisible(), response.IsHostRenderAvailable());
}

void CTextService::_DestroyHostWindow()
{
    // Destroy non-candidate (owned) windows first, then the candidate (owner) last, so
    // owned tooltip/status windows are torn down before their z-order owner disappears.
    for (int k = HOST_WINDOW_KIND_COUNT - 1; k >= 0; --k)
    {
        if (_pHostWindow[k] != nullptr)
        {
            _pHostWindow[k]->Uninitialize();
            delete _pHostWindow[k];
            _pHostWindow[k] = nullptr;
        }
    }
}

void CTextService::_EnsureHostRenderSetup(const ServiceResponse& response, BOOL forceRefresh)
{
    if (_pIPCClient == nullptr || !_pIPCClient->IsConnected())
        return;

    CHostWindow*& candidate = _pHostWindow[HOST_WINDOW_CANDIDATE];
    BOOL hadHostWindow = (candidate != nullptr);
    BOOL hostRenderAvailable = response.IsHostRenderAvailable();
    BOOL shouldRetryExistingHost = forceRefresh && hadHostWindow && !hostRenderAvailable;

    if (!hostRenderAvailable && !shouldRetryExistingHost)
    {
        if (forceRefresh && hadHostWindow)
        {
            WIND_LOG_INFO(L"Host render unavailable after refresh, disabling existing host windows\n");
            _DestroyHostWindow();
        }
        return;
    }

    if (shouldRetryExistingHost)
    {
        WIND_LOG_WARN(L"Host render flag missing after reconnect, retrying setup because host window was previously active\n");
    }

    if (candidate != nullptr && !forceRefresh)
    {
        // Check if the host's band has changed (e.g., user switched from Start Menu
        // search band=6 to taskbar search band=13). Recreate ALL host windows rather
        // than UpdateBand on each: the tooltip/status windows are owned by the candidate
        // hwnd for z-order, so recreating the candidate alone would leave them pointing
        // at a destroyed owner. Full re-setup keeps ownership consistent.
        DWORD currentHostBand = candidate->GetHostBand();
        if (currentHostBand > 1 && currentHostBand != candidate->GetCurrentBand())
        {
            WIND_LOG_INFO_FMT(L"Host band changed to %u, recreating all host windows\n", currentHostBand);
            _DestroyHostWindow();
            // fall through to recreate at the new band
        }
        else
        {
            return; // no change needed
        }
    }
    else if (candidate != nullptr)
    {
        WIND_LOG_INFO(L"Refreshing host render windows after service reconnection\n");
        _DestroyHostWindow();
    }

    WIND_LOG_INFO(L"Host render available, requesting setup\n");

    ServiceResponse hrResponse;
    if (_pIPCClient->SendHostRenderRequest(hrResponse) &&
        hrResponse.type == ResponseType::HostRenderSetup &&
        !hrResponse.hostRenderSetups.empty())
    {
        // Create the candidate window first (pass 0) so its hwnd can serve as the z-order
        // owner for the tooltip/status windows (pass 1) — owned windows always sit above
        // their owner, so the tooltip never gets occluded by the candidate band window.
        HWND candidateOwner = NULL;
        for (int pass = 0; pass < 2; ++pass)
        {
            for (size_t i = 0; i < hrResponse.hostRenderSetups.size(); ++i)
            {
                const HostRenderSetupInfo& info = hrResponse.hostRenderSetups[i];
                bool isCandidate = (info.windowKind == HOST_WINDOW_CANDIDATE);
                if ((pass == 0) != isCandidate)
                    continue; // pass 0: candidate only; pass 1: the rest
                if (info.windowKind >= HOST_WINDOW_KIND_COUNT)
                    continue;
                if (info.shmName.empty() || info.eventName.empty())
                    continue;
                if (_pHostWindow[info.windowKind] != nullptr)
                    continue; // already created (defensive against duplicate entries)

                CHostWindow* win = new CHostWindow();
                HWND owner = isCandidate ? NULL : candidateOwner; // others owned by candidate
                if (!win->Initialize(
                        info.shmName.c_str(),
                        info.eventName.c_str(),
                        info.maxBufferSize,
                        hrResponse.hostRenderInstanceId, // stamp frames' target against this
                        _pIPCClient, // weak ref: candidate routes mouse events back to Go
                        (HostWindowKind)info.windowKind,
                        owner))
                {
                    WIND_LOG_WARN_FMT(L"Host window kind=%u init failed, skipping\n", info.windowKind);
                    delete win;
                    continue;
                }
                _pHostWindow[info.windowKind] = win;
                if (isCandidate)
                    candidateOwner = win->GetHwnd();
            }
        }

        if (_pHostWindow[HOST_WINDOW_CANDIDATE] != nullptr)
        {
            WIND_LOG_INFO(L"Host windows initialized successfully\n");
        }
        else
        {
            WIND_LOG_WARN(L"Candidate host window missing after setup, falling back to Go window\n");
            // Tell Go so it can log centrally + notify the user the candidate fell back to its
            // local window (e.g. restricted UWP hosts where even band=0 CreateWindowInBand fails).
            uint32_t reason = HOST_RENDER_FAIL_WINDOW_CREATE;
            _pIPCClient->SendAsync(CMD_HOST_RENDER_FAILED, &reason, sizeof(reason));
        }
    }
    else
    {
        WIND_LOG_WARN(L"Host render setup request failed, falling back to Go window\n");
        uint32_t reason = HOST_RENDER_FAIL_WINDOW_CREATE;
        _pIPCClient->SendAsync(CMD_HOST_RENDER_FAILED, &reason, sizeof(reason));
    }
}

// ============================================================================
// Compartment event sink for GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
// ============================================================================

BOOL CTextService::_InitOpenCloseCompartment()
{
    if (_pThreadMgr == nullptr)
        return FALSE;

    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
    {
        WIND_LOG_ERROR(L"Failed to get ITfCompartmentMgr from ThreadMgr\n");
        return FALSE;
    }

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
    {
        WIND_LOG_ERROR(L"Failed to get GUID_COMPARTMENT_KEYBOARD_OPENCLOSE compartment\n");
        return FALSE;
    }

    // Set initial state to open (Chinese mode)
    VARIANT var;
    var.vt = VT_I4;
    var.lVal = TRUE;
    pCompartment->SetValue(_tfClientId, &var);

    // Advise for changes
    ITfSource* pSource = nullptr;
    hr = pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource);
    pCompartment->Release();

    if (FAILED(hr) || pSource == nullptr)
    {
        WIND_LOG_ERROR(L"Failed to get ITfSource from compartment\n");
        return FALSE;
    }

    hr = pSource->AdviseSink(IID_ITfCompartmentEventSink, (ITfCompartmentEventSink*)this, &_dwOpenCloseSinkCookie);
    pSource->Release();

    if (FAILED(hr))
    {
        WIND_LOG_ERROR(L"Failed to advise compartment event sink\n");
        _dwOpenCloseSinkCookie = TF_INVALID_COOKIE;
        return FALSE;
    }

    WIND_LOG_DEBUG(L"Compartment OPENCLOSE sink advised successfully\n");
    return TRUE;
}

void CTextService::_UninitOpenCloseCompartment()
{
    if (_dwOpenCloseSinkCookie == TF_INVALID_COOKIE || _pThreadMgr == nullptr)
        return;

    ITfCompartmentMgr* pCompMgr = nullptr;
    if (SUCCEEDED(_pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr)) && pCompMgr != nullptr)
    {
        ITfCompartment* pCompartment = nullptr;
        if (SUCCEEDED(pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, &pCompartment)) && pCompartment != nullptr)
        {
            ITfSource* pSource = nullptr;
            if (SUCCEEDED(pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
            {
                pSource->UnadviseSink(_dwOpenCloseSinkCookie);
                pSource->Release();
            }
            pCompartment->Release();
        }
        pCompMgr->Release();
    }

    _dwOpenCloseSinkCookie = TF_INVALID_COOKIE;
    WIND_LOG_DEBUG(L"Compartment OPENCLOSE sink unadvised\n");
}

BOOL CTextService::_SetOpenCloseCompartment(BOOL bOpen)
{
    if (_pThreadMgr == nullptr)
        return FALSE;

    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
        return FALSE;

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
        return FALSE;

    // 值没变就别写。ITfCompartment::SetValue **不做值比较**——写入相同的值同样会向
    // 宿主和所有已 advise 的 sink 广播 OnChange，而 OPENCLOSE 是线程级全局 compartment，
    // 语义是「输入法开/关状态变了」，宿主据此重建输入状态是合理反应。
    //
    // 为什么要紧：_SyncStateFromResponse 每收到一次服务端状态推送就调用本函数一次，
    // 而推送是广播给所有已加载本 DLL 的进程的（实测同一份日志里有 17 个 PID）。
    // 于是任何一个应用里的状态变化，都会让每一个宿主收到一次「IME 开关变了」的通知。
    // 参见 295350e「抑制 CapsLock 联动触发的 OPENCLOSE 变化，防模式振荡」——
    // 同一类问题此前已出现过一次。
    //
    // 姊妹函数 _SetConversionMode 早有同样的守卫（注释写着「避免触发多余 OnChange」），
    // 本函数漏了——两个 compartment 一个防了一个没防。
    const LONG desired = bOpen ? TRUE : FALSE;
    VARIANT cur;
    VariantInit(&cur);
    if (SUCCEEDED(pCompartment->GetValue(&cur)) && cur.vt == VT_I4 && cur.lVal == desired)
    {
        pCompartment->Release();
        return TRUE;
    }

    // Set guard to prevent re-entrant OnChange
    _bInCompartmentChange = TRUE;

    VARIANT var;
    var.vt = VT_I4;
    var.lVal = desired;
    hr = pCompartment->SetValue(_tfClientId, &var);
    pCompartment->Release();

    _bInCompartmentChange = FALSE;

    return SUCCEEDED(hr);
}

STDAPI CTextService::OnChange(REFGUID rguid)
{
    if (_pThreadMgr == nullptr)
        return S_OK;

    // ================================================================
    // GUID_COMPARTMENT_KEYBOARD_DISABLED
    // ================================================================
    if (IsEqualGUID(rguid, GUID_COMPARTMENT_KEYBOARD_DISABLED))
    {
        ITfCompartmentMgr* pCompMgr = nullptr;
        HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
        if (FAILED(hr) || pCompMgr == nullptr)
            return S_OK;

        ITfCompartment* pCompartment = nullptr;
        hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_DISABLED, &pCompartment);
        pCompMgr->Release();

        if (FAILED(hr) || pCompartment == nullptr)
            return S_OK;

        VARIANT var;
        VariantInit(&var);
        hr = pCompartment->GetValue(&var);
        pCompartment->Release();

        if (FAILED(hr) || var.vt != VT_I4)
            return S_OK;

        BOOL bDisabled = (var.lVal != 0);
        if (_bKeyboardDisabled == bDisabled)
            return S_OK;

        _bKeyboardDisabled = bDisabled;

        WIND_LOG_INFO_FMT(L"Compartment KEYBOARD_DISABLED changed: %d\n", bDisabled);

        // End composition when keyboard becomes disabled
        if (bDisabled)
            EndComposition();

        // Update language bar to show disabled state
        if (_pLangBarItemButton != nullptr)
            _pLangBarItemButton->UpdateKeyboardDisabled(bDisabled);

        // 输入诊断 HUD（Task 7）：compartment 变更是"焦点未变但禁用态翻转"的场景（如 SPA 内
        // 原地跳转到密码框），不会触发新的 OnSetFocus/focus_gained，因此单独上报一次
        // input_state_report 让 HUD 即时刷新。mask 用当前焦点 DocMgr 重新查询（可能与
        // OnSetFocus 时不同一个 DocMgr），仅算一次避免重复同步读锁。
        if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
        {
            ITfDocumentMgr* pDocMgrCur = nullptr;
            UINT64 curMask = 0;
            if (SUCCEEDED(_pThreadMgr->GetFocus(&pDocMgrCur)) && pDocMgrCur != nullptr)
            {
                curMask = _QueryInputScopeMask(pDocMgrCur);
                pDocMgrCur->Release();
            }
            // 与 core 同步更新自留掩码：本路径是「焦点未变但禁用态翻转」（SPA 原地跳到
            // 密码框），不走 OnSetFocus，不更新则抑制门控会一直用旧焦点的掩码。
            _focusInputScopeMask = curMask;
            uint8_t curReason = ComputeInputReason(bDisabled != FALSE, curMask);
            _pIPCClient->SendInputStateReport(GetCurrentProcessId(), bDisabled != FALSE, curReason, curMask);
        }

        return S_OK;
    }

    // ================================================================
    // GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION
    //
    // 外部工具（如 KBLSwitch 按应用锁定中英文）会写入此 compartment。
    // 我们读取 IME_CMODE_NATIVE 位并按需切换内部模式，使外部锁定生效。
    // ================================================================
    if (IsEqualGUID(rguid, GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION))
    {
        if (_bInConversionChange)
            return S_OK;  // 自身写入引起的通知，跳过

        // 无焦点时的 compartment 变化只能是系统切换输入法等噪声（用户 Ctrl+Space 必有
        // 前台焦点；KBLSwitch 也按前台应用写入）——忽略，防止污染服务端权威模式。
        // 下次聚焦/激活会从服务同步权威值。
        // 判据必须是 _hasThreadFocus（ITfThreadFocusSink，「本应用在前台」），
        // 不是 _hasFocus（DocMgr 级焦点，OnSetFocus 收到非 null 即置位）。
        // 上面那段注释的意图本来就是前台——「用户 Ctrl+Space 必有前台焦点；KBLSwitch
        // 也按前台应用写入」——只是取错了变量。实测两个方向都会出错：
        //   hasFocus=1 hasThreadFocus=0（最小化的记事本仍残留 DocMgr 焦点）
        //     → 把用户操作**别的输入法**（微软五笔）引起的线程级 compartment 变化
        //       误当成用户操作上报 → handle_system_mode_switch 覆盖全局 chinese_mode
        //       → 广播给所有 client → 切回来的应用被带成英文。
        //   hasFocus=0 hasThreadFocus=1（本应用在前台但 DocMgr 焦点尚未建立）
        //     → 把用户的真实切换当噪声丢弃。
        WIND_LOG_INFO_FMT(L"compat.conversion.onchange hasFocus=%d hasThreadFocus=%d curMode=%d\n",
                          _hasFocus ? 1 : 0, _hasThreadFocus ? 1 : 0, _bChineseMode ? 1 : 0);
        if (!_hasThreadFocus)
        {
            WIND_LOG_INFO(L"Compartment CONVERSION changed while not foreground, ignored\n");
            return S_OK;
        }
        // 激活静默期：刚 ActivateEx 完时系统会写 compartment 做初始化同步（实测 ~96ms），
        // 那不是用户操作。此前这类噪声靠 _hasFocus 尚未置位被顺带挡住，改用
        // _hasThreadFocus 后不再被挡（激活时本应用正是前台），故显式加窗口。
        if (GetTickCount64() - _lastActivateTick < kActivateSettleMs)
        {
            WIND_LOG_INFO(L"Compartment CONVERSION changed within activate settle window, ignored\n");
            return S_OK;
        }

        ITfCompartmentMgr* pCompMgr = nullptr;
        HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
        if (FAILED(hr) || pCompMgr == nullptr)
            return S_OK;

        ITfCompartment* pCompartment = nullptr;
        hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, &pCompartment);
        pCompMgr->Release();

        if (FAILED(hr) || pCompartment == nullptr)
            return S_OK;

        VARIANT var;
        VariantInit(&var);
        hr = pCompartment->GetValue(&var);
        pCompartment->Release();

        if (FAILED(hr) || var.vt != VT_I4)
            return S_OK;

        BOOL bWantChinese = ((DWORD)var.lVal & IME_CMODE_NATIVE) ? TRUE : FALSE;
        if (bWantChinese == _bChineseMode)
            return S_OK;  // 与当前一致，无需切换

        WIND_LOG_INFO_FMT(L"Compartment CONVERSION changed externally: %s -> %s\n",
            _bChineseMode ? L"Chinese" : L"English",
            bWantChinese ? L"Chinese" : L"English");

        // 与 OPENCLOSE 路径一致：清状态、通知 Go 服务、刷新 LangBar。
        if (_pKeyEventSink != nullptr)
            _pKeyEventSink->FlushEnglishStats();

        EndComposition();
        ResetComposingState(TRUE);  // 中英切换保留配对状态：光标与已插入的右符号都没变

        BOOL newChineseMode = bWantChinese;
        if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
        {
            ServiceResponse response;
            if (_pIPCClient->SendSystemModeSwitch(newChineseMode != FALSE, response))
            {
                if (response.type == ResponseType::CommitText && !response.text.empty())
                    CommitText(response.text);
                if (response.type == ResponseType::StatusUpdate || response.type == ResponseType::CommitText)
                    newChineseMode = response.IsChineseMode() ? TRUE : FALSE;
            }
        }

        _SetChineseMode(newChineseMode);
    
        if (_pLangBarItemButton != nullptr)
            _pLangBarItemButton->UpdateLangBarButton(_bChineseMode);

        // 若 Go 服务把模式仲裁成了与外部请求不同的值，回写 compartment 保持一致。
        if (newChineseMode != bWantChinese)
            _SetConversionMode(newChineseMode);

        return S_OK;
    }

    // ================================================================
    // GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
    // ================================================================
    if (!IsEqualGUID(rguid, GUID_COMPARTMENT_KEYBOARD_OPENCLOSE))
        return S_OK;

    // Avoid re-entrant handling when we set the compartment ourselves
    if (_bInCompartmentChange)
        return S_OK;

    // 无焦点时的 OPENCLOSE 变化是系统切换输入法等噪声（本路径任何变化都被视为 toggle，
    // 误报会直接翻转服务端权威模式）——忽略，下次聚焦/激活从服务同步权威值。
    // 同 CONVERSION 路径：判据须为 _hasThreadFocus（前台），而非 _hasFocus（DocMgr 级）。
    // 本路径的误报后果更重——「任何变化都被视为 toggle」，直接翻转服务端权威模式。
    WIND_LOG_INFO_FMT(L"compat.openclose.onchange hasFocus=%d hasThreadFocus=%d curMode=%d\n",
                      _hasFocus ? 1 : 0, _hasThreadFocus ? 1 : 0, _bChineseMode ? 1 : 0);
    if (!_hasThreadFocus)
    {
        WIND_LOG_INFO(L"Compartment OPENCLOSE changed while not foreground, ignored\n");
        return S_OK;
    }
    // 激活静默期，理由同 CONVERSION 路径。本路径误报后果更重：任何变化都被当作 toggle。
    if (GetTickCount64() - _lastActivateTick < kActivateSettleMs)
    {
        WIND_LOG_INFO(L"Compartment OPENCLOSE changed within activate settle window, ignored\n");
        return S_OK;
    }

    // Read current compartment value
    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
        return S_OK;

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
        return S_OK;

    VARIANT var;
    VariantInit(&var);
    hr = pCompartment->GetValue(&var);
    pCompartment->Release();

    if (FAILED(hr) || var.vt != VT_I4)
        return S_OK;

    BOOL bOpen = (var.lVal != 0);
    WIND_LOG_INFO_FMT(L"Compartment OPENCLOSE changed: %d (current mode: %s)\n",
        bOpen, _bChineseMode ? L"Chinese" : L"English");

    // CapsLock 联动噪声抑制：Windows 输入系统在 CapsLock 状态变化（物理按键或服务端
    // cancel_on_mode_switch 的注入取消）后会异步联动写 OPENCLOSE compartment（实测
    // 延迟 0.5~1s）。这不是用户的 Ctrl+Space 切换请求——若照常 toggle 并上报服务，
    // 会与服务端的 CapsLock 注入形成「注入→联动→切换→再注入」振荡回路（模式每拍
    // 翻转、大写灯乱闪）。短窗口内只把 compartment 拉回 OPEN，不 toggle 不上报。
    //
    // 例外（勿删）：用户「开大写后立刻按 Ctrl+Space」的真实切换与系统联动落在同一
    // 时间窗（都是 caps 活动后 0.5~1s），纯时间窗口会误吞真实请求（实测复现）。
    // 判据：系统热键触发的 OPENCLOSE 变化发生在 Ctrl 按住期间（Space down 即触发，
    // Ctrl 尚未释放）；系统的 CapsLock 联动写入则无任何伴随按键。Ctrl 按住 → 放行。
    if (GetTickCount64() - _lastCapsKeyTick < 1500)
    {
        BOOL ctrlHeld = (GetAsyncKeyState(VK_CONTROL) & 0x8000) || (GetKeyState(VK_CONTROL) & 0x8000);
        if (!ctrlHeld)
        {
            WIND_LOG_INFO(L"Compartment OPENCLOSE changed right after CapsLock activity (no Ctrl held), suppressed (re-open only)\n");
            _bInCompartmentChange = TRUE;
            _SetOpenCloseCompartment(TRUE);
            _bInCompartmentChange = FALSE;
            return S_OK;
        }
        WIND_LOG_INFO(L"Compartment OPENCLOSE changed within CapsLock window but Ctrl held -> treat as user toggle\n");
    }

    // The system alternates compartment between 0 and 1 each Ctrl+Space press.
    // We always re-open to 1 after handling, but _SetOpenCloseCompartment may fail
    // inside OnChange (TSF reentrancy), leaving compartment at 0. Either way the
    // system fires bOpen=TRUE on the next press and bOpen=FALSE on the one after.
    // We must handle BOTH directions as mode toggle events.
    //
    // Guard: _bInCompartmentChange is set TRUE before we call _SetOpenCloseCompartment(TRUE),
    // preventing a synchronous re-entrant OnChange from causing a spurious extra toggle.

    BOOL newChineseMode = !_bChineseMode;

    WIND_LOG_INFO_FMT(L"Compartment %s: toggling mode %s -> %s\n",
        bOpen ? L"opened" : L"closed",
        _bChineseMode ? L"Chinese" : L"English",
        newChineseMode ? L"Chinese" : L"English");

    // Flush English stats before any mode switch
    if (_pKeyEventSink != nullptr)
        _pKeyEventSink->FlushEnglishStats();

    // End any active composition since we're switching modes
    EndComposition();
    ResetComposingState(TRUE);  // 中英切换保留配对状态：光标与已插入的右符号都没变

    // Notify Go service of the mode switch (sync: may return CommitText for pending input)
    if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        ServiceResponse response;
        if (_pIPCClient->SendSystemModeSwitch(newChineseMode != FALSE, response))
        {
            if (response.type == ResponseType::CommitText && !response.text.empty())
            {
                CommitText(response.text);
                WIND_LOG_INFO_FMT(L"SystemModeSwitch: committed pending text (len=%zu)\n", response.text.size());
            }
            if (response.type == ResponseType::StatusUpdate || response.type == ResponseType::CommitText)
            {
                newChineseMode = response.IsChineseMode() ? TRUE : FALSE;
            }
        }
        else
        {
            WIND_LOG_WARN(L"SystemModeSwitch IPC failed, proceeding with local toggle\n");
        }
    }

    _SetChineseMode(newChineseMode);

    if (_pLangBarItemButton != nullptr)
        _pLangBarItemButton->UpdateLangBarButton(_bChineseMode);

    // Re-open compartment so TSF keeps calling OnTestKeyDown for English stats/auto-pair.
    // Set the re-entrance guard so that if SetValue fires synchronously, the resulting
    // OnChange(bOpen=TRUE) is suppressed and doesn't trigger a spurious extra toggle.
    _bInCompartmentChange = TRUE;
    _SetOpenCloseCompartment(TRUE);
    _bInCompartmentChange = FALSE;

    // 同步真实中英文模式到 INPUTMODE_CONVERSION（KBLSwitch / 任务栏读取此 compartment）
    _SetConversionMode(_bChineseMode);

    WIND_LOG_INFO_FMT(L"Mode toggled via system compartment -> %s (compartment kept open)\n",
        _bChineseMode ? L"Chinese" : L"English");

    return S_OK;
}

BOOL CTextService::_InitKeyboardDisabledCompartment()
{
    if (_pThreadMgr == nullptr)
        return FALSE;

    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
        return FALSE;

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_DISABLED, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
        return FALSE;

    // Read current value
    VARIANT var;
    VariantInit(&var);
    if (SUCCEEDED(pCompartment->GetValue(&var)) && var.vt == VT_I4)
        _bKeyboardDisabled = (var.lVal != 0);

    // Advise for changes
    ITfSource* pSource = nullptr;
    hr = pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource);
    pCompartment->Release();

    if (FAILED(hr) || pSource == nullptr)
        return FALSE;

    hr = pSource->AdviseSink(IID_ITfCompartmentEventSink, (ITfCompartmentEventSink*)this, &_dwKeyboardDisabledSinkCookie);
    pSource->Release();

    if (FAILED(hr))
    {
        _dwKeyboardDisabledSinkCookie = TF_INVALID_COOKIE;
        return FALSE;
    }

    WIND_LOG_DEBUG_FMT(L"Compartment KEYBOARD_DISABLED sink advised, current=%d\n", _bKeyboardDisabled);
    return TRUE;
}

void CTextService::_UninitKeyboardDisabledCompartment()
{
    if (_dwKeyboardDisabledSinkCookie == TF_INVALID_COOKIE || _pThreadMgr == nullptr)
        return;

    ITfCompartmentMgr* pCompMgr = nullptr;
    if (SUCCEEDED(_pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr)) && pCompMgr != nullptr)
    {
        ITfCompartment* pCompartment = nullptr;
        if (SUCCEEDED(pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_DISABLED, &pCompartment)) && pCompartment != nullptr)
        {
            ITfSource* pSource = nullptr;
            if (SUCCEEDED(pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
            {
                pSource->UnadviseSink(_dwKeyboardDisabledSinkCookie);
                pSource->Release();
            }
            pCompartment->Release();
        }
        pCompMgr->Release();
    }

    _dwKeyboardDisabledSinkCookie = TF_INVALID_COOKIE;
    WIND_LOG_DEBUG(L"Compartment KEYBOARD_DISABLED sink unadvised\n");
}

// ============================================================================
// Compartment event sink for GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION
//
// 此 compartment 是 Windows 标准的「中/英文模式」对外通信通道：
//   - IME_CMODE_NATIVE 位置 1：当前为本地（中文）输入
//   - IME_CMODE_NATIVE 位置 0：当前为字母（英文）输入
// 第三方工具（KBLSwitch 等按应用锁中英文）与 Win11 任务栏语言指示器都
// 读写此 compartment。OPENCLOSE 在内部约定下始终为 TRUE，不应承担模式信号。
// ============================================================================

// IME_CMODE_NATIVE from imm.h. 不引入 imm.h，避免拉入整个 IMM32 头文件。
#ifndef IME_CMODE_NATIVE
#define IME_CMODE_NATIVE 0x0001
#endif

BOOL CTextService::_InitConversionCompartment()
{
    if (_pThreadMgr == nullptr)
        return FALSE;

    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
        return FALSE;

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
        return FALSE;

    // Sync initial value to current internal mode.
    VARIANT var;
    var.vt = VT_I4;
    var.lVal = _bChineseMode ? IME_CMODE_NATIVE : 0;
    _bInConversionChange = TRUE;
    pCompartment->SetValue(_tfClientId, &var);
    _bInConversionChange = FALSE;

    ITfSource* pSource = nullptr;
    hr = pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource);
    pCompartment->Release();

    if (FAILED(hr) || pSource == nullptr)
        return FALSE;

    hr = pSource->AdviseSink(IID_ITfCompartmentEventSink, (ITfCompartmentEventSink*)this, &_dwConversionSinkCookie);
    pSource->Release();

    if (FAILED(hr))
    {
        _dwConversionSinkCookie = TF_INVALID_COOKIE;
        return FALSE;
    }

    WIND_LOG_DEBUG_FMT(L"Compartment INPUTMODE_CONVERSION sink advised, initial=%d\n", _bChineseMode);
    return TRUE;
}

void CTextService::_UninitConversionCompartment()
{
    if (_dwConversionSinkCookie == TF_INVALID_COOKIE || _pThreadMgr == nullptr)
        return;

    ITfCompartmentMgr* pCompMgr = nullptr;
    if (SUCCEEDED(_pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr)) && pCompMgr != nullptr)
    {
        ITfCompartment* pCompartment = nullptr;
        if (SUCCEEDED(pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, &pCompartment)) && pCompartment != nullptr)
        {
            ITfSource* pSource = nullptr;
            if (SUCCEEDED(pCompartment->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
            {
                pSource->UnadviseSink(_dwConversionSinkCookie);
                pSource->Release();
            }
            pCompartment->Release();
        }
        pCompMgr->Release();
    }

    _dwConversionSinkCookie = TF_INVALID_COOKIE;
    WIND_LOG_DEBUG(L"Compartment INPUTMODE_CONVERSION sink unadvised\n");
}

BOOL CTextService::_SetConversionMode(BOOL bChinese)
{
    if (_pThreadMgr == nullptr)
        return FALSE;

    ITfCompartmentMgr* pCompMgr = nullptr;
    HRESULT hr = _pThreadMgr->QueryInterface(IID_ITfCompartmentMgr, (void**)&pCompMgr);
    if (FAILED(hr) || pCompMgr == nullptr)
        return FALSE;

    ITfCompartment* pCompartment = nullptr;
    hr = pCompMgr->GetCompartment(GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, &pCompartment);
    pCompMgr->Release();

    if (FAILED(hr) || pCompartment == nullptr)
        return FALSE;

    // 仅维护 IME_CMODE_NATIVE 位，保留外界可能写入的其他位（FULLSHAPE/SYMBOL 等）。
    VARIANT cur;
    VariantInit(&cur);
    DWORD prev = 0;
    if (SUCCEEDED(pCompartment->GetValue(&cur)) && cur.vt == VT_I4)
        prev = (DWORD)cur.lVal;

    DWORD next = bChinese ? (prev | IME_CMODE_NATIVE) : (prev & ~IME_CMODE_NATIVE);
    if (next == prev)
    {
        pCompartment->Release();
        return TRUE;  // 无需写入，避免触发多余 OnChange
    }

    _bInConversionChange = TRUE;

    VARIANT var;
    var.vt = VT_I4;
    var.lVal = (LONG)next;
    hr = pCompartment->SetValue(_tfClientId, &var);
    pCompartment->Release();

    _bInConversionChange = FALSE;
    return SUCCEEDED(hr);
}

void CTextService::_DoFullStateSync()
{
    if (_pIPCClient == nullptr)
        return;

    // Lazy connect: push pipe may reconnect before main pipe is established (service restart).
    if (!_pIPCClient->IsConnected() && !_pIPCClient->Connect())
    {
        WIND_LOG_WARN(L"_DoFullStateSync: main pipe not connected, skipping\n");
        return;
    }

    // 异步化（Go 端 server.go::handleClient 走"先 Ack 后处理"）：本调用仅发 fire-and-forget
    // CMD_IME_ACTIVATED。Go 端收到立即回 Ack 解除本同步调用，HandleIMEActivated 在 Go
    // 端的 handler goroutine 中完成后通过 push pipe 推 CMD_ACTIVATION_STATUS_PUSH，
    // AsyncReader 线程上的回调 PostMessage 到 TSF 线程触发 ApplyActivationStatusResponse,
    // 完成 _SyncStateFromResponse + _EnsureHostRenderSetup 的全套状态同步动作。
    //
    // 这条路径消除了原同步 ReceiveResponse 在宿主 UI 线程上的 1500ms 阻塞窗口——
    // explorer.exe 等 shell 宿主进程不再因任何 Go 端 handler 内的跨进程调用形成环形等待。
    WIND_LOG_INFO(L"Sending IMEActivated (async); state will arrive via CMD_ACTIVATION_STATUS_PUSH\n");

    if (!_pIPCClient->SendIMEActivated())
    {
        // Stale pipe: write failed and Disconnect() was called; retry after fresh connect.
        if (_pIPCClient->Connect())
        {
            _pIPCClient->SendIMEActivated();
        }
        else
        {
            WIND_LOG_WARN(L"_DoFullStateSync: SendIMEActivated failed, toolbar may not show until next focus event\n");
        }
    }

    // 注：清除 _needsStateSync / _needsFocusRecovery 提前到此处，让后续 KeyEventSink 路径
    // 不再触发重复 state sync。即便 push 暂时未到，下一次焦点切换会重新拉起 activation 流程。
    _pIPCClient->ClearNeedsSyncFlag();
    _needsFocusRecovery = FALSE;
}

// ApplyActivationStatusResponse 在 TSF 线程上把 push pipe 接收到的 activation status 落地。
// 等价于原同步路径 ReceiveResponse → _SyncStateFromResponse + _EnsureHostRenderSetup。
// 调用点：CLangBarItemButton::_MsgWndProc 处理 WM_ACTIVATION_STATUS 时。
void CTextService::ApplyActivationStatusResponse(const ServiceResponse& response)
{
    _SyncStateFromResponse(response);
    // 与原 _DoFullStateSync 一致使用 forceRefresh=TRUE：activation 是新建/重建状态机的时刻,
    // 需要主动 (re)setup HostRender, 而不是惰性等下次窗口变化触发。
    _EnsureHostRenderSetup(response, TRUE);
}

void CTextService::TryRecoverFocusState()
{
    if (!_needsFocusRecovery || _pIPCClient == nullptr || !_pIPCClient->IsConnected())
        return;

    LONG caretX = _lastFocusCaretX;
    LONG caretY = _lastFocusCaretY;
    LONG caretHeight = _lastFocusCaretHeight > 0 ? _lastFocusCaretHeight : DEFAULT_CARET_HEIGHT;

    if (GetCaretPosition(&caretX, &caretY, &caretHeight))
    {
        _lastFocusCaretX = caretX;
        _lastFocusCaretY = caretY;
        _lastFocusCaretHeight = caretHeight > 0 ? caretHeight : DEFAULT_CARET_HEIGHT;
    }
    else if (_hasLastKnownCaretPos)
    {
        caretX = _lastKnownCaretX;
        caretY = _lastKnownCaretY;
        caretHeight = _lastKnownCaretHeight;
        WIND_LOG_INFO_FMT(L"Recovering focus state with last known caret x=%ld y=%ld h=%ld", caretX, caretY, caretHeight);
    }

    WIND_LOG_INFO_FMT(L"Attempting deferred focus recovery focusSession=%llu x=%ld y=%ld h=%ld",
        _focusSessionId, caretX, caretY, caretHeight);

    // 异步化：SendFocusGained 现在是 fire-and-forget。状态由 push pipe 经
    // CMD_ACTIVATION_STATUS_PUSH 异步送达，AsyncReader 线程的回调走 PostMessage
    // 到 TSF 线程的 WM_ACTIVATION_STATUS, 最终触发 ApplyActivationStatusResponse。
    // 输入诊断 HUD（Task 7）：与 OnSetFocus / compartment OnChange 一致，重新查询当前
    // 焦点 DocMgr 的 InputScope mask，避免恢复路径硬编码 mask=0 让 HUD 误显示
    // "原因: 无"。若此刻拿不到焦点 DocMgr（理论上不应发生，仅作防御），mask 退化为 0，
    // reason 仍据线程级 _bKeyboardDisabled + mask 计算，与另两条上报路径同语义。
    UINT64 recoveryMask = 0;
    ITfDocumentMgr* pDocMgrRecover = nullptr;
    if (_pThreadMgr != nullptr && SUCCEEDED(_pThreadMgr->GetFocus(&pDocMgrRecover)) && pDocMgrRecover != nullptr)
    {
        recoveryMask = _QueryInputScopeMask(pDocMgrRecover);
        pDocMgrRecover->Release();
    }
    uint8_t recoveryReason = ComputeInputReason(_bKeyboardDisabled != FALSE, recoveryMask);
    if (_pIPCClient->SendFocusGained((int)caretX, (int)caretY, (int)caretHeight, recoveryMask, _bKeyboardDisabled != FALSE, recoveryReason))
    {
        _needsFocusRecovery = FALSE;
        _pIPCClient->ClearNeedsSyncFlag();
        SendCaretPositionUpdate();
        WIND_LOG_INFO(L"Deferred focus recovery sent (async), state will arrive via push\n");
    }
    else
    {
        WIND_LOG_WARN_FMT(L"Deferred focus recovery send failed focusSession=%llu", _focusSessionId);
        _needsFocusRecovery = FALSE;
    }
}

BOOL CTextService::_InitIPCClient()
{
    _pIPCClient = new CIPCClient();
    if (_pIPCClient == nullptr)
        return FALSE;

    // Try to connect to Go Service (failure is OK, will retry later)
    if (!_pIPCClient->Connect())
    {
        WIND_LOG_WARN(L"Failed to connect to Service, will retry later\n");
    }

    // Set up activation status push callback (CMD_ACTIVATION_STATUS_PUSH)
    // 触发链：Go HandleIMEActivated / HandleFocusGained 异步完成 → push pipe → 本回调。
    // 必须 Post 到 TSF 线程：_SyncStateFromResponse / _EnsureHostRenderSetup 会触碰 TSF
    // COM 对象（compartment、LangBar 等），它们都是 STA-bound。
    CTextService* pThis = this;
    _pIPCClient->SetActivationPushCallback([pThis](const ServiceResponse& response) {
        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostActivationStatus(response);
        }
    });

    // Set up mode-only push callback (CMD_MODE_PUSH)
    // FocusGained 竞态优化：Go 在回 Ack 前入队此轻量包，使 _bChineseMode/_bFullWidth
    // 在 ~1ms 内就绪（vs 激活 push 的 ~15ms），消除首次按键竞态窗口。
    // 此回调在 AsyncReader 线程执行，不得访问 TSF COM 对象。
    // InterlockedExchange 提供全内存屏障，保证 TSF 主线程 OnTestKeyDown 立即见到新值。
    _pIPCClient->SetModePushCallback([pThis](bool chineseMode, bool fullWidth) {
        ::InterlockedExchange(reinterpret_cast<LONG*>(&pThis->_bChineseMode),
                              chineseMode ? TRUE : FALSE);
        ::InterlockedExchange(reinterpret_cast<LONG*>(&pThis->_bFullWidth),
                              fullWidth ? TRUE : FALSE);
    });

    // Set up shell exec callback (CMD_SHELL_EXEC)
    // 在前台应用进程中调用 ShellExecuteW，拥有前台权限，打开的窗口可正确置顶。
    // 回调在 AsyncReader 线程执行，ShellExecuteW 是线程安全的，无需切换到 TSF 主线程。
    // lpDirectory 传服务端算好的工作目录：传 nullptr 等于沿用**本进程**（即用户正在
    // 打字的那个宿主应用）的当前目录，那个值不可控——通用文件对话框会改掉它——
    // 于是靠相对路径找数据文件的程序（词典等）会找不到自己的词库。
    _pIPCClient->SetShellExecCallback([](const std::wstring& target, const std::wstring& params, const std::wstring& dir, const std::wstring& verb, const std::wstring& show) {
        const wchar_t* pParams = params.empty() ? nullptr : params.c_str();
        const wchar_t* pDir = dir.empty() ? nullptr : dir.c_str();
        // 空 verb 传 nullptr 而非 L"open"：nullptr 用的是该文件类型的**默认动词**，
        // 未必是 open（如某些类型默认 play/edit），与不带 verb 的历史行为一致。
        const wchar_t* pVerb = verb.empty() ? nullptr : verb.c_str();
        // 语义名 → SW_ 常量。取值已由服务端白名单校验；未知值落 SW_SHOWNORMAL，
        // 这只会在「新服务加了新取值 + 旧 DLL」的版本错配下出现。
        int nShow = SW_SHOWNORMAL;
        if      (show == L"min")    nShow = SW_SHOWMINNOACTIVE;  // 最小化且不抢焦点
        else if (show == L"max")    nShow = SW_SHOWMAXIMIZED;
        else if (show == L"hidden") nShow = SW_HIDE;
        HINSTANCE ret = ::ShellExecuteW(nullptr, pVerb, target.c_str(), pParams, pDir, nShow);
        if (reinterpret_cast<INT_PTR>(ret) <= 32)
            WIND_LOG_ERROR_FMT(L"ShellExecuteW failed: target=%s dir=%s verb=%s code=%d", target.c_str(), dir.c_str(), verb.c_str(), static_cast<int>(reinterpret_cast<INT_PTR>(ret)));
    });

    // Set up state push callback
    _pIPCClient->SetStatePushCallback([pThis](const ServiceResponse& response) {
        // This callback is called from the async reader thread
        // We need to update our state and notify the language bar
        WIND_LOG_INFO_FMT(L"State push received: mode=%d, fullWidth=%d, punct=%d, caps=%d\n",
                     response.IsChineseMode(), response.IsFullWidth(),
                     response.IsChinesePunct(), response.IsCapsLock());

        // Update internal state (atomic operation, thread-safe)
        pThis->_SetChineseMode(response.IsChineseMode());
        pThis->_bFullWidth = response.IsFullWidth();

        // Update language bar button using thread-safe PostUpdateFullStatus
        // This posts a message to the UI thread instead of calling COM directly
        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostUpdateFullStatus(
                response.IsChineseMode(),
                response.IsFullWidth(),
                response.IsChinesePunct(),
                response.IsToolbarVisible(),
                response.IsCapsLock(),
                response.iconLabel.empty() ? nullptr : response.iconLabel.c_str()
            );
        }
    });

    // Set up commit text callback for mouse click on candidate
    _pIPCClient->SetCommitTextCallback([pThis](const std::wstring& text) {
        // This callback is called from the async reader thread
        WIND_LOG_DEBUG_FMT(L"Commit text received from Go, textLen=%zu\n", text.length());

        // Use PostCommitText to ensure EndComposition is called before InsertText on UI thread
        // This fixes the issue where text was inserted into composition range
        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostCommitText(text);
        }
        else
        {
            // Fallback: direct InsertText (composition won't be ended properly)
            pThis->InsertText(text);
        }
    });

    // Set up replace-backward callback for undo commit push from service
    _pIPCClient->SetReplaceBackwardCallback([pThis](int count, const std::wstring& text) {
        // This callback is called from the async reader thread; hop to the TSF
        // thread via the message window (ReplacePrecedingChars needs it).
        WIND_LOG_DEBUG_FMT(L"Replace backward received from service, count=%d\n", count);
        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostReplaceBackward(count, text);
        }
        else
        {
            WIND_LOG_WARN(L"Replace backward push dropped: no LangBarItemButton\n");
        }
    });

    // Set up clear composition callback for mode toggle via menu
    _pIPCClient->SetClearCompositionCallback([pThis]() {
        // This callback is called from the async reader thread
        WIND_LOG_DEBUG(L"Clear composition received from service\n");

        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostClearComposition();
        }
        else
        {
            // Fallback: direct EndComposition
            pThis->EndComposition();
        }
    });

    // Set up update composition callback for mouse click partial confirm
    _pIPCClient->SetUpdateCompositionCallback([pThis](const std::wstring& text, int caretPos) {
        // This callback is called from the async reader thread
        WIND_LOG_DEBUG_FMT(L"Update composition received from service, textLen=%zu, caret=%d\n",
                           text.length(), caretPos);

        if (pThis->_pLangBarItemButton != nullptr)
        {
            pThis->_pLangBarItemButton->PostUpdateComposition(text, caretPos);
        }
        else
        {
            // Fallback: direct UpdateComposition
            pThis->UpdateComposition(text, caretPos);
        }
    });

    // Set up config sync callback for English auto-pair
    _pIPCClient->SetSyncConfigCallback([pThis](const std::string& key, const std::vector<uint8_t>& value) {
        if (pThis->_pKeyEventSink != nullptr)
        {
            pThis->_pKeyEventSink->OnSyncConfig(key, value);
        }
    });

    // Start async reader thread for receiving state pushes from Go
    if (!_pIPCClient->StartAsyncReader())
    {
        WIND_LOG_WARN(L"Failed to start async reader thread (non-fatal)\n");
        // Non-fatal - we can still use sync IPC
    }
    else
    {
        WIND_LOG_INFO(L"Async reader thread started for state push\n");
    }

    // Service-ready callback: Go sends CMD_SERVICE_READY when push pipe connects.
    // Route through LangBarItemButton's proven message window (same TSF thread,
    // known-working cross-thread channel used by PostUpdateFullStatus et al.).
    _pIPCClient->SetServiceReadyCallback([pThis]() {
        if (pThis->_pLangBarItemButton != nullptr)
            pThis->_pLangBarItemButton->PostServiceReady();
    });

    return TRUE;
}

void CTextService::_UninitIPCClient()
{
    if (_pIPCClient != nullptr)
    {
        // Stop async reader thread first
        _pIPCClient->StopAsyncReader();
        _pIPCClient->Disconnect();
        delete _pIPCClient;
        _pIPCClient = nullptr;
    }
}

// EditSession for inserting text at current selection
class CInsertTextEditSession : public ITfEditSession
{
public:
    CInsertTextEditSession(CTextService* pTextService, ITfContext* pContext, const std::wstring& text)
        : _refCount(1), _pTextService(pTextService), _pContext(pContext), _text(text), _success(FALSE)
    {
        _pTextService->AddRef();
        _pContext->AddRef();
    }

    ~CInsertTextEditSession()
    {
        _pTextService->Release();
        _pContext->Release();
    }

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj)
    {
        if (ppvObj == nullptr) return E_INVALIDARG;
        *ppvObj = nullptr;
        if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
        {
            *ppvObj = (ITfEditSession*)this;
            AddRef();
            return S_OK;
        }
        return E_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() { return InterlockedIncrement(&_refCount); }
    STDMETHODIMP_(ULONG) Release()
    {
        LONG cr = InterlockedDecrement(&_refCount);
        if (cr == 0) delete this;
        return cr;
    }

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec)
    {
        // Get ITfInsertAtSelection interface
        ITfInsertAtSelection* pInsertAtSel = nullptr;
        HRESULT hr = _pContext->QueryInterface(IID_ITfInsertAtSelection, (void**)&pInsertAtSel);
        if (FAILED(hr) || pInsertAtSel == nullptr)
        {
            WIND_LOG_DEBUG(L"InsertTextEditSession: Failed to get ITfInsertAtSelection\n");
            return E_FAIL;
        }

        // Insert text at current selection
        ITfRange* pRange = nullptr;
        hr = pInsertAtSel->InsertTextAtSelection(
            ec,
            0,  // No special flags
            _text.c_str(),
            (LONG)_text.length(),
            &pRange);

        pInsertAtSel->Release();

        if (FAILED(hr))
        {
            WIND_LOG_DEBUG_FMT(L"InsertTextEditSession: InsertTextAtSelection failed hr=0x%08X\n", hr);
            return hr;
        }

        if (pRange != nullptr)
        {
            // Move selection to end of inserted text
            pRange->Collapse(ec, TF_ANCHOR_END);

            TF_SELECTION sel = {};
            sel.range = pRange;
            sel.style.ase = TF_AE_NONE;
            sel.style.fInterimChar = FALSE;
            _pContext->SetSelection(ec, 1, &sel);

            pRange->Release();
        }

        _success = TRUE;
        WIND_LOG_DEBUG_FMT(L"InsertTextEditSession: Successfully inserted '%s'\n", _text.c_str());
        return S_OK;
    }

    BOOL GetSuccess() const { return _success; }

private:
    LONG _refCount;
    CTextService* _pTextService;
    ITfContext* _pContext;
    std::wstring _text;
    BOOL _success;
};

BOOL CTextService::InsertText(const std::wstring& text)
{
    if (text.empty())
    {
        return TRUE;
    }

    // Try TSF method first (works on main thread with proper context)
    if (_pThreadMgr != nullptr)
    {
        // Get current document manager
        ITfDocumentMgr* pDocMgr = nullptr;
        HRESULT hr = _pThreadMgr->GetFocus(&pDocMgr);
        if (SUCCEEDED(hr) && pDocMgr != nullptr)
        {
            // Get top context
            ITfContext* pContext = nullptr;
            hr = pDocMgr->GetTop(&pContext);
            pDocMgr->Release();

            if (SUCCEEDED(hr) && pContext != nullptr)
            {
                // Try to insert using TSF EditSession
                CInsertTextEditSession* pEditSession = new CInsertTextEditSession(this, pContext, text);

                HRESULT hrSession;
                // Use TF_ES_SYNC to ensure synchronous execution
                hr = pContext->RequestEditSession(_tfClientId, pEditSession, TF_ES_SYNC | TF_ES_READWRITE, &hrSession);

                BOOL success = pEditSession->GetSuccess();
                pEditSession->Release();
                pContext->Release();

                if (SUCCEEDED(hr) && SUCCEEDED(hrSession) && success)
                {
                    WIND_LOG_DEBUG(L"InsertText: Successfully used TSF method\n");
                    return TRUE;
                }

                WIND_LOG_DEBUG_FMT(L"InsertText: TSF method failed (hr=0x%08X, hrSession=0x%08X), falling back to SendInput\n", hr, hrSession);
                WIND_LOG_DEBUG_FMT(
                    L"compat.insert_text_fallback focusSession=%llu textLen=%zu hr=0x%08X hrSession=0x%08X",
                    _focusSessionId, text.length(), hr, hrSession
                );
                WindLogForegroundProcessInfo(4, L"compat.insert_text_fallback.foreground_host");
            }
        }
    }

    // Fallback: Use SendInput for batch input (all characters at once)
    // This works from any thread and is used when TSF method fails
    WIND_LOG_DEBUG_FMT(L"InsertText: Using SendInput batch method for '%s'\n", text.c_str());

    // Allocate INPUT structures for all characters (2 per char: down + up)
    std::vector<INPUT> inputs;
    inputs.reserve(text.length() * 2);

    for (wchar_t ch : text)
    {
        // 标记为自生成，避免被自己的 OnTestKeyDown 钩子当成真实按键二次处理
        // （同 ReplacePrecedingChars 兜底路径，见其注释）。
        if (_pKeyEventSink != nullptr) _pKeyEventSink->MarkSyntheticKey(VK_PACKET);

        INPUT inputDown = {};
        inputDown.type = INPUT_KEYBOARD;
        inputDown.ki.wVk = 0;
        inputDown.ki.wScan = ch;
        inputDown.ki.dwFlags = KEYEVENTF_UNICODE;
        inputs.push_back(inputDown);

        INPUT inputUp = {};
        inputUp.type = INPUT_KEYBOARD;
        inputUp.ki.wVk = 0;
        inputUp.ki.wScan = ch;
        inputUp.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
        inputs.push_back(inputUp);
    }

    // Send all inputs at once - this makes text appear instantly
    UINT sent = SendInput((UINT)inputs.size(), inputs.data(), sizeof(INPUT));

    if (sent != inputs.size())
    {
        WIND_LOG_WARN_FMT(L"InsertText: SendInput sent %u of %u inputs\n", sent, (UINT)inputs.size());
    }
    else
    {
        WIND_LOG_DEBUG_FMT(
            L"compat.sendinput_commit focusSession=%llu textLen=%zu inputs=%u",
            _focusSessionId, text.length(), (UINT)inputs.size()
        );
    }

    return TRUE;
}

// Static variables to track last known good caret position
static LONG s_lastCaretX = 0;
static LONG s_lastCaretY = 0;
static LONG s_lastCaretHeight = 20;
static BOOL s_hasLastCaretPos = FALSE;

// HoldComposition 计时器回调使用的 thread_local 实例指针。
// SetTimer(NULL,...) 的回调在 TSF UI 线程上触发，thread_local 保证线程安全。
static thread_local CTextService* g_holdTimerInstance = nullptr;

// Get caret position using TSF APIs (for browsers and modern apps)
BOOL CTextService::GetCaretPositionFromTSF(LONG* px, LONG* py, LONG* pHeight)
{
    if (_pThreadMgr == nullptr)
    {
        return FALSE;
    }

    // Get current document manager
    ITfDocumentMgr* pDocMgr = nullptr;
    HRESULT hr = _pThreadMgr->GetFocus(&pDocMgr);
    if (FAILED(hr) || pDocMgr == nullptr)
    {
        return FALSE;
    }

    // Get top context
    ITfContext* pContext = nullptr;
    hr = pDocMgr->GetTop(&pContext);
    pDocMgr->Release();

    if (FAILED(hr) || pContext == nullptr)
    {
        return FALSE;
    }

    // Use EditSession to get caret position
    RECT rc = {};
    BOOL result = CCaretEditSession::GetCaretRect(pContext, _tfClientId, &rc);
    pContext->Release();

    if (result)
    {
        // rc contains screen coordinates
        *px = rc.left;
        *py = rc.bottom;  // Position below the caret
        *pHeight = rc.bottom - rc.top;

        // A zero-height rect (top == bottom) means GetTextExt returned a degenerate
        // result — common in apps like WPS when no TSF composition is active (e.g.,
        // non-inline-preedit mode). Return FALSE so the caller falls through to
        // GetGUIThreadInfo which tracks the Win32 caret independently of composition.
        if (*pHeight <= 0)
        {
            WIND_LOG_DEBUG(L"GetCaretPositionFromTSF: Degenerate rect (height=0), falling back\n");
            return FALSE;
        }

        // 坐标越界保护：TSF 坐标落在前台窗口外时判定不可信，返回 FALSE，让 GetCaretPosition
        // 回退到 GUIThreadInfo / GetCaretPos（窗口相对、可靠）。详见 IsScreenPointOutsideForegroundWindow。
        if (IsScreenPointOutsideForegroundWindow(rc.left, rc.top))
        {
            WIND_LOG_DEBUG_FMT(L"GetCaretPositionFromTSF: caret(%ld,%ld) outside fg window, falling back\n", rc.left, rc.top);
            return FALSE;
        }

        // Save as last known good position
        s_lastCaretX = *px;
        s_lastCaretY = *py;
        s_lastCaretHeight = *pHeight;
        s_hasLastCaretPos = TRUE;

        WIND_LOG_DEBUG(L"GetCaretPositionFromTSF: Success\n");
        return TRUE;
    }

    return FALSE;
}

BOOL CTextService::RefreshTextInputContext()
{
    if (!_hasTextInputContext && _pThreadMgr != nullptr)
    {
        ITfDocumentMgr* pDocMgr = nullptr;
        HRESULT hr = _pThreadMgr->GetFocus(&pDocMgr);
        if (SUCCEEDED(hr) && pDocMgr != nullptr)
        {
            _hasTextInputContext = _DocMgrHasEditableContext(pDocMgr);
            pDocMgr->Release();
            if (_hasTextInputContext)
                WIND_LOG_DEBUG_FMT(L"RefreshTextInputContext: late editable context focusSession=%llu", _focusSessionId);
            else
                WIND_LOG_DEBUG_FMT(L"RefreshTextInputContext: docmgr present but not editable focusSession=%llu", _focusSessionId);
        }
        else
        {
            WIND_LOG_DEBUG_FMT(L"RefreshTextInputContext: no focused docmgr hr=0x%08X focusSession=%llu",
                (uint32_t)hr, _focusSessionId);
        }
    }
    return _hasTextInputContext;
}

// 读取焦点文档的 TSF InputScope 集合，编码为 bitmask（bit N = 枚举值 N 存在）。
// 失败/无 InputScope 时返回 0（视为 IS_DEFAULT/未知）。详见 CQueryInputScopeEditSession。
UINT64 CTextService::_QueryInputScopeMask(ITfDocumentMgr* pDocMgr)
{
    if (pDocMgr == nullptr)
        return 0;

    ITfContext* pCtx = nullptr;
    if (FAILED(pDocMgr->GetTop(&pCtx)) || pCtx == nullptr)
        return 0;

    UINT64 mask = 0;
    CQueryInputScopeEditSession* pES = new CQueryInputScopeEditSession(pCtx, &mask);
    if (pES != nullptr)
    {
        HRESULT hrSession = S_OK;
        HRESULT hr = pCtx->RequestEditSession(_tfClientId, pES, TF_ES_SYNC | TF_ES_READ, &hrSession);
        if (FAILED(hr) || FAILED(hrSession))
        {
            WIND_LOG_DEBUG_FMT(L"_QueryInputScopeMask: RequestEditSession hr=0x%08X hrSession=0x%08X", hr, hrSession);
        }
        pES->Release();
    }

    pCtx->Release();
    WIND_LOG_DEBUG_FMT(L"_QueryInputScopeMask: mask=0x%016llX", (unsigned long long)mask);
    return mask;
}

// 读取焦点 context 上某个 bool 型 compartment（VT_I4），并打诊断日志。未设置时返回 false。
static bool ReadContextCompartmentBool(ITfContext* pContext, REFGUID guid, const wchar_t* name)
{
    bool value = false;
    ITfCompartmentMgr* pCompMgr = nullptr;
    if (SUCCEEDED(pContext->QueryInterface(IID_ITfCompartmentMgr, reinterpret_cast<void**>(&pCompMgr))) && pCompMgr != nullptr)
    {
        ITfCompartment* pComp = nullptr;
        if (SUCCEEDED(pCompMgr->GetCompartment(guid, &pComp)) && pComp != nullptr)
        {
            VARIANT var;
            VariantInit(&var);
            if (SUCCEEDED(pComp->GetValue(&var)) && var.vt == VT_I4)
                value = (var.lVal != 0);
            VariantClear(&var);
            pComp->Release();
        }
        pCompMgr->Release();
    }
    WIND_LOG_DEBUG_FMT(L"compartment %s = %d", name, value ? 1 : 0);
    return value;
}

// 判断焦点 context 是否被宿主标记为"禁用输入法"（GUID_COMPARTMENT_KEYBOARD_DISABLED）。
// 这是 Weasel/小狼毫采用的密码框判定信号：Chromium 系浏览器密码框会置位它，而无痕模式
// 的普通可编辑框不会，因此能精确区分密码框与隐私字段，无需 UIA。
bool CTextService::_IsFocusKeyboardDisabled(ITfDocumentMgr* pDocMgr)
{
    if (pDocMgr == nullptr)
        return false;
    ITfContext* pContext = nullptr;
    if (FAILED(pDocMgr->GetTop(&pContext)) || pContext == nullptr)
        return false;
    bool disabled = ReadContextCompartmentBool(pContext, kGuidCompartmentKeyboardDisabled, L"KEYBOARD_DISABLED");
    pContext->Release();
    return disabled;
}

// 密码框强制英文抑制当前是否生效。**必须镜像** core `apply_input_diag` 的判据：
//   suppress = is_password_scope(mask) && password_suppress_enabled
// 两侧判据一旦漂移就会重现「吃了再吐」——DLL 吃了键、core 却回 PassThrough → 严格 TSF
// 宿主（Chrome/Electron）不回退合成 WM_CHAR，键直接丢失（密码框里表现为完全打不出字）。
//
// ⚠ KEYBOARD_DISABLED 有**两个层级**，混为一谈正是本函数此前失效的原因（2026-07-27 修）：
//   · 线程级 `_bKeyboardDisabled`（advise 在 _pThreadMgr 的 compartment 上）——
//     OnTestKeyDown 开头 `IsKeyboardDisabled()` 全放行看的就是它。此时引擎压根收不到键，
//     抑制无从谈起，故在此早退。
//   · context 级 `_focusIsPassword`（读焦点 context 的 compartment，见 _IsFocusKeyboardDisabled）
//     —— **Chromium 系网页密码框置的是这一层**，线程级纹丝不动，DLL 照常吃键。
//
// 旧判据把 `_focusIsPassword` 也列为早退条件，理由是「compartment 置位时键已被放行」，
// 但那句话只对线程级成立。于是网页密码框同时逃过了「放行」与「抑制」两道闸，中文照打，
// 而高级菜单里的开关怎么切都没反应（它只能改 _passwordSuppressEnabled，改不动早退）。
//
// context 级的密码信号已在 OnSetFocus 折进 mask 的 IS_PASSWORD 位（见 _focusIsPassword
// 赋值处），因此这里只判 mask 就已覆盖两种来源，无需再单独看 _focusIsPassword。
BOOL CTextService::IsPasswordSuppressActive() const
{
    if (!_passwordSuppressEnabled)
        return FALSE;
    if (_bKeyboardDisabled)
        return FALSE;
    // IS_PASSWORD=31 / IS_NUMERIC_PASSWORD=63（对齐 core is_password_scope 的两位）
    const UINT64 kPasswordScopeBits = (1ULL << 31) | (1ULL << 63);
    return (_focusInputScopeMask & kPasswordScopeBits) != 0;
}

BOOL CTextService::_DocMgrHasEditableContext(ITfDocumentMgr* pDocMgr, DWORD* pDynFlagsOut)
{
    if (pDynFlagsOut)
        *pDynFlagsOut = 0;

    if (pDocMgr == nullptr)
        return FALSE;

    ITfContext* pCtx = nullptr;
    HRESULT hr = pDocMgr->GetTop(&pCtx);
    if (FAILED(hr) || pCtx == nullptr)
    {
        WIND_LOG_DEBUG_FMT(L"_DocMgrHasEditableCtx: GetTop hr=0x%08X ctx=%p -> FALSE", hr, pCtx);
        if (pCtx) pCtx->Release();
        return FALSE;
    }

    TF_STATUS status = {};
    BOOL result = TRUE;
    HRESULT hrStatus = pCtx->GetStatus(&status);
    if (SUCCEEDED(hrStatus))
    {
        // Only TF_SD_READONLY (bit 0 of dwDynamicFlags) reliably means "no writable text
        // input". Chrome dynamically sets/clears this bit when text fields gain/lose focus.
        // TF_SS_TRANSITORY (0x4 of dwStaticFlags) is NOT a reliable signal — Chrome and
        // JetBrains both set it on contexts that do have real text input.
        WIND_LOG_DEBUG_FMT(L"_DocMgrHasEditableCtx: dynFlags=0x%X statFlags=0x%X", status.dwDynamicFlags, status.dwStaticFlags);
        if (status.dwDynamicFlags & TF_SD_READONLY)
            result = FALSE;
        if (pDynFlagsOut)
            *pDynFlagsOut = status.dwDynamicFlags;
    }
    else
    {
        WIND_LOG_DEBUG_FMT(L"_DocMgrHasEditableCtx: GetStatus hr=0x%08X -> default TRUE", hrStatus);
    }

    WIND_LOG_DEBUG_FMT(L"_DocMgrHasEditableCtx: -> %d", result);
    pCtx->Release();
    return result;
}

// Helper function to check if a window is a console/terminal window
static BOOL IsConsoleWindow(HWND hwnd)
{
    if (hwnd == nullptr)
        return FALSE;

    WCHAR className[256] = {0};
    if (GetClassNameW(hwnd, className, 256) == 0)
        return FALSE;

    // Check for known console window classes
    // ConsoleWindowClass - Traditional conhost.exe console
    // CASCADIA_HOSTING_WINDOW_CLASS - Windows Terminal
    // PseudoConsoleWindow - ConPTY pseudo console
    if (wcscmp(className, L"ConsoleWindowClass") == 0 ||
        wcscmp(className, L"CASCADIA_HOSTING_WINDOW_CLASS") == 0 ||
        wcsstr(className, L"Console") != nullptr ||
        wcsstr(className, L"Terminal") != nullptr)
    {
        return TRUE;
    }

    return FALSE;
}

// Try to get caret position for console/terminal windows
static BOOL GetConsoleCaretPosition(HWND hwndConsole, LONG* px, LONG* py, LONG* pHeight)
{
    if (hwndConsole == nullptr)
        return FALSE;

    // For Windows Terminal and modern consoles, we can try to get the console buffer info
    // This requires the console to be attached to our process or accessible

    // First, try to get the console window handle and screen buffer info
    // Note: GetConsoleWindow() returns the console for the CURRENT process,
    // which may not be the foreground console. We need a different approach.

    // Get window rect for calculations
    RECT rcWindow;
    if (!GetWindowRect(hwndConsole, &rcWindow))
        return FALSE;

    // Get client rect
    RECT rcClient;
    if (!GetClientRect(hwndConsole, &rcClient))
        return FALSE;

    // Calculate client area origin in screen coordinates
    POINT clientOrigin = {0, 0};
    ClientToScreen(hwndConsole, &clientOrigin);

    // Try to use GUITHREADINFO - sometimes works for console windows
    DWORD threadId = GetWindowThreadProcessId(hwndConsole, nullptr);
    GUITHREADINFO guiInfo = { sizeof(GUITHREADINFO) };

    if (GetGUIThreadInfo(threadId, &guiInfo) && guiInfo.hwndCaret != nullptr)
    {
        POINT caretPos;
        caretPos.x = guiInfo.rcCaret.left;
        caretPos.y = guiInfo.rcCaret.bottom;

        // Convert from client coordinates to screen coordinates
        ClientToScreen(guiInfo.hwndCaret, &caretPos);

        // Validate that it's within the console window area
        if (caretPos.x >= rcWindow.left && caretPos.x <= rcWindow.right &&
            caretPos.y >= rcWindow.top && caretPos.y <= rcWindow.bottom)
        {
            *px = caretPos.x;
            *py = caretPos.y;
            // 不依赖 max 宏（MSVC 由 <windows.h> 提供、MinGW 不一定有），用等价三元表达式
            LONG caretH = guiInfo.rcCaret.bottom - guiInfo.rcCaret.top;
            *pHeight = (caretH > 16) ? caretH : 16;

            WIND_LOG_DEBUG(L"GetConsoleCaretPosition: Got caret from GUITHREADINFO\n");
            return TRUE;
        }
    }

    // Fallback: Position the candidate window at a reasonable location
    // For consoles, we position it near the bottom of the visible area
    // This is better than the center, as typing usually happens at the bottom

    // Estimate: console typically shows text near the current cursor line
    // Position the IME window near the bottom-left of the console
    int clientWidth = rcClient.right - rcClient.left;
    int clientHeight = rcClient.bottom - rcClient.top;

    // Position at roughly 10% from left, 80% from top (near bottom where typing usually occurs)
    *px = clientOrigin.x + (clientWidth * 10 / 100);
    *py = clientOrigin.y + (clientHeight * 80 / 100);
    *pHeight = 16;  // Standard console line height approximation

    WIND_LOG_DEBUG_FMT(L"GetConsoleCaretPosition: Using console fallback position (%ld, %ld)\n", *px, *py);

    return TRUE;
}

BOOL CTextService::GetCaretPosition(LONG* px, LONG* py, LONG* pHeight)
{
    // First, check if the foreground window is a console/terminal
    HWND hwndForeground = GetForegroundWindow();
    BOOL isConsole = IsConsoleWindow(hwndForeground);

    if (isConsole)
    {
        WIND_LOG_DEBUG(L"GetCaretPosition: Detected console window\n");
    }

    // Method 1: Try TSF APIs first - this is the most reliable for browsers and modern apps
    // ITfContextView::GetTextExt provides accurate caret position in Chrome, Edge, etc.
    if (GetCaretPositionFromTSF(px, py, pHeight))
    {
        return TRUE;
    }

    // For console windows, use specialized handling
    if (isConsole)
    {
        if (GetConsoleCaretPosition(hwndForeground, px, py, pHeight))
        {
            // Save as last known good position
            s_lastCaretX = *px;
            s_lastCaretY = *py;
            s_lastCaretHeight = *pHeight;
            s_hasLastCaretPos = TRUE;
            return TRUE;
        }
    }

    // Method 3: Try to get caret position from the GUI thread info
    // This works well for traditional Win32 applications
    GUITHREADINFO guiInfo = { sizeof(GUITHREADINFO) };

    if (GetGUIThreadInfo(0, &guiInfo))
    {
        // Check if there's an active caret
        if (guiInfo.hwndCaret != nullptr)
        {
            POINT caretPos;
            caretPos.x = guiInfo.rcCaret.left;
            caretPos.y = guiInfo.rcCaret.bottom;

            // Convert from client coordinates to screen coordinates
            ClientToScreen(guiInfo.hwndCaret, &caretPos);

            // Validate position (not at origin, which usually means failure)
            if (caretPos.x > 0 || caretPos.y > 0)
            {
                *px = caretPos.x;
                *py = caretPos.y;
                *pHeight = guiInfo.rcCaret.bottom - guiInfo.rcCaret.top;

                if (*pHeight <= 0)
                    *pHeight = 20;  // Default caret height

                // Save as last known good position
                s_lastCaretX = *px;
                s_lastCaretY = *py;
                s_lastCaretHeight = *pHeight;
                s_hasLastCaretPos = TRUE;

                return TRUE;
            }
        }
    }

    // Fallback to GetCaretPos
    POINT pt;
    if (GetCaretPos(&pt))
    {
        // Get the foreground window to convert coordinates
        HWND hwnd = GetForegroundWindow();
        if (hwnd != nullptr)
        {
            ClientToScreen(hwnd, &pt);

            // Validate position
            if (pt.x > 0 || pt.y > 0)
            {
                *px = pt.x;
                *py = pt.y + 20;  // Estimate caret height
                *pHeight = 20;

                // Save as last known good position
                s_lastCaretX = *px;
                s_lastCaretY = *py;
                s_lastCaretHeight = *pHeight;
                s_hasLastCaretPos = TRUE;

                return TRUE;
            }
        }
    }

    // Method 4: For browsers/WebView2, try to get focus window position
    // Browsers often don't expose caret position properly, so we use the focus window
    HWND hwndFocus = GetForegroundWindow();
    if (hwndFocus != nullptr)
    {
        RECT rc;
        if (GetWindowRect(hwndFocus, &rc))
        {
            // If we have a last known position within this window, use it
            if (s_hasLastCaretPos &&
                s_lastCaretX >= rc.left && s_lastCaretX <= rc.right &&
                s_lastCaretY >= rc.top && s_lastCaretY <= rc.bottom)
            {
                *px = s_lastCaretX;
                *py = s_lastCaretY;
                *pHeight = s_lastCaretHeight;
                return TRUE;
            }

            // Otherwise, position near the center-left of the window
            // This is a fallback for browsers that don't report caret position
            *px = rc.left + 100;  // Some offset from left edge
            *py = rc.top + (rc.bottom - rc.top) / 2;  // Vertical center
            *pHeight = 20;

            WIND_LOG_DEBUG(L"GetCaretPosition: Using window position fallback\n");
            return TRUE;
        }
    }

    // Method 5: Use last known good position if available
    if (s_hasLastCaretPos)
    {
        *px = s_lastCaretX;
        *py = s_lastCaretY;
        *pHeight = s_lastCaretHeight;
        WIND_LOG_DEBUG(L"GetCaretPosition: Using last known position\n");
        return TRUE;
    }

    WIND_LOG_DEBUG(L"GetCaretPosition: Failed to get caret position\n");
    return FALSE;
}

// Convert logical coordinates to physical screen coordinates when the host process
// is not Per-Monitor DPI aware. DPI-unaware apps receive virtualized 96-DPI coordinates
// from Windows, but our Go service (wind_input.exe) is Per-Monitor DPI aware and works
// in physical pixels. This mismatch causes the candidate window to appear at the wrong
// position in legacy/old applications.
static void ConvertToPhysicalCoordinates(LONG& x, LONG& y, LONG& height,
                                         LONG& compStartX, LONG& compStartY)
{
    // Dynamically load to support older Windows versions
    static auto pGetProcessDpiAwareness =
        reinterpret_cast<decltype(&GetProcessDpiAwareness)>(
            GetProcAddress(GetModuleHandleW(L"shcore.dll"), "GetProcessDpiAwareness"));
    static auto pLogicalToPhysicalPointForPerMonitorDPI =
        reinterpret_cast<BOOL(WINAPI*)(HWND, LPPOINT)>(
            GetProcAddress(GetModuleHandleW(L"user32.dll"), "LogicalToPhysicalPointForPerMonitorDPI"));
    static auto pGetDpiForMonitor =
        reinterpret_cast<decltype(&GetDpiForMonitor)>(
            GetProcAddress(GetModuleHandleW(L"shcore.dll"), "GetDpiForMonitor"));

    if (!pGetProcessDpiAwareness || !pLogicalToPhysicalPointForPerMonitorDPI)
        return;

    PROCESS_DPI_AWARENESS awareness = PROCESS_PER_MONITOR_DPI_AWARE;
    if (FAILED(pGetProcessDpiAwareness(nullptr, &awareness)))
        return;

    if (awareness == PROCESS_PER_MONITOR_DPI_AWARE)
        return; // Already physical coordinates, no conversion needed

    WIND_LOG_DEBUG_FMT(L"ConvertToPhysicalCoordinates: host DPI awareness=%d, before: caret(%ld,%ld h=%ld) comp(%ld,%ld)",
                       (int)awareness, x, y, height, compStartX, compStartY);

    HWND hwnd = GetForegroundWindow();
    if (!hwnd)
        return;

    // Convert caret position
    POINT ptCaret = { x, y };
    if (!pLogicalToPhysicalPointForPerMonitorDPI(hwnd, &ptCaret))
        return;

    // Convert a second point to derive the physical height
    POINT ptCaretTop = { x, y - height };
    if (!pLogicalToPhysicalPointForPerMonitorDPI(hwnd, &ptCaretTop))
    {
        // Fallback: scale height using monitor DPI
        if (pGetDpiForMonitor)
        {
            HMONITOR hMon = MonitorFromPoint(ptCaret, MONITOR_DEFAULTTONEAREST);
            UINT dpiX = 96, dpiY = 96;
            if (hMon && SUCCEEDED(pGetDpiForMonitor(hMon, MDT_EFFECTIVE_DPI, &dpiX, &dpiY)))
            {
                height = MulDiv(height, (int)dpiX, 96);
            }
        }
    }
    else
    {
        height = ptCaret.y - ptCaretTop.y;
    }

    x = ptCaret.x;
    y = ptCaret.y;

    // Convert composition start position if present
    if (compStartX != 0 || compStartY != 0)
    {
        POINT ptComp = { compStartX, compStartY };
        if (pLogicalToPhysicalPointForPerMonitorDPI(hwnd, &ptComp))
        {
            compStartX = ptComp.x;
            compStartY = ptComp.y;
        }
    }

    WIND_LOG_DEBUG_FMT(L"ConvertToPhysicalCoordinates: after: caret(%ld,%ld h=%ld) comp(%ld,%ld)",
                       x, y, height, compStartX, compStartY);
}

// 坐标出口：DPI 归一 → 记为 last known → 发 IPC。同步与异步两条取坐标路径共用，
// 保证「记住的坐标」和「发出去的坐标」永远是同一份（曾经只有同步路径更新 last known）。
void CTextService::_EmitCaretUpdate(LONG x, LONG y, LONG height, LONG compStartX, LONG compStartY)
{
    ConvertToPhysicalCoordinates(x, y, height, compStartX, compStartY);

    _hasLastKnownCaretPos = TRUE;
    _lastKnownCaretX = x;
    _lastKnownCaretY = y;
    _lastKnownCaretHeight = height > 0 ? height : DEFAULT_CARET_HEIGHT;

    if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        _pIPCClient->SendCaretUpdate((int)x, (int)y, (int)height, (int)compStartX, (int)compStartY);
    }
}

// 非按键上下文里发起取坐标：见 CCaretEditSession::RequestCaretRectAsync 的说明。
// 返回 FALSE 表示请求根本没发出去（无焦点 context 等），调用方需自己兜底。
BOOL CTextService::RequestCaretPositionUpdateAsync()
{
    if (_pThreadMgr == nullptr || _pComposition == nullptr)
    {
        return FALSE;
    }

    ITfDocumentMgr* pDocMgr = nullptr;
    HRESULT hr = _pThreadMgr->GetFocus(&pDocMgr);
    if (FAILED(hr) || pDocMgr == nullptr)
    {
        return FALSE;
    }

    ITfContext* pContext = nullptr;
    hr = pDocMgr->GetTop(&pContext);
    pDocMgr->Release();
    if (FAILED(hr) || pContext == nullptr)
    {
        return FALSE;
    }

    BOOL requested = CCaretEditSession::RequestCaretRectAsync(
        pContext, _tfClientId, _pComposition, (LONG)GetPendingCommitPrefixLength(), this);
    pContext->Release();

    return requested;
}

// 异步 edit session 的回调出口。这里**刻意不做任何回退**：取不到就不发，服务端会用按键
// 时缓存的坐标兜底，那份来自按键路径的同步 edit session，比 Win32 caret 可信得多。
// 曾经的 bug 正是回退到 GetGUIThreadInfo，在 Word 非正文样式行上拿到无关窗口的 caret。
void CTextService::OnAsyncCaretRectReady(const RECT& caretRect, BOOL hasCompStart, const RECT& compStartRect)
{
    // 排队期间用户可能已上屏：这份坐标属于上一轮组合，发出去会把候选窗钉在已消失的组合上。
    if (_pComposition == nullptr)
    {
        WIND_LOG_DEBUG(L"OnAsyncCaretRectReady: composition already ended, dropping\n");
        return;
    }

    LONG height = caretRect.bottom - caretRect.top;
    if (height <= 0)
    {
        // 退化矩形 = 宿主尚未完成排版，判据同 GetCaretPositionFromTSF。
        WIND_LOG_DEBUG(L"OnAsyncCaretRectReady: degenerate rect (height=0), dropping\n");
        return;
    }

    if (IsScreenPointOutsideForegroundWindow(caretRect.left, caretRect.top))
    {
        WIND_LOG_DEBUG_FMT(L"OnAsyncCaretRectReady: caret(%ld,%ld) outside fg window, dropping\n",
                           caretRect.left, caretRect.top);
        return;
    }

    LONG compStartX = 0, compStartY = 0;
    if (hasCompStart)
    {
        compStartX = compStartRect.left;
        compStartY = compStartRect.bottom;
    }

    WIND_LOG_DEBUG_FMT(L"OnAsyncCaretRectReady: caret(%ld,%ld h=%ld) compStart=(%ld,%ld)\n",
                       caretRect.left, caretRect.bottom, height, compStartX, compStartY);

    _EmitCaretUpdate(caretRect.left, caretRect.bottom, height, compStartX, compStartY);
}

void CTextService::SendCaretPositionUpdate()
{
    // Weasel 模式：composition 刚创建后第一次调用，不立即发 IPC。
    // 应用尚未完成 layout reflow，GetTextExt 此时返回的可能是旧坐标
    // （WPS 中 h>0 但坐标陈旧），先发会导致候选窗显示在错误位置然后跳到正确位置。
    // 改为等 OnLayoutChange 触发（reflow 完成的权威信号），50ms timer 兜底。
    if (_compositionJustStarted && _pComposition != nullptr)
    {
        if (_pLangBarItemButton != nullptr)
        {
            _pLangBarItemButton->PostDelayedCaretPositionUpdate();
        }
        // 通知 Go 端：composition 刚启动, 真正的 caret 会在 reflow 后到达。
        // Go 端据此延长 pendingFirstShow 超时, 避免回退到按键前的旧坐标。
        // 适用于 OnLayoutChange burst 跨度较长的应用 (如 EverEdit ~200ms 间隔)。
        if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
        {
            _pIPCClient->SendCaretPending();
        }
        return;
    }

    LONG x = 0, y = 0, height = 0;
    LONG compStartX = 0, compStartY = 0;
    BOOL hasPosition = FALSE;

    // Priority 1: Use cached position from edit session (reliable for WebView apps
    // where separate CaretEditSession with TF_INVALID_COOKIE is rejected).
    // The cache is set inside CUpdateCompositionEditSession::DoEditSession, which
    // guarantees that caret and composition-start come from the SAME edit session
    // and thus the same coordinate space.
    if (_hasCachedCaretPos)
    {
        x = _cachedCaretRect.left;
        y = _cachedCaretRect.bottom;
        height = _cachedCaretRect.bottom - _cachedCaretRect.top;
        hasPosition = TRUE;

        if (_hasCachedCompStartPos)
        {
            compStartX = _cachedCompStartRect.left;
            compStartY = _cachedCompStartRect.bottom;
        }

        // Clear cache (one-shot: next call falls back to normal methods)
        _hasCachedCaretPos = FALSE;
        _hasCachedCompStartPos = FALSE;
    }

    // Priority 2: Normal method (separate edit session + fallbacks)
    if (!hasPosition)
    {
        if (!GetCaretPosition(&x, &y, &height))
        {
            if (_hasLastKnownCaretPos)
            {
                x = _lastKnownCaretX;
                y = _lastKnownCaretY;
                height = _lastKnownCaretHeight;
                hasPosition = TRUE;
                WIND_LOG_INFO_FMT(L"SendCaretPositionUpdate: using last known caret x=%ld y=%ld h=%ld", x, y, height);
            }
            else
            {
                return; // No position available at all
            }
        }
        else
        {
            hasPosition = TRUE;
        }

        if (_pComposition != nullptr)
        {
            GetCompositionStartPosition(&compStartX, &compStartY);
        }
    }

    if (hasPosition)
    {
        // DPI 归一 + 记 last known + 发 IPC（fire-and-forget，无响应）
        _EmitCaretUpdate(x, y, height, compStartX, compStartY);
    }
}

BOOL CTextService::GetCompositionStartPosition(LONG* px, LONG* py)
{
    if (_pComposition == nullptr || _pThreadMgr == nullptr)
    {
        return FALSE;
    }

    ITfDocumentMgr* pDocMgr = nullptr;
    HRESULT hr = _pThreadMgr->GetFocus(&pDocMgr);
    if (FAILED(hr) || pDocMgr == nullptr)
    {
        return FALSE;
    }

    ITfContext* pContext = nullptr;
    hr = pDocMgr->GetTop(&pContext);
    pDocMgr->Release();
    if (FAILED(hr) || pContext == nullptr)
    {
        return FALSE;
    }

    RECT caretRect = {}, compStartRect = {};
    BOOL hasCompStart = FALSE;
    BOOL result = CCaretEditSession::GetCaretAndCompositionStartRect(
        pContext, _tfClientId, _pComposition, &caretRect, &compStartRect, &hasCompStart,
        (LONG)GetPendingCommitPrefixLength());
    pContext->Release();

    if (result && hasCompStart)
    {
        *px = compStartRect.left;
        *py = compStartRect.bottom;
        WIND_LOG_DEBUG_FMT(L"GetCompositionStartPosition: x=%ld, y=%ld\n", *px, *py);
        return TRUE;
    }

    return FALSE;
}

BOOL CTextService::_InitLangBarButton()
{
    _pLangBarItemButton = new CLangBarItemButton(this);
    if (_pLangBarItemButton == nullptr)
        return FALSE;

    if (!_pLangBarItemButton->Initialize())
    {
        _pLangBarItemButton->Release();
        _pLangBarItemButton = nullptr;
        return FALSE;
    }

    return TRUE;
}

void CTextService::_UninitLangBarButton()
{
    if (_pLangBarItemButton != nullptr)
    {
        _pLangBarItemButton->Uninitialize();
        _pLangBarItemButton->Release();
        _pLangBarItemButton = nullptr;
    }
}

void CTextService::HandleCtrlSpaceToggle()
{
    BOOL newChineseMode = !_bChineseMode;
    WIND_LOG_INFO_FMT(L"HandleCtrlSpaceToggle: %s -> %s\n",
        _bChineseMode ? L"Chinese" : L"English",
        newChineseMode ? L"Chinese" : L"English");

    // Flush English stats before mode switch
    if (_pKeyEventSink != nullptr)
        _pKeyEventSink->FlushEnglishStats();

    // End any active composition
    EndComposition();
    ResetComposingState(TRUE);  // 中英切换保留配对状态：光标与已插入的右符号都没变

    // Notify Go service of the mode switch
    if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        ServiceResponse response;
        if (_pIPCClient->SendSystemModeSwitch(newChineseMode != FALSE, response))
        {
            if (response.type == ResponseType::CommitText && !response.text.empty())
            {
                CommitText(response.text);
                WIND_LOG_INFO_FMT(L"HandleCtrlSpaceToggle: committed pending text (len=%zu)\n", response.text.size());
            }
            if (response.type == ResponseType::StatusUpdate || response.type == ResponseType::CommitText)
                newChineseMode = response.IsChineseMode() ? TRUE : FALSE;
        }
        else
        {
            WIND_LOG_WARN(L"HandleCtrlSpaceToggle: IPC failed, proceeding with local toggle\n");
        }
    }

    _SetChineseMode(newChineseMode);

    if (_pLangBarItemButton != nullptr)
        _pLangBarItemButton->UpdateLangBarButton(_bChineseMode);

    // Do NOT call _SetOpenCloseCompartment here: compartment stays at 1 always,
    // so the system never gets a chance to desync its internal toggle state.
    // 但需要把真实模式写入 INPUTMODE_CONVERSION，让 KBLSwitch / 任务栏感知。
    _SetConversionMode(_bChineseMode);

    WIND_LOG_INFO_FMT(L"Mode toggled via Ctrl+Space interception -> %s (compartment unchanged)\n",
        _bChineseMode ? L"Chinese" : L"English");
}

void CTextService::ToggleInputMode()
{
    WIND_LOG_INFO(L"ToggleInputMode called (local fallback)\n");

    if (!_bChineseMode && _pKeyEventSink != nullptr)
    {
        _pKeyEventSink->FlushEnglishStats();
    }

    // Toggle mode locally (this is used as a fallback when Go service is unavailable)
    // The actual mode toggle is handled via KeyUp event -> Go service -> StatusUpdate response
    EndComposition();
    _SetChineseMode(!_bChineseMode);

    WIND_LOG_INFO_FMT(L"Switched to %s mode\n", _bChineseMode ? L"Chinese" : L"English");

    // Keep compartment always OPEN so TSF calls OnTestKeyDown even in English mode.
    // This allows English stats collection, auto-pair, and other features to work.
    // The actual key pass-through is handled by pfEaten=FALSE in OnTestKeyDown.
    _SetOpenCloseCompartment(TRUE);
    _SetConversionMode(_bChineseMode);

    // Update language bar button
    if (_pLangBarItemButton != nullptr)
    {
        _pLangBarItemButton->UpdateLangBarButton(_bChineseMode);
    }
}

void CTextService::SetInputMode(BOOL bChineseMode)
{
    // Set mode directly from service response (no IPC call)
    if (!_bChineseMode && bChineseMode && _pKeyEventSink != nullptr)
    {
        _pKeyEventSink->FlushEnglishStats();
    }

    _SetChineseMode(bChineseMode);

    WIND_LOG_INFO_FMT(L"Mode set to %s (from service)\n", _bChineseMode ? L"Chinese" : L"English");

    // Keep compartment always OPEN so TSF calls OnTestKeyDown even in English mode.
    _SetOpenCloseCompartment(TRUE);
    _SetConversionMode(_bChineseMode);

    // Update language bar button
    if (_pLangBarItemButton != nullptr)
    {
        _pLangBarItemButton->UpdateLangBarButton(_bChineseMode);
    }
}

void CTextService::UpdateCapsLockState(BOOL bCapsLock)
{
    if (_pLangBarItemButton != nullptr)
    {
        _pLangBarItemButton->UpdateCapsLockState(bCapsLock);
    }
}

void CTextService::SendMenuCommand(const char* command)
{
    WIND_LOG_INFO_FMT(L"SendMenuCommand: command=%hs\n", command);

    CIPCClient* pClient = _pIPCClient;
    CIPCClient* pTempClient = nullptr;

    // If main IPC client is null (Deactivate was called), create temporary connection
    if (pClient == nullptr)
    {
        WIND_LOG_INFO(L"SendMenuCommand: Main IPC null, creating temporary connection\n");
        pTempClient = new CIPCClient();
        if (pTempClient == nullptr)
        {
            WIND_LOG_ERROR(L"SendMenuCommand: Failed to create temporary IPC client\n");
            return;
        }
        if (!pTempClient->Connect())
        {
            WIND_LOG_WARN(L"SendMenuCommand: Temporary connection failed\n");
            delete pTempClient;
            return;
        }
        pClient = pTempClient;
        WIND_LOG_INFO(L"SendMenuCommand: Temporary connection established\n");
    }
    else if (!pClient->IsConnected())
    {
        // Main client exists but disconnected, try to reconnect
        WIND_LOG_INFO(L"SendMenuCommand: IPC disconnected, attempting reconnect\n");
        if (!pClient->Connect())
        {
            WIND_LOG_WARN(L"SendMenuCommand: Reconnect failed\n");
            return;
        }
        WIND_LOG_INFO(L"SendMenuCommand: Reconnected successfully\n");
    }

    // Send menu command via IPC (command is UTF-8 string)
    size_t commandLen = strlen(command);
    ServiceResponse response;
    if (pClient->SendSync(CMD_MENU_COMMAND, command, (uint32_t)commandLen, response))
    {
        WIND_LOG_INFO(L"SendMenuCommand: Command sent successfully\n");

        // Apply any status updates from response
        if (response.type == ResponseType::StatusUpdate)
        {
            BOOL bChineseMode = response.IsChineseMode();
            BOOL bFullWidth = response.IsFullWidth();
            BOOL bChinesePunct = response.IsChinesePunct();
            BOOL bToolbarVisible = response.IsToolbarVisible();
            BOOL bCapsLock = response.IsCapsLock();

            UpdateFullStatus(bChineseMode, bFullWidth, bChinesePunct, bToolbarVisible, bCapsLock,
                            response.iconLabel.empty() ? nullptr : response.iconLabel.c_str());
        }
    }
    else
    {
        WIND_LOG_WARN(L"SendMenuCommand: Failed to send command\n");
    }

    // Clean up temporary client if we created one
    if (pTempClient != nullptr)
    {
        pTempClient->Disconnect();
        delete pTempClient;
        WIND_LOG_DEBUG(L"SendMenuCommand: Temporary connection closed\n");
    }
}

void CTextService::SendShowContextMenu(int screenX, int screenY)
{
    WIND_LOG_INFO_FMT(L"SendShowContextMenu: x=%d, y=%d\n", screenX, screenY);

    CIPCClient* pClient = _pIPCClient;
    CIPCClient* pTempClient = nullptr;

    // If main IPC client is null (Deactivate was called), create temporary connection
    if (pClient == nullptr)
    {
        WIND_LOG_INFO(L"SendShowContextMenu: Main IPC null, creating temporary connection\n");
        pTempClient = new CIPCClient();
        if (pTempClient == nullptr)
        {
            WIND_LOG_ERROR(L"SendShowContextMenu: Failed to create temporary IPC client\n");
            return;
        }
        if (!pTempClient->Connect())
        {
            WIND_LOG_WARN(L"SendShowContextMenu: Temporary connection failed\n");
            delete pTempClient;
            return;
        }
        pClient = pTempClient;
        WIND_LOG_INFO(L"SendShowContextMenu: Temporary connection established\n");
    }
    else if (!pClient->IsConnected())
    {
        WIND_LOG_INFO(L"SendShowContextMenu: IPC disconnected, attempting reconnect\n");
        if (!pClient->Connect())
        {
            WIND_LOG_WARN(L"SendShowContextMenu: Reconnect failed\n");
            return;
        }
        WIND_LOG_INFO(L"SendShowContextMenu: Reconnected successfully\n");
    }

    // Build payload: int32 x + int32 y = 8 bytes
    struct {
        int32_t x;
        int32_t y;
    } payload;
    payload.x = (int32_t)screenX;
    payload.y = (int32_t)screenY;

    // Send async (fire-and-forget, Go side will show the menu)
    pClient->SendAsync(CMD_SHOW_CONTEXT_MENU, &payload, sizeof(payload));

    // Clean up temporary client if we created one
    if (pTempClient != nullptr)
    {
        pTempClient->Disconnect();
        delete pTempClient;
        WIND_LOG_DEBUG(L"SendShowContextMenu: Temporary connection closed\n");
    }
}

void CTextService::UpdateFullStatus(BOOL bChineseMode, BOOL bFullWidth, BOOL bChinesePunct, BOOL bToolbarVisible, BOOL bCapsLock, const wchar_t* iconLabel)
{
    _SetChineseMode(bChineseMode);
    _bFullWidth = bFullWidth;

    // Keep compartment always OPEN so TSF calls OnTestKeyDown even in English mode.
    _SetOpenCloseCompartment(TRUE);
    _SetConversionMode(_bChineseMode);

    if (_pLangBarItemButton != nullptr)
    {
        _pLangBarItemButton->UpdateFullStatus(bChineseMode, bFullWidth, bChinesePunct, bToolbarVisible, bCapsLock, iconLabel);
    }

    WIND_LOG_DEBUG_FMT(L"UpdateFullStatus: mode=%d, width=%d, punct=%d, toolbar=%d, caps=%d, label=%ls\n",
                 bChineseMode, bFullWidth, bChinesePunct, bToolbarVisible, bCapsLock,
                 iconLabel ? iconLabel : L"(none)");
}

// ITfCompositionSink implementation
STDAPI CTextService::OnCompositionTerminated(TfEditCookie ecWrite, ITfComposition* pComposition)
{
    // 组合被强制终止时（焦点切换、宿主 EndComposition 等），取消 HoldComposition 计时器。
    // 先记住 hold 是否活跃：活跃说明组合内容是智能符号（预上屏语义），下方须保留而非清空。
    BOOL holdWasActive = (_hHoldTimer != 0);
    CancelHoldTimer();
    // 宿主已强杀组合：待重开的余码暂存作废，避免野定时器回调开一个无主组合。
    CancelDeferredComposition();

    WIND_LOG_DEBUG(L"OnCompositionTerminated called\n");

    // Clear composition text cache
    _lastCompositionText.clear();
    _lastCaretPos = -1;

    // Only release if this is the same composition we're tracking
    // It may have already been released in DoEditSession
    if (_pComposition != nullptr && _pComposition == pComposition)
    {
        // CRITICAL: This is an unexpected termination (we didn't call EndComposition)
        // This can happen when:
        // 1. Fast typing: new composition starts before previous InsertText completes
        // 2. Application forcefully terminates composition
        //
        // We MUST clear the composition text to prevent it from leaking to the document
        // as plain text (which would cause the "d being committed directly" bug)
        //
        // 例外：智能符号 hold 中被宿主终止（切窗口等）。组合内容（prefix+held 符号）
        // 语义上是"预上屏"，宿主终止组合时文本默认 finalize 留在文档——不清空即等于
        // 提交，正是"切窗口应直接提交符号"的期望行为；照常清空反而丢字。
        if (holdWasActive)
        {
            // 文本已随 finalize 留在文档，prefix 记账必须清，防下次 CommitText 双写。
            _pendingCommitPrefix.clear();
            WIND_LOG_DEBUG(L"OnCompositionTerminated: hold active, keeping composition text (finalized as committed)\n");
        }
        else
        {
        ITfRange* pRange = nullptr;
        if (SUCCEEDED(pComposition->GetRange(&pRange)) && pRange != nullptr)
        {
            // Clear the composition text by setting it to empty
            HRESULT hr = pRange->SetText(ecWrite, 0, L"", 0);
            if (SUCCEEDED(hr))
            {
                WIND_LOG_DEBUG(L"OnCompositionTerminated: Cleared composition text (unexpected termination)\n");
            }
            else
            {
                WIND_LOG_ERROR_FMT(L"OnCompositionTerminated: SetText failed hr=0x%08X\n", hr);
            }
            pRange->Release();
        }
        }

        WIND_LOG_DEBUG(L"OnCompositionTerminated: Releasing composition\n");
        _pComposition->Release();
        _pComposition = nullptr;
        _compositionJustStarted = FALSE;

        // Notify KeyEventSink that composition was unexpectedly terminated
        // This ensures _isComposing and _hasCandidates flags are properly reset
        if (_pKeyEventSink != nullptr)
        {
            _pKeyEventSink->OnCompositionUnexpectedlyTerminated();
        }
    }
    else if (_pComposition == nullptr)
    {
        WIND_LOG_DEBUG(L"OnCompositionTerminated: Already released\n");
    }

    return S_OK;
}

// ITfTextLayoutSink - called by TSF when the text layout changes.
// This fires after the app has reflowed text (processed WM_PAINT etc.),
// so GetTextExt now returns the correct, up-to-date coordinates.
STDAPI CTextService::OnLayoutChange(ITfContext* pContext, TfLayoutCode lCode, ITfContextView* pView)
{
    if (lCode == TF_LC_CHANGE && _pComposition != nullptr)
    {
        // 首次 reflow 阶段（_compositionJustStarted）：WPS 等宿主会在 reflow 完成前
        // 连续触发多次 OnLayoutChange，前几次 GetTextExt 仍返回旧坐标。这里改用
        // debounce：每次 OnLayoutChange 都重置 timer，等事件 burst 结束后再 flush，
        // 此时 reflow 已稳定。timer 兜底也覆盖了完全不发 OnLayoutChange 的应用。
        if (_compositionJustStarted)
        {
            _hasCachedCaretPos = FALSE;
            _hasCachedCompStartPos = FALSE;
            if (_pLangBarItemButton != nullptr)
            {
                _pLangBarItemButton->PostDelayedCaretPositionUpdate();
            }
            // 首帧 reflow 期间的**试探采样**：每次 layout change 取一次坐标发给服务端。
            // DLL 一侧不做任何判断——哪一帧可信取决于 per-app 策略，而策略要读 compat.toml，
            // 那是服务端的事。实测两类宿主表现相反：EverEdit 第 1 次采样就已是 reflow 后的
            // 正确值；WPS 前两次仍是上一轮的旧坐标、第 3 次才更新。服务端据此判定。
            //
            // ⚠ 本采样的前提是宿主**会**发 OnLayoutChange，而这远非普遍：实测 Word 在 50 轮
            // 连打中一次都没发过（记事本仅首轮 1 次），它俩的组合坐标只能靠下面的 50ms timer
            // 兜底 + 异步 GetTextExt 拿到，实测要 60~190ms（Word 的 edit session 排队）。
            // 所以服务端的 fast 档必须自带短兜底（ui.candidate.fast_first_show_fallback_ms），
            // 否则在这类宿主上它会退化成 wait 档、候选窗几乎不出现。
            //
            // 仍然保留下面的 debounce + 50ms timer 兜底：本采样只是让**已启用快速首显的**宿主
            // 能提前放行，不改变默认行为（服务端默认忽略 probe），也不改变本地 composition 状态。
            // 限前 5 次：burst 长的宿主会刷 IPC，且嵌套 EditSession 有触发额外 layout change 的
            // 风险，限次数同时兜住这两点。
            if (++_firstShowProbeSeq <= 5 && _pIPCClient != nullptr && _pIPCClient->IsConnected())
            {
                RECT probeCaret = {};
                RECT probeCs = {};
                BOOL probeHasCs = FALSE;
                if (CCaretEditSession::GetCaretAndCompositionStartRect(
                        pContext, _tfClientId, _pComposition,
                        &probeCaret, &probeCs, &probeHasCs, 0))
                {
                    // 与 SendCaretPositionUpdate 同口径：y 取 bottom、height 由 rect 高度算。
                    LONG ph = probeCaret.bottom - probeCaret.top;
                    LONG csx = probeHasCs ? probeCs.left : 0;
                    LONG csy = probeHasCs ? probeCs.bottom : 0;
                    _pIPCClient->SendCaretProbe(probeCaret.left, probeCaret.bottom, ph, csx, csy);
                }
            }
            WIND_LOG_DEBUG(L"OnLayoutChange (first show): debouncing caret flush\n");
            return S_OK;
        }
        WIND_LOG_DEBUG(L"OnLayoutChange: TF_LC_CHANGE with active composition, updating caret position\n");
        SendCaretPositionUpdate();
        return S_OK;
    }
    // 无活跃组合期的 layout change 无需处理（无组合＝无候选窗要跟）。
    return S_OK;
}

void CTextService::_AdviseTextLayoutSink(ITfContext* pContext)
{
    // Unadvise previous if any
    _UnadviseTextLayoutSink();

    if (pContext == nullptr)
        return;

    ITfSource* pSource = nullptr;
    if (SUCCEEDED(pContext->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
    {
        if (SUCCEEDED(pSource->AdviseSink(IID_ITfTextLayoutSink, (ITfTextLayoutSink*)this, &_dwLayoutSinkCookie)))
        {
            _pLayoutSinkContext = pContext;
            _pLayoutSinkContext->AddRef();
            WIND_LOG_DEBUG(L"TextLayoutSink advised successfully\n");
        }
        pSource->Release();
    }
}

void CTextService::_UnadviseTextLayoutSink()
{
    if (_pLayoutSinkContext != nullptr && _dwLayoutSinkCookie != TF_INVALID_COOKIE)
    {
        ITfSource* pSource = nullptr;
        if (SUCCEEDED(_pLayoutSinkContext->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
        {
            pSource->UnadviseSink(_dwLayoutSinkCookie);
            pSource->Release();
        }
        _pLayoutSinkContext->Release();
        _pLayoutSinkContext = nullptr;
        _dwLayoutSinkCookie = TF_INVALID_COOKIE;
        WIND_LOG_DEBUG(L"TextLayoutSink unadvised\n");
    }
}

// ============================================================================
// ITfTextEditSink implementation
// ============================================================================

STDAPI CTextService::OnEndEdit(ITfContext* pContext, TfEditCookie ecReadOnly, ITfEditRecord* pEditRecord)
{
    // Always update cached prevChar (character before caret) for smart punctuation
    WCHAR prevChar = 0;

    TF_SELECTION sel[1];
    ULONG fetched = 0;
    HRESULT hr = pContext->GetSelection(ecReadOnly, TF_DEFAULT_SELECTION, 1, sel, &fetched);

    if (SUCCEEDED(hr) && fetched > 0 && sel[0].range != nullptr)
    {
        // Clone range and shift start back by 1 character to get the char before caret
        ITfRange* pRange = nullptr;
        hr = sel[0].range->Clone(&pRange);
        if (SUCCEEDED(hr) && pRange != nullptr)
        {
            LONG shifted = 0;
            hr = pRange->ShiftStart(ecReadOnly, -1, &shifted, nullptr);
            if (SUCCEEDED(hr) && shifted == -1)
            {
                WCHAR buf[2] = {0};
                ULONG charCount = 0;
                hr = pRange->GetText(ecReadOnly, 0, buf, 1, &charCount);
                if (SUCCEEDED(hr) && charCount > 0)
                {
                    prevChar = buf[0];
                }
            }
            pRange->Release();
        }
        sel[0].range->Release();
    }

    _cachedPrevChar = prevChar;

    // Check if selection changed (cursor moved)
    BOOL selChanged = FALSE;
    pEditRecord->GetSelectionStatus(&selChanged);

    // When selection changes outside of composition (e.g., mouse click, arrow keys),
    // notify Go to reset smart punct state.
    // During composition, Go tracks state internally via key events.
    // NOTE: Do NOT call ClearPassthroughDigit() here. OnEndEdit fires for normal digit
    // insertion too (cursor moves after typing '1'), which would incorrectly clear the
    // digit tracking that OnTestKeyDown just set. Mouse click detection relies on
    // caret Y comparison in _SendKeyToService instead.
    if (selChanged && _pComposition == nullptr)
    {
        // Notify Go side to reset its smart punct state
        if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
        {
            _pIPCClient->SendSelectionChanged((uint16_t)prevChar);
        }
    }

    return S_OK;
}

void CTextService::_AdviseTextEditSink(ITfContext* pContext)
{
    _UnadviseTextEditSink();

    if (pContext == nullptr)
        return;

    ITfSource* pSource = nullptr;
    if (SUCCEEDED(pContext->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
    {
        if (SUCCEEDED(pSource->AdviseSink(IID_ITfTextEditSink, (ITfTextEditSink*)this, &_dwTextEditSinkCookie)))
        {
            _pTextEditSinkContext = pContext;
            _pTextEditSinkContext->AddRef();
            WIND_LOG_DEBUG(L"TextEditSink advised successfully\n");
        }
        pSource->Release();
    }
}

void CTextService::_UnadviseTextEditSink()
{
    if (_pTextEditSinkContext != nullptr && _dwTextEditSinkCookie != TF_INVALID_COOKIE)
    {
        ITfSource* pSource = nullptr;
        if (SUCCEEDED(_pTextEditSinkContext->QueryInterface(IID_ITfSource, (void**)&pSource)) && pSource != nullptr)
        {
            pSource->UnadviseSink(_dwTextEditSinkCookie);
            pSource->Release();
        }
        _pTextEditSinkContext->Release();
        _pTextEditSinkContext = nullptr;
        _dwTextEditSinkCookie = TF_INVALID_COOKIE;
        WIND_LOG_DEBUG(L"TextEditSink unadvised\n");
    }
}

// Update composition text
BOOL CTextService::UpdateComposition(const std::wstring& text, int caretPos, BOOL noUnderline)
{
    // 顶码聚合（微软 IME 行为）：组合实际显示 = 待提交前缀 + 引擎组合文本，
    // 光标位置随前缀偏移。前缀为空时与原行为完全一致。
    std::wstring full = _pendingCommitPrefix + text;
    int fullCaret = (caretPos >= 0) ? (int)(_pendingCommitPrefix.length() + caretPos) : caretPos;

    WIND_LOG_DEBUG_FMT(L"UpdateComposition called, textLen=%zu, prefixLen=%zu, _pComposition=%p\n",
                 text.length(), _pendingCommitPrefix.length(), _pComposition);

    // 优化：文本与光标都与上次相同则跳过，避免多余的 RequestEditSession
    //（必须同时比较光标，否则左右移动光标会被跳过）
    if (full == _lastCompositionText && fullCaret == _lastCaretPos && _pComposition != nullptr)
    {
        WIND_LOG_DEBUG(L"UpdateComposition: Skipping duplicate (same text and caret)\n");
        return TRUE;
    }

    // Need a document manager
    ITfDocumentMgr* pDocMgr = nullptr;
    if (_pThreadMgr == nullptr || FAILED(_pThreadMgr->GetFocus(&pDocMgr)) || pDocMgr == nullptr)
    {
        WIND_LOG_ERROR(L"UpdateComposition: Failed to get DocMgr\n");
        return FALSE;
    }

    ITfContext* pContext = nullptr;
    HRESULT hr = pDocMgr->GetTop(&pContext);
    pDocMgr->Release();

    if (FAILED(hr) || pContext == nullptr)
    {
        WIND_LOG_ERROR(L"UpdateComposition: Failed to get Context\n");
        return FALSE;
    }

    CUpdateCompositionEditSession* pEditSession = new CUpdateCompositionEditSession(this, pContext, full, fullCaret, noUnderline);

    // Timing: measure RequestEditSession duration
    LARGE_INTEGER startTime, endTime, freq;
    QueryPerformanceCounter(&startTime);
    QueryPerformanceFrequency(&freq);

    HRESULT hrSession;
    hr = pContext->RequestEditSession(_tfClientId, pEditSession, TF_ES_ASYNCDONTCARE | TF_ES_READWRITE, &hrSession);

    QueryPerformanceCounter(&endTime);
    int durationMs = (int)((endTime.QuadPart - startTime.QuadPart) * 1000 / freq.QuadPart);

    // Track if this was async (Weasel optimization pattern)
    _asyncEdit = (hrSession == TF_S_ASYNC);

    WIND_LOG_DEBUG_FMT(L"UpdateComposition: RequestEditSession hr=0x%08X, hrSession=0x%08X, async=%d, duration=%dms\n",
                 hr, hrSession, _asyncEdit ? 1 : 0, durationMs);

    pEditSession->Release();
    pContext->Release();

    // 成功后更新去重缓存（缓存的是含前缀的完整显示文本）
    if (SUCCEEDED(hr))
    {
        _lastCompositionText = full;
        _lastCaretPos = fullCaret;
    }

    return SUCCEEDED(hr);
}

// 把光标前 count 个已上屏字符替换为 text（智能符号纠错替换）。
//
// 默认优先走 TSF 同步范围替换（CReplaceBackwardEditSession：ShiftStart+SetText，
// 原子、不发任何按键）——这是通用性最好的方案，不依赖修饰键是否松开：用户按住
// Shift 连续输入多个符号（如连按 Shift+1 多次）也不受影响。失败才回退到真实合成
// 按键（Backspace × count + Unicode 注入 text，已通过 MarkSyntheticKey 防止被自己
// 的 OnTestKeyDown 钩子二次处理）。
//
// 已知例外（2026-07 实测，Tabby/微信）：这两个宿主自制的 TSFTextStore（Chromium
// 内嵌 / Qt）会对 ShiftStart+SetText 全程报告成功（hr、hrSession、GetSuccess() 皆
// S_OK），但实际画面上旧符号没删掉、新符号又插入了一份——不是我们这边 range 算错，
// 是宿主自己的 TSFTextStore 跟它真实渲染的内容对不上，靠更严格检查 TSF 返回码也
// 识别不出来。反过来，全局默认改用合成按键又会在 EverEdit 这类应用里撞上 Shift
// 类标点的发键抑制问题（Shift 还按着时发退格，被宿主解读成别的操作，同样表现为
// 重复上屏），且用户连续按住 Shift 输入多个符号时无法排队处理。两条路径互有短板，
// 目前没有对所有宿主都成立的通用方案，因此保留 TSF 优先作为默认（覆盖面更广），
// Tabby/微信类宿主的问题如果后续真机测试仍然存在，再按宿主进程名特判走合成按键
// （kTryTsfRangeReplace 换成按 host 判断），而不是全局切换。
BOOL CTextService::ReplacePrecedingChars(int count, const std::wstring& text)
{
    if (count <= 0)
    {
        // 无删除需求，等价于直接上屏。
        return CommitText(text);
    }

    _lastCompositionText.clear();
    _lastCaretPos = -1;

    constexpr bool kTryTsfRangeReplace = true;
    if (kTryTsfRangeReplace)
    {
        ITfDocumentMgr* pDocMgr = nullptr;
        if (_pThreadMgr != nullptr && SUCCEEDED(_pThreadMgr->GetFocus(&pDocMgr)) && pDocMgr != nullptr)
        {
            ITfContext* pContext = nullptr;
            HRESULT hr = pDocMgr->GetTop(&pContext);
            pDocMgr->Release();

            if (SUCCEEDED(hr) && pContext != nullptr)
            {
                CReplaceBackwardEditSession* pEditSession =
                    new CReplaceBackwardEditSession(this, pContext, count, text);

                HRESULT hrSession;
                hr = pContext->RequestEditSession(_tfClientId, pEditSession,
                                                  TF_ES_SYNC | TF_ES_READWRITE, &hrSession);
                BOOL success = pEditSession->GetSuccess();
                pEditSession->Release();
                pContext->Release();

                // 同 CommitText：只信 GetSuccess()，不要求外层 hr/hrSession 也成功。
                // 部分终端模拟器的 TSF 支持不完整，RequestEditSession 外层可能报告失败，
                // 但 DoEditSession 里的 ShiftStart+SetText 其实已经原子替换成功；若仍
                // 按 hr/hrSession 判失败去做 SendInput 退格+重打兜底，就会在已经替换
                // 正确的文字上再删再打一遍，表现为"文字被复制"。
                if (success)
                {
                    WIND_LOG_DEBUG_FMT(L"ReplacePrecedingChars: TSF range replace succeeded count=%d\n", count);
                    return TRUE;
                }
                WIND_LOG_DEBUG_FMT(L"ReplacePrecedingChars: TSF failed (hr=0x%08X, hrSession=0x%08X), falling back to SendInput\n",
                                   hr, hrSession);
            }
        }
    }

    // 真实合成按键：count 次 Backspace + Unicode 注入 text。
    // 注入前须把每个键标记为"自生成"（MarkSyntheticKey），否则这些合成按键会被
    // 自己的 OnTestKeyDown 钩子当成真实用户按键二次处理：在 TSF EditSession 本身
    // 就走不通的宿主（终端模拟器/微信/部分纯文本编辑器）里，退格没被真正放行、
    // 新字符又被当作"新按键"重新走一遍输入流程，表现为替换后的符号重复上屏。
    std::vector<INPUT> inputs;
    inputs.reserve((size_t)count * 2 + text.length() * 2);
    for (int i = 0; i < count; i++)
    {
        if (_pKeyEventSink != nullptr) _pKeyEventSink->MarkSyntheticKey(VK_BACK);

        INPUT down = {};
        down.type = INPUT_KEYBOARD;
        down.ki.wVk = VK_BACK;
        inputs.push_back(down);

        INPUT up = {};
        up.type = INPUT_KEYBOARD;
        up.ki.wVk = VK_BACK;
        up.ki.dwFlags = KEYEVENTF_KEYUP;
        inputs.push_back(up);
    }
    for (wchar_t ch : text)
    {
        if (_pKeyEventSink != nullptr) _pKeyEventSink->MarkSyntheticKey(VK_PACKET);

        INPUT down = {};
        down.type = INPUT_KEYBOARD;
        down.ki.wScan = ch;
        down.ki.dwFlags = KEYEVENTF_UNICODE;
        inputs.push_back(down);

        INPUT up = {};
        up.type = INPUT_KEYBOARD;
        up.ki.wScan = ch;
        up.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
        inputs.push_back(up);
    }
    if (!inputs.empty())
    {
        SendInput((UINT)inputs.size(), inputs.data(), sizeof(INPUT));
    }
    WIND_LOG_DEBUG_FMT(L"ReplacePrecedingChars: SendInput fallback count=%d textLen=%zu\n", count, text.length());
    return TRUE;
}

// Commit text atomically: end composition + insert text in a single EditSession.
// This avoids race conditions in browsers where async EndComposition could clear
// text that was inserted by a subsequent synchronous InsertText.
BOOL CTextService::CommitText(const std::wstring& text, BOOL fromHoldTimer, BOOL replacingHeld)
{
    // hold 预览态活跃时（智能符号已把中文符号放进组合、等 press2），本次提交必须交代
    // 那个符号的去向——下面提交走的是**组合 range 的 SetText**，range 里此刻显示的正是
    // 它，不主动处置就会被静默覆盖掉。
    //
    //   replacingHeld=TRUE （press2）：本就是要拿英文符号换掉它 → 丢弃，让 SetText 覆盖。
    //   replacingHeld=FALSE（其余一切）：并入 prefix，与本次文本一起上屏（追加语义）。
    //
    // 默认取追加：hold 期间可能触发提交的路径远不止一处（全角空格/数字、临时英文、各
    // 独占模式出字……），把安全的一侧设为默认，新增路径自动正确。曾经默认丢弃，表现为
    // 全角下「。」+空格 → 符号消失、只剩全角空格。
    //
    // fromHoldTimer 时计时器已在 OnHoldTimerExpired 里清零、文本也已 move 进 text 参数，
    // 两个分支在那条路上都是无副作用的 no-op。
    if (replacingHeld)
        CancelHoldTimer();
    else
        AbsorbHeldIntoPrefix();

    // 顶码聚合：真正提交 = 待提交前缀 + 本次文本（微软 IME 的延迟提交在此收口）。
    std::wstring full = _pendingCommitPrefix + text;
    _pendingCommitPrefix.clear();

    LARGE_INTEGER startTime, endTime, freq;
    QueryPerformanceCounter(&startTime);
    QueryPerformanceFrequency(&freq);

    // Clear composition text cache
    _lastCompositionText.clear();
    _lastCaretPos = -1;

    // Transfer ownership of _pComposition to the EditSession
    ITfComposition* pCompToEnd = _pComposition;
    _pComposition = nullptr;

    if (full.empty() && pCompToEnd == nullptr)
    {
        WIND_LOG_DEBUG(L"CommitText: Nothing to do (no text, no composition)\n");
        return TRUE;
    }

    // Need a document manager to request edit session
    ITfDocumentMgr* pDocMgr = nullptr;
    if (_pThreadMgr == nullptr || FAILED(_pThreadMgr->GetFocus(&pDocMgr)) || pDocMgr == nullptr)
    {
        WIND_LOG_DEBUG(L"CommitText: Can't get DocMgr, falling back\n");
        if (pCompToEnd != nullptr) pCompToEnd->Release();
        goto fallback;
    }

    {
        ITfContext* pContext = nullptr;
        HRESULT hr = pDocMgr->GetTop(&pContext);
        pDocMgr->Release();

        if (FAILED(hr) || pContext == nullptr)
        {
            WIND_LOG_DEBUG(L"CommitText: Can't get Context, falling back\n");
            if (pCompToEnd != nullptr) pCompToEnd->Release();
            goto fallback;
        }

        CCommitTextEditSession* pEditSession = new CCommitTextEditSession(this, pContext, pCompToEnd, full);
        // pCompToEnd ownership transferred to pEditSession

        if (fromHoldTimer)
        {
            // 智能符号 HoldComposition 超时收口跑在裸 WM_TIMER 回调里，不在按键/编辑
            // 上下文中。Word 对此校验严格，会拒发同步会话（hrSession=TS_E_SYNCHRONOUS
            // 0x80040208），DoEditSession 根本不执行 → 组合被当孤儿 Release（Word 默认把
            // 组合里已显示的符号 finalize 落进文档）→ 旧逻辑再见 GetSuccess()==FALSE 就走
            // SendInput 兜底又打一遍，造成"符号 500ms 后重复上屏"（d5d5815 只治了"谎报
            // 失败"，治不了这里的"真拒绝同步"）。
            //
            // 改用异步会话：超时时最终文字已在组合态里显示，交给 TSF 在能拿到锁时原地
            // SetText+EndComposition 落定即可，绝不 SendInput。TF_ES_ASYNCDONTCARE 在能立即
            // 给锁的宿主（如 Tabby）会同步执行、行为不变；Word 则延后到可授予锁时执行。
            // 异步下 pEditSession 由 TSF 保活至 DoEditSession 运行，pCompToEnd 随之正确收尾。
            HRESULT hrSession = S_OK;
            hr = pContext->RequestEditSession(_tfClientId, pEditSession,
                                              TF_ES_ASYNCDONTCARE | TF_ES_READWRITE, &hrSession);
            pEditSession->Release();
            pContext->Release();

            if (SUCCEEDED(hr))
            {
                WIND_LOG_DEBUG_FMT(L"CommitText(holdTimer): async commit requested, hrSession=0x%08X\n", hrSession);
                return TRUE;
            }
            // 极少数：异步请求本身就被拒（组合已随会话析构释放）。落到 SendInput 末路兜底。
            WIND_LOG_DEBUG_FMT(L"CommitText(holdTimer): async request rejected hr=0x%08X, falling back to SendInput\n", hr);
        }
        else
        {
            HRESULT hrSession;
            hr = pContext->RequestEditSession(_tfClientId, pEditSession, TF_ES_SYNC | TF_ES_READWRITE, &hrSession);

            BOOL success = pEditSession->GetSuccess();
            pEditSession->Release();
            pContext->Release();

            QueryPerformanceCounter(&endTime);
            int durationMs = (int)((endTime.QuadPart - startTime.QuadPart) * 1000 / freq.QuadPart);

            // 只信 DoEditSession 内部的 GetSuccess()——它只在 SetText/EndComposition 真正
            // 执行完毕后才置 TRUE，是文档是否已被修改的唯一可信信号。外层 hr/hrSession 在
            // 部分宿主里会出现"编辑其实已经执行、但外层返回码不达标"的情况；若仍要求三者
            // 同时成功，会误判为失败并接着走 SendInput 兜底，在已经正确写入的文字后面再打
            // 一遍。（Word 超时路径的"真拒绝同步"另见上面 fromHoldTimer 分支。）
            if (success)
            {
                WIND_LOG_DEBUG_FMT(L"CommitText: TSF atomic commit succeeded, duration=%dms\n", durationMs);
                return TRUE;
            }

            WIND_LOG_DEBUG_FMT(L"CommitText: TSF method failed (hr=0x%08X, hrSession=0x%08X), falling back to SendInput, duration=%dms\n",
                         hr, hrSession, durationMs);
        }
    }

fallback:
    // 兜底：用 SendInput 注入（与 InsertText 的兜底路径一致）
    if (full.empty())
    {
        return TRUE;
    }

    WIND_LOG_DEBUG_FMT(L"CommitText: Using SendInput fallback for textLen=%zu\n", full.length());

    std::vector<INPUT> inputs;
    inputs.reserve(full.length() * 2);

    for (wchar_t ch : full)
    {
        // 标记为自生成，避免被自己的 OnTestKeyDown 钩子当成真实按键二次处理
        // （同 ReplacePrecedingChars 兜底路径，见其注释）。
        if (_pKeyEventSink != nullptr) _pKeyEventSink->MarkSyntheticKey(VK_PACKET);

        INPUT inputDown = {};
        inputDown.type = INPUT_KEYBOARD;
        inputDown.ki.wVk = 0;
        inputDown.ki.wScan = ch;
        inputDown.ki.dwFlags = KEYEVENTF_UNICODE;
        inputs.push_back(inputDown);

        INPUT inputUp = {};
        inputUp.type = INPUT_KEYBOARD;
        inputUp.ki.wVk = 0;
        inputUp.ki.wScan = ch;
        inputUp.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
        inputs.push_back(inputUp);
    }

    UINT sent = SendInput((UINT)inputs.size(), inputs.data(), sizeof(INPUT));
    if (sent != inputs.size())
    {
        WIND_LOG_WARN_FMT(L"CommitText: SendInput sent %u of %u inputs\n", sent, (UINT)inputs.size());
    }

    return TRUE;
}

// FOCUS_LOST_REASON_* 的日志名。日志文案与协议值共用一处，避免两边各写一份而说反
// （TextService.cpp 的 "notifying service" 就是这么错了一年）。
static const wchar_t* FocusLostReasonName(uint8_t reason)
{
    switch (reason)
    {
    case FOCUS_LOST_REASON_THREAD:      return L"thread_focus_lost";
    case FOCUS_LOST_REASON_DOC_CHANGED: return L"doc_changed";
    case FOCUS_LOST_REASON_CTX_LOST:    return L"ctx_lost";
    default:                            return L"?";
    }
}

// 输入态整体清理。**只应由「离开了原来那个文档」的两条路径调用**（OnKillThreadFocus /
// doc_changed），不要挂回失焦回调：DocMgr 级失焦是噪声信号（VSCode 实测一次应用切换伴随
// 5 次 DocMgr 焦点事件，Excel 更是同一指针 6ms 内掉了又回），在那里销毁用户输入正是
// 「首字符直接上屏」的根因。
// DocMgr 级失焦要通知服务端隐藏工具栏时，走 _ReportEditContextLost() 而**不是**本函数——
// 那条路径只翻可见性标志、不碰输入态。
void CTextService::CleanupInputStateForDocChange(ITfDocumentMgr* pDocMgrHint, uint8_t reason,
                                                 BOOL sendFocusLost)
{
    WIND_LOG_DEBUG_FMT(L"CleanupInputStateForDocChange reason=%ls hint=0x%p focusLostSent=%d sendLost=%d",
                       FocusLostReasonName(reason), pDocMgrHint, _focusLostSent ? 1 : 0, sendFocusLost ? 1 : 0);

    // 先结束 composition 再发 focus_lost：EndComposition 会清空 composition 范围的文本，
    // 顺序反了则服务端已清 buffer 而宿主里仍留着未清的 composition 文本。
    EndComposition(pDocMgrHint);

    // sendFocusLost=FALSE：本地清理照做，但**不通知服务端失焦**。用于「新 DocMgr 会被
    // XamlIsland locked 守卫跳过 focus_gained」的情形——发了就没人配对，服务端的
    // ime_active 会被永久清掉（见 OnSetFocus doc_changed 分支注释）。
    if (sendFocusLost && !_focusLostSent && _pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        _pIPCClient->SendFocusLost(reason);
        _focusLostSent = TRUE;
        // 本函数发出的两种 reason（THREAD / DOC_CHANGED）都意味着「已经不在原来那个可编辑
        // 上下文里了」，故一并复位上报态：否则紧接着的 DocMgr 失焦会再补一条 CTX_LOST。
        _editCtxReported = FALSE;
    }

    if (_pKeyEventSink != nullptr)
    {
        // 失焦一律复位，**配对状态也不例外**（keepPair 取默认 FALSE）。
        // 曾按 reason 细分保留过（THREAD 时光标其实还在括号中间），2026-07-29 真机后放弃：
        // 本 DLL 的 _pairPendingDepth 是**每个宿主进程各自一份**的，而 core 侧是全局单栈，
        // 两种作用域模型对不齐；开启「为每个应用配置不同输入法」后切换应用还会重建整个
        // IME 上下文。实测「大部分情况不行」——功能时灵时不灵比没有更糟，故收敛为确定行为。
        _pKeyEventSink->ResetComposingState();
    }
}

// 焦点离开可编辑控件（DocMgr 级失焦）时通知服务端隐藏工具栏。
//
// 与 CleanupInputStateForDocChange 的分工是本次设计的要点：本函数**只翻可见性标志，
// 绝不碰输入态**。这正是它能在 DocMgr 噪声层安全调用的原因——Excel 那种「同一 DocMgr
// 6ms 内掉了又回」的抖动，最多让工具栏闪一下（UI 层 50ms 隐藏防抖会吸收），而输入缓冲
// 毫发无损。反过来，若在这里调 CleanupInputStateForDocChange，就是把「首字符不进编码、
// 直接上屏」原样请回来。
//
// **不设 _focusLostSent**：那个标志表示「真失焦已上报」，供 OnKillThreadFocus 去重。
// CTX_LOST 不是真失焦（应用还在前台、输入法仍激活），置位会让随后真正的
// thread_focus_lost 被吞掉，ime_active 就永远清不掉了。
//
// 靠 _editCtxReported 去重：DocMgr 级失焦实测可达 60~98 次/秒，每次都发会造成 IPC 洪泛。
// 只在「上报过有可编辑上下文 → 现在没有了」这个翻转沿发一次。
void CTextService::_ReportEditContextLost()
{
    if (!_editCtxReported)
    {
        return; // 本来就没上报过有上下文，无需再说一遍
    }
    _editCtxReported = FALSE;

    if (_pIPCClient != nullptr && _pIPCClient->IsConnected())
    {
        _pIPCClient->SendFocusLost(FOCUS_LOST_REASON_CTX_LOST);
    }
}

// End composition
// NOTE: This method is now ASYNC - it returns immediately without waiting for
// the composition to actually end. The _pComposition pointer is cleared immediately
// so that HasActiveComposition() returns FALSE and new compositions can start.
void CTextService::EndComposition(ITfDocumentMgr* pDocMgrHint)
{
    // direct_commit 顶码：失焦/强制收口前，先把待重开的余码组合落定，语义与下方
    // HoldComposition flush 一致——收口时不能让余码组合悬空未开。
    StartDeferredCompositionIfPending();

    // 智能符号 hold 中（符号预上屏观感，语义=待提交）：主动结束组合（失焦/Deactivate
    // 等）时转为提交而非放弃，模拟标准输入流程——切换窗口时符号应直接上屏而不是清空。
    // CommitText 内 full = prefix + held，聚合定格的旧符号一并收口。
    if (_hHoldTimer != 0 && _pComposition != nullptr)
    {
        WIND_LOG_DEBUG(L"EndComposition: flushing held smart symbol as commit\n");
        FlushHoldCompositionIfActive();
        return;
    }

    // 顶码聚合中（组合头部有待提交前缀）：不能按「放弃组合」处理——引擎已把
    // 顶出的字按上屏记账（词频/统计）。转为提交前缀、丢弃余码（CommitText 的
    // EditSession 会 SetText(前缀)+EndComposition）。
    // 注：此路径使用当前焦点 context 提交；失焦竞态下可能落到兜底 SendInput。
    if (!_pendingCommitPrefix.empty() && _pComposition != nullptr)
    {
        WIND_LOG_DEBUG(L"EndComposition: flushing pending top-code prefix as commit\n");
        CommitText(L"");
        return;
    }

    LARGE_INTEGER startTime, endTime, freq;
    QueryPerformanceCounter(&startTime);
    QueryPerformanceFrequency(&freq);

    // Clear composition text cache
    _lastCompositionText.clear();
    _lastCaretPos = -1;

    // 无组合但有残留前缀（异常路径）：直接丢弃，防止污染下一次组合。
    _pendingCommitPrefix.clear();

    // If there's no active composition, nothing to do
    if (_pComposition == nullptr)
    {
        WIND_LOG_DEBUG(L"EndComposition: No active composition\n");
        return;
    }

    WIND_LOG_DEBUG(L"EndComposition: Ending active composition\n");

    // CRITICAL: Transfer ownership of _pComposition immediately
    // This allows new compositions to start while the old one is being ended async
    ITfComposition* pCompToEnd = _pComposition;
    _pComposition = nullptr;  // Clear immediately - HasActiveComposition() now returns FALSE
    _compositionJustStarted = FALSE;

    // Need a document manager to request edit session.
    // pDocMgrHint 一旦给出即**具有权威性**，不再只是 GetFocus 失败时的兜底：
    // composition 属于创建它的那个 context，调用方（CleanupInputStateForDocChange）
    // 明确知道是哪一个。而此刻 GetFocus() 可能已经指向**新**文档（doc_changed 路径就是
    // 在新文档拿到焦点后才收口的），照它去跑 EditSession 会拿着新 context 的 cookie 去
    // 清旧 context 的 range —— 轻则失败，重则动到新文档的内容。
    // 无 hint 时（其余调用点均在焦点未变时触发）仍回落 GetFocus。
    ITfDocumentMgr* pDocMgr = nullptr;
    if (pDocMgrHint != nullptr)
    {
        WIND_LOG_DEBUG(L"EndComposition: using pDocMgrHint (authoritative)\n");
        pDocMgr = pDocMgrHint;
        pDocMgr->AddRef();
    }
    else if (_pThreadMgr == nullptr || FAILED(_pThreadMgr->GetFocus(&pDocMgr)) || pDocMgr == nullptr)
    {
        // Can't get document manager, force cleanup
        WIND_LOG_DEBUG(L"EndComposition: Can't get DocMgr, forcing cleanup\n");
        pCompToEnd->Release();
        return;
    }

    ITfContext* pContext = nullptr;
    HRESULT hr = pDocMgr->GetTop(&pContext);
    pDocMgr->Release();

    if (FAILED(hr) || pContext == nullptr)
    {
        // Can't get context, force cleanup
        WIND_LOG_DEBUG(L"EndComposition: Can't get Context, forcing cleanup\n");
        pCompToEnd->Release();
        return;
    }

    // Create edit session with ownership transfer of pCompToEnd
    CEndCompositionEditSession* pEditSession = new CEndCompositionEditSession(this, pCompToEnd);

    HRESULT hrSession;
    // Use TF_ES_ASYNCDONTCARE for non-blocking operation
    // The edit session will complete asynchronously, and pCompToEnd will be
    // released in DoEditSession or in ~CEndCompositionEditSession if the request fails
    hr = pContext->RequestEditSession(_tfClientId, pEditSession, TF_ES_ASYNCDONTCARE | TF_ES_READWRITE, &hrSession);

    QueryPerformanceCounter(&endTime);
    int durationMs = (int)((endTime.QuadPart - startTime.QuadPart) * 1000 / freq.QuadPart);
    WIND_LOG_DEBUG_FMT(L"EndComposition: RequestEditSession hr=0x%08X, hrSession=0x%08X, duration=%dms\n",
                 hr, hrSession, durationMs);

    if (FAILED(hr))
    {
        // Request failed - pEditSession destructor will release pCompToEnd
        WIND_LOG_DEBUG(L"EndComposition: RequestEditSession failed\n");
    }

    pEditSession->Release();
    pContext->Release();
}

void CTextService::ResetComposingState(BOOL keepPairState)
{
    if (_pKeyEventSink != nullptr)
    {
        _pKeyEventSink->ResetComposingState(keepPairState);
    }
}

// Insert text and start new composition (for top code commit)
BOOL CTextService::InsertTextAndStartComposition(const std::wstring& insertText, const std::wstring& newComposition)
{
    WIND_LOG_DEBUG_FMT(L"InsertTextAndStartComposition: insert='%s', newComp='%s', prefixLen=%zu, _pComposition=%p\n",
                 insertText.c_str(), newComposition.c_str(), _pendingCommitPrefix.length(), _pComposition);

    // 顶码聚合（微软 IME 行为，经 Chrome 事件探针实测微软五笔确认）：顶出的文本
    // 不立即提交文档，而是累入待提交前缀、留在组合头部显示（无下划线段），真正的
    // 提交推迟到最终上屏（CommitText）一次完成。宿主全程只看到 compositionupdate、
    // 最后一次 compositionend —— 与微软五笔事件流完全一致，快照式宿主（tabby/
    // xterm.js 读 textarea 快照）与 diff 式宿主（Chromium TSFTextStore）都不会
    // 把余码并入提交（此前 EndComposition→StartComposition 各种拆法均双写余码）。
    //
    // 有活动组合：纯组合内容更新（'skce' → '可能y'）；无活动组合（罕见）：
    // UpdateComposition 会新建组合并显示 前缀+余码。
    _pendingCommitPrefix += insertText;
    return UpdateComposition(newComposition, (int)newComposition.length());
}

// ============================================================================
// ITfDisplayAttributeProvider implementation
// ============================================================================

STDAPI CTextService::EnumDisplayAttributeInfo(IEnumTfDisplayAttributeInfo** ppEnum)
{
    if (ppEnum == nullptr)
        return E_INVALIDARG;

    *ppEnum = new CEnumDisplayAttributeInfo();
    return (*ppEnum != nullptr) ? S_OK : E_OUTOFMEMORY;
}

STDAPI CTextService::GetDisplayAttributeInfo(REFGUID guid, ITfDisplayAttributeInfo** ppInfo)
{
    if (ppInfo == nullptr)
        return E_INVALIDARG;

    *ppInfo = nullptr;

    if (IsEqualGUID(guid, c_guidDisplayAttributeInput))
    {
        *ppInfo = new CDisplayAttributeInfoInput();
        return (*ppInfo != nullptr) ? S_OK : E_OUTOFMEMORY;
    }

    return E_INVALIDARG;
}

// ============================================================================
// Display Attribute initialization
// ============================================================================

BOOL CTextService::_InitDisplayAttribute()
{
    // Get category manager
    ITfCategoryMgr* pCategoryMgr = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_TF_CategoryMgr, nullptr, CLSCTX_INPROC_SERVER,
                                   IID_ITfCategoryMgr, (void**)&pCategoryMgr);
    if (FAILED(hr) || pCategoryMgr == nullptr)
    {
        WIND_LOG_ERROR(L"Failed to create category manager\n");
        return FALSE;
    }

    // Register display attribute GUID
    hr = pCategoryMgr->RegisterGUID(c_guidDisplayAttributeInput, &_gaDisplayAttributeInput);
    if (FAILED(hr))
    {
        WIND_LOG_ERROR(L"Failed to register display attribute GUID\n");
        pCategoryMgr->Release();
        return FALSE;
    }

    WIND_LOG_DEBUG_FMT(L"Display attribute registered, atom=%lu\n", (unsigned long)_gaDisplayAttributeInput);

    pCategoryMgr->Release();
    return TRUE;
}

void CTextService::_UninitDisplayAttribute()
{
    // Reset the GUID atom
    _gaDisplayAttributeInput = TF_INVALID_GUIDATOM;
}

// ─── HoldComposition ─────────────────────────────────────────────────────────

BOOL CTextService::HoldComposition(const std::wstring& text, UINT timeoutMs)
{
    WIND_LOG_DEBUG_FMT(L"HoldComposition: text=%s timeoutMs=%u\n",
                       text.c_str(), timeoutMs);

    // 旧 held 符号定格并入 prefix（不 commit、不动文档），与新符号在同一次
    // UpdateComposition（单一 EditSession）内完成显示更新。曾用「先 CommitText 旧符号
    // 再开新组合」——Chromium TSFTextStore（微信等）按整锁 diff 会把 commit 与紧随的
    // 新组合合并解读成替换、WPS 同步锁失败走 SendInput 乱序，表现为后一个符号顶掉
    // 前一个（与顶码双写 7f616c2 同根，同用聚合方案修复）。
    AbsorbHeldIntoPrefix();

    // 将中文符号放入 TSF 组合态（caretPos = 文本长度，光标置末；显示 = prefix + text）。
    // noUnderline：符号观感与已上屏一致（预上屏），实际仍在组合态内可替换。
    if (!UpdateComposition(text, static_cast<int>(text.length()), TRUE))
    {
        WIND_LOG_ERROR(L"HoldComposition: UpdateComposition failed\n");
        return FALSE;
    }

    _heldCompositionText = text;
    g_holdTimerInstance  = this;
    _hHoldTimer = SetTimer(NULL, 0, timeoutMs, HoldTimerProc);

    if (_hHoldTimer == 0)
    {
        WIND_LOG_ERROR(L"HoldComposition: SetTimer failed\n");
        g_holdTimerInstance  = nullptr;
        _heldCompositionText.clear();
        return FALSE;
    }

    WIND_LOG_DEBUG_FMT(L"HoldComposition: timer started id=%llu\n",
                       static_cast<unsigned long long>(_hHoldTimer));
    return TRUE;
}

void CTextService::FlushHoldCompositionIfActive()
{
    if (_hHoldTimer != 0)
        OnHoldTimerExpired();
}

// 把当前 held 的智能符号定格并入 _pendingCommitPrefix（不 commit、不动文档）。
// 用于「定格旧符号 + 立即更新/开启组合」场景（连续智能符号、符号后快速输入）：
// 宿主只见组合文本更新，最终由 CommitText 一次收口（full = prefix + text），
// 规避「commit + 立即重启组合」在 Chromium/WPS 下的替换误读。
// 定格后的符号不可再被 press2 替换——语义上已承诺提交，与服务端状态机一致
// （press2 只作用于最新 armed 的符号）。
void CTextService::AbsorbHeldIntoPrefix()
{
    if (_hHoldTimer == 0)
        return;

    _pendingCommitPrefix += _heldCompositionText;
    KillTimer(NULL, _hHoldTimer);
    _hHoldTimer          = 0;
    _heldCompositionText.clear();
    g_holdTimerInstance  = nullptr;
    WIND_LOG_DEBUG_FMT(L"AbsorbHeldIntoPrefix: held symbol pinned, prefixLen=%zu\n",
                       _pendingCommitPrefix.length());
}

void CTextService::CancelHoldTimer()
{
    if (_hHoldTimer == 0)
        return;

    KillTimer(NULL, _hHoldTimer);
    WIND_LOG_DEBUG_FMT(L"CancelHoldTimer: killed timer id=%llu\n",
                       static_cast<unsigned long long>(_hHoldTimer));
    _hHoldTimer          = 0;
    _heldCompositionText.clear();
    g_holdTimerInstance  = nullptr;
}

// static
VOID CALLBACK CTextService::HoldTimerProc(HWND /*hwnd*/, UINT /*uMsg*/,
                                           UINT_PTR idEvent, DWORD /*dwTime*/)
{
    if (g_holdTimerInstance != nullptr
        && idEvent == g_holdTimerInstance->_hHoldTimer)
    {
        // 真正的 WM_TIMER 回调：拿不到同步编辑会话，须异步收口。
        g_holdTimerInstance->OnHoldTimerExpired(TRUE);
    }
}

void CTextService::OnHoldTimerExpired(BOOL fromTimerCallback)
{
    WIND_LOG_DEBUG_FMT(L"OnHoldTimerExpired: committing chinese text, fromTimerCallback=%d\n",
                       fromTimerCallback);

    UINT_PTR timerId = _hHoldTimer;
    std::wstring textToCommit = std::move(_heldCompositionText);
    _hHoldTimer         = 0;
    g_holdTimerInstance = nullptr;
    KillTimer(NULL, timerId);

    // 仅 WM_TIMER 回调路径走异步会话收口（不走 SendInput 兜底，见 CommitText 内注释）；
    // Flush 路径（PassThrough/失焦）在按键同步上下文，保持同步以确保收口先于后续字符。
    CommitText(textToCommit, fromTimerCallback);
}

// ─── DeferredComposition（direct_commit 顶码，延迟到 keyup 才开新组合）───────────

// 延迟组合（direct_commit 顶码）定时器的 thread_local 实例指针。
static thread_local CTextService* g_deferredTimerInstance = nullptr;

void CTextService::StashDeferredComposition(const std::wstring& composition, UINT fallbackMs)
{
    // 异常保护：若已有待重开的余码，先把旧的落定，避免丢失。
    StartDeferredCompositionIfPending();

    _deferredCompText = composition;
    g_deferredTimerInstance = this;
    _hDeferredTimer = SetTimer(NULL, 0, fallbackMs, DeferredTimerProc);

    if (_hDeferredTimer == 0)
    {
        WIND_LOG_ERROR(L"StashDeferredComposition: SetTimer failed\n");
        g_deferredTimerInstance = nullptr;
        _deferredCompText.clear();
        return;
    }

    WIND_LOG_DEBUG_FMT(L"StashDeferredComposition: text=%s fallbackMs=%u timer=%llu\n",
                       composition.c_str(), fallbackMs,
                       static_cast<unsigned long long>(_hDeferredTimer));
}

void CTextService::StartDeferredCompositionIfPending()
{
    if (_deferredCompText.empty())
    {
        // 仅残留定时器（理论不达）也一并清掉。
        if (_hDeferredTimer != 0) { KillTimer(NULL, _hDeferredTimer); _hDeferredTimer = 0; }
        g_deferredTimerInstance = nullptr;
        return;
    }
    std::wstring text = std::move(_deferredCompText);
    _deferredCompText.clear();
    if (_hDeferredTimer != 0) { KillTimer(NULL, _hDeferredTimer); _hDeferredTimer = 0; }
    g_deferredTimerInstance = nullptr;

    WIND_LOG_DEBUG_FMT(L"StartDeferredComposition: opening new composition text=%s\n", text.c_str());
    // 此刻 CommitText 已结束旧组合、_pComposition 为空 → UpdateComposition 新建组合并显示余码
    //（有下划线的正常编码态）。对齐真实输入法 compositionstart@keyup。
    UpdateComposition(text, static_cast<int>(text.length()));
}

void CTextService::CancelDeferredComposition()
{
    if (_hDeferredTimer != 0) { KillTimer(NULL, _hDeferredTimer); _hDeferredTimer = 0; }
    _deferredCompText.clear();
    g_deferredTimerInstance = nullptr;
}

// static
VOID CALLBACK CTextService::DeferredTimerProc(HWND, UINT, UINT_PTR idEvent, DWORD)
{
    if (g_deferredTimerInstance != nullptr
        && idEvent == g_deferredTimerInstance->_hDeferredTimer)
    {
        WIND_LOG_DEBUG(L"DeferredTimerProc: keyup 未达，兜底开余码组合\n");
        g_deferredTimerInstance->StartDeferredCompositionIfPending();
    }
}
