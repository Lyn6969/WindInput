//! wind_input: 输入法主服务进程
//!
//! 最小可输入服务：启动 Named Pipe 服务器，接收 TSF DLL 按键事件并响应。
//! 与 Go 版本 `wind_input/cmd/service/main.go` 的启动序列对齐。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use wind_bridge::deferred::DeferredHandler;
use wind_bridge::push::{PushConfig, PushServer};
use wind_bridge::server::{BridgeConfig, BridgeServer};

mod config_cli;

fn main() {
    // CLI 子命令：`wind_input config ...`（查看/读写配置）。在服务启动前拦截，处理完即退出。
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.get(1).map(String::as_str) == Some("config") {
        std::process::exit(config_cli::run(&cli_args[2..]));
    }

    // 0. 设置 DPI 感知（与 Go 版 setDPIAwareness 对齐）
    // 必须在任何窗口创建之前调用，否则坐标会被 Windows DPI 虚拟化
    set_dpi_awareness();

    // 1. 初始化日志
    init_logger();

    let pipe_suffix = wind_config::variant::pipe_suffix();
    let variant = if wind_config::variant::is_dev() {
        "dev"
    } else {
        "release"
    };
    info!(
        "WindInput service starting (v{}, {} variant)",
        env!("CARGO_PKG_VERSION"),
        variant
    );

    // 2. 单例检查（与 Go 版 checkSingleton 对齐）
    let _singleton_guard = match check_singleton() {
        Some(guard) => guard,
        None => {
            error!("Another instance is already running, exiting");
            eprintln!("WindInput: 另一个实例已在运行中");
            std::process::exit(1);
        }
    };
    info!("Singleton check passed");

    // 3. 创建 DeferredHandler（启动时返回安全默认值）
    let deferred = DeferredHandler::new();

    // 4. 创建 tokio runtime
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");

    // 5. 启动 Push 管道服务器
    let push_config = PushConfig {
        suffix: pipe_suffix.to_string(),
        write_timeout_ms: 30_000,
    };
    let push_server = Arc::new(PushServer::new(push_config));

    runtime.block_on(async {
        if let Err(e) = push_server.start().await {
            error!("Push server failed to start: {}", e);
            std::process::exit(1);
        }
    });

    // 6. 创建 Bridge 服务器
    let bridge_config = BridgeConfig {
        suffix: pipe_suffix.to_string(),
        request_timeout_ms: 1000,
    };
    let bridge = BridgeServer::new(bridge_config, deferred.clone());

    // 7. 启动 Bridge 服务器
    runtime.block_on(async {
        if let Err(e) = bridge.start().await {
            error!("Bridge server failed to start: {}", e);
            std::process::exit(1);
        }
    });

    // 8. 创建重启信号通道（须在协调器创建前，使 request_restart 的发送端就绪）
    let restart_rx = wind_coordinator::restart_signal();

    // 9. 创建中央协调器（传入 PushServer 用于激活状态推送）
    let coordinator = wind_coordinator::Coordinator::new(push_server.clone());
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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // 日志输出到可执行文件同目录的 logs/ 子目录
    // 日志写入 %LOCALAPPDATA%\WindInput\logs（不随漫游；避免装到 Program Files 时
    // exe 目录只读导致写入失败）。取不到则回退到 exe 旁 logs。
    let log_dir = wind_config::Config::log_dir()
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("logs")))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "wind_input.log");
    let (file_writer, _guard) = tracing_appender::non_blocking(file_appender);

    // 控制台输出
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_filter(filter);

    // 文件输出（debug 级别，记录更多细节）
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_target(true)
        .with_thread_ids(true)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    // 保持 _guard 存活（non_blocking writer 的 flush guard）
    // 使用 Box::leak 让 guard 在进程生命周期内不被释放
    Box::leak(Box::new(_guard));

    info!("Log directory: {}", log_dir.display());
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
        error!("CreateMutexW failed");
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
