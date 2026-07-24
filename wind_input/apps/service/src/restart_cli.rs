//! `wind_input restart` 命令行：重启（或启动）输入法服务。
//!
//! - core 在线：经 RPC `system.restart` 走既有重启流程（main 的 restart_rx →
//!   释放单例 Named Mutex → relaunch_self），与托盘菜单「重启服务」同一条路。
//! - core 未运行：直接 spawn 自身 exe（无参数 = 服务启动）；若其实有实例在
//!   运行（RPC 误判离线的边缘态），新实例会被单例检查挡掉，无害。

use serde_json::json;

use crate::cli_util::{RpcFailure, rpc};

/// 子命令入口。`args` 为 `restart` 之后的参数（不接受任何参数）。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    if let Some(first) = args.first() {
        if matches!(first.as_str(), "help" | "--help" | "-h") {
            println!("用法: wind_input restart   （服务在线则重启，未运行则直接启动）");
            return 0;
        }
        eprintln!("restart 不接受参数: {first}");
        return 2;
    }
    match rpc("system.restart", json!({})) {
        Ok(_) => {
            println!("✓ 已请求重启，服务将自动重新启动");
            0
        }
        Err(RpcFailure::Offline) => {
            // 服务未运行：以脱离形态拉起（CLI 此刻附着用户终端，裸 spawn 会让
            // 服务继承控制台与作业对象——关终端连带杀服务，见 spawn_detached_self）。
            match crate::spawn_detached_self(false) {
                Ok(()) => {
                    println!("服务未运行，已直接启动");
                    0
                }
                Err(e) => {
                    eprintln!("启动服务失败: {e}");
                    1
                }
            }
        }
        Err(RpcFailure::Remote(e)) => {
            eprintln!("重启请求失败: {e}");
            1
        }
    }
}
