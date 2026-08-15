//! `COMPLETION_WEAK_CEILING` 的标定探针 —— **测量工具，不是门禁**（门禁在
//! `wind-coordinator/tests/pinyin_short_context_sentence.rs`）。
//!
//! ## 为什么判据不能依赖整句分数
//!
//! 闸门首版用「整句 / 最强 d=1 补全」的**倍数**(24×)，在 grammar 关闭下标定。而语法模型
//! 给每个词间转移叠加负的上下文分，整句 `log_prob` 系统性下移（`zaim` 的「在吗」
//! −16.96 → −19.91），经 `sentence_weight` 的几何平均换算后 weight 从 50189 掉到 11483，
//! 倍数 61.3× → 14.0×，**被自己的闸门拦掉** —— 真机表现为「zdm 还是在美国」。
//!
//! 补全侧是词典词频，完全不受 grammar 影响：两轴发生了相对平移，跨轴比较的病根一直在，
//! 只是 grammar 关闭时恰好没暴露。故判据改为**补全侧的绝对词频**（单轴、grammar 无关）。
//!
//! ## 本探针给出什么
//!
//! 在 OFF / 万象 w=0.5 两种配置下跑同一批「1 音节 + 残码」场景，并排打印补全侧最强者的
//! 权重与倍数。改 `COMPLETION_WEAK_CEILING` 或
//! `SENTENCE_KEEP_MAX_COMPLETED_SYLS` 前**先跑它**，确认分界线两侧仍有余量：
//!
//! ```text
//! cargo test -p wind-engine --test grammar_ratio_calibration -- --ignored --nocapture
//! ```
//!
//! ⚠️ 要看未经闸门过滤的原始分布时，注意本探针跑的是**闸门之后**的候选：被拦下的整句
//! 不会出现在 `candidates` 里。标定新阈值时应先临时放行再测。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data")
}

fn cfg_with(grammar: Option<(&str, f64)>) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".into()];
    cfg.schema.active = "pinyin".into();
    cfg.input.default.chinese_mode = true;
    if let Some((model, weight)) = grammar {
        cfg.schema.pinyin.grammar.model = model.to_string();
        cfg.schema.pinyin.grammar.weight = weight;
    }
    cfg
}

/// 期望首选：整句该赢的标 true。
const CASES: &[(&str, &str, bool)] = &[
    ("zaim", "在吗", true),
    ("nih", "你会", false),
    ("meiy", "没有", false),
    ("wom", "我们", false),
    ("tam", "他们", false),
    ("nim", "你们", false),
    ("zenm", "怎么", false),
    ("shenm", "什么", false),
    ("zhid", "知道", false),
    ("yinw", "因为", false),
    ("shih", "适合", false),
    ("xiex", "谢谢", false),
    ("haiy", "还有", false),
    ("meig", "美国", false),
];

#[test]
#[ignore = "标定探针"]
fn ratio_under_grammar() {
    let dir = data_dir();
    if !dir
        .join("schemas/pinyin/grammar/wanxiang-lts-zh-hans.gram")
        .exists()
    {
        eprintln!("跳过：万象模型不存在");
        return;
    }
    for (label, g) in [
        ("grammar OFF", None),
        ("万象 w=0.5", Some(("wanxiang-lts-zh-hans.gram", 0.5))),
    ] {
        let mgr = EngineManager::new(&cfg_with(g), Some(&dir));
        println!("\n════════ {label} ════════");
        println!(
            "{:<10} {:<10} {:>9}  {:<10} {:>9} {:>9}  {}",
            "输入", "残码整句", "整句w", "最强d=1补全", "补全w", "倍数", "整句该赢?"
        );
        for (input, _want, sentence_should_win) in CASES {
            let res = mgr.convert(input, 500);
            let completed = res.completed_syllables.len() as u32;
            let sent = res
                .candidates
                .iter()
                .filter(|c| c.is_sentence)
                .max_by_key(|c| c.consumed_length);
            // ⚠️ 必须排除 is_sentence：step 2c 与 step4 同文时走**合并**分支，那条候选
            // 既是整句又带 is_prefix，算进来会让两列取到同一条、倍数恒为 1.0×（假值）。
            let comp = res
                .candidates
                .iter()
                .filter(|c| {
                    c.is_prefix
                        && !c.is_sentence
                        && !c.is_fuzzy
                        && c.boundary.count_ones() == completed + 1
                })
                .max_by_key(|c| c.weight);
            match (sent, comp) {
                (Some(s), Some(p)) => println!(
                    "{:<10} {:<10} {:>9}  {:<10} {:>9} {:>8.1}×  {}",
                    input,
                    s.text,
                    s.weight,
                    p.text,
                    p.weight,
                    s.weight as f64 / p.weight.max(1) as f64,
                    if *sentence_should_win {
                        "★是"
                    } else {
                        "否"
                    }
                ),
                _ => println!(
                    "{:<10} （无残码整句 / 无补全）{}",
                    input,
                    if *sentence_should_win { "  ★是" } else { "" }
                ),
            }
        }
    }
}
