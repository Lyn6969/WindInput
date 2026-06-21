//! 单向事件推送通道：core 在 config/dict 变更、needsRestart 时向订阅者广播 JSON 事件。
//!
//! 设计参考 wind-bridge/push.rs：每连接一个 mpsc 发送端 + 独立 writer 线程；
//! [`EventSink`] 是广播句柄（可 clone，跨线程），core/dispatch 经它 `emit_*` 推事件。
//!
//! 传输：
//! - Windows: 单向 named pipe `\\.\pipe\wind_input{suffix}_events`（参考 push.rs）。
//! - unix(macOS/Linux): unix socket `..._events.sock`（见 transport.rs 的接线点）。
//!
//! 线路帧复用 wind-ipc 的 4 字节大端长度前缀 + JSON（[`EventMessage`]）。

use std::sync::{Arc, Mutex};

use serde_json::Value;
use wind_ipc::rpc::{EventMessage, encode_message};

/// 单个订阅者（writer 线程）的发送端。
struct Subscriber {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

/// 事件广播中心：持有所有订阅者发送端，`broadcast` 向全部投递（幂等、无副作用）。
#[derive(Clone)]
pub struct EventSink {
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
}

impl Default for EventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink {
    pub fn new() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 未接传输时的占位句柄：emit 仍可调用（注册订阅者前为 no-op）。
    /// 与 `new()` 等价，仅语义化命名（dispatch 默认值用）。
    pub fn disconnected() -> Self {
        Self::new()
    }

    /// 注册一个订阅者，返回其接收端（由传输层 writer 线程消费并写线路）。
    pub(crate) fn subscribe(&self) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        self.subscribers.lock().unwrap().push(Subscriber { tx });
        rx
    }

    /// 广播一条事件给所有订阅者。失败的订阅者（writer 线程已退出）下次广播时
    /// send 仍会失败但无副作用；如需主动回收可在此 retain（当前订阅者数量极少，从简）。
    pub fn broadcast(&self, event: &str, data: Value) {
        let msg = EventMessage {
            event: event.to_string(),
            data,
        };
        let bytes = match encode_message(&msg) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("事件编码失败 {}: {}", event, e);
                return;
            }
        };
        let subs = self.subscribers.lock().unwrap();
        for s in subs.iter() {
            let _ = s.tx.send(bytes.clone());
        }
    }

    /// 配置变更事件（setItems/reload 后）。事件名与前端约定一致："config.changed"。
    pub fn emit_config_changed(&self, data: Value) {
        self.broadcast("config.changed", data);
    }

    /// 词库变更事件（dict.* 写操作后，宿主按需调用）。
    pub fn emit_dict_changed(&self, data: Value) {
        self.broadcast("dict.changed", data);
    }

    /// 需要重启才能完全生效的提示事件。
    pub fn emit_needs_restart(&self, data: Value) {
        self.broadcast("needsRestart", data);
    }
}
