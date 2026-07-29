//! 用户词音节边界的跨层贯通测试（store → DictLayer → engine → Candidate）。
//!
//! 边界从用户词表（redb 24B value）出发，要穿过 `record_to_candidate` → `convert`
//! 才能到达候选，中途任何一处重建 `Candidate` 都会把它丢掉——丢了不报错，只会让
//! 依赖边界的判据静默退化。
//!
//! **这条链曾经就是断的**：`store_layer.rs` 的 `record_to_candidate` 用
//! `..Default::default()` 收尾，`r.boundary` 从未被带上，用户词候选 boundary 恒 0。
//! 影响面不止一处——双拼边界校验（任一侧 0 即放行 ⇒ 用户词一律不校验）、
//! 长词上浮判据、自动造词沿用边界，全部静默走「无边界」分支。
//! 见 docs/design/pinyin-code-domains.md §3 L2。
//!
//! 词典缺失时自动跳过。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use wind_config::Config;
use wind_engine::EngineManager;
use wind_store::Store;

fn data_dir() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../build_dev/data");
    p.join("schemas/pinyin/cn_dicts/base.dict.yaml")
        .exists()
        .then_some(p)
}

/// `words` = (code, text, weight, boundary)。每个用例独立目录，避免串扰。
fn manager(dir: &Path, tag: &str, words: &[(&str, &str, i32, u64)]) -> EngineManager {
    let root = std::env::temp_dir().join(format!("wind_uw_boundary_{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let store = Arc::new(Store::open(root.join("user_data.db")).expect("打开 store"));
    for (code, text, weight, boundary) in words {
        store
            .add_user_word("pinyin", code, text, *weight, *boundary)
            .expect("写入用户词");
    }
    let mut cfg = Config::default();
    cfg.schema.available = vec!["pinyin".to_string()];
    cfg.schema.active = "pinyin".to_string();
    EngineManager::with_store_override(&cfg, Some(dir), Some(store), Some(root.join("ov")))
}

/// 返回 (候选位次, boundary, is_promoted_completion)。未命中为 None。
fn find(mgr: &EngineManager, input: &str, text: &str) -> Option<(usize, u64, bool)> {
    let r = mgr.convert(input, 30);
    let i = r.candidates.iter().position(|c| c.text == text)?;
    let c = &r.candidates[i];
    Some((i, c.boundary, c.is_promoted_completion))
}

/// 边界须原样到达候选，且精确匹配与前缀补全两条路径都要带上。
#[test]
fn user_word_boundary_reaches_candidate() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    // 「大菠萝哥」da|bo|luo|ge → 起始字节位 {0,2,4,7}
    const B: u64 = 0b10010101;
    let mgr = manager(&dir, "reach", &[("daboluoge", "大菠萝哥", 5000, B)]);

    assert_eq!(
        find(&mgr, "daboluoge", "大菠萝哥").map(|t| t.1),
        Some(B),
        "整串精确命中须带边界"
    );
    assert_eq!(
        find(&mgr, "daboluo", "大菠萝哥").map(|t| t.1),
        Some(B),
        "前缀补全须带边界（长词上浮判据吃这个值）"
    );
}

/// **简拼须按真值边界投影声母，不得用 `maximum_match` 现猜**。
///
/// 「西安宁」真值切分 `xi|an|ning` ⇒ 简拼 `xan`。而 `maximum_match` 会把
/// `xianning` 切成 `xian|ning` ⇒ 猜出 `xn`。重猜的后果是**既漏又错**：
/// 真简拼 `xan` 打不出来，假简拼 `xn` 反而命中。
///
/// ⚠️ **本用例必须用歧义切分码**。仓库原有的两个简拼测试用
/// `cainiaoyizhan` / `lanshoubing`——`maximum_match` 恰好猜对，测不出这个缺陷，
/// 是「测试样本集体避开失效分支」的典型。
#[test]
fn user_word_abbrev_uses_true_boundary_not_maximum_match() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    const B: u64 = 0b10101; // xi|an|ning
    let with = manager(&dir, "abbr_on", &[("xianning", "西安宁", 5000, B)]);
    let without = manager(&dir, "abbr_off", &[("xianning", "西安宁", 5000, 0)]);

    let hit = |m: &EngineManager, q: &str| {
        m.convert(q, 30)
            .candidates
            .iter()
            .any(|c| c.text == "西安宁")
    };

    assert!(hit(&with, "xan"), "真值边界下 xi|an|ning 的简拼应为 xan");
    assert!(
        !hit(&with, "xn"),
        "xn 是 maximum_match 猜的 xian|ning 的投影，不该再命中"
    );

    // 对照组 = 修复前的行为，方向恰好相反
    assert!(
        !hit(&without, "xan"),
        "无边界只能退回 DAG 猜，真简拼打不出（这正是修复前的现场）"
    );
    assert!(hit(&without, "xn"), "无边界时反而是假简拼命中");

    // 全拼整串不受影响
    assert!(hit(&with, "xianning"));
    assert!(hit(&without, "xianning"));
}

/// **用户长词打 2 个音节即可上浮**——这正是边界接通后才拿得到的行为。
///
/// 「大菠萝哥」4 音节，输入 `dabo`（2 音节）：
/// - 有边界：`started=2`、距词尾 `remaining=2` ≤ COMPLETION_NEAR_SYLLABLES → 上浮
/// - 无边界：只能退回 `started >= 3` → 不上浮，于是被首音节一大批同音子短语整层压住，
///   **30 个候选里根本找不到**
///
/// 断链期间用户词恒走后者，「用户长词打部分拼音即上浮」对用户词从未真正生效。
#[test]
fn user_long_word_promotes_at_two_syllables_only_with_boundary() {
    let Some(dir) = data_dir() else {
        eprintln!("跳过：拼音词库不存在");
        return;
    };
    const B: u64 = 0b10010101; // da|bo|luo|ge
    let with = manager(&dir, "promo_on", &[("daboluoge", "大菠萝哥", 5000, B)]);
    let without = manager(&dir, "promo_off", &[("daboluoge", "大菠萝哥", 5000, 0)]);

    let hit = find(&with, "dabo", "大菠萝哥");
    assert!(
        hit.is_some_and(|(_, b, promoted)| b == B && promoted),
        "有边界时 dabo 应上浮进完整匹配层，实际 {hit:?}"
    );

    assert!(
        find(&without, "dabo", "大菠萝哥").is_none(),
        "对照组：无边界时 started=2 不足以上浮，用户词被同音子短语淹没在 30 名之外"
    );

    // 3 音节两侧都上浮（无边界分支的 started>=3 也满足）——确认差异只在 2 音节这一档，
    // 不是整体行为翻转。
    assert!(
        find(&with, "daboluo", "大菠萝哥").is_some_and(|(_, _, p)| p),
        "有边界：3 音节上浮"
    );
    assert!(
        find(&without, "daboluo", "大菠萝哥").is_some_and(|(_, _, p)| p),
        "无边界：3 音节同样上浮"
    );
}
