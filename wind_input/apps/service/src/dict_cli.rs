//! `wind_input dict ...` 命令行：用户词库（含临时词/词频/候选调整）按方案导入导出。
//!
//! 经 RPC 打给运行中的 core（仅在线）。文件读写在 CLI 侧完成，RPC 只传内容
//! 字符串——与设置页共用 `dict.export` / `dict.import` 同一契约（多段 wdict、
//! 引擎类型校验、Rime/TSV 自动识别）。文件路径经 `resolve_path` 解析，支持
//! `${APP_DIR}` / `${USER_DATA}` / `${LOCAL_DATA}` 内部目录变量。

use serde_json::{Value, json};

use crate::cli_util::{resolve_path, rpc_online};

/// 合法的段类型 key（与 wind-store `DictSection::key()` 驼峰一致）。
const SECTION_KEYS: &[&str] = &["userWords", "tempWords", "freq", "shadow"];

/// 子命令入口。`args` 为 `dict` 之后的参数。返回进程退出码。
pub fn run(args: &[String]) -> i32 {
    let r = match args.first().map(String::as_str) {
        Some("export") => match (args.get(1), args.get(2)) {
            (Some(id), Some(file)) => cmd_export(id, file, &args[3..]),
            _ => return usage_err("export <方案id> <文件> [--sections a,b,...]"),
        },
        Some("import") => match (args.get(1), args.get(2)) {
            (Some(id), Some(file)) => cmd_import(id, file, &args[3..]),
            _ => return usage_err("import <方案id> <文件> [--replace] [--sections a,b,...]"),
        },
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
        "用法: wind_input dict <命令>   （需要输入法服务在线）\n\
         \n\
         命令:\n  \
         export <方案id> <文件> [--sections a,b]   导出词库数据到文件（缺省按引擎默认段）\n  \
         import <方案id> <文件> [--replace] [--sections a,b]\n                                            \
         从文件导入（缺省合并；格式自动识别 WindDict/Rime/TSV）\n\
         \n\
         段类型: userWords(用户词库) tempWords(临时词库) freq(词频) shadow(候选调整)"
    );
}

fn usage_err(form: &str) -> i32 {
    eprintln!("用法: wind_input dict {form}");
    2
}

/// 解析 `--sections a,b` 与 `--replace` 旗标；未知旗标或未知段名报错。
fn parse_flags(rest: &[String]) -> anyhow::Result<(Option<Vec<String>>, bool)> {
    let mut sections: Option<Vec<String>> = None;
    let mut replace = false;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--replace" => replace = true,
            "--sections" => {
                let raw = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--sections 缺少参数（逗号分隔的段名）"))?;
                let keys: Vec<String> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                for k in &keys {
                    if !SECTION_KEYS.contains(&k.as_str()) {
                        anyhow::bail!("未知段类型: {k}（可选: {}）", SECTION_KEYS.join(" / "));
                    }
                }
                if keys.is_empty() {
                    anyhow::bail!("--sections 参数为空");
                }
                sections = Some(keys);
            }
            other => anyhow::bail!("未知参数: {other}"),
        }
    }
    Ok((sections, replace))
}

fn cmd_export(id: &str, file: &str, rest: &[String]) -> anyhow::Result<i32> {
    let (sections, replace) = parse_flags(rest)?;
    if replace {
        anyhow::bail!("export 不支持 --replace");
    }
    let mut params = json!({ "schemaId": id });
    if let Some(s) = &sections {
        params["sections"] = json!(s);
    }
    let v = rpc_online("dict.export", params)?;
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("dict.export 返回了意外形状"))?;
    let path = resolve_path(file)?;
    std::fs::write(&path, content).map_err(|e| anyhow::anyhow!("写入 {path} 失败: {e}"))?;
    println!("✓ 已导出 {id} 词库数据到 {path}（{} 字节）", content.len());
    Ok(0)
}

fn cmd_import(id: &str, file: &str, rest: &[String]) -> anyhow::Result<i32> {
    let (sections, replace) = parse_flags(rest)?;
    let path = resolve_path(file)?;
    let content =
        std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("读取 {path} 失败: {e}"))?;
    let mut params = json!({ "schemaId": id, "content": content });
    if replace {
        params["strategy"] = json!("replace");
    }
    if let Some(s) = &sections {
        params["sections"] = json!(s);
    }
    let v = rpc_online("dict.import", params)?;
    print_import_report(&v);
    Ok(0)
}

/// 打印 `{sections:[{key, added/updated/unchanged | imported, skipped}]}` 逐段结果。
fn print_import_report(v: &Value) {
    let Some(secs) = v.get("sections").and_then(Value::as_array) else {
        println!("✓ 导入完成");
        return;
    };
    // core 只处理「所选 ∩ 文件所含」的段：交集为空时若不提示，会零输出静默成功。
    if secs.is_empty() {
        println!("⚠ 文件不含所选段类型，未导入任何数据");
        return;
    }
    fn label(k: &str) -> &str {
        match k {
            "userWords" => "用户词库",
            "tempWords" => "临时词库",
            "freq" => "词频",
            "shadow" => "候选调整",
            other => other,
        }
    }
    for s in secs {
        let key = s.get("key").and_then(Value::as_str).unwrap_or("?");
        let skipped = s.get("skipped").and_then(Value::as_u64).unwrap_or(0);
        let mut parts = Vec::new();
        for (field, name) in [
            ("added", "新增"),
            ("updated", "更新"),
            ("unchanged", "不变"),
            ("imported", "导入"),
        ] {
            if let Some(n) = s.get(field).and_then(Value::as_u64) {
                parts.push(format!("{name} {n}"));
            }
        }
        if skipped > 0 {
            parts.push(format!("跳过 {skipped}"));
        }
        println!("✓ {}: {}", label(key), parts.join(" · "));
    }
}
