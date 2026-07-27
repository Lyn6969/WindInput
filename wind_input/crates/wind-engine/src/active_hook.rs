//! 活跃方案变更的对外通知钩子。
//!
//! 背景：活跃方案由 [`crate::manager::EngineManager`] 持有，但需要知道它变化的是
//! 上层——RPC 事件通道要把变更广播给设置界面等外部客户端。而 wind-rpc 依赖
//! wind-engine，反向依赖不成立，EngineManager 无法直接持有 `EventSink`
//! （同款分层说明见 wind-coordinator 的 `handle_addword.rs`）。
//!
//! 故此处用一个进程级注册点做依赖倒置：wind-rpc 启动事件服务时注入闭包，
//! EngineManager 只管在 active 变化时调 [`notify_active_changed`]。未注册时
//! （无 RPC 服务的测试、CLI 等场景）是 no-op，不影响任何既有行为。
//!
//! 载荷只有方案 id 一个 `&str`，刻意不用 `serde_json::Value`——那会让
//! wind-engine 平白多一个依赖，而 JSON 封装是 wind-rpc 侧的事。

use std::sync::{Arc, OnceLock};

/// 活跃方案变更回调：参数为新的方案 id。
pub type ActiveSchemaHook = Arc<dyn Fn(&str) + Send + Sync>;

static HOOK: OnceLock<ActiveSchemaHook> = OnceLock::new();

/// 注册回调（进程内仅首次生效，重复注册静默忽略）。
pub fn set_active_schema_hook(hook: ActiveSchemaHook) {
    let _ = HOOK.set(hook);
}

/// 通知活跃方案已变更。**调用方必须先释放 active 锁**——回调是上层代码，
/// 不应在持锁期间执行。
pub(crate) fn notify_active_changed(id: &str) {
    if let Some(hook) = HOOK.get() {
        hook(id);
    }
}
