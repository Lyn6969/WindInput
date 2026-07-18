//! wind_repl —— 无需 TSF/UI 的候选 REPL（平台无关，可在 Linux 跑）。
//!
//! 加载 data/ + 输入方案，从 stdin 读编码，打印候选。用于快速验证 engine/dict/store
//! 的"输入码 → 候选"逻辑，不依赖 Windows/TSF。
//!
//! 用法：`wind_repl [data_dir]`（data_dir 默认取环境变量 WIND_DATA，再退到 exe 同级 data/）。
//! 交互：输入编码回车看候选；`:l` 列方案；`:s <id>` 切换方案；`:q` 退出。

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn resolve_data_dir() -> Option<PathBuf> {
    if let Some(arg) = std::env::args().nth(1) {
        return Some(PathBuf::from(arg));
    }
    if let Ok(env) = std::env::var("WIND_DATA") {
        return Some(PathBuf::from(env));
    }
    Config::data_dir()
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        // 与 service / wind_tsf 一致，用本地时间而非默认的 UTC
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%d %H:%M:%S%.3f".to_string(),
        ))
        .with_writer(io::stderr)
        .init();

    let data_dir = resolve_data_dir();
    match &data_dir {
        Some(d) => eprintln!("data_dir = {}", d.display()),
        None => eprintln!("⚠ 未找到 data 目录（传参或设 WIND_DATA）；候选将为空"),
    }

    let config = Config::load(data_dir.as_deref()).unwrap_or_default();
    let mgr = EngineManager::new(&config, data_dir.as_deref());

    println!("== wind_repl ==");
    println!("当前方案: {}", mgr.active_schema_id());
    println!("可用方案: {:?}", mgr.available_schemas());
    println!("输入编码回车看候选；:l 列方案；:s <id> 切换；:q 退出");

    let stdin = io::stdin();
    let mut out = io::stdout();
    loop {
        print!("\n> ");
        out.flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF
        }
        let line = line.trim();
        match line {
            "" => continue,
            ":q" | ":quit" => break,
            ":l" | ":list" => {
                println!(
                    "可用方案: {:?}（当前 {}）",
                    mgr.available_schemas(),
                    mgr.active_schema_id()
                );
                continue;
            }
            _ if line.starts_with(":s ") => {
                let id = line[3..].trim();
                if mgr.switch_schema(id) {
                    println!("→ 已切换到 {}", mgr.active_schema_id());
                } else {
                    println!(
                        "✗ 切换失败（不存在或不可用）；当前 {}",
                        mgr.active_schema_id()
                    );
                }
                continue;
            }
            _ => {}
        }

        let r = mgr.convert(line, 9);
        if !r.preedit_display.is_empty() && r.preedit_display != line {
            println!("preedit: {}", r.preedit_display);
        }
        if r.candidates.is_empty() {
            println!("  (无候选)");
            continue;
        }
        for (i, c) in r.candidates.iter().enumerate() {
            println!(
                "  {}. {}   [w={} src={:?}{}{}]",
                i + 1,
                c.text,
                c.weight,
                c.source,
                if c.meta.is_user_dict { " user" } else { "" },
                if c.meta.is_temp_dict { " temp" } else { "" },
            );
        }
    }
    Ok(())
}
