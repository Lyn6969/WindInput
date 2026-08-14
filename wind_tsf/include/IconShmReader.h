#pragma once

#include <windows.h>
#include <vector>

// 语言栏图标共享内存读端。
//
// 服务端把当前状态预渲染成多档位图写进 SHM（见 wind-bridge 的 icon_shm_windows.rs），
// 本类在 GetIcon 里按本进程的 DPI 与任务栏主题挑一档取回。
//
// ## 为什么读端在这里而不是等推送
//
// GetIcon 是**被动的同步 COM 回调**——系统什么时候要图标由它决定，我们不能在回调里
// 发起 IPC 等回应（会卡住宿主）。共享内存把这件事变成一次纯内存拷贝：数据在状态
// 变化时就已经备好，回调只管取。因此这里不需要 Event，也不需要后台线程。
//
// ## 并发
//
// 双缓冲 + seqlock。读 sequence → 拷贝 → 重读 sequence；两次相同才说明拷贝期间
// 没发生过发布，拿到的是一致快照。不相同则重试，重试仍失败就让调用方退回本地绘制。
class CIconShmReader
{
public:
    CIconShmReader();
    ~CIconShmReader();

    CIconShmReader(const CIconShmReader&) = delete;
    CIconShmReader& operator=(const CIconShmReader&) = delete;

    // 取回最接近 desiredSizePx 的变体。
    //
    // 成功时 outPixels 是 BGRA（**非预乘**，CreateIconIndirect 的 hbmColor 约定），
    // outSizePx 是实际选中的边长——**可能不等于 desiredSizePx**，调用方须按它建位图。
    //
    // 返回 false 的全部情形都意味着「这次用不了预渲染图标」，调用方应退回本地绘制：
    // 服务未启动 / SHM 尚未发布过内容 / 头部校验不过 / 拷贝期间被并发发布打断。
    bool ReadVariant(int desiredSizePx, bool darkTheme,
                     std::vector<BYTE>& outPixels, int& outSizePx);

private:
    // 懒打开。失败不记忆——服务可能晚于宿主进程启动，下次 GetIcon 再试即可自愈。
    // OpenFileMappingW 失败开销只有几微秒，而 GetIcon 频率极低，不值得为此加重试节流。
    bool _EnsureOpen();
    void _Close();

    HANDLE _hMap;
    const volatile BYTE* _pView;
};
