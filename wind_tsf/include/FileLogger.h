#pragma once

#include <windows.h>
#include <cstdint>
#include <string>

// Log path macros for debug variant coexistence
//
// 变体隔离**只做在目录一层**：dev 与正式版的数据目录本就分开
// （`WindInputDev` / `WindInput`），目录内的文件名再带 `_dev` 是重复隔离，
// 只会让两版本的排查步骤（找哪个文件、写哪个配置）无谓地不一致。
// 故文件名与配置名两版本一律同名，切换变体时排查命令原样可用。
#ifdef WIND_DEV_VARIANT
#define WIND_LOG_DIR_NAME       L"WindInputDev"
#else
#define WIND_LOG_DIR_NAME       L"WindInput"
#endif
// 日志文件名前缀。实际文件是 `<前缀>.<宿主名>.<pid>.log`（轮转产物再加 `.old`）——
// 每进程一个，见 CFileLogger::_BuildPaths。core 侧清理旧日志时按这个前缀匹配。
#define WIND_LOG_FILE_PREFIX    L"wind_tsf"
#define WIND_LOG_CONFIG_NAME    L"tsf_log_config"

// ============================================================================
// FileLogger - Multi-process safe logging for TSF DLL
//
// Output modes (controlled by config file):
//   none        - No output (default, near-zero overhead)
//   file        - Write to %LOCALAPPDATA%\<WIND_LOG_DIR_NAME>\logs\<前缀>.<宿主名>.<pid>.log
//   debugstring - OutputDebugStringW only (viewable in DebugView)
//   all         - Both file and OutputDebugStringW
//
// Config file: %LOCALAPPDATA%\<WIND_LOG_DIR_NAME>\logs\<WIND_LOG_CONFIG_NAME>
//   mode=none
//   level=debug
//
// Multi-process safety: Named Mutex + append-mode file I/O
// Auto-rotation: 5MB max, rotates to wind_tsf.old.log
//
// Ring Buffer: Always captures last RING_BUFFER_LINES log entries in memory,
//   regardless of mode. Press Ctrl+Shift+F12 to dump via text insertion.
// ============================================================================

class CFileLogger
{
public:
    enum class LogLevel : int
    {
        Off = 0,
        Error = 1,
        Warn = 2,
        Info = 3,
        Debug = 4,
        Trace = 5
    };

    enum class LogMode : int
    {
        None = 0,         // No output (default)
        File = 1,         // File only
        DebugString = 2,  // OutputDebugStringW only
        All = 3           // Both file and OutputDebugStringW
    };

    // Get singleton instance
    static CFileLogger& Instance();

    // Initialize logger (call once at DLL_PROCESS_ATTACH)
    void Init();

    // Shutdown logger (call at DLL_PROCESS_DETACH)
    void Shutdown();

    // Write a log entry (thread-safe, multi-process safe)
    // Also always writes to the in-memory ring buffer.
    void Write(LogLevel level, const wchar_t* message);

    // Fast-path check: is logging enabled at this level?
    // Inlined for minimal overhead when mode=none
    bool IsEnabled(LogLevel level) const {
        return _mode != LogMode::None && level != LogLevel::Off && level <= _level;
    }

    // Ring buffer: always enabled (captures even when mode=none)
    // Returns true if ring buffer has captured entries
    bool IsRingBufferEnabled() const { return true; }

    // Write directly to ring buffer (bypasses mode/level checks)
    void WriteToRingBuffer(LogLevel level, const wchar_t* message);

    // Dump all ring buffer entries as a single wstring, then clear
    std::wstring DumpRingBuffer();

    // Accessors
    LogLevel GetLevel() const { return _level; }
    LogMode GetMode() const { return _mode; }
    void SetLevel(LogLevel level) { _level = level; }
    void SetMode(LogMode mode) { _mode = mode; }

private:
    CFileLogger();
    ~CFileLogger();

    CFileLogger(const CFileLogger&) = delete;
    CFileLogger& operator=(const CFileLogger&) = delete;

    // Read config from file
    void _ReadConfig();

    // Build log directory and file paths
    void _BuildPaths();

    // Rotate log file if needed (caller must hold mutex)

    // Write to file (caller must hold mutex)
    void _WriteToFile(const char* utf8Line, int utf8Len);

    // Write to OutputDebugStringW
    void _WriteToDebugString(LogLevel level, const wchar_t* message);
    // 打开常开的追加句柄；失败返回 nullptr。顺带把已有文件大小计入 _written。
    HANDLE _OpenLogFile();
    // 立即轮转（关句柄 → 改名到 .old → 重开）。本进程独占文件，无需与他人协调。
    void _RotateNow();

    // Format timestamp
    static void _FormatTimestamp(wchar_t* buf, size_t bufSize);

    // Level to string
    static const wchar_t* _LevelStr(LogLevel level);

    LogMode _mode;
    LogLevel _level;
    bool    _initialized;
    // 常开的追加句柄。此前是「每行 CreateFile/WriteFile/CloseHandle 各一次 + 抢一把
    // 跨进程互斥锁」，而这一切都发生在 **TSF 输入线程**上——切换窗口时新旧宿主同时
    // 爆发写日志，几个进程的输入线程互相排队，用户能直接感到卡顿。
    // 日志文件改为每进程一个之后，这个句柄独占，于是锁与每行开关文件一并消失。
    HANDLE  _hFile;
    // 已写入字节数，用于轮转判定。**精确而非估算**：本进程独占该文件，没有别人往里写。
    // 此前每行都要 GetFileAttributesExW 查一次真实大小，现在一次系统调用都不需要。
    DWORD   _written;
    DWORD   _pid;
    wchar_t _logDir[MAX_PATH];
    wchar_t _logPath[MAX_PATH];
    wchar_t _oldPath[MAX_PATH];
    wchar_t _configPath[MAX_PATH];

    // Ring buffer for in-memory log capture (always active)
    static constexpr int RING_BUFFER_LINES = 200;
    static constexpr int RING_LINE_MAX = 256;
    wchar_t _ringBuffer[RING_BUFFER_LINES][RING_LINE_MAX];
    int _ringHead;   // Next write position
    int _ringCount;  // Total entries written (capped at RING_BUFFER_LINES)
    CRITICAL_SECTION _ringLock;

    static constexpr DWORD MAX_LOG_SIZE = 5 * 1024 * 1024; // 5MB
    // 宿主进程名在文件名里的截断长度（超长的 exe 名不该把路径顶到 MAX_PATH）。
    static constexpr size_t HOST_NAME_MAX = 48;
};
