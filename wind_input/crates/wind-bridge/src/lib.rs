//! wind-bridge: Named Pipe 服务器 + 共享内存
//!
//! 与 Go 版本 `wind_input/internal/bridge/` 对齐。

pub mod deferred;
pub mod endpoint;
pub mod handler;
pub mod host_render_sink;
pub mod pipe_scope;
pub mod push;
pub mod security;
pub mod server;
pub mod shared_render_frame;

// macOS / Linux：UDS 请求/推送服务器 + POSIX SHM hostrender 写端。
// Windows 的请求/推送管道主循环内联在 server.rs / push.rs（cfg(windows)）；
// host-render 专属写端拆到独立 *_windows 模块（见下）。
#[cfg(unix)]
pub mod push_unix;
#[cfg(unix)]
pub mod server_unix;
// Android（bionic）无 POSIX SHM：libc 不导出 shm_open/shm_unlink，整模块排除。
// 唯一消费者是 wind-ui 的 macOS forwarder，Android 走进程内直调不经此。
#[cfg(all(unix, not(target_os = "android")))]
pub mod shared_memory_posix;

// Windows：命名 SHM 写端 + 命名 Event（带 AppContainer SDDL）+ HostRenderManager
#[cfg(windows)]
pub mod host_render_windows;
#[cfg(windows)]
pub mod named_event;
#[cfg(windows)]
pub mod shared_memory_windows;

pub use host_render_sink::HostRenderSink;
