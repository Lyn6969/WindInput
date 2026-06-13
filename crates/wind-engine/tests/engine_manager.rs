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
        result.candidates.iter().map(|c| c.text.as_str()).collect::<Vec<_>>()
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
    let _ = std::fs::remove_file(
        dir.join("schemas/wubi86/wubi86_jidian.dict.combined.wdb"),
    );
    let cfg = make_config(&["wubi86"]);
    let mgr = EngineManager::new(&cfg, Some(&dir));

    // "甘蓝菜"(aaae) 仅存在于扩展库 wubi86_jidian_extra；主库没有。
    // 能查到即证明扩展库已被合并加载。
    let r = mgr.convert("aaae", 20);
    assert!(
        r.candidates.iter().any(|c| c.text == "甘蓝菜"),
        "扩展库词 '甘蓝菜'(aaae) 应能查到，实际: {:?}",
        r.candidates.iter().take(10).map(|c| c.text.as_str()).collect::<Vec<_>>()
    );

    // 主库词仍在
    let a = mgr.convert("aaaa", 20);
    assert!(a.candidates.iter().any(|c| c.text == "恭恭敬敬"), "主库词应仍在");
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
    assert!(
        !result.candidates.is_empty(),
        "拼音 'nihao' 应产出候选"
    );
    assert!(
        result.candidates.iter().any(|c| c.text.contains("你好") || c.text == "你好"),
        "应包含 你好，实际: {:?}",
        result.candidates.iter().take(10).map(|c| c.text.as_str()).collect::<Vec<_>>()
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
        r.candidates.iter().take(8).map(|c| c.text.as_str()).collect::<Vec<_>>()
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
fn test_schema_cycle() {
    let dir = data_dir();
    if !schema_exists(&dir, "wubi86")
        || !schema_exists(&dir, "pinyin")
    {
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
    assert_eq!(next.as_deref(), Some("pinyin"), "应跳过未加载方案直达 pinyin");
}
