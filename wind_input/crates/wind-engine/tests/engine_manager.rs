//! 引擎管理器端到端测试
//!
//! 用仓库内真实 schema 构建 EngineManager，验证五笔/拼音转换产出候选。
//! 词典缺失时自动跳过。

use std::path::PathBuf;
use wind_config::Config;
use wind_engine::EngineManager;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build_debug/data")
}

fn make_config(schemas: &[&str]) -> Config {
    let mut cfg = Config::default();
    cfg.schema.available = schemas.iter().map(|s| s.to_string()).collect();
    cfg.schema.active = schemas[0].to_string();
    cfg
}

fn schema_exists(dir: &std::path::Path, id: &str) -> bool {
    dir.join(format!("schemas/{}.schema.toml", id)).exists()
        || dir.join(format!("schemas/{}.schema.yaml", id)).exists()
}

#[test]
fn test_wubi_engine_candidates() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") {
        eprintln!("跳过：wubi86 schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    let result = mgr.convert("aaaa", 9);
    assert!(!result.candidates.is_empty(), "五笔 'aaaa' 应产出候选");
    assert!(
        result.candidates.iter().any(|c| c.text == "恭恭敬敬"),
        "应包含 恭恭敬敬，实际: {:?}",
        result
            .candidates
            .iter()
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );
    assert!(!mgr.is_pinyin(), "wubi86 不应判定为拼音");
}

#[test]
fn test_wubi_extra_dict_loaded() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") {
        eprintln!("跳过：wubi86 schema 不存在");
        return;
    }
    // 删除旧 combined 缓存，强制重新合并多库
    let _ = std::fs::remove_file(dir.join("schemas/wubi86/wubi86_jidian.dict.combined.wdb"));
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // "甘蓝菜"(aaae) 仅存在于扩展库 wubi86_jidian_extra；主库没有。
    // 能查到即证明扩展库已被合并加载。
    let r = mgr.convert("aaae", 20);
    assert!(
        r.candidates.iter().any(|c| c.text == "甘蓝菜"),
        "扩展库词 '甘蓝菜'(aaae) 应能查到，实际: {:?}",
        r.candidates
            .iter()
            .take(10)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );

    // 主库词仍在
    let a = mgr.convert("aaaa", 20);
    assert!(
        a.candidates.iter().any(|c| c.text == "恭恭敬敬"),
        "主库词应仍在"
    );
}

#[test]
fn test_pinyin_engine_candidates() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    assert!(mgr.is_pinyin(), "pinyin 应判定为拼音");

    let result = mgr.convert("nihao", 9);
    assert!(!result.candidates.is_empty(), "拼音 'nihao' 应产出候选");
    let top10: Vec<&str> = result
        .candidates
        .iter()
        .take(10)
        .map(|c| c.text.as_str())
        .collect();
    // 整句应在首位（SENTENCE_WEIGHT_BASE 置顶）。
    assert_eq!(result.candidates[0].text, "你好", "首候选应为 你好，实际: {top10:?}");
    // 前缀子候选「你」应存在并标注只消费「ni」（分段上屏）。
    let ni = result.candidates.iter().find(|c| c.text == "你");
    assert!(ni.is_some(), "应包含前缀候选 你，实际: {top10:?}");
    assert_eq!(ni.unwrap().consumed_length, 2, "你 应只消费 ni 两字节");
    // 非前缀子串「好」（来自 hao 段）不应作为 nihao 的直接候选出现。
    assert!(
        !result.candidates.iter().any(|c| c.text == "好"),
        "不应包含非前缀子候选 好，实际: {top10:?}"
    );
}

#[test]
fn test_pinyin_long_sentence() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // 长拼音串：Viterbi 整句解码应产出一个 >=4 字的合理句子候选
    let r = mgr.convert("woaizhongguo", 20);
    let longest = r
        .candidates
        .iter()
        .map(|c| c.text.chars().count())
        .max()
        .unwrap_or(0);
    eprintln!(
        "woaizhongguo 候选: {:?}",
        r.candidates
            .iter()
            .take(8)
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        longest >= 4,
        "长句应产出 >=4 字候选（Viterbi+unigram），最长仅 {} 字",
        longest
    );
    assert!(
        r.candidates.iter().any(|c| c.text == "我爱中国"),
        "应能整句解码出 我爱中国"
    );
}

#[test]
fn test_mixed_wubi_priority_and_consistency() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86_pinyin") {
        eprintln!("跳过：wubi86_pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86_pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // cang：五笔精确全码「駏」(+10M tier) 应压过拼音「藏」(/100)，首候选=駏（五笔优先）。
    let r = mgr.convert("cang", 9);
    let top: Vec<&str> = r
        .candidates
        .iter()
        .take(6)
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(r.candidates[0].text, "駏", "cang 首候选应为五笔精确码 駏，实际: {top:?}");
    // 一致性：若放行全码自动上屏，commit_text 必等于显示首候选（杜绝显示/上屏漂移）。
    if r.should_commit {
        assert_eq!(
            r.commit_text, r.candidates[0].text,
            "全码上屏文本应与首候选一致"
        );
    }
    // 拼音「藏」仍在候选中（可选），只是不在首位。
    assert!(r.candidates.iter().any(|c| c.text == "藏"), "藏 应仍可选: {top:?}");
}

#[test]
fn test_mixed_multisyllable_pinyin_preedit_separated() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86_pinyin") {
        eprintln!("跳过：wubi86_pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["wubi86_pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    // 多音节拼音：组合区应带音节分隔（"ni hao"），而非连写。
    let r = mgr.convert("nihao", 9);
    assert!(
        r.preedit_display.contains(' '),
        "多音节拼音组合区应有音节分隔，实际 preedit: {:?}",
        r.preedit_display
    );
}

#[test]
fn test_pinyin_trailing_partial_keeps_sentence() {
    let dir = data_dir();
    if !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：pinyin schema 不存在");
        return;
    }
    let cfg = make_config(&["pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // 尾部多打一个不成音节的残码「m」：整句「你好」仍应排首位（bug①），
    // 残码不计入消费（consumed_length=5，留「m」在缓冲续输）。
    let r = mgr.convert("nihaom", 9);
    let top: Vec<&str> = r
        .candidates
        .iter()
        .take(8)
        .map(|c| c.text.as_str())
        .collect();
    assert_eq!(r.candidates[0].text, "你好", "首候选应为 你好（残码不破坏整句），实际: {top:?}");
    assert_eq!(
        r.candidates[0].consumed_length, 5,
        "你好 应只消费 nihao 五字节，残码 m 留缓冲"
    );
}

#[test]
fn test_schema_cycle() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    let cfg = make_config(&["wubi86", "pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    assert_eq!(mgr.active_schema_id(), "wubi86");
    let next = mgr.cycle_schema();
    assert_eq!(next.as_deref(), Some("pinyin"));
    assert_eq!(mgr.active_schema_id(), "pinyin");
    assert!(mgr.is_pinyin());
}

/// available 中夹杂构建失败的方案时，循环应跳过它找到下一个已加载方案。
#[test]
fn test_schema_cycle_skips_unloaded() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86") || !schema_exists(&dir, "pinyin") {
        eprintln!("跳过：缺少 schema");
        return;
    }
    // 中间插入一个不存在的方案 → 构建失败、不会进入 engines
    let cfg = make_config(&["wubi86", "__nonexistent__", "pinyin"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));
    assert_eq!(mgr.active_schema_id(), "wubi86");
    let next = mgr.cycle_schema();
    assert_eq!(
        next.as_deref(),
        Some("pinyin"),
        "应跳过未加载方案直达 pinyin"
    );
}
