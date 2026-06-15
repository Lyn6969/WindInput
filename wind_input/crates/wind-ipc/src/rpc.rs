//! JSON-RPC 协议定义（设置前端 ↔ 服务通信）
//!
//! 与 Go 版本 `wind_input/pkg/rpcapi/protocol.go` 对齐。

use serde::{Deserialize, Serialize};

/// 最大消息大小 (16MB)
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

/// 协议版本
pub const PROTOCOL_VERSION: i32 = 1;

/// JSON-RPC 请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(rename = "v")]
    pub version: i32,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// JSON-RPC 响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: u64, error: String) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// 事件消息（推送到订阅者）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    pub event: String,
    pub data: serde_json::Value,
}

/// 读取长度前缀帧的 JSON 消息
///
/// 格式: 4 字节 big-endian uint32 长度 + JSON 载荷
pub fn encode_message<T: Serialize>(msg: &T) -> Result<Vec<u8>, serde_json::Error> {
    let payload = serde_json::to_vec(msg)?;
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// 从字节流解码长度前缀帧
pub fn decode_message_header(buf: &[u8]) -> Option<u32> {
    if buf.len() < 4 {
        return None;
    }
    Some(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}
