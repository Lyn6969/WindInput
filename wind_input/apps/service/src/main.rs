//! wind_input: 输入法主服务进程
//!
//! 最小可输入服务：启动 Named Pipe 服务器，接收 TSF DLL 按键事件并响应。
//! 与 Go 版本 `wind_input/cmd/service/main.go` 的启动序列对齐。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use file_rotate::compression::Compression;
use file_rotate::{ContentLimit, FileRotate};
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::ChronoLocal;

use wind_bridge::deferred::DeferredHandler;
use wind_bridge::push::{PushConfig, PushServer};
use wind_bridge::server::{BridgeConfig, BridgeServer};

// 启动轨迹下沉在 wind-config，好让 UI 线程等下层也能打点（见该模块文档）。
// 时间戳格式一并取自那里：与主日志、wind_tsf 的 `CFileLogger::_FormatTimestamp`
// 共用一处定义，三份日志才能归并排序，也避免两边各写一份而漂移。
use wind_config::startup_trace::{self, LOG_TIME_FORMAT};

mod backup_cli;
mod cli_util;
mod config_cli;
mod dict_cli;
mod log_rotate;
mod phrase_cli;
mod schema_cli;

/// GUI 子系统（release profile，`windows_subsystem="windows"`）下进程不附着控制台，
/// 故 CLI 子命令的 `println!` 无处可写。此函数把进程附着到**父控制台**（调用它的 cmd/PowerShell），
/// 并把 `CONOUT$/CONIN$` 设回标准句柄，让 stdout/stderr/stdin 直达该终端。
///
/// 注意：GUI 子系统进程不会让 shell 等待——提示符会先返回、输出随后插入（交错）。
/// 需"等它跑完"时请重定向（`> out.txt 2>&1`）或 `start /wait`。
/// 无父控制台（双击启动/被服务拉起）时静默返回——那种场景本就不该走 CLI。
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    // GENERIC_READ | GENERIC_WRITE（用裸常量避免跨版本导入路径差异）。
    const GENERIC_RW: u32 = 0xC000_0000;

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return; // 没有父控制台
        }
        // 重新打开控制台读写设备并设为进程标准句柄；否则 GUI 子系统启动时标准句柄为空。
        if let Ok(out) = CreateFileW(
            w!("CONOUT$"),
            GENERIC_RW,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, out);
            let _ = SetStdHandle(STD_ERROR_HANDLE, out);
        }
        if let Ok(inp) = CreateFileW(
            w!("CONIN$"),
            GENERIC_RW,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) {
            let _ = SetStdHandle(STD_INPUT_HANDLE, inp);
        }
    }
}

fn main() {
    // CLI 子命令（config/schema/...）：在服务启动前拦截，处理完即退出。
    // 注意保持在单例检查之前——CLI 进程不该被「另一实例已运行」挡掉。
    let cli_args: Vec<String> = std::env::args().collect();
    let sub = cli_args.get(1).map(String::as_str);
    if matches!(
        sub,
        Some("config" | "schema" | "dict" | "phrase" | "backup")
    ) {
        // GUI 子系统下附着父控制台，让输出回到调用的终端（详见 attach_parent_console）。
        #[cfg(windows)]
        attach_parent_console();
        let code = match sub {
            Some("config") => config_cli::run(&cli_args[2..]),
            Some("schema") => schema_cli::run(&cli_args[2..]),
            Some("dict") => dict_cli::run(&cli_args[2..]),
            Some("phrase") => phrase_cli::run(&cli_args[2..]),
            Some("backup") => backup_cli::run(&cli_args[2..]),
            _ => unreachable!(),
        };
        // process::exit 不会刷新缓冲，重定向/管道时可能丢尾部输出——显式 flush。
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(code);
    }

    // 0. 设置 DPI 感知（与 Go 版 setDPIAwareness 对齐）
    // 必须在任何窗口创建之前调用，否则坐标会被 Windows DPI 虚拟化
    set_dpi_awareness();

    // 启动轨迹的第一个探针必须早于 init_logger：主日志失效时，
    // 「究竟有没有走到日志初始化」本身就是要回答的问题。
    startup_trace::stage("begin");

    // 1. 单例检查（与 Go 版 checkSingleton 对齐）
    //
    // **必须早于 init_logger**：后者会滚动日志（rotate_on_startup），而一个注定要退出的
    // 重复实例不该有权改动正在运行实例的日志。此前顺序相反，于是形成了恶性反馈——
    // 用户遇到故障 → 尝试重启输入法 → 新实例先把故障现场的日志顶到 .1.log、再写下一个
    // 两行空壳、然后被单例挡掉退出。越排查，现场越少。
    //
    // 代价是此处无法用 tracing（subscriber 尚未装），改用启动轨迹 + stderr：
    // 反正 `exit(1)` 也刷不出 tracing 的 non_blocking 缓冲，同步落盘的轨迹反而更可靠。
    let _singleton_guard = match check_singleton() {
        Some(guard) => guard,
        None => {
            startup_trace::stage("singleton-BLOCKED 另一实例已在运行，本实例退出");
            eprintln!("WindInput: 另一个实例已在运行中");
            std::process::exit(1);
        }
    };
    startup_trace::stage("singleton-ok");

    // 2. 初始化日志（含启动滚动：至此才确定本实例会真正运行）
    init_logger();
    startup_trace::stage("logger-ready");

    let pipe_suffix = wind_config::variant::pipe_suffix();
    let variant = if wind_config::variant::is_dev() {
        "dev"
    } else {
        "release"
    };
    info!(
        "WindInput service starting (v{}, {} variant, build {} git:{})",
        env!("WIND_APP_VERSION"),
        variant,
        env!("WIND_BUILD_TIME"),
        env!("WIND_GIT_HASH"),
    );
    info!("Singleton check passed");

    // 2.5 等待用户配置目录就绪。
    //
    // 开机自启时服务可能跑在登录会话很早的阶段，此时漫游 known folder（%APPDATA%）
    // 未必已解析/挂载完成，而日志目录走的是另一个 known folder（%LOCALAPPDATA%，
    // 见 Config::log_dir vs user_config_dir）——两者独立解析，于是会出现
    // 「日志正常写出、用户配置却像不存在一样」，配置静默退化为系统预置，
    // 用户表现为「设置成全拼，重启后工具栏又变回五笔」。
    //
    // 必须在 init_logger() 之后：探测过程本身要留下日志。
    wind_config::Config::wait_user_config_ready(std::time::Duration::from_secs(10));
    startup_trace::stage("config-ready");

    // 3. 创建 DeferredHandler（启动时返回安全默认值）
    let deferred = DeferredHandler::new();

    // 4. 启动 Push 管道服务器
    let push_config = PushConfig {
        suffix: pipe_suffix.to_string(),
        write_timeout_ms: 30_000,
    };
    let push_server = Arc::new(PushServer::new(push_config));

    if let Err(e) = push_server.start() {
        fatal_exit(
            "push-server-FAILED",
            &format!("Push server failed to start: {e}"),
        );
    }

    // 5. 创建 Bridge 服务器
    let bridge_config = BridgeConfig {
        suffix: pipe_suffix.to_string(),
        request_timeout_ms: 1000,
    };
    // host-render 管理器（Windows）：白名单取自 compat.host_render_processes。
    // 同一 Arc 实例同时注入 BridgeServer（连接循环 setup/清理）与 Coordinator（写帧/隐藏）。
    #[cfg(windows)]
    let host_render = {
        let whitelist = wind_config::Config::load(wind_config::Config::data_dir().as_deref())
            .map(|c| c.compat.host_render_processes)
            .unwrap_or_default();
        wind_bridge::host_render_windows::HostRenderManager::new(pipe_suffix, whitelist)
    };
    let bridge = BridgeServer::new(bridge_config, deferred.clone());
    #[cfg(windows)]
    let bridge = bridge.with_host_render(host_render.clone());

    // 6. 启动 Bridge 服务器
    if let Err(e) = bridge.start() {
        fatal_exit(
            "bridge-server-FAILED",
            &format!("Bridge server failed to start: {e}"),
        );
    }

    startup_trace::stage("bridge-ready");

    // 8. 创建重启信号通道（须在协调器创建前，使 request_restart 的发送端就绪）
    let restart_rx = wind_coordinator::restart_signal();

    // 9. 创建中央协调器（传入 PushServer 用于激活状态推送）
    // 前后各打一次：候选窗/工具栏的窗口线程在此创建，是「有服务无 GUI」的头号嫌疑段。
    startup_trace::stage("coordinator-begin");
    let coordinator = wind_coordinator::Coordinator::new(push_server.clone());
    startup_trace::stage("coordinator-done");
    // 注入 host-render 管理器（与 BridgeServer 共享同一实例），供后续写帧/隐藏使用。
    #[cfg(windows)]
    coordinator.set_host_render(host_render.clone());
    // push 客户端注册回调：host-render 白名单受限宿主（SearchHost 等 transient DocMgr）
    // 服务重启重连时不发任何激活事件，由此回调补推 activation 握手使 DLL 重新 setup。
    #[cfg(windows)]
    {
        let coord = coordinator.clone();
        push_server.set_client_connected_hook(Box::new(move |token| {
            coord.on_push_client_connected(token);
        }));
    }
    let coord_for_web = coordinator.clone();
    deferred.set_ready(coordinator);

    // 9.5 启动本地控制 / 配置 JSON-RPC 服务（命名管道：..._ctrl + ..._events）。
    // 本地授权靠 OS ACL（SDDL），不再需要 token/Origin/CORS/端口发现。
    // 同步线程模型（与 bridge/push 一致），不引入 tokio 到控制路径。
    let core_rpc: Arc<dyn wind_rpc::CoreRpc> = Arc::new(RpcCore(coord_for_web));
    match wind_rpc::RpcServer::new(core_rpc, variant, pipe_suffix) {
        Ok(rpc_server) => {
            if let Err(e) = rpc_server.start() {
                error!("RPC server failed to start: {}", e);
            }
            // 句柄需在进程生命周期内保活（控制/事件 server 已分别在后台线程运行）。
            Box::leak(Box::new(rpc_server));
        }
        Err(e) => error!("RPC server init failed (网页/GUI 设置不可用): {}", e),
    }

    info!(
        "WindInput service ready (pipes: wind_input{}, wind_input_push{}, wind_input_ctrl{})",
        pipe_suffix, pipe_suffix, pipe_suffix
    );
    startup_trace::stage("service-ready");

    // 10. 阻塞主线程，直到菜单触发"重启服务"
    match restart_rx.recv() {
        Ok(()) => {
            info!("Restart requested, relaunching service...");
            // 先释放单例 Named Mutex（关闭句柄），让新实例可获取所有权，避免竞争
            drop(_singleton_guard);
            relaunch_self();
            std::process::exit(0);
        }
        Err(_) => loop {
            // 通道断开（不应发生）：退回挂起
            std::thread::park();
        },
    }
}

/// 适配 Coordinator 为 RPC 服务的运行时状态来源（解耦 wind-rpc 与 wind-coordinator）。
struct RpcCore(Arc<wind_coordinator::Coordinator>);

impl wind_rpc::CoreRpc for RpcCore {
    fn is_chinese_mode(&self) -> bool {
        self.0.is_chinese_mode()
    }
    fn active_schema_id(&self) -> String {
        self.0.active_schema_id()
    }
    fn apply_config(&self) -> bool {
        self.0.reload_user_config()
    }
    fn data_rpc(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.0.web_data_rpc(method, params)
    }
    fn fonts(&self) -> Vec<(String, String)> {
        self.0.list_font_families()
    }
}

/// 以分离子进程重新启动自身（用于"重启服务"）。
#[cfg(windows)]
fn relaunch_self() {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    // 脱离父进程的 Job Object：IME/TSF 宿主进程常处于 kill-on-job-close 作业对象中，
    // 不加此标志时父进程一退出会连带杀掉刚拉起的子进程（症状：重启只退出、新进程不存活）。
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to resolve current exe for relaunch: {}", e);
            return;
        }
    };
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;

    // 优先带 breakaway；若作业对象不允许 breakaway（spawn 报错）则回退到不带该标志再试。
    match std::process::Command::new(&exe)
        .creation_flags(base | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Ok(_) => {
            info!("Relaunched (breakaway): {}", exe.display());
            return;
        }
        Err(e) => error!("Relaunch with breakaway failed ({e}); retrying without breakaway"),
    }
    match std::process::Command::new(&exe)
        .creation_flags(base)
        .spawn()
    {
        Ok(_) => info!("Relaunched: {}", exe.display()),
        Err(e) => error!("Failed to relaunch: {}", e),
    }
}

#[cfg(not(windows))]
fn relaunch_self() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

fn init_logger() {
    // 读取配置（失败时用默认值，不阻断日志初始化）
    let cfg_debug = wind_config::Config::load(wind_config::Config::data_dir().as_deref())
        .map(|c| c.debug)
        .unwrap_or_default();

    // RUST_LOG 最优先，其次 debug.log_level，默认 info。
    // info 级别日志不得包含用户输入内容、词库词条等隐私数据。
    let level = std::env::var("RUST_LOG")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let l = &cfg_debug.log_level;
            if l.is_empty() { None } else { Some(l.clone()) }
        })
        .unwrap_or_else(|| "info".to_string());

    // 便携模式：<exe>/userdata/logs；正常模式：%LOCALAPPDATA%\WindInput[Dev]\logs。
    let log_dir = wind_config::Config::log_dir()
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("logs")))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    let _ = std::fs::create_dir_all(&log_dir);

    // 滚动命名：wind_input.log → wind_input.1.log → … → wind_input.N.log
    // （序号在扩展名之前，滚动后仍是 .log，编辑器认得、按 *.log 也搜得到）
    let log_path = log_dir.join("wind_input.log");

    // 升级路径：把老方案写下的 wind_input.log.N 迁成新命名，否则新的扫描认不出它们，
    // 会永久滞留在目录里。须在 FileRotate::new 之前——构造时就会扫描既存序号。
    log_rotate::migrate_legacy_suffix(&log_path);

    let mut rotate = FileRotate::new(
        &log_path,
        log_rotate::AppendCountBeforeExt::new(cfg_debug.log_max_files),
        ContentLimit::Bytes((cfg_debug.log_max_size_mb * 1024 * 1024) as usize),
        Compression::None,
        None,
    );

    log_rotate::rotate_on_startup(&mut rotate, &log_path);

    let (writer, _guard) = tracing_appender::non_blocking(rotate);
    // 丢弃计数器：worker 线程一旦出事，channel 断开后 lossy 模式会静默丢掉此后每一条
    // 日志，`wind_input.log` 就永久停在某一行而进程照常运行。守护线程据此留痕。
    let dropped = writer.error_counter();

    // 时间戳用本地时区，且格式与 wind_tsf 的 FileLogger 完全一致
    // （`GetLocalTime` → `%04d-%02d-%02d %02d:%02d:%02d.%03d`），
    // 两份日志才能直接按时间对齐排查。默认的 SystemTime timer 输出 UTC，
    // 与 TSF 日志差一个时区，务必不要退回默认值。
    // 注意：不能用 fmt::time::LocalTime —— 它依赖 time crate，
    // 而 time 在多线程进程中会拒绝获取本地时区偏移，导致时间戳静默变空。
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true)
        .with_timer(ChronoLocal::new(LOG_TIME_FORMAT.to_string()))
        .with_env_filter(EnvFilter::new(&level))
        .init();

    // Box::leak 保持 guard 存活至进程退出，确保缓冲日志全部落盘。
    Box::leak(Box::new(_guard));

    spawn_log_health_watch(dropped);

    info!(
        log_dir = %log_dir.display(),
        level = %level,
        max_size_mb = cfg_debug.log_max_size_mb,
        max_files = cfg_debug.log_max_files,
        portable = wind_config::variant::is_portable(),
        // 时间戳是本地时间；记一次 UTC 偏移，让日志自描述所处时区
        tz_offset = %chrono::Local::now().format("%:z"),
        "logger initialized"
    );
}

/// 带必达落盘的致命退出。
///
/// `std::process::exit` 不运行析构，也不刷 `tracing_appender` 的 non_blocking 缓冲——
/// 退出原因能否落进主日志纯看 worker 线程有没有抢在进程消失前刷一次盘。这曾直接误导过
/// 排查：同一条退出路径，有的日志里三条俱全，有的只剩一行，让人误以为是两种不同的故障。
///
/// 故退出原因先同步写进启动轨迹（必达），再给 worker 一点时间尽力刷出主日志。
fn fatal_exit(stage: &str, msg: &str) -> ! {
    error!("{}", msg);
    startup_trace::stage(&format!("{stage}: {msg}"));
    eprintln!("WindInput: {msg}");
    // best-effort：让 non_blocking worker 有机会把上面那条 error! 写出去。
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::process::exit(1);
}

/// 守护主日志的健康：丢弃计数一旦增长就写进启动轨迹。
///
/// 解决的是「观测工具自己成了故障的一部分」——`tracing_appender::non_blocking` 的
/// worker 线程出事后，`NonBlocking` 在 lossy 模式下会**静默丢弃**其后每一条日志。
/// 主日志因此停在某一行，而进程仍在正常跑，极易被误读成「进程卡在那一行」。
/// 轨迹文件不经 tracing，故此时仍写得出去。
///
/// 30 秒一次、且只在计数**变化**时落笔：正常运行零写入，不会撑大轨迹文件。
fn spawn_log_health_watch(dropped: tracing_appender::non_blocking::ErrorCounter) {
    std::thread::Builder::new()
        .name("log-health".into())
        .spawn(move || {
            let mut last = 0usize;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(30));
                let n = dropped.dropped_lines();
                if n != last {
                    startup_trace::stage(&format!(
                        "LOG-DROPPED lines={n} (主日志已丢日志，其后内容不可信)"
                    ));
                    last = n;
                }
            }
        })
        .ok();
}

/// 单例检查：通过 Windows Named Mutex 确保只有一个实例运行
///
/// 与 Go 版 `checkSingleton()` 对齐：
/// - Mutex 名称：`Global\WindInput{Suffix}IMEService`
/// - Global namespace 让所有桌面共享同一实例
/// - 返回 Some(guard) 表示成功获取锁，guard 析构时释放
/// - 返回 None 表示已有另一实例在运行
#[cfg(windows)]
fn check_singleton() -> Option<SingletonGuard> {
    // 直接调用 kernel32 的 CreateMutexW，绕过 windows crate 的 Result 包装。
    // Go 版 CreateMutex 在 ERROR_ALREADY_EXISTS 时仍返回有效 handle，
    // 但 windows crate 的 .ok() 会把它转为 Err 丢弃 handle。
    unsafe extern "system" {
        fn CreateMutexW(
            lpMutexAttributes: *const std::ffi::c_void,
            bInitialOwner: i32,
            lpName: *const u16,
        ) -> windows::Win32::Foundation::HANDLE;
    }

    let mutex_name = format!(
        "Global\\WindInputIMEService{}",
        wind_config::variant::pipe_suffix()
    );
    let wide_name: Vec<u16> = mutex_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };

    if handle.is_invalid() {
        // 本函数现在早于 init_logger 运行，tracing 尚无 subscriber，只能走启动轨迹。
        startup_trace::stage("singleton-CreateMutexW-FAILED");
        return None;
    }

    // 检查 ERROR_ALREADY_EXISTS（GetLastError = 183）
    let last_err = unsafe { windows::Win32::Foundation::GetLastError() };

    if last_err == windows::Win32::Foundation::ERROR_ALREADY_EXISTS {
        // 另一个实例已在运行，释放 handle
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return None;
    }

    // 等待获取 mutex 所有权（立即返回）
    let wait_result = unsafe { windows::Win32::System::Threading::WaitForSingleObject(handle, 0) };

    if wait_result == windows::Win32::Foundation::WAIT_OBJECT_0
        || wait_result == windows::Win32::Foundation::WAIT_ABANDONED
    {
        Some(SingletonGuard { _handle: handle })
    } else {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        None
    }
}

#[cfg(not(windows))]
fn check_singleton() -> Option<SingletonGuard> {
    // 非 Windows 平台：暂不实现单例检查
    Some(SingletonGuard {})
}

/// 单例守卫：析构时释放 Mutex
#[cfg(windows)]
struct SingletonGuard {
    _handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for SingletonGuard {
    fn drop(&mut self) {
        // Mutex 在 handle 关闭时自动释放
    }
}

#[cfg(not(windows))]
struct SingletonGuard {}

/// 设置进程 DPI 感知（与 Go 版 setDPIAwareness 对齐）
///
/// 使用 SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) 设置 Per-Monitor DPI 感知。
/// 必须在任何窗口创建之前调用，否则坐标会被 Windows DPI 虚拟化。
#[cfg(windows)]
fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{PROCESS_PER_MONITOR_DPI_AWARE, SetProcessDpiAwareness};

    let result = unsafe { SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE) };
    if result.is_ok() {
        tracing::info!("DPI awareness set to Per-Monitor DPI Aware");
    } else {
        tracing::warn!("Failed to set DPI awareness: {:?}", result);
    }
}

#[cfg(not(windows))]
fn set_dpi_awareness() {
    // 非 Windows 平台无需设置
}
