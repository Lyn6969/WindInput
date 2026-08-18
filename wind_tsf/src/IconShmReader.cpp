#include "IconShmReader.h"
#include "BinaryProtocol.h"
#include "Globals.h"

#include <climits>
#include <cstdlib>
#include <cstring>

// SHM 名里的 `_v1`（Globals.h 的 WIND_ICON_SHM_NAME）与 ICON_SHM_VERSION 是同一个
// 版本，但一个在宽字符串字面量里、一个是整数，没法直接比对。退而求其次把当前值钉死：
// 改版本号时这里编译失败，强制回去同步那个名字。
//
// 漏改的后果无声——Rust 侧的 icon_shm_name 会自动带上新版本，本端还在开旧名字，
// OpenFileMappingW 恒失败，图标退回本地绘制照常显示、只是永远不跟随标点变化。
static_assert(ICON_SHM_VERSION == 1,
              "改了 ICON_SHM_VERSION 必须同步 Globals.h 里 WIND_ICON_SHM_NAME 的 _v1");

// 从映射里读一个 4 字节小端量。
//
// volatile 是必需的：seqlock 依赖「先读 sequence、拷贝、再读 sequence」这个顺序，
// 而两次读的是同一地址且中间没有本线程的写——编译器有权把第二次读优化成复用第一次
// 的结果，那样校验就恒真、撕裂检测彻底失效。
static inline uint32_t ReadU32(const volatile BYTE* base, size_t off)
{
    return *reinterpret_cast<const volatile uint32_t*>(base + off);
}

static inline uint16_t ReadU16(const volatile BYTE* base, size_t off)
{
    return *reinterpret_cast<const volatile uint16_t*>(base + off);
}

CIconShmReader::CIconShmReader()
    : _hMap(NULL)
    , _pView(nullptr)
{
}

CIconShmReader::~CIconShmReader()
{
    _Close();
}

bool CIconShmReader::_EnsureOpen()
{
    if (_pView != nullptr)
        return true;

    HANDLE hMap = OpenFileMappingW(FILE_MAP_READ, FALSE, WIND_ICON_SHM_NAME);
    if (hMap == NULL)
    {
        // 常态而非异常：服务尚未启动时每次 GetIcon 都会走到这里。
        // 不记日志——GetIcon 由系统按需回调，记了会刷屏。
        return false;
    }

    const void* view = MapViewOfFile(hMap, FILE_MAP_READ, 0, 0, ICON_SHM_SIZE);
    if (view == nullptr)
    {
        WIND_LOG_WARN_FMT(L"IconShm: MapViewOfFile failed, err=%u\n", GetLastError());
        CloseHandle(hMap);
        return false;
    }

    _hMap = hMap;
    _pView = static_cast<const volatile BYTE*>(view);
    WIND_LOG_INFO_FMT(L"IconShm: opened %ls\n", WIND_ICON_SHM_NAME);
    return true;
}

void CIconShmReader::_Close()
{
    if (_pView != nullptr)
    {
        UnmapViewOfFile(const_cast<const void*>(reinterpret_cast<const volatile void*>(_pView)));
        _pView = nullptr;
    }
    if (_hMap != NULL)
    {
        CloseHandle(_hMap);
        _hMap = NULL;
    }
}

bool CIconShmReader::ReadVariant(int desiredSizePx, bool darkTheme,
                                 std::vector<BYTE>& outPixels, int& outSizePx,
                                uint32_t* outSeq)
{
    if (!_EnsureOpen())
        return false;

    const volatile BYTE* base = _pView;

    if (ReadU32(base, 0) != ICON_SHM_MAGIC)
        return false;
    if (ReadU32(base, 4) != ICON_SHM_VERSION)
        return false;

    const uint8_t wantTheme = darkTheme ? ICON_THEME_DARK : ICON_THEME_LIGHT;

    // 重试 3 次：撞上并发发布的概率本就极低（发布是用户操作级频率，拷贝是微秒级），
    // 连撞三次基本只能是 SHM 内容异常，此时退回本地绘制比继续转圈更合适。
    for (int attempt = 0; attempt < 3; ++attempt)
    {
        const uint32_t seq1 = ReadU32(base, 8);
        if (seq1 == 0)
        {
            // SHM 已建但服务还没发布过内容。若照读会得到一张全透明的空图标，
            // 那比退回本地绘制糟得多——用户会看到图标"消失"。
            return false;
        }

        MemoryBarrier();

        const uint32_t slot        = ReadU32(base, 12);
        const uint32_t count       = ReadU32(base, 16);
        const uint32_t slotStride  = ReadU32(base, 20);
        const uint32_t slot0Offset = ReadU32(base, 24);
        const uint32_t tableOffset = ReadU32(base, 28);

        // 下面这组校验防的是「SHM 内容异常时读穿映射」。数据来自另一个进程，
        // 即使那进程是我们自己写的，也不该无条件信任它给的偏移。
        if (count == 0 || count > MAX_ICON_VARIANTS)
            return false;
        if (slot > 1)
            return false;
        if (static_cast<uint64_t>(slot0Offset) +
            static_cast<uint64_t>(slotStride) * 2 > ICON_SHM_SIZE)
            return false;
        if (static_cast<uint64_t>(tableOffset) +
            static_cast<uint64_t>(count) * sizeof(IconVariant) > slot0Offset)
            return false;

        // 选档：优先精确匹配，否则取边长最接近的。
        //
        // 之所以允许不精确匹配：GetIcon 没有尺寸参数，我们对"系统究竟想要多大"
        // 只有基于 DPI 的推测。备了多档就总能给出最接近的一档，而不是硬凑一个。
        uint32_t bestOffset = 0;
        uint32_t bestLen    = 0;
        int      bestSize   = 0;
        int      bestDelta  = INT_MAX;

        for (uint32_t i = 0; i < count; ++i)
        {
            const size_t e = tableOffset + static_cast<size_t>(i) * sizeof(IconVariant);
            const uint16_t sizePx = ReadU16(base, e);
            const uint8_t  theme  = *(base + e + 2);
            if (theme != wantTheme)
                continue;

            const uint32_t off = ReadU32(base, e + 4);
            const uint32_t len = ReadU32(base, e + 8);
            if (sizePx == 0 || len != static_cast<uint32_t>(sizePx) * sizePx * 4)
                continue;
            if (static_cast<uint64_t>(off) + len > slotStride)
                continue;

            const int delta = abs(static_cast<int>(sizePx) - desiredSizePx);
            if (delta < bestDelta)
            {
                bestDelta  = delta;
                bestOffset = off;
                bestLen    = len;
                bestSize   = sizePx;
            }
        }

        if (bestSize == 0)
            return false;

        const size_t src = static_cast<size_t>(slot0Offset) +
                           static_cast<size_t>(slot) * slotStride + bestOffset;

        std::vector<BYTE> tmp(bestLen);
        memcpy(tmp.data(),
               const_cast<const void*>(reinterpret_cast<const volatile void*>(base + src)),
               bestLen);

        MemoryBarrier();

        // seqlock 收口：序号没变 ⇒ 拷贝期间没有发生发布 ⇒ tmp 是一致快照。
        if (ReadU32(base, 8) == seq1)
        {
            outPixels.swap(tmp);
            outSizePx = bestSize;
            if (outSeq != nullptr)
                *outSeq = seq1;
            return true;
        }
        // 序号变了：拷到的可能是新旧混合的字节，丢弃重来。
    }

    WIND_LOG_DEBUG(L"IconShm: seqlock retries exhausted, falling back to local draw\n");
    return false;
}
