#pragma once

#include "Globals.h"

class CTextService;

// EditSession for getting caret position using TSF APIs
// This is required to call ITfContextView::GetTextExt which needs an edit cookie
class CCaretEditSession : public ITfEditSession
{
public:
    CCaretEditSession(ITfContext* pContext);
    ~CCaretEditSession();

    // IUnknown
    STDMETHODIMP QueryInterface(REFIID riid, void** ppvObj);
    STDMETHODIMP_(ULONG) AddRef();
    STDMETHODIMP_(ULONG) Release();

    // ITfEditSession
    STDMETHODIMP DoEditSession(TfEditCookie ec);

    // Execute the session and get caret position
    // Returns TRUE if successful, FALSE otherwise
    static BOOL GetCaretRect(ITfContext* pContext, TfClientId tfClientId, RECT* prc);

    // Execute the session and get both caret position and composition start position
    // compStartOffset: 组合起点偏移（wchar 数），见 SetCompositionStartOffset
    static BOOL GetCaretAndCompositionStartRect(ITfContext* pContext, TfClientId tfClientId,
                                                 ITfComposition* pComposition,
                                                 RECT* pCaretRect, RECT* pCompStartRect, BOOL* pHasCompStart,
                                                 LONG compStartOffset = 0);

    // 异步取坐标：用 TF_ES_ASYNCDONTCARE 请求锁，结果经 pOwner->OnAsyncCaretRectReady 回调返回。
    //
    // 上面两个同步入口用的 TF_ES_SYNC 只在**按键处理期间**可以期待成功——这是 MSDN 对该标志的
    // 明文限制（"should only be used in documented situations (such as keystroke handling)"）。
    // 在 WM_TIMER、OnLayoutChange 这类非按键上下文里，宿主可以合法地拒绝同步锁并返回
    // TS_E_SYNCHRONOUS（Word 实测 15/15 全拒），此时必须走异步：宿主会把请求排队，等文档可用
    // 时再回调 DoEditSession，而不是当场失败。
    //
    // 返回 TRUE 表示请求已被受理（可能已同步执行完，也可能排队等待回调），FALSE 表示发起失败。
    static BOOL RequestCaretRectAsync(ITfContext* pContext, TfClientId tfClientId,
                                       ITfComposition* pComposition, LONG compStartOffset,
                                       CTextService* pOwner);

    // Get the result after DoEditSession is called
    BOOL GetResult(RECT* prc);

    // Set composition to also query its start position
    void SetComposition(ITfComposition* pComposition) { _pComposition = pComposition; }
    // 组合起点偏移（wchar 数）：组合头部有顶码待提交前缀时，上报的组合起点
    // 应指向余码段起点（候选窗锚点跟随余码，而非已顶出的文字）。
    void SetCompositionStartOffset(LONG offset) { _compStartOffset = offset; }
    BOOL GetCompositionStartResult(RECT* prc);
    // 设为异步模式并持有 owner 强引用；见 RequestCaretRectAsync
    void SetAsyncOwner(CTextService* pOwner);

private:
    LONG _refCount;
    ITfContext* _pContext;
    ITfComposition* _pComposition;
    LONG _compStartOffset;
    RECT _caretRect;
    RECT _compositionStartRect;
    BOOL _hasCompositionStart;
    BOOL _succeeded;
    // 非空 = 异步模式：DoEditSession 完成后直接回调它，因为异步执行时静态入口早已返回、
    // 调用方拿不到结果。持有强引用（AddRef/Release），避免回调到达前 owner 被销毁。
    CTextService* _pAsyncOwner;
};
