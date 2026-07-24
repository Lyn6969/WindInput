//! `wind_input phrase ...` 命令行：用户短语导入导出与系统短语恢复。
//!
//! 经 RPC 打给运行中的 core（仅在线）。导入为合并语义（与设置页
//! `phrase.import` 同一契约，core 端无 replace 策略）。文件路径经 `resolve_path`
//! 解析，支持 `${APP_DIR}` / `${USER_DATA}` / `${LOCAL_DATA}` 内部目录变量。

use serde_json::{Value, json};

use crate::cli_util::{resolve_path, rpc_online};

/// 子命令入口。`args` 为 `phrase` 之后的参数。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("export") => match args.get(1) {
            Some(file) => cmd_export(file),
            None => return usage_err("export <文件>"),
        },
        Some("import") => match args.get(1) {
            Some(file) => cmd_import(file),
            None => return usage_err("import <文件>"),
        },
        Some("reset-system") => cmd_reset_system(),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_usage();
            return 0;
        }
        Some(other) => {
            eprintln!("未知子命令: {other}");
            print_usage();
            return 2;
        }
    };
    match r {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn print_usage() {
    eprintln!(
        "用法: wind_input phrase <命令>   （需要输入法服务在线）\n\
         \n\
         命令:\n  \
         export <文件>        导出用户短语到文件（wdict 文本）\n  \
         import <文件>        从文件导入用户短语（合并，同码同文条目以文件为准）\n  \
         reset-system         恢复系统预置短语（不动用户自建短语）"
    );
}

fn usage_err(form: &str) -> i32 {
    eprintln!("用法: wind_input phrase {form}");
    2
}

fn cmd_export(file: &str) -> anyhow::Result<i32> {
    let v = rpc_online("phrase.export", json!({}))?;
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("phrase.export 返回了意外形状"))?;
    let path = resolve_path(file)?;
    std::fs::write(&path, content).map_err(|e| anyhow::anyhow!("写入 {path} 失败: {e}"))?;
    println!("✓ 已导出用户短语到 {path}（{} 字节）", content.len());
    Ok(0)
}

fn cmd_import(file: &str) -> anyhow::Result<i32> {
    let path = resolve_path(file)?;
    let content =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("读取 {path} 失败: {e}"))?;
    let v = rpc_online("phrase.import", json!({ "content": content }))?;
    let imported = v.get("imported").and_then(Value::as_u64).unwrap_or(0);
    let skipped = v.get("skipped").and_then(Value::as_u64).unwrap_or(0);
    if skipped > 0 {
        println!("✓ 已导入 {imported} 条短语（跳过 {skipped} 条无效行）");
    } else {
        println!("✓ 已导入 {imported} 条短语");
    }
    Ok(0)
}

fn cmd_reset_system() -> anyhow::Result<i32> {
    let v = rpc_online("phrase.resetSystem", json!({}))?;
    let changed = v.get("changed").and_then(Value::as_u64).unwrap_or(0);
    println!("✓ 系统短语已恢复预置（变更 {changed} 条）");
    Ok(0)
}
