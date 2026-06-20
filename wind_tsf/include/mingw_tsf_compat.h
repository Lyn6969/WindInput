// ============================================================================
// mingw_tsf_compat.h — MinGW(GCC) TSF 兼容垫片
// ----------------------------------------------------------------------------
// 目的：MinGW-w64 自带的 <msctf.h> / <ctfutb.h> 不完整，缺少一批 TSF COM 接口、
//       类别 GUID 与常量；而 Windows SDK(MSVC) 头是完整的。本文件仅在 MinGW
//       交叉编译时补齐这些缺失声明，使同一份 C++ 源码既能用 MSVC/Windows SDK
//       编译，也能用 MinGW 在 Linux 上交叉编译，且业务源码零改动。
//
//   * 本垫片整体被 #ifdef __MINGW32__ 包裹 —— MSVC 构建时本文件完全为空，
//     不影响 Windows SDK 原生编译路径。
//   * 接口 vtable 顺序、IID、GUID 值均取自权威来源并交叉校验：
//       - 接口 IID / vtable 顺序：windows crate 0.58（win32metadata 自动生成）
//       - 类别/区间 GUID 值：windows crate 0.58 + TSF-TypeLib（已用 4 个已知值交叉验证）
//       - 常量数值：windows crate 0.58
//     详见 docs/redesign/tsf-migration.md「MinGW 兼容垫片」一节。
//   * GUID 的「定义」集中在 src/mingw_tsf_compat.cpp（INITGUID），本头仅做声明。
// ============================================================================
#pragma once

#ifdef __MINGW32__

#include <windows.h>
#include <msctf.h>
#include <ctfutb.h>

// PACKAGE_FAMILY_NAME_MAX_LENGTH：appmodel.h 提供 GetPackageFamilyName，但该常量
// 在 MinGW 被拆到 minappmodel.h —— 而该头的 include guard 写反(#ifdef 而非 #ifndef)
// 且整体被 WINAPI_PARTITION_APP(UWP) 守卫，桌面构建始终拿不到。直接补定义。
// 值 = PACKAGE_NAME_MAX_LENGTH(50) + 1 + PACKAGE_PUBLISHERID_MAX_LENGTH(13) = 64，
// 与 Windows SDK 一致。
#ifndef PACKAGE_FAMILY_NAME_MAX_LENGTH
#define PACKAGE_FAMILY_NAME_MAX_LENGTH 64
#endif

// ----------------------------------------------------------------------------
// 枚举（MinGW 缺失）
// ----------------------------------------------------------------------------
typedef enum _TfLBIClick {
    TF_LBI_CLK_RIGHT = 1,
    TF_LBI_CLK_LEFT  = 2
} TfLBIClick;

typedef enum _TfLayoutCode {
    TF_LC_CREATE  = 0,
    TF_LC_CHANGE  = 1,
    TF_LC_DESTROY = 2
} TfLayoutCode;

// ----------------------------------------------------------------------------
// 哨兵常量（MinGW 缺失）
// ----------------------------------------------------------------------------
#ifndef TF_INVALID_GUIDATOM
#define TF_INVALID_GUIDATOM ((TfGuidAtom)0)
#endif
#ifndef TF_CLIENTID_NULL
#define TF_CLIENTID_NULL ((TfClientId)0)
#endif
// TF_S_ASYNC：旧版 MinGW msctf.h 缺失（14.x 才补；CI 的 apt mingw-w64 11.x 无）。
// 与 textstor.h 的 TS_S_ASYNC 同值（FACILITY_ITF, 0x0300）。异步编辑会话标记用。
#ifndef TF_S_ASYNC
#define TF_S_ASYNC MAKE_HRESULT(SEVERITY_SUCCESS, FACILITY_ITF, 0x0300)
#endif

// ----------------------------------------------------------------------------
// 语言栏项信息/样式标志（TF_LBI_*，MinGW 缺失）
// ----------------------------------------------------------------------------
#ifndef TF_LBI_ICON
#define TF_LBI_ICON    0x00000001
#endif
#ifndef TF_LBI_TEXT
#define TF_LBI_TEXT    0x00000002
#endif
#ifndef TF_LBI_TOOLTIP
#define TF_LBI_TOOLTIP 0x00000004
#endif
#ifndef TF_LBI_STATUS
#define TF_LBI_STATUS  0x00010000
#endif
#ifndef TF_LBI_STYLE_SHOWNINTRAY
#define TF_LBI_STYLE_SHOWNINTRAY  0x00000002
#endif
#ifndef TF_LBI_STYLE_TEXTCOLORICON
#define TF_LBI_STYLE_TEXTCOLORICON 0x00000020
#endif
#ifndef TF_LBI_STYLE_BTN_BUTTON
#define TF_LBI_STYLE_BTN_BUTTON   0x00010000
#endif
#ifndef TF_LBI_STYLE_BTN_MENU
#define TF_LBI_STYLE_BTN_MENU     0x00020000
#endif

// ----------------------------------------------------------------------------
// 候选列表 UIElement 更新标志（TF_CLUIE_*，MinGW 缺失）
// ----------------------------------------------------------------------------
#ifndef TF_CLUIE_DOCUMENTMGR
#define TF_CLUIE_DOCUMENTMGR 0x00000001
#endif
#ifndef TF_CLUIE_COUNT
#define TF_CLUIE_COUNT       0x00000002
#endif
#ifndef TF_CLUIE_SELECTION
#define TF_CLUIE_SELECTION   0x00000004
#endif
#ifndef TF_CLUIE_STRING
#define TF_CLUIE_STRING      0x00000008
#endif
#ifndef TF_CLUIE_PAGEINDEX
#define TF_CLUIE_PAGEINDEX   0x00000010
#endif
#ifndef TF_CLUIE_CURRENTPAGE
#define TF_CLUIE_CURRENTPAGE 0x00000020
#endif

// ----------------------------------------------------------------------------
// COM 接口（MinGW 缺失）—— vtable 顺序与 Windows SDK 一致，不可调整。
// 这些接口都不被 __uuidof 引用，故无需 __declspec(uuid)/__CRT_UUID_DECL，
// 仅需正确的纯虚函数表 + 下方对应的 IID_ 命名 GUID。
// ----------------------------------------------------------------------------

// ITfTextInputProcessorEx : ITfTextInputProcessor（+ ActivateEx）
struct ITfTextInputProcessorEx : public ITfTextInputProcessor {
    virtual HRESULT STDMETHODCALLTYPE ActivateEx(ITfThreadMgr *ptim, TfClientId tid, DWORD dwFlags) = 0;
};

// ITfDisplayAttributeProvider : IUnknown
struct ITfDisplayAttributeProvider : public IUnknown {
    virtual HRESULT STDMETHODCALLTYPE EnumDisplayAttributeInfo(IEnumTfDisplayAttributeInfo **ppEnum) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetDisplayAttributeInfo(REFGUID guid, ITfDisplayAttributeInfo **ppInfo) = 0;
};

// ITfTextLayoutSink : IUnknown
struct ITfTextLayoutSink : public IUnknown {
    virtual HRESULT STDMETHODCALLTYPE OnLayoutChange(ITfContext *pic, TfLayoutCode lcode, ITfContextView *pView) = 0;
};

// ITfMenu : IUnknown（仅 AddMenuItem，与 SDK 一致）
struct ITfMenu : public IUnknown {
    virtual HRESULT STDMETHODCALLTYPE AddMenuItem(UINT uId, DWORD dwFlags, HBITMAP hbmp, HBITMAP hbmpMask,
                                                  const WCHAR *pch, ULONG cch, ITfMenu **ppMenu) = 0;
};

// ITfLangBarItemButton : ITfLangBarItem
struct ITfLangBarItemButton : public ITfLangBarItem {
    virtual HRESULT STDMETHODCALLTYPE OnClick(TfLBIClick click, POINT pt, const RECT *prcArea) = 0;
    virtual HRESULT STDMETHODCALLTYPE InitMenu(ITfMenu *pMenu) = 0;
    virtual HRESULT STDMETHODCALLTYPE OnMenuSelect(UINT wID) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetIcon(HICON *phIcon) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetText(BSTR *pbstrText) = 0;
};

// ITfCandidateListUIElement : ITfUIElement
struct ITfCandidateListUIElement : public ITfUIElement {
    virtual HRESULT STDMETHODCALLTYPE GetUpdatedFlags(DWORD *pdwFlags) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetDocumentMgr(ITfDocumentMgr **ppdim) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetCount(UINT *puCount) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetSelection(UINT *puIndex) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetString(UINT uIndex, BSTR *pstr) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetPageIndex(UINT *pIndex, UINT uSize, UINT *puPageCnt) = 0;
    virtual HRESULT STDMETHODCALLTYPE SetPageIndex(UINT *pIndex, UINT uPageCnt) = 0;
    virtual HRESULT STDMETHODCALLTYPE GetCurrentPage(UINT *puPage) = 0;
};

// ITfCandidateListUIElementBehavior : ITfCandidateListUIElement
struct ITfCandidateListUIElementBehavior : public ITfCandidateListUIElement {
    virtual HRESULT STDMETHODCALLTYPE SetSelection(UINT nIndex) = 0;
    virtual HRESULT STDMETHODCALLTYPE Finalize(void) = 0;
    virtual HRESULT STDMETHODCALLTYPE Abort(void) = 0;
};

// ----------------------------------------------------------------------------
// IID / 类别 GUID 声明（定义见 src/mingw_tsf_compat.cpp）
// 仅声明 MinGW 缺失的；MinGW 已有的（如 GUID_TFCAT_TIP_KEYBOARD、
// GUID_TFCAT_DISPLAYATTRIBUTEPROVIDER 等）不在此重复，避免重定义。
// ----------------------------------------------------------------------------
EXTERN_C const GUID IID_ITfTextInputProcessorEx;
EXTERN_C const GUID IID_ITfDisplayAttributeProvider;
EXTERN_C const GUID IID_ITfTextLayoutSink;
EXTERN_C const GUID IID_ITfMenu;
EXTERN_C const GUID IID_ITfLangBarItemButton;
EXTERN_C const GUID IID_ITfCandidateListUIElement;
EXTERN_C const GUID IID_ITfCandidateListUIElementBehavior;

EXTERN_C const GUID GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION;
EXTERN_C const GUID GUID_TFCAT_CATEGORY_OF_TIP;
EXTERN_C const GUID GUID_TFCAT_DISPLAYATTRIBUTEPROPERTY;
EXTERN_C const GUID GUID_TFCAT_PROP_AUDIODATA;
EXTERN_C const GUID GUID_TFCAT_PROP_INKDATA;
EXTERN_C const GUID GUID_TFCAT_PROPSTYLE_STATIC;
// 注：GUID_TFCAT_PROPSTYLE_CUSTOM / GUID_TFCAT_PROPSTYLE_STATICCOMPACT 这两个 legacy
// 文本属性样式类别在所有可得权威源（官方 Win32 元数据/mingw-w64/Wine/ReactOS）均已
// 移除，无可信值可用。它们与键盘 TIP 的 Win11/UWP 兼容性无关，故 MinGW 构建在
// Register.cpp 中用 #ifndef __MINGW32__ 跳过其注册；MSVC 构建经 uuid.lib 仍完整保留。
EXTERN_C const GUID GUID_TFCAT_TIPCAP_COMLESS;
EXTERN_C const GUID GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT;
EXTERN_C const GUID GUID_TFCAT_TIPCAP_WOW16;

#endif // __MINGW32__
