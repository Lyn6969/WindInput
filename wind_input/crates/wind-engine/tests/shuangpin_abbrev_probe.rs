//! 双拼「完整字 + 末位简拼」的候选标志位探针 —— **测量工具，不是门禁**。
//!
//! 起因：小鹤双拼下打 `zdm`（zd=在，m=吗的声母），首选是「在美国」而不是「在吗」。
//! 「在吗」两个字都被编码覆盖，「在美国」的第三个字**没有任何编码**、是词库补全出来的，
//! 完整覆盖本该压过前缀补全。
//!
//! 排序的真相源是 `wind_candidate::source_tier`：拼音候选只分「精确档 1」与「其余档 4」，
//! 而档 1 的判据里有 `!c.is_abbrev` —— 末位带简拼的「在吗」因此被一票踢出，与纯补全的
//! 「在美国」同落档 4，只比词频，于是高频长词抢位。
//!
//! 本探针打印二者的标志位与消费长度，用来确定修复该落在哪个字段上。
//!
//! ```text
//! cargo test -p wind-engine --test shuangpin_abbrev_probe -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn shuangpin_config() -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["shuangpin".into()];
    cfg.schema.active = "shuangpin".into();
    cfg.input.default.chinese_mode = true;
    cfg
}

#[test]
#[ignore = "定点探针：依赖 build_dev 真实词库，用 --ignored 显式运行"]
fn shuangpin_abbrev_flags_probe() {
    let dir = data_dir();
    if !dir.join("schemas/shuangpin.schema.toml").exists() {
        eprintln!("跳过：build_dev 缺少双拼方案");
        return;
    }
    let mgr = EngineManager::new(&shuangpin_config(), Some(&dir));

    for input in ["zdm", "zdma", "zd"] {
        let res = mgr.convert(input, 500);
        let cands = &res.candidates;
        println!("  completed_syllables={:?}", res.completed_syllables);
        println!("\n=== 输入 {input} → {} 条 ===", cands.len());
        println!(
            "{:<10} {:>4} {:>7} {:>7} {:>8} {:>7} {:>9} {:>10}",
            "候选", "字数", "common", "prefix", "abbrev", "partial", "consumed", "weight"
        );
        for c in cands.iter().take(12) {
            println!(
                "{:<10} {:>4} {:>7} {:>7} {:>8} {:>7} {:>9} {:>10}",
                c.text,
                c.text.chars().count(),
                c.is_common,
                c.is_prefix,
                c.is_abbrev,
                c.is_partial,
                c.consumed_length,
                c.weight,
            );
        }
        // 「在吗」若存在，单独点名它的位置与标志——它是本次要救的候选
        if let Some((i, c)) = cands.iter().enumerate().find(|(_, c)| c.text == "在吗") {
            println!(
                "  ★「在吗」在第 {} 位：common={} prefix={} abbrev={} partial={} consumed={} weight={}",
                i + 1,
                c.is_common,
                c.is_prefix,
                c.is_abbrev,
                c.is_partial,
                c.consumed_length,
                c.weight,
            );
        } else {
            println!("  ★「在吗」不在候选里");
        }
    }
}
