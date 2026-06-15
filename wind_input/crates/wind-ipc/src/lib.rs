//! wind-ipc: IPC 协议定义与编解码
//!
//! 与 Go 版本 `wind_input/internal/ipc/` 和 `wind_input/pkg/rpcapi/` 对齐。
//! 二进制协议用于 TSF DLL ↔ Go 服务通信，JSON-RPC 用于设置前端 ↔ 服务通信。

pub mod codec;
pub mod protocol;
pub mod rpc;

pub use codec::*;
pub use protocol::*;
