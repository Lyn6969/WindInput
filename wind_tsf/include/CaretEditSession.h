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

    // Get the result after DoEditSession is called
    BOOL GetResult(RECT* prc);

    // Set composition to also query its start position
    void SetComposition(ITfComposition* pComposition) { _pComposition = pComposition; }
    // 组合起点偏移（wchar 数）：组合头部有顶码待提交前缀时，上报的组合起点
    // 应指向余码段起点（候选窗锚点跟随余码，而非已顶出的文字）。
    void SetCompositionStartOffset(LONG offset) { _compStartOffset = offset; }
    BOOL GetCompositionStartResult(RECT* prc);

private:
    LONG _refCount;
    ITfContext* _pContext;
    ITfComposition* _pComposition;
    LONG _compStartOffset;
    RECT _caretRect;
    RECT _compositionStartRect;
    BOOL _hasCompositionStart;
    BOOL _succeeded;
};
