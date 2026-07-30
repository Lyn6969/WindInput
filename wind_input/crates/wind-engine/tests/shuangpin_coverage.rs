//! 双拼方案覆盖率：**每个内置方案能否打出全部 410 个标准音节**。
//!
//! 现有双拼测试（`shuangpin.rs` 34 条）是逐例断言——小鹤 9 条、手道 7 条，
//! 自然码/搜狗/紫光/微软各 1 条，**abc 一条没有**。逐例测试只能覆盖写测试的人想到的
//! 那几个音节，方案数据表里缺一行（某个韵母没编码、某个零声母漏了）不会被任何断言碰到，
//! 而用户会直接撞上「这个音节打不出来」。
//!
//! 本文件换一种覆盖方式：**反向枚举**。把全部键对（a-z 及符号键的两两组合）过一遍
//! `convert`，得到该方案实际能产出的音节集合，再与 `STANDARD_SYLLABLES` 对账。
//! 一次断言覆盖 7 方案 × 410 音节，且新增方案自动纳入。

use std::collections::{HashMap, HashSet};
use wind_engine::pinyin::shuangpin::{Layout, ShuangpinConverter};
use wind_engine::pinyin::syllable::STANDARD_SYLLABLES;

/// 内置方案清单（`data/schemas/shuangpin/*.toml`）。
const LAYOUTS: &[&str] = &[
    "xiaohe", "ziranma", "mspy", "sogou", "abc", "ziguang", "shoudao",
];

/// 双拼可用的键位：26 字母 + 各方案用到的符号键（微软用 `;` 作韵母键）。
fn keys() -> Vec<u8> {
    (b'a'..=b'z').chain([b';']).collect()
}

fn load(id: &str) -> ShuangpinConverter {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../data/schemas/shuangpin")
        .join(format!("{id}.toml"));
    ShuangpinConverter::new(Layout::from_toml(&p).unwrap_or_else(|e| panic!("加载 {id}: {e}")))
}

/// 该方案实际能打出的音节 → 击键对。
///
/// 走的是**公开的 `convert`**，不是内部映射表——测的因此是「用户敲这两个键会得到什么」，
/// 而不是「表里写了什么」。两者不等价：零声母有三条查找路径、模糊声母还会追加变体。
fn reachable(conv: &ShuangpinConverter) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for k1 in keys() {
        for k2 in keys() {
            let stroke = String::from_utf8(vec![k1, k2]).unwrap();
            let r = conv.convert(&stroke);
            // 只认「恰好一个完整音节、且完整覆盖两键」的结果：partial 与原样回写不算打得出。
            if r.has_partial || r.syllables.len() != 1 {
                continue;
            }
            let syl = &r.syllables[0].pinyin;
            if r.syllables[0].raw_end != 2 {
                continue;
            }
            m.entry(syl.clone()).or_insert(stroke);
        }
    }
    m
}

/// 已知打不出的音节，逐条都要有性质说明 —— 白名单是**记录取舍**，不是掩盖缺口。
///
/// `lo`（全方案）：所有方案的 `o` 键都是 `["uo", "o"]` 一键双韵母，转换只取第一个，
/// 于是 `lo` 恒被 `luo` 遮蔽。`lo` 只用于「咯」的一个读音，而 `luo`（罗/落/络…）
/// 是高频音节 —— 这是双拼编码本身的容量限制，不是数据缺失，各家商业方案同样如此。
const KNOWN_UNREACHABLE: &[&str] = &["lo"];

/// **门禁**：每个内置方案都必须能打出全部标准音节（白名单除外）。
///
/// 这条断言是本文件存在的理由。它替代不了逐例测试的精确性，但覆盖的是逐例测试
/// 结构上够不到的地方 —— 「没人想到要测的那个音节」。历史上一跑就抓到两处：
/// - `abc` 的 `zero_initials` 只填了 12 个零声母里的 2 个 ⇒ 爱/安/恩/儿全打不出；
/// - `ziguang` 的 finals 缺 `v = ["v"]` ⇒ 绿/女打不出（而略/虐正常）。
///
/// 两处都是「结构就位、数据没填满」，加载测试、逐例测试、真机常用字全都碰不到。
#[test]
fn every_layout_covers_all_standard_syllables() {
    let allow: HashSet<&str> = KNOWN_UNREACHABLE.iter().copied().collect();
    let mut failures = Vec::new();

    for id in LAYOUTS {
        let got = reachable(&load(id));
        let mut missing: Vec<&str> = STANDARD_SYLLABLES
            .iter()
            .copied()
            .filter(|s| !got.contains_key(*s) && !allow.contains(s))
            .collect();
        missing.sort_unstable();
        if !missing.is_empty() {
            failures.push(format!("{id} 打不出 {} 个音节: {missing:?}", missing.len()));
        }
    }

    assert!(
        failures.is_empty(),
        "双拼方案覆盖不全（补 data/schemas/shuangpin/<id>.toml 的 finals / zero_initials）:\n{}",
        failures.join("\n")
    );
}

/// 白名单自身必须**当前真的打不出**，否则它就在掩盖一条已经恢复的能力。
/// 缺了这条自检，白名单会随着方案数据修好而悄悄变成一张废纸，还继续豁免着别的东西。
#[test]
fn whitelist_entries_are_still_actually_unreachable() {
    for id in LAYOUTS {
        let got = reachable(&load(id));
        for s in KNOWN_UNREACHABLE {
            assert!(
                !got.contains_key(*s),
                "{id} 现在打得出「{s}」了（击键 {:?}）—— 请把它从 KNOWN_UNREACHABLE 移除",
                got.get(*s)
            );
        }
    }
}

/// 零声母是双拼最容易漏的一类（每个方案规则都不同，且不在常用字里露头）。
/// 单独立一条断言，让失败信息直接指出「是零声母漏了」而不是混在 400 个音节里。
#[test]
fn every_layout_covers_zero_initial_syllables() {
    // 全部以元音开头的标准音节
    let zero: Vec<&str> = STANDARD_SYLLABLES
        .iter()
        .copied()
        .filter(|s| s.starts_with(['a', 'e', 'o']))
        .collect();
    assert_eq!(zero.len(), 12, "零声母音节应有 12 个: {zero:?}");

    for id in LAYOUTS {
        let got = reachable(&load(id));
        let missing: Vec<&&str> = zero.iter().filter(|s| !got.contains_key(**s)).collect();
        assert!(
            missing.is_empty(),
            "{id} 的零声母缺 {missing:?} —— 检查 [zero_initials] 引导键的允许列表是否列全"
        );
    }
}

/// 探测报告：列出每个方案打不出的音节。**非断言**，供调整白名单时看现状。
#[test]
#[ignore = "报告用，不作为门禁；cargo test --test shuangpin_coverage -- --ignored --nocapture"]
fn coverage_report() {
    let all: HashSet<&str> = STANDARD_SYLLABLES.iter().copied().collect();
    for id in LAYOUTS {
        let conv = load(id);
        let got = reachable(&conv);
        let mut missing: Vec<&str> = all
            .iter()
            .copied()
            .filter(|s| !got.contains_key(*s))
            .collect();
        missing.sort_unstable();
        println!(
            "\n=== {id}: {}/{} 可达，缺 {} 个 ===",
            got.len().min(all.len()),
            all.len(),
            missing.len()
        );
        println!("{missing:?}");
    }
}
