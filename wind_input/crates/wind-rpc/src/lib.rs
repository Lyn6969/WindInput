//! wind-rpc: core(wind_input) 的本地控制 / 配置 JSON-RPC 服务（命名管道 / unix socket）。
//!
//! 从内嵌 HTTP webapi 回退而来：去掉 axum/CORS/PNA/token/端口发现，本地授权靠 OS ACL。
//!
//! 模块：
//! - [`dispatch`]：传输无关的 JSON-RPC 分发（system.*/config.* + 转发 [`CoreRpc`]）。
//! - [`events`]：单向事件推送通道（config/dict 变更广播）。
//! - [`server`]：[`RpcServer`] + 传输抽象（windows pipe / unix socket）。
//!
//! 复用 wind-ipc 的 JSON-RPC 协议（Request/Response/EventMessage + 4 字节大端长度前缀帧）。

mod capabilities;
pub mod client;
mod dispatch;
mod events;
mod security;
mod server;

pub(crate) use dispatch::APP_VERSION;

pub use dispatch::{CoreRpc, DispatchState, dispatch};
pub use events::EventSink;
pub use server::{RpcServer, ctrl_endpoint, events_endpoint};
