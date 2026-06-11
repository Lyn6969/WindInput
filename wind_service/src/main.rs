//! wind_input: 输入法主服务进程
//!
//! 最小可输入服务：启动 Named Pipe 服务器，接收 TSF DLL 按键事件并响应。
//! 与 Go 版本 `wind_input/cmd/service/main.go` 的启动序列对齐。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge_impl;

use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use wind_bridge::deferred::DeferredHandler;
use wind_bridge::push::{PushConfig, PushServer};
use wind_bridge::server::{BridgeConfig, BridgeServer};

/// 获取管道名称后缀（debug 变体使用 "_debug"）
#[cfg(feature = "debug_variant")]
const PIPE_SUFFIX: &str = "_debug";

#[cfg(not(feature = "debug_variant"))]
const PIPE_SUFFIX: &str = "";

fn main() {
    // 1. 初始化日志
    init_logger();

    let variant = if PIPE_SUFFIX.is_empty() {
        "release"
    } else {
        "debug"
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
        suffix: PIPE_SUFFIX.to_string(),
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
        suffix: PIPE_SUFFIX.to_string(),
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

    // 8. 创建最小协调器（传入 PushServer 用于激活状态推送）
    let coordinator = bridge_impl::MinimalCoordinator::new(push_server.clone());
    deferred.set_ready(coordinator);

    info!(
        "WindInput service ready (pipes: wind_input{}, wind_input_push{})",
        PIPE_SUFFIX, PIPE_SUFFIX
    );

    // 9. 阻塞主线程
    // TODO: 监听退出信号（如 Ctrl+C 或 Windows 消息）
    loop {
        std::thread::park();
    }
}

fn init_logger() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .init();
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

    let mutex_name = format!("Global\\WindInput{}IMEService", PIPE_SUFFIX);
    let wide_name: Vec<u16> = mutex_name.encode_utf16().chain(std::iter::once(0)).collect();

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
    let wait_result = unsafe {
        windows::Win32::System::Threading::WaitForSingleObject(handle, 0)
    };

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
