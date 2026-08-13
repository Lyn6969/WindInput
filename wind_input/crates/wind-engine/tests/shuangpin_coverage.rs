//! 双拼方案覆盖率：**每个内置方案能否打出全部 410 个标准音节**。
//!
//! 现有双拼测试（`shuangpin.rs` 34 条）是逐例断言——小鹤 9 条、手道 7 条，
//! 自然码/搜狗/紫光/微软各 1 条，**abc 一条没有**。逐例测试只能覆盖写测试的人想到的
//! 那几个音节，方案数据表里缺一行（某个韵母没编码、某个零声母漏了）不会被任何断言碰到，
//! 而用户会直接撞上「这个音节打不出来」。
//!
//! 本文件换一种覆盖方式：**反向枚举**。把全部键对（a-z 及符号键的两两组合）过一遍
//! `convert`，得到该方案实际能产出的音节集合，再与 `STANDARD_SYLLABLES` 对账。
//! 一次断言覆盖「目录里的全部方案 × 410 音节」，新增方案自动纳入（方案清单扫目录得来）。
//!
//! ⚠️ 覆盖率只回答「打不打得出」，不回答「**官方**击键打不打得出」。方案之间零声母
//! 规则不同（O 引导 vs 首字母引导），抄串了照样全绿 —— 那一层由本文件末尾的
//! `official_zero_initial_strokes_work` 正向击键表把守。

use std::collections::{HashMap, HashSet};
use wind_engine::pinyin::shuangpin::{Layout, ShuangpinConverter};
use wind_engine::pinyin::syllable::STANDARD_SYLLABLES;

/// 内置方案清单：**扫描目录**得来，不是硬编码列表。
///
/// ⚠️ 原先这里写死七个 id，本文件却宣称「新增方案自动纳入」—— 后来加的 `jiajia.toml`
/// 因此从未进过任何门禁。扫目录才让那句话成立：漏登记的方案不会静默豁免覆盖率检查。
fn layouts() -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(schema_dir()).expect("读取 data/schemas/shuangpin 失败") {
        let path = entry.expect("遍历目录项失败").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("方案文件名非 UTF-8");
        ids.push(stem.to_string());
    }
    ids.sort();
    assert!(!ids.is_empty(), "未扫到任何双拼方案 TOML");
    ids
}

fn schema_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../data/schemas/shuangpin")
}

/// 双拼可用的键位：26 字母 + 各方案用到的符号键（微软用 `;` 作韵母键）。
fn keys() -> Vec<u8> {
    (b'a'..=b'z').chain(*b";").collect()
}

/// 一组击键产出的**完整音节**；无匹配（原样回写）返回 `None`。
///
/// ⚠️ **不能用 `full_pinyin()` 判**：无匹配时它把击键原样回写，于是 `ai`/`an`/`er`/`ao`
/// 这些「击键字面恰好等于音节」的用例会假绿 —— 明明这个方案打不出，断言却过。
/// 判据必须是「转换器认下了一个音节」，即 `syllables` 恰好一条且吃满全部按键。
fn syllable_of(conv: &ShuangpinConverter, stroke: &str) -> Option<String> {
    let r = conv.convert(stroke);
    if r.has_partial || r.syllables.len() != 1 || r.syllables[0].raw_end != stroke.len() {
        return None;
    }
    Some(r.syllables[0].pinyin.clone())
}

fn load(id: impl AsRef<str>) -> ShuangpinConverter {
    let id = id.as_ref();
    let p = schema_dir().join(format!("{id}.toml"));
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

    for id in layouts() {
        let got = reachable(&load(&id));
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
    for id in layouts() {
        let got = reachable(&load(&id));
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

    for id in layouts() {
        let got = reachable(&load(&id));
        let missing: Vec<&&str> = zero.iter().filter(|s| !got.contains_key(**s)).collect();
        assert!(
            missing.is_empty(),
            "{id} 的零声母缺 {missing:?} —— 检查 [zero_initials] 引导键的允许列表是否列全"
        );
    }
}

/// 各方案**官方**零声母击键表 —— 每条都是「这个方案的用户实际会敲什么」。
///
/// ★ 为什么单有上面两条覆盖断言还不够：它们问的是「**存不存在某组**击键能打出这个音节」，
/// 不问「**官方那组**击键能不能打出」。微软双拼的 `[zero_initials]` 曾整段抄自首字母引导
/// 的模板，12 个零声母里 10 个官方击键（oa/ol/oj/oh/ok/oe/oz/of/og/or）全部打不出，
/// 用户被迫改用小鹤/自然码打法 —— 而两条覆盖断言全绿，因为音节确实「打得出」。
/// 这类「方案串味」缺陷只有正向击键表抓得到。
///
/// 规则依据（2026-08-12 核对）：**微软/搜狗/智能ABC/紫光**用 `O` 键引导零声母；
/// **自然码/小鹤**用首字母引导（单韵母重复 `aa`/`oo`/`ee`，双字母韵母打字面 `ai`/`an`，
/// 三字母韵母打首字母+韵母键 `ah`/`eg`）。两类规则不可互抄 —— 本次缺陷正是抄串了。
const OFFICIAL_ZERO_STROKES: &[(&str, [(&str, &str); 12])] = &[
    (
        "mspy",
        [
            ("oa", "a"),
            ("ol", "ai"),
            ("oj", "an"),
            ("oh", "ang"),
            ("ok", "ao"),
            ("oe", "e"),
            ("oz", "ei"),
            ("of", "en"),
            ("og", "eng"),
            ("or", "er"),
            ("oo", "o"),
            ("ob", "ou"),
        ],
    ),
    (
        // 搜狗与微软同键位、同零声母规则。
        "sogou",
        [
            ("oa", "a"),
            ("ol", "ai"),
            ("oj", "an"),
            ("oh", "ang"),
            ("ok", "ao"),
            ("oe", "e"),
            ("oz", "ei"),
            ("of", "en"),
            ("og", "eng"),
            ("or", "er"),
            ("oo", "o"),
            ("ob", "ou"),
        ],
    ),
    (
        // 智能ABC：同为 O 引导，但 ei 在 q 键（韵母键位与微软不同）。
        "abc",
        [
            ("oa", "a"),
            ("ol", "ai"),
            ("oj", "an"),
            ("oh", "ang"),
            ("ok", "ao"),
            ("oe", "e"),
            ("oq", "ei"),
            ("of", "en"),
            ("og", "eng"),
            ("or", "er"),
            ("oo", "o"),
            ("ob", "ou"),
        ],
    ),
    (
        // 自然码：首字母引导，**没有** O 引导规则。
        "ziranma",
        [
            ("aa", "a"),
            ("ai", "ai"),
            ("an", "an"),
            ("ah", "ang"),
            ("ao", "ao"),
            ("ee", "e"),
            ("ei", "ei"),
            ("en", "en"),
            ("eg", "eng"),
            ("er", "er"),
            ("oo", "o"),
            ("ou", "ou"),
        ],
    ),
    (
        // 小鹤：同为首字母引导。
        "xiaohe",
        [
            ("aa", "a"),
            ("ai", "ai"),
            ("an", "an"),
            ("ah", "ang"),
            ("ao", "ao"),
            ("ee", "e"),
            ("ei", "ei"),
            ("en", "en"),
            ("eg", "eng"),
            ("er", "er"),
            ("oo", "o"),
            ("ou", "ou"),
        ],
    ),
];
// 未纳入本表的方案及原因：
//   shoudao —— 零声母走 [zero_pairs] 显式键对，12 条已在 shuangpin.rs
//              `shoudao_zero_pairs_all` 逐条断言，不重复。
//   ziguang —— 现实现是「e 系列用 e 引导、其余用 o 引导」的双引导，而公开资料把紫光
//              列入「用 O 键作零声母」的方案。官方规则未核实，先不写进门禁以免锁错。
//   jiajia  —— 拼音加加的官方零声母规则未核实（RIME 的 pyjj 方案同时接受两套写法）。

/// **门禁**：官方击键必须打出对应零声母音节。
#[test]
fn official_zero_initial_strokes_work() {
    let mut failures = Vec::new();
    for (id, table) in OFFICIAL_ZERO_STROKES {
        let conv = load(id);
        for (stroke, want) in table {
            match syllable_of(&conv, stroke) {
                Some(got) if got == *want => {}
                Some(got) => failures.push(format!(
                    "{id}: 官方击键 {stroke:?} 应得 {want:?}，实得 {got:?}"
                )),
                None => failures.push(format!(
                    "{id}: 官方击键 {stroke:?} 应得 {want:?}，实际打不出任何音节"
                )),
            }
        }
    }
    assert!(
        failures.is_empty(),
        "零声母官方击键打不出（检查 data/schemas/shuangpin/<id>.toml 的 [zero_initials] \
         引导键与允许列表）:\n{}",
        failures.join("\n")
    );
}

/// **门禁**：O 引导方案里，首字母引导的打法必须**不**产出零声母。
///
/// 这条是上一条的反面。缺了它，`[zero_initials]` 里多留一行 `a = [...]` 不会被任何断言
/// 碰到 —— 官方击键照样绿，方案却悄悄同时接受了别家的打法，正是本次缺陷的形态。
#[test]
fn o_guided_layouts_reject_initial_letter_strokes() {
    // 这些击键在首字母引导方案里是零声母，在 O 引导方案里必须打不出零声母。
    const FOREIGN: &[(&str, &str)] = &[
        ("aa", "a"),
        ("ah", "ang"),
        ("ee", "e"),
        ("eg", "eng"),
        ("er", "er"),
    ];
    for id in ["mspy", "sogou", "abc"] {
        let conv = load(id);
        for (stroke, foreign) in FOREIGN {
            assert_ne!(
                syllable_of(&conv, stroke).as_deref(),
                Some(*foreign),
                "{id}：{stroke:?} 是首字母引导方案的打法，O 引导方案不应接受它"
            );
        }
    }
}

/// 探测报告：列出每个方案打不出的音节。**非断言**，供调整白名单时看现状。
#[test]
#[ignore = "报告用，不作为门禁；cargo test --test shuangpin_coverage -- --ignored --nocapture"]
fn coverage_report() {
    let all: HashSet<&str> = STANDARD_SYLLABLES.iter().copied().collect();
    for id in layouts() {
        let conv = load(&id);
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
