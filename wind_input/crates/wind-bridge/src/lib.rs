//! wind-bridge: Named Pipe 服务器 + 共享内存
//!
//! 与 Go 版本 `wind_input/internal/bridge/` 对齐。

pub mod deferred;
pub mod endpoint;
pub mod handler;
pub mod host_render_sink;
pub mod push;
pub mod security;
pub mod server;
pub mod shared_memory;
pub mod shared_render_frame;

// macOS / Linux：UDS 请求/推送服务器 + POSIX SHM hostrender 写端。
// Windows 路径仍内联在 server.rs / push.rs（cfg(windows)），不引入 *_windows.rs。
#[cfg(unix)]
pub mod push_unix;
#[cfg(unix)]
pub mod server_unix;
#[cfg(unix)]
pub mod shared_memory_posix;

pub use host_render_sink::HostRenderSink;
