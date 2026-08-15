//! 「完整音节 + 末位残码」的候选标志位/权重探针 —— **测量工具，不是门禁**。
//!
//! 门禁在 `wind-coordinator/tests/pinyin_short_context_sentence.rs`（残码排序由协调器定夺，
//! 引擎级测不出来）。本文件只负责把中间量打出来，供改判据时标定。
//!
//! ## 起因与最终结论
//!
//! 双拼打 `zdm`（zd=zai，m=「吗」的声母）首选是「在美国」而不是「在吗」。根因是
//! **词库数据缺陷 + 层级硬闸门**两层叠加：「在吗」在 `base.dict.yaml:103576` 里 `w=0`，
//! 命中 `demote_to_prefix_layer` 被踢进前缀层，而 `eff_prefix` 是层级键、跨层不比权重。
//! 修复是给它开第二条生成路径（残码整句，走单字乘积绕开 w=0），见 step 2c / 6.5c。
//!
//! ## ⚠️ 两条**被实测推翻**的假设（勿再据此推理）
//!
//! - ~~`source_tier` 档 1 的 `!c.is_abbrev` 把末位带简拼的「在吗」一票踢出~~
//!   —— 实测全部候选 `is_abbrev=false`，简拼判据压根没参与。
//! - ~~折扣 `COMPLETION_WEIGHT_DISCOUNT` 把它压成了 0~~
//!   —— `completion_penalized` 有 `.max(1.0)`，折扣**造不出 0**；0 是词库原值。
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
    schema_config("shuangpin")
}

fn schema_config(schema: &str) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec![schema.into()];
    cfg.schema.active = schema.into();
    cfg.input.default.chinese_mode = true;
    cfg
}

/// 6.5b 让位阈值的**标定**探针：残码整句 vs 最强「恰好用完残码」补全的分数比。
///
/// 6.5b 原本无条件让整句让位（降到 `补全max - 1`）。step 2c 的残码整句消费了整串、
/// 自己就是「用完残码」的一方，被误伤（`zaim` 的「在吗」被压到 818）。但简单豁免又会
/// 让高频字拼出的虚高合成解翻上来（`nih`→「你和」、`beijingd`→「背景的」）。
///
/// 判据取「整句 / 最强 d=1 补全」的倍数：倍数大 = 词库在这个码上给不出好答案，该信整句。
/// 本探针批量打印该倍数，用于确认分界线两侧有没有反例。
#[test]
#[ignore = "标定探针：依赖 build_dev 真实词库，用 --ignored 显式运行"]
fn sentence_yield_ratio_calibration() {
    let dir = data_dir();
    if !dir.join("schemas/pinyin/cn_dicts/base.dict.yaml").exists() {
        eprintln!("跳过：build_dev 缺少拼音词库");
        return;
    }
    let mgr = EngineManager::new(&schema_config("pinyin"), Some(&dir));
    println!(
        "{:<16} {:<10} {:>9}  {:<10} {:>9} {:>8}",
        "输入", "残码整句", "整句w", "最强d=1补全", "补全w", "倍数"
    );
    for input in [
        "zaim",
        "nih",
        "meiy",
        "beijingd",
        "zhongguorenm",
        "nihaom",
        "zhonghuar",
        "buzhidaok",
        "jisuanjik",
        "wom",
        "tam",
        "nim",
        "zenm",
        "zhem",
        "shenm",
        "zhid",
        "shih",
        "yinw",
        "keyi",
        "bush",
        "xiex",
        "duibuq",
        "meig",
        "haiy",
        "jiush",
        "dansh",
        "yinggail",
        "womenj",
    ] {
        let res = mgr.convert(input, 500);
        let completed = res.completed_syllables.len() as u32;
        // 残码整句 = is_sentence 且消费长度达整串（step 2 整句只消费 completed 段）
        let sent = res
            .candidates
            .iter()
            .filter(|c| c.is_sentence)
            .max_by_key(|c| c.consumed_length);
        // 「恰好用完残码」的补全：音节数 == 已完成 + 1
        let comp = res
            .candidates
            .iter()
            .filter(|c| c.is_prefix && !c.is_fuzzy && c.boundary.count_ones() == completed + 1)
            .max_by_key(|c| c.weight);
        match (sent, comp) {
            (Some(s), Some(p)) => println!(
                "{:<16} {:<10} {:>9}  {:<10} {:>9} {:>7.1}×",
                input,
                s.text,
                s.weight,
                p.text,
                p.weight,
                s.weight as f64 / p.weight.max(1) as f64
            ),
            (Some(s), None) => println!(
                "{:<16} {:<10} {:>9}  {:<10} {:>9} {:>8}",
                input, s.text, s.weight, "—", "—", "无补全"
            ),
            _ => println!("{input:<16} （无残码整句）"),
        }
    }
}

/// 残码整句的**分数**探针：整句是否生成、拿了多少 weight、与谁竞争。
///
/// step 2c 放开门槛后，「在吗」由 Viterbi 拼出（`is_sentence=true`，绕开词条 w=0），
/// 但它要在同层里压过 distance=1 的词典补全「再买」(819) 与 distance=2 的「在美国」(3699)。
/// 本探针打出 `sent` 列以确认谁是整句，并给出各自的 weight 用于定夺让位手法。
#[test]
#[ignore = "定点探针：依赖 build_dev 真实词库，用 --ignored 显式运行"]
fn trailing_partial_sentence_weight_probe() {
    let dir = data_dir();
    if !dir.join("schemas/pinyin/cn_dicts/base.dict.yaml").exists() {
        eprintln!("跳过：build_dev 缺少拼音词库");
        return;
    }
    for (schema, input) in [
        ("pinyin", "zaim"),
        ("shuangpin", "zdm"),
        ("pinyin", "nih"),
        ("pinyin", "meiy"),
        ("pinyin", "beijingd"),
        ("pinyin", "zhongguorenm"),
        ("pinyin", "nihaom"),
        ("pinyin", "zhonghuar"),
        ("pinyin", "buzhidaok"),
        ("pinyin", "jisuanjik"),
    ] {
        let mgr = EngineManager::new(&schema_config(schema), Some(&dir));
        let res = mgr.convert(input, 500);
        println!(
            "\n=== {schema} / {input} → {} 条  completed={:?} ===",
            res.candidates.len(),
            res.completed_syllables
        );
        println!(
            "{:<8} {:>5} {:>7} {:>7} {:>9} {:>10}",
            "候选", "sent", "prefix", "partial", "consumed", "weight"
        );
        for c in res.candidates.iter().take(8) {
            println!(
                "{:<8} {:>5} {:>7} {:>7} {:>9} {:>10}",
                c.text, c.is_sentence, c.is_prefix, c.is_partial, c.consumed_length, c.weight
            );
        }
        if let Some((i, c)) = res
            .candidates
            .iter()
            .enumerate()
            .find(|(_, c)| c.text == "在吗")
        {
            println!(
                "  ★「在吗」第 {} 位：sent={} prefix={} consumed={} weight={}",
                i + 1,
                c.is_sentence,
                c.is_prefix,
                c.consumed_length,
                c.weight
            );
        }
    }
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
