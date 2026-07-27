//! 用户配置落盘后的对外通知钩子。
//!
//! 背景：托盘/右键菜单、快捷键、命令栏都能直接改配置（「显示工具栏」「简入繁出」
//! 「主题」「常驻显示」…），这些路径不经 RPC，外部客户端无从得知。设置界面因此
//! 会显示打开那一刻的陈旧值——看起来权威，实则已经不对。
//!
//! 钩子挂在 [`Config::set_user_value`](crate::config::Config::set_user_value) 这个
//! **唯一落盘入口**上（`set_user_string`/`set_user_bool` 都是它的包装，命令栏那条
//! 能写任意键的路径也走它），一次覆盖全部写入点，不必逐个入口补广播、也不会随
//! 新增入口而漏掉。
//!
//! 依赖倒置同 wind-engine 的 `active_hook`：广播设施在 wind-rpc，位于本 crate 之上，
//! 故这里只留注册点，由 apps/service 在启动时注入。未注册时是 no-op。
//!
//! 载荷用 `toml::Value` 原样传出（本 crate 已依赖 toml，零新增依赖），JSON 封装
//! 是 wind-rpc 侧的事。

use std::sync::{Arc, OnceLock};

/// 配置变更回调：参数为配置路径（如 `["ui","theme","name"]`）与写入后的值。
///
/// 注意值是**入参值**而非重读文件的结果：`set_user_value` 在「值等于出厂默认」时
/// 会删除该键而不是写入，此时用户层无此键，但生效值仍等于这里传出的值，故对
/// 订阅方而言语义一致。
pub type ConfigChangeHook = Arc<dyn Fn(&[&str], &toml::Value) + Send + Sync>;

static HOOK: OnceLock<ConfigChangeHook> = OnceLock::new();

/// 注册回调（进程内仅首次生效，重复注册静默忽略）。
pub fn set_config_change_hook(hook: ConfigChangeHook) {
    let _ = HOOK.set(hook);
}

/// 通知配置项已落盘。仅在写入**成功**后调用。
pub(crate) fn notify_changed(path: &[&str], value: &toml::Value) {
    if let Some(hook) = HOOK.get() {
        hook(path, value);
    }
}
