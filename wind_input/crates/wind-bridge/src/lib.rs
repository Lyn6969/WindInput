//! wind-bridge: Named Pipe 服务器 + 共享内存
//!
//! 与 Go 版本 `wind_input/internal/bridge/` 对齐。

pub mod deferred;
pub mod handler;
pub mod push;
pub mod security;
pub mod server;
pub mod shared_memory;
