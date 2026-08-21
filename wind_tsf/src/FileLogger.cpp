#include "FileLogger.h"
#include <shlobj.h>  // SHGetFolderPathW
#include <cstdio>
#include <cstring>

// ============================================================================
// CFileLogger implementation
// ============================================================================

CFileLogger::CFileLogger()
    : _mode(LogMode::None)
    , _level(LogLevel::Info)
    , _initialized(false)
    , _hFile(nullptr)
    , _written(0)
    , _pid(0)
    , _ringHead(0)
    , _ringCount(0)
{
    _logDir[0] = L'\0';
    _logPath[0] = L'\0';
    _configPath[0] = L'\0';
    memset(_ringBuffer, 0, sizeof(_ringBuffer));
    InitializeCriticalSection(&_ringLock);
}

CFileLogger::~CFileLogger()
{
    Shutdown();
    DeleteCriticalSection(&_ringLock);
}

CFileLogger& CFileLogger::Instance()
{
    static CFileLogger instance;
    return instance;
}

void CFileLogger::Init()
{
    if (_initialized)
        return;

    _initialized = true;
    _pid = GetCurrentProcessId();

    // Build paths first
    _BuildPaths();
    if (_logDir[0] == L'\0')
        return;

    // Ensure log directory exists
    CreateDirectoryW(_logDir, nullptr);

    // Read config (mode + level)
    _ReadConfig();

    // If mode is none, skip file handle creation entirely
    if (_mode == LogMode::None)
        return;

    // 打开**常开**的追加句柄。
    //
    // ⚠ 本函数跑在 `DllMain(DLL_PROCESS_ATTACH)` 里，即 loader lock 之下。这里只做内核
    // 对象操作（开文件），**不能**加载别的 DLL、创建线程或等待同步对象——旧日志的清理
    // 因此不放在 DLL 侧，交给 core 服务启动时做。
    //
    // 不再需要互斥锁：日志文件已按 pid 拆开（见 _BuildPaths），本进程独占该文件，
    // 没有可竞争的对象。此前那把 `Local\WindInput*TSFLogMutex` 是所有宿主进程共用的，
    // 而抢锁发生在 TSF 输入线程上——切换窗口时新旧宿主同时爆发写日志，几个进程的输入
    // 线程互相排队，超时上限 500ms。
    if (_mode == LogMode::File || _mode == LogMode::All)
    {
        _hFile = _OpenLogFile();
        if (_hFile == nullptr)
        {
            OutputDebugStringW(L"[WindInput][FileLogger] Failed to open log file, file logging disabled\n");
            // Fall back to debugstring-only if we had All mode
            if (_mode == LogMode::All)
                _mode = LogMode::DebugString;
            else
                _mode = LogMode::None;
        }
    }

    // Write startup marker
    wchar_t startMsg[256];
    _snwprintf_s(startMsg, _countof(startMsg), _TRUNCATE,
        L"FileLogger initialized (mode=%d, level=%ls, pid=%lu)",
        (int)_mode, _LevelStr(_level), _pid);
    Write(LogLevel::Info, startMsg);
}

void CFileLogger::Shutdown()
{
    if (!_initialized)
        return;

    if (_mode != LogMode::None)
    {
        Write(LogLevel::Info, L"FileLogger shutdown");
    }

    if (_hFile != nullptr)
    {
        CloseHandle(_hFile);
        _hFile = nullptr;
    }

    _mode = LogMode::None;
    _initialized = false;
}

void CFileLogger::Write(LogLevel level, const wchar_t* message)
{
    if (!IsEnabled(level) || message == nullptr)
        return;

    // Format: "2026-03-17 07:11:02.985 [DEBUG] [PID: 1234] message"
    wchar_t timestamp[32];
    _FormatTimestamp(timestamp, _countof(timestamp));

    // OutputDebugStringW path
    if (_mode == LogMode::DebugString || _mode == LogMode::All)
    {
        _WriteToDebugString(level, message);
    }

    // File path
    if (_mode == LogMode::File || _mode == LogMode::All)
    {
        wchar_t line[1200];
        int len = _snwprintf_s(line, _countof(line), _TRUNCATE,
            L"%ls [%-5ls] [PID: %5lu] %ls\r\n",
            timestamp, _LevelStr(level), _pid, message);

        if (len <= 0)
            return;

        // Convert to UTF-8
        char utf8Line[2400];
        int utf8Len = WideCharToMultiByte(CP_UTF8, 0, line, len, utf8Line, sizeof(utf8Line) - 1, nullptr, nullptr);
        if (utf8Len <= 0)
            return;

        _WriteToFile(utf8Line, utf8Len);
    }
}

void CFileLogger::_WriteToDebugString(LogLevel level, const wchar_t* message)
{
    WCHAR buf[600];
    _snwprintf_s(buf, _countof(buf), _TRUNCATE,
        L"[WindInput][%ls] %ls\n", _LevelStr(level), message);
    OutputDebugStringW(buf);
}

void CFileLogger::_WriteToFile(const char* utf8Line, int utf8Len)
{
    if (_hFile == nullptr)
        return;

    // 常规路径只剩**一次 WriteFile**。
    //
    // 此前这里是：抢跨进程互斥锁 → GetFileAttributesExW 查大小 → CreateFileW →
    // WriteFile → CloseHandle → ReleaseMutex，一行日志四次系统调用外加一次跨进程同步，
    // 全在 TSF 输入线程上。日志文件按 pid 拆开后，本进程独占，锁与开关文件都不再需要。
    DWORD written = 0;
    if (!WriteFile(_hFile, utf8Line, (DWORD)utf8Len, &written, nullptr))
        return;

    // 轮转判定用**累计字节数**，不再查文件大小：本进程独占该文件，写了多少自己最清楚。
    // 溢出保护：_written 是 DWORD，MAX_LOG_SIZE 只有 5MB，正常路径下轮转会先发生；
    // 但若某次 WriteFile 异常地大，饱和累加可避免绕回后长期不轮转。
    if (_written > MAXDWORD - written)
        _written = MAXDWORD;
    else
        _written += written;

    if (_written >= MAX_LOG_SIZE)
        _RotateNow();
}

// 打开常开的追加句柄。失败返回 nullptr（而非 INVALID_HANDLE_VALUE，省得每个调用点
// 各判一次哨兵值）。
//
// FILE_APPEND_DATA 而非 GENERIC_WRITE：追加语义由内核保证，不必自己维护写指针。
// FILE_SHARE_READ 让排查时能一边跑一边 tail 日志；FILE_SHARE_DELETE 让文件在被打开
// 的状态下仍可被改名或删除——core 侧清理旧日志时不会因为某个宿主还开着而失败。
HANDLE CFileLogger::_OpenLogFile()
{
    HANDLE h = CreateFileW(
        _logPath,
        FILE_APPEND_DATA,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        nullptr,
        OPEN_ALWAYS,
        FILE_ATTRIBUTE_NORMAL,
        nullptr
    );
    if (h == INVALID_HANDLE_VALUE)
        return nullptr;

    // 续写已有文件时把已有大小计入，否则重启宿主会让轮转阈值从零重新开始数，
    // 文件可以长到远超 MAX_LOG_SIZE。
    LARGE_INTEGER size;
    _written = GetFileSizeEx(h, &size) && size.QuadPart < MAXDWORD
        ? (DWORD)size.QuadPart
        : 0;
    return h;
}

// 立即轮转：关句柄 → 覆盖式改名到 .old → 重开。
//
// 只有本进程持有这个文件，所以不需要与任何人协调——这正是「每进程一个文件」换来的
// 简化。此前的 _RotateIfNeeded 要在共享文件上做，还得先靠互斥锁排他。
void CFileLogger::_RotateNow()
{
    if (_hFile != nullptr)
    {
        CloseHandle(_hFile);
        _hFile = nullptr;
    }
    DeleteFileW(_oldPath);
    MoveFileW(_logPath, _oldPath);
    _hFile = _OpenLogFile();
    // 重开失败则文件日志就此停摆（_hFile 为 nullptr，后续 _WriteToFile 直接返回）。
    // 不改 _mode：DebugString 那一路与文件无关，不该被文件问题连累关掉。
}

void CFileLogger::_BuildPaths()
{
    wchar_t appData[MAX_PATH];
    if (FAILED(SHGetFolderPathW(nullptr, CSIDL_LOCAL_APPDATA, nullptr, 0, appData)))
        return;

    _snwprintf_s(_logDir, _countof(_logDir), _TRUNCATE,
        L"%ls\\" WIND_LOG_DIR_NAME L"\\logs", appData);

    // 日志文件**每进程一个**：`wind_tsf.<宿主名>.<pid>.log`。
    //
    // TSF DLL 被每个宿主进程各加载一份，此前它们共写一个文件，只能靠一把跨进程互斥锁
    // 串行化——而写日志发生在输入线程上，切换窗口时新旧宿主同时爆发写入，几个进程的
    // 输入线程互相排队。拆开之后没有共享资源，锁与「每行开关文件」一并不再需要。
    //
    // 带宿主名是为了排查时一眼认出是谁（wind_tsf.feishu.12345.log）；**还要带 pid**，
    // 因为同一个程序可以多开，Chrome 那类多进程宿主更是一开就是一串——只带名字会撞回
    // 共享，锁也就白去了。
    wchar_t hostExe[MAX_PATH] = {};
    wchar_t hostName[HOST_NAME_MAX + 1];
    wcscpy_s(hostName, L"host");
    if (GetModuleFileNameW(nullptr, hostExe, _countof(hostExe)) > 0)
    {
        const wchar_t* base = wcsrchr(hostExe, L'\\');
        base = base ? base + 1 : hostExe;
        size_t n = 0;
        // 取文件名主干，且**逐字符白名单**：进程名要进文件路径，混进路径分隔符或通配符
        // 会把日志写去别的目录，也会让 core 侧按前缀清理时匹配到意外的文件。
        for (const wchar_t* c = base; *c != L'\0' && *c != L'.' && n < HOST_NAME_MAX; ++c)
        {
            if ((*c >= L'a' && *c <= L'z') || (*c >= L'A' && *c <= L'Z') ||
                (*c >= L'0' && *c <= L'9') || *c == L'_' || *c == L'-')
            {
                hostName[n++] = *c;
            }
        }
        if (n > 0)
            hostName[n] = L'\0';
    }

    _snwprintf_s(_logPath, _countof(_logPath), _TRUNCATE,
        L"%ls\\" WIND_LOG_FILE_PREFIX L".%ls.%lu.log", _logDir, hostName, _pid);

    _snwprintf_s(_oldPath, _countof(_oldPath), _TRUNCATE,
        L"%ls\\" WIND_LOG_FILE_PREFIX L".%ls.%lu.old.log", _logDir, hostName, _pid);

    _snwprintf_s(_configPath, _countof(_configPath), _TRUNCATE,
        L"%ls\\" WIND_LOG_CONFIG_NAME, _logDir);
}

void CFileLogger::_ReadConfig()
{
    // Default: mode=none, level=info
    _mode = LogMode::None;
    _level = LogLevel::Info;

    HANDLE hFile = CreateFileW(
        _configPath,
        GENERIC_READ,
        FILE_SHARE_READ,
        nullptr,
        OPEN_EXISTING,
        FILE_ATTRIBUTE_NORMAL,
        nullptr
    );

    if (hFile == INVALID_HANDLE_VALUE)
        return; // No config file → mode=none

    char buf[256] = {};
    DWORD bytesRead = 0;
    ReadFile(hFile, buf, sizeof(buf) - 1, &bytesRead, nullptr);
    CloseHandle(hFile);

    // Parse line by line
    char* ctx = nullptr;
    char* line = strtok_s(buf, "\r\n", &ctx);
    while (line != nullptr)
    {
        // Skip leading whitespace
        while (*line == ' ' || *line == '\t') line++;

        // Skip comments and empty lines
        if (*line == '#' || *line == '\0')
        {
            line = strtok_s(nullptr, "\r\n", &ctx);
            continue;
        }

        // Parse key=value
        char* eq = strchr(line, '=');
        if (eq != nullptr)
        {
            *eq = '\0';
            char* key = line;
            char* val = eq + 1;

            // Trim key
            char* kEnd = eq - 1;
            while (kEnd > key && (*kEnd == ' ' || *kEnd == '\t')) *kEnd-- = '\0';

            // Trim value
            while (*val == ' ' || *val == '\t') val++;
            char* vEnd = val + strlen(val) - 1;
            while (vEnd > val && (*vEnd == ' ' || *vEnd == '\t')) *vEnd-- = '\0';

            if (_stricmp(key, "mode") == 0)
            {
                if (_stricmp(val, "none") == 0 || _stricmp(val, "off") == 0)
                    _mode = LogMode::None;
                else if (_stricmp(val, "file") == 0)
                    _mode = LogMode::File;
                else if (_stricmp(val, "debugstring") == 0 || _stricmp(val, "debug_string") == 0)
                    _mode = LogMode::DebugString;
                else if (_stricmp(val, "all") == 0)
                    _mode = LogMode::All;
            }
            else if (_stricmp(key, "level") == 0)
            {
                if (_stricmp(val, "off") == 0) _level = LogLevel::Off;
                else if (_stricmp(val, "error") == 0) _level = LogLevel::Error;
                else if (_stricmp(val, "warn") == 0) _level = LogLevel::Warn;
                else if (_stricmp(val, "info") == 0) _level = LogLevel::Info;
                else if (_stricmp(val, "debug") == 0) _level = LogLevel::Debug;
                else if (_stricmp(val, "trace") == 0) _level = LogLevel::Trace;
            }
        }

        line = strtok_s(nullptr, "\r\n", &ctx);
    }
}

void CFileLogger::_FormatTimestamp(wchar_t* buf, size_t bufSize)
{
    SYSTEMTIME st;
    GetLocalTime(&st);
    _snwprintf_s(buf, bufSize, _TRUNCATE,
        L"%04d-%02d-%02d %02d:%02d:%02d.%03d",
        st.wYear, st.wMonth, st.wDay,
        st.wHour, st.wMinute, st.wSecond, st.wMilliseconds);
}

const wchar_t* CFileLogger::_LevelStr(LogLevel level)
{
    switch (level)
    {
    case LogLevel::Error: return L"ERROR";
    case LogLevel::Warn:  return L"WARN";
    case LogLevel::Info:  return L"INFO";
    case LogLevel::Debug: return L"DEBUG";
    case LogLevel::Trace: return L"TRACE";
    default:              return L"?????";
    }
}

// ============================================================================
// Ring Buffer (always active, no file system / debug string dependency)
// ============================================================================

void CFileLogger::WriteToRingBuffer(LogLevel level, const wchar_t* message)
{
    if (message == nullptr)
        return;

    EnterCriticalSection(&_ringLock);

    wchar_t timestamp[32];
    _FormatTimestamp(timestamp, _countof(timestamp));

    _snwprintf_s(_ringBuffer[_ringHead], RING_LINE_MAX, _TRUNCATE,
        L"%ls [%-5ls] %ls",
        timestamp, _LevelStr(level), message);

    _ringHead = (_ringHead + 1) % RING_BUFFER_LINES;
    if (_ringCount < RING_BUFFER_LINES)
        _ringCount++;

    LeaveCriticalSection(&_ringLock);
}

std::wstring CFileLogger::DumpRingBuffer()
{
    EnterCriticalSection(&_ringLock);

    std::wstring result;
    result.reserve(_ringCount * 128);

    // Header
    wchar_t header[128];
    _snwprintf_s(header, _countof(header), _TRUNCATE,
        L"=== WindInput TSF Log Dump (PID:%lu, entries:%d) ===\r\n", _pid, _ringCount);
    result += header;

    if (_ringCount == 0)
    {
        result += L"(empty)\r\n";
    }
    else
    {
        // Start from oldest entry
        int start = (_ringCount < RING_BUFFER_LINES) ? 0 : _ringHead;
        for (int i = 0; i < _ringCount; i++)
        {
            int idx = (start + i) % RING_BUFFER_LINES;
            result += _ringBuffer[idx];
            result += L"\r\n";
        }
    }

    result += L"=== End Log Dump ===\r\n";

    // Clear buffer after dump
    _ringHead = 0;
    _ringCount = 0;

    LeaveCriticalSection(&_ringLock);
    return result;
}
