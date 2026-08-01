#include "CaretEditSession.h"
#include "TextService.h"
#include "Globals.h"

CCaretEditSession::CCaretEditSession(ITfContext* pContext)
    : _refCount(1)
    , _pContext(pContext)
    , _pComposition(nullptr)
    , _compStartOffset(0)
    , _hasCompositionStart(FALSE)
    , _succeeded(FALSE)
    , _usedCompStartAsCaret(FALSE)
    , _pAsyncOwner(nullptr)
{
    if (_pContext)
    {
        _pContext->AddRef();
    }
    ZeroMemory(&_caretRect, sizeof(_caretRect));
    ZeroMemory(&_compositionStartRect, sizeof(_compositionStartRect));
}

CCaretEditSession::~CCaretEditSession()
{
    SafeRelease(_pContext);
    SafeRelease(_pAsyncOwner);
}

STDAPI CCaretEditSession::QueryInterface(REFIID riid, void** ppvObj)
{
    if (ppvObj == nullptr)
        return E_INVALIDARG;

    *ppvObj = nullptr;

    if (IsEqualIID(riid, IID_IUnknown) || IsEqualIID(riid, IID_ITfEditSession))
    {
        *ppvObj = (ITfEditSession*)this;
    }

    if (*ppvObj)
    {
        AddRef();
        return S_OK;
    }

    return E_NOINTERFACE;
}

STDAPI_(ULONG) CCaretEditSession::AddRef()
{
    return InterlockedIncrement(&_refCount);
}

STDAPI_(ULONG) CCaretEditSession::Release()
{
    LONG cr = InterlockedDecrement(&_refCount);

    if (cr == 0)
    {
        delete this;
    }

    return cr;
}

STDAPI CCaretEditSession::DoEditSession(TfEditCookie ec)
{
    _succeeded = FALSE;

    if (!_pContext)
    {
        WIND_LOG_ERROR(L"CaretEditSession: Context is null\n");
        return E_FAIL;
    }

    // Get the active view
    ITfContextView* pContextView = nullptr;
    HRESULT hr = _pContext->GetActiveView(&pContextView);
    if (FAILED(hr) || pContextView == nullptr)
    {
        WIND_LOG_ERROR(L"CaretEditSession: Failed to get active view\n");
        return hr;
    }

    // Get the current selection
    TF_SELECTION sel[1];
    ULONG fetched = 0;
    hr = _pContext->GetSelection(ec, TF_DEFAULT_SELECTION, 1, sel, &fetched);

    if (SUCCEEDED(hr) && fetched > 0 && sel[0].range != nullptr)
    {
        // Get the text extent of the selection (caret position)
        BOOL clipped = FALSE;
        hr = pContextView->GetTextExt(ec, sel[0].range, &_caretRect, &clipped);

        if (SUCCEEDED(hr))
        {
            _succeeded = TRUE;

            WIND_LOG_DEBUG_FMT(L"CaretEditSession: Got caret rect (%ld, %ld, %ld, %ld) clipped=%d\n",
                      _caretRect.left, _caretRect.top, _caretRect.right, _caretRect.bottom, clipped);
        }
        else
        {
            WIND_LOG_ERROR_FMT(L"CaretEditSession: GetTextExt failed hr=0x%08X\n", hr);
        }

        sel[0].range->Release();

        // If a composition is set, also get the start position of the composition range
        //
        // ⚠ 这里**不再受 _succeeded 守卫**（2026-08-01）：caret(selection) 取失败或退化时，
        // 组合起点往往仍然有效——实测 shell 的临时输入小窗，selection 恒返回退化矩形
        // (2559,1367,2560,1367) h=0，而 composition range 给出有效的 (473,189,473,217)。
        // 那时它是手上唯一可信的坐标，原先却因为这个守卫压根不去取。
        if (_pComposition != nullptr)
        {
            ITfRange* pCompRange = nullptr;
            hr = _pComposition->GetRange(&pCompRange);
            if (SUCCEEDED(hr) && pCompRange != nullptr)
            {
                // Clone the range and collapse to the start
                ITfRange* pStartRange = nullptr;
                hr = pCompRange->Clone(&pStartRange);
                if (SUCCEEDED(hr) && pStartRange != nullptr)
                {
                    pStartRange->Collapse(ec, TF_ANCHOR_START);
                    // 有顶码待提交前缀时，组合起点偏移到余码段起点
                    if (_compStartOffset > 0)
                    {
                        LONG moved = 0;
                        pStartRange->ShiftEnd(ec, _compStartOffset, &moved, nullptr);
                        pStartRange->ShiftStart(ec, _compStartOffset, &moved, nullptr);
                    }
                    BOOL clippedComp = FALSE;
                    hr = pContextView->GetTextExt(ec, pStartRange, &_compositionStartRect, &clippedComp);
                    if (SUCCEEDED(hr))
                    {
                        _hasCompositionStart = TRUE;
                        // 打全 4 值 + clipped：与上面 caret rect 同口径，便于直接比对两者差值
                        // （caret 落在组合内容之后、compStart 锚在组合头部，两者之差 = 已插入的
                        // 组合宽度）。只打 left/bottom 时看不出该 rect 是否退化（top==bottom），
                        // 而退化正是宿主尚未 reflow 的信号。
                        WIND_LOG_DEBUG_FMT(L"CaretEditSession: Composition start rect (%ld, %ld, %ld, %ld) clipped=%d\n",
                                  _compositionStartRect.left, _compositionStartRect.top,
                                  _compositionStartRect.right, _compositionStartRect.bottom, clippedComp);
                    }
                    else
                    {
                        // 失败即 compStart=(0,0) 上报，Rust 侧据此判定「本轮 reflow 坐标未到」而继续
                        // 等待。此前无日志，表现为「组合起点一直锁不上」却查不到原因。
                        WIND_LOG_DEBUG_FMT(L"CaretEditSession: Composition start GetTextExt failed hr=0x%08X\n", hr);
                    }
                    pStartRange->Release();
                }
                pCompRange->Release();
            }
        }

        // ★ 锚点降级：caret 无效而组合起点有效时，用组合起点当 caret。
        //
        // 候选窗本来就该跟随「正在编辑的那段文本」，而不是插入点——两者只差一个组合宽度。
        // 原先的行为是让上层判定 caret 无效后下坠到 GUIThreadInfo，去取一个**属于别的窗口**
        // 的系统光标（shell 场景实测取到任务栏残留的 (0,1388)，与真实位置差 1171px），
        // 那才是真正的错位。手上既然有同一个 edit session、同一个 cookie 下语义精确的矩形，
        // 就没有理由舍近求远。
        const LONG caretHeight = _caretRect.bottom - _caretRect.top;
        const LONG compStartHeight = _compositionStartRect.bottom - _compositionStartRect.top;
        if ((!_succeeded || caretHeight <= 0) && _hasCompositionStart && compStartHeight > 0)
        {
            WIND_LOG_DEBUG_FMT(L"CaretEditSession: caret 无效(succeeded=%d h=%ld)，降级用组合起点 (%ld, %ld, %ld, %ld)\n",
                               _succeeded, caretHeight,
                               _compositionStartRect.left, _compositionStartRect.top,
                               _compositionStartRect.right, _compositionStartRect.bottom);
            _caretRect = _compositionStartRect;
            _succeeded = TRUE;
            _usedCompStartAsCaret = TRUE;

            // 顺带探测本 context 自己的显示区域。GetScreenExt 是 TSF 语义内的参照系，不依赖
            // 窗口层级与前台状态；若实测可靠，将来可取代「所有显示器」做越界校验——「前台窗口」
            // 那个参照已被 shell 场景证伪。只在降级时打，避免每帧噪音。
            RECT rcScreen = {};
            if (SUCCEEDED(pContextView->GetScreenExt(&rcScreen)))
            {
                WIND_LOG_DEBUG_FMT(L"CaretEditSession: context GetScreenExt = (%ld, %ld, %ld, %ld)\n",
                                   rcScreen.left, rcScreen.top, rcScreen.right, rcScreen.bottom);
            }
            else
            {
                WIND_LOG_DEBUG(L"CaretEditSession: context GetScreenExt failed\n");
            }
        }
    }
    else
    {
        // No selection, try to get the end of the document or use insertion point
        WIND_LOG_DEBUG(L"CaretEditSession: No selection available\n");

        // Try to get screen extent as fallback
        hr = pContextView->GetScreenExt(&_caretRect);
        if (SUCCEEDED(hr))
        {
            // Use the top-left of the screen extent as a fallback
            _caretRect.right = _caretRect.left + 2;
            _caretRect.bottom = _caretRect.top + 20;
            _succeeded = TRUE;
            WIND_LOG_DEBUG(L"CaretEditSession: Using screen extent as fallback\n");
        }
    }

    pContextView->Release();

    // 异步模式：结果只能从这里出去——静态入口在排队执行时早已返回。
    // 失败时**不回调**：服务端会继续等自己的兜底超时，用按键时缓存的坐标显示，
    // 那份坐标来自按键路径的同步 edit session，比任何回退值都可信。
    if (_pAsyncOwner != nullptr)
    {
        if (_succeeded)
        {
            _pAsyncOwner->OnAsyncCaretRectReady(_caretRect, _hasCompositionStart, _compositionStartRect,
                                                _usedCompStartAsCaret);
        }
        else
        {
            WIND_LOG_DEBUG(L"CaretEditSession(async): no rect obtained, not notifying owner\n");
        }
    }

    return _succeeded ? S_OK : E_FAIL;
}

BOOL CCaretEditSession::GetResult(RECT* prc)
{
    if (_succeeded && prc)
    {
        *prc = _caretRect;
        return TRUE;
    }
    return FALSE;
}

void CCaretEditSession::SetAsyncOwner(CTextService* pOwner)
{
    SafeRelease(_pAsyncOwner);
    _pAsyncOwner = pOwner;
    if (_pAsyncOwner)
    {
        _pAsyncOwner->AddRef();
    }
}

BOOL CCaretEditSession::GetCompositionStartResult(RECT* prc)
{
    if (_hasCompositionStart && prc)
    {
        *prc = _compositionStartRect;
        return TRUE;
    }
    return FALSE;
}

// Static method to get both caret rect and composition start rect
BOOL CCaretEditSession::GetCaretAndCompositionStartRect(ITfContext* pContext, TfClientId tfClientId,
                                                         ITfComposition* pComposition,
                                                         RECT* pCaretRect, RECT* pCompStartRect, BOOL* pHasCompStart,
                                                         LONG compStartOffset,
                                                         BOOL* pUsedCompStartAsCaret)
{
    if (pUsedCompStartAsCaret)
    {
        *pUsedCompStartAsCaret = FALSE;
    }
    if (pContext == nullptr || pCaretRect == nullptr)
    {
        return FALSE;
    }

    CCaretEditSession* pEditSession = new CCaretEditSession(pContext);
    if (pEditSession == nullptr)
    {
        return FALSE;
    }

    pEditSession->SetComposition(pComposition);
    pEditSession->SetCompositionStartOffset(compStartOffset);

    HRESULT hrSession = S_OK;
    HRESULT hr = pContext->RequestEditSession(
        tfClientId,
        pEditSession,
        TF_ES_SYNC | TF_ES_READ,
        &hrSession
    );

    BOOL result = FALSE;
    if (SUCCEEDED(hr) && SUCCEEDED(hrSession))
    {
        result = pEditSession->GetResult(pCaretRect);
        if (pCompStartRect && pHasCompStart)
        {
            *pHasCompStart = pEditSession->GetCompositionStartResult(pCompStartRect);
        }
        if (pUsedCompStartAsCaret)
        {
            *pUsedCompStartAsCaret = pEditSession->UsedCompStartAsCaret();
        }
    }
    else
    {
        WIND_LOG_ERROR_FMT(L"RequestEditSession failed hr=0x%08X, hrSession=0x%08X\n", hr, hrSession);
    }

    pEditSession->Release();
    return result;
}

// Static method to request the caret rect asynchronously (see header for why)
BOOL CCaretEditSession::RequestCaretRectAsync(ITfContext* pContext, TfClientId tfClientId,
                                               ITfComposition* pComposition, LONG compStartOffset,
                                               CTextService* pOwner)
{
    if (pContext == nullptr || pOwner == nullptr)
    {
        return FALSE;
    }

    CCaretEditSession* pEditSession = new CCaretEditSession(pContext);
    if (pEditSession == nullptr)
    {
        return FALSE;
    }

    pEditSession->SetComposition(pComposition);
    pEditSession->SetCompositionStartOffset(compStartOffset);
    pEditSession->SetAsyncOwner(pOwner);

    HRESULT hrSession = S_OK;
    HRESULT hr = pContext->RequestEditSession(
        tfClientId,
        pEditSession,
        TF_ES_ASYNCDONTCARE | TF_ES_READ,
        &hrSession
    );

    // 释放我们这一份引用。异步排队时 TSF 自己持有一份，对象活到 DoEditSession 回调完成为止。
    pEditSession->Release();

    if (FAILED(hr))
    {
        WIND_LOG_ERROR_FMT(L"RequestCaretRectAsync: RequestEditSession failed hr=0x%08X\n", hr);
        return FALSE;
    }

    // hrSession == TF_S_ASYNC 表示已排队、回调稍后到达；S_OK 表示 manager 选择了同步执行，
    // 此时回调已经在上面的调用里跑完了。两者都算受理成功。
    WIND_LOG_DEBUG_FMT(L"RequestCaretRectAsync: accepted hrSession=0x%08X (%s)\n",
                       hrSession,
                       hrSession == TF_S_ASYNC ? L"queued" : L"executed inline");
    return TRUE;
}
